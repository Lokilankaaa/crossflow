//! Private inherited-pipe protocol; no network endpoint or input injection.
//!
//! Timestamps must use one parent/worker monotonic clock origin. A process-local
//! elapsed Instant is not interchangeable with another process's elapsed time.
//! Validation failure permanently closes the lease. The worker must then release
//! its owned inputs directly, without routing cleanup through `accept` again.
//! A Completed response means the adapter returned, not that the OS delivered it.

use anyhow::{Result, bail, ensure};

pub const FRAME_LEN: usize = 80;
pub const VERSION: u8 = 2;
pub const ACTIVE_LEASE_NS: u64 = 150_000_000;
pub const IDLE_LEASE_NS: u64 = 10_000_000_000;
pub const MAX_REQUEST_AGE_NS: u64 = 30_000_000;
const MAGIC: [u8; 4] = *b"CFIW";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    Ready = 1,
    Reset = 2,
    Move = 3,
    Button = 4,
    Key = 5,
    Scroll = 6,
    Heartbeat = 7,
    Release = 8,
    Shutdown = 9,
    Completed = 10,
    Failed = 11,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Message {
    pub op: Op,
    pub seq: u64,
    pub epoch: u64,
    pub sent_ns: u64,
    pub x: f64,
    pub y: f64,
    pub a: i32,
    pub b: i32,
    pub code: u16,
    pub down: bool,
    pub call_start_ns: u64,
    pub call_end_ns: u64,
}

