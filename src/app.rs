use crate::api::{Config, Mastodon, Notification, Status, Timeline};
use crate::media;
use crate::stream::{self, StreamEvent, StreamKind, StreamMessage};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use std::{
    collections::HashMap,
    sync::mpsc::{self, Sender},
    thread,
    time::Duration,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Compose,
    AddTab,
    Help,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Home,
    Notifications,
    Local,
    Federated,
}
pub struct Tab {
    pub title: String,
    pub items: Vec<ViewItem>,
    pub selected: usize,
    kind: Kind,
}
#[derive(Clone)]
pub struct ViewItem {
    pub id: String,
    key: String,
    pub author: String,
    pub acct: String,
    pub content: String,
    pub timestamp: String,
    pub context: String,
    pub media: Vec<String>,
    favourite: bool,
    boosted: bool,
}
pub struct App {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub status: String,
    pub mode: Mode,
    pub input: String,
    pub preview: media::Preview,
    pub instance: String,
    pub account: String,
    config: Config,
    tx: Sender<Message>,
    stream_tx: Sender<StreamMessage>,
    pending: HashMap<Kind, (u64, Vec<StreamEvent>)>,
    request_id: u64,
    preview_url: Option<String>,
    attachments: Vec<Vec<u8>>,
    busy: bool,
}
enum Message {
    Loaded(Kind, u64, Result<Vec<ViewItem>, String>),
    Stream(StreamMessage),
    Preview(String, Result<media::Preview, String>),
    Clipboard(Result<Vec<u8>, String>),
    Posted(Result<(), String>),
    Action(Result<(), String>),
}
impl Tab {
    fn new(kind: Kind) -> Self {
        let title = match kind {
            Kind::Home => "Home",
            Kind::Notifications => "Notifications",
            Kind::Local => "Local",
            Kind::Federated => "Federated",
        }
        .into();
        Self {
            title,
            items: Vec::new(),
            selected: 0,
            kind,
        }
    }
}
fn plain(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 100)
        .unwrap_or_else(|_| html.to_owned())
        .trim()
        .to_owned()
}
fn view(status: Status, context: String) -> ViewItem {
    let key = status.id.clone();
    let (status, context) = if let Some(ref original) = status.reblog {
        (
            (**original).clone(),
            format!("{} boosted · {}", status.account.acct, context),
        )
    } else {
        (status, context)
    };
    let mut content = plain(&status.content);
    if !status.spoiler_text.is_empty() {
        content = format!("CW: {}\n{}", status.spoiler_text, content);
    }
    let acct = status.account.acct.clone();
    ViewItem {
        id: status.id,
        key,
        author: if status.account.display_name.is_empty() {
            acct.clone()
        } else {
            status.account.display_name
        },
        acct,
        content,
        timestamp: status
            .created_at
            .replace('T', " ")
            .trim_end_matches('Z')
            .to_owned(),
        context: format!(
            "{}{} ♥{} ↻{}",
            context,
            if status.in_reply_to_id.is_some() {
                " ↳ reply"
            } else {
                ""
            },
            status.favourites_count,
            status.reblogs_count
        )
        .trim()
        .to_owned(),
        media: status
            .media_attachments
            .into_iter()
            .filter(|m| m.kind == "image")
            .filter_map(|m| m.preview_url.or(m.url))
            .collect(),
        favourite: status.favourited.unwrap_or(false),
        boosted: status.reblogged.unwrap_or(false),
    }
}
fn notification(n: Notification) -> ViewItem {
    let label = format!("{}: @{}", n.kind, n.account.acct);
    if let Some(s) = n.status {
        let mut item = view(s, label);
        item.key = format!("notification:{}", n.id);
        item
    } else {
        ViewItem {
            id: String::new(),
            key: format!("notification:{}", n.id),
            author: if n.account.display_name.is_empty() {
                n.account.acct.clone()
            } else {
                n.account.display_name
            },
            acct: n.account.acct,
            content: label,
            timestamp: n.created_at,
            context: "notification".into(),
            media: vec![],
            favourite: false,
            boosted: false,
        }
    }
}
// Keep the selected post stable as live items arrive and bound memory use.
fn apply_event(tab: &mut Tab, event: &StreamEvent) {
    match event {
        StreamEvent::Update(status) if tab.kind != Kind::Notifications => {
            let item = view(status.clone(), String::new());
            if item.key.is_empty() || tab.items.iter().any(|existing| existing.key == item.key) {
                return;
            }
            if !tab.items.is_empty() {
                tab.selected += 1;
            }
            tab.items.insert(0, item);
            tab.items.truncate(200);
            tab.selected = tab.selected.min(tab.items.len().saturating_sub(1));
        }
        StreamEvent::Notification(n) if tab.kind == Kind::Notifications => {
            let item = notification(n.clone());
            if n.id.is_empty() || tab.items.iter().any(|existing| existing.key == item.key) {
                return;
            }
            if !tab.items.is_empty() {
                tab.selected += 1;
            }
            tab.items.insert(0, item);
            tab.items.truncate(200);
            tab.selected = tab.selected.min(tab.items.len().saturating_sub(1));
        }
        StreamEvent::Delete(id) => {
            let before = tab
                .items
                .iter()
                .take(tab.selected)
                .filter(|i| &i.id == id || &i.key == id)
                .count();
            tab.items.retain(|i| &i.id != id && &i.key != id);
            tab.selected = tab
                .selected
                .saturating_sub(before)
                .min(tab.items.len().saturating_sub(1));
        }
        StreamEvent::Edit(status) => {
            let updated = view(status.clone(), String::new());
            for existing in &mut tab.items {
                if existing.id == updated.id && !updated.id.is_empty() {
                    existing.author = updated.author.clone();
                    existing.acct = updated.acct.clone();
                    existing.content = updated.content.clone();
                    existing.media = updated.media.clone();
                    existing.favourite = updated.favourite;
                    existing.boosted = updated.boosted;
                }
            }
        }
        _ => {}
    }
}
fn event_applies(kind: Kind, stream: StreamKind, event: &StreamEvent) -> bool {
    match event {
        StreamEvent::Update(_) => matches!(
            (kind, stream),
            (Kind::Home, StreamKind::User)
                | (Kind::Local, StreamKind::Local)
                | (Kind::Federated, StreamKind::Federated)
        ),
        StreamEvent::Notification(_) => kind == Kind::Notifications && stream == StreamKind::User,
        StreamEvent::Delete(_) | StreamEvent::Edit(_) => true,
        StreamEvent::RefreshNotifications => false,
    }
}
impl App {
    fn new(config: Config, account: String) -> (Self, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel();
        let (stream_tx, stream_rx) = mpsc::channel();
        let app_tx = tx.clone();
        thread::spawn(move || {
            while let Ok(message) = stream_rx.recv() {
                if app_tx.send(Message::Stream(message)).is_err() {
                    break;
                }
            }
        });
        (
            Self {
                tabs: vec![Tab::new(Kind::Home), Tab::new(Kind::Notifications)],
                active: 0,
                status: "Loading…".into(),
                mode: Mode::Normal,
                input: String::new(),
                preview: vec![],
                instance: config.instance.clone(),
                account,
                config,
                tx,
                stream_tx,
                pending: HashMap::new(),
                request_id: 0,
                preview_url: None,
                attachments: vec![],
                busy: false,
            },
            rx,
        )
    }
    pub fn attachment_count(&self) -> usize {
        self.attachments.len()
    }
    fn refresh(&mut self, kind: Kind) {
        self.request_id += 1;
        let request_id = self.request_id;
        self.pending.insert(kind, (request_id, Vec::new()));
        self.status = "Loading timeline…".into();
        let tx = self.tx.clone();
        let config = self.config.clone();
        thread::spawn(move || {
            let result = (|| -> Result<Vec<ViewItem>> {
                let client = Mastodon::new(config)?;
                Ok(match kind {
                    Kind::Notifications => client
                        .notifications()?
                        .into_iter()
                        .map(notification)
                        .collect(),
                    other => client
                        .timeline(match other {
                            Kind::Home => Timeline::Home,
                            Kind::Local => Timeline::Local,
                            Kind::Federated => Timeline::Federated,
                            _ => unreachable!(),
                        })?
                        .into_iter()
                        .map(|s| view(s, String::new()))
                        .collect(),
                })
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Loaded(kind, request_id, result));
        });
    }
    fn handle_stream(&mut self, message: StreamMessage) {
        match message {
            StreamMessage::Connected(kind) => {
                // A new connection cannot replay the missed interval.
                match kind {
                    StreamKind::User => {
                        self.refresh(Kind::Home);
                        self.refresh(Kind::Notifications);
                    }
                    StreamKind::Local => self.refresh(Kind::Local),
                    StreamKind::Federated => self.refresh(Kind::Federated),
                }
            }
            StreamMessage::Disconnected(StreamKind::User) => {
                self.status = "Stream disconnected; reconnecting…".into()
            }
            StreamMessage::Disconnected(_) => {}
            StreamMessage::Event(kind, StreamEvent::RefreshNotifications)
                if kind == StreamKind::User =>
            {
                self.refresh(Kind::Notifications)
            }
            StreamMessage::Event(kind, event) => {
                for tab in &mut self.tabs {
                    if event_applies(tab.kind, kind, &event) {
                        if let Some((_, events)) = self.pending.get_mut(&tab.kind) {
                            events.push(event.clone());
                        }
                        apply_event(tab, &event);
                    }
                }
                self.update_preview();
            }
        }
    }
    fn update_preview(&mut self) {
        let url = self
            .tabs
            .get(self.active)
            .and_then(|t| t.items.get(t.selected))
            .and_then(|i| i.media.first())
            .cloned();
        if url == self.preview_url {
            return;
        }
        self.preview_url = url.clone();
        self.preview.clear();
        if let Some(url) = url {
            let tx = self.tx.clone();
            thread::spawn(move || {
                let result = media::preview(&url).map_err(|e| format!("{e:#}"));
                let _ = tx.send(Message::Preview(url, result));
            });
        }
    }
    fn selected(&self) -> Option<&ViewItem> {
        self.tabs
            .get(self.active)?
            .items
            .get(self.tabs.get(self.active)?.selected)
    }
    fn action(&mut self, boost: bool) {
        let Some(item) = self.selected() else {
            return;
        };
        if item.id.is_empty() {
            self.status = "This notification has no post.".into();
            return;
        }
        let action = match (boost, item.boosted, item.favourite) {
            (true, true, _) => "unreblog",
            (true, false, _) => "reblog",
            (false, _, true) => "unfavourite",
            _ => "favourite",
        };
        let id = item.id.clone();
        let config = self.config.clone();
        let tx = self.tx.clone();
        self.status = format!("{action}…");
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|c| c.action(&id, action))
                .map(|_| ())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Action(result));
        });
    }
    fn handle(&mut self, message: Message) {
        match message {
            Message::Loaded(kind, request_id, Ok(items)) => {
                if self.pending.get(&kind).map(|p| p.0) != Some(request_id) {
                    return;
                }
                let (_, events) = self.pending.remove(&kind).unwrap();
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.kind == kind) {
                    let selected = tab.items.get(tab.selected).map(|item| item.key.clone());
                    tab.items = items;
                    tab.selected = tab.selected.min(tab.items.len().saturating_sub(1));
                    for event in &events {
                        apply_event(tab, event);
                    }
                    if let Some(index) =
                        selected.and_then(|key| tab.items.iter().position(|item| item.key == key))
                    {
                        tab.selected = index;
                    }
                }
                if self.status != "Posted." {
                    self.status = "Updated.".into();
                }
                self.update_preview();
            }
            Message::Loaded(kind, request_id, Err(e)) => {
                if self.pending.get(&kind).map(|p| p.0) == Some(request_id) {
                    self.pending.remove(&kind);
                    self.status = format!("Load failed: {e}");
                }
            }
            Message::Stream(message) => self.handle_stream(message),
            Message::Preview(url, result) if self.preview_url.as_ref() == Some(&url) => {
                self.preview = result.unwrap_or_else(|e| {
                    vec![
                        Line::raw(format!("Preview unavailable: {e}")),
                        Line::raw(url),
                    ]
                })
            }
            Message::Preview(_, _) => {}
            Message::Clipboard(result) => {
                self.busy = false;
                match result {
                    Ok(data) => {
                        self.attachments.push(data);
                        self.status = format!("{} image(s) attached.", self.attachments.len());
                    }
                    Err(e) => self.status = format!("Clipboard: {e}"),
                }
            }
            Message::Posted(result) => {
                self.busy = false;
                match result {
                    Ok(()) => {
                        self.input.clear();
                        self.attachments.clear();
                        self.mode = Mode::Normal;
                        self.refresh(Kind::Home);
                        self.status = "Posted.".into();
                    }
                    Err(e) => self.status = format!("Post failed (draft retained): {e}"),
                }
            }
            Message::Action(result) => match result {
                Ok(()) => {
                    let kind = self.tabs[self.active].kind;
                    self.refresh(kind);
                }
                Err(e) => self.status = format!("Action failed: {e}"),
            },
        }
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        match self.mode {
            Mode::Normal => match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('a') => self.mode = Mode::AddTab,
                KeyCode::Char('c') => {
                    self.mode = Mode::Compose;
                    self.status = format!("{} image(s) attached.", self.attachments.len());
                }
                KeyCode::Char('r') => self.refresh(self.tabs[self.active].kind),
                KeyCode::Char('f') => self.action(false),
                KeyCode::Char('b') => self.action(true),
                KeyCode::Char('j') | KeyCode::Down => {
                    let t = &mut self.tabs[self.active];
                    t.selected = (t.selected + 1).min(t.items.len().saturating_sub(1));
                    self.update_preview();
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    let t = &mut self.tabs[self.active];
                    t.selected = t.selected.saturating_sub(1);
                    self.update_preview();
                }
                KeyCode::Char('g') => {
                    self.tabs[self.active].selected = 0;
                    self.update_preview();
                }
                KeyCode::Char('G') => {
                    let t = &mut self.tabs[self.active];
                    t.selected = t.items.len().saturating_sub(1);
                    self.update_preview();
                }
                KeyCode::Char('h') | KeyCode::Left => {
                    self.active = self.active.saturating_sub(1);
                    self.update_preview();
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    self.active = (self.active + 1).min(self.tabs.len() - 1);
                    self.update_preview();
                }
                _ => {}
            },
            Mode::Help => {
                self.mode = Mode::Normal;
            }
            Mode::AddTab => {
                let kind = match key.code {
                    KeyCode::Char('l') => Some(Kind::Local),
                    KeyCode::Char('f') => Some(Kind::Federated),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if let Some(i) = self.tabs.iter().position(|t| t.kind == kind) {
                        self.active = i;
                    } else {
                        self.tabs.push(Tab::new(kind));
                        let stream_kind = if kind == Kind::Local {
                            StreamKind::Local
                        } else {
                            StreamKind::Federated
                        };
                        stream::start(self.config.clone(), stream_kind, self.stream_tx.clone());
                        self.active = self.tabs.len() - 1;
                        self.refresh(kind);
                    }
                    self.mode = Mode::Normal;
                    self.update_preview();
                } else if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Compose => {
                if key.code == KeyCode::Esc && !self.busy {
                    self.mode = Mode::Normal;
                    self.status = "Draft retained. Press c to resume.".into();
                } else if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('v')
                    && !self.busy
                {
                    if self.attachments.len() >= 4 {
                        self.status = "Maximum 4 images.".into();
                    } else {
                        self.busy = true;
                        self.status = "Reading clipboard…".into();
                        let tx = self.tx.clone();
                        thread::spawn(move || {
                            let _ = tx.send(Message::Clipboard(
                                media::clipboard_png().map_err(|e| format!("{e:#}")),
                            ));
                        });
                    }
                } else if key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('s')
                    && !self.busy
                {
                    if self.input.trim().is_empty() && self.attachments.is_empty() {
                        self.status = "Write a post or attach an image first.".into();
                    } else {
                        self.busy = true;
                        self.status = "Uploading and posting…".into();
                        let tx = self.tx.clone();
                        let config = self.config.clone();
                        let text = self.input.clone();
                        let attachments = self.attachments.clone();
                        thread::spawn(move || {
                            let result = (|| -> Result<()> {
                                let client = Mastodon::new(config)?;
                                let mut ids = Vec::new();
                                for bytes in attachments {
                                    ids.push(client.upload_media(bytes, "clipboard.png")?.id);
                                }
                                client.post(&text, &ids, None)?;
                                Ok(())
                            })()
                            .map_err(|e| format!("{e:#}"));
                            let _ = tx.send(Message::Posted(result));
                        });
                    }
                } else if !self.busy {
                    match key.code {
                        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                            self.input.push(c)
                        }
                        KeyCode::Enter => self.input.push('\n'),
                        KeyCode::Backspace => {
                            self.input.pop();
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }
}
pub fn run(terminal: &mut ratatui::DefaultTerminal, config: Config, account: String) -> Result<()> {
    let (mut app, rx) = App::new(config, account);
    app.refresh(Kind::Home);
    app.refresh(Kind::Notifications);
    stream::start(app.config.clone(), StreamKind::User, app.stream_tx.clone());
    loop {
        for _ in 0..100 {
            match rx.try_recv() {
                Ok(msg) => app.handle(msg),
                Err(_) => break,
            }
        }
        terminal.draw(|f| crate::ui::draw(f, &app))?;
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if app.key(key) {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_app() -> App {
    let config = Config {
        instance: "https://example.com".into(),
        access_token: "test".into(),
    };
    let (mut app, _) = App::new(config, "@me".into());
    app.tabs[0].items.push(ViewItem {
        id: "1".into(),
        key: "1".into(),
        author: "Alice".into(),
        acct: "alice@example.com".into(),
        content: "Hello from Alice".into(),
        timestamp: "2026-01-01".into(),
        context: String::new(),
        media: vec!["https://example.com/image.png".into()],
        favourite: false,
        boosted: false,
    });
    app.preview = vec![Line::raw("[preview]")];
    app
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_tabs_and_draft_retention() {
        let mut app = test_app();
        assert_eq!(
            app.tabs
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            ["Home", "Notifications"]
        );
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('あ'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.input, "あ");
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(matches!(app.mode, Mode::Compose));
        assert_eq!(app.input, "あ");
    }
    #[test]
    fn boost_displays_original_author_and_id() {
        let original = Status {
            id: "123".into(),
            account: api_account("alice"),
            content: "<p>Hello &amp; hi</p>".into(),
            ..Default::default()
        };
        let boosted = Status {
            id: "456".into(),
            account: api_account("bob"),
            reblog: Some(Box::new(original)),
            ..Default::default()
        };
        let item = view(boosted, String::new());
        assert_eq!(item.id, "123");
        assert_eq!(item.acct, "alice");
        assert!(item.context.contains("bob boosted"));
        assert!(item.content.contains("Hello & hi"));
    }
    #[test]
    fn live_updates_preserve_selection_deduplicate_and_delete() {
        let mut tab = Tab::new(Kind::Home);
        let first = Status {
            id: "1".into(),
            account: api_account("alice"),
            ..Default::default()
        };
        let next = Status {
            id: "2".into(),
            account: api_account("bob"),
            ..Default::default()
        };
        apply_event(&mut tab, &StreamEvent::Update(first));
        apply_event(&mut tab, &StreamEvent::Update(next.clone()));
        apply_event(&mut tab, &StreamEvent::Update(next));
        assert_eq!(tab.items.len(), 2);
        assert_eq!(tab.items[tab.selected].id, "1");
        apply_event(&mut tab, &StreamEvent::Delete("2".into()));
        assert_eq!(tab.items[tab.selected].id, "1");
        assert_eq!(tab.selected, 0);
    }
    #[test]
    fn notifications_have_distinct_identity_even_for_same_post() {
        let mut tab = Tab::new(Kind::Notifications);
        let n = Notification {
            id: "a".into(),
            status: Some(Status {
                id: "post".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_event(&mut tab, &StreamEvent::Notification(n.clone()));
        apply_event(&mut tab, &StreamEvent::Notification(n.clone()));
        apply_event(
            &mut tab,
            &StreamEvent::Notification(Notification {
                id: "b".into(),
                ..n
            }),
        );
        assert_eq!(tab.items.len(), 2);
        apply_event(&mut tab, &StreamEvent::Delete("post".into()));
        assert!(tab.items.is_empty());
    }
    #[test]
    fn events_received_during_load_are_replayed_over_snapshot() {
        let mut app = test_app();
        app.tabs[0].items.clear();
        app.pending.insert(Kind::Home, (1, Vec::new()));
        app.handle_stream(StreamMessage::Event(
            StreamKind::User,
            StreamEvent::Update(Status {
                id: "new".into(),
                account: api_account("alice"),
                ..Default::default()
            }),
        ));
        app.handle(Message::Loaded(
            Kind::Home,
            1,
            Ok(vec![view(
                Status {
                    id: "old".into(),
                    account: api_account("bob"),
                    ..Default::default()
                },
                String::new(),
            )]),
        ));
        assert_eq!(
            app.tabs[0]
                .items
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
    }
    fn api_account(name: &str) -> crate::api::Account {
        crate::api::Account {
            acct: name.into(),
            ..Default::default()
        }
    }
}
