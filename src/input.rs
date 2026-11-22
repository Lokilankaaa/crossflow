//! Ordered source-side input for one explicitly armed control epoch.
//!
//! This module does not inject input or release remote keys. Before applying
//! any pause/session transition (including a `Paused` result or `pop` error),
//! the integration must release/reset the receiver's remotely owned buttons
//! and keys. Clearing this queue alone cannot release input already delivered.
//! Remote ownership must be tracked separately from local physical held state.

use std::collections::{BTreeSet, VecDeque};

pub const INPUT_QUEUE_CAPACITY: usize = 256;
pub const INPUT_MAX_AGE_NS: u64 = 30_000_000;
const MAX_COORDINATE: f64 = (1u64 << 48) as f64;
const MAX_RELATIVE_MOVE: f64 = 20_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputAction {
    /// Absolute target-native desktop coordinates; does not rebase raw displacement.
    Position {
        x: f64,
        y: f64,
        display_id: u32,
    },
    /// Cumulative source displacement since the current arm, not screen pixels.
    Move {
        x: f64,
        y: f64,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// Raw wheel units: 120 is one conventional notch. Preserve sub-notch input.
    Scroll {
        horizontal: i32,
        vertical: i32,
    },
    /// Usage ID on the USB HID Keyboard/Keypad page (0x07), never a platform VK.
    Key {
        usage: u16,
        down: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueuedInput {
    pub action: InputAction,
    /// Earliest capture in a coalesced move; all times share one monotonic clock.
    pub captured_at_ns: u64,
    pub captures_coalesced: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    QueueOverflow,
    StaleInput,
    ClockRegressed,
    InvalidMovement,
    InvalidKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedError {
    Active,
    InvalidKey,
}

/// Keyboard/Keypad page usages defined by USB HID Usage Tables 1.7, pp. 93–95.
/// Excludes no-event/error codes and reserved ranges, never a platform keycode.
pub fn is_keyboard_usage(usage: u16) -> bool {
    matches!(usage, 0x04..=0xA4 | 0xB0..=0xDD | 0xE0..=0xE7)
}

/// Returns true for keys that legitimately auto-repeat when held down
/// (e.g., Backspace, Delete, Arrows, Letters, Digits, Punctuation, Space, Volume Up/Down).
/// Excludes modifiers, lock toggles, Fn/Globe, and discrete media toggles.
pub fn is_repeatable_key(usage: u16) -> bool {
    is_keyboard_usage(usage)
        && !(0xE0..=0xE7).contains(&usage)
        && !matches!(usage, 0x39 | 0x47 | 0x53 | 0xA4 | 0x7F | 0x9B | 0x9C | 0x9D)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureOutcome {
    Queued,
    Coalesced,
    Ignored,
    /// The queue is now paused and empty. The caller must release remote input.
    Paused(PauseReason),
}

/// A bounded, ordered queue. Buttons, keys and wheel events are movement
/// barriers; only adjacent movement events coalesce. Capture physical button
/// and key changes even while paused/disconnected, otherwise pre-held input
/// cannot be distinguished from a newly pressed control after arming. Call
/// `seed_physical_held` with a platform snapshot before the first arm and after
/// capture gaps; a successful initial seed is required to arm. The epoch sets
/// track accepted source downs, not remote delivery.
pub struct SourceInputQueue {
    connected: bool,
    active: bool,
    control_epoch: u64,
    queue: VecDeque<QueuedInput>,
    x: f64,
    y: f64,
    last_move_group_delta: (f64, f64),
    physical_state_seeded: bool,
    local_buttons: BTreeSet<Button>,
    local_keys: BTreeSet<u16>,
    epoch_buttons: BTreeSet<Button>,
    epoch_keys: BTreeSet<u16>,
    last_capture_ns: Option<u64>,
    last_poll_ns: Option<u64>,
}

impl Default for SourceInputQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceInputQueue {
    pub fn new() -> Self {
        Self {
            connected: false,
            active: false,
            control_epoch: 0,
            queue: VecDeque::with_capacity(INPUT_QUEUE_CAPACITY),
            x: 0.0,
            y: 0.0,
            last_move_group_delta: (0.0, 0.0),
            physical_state_seeded: false,
            local_buttons: BTreeSet::new(),
            local_keys: BTreeSet::new(),
            epoch_buttons: BTreeSet::new(),
            epoch_keys: BTreeSet::new(),
            last_capture_ns: None,
            last_poll_ns: None,
        }
    }

    /// A new authenticated session always starts paused. Local physical held
    /// state survives reconnection; a held key must be released and pressed anew.
    pub fn begin_session(&mut self) {
        self.pause();
        self.connected = true;
    }

    pub fn end_session(&mut self) {
        self.pause();
        self.connected = false;
    }

    /// A capture stop/restart invalidates the physical snapshot and session.
    /// The caller must release remote input, then seed a fresh snapshot and
    /// establish a session before another arm. A network reconnect alone does
    /// not require this reset when physical capture remained continuous.
    pub fn capture_reset(&mut self) {
        self.end_session();
        self.physical_state_seeded = false;
        self.local_buttons.clear();
        self.local_keys.clear();
    }

    /// Replace the local physical snapshot atomically while paused. Invalid
    /// snapshots leave prior state unchanged. The platform must keep capture
    /// running around this snapshot so presses cannot be missed before arming.
    pub fn seed_physical_held(
        &mut self,
        buttons: impl IntoIterator<Item = Button>,
        keys: impl IntoIterator<Item = u16>,
    ) -> Result<(), SeedError> {
        if self.active {
            return Err(SeedError::Active);
        }
        let keys: BTreeSet<_> = keys.into_iter().collect();
        if keys.iter().any(|&usage| !is_keyboard_usage(usage)) {
            return Err(SeedError::InvalidKey);
        }
        self.local_buttons = buttons.into_iter().collect();
        self.local_keys = keys;
        self.physical_state_seeded = true;
        Ok(())
    }

    /// Returns true only for a new explicit arm of a connected, paused session.
    pub fn arm(&mut self) -> bool {
        if !self.connected || self.active || !self.physical_state_seeded {
            return false;
        }
        self.pause();
        self.active = true;
        true
    }

    /// Discard queued input and accepted epoch downs, preserving physical held state.
    /// The caller must also release remote ownership; no release is injected here.
    pub fn pause(&mut self) {
        self.control_epoch = self.control_epoch.wrapping_add(1);
        self.active = false;
        self.queue.clear();
        self.epoch_buttons.clear();
        self.epoch_keys.clear();
        self.x = 0.0;
        self.y = 0.0;
        self.last_move_group_delta = (0.0, 0.0);
        self.last_capture_ns = None;
        self.last_poll_ns = None;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn has_held_local_input(&self) -> bool {
        !self.local_keys.is_empty() || !self.local_buttons.is_empty()
    }

    /// Changes even when a pause and re-arm occur between transport polls.
    /// A new Arm must reset/release remote state before this epoch's input.
    pub fn control_epoch(&self) -> u64 {
        self.control_epoch
    }

    pub fn front_action(&self) -> Option<InputAction> {
        self.queue.front().map(|event| event.action)
    }

    /// Validate before transport writes, even when rate limiting delays a pop.
    /// Use the same monotonic clock as capture, while holding the queue lock.
    pub fn validate_pending(&mut self, now_ns: u64) -> Result<(), PauseReason> {
        if self.active {
            self.check_poll(now_ns)
        } else {
            Ok(())
        }
    }

    pub fn pending_len(&self) -> usize {
        self.queue.len()
    }

    fn fail(&mut self, reason: PauseReason) -> CaptureOutcome {
        self.pause();
        CaptureOutcome::Paused(reason)
    }

    fn check_age(&mut self, now_ns: u64, clock_regressed: bool) -> Result<(), PauseReason> {
        let reason = if clock_regressed {
            Some(PauseReason::ClockRegressed)
        } else if self
            .queue
            .front()
            .is_some_and(|event| now_ns.saturating_sub(event.captured_at_ns) > INPUT_MAX_AGE_NS)
        {
            Some(PauseReason::StaleInput)
        } else {
            None
        };
        if let Some(reason) = reason {
            self.pause();
            return Err(reason);
        }
        Ok(())
    }

    fn check_capture(&mut self, captured_at_ns: u64) -> Result<(), PauseReason> {
        // A physical event can wait for this mutex while the sender polls.
        // Preserve its original capture time, comparing only capture order.
        let regressed = self
            .last_capture_ns
            .is_some_and(|last| captured_at_ns < last);
        self.check_age(captured_at_ns, regressed)?;
        self.last_capture_ns = Some(captured_at_ns);
        Ok(())
    }

    fn check_poll(&mut self, now_ns: u64) -> Result<(), PauseReason> {
        // Poll timestamps are sampled under the queue mutex. They must not
        // precede an earlier poll or a capture already observed by this queue.
        let regressed = self.last_poll_ns.is_some_and(|last| now_ns < last)
            || self.last_capture_ns.is_some_and(|last| now_ns < last);
        self.check_age(now_ns, regressed)?;
        self.last_poll_ns = Some(now_ns);
        Ok(())
    }

    fn enqueue(&mut self, action: InputAction, now_ns: u64) -> CaptureOutcome {
        if let Some(last) = self.queue.back_mut() {
            if matches!(
                (last.action, action),
                (InputAction::Move { .. }, InputAction::Move { .. })
            ) || matches!((last.action, action), (InputAction::Position {display_id:a,..}, InputAction::Position {display_id:b,..}) if a == b)
            {
                last.action = action;
                last.captures_coalesced = last.captures_coalesced.saturating_add(1);
                return CaptureOutcome::Coalesced;
            }
        }
        if self.queue.len() == INPUT_QUEUE_CAPACITY {
            return self.fail(PauseReason::QueueOverflow);
        }
        self.queue.push_back(QueuedInput {
            action,
            captured_at_ns: now_ns,
            captures_coalesced: 1,
        });
        CaptureOutcome::Queued
    }

    /// Source raw relative motion. `now_ns` is the original capture timestamp,
    /// which can precede mutex acquisition. Paused movement is not carried forward.
    pub fn move_by(&mut self, dx: f64, dy: f64, now_ns: u64) -> CaptureOutcome {
        if !self.active {
            return CaptureOutcome::Ignored;
        }
        if let Err(reason) = self.check_capture(now_ns) {
            return CaptureOutcome::Paused(reason);
        }
        let x = self.x + dx;
        let y = self.y + dy;
        let group_delta = if self
            .queue
            .back()
            .is_some_and(|event| matches!(event.action, InputAction::Move { .. }))
        {
            (
                self.last_move_group_delta.0 + dx,
                self.last_move_group_delta.1 + dy,
            )
        } else {
            (dx, dy)
        };
        if !dx.is_finite()
            || !dy.is_finite()
            || dx.abs() > MAX_RELATIVE_MOVE
            || dy.abs() > MAX_RELATIVE_MOVE
            || !x.is_finite()
            || !y.is_finite()
            || x.abs() > MAX_COORDINATE
            || y.abs() > MAX_COORDINATE
            || group_delta.0.abs() > MAX_RELATIVE_MOVE
            || group_delta.1.abs() > MAX_RELATIVE_MOVE
        {
            return self.fail(PauseReason::InvalidMovement);
        }
        if dx == 0.0 && dy == 0.0 {
            return CaptureOutcome::Ignored;
        }
        self.x = x;
        self.y = y;
        self.last_move_group_delta = group_delta;
        self.enqueue(InputAction::Move { x, y }, now_ns)
    }

    /// Ordered absolute positioning. Adjacent positions replace only positions,
    /// keeping the oldest timestamp so a sustained backlog cannot appear fresh.
    pub fn position(&mut self, x: f64, y: f64, display_id: u32, now_ns: u64) -> CaptureOutcome {
        if !self.active {
            return CaptureOutcome::Ignored;
        }
        if let Err(reason) = self.check_capture(now_ns) {
            return CaptureOutcome::Paused(reason);
        }
        if !x.is_finite() || !y.is_finite() || x.abs() > MAX_COORDINATE || y.abs() > MAX_COORDINATE
        {
            return self.fail(PauseReason::InvalidMovement);
        }
        self.enqueue(InputAction::Position { x, y, display_id }, now_ns)
    }

    pub fn button(&mut self, button: Button, down: bool, now_ns: u64) -> CaptureOutcome {
        let changed = if down {
            self.local_buttons.insert(button)
        } else {
            self.local_buttons.remove(&button)
        };
        if !self.active {
            return CaptureOutcome::Ignored;
        }
        if let Err(reason) = self.check_capture(now_ns) {
            return CaptureOutcome::Paused(reason);
        }
        if !changed {
            return CaptureOutcome::Ignored;
        }
        if down {
            self.epoch_buttons.insert(button);
        } else if !self.epoch_buttons.remove(&button) {
            return CaptureOutcome::Ignored;
        }
        self.enqueue(InputAction::Button { button, down }, now_ns)
    }

    pub fn key(&mut self, usage: u16, down: bool, now_ns: u64) -> CaptureOutcome {
        // Validate before touching physical state, including while paused.
        if !is_keyboard_usage(usage) {
            return if self.active {
                self.fail(PauseReason::InvalidKey)
            } else {
                CaptureOutcome::Ignored
            };
        }
        let changed = if down {
            self.local_keys.insert(usage)
        } else {
            self.local_keys.remove(&usage)
        };
        if !self.active {
            return CaptureOutcome::Ignored;
        }
        if let Err(reason) = self.check_capture(now_ns) {
            return CaptureOutcome::Paused(reason);
        }
        if !changed {
            if down && is_repeatable_key(usage) && self.epoch_keys.contains(&usage) {
                return self.enqueue(InputAction::Key { usage, down: true }, now_ns);
            }
            return CaptureOutcome::Ignored;
        }
        if down {
            self.epoch_keys.insert(usage);
        } else if !self.epoch_keys.remove(&usage) {
            return CaptureOutcome::Ignored;
        }
        self.enqueue(InputAction::Key { usage, down }, now_ns)
    }

    pub fn scroll(&mut self, horizontal: i32, vertical: i32, now_ns: u64) -> CaptureOutcome {
        if !self.active {
            return CaptureOutcome::Ignored;
        }
        if let Err(reason) = self.check_capture(now_ns) {
            return CaptureOutcome::Paused(reason);
        }
        if horizontal == 0 && vertical == 0 {
            return CaptureOutcome::Ignored;
        }
        self.enqueue(
            InputAction::Scroll {
                horizontal,
                vertical,
            },
            now_ns,
        )
    }

    /// Check freshness again at delivery. Any failure pauses and empties the
    /// queue; the caller must release remote input, not send stale key releases.
    pub fn pop(&mut self, now_ns: u64) -> Result<Option<QueuedInput>, PauseReason> {
        if !self.active {
            return Ok(None);
        }
        self.check_poll(now_ns)?;
        Ok(self.queue.pop_front())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed() -> SourceInputQueue {
        let mut queue = SourceInputQueue::new();
        queue.seed_physical_held([], []).unwrap();
        queue.begin_session();
        assert!(queue.arm());
        queue
    }

    #[test]
    fn positions_preserve_barriers_and_relative_baseline() {
        let mut queue = armed();
        queue.move_by(1.0, 2.0, 1);
        queue.position(-400.0, 500.0, 7, 2);
        assert_eq!(
            queue.position(-390.0, 510.0, 7, 3),
            CaptureOutcome::Coalesced
        );
        queue.key(0x04, true, 4);
        queue.position(10.0, 20.0, 7, 5);
        queue.move_by(3.0, 4.0, 6);
        let events: Vec<_> = std::iter::from_fn(|| queue.pop(7).unwrap()).collect();
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].action, InputAction::Move { x: 1.0, y: 2.0 });
        assert_eq!(
            events[1].action,
            InputAction::Position {
                x: -390.0,
                y: 510.0,
                display_id: 7
            }
        );
        assert_eq!(
            (events[1].captured_at_ns, events[1].captures_coalesced),
            (2, 2)
        );
        assert_eq!(
            events[2].action,
            InputAction::Key {
                usage: 0x04,
                down: true
            }
        );
        assert_eq!(
            events[3].action,
            InputAction::Position {
                x: 10.0,
                y: 20.0,
                display_id: 7
            }
        );
        assert_eq!(events[4].action, InputAction::Move { x: 4.0, y: 6.0 });
    }

    #[test]
    fn positions_cannot_refresh_backlog_or_survive_pause() {
        let mut queue = armed();
        queue.position(1.0, 2.0, 7, 0);
        assert_eq!(
            queue.position(3.0, 4.0, 7, INPUT_MAX_AGE_NS + 1),
            CaptureOutcome::Paused(PauseReason::StaleInput)
        );
        assert_eq!(queue.pending_len(), 0);
        queue.arm();
        assert_eq!(
            queue.position(f64::NAN, 0.0, 7, 1),
            CaptureOutcome::Paused(PauseReason::InvalidMovement)
        );
        queue.arm();
        queue.position(10.0, 20.0, 7, 2);
        queue.pause();
        queue.arm();
        assert!(queue.pop(3).unwrap().is_none());
    }

    #[test]
    fn display_identity_change_is_a_position_barrier() {
        let mut queue = armed();
        queue.position(10.0, 20.0, 7, 0);
        assert_eq!(queue.position(30.0, 40.0, 8, 1), CaptureOutcome::Queued);
        assert_eq!(queue.pending_len(), 2);
        assert!(matches!(
            queue.pop(2).unwrap().unwrap().action,
            InputAction::Position { display_id: 7, .. }
        ));
        assert!(matches!(
            queue.pop(2).unwrap().unwrap().action,
            InputAction::Position { display_id: 8, .. }
        ));
    }

    #[test]
    fn movement_coalesces_only_between_ordered_barriers() {
        let mut queue = armed();
        queue.move_by(2.0, 3.0, 1);
        assert_eq!(queue.move_by(4.0, -1.0, 2), CaptureOutcome::Coalesced);
        queue.button(Button::Left, true, 3);
        queue.move_by(1.0, 1.0, 4);
        queue.scroll(15, -120, 5);
        queue.key(0x04, true, 6);
        queue.move_by(-2.0, 0.0, 7);
        assert_eq!(queue.pending_len(), 6);
        let first = queue.pop(8).unwrap().unwrap();
        assert_eq!(first.action, InputAction::Move { x: 6.0, y: 2.0 });
        assert_eq!((first.captured_at_ns, first.captures_coalesced), (1, 2));
        let actions: Vec<_> = std::iter::from_fn(|| queue.pop(8).unwrap())
            .map(|event| event.action)
            .collect();
        assert_eq!(
            actions,
            vec![
                InputAction::Button {
                    button: Button::Left,
                    down: true
                },
                InputAction::Move { x: 7.0, y: 3.0 },
                InputAction::Scroll {
                    horizontal: 15,
                    vertical: -120
                },
                InputAction::Key {
                    usage: 0x04,
                    down: true
                },
                InputAction::Move { x: 5.0, y: 3.0 },
            ]
        );
    }

    #[test]
    fn clicks_and_key_releases_survive_but_repeats_and_unmatched_ups_do_not() {
        let mut queue = armed();
        for now in [1, 3] {
            queue.button(Button::Left, true, now);
            queue.button(Button::Left, false, now + 1);
        }
        // Modifier repeats (e.g. holding Shift) remain ignored.
        queue.key(0xE1, true, 5);
        assert_eq!(queue.key(0xE1, true, 6), CaptureOutcome::Ignored);
        queue.key(0xE1, false, 7);
        assert_eq!(queue.key(0xE1, false, 8), CaptureOutcome::Ignored);
        assert_eq!(
            queue.button(Button::Right, false, 9),
            CaptureOutcome::Ignored
        );
        // Active-epoch Backspace repeat IS forwarded so long-press delete works.
        assert_eq!(queue.key(0x2A, true, 10), CaptureOutcome::Queued);
        assert_eq!(queue.key(0x2A, true, 11), CaptureOutcome::Queued);
        assert_eq!(queue.key(0x2A, false, 12), CaptureOutcome::Queued);
        assert_eq!(queue.key(0x2A, false, 13), CaptureOutcome::Ignored);
        assert_eq!(queue.pending_len(), 9);
        let actions: Vec<_> = std::iter::from_fn(|| queue.pop(14).unwrap())
            .map(|event| event.action)
            .collect();
        assert_eq!(
            actions[0],
            InputAction::Button {
                button: Button::Left,
                down: true
            }
        );
        assert_eq!(
            actions[1],
            InputAction::Button {
                button: Button::Left,
                down: false
            }
        );
        assert_eq!(actions[2..4], actions[0..2]);
        assert_eq!(
            actions[4..],
            [
                InputAction::Key {
                    usage: 0xE1,
                    down: true
                },
                InputAction::Key {
                    usage: 0xE1,
                    down: false
                },
                InputAction::Key {
                    usage: 0x2A,
                    down: true
                },
                InputAction::Key {
                    usage: 0x2A,
                    down: true
                },
                InputAction::Key {
                    usage: 0x2A,
                    down: false
                },
            ]
        );
    }

    #[test]
    fn physically_held_inputs_do_not_cross_pause_or_session_boundaries() {
        let mut queue = SourceInputQueue::new();
        queue.seed_physical_held([], []).unwrap();
        queue.key(0xE1, true, 1);
        queue.button(Button::Back, true, 2);
        queue.begin_session();
        assert!(queue.arm());
        assert_eq!(queue.key(0xE1, true, 3), CaptureOutcome::Ignored);
        assert_eq!(queue.key(0xE1, false, 4), CaptureOutcome::Ignored);
        assert_eq!(
            queue.button(Button::Back, false, 5),
            CaptureOutcome::Ignored
        );
        queue.key(0xE1, true, 6);
        queue.end_session();
        assert_eq!(queue.pending_len(), 0);
        assert!(!queue.arm());
        queue.begin_session();
        queue.arm();
        assert_eq!(queue.key(0xE1, true, 7), CaptureOutcome::Ignored);
        assert_eq!(queue.key(0xE1, false, 8), CaptureOutcome::Ignored);
        assert_eq!(queue.key(0xE1, true, 9), CaptureOutcome::Queued);
        queue.pause();
        queue.move_by(100.0, 100.0, 10);
        queue.arm();
        queue.move_by(1.0, 2.0, 11);
        assert_eq!(
            queue.pop(12).unwrap().unwrap().action,
            InputAction::Move { x: 1.0, y: 2.0 }
        );
    }

    #[test]
    fn overflow_discards_whole_epoch_and_never_loses_only_a_release() {
        let mut queue = armed();
        queue.key(0x04, true, 0);
        for now in 1..INPUT_QUEUE_CAPACITY as u64 {
            queue.scroll(0, 1, now);
        }
        assert_eq!(queue.pending_len(), INPUT_QUEUE_CAPACITY);
        assert_eq!(
            queue.key(0x04, false, 256),
            CaptureOutcome::Paused(PauseReason::QueueOverflow)
        );
        assert!(!queue.is_active());
        assert_eq!(queue.pending_len(), 0);
        assert!(queue.pop(257).unwrap().is_none());
        queue.arm();
        assert_eq!(queue.key(0x04, true, 258), CaptureOutcome::Queued);
    }

    #[test]
    fn full_queue_can_coalesce_last_move_but_cannot_displace_a_control() {
        let mut queue = armed();
        for now in 0..255 {
            queue.scroll(0, 1, now);
        }
        queue.move_by(1.0, 2.0, 255);
        assert_eq!(queue.pending_len(), INPUT_QUEUE_CAPACITY);
        assert_eq!(queue.move_by(2.0, 3.0, 256), CaptureOutcome::Coalesced);
        assert!(queue.is_active());
        assert_eq!(
            queue.button(Button::Forward, true, 257),
            CaptureOutcome::Paused(PauseReason::QueueOverflow)
        );
        assert_eq!(queue.pending_len(), 0);
        queue.arm();
        assert_eq!(
            queue.button(Button::Forward, true, 258),
            CaptureOutcome::Ignored
        );
        assert_eq!(
            queue.button(Button::Forward, false, 259),
            CaptureOutcome::Ignored
        );
    }

    #[test]
    fn stale_coalesced_movement_keeps_earliest_age_and_fails_closed() {
        let mut queue = armed();
        queue.move_by(1.0, 0.0, 1);
        queue.move_by(1.0, 0.0, INPUT_MAX_AGE_NS);
        assert_eq!(
            queue.pop(INPUT_MAX_AGE_NS + 2),
            Err(PauseReason::StaleInput)
        );
        assert!(!queue.is_active());
        assert_eq!(queue.pending_len(), 0);
        queue.arm();
        queue.key(0x04, true, 1);
        assert_eq!(
            queue.key(0x04, false, INPUT_MAX_AGE_NS + 2),
            CaptureOutcome::Paused(PauseReason::StaleInput)
        );
        queue.arm();
        assert_eq!(
            queue.key(0x04, true, INPUT_MAX_AGE_NS + 3),
            CaptureOutcome::Queued
        );
    }

    #[test]
    fn invalid_motion_and_clock_regression_pause_without_forwarding() {
        for delta in [f64::NAN, f64::INFINITY, MAX_RELATIVE_MOVE + 1.0] {
            let mut queue = armed();
            queue.key(0x04, true, 1);
            assert_eq!(
                queue.move_by(delta, 0.0, 2),
                CaptureOutcome::Paused(PauseReason::InvalidMovement)
            );
            assert!(!queue.is_active());
            assert_eq!(queue.pending_len(), 0);
        }
        let mut queue = armed();
        queue.x = MAX_COORDINATE;
        assert_eq!(
            queue.move_by(1.0, 0.0, 1),
            CaptureOutcome::Paused(PauseReason::InvalidMovement)
        );
        assert!(!queue.is_active());
        let mut queue = armed();
        queue.move_by(1.0, 0.0, 10);
        assert_eq!(queue.pop(9), Err(PauseReason::ClockRegressed));
        assert_eq!(queue.pending_len(), 0);
    }

    #[test]
    fn snapshot_must_be_valid_and_paused_and_is_invalidated_on_capture_reset() {
        let mut queue = SourceInputQueue::new();
        queue.begin_session();
        assert!(!queue.arm());
        assert_eq!(
            queue.seed_physical_held([], [0]),
            Err(SeedError::InvalidKey)
        );
        assert!(!queue.arm());
        queue.seed_physical_held([Button::Left], [0xE1]).unwrap();
        assert_eq!(
            queue.seed_physical_held([], [0xFFFF]),
            Err(SeedError::InvalidKey)
        );
        assert!(queue.arm());
        assert_eq!(queue.seed_physical_held([], []), Err(SeedError::Active));
        assert_eq!(queue.key(0xE1, false, 1), CaptureOutcome::Ignored);
        assert_eq!(
            queue.button(Button::Left, false, 2),
            CaptureOutcome::Ignored
        );
        queue.key(0x04, true, 3);
        queue.capture_reset();
        assert_eq!(queue.pending_len(), 0);
        assert!(!queue.arm());
        queue.begin_session();
        assert!(!queue.arm());
        queue.seed_physical_held([], [0x04]).unwrap();
        assert!(queue.arm());
        assert_eq!(queue.key(0x04, true, 4), CaptureOutcome::Ignored);
        assert_eq!(queue.key(0x04, false, 5), CaptureOutcome::Ignored);
        assert_eq!(queue.key(0x04, true, 6), CaptureOutcome::Queued);
    }

    #[test]
    fn reserved_keyboard_usages_never_enter_physical_state_or_queue() {
        for usage in [0, 1, 2, 3, 0xA5, 0xAF, 0xDE, 0xDF, 0xE8, u16::MAX] {
            let mut queue = SourceInputQueue::new();
            assert_eq!(queue.key(usage, true, 1), CaptureOutcome::Ignored);
            assert!(queue.local_keys.is_empty());
            queue.seed_physical_held([], []).unwrap();
            queue.begin_session();
            queue.arm();
            queue.key(0x04, true, 2);
            assert_eq!(
                queue.key(usage, true, 3),
                CaptureOutcome::Paused(PauseReason::InvalidKey)
            );
            assert!(!queue.is_active());
            assert_eq!(queue.pending_len(), 0);
            assert!(!queue.local_keys.contains(&usage));
        }
        for usage in [0x04, 0xA4, 0xB0, 0xDD, 0xE0, 0xE7] {
            let mut queue = armed();
            assert_eq!(queue.key(usage, true, 1), CaptureOutcome::Queued);
            assert_eq!(queue.key(usage, false, 2), CaptureOutcome::Queued);
        }
    }

    #[test]
    fn rapid_pause_and_rearm_exposes_a_new_control_epoch() {
        let mut queue = armed();
        let previous = queue.control_epoch();
        queue.move_by(1.0, 2.0, 1);
        queue.pause();
        assert!(queue.arm());
        assert!(queue.is_active());
        assert_ne!(queue.control_epoch(), previous);
        assert_eq!(queue.pending_len(), 0);
        queue.move_by(3.0, 4.0, 2);
        assert_eq!(
            queue.pop(3).unwrap().unwrap().action,
            InputAction::Move { x: 3.0, y: 4.0 }
        );
        queue.end_session();
        assert!(!queue.is_connected());
        queue.begin_session();
        assert!(queue.is_connected());
        assert!(!queue.is_active());
    }

    #[test]
    fn delayed_capture_preserves_timestamp_without_regressing_sender_poll_clock() {
        let mut queue = armed();
        queue.move_by(1.0, 0.0, 80);
        assert_eq!(queue.pop(100).unwrap().unwrap().captured_at_ns, 80);
        // Captured in order, but delivery to this mutex occurred after poll 100.
        assert_eq!(queue.move_by(2.0, 0.0, 90), CaptureOutcome::Queued);
        let delivered = queue.pop(110).unwrap().unwrap();
        assert_eq!(delivered.captured_at_ns, 90);
        assert_eq!(delivered.action, InputAction::Move { x: 3.0, y: 0.0 });
        assert!(queue.is_active());
        // A genuinely reversed capture still fails, independently of polling.
        assert_eq!(
            queue.key(0x04, true, 89),
            CaptureOutcome::Paused(PauseReason::ClockRegressed)
        );
        assert_eq!(queue.pending_len(), 0);
    }

    #[test]
    fn delayed_capture_is_still_aged_and_poll_rollback_is_independent() {
        let mut queue = armed();
        queue.move_by(1.0, 0.0, 80);
        queue.pop(100).unwrap();
        queue.validate_pending(INPUT_MAX_AGE_NS + 200).unwrap();
        assert_eq!(queue.key(0x04, true, 90), CaptureOutcome::Queued);
        assert_eq!(
            queue.pop(INPUT_MAX_AGE_NS + 201),
            Err(PauseReason::StaleInput)
        );
        assert!(!queue.is_active());
        assert_eq!(queue.pending_len(), 0);
        queue.arm();
        queue.move_by(1.0, 0.0, 80);
        queue.pop(100).unwrap();
        // No reversed physical capture is involved in this polling rollback.
        assert_eq!(queue.validate_pending(99), Err(PauseReason::ClockRegressed));
        assert!(!queue.is_active());
    }

    #[test]
    fn grouped_movement_bound_resets_at_barriers_and_after_delivery() {
        let mut queue = armed();
        queue.move_by(15_000.0, 0.0, 1);
        assert_eq!(
            queue.move_by(15_000.0, 0.0, 2),
            CaptureOutcome::Paused(PauseReason::InvalidMovement)
        );
        assert_eq!(queue.pending_len(), 0);
        queue.arm();
        queue.move_by(15_000.0, 0.0, 3);
        queue.scroll(0, 1, 4);
        assert_eq!(queue.move_by(15_000.0, 0.0, 5), CaptureOutcome::Queued);
        assert_eq!(queue.pending_len(), 3);
        queue.pop(6).unwrap();
        queue.pop(6).unwrap();
        assert_eq!(
            queue.pop(6).unwrap().unwrap().action,
            InputAction::Move {
                x: 30_000.0,
                y: 0.0
            }
        );
        assert_eq!(queue.move_by(15_000.0, 0.0, 7), CaptureOutcome::Queued);
        assert_eq!(
            queue.pop(8).unwrap().unwrap().action,
            InputAction::Move {
                x: 45_000.0,
                y: 0.0
            }
        );
    }
}
