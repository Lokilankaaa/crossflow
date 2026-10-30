# Crossflow for macOS

Native SwiftUI layout editor and device connection page, built with SwiftPM and
packaged as a macOS app. The layout editor remains the default opening page.
The editor preserves unknown JSON fields for Rust to reject; layout validation
and crossing decisions run through the bundled `crossflow-bridge` C ABI.

## Build and verify

Use a native Mac Rust toolchain and Swift command-line tools:

```sh
sh scripts/validate-mac-core.sh --offline
sh scripts/build-macos-ui.sh
dist/Crossflow.app/Contents/MacOS/CrossflowMac --self-test runs/macos-ui-self-test.json
codesign --verify --deep --strict dist/Crossflow.app
```

The core validation builds the architecture-specific release library and CLI under
`target/aarch64-apple-darwin/release` or `target/x86_64-apple-darwin/release`.
The app build requires both; `CROSSFLOW_BRIDGE_LIBRARY` and
`CROSSFLOW_CORE_EXECUTABLE` can explicitly select other native builds. The CLI is
copied to `Contents/MacOS/crossflow` and signed before the containing app.
The script does not silently use older `target/release` artifacts.
Neither script launches input sharing.

## Input permissions belong to the app

The receiver needs Accessibility (including event posting) and Input Monitoring
for the local Escape stop check. A successful `environment` command launched by
Codex or Terminal checks that host's context, not the app-launched receiver.
The user must grant the packaged Crossflow app access in System Settings →
Privacy & Security → Accessibility and Input Monitoring, then fully quit and
reopen it. No script or client automatically changes these permissions.

Before creating an input adapter or worker, the receiver now checks its own
permissions. A denied preflight reports `startup_rejected`; the UI displays the
missing permission and allows retry/quit after the process exits normally.
Other errors, malformed status, signal exits and forced cleanup remain
unconfirmed. Rebuilding an ad-hoc signed app may require checking its permissions
again; stable signing and access across upgrades are not yet validated.

## Managed identity storage

The shared Rust core now supports the Mac role in its managed peer store at
`~/Library/Application Support/Crossflow/peers`. Secret identity bytes are stored
in the default local file-based Keychain; `identity.keychain` contains only a
bounded locator in the fixed Crossflow service. This is not a file that users
need to choose, copy or share. A damaged/foreign protected record cannot activate
legacy plaintext fallback. Windows continues to use current-user DPAPI.

Keychain calls run in the bundled `crossflow-keychain` helper with no command-line
secrets. Only that dedicated process disables its own Keychain interaction; the
UI process and system settings are unchanged. Locked or inaccessible Keychains
fail explicitly. The caller waits up to ten seconds for normal completion, then
attempts bounded termination; inability to confirm exit is reported as an unknown
outcome, with a background reaper retaining the child. The helper never unlocks
a Keychain or modifies its access list. Stable signing/ACL access across app
updates still needs native validation; no device-only protection is claimed.

