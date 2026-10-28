#!/bin/sh
# Native macOS validation only: fake adapters, local sockets and isolated fixtures.
# Never starts actual capture/injection, the native input worker, or a UI.
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo 'This validation must run natively on macOS.' >&2
    exit 2
fi
case "$#:$*" in
    0:) set -- --locked ;;
    1:--offline) set -- --locked --offline ;;
    *) echo 'Usage: sh scripts/validate-mac-core.sh [--offline]' >&2; exit 2 ;;
esac
command -v cargo >/dev/null 2>&1 || {
    echo 'Cargo is not on PATH. Use the existing Rust toolchain, then retry.' >&2
    exit 2
}
compiler_details=$(rustc -vV)
host_target=$(printf '%s\n' "$compiler_details" | sed -n 's/^host: //p')
case "$(uname -m):$host_target" in
    arm64:aarch64-apple-darwin|x86_64:x86_64-apple-darwin) ;;
    *) echo 'Use a native Darwin Rust toolchain matching this Mac architecture.' >&2; exit 2 ;;
esac
translated=$(sysctl -in sysctl.proc_translated 2>/dev/null || printf '0')
if [ "$translated" = 1 ]; then
    echo 'Run from a native Mac shell, not a Rosetta-translated shell.' >&2
    exit 2
fi
set -- "$@" --target "$host_target"
project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$project_root"
mkdir -p "$project_root/runs"
run_dir=$(mktemp -d "$project_root/runs/mac-core-validation-$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
# Consistent location for the fixture and native CLI; only this script's process
# environment is changed. --locked prevents dependency resolution updates.
export CARGO_TARGET_DIR="$project_root/target"
export CROSSFLOW_WORKER_FIXTURE="$CARGO_TARGET_DIR/$host_target/debug/examples/worker_fixture"
export GIT_OPTIONAL_LOCKS=0
printf '%s\n' running > "$run_dir/status.txt"
printf '%s\n' "$run_dir" > "$run_dir/evidence-directory.txt"
printf 'Evidence directory: %s\n' "$run_dir"

finish_status() {
    result=$?
    trap - 0
    if [ "$result" -ne 0 ]; then
        printf 'failed:%s\n' "$result" > "$run_dir/status.txt"
        printf 'Validation stopped. Inspect %s\n' "$run_dir" >&2
    fi
    exit "$result"
}
trap finish_status 0

run_step() {
    step=$1
    shift
    printf 'Checking %s ...\n' "$step"
    if "$@" > "$run_dir/$step.log" 2>&1; then
        printf '%s\tpassed\n' "$step" >> "$run_dir/steps.tsv"
    else
        result=$?
        printf '%s\tfailed\t%s\n' "$step" "$result" >> "$run_dir/steps.tsv"
        tail -n 30 "$run_dir/$step.log" >&2
        exit "$result"
    fi
}

run_step source-revision git rev-parse HEAD
run_step source-status git status --porcelain --untracked-files=normal
run_step compiler rustc -vV
run_step operating-system uname -a
run_step format cargo fmt --all -- --check
run_step unit-and-bridge cargo test --workspace "$@"
run_step lint cargo clippy --workspace --all-targets "$@" -- -D warnings
run_step native-release cargo build --workspace --release "$@"
run_step fake-worker-build cargo build --example worker_fixture "$@"
run_step security-and-transport cargo test --lib "$@" -- --ignored --test-threads=1
run_step input-and-recovery cargo test --test input_transport --test session_recovery "$@" -- --ignored --test-threads=1
run_step cooperative-stop cargo test --test shutdown_process "$@" -- --ignored --test-threads=1
run_step isolated-worker cargo test --test injection_worker_process "$@" -- --ignored --test-threads=1
run_step stream-stalls cargo test --test network_stall "$@" -- --ignored --test-threads=1 --nocapture
# Read-only display/permission preflight. This command posts no input and does
# not request Accessibility/Input Monitoring approval or create a UI window.
run_step native-environment "$CARGO_TARGET_DIR/$host_target/release/crossflow" environment
run_step source-status-after git status --porcelain --untracked-files=normal
printf '%s\n' passed > "$run_dir/status.txt"
printf 'Native build and fake-input validation passed. Evidence: %s\n' "$run_dir"
printf 'Validated binary: %s\n' "$CARGO_TARGET_DIR/$host_target/release/crossflow"
printf 'Built shared bridge: %s\n' "$CARGO_TARGET_DIR/$host_target/release/libcrossflow_bridge.dylib"
printf '%s\n' 'For a later attended test, pass this path with start-mac.sh --binary; the default target/release binary may be older.'
printf '%s\n' 'Actual key release, Escape, permission changes, visible smoothness and ShareMouse comparison remain untested.'
