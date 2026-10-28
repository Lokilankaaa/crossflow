//! Opt-in, real mDNS + TCP recovery on ONE explicitly selected local RFC1918 IP.
//! No capture, injection, GUI, real saved peer, or pairing state is used.
//! Run: $env:CROSSFLOW_TEST_LAN_IP='192.168.1.164';
//! cargo test --test peer_discovery_recovery -- --ignored --nocapture
//! Optional CROSSFLOW_TEST_SECOND_LAN_IP exercises another local RFC1918 IP.
//! This tests same-host LAN-interface routing, NOT cross-machine discovery or
//! physical-input/latency acceptance. Advertisements remain unauthenticated.

use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    discovery::{self, Advertisement, DiscoveryConfig, DiscoveryHandle, DiscoveryState},
    metrics::{self, Log, LogWriter},
    protocol::WIRE_VERSION,
    security,
    session::{self, DrySink, MouseSink, SendOptions},
    state::Motion,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SESSION_SECONDS: f64 = 120.0;
const JOIN_BUDGET: Duration = Duration::from_secs(6);

fn requested_ip(value: &str) -> Result<Ipv4Addr> {
    let ip: Ipv4Addr = value
        .parse()
        .context("CROSSFLOW_TEST_LAN_IP must be a literal IPv4 address")?;
    ensure!(
        ip.is_private(),
        "LAN test requires an explicit RFC1918 address, not loopback or public IP"
    );
    Ok(ip)
}

fn bind_local(ip: Ipv4Addr) -> Result<TcpListener> {
    // Successful explicit binding also verifies that this is a local address.
    // Never fall back to an unspecified address or resolve a hostname.
    TcpListener::bind(SocketAddr::new(IpAddr::V4(ip), 0))
        .context("Cannot bind requested local LAN IP; no fallback interface is permitted")
}

fn wait_for(label: &str, limit: Duration, mut ready: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + limit;
    while !ready() {
        ensure!(Instant::now() < deadline, "timed out: {label}");
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

#[derive(Default, Clone, Copy, Debug, Serialize)]
struct Observed {
    resets: u64,
    moves: u64,
    x: f64,
    y: f64,
}

struct RecordingDrySink {
    sink: DrySink,
    observed: Arc<Mutex<Observed>>,
}
impl MouseSink for RecordingDrySink {
    fn reset(&mut self) -> Result<()> {
        self.observed.lock().unwrap().resets += 1;
        Ok(())
    }
    fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64)> {
        let point = self.sink.move_by(dx, dy)?;
        let mut observed = self.observed.lock().unwrap();
        observed.moves = self.sink.moves;
        observed.x = point.0;
        observed.y = point.1;
        Ok(point)
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<()>>>,
}
impl Worker {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = &self.thread {
            wait_for("test session shutdown", JOIN_BUDGET, || {
                handle.is_finished()
            })?;
        }
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("test session thread panicked"))??;
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // Never unconditionally join a stuck worker. A timeout is recorded by
        // Rig::finish; detaching cannot claim that the worker was reclaimed.
        let _ = self.finish();
    }
}

struct Rig {
    root: PathBuf,
    pair_dirs: Vec<PathBuf>,
    workers: Vec<Worker>,
    advertisements: Vec<DiscoveryHandle>,
    logs: Vec<PathBuf>,
}
impl Rig {
    fn new() -> Result<Self> {
        let root = std::env::current_dir()?.join("runs").join(format!(
            "peer-discovery-recovery-{}",
            discovery::new_public_id()?
        ));
        fs::create_dir_all(root.parent().context("missing evidence parent")?)?;
        fs::create_dir(&root)?;
        eprintln!("LAN discovery recovery evidence: {}", root.display());
        Ok(Self {
            root,
            pair_dirs: vec![],
            workers: vec![],
            advertisements: vec![],
            logs: vec![],
        })
    }

    fn pair(&mut self, name: &str) -> Result<security::PairInfo> {
        let path = self.root.join(name);
        self.pair_dirs.push(path.clone());
        security::create_pair(&path)
    }

