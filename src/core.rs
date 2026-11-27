//! App state: applies nchat events, turns user actions into nchat requests, and builds the
//! Slint models. Everything here runs on the GUI thread except `push_event`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{Datelike, Local, TimeZone};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::ffi::{self, Chat, Contact, Event, EventKind, Message, Profile};
use crate::settings::{self, Settings};
use crate::setup::{Mode, Setup};
use crate::term::TermEvent;
use crate::audio;
use crate::desktop::{self, TrayCmd};
use crate::openwith;
use crate::record;
use crate::notify::{self, NotifyAction};
use slint::winit_030::WinitWindowAccessor;
use crate::{
    emoji, richtext, AccountItem, AppWindow, ChatItem, ConfirmState, EmojiState, InfoState, MsgItem, PickItem,
    PickerState, SettingsState, SetupState, Theme,
};

// ---------------------------------------------------------------------------------------
// Event channel (nchat threads -> GUI thread)
// ---------------------------------------------------------------------------------------

pub enum AppEvent {
    Core(Event),
    CoreExited(i32),
    Term(TermEvent),
    /// An image decoded off the GUI thread (None: not decodable).
    Image(String, Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>),
}

static SENDER: OnceLock<Sender<AppEvent>> = OnceLock::new();

/// Set while the process is about to exec() a fresh copy of itself.
pub static RESTARTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Logs GUI-thread work that took long enough to make the window feel stuck.
pub fn report_slow(what: &str, items: usize, started: Instant) {
    let took = started.elapsed();
    if took >= Duration::from_millis(200) {
        let n = if items > 0 { format!(" ({items} events)") } else { String::new() };
        eprintln!("nchat-ui: slow: {what}{n} took {} ms", took.as_millis());
    }
}

pub fn channel() -> Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel();
    let _ = SENDER.set(tx);
    rx
}

pub fn sender() -> Sender<AppEvent> {
    SENDER.get().expect("channel initialised").clone()
}

/// Called from C++ (nchat backend threads).
pub fn push_event(ev: Event) {
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(AppEvent::Core(ev));
    }
}

// ---------------------------------------------------------------------------------------
// nchat constants (protocol.h ProtocolFeature, status.h Status::Flag)
// ---------------------------------------------------------------------------------------

const FEATURE_LIMITED_REACTIONS: i32 = 1 << 4;
const FEATURE_PIN_CHAT: i32 = 1 << 7;
const FEATURE_ARCHIVE_CHAT: i32 = 1 << 8;
const FEATURE_PIN_MESSAGE: i32 = 1 << 9;
const STATUS_FETCHING: u32 = 1 << 3;
const STATUS_SYNCING: u32 = 1 << 6;


// ---------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------

type ChatKey = (String, String); // (profile id, chat id)

const PAGE: i32 = 40;
const KEY_SEP: char = '\u{1f}';
/// separates "status@broadcast" from a sender in status-view keys
const SENDER_SEP: char = '\u{1e}';
/// WhatsApp puts everyone's status updates (stories) in this one chat
const STATUS_CHAT: &str = "status@broadcast";
/// statuses expire after a day on WhatsApp
const STATUS_WINDOW_MS: i64 = 24 * 3600 * 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChatKind {
    Chat,
    Group,
    Channel,
    Status,
}

// list filters (must match the chips in app.slint)
const FILTER_UNREAD: i32 = 1;
const FILTER_GROUPS: i32 = 2;
const FILTER_ARCHIVED: i32 = 3;
const FILTER_CHANNELS: i32 = 4;
const FILTER_STATUS: i32 = 5;

fn key_str(k: &ChatKey) -> String {
    format!("{}{}{}", k.0, KEY_SEP, k.1)
}

fn key_parse(s: &str) -> Option<ChatKey> {
    let (p, c) = s.split_once(KEY_SEP)?;
    Some((p.to_string(), c.to_string()))
}

#[derive(Default)]
struct ChatState {
    info: Chat,
    msgs: HashMap<String, Message>,
    initial_requested: bool,
    preview_requested: bool,
    older_requested: HashSet<String>,
    all_loaded: bool,
    is_group: Option<bool>,
    /// detected Telegram channel (posts are sent by the chat itself)
    is_channel: bool,
    mark_read_on_load: bool,
}

impl ChatState {
    fn sorted_ids(&self) -> Vec<String> {
        let mut ids: Vec<&Message> = self.msgs.values().collect();
        ids.sort_by(|a, b| a.time_sent.cmp(&b.time_sent).then_with(|| a.id.cmp(&b.id)));
        ids.into_iter().map(|m| m.id.clone()).collect()
    }

    fn newest(&self) -> Option<&Message> {
        self.msgs.values().max_by_key(|m| m.time_sent)
    }

    fn oldest(&self) -> Option<&Message> {
        self.msgs.values().min_by_key(|m| m.time_sent)
    }

    fn last_time(&self) -> i64 {
        self.newest().map(|m| m.time_sent).unwrap_or(0).max(self.info.last_message_time)
    }

    fn unread_count(&self) -> i32 {
        self.msgs.values().filter(|m| !m.is_outgoing && !m.is_read && !m.is_deleted).count() as i32
    }

    fn is_unread(&self) -> bool {
        self.info.is_unread || self.unread_count() > 0
    }
}

#[derive(Clone)]
enum PickerMode {
    NewChat,
    Forward(Box<Message>, String /* source profile */),
    /// file path; items are the applications that can open it
    OpenWith(String, String /* file type */),
}

#[derive(Clone)]
enum Pending {
    SendFiles(ChatKey, Vec<PathBuf>),
    DeleteChat(ChatKey),
    DeleteMessage(ChatKey, String),
    RemoveAccount(String),
    CloseWhileSyncing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Document,
    Image,
    Audio,
    Video,
}

pub struct State {
    window: slint::Weak<AppWindow>,
    started: Instant,
    start_ms: i64,
    profiles: Vec<Profile>,
    features: HashMap<String, i32>,
    connected: HashMap<String, bool>,
    self_ids: HashMap<String, String>,
    contacts: HashMap<ChatKey, Contact>,
    chats: HashMap<ChatKey, ChatState>,
    group_members: HashMap<ChatKey, Vec<String>>,
    current: Option<ChatKey>,
    search: String,
    filter: i32,
    drafts: HashMap<ChatKey, String>,
    reply_to: Option<String>,
    edit_of: Option<String>,
    react_to: Option<String>,
    online: HashMap<ChatKey, (bool, i64)>,
    typing: HashMap<ChatKey, HashMap<String, Instant>>,
    images: HashMap<String, slint::Image>,
    images_pending: HashSet<String>,
    images_failed: HashSet<String>,
    download_requested: HashSet<(ChatKey, String)>,
    last_typing_sent: Option<Instant>,
    core_exited: Option<i32>,
    ready: bool,
    status_flags: u32,
    last_status_poll: Instant,
    info_open: bool,
    picker: Option<PickerMode>,
    pending: Option<Pending>,
    pending_open: Option<ChatKey>,
    highlight: Option<(String, Instant)>,
    scroll_to: Option<String>,
    last_found: Option<String>,
    last_find_text: String,
    find_started: Option<Instant>,
    toast: Option<(String, Instant)>,
    pub close_confirmed: bool,
    new_below: i32,
    /// in the Status tab: whose updates are open
    status_sender: Option<String>,
    /// files dropped on the window, collected until the drop ends
    dropped: Vec<PathBuf>,
    status_requested: HashSet<String>,
    playing: Option<audio::Playing>,
    /// audio message waiting to play (downloading or decoding)
    audio_pending: Option<(ChatKey, String)>,
    audio_speed: f32,
    tray: Option<desktop::Tray>,
    tray_available: bool,
    tray_state: (i32, bool, String),
    sync_msgs: usize,
    sync_chats: HashSet<ChatKey>,
    emoji_tab: usize,
    emoji_search: String,
    /// Some(limited list) when reacting on a service with a fixed reaction set
    emoji_limited: Option<Vec<String>>,
    settings: Settings,
    dirty_list: bool,
    dirty_msgs: bool,
    dirty_info: bool,
    scroll_bottom: bool,
    chat_model: Rc<VecModel<ChatItem>>,
    msg_model: Rc<VecModel<MsgItem>>,
    // process / setup
    pub setup: Setup,
    confdir: PathBuf,
    /// args passed to nchat's main() (without -s / -r)
    core_args: Vec<String>,
    /// our own command line, for restarting (add account / cancel setup)
    restart_args: Vec<String>,
    core_thread: Option<std::thread::JoinHandle<i32>>,
    /// account types this build supports: (nchat protocol name, display name)
    protocols: Vec<(String, String)>,
    dirty_setup: bool,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().map(f))
}

pub struct Launch {
    pub confdir: PathBuf,
    pub core_args: Vec<String>,
    pub restart_args: Vec<String>,
    pub protocols: Vec<(String, String)>,
}

pub fn init(window: &AppWindow, launch: Launch) {
    let chat_model = Rc::new(VecModel::<ChatItem>::default());
    let msg_model = Rc::new(VecModel::<MsgItem>::default());
    window.set_chats(ModelRc::from(chat_model.clone()));
    window.set_messages(ModelRc::from(msg_model.clone()));
    let settings = Settings::load();
    let state = State {
        window: window.as_weak(),
        started: Instant::now(),
        start_ms: Local::now().timestamp_millis(),
        profiles: Vec::new(),
        features: HashMap::new(),
        connected: HashMap::new(),
        self_ids: HashMap::new(),
        contacts: HashMap::new(),
        chats: HashMap::new(),
        group_members: HashMap::new(),
        current: None,
        search: String::new(),
        filter: 0,
        drafts: HashMap::new(),
        reply_to: None,
        edit_of: None,
        react_to: None,
        online: HashMap::new(),
        typing: HashMap::new(),
        images: HashMap::new(),
        images_pending: HashSet::new(),
        images_failed: HashSet::new(),
        download_requested: HashSet::new(),
        last_typing_sent: None,
        core_exited: None,
        ready: false,
        status_flags: 0,
        last_status_poll: Instant::now(),
        info_open: false,
        picker: None,
        pending: None,
        pending_open: None,
        highlight: None,
        scroll_to: None,
        last_found: None,
        last_find_text: String::new(),
        find_started: None,
        toast: None,
        close_confirmed: false,
        new_below: 0,
        status_sender: None,
        dropped: Vec::new(),
        status_requested: HashSet::new(),
        playing: None,
        audio_pending: None,
        audio_speed: 1.0,
        tray: None,
        tray_available: desktop::tray_host_available(),
        tray_state: (-1, false, String::new()),
        sync_msgs: 0,
        sync_chats: HashSet::new(),
        emoji_tab: 1,
        emoji_search: String::new(),
        emoji_limited: None,
        settings,
        dirty_list: true,
        dirty_msgs: true,
        dirty_info: false,
        scroll_bottom: false,
        chat_model,
        msg_model,
        setup: Setup::new(),
        confdir: launch.confdir,
        core_args: launch.core_args,
        restart_args: launch.restart_args,
        core_thread: None,
        protocols: launch.protocols,
        dirty_setup: true,
    };
    STATE.with(|s| *s.borrow_mut() = Some(state));
    with_state(|s| s.apply_settings());
}

// ---------------------------------------------------------------------------------------
// Applying nchat events
// ---------------------------------------------------------------------------------------

