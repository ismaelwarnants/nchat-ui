//! Rust <-> C++ boundary (cxx). The C++ side is shim/bridge.cpp, which converts between
//! these plain structs and nchat's protocol.h types.

#[cxx::bridge(namespace = "nchatui")]
pub mod ffi {
    #[derive(Clone, Debug, Default)]
    pub struct Profile {
        pub id: String,
        pub protocol: String,
        pub display_name: String,
    }

    #[derive(Clone, Debug, Default)]
    pub struct Contact {
        pub id: String,
        pub name: String,
        pub phone: String,
        pub is_self: bool,
        /// WhatsApp alias id (LID) for another contact
        pub is_alias: bool,
    }

    #[derive(Clone, Debug, Default)]
    pub struct Chat {
        pub id: String,
        pub is_unread: bool,
        pub is_muted: bool,
        pub is_pinned: bool,
        pub is_archived: bool,
        pub last_message_time: i64,
    }

    #[derive(Clone, Debug, Default)]
    pub struct Reaction {
        pub emoji: String,
        pub count: u32,
        pub mine: bool,
    }

    #[derive(Clone, Debug, Default)]
    pub struct Message {
        pub id: String,
        pub sender_id: String,
        pub text: String,
        pub quoted_id: String,
        pub quoted_text: String,
        pub quoted_sender: String,
        pub has_file: bool,
        pub file_id: String,
        pub file_path: String,
        pub file_type: String,
        /// -1 none, 0 not downloaded, 1 downloaded, 2 downloading, 3 failed
        pub file_status: i32,
        pub time_sent: i64,
        pub is_outgoing: bool,
        pub is_read: bool,
        pub is_edited: bool,
        pub is_deleted: bool,
        pub is_pinned: bool,
        pub reactions: Vec<Reaction>,
    }

    #[derive(Debug)]
    pub enum EventKind {
        /// Ui::Init() ran: `profiles` is filled, login is starting.
        Ready,
        /// `success` = connected
        Connect,
        /// `contacts`, `full_sync`
        Contacts,
        /// `chats`
        Chats,
        /// `chat_id`, `messages`, `from_msg_id`, `cached`
        Messages,
        /// `chat_id`, `success`, `messages[0]` = the message we sent
        SendResult,
        /// `chat_id`, `msg_id`, `flag` = is_read
        MessageStatus,
        /// `chat_id`, `msg_id`
        MessageDeleted,
        /// `chat_id`, `user_id`, `flag` = typing
        Typing,
        /// `user_id`, `flag` = online, `time` = last seen
        UserStatus,
        /// `chat_id`, `msg_id`, `messages[0]` file fields updated
        MessageFile,
        /// `chat_id`, `msg_id`, `messages[0].reactions`
        MessageReactions,
        /// `chat_id`, `msg_id`, `flag` = pinned
        MessagePinned,
        /// `chat_id`
        ChatDeleted,
        /// `chat_id` -> `new_chat_id`
        ChatMoved,
        /// `chat_id`, `flag` = muted
        ChatMuted,
        /// `chat_id`, `flag` = pinned, `time` = time pinned
        ChatPinned,
        /// `chat_id`, `flag` = archived
        ChatArchived,
        /// Backend asked the app to exit
        AppExit,
        /// `chat_id`, `msg_id` (empty = not found), `success`
        Found,
        /// `chat_id`, `msg_id`, `emojis`
        AvailableReactions,
        /// `chat_id`, `contacts` = members
        GroupMembers,
        /// A notify type we don't handle yet; `other_type` = nchat MessageType value
        Other,
    }

    #[derive(Debug)]
    pub struct Event {
        pub kind: EventKind,
        pub profile_id: String,
        pub chat_id: String,
        pub msg_id: String,
        pub user_id: String,
        pub new_chat_id: String,
        pub from_msg_id: String,
        pub success: bool,
        pub flag: bool,
        pub cached: bool,
        pub full_sync: bool,
        pub time: i64,
        pub other_type: i32,
        pub profiles: Vec<Profile>,
        pub contacts: Vec<Contact>,
        pub chats: Vec<Chat>,
        pub messages: Vec<Message>,
        pub emojis: Vec<String>,
    }

    extern "Rust" {
        fn on_event(ev: Event);
    }

    unsafe extern "C++" {
        include!("nchat-ui/shim/bridge.h");

        /// Runs upstream nchat main() (blocking). args[0] is the program name.
        fn run_core(args: Vec<String>) -> i32;
        fn request_quit();

        /// nchat's default config dir (~/.config/nchat, or legacy ~/.nchat)
        fn default_confdir() -> String;
        /// where nchat saves downloaded attachments
        fn downloads_dir() -> String;
        fn self_id(profile: &str) -> String;
        fn is_group_chat(profile: &str, chat: &str) -> bool;

        fn get_chats(profile: &str);
        fn get_contacts(profile: &str);
        fn get_messages(profile: &str, chat: &str, from_msg_id: &str, limit: i32);
        fn get_status(profile: &str, user_id: &str);
        fn set_current_chat(profile: &str, chat: &str);
        fn send_message(profile: &str, chat: &str, text: &str, quoted_id: &str, quoted_text: &str,
                        quoted_sender: &str, file_path: &str);
        fn edit_message(profile: &str, chat: &str, msg_id: &str, text: &str);
        fn delete_message(profile: &str, chat: &str, sender_id: &str, msg_id: &str);
        fn mark_read(profile: &str, chat: &str, sender_id: &str, msg_id: &str);
        fn send_typing(profile: &str, chat: &str, typing: bool);
        fn set_online(profile: &str, online: bool);
        fn send_reaction(profile: &str, chat: &str, sender_id: &str, msg_id: &str, emoji: &str,
                         prev_emoji: &str);
        fn download_file(profile: &str, chat: &str, msg_id: &str, file_id: &str);
        /// Sends a copy of a message (text and/or attachment), like the TUI's forward.
        fn forward_message(profile: &str, chat: &str, text: &str, file_id: &str, file_path: &str,
                           file_type: &str, file_status: i32);
        fn delete_chat(profile: &str, chat: &str);
        fn archive_chat(profile: &str, chat: &str, archived: bool);
        fn pin_chat(profile: &str, chat: &str, pinned: bool);
        fn pin_message(profile: &str, chat: &str, sender_id: &str, msg_id: &str, pinned: bool);
        fn find_message(profile: &str, chat: &str, from_msg_id: &str, last_msg_id: &str, text: &str,
                        find_msg_id: &str, pinned: bool);
        fn create_chat(profile: &str, user_id: &str);
        fn get_group_members(profile: &str, chat: &str);
        fn get_available_reactions(profile: &str, chat: &str, msg_id: &str);
        /// nchat ProtocolFeature bit (protocol.h)
        fn has_feature(profile: &str, feature: i32) -> bool;
        /// nchat Status flags for all profiles combined (status.h: 0x40 = syncing, ...)
        fn status_flags() -> u32;
        /// share "online" presence on connect (the TUI's online_status_share)
        fn set_share_online(share: bool);
    }
}

pub use ffi::*;

/// Called by C++ from nchat's backend threads.
fn on_event(ev: Event) {
    // Never let a panic unwind into C++.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::core::push_event(ev)));
}
