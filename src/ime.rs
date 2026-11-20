//! Best-effort return to direct input when leaving the composer.
//!
//! Fcitx's controller affects the currently focused input context. It cannot
//! control an IME running on another machine (for example over SSH).

#[cfg(target_os = "linux")]
use std::{
    ffi::OsStr,
    io::Read,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Cancel a pending switch when the composer is reopened.
pub fn cancel_pending() {
    #[cfg(target_os = "linux")]
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Ask the local IME to return to direct input without blocking the terminal.
/// Unsupported, unavailable, unresponsive, or ambiguous setups are ignored.
pub fn request_direct_input() {
    #[cfg(target_os = "linux")]
    {
        // A separately running fcitx daemon may not be the terminal's IME.
        // Explicit IBus configuration makes the controller choice ambiguous;
        // never change either daemon in that case.
        let ibus_selected = ["XMODIFIERS", "GTK_IM_MODULE", "QT_IM_MODULE"]
            .iter()
            .filter_map(std::env::var_os)
            .any(|value| {
                value
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains("ibus")
            });
        if ibus_selected {
            return;
        }
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let _ = thread::Builder::new()
            .name("lazydon-ime".into())
            .spawn(move || {
                reset_with(
                    OsStr::new("dbus-send"),
                    [
                        (OsStr::new("fcitx5-remote"), true),
                        (OsStr::new("fcitx-remote"), false),
                    ],
                    || GENERATION.load(Ordering::SeqCst) == generation,
                )
            });
    }
}

#[cfg(target_os = "linux")]
fn reset_with<'a>(
    dbus: &OsStr,
    remotes: impl IntoIterator<Item = (&'a OsStr, bool)>,
    still_valid: impl Fn() -> bool,
) {
    for (remote, is_fcitx5) in remotes {
        if !still_valid() {
            return;
        }
        // Check the bus daemon, not Fcitx, so an absent Fcitx5 is not activated.
        // Fcitx 4 instead connects directly to an existing Unix socket.
        if is_fcitx5 && !fcitx5_has_owner(dbus) {
            continue;
        }
        match run(remote, &[]).as_deref() {
            // Connected, already direct input: do not affect another daemon.
            Some("1") => return,
            // Connected, active: deactivate it. Even if it fails, do not
            // attempt to alter a different running input method framework.
            Some("2") => {
                if (!is_fcitx5 || fcitx5_has_owner(dbus)) && still_valid() {
                    let _ = run(remote, &["-c"]);
                }
                return;
            }
            // 0, invalid output, nonzero exit, timeout, or unavailable binary:
            // try the next compatible controller, then quietly do nothing.
            _ => {}
        }
    }
}

// NameHasOwner asks the session bus itself and does not activate the target.
#[cfg(target_os = "linux")]
fn fcitx5_has_owner(dbus: &OsStr) -> bool {
    run(
        dbus,
        &[
            "--session",
            "--print-reply=literal",
            "--dest=org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.NameHasOwner",
            "string:org.fcitx.Fcitx5",
        ],
    )
    .is_some_and(|output| output.lines().any(|line| line.trim() == "boolean true"))
}

/// Bound subprocess runtime and output; neither errors nor stderr reach the TUI.
#[cfg(target_os = "linux")]
fn run(program: &OsStr, args: &[&str]) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut text = String::new();
                child
                    .stdout
                    .take()?
                    .take(128)
                    .read_to_string(&mut text)
                    .ok()?;
                return Some(text.trim().to_owned());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Mock {
        dir: PathBuf,
    }
    impl Mock {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "lazydon-ime-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self { dir }
        }
        fn remote(&self, name: &str, state: &str, exit: u8) -> PathBuf {
            let path = self.dir.join(name);
            let log = self.dir.join("log");
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = '-c' ] || [ \"$2\" = '-c' ]; then printf '%s\\n' '{name}' >> '{}'; exit 0; fi\nprintf '%s\\n' '{state}'\nexit {exit}\n", log.display()
            );
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
        fn bus(&self, owned: bool) -> PathBuf {
            let path = self.dir.join("dbus-send");
            fs::write(&path, format!("#!/bin/sh\nprintf 'boolean {}\\n'\n", owned)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
        fn log(&self) -> String {
            fs::read_to_string(self.dir.join("log")).unwrap_or_default()
        }
    }
    impl Drop for Mock {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn active_fcitx_is_deactivated_once_without_touching_the_other_daemon() {
        let mock = Mock::new();
        let a = mock.remote("fcitx5", "2", 0);
        let b = mock.remote("fcitx4", "2", 0);
        reset_with(
            mock.bus(true).as_os_str(),
            [(a.as_os_str(), true), (b.as_os_str(), false)],
            || true,
        );
        assert_eq!(mock.log(), "fcitx5\n");
    }

    #[test]
    fn connected_inactive_fcitx_is_left_unchanged() {
        let mock = Mock::new();
        let a = mock.remote("fcitx5", "1", 0);
        let b = mock.remote("fcitx4", "2", 0);
        reset_with(
            mock.bus(true).as_os_str(),
            [(a.as_os_str(), true), (b.as_os_str(), false)],
            || true,
        );
        assert_eq!(mock.log(), "");
    }

    #[test]
    fn disconnected_or_broken_fcitx5_uses_a_connected_fcitx4() {
        let mock = Mock::new();
        let a = mock.remote("fcitx5", "0", 0);
        let b = mock.remote("fcitx4", "2", 0);
        reset_with(
            mock.bus(true).as_os_str(),
            [(a.as_os_str(), true), (b.as_os_str(), false)],
            || true,
        );
        assert_eq!(mock.log(), "fcitx4\n");
        fs::remove_file(mock.dir.join("log")).unwrap();
        let a = mock.remote("fcitx5", "2", 1);
        reset_with(
            mock.bus(true).as_os_str(),
            [(a.as_os_str(), true), (b.as_os_str(), false)],
            || true,
        );
        assert_eq!(mock.log(), "fcitx4\n");
    }

    #[test]
    fn invalid_or_missing_controllers_do_not_change_anything() {
        let mock = Mock::new();
        let invalid = mock.remote("invalid", "2 extra", 0);
        let absent = mock.dir.join("absent");
        reset_with(
            mock.bus(true).as_os_str(),
            [(invalid.as_os_str(), true), (absent.as_os_str(), false)],
            || true,
        );
        assert_eq!(mock.log(), "");
    }
    #[test]
    fn missing_or_unowned_dbus_never_launches_fcitx5() {
        let mock = Mock::new();
        let a = mock.remote("fcitx5", "2", 0);
        reset_with(mock.bus(false).as_os_str(), [(a.as_os_str(), true)], || {
            true
        });
        assert_eq!(mock.log(), "");
        reset_with(
            mock.dir.join("missing-bus").as_os_str(),
            [(a.as_os_str(), true)],
            || true,
        );
        assert_eq!(mock.log(), "");
    }

    #[test]
    fn cancelled_request_does_not_switch_input_method() {
        let mock = Mock::new();
        let a = mock.remote("fcitx5", "2", 0);
        let calls = std::cell::Cell::new(0);
        reset_with(mock.bus(true).as_os_str(), [(a.as_os_str(), true)], || {
            calls.set(calls.get() + 1);
            calls.get() == 1
        });
        assert_eq!(calls.get(), 2);
        assert_eq!(mock.log(), "");
    }
    #[test]
    fn unresponsive_bus_is_timed_out_without_changing_ime() {
        let mock = Mock::new();
        let bus = mock.dir.join("slow-bus");
        fs::write(&bus, "#!/bin/sh\nexec /bin/sleep 4\n").unwrap();
        fs::set_permissions(&bus, fs::Permissions::from_mode(0o700)).unwrap();
        let remote = mock.remote("fcitx5", "2", 0);
        let started = Instant::now();
        reset_with(bus.as_os_str(), [(remote.as_os_str(), true)], || true);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(mock.log(), "");
    }
}
