//! Real OS shutdown signals in isolated child processes. The receiver is always
//! a recording fake or CLI DrySink; no mouse/keyboard capture or injection runs.
//! Run: cargo test --test shutdown_process -- --ignored --test-threads=1

use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    input::Button,
    metrics::{LogWriter, summarize},
    protocol::{Frame, Kind, button_code},
    security::{self, Identity, SecureChannel},
    session::{self, MouseSink},
    shutdown::Shutdown,
};
use serde_json::{Value, json};
#[cfg(windows)]
use std::ffi::OsStr;
use std::{
    ffi::OsString,
    fs,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const CHILD_MODE: &str = "CROSSFLOW_SHUTDOWN_TEST_MODE";
const CHILD_ROOT: &str = "CROSSFLOW_SHUTDOWN_TEST_ROOT";
const CHILD_ADDRESS: &str = "CROSSFLOW_SHUTDOWN_TEST_ADDRESS";
const CHILD_SIGNAL: &str = "CROSSFLOW_SHUTDOWN_TEST_SIGNAL";
#[cfg(windows)]
const CHILD_TARGET: &str = "CROSSFLOW_SHUTDOWN_TEST_TARGET";

struct Fixture {
    root: PathBuf,
    address: String,
    passed: bool,
}
impl Fixture {
    fn new(label: &str) -> Result<Self> {
        let root = std::env::current_dir()?.join("runs").join(format!(
            "shutdown-process-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&root)?;
        security::create_pair(&root.join("identities"))?;
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        let address = reservation.local_addr()?.to_string();
        drop(reservation);
        Ok(Self {
            root,
            address,
            passed: false,
        })
    }
    fn environment(&self, mode: &str) -> Vec<(OsString, OsString)> {
        vec![
            (CHILD_MODE.into(), mode.into()),
            (CHILD_ROOT.into(), self.root.as_os_str().into()),
            (CHILD_ADDRESS.into(), self.address.as_str().into()),
        ]
    }
    fn connect(&self) -> Result<Wire> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let socket = loop {
            match TcpStream::connect(&self.address) {
                Ok(socket) => break socket,
                Err(error)
                    if error.kind() == ErrorKind::ConnectionRefused
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error).context("connect isolated shutdown receiver"),
            }
        };
        Wire::new(
            socket,
            &Identity::load(&self.root.join("identities/windows"))?,
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this test's newly-created directory; keep failures for diagnosis.
        if self.passed {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

struct Wire {
    socket: TcpStream,
    secure: SecureChannel,
    lease: u64,
    seq: u64,
}
impl Wire {
    fn new(mut socket: TcpStream, identity: &Identity) -> Result<Self> {
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut secure = security::handshake(&mut socket, identity, true)?;
        let mut header = [0; 2];
        socket.read_exact(&mut header)?;
        let length = u16::from_be_bytes(header) as usize;
        ensure!((16..=2048).contains(&length), "bad lease length");
        let mut bytes = vec![0; length];
        socket.read_exact(&mut bytes)?;
        let lease = Frame::decode(&secure.decrypt(&bytes)?)?;
        ensure!(lease.kind == Kind::Lease, "expected receiver lease");
        Ok(Self {
            socket,
            secure,
            lease: lease.lease,
            seq: 0,
        })
    }
    fn send(&mut self, mut frame: Frame) -> Result<()> {
        self.seq += 1;
        frame.seq = self.seq;
        frame.lease = self.lease;
        let cipher = self.secure.encrypt(&frame.encode()?)?;
        let mut bytes = Vec::with_capacity(cipher.len() + 2);
        bytes.extend_from_slice(&(cipher.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&cipher);
        self.socket.write_all(&bytes)?;
        Ok(())
    }
}

/// Always kills and joins its own child if an assertion, signal delivery or
/// timeout fails. Never targets another process or the parent console.
struct ChildProcess {
    #[cfg(windows)]
    process: windows_child::Process,
    #[cfg(unix)]
    process: std::process::Child,
}
impl ChildProcess {
    fn spawn(exe: &Path, args: &[OsString], environment: &[(OsString, OsString)]) -> Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                process: windows_child::Process::spawn(exe, args, environment)?,
            })
        }
        #[cfg(unix)]
        {
            use std::process::{Command, Stdio};
            Ok(Self {
                process: Command::new(exe)
                    .args(args)
                    .envs(environment.iter().cloned())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()?,
            })
        }
    }
    fn id(&self) -> u32 {
        #[cfg(windows)]
        {
            self.process.id
        }
        #[cfg(unix)]
        {
            self.process.id()
        }
    }
    fn wait(&mut self, timeout: Duration) -> Result<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            #[cfg(windows)]
            if let Some(code) = self.process.try_wait()? {
                return Ok(code as i32);
            }
            #[cfg(unix)]
            if let Some(status) = self.process.try_wait()? {
                return status
                    .code()
                    .context("child terminated by an unhandled signal");
            }
            ensure!(
                Instant::now() < deadline,
                "isolated child exceeded shutdown deadline"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for ChildProcess {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if matches!(self.process.try_wait(), Ok(None)) {
                let _ = self.process.kill();
            }
            let _ = self.process.wait();
        }
        // Windows Process owns the exact native child handle and cleans it up.
    }
}

#[cfg(windows)]
mod windows_child {
    use super::*;
    use std::{collections::BTreeMap, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{
            CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetExitCodeProcess,
            PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW, TerminateProcess,
            WaitForSingleObject,
        },
    };

    pub struct Process {
        handle: HANDLE,
        pub id: u32,
    }
    // Quote directly as UTF-16 using Windows argv backslash/quote rules.
    fn quoted(value: &OsStr) -> Vec<u16> {
        let mut output = vec![b'"' as u16];
        let mut slashes = 0;
        for unit in value.encode_wide() {
            if unit == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            let count = if unit == b'"' as u16 {
                2 * slashes + 1
            } else {
                slashes
            };
            output.extend(std::iter::repeat_n(b'\\' as u16, count));
            slashes = 0;
            output.push(unit);
        }
        output.extend(std::iter::repeat_n(b'\\' as u16, 2 * slashes));
        output.push(b'"' as u16);
        output
    }
    impl Process {
        pub fn spawn(
            exe: &Path,
            args: &[OsString],
            additions: &[(OsString, OsString)],
        ) -> Result<Self> {
            let application: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
            let mut command = quoted(exe.as_os_str());
            for arg in args {
                command.push(b' ' as u16);
                command.extend(quoted(arg));
            }
            command.push(0);
            let mut environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
            environment.extend(additions.iter().cloned());
            let mut block = Vec::<u16>::new();
            for (key, value) in environment {
                block.extend(key.encode_wide());
                block.push(b'=' as u16);
                block.extend(value.encode_wide());
                block.push(0);
            }
            block.push(0);
            let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
            startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            startup.dwFlags = STARTF_USESHOWWINDOW;
            startup.wShowWindow = 0; // SW_HIDE: the dedicated console is never shown.
            let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
            let created = unsafe {
                CreateProcessW(
                    application.as_ptr(),
                    command.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
                    block.as_ptr().cast(),
                    std::ptr::null(),
                    &startup,
                    &mut info,
                )
            };
            ensure!(
                created != 0,
                "CreateProcessW failed: {}",
                std::io::Error::last_os_error()
            );
            unsafe {
                CloseHandle(info.hThread);
            }
            Ok(Self {
                handle: info.hProcess,
                id: info.dwProcessId,
            })
        }
        pub fn try_wait(&self) -> Result<Option<u32>> {
            let waited = unsafe { WaitForSingleObject(self.handle, 0) };
            if waited == WAIT_TIMEOUT {
                return Ok(None);
            }
            ensure!(waited == WAIT_OBJECT_0, "WaitForSingleObject failed");
            let mut code = 0;
            ensure!(
                unsafe { GetExitCodeProcess(self.handle, &mut code) } != 0,
                "GetExitCodeProcess failed"
            );
            Ok(Some(code))
        }
    }
    impl Drop for Process {
        fn drop(&mut self) {
            unsafe {
                if WaitForSingleObject(self.handle, 0) == WAIT_TIMEOUT {
                    TerminateProcess(self.handle, 99);
                    WaitForSingleObject(self.handle, 5000);
                }
                CloseHandle(self.handle);
            }
        }
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
        fn AttachConsole(process: u32) -> i32;
        fn GetConsoleProcessList(processes: *mut u32, count: u32) -> u32;
        fn GenerateConsoleCtrlEvent(event: u32, group: u32) -> i32;
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    unsafe extern "system" fn ignore_helper_event(_: u32) -> i32 {
        1
    }

    pub fn signal(target: Option<u32>, ctrl_c: bool) -> Result<()> {
        if let Some(pid) = target {
            // This helper starts in its own private console and attaches only
            // to the fresh target PID passed by its parent test, never the host.
            unsafe {
                FreeConsole();
            }
            ensure!(
                unsafe { AttachConsole(pid) } != 0,
                "AttachConsole(test child) failed"
            );
            ensure!(
                unsafe { SetConsoleCtrlHandler(Some(ignore_helper_event), 1) } != 0,
                "helper handler failed"
            );
        }
        let mut processes = [0u32; 16];
        let count = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) }
            as usize;
        ensure!(
            count > 0 && count <= processes.len(),
            "private console inventory failed"
        );
        let own = std::process::id();
        ensure!(
            processes[..count]
                .iter()
                .all(|pid| *pid == own || Some(*pid) == target),
            "refuse to signal a console containing any unrelated process"
        );
        ensure!(
            target.is_none_or(|pid| processes[..count].contains(&pid)),
            "target is not in private console"
        );
        ensure!(
            unsafe { GenerateConsoleCtrlEvent(if ctrl_c { 0 } else { 1 }, 0) } != 0,
            "GenerateConsoleCtrlEvent failed"
        );
        // Delivery is asynchronous; do not detach before the console queues it.
        thread::sleep(Duration::from_millis(20));
        Ok(())
    }
}

#[cfg(unix)]
fn unix_signal(pid: u32, name: &str) -> Result<()> {
    let signal = match name {
        "interrupt" => libc::SIGINT,
        "terminate" => libc::SIGTERM,
        "hangup" => libc::SIGHUP,
        _ => bail!("unknown test signal"),
    };
    ensure!(
        unsafe { libc::kill(libc::pid_t::try_from(pid)?, signal) } == 0,
        "kill(test child) failed"
    );
    Ok(())
}

fn signal_self(name: &str) -> Result<()> {
    #[cfg(windows)]
    {
        windows_child::signal(None, name == "interrupt")
    }
    #[cfg(unix)]
    {
        unix_signal(std::process::id(), name)
    }
}

struct FakeSink {
    events: Vec<&'static str>,
    key_held: bool,
    button_held: bool,
    signal_sent: bool,
    signal: String,
}
impl FakeSink {
    fn maybe_signal(&mut self) -> Result<()> {
        if self.key_held && self.button_held && !self.signal_sent {
            self.signal_sent = true;
            signal_self(&self.signal)?;
        }
        Ok(())
    }
}
impl MouseSink for FakeSink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        Ok((x, y))
    }
    fn key(&mut self, _usage: u16, down: bool) -> Result<()> {
        self.events.push(if down { "key_down" } else { "key_up" });
        self.key_held = down;
        self.maybe_signal()
    }
    fn button(&mut self, _button: Button, down: bool) -> Result<()> {
        self.events
            .push(if down { "button_down" } else { "button_up" });
        self.button_held = down;
        self.maybe_signal()
    }
}

