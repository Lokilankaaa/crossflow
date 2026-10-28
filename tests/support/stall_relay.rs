//! Local TCP byte-stream stall fixture, not a packet loss or network emulator.
//! No decoding, record boundaries, external interfaces or OS network changes.
//! A stall preserves existing sockets and buffers but does no accept/read/write.
//! EOF/error discards both buffered directions and closes that connection.

use anyhow::{Context, Result, bail, ensure};
use std::{
    io::{self, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const BUFFER_LIMIT: usize = 64 * 1024;
const LOOP_DELAY: Duration = Duration::from_millis(2);
const CONNECT_TIMEOUT: Duration = Duration::from_millis(25);
const FINISH_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Default)]
pub struct RelayStats {
    /// Front-end TCP connections accepted, including failed backend connects.
    pub accepted: u64,
    /// Sum of bytes successfully written to either peer, never packets.
    pub forwarded_bytes: u64,
    /// Successful bytes read from the front-end/client socket.
    pub read_front_bytes: u64,
    /// Successful bytes read from the backend socket.
    pub read_back_bytes: u64,
    pub forwarded_to_back_bytes: u64,
    pub forwarded_to_front_bytes: u64,
    /// Peak combined unforwarded bytes in both directions, at most 128 KiB.
    pub max_buffered_bytes: usize,
    /// Number of distinct pause generations actually observed by the pump.
    pub stalled_cycles: u64,
}

#[derive(Default)]
struct Shared {
    stop: AtomicBool,
    // Low bit is stalled; upper bits distinguish repeated pause/resume cycles.
    requested: AtomicU64,
    observed_stall: AtomicU64,
    accepted: AtomicU64,
    forwarded: AtomicU64,
    read_front: AtomicU64,
    read_back: AtomicU64,
    forwarded_to_back: AtomicU64,
    forwarded_to_front: AtomicU64,
    max_buffered: AtomicUsize,
    stalled_cycles: AtomicU64,
}
impl Shared {
    fn allow_io(&self) -> bool {
        !self.stop.load(Ordering::Acquire) && self.requested.load(Ordering::Acquire) & 1 == 0
    }
}

