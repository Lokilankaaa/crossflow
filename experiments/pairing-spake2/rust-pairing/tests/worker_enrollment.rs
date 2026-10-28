//! Explicit Windows child-process pairing test. Real local LAN TCP/mDNS, actual
//! Noise/SPAKE2 and isolated DPAPI; no UI, input capture or OS input injection.
//! Set CROSSFLOW_TEST_LAN_IP to an owned private LAN interface. Build the
//! pairing_fixture example before explicitly running this ignored test.
#![cfg(windows)]

use anyhow::{Context, Result, bail, ensure};
use crossflow::{discovery, peers::PeerStore, security::Identity};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    os::windows::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

enum Event {
    Code(Zeroizing<String>),
    Public(Value),
}

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    events: Receiver<Result<Event, &'static str>>,
    reader: Option<JoinHandle<()>>,
}
impl Process {
    fn start(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x0800_0000)
            .spawn()
            .context("cannot start enrollment child")?;
        let input = child.stdin.take().context("child stdin unavailable")?;
        let stdout = child.stdout.take().context("child stdout unavailable")?;
        let (send, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut input = stdout;
            for _ in 0..32 {
                let mut line = Zeroizing::new(Vec::with_capacity(4097));
                let result = (|| -> Result<Option<Event>, &'static str> {
                    // A BufReader would retain another non-zeroizing copy of
                    // the code event. The small control stream uses bounded
                    // byte reads into explicitly zeroizing storage instead.
                    let mut byte = Zeroizing::new([0u8; 1]);
                    while line.len() <= 4096 {
                        match input.read(&mut byte[..]) {
                            Ok(0) => break,
                            Ok(_) => {
                                line.push(byte[0]);
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                            Err(_) => return Err("child read failed"),
                        }
                    }
                    let size = line.len();
                    if size == 0 {
                        return Ok(None);
                    }
                    if size > 4096 || line.last() != Some(&b'\n') {
                        return Err("invalid child record size");
                    }
                    let mut value: Value =
                        serde_json::from_slice(&line).map_err(|_| "invalid child JSON")?;
                    if value["event"] == "code" {
                        // Move, rather than clone, the parsed secret into a
                        // zeroizing owner. Never retain the original JSON line.
                        let code = value
                            .as_object_mut()
                            .and_then(|fields| fields.remove("code"));
                        let Some(Value::String(code)) = code else {
                            return Err("invalid fixture code event");
                        };
                        let code = Zeroizing::new(code);
                        if code.len() != 8 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
                            return Err("invalid fixture code");
                        }
                        Ok(Some(Event::Code(code)))
                    } else {
                        Ok(Some(Event::Public(value)))
                    }
                })();
                match result {
                    Ok(Some(event)) => {
                        if send.send(Ok(event)).is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(error) => {
                        let _ = send.send(Err(error));
                        return;
                    }
                }
            }
            let _ = send.send(Err("too many child records"));
        });
        Ok(Self {
            child,
            input: Some(input),
            events,
            reader: Some(reader),
        })
    }

    fn event(&self, timeout: Duration) -> Result<Event> {
        self.events
            .recv_timeout(timeout)
            .context("child event timeout or closed pipe")?
            .map_err(anyhow::Error::msg)
    }
    fn public(&self, wanted: &str, timeout: Duration) -> Result<Value> {
        let end = Instant::now() + timeout;
        loop {
            let Event::Public(value) = self.event(end.saturating_duration_since(Instant::now()))?
            else {
                bail!("unexpected private code event");
            };
            if value["event"] == wanted {
                return Ok(value);
            }
            ensure!(
                value["event"] != "error",
                "child reported an unexpected error"
            );
            ensure!(Instant::now() < end, "expected child event missing");
        }
    }
    fn command(&mut self, op: &str) -> Result<()> {
        let input = self.input.as_mut().context("child stdin already closed")?;
        serde_json::to_writer(&mut *input, &json!({"op":op}))?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    }
    fn begin(&mut self, endpoint: SocketAddr, code: &str) -> Result<()> {
        ensure!(
            code.len() == 8 && code.bytes().all(|byte| byte.is_ascii_digit()),
            "invalid test code"
        );
        let record = Zeroizing::new(format!(
            "{{\"op\":\"begin\",\"endpoint\":\"{endpoint}\",\"code\":\"{code}\"}}\n"
        ));
        let input = self.input.as_mut().context("child stdin already closed")?;
        input.write_all(record.as_bytes())?;
        input.flush()?;
        Ok(())
    }
    fn close_input(&mut self) {
        drop(self.input.take());
    }
    fn wait_exit(&mut self, timeout: Duration) -> Result<ExitStatus> {
        let end = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            ensure!(Instant::now() < end, "enrollment child did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn reap_reader(&mut self) -> Result<()> {
        if let Some(reader) = self.reader.take() {
            let end = Instant::now() + Duration::from_secs(2);
            while !reader.is_finished() {
                ensure!(Instant::now() < end, "child reader did not finish");
                thread::sleep(Duration::from_millis(5));
            }
            reader
                .join()
                .map_err(|_| anyhow::anyhow!("child reader panicked"))?;
        }
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.close_input();
        if self.wait_exit(Duration::from_secs(1)).is_err() {
            let _ = self.child.kill();
            let _ = self.wait_exit(Duration::from_secs(2));
        }
        let _ = self.reap_reader();
    }
}

