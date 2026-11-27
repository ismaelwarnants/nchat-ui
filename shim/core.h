// core.h - plain C++ API around upstream nchat, used by the Rust bridge (bridge.cpp).
//
// Knows nothing about Rust. Lets the bridge run upstream main(), receive every
// ServiceMessage the backends emit, and send RequestMessages to a profile.

#pragma once

#include <functional>
#include <memory>
#include <string>
#include <vector>

class Protocol;
class ServiceMessage;

namespace nchatcore
{
struct Hooks
{
  // Called once Ui::Init() runs, i.e. all profiles are loaded and login is about to start.
  std::function<void()> onReady;
  // Called from nchat's backend threads for every event (same as the TUI's MessageHandler).
  std::function<void(std::shared_ptr<ServiceMessage>)> onMessage;
};

void SetHooks(const Hooks& p_Hooks);

// Runs upstream nchat main() with the given arguments (args[0] = program name).
// Blocks until the app quits. Returns main()'s exit code.
int RunMain(const std::vector<std::string>& p_Args);

// Ask Ui::Run() to return, which makes main() log out and clean up.
void RequestQuit();

std::vector<std::shared_ptr<Protocol>> GetProtocols();
std::shared_ptr<Protocol> GetProtocol(const std::string& p_ProfileId);
}
