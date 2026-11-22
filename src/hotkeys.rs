//! Cross-platform customizable hotkey configuration and chord matcher.
//!
//! Operates purely on canonical USB HID Keyboard/Keypad usages (Page 0x07), so the
//! exact same config and matching logic works identically on Windows and macOS
//! without OS-level `RegisterHotKey` conflicts or hardcoded bare-Escape stops.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// Canonical hotkey binding using USB HID usage IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyBinding {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub key: u16,
}

/// Modifier-only prefix used with digits `1`–`9` (e.g. `"Ctrl+Alt"` -> `Ctrl+Alt+1..9`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModifierPrefix {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

impl ModifierPrefix {
    pub fn format_prefix(self) -> String {
        let mut parts: Vec<&'static str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.meta {
            parts.push("Meta");
        }
        parts.join("+")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleSharing,
    ToggleLockScreen,
    QuickJumpNext,
    QuickJumpHome,
    QuickJumpDevice(usize), // 0-based remote device index (legacy)
    QuickJumpScreen(usize), // 0-based global screen index (0..9 => Screen #1..#9)
    EmergencyStop,
}

/// User-configurable hotkey bindings persisted in `peer-config.json`.
/// Each field is an optional human-readable chord string such as `"Ctrl+Alt+S"`,
/// or `None` / empty string to disable that hotkey.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotkeyConfig {
    #[serde(default = "default_toggle_sharing")]
    pub toggle_sharing: Option<String>,
    #[serde(default = "default_lock_screen")]
    pub lock_screen: Option<String>,
    #[serde(default = "default_quick_jump_next")]
    pub quick_jump_next: Option<String>,
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
            .and_then(parse_modifier_prefix)
            .map(ModifierPrefix::format_prefix);
        Self {
            toggle_sharing: norm(&self.toggle_sharing),
            lock_screen: norm(&self.lock_screen),
            quick_jump_next: norm(&self.quick_jump_next),
            quick_jump_home: norm(&self.quick_jump_home),
            quick_jump_screen_prefix: prefix,
            quick_jump_1: norm(&self.quick_jump_1),
            quick_jump_2: norm(&self.quick_jump_2),
            emergency_stop: norm(&self.emergency_stop),
        }
    }

    pub fn compiled_screen_prefix(&self) -> Option<ModifierPrefix> {
        self.quick_jump_screen_prefix
            .as_deref()
            .and_then(parse_modifier_prefix)
    }

    pub fn compiled(&self) -> Vec<(HotkeyBinding, HotkeyAction)> {
        let mut out = Vec::new();
        let pairs: [(&Option<String>, HotkeyAction); 7] = [
            (&self.emergency_stop, HotkeyAction::EmergencyStop),
            (&self.toggle_sharing, HotkeyAction::ToggleSharing),
            (&self.lock_screen, HotkeyAction::ToggleLockScreen),
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

impl HotkeyBinding {
    pub fn format_chord(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".into());
        }
        if self.alt {
            parts.push("Alt".into());
        }
        if self.shift {
            parts.push("Shift".into());
        }
        if self.meta {
            parts.push("Meta".into());
        }
        if let Some(name) = hid_to_key_name(self.key) {
            parts.push(name);
        }
        parts.join("+")
    }
}

/// Parse a modifier-only prefix such as `"Ctrl+Alt"`, `"Ctrl+Shift"`, `"Alt+Shift"`, `"Ctrl+Option"`.
/// Requires at least one modifier and rejects non-modifier keys.
pub fn parse_modifier_prefix(raw: &str) -> Option<ModifierPrefix> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    for token in trimmed.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" | "opt" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" | "win" | "super" => meta = true,
            _ => return None,
        }
    }
    if !(ctrl || alt || shift || meta) {
        return None;
    }
    Some(ModifierPrefix {
        ctrl,
        alt,
        shift,
        meta,
    })
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

/// Parse a chord string such as `"Ctrl+Alt+S"`, `"Ctrl+Option+1"`, `"Cmd+Shift+Tab"`.
/// Bare single keys without modifiers are rejected unless they are F1–F20 or ScrollLock/Pause,
/// preventing accidental everyday key hijacking (such as bare Escape).
pub fn parse_chord(raw: &str) -> Option<HotkeyBinding> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut meta = false;
    let mut key: Option<u16> = None;

    for token in trimmed.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" | "option" | "opt" => alt = true,
            "shift" => shift = true,
            "meta" | "cmd" | "command" | "win" | "super" => meta = true,
            other => {
                if key.is_some() {
                    return None;
                }
                key = key_name_to_hid(other);
            }
        }
    }
    let usage = key?;
    let has_mod = ctrl || alt || shift || meta;
    let is_standalone_allowed = matches!(usage, 0x3A..=0x45 | 0x47 | 0x68..=0x6F); // F1..F20, ScrollLock
    if !has_mod && !is_standalone_allowed {
        return None;
    }
    Some(HotkeyBinding {
        ctrl,
        alt,
        shift,
        meta,
        key: usage,
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
    if (0x68..=0x6F).contains(&usage) {
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
            _ => return None,
        }
        .into(),
    )
}

