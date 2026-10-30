use crate::{
    discovery::{self, Advertisement, DiscoveryConfig, DiscoveryHandle, DiscoveryState},
    input::{Button, InputAction, SourceInputQueue},
    metrics::Log,
    protocol::{Frame, Kind},
    security::{self, Identity, SecureChannel},
    state::{Motion, ReceiverState},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
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
const DISCOVERED_CONNECT_BUDGET: Duration = Duration::from_millis(250);
const DISCOVERED_HANDSHAKE_BUDGET: Duration = Duration::from_millis(500);

/// Platform-native display geometry; authenticated metadata, never a trust grant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDisplay {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub fn validate_displays(displays: &[NativeDisplay]) -> Result<()> {
    ensure!(
        !displays.is_empty() && displays.len() <= 32,
        "expected 1..32 native displays"
    );
    let mut ids = BTreeSet::new();
    for display in displays {
        ensure!(ids.insert(display.id), "duplicate native display ID");
        ensure!(
            [display.x, display.y, display.width, display.height]
                .iter()
                .all(|n| n.is_finite() && n.abs() <= (1u64 << 48) as f64)
                && display.width > 0.0
                && display.height > 0.0
                && display.x + display.width > display.x
                && display.y + display.height > display.y,
            "invalid native display bounds"
        );
    }
    Ok(())
}

fn display_frames(displays: &[NativeDisplay]) -> Result<Vec<Frame>> {
    validate_displays(displays)?;
    let mut frames = Vec::with_capacity(displays.len() * 2 + 1);
    for (index, display) in displays.iter().enumerate() {
        let mut origin = Frame::new(Kind::DisplayOrigin);
        origin.code = index as u16;
        origin.seq = u64::from(display.id);
        origin.x = display.x;
        origin.y = display.y;
        let mut size = origin;
        size.kind = Kind::DisplaySize;
        size.x = display.width;
        size.y = display.height;
        frames.extend([origin, size]);
    }
    let mut end = Frame::new(Kind::DisplayEnd);
    end.code = displays.len() as u16;
    frames.push(end);
    Ok(frames)
}

#[derive(Default)]
struct DisplayReply {
    displays: Vec<NativeDisplay>,
    origin: Option<Frame>,
    ended: bool,
}
impl DisplayReply {
    fn accept(&mut self, frame: Frame) -> Result<()> {
        frame.encode()?;
        ensure!(!self.ended, "data after display terminator");
        match frame.kind {
            Kind::Lease => {}
            Kind::DisplayOrigin => {
                ensure!(
                    self.origin.is_none()
                        && usize::from(frame.code) == self.displays.len()
                        && self.displays.len() < 32,
                    "display origin out of order"
                );
                self.origin = Some(frame);
            }
            Kind::DisplaySize => {
                let origin = self.origin.take().context("display size without origin")?;
                ensure!(
                    origin.code == frame.code && origin.seq == frame.seq,
                    "display identity changed"
                );
                self.displays.push(NativeDisplay {
                    id: u32::try_from(frame.seq)?,
                    x: origin.x,
                    y: origin.y,
                    width: frame.x,
                    height: frame.y,
                });
            }
            Kind::DisplayEnd => {
                ensure!(
                    self.origin.is_none() && usize::from(frame.code) == self.displays.len(),
                    "incomplete display inventory"
                );
                validate_displays(&self.displays)?;
                self.ended = true;
            }
            _ => bail!("unexpected display response"),
        }
        Ok(())
    }
}