struct Evidence(PathBuf);
impl Evidence {
    fn new() -> Result<Self> {
        let root = std::env::current_dir()?
            .join("runs")
            .join(format!("worker-enrollment-{}", discovery::new_public_id()?));
        fs::create_dir_all(root.parent().context("missing report parent")?)?;
        fs::create_dir(&root)?;
        Ok(Self(root))
    }
    fn cleanup(&self) -> Result<()> {
        ensure!(plain_directory(&self.0)?, "test evidence root disappeared");
        for case in ["success", "wrong-code", "eof"] {
            let profile = self.0.join(format!("{case}-profile"));
            let peers = profile.join("Crossflow/peers");
            if plain_directory(&profile)?
                && plain_directory(&profile.join("Crossflow"))?
                && plain_directory(&peers)?
            {
                for entry in fs::read_dir(&peers)? {
                    let path = entry?.path();
                    let id = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("invalid test peer directory")?;
                    ensure!(is_id(id), "unexpected profile contents");
                    remove_bundle(&path, &["identity.dpapi", "peer.json", "peer.pending.json"])?;
                }
                fs::remove_dir(&peers)?;
            }
            for dir in [profile.join("Crossflow"), profile] {
                if plain_directory(&dir)? {
                    fs::remove_dir(dir)?;
                }
            }
            let fixture = self.0.join(format!("{case}-fixture"));
            if plain_directory(&fixture)? {
                remove_bundle(&fixture.join("mac"), &["identity.dpapi"])?;
            }
        }
        Ok(())
    }
}
impl Drop for Evidence {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn plain_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
                "unsafe test credential directory"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
fn remove_bundle(path: &Path, allowed: &[&str]) -> Result<()> {
    if !plain_directory(path)? {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        ensure!(
            allowed.iter().any(|name| entry.file_name() == *name),
            "unknown credential file: refusing recursive cleanup"
        );
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
            "unsafe test credential file"
        );
        fs::remove_file(entry.path())?;
    }
    fs::remove_dir(path)?;
    Ok(())
}
fn is_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn fixture(
    binary: &Path,
    root: &Path,
    ip: IpAddr,
) -> Result<(Process, Zeroizing<String>, SocketAddr, SocketAddr)> {
    let mut command = Command::new(binary);
    command
        .arg("--root")
        .arg(root)
        .arg("--bind")
        .arg(SocketAddr::new(ip, 0).to_string())
        .args(["--seconds", "30"]);
    let process = Process::start(command)?;
    let Event::Code(code) = process.event(Duration::from_secs(5))? else {
        bail!("fixture did not provide private code");
    };
    let ready = process.public("ready", Duration::from_secs(5))?;
    let pairing = ready["pairing_endpoint"]
        .as_str()
        .context("missing fixture pairing endpoint")?
        .parse()?;
    let input = ready["input_endpoint"]
        .as_str()
        .context("missing fixture input endpoint")?
        .parse()?;
    Ok((process, code, pairing, input))
}

