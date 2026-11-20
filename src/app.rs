use crate::api::{Config, Mastodon, Notification, Status, Timeline};
use crate::media;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
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
#[derive(Clone, Copy, PartialEq, Eq)]
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
    pub preview: Vec<String>,
    pub instance: String,
    pub account: String,
    config: Config,
    tx: Sender<Message>,
    preview_url: Option<String>,
    attachments: Vec<Vec<u8>>,
    busy: bool,
}
enum Message {
    Loaded(Kind, Result<Vec<ViewItem>, String>),
    Preview(String, Result<Vec<String>, String>),
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
        view(s, label)
    } else {
        ViewItem {
            id: String::new(),
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
impl App {
    fn new(config: Config, account: String) -> (Self, mpsc::Receiver<Message>) {
        let (tx, rx) = mpsc::channel();
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
                preview_url: None,
                attachments: vec![],
                busy: false,
            },
            rx,
        )
    }
    fn refresh(&mut self, kind: Kind) {
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
            let _ = tx.send(Message::Loaded(kind, result));
        });
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
            Message::Loaded(kind, Ok(items)) => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.kind == kind) {
                    tab.items = items;
                    tab.selected = tab.selected.min(tab.items.len().saturating_sub(1));
                }
                if self.status != "Posted." {
                    self.status = "Updated.".into();
                }
                self.update_preview();
            }
            Message::Loaded(_, Err(e)) => self.status = format!("Load failed: {e}"),
            Message::Preview(url, result) if self.preview_url.as_ref() == Some(&url) => {
                self.preview =
                    result.unwrap_or_else(|e| vec![format!("Preview unavailable: {e}"), url])
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
    loop {
        while let Ok(msg) = rx.try_recv() {
            app.handle(msg);
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
        author: "Alice".into(),
        acct: "alice@example.com".into(),
        content: "Hello from Alice".into(),
        timestamp: "2026-01-01".into(),
        context: String::new(),
        media: vec!["https://example.com/image.png".into()],
        favourite: false,
        boosted: false,
    });
    app.preview = vec!["[preview]".into()];
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
    fn api_account(name: &str) -> crate::api::Account {
        crate::api::Account {
            acct: name.into(),
            ..Default::default()
        }
    }
}