pub struct StallRelay {
    address: SocketAddr,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<Result<()>>>,
}
impl StallRelay {
    pub fn start(backend: SocketAddr) -> Result<Self> {
        ensure!(backend.ip().is_loopback(), "relay backend must be loopback");
        ensure!(backend.port() != 0, "relay backend needs a bound port");
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let shared = Arc::new(Shared::default());
        let state = shared.clone();
        let worker = thread::Builder::new()
            .name("test-stall-relay".into())
            .spawn(move || pump(listener, backend, &state))
            .context("spawn local stall relay")?;
        Ok(Self {
            address,
            shared,
            worker: Some(worker),
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn set_stalled(&self, stalled: bool) {
        let mut old = self.shared.requested.load(Ordering::Acquire);
        loop {
            let next = (old.wrapping_add(2) & !1) | u64::from(stalled);
            match self.shared.requested.compare_exchange_weak(
                old,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => old = current,
            }
        }
    }

    /// Acknowledges this exact pause request, not a previous pause generation.
    /// No further IO begins while that acknowledged request remains stalled.
    pub fn wait_stalled(&self, timeout: Duration) -> Result<()> {
        let request = self.shared.requested.load(Ordering::Acquire);
        ensure!(request & 1 != 0, "relay pause was not requested");
        let started = Instant::now();
        loop {
            ensure!(
                !self.shared.stop.load(Ordering::Acquire),
                "relay stopped before pause confirmation"
            );
            ensure!(
                self.shared.requested.load(Ordering::Acquire) == request,
                "relay pause request changed while waiting"
            );
            ensure!(
                self.worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()),
                "relay pump exited before pause confirmation"
            );
            if self.shared.observed_stall.load(Ordering::Acquire) == request {
                return Ok(());
            }
            ensure!(
                started.elapsed() < timeout,
                "relay pause confirmation timed out"
            );
            thread::sleep(LOOP_DELAY.min(timeout.saturating_sub(started.elapsed())));
        }
    }

    /// Independently sampled monotonic counters, not one atomic transaction.
    pub fn stats(&self) -> RelayStats {
        RelayStats {
            accepted: self.shared.accepted.load(Ordering::Relaxed),
            forwarded_bytes: self.shared.forwarded.load(Ordering::Relaxed),
            read_front_bytes: self.shared.read_front.load(Ordering::Relaxed),
            read_back_bytes: self.shared.read_back.load(Ordering::Relaxed),
            forwarded_to_back_bytes: self.shared.forwarded_to_back.load(Ordering::Relaxed),
            forwarded_to_front_bytes: self.shared.forwarded_to_front.load(Ordering::Relaxed),
            max_buffered_bytes: self.shared.max_buffered.load(Ordering::Relaxed),
            stalled_cycles: self.shared.stalled_cycles.load(Ordering::Relaxed),
        }
    }

    pub fn finish(&mut self) -> Result<()> {
        self.shared.stop.store(true, Ordering::Release);
        let started = Instant::now();
        while self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            ensure!(
                started.elapsed() < FINISH_TIMEOUT,
                "relay pump did not stop within deadline"
            );
            thread::sleep(LOOP_DELAY);
        }
        if let Some(worker) = self.worker.take() {
            // Joining is permitted only after is_finished, never on a live pump.
            return worker
                .join()
                .map_err(|_| anyhow::anyhow!("relay pump panicked"))?;
        }
        Ok(())
    }
}
impl Drop for StallRelay {
    fn drop(&mut self) {
        // If the scheduler cannot finish the pump within the bound, dropping
        // JoinHandle detaches it; stop remains latched. Explicit finish reports it.
        let _ = self.finish();
    }
}

struct Buffer {
    bytes: Box<[u8]>,
    start: usize,
    end: usize,
}
impl Buffer {
    fn new() -> Self {
        Self {
            bytes: vec![0; BUFFER_LIMIT].into_boxed_slice(),
            start: 0,
            end: 0,
        }
    }
    fn len(&self) -> usize {
        self.end - self.start
    }
    fn read_from(&mut self, source: &mut TcpStream) -> io::Result<usize> {
        if self.len() == BUFFER_LIMIT {
            return Ok(0);
        }
        if self.end == BUFFER_LIMIT {
            self.bytes.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        match source.read(&mut self.bytes[self.end..]) {
            Ok(0) => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(count) => {
                self.end += count;
                Ok(count)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(0)
            }
            Err(error) => Err(error),
        }
    }
    fn write_to(&mut self, destination: &mut TcpStream) -> io::Result<usize> {
        if self.len() == 0 {
            return Ok(0);
        }
        match destination.write(&self.bytes[self.start..self.end]) {
            Ok(0) => Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(count) => {
                self.start += count;
                if self.start == self.end {
                    self.start = 0;
                    self.end = 0;
                }
                Ok(count)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(0)
            }
            Err(error) => Err(error),
        }
    }
}

struct Connection {
    front: TcpStream,
    back: TcpStream,
    to_back: Buffer,
    to_front: Buffer,
}
impl Connection {
    fn open(front: TcpStream, backend: SocketAddr) -> io::Result<Self> {
        // std has no portable nonblocking connect constructor. This loopback
        // setup call alone is bounded to 25ms; all stream reads/writes are nonblocking.
        let back = TcpStream::connect_timeout(&backend, CONNECT_TIMEOUT)?;
        front.set_nonblocking(true)?;
        back.set_nonblocking(true)?;
        front.set_nodelay(true)?;
        back.set_nodelay(true)?;
        Ok(Self {
            front,
            back,
            to_back: Buffer::new(),
            to_front: Buffer::new(),
        })
    }
    fn buffered(&self) -> usize {
        self.to_back.len() + self.to_front.len()
    }
    fn step(&mut self, shared: &Shared) -> io::Result<()> {
        if !shared.allow_io() {
            return Ok(());
        }
        let count = self.to_back.read_from(&mut self.front)?;
        shared.read_front.fetch_add(count as u64, Ordering::Relaxed);
        shared
            .max_buffered
            .fetch_max(self.buffered(), Ordering::Relaxed);
        if !shared.allow_io() {
            return Ok(());
        }
        let count = self.to_front.read_from(&mut self.back)?;
        shared.read_back.fetch_add(count as u64, Ordering::Relaxed);
        shared
            .max_buffered
            .fetch_max(self.buffered(), Ordering::Relaxed);
        if !shared.allow_io() {
            return Ok(());
        }
        let count = self.to_back.write_to(&mut self.back)?;
        shared.forwarded.fetch_add(count as u64, Ordering::Relaxed);
        shared
            .forwarded_to_back
            .fetch_add(count as u64, Ordering::Relaxed);
        if !shared.allow_io() {
            return Ok(());
        }
        let count = self.to_front.write_to(&mut self.front)?;
        shared.forwarded.fetch_add(count as u64, Ordering::Relaxed);
        shared
            .forwarded_to_front
            .fetch_add(count as u64, Ordering::Relaxed);
        Ok(())
    }
}
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.front.shutdown(Shutdown::Both);
        let _ = self.back.shutdown(Shutdown::Both);
    }
}

fn pump(listener: TcpListener, backend: SocketAddr, shared: &Shared) -> Result<()> {
    let mut connection: Option<Connection> = None;
    let mut pending_front: Option<TcpStream> = None;
    loop {
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let requested = shared.requested.load(Ordering::Acquire);
        if requested & 1 != 0 {
            if shared.observed_stall.load(Ordering::Relaxed) != requested {
                shared.stalled_cycles.fetch_add(1, Ordering::Relaxed);
                // Publish confirmation only after all counters are settled.
                shared.observed_stall.store(requested, Ordering::Release);
            }
            thread::sleep(LOOP_DELAY);
            continue;
        }
        if connection.is_none() && pending_front.is_none() && shared.allow_io() {
            match listener.accept() {
                Ok((front, _)) => {
                    shared.accepted.fetch_add(1, Ordering::Relaxed);
                    pending_front = Some(front);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => bail!("relay listener failed: {error}"),
            }
        }
        if pending_front.is_some() && shared.allow_io() {
            // If pause raced accept, retain the accepted socket until resume.
            // A pause during the bounded connect is acknowledged only afterward.
            // A failed connect closes both halves; later accepts can retry.
            connection = Connection::open(pending_front.take().unwrap(), backend).ok();
        }
        if let Some(current) = connection.as_mut() {
            if current.step(shared).is_err() {
                connection = None;
            }
        }
        thread::sleep(LOOP_DELAY);
    }
}
