# Crossflow for macOS

Native SwiftUI layout editor, built with SwiftPM and packaged as a macOS app.
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

The core validation builds the architecture-specific release library under
`target/aarch64-apple-darwin/release` or `target/x86_64-apple-darwin/release`.
The app build requires that library; `CROSSFLOW_BRIDGE_LIBRARY` can explicitly
select another build. It does not silently use an older `target/release` library.
Neither script launches input sharing.

## Managed identity storage (native validation pending)

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

Neither the native tests nor the packaged Mac Keychain workflow has run yet.
The Mac pairing invitation worker is implemented but has not run natively;
the invitation UI remains pending. A crash after Keychain add
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
an identity file. Native UI receiver startup and packaging of the core executable
are still separate integration work; the current app cannot yet drive this flow.

The app bundles the canonical `clients/design-tokens.json` and shared layouts
from `examples/`. The four-screen example contains two devices; these are
sample entries, not discovered devices. The two-screen example also ships for
the bridge regression checks. Resource files must not be copied into source
as a second set of maintained values.

`--self-test` runs without creating an application window or posting OS input.
Its JSON report checks the real packaged ABI, both samples, overlapping geometry,
unknown-field preservation and Rust rejection, the next-representable-Double
overlap case, Swift numeric round trips, and the actual crossing target.
The XCTest target additionally covers editor history and stale async responses;
it needs an SDK providing XCTest. This Mac's Command Line Tools do not provide
that module, so those XCTest cases have not been executed here.

## Current scope

Native CI on GitHub's arm64 macOS runner now passes for `26909c2`: release
SwiftUI/package build, all six XCTest cases, packaged bridge self-test and app
signature verification. The invitation and Keychain helpers also link natively.
This updates the earlier build-only limitations above; their native storage and
invitation-process behavior, and the user's Mac UI/input remain untested.
See [native CI evidence](../../docs/macos-native-ci-validation-20261029.json).

The core's opt-in [receiver control channel](../../docs/receiver-client-control.md)
adds parent-EOF stop and actual lifecycle status for the next UI integration.
The current SwiftUI screen does not yet launch it.

- Select and drag individual screens, edit draft properties, undo/redo, and
  import/export JSON using native file panels.
- Edits invalidate validation. Late validation responses are discarded, and
  export validates its exact snapshot before presenting the save panel.
- The bridge exposes layout operations only. Session control, device discovery,
  and applying a layout to an input session are not connected to this Mac UI.
  The UI reports that missing integration explicitly.
- The packaged build uses the shared light palette. A native screenshot and
  interaction pass for the updated four-screen build remain to be performed.

The SDK 27 Command Line Tools lack the new SwiftUI `State` macro plugin. The
`ViewState` alias explicitly selects the established property-wrapper type;
the package retains its macOS 14 deployment target.
