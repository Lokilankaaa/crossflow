//! Private stdio supervision for a dedicated native-client child process.
//! Input events never use this channel. Blocking pipe threads are detached;
//! process exit, not Drop, owns their final lifetime.

use crate::{
    pair_session::{Command, SessionStatus},
    receiver_control,
    session_metadata::SharedLayout,
};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read, Write},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_LINE: usize = 4095;
const COMMAND_CAPACITY: usize = 16;
const COMMANDS_PER_SECOND: usize = 64;
const OUTPUT_LEASE_MS: u64 = 2000;
const STATUS_INTERVAL: Duration = Duration::from_millis(100);
const FINAL_ACK: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Sender,
    Receiver,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum WireCommand {
    Arm { epoch: u64 },
    Pause {},
    Stop {},
}

#[derive(Clone, Copy)]
#[repr(u8)]
enum Failure {
    Invalid = 1,
    Read = 2,
    Limit = 3,
    Flood = 4,
    Queue = 5,
    Output = 6,
    Stalled = 7,
    Session = 8,
    Final = 9,
    Startup = 10,
    Accessibility = 11,
    InputMonitoring = 12,
}
impl Failure {
    fn message(self) -> &'static str {
        match self {
            Self::Invalid => "invalid control message",
            Self::Read => "control input read failed",
            Self::Limit => "control message exceeds limit or is incomplete",
            Self::Flood => "control message rate exceeded",
            Self::Queue => "control message queue exceeded",
            Self::Output => "control output unavailable",
            Self::Stalled => "native client stopped reading status",
            Self::Session => "pair session failed",
            Self::Final => "final client status was not delivered",
            Self::Startup => "native client supervision unavailable",
            Self::Accessibility => "accessibility permission required",
            Self::InputMonitoring => "input monitoring permission required",
        }
    }
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Invalid,
            2 => Self::Read,
            3 => Self::Limit,
            4 => Self::Flood,
            5 => Self::Queue,
            6 => Self::Output,
            7 => Self::Stalled,
            8 => Self::Session,
            9 => Self::Final,
            10 => Self::Startup,
            11 => Self::Accessibility,
            12 => Self::InputMonitoring,
            _ => Self::Session,
        }
    }
    fn public(message: &str) -> Self {
        match message {
            "accessibility permission required" => Self::Accessibility,
            "input monitoring permission required" => Self::InputMonitoring,
            _ => Self::Session,
        }
    }
}
#[derive(Clone)]
struct Shared {
    stop: Arc<AtomicBool>,
    failure: Arc<AtomicU8>,
    // Zero means no published, capture-ready session. Nonzero values include
    // the connection namespace, so commands are tagged at read AND poll time.
    command_epoch: Arc<AtomicU64>,
}

struct QueuedCommand {
    command: Command,
    ui_epoch: u64,
}
impl Shared {
    fn fail(&self, reason: Failure) {
        let _ = self
            .failure
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
        self.stop.store(true, Ordering::Release);
    }
    fn error(&self) -> Option<&'static str> {
        let value = self.failure.load(Ordering::Acquire);
        (value != 0).then(|| Failure::from_code(value).message())
    }
}

