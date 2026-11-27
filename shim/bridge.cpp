// bridge.cpp - converts between nchat's protocol.h types and the cxx structs in src/ffi.rs.
//
// This is the only file that knows protocol.h in detail. When upstream adds or changes
// a request/notify type, this is where to adapt.

#include "nchat-ui/src/ffi.rs.h"

#include <atomic>
#include <map>
#include <mutex>
#include <string>
#include <tuple>

#include "core.h"
#include "fileutil.h"
#include "log.h"
#include "protocol.h"
#include "protocolutil.h"
#include "status.h"

namespace nchatui
{
namespace
{
std::string S(rust::Str p_Str)
{
  return std::string(p_Str);
}

rust::String R(const std::string& p_Str)
{
  // Backends should give UTF-8, but never let invalid bytes abort the process.
  return rust::String::lossy(p_Str);
}

// Outgoing messages we've seen, so an edit can start from the original message
// (the TUI does the same: it copies the message and replaces the text).
std::mutex s_OwnMessagesMutex;
std::map<std::tuple<std::string, std::string, std::string>, ChatMessage> s_OwnMessages;

void RememberOwnMessage(const std::string& p_ProfileId, const std::string& p_ChatId, const ChatMessage& p_Msg)
{
  if (!p_Msg.isOutgoing || p_Msg.id.empty()) return;

  std::lock_guard<std::mutex> lock(s_OwnMessagesMutex);
  s_OwnMessages[std::make_tuple(p_ProfileId, p_ChatId, p_Msg.id)] = p_Msg;
}

Event NewEvent(EventKind p_Kind, const std::string& p_ProfileId)
{
  Event ev;
  ev.kind = p_Kind;
  ev.profile_id = R(p_ProfileId);
  ev.success = false;
  ev.flag = false;
  ev.cached = false;
  ev.full_sync = false;
  ev.time = 0;
  ev.other_type = 0;
  return ev;
}

Contact ToContact(const ContactInfo& p_Info)
{
  Contact contact;
  contact.id = R(p_Info.id);
  contact.name = R(p_Info.name);
  contact.phone = R(p_Info.phone);
  contact.is_self = p_Info.isSelf;
  contact.is_alias = p_Info.isAlias;
  return contact;
}

Chat ToChat(const ChatInfo& p_Info)
{
  Chat chat;
  chat.id = R(p_Info.id);
  chat.is_unread = p_Info.isUnread;
  chat.is_muted = p_Info.isMuted;
  chat.is_pinned = p_Info.isPinned;
  chat.is_archived = p_Info.isArchived;
  chat.last_message_time = p_Info.lastMessageTime;
  return chat;
}

rust::Vec<Reaction> ToReactions(const Reactions& p_Reactions)
{
  rust::Vec<Reaction> reactions;
  std::string selfEmoji;
  auto sit = p_Reactions.senderEmojis.find(s_ReactionsSelfId);
  if (sit != p_Reactions.senderEmojis.end())
  {
    selfEmoji = sit->second;
  }

  // Prefer explicit counts; fall back to counting senders.
  std::map<std::string, size_t> counts = p_Reactions.emojiCounts;
  if (counts.empty())
  {
    for (const auto& senderEmoji : p_Reactions.senderEmojis)
    {
      if (!senderEmoji.second.empty())
      {
        ++counts[senderEmoji.second];
      }
    }
  }

  for (const auto& count : counts)
  {
    if (count.first.empty() || (count.second == 0)) continue;

    Reaction reaction;
    reaction.emoji = R(count.first);
    reaction.count = static_cast<uint32_t>(count.second);
    reaction.mine = (count.first == selfEmoji);
    reactions.push_back(std::move(reaction));
  }

  return reactions;
}

void SetFileFields(Message& p_Msg, const std::string& p_FileInfoHex)
{
  p_Msg.has_file = !p_FileInfoHex.empty();
  p_Msg.file_status = FileStatusNone;
  if (!p_Msg.has_file) return;

  FileInfo fileInfo = ProtocolUtil::FileInfoFromHex(p_FileInfoHex);
  p_Msg.file_id = R(fileInfo.fileId);
  p_Msg.file_path = R(fileInfo.filePath);
  p_Msg.file_type = R(fileInfo.fileType);
  p_Msg.file_status = fileInfo.fileStatus;
}

Message ToMessage(const ChatMessage& p_Msg)
{
  Message msg;
  msg.id = R(p_Msg.id);
  msg.sender_id = R(p_Msg.senderId);
  msg.text = R(p_Msg.text);
  msg.quoted_id = R(p_Msg.quotedId);
  msg.quoted_text = R(p_Msg.quotedText);
  msg.quoted_sender = R(p_Msg.quotedSender);
  SetFileFields(msg, p_Msg.fileInfo);
  msg.time_sent = p_Msg.timeSent;
  msg.is_outgoing = p_Msg.isOutgoing;
  msg.is_read = p_Msg.isRead;
  msg.is_edited = p_Msg.isEdited;
  msg.is_deleted = p_Msg.isDeleted;
  msg.is_pinned = p_Msg.isPinned;
  msg.reactions = ToReactions(p_Msg.reactions);
  return msg;
}

void SendRequest(const std::string& p_ProfileId, std::shared_ptr<RequestMessage> p_Request)
{
  std::shared_ptr<Protocol> protocol = nchatcore::GetProtocol(p_ProfileId);
  if (!protocol)
  {
    LOG_WARNING("nchat-ui: no protocol for profile %s", p_ProfileId.c_str());
    return;
  }

  protocol->SendRequest(p_Request);
}

std::atomic<bool> s_ShareOnline{true};

// Same steps the TUI takes on connect (nchat/src/uimodel.cpp, ConnectNotifyType).
void OnConnect(const std::string& p_ProfileId, bool p_Success)
{
  std::shared_ptr<Protocol> protocol = nchatcore::GetProtocol(p_ProfileId);
  if (!protocol) return;

  if (!protocol->HasFeature(FeatureAutoGetChatsOnLogin))
  {
    protocol->SendRequest(std::make_shared<GetChatsRequest>());
  }

  if (!protocol->HasFeature(FeatureAutoGetContactsOnLogin))
  {
    protocol->SendRequest(std::make_shared<GetContactsRequest>());
  }

  if (p_Success && s_ShareOnline)
  {
    std::shared_ptr<SetStatusRequest> setStatusRequest = std::make_shared<SetStatusRequest>();
    setStatusRequest->isOnline = true;
    protocol->SendRequest(setStatusRequest);
  }
}

void OnReady()
{
  Event ev = NewEvent(EventKind::Ready, "");
  for (const auto& protocol : nchatcore::GetProtocols())
  {
    Profile profile;
    const std::string profileId = protocol->GetProfileId();
    profile.id = R(profileId);
    profile.protocol = R(profileId.substr(0, profileId.find('_')));
    profile.display_name = R(protocol->GetProfileDisplayName());
    ev.profiles.push_back(std::move(profile));
  }

  on_event(std::move(ev));
}

void OnMessage(std::shared_ptr<ServiceMessage> p_ServiceMessage)
{
  const std::string& profileId = p_ServiceMessage->profileId;
  switch (p_ServiceMessage->GetMessageType())
  {
    case ConnectNotifyType:
      {
        auto notify = std::static_pointer_cast<ConnectNotify>(p_ServiceMessage);
        OnConnect(profileId, notify->success);
        Event ev = NewEvent(EventKind::Connect, profileId);
        ev.success = notify->success;
        on_event(std::move(ev));
      }
      break;

    case NewContactsNotifyType:
      {
        auto notify = std::static_pointer_cast<NewContactsNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::Contacts, profileId);
        ev.full_sync = notify->fullSync;
        for (const auto& contactInfo : notify->contactInfos)
        {
          ev.contacts.push_back(ToContact(contactInfo));
        }
        on_event(std::move(ev));
      }
      break;

    case NewChatsNotifyType:
      {
        auto notify = std::static_pointer_cast<NewChatsNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::Chats, profileId);
        for (const auto& chatInfo : notify->chatInfos)
        {
          ev.chats.push_back(ToChat(chatInfo));
        }
        on_event(std::move(ev));
      }
      break;

