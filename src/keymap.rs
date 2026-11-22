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

pub const HID_LEFT_CTRL: u16 = 0xE0;
pub const HID_LEFT_GUI: u16 = 0xE3;
pub const HID_RIGHT_CTRL: u16 = 0xE4;
pub const HID_RIGHT_GUI: u16 = 0xE7;

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