fn exercise(
    evidence: &Evidence,
    fixture_binary: &Path,
    worker_binary: &Path,
    ip: IpAddr,
    case: &str,
) -> Result<Value> {
    let (mut fixture, mut code, pairing, input) = fixture(
        fixture_binary,
        &evidence.0.join(format!("{case}-fixture")),
        ip,
    )?;
    let profile = evidence.0.join(format!("{case}-profile"));
    let mut command = Command::new(worker_binary);
    command.env("LOCALAPPDATA", &profile);
    let mut worker = Process::start(command)?;
    if case == "wrong-code" {
        let replacement = if code.as_bytes()[0] == b'0' { "1" } else { "0" };
        code.replace_range(0..1, replacement);
    }
    worker.begin(pairing, &code)?;
    drop(code);
    let result = if case == "wrong-code" {
        let error = worker.public("error", Duration::from_secs(8))?;
        let category = error["error"]
            .as_str()
            .context("missing fixed error category")?;
        ensure!(
            category == "authentication_failed",
            "wrong code produced incorrect fixed error category"
        );
        ensure!(
            !worker.wait_exit(Duration::from_secs(3))?.success(),
            "wrong code succeeded"
        );
        let responder_error = fixture.public("error", Duration::from_secs(3))?;
        ensure!(
            responder_error["code"] == "pairing_failed",
            "responder did not reject the incorrect code"
        );
        ensure!(
            !profile.join("Crossflow/peers").exists(),
            "wrong code wrote trust"
        );
        json!({"case":case,"error":category,"saved":false})
    } else {
        let pending = worker.public("pending", Duration::from_secs(8))?;
        fixture.public("pending", Duration::from_secs(3))?;
        let fingerprint = pending["fingerprint"]
            .as_str()
            .context("missing fingerprint")?;
        ensure!(
            is_id(fingerprint)
                && pending["platform"] == "macos"
                && pending["name"] == "Crossflow Mac test fixture",
            "incorrect authenticated peer info"
        );
        if case == "eof" {
            worker.close_input();
            let error = worker.public("error", Duration::from_secs(3))?;
            ensure!(
                error["error"] == "cancelled",
                "EOF produced incorrect cancellation category"
            );
            ensure!(
                !worker.wait_exit(Duration::from_secs(3))?.success(),
                "EOF returned saved success"
            );
            ensure!(!profile.join("Crossflow/peers").exists(), "EOF wrote trust");
            json!({"case":case,"saved":false,"eof_cancelled":true})
        } else {
            // These are two explicit fixture control decisions, never auto-arm.
            fixture.command("approve")?;
            worker.command("approve")?;
            let saved = worker.public("saved", Duration::from_secs(5))?;
            ensure!(
                saved["id"] == fingerprint,
                "saved ID differs from approved fingerprint"
            );
            ensure!(
                worker.wait_exit(Duration::from_secs(3))?.success(),
                "saved worker did not exit cleanly"
            );
            let ready = fixture.public("input_ready", Duration::from_secs(5))?;
            ensure!(
                ready["protection"] == "windows_dpapi" && ready["role"] == "mac",
                "fixture storage claim mismatch"
            );
            let store = PeerStore::at(profile.join("Crossflow/peers"));
            let (summary, bundle) = store.resolve(fingerprint)?;
            ensure!(
                summary.last_endpoint == Some(input),
                "saved input endpoint differs from authenticated metadata"
            );
            ensure!(
                Identity::load(&bundle)?.role == "windows",
                "saved sender role mismatch"
            );
            ensure!(
                !bundle.join("identity.json").exists(),
                "plaintext identity created"
            );
            json!({"case":case,"saved":true,"fingerprint_matches_saved_id":true,"endpoint_matches_authenticated_input_port":true,"protected_identity_reopened":true})
        }
    };
    fixture.command("stop")?;
    fixture.close_input();
    fixture.public("stopped", Duration::from_secs(5))?;
    ensure!(
        fixture.wait_exit(Duration::from_secs(3))?.success(),
        "fixture did not cleanly stop"
    );
    worker.reap_reader()?;
    fixture.reap_reader()?;
    Ok(result)
}

#[test]
#[ignore = "real child processes, local LAN TCP/mDNS and isolated Windows DPAPI; no OS input; requires built pairing_fixture and CROSSFLOW_TEST_LAN_IP"]
fn product_worker_saves_only_after_two_explicit_approvals_and_cancels_without_trust() -> Result<()>
{
    let ip: IpAddr = std::env::var("CROSSFLOW_TEST_LAN_IP")
        .context("set CROSSFLOW_TEST_LAN_IP to an owned private LAN address")?
        .parse()?;
    let worker = PathBuf::from(env!("CARGO_BIN_EXE_crossflow-enroll"));
    let fixture = std::env::var_os("CROSSFLOW_PAIRING_FIXTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            worker
                .parent()
                .expect("worker binary parent")
                .join("examples/pairing_fixture.exe")
        });
    ensure!(
        worker.is_file() && fixture.is_file(),
        "build worker and pairing_fixture first"
    );
    let evidence = Evidence::new()?;
    let mut cases = Vec::new();
    let result = (|| -> Result<()> {
        for case in ["success", "wrong-code", "eof"] {
            cases.push(exercise(&evidence, &fixture, &worker, ip, case)?);
        }
        Ok(())
    })();
    let cleanup = evidence.cleanup();
    let report = json!({"ok":result.is_ok() && cleanup.is_ok(),"cases":cases,"os_input":false,"ui":false,
        "secrets_logged":false,"credentials_removed":cleanup.is_ok(),"responder_storage":"Windows DPAPI, not macOS Keychain"});
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(evidence.0.join("result.json"))?;
    serde_json::to_writer_pretty(output, &report)?;
    eprintln!(
        "Non-secret worker enrollment report: {}",
        evidence.0.join("result.json").display()
    );
    cleanup?;
    result
}
