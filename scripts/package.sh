#!/usr/bin/env bash
# Builds Linux installers into dist/, named with the version from Cargo.toml:
#   nchat-ui_<ver>-1_<arch>.deb             Debian / Ubuntu (dpkg-deb)
#   nchat-ui-<ver>-1.<arch>.rpm             Fedora / openSUSE / RHEL (rpmbuild)
#   nchat-ui-<ver>-1-<arch>.pkg.tar.zst     Arch Linux / Manjaro (pacman)
#   nchat-ui-<ver>-<arch>.AppImage          any distro (needs the AppImage runtime, see below)
#   nchat-ui-<ver>-linux-<arch>.tar.gz      portable archive with a per-user install.sh
#   SHA256SUMS
#
# scripts/package.sh [deb|rpm|arch|appimage|tar ...]   (default: all available)
#
# The binary links glibc and system libraries of the build machine, so packages built on
# Ubuntu 24.04 need a distro of similar age (glibc >= 2.39, OpenSSL 3).
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(uname -m)                                   # x86_64, aarch64
DEB_ARCH=$(dpkg --print-architecture 2>/dev/null || { [ "$ARCH" = x86_64 ] && echo amd64 || echo "$ARCH"; })
REL=1
OUT=dist
WORK=target/package
# Release builds: WhatsApp only, unless your own Telegram API key is given (README, "Telegram").
# nchat's built-in key must not be used for binaries that are distributed.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-target/pkg}
export NCHAT_UI_SIGNAL=0
if [ -n "${NCHAT_UI_TG_APIID:-}" ] && [ -n "${NCHAT_UI_TG_APIHASH:-}" ]; then
  export NCHAT_UI_TELEGRAM=1 PROTOCOLS="WhatsApp and Telegram"
else
  export NCHAT_UI_TELEGRAM=0 PROTOCOLS="WhatsApp"
  unset NCHAT_UI_TG_APIID NCHAT_UI_TG_APIHASH
