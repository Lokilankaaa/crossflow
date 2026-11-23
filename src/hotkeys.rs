//! Cross-platform customizable hotkey configuration and chord matcher.
//!
//! Operates on canonical USB HID Keyboard/Keypad usages (Page 0x07) plus canonical
//! auxiliary mouse button codes (`MouseMid`, `Mouse4`, `Mouse5`), so any combination
//! of modifiers, keyboard keys, and mouse side/middle buttons works identically on
//! Windows and macOS without OS-level `RegisterHotKey` conflicts or bare-Escape stops.

use crate::input::Button;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// Canonical virtual input codes for mouse buttons (outside USB HID Page 0x07 range).
pub const INPUT_MOUSE_MIDDLE: u16 = 0xF003;
pub const INPUT_MOUSE_BACK: u16 = 0xF004;
pub const INPUT_MOUSE_FORWARD: u16 = 0xF005;

/// Map an auxiliary mouse button to its canonical hotkey input code.
/// Primary Left/Right buttons are intentionally excluded so everyday clicking is never hijacked.
pub fn mouse_button_input_code(button: Button) -> Option<u16> {
    match button {
        Button::Middle => Some(INPUT_MOUSE_MIDDLE),
        Button::Back => Some(INPUT_MOUSE_BACK),
        Button::Forward => Some(INPUT_MOUSE_FORWARD),
        Button::Left | Button::Right => None,
    }
}

pub fn is_mouse_input_code(code: u16) -> bool {
    matches!(
        code,
        INPUT_MOUSE_MIDDLE | INPUT_MOUSE_BACK | INPUT_MOUSE_FORWARD
    )
}

/// Canonical hotkey binding supporting modifiers + one or more keyboard keys / mouse buttons
/// (e.g. `"Ctrl+Alt+S"`, `"Alt+Mouse4"`, `"Q+Mouse5"`, `"Mouse4"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub keys: BTreeSet<u16>,
}

/// Arbitrary custom prefix held together with digits `1`–`9` to QuickJump directly to
/// global screen `#1..#9` (e.g. `"Ctrl+Alt"`, `"Alt"`, `"Mouse4"`, `"Alt+Mouse4"`, `` "`" ``).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyPrefix {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub extra_keys: BTreeSet<u16>,
}

impl HotkeyPrefix {
    pub fn format_prefix(&self) -> String {
        format_parts(self.ctrl, self.alt, self.shift, self.meta, &self.extra_keys)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleSharing,
    ToggleLockScreen,
    QuickJumpNext,
    QuickJumpPrevScreen,
    QuickJumpNextScreen,
    QuickJumpHome,
    QuickJumpDevice(usize), // 0-based remote device index (legacy)
    QuickJumpScreen(usize), // 0-based global screen index (0..9 => Screen #1..#9)
    EmergencyStop,
}

/// User-configurable hotkey bindings persisted in `peer-config.json`.
/// Each field is an optional human-readable chord/prefix string such as `"Ctrl+Alt+S"`,
/// `"Alt+Mouse4"`, `"Q+Mouse5"`, or `None` / empty string to disable that hotkey.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyConfig {
    #[serde(default = "default_toggle_sharing")]
    pub toggle_sharing: Option<String>,
    #[serde(default = "default_lock_screen")]
    pub lock_screen: Option<String>,
    #[serde(default = "default_quick_jump_next")]
    pub quick_jump_next: Option<String>,
    #[serde(default)]
    pub quick_jump_prev_screen: Option<String>,
    #[serde(default)]
    pub quick_jump_next_screen: Option<String>,
    #[serde(default = "default_quick_jump_home")]
    pub quick_jump_home: Option<String>,
    #[serde(default = "default_quick_jump_screen_prefix")]
    pub quick_jump_screen_prefix: Option<String>,
    #[serde(default)]
    pub quick_jump_1: Option<String>,
    #[serde(default)]
    pub quick_jump_2: Option<String>,
    #[serde(default = "default_emergency_stop")]
    pub emergency_stop: Option<String>,
}

