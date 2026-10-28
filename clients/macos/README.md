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