#[derive(Serialize)]
struct SenderStatus {
    protocol: u8,
    state: &'static str,
    epoch: u64,
    capture_ready: bool,
    error: Option<String>,
    route_enabled: bool,
    focus_device_id: Option<String>,
    focus_screen_id: Option<String>,
}
#[derive(Serialize)]
#[serde(untagged)]
enum Status {
    Sender(SenderStatus),
    Receiver(receiver_control::Status),
}
struct View {
    state: &'static str,
    epoch: u64,
    capture_ready: bool,
    route_enabled: bool,
    input_active: bool,
    focus_device_id: Option<String>,
    focus_screen_id: Option<String>,
    layout: Option<SharedLayout>,
}
impl Default for View {
    fn default() -> Self {
        Self {
            state: "starting",
            epoch: 0,
            capture_ready: false,
            route_enabled: false,
            input_active: false,
            focus_device_id: None,
            focus_screen_id: None,
            layout: None,
        }
    }
}
impl View {
    fn clear_authority(&mut self) {
        self.capture_ready = false;
        self.route_enabled = false;
        self.input_active = false;
        self.focus_device_id = None;
        self.focus_screen_id = None;
        self.layout = None;
    }
    fn serialize(
        &self,
        role: Role,
        local_id: &str,
        endpoint: Option<&str>,
        error: Option<&str>,
        stopping: bool,
        finished: bool,
    ) -> Status {
        let state = if finished {
            if error.is_some() { "error" } else { "stopped" }
        } else if stopping {
            "stopping"
        } else {
            self.state
        };
        let live = !stopping && !finished && error.is_none();
        let focus_device = live.then(|| self.focus_device_id.clone()).flatten();
        let focus_screen = live.then(|| self.focus_screen_id.clone()).flatten();
        match role {
            Role::Sender => Status::Sender(SenderStatus {
                protocol: 2,
                state,
                epoch: self.epoch,
                capture_ready: self.capture_ready,
                error: error.map(str::to_owned),
                route_enabled: live && self.route_enabled,
                focus_device_id: focus_device
                    .map(|id| if id == local_id { "local".into() } else { id }),
                focus_screen_id: focus_screen,
            }),
            Role::Receiver => Status::Receiver(receiver_control::Status {
                protocol: 2,
                state,
                listen_endpoint: endpoint.map(str::to_owned),
                error: error.map(str::to_owned),
                route_enabled: live && self.route_enabled,
                input_active: live && self.input_active,
                focus_device_id: focus_device,
                focus_screen_id: focus_screen,
                shared_layout: if live { self.layout.clone() } else { None },
            }),
        }
    }
}

fn read_command(
    reader: &mut impl BufRead,
    role: Role,
) -> std::result::Result<Option<Command>, Failure> {
    let mut bytes = Vec::with_capacity(MAX_LINE + 1);
    let count = reader
        .take((MAX_LINE + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| Failure::Read)?;
    if count == 0 {
        return Ok(None);
    }
    if count > MAX_LINE || bytes.last() != Some(&b'\n') {
        return Err(Failure::Limit);
    }
    let command: WireCommand = serde_json::from_slice(&bytes).map_err(|_| Failure::Invalid)?;
    match command {
        WireCommand::Stop {} => Ok(Some(Command::Stop)),
        WireCommand::Arm { epoch } if role == Role::Sender => Ok(Some(Command::Enable { epoch })),
        WireCommand::Pause {} if role == Role::Sender => Ok(Some(Command::Pause)),
        _ => Err(Failure::Invalid),
    }
}
fn rate_allowed(recent: &mut VecDeque<Instant>, now: Instant) -> bool {
    while recent
        .front()
        .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(1))
    {
        recent.pop_front();
    }
    if recent.len() >= COMMANDS_PER_SECOND {
        return false;
    }
    recent.push_back(now);
    true
}
fn read_controls(input: impl Read, role: Role, shared: Shared, send: SyncSender<QueuedCommand>) {
    let mut reader = BufReader::with_capacity(MAX_LINE + 1, input);
    let mut recent = VecDeque::with_capacity(COMMANDS_PER_SECOND);
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return;
        }
        let command = match read_command(&mut reader, role) {
            Ok(None) => {
                shared.stop.store(true, Ordering::Release);
                return;
            }
            Ok(Some(command)) => command,
            Err(reason) => {
                shared.fail(reason);
                return;
            }
        };
        if !rate_allowed(&mut recent, Instant::now()) {
            shared.fail(Failure::Flood);
            return;
        }
        if matches!(command, Command::Stop) {
            shared.stop.store(true, Ordering::Release);
            return;
        }
        let ui_epoch = shared.command_epoch.load(Ordering::Acquire);
        if ui_epoch == 0 || matches!(command,Command::Enable{epoch} if epoch!=ui_epoch) {
            continue;
        }
        match send.try_send(QueuedCommand { command, ui_epoch }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                shared.fail(Failure::Queue);
                return;
            }
            Err(TrySendError::Disconnected(_)) => {
                shared.stop.store(true, Ordering::Release);
                return;
            }
        }
    }
}

