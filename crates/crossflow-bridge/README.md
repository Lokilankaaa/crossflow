# Native client bridge

`crossflow-bridge` provides the same read-only Rust core to WinUI and SwiftUI.
The public contract is [`include/crossflow.h`](../../include/crossflow.h). It
does not create a connection, read pairing identities, capture input or inject
events. Invoke layout validation after user edits, not on the input hot path.

## Build

From the repository root:

```text
cargo build -p crossflow-bridge --release --locked
cargo test -p crossflow-bridge --locked
```

The workspace owns build profiles. Use `panic=unwind` so recoverable Rust panics
are caught by the ABI boundary. Invalid caller pointers, allocation failure and
process aborts are outside that guarantee. Clients must verify
`crossflow_bridge_api_version() == 1` before sending requests.

The caught panic payload is never included in an ABI return value or JSON error.
Rust invokes the process's panic hook before unwinding, so that hook may still
write diagnostics to stderr. This bridge does not replace the host's global
panic hook and does not guarantee privacy of all host diagnostics.

The library builds as `cdylib`, `staticlib`, and `rlib`. Native clients can load
`crossflow_bridge.dll` on Windows or link `libcrossflow_bridge.dylib` / `.a` on
macOS. The root core has no dependency on this crate or either UI framework.

## Requests and results

All messages are UTF-8 JSON without a required NUL. Caller-owned output buffers
use a two-call size probe; no allocator ownership crosses the ABI. Request and
response sizes are bounded to 1 MiB. Numeric null/alignment/overlap/overflow
mistakes are rejected; callers still own pointer accessibility and lifetime.

`{"op":"environment"}` returns actual process `os`, `arch`, bridge version and
core wire version. `environment_scope` is `process_and_bridge` and
`session_status` is `not_exposed`. Capabilities indicate only what this bridge
can do; unavailable connection state is never converted to “disconnected”.

`{"op":"layout_validate","layout":{"screens":[...]}}` delegates parsing and
validation to `crossflow::arrangement::Layout`. Successful result fields are
`valid`, `screen_count`, `layout`, and `applied_to_session:false`.

`{"op":"layout_crossing","layout":{"screens":[...]},"from_screen":"a",`
`"start":{"x":50,"y":50},"end":{"x":150,"y":50}}` returns `crossing` as the
core's crossing structure or null, and `system_input_injected:false`. Coordinates
are shared logical geometry; callers must not supply raw mouse counts as points.

Every response has `api_version:1` and `ok`. Success carries `result`; validation
failure carries `error:{code,message}`. Error codes are `invalid_request`,
`invalid_layout`, and `invalid_crossing`; messages never echo submitted strings.
ABI status 0 means a complete response was written, including an `ok:false`
validation response. See the header for the remaining numeric statuses.

## Real C ABI smoke

`tests/abi_smoke.c` is a separate C consumer of the public header/library. It
tests exact/insufficient capacity, no implicit terminator, malformed JSON and
UTF-8, null/oversized/overlapping regions, invalid geometry, shared-edge/gap
crossings, and isolation between calls. It never activates OS input.

Example macOS invocation after building the release library:

```sh
cc -std=c11 -Wall -Wextra -Werror -I include crates/crossflow-bridge/tests/abi_smoke.c \
  -L target/release -lcrossflow_bridge -Wl,-rpath,"$PWD/target/release" \
  -o target/release/crossflow-bridge-c-abi
target/release/crossflow-bridge-c-abi
```

On Windows, use a C compiler matching the built library/import library and put
the smoke executable next to `crossflow_bridge.dll` before running it. On macOS,
signing/runtime search paths for the final app remain packaging responsibilities.
