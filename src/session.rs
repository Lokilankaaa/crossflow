use crate::{
    metrics::Log,
    protocol::{Frame, Kind},
    security::{self, Identity, SecureChannel},
    state::{Motion, ReceiverState},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream, ToSocketAddrs},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const LEASE_PERIOD: Duration = Duration::from_millis(25);
const LEASE_TTL_NS: u64 = 150_000_000;
const SEND_STALL: Duration = Duration::from_millis(100);

/// One encrypted frame in flight; callers coalesce in shared state, not here.
struct Peer {
    socket: TcpStream,
    secure: SecureChannel,
    input: Vec<u8>,
    output: Vec<u8>,
    offset: usize,
    pending_since: Instant,
}
impl Peer {
    fn new(socket: TcpStream, secure: SecureChannel) -> Result<Self> {
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            secure,
            input: vec![],
            output: vec![],
            offset: 0,
            pending_since: Instant::now(),
        })
    }
    fn pending(&self) -> bool {
        self.offset < self.output.len()
    }
    fn queue(&mut self, f: Frame) -> Result<()> {
        ensure!(!self.pending(), "outbound queue already occupied");
        let cipher = self.secure.encrypt(&f.encode()?)?;
        ensure!(cipher.len() <= 2048, "record too large");
        self.output.clear();
        self.output
            .extend_from_slice(&(cipher.len() as u16).to_be_bytes());
        self.output.extend_from_slice(&cipher);
        self.offset = 0;
        self.pending_since = Instant::now();
        Ok(())
    }
    fn pump(&mut self) -> Result<Vec<(Frame, Instant)>> {
        while self.pending() {
            match self.socket.write(&self.output[self.offset..]) {
                Ok(0) => bail!("peer disconnected during send"),
                Ok(n) => self.offset += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        ensure!(
            !self.pending() || self.pending_since.elapsed() < SEND_STALL,
            "send stalled; discard session rather than replay backlog"
        );
        let mut buffer = [0u8; 4096];
        for _ in 0..8 {
            match self.socket.read(&mut buffer) {
                Ok(0) => bail!("peer disconnected"),
                Ok(n) => self.input.extend_from_slice(&buffer[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e.into()),
            }
        }
        // All complete frames drained together share this user-space read
        // completion stamp. This is not a NIC/hardware arrival timestamp.
        let read_completed = Instant::now();
        ensure!(self.input.len() <= 65536, "receive backlog too large");
        let mut frames = vec![];
        let mut consumed = 0;
        while self.input.len() - consumed >= 2 {
            let len =
                u16::from_be_bytes(self.input[consumed..consumed + 2].try_into().unwrap()) as usize;
            ensure!(
                (16..=2048).contains(&len),
                "invalid encrypted record length"
            );
            if self.input.len() - consumed < 2 + len {
                break;
            }
            let plain = self
                .secure
                .decrypt(&self.input[consumed + 2..consumed + 2 + len])?;
            frames.push((Frame::decode(&plain)?, read_completed));
            consumed += 2 + len;
        }
        self.input.drain(..consumed);
        Ok(frames)
    }
}

pub struct SendOptions<'a> {
    pub host: &'a str,
    pub bundle: &'a Path,
    pub seconds: f64,
    pub max_hz: u32,
    pub synthetic: bool,
}

pub fn send(
    options: SendOptions<'_>,
    motion: Arc<Mutex<Motion>>,
    stop: Arc<AtomicBool>,
    log: Log,
) -> Result<()> {
    let SendOptions {
        host,
        bundle,
        seconds,
        max_hz,
        synthetic,
    } = options;
    ensure!((60..=8000).contains(&max_hz), "max-hz must be 60..8000");
    let identity = Identity::load(bundle)?;
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let interval = Duration::from_secs_f64(1.0 / max_hz as f64);
    let mut connected_once = false;
    while !stop.load(Ordering::Relaxed) && Instant::now() < end {
        let attempt = (|| -> Result<()> {
            let addr = host
                .to_socket_addrs()?
                .next()
                .context("no target address")?;
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.set_write_timeout(Some(Duration::from_secs(3)))?;
            let channel = security::handshake(&mut stream, &identity, true)?;
            let mut peer = Peer::new(stream, channel)?;
            connected_once = true;
            motion.lock().unwrap().connected(true);
            log.write(
                "connected",
                json!({"target":host,"synthetic":synthetic,"requires_manual_arm":!synthetic}),
            );
            eprintln!(
                "Connected. {}",
                if synthetic {
                    "Synthetic test starting"
                } else {
                    "Movement paused. Ctrl+Alt+F8 toggles; Ctrl+Alt+F9 stops."
                }
            );
            let mut lease = 0;
            let mut last_lease = Instant::now();
            let mut seq = 0;
            let mut epoch = u64::MAX;
            let mut last_captures = 0;
            let mut sent = Instant::now() - Duration::from_secs(1);
            let mut last_heartbeat = Instant::now();
            let began = Instant::now();
            let mut synthetic_position = (0.0, 0.0);
            let mut synth_armed = false;
            while !stop.load(Ordering::Relaxed) && Instant::now() < end {
                for (f, received_at) in peer.pump()? {
                    ensure!(f.kind == Kind::Lease, "receiver sent unexpected frame");
                    ensure!(f.lease > lease, "non-increasing receiver lease");
                    lease = f.lease;
                    last_lease = received_at;
                }
                ensure!(
                    last_lease.elapsed() < Duration::from_millis(150),
                    "receiver lease stream lost"
                );
                if synthetic && lease > 0 {
                    if !synth_armed {
                        motion.lock().unwrap().toggle();
                        synth_armed = true;
                    }
                    let phase = began.elapsed().as_secs_f64() * 2.0;
                    let pos = (phase.sin() * 100.0, phase.cos() * 100.0);
                    let mut state = motion.lock().unwrap();
                    state.add(
                        pos.0 - synthetic_position.0,
                        pos.1 - synthetic_position.1,
                        log.ns(),
                    );
                    synthetic_position = pos;
                }
                if lease > 0 && !peer.pending() {
                    // Snapshot and drain under the same lock. A capture after
                    // this drain will establish a fresh pending_since value,
                    // rather than inheriting the timestamp of already-sent data.
                    // queue() encrypts into memory only; socket I/O is outside
                    // this lock in pump().
                    let mut state = motion.lock().unwrap();
                    let now = log.ns();
                    if state.epoch == epoch
                        && state.active
                        && state
                            .pending_since_ns
                            .is_some_and(|t| now.saturating_sub(t) > 30_000_000)
                    {
                        let age = now - state.pending_since_ns.unwrap();
                        state.active = false;
                        state.epoch += 1;
                        state.pending_since_ns = None;
                        drop(state);
                        log.write("stale_source_discard", json!({"pending_age_ns":age}));
                        eprintln!("Source backlog discarded; Ctrl+Alt+F8 to re-arm.");
                        continue;
                    }
                    let kind = if state.epoch != epoch {
                        Some(if state.active { Kind::Arm } else { Kind::Pause })
                    } else if state.active
                        && state.captures != last_captures
                        && sent.elapsed() >= interval
                    {
                        Some(Kind::Move)
                    } else if last_heartbeat.elapsed() >= Duration::from_millis(40) {
                        Some(Kind::Heartbeat)
                    } else {
                        None
                    };
                    if let Some(kind) = kind {
                        seq += 1;
                        let mut frame = Frame::new(kind);
                        frame.seq = seq;
                        frame.lease = lease;
                        frame.x = state.x;
                        frame.y = state.y;
                        frame.t1_ns = now;
                        frame.t0_ns = if kind == Kind::Move {
                            state.pending_since_ns.unwrap_or(state.t0_ns)
                        } else {
                            now
                        };
                        peer.queue(frame)?;
                        if kind == Kind::Move {
                            log.write("sent",json!({"seq":seq,"t0_ns":frame.t0_ns,"t1_ns":now,"captures_coalesced":state.captures-last_captures,"lease":lease}));
                            sent = Instant::now();
                        } else {
                            log.write("control", json!({"kind":format!("{kind:?}"),"seq":seq}));
                        }
                        if matches!(kind, Kind::Arm | Kind::Move | Kind::Pause) {
                            last_captures = state.captures;
                            epoch = state.epoch;
                            state.pending_since_ns = None;
                        }
                        last_heartbeat = Instant::now();
                    }
                }
                thread::sleep(Duration::from_micros(500));
            }
            Ok(())
        })();
        motion.lock().unwrap().connected(false);
        if let Err(e) = attempt {
            log.write(
                "error",
                json!({"stage":"sender_session","message":e.to_string()}),
            );
            eprintln!("Connection stopped: {e}. Reconnect stays paused.");
        }
        if !stop.load(Ordering::Relaxed) && Instant::now() < end {
            thread::sleep(Duration::from_millis(500));
        }
    }
    stop.store(true, Ordering::Relaxed);
    ensure!(
        connected_once,
        "No authenticated receiver connection was established"
    );
    Ok(())
}

pub trait MouseSink {
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
    fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64)>;
    fn emergency_pressed(&self) -> bool {
        false
    }
}
#[derive(Default)]
pub struct DrySink {
    pub x: f64,
    pub y: f64,
    pub moves: u64,
}
impl MouseSink for DrySink {
    fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64)> {
        self.x += dx;
        self.y += dy;
        self.moves += 1;
        Ok((self.x, self.y))
    }
}

