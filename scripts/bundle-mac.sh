#!/usr/bin/env bash
# Build a macOS .app bundle for Yutani.
#
# The yutani.icon source is compiled by build.rs (during the cargo build that
# cargo-bundle runs) into target/macos-icon/{yutani.icns, Assets.car}.
# cargo-bundle picks up yutani.icns via Cargo.toml's `icon = [...]`; this script
# then does the two things cargo-bundle can't:
#   - copy Assets.car into Contents/Resources (the macOS 26 layered icon data)
#   - set CFBundleIconName in Info.plist (the IconKit-era key macOS consults)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ICON_OUT="$ROOT/target/macos-icon"
PROFILE="${PROFILE:-release}"
BUNDLE_FLAGS=(--format osx)
if [[ "$PROFILE" == "release" ]]; then
    BUNDLE_FLAGS+=(--release)
fi

echo ">> cargo bundle ${BUNDLE_FLAGS[*]} (build.rs compiles yutani.icon)"
cd "$ROOT"
cargo bundle "${BUNDLE_FLAGS[@]}"

if [[ ! -e "$ICON_OUT/Assets.car" ]]; then
    echo "error: $ICON_OUT/Assets.car missing — build.rs could not compile the icon" >&2
    echo "       (a full Xcode install is required for actool)" >&2
    exit 1
fi

APP="$ROOT/target/$PROFILE/bundle/osx/Yutani.app"
if [[ ! -d "$APP" ]]; then
    echo "error: expected bundle at $APP" >&2
    exit 1
fi

echo ">> installing Assets.car + patching Info.plist in $APP"
cp "$ICON_OUT/Assets.car" "$APP/Contents/Resources/Assets.car"
/usr/bin/plutil -replace CFBundleIconName -string yutani "$APP/Contents/Info.plist"

echo ">> done: $APP"