fn default_toggle_sharing() -> Option<String> {
    Some("Ctrl+Alt+S".into())
}
fn default_lock_screen() -> Option<String> {
    Some("Ctrl+Alt+L".into())
}
fn default_quick_jump_next() -> Option<String> {
    Some("Ctrl+Alt+Tab".into())
}
fn default_quick_jump_home() -> Option<String> {
    Some("Ctrl+Alt+H".into())
}
fn default_quick_jump_screen_prefix() -> Option<String> {
    Some("Ctrl+Alt".into())
}
fn default_emergency_stop() -> Option<String> {
    Some("Ctrl+Alt+Shift+Esc".into())
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            toggle_sharing: default_toggle_sharing(),
            lock_screen: default_lock_screen(),
            quick_jump_next: default_quick_jump_next(),
            quick_jump_prev_screen: None,
            quick_jump_next_screen: None,
            quick_jump_home: default_quick_jump_home(),
            quick_jump_screen_prefix: default_quick_jump_screen_prefix(),
            quick_jump_1: None,
            quick_jump_2: None,
            emergency_stop: default_emergency_stop(),
        }
    }
}

impl HotkeyConfig {
    pub fn normalized(&self) -> Self {
        let norm = |v: &Option<String>| {
            v.as_deref()
                .and_then(parse_chord)
                .map(|b| b.format_chord())
        };
        let prefix = self
            .quick_jump_screen_prefix
            .as_deref()
            .and_then(parse_screen_prefix)
            .map(|p| p.format_prefix());
        Self {
            toggle_sharing: norm(&self.toggle_sharing),
            lock_screen: norm(&self.lock_screen),
            quick_jump_next: norm(&self.quick_jump_next),
            quick_jump_prev_screen: norm(&self.quick_jump_prev_screen),
            quick_jump_next_screen: norm(&self.quick_jump_next_screen),
            quick_jump_home: norm(&self.quick_jump_home),
            quick_jump_screen_prefix: prefix,
            quick_jump_1: norm(&self.quick_jump_1),
            quick_jump_2: norm(&self.quick_jump_2),
            emergency_stop: norm(&self.emergency_stop),
        }
    }

    pub fn compiled_screen_prefix(&self) -> Option<HotkeyPrefix> {
        self.quick_jump_screen_prefix
            .as_deref()
            .and_then(parse_screen_prefix)
    }

    pub fn compiled(&self) -> Vec<(HotkeyBinding, HotkeyAction)> {
        let mut out = Vec::new();
        let pairs: [(&Option<String>, HotkeyAction); 9] = [
            (&self.emergency_stop, HotkeyAction::EmergencyStop),
            (&self.toggle_sharing, HotkeyAction::ToggleSharing),
            (&self.lock_screen, HotkeyAction::ToggleLockScreen),
            (&self.quick_jump_prev_screen, HotkeyAction::QuickJumpPrevScreen),
            (&self.quick_jump_next_screen, HotkeyAction::QuickJumpNextScreen),
            (&self.quick_jump_next, HotkeyAction::QuickJumpNext),
            (&self.quick_jump_home, HotkeyAction::QuickJumpHome),
            (&self.quick_jump_1, HotkeyAction::QuickJumpDevice(0)),
            (&self.quick_jump_2, HotkeyAction::QuickJumpDevice(1)),
        ];
        for (spec, action) in pairs {
            if let Some(binding) = spec.as_deref().and_then(parse_chord) {
                out.push((binding, action));
            }
        }
        out
    }
}

fn format_parts(ctrl: bool, alt: bool, shift: bool, meta: bool, keys: &BTreeSet<u16>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if ctrl {
        parts.push("Ctrl".into());
    }
    if alt {
        parts.push("Alt".into());
    }
    if shift {
        parts.push("Shift".into());
    }
    if meta {
        parts.push("Meta".into());
    }
    // Keyboard keys first, mouse buttons last for clean readability (e.g. "Q+Mouse4", "Alt+Mouse5")
    for &code in keys.iter().filter(|&&c| !is_mouse_input_code(c)) {
        if let Some(name) = hid_to_key_name(code) {
            parts.push(name);
        }
    }
    for &code in keys.iter().filter(|&&c| is_mouse_input_code(c)) {
        if let Some(name) = hid_to_key_name(code) {
            parts.push(name);
        }
    }
    parts.join("+")
}

impl HotkeyBinding {
    pub fn format_chord(&self) -> String {
        format_parts(self.ctrl, self.alt, self.shift, self.meta, &self.keys)
    }
}