    case CreateChatNotifyType:
      {
        auto notify = std::static_pointer_cast<CreateChatNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::Chats, profileId);
        ev.chats.push_back(ToChat(notify->chatInfo));
        on_event(std::move(ev));
      }
      break;

    case NewMessagesNotifyType:
      {
        auto notify = std::static_pointer_cast<NewMessagesNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::Messages, profileId);
        ev.chat_id = R(notify->chatId);
        ev.from_msg_id = R(notify->fromMsgId);
        ev.cached = notify->cached;
        for (const auto& chatMessage : notify->chatMessages)
        {
          RememberOwnMessage(profileId, notify->chatId, chatMessage);
          ev.messages.push_back(ToMessage(chatMessage));
        }
        on_event(std::move(ev));
      }
      break;

    case SendMessageNotifyType:
      {
        auto notify = std::static_pointer_cast<SendMessageNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::SendResult, profileId);
        ev.chat_id = R(notify->chatId);
        ev.success = notify->success;
        ev.messages.push_back(ToMessage(notify->chatMessage));
        on_event(std::move(ev));
      }
      break;

    case NewMessageStatusNotifyType:
      {
        auto notify = std::static_pointer_cast<NewMessageStatusNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::MessageStatus, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        ev.flag = notify->isRead;
        on_event(std::move(ev));
      }
      break;

    case MarkMessageReadNotifyType:
      {
        auto notify = std::static_pointer_cast<MarkMessageReadNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::MessageStatus, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        ev.flag = true;
        on_event(std::move(ev));
      }
      break;

    case DeleteMessageNotifyType:
      {
        auto notify = std::static_pointer_cast<DeleteMessageNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::MessageDeleted, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        on_event(std::move(ev));
      }
      break;

    case ReceiveTypingNotifyType:
      {
        auto notify = std::static_pointer_cast<ReceiveTypingNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::Typing, profileId);
        ev.chat_id = R(notify->chatId);
        ev.user_id = R(notify->userId);
        ev.flag = notify->isTyping;
        on_event(std::move(ev));
      }
      break;

    case ReceiveStatusNotifyType:
      {
        auto notify = std::static_pointer_cast<ReceiveStatusNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::UserStatus, profileId);
        ev.user_id = R(notify->userId);
        ev.flag = notify->isOnline;
        ev.time = notify->timeSeen;
        on_event(std::move(ev));
      }
      break;

    case NewMessageFileNotifyType:
      {
        auto notify = std::static_pointer_cast<NewMessageFileNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::MessageFile, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        Message msg;
        msg.id = R(notify->msgId);
        SetFileFields(msg, notify->fileInfo);
        ev.messages.push_back(std::move(msg));
        on_event(std::move(ev));
      }
      break;

    case NewMessageReactionsNotifyType:
      {
        auto notify = std::static_pointer_cast<NewMessageReactionsNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::MessageReactions, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        Message msg;
        msg.id = R(notify->msgId);
        msg.reactions = ToReactions(notify->reactions);
        ev.messages.push_back(std::move(msg));
        on_event(std::move(ev));
      }
      break;

    case NewMessageIsPinnedNotifyType:
      {
        auto notify = std::static_pointer_cast<NewMessageIsPinnedNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::MessagePinned, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        ev.flag = notify->isPinned;
        on_event(std::move(ev));
      }
      break;

    case DeleteChatNotifyType:
      {
        auto notify = std::static_pointer_cast<DeleteChatNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::ChatDeleted, profileId);
        ev.chat_id = R(notify->chatId);
        on_event(std::move(ev));
      }
      break;

    case MoveChatNotifyType:
      {
        auto notify = std::static_pointer_cast<MoveChatNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::ChatMoved, profileId);
        ev.chat_id = R(notify->chatId);
        ev.new_chat_id = R(notify->newChatId);
        on_event(std::move(ev));
      }
      break;

    case UpdateMuteNotifyType:
      {
        auto notify = std::static_pointer_cast<UpdateMuteNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::ChatMuted, profileId);
        ev.chat_id = R(notify->chatId);
        ev.flag = notify->isMuted;
        on_event(std::move(ev));
      }
      break;

    case UpdatePinNotifyType:
      {
        auto notify = std::static_pointer_cast<UpdatePinNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::ChatPinned, profileId);
        ev.chat_id = R(notify->chatId);
        ev.flag = notify->isPinned;
        ev.time = notify->timePinned;
        on_event(std::move(ev));
      }
      break;

    case UpdateArchivedNotifyType:
      {
        auto notify = std::static_pointer_cast<UpdateArchivedNotify>(p_ServiceMessage);
        if (!notify->success) break;

        Event ev = NewEvent(EventKind::ChatArchived, profileId);
        ev.chat_id = R(notify->chatId);
        ev.flag = notify->isArchived;
        on_event(std::move(ev));
      }
      break;

    case RequestAppExitNotifyType:
      on_event(NewEvent(EventKind::AppExit, profileId));
      break;

    case FindMessageNotifyType:
      {
        auto notify = std::static_pointer_cast<FindMessageNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::Found, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        ev.success = notify->success;
        on_event(std::move(ev));
      }
      break;

    case AvailableReactionsNotifyType:
      {
        auto notify = std::static_pointer_cast<AvailableReactionsNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::AvailableReactions, profileId);
        ev.chat_id = R(notify->chatId);
        ev.msg_id = R(notify->msgId);
        for (const auto& emoji : notify->emojis)
        {
          ev.emojis.push_back(R(emoji));
        }
        on_event(std::move(ev));
      }
      break;

    case NewGroupMembersNotifyType:
      {
        auto notify = std::static_pointer_cast<NewGroupMembersNotify>(p_ServiceMessage);
        Event ev = NewEvent(EventKind::GroupMembers, profileId);
        ev.chat_id = R(notify->chatId);
        for (const auto& contactInfo : notify->contactInfos)
        {
          ev.contacts.push_back(ToContact(contactInfo));
        }
        on_event(std::move(ev));
      }
      break;

    default:
      {
        Event ev = NewEvent(EventKind::Other, profileId);
        ev.other_type = static_cast<int32_t>(p_ServiceMessage->GetMessageType());
        on_event(std::move(ev));
      }
      break;
  }
}
}