impl Message {
    pub fn new(op: Op) -> Self {
        Self {
            op,
            seq: 0,
            epoch: 0,
            sent_ns: 0,
            x: 0.0,
            y: 0.0,
            a: 0,
            b: 0,
            code: 0,
            down: false,
            call_start_ns: 0,
            call_end_ns: 0,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.op == Op::Completed {
            ensure!(
                self.call_start_ns <= self.call_end_ns && self.call_end_ns <= self.sent_ns,
                "invalid worker completion timestamps"
            );
        } else {
            ensure!(
                self.call_start_ns == 0 && self.call_end_ns == 0,
                "unrelated worker call timestamps"
            );
        }
        ensure!(
            self.x.is_finite() && self.y.is_finite(),
            "invalid worker coordinates"
        );
        let zero_xy = self.x.to_bits() == 0 && self.y.to_bits() == 0;
        let zero_ab = self.a == 0 && self.b == 0;
        let zero_key = self.code == 0 && !self.down;
        match self.op {
            Op::Move => {
                ensure!(
                    self.x.abs() <= 20_000.0 && self.y.abs() <= 20_000.0,
                    "worker movement exceeds bound"
                );
                ensure!(zero_ab && zero_key, "unrelated worker movement fields");
            }
            Op::Completed => ensure!(zero_ab && zero_key, "unrelated worker completion fields"),
            Op::Button | Op::Key => {
                ensure!(zero_xy && zero_ab, "unrelated worker input fields");
                if self.op == Op::Button {
                    crate::protocol::button_from_code(self.code)?;
                } else {
                    ensure!(
                        crate::input::is_keyboard_usage(self.code),
                        "unsupported worker keyboard usage"
                    );
                }
            }
            Op::Scroll => {
                ensure!(zero_xy && zero_key, "unrelated worker scroll fields");
                ensure!(
                    (self.a != 0 || self.b != 0)
                        && self.a.unsigned_abs() <= 32_768
                        && self.b.unsigned_abs() <= 32_768,
                    "invalid worker scroll"
                );
            }
            _ => ensure!(
                zero_xy && zero_ab && zero_key,
                "unrelated worker control fields"
            ),
        }
        Ok(())
    }

    /// Fixed-width big-endian fields. Bytes 7 and 58..64 are reserved zeroes.
    /// Version 2 adds Completed call start/end timestamps at 64..72/72..80;
    /// its 80-byte records are intentionally incompatible with version 1.
    pub fn encode(&self) -> Result<[u8; FRAME_LEN]> {
        self.validate()?;
        let mut bytes = [0; FRAME_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.op as u8;
        bytes[6] = u8::from(self.down);
        bytes[8..16].copy_from_slice(&self.seq.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.epoch.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.sent_ns.to_be_bytes());
        bytes[32..40].copy_from_slice(&self.x.to_bits().to_be_bytes());
        bytes[40..48].copy_from_slice(&self.y.to_bits().to_be_bytes());
        bytes[48..52].copy_from_slice(&self.a.to_be_bytes());
        bytes[52..56].copy_from_slice(&self.b.to_be_bytes());
        bytes[56..58].copy_from_slice(&self.code.to_be_bytes());
        bytes[64..72].copy_from_slice(&self.call_start_ns.to_be_bytes());
        bytes[72..80].copy_from_slice(&self.call_end_ns.to_be_bytes());
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8; FRAME_LEN]) -> Result<Self> {
        ensure!(
            bytes[..4] == MAGIC && bytes[4] == VERSION,
            "invalid worker protocol header"
        );
        ensure!(
            bytes[6] <= 1 && bytes[7] == 0 && bytes[58..64].iter().all(|&b| b == 0),
            "invalid worker reserved fields"
        );
        let op = match bytes[5] {
            1 => Op::Ready,
            2 => Op::Reset,
            3 => Op::Move,
            4 => Op::Button,
            5 => Op::Key,
            6 => Op::Scroll,
            7 => Op::Heartbeat,
            8 => Op::Release,
            9 => Op::Shutdown,
            10 => Op::Completed,
            11 => Op::Failed,
            _ => bail!("unknown worker operation"),
        };
        let message = Self {
            op,
            seq: u64::from_be_bytes(bytes[8..16].try_into().unwrap()),
            epoch: u64::from_be_bytes(bytes[16..24].try_into().unwrap()),
            sent_ns: u64::from_be_bytes(bytes[24..32].try_into().unwrap()),
            x: f64::from_bits(u64::from_be_bytes(bytes[32..40].try_into().unwrap())),
            y: f64::from_bits(u64::from_be_bytes(bytes[40..48].try_into().unwrap())),
            a: i32::from_be_bytes(bytes[48..52].try_into().unwrap()),
            b: i32::from_be_bytes(bytes[52..56].try_into().unwrap()),
            code: u16::from_be_bytes(bytes[56..58].try_into().unwrap()),
            down: bytes[6] == 1,
            call_start_ns: u64::from_be_bytes(bytes[64..72].try_into().unwrap()),
            call_end_ns: u64::from_be_bytes(bytes[72..80].try_into().unwrap()),
        };
        message.validate()?;
        Ok(message)
    }
}

/// Pure request validation, independent of IPC, clocks, threads and OS state.
/// Invalid requests, deadline expiry, clock regression and Shutdown are terminal.
/// Inputs do not renew the lease: only Reset/Heartbeat/Release set a deadline.
pub struct WorkerLease {
    seq: u64,
    epoch: u64,
    last_now_ns: u64,
    deadline_ns: u64,
    active: bool,
    terminal: bool,
}

impl WorkerLease {
    pub fn new(now_ns: u64) -> Self {
        let deadline = now_ns.checked_add(IDLE_LEASE_NS);
        Self {
            seq: 0,
            epoch: 0,
            last_now_ns: now_ns,
            deadline_ns: deadline.unwrap_or(now_ns),
            active: false,
            terminal: deadline.is_none(),
        }
    }

    pub fn active(&self) -> bool {
        self.active && !self.terminal
    }

    pub fn check(&mut self, now_ns: u64) -> Result<()> {
        if self.terminal || now_ns < self.last_now_ns || now_ns >= self.deadline_ns {
            self.terminal = true;
            self.active = false;
            bail!("worker lease ended");
        }
        self.last_now_ns = now_ns;
        Ok(())
    }

    pub fn accept(&mut self, message: &Message, now_ns: u64) -> Result<()> {
        // A late heartbeat can never revive a lease, even with a fresh timestamp.
        self.check(now_ns)?;
        let result = self.accept_checked(message, now_ns);
        if result.is_err() {
            self.terminal = true;
            self.active = false;
        }
        result
    }

