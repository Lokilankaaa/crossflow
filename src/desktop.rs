//! Private native-client control plane. Input never travels through these pipes.
//! The stdio runtime belongs to a dedicated child process: blocked pipe readers
//! are detached on shutdown and must not outlive that process.
use crate::state::Motion;
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_LINE: usize = 4096;
const OUTPUT_LEASE: Duration = Duration::from_secs(2);

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Arm { epoch: u64 },
    Pause {},
    Stop {},
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    protocol: u8,
    state: &'static str,
    epoch: u64,
    capture_ready: bool,
    error: Option<String>,
}

#[derive(Clone)]
pub struct Control {
    pub motion: Arc<Mutex<Motion>>,
    pub(crate) routed: Option<Arc<Mutex<crate::routed_desktop::RoutedInput>>>,
    pub stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    notice: Arc<Mutex<Option<String>>>,
}

impl Control {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            motion: Arc::new(Mutex::new(Motion::default())),
            routed: None,
            stop,
            ready: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(Mutex::new(None)),
            notice: Arc::new(Mutex::new(None)),
        }
    }
    pub fn capture_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.pause();
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.request_stop();
    }
    pub fn network_finished(&self, result: anyhow::Result<()>) {
        if let Err(error) = result {
            if !self.cancelled.load(Ordering::Acquire) {
                self.fail(&format!("{error:#}"));
            }
        }
        self.request_stop();
    }
    pub fn fail(&self, message: &str) {
        self.stop.store(true, Ordering::Release);
        let mut failure = self.failure.lock().unwrap_or_else(|e| e.into_inner());
        if failure.is_none() {
            *failure = Some(message.chars().take(512).collect());
        }
        drop(failure);
        self.pause();
    }
    fn pause(&self) {
        if let Some(route) = &self.routed {
            route.lock().unwrap_or_else(|e| e.into_inner()).pause();
            return;
        }
        let mut motion = self.motion.lock().unwrap_or_else(|e| e.into_inner());
        motion.active = false;
        motion.pending_since_ns = None;
        motion.epoch = motion.epoch.saturating_add(1);
    }
    fn apply(&self, command: Command) {
        match command {
            Command::Stop {} => self.cancel(),
            Command::Pause {} => {
                self.pause();
                *self.notice.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            Command::Arm { epoch } => {
                if let Some(route) = &self.routed {
                    let valid = !self.stop.load(Ordering::Acquire)
                        && self.ready.load(Ordering::Acquire)
                        && route
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .enable(epoch);
                    *self.notice.lock().unwrap_or_else(|e| e.into_inner()) = (!valid)
                        .then(|| "state_changed: refresh state before starting sharing".into());
                    return;
                }
                let mut motion = self.motion.lock().unwrap_or_else(|e| e.into_inner());
                let valid = !self.stop.load(Ordering::Acquire)
                    && self.ready.load(Ordering::Acquire)
                    && motion.connected
                    && !motion.active
                    && motion.epoch == epoch
                    && epoch < u64::MAX;
                if valid {
                    motion.active = true;
                    motion.epoch += 1;
                    motion.pending_since_ns = None;
                }
                drop(motion);
                *self.notice.lock().unwrap_or_else(|e| e.into_inner()) =
                    (!valid).then(|| "state_changed: refresh state before starting sharing".into());
            }
        }
    }
    pub fn snapshot(&self, finished: bool) -> Status {
        let motion = if let Some(route) = &self.routed {
            let mut r = route.lock().unwrap_or_else(|e| e.into_inner());
            let connected = r.sync();
            Motion {
                connected,
                active: r.enabled,
                epoch: r.generation,
                ..Motion::default()
            }
        } else {
            *self.motion.lock().unwrap_or_else(|e| e.into_inner())
        };
        let failure = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ready = self.ready.load(Ordering::Acquire);
        let state = if finished {
            if failure.is_some() {
                "error"
            } else {
                "stopped"
            }
        } else if self.stop.load(Ordering::Acquire) {
            "stopping"
        } else if !ready {
            "starting"
        } else if !motion.connected {
            "connecting"
        } else if motion.active {
            "sharing"
        } else {
            "paused"
        };
        Status {
            protocol: 1,
            state,
            epoch: motion.epoch,
            capture_ready: ready,
            error: failure.or_else(|| {
                self.notice
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            }),
        }
    }
    pub fn toggle(&self) {
        let status = self.snapshot(false);
        self.apply(if status.state == "sharing" {
            Command::Pause {}
        } else {
            Command::Arm {
                epoch: status.epoch,
            }
        });
    }
}

