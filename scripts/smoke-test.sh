#!/usr/bin/env bash
# End-to-end smoke test with nchat's built-in Dummy protocol (no real accounts needed).
# Renders headlessly (no display needed). Leaves screenshots in target/smoke/.
#
# Starts with an empty config dir and drives the app like a user would:
#   first-run wizard -> choose Demo -> nchat's own setup asks for a phone number ->
#   account created -> chat list -> open chat -> send message -> dark theme
set -euo pipefail
cd "$(dirname "$0")/.."

bin=${NCHAT_UI_BIN:-target/release/nchat-ui}
out=target/smoke
conf=$(mktemp -d)
trap 'rm -rf "$conf"' EXIT
rm -rf "$out" && mkdir -p "$out"

script="snap:$out/01-welcome.png;choose:Dummy;wait;snap:$out/02-setup-prompt.png"
script+=";submit:+15550000000;wait;wait;snap:$out/03-chatlist.png"
script+=";chat:1;wait;send:Hello from nchat-ui;wait;wait;snap:$out/04-chat.png"
script+=";dark;chat:0;wait;wait;snap:$out/05-dark-group.png;info;wait;snap:$out/06-info.png;close;settings;wait;snap:$out/07-settings.png;quit"

XDG_CONFIG_HOME="$conf/xdg" NCHAT_UI_SHOW_DEMO=1 NCHAT_UI_SCRIPT="$script" timeout 120 "$bin" -d "$conf" -e

fail() { echo "FAIL: $*"; exit 1; }
ls "$conf/profiles" | grep -q '^Dummy_+15550000000$' || fail "profile not created by setup wizard"
for f in 01-welcome 02-setup-prompt 03-chatlist 04-chat 05-dark-group 06-info 07-settings; do
  [ -s "$out/$f.png" ] || fail "$out/$f.png missing"
done
grep -q "nchat-ui core init" "$conf/log.txt" || fail "core did not start"
grep -q "nchat-ui core run exit" "$conf/log.txt" || fail "core did not shut down cleanly"

echo "PASS (screenshots in $out/)"
