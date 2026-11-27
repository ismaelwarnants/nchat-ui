#!/usr/bin/env bash
# Fails if upstream nchat's Ui public interface (nchat/src/ui.h) no longer matches ours
# (shim/ui.h). Upstream main.cpp is compiled against our header, so a new or changed
# method there must be mirrored in shim/ui.h + shim/ui.cpp.
set -euo pipefail
cd "$(dirname "$0")/.."

extract() {
  # public section, method declarations only, whitespace-normalised
  sed -n '/^public:/,/^private:/p' "$1" | grep -E '\(' | sed -E 's/^[[:space:]]+//; s/[[:space:]]+/ /g' | sort
}

if diff <(extract nchat/src/ui.h) <(extract shim/ui.h) >/dev/null; then
  echo "OK: Ui contract matches upstream"
else
  echo "MISMATCH between nchat/src/ui.h (upstream, <) and shim/ui.h (ours, >):"
  diff <(extract nchat/src/ui.h) <(extract shim/ui.h) || true
  exit 1
fi
