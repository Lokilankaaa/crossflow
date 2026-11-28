//! Experimental isolation for input actions. Use `run_worker` only in a dedicated
//! child process: pipe threads may remain blocked until that process exits.
//! No timer thread renews input authority. ACK means adapter-return, not OS receipt.
use crate::{
    input::Button,
    protocol::{button_code, button_from_code},
    session::{MouseSink, OwnedSink, WorkerTiming},
    worker_protocol::{FRAME_LEN, Message, Op, WorkerLease},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::File,
    io::{Read, Write},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
#[path = "injection_worker_tests.rs"]
mod tests;

const RPC_TIMEOUT: Duration = Duration::from_millis(250);
const HEARTBEAT: Duration = Duration::from_millis(50);

fn timing_for_ack(
    request: &Message,
    response: &Message,
    received_ns: u64,
) -> Result<Option<WorkerTiming>> {
    ensure!(
        response.op == Op::Completed
            && response.seq == request.seq
            && response.epoch == request.epoch,
        "input worker returned an invalid acknowledgement"
    );
    ensure!(
        received_ns
            .checked_sub(request.sent_ns)
            .is_some_and(|age| age < crate::worker_protocol::MAX_REQUEST_AGE_NS)
            && response.sent_ns >= request.sent_ns
            && response.sent_ns <= received_ns,
        "input worker response expired or clock invalid"
    );
    if !matches!(
        request.op,
        Op::Move | Op::Position | Op::Button | Op::Key | Op::Scroll
    ) {
        ensure!(
            response.call_start_ns == 0 && response.call_end_ns == 0,
            "input worker control response carries action timing"
        );
        return Ok(None);
    }
    ensure!(
        request.sent_ns <= response.call_start_ns
            && response.call_start_ns <= response.call_end_ns
            && response.call_end_ns <= response.sent_ns,
        "input worker action timing invalid"
    );
    Ok(Some(WorkerTiming {
        request_seq: request.seq,
        dispatch_ns: response.call_start_ns - request.sent_ns,
        sink_call_ns: response.call_end_ns - response.call_start_ns,
        return_ns: received_ns - response.call_end_ns,
        rpc_ns: received_ns - request.sent_ns,
        clock: if cfg!(target_os = "macos") {
            "mach_continuous_time_ns"
        } else if cfg!(windows) {
            "QueryInterruptTimePrecise_100ns"
        } else {
            "CLOCK_BOOTTIME_ns"
        },
    }))
}

/// Same machine-wide clock in both processes, including time spent asleep.
/// Windows uses the precise interrupt clock, shared across processes and biased
/// across sleep. Its 100 ns units are a representation, not an accuracy promise.
pub fn continuous_ns() -> Result<u64> {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            fn mach_timebase_info(info: *mut Timebase) -> i32;
            fn mach_continuous_time() -> u64;
        }
        static RATIO: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
        let &(numer, denom) = RATIO
            .get_or_init(|| {
                let mut info = Timebase { numer: 0, denom: 0 };
                // SAFETY: info points to one writable timebase structure.
                let result = unsafe { mach_timebase_info(&mut info) };
                (result == 0 && info.denom != 0).then_some((info.numer, info.denom))
            })
            .as_ref()
            .context("continuous clock unavailable")?;
        // SAFETY: read-only system clock, no arguments or ownership transfer.
        let ticks = unsafe { mach_continuous_time() };
        u64::try_from(u128::from(ticks) * u128::from(numer) / u128::from(denom))
            .context("continuous clock overflow")
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn QueryInterruptTimePrecise(value: *mut u64);
        }
        let mut ticks = 0u64;
        // SAFETY: read-only clock writes one valid u64. Windows 10+ is required.
        // https://learn.microsoft.com/windows/win32/api/realtimeapiset/nf-realtimeapiset-queryinterrupttimeprecise
        unsafe { QueryInterruptTimePrecise(&mut ticks) };
        ticks.checked_mul(100).context("clock overflow")
    }
    #[cfg(target_os = "linux")]
    {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: value is a writable timespec; CLOCK_BOOTTIME includes suspend.
        ensure!(
            unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } == 0,
            "continuous clock unavailable"
        );
        u64::try_from(value.tv_sec)?
            .checked_mul(1_000_000_000)
            .and_then(|v| v.checked_add(value.tv_nsec as u64))
            .context("clock overflow")
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    bail!("worker clock unsupported on this platform")
}