int32_t run_core(rust::Vec<rust::String> args)
{
  nchatcore::Hooks hooks;
  hooks.onReady = OnReady;
  hooks.onMessage = OnMessage;
  nchatcore::SetHooks(hooks);

  std::vector<std::string> argv;
  for (const auto& arg : args)
  {
    argv.push_back(std::string(arg));
  }

  int rv = nchatcore::RunMain(argv);
  nchatcore::SetHooks(nchatcore::Hooks());
  return rv;
}

void request_quit()
{
  nchatcore::RequestQuit();
}

rust::String default_confdir()
{
  return R(FileUtil::GetDefaultApplicationDir());
}

rust::String downloads_dir()
{
  return R(FileUtil::GetDownloadsDir());
}

rust::String self_id(rust::Str profile)
{
  std::shared_ptr<Protocol> protocol = nchatcore::GetProtocol(S(profile));
  return protocol ? R(protocol->GetSelfId()) : rust::String();
}

bool is_group_chat(rust::Str profile, rust::Str chat)
{
  std::shared_ptr<Protocol> protocol = nchatcore::GetProtocol(S(profile));
  return protocol ? protocol->IsGroupChat(S(chat)) : false;
}

void get_chats(rust::Str profile)
{
  SendRequest(S(profile), std::make_shared<GetChatsRequest>());
}

