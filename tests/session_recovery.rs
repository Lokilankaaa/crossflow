//! Explicit localhost integration checks. No platform input API is called.
//! Run with: cargo test --test session_recovery -- --ignored

use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    metrics::{Log, LogWriter},
    protocol::{Frame, Kind},
    security::{self, Identity, SecureChannel},
    session::{self, DrySink, MouseSink, SendOptions},
    state::{Motion, ReceiverState},
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

struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Result<Self> {
        let path = std::env::current_dir()?.join("runs").join(format!(
            "session-test-{label}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir_all(&path)?;
        security::create_pair(&path.join("identities"))?;
        Ok(Self(path))
    }
    fn identity(&self, role: &str) -> PathBuf {
        self.0.join("identities").join(role)
    }
    fn logger(&self, name: &str) -> Result<LogWriter> {
        LogWriter::new(
            &self.0.join(format!("{name}.jsonl")),
            json!({"test":"session_recovery","no_os_input":true}),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Only remove this test's freshly-created, uniquely-named directory.
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Join background session workers even when an assertion unwinds the test.
struct Worker {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<()>>>,
}
impl Worker {
    fn finish(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.handle
            .take()
            .unwrap()
            .join()
            .expect("session thread panicked")
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Test-side authenticated transport uses the public security and wire APIs.
/// Reads retain partial records across timeouts, including split size headers.
struct Wire {
    socket: TcpStream,
    secure: SecureChannel,
    input: Vec<u8>,
}
impl Wire {
    fn new(mut socket: TcpStream, identity: &Identity, initiator: bool) -> Result<Self> {
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(700)))?;
        socket.set_write_timeout(Some(Duration::from_millis(700)))?;
        let secure = security::handshake(&mut socket, identity, initiator)?;
        socket.set_read_timeout(Some(Duration::from_millis(5)))?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        Ok(Self {
            socket,
            secure,
            input: vec![],
        })
    }
    fn send(&mut self, frame: Frame) -> Result<()> {
        let encrypted = self.secure.encrypt(&frame.encode()?)?;
        let mut record = Vec::with_capacity(encrypted.len() + 2);
        record.extend_from_slice(&(encrypted.len() as u16).to_be_bytes());
        record.extend_from_slice(&encrypted);
        self.socket.write_all(&record)?;
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Vec<Frame>>> {
        let mut buffer = [0u8; 4096];
        match self.socket.read(&mut buffer) {
            Ok(0) => return Ok(None),
            Ok(n) => self.input.extend_from_slice(&buffer[..n]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error.into()),
        }
        let mut result = vec![];
        while self.input.len() >= 2 {
            let size = u16::from_be_bytes(self.input[..2].try_into().unwrap()) as usize;
            ensure!((16..=2048).contains(&size), "invalid test-peer record");
            if self.input.len() < size + 2 {
                break;
            }
            let plain = self.secure.decrypt(&self.input[2..size + 2])?;
            result.push(Frame::decode(&plain)?);
            self.input.drain(..size + 2);
        }
        Ok(Some(result))
    }
}
impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn accept(listener: &TcpListener, limit: Duration) -> Result<TcpStream> {
    let deadline = Instant::now() + limit;
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(error).context("timed out waiting for sender connection"),
        }
    }
}

struct TestReceiver {
    wire: Wire,
    state: ReceiverState,
    sink: DrySink,
    start: Instant,
    last_grant: Instant,
    token: u64,
    observed: Vec<Frame>,
}
impl TestReceiver {
    fn new(wire: Wire) -> Self {
        Self {
            wire,
            state: ReceiverState::new(150_000_000),
            sink: DrySink::default(),
            start: Instant::now(),
            last_grant: Instant::now() - Duration::from_secs(1),
            token: 0,
            observed: vec![],
        }
    }
    fn tick(&mut self) -> Result<()> {
        if self.last_grant.elapsed() >= Duration::from_millis(20) {
            self.token += 1;
            self.state
                .grant(self.token, self.start.elapsed().as_nanos() as u64);
            let mut frame = Frame::new(Kind::Lease);
            frame.lease = self.token;
            self.wire.send(frame)?;
            self.last_grant = Instant::now();
        }
        let frames = self
            .wire
            .receive()?
            .context("sender disconnected unexpectedly")?;
        for frame in frames {
            if let Some((x, y)) = self
                .state
                .apply(frame, self.start.elapsed().as_nanos() as u64)?
            {
                self.sink.move_by(x, y)?;
            }
            self.observed.push(frame);
        }
        Ok(())
    }
    fn until(&mut self, limit: Duration, predicate: impl Fn(&Self) -> bool) -> Result<()> {
        let deadline = Instant::now() + limit;
        while !predicate(self) {
            ensure!(Instant::now() < deadline, "receiver condition timed out");
            self.tick()?;
        }
        Ok(())
    }
    fn maintain(&mut self, duration: Duration) -> Result<()> {
        let end = Instant::now() + duration;
        while Instant::now() < end {
            self.tick()?;
        }
        Ok(())
    }
}

fn add(motion: &Arc<Mutex<Motion>>, log: &Log, x: f64, y: f64) {
    motion.lock().unwrap().add(x, y, log.ns());
}

#[test]
#[ignore = "opens localhost sockets; run explicitly"]
fn authenticated_reconnect_stays_paused_and_does_not_replay_pending_motion() -> Result<()> {
    let fixture = Fixture::new("reconnect")?;
    let writer = fixture.logger("sender")?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?.to_string();
    let identity = Identity::load(&fixture.identity("mac"))?;
    let motion = Arc::new(Mutex::new(Motion::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (thread_motion, thread_stop, thread_log, bundle) = (
        motion.clone(),
        stop.clone(),
        writer.log.clone(),
        fixture.identity("windows"),
    );
    let worker = Worker {
        stop,
        handle: Some(thread::spawn(move || {
            session::send(
                SendOptions {
                    host: &address,
                    bundle: &bundle,
                    seconds: 3.5,
                    max_hz: 1000,
                    synthetic: false,
                },
                thread_motion,
                thread_stop,
                thread_log,
            )
        })),
    };

    let mut first = TestReceiver::new(Wire::new(
        accept(&listener, Duration::from_secs(1))?,
        &identity,
        false,
    )?);
    first.until(Duration::from_millis(300), |peer| {
        peer.observed.iter().any(|f| f.kind == Kind::Pause)
    })?;
    assert!(
        !motion.lock().unwrap().active,
        "new connection must start paused"
    );
    assert!(motion.lock().unwrap().toggle());
    first.until(Duration::from_millis(300), |peer| peer.state.active)?;
    add(&motion, &writer.log, 10.0, -4.0);
    first.until(Duration::from_millis(300), |peer| peer.sink.moves > 0)?;
    assert_eq!((first.sink.x, first.sink.y), (10.0, -4.0));

    {
        // Keep capture state locked until the first socket is closed. This
        // delta is accumulated before disconnect but cannot be snapshotted into
        // a fresh frame by send(). It must become only the next ARM baseline.
        let mut state = motion.lock().unwrap();
        state.add(5000.0, 6000.0, writer.log.ns());
        first.wire.socket.shutdown(Shutdown::Both)?;
    }
    drop(first);

    let mut second = TestReceiver::new(Wire::new(
        accept(&listener, Duration::from_millis(1200))?,
        &identity,
        false,
    )?);
    second.until(Duration::from_millis(300), |peer| {
        peer.observed.iter().any(|f| f.kind == Kind::Pause)
    })?;
    {
        let state = motion.lock().unwrap();
        assert!(state.connected);
        assert!(
            !state.active,
            "authenticated reconnect must not re-arm input"
        );
    }
    add(&motion, &writer.log, 123.0, 456.0); // Paused physical activity is ignored.
    second.maintain(Duration::from_millis(180))?;
    assert_eq!(second.sink.moves, 0, "old or paused movement was replayed");
    assert!(
        !second
            .observed
            .iter()
            .any(|f| matches!(f.kind, Kind::Arm | Kind::Move))
    );

    assert!(motion.lock().unwrap().toggle());
    second.until(Duration::from_millis(300), |peer| peer.state.active)?;
    let arm = second
        .observed
        .iter()
        .find(|f| f.kind == Kind::Arm)
        .unwrap();
    assert_eq!((arm.x, arm.y), (5010.0, 5996.0));
    assert_eq!(second.sink.moves, 0, "ARM must not inject its baseline");
    add(&motion, &writer.log, 3.0, -2.0);
    second.until(Duration::from_millis(300), |peer| peer.sink.moves > 0)?;
    assert_eq!((second.sink.x, second.sink.y), (3.0, -2.0));
    worker.finish()?;
    writer.finish()?;
    Ok(())
}

struct SharedDrySink {
    sink: Arc<Mutex<DrySink>>,
    stop: Arc<AtomicBool>,
}
impl MouseSink for SharedDrySink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        self.sink.lock().unwrap().move_by(x, y)
    }
    fn emergency_pressed(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
}

#[test]
#[ignore = "opens localhost sockets; run explicitly"]
fn receiver_rejects_expired_lease_without_applying_its_movement() -> Result<()> {
    let fixture = Fixture::new("expired-lease")?;
    let writer = fixture.logger("receiver")?;
    // Public receive() owns bind, so reserve and release one ephemeral port.
    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let address = reservation.local_addr()?.to_string();
    drop(reservation);
    let sink = Arc::new(Mutex::new(DrySink::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let (thread_sink, thread_stop, thread_log, bundle, thread_address) = (
        sink.clone(),
        stop.clone(),
        writer.log.clone(),
        fixture.identity("mac"),
        address.clone(),
    );
    let worker = Worker {
        stop,
        handle: Some(thread::spawn(move || {
            let mut sink = SharedDrySink {
                sink: thread_sink,
                stop: thread_stop,
            };
            session::receive(&thread_address, &bundle, &mut sink, thread_log, 2.5)
        })),
    };
    let connect_deadline = Instant::now() + Duration::from_millis(700);
    let socket = loop {
        match TcpStream::connect(&address) {
            Ok(socket) => break socket,
            Err(error) if Instant::now() < connect_deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error).context("receiver did not start"),
        }
    };
    let identity = Identity::load(&fixture.identity("windows"))?;
    let mut wire = Wire::new(socket, &identity, true)?;
    let mut lease = 0;
    let lease_deadline = Instant::now() + Duration::from_millis(300);
    while lease == 0 {
        ensure!(Instant::now() < lease_deadline, "no receiver lease");
        for frame in wire.receive()?.context("receiver closed before lease")? {
            assert_eq!(frame.kind, Kind::Lease);
            lease = frame.lease;
        }
    }
    let old_lease = lease;
    let old_received_at = Instant::now();
    let mut seq = 1;
    let mut arm = Frame::new(Kind::Arm);
    arm.seq = seq;
    arm.lease = lease;
    wire.send(arm)?;
    seq += 1;
    let mut initial = Frame::new(Kind::Move);
    initial.seq = seq;
    initial.lease = lease;
    initial.x = 1.0;
    wire.send(initial)?;
    let mut heartbeat_at = Instant::now();
    while old_received_at.elapsed() < Duration::from_millis(200) {
        for frame in wire
            .receive()?
            .context("receiver closed while refreshing lease")?
        {
            assert_eq!(frame.kind, Kind::Lease);
            lease = frame.lease;
        }
        if heartbeat_at.elapsed() >= Duration::from_millis(20) {
            seq += 1;
            let mut heartbeat = Frame::new(Kind::Heartbeat);
            heartbeat.seq = seq;
            heartbeat.lease = lease;
            wire.send(heartbeat)?;
            heartbeat_at = Instant::now();
        }
    }
    assert!(
        lease > old_lease,
        "test must refresh leases while retaining the stale token"
    );
    assert_eq!(
        sink.lock().unwrap().x,
        1.0,
        "initial authorized movement was not applied"
    );
    seq += 1;
    let mut expired = Frame::new(Kind::Move);
    expired.seq = seq;
    expired.lease = old_lease;
    expired.x = 1000.0;
    wire.send(expired)?;
    let reject_deadline = Instant::now() + Duration::from_millis(300);
    loop {
        if wire.receive()?.is_none() {
            break;
        }
        if Instant::now() >= reject_deadline {
            bail!("receiver did not close expired lease session");
        }
    }
    {
        let state = sink.lock().unwrap();
        assert_eq!(
            (state.x, state.y),
            (1.0, 0.0),
            "expired input affected the dry sink"
        );
    }
    worker.finish()?;
    writer.finish()?;
    Ok(())
}
