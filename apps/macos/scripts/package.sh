#!/usr/bin/env bash
#
# Build, bundle, and code-sign the macOS app into dist/DbClient.app.
#
# SwiftPM emits a bare Mach-O executable, and macOS hands one of those to
# Terminal instead of launching it as an application. The bundle assembled here
# is the shippable artefact.
#
# Signing identity (env CODESIGN_IDENTITY):
#   unset / "-"  -> ad-hoc signature: runs on THIS machine, no Apple account needed.
#   "Developer ID Application: Name (TEAMID)" -> distributable; enables the
#                 hardened runtime, which notarization requires.
set -euo pipefail

APP_NAME="DbClient"
BIN_NAME="DbClient"
APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # apps/macos
ROOT_DIR="$(cd "$APP_DIR/../.." && pwd)"
DIST="$ROOT_DIR/dist"
BUNDLE="$DIST/$APP_NAME.app"
IDENTITY="${CODESIGN_IDENTITY:--}"
ENTITLEMENTS="$APP_DIR/Resources/$APP_NAME.entitlements"

# The Swift target links the Rust staticlib, so the core has to be current or
# the bundle ships a binary built against a stale library.
echo "==> Building core"
( cd "$ROOT_DIR" && cargo build --release )

echo "==> Building release binary"
swift build --package-path "$APP_DIR" -c release

echo "==> Assembling $BUNDLE"
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp "$APP_DIR/.build/release/$BIN_NAME" "$BUNDLE/Contents/MacOS/$BIN_NAME"
cp "$APP_DIR/Resources/Info.plist" "$BUNDLE/Contents/Info.plist"
# Optional icon: drop an AppIcon.icns into Resources/ to brand the app.
if [ -f "$APP_DIR/Resources/AppIcon.icns" ]; then
    cp "$APP_DIR/Resources/AppIcon.icns" "$BUNDLE/Contents/Resources/AppIcon.icns"
fi

# CFBundleVersion carries the commit rather than a hand-bumped integer, so the
# About panel identifies the exact tree a build came from.
BUILD="$(cd "$ROOT_DIR" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD" "$BUNDLE/Contents/Info.plist"

echo "==> Signing (identity: $IDENTITY)"
SIGN_ARGS=(--force --sign "$IDENTITY")
if [ "$IDENTITY" != "-" ]; then
    # Real Developer ID: hardened runtime + secure timestamp (needed to notarize).
    SIGN_ARGS+=(--options runtime --timestamp)
fi
if [ -f "$ENTITLEMENTS" ]; then
    SIGN_ARGS+=(--entitlements "$ENTITLEMENTS")
fi
codesign "${SIGN_ARGS[@]}" "$BUNDLE"

echo "==> Verifying signature"
codesign --verify --strict --verbose=2 "$BUNDLE"
codesign --display --verbose=2 "$BUNDLE" 2>&1 | sed 's/^/    /'

echo "==> Done: $BUNDLE"
if [ "$IDENTITY" = "-" ]; then
    echo "    Ad-hoc signed. First launch: right-click -> Open (Gatekeeper) or"
    echo "    sign with a Developer ID + notarize for distribution."
fi
echo "    open \"$BUNDLE\""