    fn accept_checked(&mut self, message: &Message, now_ns: u64) -> Result<()> {
        message.validate()?;
        ensure!(
            !matches!(message.op, Op::Ready | Op::Completed | Op::Failed),
            "worker response is not a request"
        );
        ensure!(
            self.seq.checked_add(1) == Some(message.seq),
            "worker sequence invalid"
        );
        let age = now_ns.checked_sub(message.sent_ns);
        ensure!(
            age.is_some_and(|age| age < MAX_REQUEST_AGE_NS),
            "worker request time invalid"
        );
        if message.op == Op::Reset {
            ensure!(
                message.epoch > self.epoch && (self.epoch != 0 || message.epoch == 1),
                "worker reset epoch invalid"
            );
        } else {
            ensure!(message.epoch == self.epoch, "worker epoch invalid");
        }
        if matches!(message.op, Op::Move | Op::Button | Op::Key | Op::Scroll) {
            ensure!(self.active, "worker is not active");
        }
        let next_active = match message.op {
            Op::Reset => true,
            Op::Release | Op::Shutdown => false,
            _ => self.active,
        };
        let next_deadline = if matches!(message.op, Op::Reset | Op::Heartbeat | Op::Release) {
            message
                .sent_ns
                .checked_add(if next_active {
                    ACTIVE_LEASE_NS
                } else {
                    IDLE_LEASE_NS
                })
                .ok_or_else(|| anyhow::anyhow!("worker lease time overflow"))?
        } else {
            self.deadline_ns
        };
        self.seq = message.seq;
        self.epoch = message.epoch;
        self.active = next_active;
        self.deadline_ns = next_deadline;
        self.terminal = message.op == Op::Shutdown;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: Op, seq: u64, epoch: u64, sent_ns: u64) -> Message {
        Message {
            seq,
            epoch,
            sent_ns,
            ..Message::new(op)
        }
    }

