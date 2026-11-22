//! Shared duplex runtime. OS capture/UI adapters implement Host; input authority
//! stays in PairEndpoint and all network traffic uses one pinned Noise socket.
use crate::{
    arrangement::Point,
    input::Button,
    metrics::Log,
    pair_control::Side,
    pair_endpoint::{EndpointStatus, LocalSource, PairEndpoint},
    pair_layout::PairLayout,
    pair_source::SharedSource,
    pair_wire::{Display, Hello, Message, PairPeer, Received},
    protocol::Kind,
    screen_runtime::RuntimeLayout,
    security::Identity,
    session::{MouseSink, NativeDisplay, WorkerTiming},
    session_metadata::{RoutingState, SharedLayout},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    cell::Cell,
    collections::VecDeque,
    net::TcpStream,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Enable { epoch: u64 },
    Pause,
    Stop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionStatus {
    pub endpoint: EndpointStatus,
    pub capture_ready: bool,
    pub input_source_device_id: Option<String>,
    pub focus_device_id: Option<String>,
    pub focus_screen_id: Option<String>,
    pub shared_layout: SharedLayout,
}

pub trait Host {
    /// Poll physical capture before incoming input; apply any returned local
    /// warp outside capture locks. Capture timestamps share log.ns()'s origin.
    fn pulse(&mut self, now_ns: u64) -> Result<()>;
    fn poll_command(&mut self) -> Result<Option<Command>>;
    fn publish(&mut self, status: &SessionStatus) -> Result<()>;
}

pub struct PairConnection {
    peer: PairPeer,
    layout: PairLayout,
    runtime: RuntimeLayout,
    revision: u64,
    early_peer_ready: bool,
    max_hz: u32,
}
pub type Prepared = PairConnection;
// Only one setup result exists per connection; this is never a buffered message.
#[allow(clippy::large_enum_variant)]
pub enum EstablishOutcome {
    Session(PairConnection),
    InventoryServed,
}

const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct InputTrace {
    source_side: Side,
    owner_epoch: u64,
    layout_revision: u64,
    seq: u64,
    kind: Kind,
    t0_ns: u64,
    t1_ns: u64,
}
impl InputTrace {
    fn from_message(message: &Message) -> Option<Self> {
        match message {
            Message::Input {
                source,
                owner_epoch,
                layout_revision,
                frame,
            } => Some(Self {
                source_side: *source,
                owner_epoch: *owner_epoch,
                layout_revision: *layout_revision,
                seq: frame.seq,
                kind: frame.kind,
                t0_ns: frame.t0_ns,
                t1_ns: frame.t1_ns,
            }),
            _ => None,
        }
    }
    fn fields(self, connection_id: u64) -> serde_json::Value {
        serde_json::json!({"connection_id":connection_id,"source_side":format!("{:?}",self.source_side),
            "owner_epoch":self.owner_epoch,"layout_revision":self.layout_revision,"seq":self.seq,
            "kind":format!("{:?}",self.kind),"t0_ns":self.t0_ns,"t1_ns":self.t1_ns})
    }
}

// Diagnostic context only; it never participates in input authorization. IDs
// are process-local, not shared wire session IDs. Reconnect epochs/sequences
// can repeat, so these records do not uniquely identify a cross-host session.
struct SessionDiagnostics {
    connection_id: u64,
    current: Cell<Option<InputTrace>>,
    log: Log,
}
impl SessionDiagnostics {
    fn start(connection: &PairConnection, log: Log) -> Self {
        let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        log.write("pair_session_started",serde_json::json!({
            "connection_id":connection_id,"connection_id_scope":"process_local",
            "local_device_id":connection.local_device_id(),"remote_device_id":connection.remote_device_id(),
            "local_side":format!("{:?}",connection.local_side()),"layout_revision":connection.revision}));
        Self {
            connection_id,
            current: Cell::new(None),
            log,
        }
    }
    fn queued(&self, trace: Option<InputTrace>, pending: usize) {
        if let Some(trace) = trace {
            let mut fields = trace.fields(self.connection_id);
            fields["stage"] = "transport_queue".into();
            fields["endpoint_pending_records"] = pending.into();
            fields["transport_pending_records"] = 1.into();
            self.log.write("pair_input_queued", fields);
        }
    }
    fn dispatch(
        &self,
        trace: Option<InputTrace>,
        received_at: Instant,
        receive: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.current.set(trace);
        let began = self.log.ns();
        let result = receive();
        let ended = self.log.ns();
        self.current.set(None);
        if let Some(trace) = trace {
            let mut fields = trace.fields(self.connection_id);
            fields["stage"] = "endpoint_dispatch".into();
            fields["t2_ns"] = self.log.at(received_at).into();
            fields["t3_ns"] = began.into();
            fields["t4_ns"] = ended.into();
            // Successful dispatch may discard an old frame without injection.
            fields["success"] = result.is_ok().into();
            self.log.write("pair_input_received", fields);
        }
        result
    }
}

fn can_coalesce_motion(runtime: &RuntimeLayout, current: &Message, next: &Message) -> bool {
    let (
        Message::Input {
            source: current_source,
            owner_epoch: current_epoch,
            layout_revision: current_revision,
            frame: current_frame,
        },
        Message::Input {
            source: next_source,
            owner_epoch: next_epoch,
            layout_revision: next_revision,
            frame: next_frame,
        },
    ) = (current, next)
    else {
        return false;
    };
    if current_source != next_source
        || current_epoch != next_epoch
        || current_revision != next_revision
    {
        return false;
    }
    match (current_frame.kind, next_frame.kind) {
        (Kind::Position, Kind::Position) => {
            current_frame.display_id == next_frame.display_id
                && runtime.contains_local_native_point(
                    current_frame.display_id,
                    Point {
                        x: current_frame.x,
                        y: current_frame.y,
                    },
                )
        }
        (Kind::Move, Kind::Move) => {
            current_frame.x.is_finite()
                && current_frame.y.is_finite()
                && next_frame.x.is_finite()
                && next_frame.y.is_finite()
        }
        _ => false,
    }
}

fn drain_outbound<K: MouseSink>(
    peer: &mut PairPeer,
    endpoint: &mut PairEndpoint<SharedSource, K>,
    diagnostics: &SessionDiagnostics,
    log: &Log,
) -> Result<usize> {
    let mut drained = 0usize;
    while !peer.pending() {
        let Some(message) = endpoint.pop_outbound_at(log.ns())? else {
            break;
        };
        let trace = InputTrace::from_message(&message);
        peer.queue(message)?;
        diagnostics.queued(trace, endpoint.pending_outbound());
        drained += 1;
        peer.flush_pending()?;
    }
    Ok(drained)
}
// A pump can read the peer's readiness marker and its first runtime records
// together. Keep that ordered tail intact until the local endpoint exists.
fn accept_ready_batch(
    records: Vec<Received>,
    revision: u64,
    local_ready_sent: bool,
    received: &mut bool,
) -> Result<Vec<Received>> {
    ensure!(records.len() <= 64, "native readiness backlog exceeded");
    let mut runtime = Vec::new();
    for record in records {
        if let Message::SessionReady { revision: peer } = &record.message {
            ensure!(
                *peer == revision && !*received,
                "unexpected record before native readiness"
            );
            *received = true;
        } else {
            ensure!(
                local_ready_sent && *received,
                "unexpected record before native readiness"
            );
            runtime.push(record);
        }
    }
    Ok(runtime)
}
fn setup_alive(stop: &AtomicBool, end: Instant) -> Result<()> {
    ensure!(!stop.load(Ordering::Acquire), "pair setup stopped");
    ensure!(Instant::now() < end, "pair setup timed out");
    Ok(())
}
fn exchange_hello(
    peer: &mut PairPeer,
    local: Hello,
    stop: &AtomicBool,
    end: Instant,
) -> Result<(Hello, VecDeque<Message>)> {
    peer.queue(Message::Hello(local))?;
    let mut remote = None;
    let mut pending = VecDeque::new();
    loop {
        setup_alive(stop, end)?;
        for record in peer.pump()? {
            if let Message::Hello(hello) = record.message {
                ensure!(remote.is_none(), "duplicate authenticated hello");
                remote = Some(hello);
            } else {
                ensure!(pending.len() < 64, "setup backlog exceeded");
                pending.push_back(record.message);
            }
        }
        if peer.hello_complete() && !peer.pending() {
            return Ok((remote.context("missing authenticated inventory")?, pending));
        }
        thread::sleep(Duration::from_micros(500));
    }
}
fn flush_setup(peer: &mut PairPeer, stop: &AtomicBool, end: Instant) -> Result<()> {
    while peer.pending() {
        setup_alive(stop, end)?;
        peer.flush_pending()?;
        if peer.pending() {
            thread::sleep(Duration::from_micros(500));
        }
    }
    Ok(())
}

/// Inventory-only authenticated query. No source, sink, layout or input owner is
/// constructed. The caller converts wire_id/native bounds for its own UI.
pub fn query_inventory(
    stream: TcpStream,
    identity: &Identity,
    local: Hello,
    stop: &AtomicBool,
    log: Log,
) -> Result<Vec<Display>> {
    let end = Instant::now() + PREPARE_TIMEOUT;
    setup_alive(stop, end)?;
    let mut peer = PairPeer::handshake(stream, identity, true, Duration::from_secs(2))?;
    let (remote, mut pending) = exchange_hello(&mut peer, local, stop, end)?;
    ensure!(
        pending.is_empty(),
        "unsolicited data before inventory query"
    );
    peer.queue(Message::QueryInventory)?;
    loop {
        setup_alive(stop, end)?;
        pending.extend(peer.pump()?.into_iter().map(|record| record.message));
        if let Some(message) = pending.pop_front() {
            ensure!(
                message == Message::InventoryDone && pending.is_empty(),
                "unexpected inventory query response"
            );
            log.write(
                "pair_inventory_received",
                serde_json::json!({"screens":remote.inventory.len()}),
            );
            peer.abort();
            return Ok(remote.inventory);
        }
        thread::sleep(Duration::from_micros(500));
    }
}

impl PairConnection {
    #[allow(clippy::too_many_arguments)]
    pub fn establish(
        stream: TcpStream,
        identity: &Identity,
        initiator: bool,
        local: Hello,
        proposal: Option<RuntimeLayout>,
        stop: &AtomicBool,
        log: Log,
    ) -> Result<EstablishOutcome> {
        ensure!(
            initiator == proposal.is_some(),
            "only TCP initiator supplies initial arrangement"
        );
        let end = Instant::now() + PREPARE_TIMEOUT;
        setup_alive(stop, end)?;
        let mut peer = PairPeer::handshake(stream, identity, initiator, Duration::from_secs(2))?;
        let (remote, mut pending) = exchange_hello(&mut peer, local.clone(), stop, end)?;
        let revision = 1;
        let mut layout = proposal
            .as_ref()
            .map(|runtime| PairLayout::from_runtime(runtime, identity, &local, &remote))
            .transpose()?;
        if let Some(layout) = &layout {
            peer.queue(Message::Layout {
                revision,
                layout: layout.shared().clone(),
            })?;
        }
        let mut peer_accepted = false;
        let mut accepted_sent = false;
        let mut early_peer_ready = false;
        loop {
            setup_alive(stop, end)?;
            pending.extend(peer.pump()?.into_iter().map(|record| record.message));
            while let Some(message) = pending.pop_front() {
                match message {
                    Message::QueryInventory => {
                        ensure!(
                            !initiator
                                && layout.is_none()
                                && !accepted_sent
                                && !peer.pending()
                                && pending.is_empty(),
                            "inventory query after session setup"
                        );
                        peer.queue(Message::InventoryDone)?;
                        flush_setup(&mut peer, stop, end)?;
                        // Give the querying side time to receive Done and close. EOF
                        // here is expected; it never represents an input session.
                        let close_until = Instant::now() + Duration::from_millis(250);
                        while Instant::now() < close_until && !stop.load(Ordering::Acquire) {
                            match peer.pump() {
                                Ok(records) => {
                                    ensure!(records.is_empty(), "input after inventory query")
                                }
                                Err(_) => break,
                            }
                            thread::sleep(Duration::from_millis(1));
                        }
                        log.write(
                            "pair_inventory_served",
                            serde_json::json!({"screens":local.inventory.len()}),
                        );
                        return Ok(EstablishOutcome::InventoryServed);
                    }
                    Message::Layout {
                        revision: received,
                        layout: shared,
                    } => {
                        ensure!(
                            !initiator && layout.is_none() && received == revision,
                            "unexpected initial arrangement"
                        );
                        layout = Some(PairLayout::from_shared(&shared, identity, &local, &remote)?);
                    }
                    Message::LayoutReady { revision: received } => {
                        ensure!(
                            layout.is_some() && !peer_accepted && received == revision,
                            "unexpected layout acceptance"
                        );
                        if !initiator {
                            ensure!(
                                accepted_sent,
                                "initiator acknowledged before local validation"
                            );
                        }
                        peer_accepted = true;
                    }
                    Message::SessionReady { revision: received } => {
                        ensure!(
                            peer_accepted
                                && accepted_sent
                                && received == revision
                                && !early_peer_ready,
                            "native ready before layout confirmation"
                        );
                        early_peer_ready = true;
                    }
                    _ => bail!("runtime record before accepted arrangement"),
                }
            }
            if layout.is_some()
                && !accepted_sent
                && (!initiator || peer_accepted)
                && !peer.pending()
            {
                peer.queue(Message::LayoutReady { revision })?;
                accepted_sent = true;
            }
            if peer_accepted && accepted_sent && !peer.pending() {
                let layout = layout.context("missing accepted layout")?;
                let runtime = layout.runtime_for(peer.local_device_id())?;
                log.write("pair_layout_accepted",serde_json::json!({"revision":revision,"screens":layout.shared().screens.len()}));
                return Ok(EstablishOutcome::Session(Self {
                    peer,
                    layout,
                    runtime,
                    revision,
                    early_peer_ready,
                    max_hz: 1000,
                }));
            }
            thread::sleep(Duration::from_micros(500));
        }
    }

    /// Symmetric peer-to-peer connection over LAN (open LAN or optional shared passphrase).
    /// Initiator proposes either a saved/user-edited SharedLayout (if still valid for both
    /// peers' live monitors) or an automatic side-by-side arrangement.
    pub fn establish_peer(
        stream: TcpStream,
        local_device_id: &str,
        passphrase: Option<&str>,
        initiator: bool,
        local: Hello,
        preferred_layout: Option<SharedLayout>,
        stop: &AtomicBool,
        log: Log,
    ) -> Result<Self> {
        let end = Instant::now() + PREPARE_TIMEOUT;
        setup_alive(stop, end)?;
        let mut peer = PairPeer::handshake_lan_psk(
            stream,
            local_device_id,
            passphrase,
            initiator,
            Duration::from_secs(2),
        )?;
        let (remote, mut pending) = exchange_hello(&mut peer, local.clone(), stop, end)?;
        let revision = 1;
        let mut layout = if initiator {
            let chosen = match preferred_layout {
                Some(ref saved) => PairLayout::from_shared_ids(
                    saved,
                    peer.local_device_id(),
                    peer.remote_device_id(),
                    &local,
                    &remote,
                )
                .ok(),
                None => None,
            };
            let built = match chosen {
                Some(l) => l,
                None => PairLayout::auto_arrange(&local, &remote)?,
            };
            peer.queue(Message::Layout {
                revision,
                layout: built.shared().clone(),
            })?;
            Some(built)
        } else {
            None
        };
        let mut peer_accepted = false;
        let mut accepted_sent = false;
        let mut early_peer_ready = false;
        loop {
            setup_alive(stop, end)?;
            pending.extend(peer.pump()?.into_iter().map(|record| record.message));
            while let Some(message) = pending.pop_front() {
                match message {
                    Message::Layout {
                        revision: received,
                        layout: shared,
                    } => {
                        ensure!(
                            !initiator && layout.is_none() && received == revision,
                            "unexpected initial peer arrangement"
                        );
                        layout = Some(PairLayout::from_shared_ids(
                            &shared,
                            peer.local_device_id(),
                            peer.remote_device_id(),
                            &local,
                            &remote,
                        )?);
                    }
                    Message::LayoutReady { revision: received } => {
                        ensure!(
                            layout.is_some() && !peer_accepted && received == revision,
                            "unexpected peer layout acceptance"
                        );
                        if !initiator {
                            ensure!(
                                accepted_sent,
                                "initiator acknowledged before local validation"
                            );
                        }
                        peer_accepted = true;
                    }
                    Message::SessionReady { revision: received } => {
                        ensure!(
                            peer_accepted
                                && accepted_sent
                                && received == revision
                                && !early_peer_ready,
                            "native ready before layout confirmation"
                        );
                        early_peer_ready = true;
                    }
                    _ => bail!("runtime record before accepted peer arrangement"),
                }
            }
            if layout.is_some()
                && !accepted_sent
                && (!initiator || peer_accepted)
                && !peer.pending()
            {
                peer.queue(Message::LayoutReady { revision })?;
                accepted_sent = true;
            }
            if peer_accepted && accepted_sent && !peer.pending() {
                let layout = layout.context("missing accepted peer layout")?;
                let runtime = layout.runtime_for(peer.local_device_id())?;
                log.write(
                    "pair_layout_accepted",
                    serde_json::json!({"revision":revision,"screens":layout.shared().screens.len(),"mode":"lan_peer"}),
                );
                return Ok(Self {
                    peer,
                    layout,
                    runtime,
                    revision,
                    early_peer_ready,
                    max_hz: 1000,
                });
            }
            thread::sleep(Duration::from_micros(500));
        }
    }
    pub fn runtime(&self) -> &RuntimeLayout {
        &self.runtime
    }
    pub fn shared_layout(&self) -> &SharedLayout {
        self.layout.shared()
    }
    pub fn local_device_id(&self) -> &str {
        self.peer.local_device_id()
    }
    pub fn remote_device_id(&self) -> &str {
        self.peer.remote_device_id()
    }
    pub fn local_side(&self) -> Side {
        self.peer.local_side()
    }
    pub fn set_max_hz(&mut self, hz: u32) -> Result<()> {
        ensure!((60..=8000).contains(&hz), "max-hz must be 60..8000");
        self.max_hz = hz;
        Ok(())
    }

    fn native_ready(
        &mut self,
        source: &SharedSource,
        sink: &mut dyn MouseSink,
        host: &mut impl Host,
        stop: &AtomicBool,
        log: &Log,
    ) -> Result<Option<Vec<Received>>> {
        let end = Instant::now() + PREPARE_TIMEOUT;
        let mut sent = false;
        let mut received = self.early_peer_ready;
        loop {
            let now = log.ns();
            host.pulse(now)?;
            if matches!(host.poll_command()?, Some(Command::Stop)) {
                stop.store(true, Ordering::Release);
            }
            if stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            ensure!(Instant::now() < end, "native pair preparation timed out");
            let state = source.status();
            ensure!(
                state.failure.is_none(),
                "native capture failed during preparation"
            );
            ensure!(
                !sink.emergency_pressed(),
                "local stop during pair preparation"
            );
            sink.maintain()?; // idle worker only: never Reset/Arm during preparation
            if state.ready && !sent && !self.peer.pending() {
                self.peer.queue(Message::SessionReady {
                    revision: self.revision,
                })?;
                sent = true;
            }
            let records = self.peer.pump()?;
            let runtime = accept_ready_batch(
                records,
                self.revision,
                sent && !self.peer.pending(),
                &mut received,
            )?;
            if sent && received && !self.peer.pending() {
                return Ok(Some(runtime));
            }
            thread::sleep(Duration::from_micros(500));
        }
    }
}

struct BorrowedSink<'a> {
    sink: &'a mut dyn MouseSink,
    log: Log,
    diagnostics: &'a SessionDiagnostics,
}
impl BorrowedSink<'_> {
    fn completed(&mut self, kind: &str, start: u64, success: bool) {
        let end = self.log.ns();
        let timing = self.sink.take_timing();
        let mut fields = self.diagnostics.current.get().map_or_else(
            || serde_json::json!({"connection_id":self.diagnostics.connection_id}),
            |trace| trace.fields(self.diagnostics.connection_id),
        );
        fields["frame_kind"] = fields["kind"].clone();
        fields["kind"] = kind.into();
        fields["t3_ns"] = start.into();
        fields["t4_ns"] = end.into();
        fields["success"] = success.into();
        fields["worker_timing"] = serde_json::json!(timing);
        self.log.write("pair_input_call", fields);
    }
}
impl MouseSink for BorrowedSink<'_> {
    fn displays(&self) -> Result<Vec<NativeDisplay>> {
        self.sink.displays()
    }
    fn take_timing(&mut self) -> Option<WorkerTiming> {
        self.sink.take_timing()
    }
    fn maintain(&mut self) -> Result<()> {
        self.sink.maintain()
    }
    fn reset(&mut self) -> Result<()> {
        self.sink.reset()
    }
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        let t = self.log.ns();
        let r = self.sink.move_by(x, y);
        self.completed("Move", t, r.is_ok());
        r
    }
    fn position(&mut self, x: f64, y: f64, id: u32) -> Result<(f64, f64)> {
        let t = self.log.ns();
        let r = self.sink.position(x, y, id);
        self.completed("Position", t, r.is_ok());
        r
    }
    fn button(&mut self, b: Button, down: bool) -> Result<()> {
        let t = self.log.ns();
        let r = self.sink.button(b, down);
        self.completed("Button", t, r.is_ok());
        r
    }
    fn key(&mut self, k: u16, down: bool) -> Result<()> {
        let t = self.log.ns();
        let r = self.sink.key(k, down);
        self.completed("Key", t, r.is_ok());
        r
    }
    fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
        let t = self.log.ns();
        let r = self.sink.scroll(x, y);
        self.completed("Scroll", t, r.is_ok());
        r
    }
    fn release_owned(&mut self) -> Result<()> {
        self.sink.release_owned()
    }
    fn emergency_pressed(&self) -> bool {
        self.sink.emergency_pressed()
    }
}

