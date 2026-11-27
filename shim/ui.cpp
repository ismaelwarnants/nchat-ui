// ui.cpp - nchat-ui replacement for nchat's ncurses Ui class.
//
// Plays exactly the role of nchat/src/ui.cpp: upstream main.cpp hands it the loaded
// protocols, routes every backend event to MessageHandler(), and calls Run() as the
// main loop. Instead of drawing a terminal screen we forward events to the Rust app.

#include "ui.h"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <thread>

#include "apputil.h"
#include "core.h"
#include "log.h"
#include "protocol.h"

int nchat_main(int argc, char* argv[]); // upstream main(), renamed at compile time

namespace
{
std::mutex s_Mutex;
std::condition_variable s_Cond;
bool s_QuitRequested = false;
Ui* s_Ui = nullptr;
nchatcore::Hooks s_Hooks;
}

Ui::Ui()
{
  std::lock_guard<std::mutex> lock(s_Mutex);
  s_Ui = this;
  s_QuitRequested = false;
}

Ui::~Ui()
{
  std::lock_guard<std::mutex> lock(s_Mutex);
  if (s_Ui == this)
  {
    s_Ui = nullptr;
  }
}

void Ui::Init()
{
  LOG_INFO("nchat-ui core init");
  std::function<void()> onReady;
  {
    std::lock_guard<std::mutex> lock(s_Mutex);
    onReady = s_Hooks.onReady;
  }

  if (onReady)
  {
    onReady();
  }
}

void Ui::Cleanup()
{
  LOG_INFO("nchat-ui core cleanup");
}

void Ui::Run()
{
  std::unique_lock<std::mutex> lock(s_Mutex);
  while (!s_QuitRequested && (AppUtil::GetTerminateSignal() == 0))
  {
    // poll periodically, as the terminate signal flag is set from a signal handler
    s_Cond.wait_for(lock, std::chrono::milliseconds(200));
  }

  LOG_INFO("nchat-ui core run exit");
}

void Ui::AddProtocol(std::shared_ptr<Protocol> p_Protocol)
{
  std::lock_guard<std::mutex> lock(m_Mutex);
  m_Protocols[p_Protocol->GetProfileId()] = p_Protocol;
}

std::unordered_map<std::string, std::shared_ptr<Protocol>> Ui::GetProtocols()
{
  std::lock_guard<std::mutex> lock(m_Mutex);
  return m_Protocols;
}

void Ui::MessageHandler(std::shared_ptr<ServiceMessage> p_ServiceMessage)
{
  if (p_ServiceMessage->GetMessageType() == RequestAppExitNotifyType)
  {
    nchatcore::RequestQuit();
  }

  std::function<void(std::shared_ptr<ServiceMessage>)> onMessage;
  {
    std::lock_guard<std::mutex> lock(s_Mutex);
    onMessage = s_Hooks.onMessage;
  }

  if (onMessage)
  {
    onMessage(p_ServiceMessage);
  }
}

void Ui::RunKeyDump()
{
  // keydump is a terminal feature, not applicable to the desktop app
}

namespace nchatcore
{
void SetHooks(const Hooks& p_Hooks)
{
  std::lock_guard<std::mutex> lock(s_Mutex);
  s_Hooks = p_Hooks;
}

int RunMain(const std::vector<std::string>& p_Args)
{
  std::vector<std::string> args = p_Args;
  if (args.empty())
  {
    args.push_back("nchat-ui");
  }

  std::vector<char*> argv;
  for (auto& arg : args)
  {
    argv.push_back(const_cast<char*>(arg.c_str()));
  }

  argv.push_back(nullptr);
  return nchat_main(static_cast<int>(args.size()), argv.data());
}

void RequestQuit()
{
  {
    std::lock_guard<std::mutex> lock(s_Mutex);
    s_QuitRequested = true;
  }

  s_Cond.notify_all();
}

std::vector<std::shared_ptr<Protocol>> GetProtocols()
{
  std::vector<std::shared_ptr<Protocol>> protocols;
  std::lock_guard<std::mutex> lock(s_Mutex);
  if (s_Ui != nullptr)
  {
    for (auto& protocol : s_Ui->GetProtocols())
    {
      protocols.push_back(protocol.second);
    }
  }

  return protocols;
}

std::shared_ptr<Protocol> GetProtocol(const std::string& p_ProfileId)
{
  std::lock_guard<std::mutex> lock(s_Mutex);
  if (s_Ui == nullptr) return nullptr;

  auto protocols = s_Ui->GetProtocols();
  auto it = protocols.find(p_ProfileId);
  return (it != protocols.end()) ? it->second : nullptr;
}
}
