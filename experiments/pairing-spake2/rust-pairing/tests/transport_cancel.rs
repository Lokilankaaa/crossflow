//! Opt-in localhost regression: invitation cancellation, NOT the separate
//! transport AtomicBool, must interrupt Noise and PAKE waits and release busy.
//! One serial test uses four reservations total. No trust files or input APIs.

// The shared fixture also defines encrypted_send, unused by a silent peer.
#[allow(dead_code)]
#[path = "support/pairing_wire.rs"]
mod pairing_wire;

use anyhow::{Context, Result, ensure};
use crossflow_pairing_spike::{invitation::InvitationManager, transport};
use pairing_wire::{Wire, encrypted_receive, exchange_noise};
use std::{
    io::{ErrorKind, Read},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Worker {
    socket: TcpStream,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Worker {
    fn finish(&mut self, timeout: Duration) -> Result<Result<()>> {
        let deadline = Instant::now() + timeout;
        let worker = self.thread.as_ref().context("Worker already joined")?;
        while !worker.is_finished() {
            ensure!(
                Instant::now() < deadline,
                "Invitation cancellation did not stop transport promptly"
            );
            thread::sleep(Duration::from_millis(2));
        }
        self.thread
            .take()
            .context("Missing worker")?
            .join()
            .map_err(|_| anyhow::anyhow!("Transport cancellation worker panicked"))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Assertion/error cleanup only. The normal measured cancellation path
        // below does not touch either socket or the transport cancel AtomicBool.
        let _ = self.socket.shutdown(Shutdown::Both);
        if self.thread.is_some() {
            let _ = self.finish(Duration::from_secs(2));
        }
    }
}

fn connected_sockets() -> Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let client = TcpStream::connect_timeout(&listener.local_addr()?, Duration::from_secs(1))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((server, _)) => return Ok((client, server)),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                ensure!(Instant::now() < deadline, "Fixture accept timed out");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn cancellation_case(after_noise: bool) -> Result<()> {
    let manager = InvitationManager::new();
    let _display_code = manager.replace()?;
    let permit = manager.reserve()?;
    let (mut client, server) = connected_sockets()?;
    let retained_server = server.try_clone()?;
    let external_cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = external_cancel.clone();
    let (started, ready) = mpsc::sync_channel(1);
    let thread = thread::spawn(move || {
        started.send(()).context("Fixture ready receiver closed")?;
        transport::respond(server, permit, Duration::from_secs(4), thread_cancel).map(|_| ())
    });
    let mut worker = Worker {
        socket: retained_server,
        thread: Some(thread),
    };
    ready
        .recv_timeout(Duration::from_secs(1))
        .context("Fixture worker did not start")?;

    if after_noise {
        let mut wire = Wire::new(client.try_clone()?, Duration::from_secs(3))?;
        let (mut noise, _) = exchange_noise(&mut wire, true)?;
        let message = encrypted_receive(&mut wire, &mut noise)?;
        ensure!(
            message.len() == 32,
            "Responder did not reach the PAKE message stage"
        );
        // Never send the initiator PAKE message. The real responder now waits
        // with its InvitationPairing ticket, not the original AttemptPermit.
    } else {
        // Leave the first Noise message entirely absent. This short delay gives
        // the worker time to enter the read, but correctness does not rely on it:
        // a cancellation before the first read must also be observed immediately.
        thread::sleep(Duration::from_millis(150));
    }

    let cancelled_at = Instant::now();
    manager.cancel();
    let result = worker.finish(Duration::from_secs(1))?;
    ensure!(
        cancelled_at.elapsed() < Duration::from_secs(1),
        "Invitation cancellation exceeded one second"
    );
    let error = result
        .err()
        .context("Cancelled transport unexpectedly returned a pending session")?;
    ensure!(
        format!("{error:#}").contains("Pairing invitation revoked or expired"),
        "Transport did not report invitation revocation"
    );
    ensure!(
        !external_cancel.load(Ordering::Acquire),
        "External transport cancellation was changed"
    );

    client.set_read_timeout(Some(Duration::from_millis(500)))?;
    let mut byte = [0; 1];
    let closed = match client.read(&mut byte) {
        Ok(0) => true,
        Err(error) => matches!(
            error.kind(),
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::UnexpectedEof
        ),
        _ => false,
    };
    ensure!(closed, "Cancelled responder did not close its socket");

    // This second manager shares the production process gate. A fresh reserve
    // proves the cancelled attempt's actual Ticket was dropped, not just hidden.
    let next_manager = InvitationManager::new();
    let _next_display = next_manager.replace()?;
    drop(next_manager.reserve()?);
    Ok(())
}

#[test]
#[ignore = "explicit localhost Noise/PAKE waits; four real process-budget reservations; no trust or input"]
fn manager_cancel_interrupts_noise_and_pake_waits_without_external_cancel() -> Result<()> {
    cancellation_case(false)?;
    cancellation_case(true)
}
