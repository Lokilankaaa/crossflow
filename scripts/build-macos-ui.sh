#!/bin/sh
# Build only. Never launches the app, receiver, capture, or input injection.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
build_root="$root/target/macos-ui"
app="$root/dist/Crossflow.app"
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
codesign --force --sign - "$app"
printf 'Built native app: %s\n' "$app"
