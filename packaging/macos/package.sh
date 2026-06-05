#!/usr/bin/env bash
#
# Build a macOS .app bundle and DMG for HyprCorrect.
#
# Outputs (under `target/macos/`):
#   AppIcon.icns                          — generated icon set
#   HyprCorrect.app/                      — app bundle (menu-bar agent)
#   HyprCorrect-<version>-aarch64.dmg     — drag-to-Applications DMG
#
# Assumes `target/release/hyprcorrect` already exists. Pass `--build` to
# run `cargo build --release` first, or call this from CI after a build
# step.
#
# Tooling required (all preinstalled on GitHub's macos-latest runner):
#   iconutil       — built into macOS
#   codesign       — built into macOS (ad-hoc signing)
#   hdiutil        — built into macOS (via create-dmg)
#   create-dmg     — `brew install create-dmg`
#   rsvg-convert   — `brew install librsvg`; renders every icon slot
#                    from the source SVG. The macOS icon is pure path
#                    data (no <text>), so no fonts are needed.
#
# Signing: ad-hoc (`codesign --sign -`) unless the Developer ID secrets
# are present (see sign-and-notarize.sh). With an ad-hoc signature the
# user clears Gatekeeper once via right-click -> Open; with the secrets
# set, the DMG is notarized and opens cleanly.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(awk -F'"' '/^version =/ { print $2; exit }' Cargo.toml)"
APP_NAME="HyprCorrect"
BUNDLE_DIR="target/macos/${APP_NAME}.app"
ICNS_PATH="target/macos/AppIcon.icns"
DMG_PATH="target/macos/${APP_NAME}-${VERSION}-aarch64.dmg"
PLIST_TEMPLATE="packaging/macos/Info.plist.template"
SVG_SOURCE="crates/hyprcorrect-ui/assets/icons/svg/hyprcorrect-macos.svg"
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"   # `-` = ad-hoc

if [[ "${1:-}" == "--build" ]]; then
    echo "==> cargo build --release --bin hyprcorrect"
    cargo build --release --bin hyprcorrect
fi

if [[ ! -x "target/release/hyprcorrect" ]]; then
    echo "error: target/release/hyprcorrect not found. Run this with --build, or" >&2
    echo "       run \`cargo build --release\` first." >&2
    exit 1
fi

# ---------------------------------------------------------------- icon
echo "==> Generating AppIcon.icns"
if ! command -v rsvg-convert >/dev/null; then
    echo "error: rsvg-convert not found — install it with \`brew install librsvg\`." >&2
    echo "       It renders the macOS app icon from the source SVG." >&2
    exit 1
fi
ICONSET_DIR="$(mktemp -d)/${APP_NAME}.iconset"
mkdir -p "$ICONSET_DIR"

# macOS expects 1x and @2x at every size — we share the underlying
# bitmap where the slot sizes coincide (e.g. 32 fills both
# icon_16x16@2x.png and icon_32x32.png).
render_size_to() {
    local size="$1"
    local out="$2"
    rsvg-convert -w "$size" -h "$size" "$SVG_SOURCE" -o "$out"
}

render_size_to 16   "$ICONSET_DIR/icon_16x16.png"
render_size_to 32   "$ICONSET_DIR/icon_16x16@2x.png"
render_size_to 32   "$ICONSET_DIR/icon_32x32.png"
render_size_to 64   "$ICONSET_DIR/icon_32x32@2x.png"
render_size_to 128  "$ICONSET_DIR/icon_128x128.png"
render_size_to 256  "$ICONSET_DIR/icon_128x128@2x.png"
render_size_to 256  "$ICONSET_DIR/icon_256x256.png"
render_size_to 512  "$ICONSET_DIR/icon_256x256@2x.png"
render_size_to 512  "$ICONSET_DIR/icon_512x512.png"
render_size_to 1024 "$ICONSET_DIR/icon_512x512@2x.png"

mkdir -p "$(dirname "$ICNS_PATH")"
iconutil -c icns "$ICONSET_DIR" -o "$ICNS_PATH"
rm -rf "$(dirname "$ICONSET_DIR")"

# ---------------------------------------------------------------- bundle
echo "==> Building ${APP_NAME}.app"
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Resources"

install -m755 target/release/hyprcorrect "$BUNDLE_DIR/Contents/MacOS/hyprcorrect"
cp "$ICNS_PATH" "$BUNDLE_DIR/Contents/Resources/AppIcon.icns"
sed "s/@VERSION@/${VERSION}/g" "$PLIST_TEMPLATE" > "$BUNDLE_DIR/Contents/Info.plist"

# Touch the bundle so Finder / LaunchServices re-reads the new Info.plist
# instead of serving a cached entry from a prior build.
touch "$BUNDLE_DIR"

# Ad-hoc sign: lets the app run after the user clicks through Gatekeeper
# once. Without ANY signature (not even ad-hoc), macOS refuses to launch
# the binary from a downloaded DMG with an opaque "cannot be opened"
# error. `--deep` covers the single Mach-O inside MacOS/ since we don't
# have nested frameworks yet.
#
# Skipped when MACOS_CERTIFICATE_P12_BASE64 is set, because
# sign-and-notarize.sh below replaces this with a real Developer ID
# signature anyway.
if [[ -z "${MACOS_CERTIFICATE_P12_BASE64:-}" ]]; then
    echo "==> Codesigning (identity: ${CODESIGN_IDENTITY})"
    codesign --force --deep --sign "$CODESIGN_IDENTITY" "$BUNDLE_DIR"
    codesign --verify --verbose=2 "$BUNDLE_DIR"
fi

# Real Developer ID signing for the bundle. No-ops when the signing
# secrets aren't in the environment, so local builds stay ad-hoc-signed
# without extra config. Notarization happens once, on the DMG below.
"$REPO_ROOT/packaging/macos/sign-and-notarize.sh" --app "$BUNDLE_DIR"

# ---------------------------------------------------------------- DMG
echo "==> Creating ${DMG_PATH}"
rm -f "$DMG_PATH"
# create-dmg's defaults give us the standard drag-to-Applications
# layout: app icon on the left, Applications-folder shortcut on the
# right. `--no-internet-enable` keeps the OS from auto-mounting the DMG
# on download.
create-dmg \
    --volname "${APP_NAME} ${VERSION}" \
    --window-pos 200 120 \
    --window-size 600 400 \
    --icon-size 96 \
    --icon "${APP_NAME}.app" 150 200 \
    --app-drop-link 450 200 \
    --no-internet-enable \
    "$DMG_PATH" \
    "$BUNDLE_DIR"

# Sign, notarize, and staple the DMG. Apple inspects the nested
# (already-signed) HyprCorrect.app as part of this single submission, so
# the whole release is covered in one notary round-trip. No-op locally
# without secrets.
"$REPO_ROOT/packaging/macos/sign-and-notarize.sh" --dmg "$DMG_PATH"

echo
echo "Done."
echo "  Bundle: $BUNDLE_DIR"
echo "  DMG:    $DMG_PATH"
ls -lh "$DMG_PATH"