fn child_fixture(root: &Path, address: &str, signal: &str) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    let guard = Shutdown::install(stop.clone())?;
    ensure!(
        Shutdown::install(Arc::new(AtomicBool::new(false))).is_err(),
        "second simultaneous shutdown installation was accepted"
    );
    let writer = LogWriter::new(
        &root.join("receiver.jsonl"),
        json!({"test":"shutdown_process","actual_os_input":false}),
    )?;
    let mut sink = FakeSink {
        events: vec![],
        key_held: false,
        button_held: false,
        signal_sent: false,
        signal: signal.into(),
    };
    let received = session::receive_with_stop(
        address,
        &root.join("identities/mac"),
        &mut sink,
        writer.log.clone(),
        20.0,
        &stop,
    );
    let finalized = writer.finish();
    received?;
    finalized?;
    // Completion notification is deliberately after receiver release + log flush.
    drop(guard);
    let fresh_stop = Arc::new(AtomicBool::new(false));
    let reinstalled = Shutdown::install(fresh_stop.clone())?;
    thread::sleep(Duration::from_millis(10));
    ensure!(
        !fresh_stop.load(Ordering::Acquire),
        "new guard inherited old signal request"
    );
    drop(reinstalled);
    fs::write(
        root.join("result.json"),
        serde_json::to_vec(&json!({
            "signal_sent":sink.signal_sent,"stop_observed":stop.load(Ordering::Acquire),
            "key_held":sink.key_held,"button_held":sink.button_held,"events":sink.events,
            "receive_ok":true,"log_finalized":true,"actual_os_input":false,
            "duplicate_install_rejected":true,"reinstall_fresh":true
        }))?,
    )?;
    Ok(())
}

