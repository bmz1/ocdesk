#!/usr/bin/env bash
# Install ocdesk from the unpacked tarball for the current user
# (or system-wide with PREFIX=/usr/local, run as root).
set -euo pipefail
cd "$(dirname "$0")"
PREFIX=${PREFIX:-$HOME/.local}
install -Dm755 ocdesk "$PREFIX/bin/ocdesk"
mkdir -p "$PREFIX/share"
cp -r share/applications share/icons "$PREFIX/share/"
if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$PREFIX/share/applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null; then
  gtk-update-icon-cache -q -f -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
fi
echo "installed to $PREFIX (make sure $PREFIX/bin is on PATH)"
