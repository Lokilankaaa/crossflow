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
        if f.kind == Kind::Heartbeat {
            return Ok(None);
        }
        ensure!(
            f.kind == Kind::Move && self.active,
            "movement before explicit arm"
        );
        ensure!(f.t1_ns - f.t0_ns <= 30_000_000, "source movement stale");
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
}