pub struct ControlIo {
    shared: Shared,
    role: Role,
    local_id: String,
    endpoint: Option<String>,
    view: View,
    commands: Receiver<QueuedCommand>,
    output: SyncSender<(u64, Status)>,
    written: Arc<AtomicU64>,
    progress_ms: Arc<AtomicU64>,
    clock: Instant,
    last_publish: Instant,
    sequence: u64,
    finished: bool,
    stop_delivered: bool,
    connection_serial: u64,
    has_session_namespace: bool,
    live_endpoint_epoch: Option<u64>,
}
impl ControlIo {
    pub fn start(
        input: impl Read + Send + 'static,
        mut output: impl Write + Send + 'static,
        stop: Arc<AtomicBool>,
        role: Role,
        local_id: String,
        listen_endpoint: Option<String>,
    ) -> Result<Self> {
        if let Some(endpoint) = &listen_endpoint {
            ensure!(
                endpoint.parse::<std::net::SocketAddr>().is_ok(),
                "invalid receiver endpoint"
            );
        }
        let shared = Shared {
            stop,
            failure: Arc::new(AtomicU8::new(0)),
            command_epoch: Arc::new(AtomicU64::new(0)),
        };
        let (command_tx, commands) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (output_tx, statuses) = mpsc::sync_channel::<(u64, Status)>(1);
        let written = Arc::new(AtomicU64::new(0));
        let progress_ms = Arc::new(AtomicU64::new(0));
        let clock = Instant::now();
        let reader_shared = shared.clone();
        if thread::Builder::new()
            .name("crossflow-pair-control".into())
            .spawn(move || {
                if catch_unwind(AssertUnwindSafe(|| {
                    read_controls(input, role, reader_shared.clone(), command_tx)
                }))
                .is_err()
                {
                    reader_shared.fail(Failure::Read);
                }
            })
            .is_err()
        {
            shared.fail(Failure::Startup);
            bail!("native control reader unavailable");
        }
        let writer_shared = shared.clone();
        let ack = written.clone();
        let progress = progress_ms.clone();
        if thread::Builder::new()
            .name("crossflow-pair-status".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    for (sequence, status) in statuses {
                        if serde_json::to_writer(&mut output, &status)
                            .map_err(std::io::Error::other)
                            .and_then(|()| output.write_all(b"\n"))
                            .and_then(|()| output.flush())
                            .is_err()
                        {
                            writer_shared.fail(Failure::Output);
                            return;
                        }
                        progress.store(clock.elapsed().as_millis() as u64, Ordering::Release);
                        ack.store(sequence, Ordering::Release);
                    }
                }));
                if result.is_err() {
                    writer_shared.fail(Failure::Output);
                }
            })
            .is_err()
        {
            shared.fail(Failure::Startup);
            bail!("native status writer unavailable");
        }
        let mut io = Self {
            shared,
            role,
            local_id,
            endpoint: listen_endpoint,
            view: View::default(),
            commands,
            output: output_tx,
            written,
            progress_ms,
            clock,
            last_publish: clock,
            sequence: 0,
            finished: false,
            stop_delivered: false,
            connection_serial: 0,
            has_session_namespace: false,
            live_endpoint_epoch: None,
        };
        io.enqueue();
        Ok(io)
    }
    fn health_at(&self, elapsed_ms: u64) -> Result<()> {
        if !self.finished
            && elapsed_ms.saturating_sub(self.progress_ms.load(Ordering::Acquire)) > OUTPUT_LEASE_MS
        {
            self.shared.fail(Failure::Stalled);
        }
        if let Some(error) = self.shared.error() {
            bail!("{error}");
        }
        Ok(())
    }
    fn health(&self) -> Result<()> {
        self.health_at(self.clock.elapsed().as_millis() as u64)
    }
    fn enqueue(&mut self) -> Option<u64> {
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.shared.fail(Failure::Output);
            return None;
        };
        self.sequence = sequence;
        self.last_publish = Instant::now();
        let status = self.view.serialize(
            self.role,
            &self.local_id,
            self.endpoint.as_deref(),
            self.shared.error(),
            self.shared.stop.load(Ordering::Acquire),
            self.finished,
        );
        match self.output.try_send((sequence, status)) {
            Ok(()) => Some(sequence),
            Err(TrySendError::Full(_)) => None,
            Err(TrySendError::Disconnected(_)) => {
                self.shared.fail(Failure::Output);
                None
            }
        }
    }
    pub fn publish_session(&mut self, status: &SessionStatus) -> Result<()> {
        self.health()?;
        ensure!(!self.finished, "native status is already final");
        if status.endpoint.epoch > u64::from(u32::MAX)
            || (!self.has_session_namespace && self.connection_serial >= u64::from(u32::MAX))
        {
            self.shared.command_epoch.store(0, Ordering::Release);
            self.live_endpoint_epoch = None;
            self.shared.fail(Failure::Session);
            bail!("native control epoch exhausted");
        }
        if !self.has_session_namespace {
            self.connection_serial += 1;
            self.has_session_namespace = true;
        }
        let next_ui_epoch = (self.connection_serial << 32) | status.endpoint.epoch;
        if !status.endpoint.connected
            || !status.capture_ready
            || next_ui_epoch != self.shared.command_epoch.load(Ordering::Acquire)
        {
            self.shared.command_epoch.store(0, Ordering::Release);
            self.live_endpoint_epoch = None;
        }
        self.view.epoch = next_ui_epoch;
        self.view.capture_ready = status.capture_ready;
        self.view.route_enabled = status.endpoint.connected && status.endpoint.enabled;
        self.view.input_active = status.endpoint.connected && status.endpoint.input_active;
        self.view.focus_device_id = if self.view.route_enabled {
            status.focus_device_id.clone()
        } else {
            None
        };
        self.view.focus_screen_id = if self.view.route_enabled {
            status.focus_screen_id.clone()
        } else {
            None
        };
        self.view.layout = status
            .endpoint
            .connected
            .then(|| status.shared_layout.clone());
        self.view.state = if !status.endpoint.connected {
            match self.role {
                Role::Sender => "connecting",
                Role::Receiver => {
                    if self.endpoint.is_some() {
                        "listening"
                    } else {
                        "starting"
                    }
                }
            }
        } else if self.role == Role::Sender && !status.capture_ready {
            "starting"
        } else if self.view.route_enabled {
            "sharing"
        } else {
            "paused"
        };
        self.enqueue();
        if status.endpoint.connected
            && status.capture_ready
            && self.shared.error().is_none()
            && !self.shared.stop.load(Ordering::Acquire)
        {
            self.live_endpoint_epoch = Some(status.endpoint.epoch);
            self.shared
                .command_epoch
                .store(self.view.epoch, Ordering::Release);
        }
        self.health()
    }
    /// Startup/final phase reporting only. Sharing and focus come exclusively
    /// from publish_session, never from caller-provided strings.
    pub fn publish_state(&mut self, state: &str, error: Option<&str>) -> Result<()> {
        let mapped = match state {
            "preparing" | "starting" => "starting",
            "connecting" | "listening" => match self.role {
                Role::Sender => "connecting",
                Role::Receiver => {
                    if self.endpoint.is_some() {
                        "listening"
                    } else {
                        "starting"
                    }
                }
            },
            "stopping" => "stopping",
            "stopped" => "stopped",
            "error" => "error",
            _ => bail!("unsupported native lifecycle state"),
        };
        if let Some(error) = error {
            self.shared.fail(Failure::public(error));
        }
        if state == "error" && self.shared.error().is_none() {
            self.shared.fail(Failure::Session);
        }
        self.view.state = mapped;
        self.view.clear_authority();
        self.shared.command_epoch.store(0, Ordering::Release);
        self.live_endpoint_epoch = None;
        self.has_session_namespace = false;
        // Commands already read before setup began cannot survive into a new
        // connection. A concurrent late enqueue is rejected again when polled.
        for _ in 0..COMMAND_CAPACITY {
            if self.commands.try_recv().is_err() {
                break;
            }
        }
        if matches!(mapped, "stopping" | "stopped" | "error") {
            self.shared.stop.store(true, Ordering::Release);
        }
        if matches!(mapped, "stopped" | "error") {
            self.finished = true;
        }
        self.enqueue();
        self.health()
    }
    pub fn poll_command(&mut self) -> Result<Option<Command>> {
        self.health()?;
        if !self.finished && self.last_publish.elapsed() >= STATUS_INTERVAL {
            self.enqueue();
            self.health()?;
        }
        if self.shared.stop.load(Ordering::Acquire) {
            if self.stop_delivered {
                return Ok(None);
            }
            self.stop_delivered = true;
            return Ok(Some(Command::Stop));
        }
        for _ in 0..COMMAND_CAPACITY {
            match self.commands.try_recv() {
                Ok(queued) => {
                    if matches!(queued.command, Command::Stop) {
                        self.shared.stop.store(true, Ordering::Release);
                        self.stop_delivered = true;
                        return Ok(Some(Command::Stop));
                    }
                    let Some(internal) = self.live_endpoint_epoch else {
                        continue;
                    };
                    if queued.ui_epoch != self.view.epoch {
                        continue;
                    }
                    match queued.command {
                        Command::Enable { epoch } if epoch == self.view.epoch => {
                            return Ok(Some(Command::Enable { epoch: internal }));
                        }
                        Command::Pause => return Ok(Some(Command::Pause)),
                        _ => continue,
                    }
                }
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    self.shared.stop.store(true, Ordering::Release);
                    self.stop_delivered = true;
                    return Ok(Some(Command::Stop));
                }
            }
        }
        Ok(None)
    }
    /// Call after all input ownership and native worker cleanup completes.
    /// Only this final path waits, bounded to500ms; pipe threads are never joined.
    pub fn finish(&mut self, result: Result<()>) -> Result<()> {
        if result.is_err() {
            self.shared.fail(Failure::Session);
        }
        self.shared.stop.store(true, Ordering::Release);
        self.finished = true;
        self.view.clear_authority();
        self.live_endpoint_epoch = None;
        self.shared.command_epoch.store(0, Ordering::Release);
        let deadline = Instant::now() + FINAL_ACK;
        let mut pending = None;
        while Instant::now() < deadline {
            if pending.is_none() {
                pending = self.enqueue();
            }
            if pending.is_some_and(|seq| self.written.load(Ordering::Acquire) >= seq) {
                if let Some(error) = self.shared.error() {
                    bail!("{error}");
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(5));
        }
        self.shared.fail(Failure::Final);
        bail!("final client status was not delivered")
    }
}
impl Drop for ControlIo {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Cursor,
        sync::{Condvar, Mutex},
    };
    fn parse(text: &str, role: Role) -> std::result::Result<Option<Command>, Failure> {
        read_command(&mut Cursor::new(text.as_bytes()), role)
    }
    #[test]
    fn exact_commands_reject_unknown_fields_duplicate_fields_and_receiver_enable() {
        assert!(matches!(
            parse("{\"op\":\"arm\",\"epoch\":7}\n", Role::Sender),
            Ok(Some(Command::Enable { epoch: 7 }))
        ));
        assert!(matches!(
            parse("{\"op\":\"pause\"}\n", Role::Sender),
            Ok(Some(Command::Pause))
        ));
        assert!(matches!(
            parse("{\"op\":\"stop\"}\n", Role::Receiver),
            Ok(Some(Command::Stop))
        ));
        for bad in [
            "{\"op\":\"arm\"}\n",
            "{\"op\":\"arm\",\"epoch\":-1}\n",
            "{\"op\":\"arm\",\"epoch\":1,\"epoch\":2}\n",
            "{\"op\":\"stop\",\"epoch\":1}\n",
            "{\"op\":\"pause\",\"extra\":true}\n",
            "{\"op\":\"arm\",\"epoch\":1}",
            "\n",
        ] {
            assert!(parse(bad, Role::Sender).is_err());
        }
        assert!(parse("{\"op\":\"arm\",\"epoch\":1}\n", Role::Receiver).is_err());
        assert!(parse("{\"op\":\"pause\"}\n", Role::Receiver).is_err());
        assert!(parse(&format!("{}\n", " ".repeat(4095)), Role::Sender).is_err());
    }
    #[test]
    fn schemas_match_native_protocol_two_and_only_sender_maps_local_identity() {
        let view = View {
            state: "sharing",
            epoch: 9,
            capture_ready: true,
            route_enabled: true,
            input_active: false,
            focus_device_id: Some("my-public-id".into()),
            focus_screen_id: Some("screen".into()),
            layout: None,
        };
        let sender = serde_json::to_value(view.serialize(
            Role::Sender,
            "my-public-id",
            None,
            None,
            false,
            false,
        ))
        .unwrap();
        let receiver = serde_json::to_value(view.serialize(
            Role::Receiver,
            "my-public-id",
            Some("127.0.0.1:24800"),
            None,
            false,
            false,
        ))
        .unwrap();
        let keys = |value: &serde_json::Value| {
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&sender),
            vec![
                "capture_ready",
                "epoch",
                "error",
                "focus_device_id",
                "focus_screen_id",
                "protocol",
                "route_enabled",
                "state"
            ]
        );
        assert_eq!(
            keys(&receiver),
            vec![
                "error",
                "focus_device_id",
                "focus_screen_id",
                "input_active",
                "listen_endpoint",
                "protocol",
                "route_enabled",
                "shared_layout",
                "state"
            ]
        );
        assert_eq!(sender["protocol"], 2);
        assert_eq!(sender["focus_device_id"], "local");
        assert_eq!(receiver["focus_device_id"], "my-public-id");
        assert_eq!(receiver["input_active"], false);
        let stopped = serde_json::to_value(view.serialize(
            Role::Receiver,
            "my-public-id",
            None,
            None,
            true,
            true,
        ))
        .unwrap();
        assert_eq!(stopped["state"], "stopped");
        assert_eq!(stopped["route_enabled"], false);
        assert!(stopped["focus_screen_id"].is_null());
    }
    #[test]
    fn rate_limit_is_sliding_and_never_allows_more_than64_in_one_second() {
        let start = Instant::now();
        let mut recent = VecDeque::new();
        for _ in 0..64 {
            assert!(rate_allowed(&mut recent, start));
        }
        assert!(!rate_allowed(
            &mut recent,
            start + Duration::from_millis(999)
        ));
        assert!(rate_allowed(&mut recent, start + Duration::from_secs(1)));
    }
    struct BlockingReader(mpsc::Receiver<()>);
    impl Read for BlockingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }
    struct BlockingWriter {
        gate: Arc<(Mutex<bool>, Condvar)>,
        entered: mpsc::Sender<()>,
    }
    impl Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let _ = self.entered.send(());
            let (lock, wake) = &*self.gate;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = wake.wait(open).unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn blocked_output_is_fatal_without_blocking_runtime_or_drop() {
        let (release, reader) = mpsc::channel();
        let (entered, entry) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut io = ControlIo::start(
            BlockingReader(reader),
            BlockingWriter {
                gate: gate.clone(),
                entered,
            },
            stop.clone(),
            Role::Sender,
            "local".into(),
            None,
        )
        .unwrap();
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        let begin = Instant::now();
        assert!(io.publish_state("connecting", None).is_ok());
        assert!(io.health_at(2001).is_err());
        assert!(stop.load(Ordering::Acquire));
        assert!(io.poll_command().is_err());
        drop(io);
        assert!(begin.elapsed() < Duration::from_millis(100));
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let _ = release.send(());
    }
    #[test]
    fn eof_requests_stop_and_invalid_messages_return_fixed_errors() {
        for text in ["", "{\"op\":\"stop\",\"secret\":\"never echo\"}\n"] {
            let shared = Shared {
                stop: Arc::new(AtomicBool::new(false)),
                failure: Arc::new(AtomicU8::new(0)),
                command_epoch: Arc::new(AtomicU64::new(1)),
            };
            let (tx, _) = mpsc::sync_channel(16);
            read_controls(Cursor::new(text), Role::Sender, shared.clone(), tx);
            assert!(shared.stop.load(Ordering::Acquire));
            if text.is_empty() {
                assert!(shared.error().is_none());
            } else {
                assert_eq!(shared.error(), Some("invalid control message"));
            }
        }
    }
    #[test]
    fn command_overflow_fails_closed_without_unbounded_buffering() {
        let shared = Shared {
            stop: Arc::new(AtomicBool::new(false)),
            failure: Arc::new(AtomicU8::new(0)),
            command_epoch: Arc::new(AtomicU64::new(1)),
        };
        let (tx, rx) = mpsc::sync_channel(16);
        read_controls(
            Cursor::new("{\"op\":\"pause\"}\n".repeat(17)),
            Role::Sender,
            shared.clone(),
            tx,
        );
        assert_eq!(shared.error(), Some("control message queue exceeded"));
        assert_eq!(rx.try_iter().count(), 16);
    }
    struct MemoryWriter(Arc<Mutex<Vec<u8>>>);
    fn session(epoch: u64, ready: bool) -> SessionStatus {
        SessionStatus {
            endpoint: crate::pair_endpoint::EndpointStatus {
                connected: true,
                enabled: false,
                owner: None,
                epoch,
                layout_revision: 1,
                input_active: false,
            },
            capture_ready: ready,
            input_source_device_id: None,
            focus_device_id: None,
            focus_screen_id: None,
            shared_layout: SharedLayout {
                schema: 1,
                source_device_id: "a".repeat(64),
                receiver_device_id: "b".repeat(64),
                screens: vec![],
            },
        }
    }
    #[test]
    fn reconnect_rejects_old_arm_even_when_both_internal_epochs_are_zero() {
        let (release, reader) = mpsc::channel();
        let bytes = Arc::new(Mutex::new(vec![]));
        let mut io = ControlIo::start(
            BlockingReader(reader),
            MemoryWriter(bytes),
            Arc::new(AtomicBool::new(false)),
            Role::Sender,
            "a".repeat(64),
            None,
        )
        .unwrap();
        let (commands, queue) = mpsc::sync_channel(COMMAND_CAPACITY);
        io.commands = queue;
        io.publish_session(&session(0, true)).unwrap();
        let old_ui = io.view.epoch;
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: old_ui },
                ui_epoch: old_ui,
            })
            .unwrap();
        io.publish_state("connecting", None).unwrap();
        // Race: a reader that observed the old session enqueues after it cleared.
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: old_ui },
                ui_epoch: old_ui,
            })
            .unwrap();
        io.publish_session(&session(0, true)).unwrap();
        let new_ui = io.view.epoch;
        assert_eq!(old_ui, 1u64 << 32);
        assert_eq!(new_ui, 2u64 << 32);
        // Even an old command read during the new session cannot pass by using
        // the same bare internal epoch0 or a newly stamped arrival namespace.
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: old_ui },
                ui_epoch: new_ui,
            })
            .unwrap();
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: 0 },
                ui_epoch: new_ui,
            })
            .unwrap();
        assert_eq!(io.poll_command().unwrap(), None);
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: new_ui },
                ui_epoch: new_ui,
            })
            .unwrap();
        assert_eq!(
            io.poll_command().unwrap(),
            Some(Command::Enable { epoch: 0 })
        );
        io.publish_session(&session(5, true)).unwrap();
        let advanced = io.view.epoch;
        assert_eq!(advanced, (2u64 << 32) | 5);
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: new_ui },
                ui_epoch: new_ui,
            })
            .unwrap();
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch: advanced },
                ui_epoch: advanced,
            })
            .unwrap();
        assert_eq!(
            io.poll_command().unwrap(),
            Some(Command::Enable { epoch: 5 })
        );
        drop(io);
        let _ = release.send(());
    }
    #[test]
    fn setup_discards_commands_but_stop_always_remains_effective() {
        let (release, reader) = mpsc::channel();
        let mut io = ControlIo::start(
            BlockingReader(reader),
            MemoryWriter(Arc::new(Mutex::new(vec![]))),
            Arc::new(AtomicBool::new(false)),
            Role::Sender,
            "a".repeat(64),
            None,
        )
        .unwrap();
        let (commands, queue) = mpsc::sync_channel(COMMAND_CAPACITY);
        io.commands = queue;
        io.publish_session(&session(0, false)).unwrap();
        let epoch = io.view.epoch;
        commands
            .send(QueuedCommand {
                command: Command::Enable { epoch },
                ui_epoch: epoch,
            })
            .unwrap();
        commands
            .send(QueuedCommand {
                command: Command::Pause,
                ui_epoch: epoch,
            })
            .unwrap();
        assert_eq!(io.poll_command().unwrap(), None);
        io.publish_session(&session(0, true)).unwrap();
        assert_eq!(io.poll_command().unwrap(), None);
        io.publish_state("starting", None).unwrap();
        commands
            .send(QueuedCommand {
                command: Command::Stop,
                ui_epoch: 0,
            })
            .unwrap();
        assert_eq!(io.poll_command().unwrap(), Some(Command::Stop));
        drop(io);
        let _ = release.send(());
    }
    #[test]
    fn namespace_and_internal_epoch_overflow_stop_instead_of_reusing_epochs() {
        for serial_overflow in [false, true] {
            let (release, reader) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let mut io = ControlIo::start(
                BlockingReader(reader),
                MemoryWriter(Arc::new(Mutex::new(vec![]))),
                stop.clone(),
                Role::Sender,
                "a".repeat(64),
                None,
            )
            .unwrap();
            if serial_overflow {
                io.connection_serial = u64::from(u32::MAX);
            }
            assert!(
                io.publish_session(&session(
                    if serial_overflow {
                        0
                    } else {
                        u64::from(u32::MAX) + 1
                    },
                    true
                ))
                .is_err()
            );
            assert!(stop.load(Ordering::Acquire));
            assert_eq!(io.shared.command_epoch.load(Ordering::Acquire), 0);
            assert!(io.live_endpoint_epoch.is_none());
            drop(io);
            let _ = release.send(());
        }
    }
    impl Write for MemoryWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn session_publishes_actual_layout_and_clears_ungranted_focus() {
        let (release, reader) = mpsc::channel();
        let bytes = Arc::new(Mutex::new(vec![]));
        let mut io = ControlIo::start(
            BlockingReader(reader),
            MemoryWriter(bytes),
            Arc::new(AtomicBool::new(false)),
            Role::Receiver,
            "a".repeat(64),
            Some("127.0.0.1:24800".into()),
        )
        .unwrap();
        let layout = SharedLayout {
            schema: 1,
            source_device_id: "a".repeat(64),
            receiver_device_id: "b".repeat(64),
            screens: vec![],
        };
        let mut status = SessionStatus {
            endpoint: crate::pair_endpoint::EndpointStatus {
                connected: true,
                enabled: true,
                owner: Some(crate::pair_control::Side::A),
                epoch: 8,
                layout_revision: 1,
                input_active: true,
            },
            capture_ready: true,
            input_source_device_id: Some("a".repeat(64)),
            focus_device_id: Some("b".repeat(64)),
            focus_screen_id: Some("screen-b".into()),
            shared_layout: layout.clone(),
        };
        io.publish_session(&status).unwrap();
        let value = serde_json::to_value(io.view.serialize(
            io.role,
            &io.local_id,
            io.endpoint.as_deref(),
            None,
            false,
            false,
        ))
        .unwrap();
        assert_eq!(
            value["shared_layout"],
            serde_json::to_value(layout).unwrap()
        );
        assert_eq!(value["input_active"], true);
        assert_eq!(value["state"], "sharing");
        status.endpoint.enabled = false;
        status.endpoint.input_active = false;
        io.publish_session(&status).unwrap();
        assert!(io.view.focus_device_id.is_none() && io.view.focus_screen_id.is_none());
        assert!(!io.view.route_enabled);
        assert_eq!(io.view.state, "paused");
        drop(io);
        let _ = release.send(());
    }
    #[test]
    fn final_ack_preserves_clean_vs_failed_completion_and_redacts_raw_errors() {
        for failed in [false, true] {
            let (release, reader) = mpsc::channel();
            let bytes = Arc::new(Mutex::new(vec![]));
            let mut io = ControlIo::start(
                BlockingReader(reader),
                MemoryWriter(bytes.clone()),
                Arc::new(AtomicBool::new(false)),
                Role::Receiver,
                "public-id".into(),
                Some("127.0.0.1:24800".into()),
            )
            .unwrap();
            let result = if failed {
                Err(anyhow::anyhow!("private-token-do-not-echo"))
            } else {
                Ok(())
            };
            assert_eq!(io.finish(result).is_err(), failed);
            let text = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert!(!text.contains("private-token"));
            let final_status: serde_json::Value =
                serde_json::from_str(text.lines().last().unwrap()).unwrap();
            assert_eq!(
                final_status["state"],
                if failed { "error" } else { "stopped" }
            );
            assert_eq!(final_status["route_enabled"], false);
            assert!(final_status["shared_layout"].is_null());
            if failed {
                assert_eq!(final_status["error"], "pair session failed");
            } else {
                assert!(final_status["error"].is_null());
            }
            drop(io);
            let _ = release.send(());
        }
    }
    #[test]
    fn blocked_final_status_times_out_instead_of_claiming_delivery() {
        let (release, reader) = mpsc::channel();
        let (entered, entry) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut io = ControlIo::start(
            BlockingReader(reader),
            BlockingWriter {
                gate: gate.clone(),
                entered,
            },
            Arc::new(AtomicBool::new(false)),
            Role::Sender,
            "public-id".into(),
            None,
        )
        .unwrap();
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        let began = Instant::now();
        assert!(io.finish(Ok(())).is_err());
        assert!(began.elapsed() < Duration::from_secs(2));
        assert_eq!(
            io.shared.error(),
            Some("final client status was not delivered")
        );
        drop(io);
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let _ = release.send(());
    }
}
