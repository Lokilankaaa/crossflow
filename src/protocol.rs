use crate::input::{Button, InputAction, QueuedInput, is_keyboard_usage};
use anyhow::{Result, bail, ensure};

/// Version 2 is intentionally incompatible with the movement-only v1 wire
/// format. Both peers must be rebuilt before enabling the expanded inputs.
pub const FRAME_LEN: usize = 72;
pub const WIRE_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Lease = 1,
    Arm = 2,
    Move = 3,
    Pause = 4,
    Heartbeat = 5,
    Stop = 6,
    Button = 7,
    Scroll = 8,
    Key = 9,
}

pub fn button_code(button: Button) -> u16 {
    match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::Back => 3,
        Button::Forward => 4,
    }
}

pub fn button_from_code(code: u16) -> Result<Button> {
    Ok(match code {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::Back,
        4 => Button::Forward,
        _ => bail!("invalid button code"),
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub kind: Kind,
    pub seq: u64,
    pub t0_ns: u64,
    pub t1_ns: u64,
    pub lease: u64,
    pub x: f64,
    pub y: f64,
    pub code: u16,
    pub down: bool,
    pub wheel_x: i32,
    pub wheel_y: i32,
}

impl Frame {
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            seq: 0,
            t0_ns: 0,
            t1_ns: 0,
            lease: 0,
            x: 0.0,
            y: 0.0,
            code: 0,
            down: false,
            wheel_x: 0,
            wheel_y: 0,
        }
    }

    /// Convert an ordered source event without assigning transport sequence or
    /// receiver lease. The caller sets those fields for its authenticated session.
    pub fn from_input(input: QueuedInput, t1_ns: u64) -> Result<Self> {
        ensure!(
            t1_ns >= input.captured_at_ns,
            "source-local time order invalid"
        );
        let kind = match input.action {
            InputAction::Move { .. } => Kind::Move,
            InputAction::Button { .. } => Kind::Button,
            InputAction::Scroll { .. } => Kind::Scroll,
            InputAction::Key { .. } => Kind::Key,
        };
        let mut frame = Self::new(kind);
        frame.t0_ns = input.captured_at_ns;
        frame.t1_ns = t1_ns;
        match input.action {
            InputAction::Move { x, y } => {
                frame.x = x;
                frame.y = y;
            }
            InputAction::Button { button, down } => {
                frame.code = button_code(button);
                frame.down = down;
            }
            InputAction::Scroll {
                horizontal,
                vertical,
            } => {
                frame.wheel_x = horizontal;
                frame.wheel_y = vertical;
            }
            InputAction::Key { usage, down } => {
                frame.code = usage;
                frame.down = down;
            }
        }
        frame.validate()?;
        Ok(frame)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.x.is_finite() && self.y.is_finite(),
            "non-finite coordinates"
        );
        ensure!(
            self.x.abs() <= (1u64 << 48) as f64 && self.y.abs() <= (1u64 << 48) as f64,
            "coordinate bound"
        );
        match self.kind {
            Kind::Button | Kind::Key => {
                ensure!(
                    self.x == 0.0 && self.y == 0.0,
                    "nonzero discrete-input coordinates"
                );
                ensure!(
                    self.wheel_x == 0 && self.wheel_y == 0,
                    "unexpected wheel payload"
                );
                if self.kind == Kind::Button {
                    button_from_code(self.code)?;
                } else {
                    ensure!(is_keyboard_usage(self.code), "invalid keyboard usage");
                }
            }
            Kind::Scroll => {
                ensure!(self.x == 0.0 && self.y == 0.0, "nonzero scroll coordinates");
                ensure!(
                    self.code == 0 && !self.down,
                    "unexpected scroll key/button payload"
                );
                ensure!(
                    self.wheel_x != 0 || self.wheel_y != 0,
                    "empty scroll payload"
                );
                ensure!(
                    i64::from(self.wheel_x).abs() <= 32768
                        && i64::from(self.wheel_y).abs() <= 32768,
                    "wheel bound"
                );
            }
            _ => ensure!(
                self.code == 0 && !self.down && self.wheel_x == 0 && self.wheel_y == 0,
                "unexpected discrete-input payload"
            ),
        }
        Ok(())
    }

    pub fn encode(self) -> Result<[u8; FRAME_LEN]> {
        self.validate()?;
        let mut out = [0u8; FRAME_LEN];
        out[..4].copy_from_slice(b"CFL0");
        out[4] = WIRE_VERSION;
        out[5] = self.kind as u8;
        for (offset, value) in [
            (8, self.seq),
            (16, self.t0_ns),
            (24, self.t1_ns),
            (32, self.lease),
        ] {
            out[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        }
        out[40..48].copy_from_slice(&self.x.to_be_bytes());
        out[48..56].copy_from_slice(&self.y.to_be_bytes());
        out[56..58].copy_from_slice(&self.code.to_be_bytes());
        out[58] = u8::from(self.down);
        out[60..64].copy_from_slice(&self.wheel_x.to_be_bytes());
        out[64..68].copy_from_slice(&self.wheel_y.to_be_bytes());
        Ok(out)
    }
    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() >= 5, "invalid frame length");
        ensure!(&data[..4] == b"CFL0", "invalid header");
        ensure!(
            data[4] == WIRE_VERSION,
            "unsupported wire version {}; protocol v2 requires both peers to rebuild",
            data[4]
        );
        ensure!(data.len() == FRAME_LEN, "invalid frame length");
        ensure!(
            data[6..8] == [0, 0] && data[59] == 0 && data[68..72] == [0, 0, 0, 0],
            "nonzero reserved field"
        );
        ensure!(data[58] <= 1, "invalid down flag");
        let kind = match data[5] {
            1 => Kind::Lease,
            2 => Kind::Arm,
            3 => Kind::Move,
            4 => Kind::Pause,
            5 => Kind::Heartbeat,
            6 => Kind::Stop,
            7 => Kind::Button,
            8 => Kind::Scroll,
            9 => Kind::Key,
            _ => bail!("unknown frame type"),
        };
        let num = |o| u64::from_be_bytes(data[o..o + 8].try_into().unwrap());
        let frame = Self {
            kind,
            seq: num(8),
            t0_ns: num(16),
            t1_ns: num(24),
            lease: num(32),
            x: f64::from_be_bytes(data[40..48].try_into().unwrap()),
            y: f64::from_be_bytes(data[48..56].try_into().unwrap()),
            code: u16::from_be_bytes(data[56..58].try_into().unwrap()),
            down: data[58] == 1,
            wheel_x: i32::from_be_bytes(data[60..64].try_into().unwrap()),
            wheel_y: i32::from_be_bytes(data[64..68].try_into().unwrap()),
        };
        frame.validate()?;
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip() {
        let mut f = Frame::new(Kind::Move);
        f.x = -123.125;
        f.y = 8.5;
        f.lease = 100;
        let d = Frame::decode(&f.encode().unwrap()).unwrap();
        assert_eq!(d.x, f.x);
        assert_eq!(d.lease, 100);
    }
    #[test]
    fn rejects_invalid() {
        for n in [0, 55, 57, 71, 73] {
            assert!(Frame::decode(&vec![0; n]).is_err());
        }
        let mut f = Frame::new(Kind::Move);
        f.x = f64::NAN;
        assert!(f.encode().is_err());
        let mut b = Frame::new(Kind::Arm).encode().unwrap();
        b[6] = 1;
        assert!(Frame::decode(&b).is_err());
    }

    fn captured(action: InputAction) -> QueuedInput {
        QueuedInput {
            action,
            captured_at_ns: 100,
            captures_coalesced: 3,
        }
    }

    #[test]
    fn all_source_actions_round_trip_with_capture_time_and_no_session_assignment() {
        let mut actions = vec![
            InputAction::Move {
                x: -123.125,
                y: 8.5,
            },
            InputAction::Scroll {
                horizontal: -15,
                vertical: 120,
            },
            InputAction::Scroll {
                horizontal: 32768,
                vertical: -32768,
            },
        ];
        for button in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ] {
            for down in [false, true] {
                actions.push(InputAction::Button { button, down });
            }
        }
        for usage in [0x04, 0xA4, 0xB0, 0xDD, 0xE0, 0xE7] {
            for down in [false, true] {
                actions.push(InputAction::Key { usage, down });
            }
        }
        for action in actions {
            let mut frame = Frame::from_input(captured(action), 150).unwrap();
            assert_eq!((frame.t0_ns, frame.t1_ns), (100, 150));
            assert_eq!((frame.seq, frame.lease), (0, 0));
            match action {
                InputAction::Move { x, y } => {
                    assert_eq!(frame.kind, Kind::Move);
                    assert_eq!((frame.x, frame.y), (x, y));
                }
                InputAction::Button { button, down } => {
                    assert_eq!(frame.kind, Kind::Button);
                    assert_eq!(button_from_code(frame.code).unwrap(), button);
                    assert_eq!(frame.down, down);
                }
                InputAction::Scroll {
                    horizontal,
                    vertical,
                } => {
                    assert_eq!(frame.kind, Kind::Scroll);
                    assert_eq!((frame.wheel_x, frame.wheel_y), (horizontal, vertical));
                }
                InputAction::Key { usage, down } => {
                    assert_eq!(frame.kind, Kind::Key);
                    assert_eq!((frame.code, frame.down), (usage, down));
                }
            }
            frame.seq = 9;
            frame.lease = 12;
            assert_eq!(Frame::decode(&frame.encode().unwrap()).unwrap(), frame);
        }
    }

    #[test]
    fn wire_layout_is_big_endian_and_reserved_bytes_are_checked() {
        let frame = Frame::from_input(
            captured(InputAction::Key {
                usage: 0xE1,
                down: true,
            }),
            150,
        )
        .unwrap();
        let bytes = frame.encode().unwrap();
        assert_eq!(bytes.len(), 72);
        assert_eq!(&bytes[..6], &[b'C', b'F', b'L', b'0', 2, 9]);
        assert_eq!(&bytes[56..60], &[0, 0xE1, 1, 0]);
        assert_eq!(&bytes[60..72], &[0; 12]);
        for offset in [6, 7, 59, 68, 69, 70, 71] {
            let mut bad = bytes;
            bad[offset] = 1;
            assert!(Frame::decode(&bad).is_err(), "reserved offset {offset}");
        }
        for flag in [2, 255] {
            let mut bad = bytes;
            bad[58] = flag;
            assert!(Frame::decode(&bad).is_err());
        }
        let scroll = Frame::from_input(
            captured(InputAction::Scroll {
                horizontal: -120,
                vertical: 15,
            }),
            100,
        )
        .unwrap()
        .encode()
        .unwrap();
        assert_eq!(&scroll[60..64], &(-120i32).to_be_bytes());
        assert_eq!(&scroll[64..68], &15i32.to_be_bytes());
        let mut unknown = bytes;
        unknown[5] = 10;
        assert!(Frame::decode(&unknown).is_err());
    }

    #[test]
    fn old_wire_version_is_explicitly_rejected_even_at_old_length() {
        let mut old = Frame::new(Kind::Move).encode().unwrap();
        old[4] = 1;
        for bytes in [&old[..], &old[..56]] {
            let error = Frame::decode(bytes).unwrap_err().to_string();
            assert!(error.contains("unsupported wire version 1"));
            assert!(error.contains("both peers"));
        }
    }

    #[test]
    fn invalid_codes_and_scroll_bounds_fail_on_encode_and_decode() {
        for code in [5, u16::MAX] {
            assert!(button_from_code(code).is_err());
            let mut frame = Frame::new(Kind::Button);
            frame.code = code;
            assert!(frame.encode().is_err());
            let mut bad = Frame::new(Kind::Button).encode().unwrap();
            bad[56..58].copy_from_slice(&code.to_be_bytes());
            assert!(Frame::decode(&bad).is_err());
        }
        for usage in [0, 1, 2, 3, 0xA5, 0xAF, 0xDE, 0xDF, 0xE8, u16::MAX] {
            let mut frame = Frame::new(Kind::Key);
            frame.code = usage;
            assert!(frame.encode().is_err());
            let mut valid = Frame::new(Kind::Key);
            valid.code = 0x04;
            let mut bad = valid.encode().unwrap();
            bad[56..58].copy_from_slice(&usage.to_be_bytes());
            assert!(Frame::decode(&bad).is_err());
        }
        assert!(Frame::new(Kind::Scroll).encode().is_err());
        for wheel in [i32::MIN, -32769, 32769, i32::MAX] {
            let mut frame = Frame::new(Kind::Scroll);
            frame.wheel_x = wheel;
            assert!(frame.encode().is_err());
            frame.wheel_x = 0;
            frame.wheel_y = wheel;
            assert!(frame.encode().is_err());
            let mut valid = Frame::new(Kind::Scroll);
            valid.wheel_y = 120;
            let mut bad = valid.encode().unwrap();
            bad[60..64].copy_from_slice(&wheel.to_be_bytes());
            assert!(Frame::decode(&bad).is_err());
        }
    }

    #[test]
    fn action_payloads_cannot_be_smuggled_on_another_kind() {
        for kind in [
            Kind::Lease,
            Kind::Arm,
            Kind::Move,
            Kind::Pause,
            Kind::Heartbeat,
            Kind::Stop,
        ] {
            let valid = Frame::new(kind);
            let mut bad = valid;
            bad.code = 1;
            assert!(bad.encode().is_err());
            bad = valid;
            bad.down = true;
            assert!(bad.encode().is_err());
            bad = valid;
            bad.wheel_x = 1;
            assert!(bad.encode().is_err());
            bad = valid;
            bad.wheel_y = 1;
            assert!(bad.encode().is_err());
        }
        for kind in [Kind::Button, Kind::Key, Kind::Scroll] {
            let mut valid = Frame::new(kind);
            if kind == Kind::Key {
                valid.code = 0x04;
            }
            if kind == Kind::Scroll {
                valid.wheel_y = 120;
            }
            for coordinate in [1.0, f64::NAN, f64::INFINITY] {
                let mut bad = valid;
                bad.x = coordinate;
                assert!(bad.encode().is_err());
                bad = valid;
                bad.y = coordinate;
                assert!(bad.encode().is_err());
            }
            let mut bad = valid;
            if kind == Kind::Scroll {
                bad.code = 1;
                assert!(bad.encode().is_err());
                bad = valid;
                bad.down = true;
            } else {
                bad.wheel_x = 1;
            }
            assert!(bad.encode().is_err());
        }
    }

    #[test]
    fn conversion_rejects_invalid_time_and_payload_but_preserves_old_control_timing() {
        assert!(Frame::from_input(captured(InputAction::Move { x: 1.0, y: 2.0 }), 99).is_err());
        assert!(
            Frame::from_input(
                captured(InputAction::Scroll {
                    horizontal: 0,
                    vertical: 0
                }),
                100
            )
            .is_err()
        );
        assert!(
            Frame::from_input(
                captured(InputAction::Key {
                    usage: 0,
                    down: true
                }),
                100
            )
            .is_err()
        );
        // Lease frames stamp t0 only; input age/order checks remain session rules.
        let mut lease = Frame::new(Kind::Lease);
        lease.t0_ns = 100;
        assert!(lease.encode().is_ok());
        let mut movement = Frame::new(Kind::Move);
        movement.x = (1u64 << 48) as f64 + 1.0;
        assert!(movement.encode().is_err());
    }
}
