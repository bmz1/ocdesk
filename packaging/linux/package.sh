#!/usr/bin/env bash
# Build ocdesk for Linux and package it as an AppImage plus a tarball.
#
#   packaging/linux/package.sh            # host architecture, release build
#   SKIP_BUILD=1 packaging/linux/package.sh   # reuse target/release/ocdesk
#   SKIP_APPIMAGE=1 packaging/linux/package.sh  # tarball only (no linuxdeploy)
#
# Output: target/package/ocdesk-<version>-linux-<arch>.AppImage
#         target/package/ocdesk-<version>-linux-<arch>.tar.gz
# Needs curl (to fetch linuxdeploy once into target/tools). linuxdeploy is
# itself an AppImage; it runs without FUSE via APPIMAGE_EXTRACT_AND_RUN.
set -euo pipefail
cd "$(dirname "$0")/../.."

APP_ID=io.github.bmz1.ocdesk
ARCH=$(uname -m)
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
OUT=target/package
TOOLS=target/tools
NAME=ocdesk-$VERSION-linux-$ARCH

if [ -z "${SKIP_BUILD:-}" ]; then
  cargo build --release --locked
fi
BIN=target/release/ocdesk
[ -x "$BIN" ] || { echo "missing $BIN" >&2; exit 1; }

# Literal paths (we're at the repo root): never depends on a variable.
rm -rf target/package/AppDir target/package/icons target/package/ocdesk-*-linux-*/
mkdir -p "$OUT" "$TOOLS"

# Icon sizes for the hicolor theme.
ICONS=$OUT/icons
for s in 64 128 256 512; do
  d=$ICONS/${s}x${s}/apps
  mkdir -p "$d"
  if command -v convert >/dev/null; then
    convert packaging/icon.png -resize "${s}x${s}" "$d/$APP_ID.png"
  else
    python3 -c "from PIL import Image; Image.open('packaging/icon.png').resize(($s,$s), Image.LANCZOS).save('$d/$APP_ID.png')"
  fi
done

# ── tarball: binary + desktop entry + icons + install script ──────────────
T=$OUT/$NAME
mkdir -p "$T/share/applications" "$T/share/icons/hicolor"
install -m755 "$BIN" "$T/ocdesk"
# Release builds keep line tables for backtraces in logs; ship without them.
strip --strip-debug "$T/ocdesk"
cp packaging/linux/$APP_ID.desktop "$T/share/applications/"
cp -r "$ICONS"/* "$T/share/icons/hicolor/"
cp packaging/linux/install.sh "$T/"
cp README.md "$T/"
tar -C "$OUT" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "wrote $OUT/$NAME.tar.gz"

# ── AppImage via linuxdeploy (bundles non-system shared libraries) ────────
if [ -n "${SKIP_APPIMAGE:-}" ]; then
  exit 0
fi
LD=$TOOLS/linuxdeploy-$ARCH.AppImage
if [ ! -x "$LD" ]; then
  curl -fsSL -o "$LD" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
  chmod +x "$LD"
fi
export APPIMAGE_EXTRACT_AND_RUN=1
export LDAI_OUTPUT="$OUT/$NAME.AppImage"
rm -f "$LDAI_OUTPUT"
"$LD" --appdir "$OUT/AppDir" \
  --executable "$T/ocdesk" \
  --desktop-file "packaging/linux/$APP_ID.desktop" \
  --icon-file "$ICONS/256x256/apps/$APP_ID.png" \
  --output appimage
chmod +x "$LDAI_OUTPUT"
echo "wrote $LDAI_OUTPUT"
