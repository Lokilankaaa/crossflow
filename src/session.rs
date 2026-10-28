use crate::{
    input::{Button, InputAction, SourceInputQueue},
    metrics::Log,
    protocol::{Frame, Kind},
    security::{self, Identity, SecureChannel},
    state::{Motion, ReceiverState},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
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

/// IDs are local to this log and endpoint. Correlate the two endpoints using
/// their reversed local/peer socket addresses, not matching attempt numbers.
#[derive(Clone)]
struct AttemptTrace {
    id: u64,
    endpoint: &'static str,
    phase: &'static str,
    local: Option<SocketAddr>,
    peer: Option<SocketAddr>,
}
impl AttemptTrace {
    fn write(&self, log: &Log, event: &str, mut value: Value) {
        let fields = value
            .as_object_mut()
            .expect("diagnostic fields are objects");
        fields.insert("attempt_id".into(), self.id.into());
        fields.insert("endpoint".into(), self.endpoint.into());
        fields.insert("phase".into(), self.phase.into());
        fields.insert("local_address".into(), json!(self.local));
        fields.insert("peer_address".into(), json!(self.peer));
        log.write(event, value);
    }
}

/// Counts local queueing and full socket writes separately. Neither is a peer
/// acknowledgement, and a partially-written record is never counted complete.
#[derive(Default)]
struct TransportCounts {
    queued_records: u64,
    socket_completed_records: u64,
    queued_moves: u64,
    socket_completed_moves: u64,
    last_queued_seq: Option<u64>,
    last_socket_completed_seq: Option<u64>,
    last_queued_move_seq: Option<u64>,
    last_socket_completed_move_seq: Option<u64>,
    pending: Option<(Kind, u64)>,
}
impl TransportCounts {
    fn queued(&mut self, frame: Frame) {
        self.queued_records += 1;
        self.last_queued_seq = Some(frame.seq);
        if frame.kind == Kind::Move {
            self.queued_moves += 1;
            self.last_queued_move_seq = Some(frame.seq);
        }
        self.pending = Some((frame.kind, frame.seq));
    }
    fn completed(&mut self) {
        if let Some((kind, seq)) = self.pending.take() {
            self.socket_completed_records += 1;
            self.last_socket_completed_seq = Some(seq);
            if kind == Kind::Move {
                self.socket_completed_moves += 1;
                self.last_socket_completed_move_seq = Some(seq);
            }
        }
    }
    fn snapshot(&self, written: usize, total: usize, unread: usize) -> Value {
        json!({
            "completion":"local_socket_write_only", "peer_acknowledged":false,
            "queued_records":self.queued_records,
            "socket_completed_records":self.socket_completed_records,
            "queued_moves":self.queued_moves,
            "socket_completed_moves":self.socket_completed_moves,
            "last_queued_seq":self.last_queued_seq,
            "last_socket_completed_seq":self.last_socket_completed_seq,
            "last_queued_move_seq":self.last_queued_move_seq,
            "last_socket_completed_move_seq":self.last_socket_completed_move_seq,
            "pending_kind":self.pending.map(|(kind,_)|format!("{kind:?}")),
            "pending_seq":self.pending.map(|(_,seq)|seq),
            "pending_written_bytes":if self.pending.is_some(){written}else{0},
            "pending_total_bytes":if self.pending.is_some(){total}else{0},
            "unparsed_input_bytes":unread,
            "exit_policy":"discard_pending_without_flush"
        })
    }
}

/// One encrypted frame in flight; callers coalesce in shared state, not here.
struct Peer {
    socket: TcpStream,
    secure: SecureChannel,
    input: Vec<u8>,
    output: Vec<u8>,
    offset: usize,
    pending_since: Instant,
    counts: TransportCounts,
    summary: Option<(Log, AttemptTrace)>,
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
            counts: TransportCounts::default(),
            summary: None,
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
        self.counts.queued(f);
        Ok(())
    }
    fn pump(&mut self) -> Result<Vec<(Frame, Instant)>> {
        while self.pending() {
            match self.socket.write(&self.output[self.offset..]) {
                Ok(0) => bail!("peer disconnected during send"),
                Ok(n) => {
                    self.offset += n;
                    if !self.pending() {
                        self.counts.completed();
                    }
                }
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
                Ok(0) => bail!(
                    "peer disconnected; discarding {} unparsed input bytes",
                    self.input.len()
                ),
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
impl Drop for Peer {
    fn drop(&mut self) {
        // Runs on normal deadline, explicit stop, and transport error. Never
        // pump here: stopping must not replay the final queued movement.
        if let Some((log, trace)) = &self.summary {
            trace.write(
                log,
                "transport_summary",
                self.counts
                    .snapshot(self.offset, self.output.len(), self.input.len()),
            );
        }
    }
}

pub struct SendOptions<'a> {
    pub host: &'a str,
    pub bundle: &'a Path,
    pub seconds: f64,
    pub max_hz: u32,
    pub synthetic: bool,
}

enum SenderSource {
    Motion(Arc<Mutex<Motion>>),
    Ordered(Arc<Mutex<SourceInputQueue>>),
}
impl SenderSource {
    fn connected(&self, connected: bool) {
        match self {
            Self::Motion(motion) => motion.lock().unwrap().connected(connected),
            Self::Ordered(input) => {
                let mut input = input.lock().unwrap();
                if connected {
                    input.begin_session();
                } else {
                    input.end_session();
                }
            }
        }
    }

    fn pump(&self, peer: &mut Peer, epoch: u64, log: &Log) -> Result<Vec<(Frame, Instant)>> {
        match self {
            Self::Motion(_) => peer.pump(),
            Self::Ordered(input) => {
                // The socket is nonblocking. Keep the source lock across this
                // bounded pump so a pause cannot race a pending socket write.
                let mut input = input.lock().unwrap();
                ensure!(input.is_connected(), "local capture session ended");
                if let Err(reason) = input.validate_pending(log.ns()) {
                    log.write(
                        "input_queue_paused",
                        json!({"reason":format!("{reason:?}")}),
                    );
                }
                // Encryption consumes a Noise nonce even before socket write.
                // Cancelling a queued record therefore requires a new session;
                // never flush pre-pause input merely to preserve that nonce.
                // Pending controls carry no physical input and can finish
                // before the next epoch control (notably initial Pause -> Arm).
                let pending_control = matches!(
                    peer.counts.pending,
                    Some((Kind::Arm | Kind::Pause | Kind::Heartbeat | Kind::Stop, _))
                );
                ensure!(
                    !peer.pending() || input.control_epoch() == epoch || pending_control,
                    "input epoch changed with pending transport record; discard session"
                );
                peer.pump()
            }
        }
    }
}
impl Drop for SenderSource {
    fn drop(&mut self) {
        if let Self::Ordered(input) = self {
            // Covers setup errors and explicit stop, without replaying queued
            // events. Receiver session cleanup releases any delivered downs.
            input
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .end_session();
        }
    }
}

pub fn send(
    options: SendOptions<'_>,
    motion: Arc<Mutex<Motion>>,
    stop: Arc<AtomicBool>,
    log: Log,
) -> Result<()> {
    send_source(options, SenderSource::Motion(motion), stop, log)
}

/// Send explicitly armed, ordered physical inputs over the shared connection
/// engine. Capture must stay active while paused and use this Log's clock.
/// Preserve the original event timestamp even when it precedes acquiring the
/// source mutex; queue mutations remain locked. Sender polling timestamps are
/// sampled under that mutex. Synthetic movement is only supported by `send`.
pub fn send_ordered(
    options: SendOptions<'_>,
    input: Arc<Mutex<SourceInputQueue>>,
    stop: Arc<AtomicBool>,
    log: Log,
) -> Result<()> {
    send_source(options, SenderSource::Ordered(input), stop, log)
}

fn send_source(
    options: SendOptions<'_>,
    source: SenderSource,
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
    ensure!(
        !synthetic || matches!(&source, SenderSource::Motion(_)),
        "ordered input requires synthetic=false"
    );
    ensure!((60..=8000).contains(&max_hz), "max-hz must be 60..8000");
    let identity = Identity::load(bundle)?;
    let end = Instant::now() + Duration::from_secs_f64(seconds);
    let interval = Duration::from_secs_f64(1.0 / max_hz as f64);
    let mut connected_once = false;
    let mut attempt_id = 0u64;
    while !stop.load(Ordering::Relaxed) && Instant::now() < end {
        attempt_id += 1;
        let mut trace = AttemptTrace {
            id: attempt_id,
            endpoint: "sender",
            phase: "resolve",
            local: None,
            peer: None,
        };
        trace.write(&log, "connection_attempt", json!({"target":host}));
        let attempt = (|| -> Result<()> {
            let addr = host
                .to_socket_addrs()
                .context("Resolve receiver address")?
                .next()
                .context("no target address")?;
            trace.peer = Some(addr);
            trace.phase = "tcp_connect";
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
                .context("Connect receiver TCP socket")?;
            trace.local = Some(
                stream
                    .local_addr()
                    .context("Read sender local socket address")?,
            );
            trace.peer = Some(stream.peer_addr().context("Read receiver socket address")?);
            trace.write(&log, "tcp_connected", json!({}));
            trace.phase = "socket_setup";
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .context("Set handshake read timeout")?;
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .context("Set handshake write timeout")?;
            trace.phase = "handshake";
            trace.write(&log, "handshake_started", json!({}));
            let channel = security::handshake(&mut stream, &identity, true)?;
            trace.write(
                &log,
                "handshake_completed",
                json!({"completion":"local_only"}),
            );
            trace.phase = "transport_setup";
            let mut peer = Peer::new(stream, channel).context("Configure sender transport")?;
            trace.phase = "transport";
            peer.summary = Some((log.clone(), trace.clone()));
            connected_once = true;
            source.connected(true);
            trace.write(
                &log,
                "connected",
                json!({"target":host,"synthetic":synthetic,"requires_manual_arm":!synthetic}),
            );
            eprintln!(
                "Connected. {}",
                if synthetic {
                    "Synthetic test starting"
                } else {
                    "Movement paused. Ctrl+Alt+Space toggles; Ctrl+Alt+Esc stops."
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
                for (f, received_at) in source.pump(&mut peer, epoch, &log)? {
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
                    let SenderSource::Motion(motion) = &source else {
                        unreachable!()
                    };
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
                    match &source {
                        SenderSource::Motion(motion) => {
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
                                eprintln!("Source backlog discarded; Ctrl+Alt+Space to re-arm.");
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
                                    log.write("sent",json!({"seq":seq,"t0_ns":frame.t0_ns,"t1_ns":now,"captures_coalesced":state.captures-last_captures,"lease":lease,"completion":"queued_local"}));
                                    sent = Instant::now();
                                } else {
                                    log.write(
                                        "control",
                                        json!({"kind":format!("{kind:?}"),"seq":seq}),
                                    );
                                }
                                if matches!(kind, Kind::Arm | Kind::Move | Kind::Pause) {
                                    last_captures = state.captures;
                                    epoch = state.epoch;
                                    state.pending_since_ns = None;
                                }
                                last_heartbeat = Instant::now();
                            }
                        }
                        SenderSource::Ordered(input) => {
                            let mut input = input.lock().unwrap();
                            let now = log.ns();
                            if let Err(reason) = input.validate_pending(now) {
                                log.write(
                                    "input_queue_paused",
                                    json!({"reason":format!("{reason:?}")}),
                                );
                            }
                            let mut captures = 0;
                            let next = if input.control_epoch() != epoch {
                                // Each arm begins at zero even when captures arrived
                                // before this control frame. Receiver Arm releases
                                // old owned input before rebasing the new epoch.
                                Some(Frame::new(if input.is_active() {
                                    Kind::Arm
                                } else {
                                    Kind::Pause
                                }))
                            } else if input.is_active() && input.front_action().is_some() {
                                // Never skip a rate-limited Move to send a later
                                // button/key/wheel event out of order.
                                if matches!(input.front_action(), Some(InputAction::Move { .. }))
                                    && sent.elapsed() < interval
                                {
                                    None
                                } else {
                                    match input.pop(now) {
                                        Ok(Some(event)) => {
                                            captures = event.captures_coalesced;
                                            Some(Frame::from_input(event, now)?)
                                        }
                                        Ok(None) => None,
                                        Err(reason) => {
                                            log.write(
                                                "input_queue_paused",
                                                json!({"reason":format!("{reason:?}")}),
                                            );
                                            Some(Frame::new(Kind::Pause))
                                        }
                                    }
                                }
                            } else if last_heartbeat.elapsed() >= Duration::from_millis(40) {
                                Some(Frame::new(Kind::Heartbeat))
                            } else {
                                None
                            };
                            if let Some(mut frame) = next {
                                seq += 1;
                                frame.seq = seq;
                                frame.lease = lease;
                                if captures == 0 {
                                    frame.t0_ns = now;
                                    frame.t1_ns = now;
                                }
                                peer.queue(frame)?;
                                match frame.kind {
                                    Kind::Move => {
                                        log.write("sent",json!({"seq":seq,"t0_ns":frame.t0_ns,"t1_ns":now,"captures_coalesced":captures,"lease":lease,"completion":"queued_local"}));
                                        sent = Instant::now();
                                    }
                                    Kind::Button | Kind::Key | Kind::Scroll => {
                                        // Never log key usages, down/up payloads or
                                        // wheel values: only pipeline timing/kind.
                                        log.write("input_queued",json!({"kind":format!("{:?}",frame.kind),"seq":seq,"t0_ns":frame.t0_ns,"t1_ns":now,"completion":"queued_local"}));
                                    }
                                    _ => log.write(
                                        "control",
                                        json!({"kind":format!("{:?}",frame.kind),"seq":seq}),
                                    ),
                                }
                                epoch = input.control_epoch();
                                last_heartbeat = Instant::now();
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_micros(500));
            }
            Ok(())
        })();
        source.connected(false);
        if let Err(e) = attempt {
            trace.write(
                &log,
                "error",
                json!({"stage":"sender_session","message":format!("{e:#}")}),
            );
            eprintln!("Connection stopped: {e:#}. Reconnect stays paused.");
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

/// Same-machine worker timing, only for a successful acknowledged action.
/// The sink call includes ownership bookkeeping and the platform adapter, not
/// just CGEventPost. These are software intervals, never visible response time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct WorkerTiming {
    pub request_seq: u64,
    pub dispatch_ns: u64,
    pub sink_call_ns: u64,
    pub return_ns: u64,
    pub rpc_ns: u64,
    pub clock: &'static str,
}

pub trait MouseSink {
    /// Consume the latest action timing once, before any maintenance/control RPC.
    fn take_timing(&mut self) -> Option<WorkerTiming> {
        None
    }
    /// Called only by the receiving execution loop after its safety checks.
    /// An isolated adapter can renew its watchdog here; never from a timer thread.
    fn maintain(&mut self) -> Result<()> {
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        Ok(())
    }
    fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64)>;
    /// Releases (`down=false`) must bypass emergency/display guards that block
    /// new input. Errors must not contain keycodes, typed text or input payloads.
    fn button(&mut self, _button: Button, _down: bool) -> Result<()> {
        bail!("mouse buttons are not supported by this adapter")
    }
    /// Releases (`down=false`) must bypass emergency/display guards that block
    /// new input. Errors must not contain keycodes, typed text or input payloads.
    fn key(&mut self, _usage: u16, _down: bool) -> Result<()> {
        bail!("keyboard input is not supported by this adapter")
    }
    fn scroll(&mut self, _horizontal: i32, _vertical: i32) -> Result<()> {
        bail!("scroll input is not supported by this adapter")
    }
    /// Release only inputs owned by this remote session, even during emergency stop.
    fn release_owned(&mut self) -> Result<()> {
        Ok(())
    }
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
    fn button(&mut self, _button: Button, _down: bool) -> Result<()> {
        Ok(())
    }
    fn key(&mut self, _usage: u16, _down: bool) -> Result<()> {
        Ok(())
    }
    fn scroll(&mut self, _horizontal: i32, _vertical: i32) -> Result<()> {
        Ok(())
    }
}

/// Tracks attempted remote downs, including adapters that post then return an
/// error. Cleanup is explicit so failures can be reported, never hidden by Drop.
/// This cannot recover OS state after a forced process kill.
pub(crate) struct OwnedSink<'a> {
    sink: &'a mut dyn MouseSink,
    stop: &'a AtomicBool,
    buttons: BTreeSet<Button>,
    keys: BTreeSet<u16>,
}
impl<'a> OwnedSink<'a> {
    pub(crate) fn new(sink: &'a mut dyn MouseSink, stop: &'a AtomicBool) -> Self {
        Self {
            sink,
            stop,
            buttons: BTreeSet::new(),
            keys: BTreeSet::new(),
        }
    }
}
impl MouseSink for OwnedSink<'_> {
    fn take_timing(&mut self) -> Option<WorkerTiming> {
        self.sink.take_timing()
    }
    fn maintain(&mut self) -> Result<()> {
        check_local_stop(self)?;
        self.sink.maintain()
    }
    fn reset(&mut self) -> Result<()> {
        self.release_owned()?;
        self.sink.reset()
    }
    fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64)> {
        self.sink.move_by(dx, dy)
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        if down {
            ensure!(self.buttons.insert(button), "duplicate remote button down");
        } else {
            ensure!(
                self.buttons.contains(&button),
                "remote button up without owned down"
            );
        }
        self.sink.button(button, down)?;
        if !down {
            self.buttons.remove(&button);
        }
        Ok(())
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        if down {
            ensure!(self.keys.insert(usage), "duplicate remote key down");
        } else {
            ensure!(
                self.keys.contains(&usage),
                "remote key up without owned down"
            );
        }
        self.sink.key(usage, down)?;
        if !down {
            self.keys.remove(&usage);
        }
        Ok(())
    }
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        self.sink.scroll(horizontal, vertical)
    }
    fn emergency_pressed(&self) -> bool {
        self.stop.load(Ordering::Acquire) || self.sink.emergency_pressed()
    }
    fn release_owned(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        // Ordinary keys sort before modifiers. Attempt every release even if
        // one adapter call fails, retaining failed ownership for retry/reporting.
        for usage in self.keys.iter().copied().collect::<Vec<_>>() {
            match self.sink.key(usage, false) {
                Ok(()) => {
                    self.keys.remove(&usage);
                }
                Err(error) => failures.push(format!("key release: {error:#}")),
            }
        }
        for button in self.buttons.iter().copied().collect::<Vec<_>>() {
            match self.sink.button(button, false) {
                Ok(()) => {
                    self.buttons.remove(&button);
                }
                Err(error) => failures.push(format!("button release: {error:#}")),
            }
        }
        if let Err(error) = self.sink.release_owned() {
            failures.push(format!("adapter cleanup: {error:#}"));
        }
        ensure!(
            failures.is_empty(),
            "remote input cleanup failed: {}",
            failures.join("; ")
        );
        Ok(())
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
    // Maintenance can observe a stop between the loop's earlier check and an
    // adapter call. Preserve that cooperative stop instead of relabeling it as
    // an adapter failure; receive_with_stop must still finish normally.
    if error.is::<TerminalReceiverFailure>() {
        return error;
    }
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
    let worker_timing = sink.take_timing();
    log.write(
        "received",
        json!({
            "seq":group.last_seq,"first_seq":group.first_seq,
            "t2_ns":group.earliest_t2_ns,"t3_ns":t3,"t4_ns":t4,
            "x":x,"y":y,"lease":group.last_lease,"frames_coalesced":group.frames,
            "receive_timestamp_source":"user_space_batch_read_complete",
            "worker_timing":worker_timing
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
        // Lease validity uses processing time, not the earlier arrival stamp.
        let delta = state.apply(frame, log.ns())?;
        if frame.kind != Kind::Move {
            flush_moves(&mut pending, sink, log)?;
        }
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
        } else if matches!(frame.kind, Kind::Button | Kind::Key | Kind::Scroll) {
            check_local_stop(sink)?;
            let t3 = log.ns();
            match frame.kind {
                Kind::Button => {
                    sink.button(crate::protocol::button_from_code(frame.code)?, frame.down)
                }
                Kind::Key => sink.key(frame.code, frame.down),
                Kind::Scroll => sink.scroll(frame.wheel_x, frame.wheel_y),
                _ => unreachable!(),
            }
            .map_err(terminal_sink_failure)?;
            // Do not log key codes or typed content. Sequence and timing suffice
            // for transport evidence; tests use synthetic fixture actions.
            let t4 = log.ns();
            let worker_timing = sink.take_timing();
            log.write("input_applied", json!({"seq":frame.seq,"kind":format!("{:?}",frame.kind),"t2_ns":t2,"t3_ns":t3,"t4_ns":t4,"worker_timing":worker_timing}));
        } else {
            if frame.kind == Kind::Arm {
                sink.reset().map_err(terminal_sink_failure)?;
                log.write("armed", json!({"seq":frame.seq}));
            } else {
                if matches!(frame.kind, Kind::Pause | Kind::Stop) {
                    sink.release_owned().map_err(terminal_sink_failure)?;
                    log.write(
                        "input_released",
                        json!({"reason":format!("{:?}",frame.kind)}),
                    );
                }
                log.write(
                    "control_received",
                    json!({"kind":format!("{:?}",frame.kind),"seq":frame.seq}),
                );
            }
            if frame.kind == Kind::Stop {
                return Ok(true);
            }
        }
        if frame.kind != Kind::Pause {
            // Renew only after a validated frame has completed. A slow batch
            // must not starve the isolated input owner; stopping never renews.
            sink.maintain().map_err(terminal_sink_failure)?;
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
    receive_with_stop(address, bundle, sink, log, seconds, &AtomicBool::new(false))
}

/// A stop request follows the same explicit release path as local Escape.
/// No signal handler or process-global state is installed by the session API.
pub fn receive_with_stop(
    address: &str,
    bundle: &Path,
    sink: &mut dyn MouseSink,
    log: Log,
    seconds: f64,
    stop: &AtomicBool,
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
    let mut attempt_id = 0u64;
    while Instant::now() < end {
        if stop.load(Ordering::Acquire) || sink.emergency_pressed() {
            log.write("emergency_stop", json!({}));
            return Ok(());
        }
        sink.maintain().map_err(terminal_sink_failure)?;
        let (mut stream, address) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        attempt_id += 1;
        let mut trace = AttemptTrace {
            id: attempt_id,
            endpoint: "receiver",
            phase: "tcp_accept",
            local: stream.local_addr().ok(),
            peer: Some(address),
        };
        trace.write(&log, "tcp_accepted", json!({}));
        let mut owned = OwnedSink::new(sink, stop);
        let attempt = (|| -> Result<()> {
            check_local_stop(&owned)?;
            trace.phase = "socket_setup";
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .context("Set handshake read timeout")?;
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .context("Set handshake write timeout")?;
            trace.phase = "handshake";
            trace.write(&log, "handshake_started", json!({}));
            let channel = security::handshake(&mut stream, &identity, false)?;
            trace.write(
                &log,
                "handshake_completed",
                json!({"completion":"local_only"}),
            );
            trace.phase = "transport_setup";
            let mut peer = Peer::new(stream, channel).context("Configure receiver transport")?;
            trace.phase = "transport";
            let mut state = ReceiverState::new(LEASE_TTL_NS);
            let mut lease = 0;
            let mut last_grant = Instant::now() - LEASE_PERIOD;
            let mut last_input = Instant::now();
            trace.write(&log, "connected", json!({"peer":address.to_string()}));
            while Instant::now() < end {
                check_local_stop(&owned)?;
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
                if process_received_batch(frames, &mut state, &mut owned, &log)? {
                    return Ok(());
                }
                ensure!(
                    last_input.elapsed() < Duration::from_millis(250),
                    "sender inactive; session discarded"
                );
                owned.maintain().map_err(terminal_sink_failure)?;
                thread::sleep(Duration::from_micros(500));
            }
            Ok(())
        })();
        // Every accepted connection reaches cleanup, including transport,
        // validation, permission, emergency and normal-duration exits.
        let cleanup = owned.release_owned();
        drop(owned);
        if let Err(error) = cleanup {
            trace.write(&log, "error", json!({"stage":"receiver_cleanup","message":format!("{error:#}"),"session_error":attempt.as_ref().err().map(|e|format!("{e:#}"))}));
            return Err(terminal_sink_failure(error));
        }
        log.write("input_released", json!({"reason":"session_end"}));
        if let Err(e) = attempt {
            if let Some(terminal) = e.downcast_ref::<TerminalReceiverFailure>() {
                if terminal.emergency {
                    trace.write(&log, "emergency_stop", json!({"message":format!("{e:#}")}));
                    return Ok(());
                }
                trace.write(
                    &log,
                    "error",
                    json!({"stage":"receiver_terminal","message":format!("{e:#}")}),
                );
                return Err(e);
            }
            trace.write(
                &log,
                "error",
                json!({"stage":"receiver_session","message":format!("{e:#}")}),
            );
            eprintln!("Session closed: {e:#}");
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
        // Windows wall-clock resolution can give parallel tests equal times.
        static NEXT_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "crossflow-session-test-{}-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_LOG.fetch_add(1, Ordering::Relaxed)
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
    fn transport_accounting_distinguishes_partial_moves_from_completed_controls() {
        let mut counts = TransportCounts::default();
        counts.queued(frame(Kind::Arm, 1, 0.).0);
        counts.completed();
        counts.queued(frame(Kind::Move, 2, 1.).0);
        let partial = counts.snapshot(7, 74, 9);
        assert_eq!(partial["queued_records"], 2);
        assert_eq!(partial["socket_completed_records"], 1);
        assert_eq!(partial["queued_moves"], 1);
        assert_eq!(partial["socket_completed_moves"], 0);
        assert_eq!(partial["pending_seq"], 2);
        assert_eq!(partial["pending_kind"], "Move");
        assert_eq!(partial["pending_written_bytes"], 7);
        assert_eq!(partial["pending_total_bytes"], 74);
        assert_eq!(partial["unparsed_input_bytes"], 9);
        counts.completed();
        counts.completed(); // Repeated completion cannot count the same record twice.
        let done = counts.snapshot(74, 74, 0);
        assert_eq!(done["socket_completed_records"], 2);
        assert_eq!(done["socket_completed_moves"], 1);
        assert_eq!(done["last_socket_completed_move_seq"], 2);
        assert!(done["pending_seq"].is_null());
        assert_eq!(done["pending_total_bytes"], 0);
        assert_eq!(done["peer_acknowledged"], false);
    }

    /// Exercise real encrypted socket writes and both scope-exit paths without
    /// any input adapter. The last queued move must remain unsent on stop.
    #[test]
    #[ignore = "opens localhost sockets; run explicitly"]
    fn transport_summary_preserves_unsent_tail_on_normal_and_error_exit() -> Result<()> {
        for fail in [false, true] {
            let (writer, path) = logs();
            let identities = path.with_extension("identities");
            security::create_pair(&identities)?;
            let initiator = Identity::load(&identities.join("windows"))?;
            let responder = Identity::load(&identities.join("mac"))?;
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let worker = thread::spawn(move || -> Result<Peer> {
                let (mut socket, _) = listener.accept()?;
                socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                socket.set_write_timeout(Some(Duration::from_secs(2)))?;
                let secure = security::handshake(&mut socket, &responder, false)?;
                Peer::new(socket, secure)
            });
            let mut socket = TcpStream::connect(address)?;
            socket.set_read_timeout(Some(Duration::from_secs(2)))?;
            socket.set_write_timeout(Some(Duration::from_secs(2)))?;
            let secure = security::handshake(&mut socket, &initiator, true)?;
            let mut sender = Peer::new(socket, secure)?;
            let mut receiver = worker.join().expect("handshake worker panicked")?;
            sender.summary = Some((
                writer.log.clone(),
                AttemptTrace {
                    id: 1,
                    endpoint: "sender",
                    phase: "transport",
                    local: sender.socket.local_addr().ok(),
                    peer: sender.socket.peer_addr().ok(),
                },
            ));
            // A real fully-written Arm is counted separately from movement.
            sender.queue(frame(Kind::Arm, 1, 0.).0)?;
            sender.pump()?;
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut got_arm = false;
            while Instant::now() < deadline {
                let frames = receiver.pump()?;
                if !frames.is_empty() {
                    ensure!(frames.len() == 1 && frames[0].0.kind == Kind::Arm);
                    got_arm = true;
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            ensure!(got_arm, "receiver did not observe the completed control");
            let result = (move || -> Result<()> {
                sender.queue(frame(Kind::Move, 2, 1.).0)?;
                if fail {
                    bail!("test session failure after queueing")
                }
                Ok(()) // No pump: simulate the deadline/stop boundary.
            })();
            assert_eq!(result.is_err(), fail);
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut saw_eof = false;
            while Instant::now() < deadline {
                match receiver.pump() {
                    Ok(frames) => ensure!(frames.is_empty(), "queued tail reached receiver"),
                    Err(error) => {
                        ensure!(error.to_string().contains("peer disconnected"));
                        ensure!(receiver.input.is_empty(), "tail bytes arrived before EOF");
                        saw_eof = true;
                        break;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            ensure!(saw_eof, "sender socket did not close");
            writer.finish()?;
            let text = std::fs::read_to_string(&path)?;
            let summaries: Vec<Value> = text
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .filter(|value| value["event"] == "transport_summary")
                .collect();
            assert_eq!(summaries.len(), 1);
            let summary = &summaries[0];
            assert_eq!(summary["queued_records"], 2);
            assert_eq!(summary["socket_completed_records"], 1);
            assert_eq!(summary["queued_moves"], 1);
            assert_eq!(summary["socket_completed_moves"], 0);
            assert_eq!(summary["pending_seq"], 2);
            assert_eq!(summary["pending_written_bytes"], 0);
            assert_eq!(
                summary["pending_total_bytes"],
                2 + crate::protocol::FRAME_LEN + 16
            );
            assert_eq!(summary["attempt_id"], 1);
            std::fs::remove_file(path)?;
            std::fs::remove_dir_all(identities)?;
        }
        Ok(())
    }

    #[test]
    fn attempt_diagnostics_preserve_addresses_phase_and_error_chain() {
        let (writer, path) = logs();
        let trace = AttemptTrace {
            id: 3,
            endpoint: "sender",
            phase: "handshake",
            local: Some("127.0.0.1:50000".parse().unwrap()),
            peer: Some("127.0.0.1:24800".parse().unwrap()),
        };
        let error = anyhow::Error::new(std::io::Error::new(ErrorKind::TimedOut, "test deadline"))
            .context("Read handshake header")
            .context("Receive Noise response");
        trace.write(
            &writer.log,
            "error",
            json!({"stage":"sender_session","message":format!("{error:#}")}),
        );
        writer.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let event: Value = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|value| value["event"] == "error")
            .unwrap();
        assert_eq!(event["attempt_id"], 3);
        assert_eq!(event["phase"], "handshake");
        assert_eq!(event["endpoint"], "sender");
        assert_eq!(event["local_address"], "127.0.0.1:50000");
        assert_eq!(event["peer_address"], "127.0.0.1:24800");
        assert!(event["local_ns"].is_u64());
        assert_eq!(
            event["message"],
            "Receive Noise response: Read handshake header: test deadline"
        );
        std::fs::remove_file(path).unwrap();
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
        writer.finish().unwrap();
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
        writer.finish().unwrap();
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn worker_timings_follow_coalesced_moves_and_actions_before_maintenance() {
        #[derive(Default)]
        struct TimedSink {
            count: u64,
            latest: Option<WorkerTiming>,
        }
        impl TimedSink {
            fn called(&mut self) {
                self.count += 1;
                self.latest = Some(WorkerTiming {
                    request_seq: self.count,
                    dispatch_ns: 1,
                    sink_call_ns: self.count,
                    return_ns: 2,
                    rpc_ns: 3 + self.count,
                    clock: "mach_continuous_time_ns",
                });
            }
        }
        impl MouseSink for TimedSink {
            fn take_timing(&mut self) -> Option<WorkerTiming> {
                self.latest.take()
            }
            fn maintain(&mut self) -> Result<()> {
                self.latest = None;
                Ok(())
            }
            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                self.called();
                Ok((x, y))
            }
            fn key(&mut self, _: u16, _: bool) -> Result<()> {
                self.called();
                Ok(())
            }
        }
        let (writer, path) = logs();
        let mut state = state(&writer.log);
        let mut sink = TimedSink::default();
        let stop = AtomicBool::new(false);
        let mut owned = OwnedSink::new(&mut sink, &stop);
        let mut down = frame(Kind::Key, 4, 0.);
        down.0.code = 4;
        down.0.down = true;
        let mut up = frame(Kind::Key, 5, 0.);
        up.0.code = 4;
        let frames = vec![
            frame(Kind::Arm, 1, 0.),
            frame(Kind::Move, 2, 1.),
            frame(Kind::Move, 3, 3.),
            down,
            up,
            frame(Kind::Pause, 6, 3.),
        ];
        process_received_batch(frames, &mut state, &mut owned, &writer.log).unwrap();
        owned.release_owned().unwrap();
        writer.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let samples: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|v| v["event"] == "received" || v["event"] == "input_applied")
            .collect();
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0]["frames_coalesced"], 2);
        for (i, sample) in samples.iter().enumerate() {
            assert_eq!(sample["seq"], i as u64 + 3);
            assert_eq!(sample["worker_timing"]["request_seq"], i as u64 + 1);
        }
        let summary = crate::metrics::summarize(&path).unwrap();
        assert_eq!(summary["worker_timing"]["valid_records"], 3);
        assert_eq!(
            summary["worker_timing"]["by_clock"]["mach_continuous_time_ns"]["movement"]["rpc"]["samples"],
            1
        );
        assert_eq!(
            summary["worker_timing"]["by_clock"]["mach_continuous_time_ns"]["actions"]["rpc"]["samples"],
            2
        );
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
            writer.finish().unwrap();
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
        writer.finish().unwrap();
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