/// Map top-row `1`..`9` (`0x1E..=0x26`) and Numpad `1`..`9` (`0x59..=0x61`) to 0-based `0..9`.
fn digit_1_to_9_index(usage: u16) -> Option<usize> {
    if (0x1E..=0x26).contains(&usage) {
        Some((usage - 0x1E) as usize)
    } else if (0x59..=0x61).contains(&usage) {
        Some((usage - 0x59) as usize)
    } else {
        None
    }
}

/// Parse an arbitrary custom prefix for QuickJump Screen 1–9.
/// Accepts any combination of modifiers (`Ctrl`, `Alt`/`Option`, `Shift`, `Meta`/`Win`/`Cmd`),
/// keyboard keys (`CapsLock`, `` ` ``, `Tab`, `Space`, `F13`, `Q`, etc.), and/or mouse buttons
/// (`Mouse4`, `Mouse5`, `MouseMid`). Rejects empty prefixes, `Esc`, and digits `1–9`.
pub fn parse_screen_prefix(raw: &str) -> Option<HotkeyPrefix> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut extra_keys = BTreeSet::new();

    for token in trimmed.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" | "opt" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" | "win" | "super" => meta = true,
            other => {
                let code = key_name_to_hid(other)?;
                if code == 0x29 || digit_1_to_9_index(code).is_some() {
                    return None;
                }
                extra_keys.insert(code);
            }
        }
    }
    if !(ctrl || alt || shift || meta) && extra_keys.is_empty() {
        return None;
    }
    if extra_keys.len() > 3 {
        return None;
    }
    Some(HotkeyPrefix {
        ctrl,
        alt,
        shift,
        meta,
        extra_keys,
    })
}

/// Parse a chord string such as `"Ctrl+Alt+S"`, `"Alt+Mouse4"`, `"Q+Mouse5"`, `"Mouse4"`, `"F9"`.
/// Supports:
/// - Modifier(s) + 1 or more keyboard keys / mouse buttons
/// - 2+ simultaneous keyboard keys / mouse buttons (e.g. `"Q+Mouse4"`, `"Space+Mouse5"`)
/// - Standalone `F1–F20`, `ScrollLock`, or auxiliary mouse buttons (`Mouse4`, `Mouse5`, `MouseMid`)
/// Bare everyday single keys (such as bare `Esc`, `A`, `1`, `Space`) are rejected.
pub fn parse_chord(raw: &str) -> Option<HotkeyBinding> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut keys = BTreeSet::new();

    for token in trimmed.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" | "opt" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" | "win" | "super" => meta = true,
            other => {
                let code = key_name_to_hid(other)?;
                keys.insert(code);
            }
        }
    }
    if keys.is_empty() || keys.len() > 4 {
        return None;
    }
    let has_mod = ctrl || alt || shift || meta;
    if !has_mod && keys.len() == 1 {
        let &only = keys.iter().next()?;
        let is_standalone_allowed =
            matches!(only, 0x3A..=0x45 | 0x47 | 0x68..=0x73) || is_mouse_input_code(only);
        if !is_standalone_allowed {
            return None;
        }
    }
    Some(HotkeyBinding {
        ctrl,
        alt,
        shift,
        meta,
        keys,
    })
}

pub fn key_name_to_hid(name: &str) -> Option<u16> {
    let lower = name.to_ascii_lowercase();
    if lower.len() == 1 {
        let b = lower.as_bytes()[0];
        if b.is_ascii_lowercase() {
            return Some(0x04 + (b - b'a') as u16);
        }
        if (b'1'..=b'9').contains(&b) {
            return Some(0x1E + (b - b'1') as u16);
        }
        if b == b'0' {
            return Some(0x27);
        }
    }
    Some(match lower.as_str() {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" | "back" => 0x2A,
        "tab" => 0x2B,
        "space" => 0x2C,
        "-" | "minus" => 0x2D,
        "=" | "equal" => 0x2E,
        "[" | "lbracket" => 0x2F,
        "]" | "rbracket" => 0x30,
        "\\" | "backslash" => 0x31,
        ";" | "semicolon" => 0x33,
        "'" | "quote" => 0x34,
        "`" | "grave" | "tilde" => 0x35,
        "," | "comma" => 0x36,
        "." | "period" => 0x37,
        "/" | "slash" => 0x38,
        "capslock" | "caps" => crate::keymap::HID_CAPS_LOCK,
        "f1" => 0x3A,
        "f2" => 0x3B,
        "f3" => 0x3C,
        "f4" => 0x3D,
        "f5" => 0x3E,
        "f6" => 0x3F,
        "f7" => 0x40,
        "f8" => 0x41,
        "f9" => 0x42,
        "f10" => 0x43,
        "f11" => 0x44,
        "f12" => 0x45,
        "scrolllock" => 0x47,
        "insert" => 0x49,
        "home" => 0x4A,
        "pageup" | "pgup" => 0x4B,
        "delete" | "del" => 0x4C,
        "end" => 0x4D,
        "pagedown" | "pgdn" => 0x4E,
        "right" => 0x4F,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        "f13" => 0x68,
        "f14" => 0x69,
        "f15" => 0x6A,
        "f16" => 0x6B,
        "f17" => 0x6C,
        "f18" => 0x6D,
        "f19" => 0x6E,
        "f20" => 0x6F,
        "f21" => 0x70,
        "f22" => 0x71,
        "f23" => 0x72,
        "f24" => 0x73,
        "mousemid" | "mousemiddle" | "middle" | "mouse3" | "mmb" => INPUT_MOUSE_MIDDLE,
        "mouse4" | "mouseback" | "xbutton1" | "mb4" => INPUT_MOUSE_BACK,
        "mouse5" | "mouseforward" | "xbutton2" | "mb5" => INPUT_MOUSE_FORWARD,
        _ => return None,
    })
}

