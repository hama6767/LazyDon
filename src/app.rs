use crate::api::{self, Account, Config, Mastodon, Notification, Status, Timeline};
use crate::stream::{self, StreamEvent, StreamKind, StreamMessage};
use crate::{accounts, links, media, settings};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Compose,
    AddTab,
    Help,
    Detail,
    Settings,
    AccountManage,
    AccountAdd,
    AccountDeleteConfirm,
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
#[derive(Clone, Debug)]
pub struct Attachment {
    pub kind: String,
    pub url: String,
    pub description: String,
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
    pub attachments: Vec<Attachment>,
    pub links: Vec<String>,
    pub post_url: Option<String>,
    pub details: String,
    pub link_preview: Option<String>,
    favourite: bool,
    boosted: bool,
}
pub struct App {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub status: String,
    pub mode: Mode,
    pub input: String,
    pub input_cursor: usize,
    pub settings: settings::Settings,
    pub settings_index: usize,
    pub settings_message: String,
    pub font_candidate: Option<u8>,
    pub detail_index: usize,
    pub detail_scroll: u16,
    pub native_preview: Option<media::NativePreview>,
    picker: ratatui_image::picker::Picker,
    pub instance: String,
    pub account: String,
    pub account_labels: Vec<String>,
    pub active_account: usize,
    pub settings_account_index: usize,
    pub account_input: String,
    pub account_add_step: u8,
    pub auth_url: String,
    pub account_busy: bool,
    pub favourite_effect: Option<(String, Instant)>,
    auth: Option<api::Authorization>,
    auth_attempt: u64,
    pending_actions: VecDeque<WorkspaceAction>,
    g_pending: Option<Instant>,
    config: Config,
    tx: Sender<Message>,
    stream_tx: Sender<StreamMessage>,
    stop_streams: Arc<AtomicBool>,
    pending: HashMap<Kind, (u64, Vec<StreamEvent>)>,
    pending_interactions: HashSet<(String, bool)>,
    request_id: u64,
    preview_url: Option<String>,
    attachments: Vec<Vec<u8>>,
    busy: bool,
}
enum WorkspaceAction {
    SwitchRelative(i8),
    Switch(usize),
    Delete(usize),
    Add(Config, Account),
    Upgrade(Account),
}
enum Message {
    Loaded(Kind, u64, Result<Vec<ViewItem>, String>),
    Stream(StreamMessage),
    Clipboard(Result<Vec<u8>, String>),
    Posted(Result<(), String>),
    Action(String, bool, bool, Result<Status, String>),
    AuthRegistered(u64, Result<api::Authorization, String>),
    AuthVerified(u64, Result<(Config, Account), String>),
    Identity(Result<Account, String>),
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
    let mut content = crate::formatting::plain(&status.content);
    if !status.spoiler_text.is_empty() {
        content = format!("CW: {}\n{}", status.spoiler_text, content);
    }
    let acct = status.account.acct.clone();
    let mut urls = links::extract(&status.content);
    let post_url = status.url.clone().filter(|url| links::valid_url(url));
    let link_preview = status
        .card
        .as_ref()
        .map(|card| {
            let title = links::display_text(&card.title, 140);
            let description = links::display_text(&card.description, 200);
            let provider = links::display_text(&card.provider_name, 80);
            [title, description, provider]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ")
        })
        .filter(|s| !s.is_empty());
    if let Some(url) = status
        .card
        .as_ref()
        .map(|c| &c.url)
        .filter(|url| links::valid_url(url))
    {
        if !urls.contains(url) {
            urls.push(url.clone());
        }
    }
    if let Some(url) = &post_url {
        if !urls.contains(url) {
            urls.push(url.clone());
        }
    }
    let attachments = status
        .media_attachments
        .iter()
        .filter_map(|m| {
            m.url
                .as_ref()
                .filter(|url| links::valid_url(url))
                .map(|url| Attachment {
                    kind: m.kind.clone(),
                    url: url.clone(),
                    description: m.description.clone().unwrap_or_default(),
                })
        })
        .collect();
    let details = format!(
        "{} · {} replies · {} boosts · {} favourites{}",
        status.visibility,
        status.replies_count,
        status.reblogs_count,
        status.favourites_count,
        if status.sensitive {
            " · sensitive"
        } else {
            ""
        }
    );
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
            .filter_map(|m| match m.kind.as_str() {
                "image" => m.url.or(m.preview_url),
                "video" | "gifv" => m.preview_url,
                _ => None,
            })
            .collect(),
        attachments,
        links: urls,
        post_url,
        details,
        link_preview,
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
            attachments: vec![],
            links: vec![],
            post_url: None,
            details: String::new(),
            link_preview: None,
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
                    existing.attachments = updated.attachments.clone();
                    existing.links = updated.links.clone();
                    existing.post_url = updated.post_url.clone();
                    existing.details = updated.details.clone();
                    existing.link_preview = updated.link_preview.clone();
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
        let preferences = settings::Settings::load().unwrap_or_default();
        let font_candidate = preferences.font_size;
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
                input_cursor: 0,
                settings: preferences,
                settings_index: 0,
                settings_message: String::new(),
                font_candidate,
                detail_index: 0,
                detail_scroll: 0,
                native_preview: None,
                picker: ratatui_image::picker::Picker::from_fontsize((10, 20)),
                instance: config.instance.clone(),
                account,
                account_labels: vec![],
                active_account: 0,
                settings_account_index: 0,
                account_input: String::new(),
                account_add_step: 0,
                auth_url: String::new(),
                account_busy: false,
                favourite_effect: None,
                auth: None,
                auth_attempt: 0,
                pending_actions: VecDeque::new(),
                pending_interactions: HashSet::new(),
                g_pending: None,
                config,
                tx,
                stream_tx,
                stop_streams: Arc::new(AtomicBool::new(false)),
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
        self.native_preview = url.map(|url| media::NativePreview::start(self.picker, url));
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
        if !self.pending_interactions.insert((id.clone(), boost)) {
            self.status = "Please wait for the previous action.".into();
            return;
        }
        let config = self.config.clone();
        let tx = self.tx.clone();
        self.status = format!("{action}…");
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|c| c.action(&id, action))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Action(id, boost, action == "favourite", result));
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
                        self.input_cursor = 0;
                        self.attachments.clear();
                        self.mode = Mode::Normal;
                        self.refresh(Kind::Home);
                        self.status = "Posted.".into();
                    }
                    Err(e) => self.status = format!("Post failed (draft retained): {e}"),
                }
            }
            Message::AuthRegistered(attempt, result)
                if attempt == self.auth_attempt && self.mode == Mode::AccountAdd =>
            {
                self.account_busy = false;
                match result {
                    Ok(auth) => {
                        self.auth_url = auth.authorize_url.clone();
                        self.auth = Some(auth);
                        self.account_add_step = 1;
                        self.account_input.clear();
                        self.settings_message =
                            "Authorize in your browser, then paste the code.".into();
                    }
                    Err(e) => self.settings_message = format!("Registration failed: {e}"),
                }
            }
            Message::AuthVerified(attempt, result)
                if attempt == self.auth_attempt && self.mode == Mode::AccountAdd =>
            {
                self.account_busy = false;
                match result {
                    Ok((config, account)) => {
                        self.account_input.clear();
                        self.auth = None;
                        self.pending_actions
                            .push_back(WorkspaceAction::Add(config, account));
                    }
                    Err(e) => self.settings_message = format!("Authorization failed: {e}"),
                }
            }
            Message::AuthRegistered(_, _) | Message::AuthVerified(_, _) => {}
            Message::Identity(Ok(account)) => self
                .pending_actions
                .push_back(WorkspaceAction::Upgrade(account)),
            Message::Identity(Err(_)) => {}
            Message::Action(id, boost, favourited, result) => {
                self.pending_interactions.remove(&(id.clone(), boost));
                match result {
                    Ok(updated) => {
                        for item in self
                            .tabs
                            .iter_mut()
                            .flat_map(|tab| &mut tab.items)
                            .filter(|item| item.id == id)
                        {
                            if let Some(value) = updated.favourited {
                                item.favourite = value;
                            }
                            if let Some(value) = updated.reblogged {
                                item.boosted = value;
                            }
                        }
                        if favourited {
                            self.favourite_effect = Some((id, Instant::now()));
                        }
                        let kind = self.tabs[self.active].kind;
                        self.refresh(kind);
                    }
                    Err(e) => self.status = format!("Action failed: {e}"),
                }
            }
        }
    }
    fn insert_text(&mut self, text: &str) {
        // Bracketed paste may contain CRLF or control bytes; keep only text, tabs and newlines.
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let clean: String = text
            .chars()
            .filter(|&c| !c.is_control() || c == '\n' || c == '\t')
            .collect();
        self.input.insert_str(self.input_cursor, &clean);
        self.input_cursor += clean.len();
    }
    fn prev_cursor(&self) -> usize {
        self.input[..self.input_cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }
    fn next_cursor(&self) -> usize {
        self.input_cursor
            + self.input[self.input_cursor..]
                .graphemes(true)
                .next()
                .map_or(0, str::len)
    }
    fn move_line(&mut self, direction: i8) {
        let start = self.input[..self.input_cursor]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        let column = self.input[start..self.input_cursor].graphemes(true).count();
        let target = if direction < 0 {
            if start == 0 {
                return;
            }
            let end = start - 1;
            let begin = self.input[..end].rfind('\n').map_or(0, |i| i + 1);
            (begin, end)
        } else {
            let Some(relative_end) = self.input[self.input_cursor..].find('\n') else {
                return;
            };
            let begin = self.input_cursor + relative_end + 1;
            let end = self.input[begin..]
                .find('\n')
                .map_or(self.input.len(), |i| begin + i);
            (begin, end)
        };
        self.input_cursor = target.0
            + self.input[target.0..target.1]
                .grapheme_indices(true)
                .nth(column)
                .map_or(target.1 - target.0, |(i, _)| i);
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return true;
        }
        if self.mode == Mode::Normal && self.g_pending.take().is_some() {
            match key.code {
                KeyCode::Char('t') => {
                    self.pending_actions
                        .push_back(WorkspaceAction::SwitchRelative(1));
                    return false;
                }
                KeyCode::Char('T') => {
                    self.pending_actions
                        .push_back(WorkspaceAction::SwitchRelative(-1));
                    return false;
                }
                _ => {
                    self.tabs[self.active].selected = 0;
                    self.update_preview();
                }
            }
        }
        match self.mode {
            Mode::Normal => match key.code {
                KeyCode::Char('q') => return true,
                KeyCode::Char(']') => self
                    .pending_actions
                    .push_back(WorkspaceAction::SwitchRelative(1)),
                KeyCode::Char('[') => self
                    .pending_actions
                    .push_back(WorkspaceAction::SwitchRelative(-1)),
                KeyCode::Char('?') => self.mode = Mode::Help,
                KeyCode::Char('s') => {
                    self.mode = Mode::Settings;
                    self.settings_index = 0;
                    self.settings_message.clear();
                    self.font_candidate = self.settings.font_size;
                }
                KeyCode::Enter => {
                    if self.selected().is_some() {
                        self.detail_index = 0;
                        self.detail_scroll = 0;
                        self.mode = Mode::Detail;
                    }
                }
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
                KeyCode::Char('g') => self.g_pending = Some(Instant::now()),
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
                        stream::start(
                            self.config.clone(),
                            stream_kind,
                            self.stream_tx.clone(),
                            self.stop_streams.clone(),
                        );
                        self.active = self.tabs.len() - 1;
                        self.refresh(kind);
                    }
                    self.mode = Mode::Normal;
                    self.update_preview();
                } else if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Settings => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.settings_index = (self.settings_index + 1).min(3)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.settings_index = self.settings_index.saturating_sub(1)
                }
                KeyCode::Char('0') if self.settings_index == 2 => self.font_candidate = None,
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('l') | KeyCode::Right => {
                    let forward = matches!(key.code, KeyCode::Char('l') | KeyCode::Right);
                    match self.settings_index {
                        0 => {
                            self.settings.theme = if forward {
                                self.settings.theme.next()
                            } else {
                                self.settings.theme.previous()
                            };
                        }
                        1 => {
                            self.settings.density = if forward {
                                self.settings.density.next()
                            } else {
                                self.settings.density.previous()
                            };
                        }
                        2 => {
                            self.font_candidate = Some(match (self.font_candidate, forward) {
                                (None, true) => 10,
                                (None, false) => 9,
                                (Some(size), true) => size.saturating_add(1).min(24),
                                (Some(size), false) => size.saturating_sub(1).max(8),
                            });
                        }
                        _ => {}
                    }
                    if self.settings_index < 2 {
                        self.settings_message = match self.settings.save() {
                            Ok(()) => "Settings saved.".into(),
                            Err(e) => format!("Could not save settings: {e:#}"),
                        };
                        self.status = self.settings_message.clone();
                    }
                }
                KeyCode::Enter if self.settings_index == 3 => {
                    self.settings_account_index = self.active_account;
                    self.settings_message.clear();
                    self.mode = Mode::AccountManage;
                }
                KeyCode::Enter if self.settings_index == 2 => {
                    self.settings_message =
                        match settings::apply_kitty_font_size(self.font_candidate) {
                            Ok(()) => {
                                self.settings.font_size = self.font_candidate;
                                match self.settings.save() {
                                    Ok(()) => "Kitty font size applied and saved.".into(),
                                    Err(e) => {
                                        format!("Font applied; could not save settings: {e:#}")
                                    }
                                }
                            }
                            Err(e) => format!("Font unchanged: {e:#}"),
                        };
                    self.status = self.settings_message.clone();
                }
                _ => {}
            },
            Mode::AccountManage => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.mode = Mode::Settings;
                    self.settings_index = 3;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.settings_account_index = (self.settings_account_index + 1)
                        .min(self.account_labels.len().saturating_sub(1))
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.settings_account_index = self.settings_account_index.saturating_sub(1)
                }
                KeyCode::Enter if !self.account_labels.is_empty() => self
                    .pending_actions
                    .push_back(WorkspaceAction::Switch(self.settings_account_index)),
                KeyCode::Char('d') if !self.account_labels.is_empty() => {
                    self.mode = Mode::AccountDeleteConfirm
                }
                KeyCode::Char('a') => {
                    self.auth_attempt = self.auth_attempt.wrapping_add(1);
                    self.account_input.clear();
                    self.auth_url.clear();
                    self.auth = None;
                    self.account_add_step = 0;
                    self.account_busy = false;
                    self.settings_message.clear();
                    self.mode = Mode::AccountAdd;
                }
                _ => {}
            },
            Mode::AccountDeleteConfirm => match key.code {
                KeyCode::Char('y') => self
                    .pending_actions
                    .push_back(WorkspaceAction::Delete(self.settings_account_index)),
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => {
                    self.mode = Mode::AccountManage
                }
                _ => {}
            },
            Mode::AccountAdd => {
                if key.code == KeyCode::Esc {
                    self.auth_attempt = self.auth_attempt.wrapping_add(1);
                    self.account_input.clear();
                    self.auth_url.clear();
                    self.auth = None;
                    self.account_busy = false;
                    self.mode = Mode::AccountManage;
                } else if !self.account_busy
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('o')
                    && self.account_add_step == 1
                {
                    self.settings_message = match links::open(&self.auth_url) {
                        Ok(()) => "Opening authorization URL…".into(),
                        Err(e) => format!("Could not open browser: {e:#}"),
                    };
                } else if !self.account_busy && key.code == KeyCode::Enter {
                    if self.account_input.trim().is_empty() {
                        self.settings_message = "Input cannot be empty.".into();
                    } else if self.account_add_step == 0 {
                        let instance = self.account_input.trim().to_owned();
                        let attempt = self.auth_attempt;
                        let tx = self.tx.clone();
                        self.account_busy = true;
                        self.settings_message = "Registering application…".into();
                        thread::spawn(move || {
                            let result = api::register_app(&instance).map_err(|e| format!("{e:#}"));
                            let _ = tx.send(Message::AuthRegistered(attempt, result));
                        });
                    } else if let Some(auth) = self.auth.clone() {
                        let code = self.account_input.trim().to_owned();
                        let attempt = self.auth_attempt;
                        let tx = self.tx.clone();
                        self.account_busy = true;
                        self.settings_message = "Verifying account…".into();
                        thread::spawn(move || {
                            let result = (|| -> Result<(Config, Account)> {
                                let config = api::exchange_code(&auth, &code)?;
                                let account =
                                    Mastodon::new(config.clone())?.verify_credentials()?;
                                Ok((config, account))
                            })()
                            .map_err(|e| format!("{e:#}"));
                            let _ = tx.send(Message::AuthVerified(attempt, result));
                        });
                    }
                } else if !self.account_busy {
                    match key.code {
                        KeyCode::Backspace => {
                            self.account_input.pop();
                        }
                        KeyCode::Char(c)
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !c.is_control()
                                && self.account_input.len() < 8192 =>
                        {
                            self.account_input.push(c)
                        }
                        _ => {}
                    }
                }
            }
            Mode::Detail => {
                let count = self
                    .selected()
                    .map(|item| item.attachments.len() + item.links.len())
                    .unwrap_or(0);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                    KeyCode::Char('j') => {
                        self.detail_index = (self.detail_index + 1).min(count.saturating_sub(1))
                    }
                    KeyCode::Char('k') => self.detail_index = self.detail_index.saturating_sub(1),
                    KeyCode::Down => self.detail_scroll = self.detail_scroll.saturating_add(1),
                    KeyCode::Up => self.detail_scroll = self.detail_scroll.saturating_sub(1),
                    KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                    KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                    KeyCode::Enter | KeyCode::Char('o') => {
                        let url = self.selected().and_then(|item| {
                            if self.detail_index < item.attachments.len() {
                                item.attachments
                                    .get(self.detail_index)
                                    .map(|a| a.url.clone())
                            } else {
                                item.links
                                    .get(self.detail_index - item.attachments.len())
                                    .cloned()
                            }
                        });
                        self.status = match url {
                            Some(url) => match links::open(&url) {
                                Ok(()) => "Opening in your desktop application…".into(),
                                Err(e) => format!("Cannot open: {e:#}"),
                            },
                            None => "No media or link selected.".into(),
                        };
                    }
                    _ => {}
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
                            self.insert_text(&c.to_string())
                        }
                        KeyCode::Enter => self.insert_text("\n"),
                        KeyCode::Left => self.input_cursor = self.prev_cursor(),
                        KeyCode::Right => self.input_cursor = self.next_cursor(),
                        KeyCode::Home => {
                            self.input_cursor = self.input[..self.input_cursor]
                                .rfind('\n')
                                .map_or(0, |i| i + 1)
                        }
                        KeyCode::End => {
                            self.input_cursor += self.input[self.input_cursor..]
                                .find('\n')
                                .unwrap_or(self.input.len() - self.input_cursor)
                        }
                        KeyCode::Up => self.move_line(-1),
                        KeyCode::Down => self.move_line(1),
                        KeyCode::Backspace => {
                            let prev = self.prev_cursor();
                            self.input.replace_range(prev..self.input_cursor, "");
                            self.input_cursor = prev;
                        }
                        KeyCode::Delete => {
                            let next = self.next_cursor();
                            self.input.replace_range(self.input_cursor..next, "");
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }
}
struct Session {
    id: String,
    app: App,
    rx: mpsc::Receiver<Message>,
}
impl Session {
    fn new(account: &accounts::StoredAccount, picker: ratatui_image::picker::Picker) -> Self {
        let (mut app, rx) = App::new(account.config.clone(), account.label.clone());
        app.picker = picker;
        app.refresh(Kind::Home);
        app.refresh(Kind::Notifications);
        stream::start(
            app.config.clone(),
            StreamKind::User,
            app.stream_tx.clone(),
            app.stop_streams.clone(),
        );
        if account.id.ends_with("#legacy") {
            let tx = app.tx.clone();
            let config = app.config.clone();
            thread::spawn(move || {
                let result = Mastodon::new(config)
                    .and_then(|client| client.verify_credentials())
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(Message::Identity(result));
            });
        }
        Self {
            id: account.id.clone(),
            app,
            rx,
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.app.stop_streams.store(true, Ordering::Relaxed);
    }
}
struct Workspace {
    store: accounts::AccountStore,
    sessions: Vec<Session>,
    active: usize,
    picker: ratatui_image::picker::Picker,
    exit_warning: Option<String>,
}
impl Workspace {
    fn new(store: accounts::AccountStore) -> Result<Self> {
        let picker = media::image_picker();
        let active = store
            .accounts
            .iter()
            .position(|a| Some(&a.id) == store.active_id.as_ref())
            .unwrap_or(0);
        if store.accounts.is_empty() {
            anyhow::bail!("No accounts configured");
        }
        let sessions = store
            .accounts
            .iter()
            .map(|a| Session::new(a, picker))
            .collect();
        let mut workspace = Self {
            store,
            sessions,
            active,
            picker,
            exit_warning: None,
        };
        workspace.sync_labels();
        Ok(workspace)
    }
    fn sync_labels(&mut self) {
        let labels: Vec<_> = self
            .store
            .accounts
            .iter()
            .map(|a| a.label.clone())
            .collect();
        for session in &mut self.sessions {
            session.app.account_labels = labels.clone();
            session.app.active_account = self.active;
            session.app.settings_account_index = session
                .app
                .settings_account_index
                .min(labels.len().saturating_sub(1));
        }
    }
    fn error(&mut self, source: usize, text: String) {
        if let Some(session) = self.sessions.get_mut(source) {
            session.app.settings_message = text.clone();
            session.app.status = text;
        }
    }
    fn switch(&mut self, source: usize, target: usize) {
        if target >= self.sessions.len() {
            return;
        }
        let mut candidate = self.store.clone();
        let id = self.sessions[target].id.clone();
        if let Err(e) = candidate.set_active(&id).and_then(|_| candidate.save()) {
            self.error(source, format!("Could not switch account: {e:#}"));
            return;
        }
        let preferences = self.sessions[source].app.settings.clone();
        self.sessions[source].app.mode = Mode::Normal;
        self.sessions[target].app.settings = preferences;
        self.sessions[target].app.mode = Mode::Normal;
        self.store = candidate;
        self.active = target;
        self.sync_labels();
    }
    fn process(&mut self, source: usize, action: WorkspaceAction) -> bool {
        match action {
            WorkspaceAction::SwitchRelative(delta) => {
                let len = self.sessions.len();
                let target =
                    (self.active as isize + delta as isize).rem_euclid(len as isize) as usize;
                self.switch(source, target);
            }
            WorkspaceAction::Switch(index) => self.switch(source, index),
            WorkspaceAction::Delete(index) => {
                if index >= self.sessions.len() {
                    return false;
                }
                if self.sessions[index].app.busy
                    || !self.sessions[index].app.pending_interactions.is_empty()
                {
                    self.error(
                        source,
                        "Wait for the account's pending post or clipboard operation.".into(),
                    );
                    self.sessions[source].app.mode = Mode::AccountManage;
                    return false;
                }
                let mut candidate = self.store.clone();
                let id = self.sessions[index].id.clone();
                if let Err(e) = candidate.remove(&id).and_then(|_| candidate.save()) {
                    self.error(source, format!("Could not remove account: {e:#}"));
                    self.sessions[source].app.mode = Mode::AccountManage;
                    return false;
                }
                let cleanup = candidate
                    .cleanup_removed_legacy()
                    .err()
                    .map(|e| format!("Account removed, but old credential cleanup failed: {e:#}"));
                self.store = candidate;
                self.sessions.remove(index);
                if self.sessions.is_empty() {
                    self.exit_warning = cleanup;
                    return true;
                }
                self.active = self
                    .sessions
                    .iter()
                    .position(|s| Some(&s.id) == self.store.active_id.as_ref())
                    .unwrap_or(0);
                self.sessions[self.active].app.mode = Mode::AccountManage;
                self.sessions[self.active].app.settings_account_index =
                    index.min(self.sessions.len() - 1);
                self.sessions[self.active].app.settings_message = cleanup.unwrap_or_else(|| {
                    "Account removed locally; server authorization remains active.".into()
                });
                self.sync_labels();
            }
            WorkspaceAction::Add(config, account) => {
                // If saving fails, allow the user to start authorization again.
                self.sessions[source].app.mode = Mode::AccountManage;
                let mut candidate = self.store.clone();
                let id = match candidate.add_with_identity(config, account) {
                    Ok(id) => id,
                    Err(e) => {
                        self.error(source, format!("Could not add account: {e:#}"));
                        return false;
                    }
                };
                let added = candidate
                    .accounts
                    .iter()
                    .find(|a| a.id == id)
                    .expect("added account exists")
                    .clone();
                let old_index = self.sessions.iter().position(|s| s.id == id).or_else(|| {
                    self.sessions.iter().position(|s| {
                        s.id.ends_with("#legacy")
                            && s.app.config.instance == added.config.instance
                            && s.app.config.access_token == added.config.access_token
                    })
                });
                let discarded = self
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(i, s)| {
                        Some(*i) != old_index && !candidate.accounts.iter().any(|a| a.id == s.id)
                    })
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                let has_draft = |app: &App| !app.input.is_empty() || !app.attachments.is_empty();
                if discarded.iter().any(|&i| {
                    self.sessions[i].app.busy
                        || self.sessions[i].app.account_busy
                        || !self.sessions[i].app.pending_interactions.is_empty()
                        || !self.sessions[i].app.pending_actions.is_empty()
                }) {
                    self.error(
                        source,
                        "Wait for pending account activity before merging.".into(),
                    );
                    return false;
                }
                if discarded
                    .iter()
                    .filter(|&&i| has_draft(&self.sessions[i].app))
                    .count()
                    + usize::from(old_index.is_some_and(|i| has_draft(&self.sessions[i].app)))
                    > 1
                {
                    self.error(
                        source,
                        "Both copies have drafts; keep them until you can merge them manually."
                            .into(),
                    );
                    return false;
                }
                let draft_from = discarded
                    .iter()
                    .copied()
                    .find(|&i| has_draft(&self.sessions[i].app));
                if old_index.is_some_and(|i| {
                    self.sessions[i].app.busy
                        || !self.sessions[i].app.pending_interactions.is_empty()
                }) {
                    self.error(source, "Wait for this account's pending post or clipboard operation before reauthorizing.".into());
                    return false;
                }
                if let Err(e) = candidate.save() {
                    self.error(source, format!("Could not save account: {e:#}"));
                    return false;
                }
                let warning = candidate
                    .cleanup_removed_legacy()
                    .err()
                    .map(|e| format!("Account added, but old credential cleanup failed: {e:#}"));
                let source_id = self.sessions[source].id.clone();
                let preferences = self.sessions[source].app.settings.clone();
                let mut old: Vec<Option<Session>> = std::mem::take(&mut self.sessions)
                    .into_iter()
                    .map(Some)
                    .collect();
                let transferred_draft = draft_from.map(|i| {
                    let app = &mut old[i].as_mut().expect("discarded session").app;
                    (
                        std::mem::take(&mut app.input),
                        app.input_cursor,
                        std::mem::take(&mut app.attachments),
                    )
                });
                for entry in &candidate.accounts {
                    let exact = old
                        .iter()
                        .position(|s| s.as_ref().is_some_and(|s| s.id == entry.id));
                    if entry.id == id {
                        let prior = exact.or_else(|| {
                            old.iter().position(|s| {
                                s.as_ref().is_some_and(|s| {
                                    s.id.ends_with("#legacy")
                                        && s.app.config.instance == entry.config.instance
                                        && s.app.config.access_token == entry.config.access_token
                                })
                            })
                        });
                        let mut replacement = Session::new(entry, self.picker);
                        if let Some(mut previous) = prior.and_then(|i| old[i].take()) {
                            replacement.app.tabs = std::mem::take(&mut previous.app.tabs);
                            replacement.app.active = previous.app.active;
                            replacement.app.input = std::mem::take(&mut previous.app.input);
                            replacement.app.input_cursor = previous.app.input_cursor;
                            replacement.app.attachments =
                                std::mem::take(&mut previous.app.attachments);
                            for kind in [Kind::Local, Kind::Federated] {
                                if replacement.app.tabs.iter().any(|t| t.kind == kind) {
                                    let stream_kind = if kind == Kind::Local {
                                        StreamKind::Local
                                    } else {
                                        StreamKind::Federated
                                    };
                                    stream::start(
                                        replacement.app.config.clone(),
                                        stream_kind,
                                        replacement.app.stream_tx.clone(),
                                        replacement.app.stop_streams.clone(),
                                    );
                                    replacement.app.refresh(kind);
                                }
                            }
                        }
                        if let Some((input, cursor, attachments)) = transferred_draft.clone() {
                            replacement.app.input = input;
                            replacement.app.input_cursor = cursor;
                            replacement.app.attachments = attachments;
                        }
                        replacement.app.settings = preferences.clone();
                        self.sessions.push(replacement);
                    } else if let Some(session) = exact.and_then(|i| old[i].take()) {
                        self.sessions.push(session);
                    } else {
                        self.sessions.push(Session::new(entry, self.picker));
                    }
                }
                for session in &mut self.sessions {
                    if session.id == source_id {
                        session.app.mode = Mode::Normal;
                    }
                }
                self.store = candidate;
                self.active = self
                    .sessions
                    .iter()
                    .position(|s| s.id == id)
                    .expect("added session exists");
                self.sessions[self.active].app.status =
                    warning.unwrap_or_else(|| "Account added.".into());
                self.sync_labels();
            }
            WorkspaceAction::Upgrade(account) => {
                let old_id = self.sessions[source].id.clone();
                if !old_id.ends_with("#legacy") {
                    return false;
                }
                let config = self.sessions[source].app.config.clone();
                let real_id = format!("{}#account:{}", config.instance, account.id);
                if let Some(target) = self.sessions.iter().position(|s| s.id == real_id) {
                    if self.sessions[source].app.busy
                        || self.sessions[target].app.busy
                        || self.sessions[source].app.account_busy
                        || !self.sessions[source].app.pending_actions.is_empty()
                        || !self.sessions[source].app.pending_interactions.is_empty()
                        || !self.sessions[target].app.pending_interactions.is_empty()
                    {
                        self.error(
                            source,
                            "Wait for pending activity before merging the migrated account.".into(),
                        );
                        return false;
                    }
                    let source_has_draft = !self.sessions[source].app.input.is_empty()
                        || !self.sessions[source].app.attachments.is_empty();
                    let target_has_draft = !self.sessions[target].app.input.is_empty()
                        || !self.sessions[target].app.attachments.is_empty();
                    if source_has_draft && target_has_draft {
                        self.error(
                            source,
                            "Both copies have drafts; keep them until you can merge them manually."
                                .into(),
                        );
                        return false;
                    }
                    let mut candidate = self.store.clone();
                    let was_active = candidate.active_id.as_deref() == Some(&old_id);
                    let result = candidate.remove(&old_id).and_then(|_| {
                        if was_active {
                            candidate.set_active(&real_id)?;
                        }
                        candidate.save()
                    });
                    if let Err(e) = result {
                        self.error(source, format!("Could not merge migrated account: {e:#}"));
                        return false;
                    }
                    let warning = candidate
                        .cleanup_removed_legacy()
                        .err()
                        .map(|e| format!("Old credential cleanup failed: {e:#}"));
                    if source_has_draft {
                        let text = std::mem::take(&mut self.sessions[source].app.input);
                        let cursor = self.sessions[source].app.input_cursor;
                        let attachments =
                            std::mem::take(&mut self.sessions[source].app.attachments);
                        self.sessions[target].app.input = text;
                        self.sessions[target].app.input_cursor = cursor;
                        self.sessions[target].app.attachments = attachments;
                    }
                    self.sessions.remove(source);
                    self.store = candidate;
                    self.active = self
                        .sessions
                        .iter()
                        .position(|s| Some(&s.id) == self.store.active_id.as_ref())
                        .unwrap_or(0);
                    if let Some(warning) = warning {
                        self.sessions[self.active].app.status = warning;
                    }
                    self.sync_labels();
                } else {
                    let mut candidate = self.store.clone();
                    let was_active = candidate.active_id.as_deref() == Some(&old_id);
                    let previous = candidate.active_id.clone();
                    let result = candidate
                        .add_with_identity(config, account)
                        .and_then(|new_id| {
                            if !was_active {
                                if let Some(id) = previous {
                                    candidate.set_active(&id)?;
                                }
                            }
                            candidate.save()?;
                            Ok(new_id)
                        });
                    if let Ok(id) = result {
                        self.sessions[source].id = id.clone();
                        if let Some(entry) = candidate.accounts.iter().find(|a| a.id == id) {
                            self.sessions[source].app.account = entry.label.clone();
                        }
                        self.store = candidate;
                        self.sync_labels();
                    }
                }
            }
        }
        false
    }
}
pub fn run(terminal: &mut ratatui::DefaultTerminal, store: accounts::AccountStore) -> Result<()> {
    let mut workspace = Workspace::new(store)?;
    loop {
        for session in &mut workspace.sessions {
            for _ in 0..100 {
                match session.rx.try_recv() {
                    Ok(msg) => session.app.handle(msg),
                    Err(_) => break,
                }
            }
        }
        let mut pending = None;
        for (index, session) in workspace.sessions.iter_mut().enumerate() {
            if let Some(action) = session.app.pending_actions.pop_front() {
                pending = Some((index, action));
                break;
            }
        }
        if let Some((source, action)) = pending {
            if workspace.process(source, action) {
                break;
            }
        }
        let active = workspace.active;
        if workspace.sessions[active]
            .app
            .g_pending
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(700))
        {
            let app = &mut workspace.sessions[active].app;
            app.g_pending = None;
            app.tabs[app.active].selected = 0;
            app.update_preview();
        }
        terminal.draw(|f| crate::ui::draw(f, &mut workspace.sessions[active].app))?;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if workspace.sessions[active].app.key(key) {
                        break;
                    }
                    let preferences = workspace.sessions[active].app.settings.clone();
                    for session in &mut workspace.sessions {
                        session.app.settings = preferences.clone();
                    }
                }
                Event::Paste(text) => {
                    let app = &mut workspace.sessions[active].app;
                    if app.mode == Mode::Compose && !app.busy {
                        app.insert_text(&text);
                    } else if app.mode == Mode::AccountAdd && !app.account_busy {
                        app.account_input.extend(
                            text.chars()
                                .filter(|c| !c.is_control())
                                .take(8192usize.saturating_sub(app.account_input.len())),
                        );
                    }
                }
                Event::Resize(_, _) => {
                    workspace.picker = media::image_picker();
                    for session in &mut workspace.sessions {
                        session.app.picker = workspace.picker;
                        session.app.preview_url = None;
                        session.app.update_preview();
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(warning) = workspace.exit_warning {
        anyhow::bail!("{warning}");
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
    app.settings = settings::Settings::default();
    app.font_candidate = None;
    app.tabs[0].items.push(ViewItem {
        id: "1".into(),
        key: "1".into(),
        author: "Alice".into(),
        acct: "alice@example.com".into(),
        content: "Hello from Alice".into(),
        timestamp: "2026-01-01".into(),
        context: String::new(),
        media: vec!["https://example.com/image.png".into()],
        attachments: vec![],
        links: vec![],
        post_url: None,
        details: String::new(),
        link_preview: None,
        favourite: false,
        boosted: false,
    });

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
    fn settings_message_survives_stream_status_changes() {
        let mut app = test_app();
        app.mode = Mode::Settings;
        app.settings_message = "Font unchanged: unavailable".into();
        app.handle_stream(StreamMessage::Disconnected(StreamKind::User));
        assert_eq!(app.settings_message, "Font unchanged: unavailable");
        assert!(app.status.contains("reconnecting"));
    }
    #[test]
    fn japanese_draft_paste_and_grapheme_editing() {
        let mut app = test_app();
        app.mode = Mode::Compose;
        app.insert_text("日本語\r\nか\u{3099}き");
        assert_eq!(app.input, "日本語\nか\u{3099}き");
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.input, "日本語\nき");
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        app.insert_text("追加");
        assert_eq!(app.input, "追加日本語\nき");
        assert!(app.input.is_char_boundary(app.input_cursor));
    }
    #[test]
    fn detail_lists_original_media_and_links() {
        let status = Status {
            id: "123".into(),
            content: r#"<p><a href="https://example.com/article?a=1&amp;b=2">article</a></p>"#
                .into(),
            url: Some("https://example.com/@alice/123".into()),
            media_attachments: vec![crate::api::MediaAttachment {
                kind: "video".into(),
                url: Some("https://example.com/original.mp4".into()),
                preview_url: Some("https://example.com/thumbnail.jpg".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let item = view(status, String::new());
        assert_eq!(item.attachments[0].url, "https://example.com/original.mp4");
        assert_eq!(item.media[0], "https://example.com/thumbnail.jpg");
        assert_eq!(item.links[0], "https://example.com/article?a=1&b=2");
        let mut app = test_app();
        app.tabs[0].items = vec![item];
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.mode == Mode::Detail);
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.detail_index, 1);
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
    #[test]
    fn favourite_feedback_only_follows_success_and_updates_all_views() {
        let mut app = test_app();
        let status = Status {
            id: "post".into(),
            account: api_account("alice"),
            ..Default::default()
        };
        app.tabs[0].items = vec![view(status.clone(), String::new())];
        app.tabs[1].items = vec![view(status, String::new())];
        app.pending_interactions.insert(("post".into(), false));
        app.handle(Message::Action(
            "post".into(),
            false,
            true,
            Err("network".into()),
        ));
        assert!(app.favourite_effect.is_none());
        assert!(!app.pending_interactions.contains(&("post".into(), false)));
        app.pending_interactions.insert(("post".into(), false));
        app.handle(Message::Action(
            "post".into(),
            false,
            true,
            Ok(Status {
                id: "post".into(),
                favourited: Some(true),
                ..Default::default()
            }),
        ));
        assert!(app.favourite_effect.is_some());
        assert!(app.tabs[0].items[0].favourite && app.tabs[1].items[0].favourite);
        assert!(!app.pending_interactions.contains(&("post".into(), false)));
    }
    fn api_account(name: &str) -> crate::api::Account {
        crate::api::Account {
            acct: name.into(),
            ..Default::default()
        }
    }
}