The explicit file-based backend supports the local development packaging model.
Apple's data-protection Keychain has different entitlement/provisioning rules:
[Apple TN3137](https://developer.apple.com/documentation/technotes/tn3137-on-mac-keychains).
There is no automatic fallback between these backends.

`validate-mac-core.sh` skips tests that write Keychain items. To opt into isolated
native Keychain tests, run `sh scripts/validate-mac-keychain.sh --offline` from a
logged-in Mac session. It builds the native helper, uses random test accounts,
and removes the exact created items. The tests cover reopen, corrupted locators,
missing items, isolation between two entries, and loading the stored identity
into the existing in-memory Noise handshake. They do not start input sharing.
Tests use `CROSSFLOW_KEYCHAIN_HELPER` to select the absolute freshly built binary;
normal packaged calls find it beside the executable. The app packaging script
copies and signs that native helper along with the shared bridge.

The native invitation UI is now wired to the worker. Combined native compilation,
26 XCTest cases and isolated Keychain tests pass on the GitHub macOS runner.
Actual UI-driven pairing and signing/ACL continuity on the user's Mac remain
unverified; the isolated storage checks do not prove that complete workflow.
A crash after Keychain add
but before locator commit can orphan an item, and a metadata commit failure can
leave an uncommitted peer directory; full recovery/revocation is not implemented.

To build the optional invitation worker, supply a native pinned BoringSSL archive
via `CROSSFLOW_BORINGSSL_LIB_DIR` and run
`sh scripts/build-macos-pairing.sh --offline` before the UI packaging script.
This packages `crossflow-invite` alongside `crossflow-keychain`. No invitation or
input session starts during a build. The private worker protocol is documented
in the [pairing workspace](../../experiments/pairing-spake2/rust-pairing/README.md).
Its saved result explicitly reports `input_ready:false`. The core now supports
`receive --peer <id>` to resolve managed credentials without asking the user for
an identity file. The device page starts this receiver through its supervised
control channel and the core executable is packaged beside the UI.

## Cross-screen input integration (wire v3)

The device page offers keyboard/mouse/wheel reception and a separate movement-only
mode. Full input is the selected mode, but nothing starts until the user clicks
the labeled receive button. Keyboard input remains the physical ANSI subset;
cross-screen keyboard, dragging and scroll behavior still require attended native
acceptance. Discovery marks other wire versions incompatible. macOS 27's privacy
pane is labeled “设备控制和数据访问” on the tested Chinese system; older systems
use the Accessibility label.

The native adapter enumerates up to 32 awake active displays in global logical
points, keyed by `CGDirectDisplayID`. Absolute positioning must match both a live
display ID and its bounds; relative movement crosses adjacent screens and clamps
gaps to a real display. Moving across monitors does not release held keys/buttons.
Each arm captures a display snapshot. A configuration callback plus current bounds
checks stop new input after topology changes, including a change away and back.
The session/worker must poll `emergency_pressed` while idle and perform cleanup;
the adapter also attempts owned-input releases when its readiness checks fail.
The authenticated layout snapshot and worker display binding must be validated by
the shared core before accepting absolute positions. This code does not itself
choose a remote screen from the layout editor or start sharing automatically.

This source change does not rebuild or replace the fixed phase-zero test app,
re-sign its helpers, alter Keychain access, or solve upgrade signing continuity.
Native compilation and new tests must run against the merged shared wire-v3 core
before packaging; the standalone Mac branch depends on `session::NativeDisplay`.

The app bundles the canonical `clients/design-tokens.json` and shared layouts
from `examples/`. The four-screen example contains two devices; these are
sample entries, not discovered devices. The two-screen example also ships for
the bridge regression checks. Resource files must not be copied into source
as a second set of maintained values.

`--self-test` runs without creating an application window or posting OS input.
Its JSON report checks the real packaged ABI, both samples, overlapping geometry,
unknown-field preservation and Rust rejection, the next-representable-Double
overlap case, Swift numeric round trips, and the actual crossing target. It also
runs the discovery decoder's pure boundary checks, including untrusted status,
duplicate and escaped keys, malformed output, names and endpoints. These checks
create no child processes and open no network sockets.

The XCTest target additionally covers editor history, stale async responses,
discovery child cancellation/restart, timeout/output limits and page re-entry.
Run `swift test --package-path clients/macos` with an SDK providing XCTest. The
previously available Mac Command Line Tools lacked that module; `--self-test`
does not require XCTest.

The combined device-page change was prepared on Windows. Native CI checks Swift 6
compilation, child process fixtures, protocol/model tests, discovery and layout
regressions. Network permissions, actual LAN results, window close/quit behavior
and rendered UI still need verification on the user's Mac. Static checks alone
are not native validation.

After the build and self-test commands above pass on a Mac, open the app and
choose **附近设备** in the sidebar to open the device page. Check first entry,
refresh, stop, leave/re-enter and quit while scanning against another device
advertising Crossflow. A valid empty result is
different from a failed scan. Keep native results with the tested revision.

## Current scope

Native CI on GitHub's arm64 macOS runner passes for `6155f1c`: release
SwiftUI/package build, all 26 XCTest cases, packaged bridge self-test, app
signature verification and isolated Keychain checks. Invitation-process behavior
through the actual UI, and the user's Mac window/input remain untested.
See [combined native evidence](../../docs/macos-client-control-validation-20261029.json).

The core's opt-in [receiver control channel](../../docs/receiver-client-control.md)
passed native Mac process tests at `2b85998`. The current SwiftUI device page
uses it for explicit start/stop; actual window and cross-device interaction
remain to be validated.

- Select and drag individual screens, edit draft properties, undo/redo, and
  import/export JSON using native file panels.
- Edits invalidate validation. Late validation responses are discarded, and
  export validates its exact snapshot before presenting the save panel.
- The device page browses nearby services, generates a short-lived Mac invitation,
  displays the authenticated Windows peer and requires explicit approval. A saved
  identity must match that approved peer. The code is cleared on cancellation or
  completion and never logged or passed in process arguments.
- Saved-device selection resolves managed credentials internally. Reception starts
  only on the explicit button. Movement-only input is the default; keyboard,
  buttons and scroll require the experimental checkbox. Window closure and quit
  close the parent control channel and await cleanup.
- Private process supervision uses bounded nonblocking pipes, null stderr and
  generic errors. Forced or unconfirmed receiver termination prevents restart in
  that client instance. Paired records refresh after success or cancellation,
  including cancellation during an already-authorized save.
- Discovery has an eight-second child deadline, bounded pipe output, strict JSON
  validation and cooperative cancellation with bounded termination escalation.
  Failed child cleanup retains its slot and prevents a second scan. App quit
  waits for cleanup; if cleanup cannot be confirmed, quit is cancelled and the
  page offers another stop attempt.
- The app declares its local-network use and `_crossflow._tcp` and
  `_crossflow-pair._tcp` Bonjour services.
  IP addresses and identifiers are not part of the nearby-device workflow.
- The C bridge still exposes layout operations only. Pairing and session control
  use the packaged Rust processes; applying a draft layout to an input session
  remains separate work.
- The packaged build uses the shared light palette. A native screenshot and
  interaction pass for the updated four-screen build remain to be performed.

The SDK 27 Command Line Tools lack the new SwiftUI `State` macro plugin. The
`ViewState` alias explicitly selects the established property-wrapper type;
the package retains its macOS 14 deployment target.
