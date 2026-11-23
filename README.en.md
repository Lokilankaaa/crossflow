<div align="center">

<img src="assets/icon-256.png" alt="Crossflow Icon" width="104" height="104" />

# Crossflow

**Low-latency, end-to-end encrypted P2P mouse, keyboard, clipboard & file sharing**

Windows 11 ↔ macOS · N-Device Full Mesh · Pure Rust Core · Native WinUI 3 & SwiftUI

<p>
  <a href="https://github.com/Lokilankaaa/crossflow/releases/latest">
    <img src="https://img.shields.io/github/v/release/Lokilankaaa/crossflow?style=flat-square&color=5264D9&label=Release" alt="Latest Release" />
  </a>
  <img src="https://img.shields.io/badge/Platform-Windows%2011%20%7C%20macOS%2014%2B-4F847C?style=flat-square" alt="Platform" />
  <img src="https://img.shields.io/badge/Core-Rust%201.85%2B-282A2E?style=flat-square&logo=rust&logoColor=white" alt="Rust Core" />
  <img src="https://img.shields.io/badge/Crypto-Noise__NNpsk0-187345?style=flat-square" alt="Noise Encryption" />
  <img src="https://img.shields.io/badge/Topology-Full%20Mesh%20P2P-2563EB?style=flat-square" alt="Full Mesh P2P" />
</p>

<p>
  <a href="README.md">简体中文</a> · <strong>English</strong>
</p>

<p>
  <a href="https://github.com/Lokilankaaa/crossflow/releases/latest"><strong>⬇️ Download Latest</strong></a> ·
  <a href="#key-features">Features</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#shortcuts">Shortcuts</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#build-from-source">Build</a>
</p>

<br />

<img src="assets/readme-showcase.svg" alt="Crossflow Quiet White Multi-Screen Full Mesh Showcase" width="100%" />

</div>

---

## Key Features

