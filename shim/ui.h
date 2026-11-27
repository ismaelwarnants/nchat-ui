// ui.h - nchat-ui replacement for nchat's ncurses Ui class.
//
// Upstream nchat/src/main.cpp is compiled unmodified against this header. The public
// interface must match nchat/src/ui.h (checked by scripts/check-ui-contract.sh).

#pragma once

#include <memory>
#include <mutex>
#include <string>
#include <unordered_map>

class Protocol;
class ServiceMessage;

class Ui
{
public:
  Ui();
  virtual ~Ui();

  void Init();
  void Cleanup();

  void Run();
  void AddProtocol(std::shared_ptr<Protocol> p_Protocol);
  std::unordered_map<std::string, std::shared_ptr<Protocol>> GetProtocols();
  void MessageHandler(std::shared_ptr<ServiceMessage> p_ServiceMessage);

  static void RunKeyDump();

private:
  std::mutex m_Mutex;
  std::unordered_map<std::string, std::shared_ptr<Protocol>> m_Protocols;
};
