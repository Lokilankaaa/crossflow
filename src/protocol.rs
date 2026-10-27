use anyhow::{Result, bail, ensure};

pub const FRAME_LEN: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Lease = 1,
    Arm = 2,
    Move = 3,
    Pause = 4,
    Heartbeat = 5,
    Stop = 6,
}

#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub kind: Kind,
    pub seq: u64,
    pub t0_ns: u64,
    pub t1_ns: u64,
    pub lease: u64,
    pub x: f64,
    pub y: f64,
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
        }
    }
    pub fn encode(self) -> Result<[u8; FRAME_LEN]> {
        ensure!(
            self.x.is_finite() && self.y.is_finite(),
            "non-finite coordinates"
        );
        ensure!(
            self.x.abs() <= (1u64 << 48) as f64 && self.y.abs() <= (1u64 << 48) as f64,
            "coordinate bound"
        );
        let mut out = [0u8; FRAME_LEN];
        out[..4].copy_from_slice(b"CFL0");
        out[4] = 1;
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
        Ok(out)
    }
    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() == FRAME_LEN, "invalid frame length");
        ensure!(
            &data[..4] == b"CFL0" && data[4] == 1 && data[6..8] == [0, 0],
            "invalid header"
        );
        let kind = match data[5] {
            1 => Kind::Lease,
            2 => Kind::Arm,
            3 => Kind::Move,
            4 => Kind::Pause,
            5 => Kind::Heartbeat,
            6 => Kind::Stop,
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
        };
        frame.encode()?;
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
        for n in [0, 55, 57] {
            assert!(Frame::decode(&vec![0; n]).is_err());
        }
        let mut f = Frame::new(Kind::Move);
        f.x = f64::NAN;
        assert!(f.encode().is_err());
        let mut b = Frame::new(Kind::Arm).encode().unwrap();
        b[6] = 1;
        assert!(Frame::decode(&b).is_err());
    }
}
