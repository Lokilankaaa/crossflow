#!/bin/sh
# Relative paths are resolved from the repository root. No keys are created.
set -eu

usage() {
    cat <<'USAGE'
Usage: sh scripts/start-mac.sh --bind IP:PORT [options]
  --identity DIR     Existing Mac identity bundle (default: .pairing/mac)
  --seconds N        Test duration, integer 1..3600 (default: 180)
  --gain N           Positive decimal <=10 (default: 1; used with --inject)
  --log-dir DIR      Directory for unique logs (default: runs)
  --inject           Explicitly enable macOS cursor movement; default is DrySink
  --inject-actions   Experimental keyboard/buttons/wheel; also requires --inject
  --help             Show help without starting a receiver

Example: sh scripts/start-mac.sh --bind 192.168.50.2:24800 --inject
For a Windows -DryRunSynthetic test, omit --inject. Synthetic traffic does not
measure physical mouse sampling or visible cursor smoothness.
USAGE
}
fail() { printf '%s\n' "$*" >&2; exit 2; }
need_value() { [ "$#" -ge 2 ] && [ -n "$2" ] || fail "Missing value for $1"; }

bind_address=
identity=.pairing/mac
seconds=180
gain=1
log_directory=runs
inject=false
inject_actions=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --bind) need_value "$@"; bind_address=$2; shift 2 ;;
        --identity) need_value "$@"; identity=$2; shift 2 ;;
        --seconds) need_value "$@"; seconds=$2; shift 2 ;;
        --gain) need_value "$@"; gain=$2; shift 2 ;;
        --log-dir) need_value "$@"; log_directory=$2; shift 2 ;;
        --inject) inject=true; shift ;;
        --inject-actions) inject_actions=true; shift ;;
        --help|-h) usage; exit 0 ;;
        *) fail "Unknown option: $1 (use --help)" ;;
    esac
done
[ "$inject_actions" != true ] || [ "$inject" = true ] || fail '--inject-actions requires --inject.'
[ -n "$bind_address" ] || fail 'Supply --bind with the Mac interface IP and port, e.g. 192.168.50.2:24800.'
case "$bind_address" in *:*) ;; *) fail '--bind must include a port, e.g. 192.168.50.2:24800.' ;; esac
case "$seconds" in ''|*[!0-9]*) fail '--seconds must be an integer from 1 to 3600.' ;; esac
awk -v value="$seconds" 'BEGIN { exit !(length(value) <= 4 && value >= 1 && value <= 3600) }' || fail '--seconds must be from 1 to 3600.'
awk -v value="$gain" 'BEGIN { exit !(value ~ /^[0-9]+([.][0-9]+)?$/ && value > 0 && value <= 10) }' || fail '--gain must be a positive decimal no greater than 10.'

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
binary=$project_root/target/release/crossflow
[ -f "$binary" ] && [ -x "$binary" ] || fail "Release binary missing or not executable: $binary. Build first with cargo build --release."
case "$identity" in /*) ;; *) identity=$project_root/$identity ;; esac
[ -f "$identity/identity.json" ] || fail "Identity bundle is missing: $identity. Supply the existing Mac bundle with --identity."
case "$log_directory" in /*) ;; *) log_directory=$project_root/$log_directory ;; esac
mkdir -p -- "$log_directory"
# mktemp creates a unique directory atomically; the Rust logger creates its file
# exclusively inside it. This works on macOS without GNU date or UUID tools.
run_directory=$(mktemp -d "$log_directory/mac-$(date -u +%Y%m%d-%H%M%S)-XXXXXXXX")
log_path=$run_directory/receiver.jsonl
set -- receive --bind "$bind_address" --identity "$identity" --log "$log_path" --seconds "$seconds" --gain "$gain"
printf 'Crossflow receiver: %s | %s seconds | gain %s\nLog: %s\n' "$bind_address" "$seconds" "$gain" "$log_path"
if [ "$inject" = true ]; then
    set -- "$@" --inject
    if [ "$inject_actions" = true ]; then
        set -- "$@" --inject-actions
        printf '%s\n' 'EXPERIMENTAL INPUT: keys/buttons/wheel enabled. Forced process termination cleanup is not guaranteed; use only an attended test.'
    fi
    printf '%s\n' 'LIVE CURSOR MODE: only use a physical Windows sender, not -DryRunSynthetic.'
    printf '%s\n' 'Allow required macOS permissions. Hold Escape on the Mac to stop; Ctrl+C also exits.'
    printf '%s\n' 'On Windows, Ctrl+Alt+Space starts/pauses sharing; Ctrl+Alt+Esc stops it.'
else
    printf '%s\n' 'DRYSINK MODE: receives and measures traffic without moving the Mac cursor.'
    printf '%s\n' 'Use --inject explicitly for a physical mouse/visible smoothness test. Ctrl+C exits.'
fi
# Preserve every argument, including identity paths with spaces. No eval or shell
# command construction. exec preserves signals and the receiver exit status.
exec "$binary" "$@"