void get_contacts(rust::Str profile)
{
  SendRequest(S(profile), std::make_shared<GetContactsRequest>());
}

void get_messages(rust::Str profile, rust::Str chat, rust::Str from_msg_id, int32_t limit)
{
  auto request = std::make_shared<GetMessagesRequest>();
  request->chatId = S(chat);
  request->fromMsgId = S(from_msg_id);
  request->limit = limit;
  SendRequest(S(profile), request);
}

void get_status(rust::Str profile, rust::Str user_id)
{
  auto request = std::make_shared<GetStatusRequest>();
  request->userId = S(user_id);
  SendRequest(S(profile), request);
}

void set_current_chat(rust::Str profile, rust::Str chat)
{
  auto request = std::make_shared<SetCurrentChatRequest>();
  request->chatId = S(chat);
  SendRequest(S(profile), request);
}

void send_message(rust::Str profile, rust::Str chat, rust::Str text, rust::Str quoted_id, rust::Str quoted_text,
                  rust::Str quoted_sender, rust::Str file_path)
{
  auto request = std::make_shared<SendMessageRequest>();
  request->chatId = S(chat);
  request->chatMessage.text = S(text);
  request->chatMessage.quotedId = S(quoted_id);
  request->chatMessage.quotedText = S(quoted_text);
  request->chatMessage.quotedSender = S(quoted_sender);
  if (!file_path.empty())
  {
    FileInfo fileInfo;
    fileInfo.filePath = S(file_path);
    request->chatMessage.fileInfo = ProtocolUtil::FileInfoToHex(fileInfo);
  }

  SendRequest(S(profile), request);
}

