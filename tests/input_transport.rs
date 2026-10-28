//! Explicit encrypted localhost tests with a recording adapter, never OS input.
//! Run: cargo test --test input_transport -- --ignored --test-threads=1

use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    input::{Button, CaptureOutcome, SourceInputQueue},
    metrics::{Log, LogWriter},
    protocol::{Frame, Kind, button_code},
    security::{self, Identity, SecureChannel},
    session::{self, MouseSink, SendOptions},
};
use serde_json::json;
use std::{
    fs,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
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
    Button(Button, bool),
    Key(u16, bool),
    Scroll(i32, i32),
}

struct RecordingSink {
    events: Arc<Mutex<Vec<Event>>>,
    stop: Arc<AtomicBool>,
    fail_move: Arc<AtomicBool>,
    fail_key_release: Arc<AtomicBool>,
}
impl MouseSink for RecordingSink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        if self.fail_move.load(Ordering::Acquire) {
            bail!("test adapter movement failure");
        }
        self.events.lock().unwrap().push(Event::Move(x, y));
        Ok((x, y))
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(Event::Button(button, down));
        Ok(())
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        // Record attempts as well as successful calls, so a failure cannot hide
        // whether cleanup continued to the other held inputs.
        self.events.lock().unwrap().push(Event::Key(usage, down));
        if !down && self.fail_key_release.load(Ordering::Acquire) {
            bail!("test adapter key release failure");
        }
        Ok(())
    }
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(Event::Scroll(horizontal, vertical));
        Ok(())
    }
    fn emergency_pressed(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        // This is exclusively the fresh unique directory created by start().
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    fixture: Fixture,
    address: String,
    events: Arc<Mutex<Vec<Event>>>,
    stop: Arc<AtomicBool>,
    fail_move: Arc<AtomicBool>,
    fail_key_release: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}
