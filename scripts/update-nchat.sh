#!/usr/bin/env bash
# Pull the latest upstream nchat, rebuild nchat-ui against it, and run the checks.
#   scripts/update-nchat.sh            -> latest master
#   scripts/update-nchat.sh v5.12.34   -> a specific tag/commit
set -euo pipefail
cd "$(dirname "$0")/.."

ref="${1:-origin/master}"

# take our patches off before switching versions (build.rs re-applies them)
for p in $(ls -r patches/nchat/*.patch 2>/dev/null); do
  git -C nchat apply --reverse --check "../$p" 2>/dev/null && git -C nchat apply --reverse "../$p"
done
git -C nchat fetch --tags origin
old=$(git -C nchat rev-parse --short HEAD)
git -C nchat checkout -q "$ref"
new=$(git -C nchat rev-parse --short HEAD)
echo "nchat: $old -> $new"

scripts/check-ui-contract.sh
cargo build --release
scripts/smoke-test.sh

echo
echo "Upstream changes to the files we depend on:"
git -C nchat log --oneline "$old..$new" -- src/main.cpp src/ui.h lib/common/src/protocol.h \
  lib/wmchat/go/gowm.go lib/wmchat/go/cgowm.go lib/wmchat/src/wmchat.cpp || true
echo
echo "All good. Commit the submodule bump with: git add nchat && git commit -m 'Bump nchat to $new'"