    fn logger(&mut self, name: &str) -> Result<LogWriter> {
        let path = self.root.join(format!("{name}.jsonl"));
        let writer = LogWriter::new(
            &path,
            json!({
                "test":"peer_discovery_recovery", "actual_os_input":false,
                "same_host_lan_interface":true, "role":name
            }),
        )?;
        self.logs.push(path);
        Ok(writer)
    }

    fn receiver(
        &mut self,
        name: &str,
        listener: TcpListener,
        bundle: PathBuf,
    ) -> Result<(usize, Arc<Mutex<Observed>>)> {
        let writer = self.logger(name)?;
        let observed = Arc::new(Mutex::new(Observed::default()));
        let mut sink = RecordingDrySink {
            sink: DrySink::default(),
            observed: observed.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || {
            let result = session::receive_on_listener(
                &listener,
                &bundle,
                &mut sink,
                writer.log.clone(),
                SESSION_SECONDS,
                &thread_stop,
            );
            let finalized = writer.finish(); // also attempted when the receiver fails
            result.and(finalized)
        });
        let index = self.workers.len();
        self.workers.push(Worker {
            stop,
            thread: Some(handle),
        });
        Ok((index, observed))
    }

    fn sender(
        &mut self,
        host: SocketAddr,
        bundle: PathBuf,
        motion: Arc<Mutex<Motion>>,
    ) -> Result<Log> {
        let writer = self.logger("sender")?;
        let log = writer.log.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = thread::spawn(move || {
            let host = host.to_string();
            let result = session::send_discovered(
                SendOptions {
                    host: &host,
                    bundle: &bundle,
                    seconds: SESSION_SECONDS,
                    max_hz: 250,
                    synthetic: false,
                },
                motion,
                thread_stop,
                writer.log.clone(),
            );
            let finalized = writer.finish();
            result.and(finalized)
        });
        self.workers.push(Worker {
            stop,
            thread: Some(handle),
        });
        Ok(log)
    }

    fn advertise(&mut self, address: SocketAddr) -> Result<(usize, String)> {
        let id = discovery::new_public_id()?;
        let handle = DiscoveryHandle::start(DiscoveryConfig {
            advertisement: Some(Advertisement {
                device_id: id.clone(),
                friendly_name: format!("Crossflow route test {}", &id[..8]),
                platform: std::env::consts::OS.into(),
                protocol: u16::from(WIRE_VERSION),
                port: address.port(),
                listen_ip: address.ip(),
            }),
            local_device_id: Some(id.clone()),
            compatible_protocol: u16::from(WIRE_VERSION),
        })?;
        let index = self.advertisements.len();
        self.advertisements.push(handle);
        let handle = &self.advertisements[index];
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(snapshot) = handle.try_snapshot() {
                ensure!(
                    snapshot.error.is_none(),
                    "advertisement daemon reported an error"
                );
                if snapshot.state == DiscoveryState::Browsing {
                    break;
                }
                ensure!(
                    snapshot.state == DiscoveryState::Starting,
                    "advertisement stopped unexpectedly"
                );
            }
            ensure!(
                Instant::now() < deadline,
                "advertisement did not become ready"
            );
            thread::sleep(Duration::from_millis(2));
        }
        Ok((index, id))
    }

    fn finish(&mut self) -> Result<()> {
        // Signal ALL workers before waiting for any individual one.
        for worker in &self.workers {
            worker.stop.store(true, Ordering::Release);
        }
        for ad in &self.advertisements {
            ad.stop();
        }
        let mut failures = Vec::new();
        for worker in &mut self.workers {
            if let Err(error) = worker.finish() {
                failures.push(format!("worker: {error:#}"));
            }
        }
        for ad in &mut self.advertisements {
            if let Err(error) = ad.wait_stopped(Duration::from_secs(3)) {
                failures.push(format!("discovery: {error:#}"));
            }
        }
        ensure!(
            failures.is_empty(),
            "bounded cleanup failed: {}",
            failures.join("; ")
        );
        Ok(())
    }

    fn cleanup_keys(&self) -> Result<()> {
        let mut deleted = Vec::new();
        let mut failures = Vec::new();
        for pair in &self.pair_dirs {
            for role in ["windows", "mac"] {
                let directory = pair.join(role);
                let key = directory.join("identity.json");
                match fs::remove_file(&key) {
                    Ok(()) => deleted.push(key.strip_prefix(&self.root)?.to_path_buf()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => failures.push(format!("{}: {e}", key.display())),
                }
                if let Err(e) = fs::remove_dir(&directory)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    failures.push(format!("{}: {e}", directory.display()));
                }
            }
            if let Err(e) = fs::remove_dir(pair)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                failures.push(format!("{}: {e}", pair.display()));
            }
        }
        // Explicit file removal only; never recursively delete an evidence tree.
        fs::write(
            self.root.join("key-cleanup.json"),
            serde_json::to_vec_pretty(&json!({
                "deleted_test_key_files":deleted, "failures":failures,
                "only_generated_fixture_paths":true,
                "note":"File removal is not a claim of secure media erasure."
            }))?,
        )?;
        ensure!(failures.is_empty(), "fixture key cleanup failed");
        Ok(())
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.finish();
        // Normal execution already wrote its cleanup record. Retain that list.
        if !self.root.join("key-cleanup.json").exists() {
            let _ = self.cleanup_keys();
        }
    }
}

