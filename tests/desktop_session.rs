//! Opt-in Windows process integration: REAL Raw Input/global hotkey registration,
//! but only a localhost DrySink receiver. No OS input is generated or injected.
//! Run explicitly: cargo test --test desktop_session -- --ignored --nocapture
//! Both Ctrl+Alt+Space and Ctrl+Alt+Esc must be available on the interactive desktop.
#![cfg(windows)]

use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    metrics::{LogWriter, summarize},
    security,
    session::{self, DrySink, MouseSink},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Evidence(PathBuf);
impl Evidence {
    fn new() -> Result<Self> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("runs");
        fs::create_dir_all(&root)?;
        let path = root.join(format!(
            "desktop-session-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&path)?;
        let evidence = Self(path);
        security::create_pair(&evidence.0.join("identities"))?;
        Ok(evidence)
    }
    fn identity(&self, role: &str) -> PathBuf {
        self.0.join("identities").join(role)
    }
    fn file(&self, name: &str) -> Result<File> {
        Ok(OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.0.join(name))?)
    }
    fn remove_identities(&self) -> Result<()> {
        // This fixed child is inside the run directory created exclusively by
        // new(); neither an external path nor the user's .pairing is accepted.
        let identities = self.0.join("identities");
        match fs::remove_dir_all(&identities) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove this test run's temporary identities"),
        }
    }
}
impl Drop for Evidence {
    fn drop(&mut self) {
        // Retain the test evidence, but remove only this run's temporary keys.
        // The directory is freshly created above; no user identity is touched.
        if let Err(error) = self.remove_identities() {
            let _ = writeln!(
                std::io::stderr(),
                "Temporary identity cleanup failed for {}: {error:#}",
                self.0.display()
            );
        }
    }
}

fn join_bounded(handle: JoinHandle<Result<()>>, timeout: Duration, label: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        // The caller has already released/closed its pipe or set stop. Rust
        // cannot forcibly cancel a stuck thread: report the cleanup failure
        // instead of hanging the test indefinitely on join().
        ensure!(
            Instant::now() < deadline,
            "{label} did not finish during bounded cleanup"
        );
        thread::sleep(Duration::from_millis(5));
    }
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{label} panicked"))?
}

#[derive(Default)]
struct ReceiverEvents {
    arms: AtomicUsize,
    releases: AtomicUsize,
}
struct ObservedDrySink {
    sink: DrySink,
    events: Arc<ReceiverEvents>,
}
impl MouseSink for ObservedDrySink {
    fn reset(&mut self) -> Result<()> {
        self.sink.reset()?;
        self.events.arms.fetch_add(1, Ordering::Release);
        Ok(())
    }
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        self.sink.move_by(x, y)
    }
    fn release_owned(&mut self) -> Result<()> {
        self.sink.release_owned()?;
        self.events.releases.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

struct ReceiverWorker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}
impl ReceiverWorker {
    fn start(evidence: &Evidence, address: String, events: Arc<ReceiverEvents>) -> Result<Self> {
        let writer = LogWriter::new(
            &evidence.0.join("receiver.jsonl"),
            json!({"test":"desktop_session","sink":"DrySink","os_injection":false}),
        )?;
        let identity = evidence.identity("mac");
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || {
            let mut sink = ObservedDrySink {
                sink: DrySink::default(),
                events,
            };
            let result = session::receive_with_stop(
                &address,
                &identity,
                &mut sink,
                writer.log.clone(),
                25.0,
                &thread_stop,
            );
            let finalized = writer.finish();
            result.and(finalized)
        });
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            // Allow the receiver's bounded handshake read to unwind as well.
            join_bounded(handle, Duration::from_secs(5), "receiver thread")?;
        }
        Ok(())
    }
}
impl Drop for ReceiverWorker {
    fn drop(&mut self) {
        if let Err(error) = self.finish() {
            let _ = writeln!(std::io::stderr(), "Receiver cleanup failed: {error:#}");
        }
    }
}

#[derive(Debug, Deserialize)]
struct Status {
    protocol: u8,
    state: String,
    epoch: u64,
    capture_ready: bool,
    error: Option<String>,
}