impl Harness {
    fn start(label: &str) -> Result<Self> {
        let path = std::env::current_dir()?.join("runs").join(format!(
            "input-transport-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&path)?;
        let fixture = Fixture(path);
        security::create_pair(&fixture.0.join("identities"))?;
        let writer = LogWriter::new(
            &fixture.0.join("receiver.jsonl"),
            json!({"test":"input_transport","actual_os_input":false}),
        )?;
        // receive() binds its own listener. Reserve an ephemeral port, then
        // release it immediately before the receiver worker binds it.
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        let address = reservation.local_addr()?.to_string();
        let events = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let fail_move = Arc::new(AtomicBool::new(false));
        let fail_key_release = Arc::new(AtomicBool::new(false));
        let mut sink = RecordingSink {
            events: events.clone(),
            stop: stop.clone(),
            fail_move: fail_move.clone(),
            fail_key_release: fail_key_release.clone(),
        };
        let worker_address = address.clone();
        let identity = fixture.0.join("identities/mac");
        drop(reservation);
        let worker = thread::spawn(move || {
            let result = session::receive(
                &worker_address,
                &identity,
                &mut sink,
                writer.log.clone(),
                3.0,
            );
            writer.finish()?;
            result
        });
        Ok(Self {
            fixture,
            address,
            events,
            stop,
            fail_move,
            fail_key_release,
            worker: Some(worker),
        })
    }
    fn connect(&self) -> Result<Wire> {
        let deadline = Instant::now() + Duration::from_millis(700);
        let stream = loop {
            match TcpStream::connect(&self.address) {
                Ok(stream) => break stream,
                Err(error)
                    if error.kind() == ErrorKind::ConnectionRefused
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(2))
                }
                Err(error) => return Err(error).context("connect test receiver"),
            }
        };
        Wire::new(
            stream,
            &Identity::load(&self.fixture.0.join("identities/windows"))?,
        )
    }
    fn observed(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
    fn until(&self, predicate: impl Fn(&[Event]) -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(700);
        loop {
            if predicate(&self.events.lock().unwrap()) {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "adapter observation timed out: {:?}",
                self.observed()
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .context("receiver already joined")?
            .join()
            .expect("receiver panicked")
    }
    fn log(&self) -> Result<String> {
        Ok(fs::read_to_string(self.fixture.0.join("receiver.jsonl"))?)
    }
    fn restart(&mut self) -> Result<()> {
        ensure!(
            self.worker.is_none(),
            "join previous receiver before restarting"
        );
        let writer = LogWriter::new(
            &self.fixture.0.join("receiver-restarted.jsonl"),
            json!({"test":"input_transport_reconnect","actual_os_input":false}),
        )?;
        self.stop.store(false, Ordering::Release);
        let mut sink = RecordingSink {
            events: self.events.clone(),
            stop: self.stop.clone(),
            fail_move: self.fail_move.clone(),
            fail_key_release: self.fail_key_release.clone(),
        };
        let (address, identity) = (self.address.clone(), self.fixture.0.join("identities/mac"));
        self.worker = Some(thread::spawn(move || {
            let result = session::receive(&address, &identity, &mut sink, writer.log.clone(), 3.0);
            writer.finish()?;
            result
        }));
        Ok(())
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Wire {
    socket: TcpStream,
    secure: SecureChannel,
    input: Vec<u8>,
    lease: u64,
    seq: u64,
}
impl Wire {
    fn new(mut socket: TcpStream, identity: &Identity) -> Result<Self> {
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(700)))?;
        socket.set_write_timeout(Some(Duration::from_millis(700)))?;
        let secure = security::handshake(&mut socket, identity, true)?;
        socket.set_read_timeout(Some(Duration::from_millis(5)))?;
        Ok(Self {
            socket,
            secure,
            input: vec![],
            lease: 0,
            seq: 0,
        })
    }
    fn read_leases(&mut self) -> Result<bool> {
        let mut bytes = [0; 4096];
        match self.socket.read(&mut bytes) {
            Ok(0) => return Ok(false),
            Ok(n) => self.input.extend_from_slice(&bytes[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        }
        while self.input.len() >= 2 {
            let size = u16::from_be_bytes(self.input[..2].try_into().unwrap()) as usize;
            ensure!(
                (16..=2048).contains(&size),
                "invalid encrypted test record length"
            );
            if self.input.len() < size + 2 {
                break;
            }
            let frame = Frame::decode(&self.secure.decrypt(&self.input[2..size + 2])?)?;
            ensure!(frame.kind == Kind::Lease, "receiver sent non-lease frame");
            self.lease = frame.lease;
            self.input.drain(..size + 2);
        }
        Ok(true)
    }
    fn fresh_lease(&mut self) -> Result<()> {
        let before = self.lease;
        let deadline = Instant::now() + Duration::from_millis(150);
        while self.lease == before {
            ensure!(
                self.read_leases()?,
                "receiver disconnected waiting for lease"
            );
            ensure!(Instant::now() < deadline, "receiver did not grant lease");
        }
        Ok(())
    }
    fn send(&mut self, mut frame: Frame) -> Result<()> {
        self.seq += 1;
        frame.seq = self.seq;
        if frame.lease == 0 {
            frame.lease = self.lease;
        }
        let encrypted = self.secure.encrypt(&frame.encode()?)?;
        let mut record = Vec::with_capacity(encrypted.len() + 2);
        record.extend_from_slice(&(encrypted.len() as u16).to_be_bytes());
        record.extend_from_slice(&encrypted);
        self.socket.write_all(&record)?;
        Ok(())
    }
    fn arm(&mut self) -> Result<()> {
        self.fresh_lease()?;
        self.send(Frame::new(Kind::Arm))
    }
    fn closed(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(700);
        while self.read_leases()? {
            ensure!(
                Instant::now() < deadline,
                "receiver did not close invalid session"
            );
        }
        Ok(())
    }
}
impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn key(usage: u16, down: bool) -> Frame {
    let mut frame = Frame::new(Kind::Key);
    frame.code = usage;
    frame.down = down;
    frame
}
fn button(button: Button, down: bool) -> Frame {
    let mut frame = Frame::new(Kind::Button);
    frame.code = button_code(button);
    frame.down = down;
    frame
}
fn movement(x: f64, y: f64) -> Frame {
    let mut frame = Frame::new(Kind::Move);
    frame.x = x;
    frame.y = y;
    frame
}
fn scroll(x: i32, y: i32) -> Frame {
    let mut frame = Frame::new(Kind::Scroll);
    frame.wheel_x = x;
    frame.wheel_y = y;
    frame
}

fn held_pair(harness: &Harness, wire: &mut Wire) -> Result<()> {
    wire.send(key(0xE0, true))?;
    wire.send(button(Button::Left, true))?;
    // Observe injection before disconnect/error: read + EOF in one pump may
    // deliberately discard buffered frames as part of fail-closed behavior.
    harness.until(|events| {
        events.contains(&Event::Key(0xE0, true))
            && events.contains(&Event::Button(Button::Left, true))
    })
}
fn both_released(events: &[Event]) -> bool {
    events.contains(&Event::Key(0xE0, false))
        && events.contains(&Event::Button(Button::Left, false))
}

struct OrderedSender {
    queue: Arc<Mutex<SourceInputQueue>>,
    stop: Arc<AtomicBool>,
    log: Log,
    worker: Option<JoinHandle<Result<()>>>,
}
impl OrderedSender {
    fn start(harness: &Harness) -> Result<Self> {
        let writer = LogWriter::new(
            &harness.fixture.0.join("ordered-sender.jsonl"),
            json!({"test":"ordered_input_transport","actual_os_input":false}),
        )?;
        let log = writer.log.clone();
        let mut input = SourceInputQueue::new();
        input
            .seed_physical_held([], [])
            .expect("empty physical seed");
        let queue = Arc::new(Mutex::new(input));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_queue, thread_stop) = (queue.clone(), stop.clone());
        let (host, bundle) = (
            harness.address.clone(),
            harness.fixture.0.join("identities/windows"),
        );
        let worker = thread::spawn(move || {
            let result = session::send_ordered(
                SendOptions {
                    host: &host,
                    bundle: &bundle,
                    seconds: 4.0,
                    max_hz: 1000,
                    synthetic: false,
                },
                thread_queue,
                thread_stop,
                writer.log.clone(),
            );
            writer.finish()?;
            result
        });
        Ok(Self {
            queue,
            stop,
            log,
            worker: Some(worker),
        })
    }
    fn until_connected(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(1200);
        while !self.queue.lock().unwrap().is_connected() {
            ensure!(Instant::now() < deadline, "ordered sender did not connect");
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn until_disconnected(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(700);
        while self.queue.lock().unwrap().is_connected() {
            ensure!(
                Instant::now() < deadline,
                "ordered sender did not observe disconnect"
            );
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .context("sender already joined")?
            .join()
            .expect("ordered sender panicked")
    }
}
impl Drop for OrderedSender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn emergency_while_held_releases_owned_inputs_and_terminates() -> Result<()> {
    let mut harness = Harness::start("emergency")?;
    let mut wire = harness.connect()?;
    wire.arm()?;
    held_pair(&harness, &mut wire)?;
    harness.stop.store(true, Ordering::Release);
    harness.until(both_released)?;
    wire.closed()?;
    harness.finish()?;
    assert_eq!(
        harness.observed().len(),
        4,
        "emergency must release each held input, with no new action"
    );
    let log = harness.log()?;
    assert!(
        log.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|entry| entry["event"] == "emergency_stop")
    );
    Ok(())
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn ordered_source_queue_drives_encrypted_receiver_without_os_capture() -> Result<()> {
    let mut harness = Harness::start("ordered-source")?;
    let mut sender = OrderedSender::start(&harness)?;
    sender.until_connected()?;
    {
        let mut queue = sender.queue.lock().unwrap();
        assert!(!queue.is_active(), "authentication must not arm input");
        assert_eq!(
            queue.move_by(500., 500., sender.log.ns()),
            CaptureOutcome::Ignored
        );
        assert!(queue.arm());
        let now = sender.log.ns();
        assert_eq!(queue.move_by(3., 4., now), CaptureOutcome::Queued);
        assert_eq!(
            queue.button(Button::Left, true, now),
            CaptureOutcome::Queued
        );
        assert_eq!(queue.move_by(5., 6., now), CaptureOutcome::Queued);
        assert_eq!(queue.key(0x04, true, now), CaptureOutcome::Queued);
        assert_eq!(queue.scroll(-30, 120, now), CaptureOutcome::Queued);
        assert_eq!(queue.key(0x04, false, now), CaptureOutcome::Queued);
        assert_eq!(
            queue.button(Button::Left, false, now),
            CaptureOutcome::Queued
        );
    }
    harness.until(|events| events.len() >= 7)?;
    assert_eq!(
        harness.observed(),
        vec![
            Event::Move(3., 4.),
            Event::Button(Button::Left, true),
            Event::Move(5., 6.),
            Event::Key(0x04, true),
            Event::Scroll(-30, 120),
            Event::Key(0x04, false),
            Event::Button(Button::Left, false),
        ]
    );
    // Hold another pair, wait for real receiver observation, then pause. This
    // makes the pause's remote cleanup observable without arbitrary sleeps.
    {
        let mut queue = sender.queue.lock().unwrap();
        let now = sender.log.ns();
        assert_eq!(queue.key(0xE0, true, now), CaptureOutcome::Queued);
        assert_eq!(
            queue.button(Button::Left, true, now),
            CaptureOutcome::Queued
        );
    }
    harness.until(|events| events.len() >= 9)?;
    {
        let mut queue = sender.queue.lock().unwrap();
        queue.move_by(9999., 9999., sender.log.ns());
        queue.pause();
    }
    harness.until(|events| events.len() >= 11)?;
    assert!(both_released(&harness.observed()[7..]));
    sender.until_connected()?;
    {
        let mut queue = sender.queue.lock().unwrap();
        assert!(!queue.is_active());
        // Continue physical capture while paused; old held inputs are not
        // carried into the next epoch and their late ups are ignored.
        let now = sender.log.ns();
        assert_eq!(queue.key(0xE0, false, now), CaptureOutcome::Ignored);
        assert_eq!(
            queue.button(Button::Left, false, now),
            CaptureOutcome::Ignored
        );
        assert!(queue.arm());
        assert_eq!(
            queue.move_by(2., 1., sender.log.ns()),
            CaptureOutcome::Queued
        );
    }
    harness.until(|events| events.len() >= 12)?;
    sender.finish()?;
    harness.finish()?;
    assert_eq!(
        harness.observed().len(),
        12,
        "paused movement or old actions replayed"
    );
    assert_eq!(harness.observed().last(), Some(&Event::Move(2., 1.)));
    Ok(())
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn ordered_sender_reconnect_requires_arm_and_does_not_replay_preheld_inputs() -> Result<()> {
    let mut harness = Harness::start("ordered-reconnect")?;
    let mut sender = OrderedSender::start(&harness)?;
    sender.until_connected()?;
    {
        let mut queue = sender.queue.lock().unwrap();
        assert!(queue.arm());
        let now = sender.log.ns();
        assert_eq!(queue.key(0xE0, true, now), CaptureOutcome::Queued);
        assert_eq!(
            queue.button(Button::Left, true, now),
            CaptureOutcome::Queued
        );
    }
    harness.until(|events| events.len() == 2)?;
    harness.stop.store(true, Ordering::Release);
    harness.until(both_released)?;
    harness.finish()?;
    sender.until_disconnected()?;
    harness.restart()?;
    sender.until_connected()?;
    {
        let mut queue = sender.queue.lock().unwrap();
        assert!(!queue.is_active(), "reconnect must not auto-arm");
        assert_eq!(queue.pending_len(), 0);
        assert_eq!(
            queue.move_by(8000., 9000., sender.log.ns()),
            CaptureOutcome::Ignored
        );
        assert!(queue.arm());
        let now = sender.log.ns();
        // The physical keys remain held across network reconnection. Repeating
        // their downs cannot recreate the old remote hold in this new epoch.
        assert_eq!(queue.key(0xE0, true, now), CaptureOutcome::Ignored);
        assert_eq!(
            queue.button(Button::Left, true, now),
            CaptureOutcome::Ignored
        );
        assert_eq!(queue.key(0xE0, false, now), CaptureOutcome::Ignored);
        assert_eq!(
            queue.button(Button::Left, false, now),
            CaptureOutcome::Ignored
        );
        assert_eq!(queue.key(0x04, true, now), CaptureOutcome::Queued);
        assert_eq!(queue.key(0x04, false, now), CaptureOutcome::Queued);
    }
    harness.until(|events| events.len() >= 6)?;
    sender.finish()?;
    harness.finish()?;
    let events = harness.observed();
    assert_eq!(events.len(), 6, "old input replayed after reconnect");
    assert_eq!(
        &events[4..],
        &[Event::Key(0x04, true), Event::Key(0x04, false)]
    );
    Ok(())
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn ordered_actions_pause_release_and_rearm_do_not_replay() -> Result<()> {
    let mut harness = Harness::start("ordered")?;
    let mut wire = harness.connect()?;
    wire.arm()?;
    held_pair(&harness, &mut wire)?;
    wire.send(movement(7., 9.))?;
    wire.send(scroll(-30, 120))?;
    wire.send(movement(10., 13.))?;
    harness.until(|events| events.len() >= 5)?;
    assert_eq!(
        harness.observed(),
        vec![
            Event::Key(0xE0, true),
            Event::Button(Button::Left, true),
            Event::Move(7., 9.),
            Event::Scroll(-30, 120),
            Event::Move(3., 4.)
        ]
    );
    wire.send(Frame::new(Kind::Pause))?;
    harness.until(both_released)?;
    let released = harness.observed();
    assert_eq!(
        released.len(),
        7,
        "pause must release each owned input exactly once"
    );
    wire.fresh_lease()?;
    wire.send(Frame::new(Kind::Arm))?;
    wire.send(movement(2., 3.))?;
    harness.until(|events| events.len() == 8)?;
    assert_eq!(harness.observed().last(), Some(&Event::Move(2., 3.)));
    wire.send(Frame::new(Kind::Stop))?;
    wire.closed()?;
    harness.finish()?;
    assert_eq!(
        harness.observed().len(),
        8,
        "rearm/stop must not replay old buttons or duplicate releases"
    );
    Ok(())
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn disconnect_releases_observed_held_key_and_button() -> Result<()> {
    let mut harness = Harness::start("disconnect")?;
    let mut wire = harness.connect()?;
    wire.arm()?;
    held_pair(&harness, &mut wire)?;
    drop(wire);
    harness.until(both_released)?;
    harness.finish()?;
    assert_eq!(harness.observed().len(), 4);
    Ok(())
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn adapter_failure_cleans_remaining_inputs_and_release_failure_is_terminal() -> Result<()> {
    for fail_release in [false, true] {
        let mut harness = Harness::start(if fail_release {
            "release-failure"
        } else {
            "adapter-failure"
        })?;
        let mut wire = harness.connect()?;
        wire.arm()?;
        held_pair(&harness, &mut wire)?;
        harness
            .fail_key_release
            .store(fail_release, Ordering::Release);
        if fail_release {
            wire.send(Frame::new(Kind::Pause))?;
        } else {
            harness.fail_move.store(true, Ordering::Release);
            wire.send(movement(1., 1.))?;
        }
        harness.until(both_released)?;
        wire.closed()?;
        let result = harness.finish();
        assert!(
            result.is_err(),
            "adapter/cleanup failure must terminate receiver, not silently relisten"
        );
        let events = harness.observed();
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == Event::Button(Button::Left, false))
                .count(),
            1
        );
        assert!(events.contains(&Event::Key(0xE0, false)));
        assert!(!events.iter().any(|e| matches!(e, Event::Move(_, _))));
    }
    Ok(())
}

#[test]
#[ignore = "opens encrypted localhost sockets; fake adapter only"]
fn invalid_lease_down_is_not_injected_and_stale_up_triggers_cleanup() -> Result<()> {
    let mut harness = Harness::start("invalid")?;
    let mut wire = harness.connect()?;
    wire.arm()?;
    let mut invalid = key(0x04, true);
    invalid.lease = u64::MAX;
    wire.send(invalid)?;
    wire.closed()?;
    assert!(
        harness.observed().is_empty(),
        "ungranted lease injected input"
    );
    drop(wire);
    let mut wire = harness.connect()?;
    wire.arm()?;
    held_pair(&harness, &mut wire)?;
    let mut stale = key(0xE0, false);
    stale.t0_ns = 0;
    stale.t1_ns = 30_000_001;
    wire.send(stale)?;
    wire.closed()?;
    harness.until(both_released)?;
    harness.finish()?;
    let log = harness.log()?;
    let errors: Vec<_> = log
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["event"] == "error")
        .collect();
    assert!(
        errors.len() >= 2,
        "invalid lease and stale input must both be recorded as session errors"
    );
    assert_eq!(
        harness.observed().len(),
        4,
        "stale Up plus cleanup must not duplicate release"
    );
    Ok(())
}
