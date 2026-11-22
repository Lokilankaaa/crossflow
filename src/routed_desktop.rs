//! Source-side focus ownership. The network owns the queue's connection epoch;
//! any unexpected epoch change revokes routing and local suppression.
use crate::{
    arrangement::Point,
    input::{CaptureOutcome, SourceInputQueue},
    screen_runtime::{Focus, RuntimeLayout},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub struct RoutedInput {
    pub config: RuntimeLayout,
    pub input: Arc<Mutex<SourceInputQueue>>,
    pub remote_gate: Arc<AtomicBool>,
    pub enabled: bool,
    pub edge_locked: bool,
    pub generation: u64,
    pub routing: Arc<Mutex<crate::session_metadata::RoutingState>>,
    focus: Option<Focus>,
    last_local_native: Option<Point>,
    expected_queue_epoch: u64,
}
impl RoutedInput {
    pub fn new(config: RuntimeLayout) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            input: Arc::new(Mutex::new(SourceInputQueue::new())),
            remote_gate: Arc::new(AtomicBool::new(false)),
            enabled: false,
            edge_locked: false,
            generation: 0,
            routing: Arc::new(Mutex::new(crate::session_metadata::RoutingState::default())),
            focus: None,
            last_local_native: None,
            expected_queue_epoch: 0,
        })
    }
    pub fn set_edge_locked(&mut self, locked: bool) {
        self.edge_locked = locked;
    }
    pub fn is_edge_locked(&self) -> bool {
        self.edge_locked
    }
    pub fn sync(&mut self) -> bool {
        let q = self.input.lock().unwrap_or_else(|e| e.into_inner());
        let connected = q.is_connected();
        let changed = q.control_epoch() != self.expected_queue_epoch;
        let epoch = q.control_epoch();
        drop(q);
        if changed {
            self.expected_queue_epoch = epoch;
            self.enabled = false;
            self.focus = None;
            self.last_local_native = None;
            self.generation = self.generation.saturating_add(1);
            self.remote_gate.store(false, Ordering::Release);
            self.publish_routing();
        }
        connected
    }
    pub fn enable(&mut self, epoch: u64) -> bool {
        if !self.sync() || self.enabled || self.generation != epoch {
            return false;
        }
        self.enabled = true;
        self.generation = self.generation.saturating_add(1);
        self.publish_routing();
        true
    }
    pub fn pause(&mut self) {
        self.remote_gate.store(false, Ordering::Release);
        self.enabled = false;
        self.focus = None;
        self.last_local_native = None;
        self.generation = self.generation.saturating_add(1);
        let mut q = self.input.lock().unwrap_or_else(|e| e.into_inner());
        q.pause();
        self.expected_queue_epoch = q.control_epoch();
        drop(q);
        self.publish_routing();
    }
    fn publish_routing(&self) {
        let focus = if self.enabled {
            self.focus.clone().or_else(|| {
                self.last_local_native
                    .and_then(|p| self.config.local_focus(p))
            })
        } else {
            None
        };
        *self.routing.lock().unwrap_or_else(|e| e.into_inner()) =
            crate::session_metadata::RoutingState {
                enabled: self.enabled,
                focus_screen_id: focus.map(|f| f.screen_id),
                source_epoch: self.expected_queue_epoch,
            };
    }
    pub fn focus_identity(&self) -> (Option<String>, Option<String>) {
        let state = self.routing.lock().unwrap_or_else(|e| e.into_inner());
        let screen = state
            .focus_screen_id
            .as_ref()
            .and_then(|id| self.config.screen(id).ok());
        match screen {
            Some(screen) if self.enabled => {
                (Some(screen.device_id.clone()), Some(screen.id.clone()))
            }
            _ => (None, None),
        }
    }
    pub fn active_remote_device_id(&self) -> Option<String> {
        let f = self.focus.as_ref()?;
        let s = self.config.screen(&f.screen_id).ok()?;
        (s.device_id != self.config.local_device_id).then(|| s.device_id.clone())
    }
    /// Hot-reload the 2D screen graph (e.g., when a 3rd device joins/leaves the mesh
    /// or the user drags a screen) without restarting native input capture.
    pub fn update_runtime(&mut self, next: RuntimeLayout) -> anyhow::Result<()> {
        next.validate()?;
        let still_valid_focus = self
            .focus
            .as_ref()
            .and_then(|f| next.screen(&f.screen_id).ok())
            .is_some();
        self.config = next;
        if !still_valid_focus && self.focus.is_some() {
            self.remote_gate.store(false, Ordering::Release);
            let mut q = self.input.lock().unwrap_or_else(|e| e.into_inner());
            q.pause();
            self.expected_queue_epoch = q.control_epoch();
            drop(q);
            self.focus = None;
        }
        self.publish_routing();
        Ok(())
    }
    /// Immediately return focus to this local device (e.g., when a remote peer
    /// takes over its own physical mouse/keyboard and sends RevokeControl).
    pub fn force_return_local(&mut self, t0: u64) -> anyhow::Result<Option<Point>> {
        let local_id = self.config.local_device_id.clone();
        self.quick_jump_to_device(&local_id, t0)
    }
    pub fn outcome(&mut self, outcome: CaptureOutcome) {
        if matches!(outcome, CaptureOutcome::Paused(_)) {
            self.sync();
        }
    }
    /// Returns a local warp only on a real return/handoff. Native local motion
    /// otherwise remains untouched. Native adapters execute the warp outside
    /// this lock, after remote_gate has been revoked.
    pub fn movement(
        &mut self,
        native: Point,
        dx: f64,
        dy: f64,
        t0: u64,
    ) -> anyhow::Result<Option<Point>> {
        self.sync();
        if !self.enabled {
            self.last_local_native = Some(native);
            return Ok(None);
        }
        let next = if let Some(f) = &self.focus {
            let candidate = self.config.advance(f, dx, dy)?;
            if self.edge_locked {
                let current_dev = self.config.screen(&f.screen_id)?.device_id.as_str();
                let next_dev = self.config.screen(&candidate.screen_id)?.device_id.as_str();
                if current_dev != next_dev {
                    let s = self.config.screen(&f.screen_id)?;
                    Focus {
                        screen_id: f.screen_id.clone(),
                        point: Point {
                            x: (f.point.x + dx).clamp(s.x, s.right()),
                            y: (f.point.y + dy).clamp(s.y, s.bottom()),
                        },
                    }
                } else {
                    candidate
                }
            } else {
                candidate
            }
        } else {
            if self.edge_locked {
                self.last_local_native = Some(native);
                self.publish_routing();
                return Ok(None);
            }
            let previous = self.last_local_native;
            let segment = if let Some(previous) = previous {
                self.config.local_segment(previous, native)?
            } else {
                None
            };
            self.last_local_native = Some(native);
            let candidate = match segment {
                Some(f) => Some(f),
                None => {
                    if let Some(p) = previous {
                        // Only axes already clamped at the edge represent unused
                        // motion. Native movement along the edge was applied by OS.
                        let push_x = if p.x == native.x { dx } else { 0.0 };
                        let push_y = if p.y == native.y { dy } else { 0.0 };
                        self.config.local_exit(native, push_x, push_y)?
                    } else {
                        None
                    }
                }
            };
            let Some(f) = candidate else {
                self.publish_routing();
                return Ok(None);
            };
            f
        };
        self.apply_focus(next, t0)
    }

    /// Programmatically jump cursor/keyboard focus directly to a device (bypasses edge lock).
    /// Returns `Ok(Some(local_point))` when returning focus to the local device so the caller
    /// can restore the OS cursor cleanly without stealing foreground window focus.
    pub fn quick_jump_to_device(
        &mut self,
        target_device_id: &str,
        t0: u64,
    ) -> anyhow::Result<Option<Point>> {
        self.sync();
        if !self.enabled {
            return Ok(None);
        }
        if target_device_id == self.config.local_device_id {
            if self.focus.is_none() {
                return Ok(None);
            }
            let target = self
                .last_local_native
                .and_then(|p| self.config.local_focus(p))
                .or_else(|| {
                    self.config
                        .layout
                        .screens()
                        .iter()
                        .find(|s| s.device_id == self.config.local_device_id)
                        .map(|s| Focus {
                            screen_id: s.id.clone(),
                            point: Point {
                                x: s.x + s.width * 0.5,
                                y: s.y + s.height * 0.5,
                            },
                        })
                });
            let Some(next) = target else {
                return Ok(None);
            };
            return self.apply_focus(next, t0);
        }
        let Some(screen) = self
            .config
            .layout
            .screens()
            .iter()
            .find(|s| s.device_id == target_device_id)
        else {
            return Ok(None);
        };
        let next = Focus {
            screen_id: screen.id.clone(),
            point: Point {
                x: screen.x + screen.width * 0.5,
                y: screen.y + screen.height * 0.5,
            },
        };
        self.apply_focus(next, t0)
    }
    fn apply_focus(&mut self, next: Focus, t0: u64) -> anyhow::Result<Option<Point>> {
        let point = self.config.native_point(&next)?;
        if self.config.is_local(&next) {
            self.remote_gate.store(false, Ordering::Release);
            let mut q = self.input.lock().unwrap_or_else(|e| e.into_inner());
            if !q.is_connected() || q.control_epoch() != self.expected_queue_epoch {
                drop(q);
                self.sync();
                return Ok(None);
            }
            q.pause();
            self.expected_queue_epoch = q.control_epoch();
            drop(q);
            self.focus = None;
            self.last_local_native = Some(point);
            self.publish_routing();
            Ok(Some(point))
        } else {
            let prev_remote_dev = self.active_remote_device_id();
            let next_remote_dev = self.config.screen(&next.screen_id)?.device_id.clone();
            let switched_remote_device = prev_remote_dev
                .as_deref()
                .is_some_and(|prev| prev != next_remote_dev);
            let mut q = self.input.lock().unwrap_or_else(|e| e.into_inner());
            if !q.is_connected() || q.control_epoch() != self.expected_queue_epoch {
                drop(q);
                self.sync();
                return Ok(None);
            }
            if switched_remote_device {
                q.pause();
            }
            if !q.is_active() && !q.arm() {
                drop(q);
                self.pause();
                return Ok(None);
            }
            self.expected_queue_epoch = q.control_epoch();
            let display_id = self
                .config
                .bindings
                .iter()
                .find(|b| b.screen_id == next.screen_id)
                .expect("validated binding")
                .native_id
                .parse::<u32>()?;
            let outcome = q.position(point.x, point.y, display_id, t0);
            drop(q);
            if matches!(outcome, CaptureOutcome::Paused(_)) {
                self.sync();
                return Ok(None);
            }
            self.focus = Some(next);
            self.remote_gate.store(true, Ordering::Release);
            self.publish_routing();
            Ok(None)
        }
    }
}