fn guarded<T>(call: impl FnOnce() -> Result<T>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(call))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("worker adapter panicked")))
}

/// Convert each native call's unwind separately so one failing Up cannot stop
/// attempts to release the other owned inputs. The process entry point installs
/// a redacting panic hook; the shared core does not replace the host's hook.
struct Guarded<'a>(&'a mut dyn MouseSink);
impl MouseSink for Guarded<'_> {
    fn displays(&self) -> Result<Vec<crate::session::NativeDisplay>> {
        guarded(|| self.0.displays())
    }
    fn reset(&mut self) -> Result<()> {
        guarded(|| self.0.reset())
    }
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        guarded(|| self.0.move_by(x, y))
    }
    fn position(&mut self, x: f64, y: f64, display_id: u32) -> Result<(f64, f64)> {
        guarded(|| self.0.position(x, y, display_id))
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        guarded(|| self.0.button(button, down))
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        guarded(|| self.0.key(usage, down))
    }
    fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
        guarded(|| self.0.scroll(x, y))
    }
    fn release_owned(&mut self) -> Result<()> {
        guarded(|| self.0.release_owned())
    }
    fn emergency_pressed(&self) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.0.emergency_pressed()))
            .unwrap_or(true)
    }
}

/// Duplicate standard handles without holding Rust's global stdout lock.
pub fn stdio_files() -> Result<(File, File)> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        Ok((
            File::from(std::io::stdin().as_fd().try_clone_to_owned()?),
            File::from(std::io::stdout().as_fd().try_clone_to_owned()?),
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        Ok((
            File::from(std::io::stdin().as_handle().try_clone_to_owned()?),
            File::from(std::io::stdout().as_handle().try_clone_to_owned()?),
        ))
    }
}

#[cfg(target_os = "macos")]
pub fn run_native(gain: f64) -> Result<()> {
    struct Native(crate::platform::macos::MacMouseSink);
    impl MouseSink for Native {
        fn reset(&mut self) -> Result<()> {
            self.0.reset().map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
            self.0
                .move_by(x, y)
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn button(&mut self, button: Button, down: bool) -> Result<()> {
            self.0
                .button(button, down)
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn position(&mut self, x: f64, y: f64, display_id: u32) -> Result<(f64, f64)> {
            self.0
                .position(x, y, display_id)
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn key(&mut self, usage: u16, down: bool) -> Result<()> {
            self.0
                .key(usage, down)
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
            self.0
                .scroll(x, y)
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn release_owned(&mut self) -> Result<()> {
            self.0
                .release_owned()
                .map_err(|e| anyhow::anyhow!(e.to_string()))
        }
        fn emergency_pressed(&self) -> bool {
            self.0.emergency_pressed()
        }
    }
    let (input, output) = stdio_files()?;
    let mut sink = Native(
        crate::platform::macos::MacMouseSink::new(gain)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?,
    );
    run_worker(input, output, &mut sink)
}

#[cfg(windows)]
pub fn run_native(gain: f64) -> Result<()> {
    let (input, output) = stdio_files()?;
    let mut sink =
        crate::platform::windows::WindowsMouseSink::new(gain, Arc::new(AtomicBool::new(false)))?;
    run_worker(input, output, &mut sink)
}

fn reader<R: Read + Send + 'static>(mut input: R, failed: Arc<AtomicBool>) -> Receiver<Message> {
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            let mut bytes = [0; FRAME_LEN];
            let message = input
                .read_exact(&mut bytes)
                .ok()
                .and_then(|()| Message::decode(&bytes).ok());
            let Some(message) = message else { break };
            // Overflow is terminal, never silent loss or unbounded buffering.
            if send.try_send(message).is_err() {
                break;
            }
        }
        failed.store(true, Ordering::Release);
    });
    receive
}

fn writer<W: Write + Send + 'static>(
    mut output: W,
    failed: Arc<AtomicBool>,
) -> (SyncSender<[u8; FRAME_LEN]>, Arc<AtomicU64>) {
    let (send, receive) = mpsc::sync_channel::<[u8; FRAME_LEN]>(1);
    let completed = Arc::new(AtomicU64::new(0));
    let written = completed.clone();
    thread::spawn(move || {
        while let Ok(bytes) = receive.recv() {
            if output
                .write_all(&bytes)
                .and_then(|()| output.flush())
                .is_err()
            {
                failed.store(true, Ordering::Release);
                break;
            }
            if let Ok(message) = Message::decode(&bytes) {
                written.store(message.seq, Ordering::Release);
            }
        }
    });
    (send, completed)
}