#[test]
#[ignore = "child fixture for explicit isolated-process tests"]
fn shutdown_child_fixture() -> Result<()> {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return Ok(());
    };
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).context("missing child root")?);
    let result = if mode == "fake" {
        child_fixture(
            &root,
            &std::env::var(CHILD_ADDRESS)?,
            &std::env::var(CHILD_SIGNAL)?,
        )
    } else if mode == "signal-helper" {
        #[cfg(windows)]
        {
            windows_child::signal(Some(std::env::var(CHILD_TARGET)?.parse()?), false)
        }
        #[cfg(unix)]
        {
            bail!("console helper is Windows-only")
        }
    } else {
        bail!("unknown shutdown fixture mode")
    };
    if let Err(error) = &result {
        let _ = fs::write(root.join("child-error.txt"), format!("{error:#}"));
    }
    result
}

fn test_child_args() -> Vec<OsString> {
    [
        "--exact",
        "shutdown_child_fixture",
        "--ignored",
        "--nocapture",
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

#[test]
#[ignore = "opens localhost sockets and hidden isolated child processes; no OS input"]
fn os_signal_releases_held_fake_inputs_and_finalizes_log() -> Result<()> {
    #[cfg(windows)]
    let signals = ["interrupt", "break"];
    #[cfg(unix)]
    let signals = ["interrupt", "terminate", "hangup"];
    for signal in signals {
        let mut fixture = Fixture::new(signal)?;
        let mut environment = fixture.environment("fake");
        environment.push((CHILD_SIGNAL.into(), signal.into()));
        let mut child =
            ChildProcess::spawn(&std::env::current_exe()?, &test_child_args(), &environment)?;
        let mut wire = fixture.connect()?;
        wire.send(Frame::new(Kind::Arm))?;
        let mut button = Frame::new(Kind::Button);
        button.code = button_code(Button::Left);
        button.down = true;
        wire.send(button)?;
        let mut key = Frame::new(Kind::Key);
        key.code = 0x04;
        key.down = true;
        let started = Instant::now();
        wire.send(key)?;
        let status = child.wait(Duration::from_secs(6))?;
        ensure!(
            status == 0,
            "signal child failed: {}",
            fs::read_to_string(fixture.root.join("child-error.txt")).unwrap_or_default()
        );
        ensure!(
            started.elapsed() < Duration::from_secs(5),
            "signal did not stop active receiver promptly"
        );
        let result: Value = serde_json::from_slice(&fs::read(fixture.root.join("result.json"))?)?;
        ensure!(
            result["signal_sent"] == true && result["stop_observed"] == true,
            "actual OS signal was not handled: {result}"
        );
        ensure!(
            result["receive_ok"] == true && result["log_finalized"] == true,
            "shutdown failed: {result}"
        );
        ensure!(
            result["key_held"] == false && result["button_held"] == false,
            "owned input remained held: {result}"
        );
        ensure!(
            result["events"] == json!(["button_down", "key_down", "key_up", "button_up"]),
            "cleanup did not release both owned inputs exactly once: {result}"
        );
        let summary = summarize(&fixture.root.join("receiver.jsonl"))?;
        ensure!(
            summary["complete_log"] == true,
            "receiver log incomplete: {summary}"
        );
        drop(wire);
        ensure!(
            TcpStream::connect(&fixture.address).is_err(),
            "receiver port remained open"
        );
        fixture.passed = true;
    }
    Ok(())
}

#[test]
#[ignore = "starts real CLI DrySink and hidden isolated signal helper; no OS input"]
fn cli_dry_receiver_signal_finishes_log_and_closes_listener() -> Result<()> {
    let mut fixture = Fixture::new("cli")?;
    let log = fixture.root.join("cli.jsonl");
    let args: Vec<OsString> = vec![
        "receive".into(),
        "--bind".into(),
        fixture.address.clone().into(),
        "--identity".into(),
        fixture.root.join("identities/mac").into_os_string(),
        "--log".into(),
        log.clone().into_os_string(),
        "--seconds".into(),
        "20".into(),
    ];
    let mut child = ChildProcess::spawn(Path::new(env!("CARGO_BIN_EXE_crossflow")), &args, &[])?;
    let mut wire = fixture.connect()?;
    wire.send(Frame::new(Kind::Arm))?;
    #[cfg(windows)]
    {
        let mut environment = fixture.environment("signal-helper");
        environment.push((CHILD_TARGET.into(), child.id().to_string().into()));
        let mut helper =
            ChildProcess::spawn(&std::env::current_exe()?, &test_child_args(), &environment)?;
        let status = helper.wait(Duration::from_secs(5))?;
        ensure!(
            status == 0,
            "isolated signal helper failed: {}",
            fs::read_to_string(fixture.root.join("child-error.txt")).unwrap_or_default()
        );
    }
    #[cfg(unix)]
    unix_signal(child.id(), "interrupt")?;
    ensure!(
        child.wait(Duration::from_secs(6))? == 0,
        "CLI did not gracefully exit"
    );
    let summary = summarize(&log)?;
    ensure!(
        summary["complete_log"] == true,
        "CLI log incomplete: {summary}"
    );
    drop(wire);
    ensure!(
        TcpStream::connect(&fixture.address).is_err(),
        "CLI listener remained open"
    );
    fixture.passed = true;
    Ok(())
}
