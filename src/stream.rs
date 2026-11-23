//! Mastodon server-sent events. Each subscribed timeline has one worker; all
//! application state remains on the terminal thread.
use crate::api::{normalize_instance, Config, Conversation, Notification, Status};
use anyhow::{bail, Context, Result};
use reqwest::{blocking::Client, header};
use std::{
    io::{BufRead, BufReader, Read},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StreamKind {
    User,
    Local,
    Federated,
    Direct,
}
impl StreamKind {
    fn path(self) -> &'static str {
        match self {
            Self::User => "/api/v1/streaming/user",
            Self::Local => "/api/v1/streaming/public/local",
            Self::Federated => "/api/v1/streaming/public",
            Self::Direct => "/api/v1/streaming/direct",
        }
    }
}
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Update(Status),
    Edit(Status),
    Delete(String),
    Notification(Notification),
    Conversation(Conversation),
    RefreshNotifications,
}
#[derive(Debug)]
pub enum StreamMessage {
    Event(StreamKind, StreamEvent),
    Connected(StreamKind),
    Disconnected(StreamKind),
}

/// Start a connection loop; cancellation is shared by all workers for an account.
pub fn start(config: Config, kind: StreamKind, tx: Sender<StreamMessage>, cancel: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut delay = Duration::from_secs(2);
        while !cancel.load(Ordering::Relaxed) {
            let started = Instant::now();
            let result = connect(&config, kind, &tx, &cancel);
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if tx.send(StreamMessage::Disconnected(kind)).is_err() {
                break;
            }
            // A long-lived connection is healthy even if it ends by timeout.
            if started.elapsed() >= Duration::from_secs(30) {
                delay = Duration::from_secs(2);
            }
            if result.is_err() && delay < Duration::from_secs(60) {
                delay = (delay * 2).min(Duration::from_secs(60));
            }
            if !sleep_until_retry(delay, &cancel) {
                break;
            }
        }
    });
}

fn sleep_until_retry(delay: Duration, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + delay;
    while !cancel.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
    false
}

fn client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("LazyDon/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()?)
}