pub fn hid_to_key_name(usage: u16) -> Option<String> {
    if (0x04..=0x1D).contains(&usage) {
        let ch = (b'A' + (usage - 0x04) as u8) as char;
        return Some(ch.to_string());
    }
    if (0x1E..=0x26).contains(&usage) {
        let ch = (b'1' + (usage - 0x1E) as u8) as char;
        return Some(ch.to_string());
    }
    if usage == 0x27 {
        return Some("0".into());
    }
    if (0x3A..=0x45).contains(&usage) {
        return Some(format!("F{}", usage - 0x3A + 1));
    }
    if (0x68..=0x73).contains(&usage) {
        return Some(format!("F{}", usage - 0x68 + 13));
    }
    Some(
        match usage {
            0x28 => "Enter",
            0x29 => "Esc",
            0x2A => "Backspace",
            0x2B => "Tab",
            0x2C => "Space",
            0x2D => "-",
            0x2E => "=",
            0x2F => "[",
            0x30 => "]",
            0x31 => "\\",
            0x33 => ";",
            0x34 => "'",
            0x35 => "`",
            0x36 => ",",
            0x37 => ".",
            0x38 => "/",
            0x39 => "CapsLock",
            0x47 => "ScrollLock",
            0x49 => "Insert",
            0x4A => "Home",
            0x4B => "PageUp",
            0x4C => "Delete",
            0x4D => "End",
            0x4E => "PageDown",
            0x4F => "Right",
            0x50 => "Left",
            0x51 => "Down",
            0x52 => "Up",
            INPUT_MOUSE_MIDDLE => "MouseMid",
            INPUT_MOUSE_BACK => "Mouse4",
            INPUT_MOUSE_FORWARD => "Mouse5",
            _ => return None,
        }
        .into(),
    )
}

#[derive(Debug)]
struct EngineInner {
    bindings: Vec<(HotkeyBinding, HotkeyAction)>,
    screen_prefix: Option<HotkeyPrefix>,
    tracked_inputs: BTreeSet<u16>,
    screen_count: usize,
    held_mods: BTreeSet<u16>,
    held_inputs: BTreeSet<u16>,
    pending_actions: Vec<HotkeyAction>,
    suspended: bool,
}

fn compute_tracked_inputs(
    bindings: &[(HotkeyBinding, HotkeyAction)],
    screen_prefix: &Option<HotkeyPrefix>,
) -> BTreeSet<u16> {
    let mut tracked = BTreeSet::new();
    if let Some(p) = screen_prefix {
        tracked.extend(p.extra_keys.iter().copied());
    }
    for (b, _) in bindings {
        if b.keys.len() > 1 {
            tracked.extend(b.keys.iter().copied());
        }
    }
    tracked
}

/// Thread-safe global hotkey matcher shared between native capture and PeerHost.
#[derive(Clone, Debug)]
pub struct SharedHotkeyEngine(Arc<Mutex<EngineInner>>);

impl Default for SharedHotkeyEngine {
    fn default() -> Self {
        Self::new(&HotkeyConfig::default())
    }
}

