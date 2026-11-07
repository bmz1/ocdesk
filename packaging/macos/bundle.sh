#!/usr/bin/env bash
# Build ocdesk.app and package it as a .dmg and a .zip (macOS only).
#
#   packaging/macos/bundle.sh                     # host arch
#   ARCHS="arm64 x86_64" packaging/macos/bundle.sh  # universal binary
#
# Signing: ad-hoc by default (runs locally; a downloaded copy needs
# right-click → Open, or `xattr -dr com.apple.quarantine ocdesk.app`).
# Set MACOS_SIGN_IDENTITY="Developer ID Application: …" to sign for real, and
# NOTARY_PROFILE=<keychain profile from `xcrun notarytool store-credentials`>
# to notarize and staple.
#
# Output: target/package/ocdesk.app
#         target/package/ocdesk-<version>-macos-<arch>.dmg / .zip
set -euo pipefail
cd "$(dirname "$0")/../.."
[ "$(uname -s)" = Darwin ] || { echo "bundle.sh runs on macOS only" >&2; exit 1; }

APP_ID=io.github.bmz1.ocdesk
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCHS=${ARCHS:-$(uname -m)}
OUT=target/package
APP=$OUT/ocdesk.app
SIGN_ID=${MACOS_SIGN_IDENTITY:--}

if [ "$(echo "$ARCHS" | wc -w)" -gt 1 ]; then LABEL=universal; else LABEL=$ARCHS; fi
NAME=ocdesk-$VERSION-macos-$LABEL

# ── build each architecture, then lipo them together ──────────────────────
bins=()
for a in $ARCHS; do
  case $a in
    arm64|aarch64) t=aarch64-apple-darwin ;;
    x86_64) t=x86_64-apple-darwin ;;
    *) echo "unknown arch $a" >&2; exit 1 ;;
  esac
  if [ -z "${SKIP_BUILD:-}" ]; then
    rustup target add "$t" >/dev/null 2>&1 || true
    cargo build --release --locked --target "$t"
  fi
  bins+=("target/$t/release/ocdesk")
done

# Literal path (we're at the repo root): never depends on a variable.
rm -rf target/package/ocdesk.app target/package/ocdesk.iconset target/package/dmg
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create -output "$APP/Contents/MacOS/ocdesk" "${bins[@]}"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@APP_ID@/$APP_ID/g" \
  packaging/macos/Info.plist.in > "$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"

# ── icon ──────────────────────────────────────────────────────────────────
ICONSET=$OUT/ocdesk.iconset
mkdir -p "$ICONSET"
for s in 16 32 128 256 512; do
  sips -z "$s" "$s" packaging/icon.png --out "$ICONSET/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z "$d" "$d" packaging/icon.png --out "$ICONSET/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/ocdesk.icns"

# ── sign ──────────────────────────────────────────────────────────────────
if [ "$SIGN_ID" = - ]; then
  codesign --force --sign - "$APP"
else
  codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$APP"
fi
codesign --verify --strict --verbose=2 "$APP"

# ── zip (also what notarization takes) ────────────────────────────────────
ditto -c -k --keepParent "$APP" "$OUT/$NAME.zip"
if [ -n "${NOTARY_PROFILE:-}" ]; then
  xcrun notarytool submit "$OUT/$NAME.zip" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$APP"
  rm -f "$OUT/$NAME.zip"
  ditto -c -k --keepParent "$APP" "$OUT/$NAME.zip"
fi
echo "wrote $OUT/$NAME.zip"

# ── dmg with an Applications shortcut ─────────────────────────────────────
STAGE=$OUT/dmg
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT/$NAME.dmg"
hdiutil create -volname ocdesk -srcfolder "$STAGE" -ov -format UDZO "$OUT/$NAME.dmg" >/dev/null
if [ "$SIGN_ID" != - ]; then
  codesign --force --sign "$SIGN_ID" "$OUT/$NAME.dmg"
fi
echo "wrote $OUT/$NAME.dmg"
