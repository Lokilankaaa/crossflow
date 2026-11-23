//! Automatic cross-OS keyboard modifier translation (Windows <-> macOS).
//!
//! Operates on canonical USB HID Keyboard/Keypad Page (0x07) usage IDs:
//! - 0xE0: Left Control   <-> 0xE3: Left GUI (Cmd / Win)
//! - 0xE4: Right Control  <-> 0xE7: Right GUI (Right Cmd / Win)
//!
//! Tracks each physically pressed key's mapped output usage so releasing a key
//! always releases the exact usage that was pressed, even if the user toggles
//! translation mid-chord.

use std::collections::HashMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub const HID_CAPS_LOCK: u16 = 0x39;
pub const HID_MUTE: u16 = 0x7F;
pub const HID_VOLUME_UP: u16 = 0x80;
pub const HID_VOLUME_DOWN: u16 = 0x81;
pub const HID_MEDIA_PLAY_PAUSE: u16 = 0x9B;
pub const HID_MEDIA_NEXT: u16 = 0x9C;
pub const HID_MEDIA_PREV: u16 = 0x9D;
pub const HID_MEDIA_STOP: u16 = 0x9E;
pub const HID_LAUNCH_MAIL: u16 = 0x9F;
pub const HID_LAUNCH_MEDIA: u16 = 0xA0;
pub const HID_LAUNCH_APP1: u16 = 0xA1;
pub const HID_CALCULATOR: u16 = 0xA2;
pub const HID_BROWSER_HOME: u16 = 0xA3;
/// Canonical usage ID used by Crossflow for the Apple Fn / Globe modifier key
/// (fits inside `input::is_keyboard_usage` without colliding with standard keys).
pub const HID_FN_GLOBE: u16 = 0xA4;
pub const HID_LEFT_CTRL: u16 = 0xE0;
pub const HID_LEFT_GUI: u16 = 0xE3;
pub const HID_RIGHT_CTRL: u16 = 0xE4;
pub const HID_RIGHT_GUI: u16 = 0xE7;

#[inline]
pub fn is_media_usage(usage: u16) -> bool {
    matches!(
        usage,
        HID_MUTE
            | HID_VOLUME_UP
            | HID_VOLUME_DOWN
            | HID_MEDIA_PLAY_PAUSE
            | HID_MEDIA_NEXT
            | HID_MEDIA_PREV
            | HID_MEDIA_STOP
            | HID_LAUNCH_MAIL
            | HID_LAUNCH_MEDIA
            | HID_LAUNCH_APP1
            | HID_CALCULATOR
            | HID_BROWSER_HOME
    )
}

#[derive(Clone, Debug)]
pub struct KeyTranslator {
    enabled: Arc<AtomicBool>,
    different_os: bool,
    active_down: HashMap<u16, u16>,
}

impl Default for KeyTranslator {
    fn default() -> Self {
        Self::passthrough()
    }
}

impl KeyTranslator {
    pub fn passthrough() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            different_os: false,
            active_down: HashMap::new(),
        }
    }

    pub fn new(enabled: Arc<AtomicBool>, different_os: bool) -> Self {
        Self {
            enabled,
            different_os,
            active_down: HashMap::new(),
        }
    }

    pub fn translate(&mut self, physical_usage: u16, down: bool) -> u16 {
        if down {
            let mapped = if self.different_os && self.enabled.load(Ordering::Relaxed) {
                swap_ctrl_cmd(physical_usage)
            } else {
                physical_usage
            };
            self.active_down.insert(physical_usage, mapped);
            mapped
        } else if let Some(mapped) = self.active_down.remove(&physical_usage) {
            mapped
        } else if self.different_os && self.enabled.load(Ordering::Relaxed) {
            swap_ctrl_cmd(physical_usage)
        } else {
            physical_usage
        }
    }

    pub fn reset(&mut self) {
        self.active_down.clear();
    }
}

#[inline]
fn swap_ctrl_cmd(usage: u16) -> u16 {
    match usage {
        HID_LEFT_CTRL => HID_LEFT_GUI,
        HID_LEFT_GUI => HID_LEFT_CTRL,
        HID_RIGHT_CTRL => HID_RIGHT_GUI,
        HID_RIGHT_GUI => HID_RIGHT_CTRL,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_ctrl_and_cmd_only_across_different_operating_systems() {
        let flag = Arc::new(AtomicBool::new(true));
        let mut cross = KeyTranslator::new(flag.clone(), true);
        assert_eq!(cross.translate(HID_LEFT_CTRL, true), HID_LEFT_GUI);
        assert_eq!(cross.translate(0x06, true), 0x06); // 'C' unchanged
        assert_eq!(cross.translate(0x06, false), 0x06);
        assert_eq!(cross.translate(HID_LEFT_CTRL, false), HID_LEFT_GUI);

        assert_eq!(cross.translate(HID_RIGHT_GUI, true), HID_RIGHT_CTRL);
        assert_eq!(cross.translate(HID_RIGHT_GUI, false), HID_RIGHT_CTRL);

        let mut same = KeyTranslator::new(flag, false);
        assert_eq!(same.translate(HID_LEFT_CTRL, true), HID_LEFT_CTRL);
        assert_eq!(same.translate(HID_LEFT_GUI, true), HID_LEFT_GUI);
    }

    #[test]
    fn toggling_mid_press_still_releases_the_exact_pressed_usage() {
        let flag = Arc::new(AtomicBool::new(true));
        let mut tr = KeyTranslator::new(flag.clone(), true);
        // Press Left Ctrl while enabled -> emits Left Cmd
        assert_eq!(tr.translate(HID_LEFT_CTRL, true), HID_LEFT_GUI);
        // User disables smart keymap while still holding Left Ctrl
        flag.store(false, Ordering::Relaxed);
        // Releasing Left Ctrl MUST still release Left Cmd (never stuck!)
        assert_eq!(tr.translate(HID_LEFT_CTRL, false), HID_LEFT_GUI);
        // Next press uses passthrough
        assert_eq!(tr.translate(HID_LEFT_CTRL, true), HID_LEFT_CTRL);
        assert_eq!(tr.translate(HID_LEFT_CTRL, false), HID_LEFT_CTRL);
    }
}
