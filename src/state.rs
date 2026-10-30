use crate::protocol::{Frame, Kind};
use anyhow::{Result, ensure};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default)]
pub struct Motion {
    pub connected: bool,
    pub active: bool,
    pub epoch: u64,
    pub x: f64,
    pub y: f64,
    pub t0_ns: u64,
    pub captures: u64,
    pub pending_since_ns: Option<u64>,
}
impl Motion {
    pub fn connected(&mut self, value: bool) {
        self.connected = value;
        self.active = false;
        self.epoch += 1;
        self.pending_since_ns = None;
    }
    pub fn toggle(&mut self) -> bool {
        self.active = self.connected && !self.active;
        self.epoch += 1;
        self.active
    }
    pub fn add(&mut self, dx: f64, dy: f64, t0_ns: u64) {
        if self.active {
            self.x += dx;
            self.y += dy;
            self.t0_ns = t0_ns;
            self.pending_since_ns.get_or_insert(t0_ns);
            self.captures += 1;
        }
    }
}

pub struct ReceiverState {
    leases: HashMap<u64, u64>,
    last_seq: Option<u64>,
    pub active: bool,
    x: f64,
    y: f64,
    max_age_ns: u64,
}
impl ReceiverState {
    pub fn new(max_age_ns: u64) -> Self {
        Self {
            leases: HashMap::new(),
            last_seq: None,
            active: false,
            x: 0.0,
            y: 0.0,
            max_age_ns,
        }
    }
    pub fn grant(&mut self, token: u64, now: u64) {
        self.leases
            .retain(|_, t| now.saturating_sub(*t) <= self.max_age_ns);
        self.leases.insert(token, now);
    }
    pub fn apply(&mut self, f: Frame, now: u64) -> Result<Option<(f64, f64)>> {
        // Also validate direct callers: wire decoding is not the only way to
        // construct a Frame. Reject invalid payloads before changing state.
        f.encode()?;
        ensure!(f.kind != Kind::Lease, "sender cannot grant lease");
        ensure!(
            self.last_seq.is_none_or(|n| f.seq > n),
            "duplicate or reordered sequence"
        );
        self.last_seq = Some(f.seq);
        if matches!(f.kind, Kind::Pause | Kind::Stop) {
            self.active = false;
            return Ok(None);
        }
        let valid = self
            .leases
            .get(&f.lease)
            .is_some_and(|t| now.saturating_sub(*t) <= self.max_age_ns);
        if !valid {
            self.active = false;
            anyhow::bail!("lease expired; reconnect and re-arm")
        }
        ensure!(f.t1_ns >= f.t0_ns, "source-local time order invalid");
        if f.kind == Kind::Arm {
            self.x = f.x;
            self.y = f.y;
            self.active = true;
            return Ok(None);
        }
        if matches!(f.kind, Kind::Heartbeat | Kind::RoutingState) {
            return Ok(None);
        }
        ensure!(
            matches!(
                f.kind,
                Kind::Move | Kind::Position | Kind::Button | Kind::Scroll | Kind::Key
            ) && self.active,
            "input before explicit arm"
        );
        ensure!(f.t1_ns - f.t0_ns <= 30_000_000, "source input stale");
        if matches!(
            f.kind,
            Kind::Position | Kind::Button | Kind::Scroll | Kind::Key
        ) {
            // Controls are ordered barriers, not displacement baselines.
            // The session layer owns applying/releasing the resulting input.
            return Ok(None);
        }
        let d = (f.x - self.x, f.y - self.y);
        ensure!(
            d.0.abs() <= 20_000.0 && d.1.abs() <= 20_000.0,
            "frame displacement too large"
        );
        self.x = f.x;
        self.y = f.y;
        Ok(Some(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(kind: Kind, seq: u64, x: f64) -> Frame {
        let mut f = Frame::new(kind);
        f.seq = seq;
        f.lease = 9;
        f.x = x;
        f
    }
    #[test]
    fn coalescing_keeps_displacement() {
        let mut s = ReceiverState::new(150);
        s.grant(9, 10);
        s.apply(frame(Kind::Arm, 1, 100.), 11).unwrap();
        assert_eq!(
            s.apply(frame(Kind::Move, 2, 110.), 12).unwrap(),
            Some((10., 0.))
        );
        assert_eq!(
            s.apply(frame(Kind::Move, 3, 150.), 13).unwrap(),
            Some((40., 0.))
        );
    }

    #[test]
    fn absolute_positions_require_arm_fresh_lease_and_do_not_rebase_relative_motion() {
        let mut state = ReceiverState::new(150);
        state.grant(9, 0);
        assert!(state.apply(frame(Kind::Position, 1, 400.0), 1).is_err());
        state.apply(frame(Kind::Arm, 2, 0.0), 2).unwrap();
        state.apply(frame(Kind::Position, 3, 400.0), 3).unwrap();
        assert_eq!(
            state.apply(frame(Kind::Move, 4, 3.0), 4).unwrap(),
            Some((3.0, 0.0))
        );
        state.apply(frame(Kind::Pause, 5, 0.0), 5).unwrap();
        assert!(state.apply(frame(Kind::Position, 6, 1.0), 6).is_err());
        state.apply(frame(Kind::Arm, 7, 0.0), 7).unwrap();
        assert!(state.apply(frame(Kind::Position, 8, 1.0), 151).is_err());
    }
    #[test]
    fn cannot_move_before_arm_or_replay() {
        let mut s = ReceiverState::new(150);
        s.grant(9, 10);
        assert!(s.apply(frame(Kind::Move, 1, 1.), 11).is_err());
        assert!(s.apply(frame(Kind::Arm, 1, 0.), 12).is_err());
    }
    #[test]
    fn old_lease_stops_input() {
        let mut s = ReceiverState::new(150);
        s.grant(9, 10);
        s.apply(frame(Kind::Arm, 1, 0.), 11).unwrap();
        assert!(s.apply(frame(Kind::Move, 2, 1.), 161).is_err());
        assert!(!s.active);
    }
    #[test]
    fn rearm_discards_paused_distance() {
        let mut s = ReceiverState::new(150);
        s.grant(9, 10);
        s.apply(frame(Kind::Arm, 1, 0.), 11).unwrap();
        s.apply(frame(Kind::Pause, 2, 0.), 12).unwrap();
        s.apply(frame(Kind::Arm, 3, 10000.), 13).unwrap();
        assert_eq!(
            s.apply(frame(Kind::Move, 4, 10001.), 14).unwrap(),
            Some((1., 0.))
        );
    }
    #[test]
    fn reconnect_requires_manual_arm() {
        let mut m = Motion::default();
        m.connected(true);
        assert!(m.toggle());
        m.add(1., 2., 3);
        m.connected(false);
        m.connected(true);
        assert!(!m.active);
        assert!(!Motion::default().toggle());
    }

    fn action(kind: Kind, seq: u64) -> Frame {
        let mut f = frame(kind, seq, 0.0);
        match kind {
            Kind::Button => {
                f.code = 0;
                f.down = true;
            }
            Kind::Scroll => f.wheel_y = -120,
            Kind::Key => {
                f.code = 0xE1;
                f.down = true;
            }
            _ => panic!("test action must be a button, scroll, or key"),
        }
        f
    }

    #[test]
    fn controls_require_arm_and_cannot_replay_or_resume_after_pause() {
        for kind in [Kind::Button, Kind::Scroll, Kind::Key] {
            let mut s = ReceiverState::new(150);
            s.grant(9, 10);
            assert!(s.apply(action(kind, 1), 11).is_err());
            s.apply(frame(Kind::Arm, 2, 100.), 12).unwrap();
            assert_eq!(s.apply(action(kind, 3), 13).unwrap(), None);
            assert!(s.apply(action(kind, 3), 14).is_err());
            s.apply(frame(Kind::Pause, 4, 0.), 15).unwrap();
            assert!(s.apply(action(kind, 5), 16).is_err());
            assert!(!s.active);
        }
    }

    #[test]
    fn controls_reject_expired_lease_and_stale_or_reversed_source_time() {
        for kind in [Kind::Button, Kind::Scroll, Kind::Key] {
            let mut s = ReceiverState::new(150);
            s.grant(9, 10);
            s.apply(frame(Kind::Arm, 1, 0.), 11).unwrap();
            assert!(s.apply(action(kind, 2), 161).is_err());
            assert!(!s.active);

            for (start, end) in [(0, 30_000_001), (2, 1)] {
                let mut s = ReceiverState::new(150);
                s.grant(9, 10);
                s.apply(frame(Kind::Arm, 1, 0.), 11).unwrap();
                let mut f = action(kind, 2);
                f.t0_ns = start;
                f.t1_ns = end;
                assert!(s.apply(f, 12).is_err());
            }
            let mut s = ReceiverState::new(150);
            s.grant(9, 10);
            s.apply(frame(Kind::Arm, 1, 0.), 11).unwrap();
            let mut boundary = action(kind, 2);
            boundary.t1_ns = 30_000_000;
            assert_eq!(s.apply(boundary, 12).unwrap(), None);
        }
    }

    #[test]
    fn controls_preserve_cumulative_movement_baseline() {
        let mut s = ReceiverState::new(150);
        s.grant(9, 10);
        s.apply(frame(Kind::Arm, 1, 100.), 11).unwrap();
        assert_eq!(
            s.apply(frame(Kind::Move, 2, 110.), 12).unwrap(),
            Some((10., 0.))
        );
        for (index, kind) in [Kind::Button, Kind::Scroll, Kind::Key]
            .into_iter()
            .enumerate()
        {
            assert_eq!(
                s.apply(action(kind, index as u64 + 3), 13 + index as u64)
                    .unwrap(),
                None
            );
        }
        assert_eq!(
            s.apply(frame(Kind::Move, 6, 120.), 16).unwrap(),
            Some((10., 0.))
        );
    }

    #[test]
    fn direct_invalid_payload_cannot_mutate_receiver_state() {
        for kind in [Kind::Button, Kind::Scroll, Kind::Key] {
            let mut s = ReceiverState::new(150);
            s.grant(9, 10);
            s.apply(frame(Kind::Arm, 1, 100.), 11).unwrap();
            let mut invalid = action(kind, 2);
            match kind {
                Kind::Button => invalid.code = 5,
                Kind::Scroll => invalid.wheel_y = 0,
                Kind::Key => invalid.code = 0,
                _ => unreachable!(),
            }
            assert!(s.apply(invalid, 12).is_err());
            assert!(s.active);
            // A malformed direct call has not consumed sequence 2.
            assert_eq!(s.apply(action(kind, 2), 12).unwrap(), None);
            assert_eq!(
                s.apply(frame(Kind::Move, 3, 105.), 13).unwrap(),
                Some((5., 0.))
            );
        }
        let mut s = ReceiverState::new(150);
        s.grant(9, 10);
        assert!(s.apply(frame(Kind::Arm, 1, f64::NAN), 11).is_err());
        assert!(!s.active);
        s.apply(frame(Kind::Arm, 1, 0.), 11).unwrap();
    }
}