#[derive(Debug)]
struct EngineInner {
    bindings: Vec<(HotkeyBinding, HotkeyAction)>,
    screen_prefix: Option<ModifierPrefix>,
    screen_count: usize,
    held_mods: BTreeSet<u16>,
    pending_actions: Vec<HotkeyAction>,
    suspended: bool,
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
        Self(Arc::new(Mutex::new(EngineInner {
            bindings: config.compiled(),
            screen_prefix: config.compiled_screen_prefix(),
            screen_count: 9,
            held_mods: BTreeSet::new(),
            pending_actions: Vec::new(),
            suspended: false,
        })))
    }

    pub fn update_config(&self, config: &HotkeyConfig) {
        if let Ok(mut inner) = self.0.lock() {
            inner.bindings = config.compiled();
            inner.screen_prefix = config.compiled_screen_prefix();
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
        }
    }

    pub fn reset_modifiers(&self) {
        if let Ok(mut inner) = self.0.lock() {
            inner.held_mods.clear();
        }
    }

    /// Observe a physical HID key transition. Returns `Some(action)` if a configured
    /// hotkey matched on key-down (also queueing it for `drain_actions`).
    pub fn observe_key(&self, usage: u16, down: bool) -> Option<HotkeyAction> {
        let Ok(mut inner) = self.0.lock() else {
            return None;
        };
        if (0xE0..=0xE7).contains(&usage) {
            if down {
                inner.held_mods.insert(usage);
            } else {
                inner.held_mods.remove(&usage);
            }
            return None;
        }
        if !down || inner.suspended {
            return None;
        }
        let ctrl = inner.held_mods.contains(&0xE0) || inner.held_mods.contains(&0xE4);
        let shift = inner.held_mods.contains(&0xE1) || inner.held_mods.contains(&0xE5);
        let alt = inner.held_mods.contains(&0xE2) || inner.held_mods.contains(&0xE6);
        let meta = inner.held_mods.contains(&0xE3) || inner.held_mods.contains(&0xE7);

        let screen_match = inner.screen_prefix.and_then(|p| {
            let idx = digit_1_to_9_index(usage)?;
            (idx < inner.screen_count
                && p.ctrl == ctrl
                && p.alt == alt
                && p.shift == shift
                && p.meta == meta)
                .then_some(HotkeyAction::QuickJumpScreen(idx))
        });

        let matched = screen_match.or_else(|| {
            inner.bindings.iter().find_map(|(b, act)| {
                (b.key == usage
                    && b.ctrl == ctrl
                    && b.alt == alt
                    && b.shift == shift
                    && b.meta == meta)
                    .then_some(*act)
            })
        });
        if let Some(act) = matched {
            inner.pending_actions.push(act);
        }
        matched
    }

    pub fn any_modifier_held(&self) -> bool {
        self.0
            .lock()
            .map(|i| !i.held_mods.is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_chords_and_rejects_bare_escape() {
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
    }

    #[test]
    fn explicit_null_clears_hotkey_binding_instead_of_restoring_default() {
        let json = r#"{
            "toggle_sharing": null,
            "lock_screen": null,
            "quick_jump_next": "Ctrl+Alt+Tab",
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
        assert_eq!(norm.quick_jump_screen_prefix, None);
        assert_eq!(norm.emergency_stop, None);
        assert_eq!(norm.compiled().len(), 1);
    }

    #[test]
    fn engine_matches_configured_chords_and_ignores_bare_escape() {
        let engine = SharedHotkeyEngine::default();
        engine.set_screen_count(4);
        // Bare Escape never triggers anything
        assert_eq!(engine.observe_key(0x29, true), None);
        assert_eq!(engine.observe_key(0x29, false), None);

        // Ctrl+Alt+S triggers ToggleSharing
        assert_eq!(engine.observe_key(0xE0, true), None);
        assert_eq!(engine.observe_key(0xE2, true), None);
        assert_eq!(
            engine.observe_key(0x16, true),
            Some(HotkeyAction::ToggleSharing)
        );
        assert_eq!(engine.observe_key(0x16, false), None);

        // Ctrl+Alt+Tab triggers QuickJumpNext
        assert_eq!(
            engine.observe_key(0x2B, true),
            Some(HotkeyAction::QuickJumpNext)
        );
        // Ctrl+Alt+1..4 triggers QuickJumpScreen(0..4), top-row and numpad!
        assert_eq!(
            engine.observe_key(0x1E, true),
            Some(HotkeyAction::QuickJumpScreen(0))
        );
        assert_eq!(
            engine.observe_key(0x5C, true), // Numpad 4
            Some(HotkeyAction::QuickJumpScreen(3))
        );
        // Ctrl+Alt+5 is ignored when only 4 screens exist (does not hijack app shortcuts!)
        assert_eq!(engine.observe_key(0x22, true), None);
        // Release modifiers
        assert_eq!(engine.observe_key(0xE2, false), None);
        assert_eq!(engine.observe_key(0xE0, false), None);
        assert!(!engine.any_modifier_held());
        assert_eq!(
            engine.drain_actions(),
            vec![
                HotkeyAction::ToggleSharing,
                HotkeyAction::QuickJumpNext,
                HotkeyAction::QuickJumpScreen(0),
                HotkeyAction::QuickJumpScreen(3),
            ]
        );
    }
}
