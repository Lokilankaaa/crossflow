//! Supplemental T05 recovery evidence, NOT T14 packet-loss/jitter acceptance.
//! This relay stalls an already reliable TCP byte stream, after transport.
//! No real input adapter, network-interface change, or third-party traffic.
#[path = "support/stall_relay.rs"]
mod stall_relay;

use anyhow::{Result, ensure};
use crossflow::{
    input::{Button, CaptureOutcome, SourceInputQueue},
    metrics::{Log, LogWriter, summarize},
    security,
    session::{self, MouseSink, SendOptions},
};
use serde_json::{Value, json};
use stall_relay::StallRelay;
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq)]
enum Event {
    Move(f64, f64),
    Key(u16, bool),
    Button(Button, bool),
    Scroll(i32, i32),
}
struct FakeSink(Arc<Mutex<Vec<Event>>>);
impl MouseSink for FakeSink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        self.0.lock().unwrap().push(Event::Move(x, y));
        Ok((x, y))
    }
    fn key(&mut self, key: u16, down: bool) -> Result<()> {
        self.0.lock().unwrap().push(Event::Key(key, down));
        Ok(())
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        self.0.lock().unwrap().push(Event::Button(button, down));
        Ok(())
    }
    fn scroll(&mut self, x: i32, y: i32) -> Result<()> {
        self.0.lock().unwrap().push(Event::Scroll(x, y));
        Ok(())
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}
impl Worker {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let Some(handle) = self.handle.as_ref() else {
            return Ok(());
        };
        let until = Instant::now() + Duration::from_secs(5);
        while !handle.is_finished() {
            ensure!(
                Instant::now() < until,
                "test session did not stop within its deadline"
            );
            thread::sleep(Duration::from_millis(5));
        }
        self.handle
            .take()
            .unwrap()
            .join()
            .map_err(|_| anyhow::anyhow!("test session panicked"))?
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

struct Harness {
    root: PathBuf,
    events: Arc<Mutex<Vec<Event>>>,
    queue: Arc<Mutex<SourceInputQueue>>,
    source_log: Log,
    relay: StallRelay,
    sender: Worker,
    receiver: Worker,
}
impl Harness {
    fn start() -> Result<Self> {
        let root = std::env::current_dir()?.join("runs").join(format!(
            "network-stall-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&root)?;
        security::create_pair(&root.join("identities"))?;
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        let address = reservation.local_addr()?;
        let events = Arc::new(Mutex::new(Vec::new()));
        let receiver_stop = Arc::new(AtomicBool::new(false));
        let worker_stop = receiver_stop.clone();
        let mut sink = FakeSink(events.clone());
        let identity = root.join("identities/mac");
        let receiver_writer = LogWriter::new(
            &root.join("receiver.jsonl"),
            json!({"test":"five_tcp_stream_stalls","actual_os_input":false}),
        )?;
        drop(reservation);
        let receiver = Worker {
            stop: receiver_stop,
            handle: Some(thread::spawn(move || {
                let result = session::receive_with_stop(
                    &address.to_string(),
                    &identity,
                    &mut sink,
                    receiver_writer.log.clone(),
                    60.0,
                    &worker_stop,
                );
                receiver_writer.finish()?;
                result
            })),
        };
        let relay = StallRelay::start(address)?;
        let source_writer = LogWriter::new(
            &root.join("sender.jsonl"),
            json!({"test":"five_tcp_stream_stalls","actual_os_input":false}),
        )?;
        let source_log = source_writer.log.clone();
        let mut source = SourceInputQueue::new();
        source
            .seed_physical_held([], [])
            .expect("empty initial snapshot");
        let queue = Arc::new(Mutex::new(source));
        let sender_stop = Arc::new(AtomicBool::new(false));
        let worker_stop = sender_stop.clone();
        let worker_queue = queue.clone();
        let host = relay.address().to_string();
        let identity = root.join("identities/windows");
        let sender = Worker {
            stop: sender_stop,
            handle: Some(thread::spawn(move || {
                let result = session::send_ordered(
                    SendOptions {
                        host: &host,
                        bundle: &identity,
                        seconds: 60.0,
                        max_hz: 1000,
                        synthetic: false,
                    },
                    worker_queue,
                    worker_stop,
                    source_writer.log.clone(),
                );
                source_writer.finish()?;
                result
            })),
        };
        Ok(Self {
            root,
            events,
            queue,
            source_log,
            relay,
            sender,
            receiver,
        })
    }
    fn wait(&self, seconds: u64, label: &str, predicate: impl Fn() -> bool) -> Result<()> {
        let until = Instant::now() + Duration::from_secs(seconds);
        while !predicate() {
            ensure!(
                Instant::now() < until,
                "{label} timed out; retained evidence: {}",
                self.root.display()
            );
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn expect_events(&self, expected: &[Event]) -> Result<()> {
        self.wait(2, "fake input observations", || {
            self.events.lock().unwrap().len() >= expected.len()
        })?;
        ensure!(
            *self.events.lock().unwrap() == expected,
            "unexpected/replayed input; evidence {}",
            self.root.display()
        );
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.sender.stop.store(true, Ordering::Release);
        self.receiver.stop.store(true, Ordering::Release);
        self.relay.finish()?;
        self.sender.finish()?;
        self.receiver.finish()
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn log_records(path: PathBuf) -> Result<Vec<Value>> {
    fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[test]
#[ignore = "five 2s localhost TCP-stream stalls; fake input only, not T14 packet loss"]
fn repeated_stream_stalls_release_inputs_and_require_fresh_rearm() -> Result<()> {
    let mut h = Harness::start()?;
    let mut expected = Vec::new();
    let mut rounds = Vec::new();
    for round in 1..=5 {
        h.wait(5, "authenticated initial/recovered connection", || {
            h.queue.lock().unwrap().is_connected()
        })?;
        {
            let mut queue = h.queue.lock().unwrap();
            ensure!(
                !queue.is_active() && queue.pending_len() == 0,
                "connection must start paused and empty"
            );
            ensure!(queue.arm(), "explicit arm failed");
            let now = h.source_log.ns();
            ensure!(
                queue.move_by(f64::from(round), 1.0, now) == CaptureOutcome::Queued,
                "initial move not queued"
            );
            ensure!(
                queue.key(0xE0, true, now) == CaptureOutcome::Queued,
                "initial key not queued"
            );
            ensure!(
                queue.button(Button::Left, true, now) == CaptureOutcome::Queued,
                "initial button not queued"
            );
        }
        expected.extend([
            Event::Move(f64::from(round), 1.0),
            Event::Key(0xE0, true),
            Event::Button(Button::Left, true),
        ]);
        h.expect_events(&expected)?;
        h.relay.set_stalled(true);
        h.relay.wait_stalled(Duration::from_secs(1))?;
        let began = Instant::now();
        let before = h.relay.stats();
        {
            let mut queue = h.queue.lock().unwrap();
            ensure!(
                queue.is_active(),
                "source expired before stall backlog could be exercised"
            );
            let now = h.source_log.ns();
            ensure!(
                queue.move_by(9999.0, 9999.0, now) == CaptureOutcome::Queued,
                "blocked move not queued"
            );
            ensure!(
                queue.scroll(0, 120, now) == CaptureOutcome::Queued,
                "blocked wheel not queued"
            );
            ensure!(
                queue.key(0x05, true, now) == CaptureOutcome::Queued,
                "blocked new key not queued"
            );
        }
        h.wait(1, "sender recognizing lost lease", || {
            !h.queue.lock().unwrap().is_connected()
        })?;
        ensure!(
            !h.queue.lock().unwrap().is_active(),
            "disconnected source remained armed"
        );
        expected.extend([Event::Key(0xE0, false), Event::Button(Button::Left, false)]);
        h.expect_events(&expected)?;
        let cleanup_observed_ms = began.elapsed().as_secs_f64() * 1000.0;
        thread::sleep(Duration::from_secs(2).saturating_sub(began.elapsed()));
        let stalled_ms = began.elapsed().as_secs_f64() * 1000.0;
        let after = h.relay.stats();
        ensure!(
            after.forwarded_bytes == before.forwarded_bytes
                && after.forwarded_to_back_bytes == before.forwarded_to_back_bytes
                && after.forwarded_to_front_bytes == before.forwarded_to_front_bytes
                && after.read_front_bytes == before.read_front_bytes
                && after.read_back_bytes == before.read_back_bytes
                && after.accepted == before.accepted,
            "relay read, wrote or accepted during confirmed blackout"
        );
        h.expect_events(&expected)?;
        h.relay.set_stalled(false);
        let resumed = Instant::now();
        h.wait(5, "authenticated reconnection", || {
            h.queue.lock().unwrap().is_connected()
        })?;
        let reconnection_observed_ms = resumed.elapsed().as_secs_f64() * 1000.0;
        // Observe several heartbeat periods without arming; reconnect alone must
        // not deliver any queued movement, click, scroll or new key from blackout.
        let quiet_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < quiet_until {
            let queue = h.queue.lock().unwrap();
            ensure!(
                queue.is_connected() && !queue.is_active() && queue.pending_len() == 0,
                "recovery changed control state"
            );
            drop(queue);
            ensure!(
                *h.events.lock().unwrap() == expected,
                "old input replayed during paused recovery"
            );
            thread::sleep(Duration::from_millis(5));
        }
        {
            let mut queue = h.queue.lock().unwrap();
            ensure!(queue.arm(), "post-recovery explicit arm failed");
            let now = h.source_log.ns();
            // Never reseed or manually begin/end a session: the production queue
            // must preserve its physical snapshot across all five reconnects.
            for key in [0xE0, 0x05] {
                ensure!(
                    queue.key(key, true, now) == CaptureOutcome::Ignored,
                    "pre-held key replayed"
                );
                ensure!(
                    queue.key(key, false, now) == CaptureOutcome::Ignored,
                    "pre-held key up leaked"
                );
            }
            ensure!(
                queue.button(Button::Left, true, now) == CaptureOutcome::Ignored,
                "pre-held button replayed"
            );
            ensure!(
                queue.button(Button::Left, false, now) == CaptureOutcome::Ignored,
                "pre-held button up leaked"
            );
            ensure!(
                queue.key(0x04, true, now) == CaptureOutcome::Queued,
                "fresh key down refused"
            );
            ensure!(
                queue.key(0x04, false, now) == CaptureOutcome::Queued,
                "fresh key up refused"
            );
            ensure!(
                queue.button(Button::Left, true, now) == CaptureOutcome::Queued,
                "fresh click down refused"
            );
            ensure!(
                queue.button(Button::Left, false, now) == CaptureOutcome::Queued,
                "fresh click up refused"
            );
            ensure!(
                queue.move_by(2.0, 1.0, now) == CaptureOutcome::Queued,
                "fresh move refused"
            );
        }
        expected.extend([
            Event::Key(0x04, true),
            Event::Key(0x04, false),
            Event::Button(Button::Left, true),
            Event::Button(Button::Left, false),
            Event::Move(2.0, 1.0),
        ]);
        h.expect_events(&expected)?;
        h.queue.lock().unwrap().pause();
        rounds.push(json!({"round":round,"stream_blackout_ms":stalled_ms,
            "cleanup_observed_ms":cleanup_observed_ms,"cumulative_fake_events":expected.len(),
            "reconnection_observed_ms":reconnection_observed_ms,
            "paused_recovery_and_preheld_snapshot_verified":true}));
    }
    h.finish()?;
    h.expect_events(&expected)?;
    let stats = h.relay.stats();
    ensure!(
        stats.stalled_cycles == 5 && stats.accepted >= 6,
        "missing relay blackout/reconnect evidence"
    );
    ensure!(
        stats.max_buffered_bytes <= 2 * 65536,
        "relay exceeded fixed memory bound"
    );
    let sender = summarize(&h.root.join("sender.jsonl"))?;
    let receiver = summarize(&h.root.join("receiver.jsonl"))?;
    for summary in [&sender, &receiver] {
        ensure!(
            summary["complete_log"] == true && summary["dropped_log_records"] == 0,
            "incomplete diagnostic evidence"
        );
    }
    let records = log_records(h.root.join("sender.jsonl"))?;
    let reconnects = records.iter().filter(|v| v["event"] == "connected").count();
    ensure!(
        reconnects >= 6,
        "reconnections were not authenticated by production sender"
    );
    // All scrolls are blackout probes. Prove they entered the encrypted sender
    // path, rather than only testing queue mutations that were never drained.
    let blocked_scrolls = records
        .iter()
        .filter(|v| v["event"] == "input_queued" && v["kind"] == "Scroll")
        .count();
    ensure!(
        blocked_scrolls == 5,
        "not all blackout probes reached encrypted queueing"
    );
    let status = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .output()?;
    let dirty = status.status.success().then_some(!status.stdout.is_empty());
    let report = json!({"test":"five TCP-stream blackout recoveries", "status":"passed",
        "core_build_revision":env!("CROSSFLOW_BUILD_REVISION"),
        "compiler":env!("CROSSFLOW_BUILD_COMPILER"),"working_tree_dirty_at_run":dirty,
        "scope":"supplemental T05; post-transport byte relay, not T14 packet loss/jitter",
        "actual_os_input":false,"visible_smoothness":"NOT_MEASURED", "rounds":rounds,
        "authenticated_connections":reconnects,"blackout_scrolls_queued":blocked_scrolls,
        "fake_events":expected.len(),"relay_accepted":stats.accepted,
        "relay_read_front_bytes":stats.read_front_bytes,"relay_read_back_bytes":stats.read_back_bytes,
        "relay_forwarded_to_back_bytes":stats.forwarded_to_back_bytes,
        "relay_forwarded_to_front_bytes":stats.forwarded_to_front_bytes,
        "relay_forwarded_bytes":stats.forwarded_bytes,"relay_max_buffered_bytes":stats.max_buffered_bytes,
        "sender":sender,"receiver":receiver});
    fs::write(
        h.root.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    eprintln!("Retained supplemental stall evidence: {}", h.root.display());
    Ok(())
}
