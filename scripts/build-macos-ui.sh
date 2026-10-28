#!/bin/sh
# Build only. Never launches the app, receiver, capture, or input injection.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_root="$root/target/macos-ui"
app="$root/dist/Crossflow.app"
test -f "$root/clients/design-tokens.json" || { printf '%s\n' 'Missing shared clients/design-tokens.json' >&2; exit 1; }
test -f "$root/examples/layout-two-devices.json" || { printf '%s\n' 'Missing shared example layout' >&2; exit 1; }
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
cp "$root/clients/macos/Resources/Info.plist" "$app/Contents/Info.plist"
cp "$root/clients/design-tokens.json" "$app/Contents/Resources/design-tokens.json"
cp "$root/examples/layout-two-devices.json" "$app/Contents/Resources/layout-two-devices.json"
# A missing bridge deliberately produces the explicit unavailable UI. Never
# load a library from the working directory or arbitrary environment variables.
bridge_library="$root/target/release/libcrossflow_bridge.dylib"
if [ -f "$bridge_library" ]; then
    mkdir -p "$app/Contents/Frameworks"
    cp "$bridge_library" "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
    codesign --force --sign - "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
elif [ -f "$app/Contents/Frameworks/libcrossflow_bridge.dylib" ]; then
    # Remove only our prior build output to avoid silently shipping a stale bridge.
    rm "$app/Contents/Frameworks/libcrossflow_bridge.dylib"
fi
codesign --force --sign - "$app"
printf 'Built native app: %s\n' "$app"