fn snapshot(observed: &Arc<Mutex<Observed>>) -> Observed {
    *observed.lock().unwrap()
}

fn assert_paused(motion: &Arc<Mutex<Motion>>, sink: &Arc<Mutex<Observed>>) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(250);
    while Instant::now() < deadline {
        let state = motion.lock().unwrap();
        ensure!(
            state.connected && !state.active,
            "authenticated reconnect must remain paused"
        );
        ensure!(
            snapshot(sink).moves == 0 && snapshot(sink).resets == 0,
            "input replayed before explicit arm"
        );
        drop(state);
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn arm_and_move(
    motion: &Arc<Mutex<Motion>>,
    sink: &Arc<Mutex<Observed>>,
    log: &Log,
    dx: f64,
    dy: f64,
) -> Result<()> {
    ensure!(
        motion.lock().unwrap().toggle(),
        "explicit arm rejected while connected"
    );
    wait_for(
        "receiver accepted explicit Arm",
        Duration::from_secs(3),
        || snapshot(sink).resets == 1,
    )?;
    // Arm establishes the cumulative baseline. Only add this test movement
    // AFTER the receiver acknowledged it by resetting the fake adapter.
    motion.lock().unwrap().add(dx, dy, log.ns());
    wait_for(
        "encrypted synthetic delta reached DrySink",
        Duration::from_secs(3),
        || {
            let got = snapshot(sink);
            got.moves == 1 && got.x == dx && got.y == dy
        },
    )
}

fn records(path: &Path) -> Result<Vec<Value>> {
    fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn scenario(
    rig: &mut Rig,
    ip: Ipv4Addr,
    wrong_listener: TcpListener,
    next_listener: TcpListener,
) -> Result<Value> {
    let correct_pair = rig.pair("correct-test-pair")?;
    let wrong_pair = rig.pair("wrong-test-pair")?;
    let wrong = wrong_listener.local_addr()?;
    let (_, wrong_sink) = rig.receiver("wrong-identity", wrong_listener, wrong_pair.mac_bundle)?;
    let first_listener = bind_local(ip)?;
    let first = first_listener.local_addr()?;
    let (first_worker, first_sink) = rig.receiver(
        "first-correct",
        first_listener,
        correct_pair.mac_bundle.clone(),
    )?;
    let (first_ad, first_id) = rig.advertise(first)?;
    let motion = Arc::new(Mutex::new(Motion::default()));
    let log = rig.sender(wrong, correct_pair.windows_bundle, motion.clone())?;
    let started = Instant::now();
    wait_for(
        "discover and authenticate correct peer after wrong saved endpoint",
        Duration::from_secs(35),
        || motion.lock().unwrap().connected,
    )?;
    let initial_ms = started.elapsed().as_millis();
    assert_paused(&motion, &first_sink)?;
    ensure!(
        snapshot(&wrong_sink).moves == 0 && snapshot(&wrong_sink).resets == 0,
        "wrong identity received input"
    );
    arm_and_move(&motion, &first_sink, &log, 17.0, -9.0)?;

    // Stop the old advertisement and session. The sender itself is never
    // restarted, nor are its stored identity or cumulative coordinates replaced.
    rig.advertisements[first_ad].wait_stopped(Duration::from_secs(3))?;
    rig.workers[first_worker].finish()?;
    wait_for(
        "sender observes disconnected old receiver",
        Duration::from_secs(5),
        || !motion.lock().unwrap().connected,
    )?;
    let next = next_listener.local_addr()?;
    ensure!(
        next != first && next != wrong,
        "OS reused a test port; rerun to exercise a distinct third endpoint"
    );
    let (_, next_sink) = rig.receiver("next-correct", next_listener, correct_pair.mac_bundle)?;
    let (_, next_id) = rig.advertise(next)?;
    ensure!(
        first_id != next_id,
        "new advertisement must have a fresh untrusted ID"
    );
    let reconnect_started = Instant::now();
    wait_for(
        "same sender discovers replacement endpoint",
        Duration::from_secs(35),
        || motion.lock().unwrap().connected,
    )?;
    let reconnect_ms = reconnect_started.elapsed().as_millis();
    // Even an attempted local delta while paused must not survive rearming.
    motion.lock().unwrap().add(1234.0, 5678.0, log.ns());
    assert_paused(&motion, &next_sink)?;
    arm_and_move(&motion, &next_sink, &log, -23.0, 11.0)?;
    ensure!(
        snapshot(&first_sink).moves == 1,
        "stopped receiver acquired further input"
    );
    ensure!(
        snapshot(&wrong_sink).moves == 0 && snapshot(&wrong_sink).resets == 0,
        "wrong identity accepted input"
    );
    Ok(
        json!({"wrong_endpoint":wrong,"first_endpoint":first,"next_endpoint":next,
        "first_advertisement_id":first_id,"next_advertisement_id":next_id,
        "initial_recovery_ms":initial_ms,"address_change_recovery_ms":reconnect_ms,
        "wrong_sink":snapshot(&wrong_sink),"first_sink":snapshot(&first_sink),"next_sink":snapshot(&next_sink),
        "sender_restart_count":0,"explicit_arm_count":2,"paused_hold_ms_per_connection":250}),
    )
}

fn verify_logs(rig: &Rig, observed: &Value) -> Result<Vec<Value>> {
    let sender = records(&rig.root.join("sender.jsonl"))?;
    let wrong = &observed["wrong_endpoint"];
    let first_tcp = sender
        .iter()
        .find(|v| v["event"] == "tcp_connected")
        .context("no TCP connection evidence")?;
    ensure!(
        &first_tcp["peer_address"] == wrong,
        "saved wrong endpoint was not actually attempted first"
    );
    ensure!(
        sender.iter().any(|v| v["event"] == "error"
            && v["phase"] == "handshake"
            && &v["peer_address"] == wrong),
        "wrong identity handshake rejection was not recorded"
    );
    let connected: Vec<_> = sender
        .iter()
        .filter(|v| v["event"] == "connected")
        .collect();
    ensure!(
        connected.len() == 2,
        "expected exactly two authenticated connections, got {}",
        connected.len()
    );
    ensure!(
        connected[0]["peer_address"] == observed["first_endpoint"]
            && connected[1]["peer_address"] == observed["next_endpoint"],
        "authenticated endpoints differed from advertised fixture endpoints"
    );
    let wrong_log = records(&rig.root.join("wrong-identity.jsonl"))?;
    ensure!(
        !wrong_log.iter().any(|v| matches!(
            v["event"].as_str(),
            Some("connected" | "armed" | "move" | "input_applied")
        )),
        "wrong-identity receiver recorded authenticated state or input"
    );
    let mut summaries = Vec::new();
    for path in &rig.logs {
        let summary = metrics::summarize(path)?;
        ensure!(
            summary["complete_log"] == true && summary["dropped_log_records"] == 0,
            "incomplete test log: {}",
            path.display()
        );
        summaries.push(
            json!({"file":path.file_name().map(|name|name.to_string_lossy()),"summary":summary}),
        );
    }
    Ok(summaries)
}

#[test]
fn lan_fixture_rejects_implicit_or_nonprivate_addresses() {
    for value in [
        "",
        "localhost",
        "127.0.0.1",
        "0.0.0.0",
        "8.8.8.8",
        "224.0.0.251",
        "::1",
        "169.254.1.2",
        "192.168.1.164:24800",
    ] {
        assert!(
            requested_ip(value).is_err(),
            "unexpectedly accepted {value}"
        );
    }
    for value in ["192.168.1.164", "10.1.2.3", "172.16.0.1", "172.31.255.254"] {
        assert!(requested_ip(value).is_ok());
    }
}

#[test]
#[ignore = "real LAN mDNS/TCP; requires explicit CROSSFLOW_TEST_LAN_IP; DrySink only"]
fn wrong_identity_then_mdns_address_change_requires_fresh_arm() -> Result<()> {
    let configured = std::env::var("CROSSFLOW_TEST_LAN_IP")
        .context("Set CROSSFLOW_TEST_LAN_IP explicitly to this machine's RFC1918 IPv4 address")?;
    let ip = requested_ip(&configured)?;
    let wrong_listener = bind_local(ip)?; // reject nonlocal IP before keys or discovery
    let second_ip = match std::env::var("CROSSFLOW_TEST_SECOND_LAN_IP") {
        Ok(value) => requested_ip(&value).context("Invalid CROSSFLOW_TEST_SECOND_LAN_IP")?,
        Err(std::env::VarError::NotPresent) => ip,
        Err(error) => return Err(error.into()),
    };
    // Keep the new listener bound but unadvertised until replacement. Besides
    // verifying locality early, this prevents ephemeral port reuse on one IP.
    let next_listener = bind_local(second_ip)?;
    let mut rig = Rig::new()?;
    let result = scenario(&mut rig, ip, wrong_listener, next_listener);
    let cleanup = rig.finish();
    let logs = match (&result, &cleanup) {
        (Ok(observed), Ok(())) => verify_logs(&rig, observed),
        _ => Ok(vec![]),
    };
    let key_cleanup = rig.cleanup_keys();
    let ok = result.is_ok() && cleanup.is_ok() && logs.is_ok() && key_cleanup.is_ok();
    fs::write(
        rig.root.join("report.json"),
        serde_json::to_vec_pretty(&json!({
        "ok":ok,"actual_os_input":false,"same_host_lan_interface":true,"lan_ip":ip,
        "replacement_lan_ip":second_ip,"ip_changed":ip!=second_ip,
            "observed":result.as_ref().ok(),"scenario_error":result.as_ref().err().map(|e|format!("{e:#}")),
            "cleanup_error":cleanup.as_ref().err().map(|e|format!("{e:#}")),
            "log_error":logs.as_ref().err().map(|e|format!("{e:#}")),"logs":logs.as_ref().ok(),
            "key_cleanup_error":key_cleanup.as_ref().err().map(|e|format!("{e:#}")),
            "limitations":"Not cross-machine discovery, pairing, physical input, packet-loss emulation, or performance acceptance."
        }))?,
    )?;
    if !ok {
        bail!(
            "LAN recovery check failed; retained evidence: {}",
            rig.root.display()
        );
    }
    Ok(())
}

#[test]
#[ignore = "opt-in loopback TCP handshake cancellation; no mDNS or OS input"]
fn stop_during_silent_saved_endpoint_handshake_is_bounded() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let mut rig = Rig::new()?;
    let pair = rig.pair("silent-test-pair")?;
    let writer = rig.logger("silent-sender")?;
    let motion = Arc::new(Mutex::new(Motion::default()));
    let source = motion.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let handle = thread::spawn(move || {
        let host = address.to_string();
        let result = session::send_discovered(
            SendOptions {
                host: &host,
                bundle: &pair.windows_bundle,
                seconds: 10.0,
                max_hz: 250,
                synthetic: false,
            },
            source,
            thread_stop,
            writer.log.clone(),
        );
        let finalized = writer.finish();
        finalized?;
        ensure!(
            result.is_err(),
            "never-authenticated stopped sender unexpectedly reported success"
        );
        Ok(())
    });
    rig.workers.push(Worker {
        stop: stop.clone(),
        thread: Some(handle),
    });
    let outcome = (|| -> Result<u128> {
        let mut accepted = None;
        let deadline = Instant::now() + Duration::from_secs(2);
        while accepted.is_none() {
            match listener.accept() {
                Ok((socket, _)) => accepted = Some(socket),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(
                        Instant::now() < deadline,
                        "sender never contacted silent saved endpoint"
                    );
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(error.into()),
            }
        }
        // Observe one handshake byte, without replying, so stopping cannot race
        // the sender's pre-handshake phase checks. This is not an input record.
        let socket = accepted.as_mut().context("missing accepted socket")?;
        // Windows accepted sockets inherit the nonblocking listener mode.
        socket.set_nonblocking(false)?;
        socket.set_read_timeout(Some(Duration::from_secs(1)))?;
        socket.read_exact(&mut [0u8; 1])?;
        // Hold the accepted socket open without sending or closing anything.
        // Stop must interrupt by the finite handshake budget, not by test EOF.
        let stopped_at = Instant::now();
        stop.store(true, Ordering::Release);
        wait_for(
            "silent handshake cancellation",
            Duration::from_secs(2),
            || rig.workers[0].thread.as_ref().unwrap().is_finished(),
        )?;
        rig.workers[0].finish()?;
        let elapsed = stopped_at.elapsed().as_millis();
        let state = *motion.lock().unwrap();
        ensure!(
            !state.connected && !state.active,
            "silent endpoint became connected or armed"
        );
        drop(accepted);
        Ok(elapsed)
    })();
    let cleanup = rig.finish();
    let log_check = (|| -> Result<Value> {
        let rows = records(&rig.root.join("silent-sender.jsonl"))?;
        ensure!(
            rows.iter().any(|v| v["event"] == "tcp_connected"),
            "missing saved TCP attempt evidence"
        );
        ensure!(
            !rows.iter().any(|v| matches!(
                v["event"].as_str(),
                Some("connected" | "armed" | "move" | "input_applied" | "route_discovery_state")
            ) || matches!(v["kind"].as_str(), Some("Arm" | "Move"))),
            "silent unauthenticated endpoint caused discovery, connection, or input"
        );
        let summary = metrics::summarize(&rig.root.join("silent-sender.jsonl"))?;
        ensure!(
            summary["complete_log"] == true && summary["dropped_log_records"] == 0,
            "incomplete silent sender log"
        );
        Ok(summary)
    })();
    let keys = rig.cleanup_keys();
    let ok = outcome.is_ok() && cleanup.is_ok() && log_check.is_ok() && keys.is_ok();
    fs::write(
        rig.root.join("report.json"),
        serde_json::to_vec_pretty(&json!({
            "ok":ok,"test":"silent_saved_endpoint_stop","actual_os_input":false,"mdns_started":false,
            "elapsed_after_stop_ms":outcome.as_ref().ok(),"allowed_stop_ms":2000,
            "scenario_error":outcome.as_ref().err().map(|e|format!("{e:#}")),
            "cleanup_error":cleanup.as_ref().err().map(|e|format!("{e:#}")),
            "log_error":log_check.as_ref().err().map(|e|format!("{e:#}")),"log":log_check.as_ref().ok(),
            "key_cleanup_error":keys.as_ref().err().map(|e|format!("{e:#}"))
        }))?,
    )?;
    ensure!(
        ok,
        "silent handshake stop check failed; evidence: {}",
        rig.root.display()
    );
    Ok(())
}