impl State {
    pub fn apply(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::CoreExited(code) => {
                self.core_exited = Some(code);
                self.core_thread = None;
                self.dirty_list = true;
                if self.setup.mode == Mode::Remove {
                    // account removed (or removal cancelled): start normally again
                    self.restart(&[]);
                } else if self.setup.mode == Mode::Setup && !self.setup.succeeded {
                    // setup ended without an account: keep the wizard up with the reason
                    self.setup.on_term(
                        TermEvent::Text("\nSetup failed: nchat stopped before the account was added.\n".into()),
                        false,
                    );
                    self.dirty_setup = true;
                } else if !self.profiles.is_empty() {
                    // nchat stopped (SIGTERM, backend asked to exit, ...): close the window too.
                    let _ = slint::quit_event_loop();
                }
            }
            AppEvent::Core(ev) => self.apply_core(ev),
            AppEvent::Image(path, pixels) => {
                self.images_pending.remove(&path);
                match pixels {
                    Some(buf) => {
                        self.images.insert(path, slint::Image::from_rgba8(buf));
                    }
                    None => {
                        self.images_failed.insert(path);
                    }
                }
                self.dirty_msgs = true;
            }
            AppEvent::Term(ev) => {
                let use_code = self.window.upgrade().map_or(false, |w| w.global::<SetupState>().get_use_code());
                if self.setup.on_term(ev, use_code) {
                    self.dirty_setup = true;
                }
            }
        }
    }

    fn apply_core(&mut self, ev: Event) {
        if std::env::var_os("NCHAT_UI_DEBUG").is_some() {
            eprintln!(
                "[event] {:?} profile={} chat={} msg={} success={} flag={} msgs={} chats={} contacts={}",
                ev.kind, ev.profile_id, ev.chat_id, ev.msg_id, ev.success, ev.flag,
                ev.messages.len(), ev.chats.len(), ev.contacts.len()
            );
        }
        let profile = ev.profile_id.clone();
        let is_current = |s: &State, chat: &str| {
            s.current.as_ref().map_or(false, |(p, c)| *p == profile && c == chat)
        };

        match ev.kind {
            EventKind::Ready => {
                self.ready = true;
                self.profiles = ev.profiles;
                for p in &self.profiles {
                    let mut f = 0;
                    for bit in 0..16 {
                        if ffi::has_feature(&p.id, 1 << bit) {
                            f |= 1 << bit;
                        }
                    }
                    self.features.insert(p.id.clone(), f);
                }
                self.dirty_list = true;
                if self.setup.mode == Mode::Setup && self.setup.succeeded {
                    self.setup.hide();
                    self.dirty_setup = true;
                }
            }
            EventKind::Connect => {
                self.connected.insert(profile.clone(), ev.success);
                if ev.success && self.setup.mode == Mode::Reauth {
                    self.setup.hide();
                    self.dirty_setup = true;
                }
                let sid = ffi::self_id(&profile);
                if !sid.is_empty() {
                    self.self_ids.insert(profile.clone(), sid);
                }
                self.dirty_list = true;
            }
            EventKind::Contacts => {
                if ev.full_sync {
                    self.contacts.retain(|k, _| k.0 != profile);
                }
                for c in ev.contacts {
                    if c.is_self {
                        self.self_ids.insert(profile.clone(), c.id.clone());
                    }
                    self.contacts.insert((profile.clone(), c.id.clone()), c);
                }
                self.dirty_list = true;
                self.dirty_msgs = true;
                self.dirty_info = true;
            }
            EventKind::Chats => {
                let mut new_chats = Vec::new();
                for chat in ev.chats {
                    let key = (profile.clone(), chat.id.clone());
                    let entry = self.chats.entry(key.clone()).or_default();
                    entry.info = chat;
                    if !entry.preview_requested && !entry.info.is_archived {
                        new_chats.push(key.clone());
                    }
                    if self.pending_open.as_ref() == Some(&key) {
                        self.pending_open = None;
                        let k = key_str(&key);
                        self.select_chat(&k);
                    }
                }
                // Fetch the newest message of the most recent chats so the list has previews
                // (served from nchat's cache for most backends).
                new_chats.sort_by_key(|k| -self.chats[k].info.last_message_time);
                for key in new_chats.into_iter().take(60) {
                    if let Some(st) = self.chats.get_mut(&key) {
                        st.preview_requested = true;
                    }
                    ffi::get_messages(&key.0, &key.1, "", 1);
                }
                self.dirty_list = true;
                self.dirty_info = true;
            }
            EventKind::Messages => {
                let key = (profile.clone(), ev.chat_id.clone());
                let current = is_current(self, &ev.chat_id);
                if self.status_flags & STATUS_SYNCING != 0 {
                    self.sync_msgs += ev.messages.len();
                    self.sync_chats.insert(key.clone());
                }
                let incoming_new = ev.messages.iter().filter(|m| !m.is_outgoing).count() as i32;
                let outgoing_new = ev.messages.iter().any(|m| m.is_outgoing);
                let (had_newer, mark_now) = {
                    let st = self.chats.entry(key.clone()).or_default();
                    let prev_newest = st.newest().map(|m| m.time_sent).unwrap_or(0);
                    let mut had_newer = false;
                    if ev.messages.is_empty() && !ev.from_msg_id.is_empty() {
                        st.all_loaded = true;
                    }
                    for m in ev.messages {
                        if m.time_sent > prev_newest {
                            had_newer = true;
                        }
                        // Telegram channel posts are sent by the channel itself
                        if !m.is_outgoing && m.sender_id == ev.chat_id && profile.starts_with("Telegram") {
                            st.is_channel = true;
                        }
                        st.msgs.insert(m.id.clone(), m);
                    }
                    (had_newer, std::mem::take(&mut st.mark_read_on_load))
                };
                if self.chats[&key].info.id.is_empty() {
                    // unknown chat: ask the backend for its info, like the TUI does
                    self.chats.get_mut(&key).unwrap().info.id = ev.chat_id.clone();
                    ffi::get_chats(&profile);
                }
                if mark_now {
                    self.mark_chat_read(&key);
                }
                if had_newer && !ev.cached {
                    self.notify_if_needed(&key);
                }
                if current {
                    self.dirty_msgs = true;
                    self.dirty_info = true;
                    if had_newer && self.scroll_to.is_none() {
                        // follow new messages only if already at the bottom (or it's our own)
                        let at_bottom = self.window.upgrade().map_or(true, |w| w.get_at_bottom());
                        if at_bottom || outgoing_new {
                            self.scroll_bottom = true;
                        } else {
                            self.new_below += incoming_new;
                        }
                    }
                }
                self.dirty_list = true;
            }
            EventKind::SendResult => {
                if !ev.success {
                    self.show_toast("Sending failed");
                }
            }
            EventKind::MessageStatus => {
                if let Some(m) = self.msg_mut(&profile, &ev.chat_id, &ev.msg_id) {
                    m.is_read = ev.flag;
                    if is_current(self, &ev.chat_id) {
                        self.dirty_msgs = true;
                    }
                    self.dirty_list = true;
                }
            }
            EventKind::MessageDeleted => {
                let key = (profile.clone(), ev.chat_id.clone());
                if let Some(st) = self.chats.get_mut(&key) {
                    if let Some(m) = st.msgs.get_mut(&ev.msg_id) {
                        m.is_deleted = true;
                        m.text.clear();
                    }
                }
                self.dirty_msgs |= is_current(self, &ev.chat_id);
                self.dirty_list = true;
            }
            EventKind::Typing => {
                let key = (profile.clone(), ev.chat_id.clone());
                let users = self.typing.entry(key).or_default();
                if ev.flag {
                    users.insert(ev.user_id, Instant::now());
                } else {
                    users.remove(&ev.user_id);
                }
                self.dirty_list = true;
            }
            EventKind::UserStatus => {
                self.online.insert((profile, ev.user_id), (ev.flag, ev.time));
                self.dirty_info = true;
                self.dirty_list = true;
            }
            EventKind::MessageFile => {
                if let Some(src) = ev.messages.into_iter().next() {
                    if let Some(m) = self.msg_mut(&profile, &ev.chat_id, &ev.msg_id) {
                        m.has_file = src.has_file;
                        m.file_id = src.file_id;
                        m.file_path = src.file_path;
                        m.file_type = src.file_type;
                        m.file_status = src.file_status;
                        if m.file_status == 1 {
                            let p = m.file_path.clone();
                            self.images_failed.remove(&p);
                            self.images.remove(&p);
                        }
                    }
                    self.dirty_msgs |= is_current(self, &ev.chat_id);
                    let key = (profile.clone(), ev.chat_id.clone());
                    if self.audio_pending.as_ref() == Some(&(key.clone(), ev.msg_id.clone())) {
                        match src.file_status {
                            1 => self.decode_and_play(&key, &ev.msg_id),
                            3 => self.audio_pending = None,
                            _ => {}
                        }
                    }
                }
            }
            EventKind::MessageReactions => {
                if let Some(src) = ev.messages.into_iter().next() {
                    if let Some(m) = self.msg_mut(&profile, &ev.chat_id, &ev.msg_id) {
                        m.reactions = src.reactions;
                    }
                    self.dirty_msgs |= is_current(self, &ev.chat_id);
                }
            }
            EventKind::MessagePinned => {
                if let Some(m) = self.msg_mut(&profile, &ev.chat_id, &ev.msg_id) {
                    m.is_pinned = ev.flag;
                }
                self.dirty_msgs |= is_current(self, &ev.chat_id);
            }
            EventKind::ChatDeleted => {
                let key = (profile, ev.chat_id);
                self.chats.remove(&key);
                if self.current.as_ref() == Some(&key) {
                    self.current = None;
                    self.info_open = false;
                    self.dirty_msgs = true;
                    self.dirty_info = true;
                }
                self.dirty_list = true;
            }
            EventKind::ChatMoved => {
                let old = (profile.clone(), ev.chat_id);
                if let Some(mut st) = self.chats.remove(&old) {
                    st.info.id = ev.new_chat_id.clone();
                    let new = (profile, ev.new_chat_id);
                    if self.current.as_ref() == Some(&old) {
                        self.current = Some(new.clone());
                    }
                    self.chats.insert(new, st);
                }
                self.dirty_list = true;
            }
            EventKind::ChatMuted => {
                if let Some(st) = self.chats.get_mut(&(profile, ev.chat_id)) {
                    st.info.is_muted = ev.flag;
                }
                self.dirty_list = true;
                self.dirty_info = true;
            }
            EventKind::ChatPinned => {
                if let Some(st) = self.chats.get_mut(&(profile, ev.chat_id)) {
                    st.info.is_pinned = ev.flag;
                }
                self.dirty_list = true;
                self.dirty_info = true;
            }
            EventKind::ChatArchived => {
                if let Some(st) = self.chats.get_mut(&(profile, ev.chat_id)) {
                    st.info.is_archived = ev.flag;
                }
                self.dirty_list = true;
                self.dirty_info = true;
            }
            EventKind::Found => {
                if !is_current(self, &ev.chat_id) {
                    return;
                }
                self.find_started = None;
                if ev.msg_id.is_empty() {
                    let msg = if self.last_find_text.is_empty() { "Not found" } else { "No more matches" };
                    self.set_find_status(msg);
                    self.show_toast(msg);
                } else {
                    self.last_found = Some(ev.msg_id.clone());
                    self.set_find_status("");
                    self.focus_message(&ev.msg_id);
                }
            }
            EventKind::AvailableReactions => {
                if self.react_to.as_deref() == Some(ev.msg_id.as_str()) {
                    let emojis: Vec<String> = ev.emojis.into_iter().collect();
                    self.open_emoji_panel(true, if emojis.is_empty() { None } else { Some(emojis) });
                }
            }
            EventKind::GroupMembers => {
                let key = (profile.clone(), ev.chat_id.clone());
                let mut ids = Vec::new();
                for c in ev.contacts {
                    ids.push(c.id.clone());
                    // members may carry names we don't have yet
                    let ck = (profile.clone(), c.id.clone());
                    let known = self.contacts.get(&ck).map_or(false, |k| !k.name.is_empty());
                    if !known && (!c.name.is_empty() || !c.phone.is_empty()) {
                        self.contacts.insert(ck, c);
                    }
                }
                self.group_members.insert(key, ids);
                self.dirty_info = true;
            }
            EventKind::AppExit => {
                let _ = slint::quit_event_loop();
            }
            _ => {}
        }
    }

    fn msg_mut(&mut self, profile: &str, chat: &str, msg: &str) -> Option<&mut Message> {
        self.chats
            .get_mut(&(profile.to_string(), chat.to_string()))
            .and_then(|st| st.msgs.get_mut(msg))
    }

    fn feature(&self, profile: &str, f: i32) -> bool {
        self.features.get(profile).map_or(false, |v| v & f != 0)
    }

    fn is_group(&mut self, key: &ChatKey) -> bool {
        if let Some(g) = self.chats.get(key).and_then(|s| s.is_group) {
            return g;
        }
        let g = ffi::is_group_chat(&key.0, &key.1);
        if let Some(st) = self.chats.get_mut(key) {
            st.is_group = Some(g);
        }
        g
    }

    fn chat_kind(&mut self, key: &ChatKey) -> ChatKind {
        if key.1 == STATUS_CHAT {
            return ChatKind::Status;
        }
        if key.1.ends_with("@newsletter") || self.chats.get(key).map_or(false, |s| s.is_channel) {
            return ChatKind::Channel;
        }
        if self.is_group(key) { ChatKind::Group } else { ChatKind::Chat }
    }

    fn notify_if_needed(&self, key: &ChatKey) {
        if !self.settings.notifications {
            return;
        }
        let Some(st) = self.chats.get(key) else { return };
        if st.info.is_muted || st.info.is_archived || st.is_channel || key.1 == STATUS_CHAT || key.1.ends_with("@newsletter") {
            return; // like WhatsApp: channels and status updates don't notify
        }
        let Some(m) = st.newest() else { return };
        // Skip history synced at startup.
        if m.is_outgoing || m.is_read || m.time_sent < self.start_ms - 15_000 {
            return;
        }
        if self.started.elapsed() < Duration::from_secs(5) {
            return;
        }
        let focused = self.window.upgrade().map_or(false, |w| {
            w.window().with_winit_window(|ww| ww.has_focus()).unwrap_or(false)
        });
        if focused && self.current.as_ref() == Some(key) {
            return;
        }
        let mut title = self.chat_name(key);
        let body = if self.settings.notification_preview {
            if self.is_group_cached(key) {
                title = format!("{title}: {}", self.sender_name(&key.0, m));
            }
            message_preview(m)
        } else {
            "New message".to_string()
        };
        let unread = st.unread_count();
        let body = if unread > 1 { format!("{body}\n({unread} unread messages)") } else { body };
        notify::show(key_str(key), title, body, |key, action| {
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| s.notification_action(&key, action));
            });
        });
    }

    /// dev/test hook: show the notification a new message in this chat would produce
    pub fn test_notification(&mut self, key: &str) {
        let Some(k) = key_parse(key) else { return };
        let title = self.chat_name(&k);
        notify::show(key.to_string(), title, "Test notification from nchat-ui <&>".into(), |key, action| {
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| s.notification_action(&key, action));
            });
        });
    }

    pub fn dismiss_notification(&self, key: &str) {
        notify::dismiss(key);
    }

    /// dev/test hook: inject WhatsApp-shaped channel and status data into the demo account
    /// dev/test hook: an incoming message (optionally a downloaded image) in the open chat
    pub fn inject_incoming(&mut self, text: &str, image: Option<&str>) {
        let Some(key) = self.current.clone() else { return };
        let n = self.chats.get(&key).map_or(0, |c| c.msgs.len());
        let m = Message {
            id: format!("test-in-{n}-{}", Local::now().timestamp_millis()),
            sender_id: key.1.clone(),
            text: text.into(),
            time_sent: Local::now().timestamp_millis(),
            has_file: image.is_some(),
            file_id: if image.is_some() { "test".into() } else { String::new() },
            file_path: image.unwrap_or_default().into(),
            file_type: if image.is_some() { "image/jpeg".into() } else { String::new() },
            file_status: if image.is_some() { 1 } else { -1 },
            ..Default::default()
        };
        let ev = Event {
            kind: EventKind::Messages,
            profile_id: key.0.clone(),
            chat_id: key.1.clone(),
            msg_id: String::new(),
            user_id: String::new(),
            new_chat_id: String::new(),
            from_msg_id: String::new(),
            success: true,
            flag: false,
            cached: false,
            full_sync: false,
            time: 0,
            other_type: 0,
            profiles: Vec::new(),
            contacts: Vec::new(),
            chats: Vec::new(),
            messages: vec![m],
            emojis: Vec::new(),
        };
        self.apply_core(ev);
    }

    pub fn inject_demo_updates(&mut self) {
        let Some(profile) = self.profiles.first().map(|p| p.id.clone()) else { return };
        let now = Local::now().timestamp_millis();
        let ev = |kind: EventKind| Event {
            kind,
            profile_id: profile.clone(),
            chat_id: String::new(),
            msg_id: String::new(),
            user_id: String::new(),
            new_chat_id: String::new(),
            from_msg_id: String::new(),
            success: true,
            flag: false,
            cached: true,
            full_sync: false,
            time: 0,
            other_type: 0,
            profiles: Vec::new(),
            contacts: Vec::new(),
            chats: Vec::new(),
            messages: Vec::new(),
            emojis: Vec::new(),
        };
        let msg = |id: &str, sender: &str, text: &str, ago_min: i64, read: bool| Message {
            id: id.into(),
            sender_id: sender.into(),
            text: text.into(),
            time_sent: now - ago_min * 60_000,
            is_read: read,
            file_status: -1,
            ..Default::default()
        };
        let mut c = ev(EventKind::Contacts);
        c.contacts.push(Contact { id: "120363@newsletter".into(), name: "Rust Weekly".into(), ..Default::default() });
        self.apply_core(c);
        let mut chats = ev(EventKind::Chats);
        chats.chats.push(Chat { id: "120363@newsletter".into(), is_unread: true, last_message_time: now - 5 * 60_000, ..Default::default() });
        chats.chats.push(Chat { id: STATUS_CHAT.into(), last_message_time: now - 10 * 60_000, ..Default::default() });
        self.apply_core(chats);
        let mut m = ev(EventKind::Messages);
        m.chat_id = "120363@newsletter".into();
        m.messages.push(msg("n1", "120363@newsletter", "This week: Rust 1.100 is out 🎉 https://blog.rust-lang.org", 5, false));
        self.apply_core(m);
        let mut st = ev(EventKind::Messages);
        st.chat_id = STATUS_CHAT.into();
        st.messages.push(msg("s1", "Pam_0", "Painting class today 🎨", 50, true));
        st.messages.push(msg("s2", "Pam_0", "Finished! What do you think?", 20, false));
        st.messages.push(msg("s3", "Jim_0", "Big tuna 🐟", 10, false));
        st.messages.push(msg("s4", "Kevin_0", "This one is two days old", 48 * 60, false));
        self.apply_core(st);
    }

    fn is_group_cached(&self, key: &ChatKey) -> bool {
        self.chats.get(key).and_then(|s| s.is_group).unwrap_or(false)
    }

    fn notification_action(&mut self, key: &str, action: NotifyAction) {
        let Some(k) = key_parse(key) else { return };
        match action {
            NotifyAction::MarkRead => self.mark_chat_read(&k),
            NotifyAction::InlineReply(text) => {
                ffi::send_message(&k.0, &k.1, &text, "", "", "", "");
                self.mark_chat_read(&k);
            }
            NotifyAction::Open | NotifyAction::Reply => {
                if self.chats.get(&k).map_or(false, |s| s.info.is_archived) && self.filter != 3 {
                    self.filter = 0;
                }
                self.select_chat(key);
                self.raise_window();
                if let Some(w) = self.window.upgrade() {
                    w.invoke_focus_composer();
                }
            }
        }
    }

    pub fn raise_window(&self) {
        if let Some(w) = self.window.upgrade() {
            let _ = w.show();
            w.window().with_winit_window(|ww| {
                ww.set_minimized(false);
                ww.focus_window();
            });
        }
    }

    fn show_toast(&mut self, text: &str) {
        self.toast = Some((text.to_string(), Instant::now()));
        if let Some(w) = self.window.upgrade() {
            w.set_toast(text.into());
        }
    }

    fn set_find_status(&self, text: &str) {
        if let Some(w) = self.window.upgrade() {
            w.set_find_status(text.into());
        }
    }

    // -----------------------------------------------------------------------------------
    // Names (same rules as nchat's TUI: contact name, else phone, else id)
    // -----------------------------------------------------------------------------------

    fn contact_name(&self, profile: &str, id: &str) -> String {
        if let Some(c) = self.contacts.get(&(profile.to_string(), id.to_string())) {
            if !c.name.trim().is_empty() {
                return c.name.clone();
            }
            if !c.phone.is_empty() {
                return format!("+{}", c.phone);
            }
        }
        // WhatsApp ids: <phone>@s.whatsapp.net (a phone number), <n>@lid (private id, not a
        // phone), <n>@g.us (group). Telegram ids are plain numbers that are NOT phones.
        if let Some(phone) = id.strip_suffix("@s.whatsapp.net") {
            return format!("+{phone}");
        }
        if id.ends_with("@lid") {
            return "Unknown contact".to_string();
        }
        if id.ends_with("@g.us") {
            return "Group".to_string();
        }
        id.to_string()
    }

    fn contact_phone(&self, profile: &str, id: &str) -> String {
        if let Some(c) = self.contacts.get(&(profile.to_string(), id.to_string())) {
            if !c.phone.is_empty() {
                return format!("+{}", c.phone);
            }
        }
        id.strip_suffix("@s.whatsapp.net").map(|p| format!("+{p}")).unwrap_or_default()
    }

    fn chat_name(&self, key: &ChatKey) -> String {
        let name = self.contact_name(&key.0, &key.1);
        if self.self_ids.get(&key.0).map_or(false, |s| *s == key.1) {
            return format!("{name} (You)");
        }
        name
    }

    fn profile_label(&self, profile: &str) -> String {
        let proto = profile.split('_').next().unwrap_or(profile);
        let name = match proto {
            "WhatsAppMd" => "WhatsApp",
            "Telegram" => "Telegram",
            "Signal" => "Signal",
            "Dummy" => "Demo",
            other => other,
        };
        let phone = profile.split_once('_').map(|(_, p)| p).unwrap_or("");
        if self.profiles.iter().filter(|p| p.protocol == proto).count() > 1 && !phone.is_empty() {
            format!("{name} {phone}")
        } else {
            name.to_string()
        }
    }

    fn sender_name(&self, profile: &str, m: &Message) -> String {
        if m.is_outgoing {
            "You".to_string()
        } else {
            self.contact_name(profile, &m.sender_id)
        }
    }

    fn fmt_time(&self, ms: i64) -> String {
        let t = local_time(ms);
        if self.settings.time_24h { t.format("%H:%M").to_string() } else { t.format("%-I:%M %p").to_string() }
    }

    // -----------------------------------------------------------------------------------
    // Model building (called from a timer when dirty)
    // -----------------------------------------------------------------------------------

    pub fn tick(&mut self) {
        // Expire typing indicators after 6 s without update
        let before = self.typing.values().map(|u| u.len()).sum::<usize>();
        for users in self.typing.values_mut() {
            users.retain(|_, t| t.elapsed() < Duration::from_secs(6));
        }
        if self.typing.values().map(|u| u.len()).sum::<usize>() != before {
            self.dirty_list = true;
        }
        // nchat status (syncing history, fetching media, ...)
        if self.last_status_poll.elapsed() > Duration::from_millis(500) {
            self.last_status_poll = Instant::now();
            let mut flags = ffi::status_flags();
            if std::env::var_os("NCHAT_UI_FAKE_SYNC").is_some() {
                flags |= STATUS_SYNCING; // dev: preview the sync indicator
            }
            if flags != self.status_flags {
                self.status_flags = flags;
                self.dirty_list = true;
            }
        }
        if self.find_started.map_or(false, |t| t.elapsed() > Duration::from_secs(6)) {
            self.find_started = None;
            self.set_find_status("No more matches");
        }
        if let Some((_, t)) = &self.highlight {
            if t.elapsed() > Duration::from_millis(2500) {
                self.highlight = None;
                self.dirty_msgs = true;
            }
        }

        let Some(w) = self.window.upgrade() else { return };
        if let Some((_, t)) = &self.toast {
            if t.elapsed() > Duration::from_millis(2500) {
                self.toast = None;
                w.set_toast("".into());
            }
        }
        if self.dirty_setup {
            self.dirty_setup = false;
            self.setup.push_to(&w, &self.protocols);
        }
        if self.dirty_list {
            self.dirty_list = false;
            self.rebuild_list(&w);
            self.rebuild_header(&w);
        }
        if self.dirty_msgs {
            self.dirty_msgs = false;
            self.rebuild_messages(&w);
            self.rebuild_header(&w);
        }
        if self.dirty_info {
            self.dirty_info = false;
            self.rebuild_info(&w);
        }
        if self.scroll_bottom {
            self.scroll_bottom = false;
            self.new_below = 0;
            w.set_scroll_to_bottom_token(w.get_scroll_to_bottom_token() + 1);
        }
        self.update_tray(&w);
        self.tick_audio();
        self.tick_recording(&w);
        if w.get_at_bottom() && self.new_below > 0 {
            self.new_below = 0;
        }
        w.set_new_below(self.new_below);
        if let Some(id) = self.scroll_to.take() {
            let idx = (0..self.msg_model.row_count())
                .find(|i| self.msg_model.row_data(*i).map_or(false, |m| m.id.as_str() == id));
            if let Some(idx) = idx {
                w.set_scroll_to_index(idx as i32);
                w.set_scroll_to_token(w.get_scroll_to_token() + 1);
            }
        }
    }

    pub fn is_syncing(&self) -> bool {
        self.status_flags & STATUS_SYNCING != 0
    }

    fn rebuild_list(&mut self, w: &AppWindow) {
        // connection status
        let status = if let Some(code) = self.core_exited {
            if self.profiles.is_empty() && code == 0 {
                "No accounts set up".to_string()
            } else {
                format!("nchat stopped (code {code}), see log.txt")
            }
        } else if !self.ready {
            "Starting…".to_string()
        } else if self.profiles.is_empty() {
            "No accounts set up".to_string()
        } else if self.status_flags & STATUS_SYNCING != 0 {
            "Syncing history… keep the app open".to_string()
        } else {
            let mut s = self
                .profiles
                .iter()
                .map(|p| {
                    let state = match self.connected.get(&p.id) {
                        Some(true) => "online",
                        Some(false) => "offline",
                        None => "connecting…",
                    };
                    format!("{} {}", self.profile_label(&p.id), state)
                })
                .collect::<Vec<_>>()
                .join(" · ");
            if self.status_flags & STATUS_FETCHING != 0 {
                s.push_str(" · fetching…");
            }
            s
        };
        w.set_syncing(self.is_syncing());
        if self.is_syncing() {
            w.set_sync_detail(
                format!(
                    "Receiving your message history: {} messages in {} chats so far",
                    group_thousands(self.sync_msgs),
                    self.sync_chats.len()
                )
                .into(),
            );
        }
        w.set_connected(!self.connected.is_empty() && self.connected.values().all(|c| *c));
        w.set_connection_status(status.into());
        let banner = if self.core_exited.is_some() && self.profiles.is_empty() {
            "No accounts yet: open ⋮ → Add account".to_string()
        } else {
            String::new()
        };
        w.set_sidebar_banner(banner.into());

        let search = self.search.to_lowercase();
        let multi = self.profiles.len() > 1;
        let keys: Vec<ChatKey> = self.chats.iter().filter(|(_, st)| !st.info.id.is_empty()).map(|(k, _)| k.clone()).collect();
        let mut unread_chats = 0;
        let mut archived_chats = 0;
        let (mut channel_chats, mut channel_unread) = (0, 0);
        let mut visible: Vec<ChatKey> = Vec::new();
        for key in keys {
            let kind = self.chat_kind(&key);
            let (archived, unread) = {
                let st = &self.chats[&key];
                (st.info.is_archived, st.is_unread())
            };
            match kind {
                ChatKind::Status => continue, // shown in the Status tab
                ChatKind::Channel => {
                    channel_chats += 1;
                    channel_unread += unread as i32;
                }
                _ if archived => archived_chats += 1,
                _ if unread => unread_chats += 1,
                _ => {}
            }
            let normal = matches!(kind, ChatKind::Chat | ChatKind::Group);
            let show = match self.filter {
                FILTER_UNREAD => unread && !archived && normal,
                FILTER_GROUPS => !archived && kind == ChatKind::Group,
                FILTER_ARCHIVED => archived,
                FILTER_CHANNELS => kind == ChatKind::Channel && !archived,
                FILTER_STATUS => false,
                _ => normal && (!archived || self.settings.show_archived_in_list),
            };
            if show {
                visible.push(key);
            }
        }
        w.set_channel_chats(channel_chats);
        w.set_channel_unread(channel_unread);
        let (status_people, status_new) = self.status_counts();
        w.set_status_people(status_people);
        w.set_status_new(status_new);
        w.set_unread_chats(unread_chats);
        let unread_total: i32 = self
            .chats
            .values()
            .filter(|c| !c.info.is_archived && !c.info.is_muted && !c.info.id.is_empty() && !c.is_channel)
            .filter(|c| c.info.id != STATUS_CHAT && !c.info.id.ends_with("@newsletter"))
            .map(|c| c.unread_count().max(c.info.is_unread as i32))
            .sum();
        w.set_unread_total(unread_total);
        w.set_archived_chats(archived_chats);
        visible.sort_by(|a, b| {
            let sa = &self.chats[a];
            let sb = &self.chats[b];
            sb.info.is_pinned.cmp(&sa.info.is_pinned).then(sb.last_time().cmp(&sa.last_time())).then(a.1.cmp(&b.1))
        });

        if self.filter == FILTER_STATUS {
            let items = self.status_items(&search, multi);
            set_model(&self.chat_model, items);
            return;
        }
        let mut items = Vec::with_capacity(visible.len());
        for key in &visible {
            let st = &self.chats[key];
            let mut name = self.chat_name(key);
            if !search.is_empty() && !name.to_lowercase().contains(&search) {
                continue;
            }
            if multi {
                name = format!("{name}  · {}", self.profile_label(&key.0));
            }
            let preview = st
                .newest()
                .map(|m| {
                    let p = message_preview(m);
                    if m.is_outgoing { format!("You: {p}") } else { p }
                })
                .unwrap_or_default();
            let unread_count = st.unread_count();
            let typing = self.typing.get(key).map_or(false, |u| !u.is_empty());
            items.push(ChatItem {
                key: key_str(key).into(),
                initials: initials(&name).into(),
                avatar_color: avatar_color(&key.1),
                name: name.into(),
                preview: preview.into(),
                time: format_list_time(st.last_time(), self.settings.time_24h).into(),
                unread: st.is_unread(),
                unread_count,
                muted: st.info.is_muted,
                pinned: st.info.is_pinned,
                archived: st.info.is_archived,
                typing,
                selected: self.current.as_ref() == Some(key) && self.status_sender.is_none(),
                can_pin: self.feature(&key.0, FEATURE_PIN_CHAT),
                can_archive: self.feature(&key.0, FEATURE_ARCHIVE_CHAT),
            });
        }
        set_model(&self.chat_model, items);
    }

    fn rebuild_header(&mut self, w: &AppWindow) {
        let Some(key) = self.current.clone() else {
            w.set_has_current(false);
            return;
        };
        let status_view = key.1 == STATUS_CHAT;
        w.set_can_compose(!status_view);
        let name = match (&self.status_sender, status_view) {
            (Some(sender), true) if self.self_ids.get(&key.0) == Some(sender) => "My status".to_string(),
            (Some(sender), true) => self.contact_name(&key.0, sender),
            _ => self.chat_name(&key),
        };
        w.set_has_current(true);
        w.set_current_initials(initials(&name).into());
        w.set_current_color(avatar_color(&key.1));
        w.set_current_title(name.into());
        w.set_current_can_pin_message(self.feature(&key.0, FEATURE_PIN_MESSAGE));

        let (subtitle, active) = if status_view {
            ("Status updates from the last 24 hours".to_string(), false)
        } else {
            self.chat_subtitle(&key)
        };
        w.set_current_subtitle(subtitle.into());
        w.set_current_subtitle_active(active);

        // reply / edit bar
        let st = self.chats.get(&key);
        if let Some(id) = &self.edit_of {
            let text = st.and_then(|s| s.msgs.get(id)).map(|m| m.text.clone()).unwrap_or_default();
            w.set_editing(true);
            w.set_reply_text(one_line(&text).into());
            w.set_reply_sender("".into());
        } else if let Some(id) = &self.reply_to {
            let (sender, text) = st
                .and_then(|s| s.msgs.get(id))
                .map(|m| (self.sender_name(&key.0, m), message_preview(m)))
                .unwrap_or_default();
            w.set_editing(false);
            w.set_reply_text(one_line(&text).into());
            w.set_reply_sender(sender.into());
        } else {
            w.set_editing(false);
            w.set_reply_text("".into());
            w.set_reply_sender("".into());
        }
    }

    fn chat_subtitle(&mut self, key: &ChatKey) -> (String, bool) {
        let group = self.is_group(key);
        let typers: Vec<String> = self.typing.get(key).map(|u| u.keys().cloned().collect()).unwrap_or_default();
        if !typers.is_empty() {
            if group {
                let names: Vec<String> = typers.iter().map(|t| self.contact_name(&key.0, t)).collect();
                return (format!("{} typing…", names.join(", ")), true);
            }
            return ("typing…".to_string(), true);
        }
        if self.chat_kind(key) == ChatKind::Channel {
            return ("channel".to_string(), false);
        }
        if group {
            let n = self.group_members.get(key).map_or(0, |m| m.len());
            return (if n > 0 { format!("group · {n} members") } else { "group".to_string() }, false);
        }
        match self.online.get(key) {
            Some((true, _)) => ("online".to_string(), true),
            Some((false, t)) if *t > 0 => (format!("last seen {}", format_last_seen(*t)), false),
            _ => (String::new(), false),
        }
    }

    fn rebuild_messages(&mut self, w: &AppWindow) {
        let _ = w;
        let Some(key) = self.current.clone() else {
            set_model(&self.msg_model, Vec::new());
            return;
        };
        let group = self.is_group(&key);
        let can_pin = self.feature(&key.0, FEATURE_PIN_MESSAGE);

        // mark as read what we are now showing (TUI default: mark_read_on_view=1)
        let mut to_mark = Vec::new();
        let mut to_download = Vec::new();
        let auto = self.settings.auto_download.clone();
        if let Some(st) = self.chats.get_mut(&key) {
            for m in st.msgs.values_mut() {
                if self.settings.mark_read_on_view && !m.is_outgoing && !m.is_read {
                    m.is_read = true;
                    to_mark.push((m.sender_id.clone(), m.id.clone()));
                }
                if m.has_file && m.file_status == 0 && !m.file_id.is_empty() {
                    let kind = file_kind(m);
                    let wanted = match auto.as_str() {
                        "all" => true,
                        "media" => matches!(kind, FileKind::Image | FileKind::Audio),
                        "images" => kind == FileKind::Image,
                        _ => false,
                    };
                    if wanted {
                        to_download.push((m.time_sent, m.id.clone(), m.file_id.clone()));
                    }
                }
            }
            if !to_mark.is_empty() {
                st.info.is_unread = false;
                self.dirty_list = true;
            }
        }
        for (sender, id) in to_mark {
            ffi::mark_read(&key.0, &key.1, &sender, &id);
        }
        // newest first, and only a few per update: old WhatsApp media makes nchat ask the
        // phone to re-upload each file (media retry), which shouldn't happen in bulk
        to_download.sort_by(|a, b| b.0.cmp(&a.0));
        let mut started = 0;
        for (_, id, file_id) in to_download {
            if started >= 15 {
                break;
            }
            if self.download_requested.insert((key.clone(), id.clone())) {
                ffi::download_file(&key.0, &key.1, &id, &file_id);
                started += 1;
            }
        }

        // decode downloaded images in the background (cached by path), newest first;
        // until one is ready its message shows as a file
        let mut image_paths: Vec<(i64, String)> = self
            .chats
            .get(&key)
            .map(|st| {
                st.msgs
                    .values()
                    .filter(|m| m.has_file && m.file_status == 1 && file_kind(m) == FileKind::Image)
                    .map(|m| (m.time_sent, m.file_path.clone()))
                    .collect()
            })
            .unwrap_or_default();
        image_paths.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, p) in &image_paths {
            self.request_image(p);
        }

        let highlight = self.highlight.as_ref().map(|(id, _)| id.clone());
        let Some(st) = self.chats.get(&key) else {
            set_model(&self.msg_model, Vec::new());
            return;
        };
        let mut ids = st.sorted_ids();
        if key.1 == STATUS_CHAT {
            let cutoff = Local::now().timestamp_millis() - STATUS_WINDOW_MS;
            let me = self.self_ids.get(&key.0).cloned().unwrap_or_default();
            ids.retain(|id| {
                let m = &st.msgs[id];
                let sender = if m.is_outgoing { me.as_str() } else { m.sender_id.as_str() };
                m.time_sent >= cutoff && self.status_sender.as_deref().map_or(true, |s| s == sender)
            });
        }
        let mut items: Vec<MsgItem> = Vec::with_capacity(ids.len() + 8);
        if !st.all_loaded && ids.len() >= 20 {
            items.push(MsgItem { kind: 2, file_status: -1, ..Default::default() });
        }
        let mut last_day: Option<(i32, u32)> = None;
        let mut prev: Option<&Message> = None;
        for id in &ids {
            let m = &st.msgs[id];
            let dt = local_time(m.time_sent);
            let day = (dt.year(), dt.ordinal());
            let mut group_start = true;
            if last_day != Some(day) {
                items.push(MsgItem { kind: 1, file_status: -1, text: format_day(m.time_sent).into(), ..Default::default() });
                last_day = Some(day);
            } else if let Some(p) = prev {
                group_start = p.sender_id != m.sender_id
                    || p.is_outgoing != m.is_outgoing
                    || (m.time_sent - p.time_sent) > 5 * 60 * 1000;
            }
            prev = Some(m);

            let sender = self.sender_name(&key.0, m);
            let quoted_sender = if m.quoted_id.is_empty() {
                String::new()
            } else if !m.quoted_sender.is_empty() {
                self.contact_name(&key.0, &m.quoted_sender)
            } else {
                st.msgs.get(&m.quoted_id).map(|q| self.sender_name(&key.0, q)).unwrap_or_default()
            };
            let quoted_text = if m.quoted_id.is_empty() {
                String::new()
            } else if !m.quoted_text.is_empty() {
                m.quoted_text.clone()
            } else {
                st.msgs.get(&m.quoted_id).map(message_preview).unwrap_or_else(|| "…".into())
            };
            let kind = file_kind(m);
            let image = if m.has_file && m.file_status == 1 && kind == FileKind::Image {
                self.images.get(&m.file_path).cloned()
            } else {
                None
            };
            let reactions = m
                .reactions
                .iter()
                .map(|r| if r.count > 1 { format!("{} {}", r.emoji, r.count) } else { r.emoji.clone() })
                .collect::<Vec<_>>()
                .join(" ");
            let (file_note, file_available) = file_note(m, kind);
            items.push(MsgItem {
                kind: 0,
                id: m.id.clone().into(),
                text: m.text.clone().into(),
                rich: richtext::styled(&m.text),
                time: self.fmt_time(m.time_sent).into(),
                outgoing: m.is_outgoing,
                status: if m.is_outgoing { if m.is_read { 2 } else { 1 } } else { 0 },
                sender: sender.into(),
                sender_color: avatar_color(&m.sender_id),
                show_sender: (group || key.1 == STATUS_CHAT) && !m.is_outgoing && group_start,
                quoted_id_visible: !m.quoted_id.is_empty(),
                quoted_sender: quoted_sender.into(),
                quoted_text: one_line(&quoted_text).into(),
                has_file: m.has_file,
                file_name: file_label(m, kind).into(),
                file_kind: kind as i32,
                file_status: m.file_status,
                file_note: file_note.into(),
                file_available,
                is_image: image.is_some(),
                audio_state: self.audio_state_of(&key, &m.id),
                audio_progress: self.audio_progress_of(&m.id).0,
                audio_time: self.audio_progress_of(&m.id).1.into(),
                audio_speed: speed_label(self.audio_speed).into(),
                is_sticker: image.as_ref().map_or(false, |img| {
                    // stickers: WebP, square, at most 512 px (WhatsApp uses 512×512)
                    let sz = img.size();
                    is_sticker(m) && sz.width <= 512 && sz.height <= 512
                        && (sz.width as f32 / sz.height.max(1) as f32 - 1.0).abs() < 0.15
                }),
                image: image.unwrap_or_default(),
                reactions: reactions.into(),
                deleted: m.is_deleted,
                edited: m.is_edited,
                pinned: m.is_pinned,
                group_start,
                highlight: highlight.as_deref() == Some(m.id.as_str()),
                has_link: !richtext::links(&m.text).is_empty(),
                can_pin,
            });
        }
        set_model(&self.msg_model, items);
    }

    /// Queues `path` for decoding unless it is decoded, queued or known to fail.
    fn request_image(&mut self, path: &str) {
        if path.is_empty()
            || self.images.contains_key(path)
            || self.images_pending.contains(path)
            || self.images_failed.contains(path)
        {
            return;
        }
        self.images_pending.insert(path.to_string());
        image_worker().send(path.to_string()).ok();
    }

    fn rebuild_info(&mut self, w: &AppWindow) {
        let s = w.global::<InfoState>();
        let Some(key) = self.current.clone().filter(|_| self.info_open) else {
            s.set_visible(false);
            return;
        };
        let name = self.chat_name(&key);
        let group = self.is_group(&key);
        s.set_visible(true);
        s.set_initials(initials(&name).into());
        s.set_avatar_color(avatar_color(&key.1));
        s.set_name(name.into());
        let (subtitle, _) = self.chat_subtitle(&key);
        s.set_subtitle(subtitle.into());
        s.set_phone(if group { String::new() } else { self.contact_phone(&key.0, &key.1) }.into());
        s.set_id(key.1.clone().into());
        s.set_account(self.profile_label(&key.0).into());
        s.set_is_group(group);
        let st = self.chats.get(&key);
        s.set_pinned(st.map_or(false, |c| c.info.is_pinned));
        s.set_archived(st.map_or(false, |c| c.info.is_archived));
        s.set_can_pin(self.feature(&key.0, FEATURE_PIN_CHAT));
        s.set_can_archive(self.feature(&key.0, FEATURE_ARCHIVE_CHAT));
        s.set_can_pin_message(self.feature(&key.0, FEATURE_PIN_MESSAGE));
        s.set_media_count(st.map_or(0, |c| c.msgs.values().filter(|m| m.has_file).count() as i32));
        let mut members: Vec<PickItem> = self
            .group_members
            .get(&key)
            .map(|ids| {
                ids.iter()
                    .map(|id| {
                        let n = if self.self_ids.get(&key.0) == Some(id) { "You".to_string() } else { self.contact_name(&key.0, id) };
                        PickItem {
                            key: key_str(&(key.0.clone(), id.clone())).into(),
                            initials: initials(&n).into(),
                            avatar_color: avatar_color(id),
                            subtitle: self.contact_phone(&key.0, id).into(),
                            name: n.into(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        members.sort_by_key(|m| m.name.to_lowercase());
        s.set_members(ModelRc::from(Rc::new(VecModel::from(members))));
    }

    // -----------------------------------------------------------------------------------
    // User actions: chats
    // -----------------------------------------------------------------------------------

    pub fn select_chat(&mut self, key: &str) {
        let Some(mut key) = key_parse(key) else { return };
        // Status tab rows: "status@broadcast<SENDER_SEP><sender>"
        let sender = match key.1.split_once(SENDER_SEP) {
            Some((chat, sender)) => {
                let sender = sender.to_string();
                key.1 = chat.to_string();
                Some(sender)
            }
            None => None,
        };
        if self.current.as_ref() == Some(&key) && self.status_sender == sender {
            return;
        }
        self.status_sender = sender;
        if record::is_recording() {
            self.record_cancel();
        }
        let w = self.window.upgrade();
        // save draft of the previous chat
        if let (Some(prev), Some(w)) = (self.current.clone(), w.as_ref()) {
            self.drafts.insert(prev, w.get_composer_text().to_string());
        }
        self.current = Some(key.clone());
        self.reply_to = None;
        self.edit_of = None;
        self.last_found = None;
        self.highlight = None;
        self.new_below = 0;
        notify::dismiss(&key_str(&key));
        if let Some(w) = w.as_ref() {
            w.set_composer_text(self.drafts.get(&key).cloned().unwrap_or_default().into());
            w.set_find_visible(false);
            w.set_find_status("".into());
        }

        ffi::set_current_chat(&key.0, &key.1);
        let st = self.chats.entry(key.clone()).or_default();
        if !st.initial_requested {
            st.initial_requested = true;
            ffi::get_messages(&key.0, &key.1, "", PAGE);
        }
        match self.chat_kind(&key) {
            ChatKind::Group => {
                if !self.group_members.contains_key(&key) {
                    ffi::get_group_members(&key.0, &key.1);
                }
            }
            ChatKind::Chat => ffi::get_status(&key.0, &key.1),
            _ => {}
        }
        self.dirty_list = true;
        self.dirty_msgs = true;
        self.dirty_info = true;
        self.scroll_bottom = true;
    }

    pub fn chat_action(&mut self, key: &str, action: &str) {
        let Some(k) = key_parse(key) else { return };
        match action {
            "read" => {
                self.mark_chat_read(&k);
            }
            "pin" | "unpin" => {
                ffi::pin_chat(&k.0, &k.1, action == "pin");
                self.show_toast(if action == "pin" { "Chat pinned" } else { "Chat unpinned" });
            }
            "archive" | "unarchive" => {
                ffi::archive_chat(&k.0, &k.1, action == "archive");
                self.show_toast(if action == "archive" { "Chat archived" } else { "Chat unarchived" });
            }
            "info" => {
                self.select_chat(key);
                self.open_info();
            }
            "delete" => {
                let name = self.chat_name(&k);
                self.confirm(
                    Pending::DeleteChat(k),
                    "Delete chat?",
                    &format!("Delete the chat with {name} from this account? This can't be undone."),
                    "Delete",
                    true,
                );
            }
            _ => {}
        }
    }

    fn mark_chat_read(&mut self, key: &ChatKey) {
        notify::dismiss(&key_str(key));
        let Some(st) = self.chats.get_mut(key) else { return };
        let mut to_mark = Vec::new();
        for m in st.msgs.values_mut() {
            if !m.is_outgoing && !m.is_read {
                m.is_read = true;
                to_mark.push((m.sender_id.clone(), m.id.clone()));
            }
        }
        if to_mark.is_empty() && st.info.is_unread {
            // nothing loaded yet: load the latest messages, then mark them
            st.mark_read_on_load = true;
            ffi::get_messages(&key.0, &key.1, "", 20);
        }
        st.info.is_unread = false;
        for (sender, id) in to_mark {
            ffi::mark_read(&key.0, &key.1, &sender, &id);
        }
        self.dirty_list = true;
        self.dirty_msgs = true;
    }

    pub fn mark_all_read(&mut self) {
        let keys: Vec<ChatKey> = self.chats.iter().filter(|(_, s)| s.is_unread()).map(|(k, _)| k.clone()).collect();
        let n = keys.len();
        for k in keys {
            self.mark_chat_read(&k);
        }
        self.show_toast(&format!("Marked {n} chats as read"));
    }

    pub fn set_filter(&mut self, filter: i32) {
        self.filter = filter;
        self.dirty_list = true;
        if filter == FILTER_STATUS {
            // load everyone's recent status updates
            for p in self.profiles.clone() {
                if self.status_requested.insert(p.id.clone()) {
                    ffi::get_messages(&p.id, STATUS_CHAT, "", 300);
                }
            }
        }
    }

    /// Recent status updates, per profile and sender: (profile, sender) -> messages
    fn status_posts(&self) -> Vec<((String, String), Vec<&Message>)> {
        let cutoff = Local::now().timestamp_millis() - STATUS_WINDOW_MS;
        let mut by: HashMap<(String, String), Vec<&Message>> = HashMap::new();
        for ((profile, chat), st) in &self.chats {
            if chat != STATUS_CHAT {
                continue;
            }
            for m in st.msgs.values() {
                if m.time_sent >= cutoff && !m.is_deleted {
                    let sender = if m.is_outgoing { self.self_ids.get(profile).cloned().unwrap_or_default() } else { m.sender_id.clone() };
                    by.entry((profile.clone(), sender)).or_default().push(m);
                }
            }
        }
        let mut v: Vec<_> = by.into_iter().collect();
        for (_, msgs) in v.iter_mut() {
            msgs.sort_by_key(|m| m.time_sent);
        }
        v.sort_by_key(|(_, msgs)| -msgs.last().map_or(0, |m| m.time_sent));
        v
    }

    fn status_counts(&self) -> (i32, i32) {
        let posts = self.status_posts();
        let new = posts.iter().filter(|(_, msgs)| msgs.iter().any(|m| !m.is_outgoing && !m.is_read)).count();
        (posts.len() as i32, new as i32)
    }

    fn status_items(&self, search: &str, multi: bool) -> Vec<ChatItem> {
        let mut items = Vec::new();
        for ((profile, sender), msgs) in self.status_posts() {
            let mine = msgs.iter().all(|m| m.is_outgoing);
            let mut name = if mine { "My status".to_string() } else { self.contact_name(&profile, &sender) };
            if !search.is_empty() && !name.to_lowercase().contains(search) {
                continue;
            }
            if multi {
                name = format!("{name}  · {}", self.profile_label(&profile));
            }
            let last = msgs.last().unwrap();
            let unseen = msgs.iter().filter(|m| !m.is_outgoing && !m.is_read).count() as i32;
            let count = msgs.len();
            let key = (profile.clone(), format!("{STATUS_CHAT}{SENDER_SEP}{sender}"));
            items.push(ChatItem {
                key: key_str(&key).into(),
                initials: initials(&name).into(),
                avatar_color: avatar_color(&sender),
                name: name.into(),
                preview: format!("{} · {} update{}", message_preview(last), count, if count == 1 { "" } else { "s" }).into(),
                time: format_list_time(last.time_sent, self.settings.time_24h).into(),
                unread: unseen > 0,
                unread_count: unseen,
                selected: self.current.as_ref().map_or(false, |(p, c)| *p == profile && c == STATUS_CHAT)
                    && self.status_sender.as_deref() == Some(sender.as_str()),
                ..Default::default()
            });
        }
        items
    }

    pub fn open_info(&mut self) {
        if self.current.as_ref().map_or(true, |(_, c)| c == STATUS_CHAT) {
            return;
        }
        self.info_open = true;
        if let Some(key) = self.current.clone() {
            if self.is_group(&key) && !self.group_members.contains_key(&key) {
                ffi::get_group_members(&key.0, &key.1);
            }
        }
        self.dirty_info = true;
    }

    pub fn close_info(&mut self) {
        self.info_open = false;
        self.dirty_info = true;
    }

    pub fn info_action(&mut self, action: &str) {
        let Some(key) = self.current.clone() else { return };
        match action {
            "search" => {
                if let Some(w) = self.window.upgrade() {
                    w.set_find_visible(true);
                }
            }
            "jump-pinned" => self.jump_pinned(),
            "media" => {
                let dir = self
                    .chats
                    .get(&key)
                    .and_then(|st| st.msgs.values().filter(|m| m.file_status == 1 && !m.file_path.is_empty()).max_by_key(|m| m.time_sent))
                    .and_then(|m| PathBuf::from(&m.file_path).parent().map(|p| p.to_path_buf()))
                    .unwrap_or_else(|| PathBuf::from(ffi::downloads_dir()));
                let _ = crate::term::xdg_open_command().arg(dir).spawn();
            }
            other => self.chat_action(&key_str(&key), other),
        }
    }

    pub fn member_clicked(&mut self, key: &str) {
        let Some((profile, user)) = key_parse(key) else { return };
        if self.self_ids.get(&profile) == Some(&user) {
            return;
        }
        self.open_or_create_chat((profile, user));
    }

    fn open_or_create_chat(&mut self, key: ChatKey) {
        if self.chats.get(&key).map_or(false, |s| !s.info.id.is_empty()) {
            if self.chats[&key].info.is_archived && self.filter != 3 {
                self.filter = 3;
                if let Some(w) = self.window.upgrade() {
                    w.set_filter(3);
                }
            }
            self.select_chat(&key_str(&key));
        } else {
            self.pending_open = Some(key.clone());
            ffi::create_chat(&key.0, &key.1);
        }
    }

    pub fn load_older(&mut self) {
        let Some(key) = self.current.clone() else { return };
        let Some(st) = self.chats.get_mut(&key) else { return };
        let Some(oldest) = st.oldest().map(|m| m.id.clone()) else { return };
        if st.older_requested.insert(oldest.clone()) {
            ffi::get_messages(&key.0, &key.1, &oldest, PAGE);
        }
    }

    // -----------------------------------------------------------------------------------
    // Search / jump
    // -----------------------------------------------------------------------------------

    fn oldest_id(&self, key: &ChatKey) -> String {
        self.chats.get(key).and_then(|s| s.oldest()).map(|m| m.id.clone()).unwrap_or_default()
    }

    pub fn find(&mut self, text: &str, older: bool) {
        let Some(key) = self.current.clone() else { return };
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if text != self.last_find_text || !older {
            self.last_found = None;
        }
        self.last_find_text = text.clone();

        // 1) loaded messages, newest first, continuing after the previous match
        let needle = text.to_lowercase();
        let local = self.chats.get(&key).and_then(|st| {
            let mut ids = st.sorted_ids();
            ids.reverse();
            let start = match &self.last_found {
                Some(prev) => ids.iter().position(|i| i == prev).map(|p| p + 1).unwrap_or(0),
                None => 0,
            };
            ids[start..].iter().find(|id| st.msgs[*id].text.to_lowercase().contains(&needle)).cloned()
        });
        if let Some(id) = local {
            self.last_found = Some(id.clone());
            self.set_find_status("");
            self.focus_message(&id);
            return;
        }

        // 2) older messages: let nchat search its cache (and load what's in between)
        let from = self.oldest_id(&key);
        self.set_find_status("Searching…");
        self.find_started = Some(Instant::now());
        ffi::find_message(&key.0, &key.1, &from, &from, &text, "", false);
    }

    pub fn find_closed(&mut self) {
        self.last_found = None;
        self.last_find_text.clear();
        self.highlight = None;
        self.dirty_msgs = true;
    }

    pub fn jump_pinned(&mut self) {
        let Some(key) = self.current.clone() else { return };
        self.last_find_text.clear();
        ffi::find_message(&key.0, &key.1, "", &self.oldest_id(&key), "", "", true);
    }

    fn focus_message(&mut self, id: &str) {
        self.highlight = Some((id.to_string(), Instant::now()));
        self.scroll_to = Some(id.to_string());
        self.dirty_msgs = true;
    }

    // -----------------------------------------------------------------------------------
    // Composer
    // -----------------------------------------------------------------------------------

    pub fn send_text(&mut self, text: &str) {
        let Some(key) = self.current.clone() else { return };
        let text = text.trim_end_matches(['\n', ' ']);
        if text.trim().is_empty() {
            return;
        }
        if let Some(id) = self.edit_of.take() {
            ffi::edit_message(&key.0, &key.1, &id, text);
            if let Some(m) = self.msg_mut(&key.0, &key.1, &id) {
                m.text = text.to_string();
                m.is_edited = true;
            }
        } else {
            let (qid, qtext, qsender) = match self.reply_to.take() {
                Some(id) => {
                    let q = self.chats.get(&key).and_then(|s| s.msgs.get(&id));
                    let qtext = q.map(|m| m.text.clone()).unwrap_or_default();
                    let qsender = q.map(|m| m.sender_id.clone()).unwrap_or_default();
                    (id, qtext, qsender)
                }
                None => Default::default(),
            };
            ffi::send_message(&key.0, &key.1, text, &qid, &qtext, &qsender, "");
        }
        if self.settings.share_typing && self.last_typing_sent.is_some() {
            ffi::send_typing(&key.0, &key.1, false);
        }
        self.last_typing_sent = None;
        self.drafts.remove(&key);
        if let Some(w) = self.window.upgrade() {
            w.set_composer_text("".into());
        }
        self.dirty_msgs = true;
        self.scroll_bottom = true;
    }

    /// Asks before sending files (attach button, paste, drag & drop).
    pub fn send_files(&mut self, files: Vec<PathBuf>) {
        let Some(key) = self.current.clone() else { return };
        let files: Vec<PathBuf> = files.into_iter().filter(|f| f.is_file()).collect();
        if files.is_empty() {
            self.show_toast("Only files can be sent (not folders)");
            return;
        }
        let name = self.chat_name(&key);
        let list = files
            .iter()
            .take(8)
            .map(|f| {
                let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
                format!("• {} ({})", f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), human_size(size))
            })
            .collect::<Vec<_>>()
            .join("\n");
        let more = if files.len() > 8 { format!("\n… and {} more", files.len() - 8) } else { String::new() };
        let title = if files.len() == 1 { format!("Send this file to {name}?") } else { format!("Send {} files to {name}?", files.len()) };
        self.confirm(Pending::SendFiles(key, files), &title, &format!("{list}{more}"), "Send", false);
    }

    fn do_send_files(&mut self, key: &ChatKey, files: &[PathBuf]) {
        for f in files {
            ffi::send_message(&key.0, &key.1, "", "", "", "", &f.to_string_lossy());
        }
        if self.current.as_ref() == Some(key) {
            self.scroll_bottom = true;
        }
    }

    // -----------------------------------------------------------------------------------
    // Paste / drop / voice notes
    // -----------------------------------------------------------------------------------

    /// Ctrl+V in the message box: files or an image on the clipboard become attachments.
    /// Returns false for plain text (the text box pastes it itself).
    pub fn paste_attachments(&mut self) -> bool {
        if self.current.is_none() {
            return false;
        }
        let Ok(mut cb) = arboard::Clipboard::new() else { return false };
        if let Ok(files) = cb.get().file_list() {
            if !files.is_empty() {
                self.send_files(files);
                return true;
            }
        }
        if cb.get_text().map_or(false, |t| !t.is_empty()) {
            return false;
        }
        if let Ok(img) = cb.get_image() {
            match save_pasted_image(&img) {
                Ok(path) => self.send_files(vec![path]),
                Err(e) => self.show_toast(&format!("Couldn't paste the image: {e}")),
            }
            return true;
        }
        false
    }

    pub fn drop_hover(&self, on: bool) {
        if let Some(w) = self.window.upgrade() {
            w.set_drop_hover(on && self.current.is_some());
        }
    }

    /// One call per dropped file; they're collected and confirmed together.
    pub fn file_dropped(&mut self, path: PathBuf) {
        self.drop_hover(false);
        if self.current.is_none() {
            self.show_toast("Open a chat first, then drop the files on it");
            return;
        }
        let first = self.dropped.is_empty();
        self.dropped.push(path);
        if first {
            slint::Timer::single_shot(Duration::from_millis(120), || {
                with_state(|s| {
                    let files = std::mem::take(&mut s.dropped);
                    s.send_files(files);
                });
            });
        }
    }

    pub fn record_start(&mut self) {
        if self.current.is_none() || record::is_recording() {
            return;
        }
        if let Some(p) = self.playing.take() {
            p.stop();
            self.dirty_msgs = true;
        }
        match record::start() {
            Ok(()) => {
                if let Some(w) = self.window.upgrade() {
                    w.set_recording(true);
                    w.set_record_time("0:00".into());
                }
                if let Some(key) = &self.current {
                    if self.settings.share_typing {
                        ffi::send_typing(&key.0, &key.1, true);
                    }
                }
            }
            Err(e) => self.show_toast(&format!("Can't record: {e}")),
        }
    }

    pub fn record_cancel(&mut self) {
        record::cancel();
        self.record_ui_off();
    }

    fn record_ui_off(&mut self) {
        if let Some(w) = self.window.upgrade() {
            w.set_recording(false);
            w.set_record_level(0.0);
        }
        if let Some(key) = &self.current {
            if self.settings.share_typing {
                ffi::send_typing(&key.0, &key.1, false);
            }
        }
    }

    pub fn record_send(&mut self) {
        let Some(key) = self.current.clone() else { return record::cancel() };
        let Some((samples, rate)) = record::stop() else { return };
        self.record_ui_off();
        let path = record::new_recording_path();
        std::thread::spawn(move || {
            let result = record::encode(&samples, rate, &path).map(|_| path);
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| match result {
                    // voice notes go out right away, like in WhatsApp
                    Ok(path) => s.do_send_files(&key, &[path]),
                    Err(e) => s.show_toast(&e),
                });
            });
        });
    }

    fn tick_recording(&mut self, w: &AppWindow) {
        if let Some((elapsed, level)) = record::status() {
            w.set_record_time(audio::fmt_duration(elapsed).into());
            w.set_record_level(level);
            if elapsed >= record::MAX_LENGTH {
                self.record_send();
            }
        }
    }

    pub fn composer_edited(&mut self, text: &str) {
        let Some(key) = self.current.clone() else { return };
        if !self.settings.share_typing {
            return;
        }
        if text.is_empty() {
            if self.last_typing_sent.take().is_some() {
                ffi::send_typing(&key.0, &key.1, false);
            }
            return;
        }
        if self.last_typing_sent.map_or(true, |t| t.elapsed() > Duration::from_secs(4)) {
            self.last_typing_sent = Some(Instant::now());
            ffi::send_typing(&key.0, &key.1, true);
        }
    }

    pub fn cancel_reply(&mut self) {
        if self.edit_of.take().is_some() {
            if let Some(w) = self.window.upgrade() {
                w.set_composer_text("".into());
            }
        }
        self.reply_to = None;
        self.dirty_msgs = true;
    }

    pub fn set_search(&mut self, text: &str) {
        self.search = text.to_string();
        self.dirty_list = true;
    }

    // -----------------------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------------------

    pub fn message_action(&mut self, id: &str, action: &str) {
        let Some(key) = self.current.clone() else { return };
        if let Some(url) = action.strip_prefix("url:") {
            // a link inside the message text was clicked
            if url.starts_with("http://") || url.starts_with("https://") {
                let _ = crate::term::xdg_open_command().arg(url).spawn();
            }
            return;
        }
        let Some(m) = self.chats.get(&key).and_then(|s| s.msgs.get(id)).cloned() else { return };
        match action {
            "reply" => {
                self.edit_of = None;
                self.reply_to = Some(id.to_string());
                self.dirty_msgs = true;
            }
            "edit" if m.is_outgoing => {
                self.reply_to = None;
                self.edit_of = Some(id.to_string());
                if let Some(w) = self.window.upgrade() {
                    w.set_composer_text(m.text.clone().into());
                }
                self.dirty_msgs = true;
            }
            "copy" => {
                copy_to_clipboard(m.text.clone());
                self.show_toast("Copied");
            }
            "link" => {
                if let Some(url) = richtext::links(&m.text).into_iter().next() {
                    let _ = crate::term::xdg_open_command().arg(url).spawn();
                }
            }
            "copy-link" => {
                let links = richtext::links(&m.text);
                if !links.is_empty() {
                    copy_to_clipboard(links.join("\n"));
                    self.show_toast(if links.len() > 1 { "Links copied" } else { "Link copied" });
                }
            }
            "play" => self.play_audio(&key, &m),
            "speed" => {
                self.audio_speed = match self.audio_speed { s if s < 1.25 => 1.5, s if s < 1.75 => 2.0, _ => 1.0 };
                if let Some(p) = &self.playing {
                    p.set_speed(self.audio_speed);
                }
                self.refresh_audio_row();
            }
            s if s.starts_with("seek:") => {
                let f: f32 = s[5..].parse().unwrap_or(0.0);
                match &self.playing {
                    Some(p) if p.msg_id == m.id => p.seek(f),
                    _ => self.play_audio(&key, &m),
                }
            }
            "open-with" => self.open_with(&m),
            "jump-quoted" => {
                let loaded = self.chats.get(&key).map_or(false, |s| s.msgs.contains_key(&m.quoted_id));
                if loaded {
                    self.focus_message(&m.quoted_id);
                } else if !m.quoted_id.is_empty() {
                    self.last_find_text.clear();
                    ffi::find_message(&key.0, &key.1, "", &self.oldest_id(&key), "", &m.quoted_id, false);
                }
            }
            "pin" | "unpin" => {
                ffi::pin_message(&key.0, &key.1, &m.sender_id, &m.id, action == "pin");
                if let Some(mm) = self.msg_mut(&key.0, &key.1, id) {
                    mm.is_pinned = action == "pin";
                }
                self.dirty_msgs = true;
            }
            "react" => {
                self.react_to = Some(id.to_string());
                if self.feature(&key.0, FEATURE_LIMITED_REACTIONS) {
                    ffi::get_available_reactions(&key.0, &key.1, id);
                } else {
                    self.open_emoji_panel(true, None);
                }
            }
            "forward" => {
                self.picker = Some(PickerMode::Forward(Box::new(m), key.0.clone()));
                self.open_picker("Forward to…", "");
            }
            "save" => {
                if m.file_status == 1 && !m.file_path.is_empty() {
                    let src = PathBuf::from(&m.file_path);
                    let name = src.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
                    std::thread::spawn(move || {
                        if let Some(dst) = rfd::FileDialog::new().set_file_name(&name).save_file() {
                            let ok = std::fs::copy(&src, &dst).is_ok();
                            let _ = slint::invoke_from_event_loop(move || {
                                with_state(|s| s.show_toast(if ok { "Saved" } else { "Could not save the file" }));
                            });
                        }
                    });
                }
            }
            "open" | "show" | "download" => {
                if m.has_file && m.file_status == 1 && !m.file_path.is_empty() {
                    let path = PathBuf::from(&m.file_path);
                    let target = if action == "show" { path.parent().map(|p| p.to_path_buf()).unwrap_or(path) } else { path };
                    let _ = crate::term::xdg_open_command().arg(target).spawn();
                } else if m.has_file && m.file_id.is_empty() {
                    self.show_toast("This file isn't available to download");
                } else if m.has_file && m.file_status != 2 {
                    ffi::download_file(&key.0, &key.1, &m.id, &m.file_id);
                    if let Some(mm) = self.msg_mut(&key.0, &key.1, id) {
                        mm.file_status = 2;
                    }
                    self.dirty_msgs = true;
                }
            }
            "delete" => {
                self.confirm(
                    Pending::DeleteMessage(key.clone(), id.to_string()),
                    "Delete message?",
                    if m.is_outgoing { "The message will be deleted for everyone if the service allows it." } else { "Delete this message?" },
                    "Delete",
                    true,
                );
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------------------
    // Emoji panel (composer and reactions)
    // -----------------------------------------------------------------------------------

    pub fn open_emoji_panel(&mut self, react: bool, limited: Option<Vec<String>>) {
        let Some(w) = self.window.upgrade() else { return };
        if !react {
            self.react_to = None;
        }
        self.emoji_limited = limited;
        self.emoji_search.clear();
        self.emoji_tab = if self.settings.recent_emojis.is_empty() { 1 } else { 0 };
        let s = w.global::<EmojiState>();
        s.set_tabs(emoji::tab_icons());
        s.set_react_mode(react);
        s.set_limited(self.emoji_limited.is_some());
        s.set_search("".into());
        s.set_tab(self.emoji_tab as i32);
        self.refresh_emoji_rows();
        w.invoke_open_emoji_panel(react);
    }

    fn refresh_emoji_rows(&self) {
        let Some(w) = self.window.upgrade() else { return };
        let rows = match &self.emoji_limited {
            Some(list) => emoji::to_rows(list.clone()),
            None => emoji::rows(self.emoji_tab, &self.emoji_search, &self.settings.recent_emojis),
        };
        w.global::<EmojiState>().set_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    pub fn emoji_tab_changed(&mut self, tab: i32) {
        self.emoji_tab = tab.max(0) as usize;
        self.refresh_emoji_rows();
    }

    pub fn emoji_search_changed(&mut self, text: &str) {
        self.emoji_search = text.to_string();
        self.refresh_emoji_rows();
    }

    pub fn emoji_picked(&mut self, e: &str) {
        let recent = &mut self.settings.recent_emojis;
        recent.retain(|r| r != e);
        recent.insert(0, e.to_string());
        recent.truncate(36);
        self.settings.save();
        let Some(w) = self.window.upgrade() else { return };
        if let Some(id) = self.react_to.take() {
            w.invoke_close_emoji_panels();
            self.react(&id, e);
        } else {
            w.invoke_insert_emoji(e.into());
        }
    }

    // -----------------------------------------------------------------------------------
    // Audio player
    // -----------------------------------------------------------------------------------

    fn play_audio(&mut self, key: &ChatKey, m: &Message) {
        if let Some(p) = &self.playing {
            if p.msg_id == m.id {
                p.toggle();
                self.refresh_audio_row();
                return;
            }
        }
        if let Some(p) = self.playing.take() {
            p.stop();
            self.dirty_msgs = true;
        }
        self.audio_pending = Some((key.clone(), m.id.clone()));
        if m.file_status == 1 && !m.file_path.is_empty() {
            self.decode_and_play(key, &m.id);
        } else if !m.file_id.is_empty() && m.file_status != 2 {
            // download first; MessageFile then starts playback
            ffi::download_file(&key.0, &key.1, &m.id, &m.file_id);
            if let Some(mm) = self.msg_mut(&key.0, &key.1, &m.id) {
                mm.file_status = 2;
            }
        }
        self.dirty_msgs = true;
    }

    fn decode_and_play(&mut self, key: &ChatKey, msg_id: &str) {
        let Some(m) = self.chats.get(key).and_then(|s| s.msgs.get(msg_id)).cloned() else { return };
        let (k, id, path) = (key.clone(), msg_id.to_string(), m.file_path.clone());
        std::thread::spawn(move || {
            let result = audio::decode(&path);
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| s.audio_decoded(k, id, result));
            });
        });
    }

    fn audio_decoded(&mut self, key: ChatKey, msg_id: String, result: Result<audio::Decoded, String>) {
        if std::env::var_os("NCHAT_UI_DEBUG").is_some() {
            eprintln!("[audio] decoded {msg_id}: {:?}", result.as_ref().map(|a| a.duration()));
        }
        if self.audio_pending.as_ref() != Some(&(key.clone(), msg_id.clone())) {
            return; // user started something else meanwhile
        }
        self.audio_pending = None;
        let started = result.and_then(|a| audio::Playing::start(&msg_id, a, self.audio_speed));
        if std::env::var_os("NCHAT_UI_DEBUG").is_some() {
            eprintln!("[audio] start {msg_id}: {:?}", started.as_ref().map(|_| ()).map_err(|e| e.clone()));
        }
        match started {
            Ok(p) => self.playing = Some(p),
            Err(e) => {
                eprintln!("nchat-ui: can't play {msg_id}: {e}");
                self.show_toast("Can't play this here, choose an app to open it");
                if let Some(m) = self.chats.get(&key).and_then(|s| s.msgs.get(&msg_id)).cloned() {
                    self.open_with(&m);
                }
            }
        }
        self.dirty_msgs = true;
    }

    fn audio_state_of(&self, key: &ChatKey, msg_id: &str) -> i32 {
        match &self.playing {
            Some(p) if p.msg_id == msg_id => if p.is_paused() { 2 } else { 1 },
            _ if self.audio_pending.as_ref().map_or(false, |(k, id)| k == key && id == msg_id) => 3,
            _ => 0,
        }
    }

    fn audio_progress_of(&self, msg_id: &str) -> (f32, String) {
        match &self.playing {
            Some(p) if p.msg_id == msg_id => {
                let pos = p.position();
                let frac = if p.duration.is_zero() { 0.0 } else { pos.as_secs_f32() / p.duration.as_secs_f32() };
                (frac.clamp(0.0, 1.0), format!("{} / {}", audio::fmt_duration(pos), audio::fmt_duration(p.duration)))
            }
            _ => (0.0, String::new()),
        }
    }

    fn tick_audio(&mut self) {
        let finished = self.playing.as_ref().map_or(false, |p| p.finished());
        if finished {
            if std::env::var_os("NCHAT_UI_DEBUG").is_some() {
                eprintln!("[audio] finished at {:?}", self.playing.as_ref().map(|p| p.position()));
            }
            self.playing = None;
            self.dirty_msgs = true;
            return;
        }
        if self.playing.as_ref().map_or(false, |p| !p.is_paused()) {
            self.refresh_audio_row();
        }
    }

    /// Updates only the playing message's row (cheap, runs every tick while playing).
    fn refresh_audio_row(&self) {
        let Some(p) = &self.playing else { return };
        let Some(key) = &self.current else { return };
        let n = self.msg_model.row_count();
        for i in 0..n {
            if let Some(mut item) = self.msg_model.row_data(i) {
                if item.id.as_str() == p.msg_id {
                    let (frac, time) = self.audio_progress_of(&p.msg_id);
                    item.audio_state = self.audio_state_of(key, &p.msg_id);
                    item.audio_progress = frac;
                    item.audio_time = time.into();
                    item.audio_speed = speed_label(self.audio_speed).into();
                    self.msg_model.set_row_data(i, item);
                    break;
                }
            }
        }
    }

    fn open_with(&mut self, m: &Message) {
        if m.file_status != 1 || m.file_path.is_empty() {
            self.show_toast("Download the file first");
            return;
        }
        if cfg!(target_os = "macos") {
            let _ = std::process::Command::new("open").arg(&m.file_path).spawn();
            return;
        }
        self.picker = Some(PickerMode::OpenWith(m.file_path.clone(), m.file_type.clone()));
        self.open_picker("Open with…", "");
    }

    pub fn jumped_to_bottom(&mut self) {
        self.new_below = 0;
    }

    pub fn react(&mut self, id: &str, emoji: &str) {
        let Some(key) = self.current.clone() else { return };
        let Some(m) = self.chats.get(&key).and_then(|s| s.msgs.get(id)).cloned() else { return };
        let prev = m.reactions.iter().find(|r| r.mine).map(|r| r.emoji.clone()).unwrap_or_default();
        if prev == emoji {
            return;
        }
        ffi::send_reaction(&key.0, &key.1, &m.sender_id, &m.id, emoji, &prev);
    }

    // -----------------------------------------------------------------------------------
    // Picker (new chat / forward)
    // -----------------------------------------------------------------------------------

    pub fn new_chat(&mut self) {
        self.picker = Some(PickerMode::NewChat);
        self.open_picker("New chat", "");
    }

    fn open_picker(&mut self, title: &str, filter: &str) {
        if let Some(w) = self.window.upgrade() {
            let s = w.global::<PickerState>();
            s.set_title(title.into());
            s.set_filter(filter.into());
            s.set_visible(true);
        }
        self.picker_filter(filter);
    }

    pub fn picker_filter(&mut self, filter: &str) {
        let Some(mode) = self.picker.clone() else { return };
        let f = filter.to_lowercase();
        let multi = self.profiles.len() > 1;
        let mut items: Vec<(i64, PickItem)> = Vec::new();
        match mode {
            PickerMode::NewChat => {
                for ((profile, id), c) in &self.contacts {
                    if c.is_self || c.is_alias {
                        continue;
                    }
                    let name = self.contact_name(profile, id);
                    let phone = self.contact_phone(profile, id);
                    if !f.is_empty() && !name.to_lowercase().contains(&f) && !phone.contains(&f) {
                        continue;
                    }
                    let mut sub = phone;
                    if multi {
                        sub = format!("{sub} · {}", self.profile_label(profile));
                    }
                    items.push((0, PickItem {
                        key: key_str(&(profile.clone(), id.clone())).into(),
                        initials: initials(&name).into(),
                        avatar_color: avatar_color(id),
                        subtitle: sub.trim_start_matches(" · ").to_string().into(),
                        name: name.into(),
                    }));
                }
                items.sort_by_key(|(_, p)| p.name.to_lowercase());
            }
            PickerMode::OpenWith(path, file_type) => {
                for app in openwith::apps_for(&path, &file_type) {
                    if !f.is_empty() && !app.name.to_lowercase().contains(&f) {
                        continue;
                    }
                    let sub = app.desktop_file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    items.push((0, PickItem {
                        key: app.desktop_file.display().to_string().into(),
                        initials: initials(&app.name).into(),
                        avatar_color: avatar_color(&app.name),
                        subtitle: sub.into(),
                        name: app.name.into(),
                    }));
                }
            }
            PickerMode::Forward(..) => {
                for (key, st) in &self.chats {
                    if st.info.id.is_empty() {
                        continue;
                    }
                    let name = self.chat_name(key);
                    if !f.is_empty() && !name.to_lowercase().contains(&f) {
                        continue;
                    }
                    items.push((-st.last_time(), PickItem {
                        key: key_str(key).into(),
                        initials: initials(&name).into(),
                        avatar_color: avatar_color(&key.1),
                        subtitle: if multi { self.profile_label(&key.0) } else { String::new() }.into(),
                        name: name.into(),
                    }));
                }
                items.sort_by_key(|(t, _)| *t);
            }
        }
        let items: Vec<PickItem> = items.into_iter().map(|(_, p)| p).take(500).collect();
        if let Some(w) = self.window.upgrade() {
            w.global::<PickerState>().set_items(ModelRc::from(Rc::new(VecModel::from(items))));
        }
    }

    pub fn picker_picked(&mut self, key: &str) {
        let Some(mode) = self.picker.take() else { return };
        self.close_picker();
        if let PickerMode::OpenWith(path, _) = &mode {
            if !openwith::launch(std::path::Path::new(key), path) {
                self.show_toast("Couldn't start that application");
            }
            return;
        }
        let Some(k) = key_parse(key) else { return };
        match mode {
            PickerMode::OpenWith(..) => {}
            PickerMode::NewChat => self.open_or_create_chat(k),
            PickerMode::Forward(m, src_profile) => {
                let name = self.chat_name(&k);
                if k.0 == src_profile {
                    // same account: forward like the TUI (text + attachment info as-is)
                    ffi::forward_message(&k.0, &k.1, &m.text, &m.file_id, &m.file_path, &m.file_type, m.file_status);
                } else if m.has_file && m.file_status == 1 {
                    // other account: send the downloaded file as a new attachment
                    ffi::send_message(&k.0, &k.1, &m.text, "", "", "", &m.file_path);
                } else if !m.text.is_empty() {
                    ffi::send_message(&k.0, &k.1, &m.text, "", "", "", "");
                }
                self.show_toast(&format!("Forwarded to {name}"));
            }
        }
    }

    pub fn close_picker(&mut self) {
        self.picker = None;
        if let Some(w) = self.window.upgrade() {
            w.global::<PickerState>().set_visible(false);
        }
    }

    // -----------------------------------------------------------------------------------
    // Confirm dialog
    // -----------------------------------------------------------------------------------

    fn confirm(&mut self, what: Pending, title: &str, text: &str, ok: &str, danger: bool) {
        self.pending = Some(what);
        if let Some(w) = self.window.upgrade() {
            let s = w.global::<ConfirmState>();
            s.set_title(title.into());
            s.set_text(text.into());
            s.set_ok_label(ok.into());
            s.set_danger(danger);
            s.set_visible(true);
        }
    }

    pub fn confirm_close_while_syncing(&mut self) {
        self.confirm(
            Pending::CloseWhileSyncing,
            "Still syncing your history",
            "WhatsApp is still sending your message history (it only does this once, right after linking). \
             If you close now, part of it may be lost. Close anyway?",
            "Close anyway",
            true,
        );
    }

    pub fn confirm_result(&mut self, ok: bool) {
        if let Some(w) = self.window.upgrade() {
            w.global::<ConfirmState>().set_visible(false);
        }
        let Some(p) = self.pending.take() else { return };
        if !ok {
            return;
        }
        match p {
            Pending::SendFiles(k, files) => self.do_send_files(&k, &files),
            Pending::DeleteChat(k) => {
                ffi::delete_chat(&k.0, &k.1);
                self.show_toast("Chat deleted");
            }
            Pending::DeleteMessage(k, id) => {
                let sender = self.chats.get(&k).and_then(|s| s.msgs.get(&id)).map(|m| m.sender_id.clone()).unwrap_or_default();
                ffi::delete_message(&k.0, &k.1, &sender, &id);
            }
            Pending::RemoveAccount(id) => {
                self.restart(&["--remove-account", &id]);
            }
            Pending::CloseWhileSyncing => {
                self.close_confirmed = true;
                let _ = slint::quit_event_loop();
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Settings
    // -----------------------------------------------------------------------------------

    pub fn open_settings(&mut self) {
        let Some(w) = self.window.upgrade() else { return };
        let s = w.global::<SettingsState>();
        self.push_settings_to_ui(&w);
        let mut fams = vec!["Default".to_string()];
        fams.extend(settings::font_families());
        let idx = fams.iter().position(|f| *f == self.settings.font_family).unwrap_or(0);
        s.set_font_families(ModelRc::from(Rc::new(VecModel::from(fams.into_iter().map(SharedString::from).collect::<Vec<_>>()))));
        s.set_font_family_index(idx as i32);
        let mut efonts = vec!["Automatic".to_string()];
        efonts.extend(emoji::emoji_font_families());
        let eidx = efonts.iter().position(|f| *f == self.settings.emoji_font).unwrap_or(0);
        s.set_emoji_fonts(ModelRc::from(Rc::new(VecModel::from(efonts.into_iter().map(SharedString::from).collect::<Vec<_>>()))));
        s.set_emoji_font_index(eidx as i32);
        let accounts: Vec<AccountItem> = self
            .profiles
            .iter()
            .map(|p| AccountItem {
                id: p.id.clone().into(),
                label: self.profile_label(&p.id).into(),
                status: format!(
                    "{} · {}",
                    p.id.split_once('_').map(|(_, n)| n).unwrap_or(""),
                    match self.connected.get(&p.id) {
                        Some(true) => "online",
                        Some(false) => "offline",
                        None => "connecting",
                    }
                )
                .into(),
            })
            .collect();
        s.set_accounts(ModelRc::from(Rc::new(VecModel::from(accounts))));
        s.set_about(
            format!(
                "nchat-ui {} on nchat ({}).\nAccounts and history: {}\nUI settings: {}",
                env!("CARGO_PKG_VERSION"),
                env!("NCHAT_UI_FEATURES"),
                self.confdir.display(),
                settings::path().display()
            )
            .into(),
        );
        s.set_visible(true);
    }

    fn push_settings_to_ui(&self, w: &AppWindow) {
        let s = w.global::<SettingsState>();
        let st = &self.settings;
        s.set_theme_mode(match st.theme.as_str() { "light" => 1, "dark" => 2, _ => 0 });
        s.set_accent(st.accent.clone().into());
        s.set_font_family(st.font_family.clone().into());
        s.set_emoji_font(st.emoji_font.clone().into());
        s.set_font_size(st.font_size);
        s.set_compact(st.compact);
        s.set_bubble_radius(st.bubble_radius);
        s.set_chat_background(st.chat_background.clone().into());
        s.set_bubble_color(st.bubble_color.clone().into());
        s.set_enter_sends(st.enter_sends);
        s.set_time_24h(st.time_24h);
        s.set_auto_download(match st.auto_download.as_str() { "none" => 0, "images" => 1, "all" => 3, _ => 2 });
        s.set_show_archived_in_list(st.show_archived_in_list);
        s.set_mark_read_on_view(st.mark_read_on_view);
        s.set_share_online(st.share_online);
        s.set_share_typing(st.share_typing);
        s.set_notifications(st.notifications);
        s.set_notification_preview(st.notification_preview);
        s.set_run_in_background(st.run_in_background);
        s.set_start_at_login(st.start_at_login);
        s.set_tray_available(self.tray_available);
        s.set_x11_window(st.x11_window);
    }

    /// Settings panel edited: read everything back, save, apply.
    pub fn settings_changed(&mut self) {
        let Some(w) = self.window.upgrade() else { return };
        let s = w.global::<SettingsState>();
        let mut st = self.settings.clone();
        st.theme = ["system", "light", "dark"][s.get_theme_mode().clamp(0, 2) as usize].into();
        let accent = s.get_accent().trim().to_string();
        if settings::is_hex_color(&accent) {
            st.accent = accent;
        } else {
            s.set_accent(st.accent.clone().into());
        }
        st.font_family = s.get_font_family().to_string();
        st.emoji_font = s.get_emoji_font().to_string();
        st.font_size = s.get_font_size().round().clamp(10.0, 24.0);
        st.compact = s.get_compact();
        st.bubble_radius = s.get_bubble_radius().round();
        st.chat_background = s.get_chat_background().to_string();
        st.bubble_color = s.get_bubble_color().to_string();
        st.enter_sends = s.get_enter_sends();
        st.time_24h = s.get_time_24h();
        st.auto_download = ["none", "images", "media", "all"][s.get_auto_download().clamp(0, 3) as usize].into();
        st.show_archived_in_list = s.get_show_archived_in_list();
        st.mark_read_on_view = s.get_mark_read_on_view();
        st.share_online = s.get_share_online();
        st.share_typing = s.get_share_typing();
        st.notifications = s.get_notifications();
        st.notification_preview = s.get_notification_preview();
        st.run_in_background = s.get_run_in_background();
        st.x11_window = s.get_x11_window();
        st.start_at_login = s.get_start_at_login();
        // starting at login means starting hidden, which needs background mode
        if st.start_at_login && !self.settings.start_at_login {
            st.run_in_background = true;
        }
        if !st.run_in_background && self.settings.run_in_background {
            st.start_at_login = false;
        }
        s.set_run_in_background(st.run_in_background);
        s.set_start_at_login(st.start_at_login);
        if st != self.settings {
            self.settings = st;
            self.settings.save();
            self.apply_settings();
        }
    }

    pub fn reset_settings(&mut self) {
        self.settings = Settings::default();
        self.settings.save();
        self.apply_settings();
        if let Some(w) = self.window.upgrade() {
            self.push_settings_to_ui(&w);
            w.global::<SettingsState>().set_font_family_index(0);
        }
    }

    pub fn toggle_theme(&mut self) {
        let dark = self.window.upgrade().map_or(false, |w| w.global::<Theme>().get_dark());
        self.settings.theme = if dark { "light".into() } else { "dark".into() };
        self.settings.save();
        self.apply_settings();
    }

    fn apply_settings(&mut self) {
        let Some(w) = self.window.upgrade() else { return };
        let t = w.global::<Theme>();
        let st = &self.settings;
        t.set_mode(match st.theme.as_str() { "light" => 1, "dark" => 2, _ => 0 });
        if let Some(c) = settings::parse_color(&st.accent) {
            t.set_accent_base(c);
        }
        t.set_font_family(st.font_family.clone().into());
        t.set_font_scale(st.font_size / 14.0);
        t.set_compact(st.compact);
        t.set_radius(st.bubble_radius.into());
        match settings::parse_color(&st.chat_background) {
            Some(c) => {
                t.set_chat_bg_custom(c);
                t.set_has_chat_bg(true);
            }
            None => t.set_has_chat_bg(false),
        }
        match settings::parse_color(&st.bubble_color) {
            Some(c) => {
                let luminance = 0.2126 * c.red() as f32 + 0.7152 * c.green() as f32 + 0.0722 * c.blue() as f32;
                t.set_bubble_color_custom(c);
                t.set_bubble_color_custom_is_dark(luminance < 140.0);
                t.set_has_bubble_color(true);
            }
            None => t.set_has_bubble_color(false),
        }
        w.set_enter_sends(st.enter_sends);
        ffi::set_share_online(st.share_online);
        let autostart = st.start_at_login;
        let background = st.run_in_background;
        if let Err(e) = desktop::set_autostart(autostart, &self.restart_args) {
            eprintln!("nchat-ui: could not update start-at-login: {e}");
        }
        if background {
            self.ensure_tray();
        }
        self.dirty_list = true;
        self.dirty_msgs = true;
    }

    // -----------------------------------------------------------------------------------
    // Background mode and tray
    // -----------------------------------------------------------------------------------

    pub fn ensure_tray(&mut self) {
        if self.tray.is_some() {
            return;
        }
        self.tray = desktop::Tray::start(self.settings.start_at_login, |cmd| {
            let _ = slint::invoke_from_event_loop(move || {
                with_state(|s| s.tray_command(cmd));
            });
        });
        self.tray_state = (-1, false, String::new());
    }

    fn update_tray(&mut self, w: &AppWindow) {
        let Some(tray) = &self.tray else { return };
        let status = if self.is_syncing() { "Syncing history…".to_string() } else { String::new() };
        let state = (w.get_unread_total(), self.settings.start_at_login, status);
        if state != self.tray_state {
            tray.update(state.0, state.1, &state.2);
            self.tray_state = state;
        }
    }

    fn tray_command(&mut self, cmd: TrayCmd) {
        match cmd {
            TrayCmd::Show => self.raise_window(),
            TrayCmd::MarkAllRead => self.mark_all_read(),
            TrayCmd::ToggleStartAtLogin => {
                self.settings.start_at_login = !self.settings.start_at_login;
                if self.settings.start_at_login {
                    self.settings.run_in_background = true;
                }
                self.settings.save();
                self.apply_settings();
                if let Some(w) = self.window.upgrade() {
                    self.push_settings_to_ui(&w);
                }
            }
            TrayCmd::Quit => self.quit(),
        }
    }

    /// Quit for real (tray menu, or closing the window without background mode).
    pub fn quit(&mut self) {
        if self.is_syncing() && !self.close_confirmed {
            self.raise_window();
            self.confirm_close_while_syncing();
            return;
        }
        self.close_confirmed = true;
        let _ = slint::quit_event_loop();
    }

    /// The window's close button: hide (background mode) or quit.
    pub fn close_requested(&mut self) {
        if self.settings.run_in_background {
            if let Some(w) = self.window.upgrade() {
                let _ = w.hide();
            }
            if !self.settings.background_hint_shown {
                self.settings.background_hint_shown = true;
                self.settings.save();
                let how = if self.tray.is_some() { "the icon in the top bar" } else { "starting nchat-ui again" };
                std::thread::spawn(move || {
                    let _ = notify_rust::Notification::new()
                        .summary("nchat-ui is still running")
                        .body(&format!("You'll keep getting messages and notifications. Reopen it from {how}."))
                        .appname("nchat-ui")
                        .icon("nchat-ui")
                        .show();
                });
            }
        } else {
            self.quit();
        }
    }

    pub fn show_window(&self) {
        self.raise_window();
    }

    pub fn remove_account(&mut self, id: &str) {
        let label = self.profile_label(id);
        self.confirm(
            Pending::RemoveAccount(id.to_string()),
            &format!("Remove {label}?"),
            "This removes the account and its local history from this computer, like `nchat -r`. \
             To unlink the device completely, also remove it on your phone (Linked devices).",
            "Remove",
            true,
        );
    }

    pub fn open_log(&self) {
        let _ = crate::term::xdg_open_command().arg(self.confdir.join("log.txt")).spawn();
    }

    pub fn open_downloads(&self) {
        let _ = crate::term::xdg_open_command().arg(ffi::downloads_dir()).spawn();
    }
}

// ---------------------------------------------------------------------------------------
// Process lifecycle: starting nchat, setup wizard, restarting
// ---------------------------------------------------------------------------------------

impl State {
    pub fn has_profiles(&self) -> bool {
        !crate::setup::list_profiles(&self.confdir).is_empty()
    }

    /// Starts upstream nchat main() on its own thread, optionally with extra args
    /// (`-s` = account setup, `-r` = remove account).
    pub fn start_core_with(&mut self, extra: &[&str]) {
        if self.core_thread.is_some() {
            return;
        }
        let mut args = self.core_args.clone();
        args.extend(extra.iter().map(|s| s.to_string()));
        self.core_exited = None;
        let tx = sender();
        self.core_thread = Some(
            std::thread::Builder::new()
                .name("nchat-core".into())
                .spawn(move || {
                    let code = ffi::run_core(args);
                    let _ = tx.send(AppEvent::CoreExited(code));
                    code
                })
                .expect("spawn nchat core"),
        );
    }

    pub fn start_core(&mut self, setup: bool) {
        self.start_core_with(if setup { &["-s"] } else { &[] });
    }

    pub fn start_remove(&mut self, profile: &str) {
        self.setup.start_remove(profile);
        self.dirty_setup = true;
        self.start_core_with(&["-r"]);
    }

    pub fn core_running(&self) -> bool {
        self.core_thread.as_ref().map_or(false, |t| !t.is_finished())
    }

    pub fn show_setup_choose(&mut self, first_run: bool) {
        self.setup.show_choose(first_run, &self.confdir);
        self.dirty_setup = true;
    }

    pub fn setup_choose(&mut self, protocol: &str) {
        if self.core_running() {
            // nchat holds the config dir: restart into the wizard instead
            self.restart(&["--add-account"]);
            return;
        }
        self.setup.start(protocol);
        self.dirty_setup = true;
        self.start_core(true);
    }

    pub fn setup_submit(&mut self, value: &str) {
        self.setup.submit(value);
        self.dirty_setup = true;
    }

    pub fn setup_cancel(&mut self) {
        match self.setup.mode {
            Mode::Choose => {
                self.setup.hide();
                self.dirty_setup = true;
                self.start_core(false);
            }
            Mode::Setup => {
                // remove the half-created profile, then start over cleanly
                for dir in self.setup.new_profiles(&self.confdir) {
                    let _ = std::fs::remove_dir_all(dir);
                }
                self.restart(&[]);
            }
            _ => {}
        }
    }

    pub fn add_account(&mut self) {
        if let Some(w) = self.window.upgrade() {
            w.global::<SettingsState>().set_visible(false);
        }
        if self.core_running() {
            self.restart(&["--add-account"]);
        } else {
            self.show_setup_choose(!self.has_profiles());
        }
    }

    /// Replaces this process with a fresh nchat-ui (nchat only supports one setup/run
    /// per process and holds a lock on its config dir while running).
    fn restart(&mut self, extra: &[&str]) {
        let mut args = self.restart_args.clone();
        args.extend(extra.iter().map(|s| s.to_string()));
        let wait_core = self.setup.mode != Mode::Setup && self.core_running();
        if wait_core {
            ffi::request_quit();
        }
        let core = self.core_thread.take();
        RESTARTING.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(w) = self.window.upgrade() {
            let _ = w.hide();
        }
        std::thread::spawn(move || {
            if let (true, Some(core)) = (wait_core, core) {
                let deadline = Instant::now() + Duration::from_secs(8);
                while !core.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            exec_self(&args);
        });
    }
}

fn exec_self(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    // Close every inherited fd (nchat's flock()ed dir locks, the pty master, ...) on exec.
    // (/proc/self/fd on Linux, /dev/fd on macOS)
    if let Ok(rd) = std::fs::read_dir("/proc/self/fd").or_else(|_| std::fs::read_dir("/dev/fd")) {
        for e in rd.flatten() {
            if let Ok(fd) = e.file_name().to_string_lossy().parse::<i32>() {
                if fd > 2 {
                    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                }
            }
        }
    }
    // test hook: a follow-up script for the restarted process (see run_script in main.rs)
    unsafe {
        match std::env::var("NCHAT_UI_SCRIPT_AFTER_RESTART") {
            Ok(next) => {
                std::env::set_var("NCHAT_UI_SCRIPT", next);
                std::env::remove_var("NCHAT_UI_SCRIPT_AFTER_RESTART");
            }
            Err(_) => std::env::remove_var("NCHAT_UI_SCRIPT"),
        }
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
    let err = std::process::Command::new(exe).args(args).exec();
    eprintln!("nchat-ui: restart failed: {err}");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// Saves a pasted image (screenshot etc.) as PNG in ~/.cache/nchat-ui/paste.
fn save_pasted_image(img: &arboard::ImageData) -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache"));
    let dir = base.join("nchat-ui").join("paste");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("pasted-{}.png", Local::now().format("%Y%m%d-%H%M%S")));
    let file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width as u32, img.height as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut w| w.write_image_data(&img.bytes)).map_err(|e| e.to_string())?;
    Ok(path)
}

fn speed_label(s: f32) -> String {
    if (s - 1.0).abs() < 0.01 { "1×".into() } else if (s - 1.5).abs() < 0.01 { "1.5×".into() } else { format!("{s}×") }
}

fn group_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn copy_to_clipboard(text: String) {
    std::thread::spawn(move || {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            let _ = cb.set_text(text);
            // keep the owner alive briefly so X11 clipboard managers can grab it
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

fn set_model<T: Clone + 'static>(model: &VecModel<T>, items: Vec<T>) {
    // Replace content; keeps the same model instance bound to the UI.
    if model.row_count() == items.len() {
        for (i, it) in items.into_iter().enumerate() {
            model.set_row_data(i, it);
        }
    } else {
        model.set_vec(items);
    }
}

fn file_kind(m: &Message) -> FileKind {
    let t = m.file_type.to_lowercase();
    let p = m.file_path.to_lowercase();
    let ext = |exts: &[&str]| exts.iter().any(|e| p.ends_with(e));
    if t.starts_with("image/") || ext(&[".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp"]) {
        FileKind::Image
    } else if t.starts_with("audio/") || ext(&[".ogg", ".oga", ".opus", ".mp3", ".m4a", ".aac", ".wav", ".flac"]) {
        FileKind::Audio
    } else if t.starts_with("video/") || ext(&[".mp4", ".mkv", ".webm", ".mov", ".3gp", ".avi"]) {
        FileKind::Video
    } else {
        FileKind::Document
    }
}

/// WhatsApp (and Telegram) stickers are WebP images.
fn is_sticker(m: &Message) -> bool {
    m.file_type.eq_ignore_ascii_case("image/webp") || m.file_path.to_lowercase().ends_with(".webp")
}

/// Decodes PNG/JPEG/WebP/GIF (first frame) and scales large photos down: a chat shows them
/// at most ~320 px wide, so keeping full-resolution pixels would only waste memory.
fn decode_image(path: &str) -> Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>> {
    let img = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?.decode().ok()?;
    const MAX: u32 = 720;
    let img = if img.width() > MAX || img.height() > MAX { img.thumbnail(MAX, MAX) } else { img };
    let rgba = img.to_rgba8();
    Some(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height()))
}

/// One background thread decodes images (photos can take 100+ ms each), so opening a
/// chat full of pictures never freezes the window.
fn image_worker() -> &'static Sender<String> {
    static WORKER: OnceLock<Sender<String>> = OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::Builder::new()
            .name("image-decode".into())
            .spawn(move || {
                while let Ok(path) = rx.recv() {
                    let pixels = decode_image(&path);
                    if let Some(events) = SENDER.get() {
                        let _ = events.send(AppEvent::Image(path, pixels));
                    }
                }
            })
            .expect("spawn image thread");
        tx
    })
}

fn file_label(m: &Message, kind: FileKind) -> String {
    let name = std::path::Path::new(&m.file_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty());
    match (kind, name) {
        (FileKind::Audio, _) => "Voice message / audio".into(),
        (FileKind::Video, Some(n)) => format!("Video · {n}"),
        (FileKind::Video, None) => "Video".into(),
        (FileKind::Image, None) => "Photo".into(),
        (_, Some(n)) => n,
        (_, None) => if m.file_type.is_empty() { "File".into() } else { m.file_type.clone() },
    }
}

/// Status line under an attachment, and whether it can be downloaded/opened.
fn file_note(m: &Message, kind: FileKind) -> (String, bool) {
    let has_id = !m.file_id.is_empty();
    match m.file_status {
        1 => (
            match kind {
                FileKind::Audio => "Voice message / audio",
                FileKind::Video => "Click to play",
                _ => "Click to open",
            }
            .into(),
            true,
        ),
        2 => ("Downloading… (older media is fetched from your phone, keep it online)".into(), true),
        3 if has_id => ("Download failed. Older media can expire on the server; click to retry".into(), true),
        3 => ("Download failed".into(), false),
        0 if has_id => ("Click to download".into(), true),
        0 => ("Not available: the service didn't include this file (e.g. in the synced history)".into(), false),
        _ => ("Attachment".into(), has_id),
    }
}

fn message_preview(m: &Message) -> String {
    if m.is_deleted {
        return "🚫 Deleted message".into();
    }
    let text = one_line(&m.text);
    if m.has_file {
        let label = match file_kind(m) {
            FileKind::Image if is_sticker(m) => "Sticker".to_string(),
            FileKind::Image => "📷 Photo".to_string(),
            FileKind::Video => "🎥 Video".to_string(),
            FileKind::Audio => "🎤 Audio".to_string(),
            FileKind::Document => format!("📎 {}", file_label(m, FileKind::Document)),
        };
        if text.is_empty() { label } else { format!("{label} {text}") }
    } else {
        text
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn initials(name: &str) -> String {
    let name = name.split("  · ").next().unwrap_or(name);
    let name = name.trim_start_matches('+');
    let mut out = String::new();
    for word in name.split_whitespace().filter(|w| w.chars().next().map_or(false, |c| c.is_alphanumeric())) {
        if let Some(c) = word.chars().next() {
            out.extend(c.to_uppercase());
        }
        if out.chars().count() >= 2 {
            break;
        }
    }
    if out.is_empty() {
        name.chars().find(|c| !c.is_whitespace()).map(|c| c.to_string()).unwrap_or_else(|| "?".into())
    } else {
        out
    }
}

fn avatar_color(id: &str) -> Color {
    const COLORS: [u32; 8] = [0xe17076, 0xf0a04b, 0x7bc862, 0x6ec9cb, 0x65aadd, 0xa695e7, 0xee7aae, 0x4fae8c];
    let mut h: u32 = 2166136261;
    for b in id.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    let c = COLORS[(h % COLORS.len() as u32) as usize];
    Color::from_rgb_u8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn local_time(ms: i64) -> chrono::DateTime<Local> {
    Local.timestamp_millis_opt(ms.max(0)).single().unwrap_or_else(Local::now)
}

fn format_list_time(ms: i64, h24: bool) -> String {
    if ms <= 0 {
        return String::new();
    }
    let t = local_time(ms);
    let now = Local::now();
    match (now.date_naive() - t.date_naive()).num_days() {
        0 => if h24 { t.format("%H:%M").to_string() } else { t.format("%-I:%M %p").to_string() },
        1 => "Yesterday".into(),
        2..=6 => t.format("%a").to_string(),
        _ => t.format("%d/%m/%y").to_string(),
    }
}

fn format_day(ms: i64) -> String {
    let t = local_time(ms);
    let now = Local::now();
    match (now.date_naive() - t.date_naive()).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => t.format("%A").to_string(),
        _ if t.year() == now.year() => t.format("%-d %B").to_string(),
        _ => t.format("%-d %B %Y").to_string(),
    }
}

fn format_last_seen(secs_or_ms: i64) -> String {
    // nchat gives seconds for last seen
    let ms = if secs_or_ms < 100_000_000_000 { secs_or_ms * 1000 } else { secs_or_ms };
    let t = local_time(ms);
    match (Local::now().date_naive() - t.date_naive()).num_days() {
        0 => format!("today at {}", t.format("%H:%M")),
        1 => format!("yesterday at {}", t.format("%H:%M")),
        _ => t.format("%d/%m/%y").to_string(),
    }
}
