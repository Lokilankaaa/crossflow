//! Platform-neutral physical capture adapter for a duplex pair endpoint.
//! No OS calls, logging or transport IO occurs while these locks are held.
//! Always observe physical transitions while paused so a later grant cannot
//! mistake a pre-held control for a new press. Only the endpoint grants routing.
use crate::{
    arrangement::Point,
    input::{Button, CaptureOutcome, InputAction, PauseReason},
    pair_endpoint::LocalSource,
    protocol::{Frame, Kind},
    routed_desktop::RoutedInput,
    screen_runtime::RuntimeLayout,
    session_metadata::RoutingState,
};
use anyhow::{Result, ensure};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Debug)]
pub enum SourceEvent {
    Movement {
        native: Point,
        dx: f64,
        dy: f64,
    },
    Button {
        button: Button,
        down: bool,
    },
    Key {
        usage: u16,
        down: bool,
    },
    Scroll {
        horizontal: i32,
        vertical: i32,
    },
    /// Startup/capture-restart state, not evidence of a new physical action.
    Snapshot {
        buttons: Vec<Button>,
        keys: Vec<u16>,
    },
}
impl SourceEvent {
    fn activity(&self) -> bool {
        match self {
            Self::Snapshot { .. } => false,
            Self::Movement { dx, dy, .. } => *dx != 0. || *dy != 0.,
            Self::Scroll {
                horizontal,
                vertical,
            } => *horizontal != 0 || *vertical != 0,
            _ => true,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFailure {
    QueuePaused(PauseReason),
    RoutingPaused,
    InvalidCapture,
    InvalidSnapshot,
    ClockRegressed,
    LockPoisoned,
    CaptureStopped,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceStatus {
    pub connected: bool,
    pub ready: bool,
    pub enabled: bool,
    pub remote_focus: bool,
    pub generation: u64,
    /// Latched until the endpoint acknowledges it through revoke().
    pub failure: Option<SourceFailure>,
}
struct State {
    route: RoutedInput,
    ready: bool,
    emitted_epoch: Option<u64>,
    last_poll_ns: Option<u64>,
    failure: Option<SourceFailure>,
    last_grant: Option<(u64, u64)>,
}
impl State {
    fn fail(&mut self, reason: SourceFailure) {
        if self.failure.is_none() {
            self.failure = Some(reason);
        }
        self.route.pause();
    }
    fn sync(&mut self) -> bool {
        let enabled = self.route.enabled;
        let connected = self.route.sync();
        if enabled && !self.route.enabled && self.failure.is_none() {
            self.failure = Some(SourceFailure::RoutingPaused);
        }
        connected
    }
}

#[derive(Clone)]
pub struct SharedSource {
    state: Arc<Mutex<State>>,
    gate: Arc<AtomicBool>,
    activity: Arc<AtomicU64>,
}
impl SharedSource {
    pub fn new(layout: RuntimeLayout) -> Result<Self> {
        let mut route = RoutedInput::new(layout)?;
        route
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begin_session();
        route.sync();
        let gate = route.remote_gate.clone();
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                route,
                ready: false,
                emitted_epoch: None,
                last_poll_ns: None,
                failure: None,
                last_grant: None,
            })),
            gate,
            activity: Arc::new(AtomicU64::new(0)),
        })
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poison) => {
                let mut state = poison.into_inner();
                state.fail(SourceFailure::LockPoisoned);
                state
            }
        }
    }
    pub fn remote_gate(&self) -> Arc<AtomicBool> {
        self.gate.clone()
    }
    pub fn activity_counter(&self) -> u64 {
        self.activity.load(Ordering::Acquire)
    }
    pub fn signal_activity(&self) {
        self.activity.fetch_add(1, Ordering::Release);
    }
    pub fn set_edge_locked(&self, locked: bool) {
        self.lock().route.set_edge_locked(locked);
    }
    pub fn is_edge_locked(&self) -> bool {
        self.lock().route.is_edge_locked()
    }
    pub fn set_peer_profiles(
        &self,
        profiles: std::collections::HashMap<String, crate::routed_desktop::PeerInputProfile>,
    ) {
        self.lock().route.set_peer_profiles(profiles);
    }
    pub fn has_held_physical_input(&self) -> bool {
        let state = self.lock();
        let queue = state.route.input.lock().unwrap_or_else(|e| e.into_inner());
        queue.has_held_local_input()
    }
    /// Programmatically jump cursor/keyboard focus to `target_device_id`.
    /// Execute any returned local warp point AFTER this function returns.
    pub fn quick_jump_to_device(
        &self,
        target_device_id: &str,
        now_ns: u64,
    ) -> Result<Option<Point>> {
        self.quick_jump_to_device_with_clock(target_device_id, || now_ns)
    }
    pub fn quick_jump_to_device_with_clock(
        &self,
        target_device_id: &str,
        now: impl FnOnce() -> u64,
    ) -> Result<Option<Point>> {
        let mut state = self.lock();
        state.sync();
        if !state.route.enabled || state.failure.is_some() {
            return Ok(None);
        }
        state.route.quick_jump_to_device(target_device_id, now())
    }
    /// Programmatically jump cursor/keyboard focus to a specific global screen ID
    /// (local or remote). Execute any returned local warp point AFTER this returns.
    pub fn quick_jump_to_screen(
        &self,
        screen_id: &str,
        now_ns: u64,
    ) -> Result<Option<Point>> {
        self.quick_jump_to_screen_with_clock(screen_id, || now_ns)
    }
    pub fn quick_jump_to_screen_with_clock(
        &self,
        screen_id: &str,
        now: impl FnOnce() -> u64,
    ) -> Result<Option<Point>> {
        let mut state = self.lock();
        state.sync();
        if !state.route.enabled || state.failure.is_some() {
            return Ok(None);
        }
        state.route.quick_jump_to_screen(screen_id, now())
    }
    pub fn force_return_local(&self, now_ns: u64) -> Result<Option<Point>> {
        self.force_return_local_with_clock(|| now_ns)
    }
    pub fn force_return_local_with_clock(
        &self,
        now: impl FnOnce() -> u64,
    ) -> Result<Option<Point>> {
        let mut state = self.lock();
        state.sync();
        if !state.route.enabled || state.failure.is_some() {
            return Ok(None);
        }
        state.route.force_return_local(now())
    }
    pub fn active_remote_device_id(&self) -> Option<String> {
        let state = self.lock();
        state.route.active_remote_device_id()
    }
    pub fn update_runtime(&self, next: RuntimeLayout) -> Result<()> {
        let mut state = self.lock();
        state.route.update_runtime(next)
    }
    pub fn enable_mesh(&self) -> Result<()> {
        let mut state = self.lock();
        ensure!(
            state.sync() && state.ready && state.failure.is_none(),
            "physical source is not ready for mesh enable"
        );
        if !state.route.enabled {
            let generation = state.route.generation;
            ensure!(
                state.route.enable(generation),
                "mesh source enable rejected"
            );
            state.emitted_epoch = None;
        }
        Ok(())
    }
    pub fn pause_mesh(&self) {
        self.gate.store(false, Ordering::Release);
        let mut state = self.lock();
        state.route.pause();
        state.failure = None;
        state.emitted_epoch = None;
    }
    /// Atomically pop the next frame together with its target remote device ID
    /// (if any) for 1-hop N-device Mesh routing.
    pub fn next_mesh_frame(
        &self,
        now_ns: u64,
        allow_motion: bool,
    ) -> Result<Option<(Option<String>, Frame)>> {
        self.next_mesh_frame_with_clock(|| now_ns, allow_motion)
    }

    /// Like `next_mesh_frame`, but samples the monotonic clock AFTER acquiring
    /// both `state` and `queue` locks so a concurrent capture thread can never
    /// enqueue a newer capture timestamp ahead of our poll timestamp.
    pub fn next_mesh_frame_with_clock(
        &self,
        now: impl FnOnce() -> u64,
        allow_motion: bool,
    ) -> Result<Option<(Option<String>, Frame)>> {
        let mut state = self.lock();
        state.sync();
        if state.failure.is_some() {
            anyhow::bail!("physical source has an unacknowledged failure");
        }
        let target = state.route.active_remote_device_id();
        let queue_arc = state.route.input.clone();
        let mut queue = queue_arc.lock().unwrap_or_else(|e| e.into_inner());
        if !queue.is_connected() {
            return Ok(None);
        }
        let now_ns = now();
        if state.last_poll_ns.is_some_and(|last| now_ns < last) {
            drop(queue);
            state.fail(SourceFailure::ClockRegressed);
            anyhow::bail!("physical source poll clock regressed");
        }
        state.last_poll_ns = Some(now_ns);
        if let Err(reason) = queue.validate_pending(now_ns) {
            drop(queue);
            state.fail(SourceFailure::QueuePaused(reason));
            anyhow::bail!("physical source pending input expired: {reason:?}");
        }
        let epoch = queue.control_epoch();
        if state.emitted_epoch != Some(epoch) {
            let mut frame = Frame::new(if queue.is_active() {
                Kind::Arm
            } else {
                Kind::Pause
            });
            frame.t0_ns = now_ns;
            frame.t1_ns = now_ns;
            state.emitted_epoch = Some(epoch);
            return Ok(Some((target, frame)));
        }
        if !allow_motion
            && matches!(
                queue.front_action(),
                Some(InputAction::Move { .. } | InputAction::Position { .. })
            )
        {
            return Ok(None);
        }
        let input = match queue.pop(now_ns) {
            Ok(value) => value,
            Err(reason) => {
                drop(queue);
                state.fail(SourceFailure::QueuePaused(reason));
                anyhow::bail!("physical source queued input expired: {reason:?}");
            }
        };
        drop(queue);
        match input {
            Some(input) => match Frame::from_input(input, now_ns) {
                Ok(frame) => Ok(Some((target, frame))),
                Err(error) => {
                    state.fail(SourceFailure::InvalidCapture);
                    Err(error)
                }
            },
            None => Ok(None),
        }
    }
    pub fn runtime(&self) -> RuntimeLayout {
        self.lock().route.config.clone()
    }
    pub fn matches_runtime(&self, expected: &RuntimeLayout) -> bool {
        let state = self.lock();
        let actual = &state.route.config;
        actual.layout == expected.layout
            && actual.local_device_id == expected.local_device_id
            && actual.remote_device_id == expected.remote_device_id
            && actual.bindings.len() == expected.bindings.len()
            && actual.bindings.iter().all(|binding| {
                expected.bindings.iter().any(|other| {
                    binding.screen_id == other.screen_id
                        && binding.native_id == other.native_id
                        && binding.native == other.native
                })
            })
    }
    pub fn status(&self) -> SourceStatus {
        let mut state = self.lock();
        let connected = state.sync();
        SourceStatus {
            connected,
            ready: state.ready,
            enabled: state.route.enabled,
            remote_focus: self.gate.load(Ordering::Acquire),
            generation: state.route.generation,
            failure: state.failure,
        }
    }
    pub fn routing_snapshot(&self) -> RoutingState {
        let mut state = self.lock();
        state.sync();
        state
            .route
            .routing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn focus_identity(&self) -> (Option<String>, Option<String>) {
        let mut state = self.lock();
        state.sync();
        state.route.focus_identity()
    }

    /// `captured_at_ns` shares the endpoint's monotonic time origin; preserve the
    /// original capture timestamp, never replace queued capture time with now.
    /// Execute a returned local warp only AFTER this function releases its lock.
    pub fn observe(&self, event: SourceEvent, captured_at_ns: u64) -> Result<Option<Point>> {
        let snapshot = matches!(&event, SourceEvent::Snapshot { .. });
        let activity = event.activity();
        let mut state = self.lock();
        state.sync();
        let was_enabled = state.route.enabled;
        let result = (|| -> Result<Option<Point>> {
            ensure!(
                !snapshot || !was_enabled,
                "physical snapshot requires revoked source authority"
            );
            if let SourceEvent::Movement { native, dx, dy } = event {
                ensure!(
                    native.x.is_finite()
                        && native.y.is_finite()
                        && dx.is_finite()
                        && dy.is_finite()
                        && dx.abs() <= 20_000.
                        && dy.abs() <= 20_000.,
                    "invalid physical source movement"
                );
                return state.route.movement(native, dx, dy, captured_at_ns);
            }
            if let SourceEvent::Scroll {
                horizontal,
                vertical,
            } = event
            {
                let (h, v) = state.route.transform_scroll(horizontal, vertical);
                if h == 0 && v == 0 && (horizontal != 0 || vertical != 0) {
                    return Ok(None);
                }
                let outcome = {
                    let mut queue = state.route.input.lock().unwrap_or_else(|e| e.into_inner());
                    queue.scroll(h, v, captured_at_ns)
                };
                if let CaptureOutcome::Paused(reason) = outcome {
                    state.fail(SourceFailure::QueuePaused(reason));
                    anyhow::bail!("physical source queue paused");
                }
                state.route.outcome(outcome);
                return Ok(None);
            }
            let outcome = {
                let mut queue = state.route.input.lock().unwrap_or_else(|e| e.into_inner());
                match event {
                    SourceEvent::Snapshot { buttons, keys } => {
                        ensure!(
                            buttons.len() <= 5 && keys.len() <= 256,
                            "physical source snapshot exceeds device bounds"
                        );
                        queue
                            .seed_physical_held(buttons, keys)
                            .map_err(|_| anyhow::anyhow!("physical source snapshot rejected"))?;
                        CaptureOutcome::Ignored
                    }
                    SourceEvent::Button { button, down } => {
                        queue.button(button, down, captured_at_ns)
                    }
                    SourceEvent::Key { usage, down } => {
                        ensure!(
                            crate::input::is_keyboard_usage(usage),
                            "unsupported physical source key usage"
                        );
                        queue.key(usage, down, captured_at_ns)
                    }
                    SourceEvent::Scroll { .. } | SourceEvent::Movement { .. } => unreachable!(),
                }
            };
            if let CaptureOutcome::Paused(reason) = outcome {
                state.fail(SourceFailure::QueuePaused(reason));
                anyhow::bail!("physical source queue paused");
            }
            state.route.outcome(outcome);
            Ok(None)
        })();
        let outcome = match result {
            Ok(warp) => {
                if snapshot {
                    state.ready = true;
                }
                if was_enabled && !state.route.enabled {
                    state.fail(SourceFailure::RoutingPaused);
                    Err(anyhow::anyhow!("physical routing paused unexpectedly"))
                } else {
                    Ok(warp)
                }
            }
            Err(error) => {
                if snapshot {
                    state.ready = false;
                    state.fail(SourceFailure::InvalidSnapshot);
                } else {
                    state.fail(SourceFailure::InvalidCapture);
                }
                Err(error)
            }
        };
        // Publish activity only after physical held state is updated. A driver
        // acquiring authority in response to this counter then observes the seed
        // and this press as pre-held rather than replaying it as a fresh down.
        if activity {
            self.activity.fetch_add(1, Ordering::Release);
        }
        outcome
    }

    /// Poll only when the endpoint is current-owner/lease-ready and its output
    /// transport has capacity. Never pop and silently discard a false send_input.
    pub fn next_frame(&self, now_ns: u64) -> Result<Option<Frame>> {
        self.next_frame_rate_limited(now_ns, true)
    }
    /// Motion throttling never delays epoch controls and never lets discrete
    /// input overtake a preceding movement barrier. Freshness is checked even
    /// when the motion at the front cannot yet be sent.
    pub fn next_frame_rate_limited(
        &self,
        now_ns: u64,
        allow_motion: bool,
    ) -> Result<Option<Frame>> {
        let mut state = self.lock();
        state.sync();
        ensure!(
            state.failure.is_none(),
            "physical source has an unacknowledged failure"
        );
        if state.last_poll_ns.is_some_and(|last| now_ns < last) {
            state.fail(SourceFailure::ClockRegressed);
            anyhow::bail!("physical source poll clock regressed");
        }
        state.last_poll_ns = Some(now_ns);
        let queue_arc = state.route.input.clone();
        let mut queue = queue_arc.lock().unwrap_or_else(|e| e.into_inner());
        if !queue.is_connected() {
            return Ok(None);
        }
        if let Err(reason) = queue.validate_pending(now_ns) {
            drop(queue);
            state.fail(SourceFailure::QueuePaused(reason));
            anyhow::bail!("physical source pending input expired");
        }
        let epoch = queue.control_epoch();
        if state.emitted_epoch != Some(epoch) {
            let mut frame = Frame::new(if queue.is_active() {
                Kind::Arm
            } else {
                Kind::Pause
            });
            frame.t0_ns = now_ns;
            frame.t1_ns = now_ns;
            state.emitted_epoch = Some(epoch);
            return Ok(Some(frame));
        }
        if !allow_motion
            && matches!(
                queue.front_action(),
                Some(InputAction::Move { .. } | InputAction::Position { .. })
            )
        {
            return Ok(None);
        }
        let input = match queue.pop(now_ns) {
            Ok(value) => value,
            Err(reason) => {
                drop(queue);
                state.fail(SourceFailure::QueuePaused(reason));
                anyhow::bail!("physical source queued input expired");
            }
        };
        drop(queue);
        match input {
            Some(input) => match Frame::from_input(input, now_ns) {
                Ok(frame) => Ok(Some(frame)),
                Err(error) => {
                    state.fail(SourceFailure::InvalidCapture);
                    Err(error)
                }
            },
            None => Ok(None),
        }
    }

    /// Network lifetime ended. Native input continues locally; callers may keep
    /// observing physical releases while disposing this session's source.
    pub fn disconnect(&self) {
        let mut state = self.lock();
        state.route.pause();
        state
            .route
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .end_session();
        state.route.sync();
    }
    pub fn capture_stopped(&self) {
        let mut state = self.lock();
        state.ready = false;
        state
            .route
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .capture_reset();
        state.fail(SourceFailure::CaptureStopped);
    }
}

