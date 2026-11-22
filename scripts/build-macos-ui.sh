#!/bin/sh
# Build only. Never launches the app, receiver, capture, or input injection.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_root="$root/target/macos-ui"
app="${CROSSFLOW_MACOS_APP_PATH:-$root/dist/Crossflow.app}"
case "$app" in
    /*.app) ;;
    *) printf '%s\n' 'CROSSFLOW_MACOS_APP_PATH must be an absolute .app path.' >&2; exit 1 ;;
esac
test -f "$root/clients/design-tokens.json" || { printf '%s\n' 'Missing shared clients/design-tokens.json' >&2; exit 1; }
test -f "$root/examples/layout-two-devices.json" || { printf '%s\n' 'Missing shared example layout' >&2; exit 1; }
case "$(uname -m)" in
    arm64) native_target=aarch64-apple-darwin ;;
    x86_64) native_target=x86_64-apple-darwin ;;
    *) printf '%s\n' 'Unsupported Mac architecture' >&2; exit 1 ;;
esac
bridge_library="${CROSSFLOW_BRIDGE_LIBRARY:-$root/target/$native_target/release/libcrossflow_bridge.dylib}"
keychain_helper="${CROSSFLOW_MACOS_KEYCHAIN_HELPER:-$root/target/$native_target/release/crossflow-keychain}"
case "$keychain_helper" in
    /*) ;;
    *) printf '%s\n' 'CROSSFLOW_MACOS_KEYCHAIN_HELPER must be an absolute path.' >&2; exit 1 ;;
esac
core_executable="${CROSSFLOW_CORE_EXECUTABLE:-$root/target/$native_target/release/crossflow}"
test -f "$bridge_library" || { printf 'Build native Rust bridge first: %s\n' "$bridge_library" >&2; exit 1; }
test -f "$keychain_helper" || { printf 'Build native Rust Keychain helper first: %s\n' "$keychain_helper" >&2; exit 1; }
if [ -n "${CROSSFLOW_MACOS_KEYCHAIN_HELPER:-}" ]; then
    # Explicit diagnostic use only: preserve an already authorized helper's
    # exact signature. This is not a solution for normal upgrade continuity.
    codesign --verify --strict "$keychain_helper"
fi
test -f "$core_executable" && test -x "$core_executable" || { printf 'Build native Rust core first: %s\n' "$core_executable" >&2; exit 1; }
test -f "$root/examples/layout-four-screens.json" || { printf '%s\n' 'Missing shared four-screen layout' >&2; exit 1; }
mkdir -p "$build_root/cache" "$build_root/modules"
export CLANG_MODULE_CACHE_PATH="$build_root/modules"
swift build --package-path "$root/clients/macos" \
    --scratch-path "$build_root/swiftpm" --cache-path "$build_root/cache" \
    --configuration release
bin_dir=$(swift build --package-path "$root/clients/macos" \
    --scratch-path "$build_root/swiftpm" --cache-path "$build_root/cache" \
    --configuration release --show-bin-path)
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin_dir/CrossflowMac" "$app/Contents/MacOS/CrossflowMac"
cp "$keychain_helper" "$app/Contents/MacOS/crossflow-keychain"
cp "$core_executable" "$app/Contents/MacOS/crossflow"
if [ -f "$app/Contents/MacOS/crossflow-invite" ]; then
    rm -- "$app/Contents/MacOS/crossflow-invite"
fi
cp "$root/clients/macos/Resources/Info.plist" "$app/Contents/Info.plist"
if [ -f "$root/assets/AppIcon-1024.png" ] && command -v sips >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
    iconset="$build_root/AppIcon.iconset"
    rm -rf "$iconset"
    mkdir -p "$iconset"
    for spec in \
        "16:icon_16x16.png" \
        "32:icon_16x16@2x.png" \
        "32:icon_32x32.png" \
        "64:icon_32x32@2x.png" \
        "128:icon_128x128.png" \
        "256:icon_128x128@2x.png" \
        "256:icon_256x256.png" \
        "512:icon_256x256@2x.png" \
        "512:icon_512x512.png" \
        "1024:icon_512x512@2x.png"; do
        sz="${spec%%:*}"
        name="${spec#*:}"
        sips -z "$sz" "$sz" "$root/assets/AppIcon-1024.png" --out "$iconset/$name" >/dev/null
    done
    iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"
    rm -rf "$iconset"
fi
cp "$root/clients/design-tokens.json" "$app/Contents/Resources/design-tokens.json"
cp "$root/examples/layout-two-devices.json" "$app/Contents/Resources/layout-two-devices.json"
cp "$root/examples/layout-four-screens.json" "$app/Contents/Resources/layout-four-screens.json"
# Use the explicitly selected native library; never package an older target/release artifact.
mkdir -p "$app/Contents/Frameworks"
cp "$bridge_library" "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
codesign --force --sign - "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
if [ -z "${CROSSFLOW_MACOS_KEYCHAIN_HELPER:-}" ]; then
    codesign --force --sign - "$app/Contents/MacOS/crossflow-keychain"
fi
codesign --force --sign - "$app/Contents/MacOS/crossflow"
codesign --force --sign - "$app"
printf 'Built native app: %s\n' "$app"
