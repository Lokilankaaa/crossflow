# Rust SPAKE2 integration experiment

Independent workspace containing the experimental shared pairing library and the
Windows enrollment and macOS invitation workers. The opt-in Windows package uses its worker;
this is not a completed cross-platform pairing release. No downloads are performed
by Cargo's build script. The native library must be built from the
revision and checksum in [../source-lock.json](../source-lock.json), using the
existing parent CMake experiment. The build script checks that the explicit static
archive exists; it cannot establish the provenance of a caller-supplied archive.

## Windows client integration — 2026-10-29

The WinUI device page browses `_crossflow-pair._tcp.local.`, separately from
trusted input-route discovery. It starts the bundled Rust `crossflow-enroll`
worker over private standard streams, verifies an eight-digit invitation code,
shows the authenticated device name, and requires explicit approval on both ends.
The worker exchanges bounded device metadata inside the authenticated transport,
retains the locally generated private key, and saves the resulting identity to
the application-managed Windows DPAPI peer store. Neither the code nor private
key is passed in process arguments or logged. The code is not a reusable password.

Build with `scripts/build-windows-ui.ps1 -WithPairing -PairingLibraryDir <absolute-directory>`.
The explicit directory must contain the pinned BoringSSL `libcrypto.a`; the
packaging script also copies the three tested GNU runtime DLLs. Missing components
disable the pairing entry point. Root Cargo does not build this independent
workspace automatically. See [Windows client instructions](../../../clients/windows/README.md).

Actual WinUI testing with a same-machine LAN responder covered discovery, code
entry, bilateral approval, protected storage, connection with the existing Noise KK
protocol in paused state, and cancellation while waiting for the responder's
approval. The responder used a Mac protocol role on Windows with a DrySink;
there was no macOS execution, Keychain access or OS input injection. See
[validation evidence](../../../docs/windows-pairing-client-validation-20261029.json).

The Windows worker currently initiates enrollment only. Mac invitation UI,
native worker/Keychain validation, cross-machine validation, multi-address enrollment
fallback and distributed save recovery/revocation remain outstanding. The first
valid discovered address is attempted. Cancellation before final authorization
does not save trust; cancellation during an already authorized OS save cannot
promise rollback, so the UI refreshes the stored-device list rather than claiming
that every cancellation removed a record.

The C ABI is taken directly from that revision's `curve25519.h`, `hmac.h`,
`digest.h`, and `mem.h`. BoringSSL does not promise ABI stability; upgrading the
native library requires reviewing these bindings and rerunning the tests.

## Invitation host and macOS worker — 2026-10-29

`host::serve_one` owns one 120-second invitation, advertises its pairing service,
and accepts at most three authentication attempts using the existing process-wide
budget. Approval is bound to the authenticated attempt token; early, stale or
duplicate approvals fail closed. Cancellation, control EOF, output backpressure
and uncertain cleanup return no authorized keys. The input listener is reserved
only: it never accepts or injects input. Discovery does not establish trust.

The private `crossflow-invite` macOS process wraps this host and saves a bilaterally
approved peer through the managed Keychain store. Build natively with
`sh scripts/build-macos-pairing.sh --offline`, supplying the absolute native pinned
archive directory in `CROSSFLOW_BORINGSSL_LIB_DIR`. The script does not download
dependencies or start sharing. The UI packager includes the worker when available;
the SwiftUI invitation screen is not integrated yet.

The worker takes no arguments. Its inherited stdin accepts bounded JSON lines:
`begin` with `name`, `approve` with the current `attempt`, or `cancel`.
Private stdout emits `invitation_ready`, `code`, `pending`, `attempt_failed`,
`discovery` and `saved` events. Never capture this stream in logs: the `code`
event is for UI display only. Its `lifetime_seconds` is the original duration,
not a refreshed countdown. The authenticated `pending` event carries the attempt
token, peer name/platform and fingerprint. Closing stdin cancels the exchange.

`saved` reports the stored peer ID and a suggested receiver port with
`input_ready:false`; success means pairing was saved, not that input started.
The client must separately start the core receiver using `receive --peer <id>`
and discoverable mode. Injection remains an explicit separate choice. A sender
that advertises no listening port is stored without a target endpoint; its
ephemeral connection port is never treated as a listening address. Cancellation
during an already-authorized OS save can still commit, so the UI must refresh its
saved-device list after cancellation or an uncertain result.

Windows validation covered 29 library tests, three Windows worker unit tests,
two compile-fail doctests, and two separately invoked host tests: actual TCP
wrong-code limits/success/handshake cancellation, and actual LAN wildcard-bind
discovery/cancellation. Root peer-store tests (12) and CLI tests (4) passed.
Root and pairing all-target Clippy passed for Windows and Apple targets. Apple
checks only typecheck; the supplied Windows archive was not linked for macOS.
Native Mac linking, Keychain access, worker execution and cross-device UI pairing
remain untested. See [validation scope](../../../docs/macos-invitation-host-validation-20261029.json).

## Build and run on the existing Windows toolchain

From the repository root, with the prebuilt pinned archive:

