//! Native desktop notifications with actions (freedesktop on Linux: GNOME, KDE, ...;
//! Notification Center on macOS, toast on Windows), via notify-rust.
//!
//! One notification per chat: a new message replaces the previous one. Actions:
//!   click        -> open the app on that chat
//!   "Reply"      -> open the chat with the cursor in the message box (macOS: inline reply)
//!   "Mark as read"
//! GNOME doesn't support typing a reply inside a notification (no "inline-reply"
//! capability), so there "Reply" opens the chat.

use std::collections::HashMap;
use std::sync::Mutex;

pub enum NotifyAction {
    Open,
    Reply,
    MarkRead,
    #[allow(dead_code)]
    InlineReply(String),
}

/// chat key -> (notification id currently shown for it, generation)
static SHOWN: Mutex<Option<HashMap<String, (u32, u64)>>> = Mutex::new(None);
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A replaced notification keeps the same id, so its old listener may also see the
/// action: only the newest notification of a chat acts.
fn is_latest(chat_key: &str, gen: u64) -> bool {
    SHOWN.lock().unwrap().as_ref().and_then(|m| m.get(chat_key)).map_or(true, |(_, g)| *g == gen)
}

pub fn show(chat_key: String, title: String, body: String, on_action: impl Fn(String, NotifyAction) + Send + 'static) {
    std::thread::spawn(move || {
        let gen = GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let replaces = SHOWN.lock().unwrap().as_ref().and_then(|m| m.get(&chat_key).map(|(id, _)| *id));
        let mut n = notify_rust::Notification::new();
        // freedesktop servers (GNOME, KDE) interpret the body as markup
        #[cfg(all(unix, not(target_os = "macos")))]
        let body = body.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        n.summary(&title).body(&body).appname("nchat-ui").icon("nchat-ui");
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            n.hint(notify_rust::Hint::Category("im.received".into()));
            n.hint(notify_rust::Hint::DesktopEntry("nchat-ui".into()));
            if let Some(id) = replaces {
                n.id(id);
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = replaces;
            n.reply("Reply…");
        }
        n.action("default", "Open");
        n.action("reply", "Reply");
        n.action("read", "Mark as read");

        let Ok(handle) = n.show() else { return };
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            SHOWN.lock().unwrap().get_or_insert_with(HashMap::new).insert(chat_key.clone(), (handle.id(), gen));
        }

        #[cfg(target_os = "macos")]
        {
            let key = chat_key.clone();
            let _ = handle.wait_for_response(move |r: &notify_rust::NotificationResponse| {
                use notify_rust::NotificationResponse as R;
                let _ = gen;
                match r {
                    R::Reply(text) => on_action(key.clone(), NotifyAction::InlineReply(text.clone())),
                    R::Action(a) if a == "read" => on_action(key.clone(), NotifyAction::MarkRead),
                    R::Action(a) if a == "reply" => on_action(key.clone(), NotifyAction::Reply),
                    R::Default | R::Action(_) => on_action(key.clone(), NotifyAction::Open),
                    _ => {}
                }
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            let key = chat_key.clone();
            handle.wait_for_action(move |action| {
                if !is_latest(&key, gen) {
                    return;
                }
                match action {
                    "read" => on_action(key, NotifyAction::MarkRead),
                    "reply" => on_action(key, NotifyAction::Reply),
                    "default" | "Open" => on_action(key, NotifyAction::Open),
                    _ => {}
                }
            });
        }
        // the notification is gone (acted on or dismissed)
        if let Some(m) = SHOWN.lock().unwrap().as_mut() {
            if m.get(&chat_key).map_or(false, |(_, g)| *g == gen) {
                m.remove(&chat_key);
            }
        }
    });
}

/// Removes the notification of a chat (it was read in the app).
pub fn dismiss(chat_key: &str) {
    let id = SHOWN.lock().unwrap().as_mut().and_then(|m| m.remove(chat_key)).map(|(id, _)| id);
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(id) = id {
        std::thread::spawn(move || {
            // replace it with an empty one and close that right away
            if let Ok(h) = notify_rust::Notification::new().id(id).summary(" ").show() {
                h.close();
            }
        });
    }
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let _ = id;
}
