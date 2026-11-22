//! macOS capture adapter for the common authenticated pair session.
//! No network authority, alternate wire protocol, or UI state lives here.

use crate::platform::{
    macos,
    macos_capture::{CaptureKind, CapturedEvent, MacCapture, StopReason},
};
use crate::{
    arrangement::Point,
    metrics::Log,
    pair_native::{Capture, Platform, Shortcut},
    pair_source::{SharedSource, SourceEvent},
    pair_wire::Display,
};
use anyhow::{Result, ensure};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub fn platform() -> Platform {
    Platform {
        inventory,
        capture: start,
    }
}

fn inventory() -> Result<Vec<Display>> {
    Ok(macos::native_displays()?
        .into_iter()
        .map(|display| Display {
            native_id: display.id.to_string(),
            wire_id: display.id,
            x: display.x,
            y: display.y,
            width: display.width,
            height: display.height,
        })
        .collect())
}

fn start(source: SharedSource, log: Log) -> Result<Box<dyn Capture + Send>> {
    let native = MacCapture::start()?;
    let gate = source.remote_gate();
    let mut capture = MacPairCapture {
        native,
        source,
        gate,
        log,
        stopped: false,
        pending_stop: false,
        failure: None,
        display_epoch: macos::capture_display_epoch(),
    };
    let snapshot = SourceEvent::Snapshot {
        buttons: capture.native.initial_held.buttons.clone(),
        keys: capture.native.initial_held.keys.clone(),
    };
    // A startup failure after a native owner exists is a terminal Stop, not an
    // ordinary reconnectable factory error. No input is replayed on retry.
    if inventory()
        .and_then(|actual| validate_startup_inventory(&capture.source, &actual))
        .is_err()
        || capture.display_epoch.is_none()
        || capture.display_epoch != macos::capture_display_epoch()
    {
        capture.fail("displays changed before capture became ready");
    } else if capture.source.observe(snapshot, capture.log.ns()).is_err() {
        capture.fail("initial input state rejected");
    }
    Ok(Box::new(capture))
}

fn validate_startup_inventory(source: &SharedSource, actual: &[Display]) -> Result<()> {
    crate::pair_wire::validate_inventory(actual)?;
    let runtime = source.runtime();
    let local: Vec<_> = runtime
        .bindings
        .iter()
        .filter(|binding| {
            runtime
                .screen(&binding.screen_id)
                .is_ok_and(|screen| screen.device_id == runtime.local_device_id)
        })
        .collect();
    ensure!(local.len() == actual.len(), "Mac display inventory changed");
    for binding in local {
        let display = actual
            .iter()
            .find(|display| display.native_id == binding.native_id)
            .ok_or_else(|| anyhow::anyhow!("Mac display identity changed"))?;
        ensure!(
            display.wire_id.to_string() == binding.native_id
                && binding.native.x == display.x
                && binding.native.y == display.y
                && binding.native.width == display.width
                && binding.native.height == display.height,
            "Mac display geometry changed"
        );
    }
    Ok(())
}

// Small native boundary lets lifecycle and queued-event tests run without
// enabling an event tap or moving the real cursor.
trait NativeTap: Send {
    fn try_next(&mut self) -> Option<CapturedEvent>;
    fn set_remote_focus(&self, remote: bool) -> bool;
    fn warp_local(&mut self, x: f64, y: f64) -> Result<()>;
    fn stop_reason(&self) -> Option<StopReason>;
    fn stop(&self);
}

impl NativeTap for MacCapture {
    fn try_next(&mut self) -> Option<CapturedEvent> {
        MacCapture::try_next(self)
    }
    fn set_remote_focus(&self, remote: bool) -> bool {
        MacCapture::set_remote_focus(self, remote)
    }
    fn warp_local(&mut self, x: f64, y: f64) -> Result<()> {
        MacCapture::warp_local(self, x, y)
    }
    fn stop_reason(&self) -> Option<StopReason> {
        MacCapture::stop_reason(self)
    }
    fn stop(&self) {
        MacCapture::stop(self);
    }
}

struct MacPairCapture<T: NativeTap> {
    native: T,
    source: SharedSource,
    gate: Arc<AtomicBool>,
    log: Log,
    stopped: bool,
    pending_stop: bool,
    failure: Option<&'static str>,
    display_epoch: Option<u64>,
}

impl<T: NativeTap> MacPairCapture<T> {
    fn fail(&mut self, reason: &'static str) {
        self.halt(reason, Some(reason));
    }

