//! Mastodon server-sent events. Each subscribed timeline has one worker; all
//! application state remains on the terminal thread.
use crate::api::{normalize_instance, Config, Notification, Status};
use anyhow::{bail, Context, Result};
use reqwest::{blocking::Client, header};
use std::{
    io::{BufRead, BufReader, Read},
    sync::mpsc::Sender,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StreamKind {
    User,
    Local,
    Federated,
}
impl StreamKind {
    fn path(self) -> &'static str {
        match self {
            Self::User => "/api/v1/streaming/user",
            Self::Local => "/api/v1/streaming/public/local",
            Self::Federated => "/api/v1/streaming/public",
        }
    }
}
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Update(Status),
    Edit(Status),
    Delete(String),
    Notification(Notification),
    RefreshNotifications,
}
#[derive(Debug)]
pub enum StreamMessage {
    Event(StreamKind, StreamEvent),
    Connected(StreamKind),
    Disconnected(StreamKind),
}

/// Start a single connection loop. The receiver disappearing stops the worker
/// after its current bounded request; the process can exit without joining it.
pub fn start(config: Config, kind: StreamKind, tx: Sender<StreamMessage>) {
    thread::spawn(move || {
        let mut delay = Duration::from_secs(2);
        loop {
            let started = Instant::now();
            let result = connect(&config, kind, &tx);
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
            thread::sleep(delay);
        }
    });
}

fn client(timeout: Duration) -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("LazyDon/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout)
        .build()?)
}

fn streaming_base(config: &Config) -> Result<String> {
    let instance = normalize_instance(&config.instance)?;
    let client = client(Duration::from_secs(15))?;
    // The authenticated instance supplies the streaming host. Never follow HTTP
    // redirects with a bearer token, and never allow an HTTPS-to-HTTP downgrade.
    let discover = |path: &str, pointer: &str| {
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
fn connect(config: &Config, kind: StreamKind, tx: &Sender<StreamMessage>) -> Result<()> {
    let base = streaming_base(config)?;
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
    tx.send(StreamMessage::Connected(kind))
        .map_err(|_| anyhow::anyhow!("receiver closed"))?;
    read_events(BufReader::new(response), |event| {
        tx.send(StreamMessage::Event(kind, event)).is_ok()
    })
}
fn parse_event(name: &str, data: &str) -> Option<StreamEvent> {
    match name {
        "update" => serde_json::from_str(data).ok().map(StreamEvent::Update),
        "status.update" => serde_json::from_str(data).ok().map(StreamEvent::Edit),
        "notification" => serde_json::from_str(data)
            .ok()
            .map(StreamEvent::Notification),
        "delete" if !data.is_empty() => Some(StreamEvent::Delete(data.to_owned())),
        "notifications_merged" => Some(StreamEvent::RefreshNotifications),
        _ => None,
    }
}
fn read_events(mut reader: impl BufRead, mut emit: impl FnMut(StreamEvent) -> bool) -> Result<()> {
    const LIMIT: u64 = 1024 * 1024;
    let mut name = String::new();
    let mut data = String::new();
    loop {
        let mut bytes = Vec::new();
        let n = reader
            .by_ref()
            .take(LIMIT + 1)
            .read_until(b'\n', &mut bytes)?;
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
    #[test]
    fn parses_multiline_events_and_heartbeats() {
        let input = b":thump\r\nevent: update\r\ndata: {\"id\":\"42\",\r\ndata: \"content\":\"hello\"}\r\n\r\nevent: delete\ndata: 42\n\n";
        let mut events = vec![];
        read_events(&input[..], |event| {
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
