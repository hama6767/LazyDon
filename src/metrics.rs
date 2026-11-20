//! Lightweight, best-effort Linux host performance metrics.
//!
//! Reading /proc never blocks the UI: a single background worker samples it
//! once per second, and the UI only copies the latest snapshot.
use std::{
    fs,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Duration,
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Metrics {
    /// CPU utilization across all cores since the previous sample.
    pub cpu_percent: Option<f64>,
    pub memory_percent: Option<f64>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
struct CpuTimes {
    total: u64,
    idle: u64,
}

fn parse_cpu(input: &str) -> Option<CpuTimes> {
    let line = input
        .lines()
        .find(|line| line.split_whitespace().next() == Some("cpu"))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    // Linux reports user, nice, system, idle, iowait, irq, softirq and steal.
    // Guest counters (the following fields) are already included in user/nice.
    if fields.len() < 4 {
        return None;
    }
    let total = fields.iter().copied().fold(0u64, u64::saturating_add);
    let idle = fields[3].saturating_add(fields.get(4).copied().unwrap_or(0));
    Some(CpuTimes { total, idle })
}

fn cpu_usage(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let total = current.total.checked_sub(previous.total)?;
    let idle = current.idle.checked_sub(previous.idle)?;
    if total == 0 || idle > total {
        return None;
    }
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn parse_memory(input: &str) -> Option<(u64, u64)> {
    fn value(input: &str, key: &str) -> Option<u64> {
        input.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name != key {
                return None;
            }
            let mut parts = value.split_whitespace();
            let amount: u64 = parts.next()?.parse().ok()?;
            // /proc/meminfo reports kB, which on Linux means 1024 bytes.
            if parts.next()? != "kB" {
                return None;
            }
            Some(amount.saturating_mul(1024))
        })
    }
    let total = value(input, "MemTotal")?;
    if total == 0 {
        return None;
    }
    // MemAvailable accounts for reclaimable cache and is supported by modern
    // Linux kernels. Fall back to MemFree when it is missing.
    let available = value(input, "MemAvailable")
        .or_else(|| value(input, "MemFree"))?
        .min(total);
    Some((total - available, total))
}

fn sample(previous: &mut Option<CpuTimes>) -> Metrics {
    let cpu = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| parse_cpu(&s));
    let cpu_percent = previous.zip(cpu).and_then(|(old, new)| cpu_usage(old, new));
    *previous = cpu;
    let memory = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_memory(&s));
    Metrics {
        cpu_percent,
        memory_percent: memory.map(|(used, total)| used as f64 * 100.0 / total as f64),
        memory_used_bytes: memory.map(|(used, _)| used),
        memory_total_bytes: memory.map(|(_, total)| total),
    }
}

/// Start sampling on first use and return the most recent snapshot. Unsupported
/// hosts and temporarily unavailable /proc files are represented by None.
pub fn current() -> Metrics {
    static SNAPSHOT: OnceLock<Arc<Mutex<Metrics>>> = OnceLock::new();
    let snapshot = SNAPSHOT.get_or_init(|| {
        let snapshot = Arc::new(Mutex::new(Metrics::default()));
        let worker_snapshot = Arc::clone(&snapshot);
        thread::spawn(move || {
            let mut previous = None;
            loop {
                let next = sample(&mut previous);
                *worker_snapshot.lock().unwrap_or_else(|e| e.into_inner()) = next;
                thread::sleep(Duration::from_secs(1));
            }
        });
        snapshot
    });
    *snapshot.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_uses_deltas_and_excludes_guest_counters() {
        let first = parse_cpu("cpu 100 10 20 800 20 5 5 0 100 2\ncpu0 1 2 3 4\n").unwrap();
        let second = parse_cpu("cpu 150 10 30 850 30 5 5 0 120 2\n").unwrap();
        assert!((cpu_usage(first, second).unwrap() - 50.0).abs() < 0.001);
        assert!(cpu_usage(first, first).is_none());
        assert!(cpu_usage(second, first).is_none());
        assert!(parse_cpu("cpu 1 bad 3 4").is_none());
    }

    #[test]
    fn memory_uses_available_and_handles_missing_fields() {
        let data = "MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\n";
        assert_eq!(parse_memory(data), Some((600 * 1024, 1000 * 1024)));
        assert_eq!(
            parse_memory("MemTotal: 100 kB\nMemFree: 20 kB\n"),
            Some((80 * 1024, 100 * 1024))
        );
        assert_eq!(
            parse_memory("MemTotal: 100 kB\nMemAvailable: 120 kB\n"),
            Some((0, 100 * 1024))
        );
        assert_eq!(parse_memory("MemTotal: 0 kB\nMemAvailable: 1 kB\n"), None);
        assert_eq!(parse_memory("MemTotal: 100 kB\n"), None);
    }
}
