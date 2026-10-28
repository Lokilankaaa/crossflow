# Crossflow for macOS

Native SwiftUI layout editor and nearby-device browser, built with SwiftPM and
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

This discovery change was prepared on Windows without a Swift compiler. It has
**not been compiled or executed on macOS**: the new self-test and XCTest cases,
Swift 6 concurrency checks, bundled child launch/signature, network permission
prompt, actual LAN results, cancel/quit behavior and UI rendering still need
native validation. Script syntax and plist parsing are static checks only.

After the build and self-test commands above pass on a Mac, open the app and
choose **附近设备**. Check first entry, refresh, stop, leave/re-enter and quit while
scanning against another device advertising Crossflow. A valid empty result is
different from a failed scan. Keep native results with the tested revision.

## Current scope

- Select and drag individual screens, edit draft properties, undo/redo, and
  import/export JSON using native file panels.
- Edits invalidate validation. Late validation responses are discarded, and
  export validates its exact snapshot before presenting the save panel.
- The nearby-device page runs the bundled Rust CLI with `discover --seconds 3`
  on entry and refresh, showing names, platforms and unpaired status from its
  result. Pairing is visibly disabled; discovery does not authorize input.
- Discovery has an eight-second child deadline, bounded pipe output, strict JSON
  validation and cooperative cancellation with bounded termination escalation.
  Failed child cleanup retains its slot and prevents a second scan. App quit
  waits for cleanup; if cleanup cannot be confirmed, quit is cancelled and the
  page offers another stop attempt.
- The app declares its local-network use and `_crossflow._tcp` Bonjour service.
  IP addresses and identifiers are not part of the nearby-device workflow.
- The bridge still exposes layout operations only. Pairing, session control,
  and applying a layout to an input session are not connected to this Mac UI.
- The packaged build uses the shared light palette. A native screenshot and
  interaction pass for the updated four-screen build remain to be performed.

The SDK 27 Command Line Tools lack the new SwiftUI `State` macro plugin. The
`ViewState` alias explicitly selects the established property-wrapper type;
the package retains its macOS 14 deployment target.