impl SharedHotkeyEngine {
    pub fn new(config: &HotkeyConfig) -> Self {
        let bindings = config.compiled();
        let screen_prefix = config.compiled_screen_prefix();
        let tracked_inputs = compute_tracked_inputs(&bindings, &screen_prefix);
        Self(Arc::new(Mutex::new(EngineInner {
            bindings,
            screen_prefix,
            tracked_inputs,
            screen_count: 9,
            held_mods: BTreeSet::new(),
            held_inputs: BTreeSet::new(),
            pending_actions: Vec::new(),
            suspended: false,
        })))
    }

    pub fn update_config(&self, config: &HotkeyConfig) {
        if let Ok(mut inner) = self.0.lock() {
            let bindings = config.compiled();
            let screen_prefix = config.compiled_screen_prefix();
            let tracked = compute_tracked_inputs(&bindings, &screen_prefix);
            inner.bindings = bindings;
            inner.screen_prefix = screen_prefix;
            inner.held_inputs.retain(|k| tracked.contains(k));
            inner.tracked_inputs = tracked;
        }
    }

    /// Update the active number of numbered screens (`0..=9`) so digits beyond
    /// the current screen count are never intercepted from other applications.
    pub fn set_screen_count(&self, count: usize) {
        if let Ok(mut inner) = self.0.lock() {
            inner.screen_count = count.min(9);
        }
    }

    pub fn set_suspended(&self, suspended: bool) {
        if let Ok(mut inner) = self.0.lock() {
            inner.suspended = suspended;
            if suspended {
                inner.held_mods.clear();
                inner.held_inputs.clear();
            }
        }
    }

    pub fn reset_modifiers(&self) {
        if let Ok(mut inner) = self.0.lock() {
            inner.held_mods.clear();
            inner.held_inputs.clear();
        }
    }

    /// Reconcile modifier state against live OS hardware state so a swallowed
    /// modifier key-up (e.g. UAC / Win+L / Alt+Tab) can never jam hotkeys.
    pub fn sync_modifiers(&self, ctrl: bool, shift: bool, alt: bool, meta: bool) {
        if let Ok(mut inner) = self.0.lock() {
            if !ctrl {
                inner.held_mods.remove(&0xE0);
                inner.held_mods.remove(&0xE4);
            } else if !inner.held_mods.contains(&0xE0) && !inner.held_mods.contains(&0xE4) {
                inner.held_mods.insert(0xE0);
            }
            if !shift {
                inner.held_mods.remove(&0xE1);
                inner.held_mods.remove(&0xE5);
            } else if !inner.held_mods.contains(&0xE1) && !inner.held_mods.contains(&0xE5) {
                inner.held_mods.insert(0xE1);
            }
            if !alt {
                inner.held_mods.remove(&0xE2);
                inner.held_mods.remove(&0xE6);
            } else if !inner.held_mods.contains(&0xE2) && !inner.held_mods.contains(&0xE6) {
                inner.held_mods.insert(0xE2);
            }
            if !meta {
                inner.held_mods.remove(&0xE3);
                inner.held_mods.remove(&0xE7);
            } else if !inner.held_mods.contains(&0xE3) && !inner.held_mods.contains(&0xE7) {
                inner.held_mods.insert(0xE3);
            }
        }
    }

    /// Observe a physical HID keyboard key OR canonical mouse button transition.
    /// Returns `Some(action)` if a configured hotkey matched on press (and queues it for `drain_actions`).
    pub fn observe_input(&self, code: u16, down: bool) -> Option<HotkeyAction> {
        let Ok(mut inner) = self.0.lock() else {
            return None;
        };
        if (0xE0..=0xE7).contains(&code) {
            if down {
                inner.held_mods.insert(code);
            } else {
                inner.held_mods.remove(&code);
            }
            return None;
        }
        if !down {
            inner.held_inputs.remove(&code);
            return None;
        }
        if inner.suspended {
            return None;
        }

        let ctrl = inner.held_mods.contains(&0xE0) || inner.held_mods.contains(&0xE4);
        let shift = inner.held_mods.contains(&0xE1) || inner.held_mods.contains(&0xE5);
        let alt = inner.held_mods.contains(&0xE2) || inner.held_mods.contains(&0xE6);
        let meta = inner.held_mods.contains(&0xE3) || inner.held_mods.contains(&0xE7);

        let screen_match = inner.screen_prefix.as_ref().and_then(|p| {
            let idx = digit_1_to_9_index(code)?;
            (idx < inner.screen_count
                && p.ctrl == ctrl
                && p.alt == alt
                && p.shift == shift
                && p.meta == meta
                && inner.held_inputs == p.extra_keys)
                .then_some(HotkeyAction::QuickJumpScreen(idx))
        });

        let matched = screen_match.or_else(|| {
            let mut candidate = inner.held_inputs.clone();
            candidate.insert(code);
            inner.bindings.iter().find_map(|(b, act)| {
                (b.ctrl == ctrl
                    && b.alt == alt
                    && b.shift == shift
                    && b.meta == meta
                    && b.keys == candidate)
                    .then_some(*act)
            })
        });

        if let Some(act) = matched {
            inner.pending_actions.push(act);
            // Trigger input will be suppressed by the capture hook (both down and up),
            // so do NOT leave it in `held_inputs`.
            return Some(act);
        }

        // Only track non-modifier keys/buttons that are actually used in a configured
        // multi-key chord or custom screen prefix, so random everyday typing can never
        // leave a stale key in `held_inputs`.
        if inner.tracked_inputs.contains(&code) {
            inner.held_inputs.insert(code);
        }
        None
    }