fn read_command(reader: &mut impl BufRead) -> Result<Option<Command>, &'static str> {
    let mut bytes = Vec::new();
    let count = reader
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "control input read failed")?;
    if count == 0 {
        return Ok(None);
    }
    if count > MAX_LINE || bytes.last() != Some(&b'\n') {
        return Err("control message exceeds limit or is incomplete");
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| "invalid control message")
}

fn read_controls(input: impl Read, control: Control) {
    let mut reader = BufReader::with_capacity(MAX_LINE, input);
    let mut window = Instant::now();
    let mut count = 0u32;
    while !control.stop.load(Ordering::Acquire) {
        match read_command(&mut reader) {
            Ok(Some(command)) => {
                if window.elapsed() >= Duration::from_secs(1) {
                    window = Instant::now();
                    count = 0;
                }
                count += 1;
                if count > 64 {
                    control.fail("control command rate exceeded");
                    break;
                }
                control.apply(command);
            }
            Ok(None) => {
                control.cancel();
                break;
            }
            Err(message) => {
                control.fail(message);
                break;
            }
        }
    }
}

/// A bounded status channel; a GUI that stops draining stdout cannot keep sharing.
pub struct ControlIo {
    control: Control,
    tx: SyncSender<(u64, Status)>,
    written: Arc<AtomicU64>,
    progress_ms: Arc<AtomicU64>,
    clock: Instant,
    sequence: u64,
}
impl ControlIo {
    pub fn start(
        input: impl Read + Send + 'static,
        mut output: impl Write + Send + 'static,
        control: Control,
    ) -> Self {
        let (tx, rx) = sync_channel::<(u64, Status)>(1);
        let written = Arc::new(AtomicU64::new(0));
        let progress_ms = Arc::new(AtomicU64::new(0));
        let clock = Instant::now();
        let reader_control = control.clone();
        thread::spawn(move || read_controls(input, reader_control));
        let writer_control = control.clone();
        let ack = written.clone();
        let progress = progress_ms.clone();
        thread::spawn(move || {
            for (sequence, status) in rx {
                let result = serde_json::to_writer(&mut output, &status)
                    .map_err(std::io::Error::other)
                    .and_then(|_| output.write_all(b"\n"))
                    .and_then(|_| output.flush());
                if result.is_err() {
                    writer_control.fail("control output closed");
                    break;
                }
                progress.store(clock.elapsed().as_millis() as u64, Ordering::Release);
                ack.store(sequence, Ordering::Release);
            }
        });
        Self {
            control,
            tx,
            written,
            progress_ms,
            clock,
            sequence: 0,
        }
    }
    pub fn publish(&mut self, finished: bool) -> Option<u64> {
        let now = self.clock.elapsed().as_millis() as u64;
        if !finished
            && now.saturating_sub(self.progress_ms.load(Ordering::Acquire))
                > OUTPUT_LEASE.as_millis() as u64
        {
            self.control.fail("native client stopped reading status");
        }
        self.sequence += 1;
        match self
            .tx
            .try_send((self.sequence, self.control.snapshot(finished)))
        {
            Ok(()) => Some(self.sequence),
            Err(TrySendError::Full(_)) => None,
            Err(TrySendError::Disconnected(_)) => {
                self.control.fail("control output unavailable");
                None
            }
        }
    }
    pub fn finish(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut pending = None;
        while Instant::now() < deadline {
            if pending.is_none() {
                pending = self.publish(true);
            }
            if pending.is_some_and(|sequence| self.written.load(Ordering::Acquire) >= sequence) {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

pub struct Options {
    pub host: String,
    pub identity: std::path::PathBuf,
    /// Only app-managed pinned peers use unauthenticated LAN routing hints.
    pub discover_peer: bool,
    pub seconds: f64,
    pub max_hz: u32,
    pub runtime_layout: Option<crate::screen_runtime::RuntimeLayout>,
}

#[cfg(windows)]
pub fn run(
    options: Options,
    writer: crate::metrics::LogWriter,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use crate::{
        platform::windows::{CaptureEvent, CaptureStatus, run_capture_headless_with_status},
        session,
    };
    let (input, output) = crate::injection_worker::stdio_files()?;
    let mut control = Control::new(stop.clone());
    if let Some(config) = options.runtime_layout.clone() {
        control.routed = Some(Arc::new(Mutex::new(
            crate::routed_desktop::RoutedInput::new(config)?,
        )));
    }
    let mut io = ControlIo::start(input, output, control.clone());
    io.publish(false);
    let validated = thread::scope(|scope| {
        let check = scope.spawn(|| -> anyhow::Result<()> {
            anyhow::ensure!(
                options.host.parse::<std::net::SocketAddr>().is_ok(),
                "target must be an IP address and port"
            );
            anyhow::ensure!(
                options.seconds.is_finite() && options.seconds > 0.0 && options.seconds <= 3600.0,
                "seconds must be >0 and <=3600"
            );
            anyhow::ensure!(
                (60..=8000).contains(&options.max_hz),
                "max-hz must be 60..8000"
            );
            crate::security::Identity::load(&options.identity)?;
            if let Some(config) = &options.runtime_layout {
                validate_runtime_inventory(config, &options, &stop, &writer.log)?;
            }
            Ok(())
        });
        while !check.is_finished() {
            io.publish(false);
            thread::sleep(Duration::from_millis(50));
        }
        check
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("runtime validation panicked")))
    });
    if let Err(error) = validated {
        control.fail(&format!("{error:#}"));
        writer.finish()?;
        io.finish();
        return Err(error);
    }
    let network_control = control.clone();
    let network_log = writer.log.clone();
    let duration = Duration::from_secs_f64(options.seconds);
    let network = thread::spawn(move || {
        if let Some(route) = &network_control.routed {
            let input = route
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .input
                .clone();
            let send = if options.discover_peer {
                session::send_ordered_discovered
            } else {
                session::send_ordered
            };
            let result = send(
                session::SendOptions {
                    host: &options.host,
                    bundle: &options.identity,
                    seconds: options.seconds,
                    max_hz: options.max_hz,
                    synthetic: false,
                },
                input,
                network_control.stop.clone(),
                network_log,
            );
            network_control.network_finished(result);
            return;
        }
        let send = if options.discover_peer {
            session::send_discovered
        } else {
            session::send
        };
        let result = send(
            session::SendOptions {
                host: &options.host,
                bundle: &options.identity,
                seconds: options.seconds,
                max_hz: options.max_hz,
                synthetic: false,
            },
            network_control.motion.clone(),
            network_control.stop.clone(),
            network_log,
        );
        network_control.network_finished(result);
    });
    let event_control = control.clone();
    let status_control = control.clone();
    let ready_control = control.clone();
    let capture_control = control.clone();
    let capture_log = writer.log.clone();
    let capture = thread::spawn(move || {
        if capture_control.routed.is_some() {
            if let Err(e) =
                crate::routed_desktop::capture(capture_control.clone(), capture_log, duration)
            {
                capture_control.fail(&format!("{e:#}"));
            }
            capture_control.request_stop();
            return;
        }
        let result = run_capture_headless_with_status(
            move |event| match event {
                CaptureEvent::Delta {
                    dx,
                    dy,
                    captured_at,
                } => {
                    let t0 = capture_log.at(captured_at);
                    capture_log.write("capture", serde_json::json!({"t0_ns":t0}));
                    let mut motion = event_control
                        .motion
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if !event_control.stop.load(Ordering::Acquire) {
                        motion.add(dx as f64, dy as f64, t0);
                    }
                }
                CaptureEvent::Toggle => {
                    let (active, epoch) = {
                        let motion = event_control
                            .motion
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        (motion.active, motion.epoch)
                    };
                    event_control.apply(if active {
                        Command::Pause {}
                    } else {
                        Command::Arm { epoch }
                    });
                    capture_log.write("toggle", serde_json::json!({"active":event_control.motion.lock().unwrap_or_else(|e| e.into_inner()).active}));
                }
                CaptureEvent::Stop => event_control.cancel(),
                _ => {}
            },
            move || {
                let motion = status_control
                    .motion
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if !motion.connected {
                    CaptureStatus::Disconnected
                } else if motion.active {
                    CaptureStatus::Active
                } else {
                    CaptureStatus::Ready
                }
            },
            move || ready_control.capture_ready(),
            capture_control.stop.clone(),
            Some(duration),
        );
        if let Err(error) = result {
            capture_control.fail(&error);
        }
        capture_control.request_stop();
    });
    while !capture.is_finished() || !network.is_finished() {
        if capture.is_finished() || network.is_finished() {
            control.request_stop();
        }
        io.publish(false);
        thread::sleep(Duration::from_millis(100));
    }
    let capture_panicked = capture.join().is_err();
    let network_panicked = network.join().is_err();
    if capture_panicked || network_panicked {
        control.fail("desktop session thread panicked");
    }
    control.request_stop();
    if let Err(error) = writer.finish() {
        control.fail(&format!("{error:#}"));
    }
    let delivered = io.finish();
    let status = control.snapshot(true);
    anyhow::ensure!(delivered, "final client status was not delivered");
    if status.state == "error" {
        anyhow::bail!("{}", status.error.unwrap_or_default());
    }
    Ok(())
}

#[cfg(windows)]
fn validate_runtime_inventory(
    config: &crate::screen_runtime::RuntimeLayout,
    options: &Options,
    stop: &AtomicBool,
    log: &crate::metrics::Log,
) -> anyhow::Result<()> {
    use crate::screen_runtime::NativeRect;
    config.validate()?;
    let local = crate::platform::windows::displays().map_err(anyhow::Error::msg)?;
    let remote = crate::session::query_displays(
        &options.host,
        &options.identity,
        stop,
        log,
        options.discover_peer,
    )?;
    anyhow::ensure!(
        config
            .layout
            .screens()
            .iter()
            .filter(|s| s.device_id == config.local_device_id)
            .count()
            == local.len()
            && config
                .layout
                .screens()
                .iter()
                .filter(|s| s.device_id == config.remote_device_id)
                .count()
                == remote.len(),
        "display inventory changed; refresh all screens before connecting"
    );
    for b in &config.bindings {
        let s = config.screen(&b.screen_id)?;
        let rect = if s.device_id == config.local_device_id {
            local
                .iter()
                .find(|d| d.id == b.native_id)
                .map(|d| NativeRect {
                    x: d.x,
                    y: d.y,
                    width: d.width,
                    height: d.height,
                })
        } else {
            remote
                .iter()
                .find(|d| d.id.to_string() == b.native_id)
                .map(|d| NativeRect {
                    x: d.x,
                    y: d.y,
                    width: d.width,
                    height: d.height,
                })
        };
        anyhow::ensure!(
            rect.as_ref() == Some(&b.native),
            "screen configuration changed; refresh both devices' screens before connecting"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Condvar,
        mpsc::{Receiver, Sender, channel},
    };

    #[derive(Default)]
    struct Gate {
        released: Mutex<bool>,
        wake: Condvar,
    }
    impl Gate {
        fn wait(&self) {
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    struct GatedEof {
        gate: Arc<Gate>,
        exited: Sender<()>,
    }
    impl Read for GatedEof {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.gate.wait();
            Ok(0)
        }
    }
    impl Drop for GatedEof {
        fn drop(&mut self) {
            let _ = self.exited.send(());
        }
    }

    struct ControlledWriter {
        gate: Arc<Gate>,
        fail: bool,
        entered: Option<Sender<()>>,
        exited: Sender<()>,
    }
    impl Write for ControlledWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if self.fail {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "test output closed",
                ));
            }
            self.gate.wait();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for ControlledWriter {
        fn drop(&mut self) {
            let _ = self.exited.send(());
        }
    }

    // ControlIo intentionally detaches its pipe threads. The test fixture releases
    // both fake pipes and observes their owners being dropped, even on assertion
    // failure, so these tests leave no permanently blocked background threads.
    struct IoFixture {
        control: Control,
        io: Option<ControlIo>,
        input_gate: Arc<Gate>,
        output_gate: Arc<Gate>,
        output_entered: Receiver<()>,
        reader_exited: Option<Receiver<()>>,
        writer_exited: Option<Receiver<()>>,
    }
    impl IoFixture {
        fn active(fail_output: bool) -> Self {
            let control = control();
            control.capture_ready();
            control.motion.lock().unwrap().connected(true);
            let epoch = control.motion.lock().unwrap().epoch;
            control.apply(Command::Arm { epoch });
            control.motion.lock().unwrap().add(4.0, -2.0, 1);
            assert!(control.motion.lock().unwrap().active);
            assert!(control.motion.lock().unwrap().pending_since_ns.is_some());
            let input_gate = Arc::new(Gate::default());
            let output_gate = Arc::new(Gate::default());
            let (entered_tx, output_entered) = channel();
            let (reader_tx, reader_exited) = channel();
            let (writer_tx, writer_exited) = channel();
            let io = ControlIo::start(
                GatedEof {
                    gate: input_gate.clone(),
                    exited: reader_tx,
                },
                ControlledWriter {
                    gate: output_gate.clone(),
                    fail: fail_output,
                    entered: Some(entered_tx),
                    exited: writer_tx,
                },
                control.clone(),
            );
            Self {
                control,
                io: Some(io),
                input_gate,
                output_gate,
                output_entered,
                reader_exited: Some(reader_exited),
                writer_exited: Some(writer_exited),
            }
        }
        fn start_output(&mut self) {
            assert!(self.io.as_mut().unwrap().publish(false).is_some());
            self.output_entered
                .recv_timeout(Duration::from_secs(1))
                .expect("writer must begin output");
        }
        fn stopped_and_paused(&self) -> bool {
            let motion = self.control.motion.lock().unwrap();
            self.control.stop.load(Ordering::Acquire)
                && !motion.active
                && motion.pending_since_ns.is_none()
        }
        fn cleanup(&mut self) -> bool {
            self.control.request_stop();
            drop(self.io.take());
            self.input_gate.release();
            self.output_gate.release();
            let mut exited = true;
            for receiver in [&mut self.reader_exited, &mut self.writer_exited] {
                if let Some(receiver) = receiver.take() {
                    exited &= receiver.recv_timeout(Duration::from_secs(1)).is_ok();
                }
            }
            exited
        }
    }
    impl Drop for IoFixture {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        loop {
            if ready() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn blocked_output_expires_sharing_and_finish_is_bounded() {
        let mut fixture = IoFixture::active(false);
        fixture.start_output();
        let first_write = Instant::now();
        // Real output remains blocked beyond its two-second lease. Publishing
        // continues just as the session supervisor does; a full queue is normal.
        assert!(wait_until(OUTPUT_LEASE + Duration::from_secs(1), || {
            fixture.io.as_mut().unwrap().publish(false);
            first_write.elapsed() > OUTPUT_LEASE && fixture.stopped_and_paused()
        }));
        assert!(fixture.io.as_ref().unwrap().clock.elapsed() > OUTPUT_LEASE);
        assert!(first_write.elapsed() > OUTPUT_LEASE);
        assert_eq!(
            fixture.io.as_ref().unwrap().written.load(Ordering::Acquire),
            0
        );
        assert_eq!(
            fixture.control.snapshot(true).error.as_deref(),
            Some("native client stopped reading status")
        );
        let finish_started = Instant::now();
        assert!(
            !fixture.io.as_mut().unwrap().finish(),
            "blocked output cannot acknowledge a final status"
        );
        assert!(
            finish_started.elapsed() < Duration::from_millis(1500),
            "finish must not join the blocked writer"
        );
        assert!(
            fixture.cleanup(),
            "both fake pipe threads must exit after release"
        );
    }

    #[test]
    fn output_write_failure_stops_and_discards_pending_motion() {
        let mut fixture = IoFixture::active(true);
        fixture.start_output();
        assert!(wait_until(Duration::from_secs(1), || fixture.stopped_and_paused()));
        assert_eq!(
            fixture.control.snapshot(true).error.as_deref(),
            Some("control output closed")
        );
        assert_eq!(
            fixture.io.as_ref().unwrap().written.load(Ordering::Acquire),
            0
        );
        assert!(
            fixture.cleanup(),
            "failed writer and released reader must exit"
        );
    }

    #[test]
    fn input_eof_stops_without_waiting_for_blocked_status_output() {
        let mut fixture = IoFixture::active(false);
        fixture.start_output();
        let eof_started = Instant::now();
        fixture.input_gate.release();
        // Do not publish: EOF itself must stop the session, independently of
        // the output watchdog and without needing the writer's mutex or progress.
        assert!(wait_until(Duration::from_millis(750), || fixture.stopped_and_paused()));
        assert!(eof_started.elapsed() < OUTPUT_LEASE);
        assert!(fixture.control.snapshot(true).error.is_none());
        assert_eq!(
            fixture.io.as_ref().unwrap().written.load(Ordering::Acquire),
            0
        );
        assert!(!*fixture.output_gate.released.lock().unwrap());
        assert!(
            fixture.cleanup(),
            "EOF reader and released writer must exit"
        );
    }

    fn control() -> Control {
        Control::new(Arc::new(AtomicBool::new(false)))
    }
    #[test]
    fn network_error_is_reported_after_internal_stop_without_user_cancel() {
        let c = control();
        c.motion.lock().unwrap().active = true;
        // The network worker may set stop itself before returning its error.
        // Cleanup also calls request_stop; neither is evidence of user intent.
        c.stop.store(true, Ordering::Release);
        c.request_stop();
        assert!(!c.cancelled.load(Ordering::Acquire));
        c.network_finished(Err(anyhow::anyhow!("NoAuthenticatedSession")));
        let status = c.snapshot(true);
        assert_eq!(status.state, "error");
        assert_eq!(status.error.as_deref(), Some("NoAuthenticatedSession"));
        assert!(!c.motion.lock().unwrap().active);
    }
    #[test]
    fn explicit_cancel_keeps_later_network_error_as_normal_stop() {
        let c = control();
        c.motion.lock().unwrap().active = true;
        c.cancel();
        assert!(c.cancelled.load(Ordering::Acquire));
        c.network_finished(Err(anyhow::anyhow!("NoAuthenticatedSession")));
        let status = c.snapshot(true);
        assert_eq!(status.state, "stopped");
        assert!(status.error.is_none());
        assert!(c.stop.load(Ordering::Acquire));
        assert!(!c.motion.lock().unwrap().active);
    }
    #[test]
    fn arm_requires_ready_connection_and_exact_epoch() {
        let c = control();
        c.apply(Command::Arm { epoch: 0 });
        assert!(!c.motion.lock().unwrap().active);
        c.motion.lock().unwrap().connected(true);
        c.apply(Command::Arm { epoch: 1 });
        assert!(!c.motion.lock().unwrap().active);
        c.capture_ready();
        c.apply(Command::Arm { epoch: 0 });
        assert!(!c.motion.lock().unwrap().active);
        c.apply(Command::Arm { epoch: 1 });
        assert!(c.motion.lock().unwrap().active);
        c.apply(Command::Arm { epoch: 1 });
        assert!(
            c.motion.lock().unwrap().active,
            "duplicate arm is not a toggle"
        );
        c.apply(Command::Pause {});
        c.apply(Command::Arm { epoch: 2 });
        assert!(
            !c.motion.lock().unwrap().active,
            "old arm must not undo pause"
        );
    }
    #[test]
    fn reconnect_and_stop_never_rearm_from_stale_commands() {
        let c = control();
        c.capture_ready();
        c.motion.lock().unwrap().connected(true);
        c.apply(Command::Arm { epoch: 1 });
        c.motion.lock().unwrap().connected(false);
        c.motion.lock().unwrap().connected(true);
        c.apply(Command::Arm { epoch: 1 });
        assert_eq!(c.snapshot(false).state, "paused");
        let epoch = c.motion.lock().unwrap().epoch;
        c.apply(Command::Stop {});
        c.apply(Command::Arm { epoch });
        assert!(!c.motion.lock().unwrap().active);
        assert_eq!(c.snapshot(false).state, "stopping");
    }
    #[test]
    fn bounded_protocol_rejects_truncated_unknown_and_oversized_messages() {
        for bytes in [
            b"{\"op\":\"arm\"}\n".to_vec(),
            b"{\"op\":\"pause\",\"extra\":1}\n".to_vec(),
            b"{\"op\":\"stop\"}".to_vec(),
            vec![b'x'; MAX_LINE + 1],
        ] {
            assert!(read_command(&mut std::io::Cursor::new(bytes)).is_err());
        }
        assert!(matches!(
            read_command(&mut std::io::Cursor::new(b"{\"op\":\"pause\"}\n")),
            Ok(Some(Command::Pause {}))
        ));
    }
    #[test]
    fn eof_and_invalid_input_stop_an_active_session() {
        for bytes in [Vec::new(), b"invalid\n".to_vec()] {
            let c = control();
            c.motion.lock().unwrap().active = true;
            read_controls(std::io::Cursor::new(bytes), c.clone());
            assert!(c.stop.load(Ordering::Acquire));
            assert!(!c.motion.lock().unwrap().active);
        }
    }
    #[test]
    fn command_flood_latches_stop() {
        let c = control();
        read_controls(
            std::io::Cursor::new("{\"op\":\"pause\"}\n".repeat(65)),
            c.clone(),
        );
        assert!(c.stop.load(Ordering::Acquire));
        assert_eq!(c.snapshot(true).state, "error");
    }
}