    fn halt(&mut self, reason: &'static str, failure: Option<&'static str>) {
        if self.stopped {
            return;
        }
        self.failure = failure;
        self.stopped = true;
        self.pending_stop = true;
        self.gate.store(false, Ordering::SeqCst);
        self.native.set_remote_focus(false);
        self.native.stop();
        self.source.capture_stopped();
        // Fixed reasons only: never log captured keycodes, coordinates or text.
        self.log.write(
            "mac_pair_capture_stopped",
            json!({"reason": reason, "failed": failure.is_some()}),
        );
    }

    fn check_stop(&mut self) -> bool {
        if self.stopped {
            return true;
        }
        if let Some(reason) = self.native.stop_reason() {
            if reason == StopReason::EmergencyShortcut {
                self.halt("local emergency shortcut", None);
            } else {
                self.fail(reason.label());
            }
        } else if self.source.status().failure.is_some() {
            self.fail("reverse capture/source failed");
        } else if self.display_epoch != macos::capture_display_epoch() {
            if let Some(next_epoch) = macos::capture_display_epoch()
                && inventory()
                    .ok()
                    .is_some_and(|actual| validate_startup_inventory(&self.source, &actual).is_ok())
            {
                self.display_epoch = Some(next_epoch);
            } else {
                self.fail("Mac display configuration changed");
            }
        }
        self.stopped
    }

    fn update_focus(&mut self) -> bool {
        let want_remote = self.gate.load(Ordering::SeqCst);
        if !self.native.set_remote_focus(want_remote) {
            // Transient rejection (e.g. EventTap callback in flight or local key
            // still held) is retried on the next poll; only fail if the native
            // tap itself has latched a terminal StopReason.
            return !self.check_stop();
        }
        !self.stopped
    }
}

impl<T: NativeTap> Capture for MacPairCapture<T> {
    fn poll(&mut self, _now_ns: u64) -> Result<()> {
        // Always inspect the terminal reason even when the tap has no queued
        // events (or its thread has already exited).
        if self.check_stop() {
            return Ok(());
        }
        if !self.update_focus() {
            return Ok(());
        }
        // Bounded to the native ring capacity, so command handling cannot be
        // starved by a continuously replenished high-rate movement stream.
        for _ in 0..512 {
            if self.check_stop() {
                break;
            }
            let Some(event) = self.native.try_next() else {
                break;
            };
            if matches!(
                event.kind,
                CaptureKind::Key {
                    usage: 0x29,
                    down: true
                }
            ) {
                self.halt("local Escape shortcut", None);
                break;
            }
            let Some(mapped) = source_event(event.kind, event.native_x, event.native_y) else {
                continue;
            };
            if matches!(event.kind, CaptureKind::Delta { .. })
                && event.remote_focus != self.gate.load(Ordering::SeqCst)
            {
                // Drop stale queued movement from the opposite side of a focus transition:
                // - gate=true, remote_focus=false: unsuppressed local motion before remote focus took effect
                // - gate=false, remote_focus=true: frozen edge motion queued right before local return
                let _ = self.update_focus();
                continue;
            }
            if !matches!(event.kind, CaptureKind::Delta { .. })
                && event.remote_focus != self.gate.load(Ordering::SeqCst)
            {
                // A queued local down must never be forwarded just because an
                // earlier queued movement has since crossed the shared edge.
                self.fail("queued action crossed a focus boundary");
                break;
            }
            match self.source.observe(mapped, self.log.at(event.captured_at)) {
                Ok(Some(point)) => {
                    if self.gate.load(Ordering::SeqCst) {
                        self.fail("local cursor return has remote focus");
                        break;
                    }
                    self.native.set_remote_focus(false);
                    if self.native.warp_local(point.x, point.y).is_err() {
                        self.fail("local cursor return failed");
                        break;
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    self.fail("reverse capture/source failed");
                    break;
                }
            }
            if self.check_stop() || !self.update_focus() {
                break;
            }
        }
        self.check_stop();
        Ok(())
    }

    fn command(&mut self) -> Option<Shortcut> {
        self.check_stop();
        std::mem::take(&mut self.pending_stop).then_some(Shortcut::Stop)
    }

    fn stop(&mut self) -> Result<()> {
        // Explicit runtime cleanup is not a new capture fault or shortcut.
        self.stopped = true;
        self.pending_stop = false;
        self.gate.store(false, Ordering::SeqCst);
        self.native.set_remote_focus(false);
        self.native.stop();
        self.source.capture_stopped();
        Ok(())
    }

    fn failure(&self) -> Option<&'static str> {
        self.failure
    }
}