```powershell
$env:CROSSFLOW_BORINGSSL_LIB_DIR = 'C:/Users/loki/Downloads/temp/crossflow-pairing-spike/build/boringssl'
./scripts/cargo-local.ps1 test --offline --locked --manifest-path experiments/pairing-spake2/rust-pairing/Cargo.toml
```

The socket tests are opt-in. In an authorized localhost test environment:

```powershell
./scripts/cargo-local.ps1 -CargoArgs @('test','--manifest-path','experiments/pairing-spake2/rust-pairing/Cargo.toml','--locked','--offline','--test','noise_tcp','--','--ignored','--test-threads=1')
```

These tests perform actual three-message Noise XX handshakes, derive channel
binding from their completed states, and exchange SPAKE2/confirmation records
inside those transports. The middleman fixture terminates two independent XX
connections and relays both record types; both endpoints must reject specifically
at confirmation, rather than merely timing out. Correct codes, wrong codes,
changed transcripts, reordered keys, wrong roles, reflected/corrupt confirmation,
truncated messages, cancellation and a fragmented-header total deadline are also
covered. They use generated transient keys and fixed synthetic codes only.

Windows validation on 2026-10-29: 7 unit tests, 2 compile-fail ownership doctests
and 3 explicit TCP tests passed; all-target Clippy with warnings denied passed.
See [result-windows.json](result-windows.json). No production root dependency was
added and no native Mac run or cryptographic audit has been completed.

The later invitation-gate run adds 14 unit tests and 3 actual TCP tests; the
original checks still pass. See [result-invitation-windows.json](result-invitation-windows.json).
Run the whole explicit socket suite with the same pinned library:

```powershell
./scripts/cargo-local.ps1 -CargoArgs @('test','--manifest-path','experiments/pairing-spake2/rust-pairing/Cargo.toml','--locked','--offline','--tests','--','--ignored','--test-threads=1')
```

## Invitation lifecycle experiment

`invitation::InvitationManager` wraps the actual SPAKE2 states. It generates a
random eight-digit local display code, expires it after 120 seconds, and charges
at most three reservations per invitation. Public managers share a process-wide
rolling limit of five reservations per 60 seconds and one in-flight reservation.
Reserve before doing Noise work. Failure, cancellation, replacement, dropped
attempts and constructing a new manager do not refund or reset those limits.
Keep the pairing host alive across connections; a process restart forgets the
budget but also destroys every old invitation. Codes must never be persisted or
restored into a newly started process.

The lifecycle is `replace → reserve → start(binding) → message/process →
tag/verify_peer → PendingApproval → approve`. The initiator enters the displayed
code through the low-level API; the invitation issuer is the responder. Cancellation,
replacement, expiry or actual manager destruction invalidates every old pending
stage. An in-flight ticket keeps the shared busy slot until it is dropped, even
after cancellation; the caller must cancel its bounded network I/O and dispose it.
Decision time is checked after acquiring the relevant locks, including approval.
Admission and approval are atomic against concurrent cancellation/replacement.

The code uses rejection sampling over BoringSSL's OS-backed random generator.
`DisplayCode` is zeroizing and deliberately has no Debug/Display/Serialize/Clone
implementation; `as_bytes()` exists only for local UI display/input. Caller-made
copies and rendered text remain the caller's responsibility. No code is broadcast
or logged by this module. A returned message/tag cannot be revoked retroactively;
the networking layer still needs its own cancellation and total deadline.

`ApprovedBinding` contains only the public binding and role. It records a local
approval at that instant, not remote receipt, bilateral completion, durable trust,
or authorization to send input. Later cancellation cannot retrospectively revoke
that returned evidence. The low-level invitation TCP fixtures discard their keys.
The reusable transport and opt-in persistence fixture below retain local keys
without transferring private keys between devices. The client UI, discoverable
pairing listener and Mac native build remain unimplemented/unverified; this
experiment is not first-install product pairing.

The three invitation TCP tests cover correct random-code authentication plus
explicit approval and replay rejection; three actual wrong-code reconnections
followed by pre-Noise rejection of a correct fourth attempt; and cancellation
after successful cryptographic verification but before approval. They use five
reservations in total and serialize fixtures so Cargo's default test scheduler
does not conflict with the real process-wide single-in-flight policy.

`cargo-local.ps1` supplies the LLVM-MinGW linker and puts its runtime DLLs on PATH.
The native archive uses `libc++.dll`, `libunwind.dll`, and `libwinpthread-1.dll`.
Do not distribute a test executable without those runtime dependencies. On macOS,
build the same pinned CMake target natively, set the environment variable to its
`boringssl` output directory, and run Cargo with the local Rust/Clang toolchain.
macOS linking is supported by the build script but has not yet been verified.

## Retained local keys and bilateral approval

`transport::initiate` and `transport::respond` use the actual Noise XX and bound
SPAKE2 exchange, retain each endpoint's locally generated static private key, and
return a `PendingSession` without accessible keys. The responder requires an
`AttemptPermit` reserved before Noise work. Both endpoints must explicitly call
`approve`; the same encrypted channel exchanges exact versioned, role-labelled
Approval then Ready frames. Only then does `EnrolledKeys` expose a zeroizing local
private key and the authenticated peer's public key. No private key crosses the
network. Pair IDs use the first 16 bytes of the completed Noise handshake hash.

