#!/bin/sh
# Build only: no invitation, Keychain access, receiver or input injection starts.
set -eu
if [ "$(uname -s)" != Darwin ]; then
    echo 'Build this package natively on macOS.' >&2
    exit 2
fi
case "$#:$*" in
    0:) set -- --locked ;;
    1:--offline) set -- --locked --offline ;;
    *) echo 'Usage: CROSSFLOW_BORINGSSL_LIB_DIR=/absolute/native/library sh scripts/build-macos-pairing.sh [--offline]' >&2; exit 2 ;;
esac
case "${CROSSFLOW_BORINGSSL_LIB_DIR:-}" in
    /*) test -f "$CROSSFLOW_BORINGSSL_LIB_DIR/libcrypto.a" || { echo 'Native pinned libcrypto.a is missing.' >&2; exit 2; } ;;
    *) echo 'Supply an absolute directory containing the native pinned BoringSSL archive.' >&2; exit 2 ;;
esac
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
native_target=$(rustc -vV | sed -n 's/^host: //p')
case "$(uname -m):$native_target" in
    arm64:aarch64-apple-darwin|x86_64:x86_64-apple-darwin) ;;
    *) echo 'Use a native Rust toolchain matching this Mac.' >&2; exit 2 ;;
esac
# Explicit output and architecture prevent stale default-directory artifacts
# from being packaged when the caller has CARGO_TARGET_DIR/BUILD_TARGET set.
cargo build --manifest-path experiments/pairing-spake2/rust-pairing/Cargo.toml \
    --target-dir "$root/target/macos-pairing" --target "$native_target" \
    --bin crossflow-invite --release "$@"
cargo build --target-dir "$root/target" --target "$native_target" \
    --bin crossflow-keychain --release "$@"
package="$root/target/macos-pairing/package"
mkdir -p "$package"
cp "$root/target/macos-pairing/$native_target/release/crossflow-invite" "$package/crossflow-invite"
cp "$root/target/$native_target/release/crossflow-keychain" "$package/crossflow-keychain"
codesign --force --sign - "$package/crossflow-keychain"
codesign --force --sign - "$package/crossflow-invite"
printf 'Built invitation and Keychain workers: %s\n' "$package"
printf '%s\n' 'No process was started. Native invitation UI and receiver startup are separate integration steps.'