/// Must return into a process exit, not into a reusable in-process service.
/// EOF, malformed input, expired authority and output failure all discard queued
/// commands and attempt local cleanup. Nothing waits on blocked pipe threads.
pub fn run_worker<R: Read + Send + 'static, W: Write + Send + 'static>(
    input: R,
    output: W,
    sink: &mut dyn MouseSink,
) -> Result<()> {
    let io_failed = Arc::new(AtomicBool::new(false));
    let requests = reader(input, io_failed.clone());
    let (responses, written) = writer(output, io_failed.clone());
    let stop = AtomicBool::new(false);
    let mut guarded_sink = Guarded(sink);
    let mut owned = OwnedSink::new(&mut guarded_sink, &stop);
    let mut lease = WorkerLease::new(continuous_ns()?);
    let mut ready = Message::new(Op::Ready);
    ready.sent_ns = continuous_ns()?;
    responses
        .try_send(ready.encode()?)
        .context("worker ready channel failed")?;
    let mut shutdown_seq = None;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        loop {
            ensure!(
                !io_failed.load(Ordering::Acquire),
                "worker pipe closed or invalid"
            );
            ensure!(!owned.emergency_pressed(), "worker local emergency stop");
            lease.check(continuous_ns()?)?;
            let request = match requests.recv_timeout(Duration::from_millis(2)) {
                Ok(request) => request,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(_) => bail!("worker request pipe closed"),
            };
            // Reader-side EOF/fault wins over an already queued input command.
            ensure!(
                !io_failed.load(Ordering::Acquire),
                "worker pipe closed or invalid"
            );
            ensure!(!owned.emergency_pressed(), "worker local emergency stop");
            lease.accept(&request, continuous_ns()?)?;
            let response = dispatch_request(&mut owned, &request, continuous_ns)?;
            // Lease expiry during a native call is terminal, even if the call
            // already posted input. That uncertain result is never replayed.
            if request.op != Op::Shutdown {
                lease.check(response.sent_ns)?;
            }
            responses
                .try_send(response.encode()?)
                .context("worker response backlog")?;
            if request.op == Op::Shutdown {
                shutdown_seq = Some(request.seq);
                return Ok(());
            }
        }
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("worker adapter panicked")));
    // Cleanup precedes reporting and cannot depend on the parent reading ACKs.
    let cleanup = owned.release_owned();
    cleanup.context("worker input release unconfirmed")?;
    if let Some(seq) = shutdown_seq {
        let until = Instant::now() + RPC_TIMEOUT;
        while written.load(Ordering::Acquire) != seq {
            ensure!(
                Instant::now() < until && !io_failed.load(Ordering::Acquire),
                "worker shutdown response not written; cleanup already attempted"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    outcome
}

/// Keep acknowledgement timestamps immediately around adapter dispatch. The
/// clock parameter lets timing tests control logical time without weakening
/// the worker's production request-age, lease or pipe-shutdown deadlines.
fn dispatch_request(
    owned: &mut dyn MouseSink,
    request: &Message,
    mut now: impl FnMut() -> Result<u64>,
) -> Result<Message> {
    let mut response = Message::new(Op::Completed);
    response.seq = request.seq;
    response.epoch = request.epoch;
    let is_action = matches!(
        request.op,
        Op::Move | Op::Position | Op::Button | Op::Key | Op::Scroll
    );
    if is_action {
        response.call_start_ns = now()?;
    }
    match request.op {
        Op::Reset => owned.reset()?,
        Op::Move => {
            (response.x, response.y) = owned.move_by(request.x, request.y)?;
        }
        Op::Position => {
            (response.x, response.y) = owned.position(request.x, request.y, request.display_id)?;
        }
        Op::Button => owned.button(button_from_code(request.code)?, request.down)?,
        Op::Key => owned.key(request.code, request.down)?,
        Op::Scroll => owned.scroll(request.a, request.b)?,
        Op::Heartbeat => {}
        Op::Release | Op::Shutdown => owned.release_owned()?,
        _ => bail!("worker received a response as a request"),
    }
    response.sent_ns = now()?;
    if is_action {
        response.call_end_ns = response.sent_ns;
    }
    Ok(response)
}

/// One in-flight RPC; terminal errors cannot create a replacement worker or
/// replay an action. Construct once per receiver process, outside the input loop.
pub struct WorkerProxy {
    child: Option<Child>,
    service_backed: bool,
    requests: Option<SyncSender<[u8; FRAME_LEN]>>,
    responses: Receiver<Message>,
    io_failed: Arc<AtomicBool>,
    terminal: bool,
    seq: u64,
    epoch: u64,
    last_heartbeat: Instant,
    latest_timing: Option<WorkerTiming>,
    failure_log: Option<crate::metrics::Log>,
    displays: Vec<crate::session::NativeDisplay>,
}
impl WorkerProxy {
    fn from_streams<R, W>(
        input: R,
        output: W,
        child: Option<Child>,
        service_backed: bool,
        ready_timeout: Duration,
    ) -> Result<Self>
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let io_failed = Arc::new(AtomicBool::new(false));
        let responses = reader(input, io_failed.clone());
        let (requests, _) = writer(output, io_failed.clone());
        let mut proxy = Self {
            child,
            service_backed,
            requests: Some(requests),
            responses,
            io_failed,
            terminal: false,
            seq: 0,
            epoch: 0,
            last_heartbeat: Instant::now(),
            latest_timing: None,
            failure_log: None,
            displays: Vec::new(),
        };
        let ready = proxy.responses.recv_timeout(ready_timeout);
        if !matches!(
            ready,
            Ok(Message {
                op: Op::Ready,
                seq: 0,
                epoch: 0,
                ..
            })
        ) {
            proxy.fail();
            bail!("input worker did not become ready");
        }
        Ok(proxy)
    }

    pub fn spawn(command: &mut Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Terminal Ctrl+C must reach the parent, which requests cleanup,
            // rather than also terminating the input owner before it releases.
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW, no visible helper console.
        }
        let mut child = command.spawn().context("start input worker")?;
        let stdout = child.stdout.take().context("worker stdout missing")?;
        let stdin = child.stdin.take().context("worker stdin missing")?;
        Self::from_streams(stdout, stdin, Some(child), false, Duration::from_secs(3))
    }

    #[cfg(windows)]
    pub fn try_connect_windows_service() -> Result<Self> {
        let (reader, writer) = crate::platform::windows::service::connect_service_pipe()?;
        Self::from_streams(reader, writer, None, true, Duration::from_millis(800))
    }

    pub fn is_service_backed(&self) -> bool {
        self.service_backed
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    /// Optional bounded log records the first terminal RPC failure separately
    /// from successful timing distributions. Never logs action payloads.
    pub fn attach_failure_log(&mut self, log: crate::metrics::Log) {
        self.failure_log = Some(log);
    }
    /// Inventory is sampled by the native receiver before starting a session.
    /// Position calls still revalidate display state inside the isolated owner.
    pub fn set_displays(&mut self, displays: Vec<crate::session::NativeDisplay>) -> Result<()> {
        crate::session::validate_displays(&displays)?;
        self.displays = displays;
        Ok(())
    }

    /// Explicit normal shutdown, including ACK and actual child exit. Drop is
    /// only a bounded fallback and must not be used as success evidence.
    pub fn shutdown(&mut self) -> Result<()> {
        self.rpc(Message::new(Op::Shutdown))?;
        self.fail();
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let until = Instant::now() + Duration::from_millis(500);
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "input worker exited with cleanup failure");
                return Ok(());
            }
            ensure!(Instant::now() < until, "input worker exit unconfirmed");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn fail(&mut self) {
        self.terminal = true;
        // Does not cancel a currently blocked write. The worker's independent
        // deadline is still authoritative; Drop bounds the child exit wait.
        self.requests.take();
    }
    fn rpc(&mut self, mut message: Message) -> Result<Message> {
        // A failed call, control, or heartbeat can never expose an older action's
        // timing as if it belonged to a newly processed input event.
        self.latest_timing = None;
        let mut failure_code = "preflight";
        let result = (|| {
            ensure!(
                !self.terminal,
                "input worker is terminal; restart receiver manually"
            );
            ensure!(
                !self.io_failed.load(Ordering::Acquire),
                "input worker pipe failed"
            );
            if let Some(child) = self.child.as_mut() {
                ensure!(child.try_wait()?.is_none(), "input worker exited");
            }
            self.seq = self
                .seq
                .checked_add(1)
                .context("worker sequence exhausted")?;
            message.seq = self.seq;
            message.epoch = self.epoch;
            failure_code = "clock";
            message.sent_ns = continuous_ns()?;
            let deadline = Instant::now() + RPC_TIMEOUT;
            failure_code = "request_invalid";
            let encoded = message.encode()?;
            failure_code = "request_queue_failed";
            self.requests
                .as_ref()
                .context("worker pipe closed")?
                .try_send(encoded)
                .context("worker request backlog")?;
            let response = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|error| {
                    failure_code = if matches!(error, mpsc::RecvTimeoutError::Timeout) {
                        "timeout"
                    } else {
                        "response_read_failed"
                    };
                    anyhow::anyhow!("input worker response uncertain; action will not be replayed")
                })?;
            failure_code = "clock";
            let now_ns = continuous_ns()?;
            let response_age = now_ns
                .checked_sub(message.sent_ns)
                .context("input worker continuous clock regressed")?;
            failure_code = "timeout";
            ensure!(
                Instant::now() < deadline
                    && response_age < crate::worker_protocol::MAX_REQUEST_AGE_NS,
                "input worker response arrived too late"
            );
            failure_code = "invalid_ack";
            self.latest_timing = timing_for_ack(&message, &response, now_ns)?;
            Ok(response)
        })();
        if result.is_err() {
            if !self.terminal {
                if let Some(log) = &self.failure_log {
                    log.write(
                        "worker_rpc_failure",
                        serde_json::json!({
                            "reason":failure_code,"request_seq":message.seq,
                            "kind":format!("{:?}",message.op),"result":"uncertain_no_replay"
                        }),
                    );
                }
            }
            self.fail();
        }
        result
    }
}
impl MouseSink for WorkerProxy {
    fn displays(&self) -> Result<Vec<crate::session::NativeDisplay>> {
        crate::session::validate_displays(&self.displays)?;
        Ok(self.displays.clone())
    }
    fn take_timing(&mut self) -> Option<WorkerTiming> {
        self.latest_timing.take()
    }
    fn maintain(&mut self) -> Result<()> {
        ensure!(!self.terminal, "input worker is terminal");
        if self.last_heartbeat.elapsed() >= HEARTBEAT {
            self.rpc(Message::new(Op::Heartbeat))?;
            self.last_heartbeat = Instant::now();
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        self.epoch = self
            .epoch
            .checked_add(1)
            .context("worker epoch exhausted")?;
        self.rpc(Message::new(Op::Reset))?;
        Ok(())
    }
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        let mut message = Message::new(Op::Move);
        message.x = x;
        message.y = y;
        let response = self.rpc(message)?;
        Ok((response.x, response.y))
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        let mut message = Message::new(Op::Button);
        message.code = button_code(button);
        message.down = down;
        self.rpc(message)?;
        Ok(())
    }
    fn position(&mut self, x: f64, y: f64, display_id: u32) -> Result<(f64, f64)> {
        let mut message = Message::new(Op::Position);
        message.display_id = display_id;
        message.x = x;
        message.y = y;
        let response = self.rpc(message)?;
        Ok((response.x, response.y))
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        let mut message = Message::new(Op::Key);
        message.code = usage;
        message.down = down;
        self.rpc(message)?;
        Ok(())
    }
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        let mut message = Message::new(Op::Scroll);
        message.a = horizontal;
        message.b = vertical;
        self.rpc(message)?;
        Ok(())
    }
    fn release_owned(&mut self) -> Result<()> {
        self.rpc(Message::new(Op::Release))?;
        Ok(())
    }
    fn emergency_pressed(&self) -> bool {
        self.terminal || self.io_failed.load(Ordering::Acquire)
    }
}
impl Drop for WorkerProxy {
    fn drop(&mut self) {
        if !self.terminal && self.rpc(Message::new(Op::Shutdown)).is_err() {
            eprintln!("Input worker shutdown acknowledgement unavailable; cleanup unconfirmed.");
        }
        self.requests.take();
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let until = Instant::now() + Duration::from_millis(500);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < until => thread::sleep(Duration::from_millis(2)),
                _ => break,
            }
        }
        // Do not pretend a forced worker kill released anything. No restart.
        eprintln!("Input worker did not exit; terminating it with input release unconfirmed.");
        let _ = child.kill();
        // Child::wait could block if termination failed; leave reaping to the OS
        // when this one-shot experimental receiver process itself exits.
        let _ = child.try_wait();
    }
}
