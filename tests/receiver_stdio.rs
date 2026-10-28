//! Explicit real receiver-process checks. Only localhost and the CLI DrySink are
//! used: no physical capture, input injection, UI, Keychain or discovery starts.
//! Run: cargo test --test receiver_stdio -- --ignored --test-threads=1
use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    protocol::{Frame, Kind},
    security::{self, Identity, SecureChannel},
};
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const EXIT_TIMEOUT: Duration = Duration::from_secs(6);

struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Result<Self> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("runs");
        fs::create_dir_all(&root)?;
        let path = root.join(format!(
            "receiver-stdio-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&path)?;
        let fixture = Self(path);
        security::create_pair(&fixture.0.join("identities"))?;
        Ok(fixture)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // This exact child belongs to this freshly created unique fixture only.
        // Keep process logs as evidence, but never retain its generated secrets.
        let path = self.0.join("identities");
        if path.exists() && fs::remove_dir_all(path).is_err() {
            eprintln!("Temporary receiver test identity cleanup failed");
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    protocol: u8,
    state: String,
    listen_endpoint: Option<String>,
    error: Option<String>,
}

struct ReceiverProcess {
    child: Child,
    input: Option<ChildStdin>,
    output: Receiver<std::result::Result<Status, &'static str>>,
    reader: Option<JoinHandle<()>>,
}
impl ReceiverProcess {
    fn start(fixture: &Fixture) -> Result<Self> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crossflow"));
        command
            .arg("receive")
            .arg("--identity")
            .arg(fixture.0.join("identities/mac"))
            .args([
                "--bind",
                "127.0.0.1:0",
                "--seconds",
                "20",
                "--control-stdio",
            ])
            .arg("--log")
            .arg(fixture.0.join("receiver.jsonl"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(fixture.0.join("stderr.txt"))?);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = command.spawn().context("start dry receiver child")?;
        let input = child.stdin.take();
        let stdout = child.stdout.take().context("missing receiver output")?;
        let (tx, output) = mpsc::sync_channel(128);
        let mut result = Self {
            child,
            input,
            output,
            reader: None,
        };
        result.reader = Some(
            thread::Builder::new()
                .name("receiver-test-output".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    loop {
                        let mut bytes = Vec::with_capacity(2049);
                        let status = match reader.by_ref().take(2049).read_until(b'\n', &mut bytes)
                        {
                            Ok(0) => break,
                            Ok(n) if n <= 2048 && bytes.last() == Some(&b'\n') => {
                                serde_json::from_slice::<Status>(&bytes)
                                    .map_err(|_| "invalid receiver status")
                            }
                            _ => Err("receiver status exceeded limit or was incomplete"),
                        };
                        let failed = status.is_err();
                        if tx.try_send(status).is_err() || failed {
                            break;
                        }
                    }
                })?,
        );
        Ok(result)
    }

    fn next(&self, deadline: Instant) -> Result<Status> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let status = self
            .output
            .recv_timeout(remaining)
            .context("receiver did not publish expected status")?
            .map_err(anyhow::Error::msg)?;
        ensure!(status.protocol == 1, "unexpected receiver control protocol");
        ensure!(
            matches!(
                status.state.as_str(),
                "starting" | "listening" | "paused" | "sharing" | "stopping" | "stopped" | "error"
            ),
            "unknown receiver state"
        );
        Ok(status)
    }

    fn listening(&self) -> Result<SocketAddr> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = self.next(deadline)?;
            ensure!(status.error.is_none(), "receiver failed before listening");
            if status.state == "listening" {
                let endpoint: SocketAddr = status
                    .listen_endpoint
                    .context("missing actual bound endpoint")?
                    .parse()?;
                ensure!(
                    endpoint.ip().is_loopback() && endpoint.port() != 0,
                    "receiver reported invalid test endpoint"
                );
                return Ok(endpoint);
            }
            ensure!(
                status.state == "starting",
                "receiver stopped before listening"
            );
        }
    }

    fn command(&mut self, bytes: &[u8]) -> Result<()> {
        let input = self
            .input
            .as_mut()
            .context("receiver input already closed")?;
        input.write_all(bytes)?;
        input.flush()?;
        Ok(())
    }

    fn close_input(&mut self) {
        self.input.take();
    }

    fn finish(&mut self, success: bool, endpoint: SocketAddr) -> Result<()> {
        let deadline = Instant::now() + EXIT_TIMEOUT;
        let final_state = loop {
            let status = self.next(deadline)?;
            if matches!(status.state.as_str(), "stopped" | "error") {
                break status;
            }
        };
        ensure!(
            final_state.state == if success { "stopped" } else { "error" },
            "wrong terminal receiver state"
        );
        ensure!(
            final_state.error.is_none() == success,
            "terminal receiver error differs from outcome"
        );
        let exit = wait_exit(&mut self.child, deadline)?
            .context("receiver did not exit after terminal status")?;
        ensure!(
            exit.success() == success,
            "receiver exit differs from terminal status"
        );
        ensure!(
            TcpStream::connect_timeout(&endpoint, Duration::from_millis(100)).is_err(),
            "receiver listener remains open after exit"
        );
        if let Some(reader) = self.reader.take() {
            while !reader.is_finished() {
                ensure!(
                    Instant::now() < deadline,
                    "receiver output pipe did not close after exit"
                );
                thread::sleep(Duration::from_millis(5));
            }
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("receiver status reader panicked"))?;
        }
        Ok(())
    }
}