#[derive(Debug)]
struct TerminalReceiverFailure {
    message: String,
    emergency: bool,
}
impl std::fmt::Display for TerminalReceiverFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for TerminalReceiverFailure {}
fn terminal_sink_failure(error: anyhow::Error) -> anyhow::Error {
    TerminalReceiverFailure {
        message: format!("Input adapter stopped: {error}"),
        emergency: false,
    }
    .into()
}
fn check_local_stop(sink: &dyn MouseSink) -> Result<()> {
    if sink.emergency_pressed() {
        return Err(TerminalReceiverFailure {
            message: "Local emergency stop".into(),
            emergency: true,
        }
        .into());
    }
    Ok(())
}

/// Contiguous moves within a single control epoch are reduced to their net
/// cumulative displacement. No interpolation or delayed smoothing is added.
struct MoveGroup {
    dx: f64,
    dy: f64,
    first_seq: u64,
    last_seq: u64,
    frames: u64,
    earliest_t2_ns: u64,
    last_lease: u64,
}
impl MoveGroup {
    fn extend(&mut self, frame: Frame, delta: (f64, f64), t2_ns: u64) {
        self.dx += delta.0;
        self.dy += delta.1;
        self.last_seq = frame.seq;
        self.frames += 1;
        self.earliest_t2_ns = self.earliest_t2_ns.min(t2_ns);
        self.last_lease = frame.lease;
    }
}

