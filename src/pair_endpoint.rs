//! Pure orchestration for one already authenticated, layout-validated pair.
//! Call methods on one runtime thread, with processing-time monotonic timestamps.
//! No socket, timer thread or metadata message can independently authorize input.

use crate::{
    pair_control::{Effect, Event, PairControl, RevokeReason, Side},
    pair_wire::Message,
    protocol::{Frame, Kind, button_from_code},
    session::MouseSink,
    state::ReceiverState,
};
use anyhow::{Result, bail, ensure};
use std::collections::VecDeque;

pub const LEASE_PERIOD_NS: u64 = 50_000_000;
pub const LEASE_TTL_NS: u64 = 1_500_000_000;
pub const KEEPALIVE_PERIOD_NS: u64 = 80_000_000;
pub const BARRIER_TIMEOUT_NS: u64 = 2_000_000_000;
pub const OUTBOUND_CAPACITY: usize = 256;

/// Return only after local suppression is undone and old queued capture is
/// discarded. Implementations must not retain source locks across these calls.
pub trait LocalSource {
    fn revoke(&mut self) -> Result<()>;
    /// This enables routing/capture authority, not immediate remote suppression.
    fn grant(&mut self, owner_epoch: u64, layout_revision: u64) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointStatus {
    pub connected: bool,
    pub enabled: bool,
    pub owner: Option<Side>,
    pub epoch: u64,
    pub layout_revision: u64,
    pub input_active: bool,
}
#[derive(Clone, Copy)]
struct Barrier {
    epoch: u64,
    request_epoch: u64,
    started: u64,
    locally_released: bool,
    acquire: bool,
}

pub struct PairEndpoint<S: LocalSource, K: MouseSink> {
    side: Side,
    coordinator: Option<PairControl>,
    source: S,
    sink: K,
    status: EndpointStatus,
    barrier: Option<Barrier>,
    pending_request: bool,
    deferred_request: bool,
    discard_through: Option<u64>,
    receiver: ReceiverState,
    coalesced_move: Option<(f64, f64)>,
    routing: Option<Frame>,
    outbound: VecDeque<Message>,
    now: u64,
    last_peer: u64,
    grant_at: u64,
    last_input: Option<u64>,
    last_keepalive: u64,
    last_lease_sent: Option<u64>,
    last_heartbeat: Option<u64>,
    next_lease: u64,
    last_peer_token: u64,
    peer_lease: Option<(u64, u64)>,
    input_seq: u64,
}
impl<S: LocalSource, K: MouseSink> PairEndpoint<S, K> {
    /// `side` is assigned by authenticated identity order, never TCP role.
    /// Hello and the exact layout revision must already have been accepted.
    pub fn new(
        side: Side,
        layout_revision: u64,
        mut source: S,
        mut sink: K,
        now_ns: u64,
    ) -> Result<Self> {
        let coordinator = PairControl::new(layout_revision)?;
        let source_ok = source.revoke().is_ok();
        let sink_ok = sink.release_owned().is_ok();
        ensure!(source_ok && sink_ok, "initial pair cleanup failed");
        Ok(Self {
            side,
            coordinator: (side == Side::A).then_some(coordinator),
            source,
            sink,
            status: EndpointStatus {
                connected: true,
                enabled: false,
                owner: None,
                epoch: 0,
                layout_revision,
                input_active: false,
            },
            barrier: None,
            pending_request: false,
            deferred_request: false,
            discard_through: None,
            receiver: ReceiverState::new(LEASE_TTL_NS),
            coalesced_move: None,
            routing: None,
            outbound: VecDeque::with_capacity(OUTBOUND_CAPACITY),
            now: now_ns,
            last_peer: now_ns,
            grant_at: now_ns,
            last_input: None,
            last_keepalive: now_ns,
            last_lease_sent: None,
            last_heartbeat: None,
            next_lease: 0,
            last_peer_token: 0,
            peer_lease: None,
            input_seq: 0,
        })
    }
    pub fn status(&self) -> EndpointStatus {
        self.status
    }
    /// Observational metadata only; the caller validates its screen index
    /// against the already accepted layout. It never grants or arms input.
    pub fn routing_frame(&self) -> Option<Frame> {
        self.routing
    }
    pub fn pop_outbound(&mut self) -> Option<Message> {
        self.outbound.pop_front()
    }
    /// Runtime egress: call only when the transport has no pending record.
    /// Recheck capture age and stamp t1 at dequeue, not at initial enqueue.
    pub fn pop_outbound_at(&mut self, now: u64) -> Result<Option<Message>> {
        let result = (|| {
            self.clock(now)?;
            let Some(mut message) = self.outbound.pop_front() else {
                return Ok(None);
            };
            if let Message::Input { frame, .. } = &mut message {
                ensure!(now >= frame.t0_ns, "outgoing input clock regressed");
                if matches!(
                    frame.kind,
                    Kind::Move | Kind::Position | Kind::Key | Kind::Button | Kind::Scroll
                ) {
                    ensure!(now - frame.t0_ns <= 30_000_000, "queued pair input expired");
                }
                frame.t1_ns = now;
                frame.encode()?;
            }
            Ok(Some(message))
        })();
        self.finish(result)
    }
    pub fn pending_outbound(&self) -> usize {
        self.outbound.len()
    }
    pub fn lease_ready(&self, now_ns: u64) -> bool {
        self.status.connected
            && self.status.owner == Some(self.side)
            && !self.pending_request
            && self.barrier.is_none()
            && self
                .peer_lease
                .is_some_and(|(_, at)| now_ns >= at && now_ns - at <= LEASE_TTL_NS)
    }
    fn remote(&self) -> Side {
        if self.side == Side::A {
            Side::B
        } else {
            Side::A
        }
    }
    fn clock(&mut self, now: u64) -> Result<()> {
        ensure!(self.status.connected, "pair is disconnected");
        ensure!(now >= self.now, "pair clock regressed");
        self.now = now;
        ensure!(
            now - self.last_peer <= LEASE_TTL_NS,
            "pair peer liveness expired"
        );
        if let Some(barrier) = self.barrier {
            ensure!(
                now - barrier.started <= BARRIER_TIMEOUT_NS,
                "pair release barrier expired"
            );
        }
        if self.status.owner.is_some() {
            let since = self.peer_lease.map_or(self.grant_at, |(_, at)| at);
            ensure!(now - since <= LEASE_TTL_NS, "pair input lease expired");
            if self.receiver.active {
                ensure!(
                    self.last_input.is_some_and(|at| now - at <= LEASE_TTL_NS),
                    "pair input heartbeat expired"
                );
            }
        }
        Ok(())
    }
    fn queue(&mut self, message: Message) -> Result<()> {
        ensure!(
            self.outbound.len() < OUTBOUND_CAPACITY,
            "pair outbound queue overflow"
        );
        self.outbound.push_back(message);
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        self.coalesced_move = None;
        self.routing = None;
        self.discard_through = Some(self.status.epoch);
        self.status.owner = None;
        self.status.input_active = false;
        self.peer_lease = None;
        self.last_input = None;
        self.last_lease_sent = None;
        self.last_heartbeat = None;
        self.receiver = ReceiverState::new(LEASE_TTL_NS);
        self.outbound
            .retain(|m| !matches!(m, Message::Input { .. } | Message::Lease { .. }));
        let source_ok = self.source.revoke().is_ok();
        let sink_ok = self.sink.release_owned().is_ok();
        ensure!(source_ok && sink_ok, "pair owned-input cleanup failed");
        Ok(())
    }
    fn terminate(&mut self) {
        if !self.status.connected {
            return;
        }
        self.status.connected = false;
        self.status.enabled = false;
        self.barrier = None;
        self.pending_request = false;
        let cleanup_ok = self.cleanup().is_ok();
        self.outbound.clear();
        // A protocol/adapter failure is terminal. The runtime must close the
        // connection after best-effort notification; this never grants authority.
        self.outbound.push_back(if self.side == Side::A {
            Message::Ended {
                reason: if cleanup_ok {
                    RevokeReason::Disconnect
                } else {
                    RevokeReason::ReleaseFailed { side: self.side }
                },
            }
        } else {
            Message::ReleaseFailed
        });
    }
    fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.terminate();
        }
        result
    }
    pub fn disconnect(&mut self) {
        self.terminate();
    }
    /// Explicit completion evidence for the runtime. Unlike the best-effort
    /// disconnect/Drop paths, adapter cleanup errors are returned to the caller.
    /// Always retries local cleanup, including after a previously failed exit.
    pub fn shutdown(&mut self) -> Result<()> {
        self.status.connected = false;
        self.status.enabled = false;
        self.barrier = None;
        self.pending_request = false;
        self.deferred_request = false;
        let result = self.cleanup();
        self.outbound.clear();
        result
    }
    pub fn enable_local(&mut self, now: u64) -> Result<()> {
        let result = (|| {
            self.clock(now)?;
            ensure!(
                !self.status.enabled && self.barrier.is_none() && !self.pending_request,
                "pair sharing is already enabled or pending"
            );
            if self.side == Side::A {
                self.control(Event::Enable { source: self.side })
            } else {
                self.cleanup()?;
                self.pending_request = true;
                self.queue(Message::Enable)
            }
        })();
        self.finish(result)
    }
    pub fn request_local(&mut self, now: u64) -> Result<()> {
        let result = (|| {
            self.clock(now)?;
            ensure!(self.status.enabled, "pair sharing is disabled");
            if self.status.owner == Some(self.side) || self.pending_request {
                return Ok(());
            }
            if self.side == Side::B && self.barrier.is_some() {
                // Released is already in the ordered stream. A request sent
                // after it may reach A after Grant; defer until the new epoch.
                self.deferred_request = true;
                return Ok(());
            }
            let observed_epoch = self.barrier.map_or(self.status.epoch, |b| b.request_epoch);
            self.cleanup()?;
            self.pending_request = true;
            if self.side == Side::A {
                self.control(Event::RequestControl {
                    source: self.side,
                    observed_epoch,
                })
            } else {
                self.queue(Message::RequestControl { observed_epoch })
            }
        })();
        self.finish(result)
    }
    pub fn pause(&mut self, now: u64) -> Result<()> {
        let result = (|| {
            self.clock(now)?;
            if !self.status.enabled && self.barrier.is_none() && !self.pending_request {
                return Ok(());
            }
            self.deferred_request = false;
            self.cleanup()?;
            self.status.enabled = false;
            self.pending_request = true;
            if self.side == Side::A {
                self.control(Event::Pause)
            } else {
                self.queue(Message::Pause)
            }
        })();
        self.finish(result)
    }
    fn control(&mut self, event: Event) -> Result<()> {
        let effects = self
            .coordinator
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("coordinator-only operation"))?
            .handle(event)?;
        for effect in effects {
            match effect {
                Effect::Revoke {
                    barrier_epoch,
                    reason,
                } => {
                    self.revoke(barrier_epoch, reason)?;
                    self.queue(Message::Revoke {
                        barrier_epoch,
                        reason,
                    })?;
                    // Only successful local cleanup permits this acknowledgement.
                    if self
                        .coordinator
                        .as_ref()
                        .is_some_and(|c| c.pending_barrier() == Some(barrier_epoch))
                    {
                        self.control(Event::Released {
                            side: self.side,
                            barrier_epoch,
                        })?;
                    }
                }
                Effect::Grant {
                    source,
                    owner_epoch,
                    layout_revision,
                } => {
                    self.grant(source, owner_epoch, layout_revision)?;
                    self.queue(Message::Grant {
                        source,
                        owner_epoch,
                        layout_revision,
                    })?;
                    self.issue_lease()?;
                }
                Effect::Paused { epoch } => {
                    self.accept_paused(epoch)?;
                    self.queue(Message::Paused { epoch })?;
                }
                Effect::Ended { reason } => {
                    self.status.connected = false;
                    self.status.enabled = false;
                    self.queue(Message::Ended { reason })?;
                }
                Effect::LayoutAccepted { .. } => bail!("live layout replacement is unsupported"),
            }
        }
        Ok(())
    }
    fn revoke(&mut self, epoch: u64, reason: RevokeReason) -> Result<()> {
        ensure!(
            self.status.epoch.checked_add(1) == Some(epoch),
            "pair revoke does not match next epoch"
        );
        let previous = self.status.epoch;
        self.cleanup()?;
        self.status.epoch = epoch;
        let acquire = matches!(reason, RevokeReason::Enable | RevokeReason::Takeover);
        self.status.enabled = acquire;
        self.barrier = Some(Barrier {
            epoch,
            request_epoch: previous,
            started: self.now,
            locally_released: true,
            acquire,
        });
        self.pending_request = false;
        Ok(())
    }
    fn grant(&mut self, source: Side, epoch: u64, revision: u64) -> Result<()> {
        let barrier = self
            .barrier
            .ok_or_else(|| anyhow::anyhow!("unsolicited pair grant"))?;
        ensure!(
            barrier.locally_released
                && barrier.acquire
                && barrier.epoch == epoch
                && self.status.epoch == epoch
                && self.status.layout_revision == revision,
            "pair grant does not match completed local barrier"
        );
        ensure!(
            !self.pending_request,
            "pair grant while local revocation is pending"
        );
        self.receiver = ReceiverState::new(LEASE_TTL_NS);
        self.peer_lease = None;
        self.last_input = None;
        self.last_lease_sent = None;
        self.last_heartbeat = None;
        self.grant_at = self.now;
        if source == self.side {
            self.source
                .grant(epoch, revision)
                .map_err(|_| anyhow::anyhow!("local source grant failed"))?;
        }
        self.barrier = None;
        self.status.enabled = true;
        self.status.owner = Some(source);
        self.status.input_active = false;
        Ok(())
    }
    fn accept_paused(&mut self, epoch: u64) -> Result<()> {
        let barrier = self
            .barrier
            .ok_or_else(|| anyhow::anyhow!("unsolicited pair pause confirmation"))?;
        ensure!(
            barrier.locally_released && !barrier.acquire && barrier.epoch == epoch,
            "pair pause confirmation does not match local barrier"
        );
        self.barrier = None;
        self.pending_request = false;
        self.status.enabled = false;
        self.status.owner = None;
        Ok(())
    }
    fn issue_lease(&mut self) -> Result<()> {
        self.next_lease = self
            .next_lease
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("pair lease counter exhausted"))?;
        self.receiver.grant(self.next_lease, self.now);
        self.queue(Message::Lease {
            token: self.next_lease,
        })?;
        self.last_lease_sent = Some(self.now);
        Ok(())
    }
    pub fn receive(&mut self, message: Message, now: u64) -> Result<()> {
        self.receive_coalesced(message, false, now)
    }

    pub fn receive_coalesced(
        &mut self,
        message: Message,
        defer_motion_sink: bool,
        now: u64,
    ) -> Result<()> {
        let result = (|| {
            self.clock(now)?;
            match message {
                Message::KeepAlive => {
                    if let Some((dx, dy)) = self.coalesced_move.take() {
                        self.sink.move_by(dx, dy)?;
                    }
                }
                Message::Lease { token } => {
                    if let Some((dx, dy)) = self.coalesced_move.take() {
                        self.sink.move_by(dx, dy)?;
                    }
                    ensure!(
                        token > self.last_peer_token,
                        "duplicate or stale peer lease"
                    );
                    self.last_peer_token = token;
                    if self.status.owner.is_some()
                        && self.barrier.is_none()
                        && !self.pending_request
                    {
                        self.peer_lease = Some((token, now));
                    }
                }
                Message::Enable => {
                    ensure!(self.side == Side::A, "only coordinator accepts enable");
                    if self.coordinator.as_ref().is_some_and(PairControl::enabled) {
                        if let Some(b) = self.barrier {
                            self.control(Event::RequestControl {
                                source: self.remote(),
                                observed_epoch: b.request_epoch,
                            })?;
                        }
                    } else {
                        self.control(Event::Enable {
                            source: self.remote(),
                        })?;
                    }
                }
                Message::RequestControl { observed_epoch } => {
                    ensure!(
                        self.side == Side::A,
                        "only coordinator accepts control requests"
                    );
                    let expected = self.barrier.map_or(self.status.epoch, |b| b.request_epoch);
                    if observed_epoch >= expected {
                        self.control(Event::RequestControl {
                            source: self.remote(),
                            observed_epoch,
                        })?;
                    }
                }
                Message::Released { barrier_epoch } => {
                    ensure!(
                        self.side == Side::A,
                        "only coordinator accepts release acknowledgements"
                    );
                    if barrier_epoch >= self.status.epoch {
                        self.control(Event::Released {
                            side: self.remote(),
                            barrier_epoch,
                        })?;
                    }
                }
                Message::Pause => {
                    ensure!(
                        self.side == Side::A,
                        "only coordinator accepts pause requests"
                    );
                    self.control(Event::Pause)?;
                }
                Message::ReleaseFailed => bail!("peer owned-input cleanup failed"),
                Message::Revoke {
                    barrier_epoch,
                    reason,
                } => {
                    ensure!(self.side == Side::B, "peer cannot revoke coordinator");
                    self.revoke(barrier_epoch, reason)?;
                    self.queue(Message::Released { barrier_epoch })?;
                }
                Message::Grant {
                    source,
                    owner_epoch,
                    layout_revision,
                } => {
                    ensure!(
                        self.side == Side::B,
                        "peer cannot grant coordinator authority"
                    );
                    if self.pending_request && !self.status.enabled {
                        // A Grant already in flight before our local Pause must
                        // not undo that immediate local revocation. Still verify
                        // its completed barrier; the following Revoke will finish
                        // the pause request on the coordinator.
                        ensure!(
                            self.barrier.is_some_and(|b| b.locally_released
                                && b.acquire
                                && b.epoch == owner_epoch)
                                && owner_epoch == self.status.epoch
                                && layout_revision == self.status.layout_revision,
                            "unsolicited grant during local pause"
                        );
                    } else {
                        self.grant(source, owner_epoch, layout_revision)?;
                        self.issue_lease()?;
                        if std::mem::take(&mut self.deferred_request) && source != self.side {
                            self.request_local(now)?;
                        }
                    }
                }
                Message::Paused { epoch } => {
                    ensure!(
                        self.side == Side::B,
                        "peer cannot pause coordinator authority"
                    );
                    self.accept_paused(epoch)?;
                }
                Message::Ended { .. } => {
                    ensure!(
                        self.side == Side::B,
                        "peer cannot end coordinator authority"
                    );
                    self.cleanup()?;
                    self.status.connected = false;
                    self.status.enabled = false;
                    self.barrier = None;
                    self.outbound.clear();
                }
                Message::Input {
                    source,
                    owner_epoch,
                    layout_revision,
                    frame,
                } => self.input(
                    source,
                    owner_epoch,
                    layout_revision,
                    frame,
                    defer_motion_sink,
                )?,
                Message::Hello(_)
                | Message::Layout { .. }
                | Message::LayoutAccepted { .. }
                | Message::LayoutReady { .. }
                | Message::SessionReady { .. }
                | Message::QueryInventory
                | Message::InventoryDone => {
                    bail!("pair handshake/layout metadata is not accepted during input")
                }
            }
            self.last_peer = now;
            Ok(())
        })();
        self.finish(result)
    }
    fn input(
        &mut self,
        source: Side,
        epoch: u64,
        revision: u64,
        frame: Frame,
        defer_motion_sink: bool,
    ) -> Result<()> {
        ensure!(
            source == self.remote(),
            "input source does not match authenticated peer"
        );
        if self.discard_through.is_some_and(|old| epoch <= old) {
            return Ok(());
        }
        ensure!(
            self.status.owner == Some(source)
                && self.status.enabled
                && self.barrier.is_none()
                && !self.pending_request
                && self.status.epoch == epoch
                && self.status.layout_revision == revision,
            "input does not match pair authority"
        );
        ensure!(
            matches!(
                frame.kind,
                Kind::Arm
                    | Kind::Pause
                    | Kind::Stop
                    | Kind::Heartbeat
                    | Kind::RoutingState
                    | Kind::Move
                    | Kind::Position
                    | Kind::Key
                    | Kind::Button
                    | Kind::Scroll
            ),
            "unsupported pair input frame"
        );
        ensure!(
            !self.sink.emergency_pressed(),
            "local emergency stopped pair input"
        );
        let delta = self.receiver.apply(frame, self.now)?;
        if frame.kind == Kind::RoutingState {
            if let Some((dx, dy)) = self.coalesced_move.take() {
                self.sink.move_by(dx, dy)?;
            }
            self.routing = Some(frame);
            // Metadata passes ownership, lease and sequence checks, but cannot
            // replace the active-input heartbeat or renew the native worker.
            return Ok(());
        }
        match frame.kind {
            Kind::Arm => {
                self.coalesced_move = None;
                self.routing = None;
                self.sink.reset()?;
            }
            Kind::Pause | Kind::Stop => {
                self.coalesced_move = None;
                self.routing = None;
                self.sink.release_owned()?;
            }
            Kind::Move => {
                let (dx, dy) = delta.ok_or_else(|| anyhow::anyhow!("missing movement delta"))?;
                let (total_dx, total_dy) = match self.coalesced_move.take() {
                    Some((px, py)) => {
                        let sum_x = px + dx;
                        let sum_y = py + dy;
                        if sum_x.is_finite()
                            && sum_y.is_finite()
                            && sum_x.abs() <= 20_000.0
                            && sum_y.abs() <= 20_000.0
                        {
                            (sum_x, sum_y)
                        } else {
                            self.sink.move_by(px, py)?;
                            (dx, dy)
                        }
                    }
                    None => (dx, dy),
                };
                if defer_motion_sink {
                    self.coalesced_move = Some((total_dx, total_dy));
                    self.status.input_active = self.receiver.active;
                    self.last_input = Some(self.now);
                    return Ok(());
                }
                self.sink.move_by(total_dx, total_dy)?;
            }
            Kind::Position => {
                if let Some((dx, dy)) = self.coalesced_move.take() {
                    self.sink.move_by(dx, dy)?;
                }
                if defer_motion_sink {
                    self.status.input_active = self.receiver.active;
                    self.last_input = Some(self.now);
                    return Ok(());
                }
                self.sink.position(frame.x, frame.y, frame.display_id)?;
            }
            Kind::Key => {
                if let Some((dx, dy)) = self.coalesced_move.take() {
                    self.sink.move_by(dx, dy)?;
                }
                self.sink.key(frame.code, frame.down)?;
            }
            Kind::Button => {
                if let Some((dx, dy)) = self.coalesced_move.take() {
                    self.sink.move_by(dx, dy)?;
                }
                self.sink
                    .button(button_from_code(frame.code)?, frame.down)?;
            }
            Kind::Scroll => {
                if let Some((dx, dy)) = self.coalesced_move.take() {
                    self.sink.move_by(dx, dy)?;
                }
                self.sink.scroll(frame.wheel_x, frame.wheel_y)?;
            }
            Kind::Heartbeat => {
                if let Some((dx, dy)) = self.coalesced_move.take() {
                    self.sink.move_by(dx, dy)?;
                }
            }
            _ => unreachable!(),
        }
        self.status.input_active = self.receiver.active;
        self.last_input = Some(self.now);
        if frame.kind == Kind::Stop {
            bail!("peer stopped input connection");
        }
        // Only a validated, completed current-owner frame renews worker health.
        if self.receiver.active && frame.kind != Kind::Pause {
            self.sink.maintain()?;
        }
        Ok(())
    }
    /// Assign sequence, processing timestamp and the current receiver lease.
    /// False means capture was discarded while not authoritative/lease-ready.
    pub fn send_input(&mut self, mut frame: Frame, now: u64) -> Result<bool> {
        let result = (|| {
            self.clock(now)?;
            if !self.lease_ready(now) {
                return Ok(false);
            }
            ensure!(
                matches!(
                    frame.kind,
                    Kind::Arm
                        | Kind::Pause
                        | Kind::Stop
                        | Kind::Heartbeat
                        | Kind::RoutingState
                        | Kind::Move
                        | Kind::Position
                        | Kind::Key
                        | Kind::Button
                        | Kind::Scroll
                ),
                "unsupported outgoing pair frame"
            );
            self.input_seq = self
                .input_seq
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("pair input sequence exhausted"))?;
            frame.seq = self.input_seq;
            frame.lease = self.peer_lease.expect("lease checked").0;
            frame.t1_ns = now;
            ensure!(now >= frame.t0_ns, "captured input clock is in the future");
            if matches!(
                frame.kind,
                Kind::Move | Kind::Position | Kind::Key | Kind::Button | Kind::Scroll
            ) {
                ensure!(
                    now - frame.t0_ns <= 30_000_000,
                    "captured pair input expired"
                );
            }
            frame.encode()?;
            self.queue(Message::Input {
                source: self.side,
                owner_epoch: self.status.epoch,
                layout_revision: self.status.layout_revision,
                frame,
            })?;
            if frame.kind == Kind::RoutingState {
                self.routing = Some(frame);
            } else {
                self.last_heartbeat = Some(now);
                if matches!(frame.kind, Kind::Arm | Kind::Pause | Kind::Stop) {
                    self.routing = None;
                }
            }
            Ok(true)
        })();
        self.finish(result)
    }
    pub fn tick(&mut self, now: u64) -> Result<()> {
        let result = (|| {
            self.clock(now)?;
            if now - self.last_keepalive >= KEEPALIVE_PERIOD_NS {
                self.queue(Message::KeepAlive)?;
                self.last_keepalive = now;
            }
            if self.status.owner.is_some() && self.barrier.is_none() && !self.pending_request {
                if self
                    .last_lease_sent
                    .is_none_or(|at| now - at >= LEASE_PERIOD_NS)
                {
                    self.issue_lease()?;
                }
                if self.status.owner == Some(self.side)
                    && self.lease_ready(now)
                    && self
                        .last_heartbeat
                        .is_none_or(|at| now - at >= LEASE_PERIOD_NS)
                {
                    let mut heartbeat = Frame::new(Kind::Heartbeat);
                    heartbeat.t0_ns = now;
                    self.send_input(heartbeat, now)?;
                }
                if self.receiver.active {
                    ensure!(
                        self.peer_lease
                            .is_some_and(|(_, at)| now - at <= LEASE_TTL_NS)
                            && self.last_input.is_some_and(|at| now - at <= LEASE_TTL_NS),
                        "active pair input freshness expired"
                    );
                    ensure!(
                        !self.sink.emergency_pressed(),
                        "local emergency stopped pair input"
                    );
                    self.sink.maintain()?;
                }
            }
            if !self.receiver.active {
                // Keep the idle native worker available for a later Arm or
                // ownership handoff. clock() already checked peer liveness,
                // the current ownership lease and any release barrier. This
                // does not reset/arm the sink or renew input authority.
                ensure!(
                    !self.sink.emergency_pressed(),
                    "local emergency stopped pair input"
                );
                self.sink.maintain()?;
            }
            Ok(())
        })();
        self.finish(result)
    }
}
impl<S: LocalSource, K: MouseSink> Drop for PairEndpoint<S, K> {
    fn drop(&mut self) {
        let _ = self.source.revoke();
        let _ = self.sink.release_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
    };
    #[derive(Default)]
    struct Record {
        events: Vec<String>,
        keys: BTreeSet<u16>,
        release_failure: bool,
        moves: usize,
        maintained: usize,
    }
    struct Source(Side, Arc<Mutex<Record>>);
    impl LocalSource for Source {
        fn revoke(&mut self) -> Result<()> {
            self.1
                .lock()
                .unwrap()
                .events
                .push(format!("{:?}:revoke", self.0));
            Ok(())
        }
        fn grant(&mut self, epoch: u64, _: u64) -> Result<()> {
            self.1
                .lock()
                .unwrap()
                .events
                .push(format!("{:?}:grant:{epoch}", self.0));
            Ok(())
        }
    }
    struct Sink(Side, Arc<Mutex<Record>>);
    impl MouseSink for Sink {
        fn reset(&mut self) -> Result<()> {
            self.release_owned()
        }
        fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
            self.1.lock().unwrap().moves += 1;
            Ok((x, y))
        }
        fn position(&mut self, x: f64, y: f64, _: u32) -> Result<(f64, f64)> {
            self.move_by(x, y)
        }
        fn key(&mut self, key: u16, down: bool) -> Result<()> {
            let mut r = self.1.lock().unwrap();
            if down {
                ensure!(r.keys.insert(key), "duplicate fixture key");
            } else {
                r.keys.remove(&key);
            }
            Ok(())
        }
        fn release_owned(&mut self) -> Result<()> {
            let mut r = self.1.lock().unwrap();
            r.events.push(format!("{:?}:release", self.0));
            ensure!(!r.release_failure, "fixture release failed");
            r.keys.clear();
            Ok(())
        }
        fn maintain(&mut self) -> Result<()> {
            self.1.lock().unwrap().maintained += 1;
            Ok(())
        }
    }
    type Endpoint = PairEndpoint<Source, Sink>;
    fn endpoint(side: Side) -> (Endpoint, Arc<Mutex<Record>>) {
        let record = Arc::new(Mutex::new(Record::default()));
        (
            Endpoint::new(
                side,
                7,
                Source(side, record.clone()),
                Sink(side, record.clone()),
                0,
            )
            .unwrap(),
            record,
        )
    }
    fn pump(a: &mut Endpoint, b: &mut Endpoint, now: u64) {
        for _ in 0..40 {
            let mut progress = false;
            while let Some(m) = a.pop_outbound() {
                b.receive(m, now).unwrap();
                progress = true;
            }
            while let Some(m) = b.pop_outbound() {
                a.receive(m, now).unwrap();
                progress = true;
            }
            if !progress {
                return;
            }
        }
        panic!("pair did not quiesce");
    }
    fn enabled() -> (Endpoint, Endpoint, Arc<Mutex<Record>>, Arc<Mutex<Record>>) {
        let (mut a, ar) = endpoint(Side::A);
        let (mut b, br) = endpoint(Side::B);
        a.enable_local(0).unwrap();
        pump(&mut a, &mut b, 0);
        (a, b, ar, br)
    }
    fn input(source: &mut Endpoint, kind: Kind, now: u64) -> Frame {
        let mut f = Frame::new(kind);
        f.t0_ns = now;
        if kind == Kind::Key {
            f.code = 4;
            f.down = true;
        }
        assert!(source.send_input(f, now).unwrap());
        f
    }
    #[test]
    fn full_takeover_releases_keys_before_each_new_grant() {
        let (mut a, mut b, ar, br) = enabled();
        assert_eq!(a.status.owner, Some(Side::A));
        assert_eq!(b.status.owner, Some(Side::A));
        input(&mut a, Kind::Arm, 1);
        input(&mut a, Kind::Key, 2);
        pump(&mut a, &mut b, 2);
        assert!(br.lock().unwrap().keys.contains(&4));
        b.request_local(3).unwrap();
        assert!(br.lock().unwrap().keys.is_empty());
        assert!(!b.status.input_active);
        pump(&mut a, &mut b, 3);
        assert_eq!(a.status.owner, Some(Side::B));
        assert_eq!(b.status.owner, Some(Side::B));
        input(&mut b, Kind::Arm, 4);
        input(&mut b, Kind::Key, 5);
        pump(&mut a, &mut b, 5);
        assert!(ar.lock().unwrap().keys.contains(&4));
        a.request_local(6).unwrap();
        assert!(ar.lock().unwrap().keys.is_empty());
        pump(&mut a, &mut b, 6);
        assert_eq!(a.status.owner, Some(Side::A));
        assert_eq!(a.status.epoch, 3);
        for (side, record) in [(Side::A, ar), (Side::B, br)] {
            let r = record.lock().unwrap();
            for (i, event) in r.events.iter().enumerate() {
                if event.starts_with(&format!("{side:?}:grant:")) {
                    assert_eq!(r.events[i - 1], format!("{side:?}:release"));
                }
            }
        }
    }
    #[test]
    fn cleanup_failure_never_acknowledges_or_grants() {
        let (mut a, mut b, _, br) = enabled();
        br.lock().unwrap().release_failure = true;
        assert!(b.request_local(1).is_err());
        assert!(!b.status.connected);
        let mut messages = vec![];
        while let Some(m) = b.pop_outbound() {
            messages.push(m);
        }
        assert!(
            messages
                .iter()
                .all(|m| !matches!(m, Message::Released { .. } | Message::Grant { .. }))
        );
        assert!(a.receive(messages.remove(0), 1).is_err());
        assert!(!a.status.connected);
    }
    #[test]
    fn stale_inflight_input_is_discarded_immediately_on_local_request() {
        let (mut a, mut b, _, br) = enabled();
        input(&mut a, Kind::Arm, 1);
        pump(&mut a, &mut b, 1);
        let mut f = Frame::new(Kind::Position);
        f.x = 10.;
        f.y = 10.;
        f.display_id = 1;
        f.t0_ns = 2;
        assert!(a.send_input(f, 2).unwrap());
        let old = a.pop_outbound().unwrap();
        b.request_local(2).unwrap();
        b.receive(old, 2).unwrap();
        assert_eq!(br.lock().unwrap().moves, 0);
        pump(&mut a, &mut b, 2);
        assert_eq!(b.status.owner, Some(Side::B));
    }
    #[test]
    fn unsolicited_grant_wrong_layout_and_future_input_are_terminal() {
        let (_, mut b, _, _) = enabled();
        let epoch = b.status.epoch;
        assert!(
            b.receive(
                Message::Grant {
                    source: Side::B,
                    owner_epoch: epoch,
                    layout_revision: 7
                },
                1
            )
            .is_err()
        );
        for (source, epoch, revision) in [(Side::A, 99, 7), (Side::A, 1, 8), (Side::B, 1, 7)] {
            let (_, mut b, _, _) = enabled();
            assert!(
                b.receive(
                    Message::Input {
                        source,
                        owner_epoch: epoch,
                        layout_revision: revision,
                        frame: Frame::new(Kind::Arm)
                    },
                    1
                )
                .is_err()
            );
        }
    }
    #[test]
    fn keepalive_cannot_replace_missing_input_leases() {
        let (mut a, _, _, _) = enabled();
        for ms in [40, 80, 120] {
            a.receive(Message::KeepAlive, ms * 1_000_000).unwrap();
        }
        assert!(a.receive(Message::KeepAlive, LEASE_TTL_NS + 1_000_000).is_err());
        assert!(!a.status.connected);
    }
    #[test]
    fn paused_workers_stay_idle_and_available_beyond_ten_seconds() {
        let (mut a, mut b, ar, br) = enabled();
        a.pause(0).unwrap();
        pump(&mut a, &mut b, 0);
        for now in (25..=12_000).step_by(25).map(|ms| ms * 1_000_000) {
            let before = (ar.lock().unwrap().maintained, br.lock().unwrap().maintained);
            a.tick(now).unwrap();
            b.tick(now).unwrap();
            pump(&mut a, &mut b, now);
            assert_eq!(ar.lock().unwrap().maintained, before.0 + 1);
            assert_eq!(br.lock().unwrap().maintained, before.1 + 1);
            for endpoint in [&a, &b] {
                assert!(endpoint.status.connected);
                assert!(!endpoint.status.enabled && !endpoint.status.input_active);
                assert!(!endpoint.receiver.active);
                assert!(endpoint.status.owner.is_none());
            }
        }
        a.enable_local(12_000_000_000).unwrap();
        pump(&mut a, &mut b, 12_000_000_000);
        input(&mut a, Kind::Arm, 12_000_000_000);
        pump(&mut a, &mut b, 12_000_000_000);
        assert!(b.status.input_active);
    }
    #[test]
    fn source_idle_worker_is_maintained_until_reverse_takeover() {
        let (mut a, mut b, ar, _) = enabled();
        input(&mut a, Kind::Arm, 0);
        pump(&mut a, &mut b, 0);
        for now in (25..=12_000).step_by(25).map(|ms| ms * 1_000_000) {
            let before = ar.lock().unwrap().maintained;
            a.tick(now).unwrap();
            b.tick(now).unwrap();
            pump(&mut a, &mut b, now);
            assert_eq!(ar.lock().unwrap().maintained, before + 1);
            assert_eq!(a.status.owner, Some(Side::A));
            assert!(!a.status.input_active && !a.receiver.active);
            assert!(b.status.input_active);
        }
        b.request_local(12_000_000_000).unwrap();
        pump(&mut a, &mut b, 12_000_000_000);
        input(&mut b, Kind::Arm, 12_000_000_000);
        pump(&mut a, &mut b, 12_000_000_000);
        assert_eq!(a.status.owner, Some(Side::B));
        assert!(a.status.input_active);
        assert!(!b.status.input_active);
    }
    #[test]
    fn idle_maintenance_stops_on_disconnect_liveness_or_ownership_lease_expiry() {
        let (mut a, ar) = endpoint(Side::A);
        a.tick(1).unwrap();
        let maintained = ar.lock().unwrap().maintained;
        a.disconnect();
        assert!(a.tick(2).is_err());
        assert_eq!(ar.lock().unwrap().maintained, maintained);

        let (mut a, ar) = endpoint(Side::A);
        a.tick(1).unwrap();
        let maintained = ar.lock().unwrap().maintained;
        assert!(a.tick(LEASE_TTL_NS + 1).is_err());
        assert_eq!(ar.lock().unwrap().maintained, maintained);
        assert!(!a.status.connected);

        let (mut a, _, ar, _) = enabled();
        for ms in [40, 80, 120] {
            a.receive(Message::KeepAlive, ms * 1_000_000).unwrap();
            a.tick(ms * 1_000_000).unwrap();
        }
        let maintained = ar.lock().unwrap().maintained;
        // Peer is still live, but its ownership lease has expired.
        assert!(a.tick(LEASE_TTL_NS + 1).is_err());
        assert_eq!(ar.lock().unwrap().maintained, maintained);
        assert!(!a.status.connected);
    }
    #[test]
    fn keepalive_and_leases_cannot_renew_stale_active_input() {
        let (mut a, mut b, _, br) = enabled();
        input(&mut a, Kind::Arm, 0);
        pump(&mut a, &mut b, 0);
        for (token, ms) in [(2, 40), (3, 80), (4, 120)] {
            b.receive(Message::Lease { token }, ms * 1_000_000).unwrap();
            b.tick(ms * 1_000_000).unwrap();
        }
        let maintained = br.lock().unwrap().maintained;
        assert!(b.receive(Message::Lease { token: 5 }, LEASE_TTL_NS + 1_000_000).is_err());
        assert_eq!(br.lock().unwrap().maintained, maintained);
        assert!(!b.status.connected);
    }
    #[test]
    fn release_barrier_expires_even_with_peer_keepalives() {
        let (mut a, _) = endpoint(Side::A);
        a.enable_local(0).unwrap();
        for ms in (80..=(BARRIER_TIMEOUT_NS / 1_000_000 - 40)).step_by(80) {
            a.receive(Message::KeepAlive, ms * 1_000_000).unwrap();
        }
        assert!(a.tick(BARRIER_TIMEOUT_NS + 1_000_000).is_err());
        assert!(
            !a.outbound
                .iter()
                .any(|m| matches!(m, Message::Grant { .. }))
        );
    }
    #[test]
    fn exact_receiver_lease_arm_and_sequence_are_required() {
        let (mut a, mut b, _, _) = enabled();
        input(&mut a, Kind::Key, 1);
        assert!(b.receive(a.pop_outbound().unwrap(), 1).is_err());
        let (mut a, mut b, _, _) = enabled();
        input(&mut a, Kind::Arm, 1);
        let arm = a.pop_outbound().unwrap();
        b.receive(arm.clone(), 1).unwrap();
        assert!(b.receive(arm, 2).is_err());
        let (mut a, mut b, _, _) = enabled();
        input(&mut a, Kind::Arm, 1);
        let Message::Input {
            source,
            owner_epoch,
            layout_revision,
            mut frame,
        } = a.pop_outbound().unwrap()
        else {
            panic!()
        };
        frame.lease = 999;
        assert!(
            b.receive(
                Message::Input {
                    source,
                    owner_epoch,
                    layout_revision,
                    frame
                },
                1
            )
            .is_err()
        );
    }
    #[test]
    fn request_during_released_barrier_uses_the_next_granted_epoch() {
        let (mut a, _) = endpoint(Side::A);
        let (mut b, _) = endpoint(Side::B);
        a.enable_local(0).unwrap();
        b.receive(a.pop_outbound().unwrap(), 0).unwrap();
        b.request_local(0).unwrap();
        pump(&mut a, &mut b, 0);
        assert_eq!(a.status.owner, Some(Side::B));
        assert_eq!(b.status.owner, Some(Side::B));
    }
    #[test]
    fn local_pause_does_not_activate_an_inflight_grant() {
        let (mut a, _) = endpoint(Side::A);
        let (mut b, br) = endpoint(Side::B);
        a.enable_local(0).unwrap();
        b.receive(a.pop_outbound().unwrap(), 0).unwrap();
        b.pause(0).unwrap();
        pump(&mut a, &mut b, 0);
        assert!(!a.status.enabled && !b.status.enabled);
        assert!(
            !br.lock()
                .unwrap()
                .events
                .iter()
                .any(|e| e.starts_with("B:grant"))
        );
    }
    #[test]
    fn egress_expiry_prevents_delayed_capture_from_hiding_in_endpoint_queue() {
        let (mut a, _, _, _) = enabled();
        input(&mut a, Kind::Key, 0);
        assert!(a.pop_outbound_at(30_000_001).is_err());
        assert!(!a.status.connected);
        assert!(
            !a.outbound
                .iter()
                .any(|m| matches!(m, Message::Input { .. }))
        );
        let (mut a, _, _, _) = enabled();
        input(&mut a, Kind::Key, 0);
        let Some(Message::Input { frame, .. }) = a.pop_outbound_at(10_000_000).unwrap() else {
            panic!()
        };
        assert_eq!(frame.t1_ns, 10_000_000);
    }
    #[test]
    fn metadata_is_observational_without_arm_and_clears_on_pause_revoke_and_shutdown() {
        let (mut a, mut b, _, br) = enabled();
        let mut metadata = Frame::new(Kind::RoutingState);
        metadata.down = true;
        metadata.code = 1;
        assert!(a.send_input(metadata, 1).unwrap());
        pump(&mut a, &mut b, 1);
        assert_eq!(a.routing_frame().unwrap().code, 1);
        assert_eq!(b.routing_frame().unwrap().code, 1);
        assert!(!b.status.input_active);
        assert_eq!(br.lock().unwrap().maintained, 0);
        assert_eq!(br.lock().unwrap().moves, 0);
        input(&mut a, Kind::Pause, 2);
        pump(&mut a, &mut b, 2);
        assert!(a.routing_frame().is_none() && b.routing_frame().is_none());
        a.send_input(metadata, 3).unwrap();
        pump(&mut a, &mut b, 3);
        b.request_local(4).unwrap();
        assert!(b.routing_frame().is_none());
        pump(&mut a, &mut b, 4);
        assert!(a.routing_frame().is_none());
        a.shutdown().unwrap();
        assert!(a.routing_frame().is_none());
    }
    #[test]
    fn metadata_cannot_replace_active_input_heartbeat_or_renew_worker() {
        let (mut a, mut b, _, br) = enabled();
        input(&mut a, Kind::Arm, 0);
        pump(&mut a, &mut b, 0);
        let count = br.lock().unwrap().maintained;
        for (token, ms) in [(2, 40), (3, 80), (4, 120)] {
            let now = ms * 1_000_000;
            b.receive(Message::Lease { token }, now).unwrap();
            // Receiver issued lease1 remains valid within150ms; seq is global.
            let mut frame = Frame::new(Kind::RoutingState);
            frame.seq = token;
            frame.lease = 1;
            frame.down = true;
            frame.code = 1;
            b.receive(
                Message::Input {
                    source: Side::A,
                    owner_epoch: 1,
                    layout_revision: 7,
                    frame,
                },
                now,
            )
            .unwrap();
        }
        assert_eq!(br.lock().unwrap().maintained, count);
        assert!(b.tick(LEASE_TTL_NS + 1_000_000).is_err());
        assert!(!b.status.connected);
    }
    #[test]
    fn explicit_shutdown_reports_uncertain_release_and_allows_cleanup_retry() {
        let (mut a, _, ar, _) = enabled();
        ar.lock().unwrap().release_failure = true;
        assert!(a.shutdown().is_err());
        assert!(!a.status.connected);
        assert_eq!(a.pending_outbound(), 0);
        ar.lock().unwrap().release_failure = false;
        assert!(a.shutdown().is_ok());
    }
    #[test]
    fn metadata_wrong_epoch_and_ungranted_sender_cannot_claim_focus() {
        let (mut a, _, _, _) = enabled();
        let mut frame = Frame::new(Kind::RoutingState);
        frame.down = true;
        frame.code = 1;
        assert!(
            a.receive(
                Message::Input {
                    source: Side::B,
                    owner_epoch: 1,
                    layout_revision: 7,
                    frame
                },
                1
            )
            .is_err()
        );
        assert!(a.routing_frame().is_none());
    }
    #[test]
    fn bounded_queue_overflow_closes_and_revokes() {
        let (mut a, _, ar, _) = enabled();
        for _ in 0..OUTBOUND_CAPACITY {
            input(&mut a, Kind::Heartbeat, 0);
        }
        assert!(a.send_input(Frame::new(Kind::Heartbeat), 0).is_err());
        assert!(!a.status.connected);
        assert_eq!(a.pending_outbound(), 1);
        assert!(
            ar.lock()
                .unwrap()
                .events
                .last()
                .unwrap()
                .ends_with(":release")
        );
    }

    #[test]
    fn consecutive_positions_and_moves_coalesce_between_discrete_barriers() {
        use crate::input::Button;
        use std::cell::RefCell;
        use std::rc::Rc;

        struct TraceSink(Rc<RefCell<Vec<String>>>);
        impl MouseSink for TraceSink {
            fn reset(&mut self) -> Result<()> {
                self.0.borrow_mut().push("reset".into());
                Ok(())
            }
            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                self.0.borrow_mut().push(format!("move:{x}:{y}"));
                Ok((x, y))
            }
            fn position(&mut self, x: f64, y: f64, id: u32) -> Result<(f64, f64)> {
                self.0.borrow_mut().push(format!("pos:{x}:{y}:{id}"));
                Ok((x, y))
            }
            fn button(&mut self, button: Button, down: bool) -> Result<()> {
                self.0.borrow_mut().push(format!("btn:{button:?}:{down}"));
                Ok(())
            }
            fn release_owned(&mut self) -> Result<()> {
                Ok(())
            }
        }
        struct NoopSource;
        impl LocalSource for NoopSource {
            fn revoke(&mut self) -> Result<()> {
                Ok(())
            }
            fn grant(&mut self, _: u64, _: u64) -> Result<()> {
                Ok(())
            }
        }

        // First exercise Position coalescing around discrete Button barriers.
        let sink_calls = Rc::new(RefCell::new(Vec::new()));
        let mut b =
            PairEndpoint::new(Side::B, 7, NoopSource, TraceSink(sink_calls.clone()), 0).unwrap();
        b.receive(
            Message::Revoke {
                barrier_epoch: 1,
                reason: RevokeReason::Enable,
            },
            0,
        )
        .unwrap();
        b.receive(
            Message::Grant {
                source: Side::A,
                owner_epoch: 1,
                layout_revision: 7,
            },
            0,
        )
        .unwrap();
        b.receive(Message::Lease { token: 1 }, 0).unwrap();

        let msg = |seq: u64, kind: Kind, x: f64, y: f64, down: bool| {
            let mut f = Frame::new(kind);
            f.seq = seq;
            f.lease = 1;
            f.t0_ns = 0;
            f.t1_ns = 1;
            f.x = x;
            f.y = y;
            f.display_id = if kind == Kind::Position { 7 } else { 0 };
            f.code = 0;
            f.down = down;
            Message::Input {
                source: Side::A,
                owner_epoch: 1,
                layout_revision: 7,
                frame: f,
            }
        };
        let batch = [
            (msg(1, Kind::Arm, 0., 0., false), false),
            (msg(2, Kind::Position, 10., 10., false), true),
            (msg(3, Kind::Position, 20., 20., false), true),
            (msg(4, Kind::Position, 30., 30., false), false),
            (msg(5, Kind::Button, 0., 0., true), false),
            (msg(6, Kind::Position, 40., 40., false), true),
            (msg(7, Kind::Position, 50., 50., false), false),
            (msg(8, Kind::Button, 0., 0., false), false),
        ];
        for (m, defer) in batch {
            b.receive_coalesced(m, defer, 1).unwrap();
        }
        assert_eq!(
            *sink_calls.borrow(),
            vec![
                "reset",
                "pos:30:30:7",
                "btn:Left:true",
                "pos:50:50:7",
                "btn:Left:false",
            ]
        );
        // Duplicate sequence inside a deferred Position run must fail closed.
        b.receive_coalesced(msg(9, Kind::Position, 60., 60., false), true, 1)
            .unwrap();
        assert!(
            b.receive_coalesced(msg(9, Kind::Position, 70., 70., false), false, 1)
                .is_err()
        );
        assert!(!b.status().connected);

        // Next verify Move deltas accumulate accurately and fail closed on invalid seq.
        let move_calls = Rc::new(RefCell::new(Vec::new()));
        let mut b =
            PairEndpoint::new(Side::B, 7, NoopSource, TraceSink(move_calls.clone()), 0).unwrap();
        b.receive(
            Message::Revoke {
                barrier_epoch: 1,
                reason: RevokeReason::Enable,
            },
            0,
        )
        .unwrap();
        b.receive(
            Message::Grant {
                source: Side::A,
                owner_epoch: 1,
                layout_revision: 7,
            },
            0,
        )
        .unwrap();
        b.receive(Message::Lease { token: 1 }, 0).unwrap();
        let mk = |seq: u64, kind: Kind, x: f64, y: f64| {
            let mut f = Frame::new(kind);
            f.seq = seq;
            f.lease = 1;
            f.t0_ns = 0;
            f.t1_ns = 1;
            f.x = x;
            f.y = y;
            Message::Input {
                source: Side::A,
                owner_epoch: 1,
                layout_revision: 7,
                frame: f,
            }
        };
        b.receive_coalesced(mk(1, Kind::Arm, 0., 0.), false, 1)
            .unwrap();
        b.receive_coalesced(mk(2, Kind::Move, 3., 4.), true, 1)
            .unwrap();
        b.receive_coalesced(mk(3, Kind::Move, 10., 12.), false, 1)
            .unwrap();
        assert_eq!(*move_calls.borrow(), vec!["reset", "move:10:12"]);
        b.receive_coalesced(mk(4, Kind::Move, 11., 13.), true, 1)
            .unwrap();
        assert!(
            b.receive_coalesced(mk(4, Kind::Move, 12., 14.), false, 1)
                .is_err()
        );
        assert!(!b.status().connected);
    }
}