void edit_message(rust::Str profile, rust::Str chat, rust::Str msg_id, rust::Str text)
{
  const std::string profileId = S(profile);
  const std::string chatId = S(chat);
  const std::string msgId = S(msg_id);

  auto request = std::make_shared<EditMessageRequest>();
  request->chatId = chatId;
  request->msgId = msgId;
  {
    std::lock_guard<std::mutex> lock(s_OwnMessagesMutex);
    auto it = s_OwnMessages.find(std::make_tuple(profileId, chatId, msgId));
    if (it != s_OwnMessages.end())
    {
      request->chatMessage = it->second; // copy original (time sent, quote id, etc), like the TUI
    }
    else
    {
      request->chatMessage.id = msgId;
    }
  }

  request->chatMessage.text = S(text);
  SendRequest(profileId, request);
}

void delete_message(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id)
{
  auto request = std::make_shared<DeleteMessageRequest>();
  request->chatId = S(chat);
  request->senderId = S(sender_id);
  request->msgId = S(msg_id);
  SendRequest(S(profile), request);
}

void mark_read(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id)
{
  auto request = std::make_shared<MarkMessageReadRequest>();
  request->chatId = S(chat);
  request->senderId = S(sender_id);
  request->msgId = S(msg_id);
  SendRequest(S(profile), request);
}

void send_typing(rust::Str profile, rust::Str chat, bool typing)
{
  auto request = std::make_shared<SendTypingRequest>();
  request->chatId = S(chat);
  request->isTyping = typing;
  SendRequest(S(profile), request);
}

void set_online(rust::Str profile, bool online)
{
  auto request = std::make_shared<SetStatusRequest>();
  request->isOnline = online;
  SendRequest(S(profile), request);
}

void send_reaction(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id, rust::Str emoji,
                   rust::Str prev_emoji)
{
  auto request = std::make_shared<SendReactionRequest>();
  request->chatId = S(chat);
  request->senderId = S(sender_id);
  request->msgId = S(msg_id);
  request->emoji = S(emoji);
  request->prevEmoji = S(prev_emoji);
  SendRequest(S(profile), request);
}

