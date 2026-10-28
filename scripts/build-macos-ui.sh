#!/bin/sh
# Build only. Never launches the app, receiver, capture, or input injection.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_root="$root/target/macos-ui"
app="$root/dist/Crossflow.app"
test -f "$root/clients/design-tokens.json" || { printf '%s\n' 'Missing shared clients/design-tokens.json' >&2; exit 1; }
test -f "$root/examples/layout-two-devices.json" || { printf '%s\n' 'Missing shared example layout' >&2; exit 1; }
case "$(uname -m)" in
    arm64) native_target=aarch64-apple-darwin ;;
    x86_64) native_target=x86_64-apple-darwin ;;
    *) printf '%s\n' 'Unsupported Mac architecture' >&2; exit 1 ;;
esac
bridge_library="${CROSSFLOW_BRIDGE_LIBRARY:-$root/target/$native_target/release/libcrossflow_bridge.dylib}"
keychain_helper="$root/target/$native_target/release/crossflow-keychain"
test -f "$bridge_library" || { printf 'Build native Rust bridge first: %s\n' "$bridge_library" >&2; exit 1; }
test -f "$keychain_helper" || { printf 'Build native Rust Keychain helper first: %s\n' "$keychain_helper" >&2; exit 1; }
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
cp "$root/clients/macos/Resources/Info.plist" "$app/Contents/Info.plist"
cp "$root/clients/design-tokens.json" "$app/Contents/Resources/design-tokens.json"
cp "$root/examples/layout-two-devices.json" "$app/Contents/Resources/layout-two-devices.json"
cp "$root/examples/layout-four-screens.json" "$app/Contents/Resources/layout-four-screens.json"
# Use the explicitly selected native library; never package an older target/release artifact.
mkdir -p "$app/Contents/Frameworks"
cp "$bridge_library" "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
codesign --force --sign - "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
codesign --force --sign - "$app/Contents/MacOS/crossflow-keychain"
codesign --force --sign - "$app"
printf 'Built native app: %s\n' "$app"