fn wait_exit(child: &mut Child, deadline: Instant) -> Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}
impl Drop for ReceiverProcess {
    fn drop(&mut self) {
        self.close_input();
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            if !matches!(
                wait_exit(&mut self.child, Instant::now() + Duration::from_secs(2)),
                Ok(Some(_))
            ) {
                eprintln!("Receiver test child cleanup could not confirm exit");
            }
        }
        // The process pipe reader is bounded and owns no input resources. Do not
        // turn a failed process cleanup into an unbounded test-thread join.
        if let Some(reader) = self.reader.take().filter(JoinHandle::is_finished) {
            let _ = reader.join();
        }
    }
}

/// Small authenticated sender used only with this test's DrySink child. The
/// socket remains owned by the test until the receiver confirms final exit.
struct Wire {
    socket: TcpStream,
    channel: SecureChannel,
    bytes: Vec<u8>,
    lease: u64,
    sequence: u64,
}
impl Wire {
    fn connect(endpoint: SocketAddr, fixture: &Fixture) -> Result<Self> {
        let mut socket = TcpStream::connect_timeout(&endpoint, Duration::from_secs(1))?;
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        let identity = Identity::load(&fixture.0.join("identities/windows"))?;
        let channel = security::handshake(&mut socket, &identity, true)?;
        socket.set_read_timeout(Some(Duration::from_millis(10)))?;
        let mut wire = Self {
            socket,
            channel,
            bytes: Vec::new(),
            lease: 0,
            sequence: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        while wire.lease == 0 {
            wire.read_leases()?;
            ensure!(
                Instant::now() < deadline,
                "receiver did not grant initial lease"
            );
        }
        Ok(wire)
    }

    fn read_leases(&mut self) -> Result<()> {
        let mut buffer = [0; 4096];
        match self.socket.read(&mut buffer) {
            Ok(0) => bail!("receiver disconnected before parent EOF"),
            Ok(count) => self.bytes.extend_from_slice(&buffer[..count]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error.into()),
        }
        while self.bytes.len() >= 2 {
            let count = u16::from_be_bytes(self.bytes[..2].try_into().unwrap()) as usize;
            ensure!(
                (16..=2048).contains(&count),
                "invalid encrypted test record size"
            );
            if self.bytes.len() < count + 2 {
                break;
            }
            let frame = Frame::decode(&self.channel.decrypt(&self.bytes[2..count + 2])?)?;
            ensure!(
                frame.kind == Kind::Lease,
                "receiver returned non-lease record"
            );
            self.lease = frame.lease;
            self.bytes.drain(..count + 2);
        }
        Ok(())
    }

    fn send(&mut self, mut frame: Frame) -> Result<u64> {
        self.sequence += 1;
        frame.seq = self.sequence;
        frame.lease = self.lease;
        let encrypted = self.channel.encrypt(&frame.encode()?)?;
        let mut record = Vec::with_capacity(encrypted.len() + 2);
        record.extend_from_slice(&(encrypted.len() as u16).to_be_bytes());
        record.extend_from_slice(&encrypted);
        self.socket.write_all(&record)?;
        Ok(self.sequence)
    }

    fn await_state(&mut self, receiver: &ReceiverProcess, expected: &str) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut heartbeat = Instant::now() - Duration::from_millis(30);
        loop {
            self.read_leases()?;
            if heartbeat.elapsed() >= Duration::from_millis(30) {
                self.send(Frame::new(Kind::Heartbeat))?;
                heartbeat = Instant::now();
            }
            match receiver.output.try_recv() {
                Ok(Ok(status)) => {
                    ensure!(
                        status.protocol == 1 && status.error.is_none(),
                        "invalid authenticated receiver status"
                    );
                    ensure!(
                        !matches!(status.state.as_str(), "stopping" | "stopped" | "error"),
                        "receiver stopped before parent EOF"
                    );
                    if status.state == expected {
                        return Ok(());
                    }
                }
                Ok(Err(error)) => bail!(error),
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!("receiver output closed before parent EOF")
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            ensure!(
                Instant::now() < deadline,
                "receiver did not publish authenticated state"
            );
        }
    }
}

#[test]
#[ignore = "real localhost receiver child and temporary identities; no OS input"]
fn parent_eof_stops_listening_receiver_and_closes_port() -> Result<()> {
    let fixture = Fixture::new("eof")?;
    let mut child = ReceiverProcess::start(&fixture)?;
    let endpoint = child.listening()?;
    child.close_input();
    child.finish(true, endpoint)
}

#[test]
#[ignore = "real localhost receiver child and temporary identities; no OS input"]
fn stop_command_reports_cleanup_then_exit() -> Result<()> {
    let fixture = Fixture::new("stop")?;
    let mut child = ReceiverProcess::start(&fixture)?;
    let endpoint = child.listening()?;
    child.command(b"{\"op\":\"stop\"}\n")?;
    child.finish(true, endpoint)
}

#[test]
#[ignore = "real localhost receiver child and temporary identities; no OS input"]
fn invalid_control_stops_with_error_and_nonzero_exit() -> Result<()> {
    let fixture = Fixture::new("invalid")?;
    let mut child = ReceiverProcess::start(&fixture)?;
    let endpoint = child.listening()?;
    child.command(b"{\"op\":\"arm\"}\n")?;
    child.finish(false, endpoint)
}

#[test]
#[ignore = "real localhost silent handshake and receiver child; no OS input"]
fn parent_eof_during_silent_handshake_stops_within_existing_deadline() -> Result<()> {
    let fixture = Fixture::new("handshake")?;
    let mut child = ReceiverProcess::start(&fixture)?;
    let endpoint = child.listening()?;
    let silent = TcpStream::connect_timeout(&endpoint, Duration::from_secs(1))?;
    // KK initiator sends first. Leave it silent so the receiver waits in its
    // bounded handshake. The finished log below must prove it entered that phase.
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    child.close_input();
    child.finish(true, endpoint)?;
    if started.elapsed() > EXIT_TIMEOUT {
        bail!("receiver exceeded cancellation bound");
    }
    let events: Vec<serde_json::Value> = fs::read_to_string(fixture.0.join("receiver.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        events
            .iter()
            .any(|event| event["event"] == "handshake_started"),
        "test did not enter handshake wait"
    );
    ensure!(
        !events
            .iter()
            .any(|event| event["event"] == "handshake_completed"),
        "silent test unexpectedly authenticated"
    );
    drop(silent);
    Ok(())
}

#[test]
#[ignore = "authenticated localhost receiver child holds a synthetic DrySink key; no OS input"]
fn parent_eof_releases_authenticated_held_dry_key_before_success() -> Result<()> {
    let fixture = Fixture::new("held-key")?;
    let mut child = ReceiverProcess::start(&fixture)?;
    let endpoint = child.listening()?;
    let mut wire = Wire::connect(endpoint, &fixture)?;
    wire.await_state(&child, "paused")?;
    wire.send(Frame::new(Kind::Arm))?;
    let mut key = Frame::new(Kind::Key);
    key.code = 0x04; // Synthetic fixture action, never posted to a platform API.
    key.down = true;
    let key_sequence = wire.send(key)?;
    wire.await_state(&child, "sharing")?;
    // Keep the authenticated sender alive while another periodic status proves
    // sharing is ongoing; never send KeyUp, Pause, Stop or close this socket.
    wire.await_state(&child, "sharing")?;
    child.close_input();
    child.finish(true, endpoint)?;
    let events: Vec<serde_json::Value> = fs::read_to_string(fixture.0.join("receiver.jsonl"))?
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    let applied = events
        .iter()
        .position(|event| {
            event["event"] == "input_applied"
                && event["kind"] == "Key"
                && event["seq"] == key_sequence
        })
        .context("held synthetic key did not reach DrySink")?;
    let released = events
        .iter()
        .position(|event| event["event"] == "input_released" && event["reason"] == "session_end")
        .context("receiver did not complete owned-input release")?;
    ensure!(released > applied, "input release did not follow held key");
    ensure!(
        events
            .iter()
            .any(|event| event["event"] == "emergency_stop"),
        "parent EOF did not take local stop path"
    );
    ensure!(
        !events.iter().any(|event| event["event"] == "error"),
        "session failed before parent EOF cleanup"
    );
    let final_log = events.last().context("missing receiver log")?;
    ensure!(
        final_log["event"] == "log_summary" && final_log["dropped_records"] == 0,
        "receiver log did not complete without drops"
    );
    drop(wire);
    Ok(())
}