fi
BIN="$CARGO_TARGET_DIR/release"
HOMEPAGE=https://github.com/ismaelwarnants/nchat-ui
SUMMARY="$PROTOCOLS desktop client built on nchat"
formats=("$@")
[ ${#formats[@]} -eq 0 ] && formats=(deb rpm arch appimage tar)
want() { local f; for f in "${formats[@]}"; do [ "$f" = "$1" ] && return 0; done; return 1; }

echo "== nchat-ui $VERSION ($ARCH, $PROTOCOLS)"
cargo build --release
cargo build --release --example render_icon >/dev/null 2>&1
rm -rf "$WORK" && mkdir -p "$WORK" "$OUT"
$BIN/examples/render_icon "$WORK/icons" >/dev/null
# build-script output: build/nchat-ui-<hash>/ (older Cargo) or build/nchat-ui/<hash>/
TPL=$(ls -t $(find "$BIN/build" -path '*nchat-ui*/out/build/nchat/THIRD_PARTY_LICENSES') | head -1)
[ -f "$TPL" ] || { echo "THIRD_PARTY_LICENSES not found under $BIN/build"; exit 1; }

# File layout shared by all packages: stage_root <root> <prefix>
stage_root() {
  local root=$1 prefix=$2 png s
  install -Dm755 $BIN/nchat-ui "$root$prefix/bin/nchat-ui"
  install -Dm644 packaging/nchat-ui.desktop "$root$prefix/share/applications/nchat-ui.desktop"
  install -Dm644 packaging/nchat-ui.metainfo.xml "$root$prefix/share/metainfo/nchat-ui.metainfo.xml"
  install -Dm644 ui/icon.svg "$root$prefix/share/icons/hicolor/scalable/apps/nchat-ui.svg"
  for png in "$WORK"/icons/*.png; do
    s=$(basename "$png" .png)
    install -Dm644 "$png" "$root$prefix/share/icons/hicolor/$s/apps/nchat-ui.png"
  done
  install -Dm644 README.md "$root$prefix/share/doc/nchat-ui/README.md"
  install -Dm644 packaging/copyright "$root$prefix/share/doc/nchat-ui/copyright"
  install -Dm644 "$TPL" "$root$prefix/share/doc/nchat-ui/THIRD_PARTY_LICENSES"
  install -Dm644 nchat/LICENSE.GPL-3.0 "$root$prefix/share/licenses/nchat-ui/LICENSE"
  install -d "$root$prefix/share/man/man1"
  gzip -9n < packaging/nchat-ui.1 > "$root$prefix/share/man/man1/nchat-ui.1.gz"
  chmod 644 "$root$prefix/share/man/man1/nchat-ui.1.gz"
}

# ------------------------------------------------------------------------------- deb ---
if want deb; then
  echo "== .deb"
  root="$WORK/deb"
  stage_root "$root" /usr
  # library dependencies as Debian package names
  mkdir -p "$WORK/shlibs/debian"
  printf 'Source: nchat-ui\n\nPackage: nchat-ui\nArchitecture: any\n' > "$WORK/shlibs/debian/control"
  deps=$(cd "$WORK/shlibs" && dpkg-shlibdeps -O "$OLDPWD/$BIN/nchat-ui" 2>/dev/null | sed 's/^shlibs:Depends=//')
  # Ubuntu 24.04 / Debian 13 renamed some libraries (…t64); accept the older names too
  deps=$(echo "$deps" | tr ',' '\n' | sed 's/^ *//' | while read -r d; do
    if [[ "$d" =~ ^([a-z0-9.+-]+)t64(.*)$ ]]; then echo "$d | ${BASH_REMATCH[1]}${BASH_REMATCH[2]}"; else echo "$d"; fi
  done | paste -sd, - | sed 's/,/, /g')
  printf 'nchat-ui (%s-%s) unstable; urgency=medium\n\n  * Release %s.\n\n -- nchat-ui contributors <nchat-ui@users.noreply.localhost>  %s\n' \
    "$VERSION" "$REL" "$VERSION" "$(date -R)" | gzip -9n > "$root/usr/share/doc/nchat-ui/changelog.Debian.gz"
  chmod 644 "$root/usr/share/doc/nchat-ui/changelog.Debian.gz"
  mkdir -p "$root/DEBIAN"
  cat > "$root/DEBIAN/control" <<EOF
Package: nchat-ui
Version: $VERSION-$REL
Architecture: $DEB_ARCH
Maintainer: nchat-ui contributors <nchat-ui@users.noreply.localhost>
Installed-Size: $(du -sk "$root/usr" | cut -f1)
Depends: $deps
Recommends: xdg-utils, libglib2.0-bin
Suggests: gnome-shell-extension-appindicator
Section: net
Priority: optional
Homepage: $HOMEPAGE
Description: $SUMMARY
 nchat-ui is a lightweight desktop chat client with a familiar chat list and
 conversation layout. It runs nchat's $PROTOCOLS backend(s) in-process:
 chats, replies, reactions, media, stickers, voice notes, search, notifications
 with actions, a top-bar icon and an optional background mode.
EOF
  dpkg-deb --root-owner-group -Zxz --build "$root" "$OUT/nchat-ui_${VERSION}-${REL}_${DEB_ARCH}.deb" >/dev/null
fi

# ------------------------------------------------------------------------------- rpm ---
if want rpm; then
  if command -v rpmbuild >/dev/null; then
    echo "== .rpm"
    root="$WORK/rpmroot"
    stage_root "$root" /usr
    mkdir -p "$WORK/rpm/SPECS"
    cat > "$WORK/rpm/SPECS/nchat-ui.spec" <<EOF
%global debug_package %{nil}
%global __strip /bin/true
%define _build_id_links none
Name:           nchat-ui
Version:        $VERSION
Release:        $REL
Summary:        $SUMMARY
License:        GPL-3.0-or-later
URL:            $HOMEPAGE
Recommends:     xdg-utils
AutoReqProv:    yes

%description
nchat-ui is a lightweight desktop chat client with a familiar chat list and
conversation layout. It runs nchat's $PROTOCOLS backend(s) in-process.

%install
cp -a $PWD/$root/. %{buildroot}/

%files
/usr/bin/nchat-ui
/usr/share/applications/nchat-ui.desktop
/usr/share/metainfo/nchat-ui.metainfo.xml
/usr/share/icons/hicolor/*/apps/nchat-ui.*
/usr/share/man/man1/nchat-ui.1.gz
%doc /usr/share/doc/nchat-ui
%license /usr/share/licenses/nchat-ui/LICENSE
EOF
    rpmbuild -bb --quiet --target "$ARCH" \
      --define "_topdir $PWD/$WORK/rpm" --define "_rpmdir $PWD/$OUT" \
      --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
      "$WORK/rpm/SPECS/nchat-ui.spec" 2>&1 | grep -v "^warning: Could not canonicalize" || true
  else
    echo "== .rpm skipped (rpmbuild not installed: sudo apt install rpm)"
  fi
fi

# ------------------------------------------------------------------------------ arch ---
if want arch; then
  echo "== .pkg.tar.zst (Arch)"
  root="$WORK/arch"
  stage_root "$root" /usr
  python3 - "$root" "$VERSION-$REL" "$ARCH" "$SUMMARY" <<'PY'
import gzip, hashlib, os, sys, time
root, ver, arch, desc = sys.argv[1:5]
now = int(time.time())
size = sum(os.path.getsize(os.path.join(d, f)) for d, _, fs in os.walk(root) for f in fs)
deps = ["glibc", "gcc-libs", "openssl", "file", "fontconfig", "freetype2", "alsa-lib", "zlib", "expat",
        "xz", "bzip2", "libpng", "brotli"]
info = [f"pkgname = nchat-ui", f"pkgbase = nchat-ui", f"pkgver = {ver}", f"pkgdesc = {desc}",
        "url = https://github.com/ismaelwarnants/nchat-ui", f"builddate = {now}",
        "packager = nchat-ui contributors", f"size = {size}", f"arch = {arch}",
        "license = GPL-3.0-or-later"] + [f"depend = {d}" for d in deps] + [
        "optdepend = xdg-utils: open files and links", "optdepend = glib2: Open with…"]
open(os.path.join(root, ".PKGINFO"), "w").write("\n".join(info) + "\n")
# .MTREE (what makepkg writes, gzip'd): lets pacman verify installed files
lines = ["#mtree", "/set type=file uid=0 gid=0 mode=644"]
entries = [".PKGINFO"] + sorted(os.path.relpath(os.path.join(d, x), root)
                                for d, ds, fs in os.walk(root) for x in ds + fs if x != ".PKGINFO")
for rel in entries:
    p = os.path.join(root, rel)
    st = os.lstat(p)
    if os.path.isdir(p):
        lines.append(f"./{rel} time={now}.0 mode=755 type=dir")
    else:
        data = open(p, "rb").read()
        mode = oct(st.st_mode & 0o777)[2:]
        lines.append(f"./{rel} time={now}.0 mode={mode} size={st.st_size} "
                     f"md5digest={hashlib.md5(data).hexdigest()} sha256digest={hashlib.sha256(data).hexdigest()}")
with gzip.open(os.path.join(root, ".MTREE"), "wt") as f:
    f.write("\n".join(lines) + "\n")
PY
  ( cd "$root" && tar --owner=0 --group=0 --numeric-owner --zstd \
      -cf "$OLDPWD/$OUT/nchat-ui-${VERSION}-${REL}-${ARCH}.pkg.tar.zst" .PKGINFO .MTREE usr )
fi

# ---------------------------------------------------------------------------- AppImage ---
if want appimage; then
  runtime="packaging/appimage-runtime-$ARCH"
  # official AppImage type2 runtime, fetched once (not kept in git) and checked
  if [ ! -f "$runtime" ] && [ "$ARCH" = x86_64 ]; then
    echo "== downloading AppImage runtime"
    curl -fsSL -o "$runtime.part" https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64 \
      && mv "$runtime.part" "$runtime" || rm -f "$runtime.part"
  fi
  if [ -f "$runtime" ] && ! file "$runtime" | grep -q "ELF 64-bit.*static"; then
    echo "!! $runtime is not a static ELF runtime, skipping the AppImage"; runtime=/nonexistent
  fi
  if [ -f "$runtime" ] && command -v mksquashfs >/dev/null; then
    echo "== AppImage"
    appdir="$WORK/AppDir"
    stage_root "$appdir" /usr
    cp packaging/nchat-ui.desktop "$appdir/nchat-ui.desktop"
    cp "$WORK/icons/256x256.png" "$appdir/nchat-ui.png"
    ln -sf nchat-ui.png "$appdir/.DirIcon"
    # libmagic (file type detection) isn't installed everywhere: bundle it + its database
    mkdir -p "$appdir/usr/lib"
    cp -L "$(ldconfig -p | awk '/libmagic.so.1 /{print $NF; exit}')" "$appdir/usr/lib/"
    for mgc in /usr/lib/file/magic.mgc /usr/share/misc/magic.mgc /usr/share/file/magic.mgc; do
      [ -f "$mgc" ] && { cp -L "$mgc" "$appdir/usr/lib/magic.mgc"; break; }
    done
    cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
export LD_LIBRARY_PATH="$HERE/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
[ -f "$HERE/usr/lib/magic.mgc" ] && export MAGIC="$HERE/usr/lib/magic.mgc"
exec "$HERE/usr/bin/nchat-ui" "$@"
EOF
    chmod +x "$appdir/AppRun"
    img="$OUT/nchat-ui-${VERSION}-${ARCH}.AppImage"
    mksquashfs "$appdir" "$WORK/appimage.squashfs" -root-owned -noappend -comp zstd -quiet >/dev/null
    cat "$runtime" "$WORK/appimage.squashfs" > "$img"
    chmod +x "$img"
  else
    echo "== AppImage skipped: needs mksquashfs and $runtime"
    echo "   (the official runtime from https://github.com/AppImage/type2-runtime/releases)"
  fi
fi

# ------------------------------------------------------------------------------- tar ---
if want tar; then
  echo "== .tar.gz"
  dir="$WORK/tar/nchat-ui-$VERSION"
  mkdir -p "$dir/icons/scalable"
  install -m755 $BIN/nchat-ui "$dir/"
  install -m755 packaging/tarball-install.sh "$dir/install.sh"
  cp packaging/nchat-ui.desktop README.md packaging/copyright "$TPL" nchat/LICENSE.GPL-3.0 "$dir/"
  cp ui/icon.svg "$dir/icons/scalable/nchat-ui.svg"
  for png in "$WORK"/icons/*.png; do s=$(basename "$png" .png); mkdir -p "$dir/icons/$s"; cp "$png" "$dir/icons/$s/nchat-ui.png"; done
  tar -C "$WORK/tar" --owner=0 --group=0 -czf "$OUT/nchat-ui-${VERSION}-linux-${ARCH}.tar.gz" "nchat-ui-$VERSION"
fi

( cd "$OUT" && rm -f SHA256SUMS && sha256sum nchat-ui[-_]"$VERSION"* > SHA256SUMS )
echo "== done:"
ls -lh "$OUT" | tail -n +2
