use crate::api::{self, Account, Config, Conversation, Mastodon, Notification, Status, Timeline};
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
    Profile,
    Thread,
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
    Direct,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyDraft {
    pub id: String,
    pub author: String,
    pub acct: String,
    pub visibility: Option<String>,
    original_visibility: String,
    recipients: Vec<(String, String)>,
    mention_prefix: String,
    generation: u64,
}
#[derive(Clone)]
pub struct ViewItem {
    pub id: String,
    key: String,
    pub author: String,
    pub acct: String,
    pub account_id: String,
    pub reply_to_id: Option<String>,
    pub visibility: String,
    pub mentions: Vec<api::Mention>,
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
pub struct Profile {
    pub account: Option<Account>,
    pub account_error: Option<String>,
    pub posts: Vec<ViewItem>,
    pub posts_error: Option<String>,
    pub loading_posts: bool,
    pub has_more: bool,
    pub scroll: u16,
    pub selected: usize,
    id: String,
    next_id: Option<String>,
    generation: u64,
}
pub struct ThreadEntry {
    pub item: ViewItem,
    pub prefix: String,
}
pub struct ThreadState {
    pub entries: Vec<ThreadEntry>,
    pub selected: usize,
    pub loading: bool,
    pub error: Option<String>,
    pub truncated: bool,
    pending_selection: Option<String>,
    generation: u64,
}

// Use the server-provided parent IDs rather than assuming that descendants are
// in tree order. Duplicate IDs and cycles are ignored, and rendering stays bounded.
fn thread_entries(root: &str, statuses: Vec<Status>) -> Vec<ThreadEntry> {
    const LIMIT: usize = 500;
    const MAX_DEPTH: usize = 8;
    let mut seen = HashSet::new();
    let statuses: Vec<Status> = statuses
        .into_iter()
        .filter(|s| !s.id.is_empty() && s.id != root && seen.insert(s.id.clone()))
        .take(LIMIT)
        .collect();
    let ids: HashSet<&str> = statuses.iter().map(|s| s.id.as_str()).collect();
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (index, status) in statuses.iter().enumerate() {
        match status.in_reply_to_id.as_deref() {
            Some(parent) if parent != status.id && ids.contains(parent) => {
                children.entry(parent).or_default().push(index)
            }
            _ => roots.push(index),
        }
    }
    let mut visited = HashSet::new();
    let mut entries = Vec::new();
    let mut walk = |start: usize, last_root: bool| {
        let mut stack = vec![(start, Vec::<bool>::new(), last_root)];
        while let Some((index, ancestors, last)) = stack.pop() {
            if !visited.insert(index) {
                continue;
            }
            let mut prefix = String::new();
            for &has_next in &ancestors {
                prefix.push_str(if has_next { "│  " } else { "   " });
            }
            prefix.push_str(if last { "└─ " } else { "├─ " });
            let status = &statuses[index];
            entries.push(ThreadEntry {
                item: view(status.clone(), String::new()),
                prefix,
            });
            if let Some(kids) = children.get(status.id.as_str()) {
                for (position, child) in kids.iter().enumerate().rev() {
                    let mut next = ancestors.clone();
                    if next.len() < MAX_DEPTH {
                        next.push(!last);
                    }
                    stack.push((*child, next, position + 1 == kids.len()));
                }
            }
        }
    };
    let root_count = roots.len();
    for (position, index) in roots.into_iter().enumerate() {
        walk(index, position + 1 == root_count);
    }
    // Orphans forming a cycle have no root; show each component once.
    for index in 0..statuses.len() {
        walk(index, true);
    }
    entries
}
pub struct App {
    pub tabs: Vec<Tab>,
    pub active: usize,
    // The timeline shown on the left is independent of the focused side pane.
    left_timeline: Kind,
    pub status: String,
    pub mode: Mode,
    pub input: String,
    pub input_cursor: usize,
    pub reply_draft: Option<ReplyDraft>,
    reply_generation: u64,
    compose_return: Mode,
    discard_armed: bool,
    pub settings: settings::Settings,
    pub settings_index: usize,
    pub settings_message: String,
    pub font_candidate: Option<u8>,
    pub detail_index: usize,
    pub detail_scroll: u16,
    detail_item: Option<ViewItem>,
    detail_history: Vec<(Option<ViewItem>, Mode, usize, u16)>,
    pub profile: Option<Profile>,
    profile_history: Vec<Profile>,
    pub threads: HashMap<String, ThreadState>,
    pub thread_stack: Vec<String>,
    thread_generation: u64,
    profile_generation: u64,
    pub reply_targets: HashMap<String, Result<Account, String>>,
    reply_requests: HashSet<String>,
    pub follow_results: HashMap<String, String>,
    pub follow_requests: HashSet<String>,
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
    started_streams: HashSet<StreamKind>,
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
    ReplyIdentity(u64, Result<Account, String>),
    Action(String, bool, bool, Result<Status, String>),
    ReplyTarget(String, Result<Account, String>),
    Follow(String, Result<api::Relationship, String>),
    ThreadLoaded(String, u64, Result<Vec<Status>, String>),
    ProfileAccount(u64, Result<Account, String>),
    ProfilePosts(u64, Result<Vec<Status>, String>),
    AuthRegistered(u64, Result<api::Authorization, String>),
    AuthVerified(u64, Result<(Config, Account), String>),
    AuthUrlCopied(u64, Result<(), String>),
    Identity(Result<Account, String>),
}
impl Tab {
    fn new(kind: Kind) -> Self {
        let title = match kind {
            Kind::Home => "Home",
            Kind::Notifications => "Notifications",
            Kind::Local => "Local",
            Kind::Federated => "Federated",
            Kind::Direct => "Direct messages",
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
impl ViewItem {
    pub fn detail_account_count(&self) -> usize {
        usize::from(!self.account_id.is_empty()) + usize::from(self.reply_to_id.is_some())
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
        account_id: status.account.id.clone(),
        reply_to_id: status.in_reply_to_id.clone().filter(|id| !id.is_empty()),
        visibility: status.visibility.clone(),
        mentions: status.mentions.clone(),
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
            account_id: n.account.id,
            reply_to_id: None,
            visibility: String::new(),
            mentions: vec![],
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
fn conversation(c: Conversation) -> Option<ViewItem> {
    if c.id.is_empty() {
        return None;
    }
    let mut item = view(
        c.last_status?,
        if c.unread { "DM · unread" } else { "DM" }.into(),
    );
    item.key = format!("conversation:{}", c.id);
    Some(item)
}
// Keep the selected post stable as live items arrive and bound memory use.
fn apply_event(tab: &mut Tab, event: &StreamEvent) {
    match event {
        StreamEvent::Update(status)
            if tab.kind != Kind::Notifications && tab.kind != Kind::Direct =>
        {
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
        StreamEvent::Conversation(c) if tab.kind == Kind::Direct && !c.id.is_empty() => {
            let selected = tab.items.get(tab.selected).map(|item| item.key.clone());
            let key = format!("conversation:{}", c.id);
            tab.items.retain(|item| item.key != key);
            if let Some(item) = conversation(c.clone()) {
                tab.items.insert(0, item);
                tab.items.truncate(200);
            }
            tab.selected = selected
                .and_then(|key| tab.items.iter().position(|item| item.key == key))
                .unwrap_or_else(|| tab.selected.min(tab.items.len().saturating_sub(1)));
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
                    existing.visibility = updated.visibility.clone();
                    existing.mentions = updated.mentions.clone();
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
        StreamEvent::Conversation(_) => kind == Kind::Direct && stream == StreamKind::Direct,
        StreamEvent::Delete(_) | StreamEvent::Edit(_) => true,
        StreamEvent::RefreshNotifications => false,
    }
}
// Unknown visibility is treated as direct, never as a public post.
fn reply_visibility<'a>(original: &'a str, preferred: Option<&'a str>) -> &'a str {
    fn rank(value: &str) -> Option<u8> {
        match value {
            "direct" => Some(0),
            "private" => Some(1),
            "unlisted" => Some(2),
            "public" => Some(3),
            _ => None,
        }
    }
    let preferred = preferred.unwrap_or("");
    match (rank(original), rank(preferred)) {
        (Some(a), Some(b)) if a <= b => original,
        (Some(_), Some(_)) => preferred,
        _ => "direct",
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
                left_timeline: Kind::Home,
                status: "Loading…".into(),
                mode: Mode::Normal,
                input: String::new(),
                input_cursor: 0,
                reply_draft: None,
                reply_generation: 0,
                compose_return: Mode::Normal,
                discard_armed: false,
                settings: preferences,
                settings_index: 0,
                settings_message: String::new(),
                font_candidate,
                detail_index: 0,
                detail_scroll: 0,
                detail_item: None,
                detail_history: Vec::new(),
                profile: None,
                profile_history: Vec::new(),
                threads: HashMap::new(),
                thread_stack: Vec::new(),
                thread_generation: 0,
                profile_generation: 0,
                reply_targets: HashMap::new(),
                reply_requests: HashSet::new(),
                follow_results: HashMap::new(),
                follow_requests: HashSet::new(),
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
                started_streams: HashSet::new(),
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
    fn resume_draft(&mut self) {
        self.compose_return = self.mode;
        self.mode = Mode::Compose;
        self.status = format!("{} image(s) attached.", self.attachments.len());
    }
    fn start_reply(&mut self, item: ViewItem) {
        if item.id.is_empty() || item.acct.is_empty() {
            self.status = "This notification has no post to reply to.".into();
            return;
        }
        if self
            .reply_draft
            .as_ref()
            .is_some_and(|draft| draft.id == item.id)
        {
            self.compose_return = self.mode;
            self.mode = Mode::Compose;
            return;
        }
        if !self.input.is_empty() || !self.attachments.is_empty() {
            self.status =
                "A different draft is retained. Press c to finish it before replying.".into();
            return;
        }
        self.reply_generation = self.reply_generation.wrapping_add(1);
        let generation = self.reply_generation;
        let mut recipients = vec![(item.account_id.clone(), item.acct.clone())];
        recipients.extend(item.mentions.iter().map(|m| (m.id.clone(), m.acct.clone())));
        self.reply_draft = Some(ReplyDraft {
            id: item.id,
            author: item.author,
            acct: item.acct,
            visibility: None,
            original_visibility: item.visibility,
            recipients,
            mention_prefix: String::new(),
            generation,
        });
        self.compose_return = self.mode;
        self.mode = Mode::Compose;
        self.status = "Checking reply visibility and recipients…".into();
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|client| client.verify_credentials())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::ReplyIdentity(generation, result));
        });
    }
    fn start_stream(&mut self, kind: StreamKind) -> bool {
        if self.started_streams.insert(kind) {
            stream::start(
                self.config.clone(),
                kind,
                self.stream_tx.clone(),
                self.stop_streams.clone(),
            );
            true
        } else {
            false
        }
    }
    fn ensure_tab(&mut self, kind: Kind) -> usize {
        let index = match self.tabs.iter().position(|tab| tab.kind == kind) {
            Some(index) => index,
            None => {
                self.tabs.push(Tab::new(kind));
                self.tabs.len() - 1
            }
        };
        let stream = match kind {
            Kind::Local => StreamKind::Local,
            Kind::Federated => StreamKind::Federated,
            Kind::Direct => StreamKind::Direct,
            _ => unreachable!(),
        };
        if self.start_stream(stream) {
            self.refresh(kind);
        }
        index
    }
    fn visible_tabs(&self) -> Vec<usize> {
        use settings::Layout;
        let mut indices: Vec<_> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| {
                matches!(tab.kind, Kind::Home | Kind::Local)
                    || (tab.kind == Kind::Federated && self.settings.layout != Layout::Federation)
            })
            .map(|(index, _)| index)
            .collect();
        let right: &[Kind] = match self.settings.layout {
            Layout::Default => &[Kind::Notifications, Kind::Direct],
            Layout::YourTimelineFocus => &[Kind::Notifications],
            Layout::Federation => &[Kind::Federated, Kind::Notifications],
        };
        for kind in right {
            if let Some(index) = self.tabs.iter().position(|tab| tab.kind == *kind) {
                indices.push(index);
            }
        }
        indices
    }
    fn sync_layout(&mut self) {
        match self.settings.layout {
            settings::Layout::Default => {
                self.ensure_tab(Kind::Direct);
            }
            settings::Layout::Federation => {
                self.ensure_tab(Kind::Federated);
            }
            settings::Layout::YourTimelineFocus => {}
        }
        if !self.visible_tabs().contains(&self.active) {
            self.active = self.left_timeline_index().unwrap_or(0);
            self.update_preview();
        }
    }
    fn is_left_timeline(&self, kind: Kind) -> bool {
        matches!(kind, Kind::Home | Kind::Local)
            || (kind == Kind::Federated && self.settings.layout != settings::Layout::Federation)
    }
    fn remember_left_timeline(&mut self) {
        if let Some(tab) = self.tabs.get(self.active) {
            if self.is_left_timeline(tab.kind) {
                self.left_timeline = tab.kind;
            }
        }
    }
    pub(crate) fn left_timeline_index(&self) -> Option<usize> {
        self.tabs
            .get(self.active)
            .filter(|tab| self.is_left_timeline(tab.kind))
            .map(|_| self.active)
            .or_else(|| {
                self.tabs.iter().position(|tab| {
                    tab.kind == self.left_timeline && self.is_left_timeline(tab.kind)
                })
            })
            .or_else(|| self.tabs.iter().position(|tab| tab.kind == Kind::Home))
    }
    fn move_focus(&mut self, forward: bool) {
        self.remember_left_timeline();
        let indices = self.visible_tabs();
        if let Some(position) = indices.iter().position(|&index| index == self.active) {
            let next = if forward {
                (position + 1).min(indices.len() - 1)
            } else {
                position.saturating_sub(1)
            };
            self.active = indices[next];
            self.remember_left_timeline();
            self.update_preview();
        }
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
                    Kind::Direct => client
                        .conversations()?
                        .into_iter()
                        .filter_map(conversation)
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
                    StreamKind::Direct => self.refresh(Kind::Direct),
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
        let url = self.selected().and_then(|i| i.media.first()).cloned();
        if url == self.preview_url {
            return;
        }
        self.preview_url = url.clone();
        self.native_preview = url.map(|url| media::NativePreview::start(self.picker, url));
    }
    pub(crate) fn selected(&self) -> Option<&ViewItem> {
        self.detail_item.as_ref().or_else(|| {
            self.tabs
                .get(self.active)
                .and_then(|tab| tab.items.get(tab.selected))
        })
    }
    pub(crate) fn open_detail_item(&mut self, item: ViewItem, return_mode: Mode) {
        self.detail_history.push((
            self.detail_item.take(),
            return_mode,
            self.detail_index,
            self.detail_scroll,
        ));
        self.detail_item = Some(item);
        self.detail_index = 0;
        self.detail_scroll = 0;
        self.mode = Mode::Detail;
        self.update_preview();
        self.load_reply_target();
        self.load_thread(false);
    }
    fn close_detail(&mut self) {
        if let Some((item, mode, index, scroll)) = self.detail_history.pop() {
            self.detail_item = item;
            self.detail_index = index;
            self.detail_scroll = scroll;
            self.mode = mode;
        } else {
            self.detail_item = None;
            self.mode = Mode::Normal;
        }
        self.update_preview();
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
    fn load_thread(&mut self, retry: bool) {
        let Some(id) = self
            .selected()
            .map(|item| item.id.clone())
            .filter(|id| !id.is_empty())
        else {
            return;
        };
        if self.threads.contains_key(&id) && !retry {
            return;
        }
        self.thread_generation = self.thread_generation.wrapping_add(1);
        let generation = self.thread_generation;
        let pending_selection = self.threads.get(&id).and_then(|state| {
            state
                .entries
                .get(state.selected)
                .map(|entry| entry.item.id.clone())
                .or_else(|| state.pending_selection.clone())
        });
        self.threads.insert(
            id.clone(),
            ThreadState {
                entries: Vec::new(),
                selected: 0,
                loading: true,
                error: None,
                truncated: false,
                pending_selection,
                generation,
            },
        );
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|c| c.status_context(&id))
                .map(|context| context.descendants)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::ThreadLoaded(id, generation, result));
        });
    }
    fn refresh_after_reply(&mut self, target_id: &str) {
        let is_current = self.selected().is_some_and(|item| item.id == target_id);
        // In thread mode selected() is the parent detail. Invalidate the
        // target's cached context and refresh the visible thread as well.
        if !is_current {
            self.threads.remove(target_id);
        }
        if is_current || self.mode == Mode::Thread {
            self.load_thread(true);
        }
    }
    fn selected_thread_item(&self) -> Option<ViewItem> {
        let state = self.threads.get(self.thread_stack.last()?)?;
        state
            .entries
            .get(state.selected)
            .map(|entry| entry.item.clone())
    }
    fn load_reply_target(&mut self) {
        let Some(id) = self.selected().and_then(|item| item.reply_to_id.clone()) else {
            return;
        };
        if matches!(self.reply_targets.get(&id), Some(Ok(_)))
            || !self.reply_requests.insert(id.clone())
        {
            return;
        }
        // The direct parent status is authoritative; in_reply_to_account_id can
        // refer to someone earlier in a self-reply chain.
        self.reply_targets.remove(&id);
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|client| client.status(&id))
                .and_then(|status| {
                    if status.account.id.is_empty() {
                        anyhow::bail!("Reply account is unavailable");
                    }
                    Ok(status.account)
                })
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::ReplyTarget(id, result));
        });
    }
    fn selected_account_id(&self) -> Option<String> {
        let item = self.selected()?;
        let author_count = usize::from(!item.account_id.is_empty());
        if self.detail_index < author_count {
            Some(item.account_id.clone())
        } else if self.detail_index == author_count {
            item.reply_to_id
                .as_ref()
                .and_then(|post_id| self.reply_targets.get(post_id))
                .and_then(|result| result.as_ref().ok())
                .map(|account| account.id.clone())
        } else {
            None
        }
    }
    fn open_profile(&mut self) {
        let Some(id) = self.selected_account_id() else {
            self.status = "Select a loaded account to view its profile.".into();
            return;
        };
        if let Some(profile) = self.profile.take() {
            self.profile_history.push(profile);
        }
        self.profile_generation = self.profile_generation.wrapping_add(1);
        let generation = self.profile_generation;
        self.profile = Some(Profile {
            account: None,
            account_error: None,
            posts: vec![],
            posts_error: None,
            loading_posts: false,
            has_more: true,
            scroll: 0,
            selected: 0,
            id: id.clone(),
            next_id: None,
            generation,
        });
        self.mode = Mode::Profile;
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|c| c.account(&id))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::ProfileAccount(generation, result));
        });
        self.load_profile_posts();
    }
    fn load_profile_posts(&mut self) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        if profile.loading_posts || !profile.has_more {
            return;
        }
        profile.loading_posts = true;
        profile.posts_error = None;
        let id = profile.id.clone();
        let max_id = profile.next_id.clone();
        let generation = profile.generation;
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|c| c.account_statuses(&id, max_id.as_deref()))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::ProfilePosts(generation, result));
        });
    }
    fn follow_selected(&mut self) {
        let Some(id) = self.selected_account_id() else {
            self.status = "Select a loaded account to follow.".into();
            return;
        };
        if !self.follow_requests.insert(id.clone()) {
            self.status = "Follow request is already in progress.".into();
            return;
        }
        self.follow_results.remove(&id);
        self.status = "Following…".into();
        let config = self.config.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = Mastodon::new(config)
                .and_then(|client| client.follow(&id))
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Follow(id, result));
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
                        let reply_id = self.reply_draft.as_ref().map(|d| d.id.clone());
                        let direct_reply = self
                            .reply_draft
                            .as_ref()
                            .is_some_and(|d| d.original_visibility == "direct");
                        self.input.clear();
                        self.input_cursor = 0;
                        self.reply_draft = None;
                        self.attachments.clear();
                        self.mode = self.compose_return;
                        if let Some(id) = reply_id {
                            self.refresh_after_reply(&id);
                        }
                        self.refresh(Kind::Home);
                        if direct_reply && self.tabs.iter().any(|tab| tab.kind == Kind::Direct) {
                            self.refresh(Kind::Direct);
                        }
                        self.status = "Posted.".into();
                    }
                    Err(e) => self.status = format!("Post failed (draft retained): {e}"),
                }
            }
            Message::ReplyIdentity(generation, result) => {
                let Some(draft) = self
                    .reply_draft
                    .as_mut()
                    .filter(|d| d.generation == generation)
                else {
                    return;
                };
                let identity = result.as_ref().ok();
                let preferred = identity
                    .and_then(|a| a.source.as_ref())
                    .map(|s| s.privacy.as_str());
                draft.visibility =
                    Some(reply_visibility(&draft.original_visibility, preferred).into());
                let own_id = identity.map(|a| a.id.as_str()).unwrap_or("");
                let own_acct = identity.map(|a| a.acct.as_str()).unwrap_or("");
                let mut seen = HashSet::new();
                let mentions = draft
                    .recipients
                    .iter()
                    .filter_map(|(id, acct)| {
                        let acct = acct.trim_start_matches('@');
                        if acct.is_empty()
                            || !acct
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || "_-.@".contains(c))
                            || (!own_id.is_empty() && id == own_id)
                            || (!own_acct.is_empty() && acct.eq_ignore_ascii_case(own_acct))
                            || !seen.insert(acct.to_ascii_lowercase())
                        {
                            return None;
                        }
                        Some(format!("@{acct}"))
                    })
                    .collect::<Vec<_>>();
                if !mentions.is_empty() {
                    draft.mention_prefix = format!("{} ", mentions.join(" "));
                    self.input.insert_str(0, &draft.mention_prefix);
                    self.input_cursor += draft.mention_prefix.len();
                }
                self.status = if result.is_err() || preferred.is_none() {
                    "Could not read default visibility; reply is direct for safety.".into()
                } else {
                    "Reply ready. Review recipients and visibility before sending.".into()
                };
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
                        self.settings_message = match links::open(&self.auth_url) {
                            Ok(()) => "Opening browser; paste the authorization code below.".into(),
                            Err(e) => {
                                format!("Could not open browser: {e:#}. Use Ctrl-O or Ctrl-Y.")
                            }
                        };
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
            Message::AuthUrlCopied(attempt, result)
                if attempt == self.auth_attempt
                    && self.mode == Mode::AccountAdd
                    && self.account_add_step == 1 =>
            {
                self.settings_message = match result {
                    Ok(()) => "Full authorization URL copied.".into(),
                    Err(e) => format!("Could not copy URL: {e}"),
                };
            }
            Message::AuthRegistered(_, _)
            | Message::AuthVerified(_, _)
            | Message::AuthUrlCopied(_, _) => {}
            Message::Identity(Ok(account)) => self
                .pending_actions
                .push_back(WorkspaceAction::Upgrade(account)),
            Message::Identity(Err(_)) => {}
            Message::ReplyTarget(id, result) => {
                self.reply_requests.remove(&id);
                self.reply_targets.insert(id, result);
            }
            Message::ThreadLoaded(id, generation, result) => {
                if let Some(state) = self
                    .threads
                    .get_mut(&id)
                    .filter(|s| s.generation == generation)
                {
                    state.loading = false;
                    match result {
                        Ok(statuses) => {
                            state.truncated = statuses.len() > 500;
                            state.entries = thread_entries(&id, statuses);
                            state.selected = state
                                .pending_selection
                                .take()
                                .and_then(|selected| {
                                    state
                                        .entries
                                        .iter()
                                        .position(|entry| entry.item.id == selected)
                                })
                                .unwrap_or(0);
                        }
                        Err(error) => state.error = Some(error),
                    }
                }
            }
            Message::ProfileAccount(generation, result) => {
                if let Some(profile) = self
                    .profile
                    .as_mut()
                    .filter(|p| p.generation == generation)
                    .or_else(|| {
                        self.profile_history
                            .iter_mut()
                            .find(|p| p.generation == generation)
                    })
                {
                    match result {
                        Ok(account) => profile.account = Some(account),
                        Err(error) => profile.account_error = Some(error),
                    }
                }
            }
            Message::ProfilePosts(generation, result) => {
                if let Some(profile) = self
                    .profile
                    .as_mut()
                    .filter(|p| p.generation == generation)
                    .or_else(|| {
                        self.profile_history
                            .iter_mut()
                            .find(|p| p.generation == generation)
                    })
                {
                    profile.loading_posts = false;
                    match result {
                        Ok(statuses) => {
                            let next_id = statuses
                                .last()
                                .map(|s| s.id.clone())
                                .filter(|id| !id.is_empty());
                            profile.has_more = !statuses.is_empty()
                                && next_id.is_some()
                                && next_id != profile.next_id;
                            profile.next_id = next_id;
                            let mut seen: HashSet<String> =
                                profile.posts.iter().map(|p| p.key.clone()).collect();
                            profile.posts.extend(
                                statuses
                                    .into_iter()
                                    .filter(|s| seen.insert(s.id.clone()))
                                    .map(|s| view(s, String::new())),
                            );
                        }
                        Err(error) => profile.posts_error = Some(error),
                    }
                }
            }
            Message::Follow(id, result) => {
                self.follow_requests.remove(&id);
                let result = match result {
                    Ok(relationship) if relationship.following => "Following".into(),
                    Ok(relationship) if relationship.requested => "Follow requested".into(),
                    Ok(_) => "Follow returned no confirmation".into(),
                    Err(e) => format!("Follow failed: {e}. If permission is missing, re-add this account in Settings to reauthorize."),
                };
                self.status = result.clone();
                self.follow_results.insert(id, result);
            }
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
                    if let Some(item) = self.selected().cloned() {
                        self.open_detail_item(item, Mode::Normal);
                    }
                }
                KeyCode::Char('a') => self.mode = Mode::AddTab,
                KeyCode::Char('c') => {
                    self.resume_draft();
                }
                KeyCode::Char('R') => {
                    if let Some(item) = self.selected().cloned() {
                        self.start_reply(item);
                    }
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
                KeyCode::Char('h') | KeyCode::Left => self.move_focus(false),
                KeyCode::Char('l') | KeyCode::Right => self.move_focus(true),
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
                    self.active = self.ensure_tab(kind);
                    self.remember_left_timeline();
                    self.mode = Mode::Normal;
                    self.update_preview();
                } else if key.code == KeyCode::Esc {
                    self.mode = Mode::Normal;
                }
            }
            Mode::Settings => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Normal,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.settings_index = (self.settings_index + 1).min(4)
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.settings_index = self.settings_index.saturating_sub(1)
                }
                KeyCode::Char('0') if self.settings_index == 3 => self.font_candidate = None,
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
                            self.settings.layout = if forward {
                                self.settings.layout.next()
                            } else {
                                self.settings.layout.previous()
                            };
                        }
                        3 => {
                            self.font_candidate = Some(match (self.font_candidate, forward) {
                                (None, true) => 10,
                                (None, false) => 9,
                                (Some(size), true) => size.saturating_add(1).min(24),
                                (Some(size), false) => size.saturating_sub(1).max(8),
                            });
                        }
                        _ => {}
                    }
                    if self.settings_index < 3 {
                        self.settings_message = match self.settings.save() {
                            Ok(()) => "Settings saved.".into(),
                            Err(e) => format!("Could not save settings: {e:#}"),
                        };
                        self.status = self.settings_message.clone();
                    }
                }
                KeyCode::Enter if self.settings_index == 4 => {
                    self.settings_account_index = self.active_account;
                    self.settings_message.clear();
                    self.mode = Mode::AccountManage;
                }
                KeyCode::Enter if self.settings_index == 3 => {
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
                    self.settings_index = 4;
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
                } else if !self.account_busy
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('y')
                    && self.account_add_step == 1
                {
                    let url = self.auth_url.clone();
                    let attempt = self.auth_attempt;
                    let tx = self.tx.clone();
                    self.settings_message = "Copying authorization URL…".into();
                    thread::spawn(move || {
                        let result = links::copy_to_clipboard(&url).map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Message::AuthUrlCopied(attempt, result));
                    });
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
                    .map(|item| {
                        item.detail_account_count() + item.attachments.len() + item.links.len()
                    })
                    .unwrap_or(0);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.close_detail(),
                    KeyCode::Char('j') => {
                        self.detail_index = (self.detail_index + 1).min(count.saturating_sub(1))
                    }
                    KeyCode::Char('k') => self.detail_index = self.detail_index.saturating_sub(1),
                    KeyCode::Down => self.detail_scroll = self.detail_scroll.saturating_add(1),
                    KeyCode::Up => self.detail_scroll = self.detail_scroll.saturating_sub(1),
                    KeyCode::PageDown => self.detail_scroll = self.detail_scroll.saturating_add(10),
                    KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
                    KeyCode::Char('t') => {
                        if let Some(id) = self
                            .selected()
                            .map(|item| item.id.clone())
                            .filter(|id| !id.is_empty())
                        {
                            self.load_thread(false);
                            self.thread_stack.push(id);
                            self.mode = Mode::Thread;
                        }
                    }
                    KeyCode::Char('r') => self.load_thread(true),
                    KeyCode::Char('f') => self.follow_selected(),
                    KeyCode::Char('c') => self.resume_draft(),
                    KeyCode::Char('R') => {
                        if let Some(item) = self.selected().cloned() {
                            self.start_reply(item);
                        }
                    }
                    KeyCode::Enter
                        if self.selected().is_some_and(|item| {
                            self.detail_index < item.detail_account_count()
                        }) =>
                    {
                        self.open_profile()
                    }
                    KeyCode::Enter | KeyCode::Char('o') => {
                        let url = self.selected().and_then(|item| {
                            let index =
                                self.detail_index.checked_sub(item.detail_account_count())?;
                            if index < item.attachments.len() {
                                item.attachments.get(index).map(|a| a.url.clone())
                            } else {
                                item.links.get(index - item.attachments.len()).cloned()
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
            Mode::Thread => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.thread_stack.pop();
                    self.mode = Mode::Detail;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(state) = self
                        .thread_stack
                        .last()
                        .and_then(|id| self.threads.get_mut(id))
                    {
                        state.selected =
                            (state.selected + 1).min(state.entries.len().saturating_sub(1));
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(state) = self
                        .thread_stack
                        .last()
                        .and_then(|id| self.threads.get_mut(id))
                    {
                        state.selected = state.selected.saturating_sub(1);
                    }
                }
                KeyCode::Enter => {
                    if let Some(item) = self.selected_thread_item() {
                        self.open_detail_item(item, Mode::Thread);
                    }
                }
                KeyCode::Char('R') => {
                    if let Some(item) = self.selected_thread_item() {
                        self.start_reply(item);
                    }
                }
                KeyCode::Char('c') => self.resume_draft(),
                KeyCode::Char('r') => self.load_thread(true),
                _ => {}
            },
            Mode::Profile => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.mode = Mode::Detail;
                    self.profile = self.profile_history.pop();
                }
                KeyCode::Char('j') => {
                    if let Some(p) = &mut self.profile {
                        p.selected = (p.selected + 1).min(p.posts.len().saturating_sub(1));
                    }
                }
                KeyCode::Char('k') => {
                    if let Some(p) = &mut self.profile {
                        p.selected = p.selected.saturating_sub(1);
                    }
                }
                KeyCode::Enter => {
                    if let Some(item) = self
                        .profile
                        .as_ref()
                        .and_then(|p| p.posts.get(p.selected))
                        .cloned()
                    {
                        self.open_detail_item(item, Mode::Profile);
                    }
                }
                KeyCode::Char('R') => {
                    if let Some(item) = self
                        .profile
                        .as_ref()
                        .and_then(|p| p.posts.get(p.selected))
                        .cloned()
                    {
                        self.start_reply(item);
                    }
                }
                KeyCode::Char('c') => self.resume_draft(),
                KeyCode::Down => {
                    if let Some(p) = &mut self.profile {
                        p.scroll = p.scroll.saturating_add(1);
                    }
                }
                KeyCode::Up => {
                    if let Some(p) = &mut self.profile {
                        p.scroll = p.scroll.saturating_sub(1);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(p) = &mut self.profile {
                        p.scroll = p.scroll.saturating_add(10);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(p) = &mut self.profile {
                        p.scroll = p.scroll.saturating_sub(10);
                    }
                }
                KeyCode::Char('n') => self.load_profile_posts(),
                _ => {}
            },
            Mode::Compose => {
                let discard_key =
                    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d');
                if !discard_key {
                    self.discard_armed = false;
                }
                if discard_key && !self.busy {
                    if self.discard_armed {
                        self.input.clear();
                        self.input_cursor = 0;
                        self.attachments.clear();
                        self.reply_draft = None;
                        self.discard_armed = false;
                        self.mode = self.compose_return;
                        self.status = "Draft discarded.".into();
                    } else {
                        self.discard_armed = true;
                        self.status =
                            "Press Ctrl-D again to discard this draft and its attachments.".into();
                    }
                } else if key.code == KeyCode::Esc && !self.busy {
                    self.mode = self.compose_return;
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
                    if self
                        .reply_draft
                        .as_ref()
                        .is_some_and(|d| d.visibility.is_none())
                    {
                        self.status = "Still checking reply visibility and recipients.".into();
                    } else if (self.input.trim().is_empty()
                        || self.reply_draft.as_ref().is_some_and(|d| {
                            !d.mention_prefix.is_empty()
                                && self.input.trim() == d.mention_prefix.trim()
                        }))
                        && self.attachments.is_empty()
                    {
                        self.status = "Write a post or attach an image first.".into();
                    } else {
                        self.busy = true;
                        self.status = "Uploading and posting…".into();
                        let tx = self.tx.clone();
                        let config = self.config.clone();
                        let text = self.input.clone();
                        let attachments = self.attachments.clone();
                        let reply_to = self.reply_draft.as_ref().map(|target| target.id.clone());
                        let visibility = self
                            .reply_draft
                            .as_ref()
                            .and_then(|target| target.visibility.clone());
                        thread::spawn(move || {
                            let result = (|| -> Result<()> {
                                let client = Mastodon::new(config)?;
                                let mut ids = Vec::new();
                                for bytes in attachments {
                                    ids.push(client.upload_media(bytes, "clipboard.png")?.id);
                                }
                                client.post(
                                    &text,
                                    &ids,
                                    reply_to.as_deref(),
                                    visibility.as_deref(),
                                )?;
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
        app.start_stream(StreamKind::User);
        app.sync_layout();
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
                    || !self.sessions[index].app.follow_requests.is_empty()
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
                let has_draft = |app: &App| {
                    !app.input.is_empty()
                        || !app.attachments.is_empty()
                        || app.reply_draft.is_some()
                };
                if discarded.iter().any(|&i| {
                    self.sessions[i].app.busy
                        || self.sessions[i]
                            .app
                            .reply_draft
                            .as_ref()
                            .is_some_and(|d| d.visibility.is_none())
                        || self.sessions[i].app.account_busy
                        || !self.sessions[i].app.pending_interactions.is_empty()
                        || !self.sessions[i].app.follow_requests.is_empty()
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
                        || self.sessions[i]
                            .app
                            .reply_draft
                            .as_ref()
                            .is_some_and(|d| d.visibility.is_none())
                        || !self.sessions[i].app.pending_interactions.is_empty()
                        || !self.sessions[i].app.follow_requests.is_empty()
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
                        app.reply_draft.take(),
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
                            replacement.app.left_timeline = previous.app.left_timeline;
                            replacement.app.input = std::mem::take(&mut previous.app.input);
                            replacement.app.input_cursor = previous.app.input_cursor;
                            replacement.app.attachments =
                                std::mem::take(&mut previous.app.attachments);
                            replacement.app.reply_draft = previous.app.reply_draft.take();
                            for kind in [Kind::Local, Kind::Federated] {
                                if replacement.app.tabs.iter().any(|t| t.kind == kind) {
                                    let stream_kind = if kind == Kind::Local {
                                        StreamKind::Local
                                    } else {
                                        StreamKind::Federated
                                    };
                                    replacement.app.start_stream(stream_kind);
                                    replacement.app.refresh(kind);
                                }
                            }
                        }
                        if let Some((input, cursor, attachments, reply_draft)) =
                            transferred_draft.clone()
                        {
                            replacement.app.input = input;
                            replacement.app.input_cursor = cursor;
                            replacement.app.attachments = attachments;
                            replacement.app.reply_draft = reply_draft;
                        }
                        replacement.app.settings = preferences.clone();
                        replacement.app.sync_layout();
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
                        || self.sessions[source]
                            .app
                            .reply_draft
                            .as_ref()
                            .is_some_and(|d| d.visibility.is_none())
                        || self.sessions[source].app.account_busy
                        || !self.sessions[source].app.pending_actions.is_empty()
                        || !self.sessions[source].app.pending_interactions.is_empty()
                        || !self.sessions[target].app.pending_interactions.is_empty()
                        || !self.sessions[source].app.follow_requests.is_empty()
                        || !self.sessions[target].app.follow_requests.is_empty()
                    {
                        self.error(
                            source,
                            "Wait for pending activity before merging the migrated account.".into(),
                        );
                        return false;
                    }
                    let source_has_draft = !self.sessions[source].app.input.is_empty()
                        || !self.sessions[source].app.attachments.is_empty()
                        || self.sessions[source].app.reply_draft.is_some();
                    let target_has_draft = !self.sessions[target].app.input.is_empty()
                        || !self.sessions[target].app.attachments.is_empty()
                        || self.sessions[target].app.reply_draft.is_some();
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
                        let reply_draft = self.sessions[source].app.reply_draft.take();
                        self.sessions[target].app.input = text;
                        self.sessions[target].app.input_cursor = cursor;
                        self.sessions[target].app.attachments = attachments;
                        self.sessions[target].app.reply_draft = reply_draft;
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
        for (index, session) in workspace.sessions.iter_mut().enumerate() {
            for _ in 0..100 {
                match session.rx.try_recv() {
                    Ok(msg) => {
                        let was_composing = session.app.mode == Mode::Compose;
                        session.app.handle(msg);
                        if index == workspace.active
                            && was_composing
                            && session.app.mode != Mode::Compose
                        {
                            crate::ime::request_direct_input();
                        }
                    }
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
                    let was_composing = workspace.sessions[active].app.mode == Mode::Compose;
                    if workspace.sessions[active].app.key(key) {
                        break;
                    }
                    if !was_composing && workspace.sessions[active].app.mode == Mode::Compose {
                        crate::ime::cancel_pending();
                    } else if was_composing && workspace.sessions[active].app.mode != Mode::Compose
                    {
                        crate::ime::request_direct_input();
                    }
                    let preferences = workspace.sessions[active].app.settings.clone();
                    for session in &mut workspace.sessions {
                        session.app.settings = preferences.clone();
                        session.app.sync_layout();
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
pub(crate) fn test_add_tab(app: &mut App, title: &str) {
    let kind = match title {
        "Direct messages" => Kind::Direct,
        "Federated" => Kind::Federated,
        "Local" => Kind::Local,
        _ => panic!("unknown test tab"),
    };
    if !app.tabs.iter().any(|tab| tab.kind == kind) {
        app.tabs.push(Tab::new(kind));
    }
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
        account_id: "1".into(),
        reply_to_id: None,
        visibility: "public".into(),
        mentions: vec![],
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
    fn left_timeline_survives_side_focus_and_layout_changes_per_account() {
        let mut app = test_app();
        let other_account = test_app();
        test_add_tab(&mut app, "Local");
        test_add_tab(&mut app, "Federated");
        test_add_tab(&mut app, "Direct messages");
        let local = app.tabs.iter().position(|t| t.kind == Kind::Local).unwrap();
        let federated = app
            .tabs
            .iter()
            .position(|t| t.kind == Kind::Federated)
            .unwrap();
        let notifications = app
            .tabs
            .iter()
            .position(|t| t.kind == Kind::Notifications)
            .unwrap();
        let direct = app
            .tabs
            .iter()
            .position(|t| t.kind == Kind::Direct)
            .unwrap();

        app.active = local;
        app.move_focus(true);
        app.move_focus(true);
        assert_eq!(app.active, notifications);
        // The last visited left timeline was Federated.
        assert_eq!(app.left_timeline_index(), Some(federated));
        app.move_focus(false);
        app.move_focus(false);
        assert_eq!(app.active, local);
        // Skip over Federated to the side panes, then return to Local and change layouts.
        app.move_focus(true);
        app.move_focus(true);
        app.move_focus(true);
        assert_eq!(app.active, direct);
        app.settings.layout = settings::Layout::Federation;
        app.started_streams.insert(StreamKind::Federated);
        app.sync_layout();
        // Federated now belongs on the right, so it cannot also be rendered on the left.
        assert_eq!(app.left_timeline_index(), Some(0));
        assert_eq!(app.active, 0);
        app.active = local;
        app.move_focus(true);
        assert_eq!(app.active, federated);
        assert_eq!(app.left_timeline_index(), Some(local));
        app.move_focus(true);
        assert_eq!(app.active, notifications);
        assert_eq!(app.left_timeline_index(), Some(local));
        assert_eq!(other_account.left_timeline_index(), Some(0));
    }
    #[test]
    fn conversation_updates_replace_one_thread_and_preserve_selected_thread() {
        let make = |conversation_id: &str, status_id: &str| Conversation {
            id: conversation_id.into(),
            unread: true,
            last_status: Some(Status {
                id: status_id.into(),
                visibility: "direct".into(),
                ..Default::default()
            }),
        };
        let mut tab = Tab::new(Kind::Direct);
        apply_event(&mut tab, &StreamEvent::Conversation(make("a", "1")));
        apply_event(&mut tab, &StreamEvent::Conversation(make("b", "2")));
        tab.selected = 1;
        apply_event(&mut tab, &StreamEvent::Conversation(make("b", "3")));
        assert_eq!(tab.items.len(), 2);
        assert_eq!(tab.items[0].id, "3");
        assert_eq!(tab.items[tab.selected].id, "1");
        assert!(tab.items[0].context.contains("unread"));
        apply_event(
            &mut tab,
            &StreamEvent::Conversation(Conversation {
                id: "a".into(),
                ..Default::default()
            }),
        );
        assert_eq!(tab.items.len(), 1);
        assert_eq!(tab.items[tab.selected].id, "3");
        assert!(event_applies(
            Kind::Direct,
            StreamKind::Direct,
            &StreamEvent::Conversation(make("a", "1"))
        ));
        assert!(!event_applies(
            Kind::Direct,
            StreamKind::User,
            &StreamEvent::Conversation(make("a", "1"))
        ));
    }

    #[test]
    fn focus_visits_visible_panels_and_skips_hidden_ones() {
        let mut app = test_app();
        test_add_tab(&mut app, "Direct messages");
        test_add_tab(&mut app, "Federated");
        app.settings.layout = settings::Layout::Default;
        app.move_focus(true);
        assert_eq!(app.tabs[app.active].kind, Kind::Federated);
        app.move_focus(true);
        assert_eq!(app.tabs[app.active].kind, Kind::Notifications);
        app.move_focus(true);
        assert_eq!(app.tabs[app.active].kind, Kind::Direct);
        app.settings.layout = settings::Layout::YourTimelineFocus;
        app.sync_layout();
        assert_eq!(app.tabs[app.active].kind, Kind::Federated);
        app.settings.layout = settings::Layout::Federation;
        app.active = 0;
        app.move_focus(true);
        assert_eq!(app.tabs[app.active].kind, Kind::Federated);
        app.move_focus(true);
        assert_eq!(app.tabs[app.active].kind, Kind::Notifications);
    }
    #[test]
    fn reply_visibility_never_exceeds_original_or_account_preference() {
        let values = ["direct", "private", "unlisted", "public"];
        for (original_rank, original) in values.iter().enumerate() {
            for (preferred_rank, preferred) in values.iter().enumerate() {
                assert_eq!(
                    reply_visibility(original, Some(preferred)),
                    values[original_rank.min(preferred_rank)]
                );
            }
        }
        assert_eq!(reply_visibility("public", None), "direct");
        assert_eq!(
            reply_visibility("future-visibility", Some("public")),
            "direct"
        );
    }

    #[test]
    fn reply_draft_preserves_target_and_prevents_accidental_retargeting() {
        let mut app = test_app();
        let mut original = app.tabs[0].items[0].clone();
        original.mentions = vec![
            api::Mention {
                id: "me".into(),
                acct: "me".into(),
            },
            api::Mention {
                id: "2".into(),
                acct: "bob@example.com".into(),
            },
            api::Mention {
                id: "2".into(),
                acct: "bob@example.com".into(),
            },
        ];
        app.mode = Mode::Detail;
        app.start_reply(original.clone());
        app.insert_text("こんにちは");
        let generation = app.reply_draft.as_ref().unwrap().generation;
        app.handle(Message::ReplyIdentity(
            generation,
            Ok(Account {
                id: "me".into(),
                acct: "me".into(),
                source: Some(api::AccountSource {
                    privacy: "private".into(),
                }),
                ..Account::default()
            }),
        ));
        assert_eq!(app.input, "@alice@example.com @bob@example.com こんにちは");
        assert_eq!(app.input_cursor, app.input.len());
        assert_eq!(
            app.reply_draft.as_ref().unwrap().visibility.as_deref(),
            Some("private")
        );
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.mode == Mode::Detail);
        let mut other = original.clone();
        other.id = "other".into();
        app.start_reply(other);
        assert!(app.mode == Mode::Detail);
        assert_eq!(app.reply_draft.as_ref().unwrap().id, "1");
        app.start_reply(original);
        assert!(app.mode == Mode::Compose);
        assert!(app.input.ends_with("こんにちは"));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.mode == Mode::Compose);
        assert_eq!(app.reply_draft.as_ref().unwrap().id, "1");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.mode == Mode::Detail);
    }

    #[test]
    fn stale_identity_does_not_change_new_reply() {
        let mut app = test_app();
        let original = app.tabs[0].items[0].clone();
        app.start_reply(original.clone());
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let mut other = original;
        other.id = "other".into();
        app.start_reply(other);
        app.handle(Message::ReplyIdentity(1, Err("late".into())));
        assert_eq!(app.reply_draft.as_ref().unwrap().id, "other");
        assert!(app.reply_draft.as_ref().unwrap().visibility.is_none());
        app.handle(Message::ReplyIdentity(2, Err("offline".into())));
        assert_eq!(
            app.reply_draft.as_ref().unwrap().visibility.as_deref(),
            Some("direct")
        );
    }

    #[test]
    fn reply_to_own_post_does_not_mention_self() {
        let mut app = test_app();
        let mut own_post = app.tabs[0].items[0].clone();
        own_post.account_id = "me".into();
        own_post.acct = "me".into();
        own_post.visibility = "private".into();
        app.start_reply(own_post);
        app.handle(Message::ReplyIdentity(
            1,
            Ok(Account {
                id: "me".into(),
                acct: "me".into(),
                source: Some(api::AccountSource {
                    privacy: "public".into(),
                }),
                ..Account::default()
            }),
        ));
        assert!(app.input.is_empty());
        assert_eq!(
            app.reply_draft.as_ref().unwrap().visibility.as_deref(),
            Some("private")
        );
    }

    #[test]
    fn discarding_a_draft_requires_two_consecutive_actions() {
        let mut app = test_app();
        app.mode = Mode::Compose;
        app.input = "draft".into();
        app.input_cursor = app.input.len();
        app.attachments.push(vec![1]);
        let discard = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        app.key(discard);
        assert_eq!(app.input, "draft");
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.key(discard);
        assert_eq!(app.input, "draft");
        app.key(discard);
        assert!(app.input.is_empty());
        assert!(app.attachments.is_empty());
        assert!(app.mode == Mode::Normal);
    }

    #[test]
    fn replies_are_tree_ordered_and_invalid_graphs_are_bounded() {
        fn post(id: &str, parent: &str) -> Status {
            Status {
                id: id.into(),
                in_reply_to_id: Some(parent.into()),
                ..Default::default()
            }
        }
        let entries = thread_entries(
            "root",
            vec![
                post("grandchild", "first"),
                post("first", "root"),
                post("second", "root"),
                post("first", "root"),
                post("cycle-a", "cycle-b"),
                post("cycle-b", "cycle-a"),
                post("self", "self"),
            ],
        );
        let ids: Vec<_> = entries.iter().map(|entry| entry.item.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "first",
                "grandchild",
                "second",
                "self",
                "cycle-a",
                "cycle-b"
            ]
        );
        assert!(entries[1].prefix.contains("│"));
        assert_eq!(
            thread_entries(
                "root",
                (0..600).map(|i| post(&format!("{i}"), "root")).collect()
            )
            .len(),
            500
        );
        let mut deep = vec![post("0", "root")];
        for n in 1..50 {
            deep.push(post(&n.to_string(), &(n - 1).to_string()));
        }
        let deep = thread_entries("root", deep);
        assert_eq!(deep.len(), 50);
        assert!(deep.iter().all(|entry| entry.prefix.chars().count() <= 27));
    }

    #[test]
    fn nested_thread_detail_returns_to_previous_selection() {
        let mut app = test_app();
        let root = app.tabs[0].items[0].clone();
        app.open_detail_item(root, Mode::Normal);
        let reply = Status {
            id: "child".into(),
            in_reply_to_id: Some("1".into()),
            ..Default::default()
        };
        let generation = app.threads["1"].generation;
        app.handle(Message::ThreadLoaded(
            "1".into(),
            generation,
            Ok(vec![reply]),
        ));
        app.key(KeyEvent::from(KeyCode::Char('t')));
        app.key(KeyEvent::from(KeyCode::Enter));
        assert!(app.mode == Mode::Detail);
        assert_eq!(app.selected().unwrap().id, "child");
        app.key(KeyEvent::from(KeyCode::Char('t')));
        app.key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.selected().unwrap().id, "child");
        app.key(KeyEvent::from(KeyCode::Esc));
        assert!(app.mode == Mode::Thread);
        assert_eq!(app.selected_thread_item().unwrap().id, "child");
        app.key(KeyEvent::from(KeyCode::Esc));
        assert!(app.mode == Mode::Detail);
        assert_eq!(app.selected().unwrap().id, "1");
    }

    #[test]
    fn detail_r_reloads_replies() {
        let mut app = test_app();
        app.open_detail_item(app.tabs[0].items[0].clone(), Mode::Normal);
        let generation = app.threads["1"].generation;
        app.key(KeyEvent::from(KeyCode::Char('r')));
        assert_ne!(app.threads["1"].generation, generation);
    }

    #[test]
    fn thread_refresh_restores_selected_post_after_reorder_and_retry() {
        fn post(id: &str) -> Status {
            Status {
                id: id.into(),
                in_reply_to_id: Some("1".into()),
                ..Default::default()
            }
        }
        let mut app = test_app();
        app.open_detail_item(app.tabs[0].items[0].clone(), Mode::Normal);
        let generation = app.threads["1"].generation;
        app.handle(Message::ThreadLoaded(
            "1".into(),
            generation,
            Ok(vec![post("a"), post("b")]),
        ));
        app.threads.get_mut("1").unwrap().selected = 1;
        app.mode = Mode::Thread;
        app.thread_stack.push("1".into());
        app.refresh_after_reply("b");
        let generation = app.threads["1"].generation;
        app.handle(Message::ThreadLoaded(
            "1".into(),
            generation,
            Err("offline".into()),
        ));
        app.load_thread(true);
        let generation = app.threads["1"].generation;
        app.handle(Message::ThreadLoaded(
            "1".into(),
            generation,
            Ok(vec![post("x"), post("a"), post("b")]),
        ));
        assert_eq!(app.selected_thread_item().unwrap().id, "b");
        app.refresh_after_reply("1");
        let generation = app.threads["1"].generation;
        app.handle(Message::ThreadLoaded(
            "1".into(),
            generation,
            Ok(vec![post("x"), post("a")]),
        ));
        assert_eq!(app.selected_thread_item().unwrap().id, "x");
    }

    #[test]
    fn forced_thread_refresh_rejects_older_inflight_response() {
        let mut app = test_app();
        let root = app.tabs[0].items[0].clone();
        app.open_detail_item(root, Mode::Normal);
        let older = app.threads["1"].generation;
        assert!(app.threads["1"].loading);
        app.load_thread(true);
        let explicit = app.threads["1"].generation;
        assert_ne!(older, explicit);
        app.mode = Mode::Thread;
        app.thread_stack.push("1".into());
        app.refresh_after_reply("child");
        let newer = app.threads["1"].generation;
        assert_ne!(explicit, newer);
        app.handle(Message::ThreadLoaded(
            "1".into(),
            older,
            Ok(vec![Status {
                id: "stale".into(),
                ..Default::default()
            }]),
        ));
        app.handle(Message::ThreadLoaded(
            "1".into(),
            explicit,
            Ok(vec![Status {
                id: "stale".into(),
                ..Default::default()
            }]),
        ));
        assert!(app.threads["1"].loading);
        app.handle(Message::ThreadLoaded(
            "1".into(),
            newer,
            Ok(vec![Status {
                id: "new".into(),
                ..Default::default()
            }]),
        ));
        assert_eq!(app.threads["1"].entries[0].item.id, "new");
    }

    #[test]
    fn thread_responses_cannot_replace_another_request_or_session() {
        let mut first = test_app();
        let second = test_app();
        first.threads.insert(
            "post".into(),
            ThreadState {
                entries: vec![],
                selected: 0,
                loading: true,
                error: None,
                truncated: false,
                pending_selection: None,
                generation: 2,
            },
        );
        first.handle(Message::ThreadLoaded(
            "post".into(),
            1,
            Ok(vec![Status {
                id: "stale".into(),
                ..Default::default()
            }]),
        ));
        assert!(first.threads["post"].loading);
        first.handle(Message::ThreadLoaded(
            "post".into(),
            2,
            Ok(vec![Status {
                id: "reply".into(),
                in_reply_to_id: Some("post".into()),
                ..Default::default()
            }]),
        ));
        assert_eq!(first.threads["post"].entries[0].item.id, "reply");
        assert!(second.threads.is_empty());
    }

    fn profile_fixture(generation: u64) -> Profile {
        Profile {
            account: None,
            account_error: None,
            posts: vec![],
            posts_error: None,
            loading_posts: true,
            has_more: true,
            scroll: 0,
            selected: 0,
            id: "42".into(),
            next_id: None,
            generation,
        }
    }

    #[test]
    fn profile_responses_are_scoped_to_current_view_and_account_session() {
        let mut first = test_app();
        let mut second = test_app();
        first.mode = Mode::Profile;
        second.mode = Mode::Profile;
        first.profile = Some(profile_fixture(2));
        second.profile = Some(profile_fixture(2));
        first.handle(Message::ProfileAccount(
            1,
            Ok(Account {
                acct: "stale".into(),
                ..Default::default()
            }),
        ));
        assert!(first.profile.as_ref().unwrap().account.is_none());
        first.handle(Message::ProfileAccount(
            2,
            Ok(Account {
                acct: "alice".into(),
                ..Default::default()
            }),
        ));
        assert_eq!(
            first
                .profile
                .as_ref()
                .unwrap()
                .account
                .as_ref()
                .unwrap()
                .acct,
            "alice"
        );
        assert!(second.profile.as_ref().unwrap().account.is_none());
        // A response can arrive while a profile post is open in details.
        first.mode = Mode::Detail;
        first.handle(Message::ProfilePosts(
            2,
            Ok(vec![Status {
                id: "late".into(),
                ..Default::default()
            }]),
        ));
        assert_eq!(first.profile.as_ref().unwrap().posts.len(), 1);
        first.profile = None;
        first.handle(Message::ProfilePosts(2, Ok(vec![Status::default()])));
        assert!(first.profile.is_none());
    }

    #[test]
    fn profile_pages_deduplicate_and_handle_short_pages_failures_and_last_page() {
        let mut app = test_app();
        app.mode = Mode::Profile;
        app.profile = Some(profile_fixture(7));
        let post = |id: &str| Status {
            id: id.into(),
            content: format!("<p>{id}</p>"),
            ..Default::default()
        };
        app.handle(Message::ProfilePosts(7, Ok(vec![post("3"), post("2")])));
        let profile = app.profile.as_ref().unwrap();
        assert_eq!(profile.posts.len(), 2);
        assert!(profile.has_more); // Servers may return fewer than the requested limit.
        assert_eq!(profile.next_id.as_deref(), Some("2"));
        app.handle(Message::ProfilePosts(7, Err("offline".into())));
        assert_eq!(
            app.profile.as_ref().unwrap().posts_error.as_deref(),
            Some("offline")
        );
        assert!(app.profile.as_ref().unwrap().has_more);
        app.handle(Message::ProfilePosts(7, Ok(vec![post("2"), post("1")])));
        assert_eq!(app.profile.as_ref().unwrap().posts.len(), 3);
        app.handle(Message::ProfilePosts(7, Ok(vec![post("1")])));
        let profile = app.profile.as_ref().unwrap();
        assert_eq!(profile.posts.len(), 3);
        assert!(!profile.has_more); // A repeated cursor cannot loop indefinitely.
    }

    #[test]
    fn profile_closes_back_to_post_details() {
        let mut app = test_app();
        app.mode = Mode::Profile;
        app.profile = Some(profile_fixture(1));
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.mode == Mode::Detail);
        assert!(app.profile.is_none());
    }

    #[test]
    fn profile_selection_opens_its_post_and_restores_both_views() {
        let mut app = test_app();
        app.open_detail_item(app.tabs[0].items[0].clone(), Mode::Normal);
        app.detail_index = 2;
        app.detail_scroll = 4;
        app.mode = Mode::Profile;
        let mut profile = profile_fixture(1);
        profile.posts = (0..3)
            .map(|i| {
                view(
                    Status {
                        id: format!("profile-{i}"),
                        content: format!("<p>Profile post {i}</p>"),
                        ..Default::default()
                    },
                    String::new(),
                )
            })
            .collect();
        app.profile = Some(profile);
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.mode == Mode::Detail);
        assert_eq!(app.selected().unwrap().id, "profile-1");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.mode == Mode::Profile);
        assert_eq!(app.profile.as_ref().unwrap().selected, 1);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.mode == Mode::Detail);
        assert_eq!(app.selected().unwrap().id, app.tabs[0].items[0].id);
        assert_eq!((app.detail_index, app.detail_scroll), (2, 4));
    }

    #[test]
    fn profile_selection_is_visible_and_user_names_are_coloured_in_all_themes() {
        use ratatui::style::Color;
        for theme in [
            settings::Theme::Dark,
            settings::Theme::Light,
            settings::Theme::HighContrast,
        ] {
            let mut app = test_app();
            app.mode = Mode::Profile;
            app.settings.theme = theme;
            let mut profile = profile_fixture(1);
            profile.account = Some(Account {
                acct: "person@example.com".into(),
                ..Default::default()
            });
            profile.posts = (0..20)
                .map(|i| {
                    view(
                        Status {
                            id: i.to_string(),
                            account: Account {
                                acct: format!("user{i}@example.com"),
                                display_name: format!("User {i}"),
                                ..Default::default()
                            },
                            content: format!("<p>Post {i}</p>"),
                            ..Default::default()
                        },
                        String::new(),
                    )
                })
                .collect();
            profile.selected = 19;
            app.profile = Some(profile);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| crate::ui::draw(frame, &mut app))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let row = (0..buffer.area.height)
                .find(|&y| {
                    let text: String = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    text.contains("User 19")
                })
                .expect("selected post is kept visible");
            let marker = (0..buffer.area.width)
                .find(|&x| buffer[(x, row)].symbol() == "▌")
                .expect("selection marker");
            assert_ne!(buffer[(marker + 2, row)].bg, Color::Reset);
            let name = (marker..buffer.area.width)
                .find(|&x| buffer[(x, row)].symbol() == "U")
                .unwrap();
            assert_ne!(buffer[(name, row)].fg, Color::Reset);
        }
    }

    #[test]
    fn profile_renders_metadata_and_bounds_scroll_at_end() {
        let mut app = test_app();
        app.mode = Mode::Profile;
        let mut profile = profile_fixture(1);
        profile.loading_posts = false;
        profile.account = Some(Account {
            acct: "alice@example.com".into(),
            display_name: "Alice".into(),
            note: "<p>Artist &amp; reader</p>".into(),
            followers_count: 12,
            created_at: "2020-01-01".into(),
            fields: vec![api::ProfileField {
                name: "Website".into(),
                value: "<a href='https://example.com'>Example</a>".into(),
            }],
            ..Default::default()
        });
        profile.posts = (0..20)
            .map(|i| {
                view(
                    Status {
                        id: i.to_string(),
                        content: if i == 1 {
                            format!("<p>{}</p>", "abcdefghijklmnopqrstuv ".repeat(80))
                        } else {
                            format!("<p>Post number {i}</p>")
                        },
                        ..Default::default()
                    },
                    String::new(),
                )
            })
            .collect();
        app.profile = Some(profile);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        let contents = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(contents.contains("Artist & reader"));
        assert!(contents.contains("Website: Example"));
        assert!(contents.contains("12 followers"));
        assert!(contents.contains("Joined: 2020-01-01"));
        assert!(contents.contains("n load 40 more"));
        app.profile.as_mut().unwrap().scroll = u16::MAX;
        app.profile.as_mut().unwrap().selected = 19;
        terminal
            .draw(|frame| crate::ui::draw(frame, &mut app))
            .unwrap();
        assert!(app.profile.as_ref().unwrap().scroll < u16::MAX);
        let contents = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(contents.contains("Post number 19"));
    }
    #[test]
    fn copied_url_result_is_ignored_after_authentication_is_cancelled() {
        let mut app = test_app();
        app.mode = Mode::AccountAdd;
        app.account_add_step = 1;
        app.auth_attempt = 3;
        app.handle(Message::AuthUrlCopied(3, Ok(())));
        assert_eq!(app.settings_message, "Full authorization URL copied.");
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.settings_message = "new screen".into();
        app.handle(Message::AuthUrlCopied(3, Err("old result".into())));
        assert_eq!(app.settings_message, "new screen");
    }

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
    fn detail_targets_the_original_author_and_direct_reply() {
        let original = Status {
            id: "123".into(),
            in_reply_to_id: Some("999".into()),
            account: Account {
                id: "11".into(),
                acct: "alice".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let item = view(
            Status {
                id: "456".into(),
                account: Account {
                    id: "44".into(),
                    acct: "booster".into(),
                    ..Default::default()
                },
                reblog: Some(Box::new(original)),
                ..Default::default()
            },
            String::new(),
        );
        assert_eq!(item.account_id, "11");
        assert_eq!(item.reply_to_id.as_deref(), Some("999"));
        assert_eq!(item.detail_account_count(), 2);
        let mut app = test_app();
        app.tabs[0].items = vec![item];
        app.mode = Mode::Detail;
        app.key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.detail_index, 1);
        app.handle(Message::ReplyTarget(
            "999".into(),
            Ok(Account {
                id: "22".into(),
                acct: "bob".into(),
                ..Default::default()
            }),
        ));
        app.handle(Message::ReplyTarget(
            "other".into(),
            Ok(Account {
                id: "33".into(),
                acct: "mallory".into(),
                ..Default::default()
            }),
        ));
        assert_eq!(app.reply_targets["999"].as_ref().unwrap().id, "22");
        assert_eq!(app.reply_targets["other"].as_ref().unwrap().id, "33");
    }
    #[test]
    fn follow_result_distinguishes_follow_and_request_and_failure() {
        let mut app = test_app();
        app.follow_requests.insert("11".into());
        app.handle(Message::Follow(
            "11".into(),
            Ok(api::Relationship {
                following: true,
                requested: false,
            }),
        ));
        assert!(!app.follow_requests.contains("11"));
        assert_eq!(app.follow_results["11"], "Following");
        app.handle(Message::Follow(
            "22".into(),
            Ok(api::Relationship {
                following: false,
                requested: true,
            }),
        ));
        assert_eq!(app.follow_results["22"], "Follow requested");
        app.handle(Message::Follow("33".into(), Err("403".into())));
        assert!(app.follow_results["33"].contains("reauthorize"));
        assert_eq!(app.follow_results["11"], "Following");
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
