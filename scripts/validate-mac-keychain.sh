#!/bin/sh
# Explicit native Keychain validation. Creates only random test items in the
# Crossflow service namespace, then deletes those exact items. No OS input.
set -eu
if [ "$(uname -s)" != Darwin ]; then
    echo 'This validation must run natively on macOS.' >&2
    exit 2
fi
case "$#:$*" in
    0:) set -- --locked ;;
    1:--offline) set -- --locked --offline ;;
    *) echo 'Usage: sh scripts/validate-mac-keychain.sh [--offline]' >&2; exit 2 ;;
esac
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_root"
host_target=$(rustc -vV | sed -n 's/^host: //p')
case "$(uname -m):$host_target" in
    arm64:aarch64-apple-darwin|x86_64:x86_64-apple-darwin) ;;
    *) echo 'Use a native Rust toolchain matching this Mac.' >&2; exit 2 ;;
esac
mkdir -p "$project_root/runs"
run_dir=$(mktemp -d "$project_root/runs/mac-keychain-validation-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
export CARGO_TARGET_DIR="$project_root/target"
export CROSSFLOW_KEYCHAIN_HELPER="$CARGO_TARGET_DIR/$host_target/debug/crossflow-keychain"
git rev-parse HEAD > "$run_dir/revision.txt"
git status --porcelain > "$run_dir/source-status.txt"
printf 'Evidence directory: %s\n' "$run_dir"
cargo build --bin crossflow-keychain "$@" --target "$host_target" > "$run_dir/build.log" 2>&1
if cargo test --lib "$@" --target "$host_target" macos_keychain -- --ignored --test-threads=1 > "$run_dir/tests.log" 2>&1; then
    printf '%s\n' passed > "$run_dir/status.txt"
    printf '%s\n' 'Native isolated Keychain checks passed; cross-process packaged app and pairing UI still need separate validation.'
else
    result=$?
    printf 'failed:%s\n' "$result" > "$run_dir/status.txt"
    tail -n 40 "$run_dir/tests.log" >&2
    exit "$result"
fi
