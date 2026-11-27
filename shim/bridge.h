// bridge.h - C++ side of the cxx bridge declared in src/ffi.rs.

#pragma once

#include <cstdint>

#include "rust/cxx.h"

namespace nchatui
{
int32_t run_core(rust::Vec<rust::String> args);
void request_quit();

rust::String default_confdir();
rust::String downloads_dir();
rust::String self_id(rust::Str profile);
bool is_group_chat(rust::Str profile, rust::Str chat);

void get_chats(rust::Str profile);
void get_contacts(rust::Str profile);
void get_messages(rust::Str profile, rust::Str chat, rust::Str from_msg_id, int32_t limit);
void get_status(rust::Str profile, rust::Str user_id);
void set_current_chat(rust::Str profile, rust::Str chat);
void send_message(rust::Str profile, rust::Str chat, rust::Str text, rust::Str quoted_id, rust::Str quoted_text,
                  rust::Str quoted_sender, rust::Str file_path);
void edit_message(rust::Str profile, rust::Str chat, rust::Str msg_id, rust::Str text);
void delete_message(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id);
void mark_read(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id);
void send_typing(rust::Str profile, rust::Str chat, bool typing);
void set_online(rust::Str profile, bool online);
void send_reaction(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id, rust::Str emoji,
                   rust::Str prev_emoji);
void download_file(rust::Str profile, rust::Str chat, rust::Str msg_id, rust::Str file_id);
void forward_message(rust::Str profile, rust::Str chat, rust::Str text, rust::Str file_id, rust::Str file_path,
                     rust::Str file_type, int32_t file_status);
void delete_chat(rust::Str profile, rust::Str chat);
void archive_chat(rust::Str profile, rust::Str chat, bool archived);
void pin_chat(rust::Str profile, rust::Str chat, bool pinned);
void pin_message(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id, bool pinned);
void find_message(rust::Str profile, rust::Str chat, rust::Str from_msg_id, rust::Str last_msg_id, rust::Str text,
                  rust::Str find_msg_id, bool pinned);
void create_chat(rust::Str profile, rust::Str user_id);
void get_group_members(rust::Str profile, rust::Str chat);
void get_available_reactions(rust::Str profile, rust::Str chat, rust::Str msg_id);
bool has_feature(rust::Str profile, int32_t feature);
uint32_t status_flags();
void set_share_online(bool share);
}