fn streaming_base(config: &Config, cancel: &AtomicBool) -> Result<String> {
    let instance = normalize_instance(&config.instance)?;
    let client = client(Duration::from_secs(15))?;
    // The authenticated instance supplies the streaming host. Never follow HTTP
    // redirects with a bearer token, and never allow an HTTPS-to-HTTP downgrade.
    let discover = |path: &str, pointer: &str| {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        client
            .get(format!("{instance}{path}"))
            .bearer_auth(&config.access_token)
            .send()
            .ok()
            .filter(|r| r.status().is_success())
            .and_then(|r| {
                let mut bytes = Vec::new();
                r.take(1024 * 1024).read_to_end(&mut bytes).ok()?;
                Some(bytes)
            })
            .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
            .and_then(|v| {
                v.pointer(pointer)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
    };
    let discovered = discover("/api/v2/instance", "/configuration/urls/streaming")
        .or_else(|| discover("/api/v1/instance", "/urls/streaming_api"));
    match discovered {
        Some(url) => validate_stream_base(&url, &instance),
        None => Ok(instance),
    }
}
fn validate_stream_base(value: &str, instance: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(value).context("invalid streaming URL")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        bail!("invalid streaming URL");
    }
    let scheme = match url.scheme() {
        "wss" | "https" => "https",
        "ws" | "http"
            if instance.starts_with("http://")
                && matches!(
                    url.host_str(),
                    Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
                ) =>
        {
            "http"
        }
        _ => bail!("insecure streaming URL"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("invalid streaming URL scheme"))?;
    // Streaming API paths are relative to the discovered host.
    url.set_path("");
    Ok(url.as_str().trim_end_matches('/').to_owned())
}
fn connect(
    config: &Config,
    kind: StreamKind,
    tx: &Sender<StreamMessage>,
    cancel: &AtomicBool,
) -> Result<()> {
    let base = streaming_base(config, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    if config.access_token.trim().is_empty() {
        bail!("empty access token");
    }
    // Timeout also bounds silent/broken connections. A reconnect refreshes the
    // timeline to fill any gap, regardless of how the connection ended.
    let response = client(Duration::from_secs(300))?
        .get(format!("{base}{}", kind.path()))
        .bearer_auth(&config.access_token)
        .header(header::ACCEPT, "text/event-stream")
        .send()?
        .error_for_status()?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    tx.send(StreamMessage::Connected(kind))
        .map_err(|_| anyhow::anyhow!("receiver closed"))?;
    read_events(BufReader::new(response), cancel, |event| {
        !cancel.load(Ordering::Relaxed) && tx.send(StreamMessage::Event(kind, event)).is_ok()
    })
}
fn parse_event(name: &str, data: &str) -> Option<StreamEvent> {
    match name {
        "update" => serde_json::from_str(data).ok().map(StreamEvent::Update),
        "status.update" => serde_json::from_str(data).ok().map(StreamEvent::Edit),
        "notification" => serde_json::from_str(data)
            .ok()
            .map(StreamEvent::Notification),
        "conversation" => serde_json::from_str(data)
            .ok()
            .map(StreamEvent::Conversation),
        "delete" if !data.is_empty() => Some(StreamEvent::Delete(data.to_owned())),
        "notifications_merged" => Some(StreamEvent::RefreshNotifications),
        _ => None,
    }
}
fn read_events(
    mut reader: impl BufRead,
    cancel: &AtomicBool,
    mut emit: impl FnMut(StreamEvent) -> bool,
) -> Result<()> {
    const LIMIT: u64 = 1024 * 1024;
    let mut name = String::new();
    let mut data = String::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut bytes = Vec::new();
        let n = reader
            .by_ref()
            .take(LIMIT + 1)
            .read_until(b'\n', &mut bytes)?;
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        if n == 0 {
            break;
        }
        if bytes.len() as u64 > LIMIT {
            bail!("stream event too large");
        }
        let line = std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if let Some(event) = parse_event(&name, &data) {
                if !emit(event) {
                    return Ok(());
                }
            }
            name.clear();
            data.clear();
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => {
                    name.clear();
                    name.push_str(value);
                }
                "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(value);
                    if data.len() as u64 > LIMIT {
                        bail!("stream event too large");
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CancelOnHeartbeat<'a> {
        input: std::io::Cursor<&'a [u8]>,
        cancel: &'a AtomicBool,
    }
    impl Read for CancelOnHeartbeat<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl BufRead for CancelOnHeartbeat<'_> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            self.input.fill_buf()
        }
        fn consume(&mut self, amt: usize) {
            self.input.consume(amt);
            self.cancel.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn cancellation_is_checked_after_heartbeat_before_next_event() {
        let input = b":thump\nevent: delete\ndata: 42\n\n";
        let cancel = AtomicBool::new(false);
        let mut reader = CancelOnHeartbeat {
            input: std::io::Cursor::new(input),
            cancel: &cancel,
        };
        let mut events = Vec::new();
        read_events(&mut reader, &cancel, |event| {
            events.push(event);
            true
        })
        .unwrap();
        assert!(events.is_empty());
        assert_eq!(reader.input.position(), b":thump\n".len() as u64);
    }

    #[test]
    fn cancellation_interrupts_reconnect_backoff() {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let start = Instant::now();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            sleep_until_retry(Duration::from_secs(60), &flag)
        });
        started_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(20));
        cancel.store(true, Ordering::Relaxed);
        assert!(!worker.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn parses_multiline_events_and_heartbeats() {
        let input = b":thump\r\nevent: update\r\ndata: {\"id\":\"42\",\r\ndata: \"content\":\"hello\"}\r\n\r\nevent: delete\ndata: 42\n\n";
        let mut events = vec![];
        read_events(&input[..], &AtomicBool::new(false), |event| {
            events.push(event);
            true
        })
        .unwrap();
        assert!(
            matches!(&events[0], StreamEvent::Update(s) if s.id == "42" && s.content == "hello")
        );
        assert!(matches!(&events[1], StreamEvent::Delete(id) if id == "42"));
    }
    #[test]
    fn parses_direct_conversation_and_nullable_status() {
        let event = parse_event(
            "conversation",
            r#"{"id":"conversation-1","unread":true,"last_status":null}"#,
        );
        assert!(
            matches!(event, Some(StreamEvent::Conversation(c)) if c.id == "conversation-1" && c.unread && c.last_status.is_none())
        );
    }
    #[test]
    fn validates_discovered_stream_host() {
        assert_eq!(
            validate_stream_base("wss://stream.example", "https://example.com").unwrap(),
            "https://stream.example"
        );
        assert!(validate_stream_base("ws://stream.example", "https://example.com").is_err());
        assert!(
            validate_stream_base("wss://user:password@stream.example", "https://example.com")
                .is_err()
        );
        assert_eq!(
            validate_stream_base("ws://localhost:3000", "http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
    }
}