fn source_event(kind: CaptureKind, x: f64, y: f64) -> Option<SourceEvent> {
    Some(match kind {
        CaptureKind::Delta { dx, dy } => SourceEvent::Movement {
            native: Point { x, y },
            dx,
            dy,
        },
        CaptureKind::Button { button, down } => SourceEvent::Button { button, down },
        CaptureKind::Key { usage, down } => SourceEvent::Key { usage, down },
        CaptureKind::Scroll {
            horizontal,
            vertical,
        } => SourceEvent::Scroll {
            horizontal,
            vertical,
        },
        CaptureKind::UnsupportedKey { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        arrangement::{Layout, Screen},
        metrics::LogWriter,
        pair_endpoint::LocalSource,
        screen_runtime::{Binding, NativeRect, RuntimeLayout},
    };
    use std::{
        cell::Cell, collections::VecDeque, path::PathBuf, sync::atomic::AtomicUsize, time::Instant,
    };

    #[derive(Default)]
    struct FakeTap {
        events: VecDeque<CapturedEvent>,
        reason: Cell<Option<StopReason>>,
        remote: Cell<bool>,
        warps: Vec<Point>,
    }
    impl NativeTap for FakeTap {
        fn try_next(&mut self) -> Option<CapturedEvent> {
            self.events.pop_front()
        }
        fn set_remote_focus(&self, remote: bool) -> bool {
            self.remote.set(remote);
            true
        }
        fn warp_local(&mut self, x: f64, y: f64) -> Result<()> {
            ensure!(!self.remote.get(), "warp preceded local focus");
            self.warps.push(Point { x, y });
            Ok(())
        }
        fn stop_reason(&self) -> Option<StopReason> {
            self.reason.get()
        }
        fn stop(&self) {
            self.reason.set(Some(StopReason::Requested));
        }
    }
    fn runtime() -> RuntimeLayout {
        RuntimeLayout {
            layout: Layout::new(
                [
                    ("local-screen", "local", 0.),
                    ("remote-screen", "remote", 100.),
                ]
                .into_iter()
                .map(|(id, device, x)| Screen {
                    id: id.into(),
                    device_id: device.into(),
                    x,
                    y: 0.,
                    width: 100.,
                    height: 100.,
                })
                .collect(),
            )
            .unwrap(),
            local_device_id: "local".into(),
            remote_device_id: "remote".into(),
            bindings: [("local-screen", "1"), ("remote-screen", "2")]
                .into_iter()
                .map(|(screen, id)| Binding {
                    screen_id: screen.into(),
                    native_id: id.into(),
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
    struct Fixture {
        capture: MacPairCapture<FakeTap>,
        writer: Option<LogWriter>,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "crossflow-mac-pair-{}-{}.jsonl",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let writer = LogWriter::new(&path, json!({"fake_native": true})).unwrap();
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
            let capture = MacPairCapture {
                native: FakeTap::default(),
                gate: source.remote_gate(),
                source,
                log: writer.log.clone(),
                stopped: false,
                pending_stop: false,
                failure: None,
                display_epoch: macos::capture_display_epoch(),
            };
            Self {
                capture,
                writer: Some(writer),
                path,
            }
        }
        fn push(&mut self, kind: CaptureKind, remote_focus: bool) {
            self.capture.native.events.push_back(CapturedEvent {
                kind,
                captured_at: Instant::now(),
                native_x: 99.,
                native_y: 50.,
                native_timestamp_ns: 0,
                modifiers: 0,
                remote_focus,
            });
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.capture.stop();
            if let Some(writer) = self.writer.take() {
                let _ = writer.finish();
            }
            let _ = std::fs::remove_file(&self.path);
        }
    }
    #[test]
    fn exited_tap_without_events_delivers_terminal_fault_once() {
        let mut f = Fixture::new();
        f.capture
            .native
            .reason
            .set(Some(StopReason::PermissionsLost));
        f.capture.poll(0).unwrap();
        assert_eq!(f.capture.command(), Some(Shortcut::Stop));
        assert_eq!(f.capture.command(), None);
        assert!(f.capture.failure().is_some());
        assert!(!f.capture.source.status().ready);
        assert!(!f.capture.gate.load(Ordering::Acquire));
        f.capture.stop().unwrap();
        assert!(f.capture.failure().is_some(), "cleanup erased the fault");
    }
    #[test]
    fn emergency_and_plain_escape_are_normal_terminal_stops() {
        for native_shortcut in [true, false] {
            let mut f = Fixture::new();
            if native_shortcut {
                f.capture
                    .native
                    .reason
                    .set(Some(StopReason::EmergencyShortcut));
            } else {
                f.push(
                    CaptureKind::Key {
                        usage: 0x29,
                        down: true,
                    },
                    false,
                );
            }
            f.capture.poll(0).unwrap();
            assert_eq!(f.capture.command(), Some(Shortcut::Stop));
            assert_eq!(f.capture.failure(), None);
            assert_eq!(f.capture.source.activity_counter(), 0);
        }
    }
    #[test]
    fn explicit_cleanup_does_not_create_another_stop_or_fault() {
        let mut f = Fixture::new();
        f.capture.stop().unwrap();
        f.capture.poll(0).unwrap();
        assert_eq!(f.capture.command(), None);
        assert_eq!(f.capture.failure(), None);
    }
    #[test]
    fn queued_local_action_after_crossing_is_never_forwarded() {
        for kind in [
            CaptureKind::Key {
                usage: 4,
                down: true,
            },
            CaptureKind::Button {
                button: crate::input::Button::Left,
                down: true,
            },
            CaptureKind::Scroll {
                horizontal: 0,
                vertical: 120,
            },
        ] {
            let mut f = Fixture::new();
            f.capture.source.grant(1, 1).unwrap();
            f.capture
                .source
                .observe(
                    SourceEvent::Movement {
                        native: Point { x: 99., y: 50. },
                        dx: 0.,
                        dy: 0.,
                    },
                    0,
                )
                .unwrap();
            f.push(CaptureKind::Delta { dx: 5., dy: 0. }, false);
            f.push(kind, false);
            f.capture.poll(0).unwrap();
            assert_eq!(f.capture.command(), Some(Shortcut::Stop));
            assert_eq!(
                f.capture.failure(),
                Some("queued action crossed a focus boundary")
            );
            assert_eq!(
                f.capture.source.activity_counter(),
                1,
                "local action was observed remotely"
            );
        }
    }
    #[test]
    fn return_to_local_focus_precedes_exactly_one_cursor_warp() {
        let mut f = Fixture::new();
        f.capture.source.grant(1, 1).unwrap();
        f.capture
            .source
            .observe(
                SourceEvent::Movement {
                    native: Point { x: 99., y: 50. },
                    dx: 0.,
                    dy: 0.,
                },
                0,
            )
            .unwrap();
        f.push(CaptureKind::Delta { dx: 5., dy: 0. }, false);
        f.push(CaptureKind::Delta { dx: -10., dy: 0. }, true);
        f.capture.poll(0).unwrap();
        assert_eq!(f.capture.command(), None);
        assert_eq!(f.capture.failure(), None);
        assert_eq!(f.capture.native.warps.len(), 1);
        assert!(f.capture.native.warps[0].x < 100.);
        assert!(!f.capture.gate.load(Ordering::Acquire));
        f.capture.poll(1).unwrap();
        assert_eq!(f.capture.native.warps.len(), 1, "idle poll replayed a warp");
    }
    #[test]
    fn topology_change_stops_even_with_an_empty_event_queue() {
        let mut f = Fixture::new();
        f.capture.display_epoch = f.capture.display_epoch.map(|n| n.wrapping_add(1));
        f.capture.poll(0).unwrap();
        assert_eq!(f.capture.command(), Some(Shortcut::Stop));
        assert_eq!(
            f.capture.failure(),
            Some("Mac display configuration changed")
        );
    }
    #[test]
    fn startup_inventory_matches_identity_and_native_geometry() {
        let f = Fixture::new();
        let display = Display {
            native_id: "1".into(),
            wire_id: 1,
            x: 0.,
            y: 0.,
            width: 100.,
            height: 100.,
        };
        validate_startup_inventory(&f.capture.source, std::slice::from_ref(&display)).unwrap();
        assert!(validate_startup_inventory(&f.capture.source, &[]).is_err());
        let mut moved = display.clone();
        moved.x = -100.;
        assert!(validate_startup_inventory(&f.capture.source, &[moved]).is_err());
        let mut resized = display.clone();
        resized.width = 200.;
        assert!(validate_startup_inventory(&f.capture.source, &[resized]).is_err());
        let mut replaced = display;
        replaced.wire_id = 3;
        replaced.native_id = "3".into();
        assert!(validate_startup_inventory(&f.capture.source, &[replaced]).is_err());
    }
    #[test]
    fn unsupported_key_events_are_ignored_without_stopping_session() {
        let mut f = Fixture::new();
        f.push(CaptureKind::UnsupportedKey { keycode: 63 }, false);
        f.push(CaptureKind::UnsupportedKey { keycode: 57 }, true);
        f.capture.poll(0).unwrap();
        assert_eq!(f.capture.command(), None);
        assert_eq!(f.capture.failure(), None);
        assert!(f.capture.source.status().ready);
    }
}