struct DesktopProcess {
    child: Child,
    input: Option<ChildStdin>,
    statuses: Receiver<Result<Status, String>>,
    reader: Option<JoinHandle<Result<()>>>,
}
impl DesktopProcess {
    fn start(evidence: &Evidence, address: &str, managed_peer: bool) -> Result<Self> {
        let stderr = evidence.file("child.stderr.txt")?;
        let mut transcript = evidence.file("control-status.jsonl")?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_crossflow"));
        command.arg("desktop-send");
        if managed_peer {
            let appdata = evidence.0.join("identities").join("appdata");
            let imported = Command::new(env!("CARGO_BIN_EXE_crossflow"))
                .arg("peer-import")
                .arg("--identity")
                .arg(evidence.identity("windows"))
                .args(["--name", "Local test Mac", "--endpoint", address])
                .env("LOCALAPPDATA", &appdata)
                .creation_flags(0x0800_0000)
                .output()?;
            ensure!(
                imported.status.success(),
                "Diagnostic identity migration failed"
            );
            let peer: crossflow::peers::PeerSummary =
                serde_json::from_slice(&imported.stdout).context("Invalid public peer summary")?;
            let listed = Command::new(env!("CARGO_BIN_EXE_crossflow"))
                .arg("peers")
                .env("LOCALAPPDATA", &appdata)
                .creation_flags(0x0800_0000)
                .output()?;
            ensure!(
                listed.status.success(),
                "Fresh process could not list saved peer"
            );
            let public: Value = serde_json::from_slice(&listed.stdout)?;
            ensure!(
                public["invalid_records"] == 0
                    && public["peers"]
                        .as_array()
                        .is_some_and(|rows| rows.len() == 1)
                    && public["peers"][0]["id"] == peer.id,
                "Saved peer did not survive process restart"
            );
            // Only this child sees the temporary app store. No real profile,
            // global environment, or user's existing paired devices are changed.
            command
                .env("LOCALAPPDATA", appdata)
                .arg("--peer")
                .arg(peer.id);
        } else {
            command
                .args(["--host", address])
                .arg("--identity")
                .arg(evidence.identity("windows"));
        }
        let child = command
            .arg("--log")
            .arg(evidence.0.join("sender.jsonl"))
            .args(["--seconds", "20", "--max-hz", "1000"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW; input window is message-only.
            .spawn()
            .context("start desktop-send child")?;
        let (tx, statuses) = mpsc::channel();
        let mut process = Self {
            child,
            input: None,
            statuses,
            reader: None,
        };
        process.input = process.child.stdin.take();
        ensure!(process.input.is_some(), "child stdin unavailable");
        let stdout = process
            .child
            .stdout
            .take()
            .context("child stdout unavailable")?;
        // Always drain stdout independently of assertions, so the test itself
        // cannot accidentally trigger the native client's output lease.
        process.reader = Some(thread::spawn(move || {
            let result = (|| -> Result<()> {
                for line in BufReader::new(stdout).lines() {
                    let line = line?;
                    ensure!(line.len() <= 4096, "oversized child status");
                    writeln!(transcript, "{line}")?;
                    let status: Status =
                        serde_json::from_str(&line).context("invalid child status")?;
                    if tx.send(Ok(status)).is_err() {
                        break;
                    }
                }
                transcript.flush()?;
                Ok(())
            })();
            if let Err(error) = &result {
                let _ = tx.send(Err(format!("{error:#}")));
            }
            result
        }));
        Ok(process)
    }
    fn command(&mut self, command: Value) -> Result<()> {
        let input = self.input.as_mut().context("child stdin already closed")?;
        serde_json::to_writer(&mut *input, &command)?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }
    fn wait_status(&self, label: &str, predicate: impl Fn(&Status) -> bool) -> Result<Status> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            ensure!(Instant::now() < deadline, "timed out waiting for {label}");
            let status = self
                .statuses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .with_context(|| format!("waiting for {label}; inspect child.stderr.txt"))?
                .map_err(anyhow::Error::msg)?;
            ensure!(status.protocol == 1, "unexpected control protocol");
            if predicate(&status) {
                return Ok(status);
            }
            ensure!(
                !matches!(status.state.as_str(), "error" | "stopped"),
                "child terminated while waiting for {label}: {:?}",
                status.error
            );
        }
    }
    fn close_input(&mut self) {
        drop(self.input.take());
    }
    fn wait_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "desktop child did not exit in time"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn finish(&mut self) -> Result<()> {
        self.close_input();
        let exit = self.wait_exit(Duration::from_secs(4))?;
        ensure!(
            exit.success(),
            "desktop child exited {exit}; inspect child.stderr.txt"
        );
        if let Some(reader) = self.reader.take() {
            join_bounded(reader, Duration::from_secs(2), "status reader")?;
        }
        Ok(())
    }
}
impl Drop for DesktopProcess {
    fn drop(&mut self) {
        self.close_input();
        if self.wait_exit(Duration::from_secs(1)).is_err() {
            let _ = self.child.kill();
            if let Err(error) = self.wait_exit(Duration::from_secs(2)) {
                let _ = writeln!(std::io::stderr(), "Desktop child cleanup failed: {error:#}");
            }
        }
        if let Some(reader) = self.reader.take() {
            if let Err(error) = join_bounded(reader, Duration::from_secs(2), "status reader") {
                let _ = writeln!(std::io::stderr(), "Status pipe cleanup failed: {error:#}");
            }
        }
    }
}