fn flush_moves(pending: &mut Option<MoveGroup>, sink: &mut dyn MouseSink, log: &Log) -> Result<()> {
    let Some(group) = pending.take() else {
        return Ok(());
    };
    check_local_stop(sink)?;
    let t3 = log.ns();
    // Any adapter failure is terminal for the receiver, including an Escape
    // detected between our poll and its native post. Never silently relisten.
    let (x, y) = sink
        .move_by(group.dx, group.dy)
        .map_err(terminal_sink_failure)?;
    let t4 = log.ns();
    log.write(
        "received",
        json!({
            "seq":group.last_seq,"first_seq":group.first_seq,
            "t2_ns":group.earliest_t2_ns,"t3_ns":t3,"t4_ns":t4,
            "x":x,"y":y,"lease":group.last_lease,"frames_coalesced":group.frames,
            "receive_timestamp_source":"user_space_batch_read_complete"
        }),
    );
    Ok(())
}

/// Return true when the peer requests the end of this connection. Every frame
/// is validated in order even when its movement will be coalesced. A bad frame
/// discards pending movement; controls are never coalesced with adjacent moves.
fn process_received_batch(
    frames: Vec<(Frame, Instant)>,
    state: &mut ReceiverState,
    sink: &mut dyn MouseSink,
    log: &Log,
) -> Result<bool> {
    let mut pending: Option<MoveGroup> = None;
    for (frame, received_at) in frames {
        check_local_stop(sink)?;
        let t2 = log.at(received_at);
        if frame.kind != Kind::Move {
            flush_moves(&mut pending, sink, log)?;
        }
        // Lease validity uses processing time, not the earlier arrival stamp.
        let delta = state.apply(frame, log.ns())?;
        if let Some(delta) = delta {
            if let Some(group) = pending.as_mut() {
                group.extend(frame, delta, t2);
            } else {
                pending = Some(MoveGroup {
                    dx: delta.0,
                    dy: delta.1,
                    first_seq: frame.seq,
                    last_seq: frame.seq,
                    frames: 1,
                    earliest_t2_ns: t2,
                    last_lease: frame.lease,
                });
            }
        } else {
            if frame.kind == Kind::Arm {
                sink.reset().map_err(terminal_sink_failure)?;
                log.write("armed", json!({"seq":frame.seq}));
            } else {
                log.write(
                    "control_received",
                    json!({"kind":format!("{:?}",frame.kind),"seq":frame.seq}),
                );
            }
            if frame.kind == Kind::Stop {
                return Ok(true);
            }
        }
    }
    flush_moves(&mut pending, sink, log)?;
    Ok(false)
}

