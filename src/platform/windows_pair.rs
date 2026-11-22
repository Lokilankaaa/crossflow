//! Windows capture adapter for a shared two-way session.
use crate::{
    arrangement::Point,
    metrics::Log,
    pair_endpoint::LocalSource,
    pair_native::{Capture, Shortcut},
    pair_source::{SharedSource, SourceEvent},
    platform::windows::{self, CaptureEvent, CaptureStatus},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{Receiver, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const OWNER_LEASE_NS: u64 = 100_000_000;

pub fn inventory() -> Result<Vec<crate::pair_wire::Display>> {
    windows::displays()
        .map_err(anyhow::Error::msg)?
        .into_iter()
        .map(|display| {
            Ok(crate::pair_wire::Display {
                wire_id: windows::receiver_display_id(&display.id)?,
                native_id: display.id,
                x: display.x,
                y: display.y,
                width: display.width,
                height: display.height,
            })
        })
        .collect()
}

pub fn platform() -> crate::pair_native::Platform {
    crate::pair_native::Platform {
        inventory,
        capture: |source, log| Ok(Box::new(WindowsCapture::start(source, log)?)),
    }
}

pub struct WindowsCapture {
    source: SharedSource,
    stop: Arc<AtomicBool>,
    pulse: Arc<AtomicU64>,
    commands: Receiver<Shortcut>,
    failure: Arc<Mutex<Option<String>>>,
    requested_stop: Arc<AtomicBool>,
    stop_delivered: bool,
    worker: Option<JoinHandle<Result<()>>>,
}

fn record_failure(failure: &Mutex<Option<String>>, stop: &AtomicBool, message: String) {
    let mut current = failure.lock().unwrap_or_else(|e| e.into_inner());
    if current.is_none() {
        *current = Some(message.chars().take(512).collect());
    }
    stop.store(true, Ordering::Release);
}

fn validate_startup_inventory(
    source: &SharedSource,
    actual: &[crate::pair_wire::Display],
) -> Result<()> {
    crate::pair_wire::validate_inventory(actual)?;
    let runtime = source.runtime();
    let local = runtime
        .bindings
        .iter()
        .filter(|binding| {
            runtime.layout.screens().iter().any(|screen| {
                screen.id == binding.screen_id && screen.device_id == runtime.local_device_id
            })
        })
        .collect::<Vec<_>>();
    ensure!(
        local.len() == actual.len(),
        "Windows displays changed before capture became ready"
    );
    for binding in local {
        let token: u32 = binding
            .native_id
            .parse()
            .context("Windows local display binding is not a wire token")?;
        let display = actual
            .iter()
            .find(|display| display.wire_id == token)
            .context("Windows display identity changed before capture became ready")?;
        ensure!(
            binding.native.x == display.x
                && binding.native.y == display.y
                && binding.native.width == display.width
                && binding.native.height == display.height,
            "Windows display geometry changed before capture became ready"
        );
    }
    Ok(())
}

fn observe_discrete(
    source: &mut SharedSource,
    event: SourceEvent,
    at: u64,
    captured_remote: Option<bool>,
) -> Result<Option<Point>> {
    if captured_remote != Some(source.remote_gate().load(Ordering::Acquire)) {
        // Prevent a queued local press from becoming a fresh remote press after
        // an earlier move crosses the boundary. Keep held-state accounting, but
        // invalidate readiness first so concurrent grants cannot replay it.
        source.capture_stopped();
        let _ = source.observe(event, at);
        bail!("Windows discrete input crossed an unobserved focus transition");
    }
    source.observe(event, at)
}

impl WindowsCapture {
    pub fn start(source: SharedSource, log: Log) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let pulse = Arc::new(AtomicU64::new(log.ns()));
        let failure = Arc::new(Mutex::new(None));
        let requested_stop = Arc::new(AtomicBool::new(false));
        let (command_tx, commands) = sync_channel(8);
        let (ready_tx, ready_rx) = sync_channel(1);
        let thread_stop = stop.clone();
        let thread_pulse = pulse.clone();
        let thread_failure = failure.clone();
        let physical_stop = requested_stop.clone();
        let mut event_source = source.clone();
        let status_source = source.clone();
        let gate = source.remote_gate();
        let worker = thread::Builder::new().name("crossflow-windows-pair-capture".into()).spawn(move || {
            let event_stop = thread_stop.clone();
            let event_failure = thread_failure.clone();
            let status_stop = thread_stop.clone();
            let status_failure = thread_failure.clone();
            let event_log = log.clone();
            let status_log = log.clone();
            let ready_log = log.clone();
            let ready_pulse = thread_pulse.clone();
            let result = windows::run_routed_input_capture_with_status(
                move |event| {
                    let shortcut = match event {
                        CaptureEvent::Toggle => Some(Shortcut::Toggle),
                        CaptureEvent::Stop => Some(Shortcut::Stop),
                        _ => None,
                    };
                    if let Some(shortcut) = shortcut {
                        if shortcut == Shortcut::Stop {
                            physical_stop.store(true, Ordering::Release);
                        }
                        // Pause local suppression immediately, before the owner
                        // thread consumes the command and releases remote input.
                        if let Err(error) = event_source.revoke() {
                            record_failure(&event_failure, &event_stop, format!("{error:#}"));
                        }
                        if command_tx.try_send(shortcut).is_err() {
                            record_failure(&event_failure, &event_stop, "physical shortcut backlog".into());
                        }
                        return;
                    }
                    let handled = (|| -> Result<()> {
                        let (event, at, provenance) = match event {
                            CaptureEvent::Delta { dx, dy, captured_at } => {
                                let (x, y) = windows::cursor_position().map_err(anyhow::Error::msg)?;
                                (SourceEvent::Movement { native: Point { x, y }, dx: dx as f64, dy: dy as f64 }, event_log.at(captured_at), None)
                            }
                            CaptureEvent::Snapshot { buttons, keys } => (SourceEvent::Snapshot { buttons, keys }, event_log.ns(), None),
                            CaptureEvent::Button { button, down, captured_at, remote_at_capture } => (SourceEvent::Button { button, down }, event_log.at(captured_at), Some(remote_at_capture)),
                            CaptureEvent::Key { usage, down, captured_at, remote_at_capture } => (SourceEvent::Key { usage, down }, event_log.at(captured_at), Some(remote_at_capture)),
                            CaptureEvent::Scroll { horizontal, vertical, captured_at, remote_at_capture } => (SourceEvent::Scroll { horizontal, vertical }, event_log.at(captured_at), Some(remote_at_capture)),
                            _ => unreachable!(),
                        };
                        let warp = match provenance {
                            Some(remote) => observe_discrete(&mut event_source, event, at, remote),
                            None => event_source.observe(event, at),
                        }?;
                        if let Some(point) = warp {
                            windows::warp_cursor(point.x, point.y).map_err(anyhow::Error::msg)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = handled {
                        event_source.capture_stopped();
                        record_failure(&event_failure, &event_stop, format!("{error:#}"));
                        physical_stop.store(true, Ordering::Release);
                    }
                },
                move || {
                    let status = status_source.status();
                    if status.ready && status_log.ns().saturating_sub(thread_pulse.load(Ordering::Acquire)) > OWNER_LEASE_NS {
                        status_source.remote_gate().store(false, Ordering::Release);
                        record_failure(&status_failure, &status_stop, "session owner stopped renewing capture".into());
                        return CaptureStatus::Disconnected;
                    }
                    if !status.connected { CaptureStatus::Disconnected }
                    else if status.enabled { CaptureStatus::Active }
                    else { CaptureStatus::Ready }
                },
                move || {
                    ready_pulse.store(ready_log.ns(), Ordering::Release);
                    let _ = ready_tx.try_send(());
                },
                gate,
                thread_stop.clone(),
                None,
            ).map_err(anyhow::Error::msg);
            if let Err(error) = &result {
                record_failure(&thread_failure, &thread_stop, format!("{error:#}"));
            }
            if let Ok(stats) = &result {
                log.write("pair_capture_finished", serde_json::json!({"motion":stats.motion,"buttons":stats.buttons,"keyboard":stats.keyboard,"wheel":stats.wheel,"read_errors":stats.read_errors}));
            }
            result.map(|_| ())
        })?;
        let mut capture = Self {
            source,
            stop,
            pulse,
            commands,
            failure,
            requested_stop,
            stop_delivered: false,
            worker: Some(worker),
        };
        if ready_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            let _ = capture.stop();
            bail!("Windows input capture did not become ready");
        }
        if let Err(error) =
            inventory().and_then(|actual| validate_startup_inventory(&capture.source, &actual))
        {
            capture
                .stop()
                .context("Windows capture cleanup after changed display inventory")?;
            return Err(error);
        }
        Ok(capture)
    }
}

impl Capture for WindowsCapture {
    fn poll(&mut self, now_ns: u64) -> Result<()> {
        if self.requested_stop.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(message) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            bail!(message);
        }
        // The native loop stops immediately on the physical emergency chord.
        // Keep the command observable so the owner stops the whole session,
        // rather than interpreting that expected exit as a reconnectable fault.
        ensure!(
            !self.stop.load(Ordering::Acquire),
            "Windows capture has stopped"
        );
        ensure!(
            self.worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished()),
            "Windows capture exited"
        );
        self.pulse.store(now_ns, Ordering::Release);
        Ok(())
    }
    fn command(&mut self) -> Option<Shortcut> {
        if self.requested_stop.load(Ordering::Acquire) {
            return if std::mem::replace(&mut self.stop_delivered, true) {
                None
            } else {
                Some(Shortcut::Stop)
            };
        }
        self.commands.try_recv().ok()
    }
    fn failure(&self) -> Option<&'static str> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
            .then_some("Windows physical capture stopped for safety")
    }
    fn stop(&mut self) -> Result<()> {
        let revoke = self.source.revoke();
        self.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_millis(500);
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(2));
        }
        if let Some(worker) = self.worker.take() {
            ensure!(worker.is_finished(), "Windows capture exit not confirmed");
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("Windows capture panicked"))??;
        }
        revoke.context("Windows source could not release local suppression")
    }
    fn warp_local(&mut self, point: Point) -> Result<()> {
        windows::warp_cursor(point.x, point.y).map_err(anyhow::Error::msg)
    }
}
impl Drop for WindowsCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        arrangement::{Layout, Screen},
        input::Button,
        pair_wire::Display,
        screen_runtime::{Binding, NativeRect, RuntimeLayout},
    };

    fn source() -> SharedSource {
        let screens = vec![
            Screen {
                id: "a".into(),
                device_id: "local".into(),
                x: 0.,
                y: 0.,
                width: 100.,
                height: 100.,
            },
            Screen {
                id: "b".into(),
                device_id: "peer".into(),
                x: 100.,
                y: 0.,
                width: 100.,
                height: 100.,
            },
        ];
        let bindings = vec![
            Binding {
                screen_id: "a".into(),
                native_id: "1".into(),
                native: NativeRect {
                    x: -100.,
                    y: 0.,
                    width: 100.,
                    height: 100.,
                },
            },
            Binding {
                screen_id: "b".into(),
                native_id: "2".into(),
                native: NativeRect {
                    x: 0.,
                    y: 0.,
                    width: 200.,
                    height: 200.,
                },
            },
        ];
        let source = SharedSource::new(RuntimeLayout {
            layout: Layout::new(screens).unwrap(),
            local_device_id: "local".into(),
            remote_device_id: "peer".into(),
            bindings,
        })
        .unwrap();
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

    #[test]
    fn locally_delivered_discrete_before_queued_boundary_is_not_replayed_remotely() {
        for event in [
            SourceEvent::Key {
                usage: 4,
                down: true,
            },
            SourceEvent::Button {
                button: Button::Left,
                down: true,
            },
            SourceEvent::Scroll {
                horizontal: 0,
                vertical: 120,
            },
        ] {
            let mut source = source();
            source.grant(1, 1).unwrap();
            for (at, dx) in [(1, 0.), (2, 5.)] {
                source
                    .observe(
                        SourceEvent::Movement {
                            native: Point { x: -1., y: 50. },
                            dx,
                            dy: 0.,
                        },
                        at,
                    )
                    .unwrap();
            }
            assert!(source.remote_gate().load(Ordering::Acquire));
            let activity = source.activity_counter();
            assert!(observe_discrete(&mut source, event, 3, Some(false)).is_err());
            assert_eq!(source.activity_counter(), activity + 1);
            assert!(!source.remote_gate().load(Ordering::Acquire));
            assert!(!source.status().ready);
            assert!(source.next_frame(4).is_err());
            source.revoke().unwrap();
            assert!(
                source.grant(2, 1).is_err(),
                "capture must restart before a new grant"
            );
        }
    }

    #[test]
    fn remote_to_local_and_missing_provenance_fail_closed() {
        for provenance in [Some(true), None] {
            let mut source = source();
            assert!(
                observe_discrete(
                    &mut source,
                    SourceEvent::Key {
                        usage: 4,
                        down: false
                    },
                    1,
                    provenance
                )
                .is_err()
            );
            assert!(!source.status().ready);
        }
        let mut source = source();
        assert!(
            observe_discrete(
                &mut source,
                SourceEvent::Key {
                    usage: 4,
                    down: true
                },
                1,
                Some(false)
            )
            .is_ok()
        );
        assert!(source.status().ready);
    }

    #[test]
    fn startup_inventory_rejects_hotplug_token_and_geometry_changes() {
        let source = source();
        let display = Display {
            native_id: "native-name".into(),
            wire_id: 1,
            x: -100.,
            y: 0.,
            width: 100.,
            height: 100.,
        };
        assert!(validate_startup_inventory(&source, std::slice::from_ref(&display)).is_ok());
        assert!(validate_startup_inventory(&source, &[]).is_err());
        let mut changed = display.clone();
        changed.wire_id = 3;
        assert!(validate_startup_inventory(&source, &[changed]).is_err());
        let mut changed = display.clone();
        changed.x = 0.;
        assert!(validate_startup_inventory(&source, &[changed]).is_err());
        let mut changed = display.clone();
        changed.width = 200.;
        assert!(validate_startup_inventory(&source, &[changed]).is_err());
        assert!(validate_startup_inventory(&source, &[display.clone(), display]).is_err());
    }
}