fn wait_receiver(label: &str, ready: impl Fn() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready() {
        if Instant::now() >= deadline {
            bail!("receiver did not observe {label}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

#[test]
#[ignore = "REAL Windows Raw Input and global hotkeys; localhost DrySink only; run explicitly on an interactive desktop"]
fn desktop_process_authenticates_controls_and_exits_cleanly_on_parent_eof() -> Result<()> {
    exercise_desktop_session(false)
}

#[test]
#[ignore = "REAL Windows Raw Input and global hotkeys; app-managed DPAPI peer and localhost DrySink; run explicitly"]
fn desktop_process_connects_by_saved_peer_without_identity_or_host_arguments() -> Result<()> {
    exercise_desktop_session(true)
}

fn exercise_desktop_session(managed_peer: bool) -> Result<()> {
    let evidence = Evidence::new()?;
    eprintln!(
        "Desktop session evidence: {} (temporary keys are removed)",
        evidence.0.display()
    );
    // The public receiver owns bind. Reserve then release an ephemeral port;
    // if another process wins the bind race, this test fails rather than probing
    // or interfering with it. The sender handles the receiver startup retry.
    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?.to_string();
    drop(reservation);
    let events = Arc::new(ReceiverEvents::default());
    let mut receiver = ReceiverWorker::start(&evidence, address.clone(), events.clone())?;
    let mut child = DesktopProcess::start(&evidence, &address, managed_peer)?;
    let paused = child.wait_status("authenticated and capture-ready pause", |s| {
        s.capture_ready && s.state == "paused" && s.error.is_none()
    })?;
    ensure!(
        paused.epoch > 0,
        "connection must advance the session epoch"
    );
    ensure!(
        events.arms.load(Ordering::Acquire) == 0,
        "must not auto-arm after authentication"
    );

    child.command(json!({"op":"arm", "epoch":paused.epoch - 1}))?;
    let rejected = child.wait_status("stale epoch rejection", |s| {
        s.state == "paused"
            && s.error
                .as_deref()
                .is_some_and(|e| e.starts_with("state_changed:"))
    })?;
    ensure!(
        rejected.epoch == paused.epoch,
        "rejected arm must not change session state"
    );
    ensure!(
        events.arms.load(Ordering::Acquire) == 0,
        "stale arm reached receiver"
    );

    child.command(json!({"op":"arm", "epoch":rejected.epoch}))?;
    let sharing = child.wait_status("explicit sharing", |s| {
        s.state == "sharing" && s.error.is_none()
    })?;
    ensure!(sharing.epoch > rejected.epoch, "arm must advance epoch");
    wait_receiver("first authenticated arm", || {
        events.arms.load(Ordering::Acquire) >= 1
    })?;
    let releases = events.releases.load(Ordering::Acquire);
    child.command(json!({"op":"pause"}))?;
    let paused_again = child.wait_status("explicit pause", |s| {
        s.state == "paused" && s.epoch > sharing.epoch
    })?;
    wait_receiver("pause release", || {
        events.releases.load(Ordering::Acquire) > releases
    })?;

    child.command(json!({"op":"arm", "epoch":paused_again.epoch}))?;
    child.wait_status("second explicit sharing", |s| {
        s.state == "sharing" && s.epoch > paused_again.epoch
    })?;
    wait_receiver("second authenticated arm", || {
        events.arms.load(Ordering::Acquire) >= 2
    })?;
    child.close_input();
    let terminal = child.wait_status("normal terminal status after stdin EOF", |s| {
        s.state == "stopped"
    })?;
    ensure!(
        terminal.error.is_none(),
        "EOF should be normal cancellation"
    );
    child.finish()?;
    receiver.finish()?;
    // Successful evidence must not leave private identities behind. Drop also
    // attempts this on every early-return/unwind path, without deleting logs.
    evidence.remove_identities()?;
    ensure!(
        !evidence.0.join("identities").try_exists()?,
        "temporary identities still exist"
    );

    let sender = summarize(&evidence.0.join("sender.jsonl"))?;
    let receiver = summarize(&evidence.0.join("receiver.jsonl"))?;
    for (name, summary) in [("sender", &sender), ("receiver", &receiver)] {
        ensure!(summary["complete_log"] == true, "{name} log incomplete");
        ensure!(
            summary["dropped_log_records"] == 0,
            "{name} log dropped records"
        );
    }
    serde_json::to_writer_pretty(
        evidence.file("result.json")?,
        &json!({
            "scenario":"authenticated_desktop_controls_parent_eof",
            "app_managed_peer":managed_peer,
            "manual_identity_or_host_arguments":!managed_peer,
            "result":"passed", "native_capture":true, "os_injection":false,
            "receiver":"DrySink", "synthetic_input":false,
            "receiver_arms":events.arms.load(Ordering::Acquire),
            "sender":sender, "receiver_metrics":receiver,
            "visible_smoothness":"not measured"
        }),
    )?;
    Ok(())
}
