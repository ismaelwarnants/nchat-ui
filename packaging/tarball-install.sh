#!/usr/bin/env bash
# Installs nchat-ui from this archive for the current user (no root needed).
# Uninstall: ./install.sh --uninstall
set -euo pipefail
cd "$(dirname "$0")"
bin="$HOME/.local/bin"; apps="$HOME/.local/share/applications"; icons="$HOME/.local/share/icons/hicolor"
if [ "${1:-}" = "--uninstall" ]; then
  rm -f "$bin/nchat-ui" "$apps/nchat-ui.desktop" "$icons"/*/apps/nchat-ui.png "$icons/scalable/apps/nchat-ui.svg"
  echo "Removed nchat-ui (accounts in ~/.config/nchat are kept)."; exit 0
fi
mkdir -p "$bin" "$apps"
install -m755 nchat-ui "$bin/nchat-ui"
for d in icons/*; do
  s=$(basename "$d"); mkdir -p "$icons/$s/apps"; cp "$d"/* "$icons/$s/apps/"
done
sed "s#^Exec=nchat-ui#Exec=$bin/nchat-ui#" nchat-ui.desktop > "$apps/nchat-ui.desktop"
update-desktop-database "$apps" >/dev/null 2>&1 || true
gtk-update-icon-cache -q -t "$icons" >/dev/null 2>&1 || true
echo "Installed $bin/nchat-ui. Start it from your app menu."
