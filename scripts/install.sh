#!/usr/bin/env bash
# Installs nchat-ui for the current user (no root needed):
#   ~/.local/bin/nchat-ui        the app (a single self-contained binary)
#   app menu entry + icon
# Uninstall with: scripts/install.sh --uninstall
set -euo pipefail
cd "$(dirname "$0")/.."

bindir="$HOME/.local/bin"
appdir="$HOME/.local/share/applications"
icondir="$HOME/.local/share/icons/hicolor/scalable/apps"
legacy="$HOME/.local/opt/nchat-ui"   # layout of the shared-library build

if [ "${1:-}" = "--uninstall" ]; then
  rm -rf "$legacy" "$bindir/nchat-ui" "$appdir/nchat-ui.desktop" "$icondir/nchat-ui.svg"
  rm -f "$HOME"/.local/share/icons/hicolor/*/apps/nchat-ui.png
  echo "Removed nchat-ui (your accounts in ~/.config/nchat are untouched)."
  exit 0
fi

bin=target/release/nchat-ui
[ -x "$bin" ] || { echo "Build first: cargo build --release"; exit 1; }
mkdir -p "$bindir" "$appdir" "$icondir"

if "$bin" --help | grep -q "static=true"; then
  rm -rf "$legacy"
  rm -f "$bindir/nchat-ui"
  install -m 755 "$bin" "$bindir/nchat-ui"
  exec_path="$bindir/nchat-ui"
else
  # NCHAT_UI_SHARED=1 build: binary + nchat libraries in ../lib
  mkdir -p "$legacy/bin" "$legacy/lib"
  cp "$bin" "$legacy/bin/"
  cp -P target/lib/*.so* "$legacy/lib/"
  ln -sf "$legacy/bin/nchat-ui" "$bindir/nchat-ui"
  exec_path="$legacy/bin/nchat-ui"
fi
# icons: scalable + every hicolor size (rendered from ui/icon.svg)
cp ui/icon.svg "$icondir/nchat-ui.svg"
rm -rf target/icons
cargo build --release --example render_icon >/dev/null 2>&1 && target/release/examples/render_icon target/icons >/dev/null
for png in target/icons/[0-9]*x[0-9]*.png; do
  [ -f "$png" ] || continue
  size=$(basename "$png" .png)
  mkdir -p "$HOME/.local/share/icons/hicolor/$size/apps"
  cp "$png" "$HOME/.local/share/icons/hicolor/$size/apps/nchat-ui.png"
done

sed "s#^Exec=nchat-ui#Exec=$exec_path#" packaging/nchat-ui.desktop > "$appdir/nchat-ui.desktop"

update-desktop-database "$appdir" >/dev/null 2>&1 || true
gtk-update-icon-cache -q -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
echo "Installed $exec_path"
echo "Start it from your app menu (nchat-ui) or by running: nchat-ui"