#[cfg(windows)]
pub fn capture(
    control: crate::desktop::Control,
    log: crate::metrics::Log,
    duration: std::time::Duration,
) -> anyhow::Result<()> {
    use crate::platform::windows::{self, CaptureEvent, CaptureStatus};
    let route = control.routed.as_ref().expect("routed control").clone();
    let gate = route
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remote_gate
        .clone();
    let event_control = control.clone();
    let ready_control = control.clone();
    let status_control = control.clone();
    let stats_log = log.clone();
    let ready_log = log.clone();
    log.write(
        "capture_coordinate_context",
        serde_json::json!({
            "phase":"before_capture", "diagnostics":windows::capture_coordinate_diagnostics()
        }),
    );
    let mut last_sample = std::time::Instant::now();
    let mut movement_events = 0u64;
    let result = windows::run_routed_input_capture_with_status(
        move |event| {
            if let CaptureEvent::Stop = event {
                event_control.cancel();
                return;
            }
            if let CaptureEvent::Toggle = event {
                event_control.toggle();
                return;
            }
            let handled = (|| -> anyhow::Result<()> {
                let mut r = route.lock().unwrap_or_else(|e| e.into_inner());
                r.sync();
                let warp = if let CaptureEvent::Delta {
                    dx,
                    dy,
                    captured_at,
                } = event
                {
                    let (x, y) = windows::cursor_position().map_err(anyhow::Error::msg)?;
                    let previous = r.last_local_native;
                    let before = r.remote_gate.load(Ordering::Acquire);
                    let warp =
                        r.movement(Point { x, y }, dx as f64, dy as f64, log.at(captured_at))?;
                    movement_events += 1;
                    let after = r.remote_gate.load(Ordering::Acquire);
                    if before != after
                        || last_sample.elapsed() >= std::time::Duration::from_millis(500)
                    {
                        log.write("route_observation",serde_json::json!({
                            "movement_events":movement_events,"native_x":x,"native_y":y,"raw_dx":dx,"raw_dy":dy,
                            "previous_native":previous,"enabled":r.enabled,"remote_focus":after,
                            "focus_changed":before!=after,
                            "local_screen":r.config.local_focus(Point{x,y}).map(|f|f.screen_id)
                        }));
                        last_sample = std::time::Instant::now();
                    }
                    warp
                } else {
                    let mut q = r.input.lock().unwrap_or_else(|e| e.into_inner());
                    let outcome = match event {
                        CaptureEvent::Snapshot { buttons, keys } => {
                            q.seed_physical_held(buttons, keys)
                                .map_err(|_| anyhow::anyhow!("physical input snapshot failed"))?;
                            CaptureOutcome::Ignored
                        }
                        CaptureEvent::Button {
                            button,
                            down,
                            captured_at,
                            ..
                        } => q.button(button, down, log.at(captured_at)),
                        CaptureEvent::Key {
                            usage,
                            down,
                            captured_at,
                            ..
                        } => q.key(usage, down, log.at(captured_at)),
                        CaptureEvent::Scroll {
                            horizontal,
                            vertical,
                            captured_at,
                            ..
                        } => q.scroll(horizontal, vertical, log.at(captured_at)),
                        _ => CaptureOutcome::Ignored,
                    };
                    drop(q);
                    r.outcome(outcome);
                    None
                };
                drop(r);
                if let Some(p) = warp {
                    windows::warp_cursor(p.x, p.y).map_err(anyhow::Error::msg)?;
                }
                Ok(())
            })();
            if let Err(e) = handled {
                event_control.fail(&format!("{e:#}"));
            }
        },
        move || {
            let mut r = status_control
                .routed
                .as_ref()
                .unwrap()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !r.sync() {
                CaptureStatus::Disconnected
            } else if r.enabled {
                CaptureStatus::Active
            } else {
                CaptureStatus::Ready
            }
        },
        move || {
            ready_log.write(
                "capture_coordinate_context",
                serde_json::json!({
                    "phase":"capture_ready", "diagnostics":windows::capture_coordinate_diagnostics()
                }),
            );
            ready_control.capture_ready();
        },
        gate,
        control.stop.clone(),
        Some(duration),
    );
    control.request_stop();
    if let Ok(stats) = &result {
        stats_log.write("routed_capture_summary",serde_json::json!({"packets":stats.packets,"motion":stats.motion,"absolute":stats.absolute,"device_less":stats.device_less,"malformed":stats.malformed,"read_errors":stats.read_errors,"keys":stats.keyboard,"buttons":stats.buttons,"wheel":stats.wheel}));
    }
    result.map(|_| ()).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn routed() -> RoutedInput {
        let c=serde_json::from_str(r#"{"local_device_id":"local","remote_device_id":"peer","layout":{"screens":[{"id":"w","device_id":"local","x":0,"y":0,"width":100,"height":100},{"id":"m","device_id":"peer","x":100,"y":0,"width":100,"height":100}]},"bindings":[{"screen_id":"w","native_id":"w","native":{"x":0,"y":0,"width":100,"height":100}},{"screen_id":"m","native_id":"1","native":{"x":0,"y":0,"width":100,"height":100}}]}"#).unwrap();
        let mut r = RoutedInput::new(c).unwrap();
        {
            let mut q = r.input.lock().unwrap();
            q.seed_physical_held([], []).unwrap();
            q.begin_session();
        }
        r.sync();
        assert!(r.enable(r.generation));
        r.movement(Point { x: 99., y: 50. }, 0., 0., 0).unwrap();
        r
    }
    #[test]
    fn remote_focus_keyboard_and_return_revoke_gate_and_queue() {
        let mut r = routed();
        assert!(r.routing.lock().unwrap().enabled);
        assert_eq!(r.focus_identity(), (Some("local".into()), Some("w".into())));
        assert!(!r.input.lock().unwrap().is_active());
        r.movement(Point { x: 99., y: 50. }, 5., 0., 1).unwrap();
        assert!(r.remote_gate.load(Ordering::Acquire));
        assert_eq!(r.focus_identity(), (Some("peer".into()), Some("m".into())));
        let mut q = r.input.lock().unwrap();
        assert!(q.is_active());
        q.key(4, true, 2);
        drop(q);
        let local = r
            .movement(Point { x: 99., y: 50. }, -10., 0., 3)
            .unwrap()
            .unwrap();
        assert_eq!(local, Point { x: 95., y: 50. });
        assert!(!r.remote_gate.load(Ordering::Acquire));
        assert!(!r.input.lock().unwrap().is_active());
        assert!(r.enabled);
        assert_eq!(r.focus_identity(), (Some("local".into()), Some("w".into())));
        r.pause();
        assert!(!r.routing.lock().unwrap().enabled);
        assert_eq!(r.focus_identity(), (None, None));
    }
    #[test]
    fn reconnect_and_external_queue_failure_never_rearm() {
        let mut r = routed();
        r.movement(Point { x: 99., y: 50. }, 5., 0., 1).unwrap();
        {
            let mut q = r.input.lock().unwrap();
            q.end_session();
            q.begin_session();
        }
        assert!(r.sync());
        assert!(!r.enabled);
        assert!(!r.remote_gate.load(Ordering::Acquire));
        assert!(!r.routing.lock().unwrap().enabled);
        assert_eq!(r.focus_identity(), (None, None));
        assert!(!r.enable(r.generation.saturating_sub(1)));
    }
    #[test]
    fn reconnect_between_geometry_and_commit_revokes_both_transitions() {
        for local in [false, true] {
            let mut r = routed();
            r.movement(Point { x: 99., y: 50. }, 5., 0., 1).unwrap();
            {
                let mut q = r.input.lock().unwrap();
                q.end_session();
                q.begin_session();
            }
            let next = Focus {
                screen_id: if local { "w" } else { "m" }.into(),
                point: Point {
                    x: if local { 90. } else { 110. },
                    y: 50.,
                },
            };
            assert!(r.apply_focus(next, 2).unwrap().is_none());
            assert!(!r.enabled);
            assert!(!r.remote_gate.load(Ordering::Acquire));
            assert!(!r.input.lock().unwrap().is_active());
        }
    }
    #[test]
    fn arriving_at_edge_does_not_repeat_whole_native_packet() {
        let mut r = routed();
        r.last_local_native = Some(Point { x: 95., y: 50. });
        r.movement(Point { x: 99., y: 52. }, 5., 2., 1).unwrap();
        assert!(!r.remote_gate.load(Ordering::Acquire));
        r.movement(Point { x: 99., y: 54. }, 5., 2., 2).unwrap();
        let f = r.focus.as_ref().unwrap();
        assert_eq!(f.point, Point { x: 105., y: 54. });
    }
    #[test]
    fn real_three_monitor_layout_only_crosses_the_shared_edge() {
        let config: RuntimeLayout = serde_json::from_str(r#"{"local_device_id":"local","remote_device_id":"peer","layout":{"screens":[{"id":"primary","device_id":"local","x":0,"y":0,"width":3840,"height":2160},{"id":"portrait","device_id":"local","x":-1440,"y":-89,"width":1440,"height":2560},{"id":"mac","device_id":"peer","x":3840,"y":0,"width":1470,"height":956}]},"bindings":[{"screen_id":"primary","native_id":"primary","native":{"x":0,"y":0,"width":3840,"height":2160}},{"screen_id":"portrait","native_id":"portrait","native":{"x":-1440,"y":-89,"width":1440,"height":2560}},{"screen_id":"mac","native_id":"1","native":{"x":0,"y":0,"width":1470,"height":956}}]}"#).unwrap();
        for (y, should_cross) in [(500., true), (1000., false), (1600., false)] {
            let mut r = RoutedInput::new(config.clone()).unwrap();
            {
                let mut q = r.input.lock().unwrap();
                q.seed_physical_held([], []).unwrap();
                q.begin_session();
            }
            r.sync();
            assert!(r.enable(r.generation));
            r.movement(Point { x: 3839., y }, 1., 0., 1).unwrap();
            r.movement(Point { x: 3839., y }, 5., 0., 2).unwrap();
            assert_eq!(r.remote_gate.load(Ordering::Acquire), should_cross);
            if should_cross {
                let back = r.movement(Point { x: 3839., y }, -20., 0., 3).unwrap();
                assert!(back.is_some());
                assert!(!r.remote_gate.load(Ordering::Acquire));
            }
        }
    }
    #[test]
    fn edge_lock_blocks_edge_crossing_while_quick_jump_still_switches() {
        let mut r = routed();
        r.set_edge_locked(true);
        assert!(r.is_edge_locked());

        // Pushing past local edge does NOT cross while locked
        r.movement(Point { x: 99., y: 50. }, 10., 0., 1).unwrap();
        assert!(!r.remote_gate.load(Ordering::Acquire));
        assert_eq!(r.focus_identity(), (Some("local".into()), Some("w".into())));

        // QuickJump bypasses edge lock and jumps directly to remote screen center
        assert!(r.quick_jump_to_device("peer", 2).unwrap().is_none());
        assert!(r.remote_gate.load(Ordering::Acquire));
        assert_eq!(r.focus_identity(), (Some("peer".into()), Some("m".into())));

        // Pushing left past remote edge stays clamped on remote while locked
        assert!(r.movement(Point { x: 99., y: 50. }, -200., 0., 3).unwrap().is_none());
        assert!(r.remote_gate.load(Ordering::Acquire));
        assert_eq!(r.focus_identity(), (Some("peer".into()), Some("m".into())));

        // QuickJump back to local returns a warp point and revokes remote_gate
        let warp = r.quick_jump_to_device("local", 4).unwrap();
        assert!(warp.is_some());
        assert!(!r.remote_gate.load(Ordering::Acquire));
        assert_eq!(r.focus_identity(), (Some("local".into()), Some("w".into())));
    }
}