impl PairConnection {
    fn session_status<K: MouseSink>(
        &self,
        endpoint: &PairEndpoint<SharedSource, K>,
        source: &SharedSource,
    ) -> Result<SessionStatus> {
        let state = endpoint.status();
        let mut focus = (None, None);
        if state.connected && state.enabled {
            if state.owner == Some(self.local_side()) {
                focus = source.focus_identity();
            } else if state.owner == Some(self.peer.remote_side()) {
                if let Some(frame) = endpoint.routing_frame() {
                    let routing = self.shared_layout().routing_state(&frame)?;
                    let device = self
                        .shared_layout()
                        .focus_device(&routing)
                        .map(str::to_owned);
                    let local_focus = device.as_deref() == Some(self.local_device_id());
                    // Sender-local vs receiver-local is determined by pinned
                    // identities, never by the layout author's orientation.
                    if routing.enabled && local_focus == state.input_active {
                        focus = (device, routing.focus_screen_id);
                    }
                }
            }
        }
        let input_source_device_id = state.owner.map(|owner| {
            if owner == self.local_side() {
                self.local_device_id()
            } else {
                self.remote_device_id()
            }
            .to_owned()
        });
        Ok(SessionStatus {
            endpoint: state,
            capture_ready: source.status().ready,
            input_source_device_id,
            focus_device_id: focus.0,
            focus_screen_id: focus.1,
            shared_layout: self.shared_layout().clone(),
        })
    }