pub fn receive(
    address: &str,
    bundle: &Path,
    sink: &mut dyn MouseSink,
    log: Log,
    seconds: f64,
) -> Result<()> {
    let identity = Identity::load(bundle)?;
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    eprintln!(
        "Listening on {}. Press Escape on Mac to stop input; Ctrl+C exits.",
        listener.local_addr()?
    );
    log.write(
        "listening",
        json!({"address":listener.local_addr()?.to_string()}),
    );
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    while Instant::now() < end {
        if sink.emergency_pressed() {
            log.write("emergency_stop", json!({}));
            return Ok(());
        }
        let (mut stream, address) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let attempt = (|| -> Result<()> {
            stream.set_read_timeout(Some(Duration::from_secs(3)))?;
            stream.set_write_timeout(Some(Duration::from_secs(3)))?;
            let channel = security::handshake(&mut stream, &identity, false)?;
            let mut peer = Peer::new(stream, channel)?;
            let mut state = ReceiverState::new(LEASE_TTL_NS);
            let mut lease = 0;
            let mut last_grant = Instant::now() - LEASE_PERIOD;
            let mut last_input = Instant::now();
            log.write("connected", json!({"peer":address.to_string()}));
            while Instant::now() < end {
                check_local_stop(sink)?;
                if !peer.pending() && last_grant.elapsed() >= LEASE_PERIOD {
                    lease += 1;
                    let now = log.ns();
                    state.grant(lease, now);
                    let mut f = Frame::new(Kind::Lease);
                    f.lease = lease;
                    f.t0_ns = now;
                    peer.queue(f)?;
                    last_grant = Instant::now();
                }
                let frames = peer.pump()?;
                if !frames.is_empty() {
                    last_input = Instant::now();
                }
                if process_received_batch(frames, &mut state, sink, &log)? {
                    return Ok(());
                }
                ensure!(
                    last_input.elapsed() < Duration::from_millis(250),
                    "sender inactive; session discarded"
                );
                thread::sleep(Duration::from_micros(500));
            }
            Ok(())
        })();
        if let Err(e) = attempt {
            if let Some(terminal) = e.downcast_ref::<TerminalReceiverFailure>() {
                if terminal.emergency {
                    log.write("emergency_stop", json!({"message":e.to_string()}));
                    return Ok(());
                }
                log.write(
                    "error",
                    json!({"stage":"receiver_terminal","message":e.to_string()}),
                );
                return Err(e);
            }
            log.write(
                "error",
                json!({"stage":"receiver_session","message":e.to_string()}),
            );
            eprintln!("Session closed: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::LogWriter;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Default)]
    struct RecordingSink {
        moves: Vec<(f64, f64)>,
        resets: usize,
        fail_move: bool,
        fail_reset: bool,
        emergency: bool,
    }
    impl MouseSink for RecordingSink {
        fn reset(&mut self) -> Result<()> {
            if self.fail_reset {
                bail!("test reset failure")
            }
            self.resets += 1;
            Ok(())
        }
        fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
            if self.fail_move {
                bail!("test native stop race")
            }
            self.moves.push((x, y));
            Ok((x, y))
        }
        fn emergency_pressed(&self) -> bool {
            self.emergency
        }
    }
    fn logs() -> (LogWriter, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "crossflow-session-test-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (LogWriter::new(&path, json!({"test":true})).unwrap(), path)
    }
    fn frame(kind: Kind, seq: u64, x: f64) -> (Frame, Instant) {
        let mut f = Frame::new(kind);
        f.seq = seq;
        f.x = x;
        f.lease = 1;
        (f, Instant::now())
    }
    fn state(log: &Log) -> ReceiverState {
        let mut state = ReceiverState::new(1_000_000_000);
        state.grant(1, log.ns());
        state
    }

    #[test]
    fn contiguous_moves_coalesce_without_crossing_controls() {
        let (writer, path) = logs();
        let mut state = state(&writer.log);
        let mut sink = RecordingSink::default();
        let frames = vec![
            frame(Kind::Arm, 1, 0.),
            frame(Kind::Move, 2, 1.),
            frame(Kind::Move, 3, 3.),
            frame(Kind::Heartbeat, 4, 3.),
            frame(Kind::Move, 5, 5.),
            frame(Kind::Pause, 6, 5.),
            frame(Kind::Arm, 7, 100.),
            frame(Kind::Move, 8, 101.),
        ];
        assert!(!process_received_batch(frames, &mut state, &mut sink, &writer.log).unwrap());
        assert_eq!(sink.moves, vec![(3., 0.), (2., 0.), (1., 0.)]);
        assert_eq!(sink.resets, 2);
        writer.finish();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn invalid_frame_discards_unposted_group() {
        let (writer, path) = logs();
        let mut state = state(&writer.log);
        let mut sink = RecordingSink::default();
        let frames = vec![
            frame(Kind::Arm, 1, 0.),
            frame(Kind::Move, 2, 1.),
            frame(Kind::Move, 2, 2.),
        ];
        assert!(process_received_batch(frames, &mut state, &mut sink, &writer.log).is_err());
        assert!(sink.moves.is_empty());
        writer.finish();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn native_failure_is_terminal_instead_of_relisten() {
        for reset in [false, true] {
            let (writer, path) = logs();
            let mut state = state(&writer.log);
            let mut sink = RecordingSink {
                fail_move: !reset,
                fail_reset: reset,
                ..Default::default()
            };
            let error = process_received_batch(
                vec![frame(Kind::Arm, 1, 0.), frame(Kind::Move, 2, 1.)],
                &mut state,
                &mut sink,
                &writer.log,
            )
            .unwrap_err();
            assert!(error.downcast_ref::<TerminalReceiverFailure>().is_some());
            writer.finish();
            std::fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn local_stop_discards_pending_movement() {
        let (writer, path) = logs();
        let mut state = state(&writer.log);
        let mut sink = RecordingSink {
            emergency: true,
            ..Default::default()
        };
        let error = process_received_batch(
            vec![frame(Kind::Arm, 1, 0.), frame(Kind::Move, 2, 1.)],
            &mut state,
            &mut sink,
            &writer.log,
        )
        .unwrap_err();
        assert!(
            error
                .downcast_ref::<TerminalReceiverFailure>()
                .unwrap()
                .emergency
        );
        assert!(sink.moves.is_empty());
        writer.finish();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn group_keeps_earliest_arrival_and_latest_sequence() {
        let mut group = MoveGroup {
            dx: 1.,
            dy: 2.,
            first_seq: 2,
            last_seq: 2,
            frames: 1,
            earliest_t2_ns: 50,
            last_lease: 1,
        };
        let (frame, _) = frame(Kind::Move, 3, 4.);
        group.extend(frame, (3., 4.), 70);
        assert_eq!((group.dx, group.dy), (4., 6.));
        assert_eq!(group.earliest_t2_ns, 50);
        assert_eq!((group.first_seq, group.last_seq, group.frames), (2, 3, 2));
    }
}
