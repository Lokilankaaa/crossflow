//! Opt-in private native-client supervision for a dedicated receiver process.
//! Input events and credentials never use this channel. Pipe threads may block;
//! only use this runtime in a child process that exits after `finish`.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_LINE: usize = 256;
const OUTPUT_LEASE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub enum StartupRejection {
    Accessibility,
    InputMonitoring,
}

impl StartupRejection {
    fn reason(self) -> &'static str {
        match self {
            Self::Accessibility => "accessibility permission required",
            Self::InputMonitoring => "input monitoring permission required",
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Stop {},
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub protocol: u8,
    pub state: &'static str,
    pub listen_endpoint: Option<String>,
    pub error: Option<String>,
}

#[derive(Default)]
struct State {
    endpoint: Option<String>,
    connected: bool,
    enabled: bool,
    finished: bool,
    error: Option<&'static str>,
    startup_rejection: Option<StartupRejection>,
}

#[derive(Clone)]
pub struct Control {
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
}

impl crate::session::ReceiveObserver for Control {
    fn listening(&self, endpoint: SocketAddr) {
        self.set_listening(endpoint.to_string());
    }
    fn connected(&self, connected: bool) {
        self.set_connected(connected);
    }
    fn enabled(&self, enabled: bool) {
        self.set_enabled(enabled);
    }
}

impl Control {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Call only after binding and adapter initialization have succeeded.
    pub fn set_listening(&self, endpoint: String) {
        let Ok(endpoint) = endpoint.parse::<SocketAddr>() else {
            self.fail("invalid receiver endpoint");
            return;
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.finished {
            state.endpoint = Some(endpoint.to_string());
        }
    }

    /// Connected means an authenticated transport, not merely TCP acceptance.
    pub fn set_connected(&self, connected: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.finished {
            state.connected = connected;
            // Every connection starts paused; a previous remote enable is stale.
            state.enabled = false;
        }
    }

    /// Reports the receiver's applied enable/disable transition; never arms it.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.finished {
            state.enabled = enabled && state.connected;
        }
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Call only when a read-only preflight rejects startup, before constructing
    /// any input adapter or worker. Process exit still must be confirmed by UI.
    pub fn reject_startup(&self, reason: StartupRejection) {
        self.request_stop();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.finished || state.endpoint.is_some() || state.connected || state.enabled {
            drop(state);
            self.fail("receiver failed");
            return;
        }
        if state.error.is_none() {
            state.error = Some(reason.reason());
            state.startup_rejection = Some(reason);
        }
    }

    /// Only allow fixed public messages on this UI channel. Raw caller errors
    /// may contain credential paths or other details and are deliberately erased.
    pub fn fail(&self, message: &str) {
        let message = match message {
            "control input read failed" => "control input read failed",
            "invalid control message" => "invalid control message",
            "control message exceeds limit or is incomplete" => {
                "control message exceeds limit or is incomplete"
            }
            "control output closed" => "control output closed",
            "control output unavailable" => "control output unavailable",
            "native client stopped reading status" => "native client stopped reading status",
            "final client status was not delivered" => "final client status was not delivered",
            "invalid receiver endpoint" => "invalid receiver endpoint",
            _ => "receiver failed",
        };
        self.request_stop();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.startup_rejection = None;
        if state.error.is_none() {
            state.error = Some(message);
        }
    }

    pub fn snapshot(&self) -> Status {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let name = if state.finished {
            if state.startup_rejection.is_some() && state.endpoint.is_none() {
                "startup_rejected"
            } else if state.error.is_some() {
                "error"
            } else {
                "stopped"
            }
        } else if self.stop.load(Ordering::Acquire) {
            "stopping"
        } else if state.endpoint.is_none() {
            "starting"
        } else if !state.connected {
            "listening"
        } else if state.enabled {
            "sharing"
        } else {
            "paused"
        };
        Status {
            protocol: 1,
            state: name,
            listen_endpoint: state.endpoint.clone(),
            error: state.error.map(str::to_owned),
        }
    }