    pub fn observe_key(&self, usage: u16, down: bool) -> Option<HotkeyAction> {
        self.observe_input(usage, down)
    }

    pub fn observe_button(&self, button: Button, down: bool) -> Option<HotkeyAction> {
        let code = mouse_button_input_code(button)?;
        self.observe_input(code, down)
    }

    pub fn any_modifier_held(&self) -> bool {
        self.0
            .lock()
            .map(|i| !i.held_mods.is_empty() || !i.held_inputs.is_empty())
            .unwrap_or(false)
    }

    pub fn drain_actions(&self) -> Vec<HotkeyAction> {
        self.0
            .lock()
            .map(|mut i| std::mem::take(&mut i.pending_actions))
            .unwrap_or_default()
    }
}

static ACTIVE_ENGINE: Mutex<Option<SharedHotkeyEngine>> = Mutex::new(None);

/// Install the active hotkey engine for native capture hooks to consult.
pub fn install_active_engine(engine: Option<SharedHotkeyEngine>) {
    if let Ok(mut guard) = ACTIVE_ENGINE.lock() {
        *guard = engine;
    }
}

/// Returns true when a custom hotkey engine is active (peer mesh runtime).
pub fn has_active_engine() -> bool {
    ACTIVE_ENGINE
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

/// Observe a physical key event against the currently installed hotkey engine (if any).
pub fn active_observe_key(usage: u16, down: bool) -> Option<HotkeyAction> {
    let engine = ACTIVE_ENGINE.lock().ok()?.clone()?;
    engine.observe_key(usage, down)
}

/// Observe a physical mouse button event against the currently installed hotkey engine (if any).
pub fn active_observe_button(button: Button, down: bool) -> Option<HotkeyAction> {
    let engine = ACTIVE_ENGINE.lock().ok()?.clone()?;
    engine.observe_button(button, down)
}

/// Reconcile active hotkey engine modifiers against live OS key state.
pub fn active_sync_modifiers(ctrl: bool, shift: bool, alt: bool, meta: bool) {
    if let Ok(guard) = ACTIVE_ENGINE.lock()
        && let Some(engine) = guard.as_ref()
    {
        engine.sync_modifiers(ctrl, shift, alt, meta);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_chords_mouse_buttons_and_custom_prefixes() {
        assert_eq!(parse_chord("Esc"), None);
        assert_eq!(parse_chord("A"), None);
        assert_eq!(
            parse_chord("ctrl+option+s").map(|b| b.format_chord()),
            Some("Ctrl+Alt+S".into())
        );
        assert_eq!(
            parse_chord("Ctrl+Alt+Shift+Escape").map(|b| b.format_chord()),
            Some("Ctrl+Alt+Shift+Esc".into())
        );
        assert_eq!(
            parse_chord("F9").map(|b| b.format_chord()),
            Some("F9".into())
        );
        // Keyboard key / modifier + mouse side button
        assert_eq!(
            parse_chord("Alt+Mouse4").map(|b| b.format_chord()),
            Some("Alt+Mouse4".into())
        );
        assert_eq!(
            parse_chord("q+xbutton2").map(|b| b.format_chord()),
            Some("Q+Mouse5".into())
        );
        assert_eq!(
            parse_chord("Mouse4").map(|b| b.format_chord()),
            Some("Mouse4".into())
        );
        // Custom arbitrary prefixes for 1-9 screen jump
        assert_eq!(
            parse_screen_prefix("Mouse4").map(|p| p.format_prefix()),
            Some("Mouse4".into())
        );
        assert_eq!(
            parse_screen_prefix("alt+mouse5").map(|p| p.format_prefix()),
            Some("Alt+Mouse5".into())
        );
        assert_eq!(
            parse_screen_prefix("`").map(|p| p.format_prefix()),
            Some("`".into())
        );
        assert_eq!(
            parse_screen_prefix("CapsLock").map(|p| p.format_prefix()),
            Some("CapsLock".into())
        );
        assert_eq!(parse_screen_prefix("1"), None);
        assert_eq!(parse_screen_prefix("Esc"), None);
    }

    #[test]
    fn explicit_null_clears_hotkey_binding_instead_of_restoring_default() {
        let json = r#"{
            "toggle_sharing": null,
            "lock_screen": null,
            "quick_jump_next": "Ctrl+Alt+Tab",
            "quick_jump_prev_screen": "Alt+Mouse4",
            "quick_jump_next_screen": "Q+Mouse5",
            "quick_jump_home": null,
            "quick_jump_screen_prefix": null,
            "quick_jump_1": null,
            "quick_jump_2": null,
            "emergency_stop": null
        }"#;
        let cfg: HotkeyConfig = serde_json::from_str(json).unwrap();
        let norm = cfg.normalized();
        assert_eq!(norm.toggle_sharing, None);
        assert_eq!(norm.lock_screen, None);
        assert_eq!(norm.quick_jump_next.as_deref(), Some("Ctrl+Alt+Tab"));
        assert_eq!(norm.quick_jump_prev_screen.as_deref(), Some("Alt+Mouse4"));
        assert_eq!(norm.quick_jump_next_screen.as_deref(), Some("Q+Mouse5"));
        assert_eq!(norm.quick_jump_screen_prefix, None);
        assert_eq!(norm.emergency_stop, None);
        assert_eq!(norm.compiled().len(), 3);
    }

    #[test]
    fn engine_matches_keyboard_plus_mouse_side_buttons_and_custom_prefix() {
        let cfg = HotkeyConfig {
            quick_jump_prev_screen: Some("Alt+Mouse4".into()),
            quick_jump_next_screen: Some("Q+Mouse5".into()),
            quick_jump_screen_prefix: Some("Mouse4".into()),
            ..HotkeyConfig::default()
        };
        let engine = SharedHotkeyEngine::new(&cfg);
        engine.set_screen_count(4);

        // Bare Escape never triggers anything
        assert_eq!(engine.observe_key(0x29, true), None);
        assert_eq!(engine.observe_key(0x29, false), None);

        // 1. Alt + Mouse4 -> QuickJumpPrevScreen
        assert_eq!(engine.observe_key(0xE2, true), None);
        assert_eq!(
            engine.observe_button(Button::Back, true),
            Some(HotkeyAction::QuickJumpPrevScreen)
        );
        assert_eq!(engine.observe_key(0xE2, false), None);

        // 2. Regular keyboard key Q (0x14) + Mouse5 -> QuickJumpNextScreen
        assert_eq!(engine.observe_key(0x14, true), None);
        assert_eq!(
            engine.observe_button(Button::Forward, true),
            Some(HotkeyAction::QuickJumpNextScreen)
        );
        assert_eq!(engine.observe_key(0x14, false), None);

        // 3. Custom Mouse4 prefix + digit 3 (0x20) -> QuickJumpScreen(2)
        assert_eq!(engine.observe_button(Button::Back, true), None);
        assert_eq!(
            engine.observe_key(0x20, true),
            Some(HotkeyAction::QuickJumpScreen(2))
        );
        // Digit 5 ignored when only 4 screens exist
        assert_eq!(engine.observe_key(0x22, true), None);
        assert_eq!(engine.observe_key(0x22, false), None);
        assert_eq!(engine.observe_button(Button::Back, false), None);

        assert!(!engine.any_modifier_held());
        assert_eq!(
            engine.drain_actions(),
            vec![
                HotkeyAction::QuickJumpPrevScreen,
                HotkeyAction::QuickJumpNextScreen,
                HotkeyAction::QuickJumpScreen(2),
            ]
        );
    }
}