    /// Runs one connection only. Return leaves the sink borrowed, so its owner
    /// must still verify isolated-worker shutdown and native capture termination.
    pub fn run(
        mut self,
        source: &SharedSource,
        sink: &mut dyn MouseSink,
        host: &mut impl Host,
        stop: &AtomicBool,
        log: Log,
    ) -> Result<()> {
        let diagnostics = SessionDiagnostics::start(&self, log.clone());
        let initial = (|| -> Result<Option<Vec<Received>>> {
            ensure!(
                source.matches_runtime(self.runtime()),
                "capture source does not match authenticated arrangement"
            );
            let state = source.status();
            ensure!(
                state.connected && !state.enabled && !state.remote_focus && state.failure.is_none(),
                "pair preparation requires a fresh inactive capture source"
            );
            source.clone().revoke()?;
            sink.release_owned()?;
            self.native_ready(source, sink, host, stop, &log)
        })();
        if !matches!(initial, Ok(Some(_))) {
            self.peer.abort();
            source.disconnect();
            let cleanup = sink.release_owned();
            log.write(
                "pair_session_cleanup",
                serde_json::json!({"confirmed":cleanup.is_ok(),"stage":"preparation"}),
            );
            cleanup.context("pair preparation cleanup unconfirmed")?;
            return initial.map(|_| ());
        }
        let mut ready_records = initial?.unwrap();
        let local_side = self.local_side();
        let created = PairEndpoint::new(
            local_side,
            self.revision,
            source.clone(),
            BorrowedSink {
                sink,
                log: log.clone(),
                diagnostics: &diagnostics,
            },
            log.ns(),
        );
        let mut endpoint = match created {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.peer.abort();
                source.disconnect();
                log.write(
                    "pair_session_cleanup",
                    serde_json::json!({"confirmed":false,"stage":"initial_endpoint"}),
                );
                return Err(error.context("pair initial cleanup unconfirmed"));
            }
        };
        let mut activity = source.activity_counter();
        let mut last_status: Option<SessionStatus> = None;
        let mut last_publish = Instant::now() - Duration::from_secs(1);
        let mut last_motion: Option<Instant> = None;
        let mut sent_routing: Option<RoutingState> = None;
        let mut authority = endpoint.status().epoch;
        let interval = Duration::from_secs_f64(1. / f64::from(self.max_hz));
        let mut consecutive_busy_spins: u8 = 0;
        let result = (|| -> Result<()> {
            loop {
                host.pulse(log.ns())?;
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                let command = host.poll_command()?;
                if matches!(command, Some(Command::Stop)) {
                    stop.store(true, Ordering::Release);
                    return Ok(());
                }
                let source_state = source.status();
                ensure!(
                    source_state.connected && source_state.failure.is_none(),
                    "physical capture stopped or failed"
                );
                let previous = endpoint.status();
                let current_activity = source.activity_counter();
                if current_activity != activity {
                    activity = current_activity;
                    if previous.enabled && previous.owner != Some(local_side) {
                        endpoint.request_local(log.ns())?;
                    }
                }
                match command {
                    Some(Command::Stop) => unreachable!("handled above"),
                    Some(Command::Pause) => endpoint.pause(log.ns())?,
                    Some(Command::Enable { epoch }) => {
                        let state = endpoint.status();
                        if source_state.ready && !state.enabled && state.epoch == epoch {
                            endpoint.enable_local(log.ns())?;
                        } else {
                            log.write(
                                "pair_enable_rejected",
                                serde_json::json!({"reason":"not_ready_enabled_or_stale_epoch"}),
                            );
                        }
                    }
                    None => {}
                }
                // Noise records cannot be cancelled after nonce consumption.
                // A local revoke never flushes a pending old input merely to
                // preserve transport continuity: close this connection instead.
                if self.peer.pending_input()
                    && (endpoint.status().epoch != previous.epoch
                        || !endpoint.lease_ready(log.ns()))
                {
                    bail!("local revoke discarded pending transport input");
                }
                let records = if ready_records.is_empty() {
                    self.peer.pump()?
                } else {
                    std::mem::take(&mut ready_records)
                };
                let batch_was_empty = records.is_empty();
                let mut iter = records.into_iter().peekable();
                while let Some(record) = iter.next() {
                    host.pulse(log.ns())?;
                    if stop.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let latest = source.activity_counter();
                    if latest != activity {
                        activity = latest;
                        if endpoint.status().enabled && endpoint.status().owner != Some(local_side)
                        {
                            endpoint.request_local(log.ns())?;
                        }
                    }
                    let defer = iter.peek().is_some_and(|next| {
                        can_coalesce_motion(&self.runtime, &record.message, &next.message)
                    });
                    let trace = InputTrace::from_message(&record.message);
                    diagnostics.dispatch(trace, record.received_at, || {
                        if let Message::Input { frame, .. } = &record.message {
                            if frame.kind == Kind::RoutingState {
                                self.shared_layout().routing_state(frame)?;
                            }
                        }
                        endpoint.receive_coalesced(record.message, defer, log.ns())
                    })?;
                }
                if self.peer.pending_input() && !endpoint.lease_ready(log.ns()) {
                    bail!("remote revoke discarded pending transport input");
                }
                endpoint.tick(log.ns())?;
                let mut drained =
                    drain_outbound(&mut self.peer, &mut endpoint, &diagnostics, &log)?;
                let state = endpoint.status();
                if state.epoch != authority {
                    authority = state.epoch;
                    sent_routing = None;
                    last_motion = None;
                }
                let mut submitted = false;
                if !self.peer.pending()
                    && endpoint.pending_outbound() == 0
                    && endpoint.lease_ready(log.ns())
                {
                    let allow_motion = last_motion.is_none_or(|at| at.elapsed() >= interval);
                    if let Some(frame) = source.next_frame_rate_limited(log.ns(), allow_motion)? {
                        let motion = matches!(frame.kind, Kind::Move | Kind::Position);
                        if matches!(frame.kind, Kind::Arm | Kind::Pause | Kind::Stop) {
                            sent_routing = None;
                        }
                        ensure!(
                            endpoint.send_input(frame, log.ns())?,
                            "source authority changed while submitting captured input"
                        );
                        submitted = true;
                        if motion {
                            last_motion = Some(Instant::now());
                        }
                    }
                    let routing = source.routing_snapshot();
                    if sent_routing.as_ref() != Some(&routing) {
                        let mut frame = self.shared_layout().routing_frame(&routing)?;
                        frame.t0_ns = log.ns();
                        ensure!(
                            endpoint.send_input(frame, log.ns())?,
                            "source authority changed while submitting focus"
                        );
                        submitted = true;
                        sent_routing = Some(routing);
                    }
                }
                drained += drain_outbound(&mut self.peer, &mut endpoint, &diagnostics, &log)?;
                let status = self.session_status(&endpoint, source)?;
                if last_status.as_ref() != Some(&status)
                    || last_publish.elapsed() >= Duration::from_millis(100)
                {
                    host.publish(&status)?;
                    last_status = Some(status);
                    last_publish = Instant::now();
                }
                let did_work = !batch_was_empty || drained > 0 || submitted;
                if did_work && consecutive_busy_spins < 8 {
                    consecutive_busy_spins += 1;
                    thread::yield_now();
                } else {
                    consecutive_busy_spins = 0;
                    thread::sleep(Duration::from_micros(if state.owner.is_some() {
                        150
                    } else {
                        500
                    }));
                }
            }
        })();
        // Never flush queued input during any exit, including a normal Stop.
        self.peer.abort();
        let cleanup = endpoint.shutdown();
        let final_status = self.session_status(&endpoint, source);
        let publish = final_status.and_then(|status| host.publish(&status));
        log.write("pair_session_cleanup",serde_json::json!({"confirmed":cleanup.is_ok(),"run_success":result.is_ok(),"stage":"runtime"}));
        drop(endpoint);
        source.disconnect();
        cleanup.context("pair owned-input cleanup unconfirmed")?;
        result?;
        publish?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        arrangement::{Layout, Point, Screen},
        metrics::LogWriter,
        pair_source::SourceEvent,
        screen_runtime::{Binding, NativeRect},
        security,
    };
    use std::{
        fs,
        net::TcpListener,
        path::PathBuf,
        sync::{
            Arc, Mutex,
            mpsc::{self, Receiver, Sender},
        },
        thread::JoinHandle,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> Result<Fixture> {
        let path = std::env::current_dir()?.join("runs").join(format!(
            "pair-session-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&path)?;
        security::create_pair(&path.join("identities"))?;
        Ok(Fixture(path))
    }
    fn hello(identity: &Identity) -> Hello {
        Hello {
            device_id: identity.public_key_hex(),
            inventory: vec![Display {
                native_id: "display".into(),
                wire_id: 7,
                x: 0.,
                y: 0.,
                width: 100.,
                height: 100.,
            }],
        }
    }
    fn proposal() -> RuntimeLayout {
        RuntimeLayout {
            layout: Layout::new(vec![
                Screen {
                    id: "win".into(),
                    device_id: "local".into(),
                    x: 0.,
                    y: 0.,
                    width: 100.,
                    height: 100.,
                },
                Screen {
                    id: "mac".into(),
                    device_id: "remote".into(),
                    x: 100.,
                    y: 0.,
                    width: 100.,
                    height: 100.,
                },
            ])
            .unwrap(),
            local_device_id: "local".into(),
            remote_device_id: "remote".into(),
            bindings: ["win", "mac"]
                .into_iter()
                .map(|id| Binding {
                    screen_id: id.into(),
                    native_id: if id == "win" {
                        "display".into()
                    } else {
                        "7".into()
                    },
                    native: NativeRect {
                        x: 0.,
                        y: 0.,
                        width: 100.,
                        height: 100.,
                    },
                })
                .collect(),
        }
    }
    struct MockSink(Arc<Mutex<Vec<&'static str>>>);
    impl MouseSink for MockSink {
        fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
            self.0.lock().unwrap().push("move");
            Ok((x, y))
        }
        fn position(&mut self, x: f64, y: f64, _id: u32) -> Result<(f64, f64)> {
            let mut events = self.0.lock().unwrap();
            events.push("position");
            ensure!(!events.contains(&"fail-position"), "mock position failure");
            Ok((x, y))
        }
        fn key(&mut self, _usage: u16, down: bool) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(if down { "down" } else { "up" });
            Ok(())
        }
        fn release_owned(&mut self) -> Result<()> {
            let mut events = self.0.lock().unwrap();
            events.push("release");
            ensure!(!events.contains(&"fail-release"), "mock release failure");
            Ok(())
        }
    }
    struct MockHost {
        source: SharedSource,
        commands: Receiver<Command>,
        events: Receiver<SourceEvent>,
        status: Arc<Mutex<Option<SessionStatus>>>,
    }
    impl Host for MockHost {
        fn pulse(&mut self, now: u64) -> Result<()> {
            for _ in 0..32 {
                match self.events.try_recv() {
                    Ok(event) => {
                        self.source.observe(event, now)?;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }
            Ok(())
        }
        fn poll_command(&mut self) -> Result<Option<Command>> {
            Ok(self.commands.try_recv().ok())
        }
        fn publish(&mut self, status: &SessionStatus) -> Result<()> {
            *self.status.lock().unwrap() = Some(status.clone());
            Ok(())
        }
    }
    struct Running {
        commands: Sender<Command>,
        events: Sender<SourceEvent>,
        status: Arc<Mutex<Option<SessionStatus>>>,
        sink: Arc<Mutex<Vec<&'static str>>>,
        worker: Option<JoinHandle<Result<()>>>,
    }
    impl Running {
        fn wait(&self, test: impl Fn(&SessionStatus) -> bool) -> Result<SessionStatus> {
            let end = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(status) = self.status.lock().unwrap().clone() {
                    if test(&status) {
                        return Ok(status);
                    }
                }
                if self.worker.as_ref().is_some_and(|w| w.is_finished()) {
                    bail!("mock runtime exited before expected status");
                }
                ensure!(
                    Instant::now() < end,
                    "mock session status timed out: {:?}",
                    self.status.lock().unwrap()
                );
                thread::sleep(Duration::from_millis(1));
            }
        }
        fn has(&self, event: &str) -> Result<()> {
            let end = Instant::now() + Duration::from_secs(2);
            while !self.sink.lock().unwrap().contains(&event) {
                ensure!(Instant::now() < end, "mock sink did not receive {event}");
                thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }
        fn movement(&self, x: f64, dx: f64) -> Result<()> {
            self.events.send(SourceEvent::Movement {
                native: Point { x, y: 50. },
                dx,
                dy: 0.,
            })?;
            Ok(())
        }
    }
    fn spawn_runtime(
        stream: impl FnOnce() -> Result<TcpStream> + Send + 'static,
        identity: Identity,
        initiator: bool,
        path: PathBuf,
        stop: Arc<AtomicBool>,
        delay: Duration,
    ) -> Running {
        let (commands, rx) = mpsc::channel();
        let (events, event_rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(None));
        let sink = Arc::new(Mutex::new(vec![]));
        let worker_status = status.clone();
        let worker_sink = sink.clone();
        let worker = thread::spawn(move || -> Result<()> {
            let writer = LogWriter::new(
                &path,
                serde_json::json!({"test":"pair_session","os_input":false}),
            )?;
            let result = (|| -> Result<()> {
                let prepared = match PairConnection::establish(
                    stream()?,
                    &identity,
                    initiator,
                    hello(&identity),
                    initiator.then(proposal),
                    &stop,
                    writer.log.clone(),
                )? {
                    EstablishOutcome::Session(p) => p,
                    EstablishOutcome::InventoryServed => bail!("unexpected inventory query"),
                };
                thread::sleep(delay); // Simulate native preparation longer than the input lease.
                let source = SharedSource::new(prepared.runtime().clone())?;
                source.observe(
                    SourceEvent::Snapshot {
                        buttons: vec![],
                        keys: vec![],
                    },
                    writer.log.ns(),
                )?;
                let mut host = MockHost {
                    source: source.clone(),
                    commands: rx,
                    events: event_rx,
                    status: worker_status,
                };
                prepared.run(
                    &source,
                    &mut MockSink(worker_sink),
                    &mut host,
                    &stop,
                    writer.log.clone(),
                )
            })();
            writer.finish()?;
            result
        });
        Running {
            commands,
            events,
            status,
            sink,
            worker: Some(worker),
        }
    }
    #[test]
    fn readiness_batch_preserves_tail_and_rejects_pre_ready_or_duplicate_records() {
        let at = Instant::now();
        let record = |sequence, message| Received {
            sequence,
            message,
            received_at: at,
        };
        let ready = || record(10, Message::SessionReady { revision: 1 });
        let mut received = false;
        let tail = accept_ready_batch(
            vec![
                ready(),
                record(11, Message::KeepAlive),
                record(12, Message::Enable),
            ],
            1,
            true,
            &mut received,
        )
        .unwrap();
        assert!(received);
        assert_eq!(
            tail.iter().map(|r| r.sequence).collect::<Vec<_>>(),
            [11, 12]
        );
        assert!(tail.iter().all(|r| r.received_at == at));
        assert_eq!(tail[0].message, Message::KeepAlive);
        assert_eq!(tail[1].message, Message::Enable);
        for records in [
            vec![record(9, Message::Enable), ready()],
            vec![ready(), ready()],
            vec![ready(), record(11, Message::KeepAlive), ready()],
        ] {
            assert!(accept_ready_batch(records, 1, true, &mut false).is_err());
        }
        assert!(
            accept_ready_batch(
                vec![ready(), record(11, Message::Enable)],
                1,
                false,
                &mut false
            )
            .is_err()
        );
        assert!(accept_ready_batch(vec![ready()], 2, true, &mut false).is_err());
        assert!(accept_ready_batch(vec![ready()], 1, true, &mut true).is_err());
        let oversized = (0..65).map(|i| record(i, Message::KeepAlive)).collect();
        assert!(accept_ready_batch(oversized, 1, true, &mut true).is_err());
    }

    #[test]
    #[ignore = "real encrypted localhost readiness race; mock native adapters only"]
    fn native_readiness_retains_runtime_tail_across_encrypted_pump_boundaries() -> Result<()> {
        struct GatedHost {
            pulses: usize,
            go: Sender<()>,
            sent: Receiver<()>,
        }
        impl Host for GatedHost {
            fn pulse(&mut self, _now: u64) -> Result<()> {
                self.pulses += 1;
                if self.pulses == 2 {
                    self.go.send(())?;
                    self.sent.recv_timeout(Duration::from_secs(2))?;
                }
                Ok(())
            }
            fn poll_command(&mut self) -> Result<Option<Command>> {
                Ok(None)
            }
            fn publish(&mut self, _status: &SessionStatus) -> Result<()> {
                Ok(())
            }
        }
        let fixture = fixture()?;
        let initiator = Identity::load(&fixture.0.join("identities/windows"))?;
        let responder = Identity::load(&fixture.0.join("identities/mac"))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let (go, receive_go) = mpsc::channel();
        let (sent, receive_sent) = mpsc::channel();
        let (finished, receive_finished) = mpsc::channel();
        let server_log = LogWriter::new(&fixture.0.join("server.jsonl"), serde_json::json!({}))?;
        let server = thread::spawn(move || -> Result<()> {
            let EstablishOutcome::Session(mut connection) = PairConnection::establish(
                listener.accept()?.0,
                &responder,
                false,
                hello(&responder),
                None,
                &AtomicBool::new(false),
                server_log.log.clone(),
            )?
            else {
                bail!("unexpected inventory query")
            };
            // Wait until the client has transmitted Ready, then wait for its
            // next native pulse before sending both records. TCP may deliver
            // them in one pump or separate pumps despite both writes finishing.
            let end = Instant::now() + Duration::from_secs(2);
            while !connection.early_peer_ready {
                let records = connection.peer.pump()?;
                if !records.is_empty() {
                    ensure!(
                        records.len() == 1
                            && records[0].message == Message::SessionReady { revision: 1 }
                    );
                    break;
                }
                ensure!(Instant::now() < end, "client Ready missing");
                thread::yield_now();
            }
            receive_go.recv_timeout(Duration::from_secs(2))?;
            for message in [Message::SessionReady { revision: 1 }, Message::KeepAlive] {
                connection.peer.queue(message)?;
                flush_setup(&mut connection.peer, &AtomicBool::new(false), end)?;
            }
            sent.send(())?;
            receive_finished.recv_timeout(Duration::from_secs(2))?;
            server_log.finish()
        });
        let client_log = LogWriter::new(&fixture.0.join("client.jsonl"), serde_json::json!({}))?;
        let outcome = (|| -> Result<()> {
            let EstablishOutcome::Session(mut connection) = PairConnection::establish(
                TcpStream::connect(address)?,
                &initiator,
                true,
                hello(&initiator),
                Some(proposal()),
                &AtomicBool::new(false),
                client_log.log.clone(),
            )?
            else {
                bail!("unexpected inventory query")
            };
            let source = SharedSource::new(connection.runtime().clone())?;
            source.observe(
                SourceEvent::Snapshot {
                    buttons: vec![],
                    keys: vec![],
                },
                client_log.log.ns(),
            )?;
            let mut sink = MockSink(Arc::new(Mutex::new(vec![])));
            let mut host = GatedHost {
                pulses: 0,
                go,
                sent: receive_sent,
            };
            let started = Instant::now();
            let mut tail = connection
                .native_ready(
                    &source,
                    &mut sink,
                    &mut host,
                    &AtomicBool::new(false),
                    &client_log.log,
                )?
                .context("unexpected stop")?;
            assert!(host.pulses >= 2);
            let retained = tail
                .iter()
                .map(|record| (record.sequence, record.received_at))
                .collect::<Vec<_>>();
            let end = Instant::now() + Duration::from_secs(2);
            while tail.is_empty() {
                ensure!(Instant::now() < end, "runtime tail missing after readiness");
                tail.extend(connection.peer.pump()?);
                if tail.is_empty() {
                    thread::yield_now();
                }
            }
            assert_eq!(tail.len(), 1);
            assert_eq!(tail[0].message, Message::KeepAlive);
            // Responder records: Hello, LayoutReady, SessionReady, KeepAlive.
            assert_eq!(tail[0].sequence, 4);
            assert!(tail[0].received_at >= started);
            assert!(tail[0].received_at <= Instant::now());
            assert_eq!(
                retained,
                tail.iter()
                    .take(retained.len())
                    .map(|record| (record.sequence, record.received_at))
                    .collect::<Vec<_>>()
            );
            let mut endpoint = PairEndpoint::new(
                connection.local_side(),
                1,
                source,
                sink,
                client_log.log.ns(),
            )?;
            for record in tail {
                endpoint.receive(record.message, client_log.log.ns())?;
            }
            assert!(!endpoint.status().enabled);
            assert!(!endpoint.status().input_active);
            endpoint.shutdown()?;
            Ok(())
        })();
        let _ = finished.send(());
        let server_result = server.join().unwrap();
        client_log.finish()?;
        if outcome.is_err() || server_result.is_err() {
            eprintln!("readiness client={outcome:?}, server={server_result:?}");
        }
        outcome?;
        server_result
    }

    #[test]
    #[ignore = "real encrypted localhost session; simulated physical input and mock sinks only"]
    fn full_duplex_session_waits_for_native_ready_then_routes_both_physical_sources() -> Result<()>
    {
        let fixture = fixture()?;
        let initiator = Identity::load(&fixture.0.join("identities/windows"))?;
        let responder = Identity::load(&fixture.0.join("identities/mac"))?;
        let initiator_id = initiator.public_key_hex();
        let responder_id = responder.public_key_hex();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut b = spawn_runtime(
            move || Ok(listener.accept()?.0),
            responder,
            false,
            fixture.0.join("b.jsonl"),
            stop.clone(),
            Duration::from_millis(300),
        );
        let mut a = spawn_runtime(
            move || Ok(TcpStream::connect(address)?),
            initiator,
            true,
            fixture.0.join("a.jsonl"),
            stop.clone(),
            Duration::ZERO,
        );
        let outcome = (|| -> Result<()> {
            let initial = a.wait(|s| s.capture_ready && !s.endpoint.enabled)?;
            b.wait(|s| s.capture_ready && !s.endpoint.enabled)?;
            a.commands.send(Command::Enable {
                epoch: initial.endpoint.epoch,
            })?;
            a.wait(|s| s.input_source_device_id.as_deref() == Some(&initiator_id))?;
            b.wait(|s| s.input_source_device_id.as_deref() == Some(&initiator_id))?;
            a.movement(95., 0.)?;
            a.movement(99., 4.)?;
            a.movement(99., 6.)?;
            b.has("position")?;
            a.events.send(SourceEvent::Key {
                usage: 0x04,
                down: true,
            })?;
            b.has("down")?;
            // Real local activity at the target takes over; old remote keys
            // are released before its source can become authoritative.
            b.movement(5., 1.)?;
            b.wait(|s| s.input_source_device_id.as_deref() == Some(&responder_id))?;
            a.wait(|s| s.input_source_device_id.as_deref() == Some(&responder_id))?;
            b.movement(1., -4.)?;
            b.movement(0., -1.)?;
            b.movement(0., -6.)?;
            a.has("position")?;
            b.events.send(SourceEvent::Key {
                usage: 0x05,
                down: true,
            })?;
            a.has("down")?;
            b.commands.send(Command::Pause)?;
            a.wait(|s| !s.endpoint.enabled && !s.endpoint.input_active)?;
            b.wait(|s| !s.endpoint.enabled && !s.endpoint.input_active)?;
            ensure!(
                a.sink.lock().unwrap().contains(&"release")
                    && b.sink.lock().unwrap().contains(&"release")
            );
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let a_result = a.worker.take().unwrap().join().unwrap();
        let b_result = b.worker.take().unwrap().join().unwrap();
        if outcome.is_err() {
            eprintln!("runtime A={a_result:?}, B={b_result:?}");
        }
        outcome?;
        for path in [fixture.0.join("a.jsonl"), fixture.0.join("b.jsonl")] {
            let log = fs::read_to_string(path)?;
            ensure!(
                log.lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .any(|v| v["event"] == "pair_session_cleanup" && v["confirmed"] == true),
                "cleanup evidence missing"
            );
        }
        Ok(())
    }
    #[test]
    fn diagnostic_context_clears_after_failure_and_dispatch_does_not_imply_a_call() -> Result<()> {
        let path = std::env::current_dir()?.join("runs").join(format!(
            "pair-diagnostic-context-{}-{}.jsonl",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let writer = LogWriter::new(&path, serde_json::json!({"os_input":false}))?;
        let diagnostics = SessionDiagnostics {
            connection_id: 17,
            current: Cell::new(None),
            log: writer.log.clone(),
        };
        let trace = InputTrace {
            source_side: Side::A,
            owner_epoch: 2,
            layout_revision: 1,
            seq: 5,
            kind: Kind::Position,
            t0_ns: 1,
            t1_ns: 2,
        };
        assert!(
            diagnostics
                .dispatch(Some(trace), Instant::now(), || bail!(
                    "mock rejected dispatch"
                ))
                .is_err()
        );
        assert!(diagnostics.current.get().is_none());
        diagnostics.dispatch(Some(InputTrace { seq: 6, ..trace }), Instant::now(), || {
            Ok(())
        })?;
        assert!(diagnostics.current.get().is_none());
        let mut sink = MockSink(Arc::new(Mutex::new(vec![])));
        let mut borrowed = BorrowedSink {
            sink: &mut sink,
            log: writer.log.clone(),
            diagnostics: &diagnostics,
        };
        // An unrelated later call must never inherit the failed input's IDs.
        borrowed.position(1., 2., 7)?;
        writer.finish()?;
        let events = fs::read_to_string(&path)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let received = events
            .iter()
            .filter(|v| v["event"] == "pair_input_received")
            .collect::<Vec<_>>();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0]["success"], false);
        assert_eq!(received[1]["success"], true);
        let calls = events
            .iter()
            .filter(|v| v["event"] == "pair_input_call")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].get("seq").is_none());
        assert!(calls[0]["frame_kind"].is_null());
        fs::remove_file(path)?;
        Ok(())
    }

    #[test]
    #[ignore = "encrypted localhost diagnostics/reconnect; simulated capture and mock sinks only"]
    fn duplex_diagnostics_link_calls_preserve_failures_and_distinguish_reconnects() -> Result<()> {
        let fixture = fixture()?;
        let mut previous_ids = None;
        for fail_position in [false, true] {
            let initiator = Identity::load(&fixture.0.join("identities/windows"))?;
            let responder = Identity::load(&fixture.0.join("identities/mac"))?;
            let initiator_id = initiator.public_key_hex();
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let stop = Arc::new(AtomicBool::new(false));
            let a_path = fixture.0.join(format!("source-{fail_position}.jsonl"));
            let b_path = fixture.0.join(format!("target-{fail_position}.jsonl"));
            let mut b = spawn_runtime(
                move || Ok(listener.accept()?.0),
                responder,
                false,
                b_path.clone(),
                stop.clone(),
                Duration::ZERO,
            );
            let mut a = spawn_runtime(
                move || Ok(TcpStream::connect(address)?),
                initiator,
                true,
                a_path.clone(),
                stop.clone(),
                Duration::ZERO,
            );
            let outcome = (|| -> Result<()> {
                let initial = a.wait(|s| s.capture_ready && !s.endpoint.enabled)?;
                b.wait(|s| s.capture_ready && !s.endpoint.enabled)?;
                if fail_position {
                    b.sink.lock().unwrap().push("fail-position");
                }
                a.commands.send(Command::Enable {
                    epoch: initial.endpoint.epoch,
                })?;
                a.wait(|s| s.input_source_device_id.as_deref() == Some(&initiator_id))?;
                b.wait(|s| s.input_source_device_id.as_deref() == Some(&initiator_id))?;
                a.movement(95., 0.)?;
                a.movement(99., 4.)?;
                a.movement(99., 6.)?;
                b.has("position")?;
                if fail_position {
                    let end = Instant::now() + Duration::from_secs(2);
                    while !b.worker.as_ref().unwrap().is_finished() {
                        ensure!(Instant::now() < end, "failed sink did not end runtime");
                        thread::yield_now();
                    }
                }
                Ok(())
            })();
            stop.store(true, Ordering::Release);
            let a_result = a.worker.take().unwrap().join().unwrap();
            let b_result = b.worker.take().unwrap().join().unwrap();
            if outcome.is_err() {
                eprintln!("diagnostics source={a_result:?}, target={b_result:?}");
            }
            outcome?;
            if fail_position {
                assert!(b_result.is_err());
            }
            let read = |path: &PathBuf| -> Result<Vec<serde_json::Value>> {
                fs::read_to_string(path)?
                    .lines()
                    .map(|line| Ok(serde_json::from_str(line)?))
                    .collect()
            };
            let source_events = read(&a_path)?;
            let target_events = read(&b_path)?;
            let started = |events: &[serde_json::Value]| -> Result<u64> {
                let records = events
                    .iter()
                    .filter(|v| v["event"] == "pair_session_started")
                    .collect::<Vec<_>>();
                ensure!(records.len() == 1);
                ensure!(records[0]["connection_id_scope"] == "process_local");
                records[0]["connection_id"]
                    .as_u64()
                    .context("missing connection ID")
            };
            let ids = (started(&source_events)?, started(&target_events)?);
            if let Some((a, b)) = previous_ids {
                assert_ne!(a, ids.0);
                assert_ne!(b, ids.1);
            }
            previous_ids = Some(ids);
            let queued = source_events
                .iter()
                .filter(|v| v["event"] == "pair_input_queued")
                .collect::<Vec<_>>();
            assert!(queued.iter().any(|v| v["kind"] == "Position"));
            for value in queued {
                assert_eq!(value["connection_id"], ids.0);
                assert_eq!(value["stage"], "transport_queue");
                assert_eq!(value["transport_pending_records"], 1);
                assert!(value["endpoint_pending_records"].as_u64().is_some());
                assert!(value["t0_ns"].as_u64().unwrap() <= value["t1_ns"].as_u64().unwrap());
                for forbidden in [
                    "x", "y", "code", "usage", "text", "down", "wheel_x", "wheel_y",
                ] {
                    assert!(value.get(forbidden).is_none());
                }
            }
            let calls = target_events
                .iter()
                .filter(|v| v["event"] == "pair_input_call")
                .collect::<Vec<_>>();
            assert!(
                calls
                    .iter()
                    .any(|v| v["kind"] == "Position" && v["success"] == !fail_position)
            );
            for call in calls {
                let received = target_events
                    .iter()
                    .filter(|v| {
                        v["event"] == "pair_input_received"
                            && [
                                "connection_id",
                                "source_side",
                                "owner_epoch",
                                "layout_revision",
                                "seq",
                            ]
                            .iter()
                            .all(|field| v[*field] == call[*field])
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    received.len(),
                    1,
                    "call must link to exactly one local dispatch"
                );
                let received = received[0];
                assert_eq!(call["connection_id"], ids.1);
                assert_eq!(received["kind"], call["frame_kind"]);
                assert_eq!(received["stage"], "endpoint_dispatch");
                assert!(received["success"].is_boolean());
                assert!(received["t3_ns"].as_u64().unwrap() <= call["t3_ns"].as_u64().unwrap());
                assert!(call["t4_ns"].as_u64().unwrap() <= received["t4_ns"].as_u64().unwrap());
                if call["success"] == false {
                    assert_eq!(received["success"], false);
                }
            }
            for events in [&source_events, &target_events] {
                assert!(
                    events
                        .iter()
                        .any(|v| v["event"] == "log_summary" && v["dropped_records"] == 0)
                );
            }
        }
        Ok(())
    }

    #[test]
    #[ignore = "authenticated localhost inventory query; never constructs capture or sink"]
    fn inventory_only_query_closes_successfully_without_preparing_input() -> Result<()> {
        let fixture = fixture()?;
        let initiator = Identity::load(&fixture.0.join("identities/windows"))?;
        let responder = Identity::load(&fixture.0.join("identities/mac"))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server_log = LogWriter::new(&fixture.0.join("server.jsonl"), serde_json::json!({}))?;
        let server = thread::spawn(move || -> Result<()> {
            let result = PairConnection::establish(
                listener.accept()?.0,
                &responder,
                false,
                hello(&responder),
                None,
                &AtomicBool::new(false),
                server_log.log.clone(),
            )?;
            ensure!(matches!(result, EstablishOutcome::InventoryServed));
            server_log.finish()
        });
        let client_log = LogWriter::new(&fixture.0.join("client.jsonl"), serde_json::json!({}))?;
        let inventory = query_inventory(
            TcpStream::connect(address)?,
            &initiator,
            hello(&initiator),
            &AtomicBool::new(false),
            client_log.log.clone(),
        )?;
        assert_eq!(inventory, hello(&initiator).inventory);
        client_log.finish()?;
        server.join().unwrap()?;
        Ok(())
    }

    #[test]
    #[ignore = "encrypted localhost preparation/cleanup; mock adapters only"]
    fn preparation_stop_is_persistent_and_cleanup_failure_is_reported() -> Result<()> {
        for fail_cleanup in [false, true] {
            let fixture = fixture()?;
            let initiator = Identity::load(&fixture.0.join("identities/windows"))?;
            let responder = Identity::load(&fixture.0.join("identities/mac"))?;
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?;
            let stop = Arc::new(AtomicBool::new(false));
            let mut b = spawn_runtime(
                move || Ok(listener.accept()?.0),
                responder,
                false,
                fixture.0.join("b.jsonl"),
                stop.clone(),
                Duration::from_millis(200),
            );
            let mut a = spawn_runtime(
                move || Ok(TcpStream::connect(address)?),
                initiator,
                true,
                fixture.0.join("a.jsonl"),
                stop.clone(),
                Duration::ZERO,
            );
            if fail_cleanup {
                a.wait(|s| s.capture_ready)?;
                b.wait(|s| s.capture_ready)?;
                a.sink.lock().unwrap().push("fail-release");
            }
            // Before readiness this is consumed by native_ready; afterwards
            // by run. In both cases the outer app must observe a permanent Stop.
            a.commands.send(Command::Stop)?;
            let a_result = a.worker.take().unwrap().join().unwrap();
            let _b_result = b.worker.take().unwrap().join().unwrap();
            assert!(stop.load(Ordering::Acquire));
            if fail_cleanup {
                assert!(format!("{:#}", a_result.unwrap_err()).contains("cleanup unconfirmed"));
            } else {
                a_result?;
                assert!(a.sink.lock().unwrap().iter().all(|e| *e == "release"));
            }
            let log = fs::read_to_string(fixture.0.join("a.jsonl"))?;
            assert!(
                log.lines()
                    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    .any(
                        |v| v["event"] == "pair_session_cleanup" && v["confirmed"] == !fail_cleanup
                    )
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "real encrypted localhost batch coalescing; mock adapters only"]
    fn encrypted_batch_coalesces_consecutive_positions_into_latest_sink_call() -> Result<()> {
        use crate::protocol::Frame;
        let fixture = fixture()?;
        let initiator = Identity::load(&fixture.0.join("identities/windows"))?;
        let responder = Identity::load(&fixture.0.join("identities/mac"))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));

        struct RecordingSink {
            positions: Arc<Mutex<Vec<(f64, f64, u32)>>>,
            armed_tx: Option<Sender<()>>,
        }
        impl MouseSink for RecordingSink {
            fn reset(&mut self) -> Result<()> {
                if let Some(tx) = self.armed_tx.take() {
                    let _ = tx.send(());
                }
                Ok(())
            }
            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                Ok((x, y))
            }
            fn position(&mut self, x: f64, y: f64, id: u32) -> Result<(f64, f64)> {
                self.positions.lock().unwrap().push((x, y, id));
                Ok((x, y))
            }
            fn release_owned(&mut self) -> Result<()> {
                Ok(())
            }
        }
        struct BatchGateHost {
            gate: Option<Receiver<()>>,
            parked: Option<Sender<()>>,
            armed: Arc<AtomicBool>,
            commands: Receiver<Command>,
        }
        impl Host for BatchGateHost {
            fn pulse(&mut self, _now: u64) -> Result<()> {
                if self.armed.load(Ordering::Acquire) {
                    if let Some(rx) = self.gate.take() {
                        if let Some(tx) = self.parked.take() {
                            let _ = tx.send(());
                        }
                        rx.recv_timeout(Duration::from_secs(2))?;
                    }
                }
                Ok(())
            }
            fn poll_command(&mut self) -> Result<Option<Command>> {
                Ok(self.commands.try_recv().ok())
            }
            fn publish(&mut self, _status: &SessionStatus) -> Result<()> {
                Ok(())
            }
        }

        let positions = Arc::new(Mutex::new(Vec::new()));
        let b_positions = positions.clone();
        let b_path = fixture.0.join("b.jsonl");
        let b_stop = stop.clone();
        let (parked_tx, parked_rx) = mpsc::channel();
        let (batch_tx, batch_rx) = mpsc::channel();
        let (b_cmd_tx, b_cmd_rx) = mpsc::channel();
        let server = thread::spawn(move || -> Result<()> {
            let writer = LogWriter::new(&b_path, serde_json::json!({"os_input":false}))?;
            let result = (|| -> Result<()> {
                let EstablishOutcome::Session(prepared) = PairConnection::establish(
                    listener.accept()?.0,
                    &responder,
                    false,
                    hello(&responder),
                    None,
                    &b_stop,
                    writer.log.clone(),
                )?
                else {
                    bail!("unexpected inventory query")
                };
                let source = SharedSource::new(prepared.runtime().clone())?;
                source.observe(
                    SourceEvent::Snapshot {
                        buttons: vec![],
                        keys: vec![],
                    },
                    writer.log.ns(),
                )?;
                let armed_flag = Arc::new(AtomicBool::new(false));
                let flag_clone = armed_flag.clone();
                let (inner_armed_tx, inner_armed_rx) = mpsc::channel();
                thread::spawn(move || {
                    if inner_armed_rx.recv().is_ok() {
                        flag_clone.store(true, Ordering::Release);
                    }
                });
                let mut sink = RecordingSink {
                    positions: b_positions,
                    armed_tx: Some(inner_armed_tx),
                };
                let mut host = BatchGateHost {
                    gate: Some(batch_rx),
                    parked: Some(parked_tx),
                    armed: armed_flag,
                    commands: b_cmd_rx,
                };
                prepared.run(&source, &mut sink, &mut host, &b_stop, writer.log.clone())
            })();
            writer.finish()?;
            result
        });

        let a_log = LogWriter::new(&fixture.0.join("a.jsonl"), serde_json::json!({}))?;
        let outcome = (|| -> Result<()> {
            let EstablishOutcome::Session(mut connection_a) = PairConnection::establish(
                TcpStream::connect(address)?,
                &initiator,
                true,
                hello(&initiator),
                Some(proposal()),
                &stop,
                a_log.log.clone(),
            )?
            else {
                bail!("unexpected inventory query")
            };
            let source_a = SharedSource::new(connection_a.runtime().clone())?;
            source_a.observe(
                SourceEvent::Snapshot {
                    buttons: vec![],
                    keys: vec![],
                },
                a_log.log.ns(),
            )?;
            struct DummySink;
            impl MouseSink for DummySink {
                fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                    Ok((x, y))
                }
                fn release_owned(&mut self) -> Result<()> {
                    Ok(())
                }
            }
            struct DummyHost;
            impl Host for DummyHost {
                fn pulse(&mut self, _: u64) -> Result<()> {
                    Ok(())
                }
                fn poll_command(&mut self) -> Result<Option<Command>> {
                    Ok(None)
                }
                fn publish(&mut self, _: &SessionStatus) -> Result<()> {
                    Ok(())
                }
            }
            let _ = connection_a
                .native_ready(&source_a, &mut DummySink, &mut DummyHost, &stop, &a_log.log)?
                .context("unexpected stop")?;
            let mut endpoint_a = PairEndpoint::new(
                connection_a.local_side(),
                1,
                source_a,
                DummySink,
                a_log.log.ns(),
            )?;
            endpoint_a.enable_local(a_log.log.ns())?;
            let end = Instant::now() + Duration::from_secs(2);
            while !endpoint_a.lease_ready(a_log.log.ns()) {
                ensure!(Instant::now() < end, "sender lease timed out");
                while let Some(m) = endpoint_a.pop_outbound_at(a_log.log.ns())? {
                    connection_a.peer.queue(m)?;
                    connection_a.peer.flush_pending()?;
                }
                for r in connection_a.peer.pump()? {
                    endpoint_a.receive(r.message, a_log.log.ns())?;
                }
                thread::sleep(Duration::from_millis(1));
            }
            // Send Arm and wait until B processes Arm and parks at the batch gate.
            let mut arm = Frame::new(Kind::Arm);
            arm.t0_ns = a_log.log.ns();
            assert!(endpoint_a.send_input(arm, arm.t0_ns)?);
            while let Some(m) = endpoint_a.pop_outbound_at(a_log.log.ns())? {
                connection_a.peer.queue(m)?;
                connection_a.peer.flush_pending()?;
            }
            parked_rx.recv_timeout(Duration::from_secs(2))?;
            // Pump B's Lease messages, then flush 3 Position frames back-to-back
            // while B's pulse is parked so they arrive in a single pump().
            for r in connection_a.peer.pump()? {
                endpoint_a.receive(r.message, a_log.log.ns())?;
            }
            for (x, y) in [(10.0, 10.0), (25.0, 35.0), (60.0, 70.0)] {
                let mut f = Frame::new(Kind::Position);
                f.display_id = 7;
                f.x = x;
                f.y = y;
                f.t0_ns = a_log.log.ns();
                assert!(endpoint_a.send_input(f, f.t0_ns)?);
                let msg = endpoint_a.pop_outbound_at(a_log.log.ns())?.unwrap();
                connection_a.peer.queue(msg)?;
                connection_a.peer.flush_pending()?;
            }
            thread::sleep(Duration::from_millis(5));
            batch_tx.send(())?;
            let end = Instant::now() + Duration::from_secs(2);
            while positions.lock().unwrap().is_empty() {
                ensure!(Instant::now() < end, "coalesced position timed out");
                for r in connection_a.peer.pump()? {
                    endpoint_a.receive(r.message, a_log.log.ns())?;
                }
                thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(*positions.lock().unwrap(), vec![(60.0, 70.0, 7)]);
            let _ = b_cmd_tx.send(Command::Stop);
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let _ = batch_tx.send(());
        let server_res = server.join().unwrap();
        a_log.finish()?;
        outcome?;
        server_res?;

        let events = fs::read_to_string(fixture.0.join("b.jsonl"))?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let received_positions = events
            .iter()
            .filter(|v| v["event"] == "pair_input_received" && v["kind"] == "Position")
            .collect::<Vec<_>>();
        let call_positions = events
            .iter()
            .filter(|v| v["event"] == "pair_input_call" && v["kind"] == "Position")
            .collect::<Vec<_>>();
        assert_eq!(received_positions.len(), 3);
        assert!(received_positions.iter().all(|v| v["success"] == true));
        assert_eq!(call_positions.len(), 1);
        assert_eq!(call_positions[0]["seq"], received_positions[2]["seq"]);
        assert_eq!(call_positions[0]["success"], true);
        Ok(())
    }
}