    fn complete(&self, result: Result<()>) {
        let rejected = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .startup_rejection
            .is_some();
        if result.is_err() && !rejected {
            self.fail("receiver failed");
        }
        self.request_stop();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.finished = true;
        state.connected = false;
        state.enabled = false;
    }
}

fn read_command(reader: &mut impl BufRead) -> Result<Option<Command>, &'static str> {
    let mut bytes = Vec::with_capacity(MAX_LINE + 1);
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
    // There is exactly one valid command, and it ends control processing. No
    // command flood can keep the session running or accumulate a command queue.
    match read_command(&mut BufReader::with_capacity(MAX_LINE, input)) {
        Ok(None | Some(Command::Stop {})) => control.request_stop(),
        Err(error) => control.fail(error),
    }
}

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
    ) -> Result<Self> {
        let (tx, rx) = sync_channel::<(u64, Status)>(1);
        let written = Arc::new(AtomicU64::new(0));
        let progress_ms = Arc::new(AtomicU64::new(0));
        let clock = Instant::now();
        let reader_control = control.clone();
        if thread::Builder::new()
            .name("crossflow-receiver-control".into())
            .spawn(move || read_controls(input, reader_control))
            .is_err()
        {
            control.fail("receiver failed");
            anyhow::bail!("receiver control reader unavailable");
        }
        let writer_control = control.clone();
        let ack = written.clone();
        let progress = progress_ms.clone();
        if thread::Builder::new()
            .name("crossflow-receiver-status".into())
            .spawn(move || {
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
            })
            .is_err()
        {
            control.fail("receiver failed");
            anyhow::bail!("receiver control writer unavailable");
        }
        Ok(Self {
            control,
            tx,
            written,
            progress_ms,
            clock,
            sequence: 0,
        })
    }

    /// Call periodically (e.g. every 100 ms) from a dedicated supervisor until
    /// the synchronous receive path and all of its cleanup have completed.
    pub fn tick(&mut self) {
        let _ = self.publish(false);
    }

    fn publish(&mut self, final_status: bool) -> Option<u64> {
        let now = self.clock.elapsed().as_millis() as u64;
        if !final_status
            && now.saturating_sub(self.progress_ms.load(Ordering::Acquire))
                > OUTPUT_LEASE.as_millis() as u64
        {
            self.control.fail("native client stopped reading status");
        }
        self.sequence = self.sequence.checked_add(1).unwrap_or_else(|| {
            self.control.fail("receiver failed");
            u64::MAX
        });
        match self.tx.try_send((self.sequence, self.control.snapshot())) {
            Ok(()) => Some(self.sequence),
            Err(TrySendError::Full(_)) => None,
            Err(TrySendError::Disconnected(_)) => {
                self.control.fail("control output unavailable");
                None
            }
        }
    }

    /// Call exactly once AFTER input ownership, discovery and logs are cleaned
    /// up. A successfully delivered final error is still returned as an error.
    pub fn finish(&mut self, result: Result<()>) -> Result<()> {
        self.control.complete(result);
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut pending = None;
        while Instant::now() < deadline {
            if pending.is_none() {
                pending = self.publish(true);
            }
            if pending.is_some_and(|sequence| self.written.load(Ordering::Acquire) >= sequence) {
                let status = self.control.snapshot();
                ensure!(
                    status.error.is_none(),
                    "{}",
                    status.error.unwrap_or_default()
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.control.fail("final client status was not delivered");
        anyhow::bail!("final client status was not delivered")
    }
}

impl Drop for ControlIo {
    fn drop(&mut self) {
        self.control.request_stop();
        // Never wait for a blocked inherited pipe. Dedicated process exit owns
        // remaining pipe threads; dropping this value is not cleanup evidence.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{
            Condvar,
            mpsc::{Receiver, Sender, channel},
        },
    };

    #[test]
    fn stop_eof_and_malformed_controls_fail_closed() {
        for input in [b"".as_slice(), b"{\"op\":\"stop\"}\n"] {
            let stop = Arc::new(AtomicBool::new(false));
            let control = Control::new(stop.clone());
            read_controls(input, control.clone());
            assert!(stop.load(Ordering::Acquire));
            assert!(control.snapshot().error.is_none());
        }
        for input in [
            b"{\"op\":\"arm\"}\n".as_slice(),
            b"{\"op\":\"stop\",\"secret\":true}\n",
            b"{\"op\":\"stop\"}",
        ] {
            let stop = Arc::new(AtomicBool::new(false));
            let control = Control::new(stop.clone());
            read_controls(input, control.clone());
            assert!(stop.load(Ordering::Acquire));
            assert!(control.snapshot().error.is_some());
        }
        let mut overlong = Cursor::new(vec![b'x'; MAX_LINE * 4]);
        assert!(read_command(&mut overlong).is_err());
        assert_eq!(overlong.position(), (MAX_LINE + 1) as u64);
    }

    #[test]
    fn status_requires_authenticated_connection_and_explicit_cleanup() {
        let control = Control::new(Arc::new(AtomicBool::new(false)));
        assert_eq!(control.snapshot().state, "starting");
        control.set_listening("127.0.0.1:24800".into());
        control.set_enabled(true);
        assert_eq!(control.snapshot().state, "listening");
        control.set_connected(true);
        assert_eq!(control.snapshot().state, "paused");
        control.set_enabled(true);
        assert_eq!(control.snapshot().state, "sharing");
        control.set_connected(false);
        control.set_connected(true);
        assert_eq!(control.snapshot().state, "paused");
        control.request_stop();
        assert_eq!(control.snapshot().state, "stopping");
        control.complete(Ok(()));
        control.set_enabled(true);
        assert_eq!(control.snapshot().state, "stopped");
        control.fail("private credential path must never be serialized");
        assert_eq!(control.snapshot().state, "error");
        assert_eq!(control.snapshot().error.as_deref(), Some("receiver failed"));
    }

    #[test]
    fn permission_rejection_is_terminal_only_after_completion() {
        for rejection in [
            StartupRejection::Accessibility,
            StartupRejection::InputMonitoring,
        ] {
            let control = Control::new(Arc::new(AtomicBool::new(false)));
            control.reject_startup(rejection);
            assert_eq!(control.snapshot().state, "stopping");
            control.complete(Err(anyhow::anyhow!("permission preflight rejected")));
            let status = control.snapshot();
            assert_eq!(status.state, "startup_rejected");
            assert_eq!(status.error.as_deref(), Some(rejection.reason()));
            assert!(status.listen_endpoint.is_none());
        }
    }

    #[test]
    fn runtime_and_control_failures_cannot_be_reclassified_as_startup_rejection() {
        let live = Control::new(Arc::new(AtomicBool::new(false)));
        live.set_listening("127.0.0.1:24800".into());
        live.set_connected(true);
        live.set_enabled(true);
        live.reject_startup(StartupRejection::Accessibility);
        live.complete(Err(anyhow::anyhow!("input cleanup failed")));
        assert_eq!(live.snapshot().state, "error");

        let broken = Control::new(Arc::new(AtomicBool::new(false)));
        broken.reject_startup(StartupRejection::Accessibility);
        broken.fail("control output unavailable");
        broken.complete(Err(anyhow::anyhow!("output failed")));
        assert_eq!(broken.snapshot().state, "error");

        let malformed = Control::new(Arc::new(AtomicBool::new(false)));
        malformed.fail("invalid control message");
        malformed.reject_startup(StartupRejection::InputMonitoring);
        malformed.complete(Err(anyhow::anyhow!("permission preflight rejected")));
        assert_eq!(malformed.snapshot().state, "error");
    }

    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        wake: Condvar,
    }
    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.wake.wait(open).unwrap();
            }
        }
        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }
    struct Input {
        gate: Arc<Gate>,
        done: Sender<()>,
    }
    impl Read for Input {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            self.gate.wait();
            Ok(0)
        }
    }
    impl Drop for Input {
        fn drop(&mut self) {
            let _ = self.done.send(());
        }
    }
    struct Output {
        gate: Arc<Gate>,
        entered: Option<Sender<()>>,
        done: Sender<()>,
        bytes: Arc<Mutex<Vec<u8>>>,
        fail: bool,
    }
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            self.gate.wait();
            if self.fail {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            self.bytes.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Drop for Output {
        fn drop(&mut self) {
            let _ = self.done.send(());
        }
    }
    struct Fixture {
        control: Control,
        io: Option<ControlIo>,
        input: Arc<Gate>,
        output: Arc<Gate>,
        entered: Receiver<()>,
        done: Receiver<()>,
        bytes: Arc<Mutex<Vec<u8>>>,
    }
    impl Fixture {
        fn new(fail: bool) -> Self {
            let control = Control::new(Arc::new(AtomicBool::new(false)));
            let input = Arc::new(Gate::default());
            let output = Arc::new(Gate::default());
            let (done_tx, done) = channel();
            let (entered_tx, entered) = channel();
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let io = ControlIo::start(
                Input {
                    gate: input.clone(),
                    done: done_tx.clone(),
                },
                Output {
                    gate: output.clone(),
                    entered: Some(entered_tx),
                    done: done_tx,
                    bytes: bytes.clone(),
                    fail,
                },
                control.clone(),
            )
            .unwrap();
            Self {
                control,
                io: Some(io),
                input,
                output,
                entered,
                done,
                bytes,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.input.release();
            self.output.release();
            self.io.take();
            // Observe both thread-owned pipe fixtures dropped; no leaked waiter.
            let _ = self.done.recv_timeout(Duration::from_secs(2));
            let _ = self.done.recv_timeout(Duration::from_secs(2));
        }
    }

    #[test]
    fn blocked_status_output_stops_receiver_without_blocking_supervisor() {
        let mut fixture = Fixture::new(false);
        fixture.io.as_mut().unwrap().tick();
        fixture
            .entered
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let io = fixture.io.as_mut().unwrap();
        // Simulate elapsed lease time after the writer is confirmed blocked.
        io.clock = Instant::now() - OUTPUT_LEASE - Duration::from_millis(1);
        let began = Instant::now();
        io.tick();
        assert!(began.elapsed() < Duration::from_millis(200));
        assert_eq!(fixture.control.snapshot().state, "stopping");
        assert_eq!(
            fixture.control.snapshot().error.as_deref(),
            Some("native client stopped reading status")
        );
    }

    #[test]
    fn startup_rejection_is_delivered_as_one_final_error_frame() {
        let mut fixture = Fixture::new(false);
        fixture.output.release();
        fixture
            .control
            .reject_startup(StartupRejection::Accessibility);
        assert!(
            fixture
                .io
                .as_mut()
                .unwrap()
                .finish(Err(anyhow::anyhow!("preflight denied")))
                .is_err()
        );
        let bytes = fixture.bytes.lock().unwrap().clone();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["state"], "startup_rejected");
        assert_eq!(value["error"], "accessibility permission required");
        assert!(value["listen_endpoint"].is_null());
    }

    #[test]
    fn final_status_distinguishes_success_cleanup_failure_and_broken_output() {
        for failure in [false, true] {
            let mut fixture = Fixture::new(false);
            fixture.output.release();
            let result = if failure {
                Err(anyhow::anyhow!("private failure detail"))
            } else {
                Ok(())
            };
            assert_eq!(
                fixture.io.as_mut().unwrap().finish(result).is_err(),
                failure
            );
            let bytes = fixture.bytes.lock().unwrap().clone();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["state"], if failure { "error" } else { "stopped" });
            assert!(
                !String::from_utf8(bytes)
                    .unwrap()
                    .contains("private failure")
            );
        }
        let mut fixture = Fixture::new(true);
        fixture.output.release();
        assert!(fixture.io.as_mut().unwrap().finish(Ok(())).is_err());
        assert_eq!(fixture.control.snapshot().state, "error");
        assert_eq!(
            fixture.control.snapshot().error.as_deref(),
            Some("control output closed")
        );
    }
}