void download_file(rust::Str profile, rust::Str chat, rust::Str msg_id, rust::Str file_id)
{
  auto request = std::make_shared<DownloadFileRequest>();
  request->chatId = S(chat);
  request->msgId = S(msg_id);
  request->fileId = S(file_id);
  request->downloadFileAction = DownloadFileActionNone;
  SendRequest(S(profile), request);
}

void forward_message(rust::Str profile, rust::Str chat, rust::Str text, rust::Str file_id, rust::Str file_path,
                     rust::Str file_type, int32_t file_status)
{
  // like the TUI's forward: copy text and attachment info into a new message
  auto request = std::make_shared<SendMessageRequest>();
  request->chatId = S(chat);
  request->chatMessage.text = S(text);
  if (!file_path.empty() || !file_id.empty())
  {
    FileInfo fileInfo;
    fileInfo.fileId = S(file_id);
    fileInfo.filePath = S(file_path);
    fileInfo.fileType = S(file_type);
    fileInfo.fileStatus = static_cast<FileStatus>(file_status);
    request->chatMessage.fileInfo = ProtocolUtil::FileInfoToHex(fileInfo);
  }

  SendRequest(S(profile), request);
}

void delete_chat(rust::Str profile, rust::Str chat)
{
  auto request = std::make_shared<DeleteChatRequest>();
  request->chatId = S(chat);
  SendRequest(S(profile), request);
}

void archive_chat(rust::Str profile, rust::Str chat, bool archived)
{
  auto request = std::make_shared<ArchiveChatRequest>();
  request->chatId = S(chat);
  request->isArchived = archived;
  SendRequest(S(profile), request);
}

void pin_chat(rust::Str profile, rust::Str chat, bool pinned)
{
  auto request = std::make_shared<PinChatRequest>();
  request->chatId = S(chat);
  request->isPinned = pinned;
  SendRequest(S(profile), request);
}

void pin_message(rust::Str profile, rust::Str chat, rust::Str sender_id, rust::Str msg_id, bool pinned)
{
  auto request = std::make_shared<PinMessageRequest>();
  request->chatId = S(chat);
  request->senderId = S(sender_id);
  request->msgId = S(msg_id);
  request->isPinned = pinned;
  SendRequest(S(profile), request);
}

void find_message(rust::Str profile, rust::Str chat, rust::Str from_msg_id, rust::Str last_msg_id, rust::Str text,
                  rust::Str find_msg_id, bool pinned)
{
  auto request = std::make_shared<FindMessageRequest>();
  request->chatId = S(chat);
  request->fromMsgId = S(from_msg_id);
  request->lastMsgId = S(last_msg_id);
  request->findText = S(text);
  request->findMsgId = S(find_msg_id);
  request->findPinned = pinned;
  SendRequest(S(profile), request);
}

void create_chat(rust::Str profile, rust::Str user_id)
{
  auto request = std::make_shared<CreateChatRequest>();
  request->userId = S(user_id);
  SendRequest(S(profile), request);
}

void get_group_members(rust::Str profile, rust::Str chat)
{
  auto request = std::make_shared<GetGroupMembersRequest>();
  request->chatId = S(chat);
  SendRequest(S(profile), request);
}

void get_available_reactions(rust::Str profile, rust::Str chat, rust::Str msg_id)
{
  auto request = std::make_shared<GetAvailableReactionsRequest>();
  request->chatId = S(chat);
  request->msgId = S(msg_id);
  SendRequest(S(profile), request);
}

bool has_feature(rust::Str profile, int32_t feature)
{
  std::shared_ptr<Protocol> protocol = nchatcore::GetProtocol(S(profile));
  return protocol ? protocol->HasFeature(static_cast<ProtocolFeature>(feature)) : false;
}

uint32_t status_flags()
{
  return Status::Get(0xffffffff);
}

void set_share_online(bool share)
{
  s_ShareOnline = share;
}
}