    #[test]
    fn wire_roundtrip_and_reserved_bytes_are_strict() {
        let mut samples = vec![];
        for op in [
            Op::Ready,
            Op::Reset,
            Op::Move,
            Op::Button,
            Op::Key,
            Op::Scroll,
            Op::Heartbeat,
            Op::Release,
            Op::Shutdown,
            Op::Completed,
            Op::Failed,
        ] {
            let mut message = request(op, 0x0102030405060708, 7, 99);
            match op {
                Op::Move => {
                    message.x = -20_000.;
                    message.y = 0.25;
                }
                Op::Key => {
                    message.code = 0xE0;
                    message.down = true;
                }
                Op::Button => {
                    message.code = 4;
                    message.down = true;
                }
                Op::Scroll => {
                    message.a = -32_768;
                    message.b = 32_768;
                }
                Op::Completed => {
                    message.x = f64::MAX;
                    message.y = -f64::MAX;
                }
                _ => {}
            }
            let bytes = message.encode().unwrap();
            assert_eq!(&bytes[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
            assert_eq!(Message::decode(&bytes).unwrap(), message);
            samples.push(bytes);
        }
        for offset in [0, 4, 5, 6, 7, 58, 63] {
            let mut bytes = samples[0];
            bytes[offset] = 255;
            assert!(Message::decode(&bytes).is_err());
        }
    }

    #[test]
    fn completion_timestamps_are_big_endian_and_ordered() {
        let mut message = Message::new(Op::Completed);
        message.call_start_ns = 0x0102030405060708;
        message.call_end_ns = 0x1112131415161718;
        message.sent_ns = message.call_end_ns + 1;
        let encoded = message.encode().unwrap();
        assert_eq!(&encoded[64..72], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&encoded[72..80], &[17, 18, 19, 20, 21, 22, 23, 24]);
        assert_eq!(Message::decode(&encoded).unwrap(), message);
        assert!(Message::new(Op::Completed).encode().is_ok());
        for (start, end, sent) in [(2_u64, 1_u64, 3_u64), (1, 3, 2)] {
            message.call_start_ns = start;
            message.call_end_ns = end;
            message.sent_ns = sent;
            assert!(message.encode().is_err());
            let mut invalid = Message::new(Op::Completed).encode().unwrap();
            invalid[24..32].copy_from_slice(&sent.to_be_bytes());
            invalid[64..72].copy_from_slice(&start.to_be_bytes());
            invalid[72..80].copy_from_slice(&end.to_be_bytes());
            assert!(Message::decode(&invalid).is_err());
        }
    }

    #[test]
    fn only_completed_may_carry_call_timestamps() {
        for op in [
            Op::Ready,
            Op::Reset,
            Op::Move,
            Op::Button,
            Op::Key,
            Op::Scroll,
            Op::Heartbeat,
            Op::Release,
            Op::Shutdown,
            Op::Failed,
        ] {
            let mut message = Message::new(op);
            if op == Op::Key {
                message.code = 0x04;
            }
            if op == Op::Scroll {
                message.a = 120;
            }
            let clean = message.encode().unwrap();
            for offset in [64, 72] {
                let mut invalid = clean;
                invalid[offset..offset + 8].copy_from_slice(&1_u64.to_be_bytes());
                assert!(Message::decode(&invalid).is_err());
            }
            message.call_start_ns = 1;
            assert!(message.encode().is_err());
            message.call_start_ns = 0;
            message.call_end_ns = 1;
            assert!(message.encode().is_err());
        }
    }

    #[test]
    fn version_one_and_its_short_frame_are_incompatible() {
        let ready = Message::new(Op::Ready);
        assert_eq!((ready.seq, ready.epoch), (0, 0));
        let encoded = ready.encode().unwrap();
        assert_eq!((encoded.len(), encoded[4]), (80, 2));
        let mut padded_v1 = encoded;
        padded_v1[4] = 1;
        assert!(Message::decode(&padded_v1).is_err());
        let short: &[u8] = &padded_v1[..64];
        assert!(<&[u8; FRAME_LEN]>::try_from(short).is_err());
    }

    #[test]
    fn payload_validation_rejects_unrelated_fields_and_invalid_bounds() {
        for op in [
            Op::Ready,
            Op::Reset,
            Op::Heartbeat,
            Op::Release,
            Op::Shutdown,
            Op::Failed,
        ] {
            for field in 0..5 {
                let mut message = Message::new(op);
                match field {
                    0 => message.x = -0.0,
                    1 => message.a = 1,
                    2 => message.b = 1,
                    3 => message.code = 1,
                    _ => message.down = true,
                }
                assert!(message.encode().is_err());
            }
        }
        for x in [f64::NAN, f64::INFINITY, 20_000.1] {
            let message = Message {
                x,
                ..Message::new(Op::Move)
            };
            assert!(message.encode().is_err());
        }
        for code in [0, 1, 0xA5, 0xFFFF] {
            assert!(
                Message {
                    code,
                    ..Message::new(Op::Key)
                }
                .encode()
                .is_err()
            );
        }
        assert!(
            Message {
                code: 5,
                ..Message::new(Op::Button)
            }
            .encode()
            .is_err()
        );
        for a in [0, i32::MIN, 32_769] {
            assert!(
                Message {
                    a,
                    ..Message::new(Op::Scroll)
                }
                .encode()
                .is_err()
            );
        }
        let mut encoded = Message::new(Op::Reset).encode().unwrap();
        encoded[48..52].copy_from_slice(&1_i32.to_be_bytes());
        assert!(Message::decode(&encoded).is_err());
    }

    #[test]
    fn actions_require_reset_and_release_disarms_with_new_epoch_required() {
        let mut lease = WorkerLease::new(0);
        lease.accept(&request(Op::Release, 1, 0, 1), 1).unwrap();
        lease.accept(&request(Op::Reset, 2, 1, 2), 2).unwrap();
        lease.accept(&request(Op::Move, 3, 1, 3), 3).unwrap();
        assert!(lease.active());
        lease.accept(&request(Op::Release, 4, 1, 4), 4).unwrap();
        assert!(!lease.active());
        lease.accept(&request(Op::Reset, 5, 2, 5), 5).unwrap();
        lease.accept(&request(Op::Shutdown, 6, 2, 6), 6).unwrap();
        assert!(!lease.active());
        assert!(lease.accept(&request(Op::Heartbeat, 7, 2, 7), 7).is_err());
        for op in [Op::Move, Op::Button, Op::Key, Op::Scroll] {
            let mut lease = WorkerLease::new(0);
            let mut message = request(op, 1, 0, 1);
            message.code = if op == Op::Key { 0x04 } else { 0 };
            message.a = if op == Op::Scroll { 120 } else { 0 };
            assert!(lease.accept(&message, 1).is_err());
        }
    }

    #[test]
    fn active_deadline_is_based_on_send_time_and_expiry_is_terminal() {
        let sent = 1_000;
        let mut lease = WorkerLease::new(0);
        lease
            .accept(
                &request(Op::Reset, 1, 1, sent),
                sent + MAX_REQUEST_AGE_NS - 1,
            )
            .unwrap();
        lease.check(sent + ACTIVE_LEASE_NS - 1).unwrap();
        assert!(
            lease
                .accept(
                    &request(Op::Heartbeat, 2, 1, sent + ACTIVE_LEASE_NS),
                    sent + ACTIVE_LEASE_NS
                )
                .is_err()
        );
        assert!(!lease.active());
        assert!(
            lease
                .accept(
                    &request(Op::Reset, 2, 2, sent + ACTIVE_LEASE_NS + 1),
                    sent + ACTIVE_LEASE_NS + 1
                )
                .is_err()
        );
    }

    #[test]
    fn heartbeat_before_expiry_renews_but_actions_do_not() {
        let mut lease = WorkerLease::new(0);
        lease.accept(&request(Op::Reset, 1, 1, 0), 0).unwrap();
        let heartbeat = ACTIVE_LEASE_NS - 1;
        lease
            .accept(&request(Op::Heartbeat, 2, 1, heartbeat), heartbeat)
            .unwrap();
        lease
            .accept(&request(Op::Move, 3, 1, heartbeat + 1), heartbeat + 1)
            .unwrap();
        lease.check(heartbeat + ACTIVE_LEASE_NS - 1).unwrap();
        assert!(lease.check(heartbeat + ACTIVE_LEASE_NS).is_err());
    }

    #[test]
    fn idle_health_expires_and_clock_regression_or_suspend_is_terminal() {
        let mut lease = WorkerLease::new(0);
        lease
            .accept(
                &request(Op::Heartbeat, 1, 0, IDLE_LEASE_NS - 1),
                IDLE_LEASE_NS - 1,
            )
            .unwrap();
        lease.check(2 * IDLE_LEASE_NS - 2).unwrap();
        assert!(lease.check(2 * IDLE_LEASE_NS - 1).is_err());
        let mut lease = WorkerLease::new(10);
        assert!(lease.check(9).is_err());
        assert!(lease.check(10).is_err());
        let mut lease = WorkerLease::new(0);
        lease.accept(&request(Op::Reset, 1, 1, 0), 0).unwrap();
        assert!(lease.check(60_000_000_000).is_err());
        assert!(WorkerLease::new(u64::MAX).check(u64::MAX).is_err());
    }

    #[test]
    fn invalid_sequence_epoch_direction_time_or_payload_permanently_closes() {
        for case in 0..8 {
            let mut lease = WorkerLease::new(0);
            lease.accept(&request(Op::Reset, 1, 1, 1), 1).unwrap();
            let mut message = request(Op::Heartbeat, 2, 1, 2);
            let now = match case {
                0 => {
                    message.seq = 1;
                    2
                }
                1 => {
                    message.seq = 3;
                    2
                }
                2 => {
                    message.epoch = 2;
                    2
                }
                3 => {
                    message.op = Op::Reset;
                    2
                }
                4 => {
                    message.op = Op::Completed;
                    2
                }
                5 => {
                    message.sent_ns = 3;
                    2
                }
                6 => 2 + MAX_REQUEST_AGE_NS,
                _ => {
                    message.down = true;
                    2
                }
            };
            assert!(lease.accept(&message, now).is_err());
            assert!(!lease.active());
            assert!(
                lease
                    .accept(&request(Op::Heartbeat, 2, 1, now), now)
                    .is_err()
            );
        }
        let mut lease = WorkerLease::new(0);
        assert!(lease.accept(&request(Op::Reset, 1, 2, 0), 0).is_err());
    }
}