<table>
  <tr>
    <td width="50%" valign="top">
      <h3>🌐 N-Device Full Mesh P2P</h3>
      Zero-configuration LAN discovery via mDNS (<code>_crossflow._tcp.local.</code>) with no master/client hierarchy—connect 3+ Windows &amp; macOS machines seamlessly.<br/><br/>
      Automatic link prioritization (<strong>Direct Ethernet &gt; Wired LAN &gt; Wi-Fi &gt; ZeroTier</strong>) with TUN/proxy filtering, manual NIC binding, and <code>Noise_NNpsk0</code> end-to-end encryption (Curve25519 + ChaCha20-Poly1305 + BLAKE2s).
    </td>
    <td width="50%" valign="top">
      <h3>⚡ Global Screen Numbers &amp; &lt; 5 ms QuickJump</h3>
      Deterministic global screen numbering (<code>#1</code>–<code>#9</code>) synced live across both the visual drag-and-snap canvas and device sidebar.<br/><br/>
      Instant <strong>&lt; 5 ms</strong> screen switching without waiting for modifier release, including direct <code>#1</code>–<code>#9</code> jump and <strong>Prev/Next screen cycling</strong>. Record pure-custom prefixes, <code>F1–F24</code>, <code>CapsLock</code>, and <strong><code>Mouse4</code> / <code>Mouse5</code> / <code>MouseMid</code></strong> via low-level hooks—while keeping local Gamepad passthrough intact.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <h3>⌨️ 1000 Hz Input &amp; Full System Keys</h3>
      Windows Raw Input + LL-hook routing paired with macOS dual-layer <code>kCGHIDEventTap</code> + <code>kCGSessionEventTap</code> capture for jitter-free edge transitions.<br/><br/>
      True <strong>1:1 key passthrough</strong> (preserves native <code>Ctrl</code> / <code>Cmd</code> and macOS Spaces), full support for <code>Caps Lock</code>, <code>Fn/Globe</code>, Numpad, <code>F1–F24</code>, <code>PrtSc/ScrLk/Pause</code> (mapped to macOS <code>F13–F15</code>), <strong>bi-directional Volume &amp; Media keys</strong>, and Windows consumer launch keys (Calculator, Mail, Explorer, Browser Home).
    </td>
    <td width="50%" valign="top">
      <h3>🛡️ Optional Windows System Service Mode</h3>
      Built-in optional <code>CrossflowInputService</code> (<code>LocalSystem</code> via hardened local named pipe), installable and updatable with one click in Settings.<br/><br/>
      Seamlessly delivers cross-screen mouse and keyboard input across <strong>UAC Secure Desktops, elevated Administrator windows, and the Windows Lock Screen</strong>.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <h3>📋 Encrypted Clipboard &amp; Streaming File Transfer</h3>
      Dedicated encrypted auxiliary channel (TCP <code>24801</code>) fully decoupled from the real-time input stream (TCP <code>24800</code>), so multi-GB transfers never add cursor latency.<br/><br/>
      Syncs text (≤ 4 MiB) and PNG/screenshot images (≤ 16 MiB, auto-converting Windows <code>CF_DIB</code>) with BLAKE2s echo protection. Copy files or folders with <strong><code>Ctrl+C</code> / <code>Cmd+C</code></strong> or drop into the window for 32 KiB encrypted chunked transfer with atomic <code>.part</code> writes into <code>Downloads/Crossflow</code>.
    </td>
    <td width="50%" valign="top">
      <h3>🌙 Wake-on-Entry, IME Sync &amp; Quiet White UI</h3>
      Prevents idle system sleep while allowing display power-saving; moving the cursor into a dimmed peer wakes it immediately for lock-screen login.<br/><br/>
      Real-time bi-directional <strong>IME language &amp; Caps Lock synchronization</strong> (Windows IMM32/HKL ↔ macOS HIToolbox/IOKit), unified Quiet White native UI (WinUI 3 &amp; SwiftUI), resizable sidebar splitters, and 4-state tray/menu-bar status icons.
    </td>
  </tr>
</table>

---

## Quick Start

Download the latest binaries from [**GitHub Releases**](https://github.com/Lokilankaaa/crossflow/releases/latest):

| Platform | Package | Requirements |
| :--- | :--- | :--- |
| **Windows (x64)** | `Crossflow-vX.Y.Z-windows-x64.zip` | Windows 10 19041+ / Windows 11 · Self-contained WinUI 3 + .NET 10 runtime |
| **macOS (Apple Silicon)** | `Crossflow-vX.Y.Z-macos-arm64.dmg` / `.zip` | macOS 14.0+ (M1 / M2 / M3 / M4) · Drag `Crossflow.app` to Applications |
| **Checksums** | `SHA256SUMS.txt` | SHA-256 verification for all artifacts |

> [!TIP]
> **Windows Service Mode (Optional)**: Launch `Crossflow.Windows.exe` and click **Install System Service** in Settings to enable input across UAC prompts, elevated apps, and the Lock Screen.

> [!NOTE]
> **macOS First-Run Permissions**:
> 1. If Gatekeeper flags the unsigned/ad-hoc bundle, clear the quarantine attribute once:
>    ```sh
>    xattr -dr com.apple.quarantine /Applications/Crossflow.app
>    ```
> 2. Grant **Accessibility** and **Input Monitoring** to `Crossflow.app` in **System Settings → Privacy & Security**.

---

## Shortcuts

| Action | Default / Example | Notes |
| :--- | :--- | :--- |
| **QuickJump by Screen #** | Custom Prefix + `1`–`9` | Jumps directly to global screen `#1`–`#9` shown on the canvas |
| **Prev / Next Screen** | e.g. `Ctrl+Alt+Mouse5` / `Ctrl+Alt+Mouse4` | Supports arbitrary keyboard chords & mouse side/middle buttons |
| **Emergency Stop** | Windows: `Ctrl + Alt + Esc`<br/>macOS: `Control + Option + Esc` | Instantly releases remote control (**never hijacks bare `Esc`**) |

---

## Architecture

```mermaid
flowchart LR
    subgraph W1["#1 Windows 11 (WinUI 3 + Rust)"]
        W1Core["Rust Core & Optional UAC Service"]
    end

    subgraph M1["#2 macOS Apple Silicon (SwiftUI + Rust)"]
        M1Core["Rust Core (HID + Session EventTap)"]
    end

    subgraph W2["#3 Windows Workstation (WinUI 3 + Rust)"]
        W2Core["Rust Core & Hotkey Engine"]
    end

    W1Core <-->|"UDP 5353 mDNS · TCP 24800 Input (Noise)"| M1Core
    M1Core <-->|"TCP 24801 Clipboard / Files / IME (Noise)"| W2Core
    W1Core <-->|"Full Mesh Direct / LAN / ZeroTier"| W2Core
```

| Port | Protocol | Purpose |
| :--- | :--- | :--- |
| `5353` | UDP | mDNS local peer discovery (`_crossflow._tcp.local.`) |
| `24800` | TCP (`TCP_NODELAY`) | `Noise_NNpsk0` encrypted real-time mouse, keyboard & topology stream |
| `24801` | TCP | `Noise_NNpsk0` encrypted auxiliary stream (clipboard, files/folders, IME & Caps Lock) |

---

## Build from Source

<details>
<summary><strong>🪟 Windows (Rust 1.85+ &amp; .NET 10 SDK)</strong></summary>

```powershell
.\scripts\cargo-local.ps1 test --lib
.\scripts\build-windows-ui.ps1 -Configuration Release
```

</details>

<details>
<summary><strong>🍎 macOS (Rust 1.85+ &amp; Xcode Command Line Tools)</strong></summary>

```sh
cargo test --lib
cargo build --release --target aarch64-apple-darwin \
  --bin crossflow --bin crossflow-keychain -p crossflow -p crossflow-bridge
CROSSFLOW_MACOS_APP_PATH="$PWD/dist/cross-screen/Crossflow.app" \
CROSSFLOW_CORE_EXECUTABLE="$PWD/target/aarch64-apple-darwin/release/crossflow" \
CROSSFLOW_MACOS_KEYCHAIN_HELPER="$PWD/target/aarch64-apple-darwin/release/crossflow-keychain" \
CROSSFLOW_BRIDGE_LIBRARY="$PWD/target/aarch64-apple-darwin/release/libcrossflow_bridge.dylib" \
sh ./scripts/build-macos-ui.sh
```

</details>