impl LocalSource for SharedSource {
    fn revoke(&mut self) -> Result<()> {
        // Revoke the fast native gate before waiting for any source lock.
        self.gate.store(false, Ordering::Release);
        let mut state = self.lock();
        state.route.pause();
        ensure!(
            state.failure != Some(SourceFailure::LockPoisoned),
            "physical source lock poisoned"
        );
        state.failure = None;
        // A later grant must emit fresh control even if no input was drained.
        state.emitted_epoch = None;
        Ok(())
    }
    fn grant(&mut self, owner_epoch: u64, layout_revision: u64) -> Result<()> {
        let mut state = self.lock();
        let result = (|| {
            ensure!(
                owner_epoch > 0 && layout_revision > 0,
                "source grant requires nonzero authority and layout epochs"
            );
            ensure!(
                state.sync() && state.ready && state.failure.is_none(),
                "physical source is not ready for a grant"
            );
            if let Some((previous, revision)) = state.last_grant {
                ensure!(owner_epoch > previous, "source grant epoch did not advance");
                ensure!(
                    layout_revision == revision,
                    "source layout revision changed; rebuild from validated layout"
                );
            }
            let generation = state.route.generation;
            ensure!(
                state.route.enable(generation),
                "source routing grant was rejected"
            );
            state.last_grant = Some((owner_epoch, layout_revision));
            state.emitted_epoch = None;
            Ok(())
        })();
        if result.is_err() {
            state.fail(SourceFailure::RoutingPaused);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        arrangement::{Layout, Screen},
        screen_runtime::{Binding, NativeRect},
    };
    fn runtime() -> RuntimeLayout {
        RuntimeLayout {
            layout: Layout::new(vec![
                Screen {
                    id: "local-screen".into(),
                    device_id: "local".into(),
                    x: 0.,
                    y: 0.,
                    width: 100.,
                    height: 100.,
                },
                Screen {
                    id: "remote-screen".into(),
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
            bindings: vec![
                Binding {
                    screen_id: "local-screen".into(),
                    native_id: "1".into(),
                    native: NativeRect {
                        x: 0.,
                        y: 0.,
                        width: 100.,
                        height: 100.,
                    },
                },
                Binding {
                    screen_id: "remote-screen".into(),
                    native_id: "2".into(),
                    native: NativeRect {
                        x: 0.,
                        y: 0.,
                        width: 200.,
                        height: 200.,
                    },
                },
            ],
        }
    }
    fn seeded() -> SharedSource {
        let source = SharedSource::new(runtime()).unwrap();
        source
            .observe(
                SourceEvent::Snapshot {
                    buttons: vec![],
                    keys: vec![],
                },
                0,
            )
            .unwrap();
        source
    }
    fn movement(source: &SharedSource, dx: f64, t: u64) -> Option<Point> {
        source
            .observe(
                SourceEvent::Movement {
                    native: Point { x: 99., y: 50. },
                    dx,
                    dy: 0.,
                },
                t,
            )
            .unwrap()
    }
    fn remote() -> SharedSource {
        let mut source = seeded();
        source.grant(1, 1).unwrap();
        movement(&source, 0., 1);
        movement(&source, 5., 2);
        source
    }
    #[test]
    fn snapshot_is_required_and_never_claims_physical_activity() {
        let mut source = SharedSource::new(runtime()).unwrap();
        assert!(source.status().connected);
        assert!(!source.status().ready);
        assert!(source.grant(1, 1).is_err());
        source.revoke().unwrap();
        source
            .observe(
                SourceEvent::Snapshot {
                    buttons: vec![],
                    keys: vec![],
                },
                0,
            )
            .unwrap();
        assert_eq!(source.activity_counter(), 0);
        assert!(source.status().ready);
        source.grant(1, 1).unwrap();
        assert!(source.status().enabled);
        assert!(!source.status().remote_focus);
    }
    #[test]
    fn crossing_emits_arm_before_position_and_preserves_key_barriers() {
        let source = remote();
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: true,
                },
                3,
            )
            .unwrap();
        movement(&source, 1., 4);
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: false,
                },
                5,
            )
            .unwrap();
        let frames: Vec<_> = std::iter::from_fn(|| source.next_frame(6).unwrap()).collect();
        assert_eq!(
            frames.iter().map(|f| f.kind).collect::<Vec<_>>(),
            vec![
                Kind::Arm,
                Kind::Position,
                Kind::Key,
                Kind::Position,
                Kind::Key
            ]
        );
        assert!(frames.iter().all(|f| f.seq == 0 && f.lease == 0));
        assert_eq!(frames[1].display_id, 2);
        assert_eq!(frames[1].x, 10.);
        assert_eq!(frames[1].y, 100.);
        assert_eq!(frames[2].t0_ns, 3);
        assert_eq!(frames[2].t1_ns, 6);
        assert_eq!(
            source.routing_snapshot().source_epoch,
            source.lock().route.input.lock().unwrap().control_epoch()
        );
    }
    #[test]
    fn return_warp_revokes_remote_gate_and_emits_pause() {
        let source = remote();
        assert_eq!(source.next_frame(3).unwrap().unwrap().kind, Kind::Arm);
        assert!(source.status().remote_focus);
        assert_eq!(movement(&source, -10., 4), Some(Point { x: 95., y: 50. }));
        assert!(!source.status().remote_focus);
        assert!(source.status().enabled);
        assert_eq!(source.next_frame(5).unwrap().unwrap().kind, Kind::Pause);
        assert!(source.next_frame(5).unwrap().is_none());
        assert_eq!(
            source.focus_identity(),
            (Some("local".into()), Some("local-screen".into()))
        );
    }
    #[test]
    fn preheld_physical_keys_are_tracked_while_paused_and_not_replayed() {
        let mut source = seeded();
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: true,
                },
                1,
            )
            .unwrap();
        assert_eq!(source.activity_counter(), 1);
        source.grant(1, 1).unwrap();
        movement(&source, 0., 2);
        movement(&source, 5., 3);
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: false,
                },
                4,
            )
            .unwrap();
        let first: Vec<_> = std::iter::from_fn(|| source.next_frame(5).unwrap()).collect();
        assert!(first.iter().all(|f| f.kind != Kind::Key));
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: true,
                },
                6,
            )
            .unwrap();
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: false,
                },
                7,
            )
            .unwrap();
        assert!(source.next_frame(8).unwrap().unwrap().down);
        assert!(!source.next_frame(8).unwrap().unwrap().down);
    }
    #[test]
    fn stale_capture_pauses_before_sending_even_the_arm() {
        let mut source = remote();
        let expired = crate::input::INPUT_MAX_AGE_NS + 3;
        assert!(source.next_frame(expired).is_err());
        assert!(!source.status().enabled);
        assert!(!source.status().remote_focus);
        assert_eq!(
            source.status().failure,
            Some(SourceFailure::QueuePaused(PauseReason::StaleInput))
        );
        assert!(source.grant(2, 1).is_err());
        source.revoke().unwrap();
        assert!(source.status().failure.is_none());
        source.grant(2, 1).unwrap();
        assert!(source.next_frame(expired + 1).unwrap().is_some()); // fresh Pause, no old input
        assert!(source.next_frame(expired + 1).unwrap().is_none());
    }
    #[test]
    fn unexpected_queue_pause_is_visible_and_never_auto_rearms() {
        let source = remote();
        source.lock().route.input.lock().unwrap().pause();
        assert_eq!(source.status().failure, Some(SourceFailure::RoutingPaused));
        assert!(!source.status().remote_focus);
        assert!(source.next_frame(4).is_err());
    }
    #[test]
    fn capture_reset_and_disconnect_restore_local_delivery() {
        let source = remote();
        source.capture_stopped();
        let status = source.status();
        assert!(!status.ready);
        assert!(!status.connected);
        assert!(!status.remote_focus);
        assert_eq!(status.failure, Some(SourceFailure::CaptureStopped));
        let source = remote();
        source.disconnect();
        assert!(!source.status().connected);
        assert!(!source.status().remote_focus);
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: false,
                },
                8,
            )
            .unwrap();
        assert!(source.next_frame(9).unwrap().is_none());
    }
    #[test]
    fn rejected_grants_or_midstream_snapshot_cannot_leave_suppression_on() {
        let mut source = remote();
        assert!(source.grant(1, 1).is_err());
        assert!(!source.status().remote_focus);
        source.revoke().unwrap();
        assert!(source.grant(2, 2).is_err());
        let source = remote();
        assert!(
            source
                .observe(
                    SourceEvent::Snapshot {
                        buttons: vec![],
                        keys: vec![]
                    },
                    4
                )
                .is_err()
        );
        assert!(!source.status().ready);
        assert_eq!(
            source.status().failure,
            Some(SourceFailure::InvalidSnapshot)
        );
        assert!(!source.status().remote_focus);
    }
    #[test]
    fn poll_clock_reversal_fails_closed() {
        let source = remote();
        source.next_frame(5).unwrap();
        assert!(source.next_frame(4).is_err());
        assert_eq!(source.status().failure, Some(SourceFailure::ClockRegressed));
        assert!(!source.status().remote_focus);
    }
    #[test]
    fn source_queue_overflow_is_observable_and_clears_suppression() {
        let source = remote();
        let mut failed = false;
        for n in 0..300 {
            if source
                .observe(
                    SourceEvent::Scroll {
                        horizontal: 0,
                        vertical: 120,
                    },
                    3 + n,
                )
                .is_err()
            {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert_eq!(
            source.status().failure,
            Some(SourceFailure::QueuePaused(PauseReason::QueueOverflow))
        );
        assert!(!source.status().remote_focus);
        assert!(source.next_frame(400).is_err());
    }
    #[test]
    fn runtime_matching_includes_native_tokens_and_geometry() {
        let source = seeded();
        let mut config = runtime();
        assert!(source.matches_runtime(&config));
        config.bindings.reverse();
        assert!(source.matches_runtime(&config));
        config.bindings[0].native_id = "999".into();
        assert!(!source.matches_runtime(&config));
        config = runtime();
        config.bindings[0].native.width += 1.;
        assert!(!source.matches_runtime(&config));
    }
    #[test]
    fn rate_limit_keeps_order_but_never_delays_arm_or_pause() {
        let source = remote();
        source
            .observe(
                SourceEvent::Key {
                    usage: 4,
                    down: true,
                },
                3,
            )
            .unwrap();
        assert_eq!(
            source
                .next_frame_rate_limited(4, false)
                .unwrap()
                .unwrap()
                .kind,
            Kind::Arm
        );
        assert!(source.next_frame_rate_limited(5, false).unwrap().is_none());
        assert_eq!(
            source
                .next_frame_rate_limited(6, true)
                .unwrap()
                .unwrap()
                .kind,
            Kind::Position
        );
        assert_eq!(
            source
                .next_frame_rate_limited(7, false)
                .unwrap()
                .unwrap()
                .kind,
            Kind::Key
        );
        movement(&source, 1., 8);
        assert!(source.next_frame_rate_limited(9, false).unwrap().is_none());
        movement(&source, -20., 10);
        assert_eq!(
            source
                .next_frame_rate_limited(11, false)
                .unwrap()
                .unwrap()
                .kind,
            Kind::Pause
        );
    }

    #[test]
    fn next_mesh_frame_with_clock_routes_across_three_devices_without_clock_regression() {
        let layout = Layout::new(vec![
            Screen {
                id: "win1:1".into(),
                device_id: "win1".into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            Screen {
                id: "mac:1".into(),
                device_id: "mac".into(),
                x: 100.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            Screen {
                id: "win2:1".into(),
                device_id: "win2".into(),
                x: 200.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        ])
        .unwrap();
        let rt = RuntimeLayout {
            layout,
            local_device_id: "win1".into(),
            remote_device_id: "*".into(),
            bindings: vec![
                Binding {
                    screen_id: "win1:1".into(),
                    native_id: "1".into(),
                    native: NativeRect {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                },
                Binding {
                    screen_id: "mac:1".into(),
                    native_id: "1".into(),
                    native: NativeRect {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                },
                Binding {
                    screen_id: "win2:1".into(),
                    native_id: "1".into(),
                    native: NativeRect {
                        x: 0.0,
                        y: 0.0,
                        width: 100.0,
                        height: 100.0,
                    },
                },
            ],
        };
        let source = SharedSource::new(rt).unwrap();
        source
            .observe(
                SourceEvent::Snapshot {
                    buttons: vec![],
                    keys: vec![],
                },
                100,
            )
            .unwrap();
        source.enable_mesh().unwrap();

        // Cross from win1 -> mac -> win2 with monotonic capture timestamps
        let _ = source
            .observe(
                SourceEvent::Movement {
                    native: Point { x: 99.0, y: 50.0 },
                    dx: 1.0,
                    dy: 0.0,
                },
                1_000,
            )
            .unwrap();
        let _ = source
            .observe(
                SourceEvent::Movement {
                    native: Point { x: 99.0, y: 50.0 },
                    dx: 10.0,
                    dy: 0.0,
                },
                2_000,
            )
            .unwrap();
        let _ = source
            .observe(
                SourceEvent::Movement {
                    native: Point { x: 99.0, y: 50.0 },
                    dx: 120.0,
                    dy: 0.0,
                },
                5_000,
            )
            .unwrap();
        // Sampling under lock produces a fresh timestamp >= last_capture_ns (5_000)
        // and delivers Arm + Position directly for win2 without ClockRegressed.
        let (target1, frame1) = source
            .next_mesh_frame_with_clock(|| 6_000, true)
            .unwrap()
            .unwrap();
        assert_eq!(target1.as_deref(), Some("win2"));
        assert_eq!(frame1.kind, Kind::Arm);

        let (target2, frame2) = source
            .next_mesh_frame_with_clock(|| 7_000, true)
            .unwrap()
            .unwrap();
        assert_eq!(target2.as_deref(), Some("win2"));
        assert_eq!(frame2.kind, Kind::Position);
        assert!(source.status().failure.is_none());
    }
}
