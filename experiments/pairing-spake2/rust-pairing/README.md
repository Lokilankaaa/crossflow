# Rust SPAKE2 integration experiment

Independent workspace, not a production dependency or pairing release. No downloads
are performed by Cargo's build script. The native library must be built from the
revision and checksum in [../source-lock.json](../source-lock.json), using the
existing parent CMake experiment. The build script checks that the explicit static
archive exists; it cannot establish the provenance of a caller-supplied archive.

The C ABI is taken directly from that revision's `curve25519.h`, `hmac.h`,
`digest.h`, and `mem.h`. BoringSSL does not promise ABI stability; upgrading the
native library requires reviewing these bindings and rerunning the tests.

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
that returned evidence. The next integration step needs a separate persistence
commit policy, protected local keys, bilateral completion and revocation handling.
These TCP fixtures discard their transient Noise keys and never create production
identity records. The client UI, pairing listener and Mac native build remain
unimplemented/unverified; this experiment is not first-install product pairing.

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
   save trust after cancellation, timeout, or failure. This experiment saves no trust.

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
