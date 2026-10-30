//! Shared duplex runtime. OS capture/UI adapters implement Host; input authority
//! stays in PairEndpoint and all network traffic uses one pinned Noise socket.
use crate::{
    input::Button,
    metrics::Log,
    pair_control::Side,
    pair_endpoint::{EndpointStatus, LocalSource, PairEndpoint},
    pair_layout::PairLayout,
    pair_source::SharedSource,
    pair_wire::{Display, Hello, Message, PairPeer},
    protocol::Kind,
    screen_runtime::RuntimeLayout,
    security::Identity,
    session::{MouseSink, NativeDisplay, WorkerTiming},
    session_metadata::{RoutingState, SharedLayout},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::VecDeque,
    net::TcpStream,
    sync::atomic::{AtomicBool, Ordering},
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
    ) -> Result<bool> {
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
                return Ok(false);
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
            for record in self.peer.pump()? {
                ensure!(
                    matches!(record.message,Message::SessionReady{revision} if revision==self.revision)
                        && !received,
                    "unexpected record before native readiness"
                );
                received = true;
            }
            if sent && received && !self.peer.pending() {
                return Ok(true);
            }
            thread::sleep(Duration::from_micros(500));
        }
    }
}

struct BorrowedSink<'a> {
    sink: &'a mut dyn MouseSink,
    log: Log,
}
impl BorrowedSink<'_> {
    fn completed(&mut self, kind: &str, start: u64, success: bool) {
        let end = self.log.ns();
        let timing = self.sink.take_timing();
        self.log.write("pair_input_call",serde_json::json!({"kind":kind,"t3_ns":start,"t4_ns":end,"success":success,"worker_timing":timing}));
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
        let initial = (|| -> Result<bool> {
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
        if !matches!(initial, Ok(true)) {
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
        let local_side = self.local_side();
        let created = PairEndpoint::new(
            local_side,
            self.revision,
            source.clone(),
            BorrowedSink {
                sink,
                log: log.clone(),
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
                let records = self.peer.pump()?;
                for record in records {
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
                    if let Message::Input { frame, .. } = &record.message {
                        if frame.kind == Kind::RoutingState {
                            self.shared_layout().routing_state(frame)?;
                        }
                    }
                    let timing = match &record.message {
                        Message::Input {
                            owner_epoch, frame, ..
                        } => Some((*owner_epoch, frame.seq, format!("{:?}", frame.kind))),
                        _ => None,
                    };
                    let began = log.ns();
                    endpoint.receive(record.message, log.ns())?;
                    if let Some((epoch, seq, kind)) = timing {
                        log.write("pair_input_received",serde_json::json!({"owner_epoch":epoch,"seq":seq,"kind":kind,"t2_ns":log.at(record.received_at),"t3_ns":began,"t4_ns":log.ns()}));
                    }
                }
                if self.peer.pending_input() && !endpoint.lease_ready(log.ns()) {
                    bail!("remote revoke discarded pending transport input");
                }
                endpoint.tick(log.ns())?;
                let state = endpoint.status();
                if state.epoch != authority {
                    authority = state.epoch;
                    sent_routing = None;
                    last_motion = None;
                }
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
                        sent_routing = Some(routing);
                    }
                }
                if !self.peer.pending() {
                    if let Some(message) = endpoint.pop_outbound_at(log.ns())? {
                        self.peer.queue(message)?;
                    }
                }
                let status = self.session_status(&endpoint, source)?;
                if last_status.as_ref() != Some(&status)
                    || last_publish.elapsed() >= Duration::from_millis(100)
                {
                    host.publish(&status)?;
                    last_status = Some(status);
                    last_publish = Instant::now();
                }
                thread::sleep(Duration::from_micros(500));
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
            self.0.lock().unwrap().push("position");
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
}