One deadline, at most 120 seconds, covers authentication, local approval waiting
and both control exchanges. Blocking I/O retries in slices of at most 100 ms,
checking cancellation without renewing the deadline. The responder retains its
process-wide busy slot through the final exchange; the original invitation expiry
and cancellation still apply after local approval. Drop or rejection shuts down
the socket and drops the retained key. The caller must promptly dispose abandoned
pending sessions; no background thread expires an idle Rust value on its behalf.

The root core's trusted adapter `Identity::from_authorized_pairing` accepts already
authenticated and locally approved keys, validates them, and constructs an
identity in memory. It does not authenticate arbitrary network data itself.
Windows `PeerStore::remember_authorized` saves that identity directly with DPAPI
and commits public peer metadata last. No intermediate plaintext identity file or
user-selected identity directory is needed. The Windows client now uses these
interfaces through its packaged worker; the root library does not depend on this
separate pairing crate.

The Windows-only `enrollment_tcp` opt-in tests use this transport directly and
exercise successful approval, protected save/reload, then a new connection using
the root core's existing Noise KK protocol in both directions. Both test roles run
on Windows: protecting the responder's identity with DPAPI is **not** macOS
Keychain validation. They also cover peer decline and silent-handshake cancellation.
Test credentials are deleted from temporary directories; reports remain under
this crate's ignored `runs/` directory. They never access the real user's peer
store or enable OS input.

The additional `transport_cancel` fixture leaves the separate cancellation flag
false and cancels only the invitation manager while the responder waits for the
first Noise message or the peer's PAKE message. Both cases must return the specific
invitation-revocation error, close the socket within one second and release the
process-wide slot. The responder's read-only invitation observer is checked on
every network wait, then replaced by the held approval observer after consent.

Windows validation on 2026-10-29: 26 unit tests, 2 compile-fail doctests and all 10
opt-in TCP tests passed. Root tests, both all-target Clippy runs and the root Apple
target typecheck also passed. See [result-enrollment-windows.json](result-enrollment-windows.json)
for evidence and the distinction between Windows localhost and native Mac testing.

This is not an atomic distributed commit. A lost final message, crash or last-boundary
cancellation can leave one endpoint successful and the other unsuccessful. A
production enrollment flow still needs durable recovery/retry and revocation,
Mac protected storage and Mac native UI integration. The Windows worker/UI
integration above supplements these earlier fixtures; the fixtures alone do not
establish that cross-platform first-install pairing is ready.

## Ownership and authentication boundary

1. The network caller completes a fresh Noise XX handshake and obtains its 32-byte
   handshake hash and both static public keys, in initiator/responder order.
2. `Pairing::start(role, code, binding)` accepts exactly eight ASCII digits. The
   low-level caller owns code generation, display/input, expiration, and online-attempt limits;
   the issuer-side `invitation` wrapper above supplies the lifecycle checks.
   One-use Rust contexts alone do not enforce one guess per invitation: reopening
   a connection with the same code must not grant another attempt in the product.
   Codes and keys are never included in errors. The caller must erase its own code
   buffer; the adapter erases its temporary copy.
3. Exchange `message()`, then consume each `Pairing` with `process(peer_message)`.
   There is no retry on a state, including after malformed input or processing failure.
   Returning `Confirmation` alone does **not** authenticate a peer.
4. Send both `Confirmation::tag()` values before reading the peer tag. Consume each
   state with `verify_peer(peer_tag)`. It uses BoringSSL's constant-time comparison.
5. A successful `AuthenticatedBinding` contains only public binding and role. It
   proves local peer confirmation, not remote receipt of our tag or durable consent.
   The caller must finish its own send/ack and trust-persistence policy and must never
   save trust after cancellation, timeout, or failure. This primitive saves no trust;
   the opt-in enrollment fixture separately tests temporary protected storage.

Role-labelled identities bind the complete Noise hash and ordered static keys into
SPAKE2. The HMAC-SHA256 confirmation input includes a separate fixed domain, sending
role, and the same fixed-width binding. The intermediate 64-byte key is used only
inside `process`, erased before return, and never exposed through the public API.
No encryption primitive or short-code hash authentication is implemented here.

Native contexts are single-owner RAII allocations. The pinned default BoringSSL
allocator cleanses memory on `OPENSSL_free`; Rust secret buffers use `zeroize`.
This is not a promise to erase caller-made copies, crash dumps, swapped pages, or
register copies. No custom BoringSSL allocator is enabled in the tested build.

Unit tests exercise matching/wrong codes, role and binding mismatch, reflection,
tampered or malformed messages/tags, fresh-attempt replay, and binding bounds.
Compile-fail doctests ensure processing and confirmation cannot be reused.
Passing these tests is functional evidence, not a cryptographic audit or evidence
that the complete product pairing flow is ready to ship.