/// Probe display geometry over pinned-identity authentication. The independent
/// connection never arms input. A normal session must be established afterwards.
pub fn query_displays(
    host: &str,
    bundle: &Path,
    stop: &AtomicBool,
    log: &Log,
    discover: bool,
) -> Result<Vec<NativeDisplay>> {
    let identity = Identity::load(bundle)?;
    let mut routes = if discover {
        Some(crate::routes::LanRoutes::new(
            host.parse().context("saved endpoint must be numeric")?,
        ))
    } else {
        None
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = anyhow::anyhow!("display query cancelled");
    while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
        let attempt = (|| -> Result<Vec<NativeDisplay>> {
            let address = if let Some(routes) = &mut routes {
                routes.next_endpoint(log)
            } else {
                host.to_socket_addrs()?
                    .next()
                    .context("no display query address")?
            };
            let mut socket = TcpStream::connect_timeout(
                &address,
                sender_phase_budget(stop, deadline, Duration::from_millis(250))?,
            )?;
            let budget = sender_phase_budget(stop, deadline, Duration::from_secs(1))?;
            socket.set_read_timeout(Some(budget))?;
            socket.set_write_timeout(Some(budget))?;
            let channel = security::handshake(&mut socket, &identity, true)?;
            let mut peer = Peer::new(socket, channel)?;
            let mut query = Frame::new(Kind::DisplayQuery);
            query.seq = 1;
            peer.queue(query)?;
            let mut reply = DisplayReply::default();
            let response_deadline = deadline.min(Instant::now() + Duration::from_secs(2));
            while !stop.load(Ordering::Acquire) && Instant::now() < response_deadline {
                for (frame, _) in peer.pump()? {
                    reply.accept(frame)?;
                }
                if reply.ended {
                    ensure!(!peer.pending(), "query response before request completion");
                    let mut finish = Frame::new(Kind::Stop);
                    finish.seq = 2;
                    peer.queue(finish)?;
                    // Keep the socket open until the receiver consumes Stop.
                    // Otherwise EOF can deliberately discard unparsed input.
                    let close_deadline = deadline.min(Instant::now() + Duration::from_millis(250));
                    while !stop.load(Ordering::Acquire) && Instant::now() < close_deadline {
                        match peer.pump() {
                            Err(_) => break,
                            Ok(frames) => {
                                ensure!(frames.is_empty(), "unexpected display close response")
                            }
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                    return Ok(reply.displays);
                }
                thread::sleep(Duration::from_millis(1));
            }
            bail!("display query timed out or stopped")
        })();
        match attempt {
            Ok(displays) => return Ok(displays),
            Err(error) => last_error = error,
        }
        wait_sender_retry(stop, deadline);
    }
    Err(last_error.context("query authenticated peer displays"))
}

fn serve_display_query(peer: &mut Peer, sink: &mut dyn MouseSink, end: Instant) -> Result<()> {
    let frames = display_frames(&sink.displays()?)?;
    let deadline = end.min(Instant::now() + Duration::from_secs(2));
    for frame in frames {
        check_local_stop(sink)?;
        ensure!(!peer.pending(), "display reply overlaps pending data");
        peer.queue(frame)?;
        while peer.pending() {
            ensure!(Instant::now() < deadline, "display response timed out");
            check_local_stop(sink)?;
            let inbound = peer.pump()?;
            if frame.kind == Kind::DisplayEnd && !inbound.is_empty() {
                ensure!(
                    inbound.len() == 1 && inbound[0].0.kind == Kind::Stop && inbound[0].0.seq == 2,
                    "display query must end without input"
                );
                return Ok(());
            }
            ensure!(
                inbound.is_empty(),
                "unexpected input during display response"
            );
            if peer.pending() {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    while Instant::now() < deadline {
        check_local_stop(sink)?;
        let frames = peer.pump()?;
        if !frames.is_empty() {
            ensure!(
                frames.len() == 1 && frames[0].0.kind == Kind::Stop && frames[0].0.seq == 2,
                "display query must end without input"
            );
            return Ok(());
        }
        thread::sleep(Duration::from_millis(1));
    }
    bail!("display query close timed out")
}

fn sender_phase_budget(stop: &AtomicBool, end: Instant, maximum: Duration) -> Result<Duration> {
    ensure!(
        !stop.load(Ordering::Acquire),
        "sender stopped before next phase"
    );
    let remaining = end
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .context("sender session deadline reached")?;
    Ok(remaining.min(maximum))
}

fn wait_sender_retry(stop: &AtomicBool, end: Instant) {
    let retry_end = end.min(Instant::now() + Duration::from_millis(500));
    while !stop.load(Ordering::Acquire) {
        let Some(remaining) = retry_end.checked_duration_since(Instant::now()) else {
            break;
        };
        thread::sleep(remaining.min(Duration::from_millis(20)));
    }
}

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
    send_source(options, SenderSource::Motion(motion), stop, log, None)
}

/// Retry a previously authorized peer using bounded LAN routing hints. Every
/// endpoint still authenticates with the same pinned Identity; discovery never
/// grants trust or arms input. Discovery work stays outside the input loop.
pub fn send_discovered(
    options: SendOptions<'_>,
    motion: Arc<Mutex<Motion>>,
    stop: Arc<AtomicBool>,
    log: Log,
) -> Result<()> {
    ensure!(!options.synthetic, "discovery routing requires manual arm");
    let saved = options
        .host
        .parse()
        .context("Saved peer endpoint must be numeric")?;
    send_source(
        options,
        SenderSource::Motion(motion),
        stop,
        log,
        Some(crate::routes::LanRoutes::new(saved)),
    )
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
    send_source(options, SenderSource::Ordered(input), stop, log, None)
}

/// Ordered inputs with the same pinned-identity LAN route recovery as movement.
pub fn send_ordered_discovered(
    options: SendOptions<'_>,
    input: Arc<Mutex<SourceInputQueue>>,
    stop: Arc<AtomicBool>,
    log: Log,
) -> Result<()> {
    ensure!(!options.synthetic, "discovery routing requires manual arm");
    let saved = options
        .host
        .parse()
        .context("Saved peer endpoint must be numeric")?;
    send_source(
        options,
        SenderSource::Ordered(input),
        stop,
        log,
        Some(crate::routes::LanRoutes::new(saved)),
    )
}

fn send_source(
    options: SendOptions<'_>,
    source: SenderSource,
    stop: Arc<AtomicBool>,
    log: Log,
    mut routes: Option<crate::routes::LanRoutes>,
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
    ensure!(
        seconds.is_finite() && seconds > 0.0,
        "seconds must be finite and >0"
    );
    let identity = Identity::load(bundle)?;
    let duration =
        Duration::try_from_secs_f64(seconds).context("seconds exceeds duration range")?;
    let end = Instant::now()
        .checked_add(duration)
        .context("session deadline exceeds clock range")?;
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
        trace.write(&log, "connection_attempt", json!({"saved_target":host}));
        let attempt = (|| -> Result<()> {
            sender_phase_budget(&stop, end, DISCOVERED_CONNECT_BUDGET)?;
            let addr = if let Some(routes) = routes.as_mut() {
                routes.next_endpoint(&log)
            } else {
                host.to_socket_addrs()
                    .context("Resolve receiver address")?
                    .next()
                    .context("no target address")?
            };
            trace.peer = Some(addr);
            trace.phase = "tcp_connect";
            trace.write(
                &log,
                "target_selected",
                json!({"target":addr,"discovery_enabled":routes.is_some()}),
            );
            let connect_budget = sender_phase_budget(
                &stop,
                end,
                if routes.is_some() {
                    DISCOVERED_CONNECT_BUDGET
                } else {
                    Duration::from_secs(2)
                },
            )?;
            let mut stream = TcpStream::connect_timeout(&addr, connect_budget)
                .context("Connect receiver TCP socket")?;
            let handshake_budget = sender_phase_budget(
                &stop,
                end,
                if routes.is_some() {
                    DISCOVERED_HANDSHAKE_BUDGET
                } else {
                    Duration::from_secs(3)
                },
            )?;
            trace.local = Some(
                stream
                    .local_addr()
                    .context("Read sender local socket address")?,
            );
            trace.peer = Some(stream.peer_addr().context("Read receiver socket address")?);
            trace.write(&log, "tcp_connected", json!({}));
            trace.phase = "socket_setup";
            stream
                .set_read_timeout(Some(handshake_budget))
                .context("Set handshake read timeout")?;
            stream
                .set_write_timeout(Some(handshake_budget))
                .context("Set handshake write timeout")?;
            trace.phase = "handshake";
            trace.write(&log, "handshake_started", json!({}));
            let channel = security::handshake(&mut stream, &identity, true)?;
            sender_phase_budget(&stop, end, DISCOVERED_HANDSHAKE_BUDGET)?;
            trace.write(
                &log,
                "handshake_completed",
                json!({"completion":"local_only"}),
            );
            trace.phase = "transport_setup";
            let mut peer = Peer::new(stream, channel).context("Configure sender transport")?;
            sender_phase_budget(&stop, end, DISCOVERED_HANDSHAKE_BUDGET)?;
            if let Some(routes) = routes.as_mut() {
                routes.authenticated(addr);
            }
            trace.phase = "transport";
            peer.summary = Some((log.clone(), trace.clone()));
            connected_once = true;
            source.connected(true);
            trace.write(
                &log,
                "connected",
                json!({"target":addr,"synthetic":synthetic,"requires_manual_arm":!synthetic}),
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
                                if matches!(
                                    input.front_action(),
                                    Some(InputAction::Move { .. } | InputAction::Position { .. })
                                ) && sent.elapsed() < interval
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
                                    Kind::Position | Kind::Button | Kind::Key | Kind::Scroll => {
                                        // Never log key usages, down/up payloads or
                                        // wheel values: only pipeline timing/kind.
                                        log.write("input_queued",json!({"kind":format!("{:?}",frame.kind),"seq":seq,"t0_ns":frame.t0_ns,"t1_ns":now,"completion":"queued_local"}));
                                        if frame.kind == Kind::Position {
                                            sent = Instant::now();
                                        }
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
            wait_sender_retry(&stop, end);
        }
    }
    stop.store(true, Ordering::Release);
    let cleanup = if let Some(routes) = routes.as_mut() {
        let result = routes.finish();
        if let Err(error) = &result {
            log.write(
                "error",
                json!({"stage":"sender_discovery_cleanup","message":format!("{error:#}")}),
            );
        }
        result.context("Confirm discovery shutdown")
    } else {
        Ok(())
    };
    ensure!(
        connected_once,
        "No authenticated receiver connection was established"
    );
    cleanup
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
    fn displays(&self) -> Result<Vec<NativeDisplay>> {
        bail!("display inventory is not supported by this adapter")
    }
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
    /// Absolute target-native desktop position. Native adapters validate that
    /// the point belongs to a currently usable display before posting input.
    fn position(&mut self, _x: f64, _y: f64, _display_id: u32) -> Result<(f64, f64)> {
        bail!("absolute positioning is not supported by this adapter")
    }
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
    fn position(&mut self, x: f64, y: f64, _display_id: u32) -> Result<(f64, f64)> {
        self.x = x;
        self.y = y;
        self.moves += 1;
        Ok((x, y))
    }
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
    fn displays(&self) -> Result<Vec<NativeDisplay>> {
        self.sink.displays()
    }
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
    fn position(&mut self, x: f64, y: f64, display_id: u32) -> Result<(f64, f64)> {
        self.sink.position(x, y, display_id)
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
    absolute: bool,
    display_id: u32,
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
        if self.absolute {
            self.dx = delta.0;
            self.dy = delta.1;
        } else {
            self.dx += delta.0;
            self.dy += delta.1;
        }
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
    let (x, y) = if group.absolute {
        sink.position(group.dx, group.dy, group.display_id)
    } else {
        sink.move_by(group.dx, group.dy)
    }
    .map_err(terminal_sink_failure)?;
    let t4 = log.ns();
    let worker_timing = sink.take_timing();
    if group.absolute {
        log.write(
            "input_applied",
            json!({"seq":group.last_seq,"first_seq":group.first_seq,
            "frames_coalesced":group.frames,"kind":"Position","t2_ns":group.earliest_t2_ns,
            "t3_ns":t3,"t4_ns":t4,"worker_timing":worker_timing}),
        );
    } else {
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
    }
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
        if !matches!(frame.kind, Kind::Move | Kind::Position)
            || pending.as_ref().is_some_and(|group| {
                group.absolute != (frame.kind == Kind::Position)
                    || (group.absolute && group.display_id != frame.display_id)
            })
        {
            flush_moves(&mut pending, sink, log)?;
        }
        let coordinates = if frame.kind == Kind::Position {
            Some((frame.x, frame.y))
        } else {
            delta
        };
        if let Some(delta) = coordinates {
            if let Some(group) = pending.as_mut() {
                group.extend(frame, delta, t2);
            } else {
                pending = Some(MoveGroup {
                    absolute: frame.kind == Kind::Position,
                    display_id: frame.display_id,
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
    receive_with_discovery(address, bundle, sink, log, seconds, stop, None)
}

/// Receive on an already-bound socket, including an OS-assigned ephemeral port.
/// The caller owns the listener and any discovery advertisement for its actual
/// local address, and must keep both alive until this call returns. This API
/// starts no discovery worker and reuses the normal authenticated receive loop.
pub fn receive_on_listener(
    listener: &TcpListener,
    bundle: &Path,
    sink: &mut dyn MouseSink,
    log: Log,
    seconds: f64,
    stop: &AtomicBool,
) -> Result<()> {
    ensure!(
        seconds.is_finite() && seconds > 0.0,
        "seconds must be finite and >0"
    );
    let identity = Identity::load(bundle)?;
    listener.set_nonblocking(true)?;
    log.write(
        "listening",
        json!({"address":listener.local_addr()?.to_string()}),
    );
    receive_listener_loop(listener, &identity, sink, log, seconds, stop, None, None)
}

/// Nonblocking lifecycle notifications for a native client supervisor.
/// These callbacks describe the receiver only; they do not imply worker,
/// discovery or log cleanup has completed. Implementations must not panic,
/// perform I/O or wait for the UI, since callbacks run on the input thread.
pub trait ReceiveObserver: Send + Sync {
    fn listening(&self, address: SocketAddr);
    fn connected(&self, connected: bool);
    fn enabled(&self, enabled: bool);
}

#[derive(Clone, Debug, Default)]
pub struct ReceiveDiscoveryOptions {
    /// Explicit public label. None uses a platform label, never an OS hostname.
    pub device_name: Option<String>,
}

impl ReceiveDiscoveryOptions {
    fn advertisement(&self, bound: SocketAddr) -> Result<Advertisement> {
        Ok(Advertisement {
            device_id: discovery::new_public_id()?,
            friendly_name: self
                .device_name
                .clone()
                .unwrap_or_else(|| discovery::default_device_name().into()),
            platform: std::env::consts::OS.into(),
            protocol: u16::from(crate::protocol::WIRE_VERSION),
            port: bound.port(),
            listen_ip: bound.ip(),
        })
    }
}

/// Optional unauthenticated advertisement belongs to this bound listener only.
/// Existing session callers remain undiscoverable unless they explicitly opt in.
pub fn receive_with_discovery(
    address: &str,
    bundle: &Path,
    sink: &mut dyn MouseSink,
    log: Log,
    seconds: f64,
    stop: &AtomicBool,
    options: Option<&ReceiveDiscoveryOptions>,
) -> Result<()> {
    receive_observed(address, bundle, sink, log, seconds, stop, options, None)
}

/// Observe actual listening, authenticated transport and input state without
/// deriving lifecycle from buffered performance logs. Final completion remains
/// the caller's responsibility after this function and its input owner finish.
#[allow(clippy::too_many_arguments)]
pub fn receive_observed(
    address: &str,
    bundle: &Path,
    sink: &mut dyn MouseSink,
    log: Log,
    seconds: f64,
    stop: &AtomicBool,
    options: Option<&ReceiveDiscoveryOptions>,
    observer: Option<&dyn ReceiveObserver>,
) -> Result<()> {
    let identity = Identity::load(bundle)?;
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    let bound = listener.local_addr()?;
    if observer.is_none() {
        eprintln!(
            "Listening on {}. Press Escape on Mac to stop input; Ctrl+C exits.",
            bound
        );
    }
    log.write("listening", json!({"address":bound.to_string()}));
    let mut discovery = if let Some(options) = options.filter(|_| !stop.load(Ordering::Acquire)) {
        let ad = options.advertisement(bound)?;
        let handle = DiscoveryHandle::start(DiscoveryConfig {
            local_device_id: Some(ad.device_id.clone()),
            compatible_protocol: u16::from(crate::protocol::WIRE_VERSION),
            advertisement: Some(ad.clone()),
        })
        .context("Start receiver discovery")?;
        log.write("discovery_started", json!({"device_id":ad.device_id,"listen_address":bound.to_string(),"trusted":false,"status":"startup_requested"}));
        Some(handle)
    } else {
        None
    };
    if let Some(observer) = observer {
        observer.listening(bound);
    }

    // All loop exits, including local stop and failed input cleanup, reach the
    // explicit bounded mDNS teardown before dropping the bound listener.
    let result = receive_listener_loop(
        &listener,
        &identity,
        sink,
        log.clone(),
        seconds,
        stop,
        discovery.as_ref(),
        observer,
    );
    let cleanup = if let Some(handle) = &mut discovery {
        let stopped = handle.wait_stopped(Duration::from_secs(3));
        match &stopped {
            Ok(()) => log.write("discovery_stopped", json!({})),
            Err(error) => log.write(
                "error",
                json!({"stage":"receiver_discovery_cleanup","message":format!("{error:#}"),"receiver_error":result.as_ref().err().map(|e|format!("{e:#}"))}),
            ),
        }
        stopped.context("Stop receiver discovery")
    } else {
        Ok(())
    };
    match (result, cleanup) {
        // Cleanup failures are separately logged; preserve the input/receiver
        // error itself (including its concrete type and outermost context).
        (Err(error), _) => Err(error),
        (Ok(()), result) => result,
    }
}

fn check_discovery(discovery: Option<&DiscoveryHandle>) -> Result<()> {
    if let Some(snapshot) = discovery.and_then(DiscoveryHandle::try_snapshot) {
        ensure!(
            !matches!(
                snapshot.state,
                DiscoveryState::Error | DiscoveryState::Stopped
            ),
            "Receiver discovery stopped unexpectedly: {}",
            snapshot.error.as_deref().unwrap_or("stopped")
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn receive_listener_loop(
    listener: &TcpListener,
    identity: &Identity,
    sink: &mut dyn MouseSink,
    log: Log,
    seconds: f64,
    stop: &AtomicBool,
    discovery: Option<&DiscoveryHandle>,
    observer: Option<&dyn ReceiveObserver>,
) -> Result<()> {
    let duration =
        Duration::try_from_secs_f64(seconds).context("seconds exceeds duration range")?;
    let end = Instant::now()
        .checked_add(duration)
        .context("session deadline exceeds clock range")?;
    let mut discovery_check = Instant::now();
    let mut attempt_id = 0u64;
    while Instant::now() < end {
        if stop.load(Ordering::Acquire) || sink.emergency_pressed() {
            log.write("emergency_stop", json!({}));
            return Ok(());
        }
        check_discovery(discovery)?;
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
        let mut authenticated = false;
        let mut reported_enabled = false;
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
            let channel = security::handshake(&mut stream, identity, false)?;
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
            let mut first_sender_frame = true;
            trace.write(&log, "connected", json!({"peer":address.to_string()}));
            authenticated = true;
            if let Some(observer) = observer {
                observer.connected(true);
            }
            while Instant::now() < end {
                check_local_stop(&owned)?;
                if discovery_check.elapsed() >= Duration::from_secs(1) {
                    check_discovery(discovery)?;
                    discovery_check = Instant::now();
                }
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
                    if first_sender_frame && frames[0].0.kind == Kind::DisplayQuery {
                        ensure!(
                            frames.len() == 1 && !peer.pending(),
                            "invalid display query batch"
                        );
                        log.write("display_query", json!({}));
                        return serve_display_query(&mut peer, &mut owned, end);
                    }
                    first_sender_frame = false;
                }
                let ended = process_received_batch(frames, &mut state, &mut owned, &log)?;
                // Publish only after the complete batch and adapter operations
                // succeed. An unsuccessful release must not appear paused.
                if state.active != reported_enabled {
                    reported_enabled = state.active;
                    if let Some(observer) = observer {
                        observer.enabled(state.active);
                    }
                }
                if ended {
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
        // These notifications follow successful explicit cleanup, including
        // errors and local stop. A failed cleanup leaves state unconfirmed.
        if let Some(observer) = observer {
            if reported_enabled {
                observer.enabled(false);
            }
            if authenticated {
                observer.connected(false);
            }
        }
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
            if observer.is_none() {
                eprintln!("Session closed: {e:#}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::LogWriter;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn display_inventory_requires_complete_ordered_finite_unique_geometry() {
        let inventory = vec![
            NativeDisplay {
                id: 1,
                x: -1920.0,
                y: -100.0,
                width: 1920.0,
                height: 1080.0,
            },
            NativeDisplay {
                id: 2,
                x: 0.0,
                y: 0.0,
                width: 1470.0,
                height: 956.0,
            },
        ];
        let frames = display_frames(&inventory).unwrap();
        let mut reply = DisplayReply::default();
        for frame in &frames {
            reply.accept(*frame).unwrap();
        }
        assert!(reply.ended);
        assert_eq!(reply.displays, inventory);
        assert!(reply.accept(frames[0]).is_err());
        assert!(DisplayReply::default().accept(frames[1]).is_err());
        let mut reply = DisplayReply::default();
        reply.accept(frames[0]).unwrap();
        assert!(reply.accept(*frames.last().unwrap()).is_err());
        let mut duplicate = inventory.clone();
        duplicate[1].id = 1;
        assert!(display_frames(&duplicate).is_err());
        let mut invalid = inventory.clone();
        invalid[0].width = f64::NAN;
        assert!(display_frames(&invalid).is_err());
        assert!(display_frames(&[]).is_err());
    }

    #[test]
    fn positions_coalesce_without_crossing_relative_or_discrete_barriers() {
        struct Sink(Vec<(&'static str, f64)>);
        impl MouseSink for Sink {
            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                self.0.push(("move", x));
                Ok((x, y))
            }
            fn position(&mut self, x: f64, y: f64, _display_id: u32) -> Result<(f64, f64)> {
                self.0.push(("position", x));
                Ok((x, y))
            }
            fn key(&mut self, _: u16, _: bool) -> Result<()> {
                self.0.push(("key", 0.0));
                Ok(())
            }
        }
        let (writer, path) = logs();
        let mut state = state(&writer.log);
        let mut sink = Sink(vec![]);
        let mut key = frame(Kind::Key, 5, 0.0);
        key.0.code = 0x04;
        key.0.down = true;
        let frames = vec![
            frame(Kind::Arm, 1, 0.0),
            frame(Kind::Position, 2, 100.0),
            frame(Kind::Position, 3, 120.0),
            frame(Kind::Move, 4, 2.0),
            key,
            frame(Kind::Position, 6, -10.0),
            frame(Kind::Pause, 7, 0.0),
        ];
        process_received_batch(frames, &mut state, &mut sink, &writer.log).unwrap();
        assert_eq!(
            sink.0,
            vec![
                ("position", 120.0),
                ("move", 2.0),
                ("key", 0.0),
                ("position", -10.0)
            ]
        );
        writer.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let position: Value = text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v["kind"] == "Position")
            .unwrap();
        assert_eq!(position["first_seq"], 2);
        assert_eq!(position["seq"], 3);
        assert_eq!(position["frames_coalesced"], 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sender_phase_budgets_cap_and_clip_to_session_end() {
        let stop = AtomicBool::new(false);
        let long_end = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            sender_phase_budget(&stop, long_end, DISCOVERED_CONNECT_BUDGET).unwrap(),
            Duration::from_millis(250)
        );
        assert_eq!(
            sender_phase_budget(&stop, long_end, DISCOVERED_HANDSHAKE_BUDGET).unwrap(),
            Duration::from_millis(500)
        );
        let short_end = Instant::now() + Duration::from_millis(100);
        let budget = sender_phase_budget(&stop, short_end, DISCOVERED_HANDSHAKE_BUDGET).unwrap();
        assert!(!budget.is_zero() && budget <= Duration::from_millis(100));
    }

    #[test]
    fn sender_stop_and_deadline_prevent_entering_next_phase() {
        let stop = AtomicBool::new(false);
        let end = Instant::now() + Duration::from_secs(1);
        assert!(sender_phase_budget(&stop, end, DISCOVERED_CONNECT_BUDGET).is_ok());
        stop.store(true, Ordering::Release);
        assert!(sender_phase_budget(&stop, end, DISCOVERED_HANDSHAKE_BUDGET).is_err());
        stop.store(false, Ordering::Release);
        assert!(sender_phase_budget(&stop, Instant::now(), DISCOVERED_HANDSHAKE_BUDGET).is_err());
    }

    #[test]
    fn sender_retry_wait_observes_cancellation_without_full_backoff() {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(25));
            worker_stop.store(true, Ordering::Release);
        });
        let start = Instant::now();
        wait_sender_retry(&stop, start + Duration::from_secs(1));
        let elapsed = start.elapsed();
        worker.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(250),
            "cancelled retry took {elapsed:?}"
        );
    }

    #[test]
    fn discovered_sender_rejects_synthetic_before_loading_identity_or_network() {
        let (writer, path) = logs();
        let result = send_discovered(
            SendOptions {
                host: "127.0.0.1:1",
                bundle: Path::new("missing-identity"),
                seconds: 1.0,
                max_hz: 1000,
                synthetic: true,
            },
            Arc::new(Mutex::new(Motion::default())),
            Arc::new(AtomicBool::new(false)),
            writer.log.clone(),
        );
        assert!(result.unwrap_err().to_string().contains("manual arm"));
        writer.finish().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sender_rejects_invalid_duration_without_panicking_or_network() {
        let (writer, path) = logs();
        for seconds in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            let result = send(
                SendOptions {
                    host: "127.0.0.1:1",
                    bundle: Path::new("missing-identity"),
                    seconds,
                    max_hz: 1000,
                    synthetic: false,
                },
                Arc::new(Mutex::new(Motion::default())),
                Arc::new(AtomicBool::new(false)),
                writer.log.clone(),
            );
            assert!(result.unwrap_err().to_string().contains("seconds"));
        }
        writer.finish().unwrap();
        std::fs::remove_file(path).unwrap();
    }

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

    #[derive(Clone, Debug, PartialEq)]
    enum Observed {
        Listening(SocketAddr),
        Connected(bool),
        Enabled(bool),
    }
    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<Observed>>);
    impl ReceiveObserver for RecordingObserver {
        fn listening(&self, address: SocketAddr) {
            self.0.lock().unwrap().push(Observed::Listening(address));
        }
        fn connected(&self, connected: bool) {
            self.0.lock().unwrap().push(Observed::Connected(connected));
        }
        fn enabled(&self, enabled: bool) {
            self.0.lock().unwrap().push(Observed::Enabled(enabled));
        }
    }

    #[test]
    fn receiver_observer_does_not_claim_listening_for_invalid_identity() {
        let (writer, path) = logs();
        let observer = RecordingObserver::default();
        let result = receive_observed(
            "127.0.0.1:0",
            &path.with_extension("missing-identity"),
            &mut DrySink::default(),
            writer.log.clone(),
            0.1,
            &AtomicBool::new(false),
            None,
            Some(&observer),
        );
        assert!(result.is_err());
        assert!(observer.0.lock().unwrap().is_empty());
        writer.finish().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "actual encrypted localhost receiver; fake sink only, no OS input or multicast"]
    fn receiver_observer_reports_authenticated_input_and_confirmed_cleanup() -> Result<()> {
        struct CleanupSink(Arc<AtomicBool>);
        impl MouseSink for CleanupSink {
            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                Ok((x, y))
            }
            fn release_owned(&mut self) -> Result<()> {
                ensure!(!self.0.load(Ordering::Acquire), "fixture cleanup failure");
                Ok(())
            }
        }
        // Own only this fixture's exact files and threads. Failure or panic must
        // still stop/join the receiver before closing its log and deleting keys.
        struct Fixture {
            stop: Arc<AtomicBool>,
            receiver: Option<thread::JoinHandle<Result<()>>>,
            writer: Option<LogWriter>,
            log_path: std::path::PathBuf,
            pair_path: std::path::PathBuf,
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                if let Some(receiver) = self.receiver.take() {
                    let _ = receiver.join();
                }
                if let Some(writer) = self.writer.take() {
                    let _ = writer.finish();
                }
                for role in ["mac", "windows"] {
                    let bundle = self.pair_path.join(role);
                    let _ = std::fs::remove_file(bundle.join("identity.json"));
                    let _ = std::fs::remove_dir(bundle);
                }
                let _ = std::fs::remove_dir(&self.pair_path);
                let _ = std::fs::remove_file(&self.log_path);
            }
        }
        fn wait_for(observer: &RecordingObserver, expected: Observed) -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if observer.0.lock().unwrap().last() == Some(&expected) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(1));
            }
            bail!("missing receiver lifecycle event {expected:?}")
        }
        for fail_cleanup in [false, true] {
            let (writer, path) = logs();
            let pair_path = path.with_extension("pairing");
            let stop = Arc::new(AtomicBool::new(false));
            let mut fixture = Fixture {
                stop: stop.clone(),
                receiver: None,
                writer: Some(writer),
                log_path: path.clone(),
                pair_path: pair_path.clone(),
            };
            let pair = security::create_pair(&pair_path)?;
            let initiator = Identity::load(&pair.windows_bundle)?;
            let observer = Arc::new(RecordingObserver::default());
            let cleanup_failure = Arc::new(AtomicBool::new(false));
            let receiver = {
                let observer = observer.clone();
                let stop = stop.clone();
                let log = fixture.writer.as_ref().unwrap().log.clone();
                let cleanup_failure = cleanup_failure.clone();
                let bundle = pair.mac_bundle.clone();
                thread::spawn(move || {
                    receive_observed(
                        "127.0.0.1:0",
                        &bundle,
                        &mut CleanupSink(cleanup_failure),
                        log,
                        5.0,
                        &stop,
                        None,
                        Some(observer.as_ref()),
                    )
                })
            };
            fixture.receiver = Some(receiver);
            let exercise = (|| -> Result<()> {
                let deadline = Instant::now() + Duration::from_secs(2);
                let address = loop {
                    if let Some(Observed::Listening(address)) = observer.0.lock().unwrap().first() {
                        break *address;
                    }
                    ensure!(Instant::now() < deadline, "receiver did not become ready");
                    thread::sleep(Duration::from_millis(1));
                };
                let mut socket = TcpStream::connect(address)?;
                socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                socket.set_write_timeout(Some(Duration::from_secs(2)))?;
                let secure = security::handshake(&mut socket, &initiator, true)?;
                let mut peer = Peer::new(socket, secure)?;
                wait_for(&observer, Observed::Connected(true))?;
                let deadline = Instant::now() + Duration::from_secs(2);
                let lease = loop {
                    if let Some((frame, _)) = peer
                        .pump()?
                        .into_iter()
                        .find(|(f, _)| f.kind == Kind::Lease)
                    {
                        break frame.lease;
                    }
                    ensure!(Instant::now() < deadline, "receiver did not grant lease");
                    thread::sleep(Duration::from_millis(1));
                };
                let mut arm = Frame::new(Kind::Arm);
                arm.seq = 1;
                arm.lease = lease;
                peer.queue(arm)?;
                peer.pump()?;
                wait_for(&observer, Observed::Enabled(true))?;
                // Arm first performs reset/release. Only inject the final-cleanup
                // failure after that successful transition has been observed.
                cleanup_failure.store(fail_cleanup, Ordering::Release);
                if !fail_cleanup {
                    let mut pause = Frame::new(Kind::Pause);
                    pause.seq = 2;
                    peer.queue(pause)?;
                    peer.pump()?;
                    wait_for(&observer, Observed::Enabled(false))?;
                }
                // Stop while the authenticated TCP connection is still open.
                stop.store(true, Ordering::Release);
                Ok(())
            })();
            stop.store(true, Ordering::Release);
            let result = fixture
                .receiver
                .take()
                .unwrap()
                .join()
                .map_err(|_| anyhow::anyhow!("receiver fixture panicked"))?;
            fixture.writer.take().unwrap().finish()?;
            let events = observer.0.lock().unwrap().clone();
            for bundle in [pair.mac_bundle, pair.windows_bundle] {
                std::fs::remove_file(bundle.join("identity.json"))?;
                std::fs::remove_dir(bundle)?;
            }
            std::fs::remove_dir(pair_path)?;
            std::fs::remove_file(path)?;
            exercise?;
            if fail_cleanup {
                assert!(result.is_err());
                assert_eq!(
                    events.last(),
                    Some(&Observed::Enabled(true)),
                    "failed release must not claim paused/disconnected"
                );
            } else {
                result?;
                assert_eq!(events.last(), Some(&Observed::Connected(false)));
                assert!(events.contains(&Observed::Enabled(false)));
            }
        }
        Ok(())
    }

    #[test]
    fn receiver_advertisement_uses_bound_endpoint_and_a_fresh_public_id() {
        let options = ReceiveDiscoveryOptions::default();
        let bound: SocketAddr = "192.168.50.2:43123".parse().unwrap();
        let first = options.advertisement(bound).unwrap();
        let next = options.advertisement(bound).unwrap();
        assert_eq!(first.listen_ip, bound.ip());
        assert_eq!(first.port, bound.port());
        assert_eq!(first.friendly_name, discovery::default_device_name());
        assert_ne!(first.device_id, next.device_id);
        assert_eq!(first.protocol, u16::from(crate::protocol::WIRE_VERSION));
    }

    #[test]
    fn invalid_identity_fails_before_bind_or_discovery_start() {
        let (writer, path) = logs();
        let result = receive_with_discovery(
            "not a socket address",
            &path.with_extension("missing-identity"),
            &mut DrySink::default(),
            writer.log.clone(),
            0.1,
            &AtomicBool::new(false),
            Some(&ReceiveDiscoveryOptions::default()),
        );
        assert!(format!("{:#}", result.unwrap_err()).contains("Cannot read identity bundle"));
        writer.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"listening\""));
        assert!(!text.contains("discovery_started"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    #[ignore = "uses loopback listeners only; no LAN multicast; run explicitly"]
    fn receiver_bind_failure_and_preexisting_stop_never_start_discovery() {
        let (writer, path) = logs();
        let pair_path = path.with_extension("pairing");
        let pair = security::create_pair(&pair_path).unwrap();
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let options = ReceiveDiscoveryOptions::default();
        assert!(
            receive_with_discovery(
                &occupied.local_addr().unwrap().to_string(),
                &pair.mac_bundle,
                &mut DrySink::default(),
                writer.log.clone(),
                0.1,
                &AtomicBool::new(false),
                Some(&options),
            )
            .is_err()
        );
        assert!(
            receive_with_discovery(
                "127.0.0.1:0",
                &pair.mac_bundle,
                &mut DrySink::default(),
                writer.log.clone(),
                0.1,
                &AtomicBool::new(true),
                Some(&options),
            )
            .is_ok()
        );
        writer.finish().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"listening\"").count(), 1);
        assert!(!text.contains("discovery_started"));
        std::fs::remove_file(path).unwrap();
        for bundle in [pair.mac_bundle, pair.windows_bundle] {
            std::fs::remove_file(bundle.join("identity.json")).unwrap();
            std::fs::remove_dir(bundle).unwrap();
        }
        std::fs::remove_dir(pair_path).unwrap();
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
            absolute: false,
            display_id: 0,
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
