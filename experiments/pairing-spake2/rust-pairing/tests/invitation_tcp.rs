//! Explicit localhost invitation-gate experiments; not a production listener.
//! Random codes remain in local zeroizing memory, never logs or error text.
//! These checks neither persist trust nor start an input session. A local
//! cryptographic success is deliberately separate from explicit approval.

#[path = "support/pairing_wire.rs"]
mod pairing_wire;

use anyhow::{Context, Result, ensure};
use crossflow_pairing_spike::{Binding, Pairing, Role, invitation::InvitationManager};
use pairing_wire::{Wire, encrypted_receive, encrypted_send, exchange_noise};
use std::{
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(3);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(2);
const JOIN_TIMEOUT: Duration = Duration::from_secs(6);
// Public managers intentionally share one in-flight slot. Serialize fixtures,
// not the implementation, so the suite also works with Cargo's default threads.
static TEST_SERIAL: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy)]
enum Approval {
    ExplicitlyApprove,
    CancelBeforeApproval,
}

#[derive(Default)]
struct Progress {
    accepted: AtomicBool,
    noise_started: AtomicBool,
    cryptographically_verified: AtomicBool,
}

/// Own the test thread before any client operation that can fail. Cancellation
/// closes its socket and interrupts the nonblocking accept loop, including on
/// assertion unwind. Joins are only performed after is_finished; all socket I/O
/// also has the shared Wire's total attempt deadline.
struct Server {
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    worker: Option<JoinHandle<Result<Binding>>>,
}

impl Server {
    fn join(&mut self) -> Result<Binding> {
        let deadline = Instant::now() + JOIN_TIMEOUT;
        let worker = self.worker.as_ref().context("server already joined")?;
        while !worker.is_finished() {
            ensure!(
                Instant::now() < deadline,
                "pairing fixture server did not terminate"
            );
            thread::sleep(Duration::from_millis(2));
        }
        self.worker
            .take()
            .context("missing server thread")?
            .join()
            .map_err(|_| anyhow::anyhow!("pairing fixture server panicked"))?
    }

    fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(socket) = self
            .socket
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.cancel();
        if self.worker.is_some() {
            // No unbounded join if the platform violates its socket deadline.
            // Normal and assertion-failure paths reclaim the completed thread.
            let _ = self.join();
        }
    }
}

struct Exchange {
    client: Result<Binding>,
    server: Result<Binding>,
    progress: Arc<Progress>,
}

fn client_attempt(socket: TcpStream, code: &[u8]) -> Result<Binding> {
    let mut wire = Wire::new(socket, ATTEMPT_TIMEOUT)?;
    let (mut noise, binding) = exchange_noise(&mut wire, true)?;
    let pairing = Pairing::start(Role::Initiator, code, binding)?;
    encrypted_send(&mut wire, &mut noise, pairing.message())?;
    let peer = encrypted_receive(&mut wire, &mut noise)?;
    let confirmation = pairing.process(&peer)?;
    encrypted_send(&mut wire, &mut noise, confirmation.tag())?;
    let tag = encrypted_receive(&mut wire, &mut noise)?;
    let verified = confirmation.verify_peer(&tag)?;
    wire.remaining()?;
    ensure!(verified.role() == Role::Initiator, "incorrect client role");
    Ok(*verified.binding())
}

fn server_attempt(
    socket: TcpStream,
    manager: &InvitationManager,
    approval: Approval,
    progress: &Progress,
) -> Result<Binding> {
    // Reserve is deliberately before Wire/Noise: rejected invitations consume
    // no unauthenticated Noise exchange and cannot run additional PAKE guesses.
    let permit = manager.reserve()?;
    let mut wire = Wire::new(socket, ATTEMPT_TIMEOUT)?;
    progress.noise_started.store(true, Ordering::Release);
    let (mut noise, binding) = exchange_noise(&mut wire, false)?;
    let pairing = permit.start(binding)?;
    encrypted_send(&mut wire, &mut noise, pairing.message()?)?;
    let peer = encrypted_receive(&mut wire, &mut noise)?;
    let confirmation = pairing.process(&peer)?;
    encrypted_send(&mut wire, &mut noise, confirmation.tag()?)?;
    let tag = encrypted_receive(&mut wire, &mut noise)?;
    let pending = confirmation.verify_peer(&tag)?;
    wire.remaining()?;
    progress
        .cryptographically_verified
        .store(true, Ordering::Release);
    match approval {
        Approval::ExplicitlyApprove => {}
        Approval::CancelBeforeApproval => manager.cancel(),
    }
    // This explicit local action is required even after both directional MACs.
    // Cancellation must invalidate this exact old pending result.
    let approved = pending.approve()?;
    ensure!(
        approved.role() == Role::Responder,
        "incorrect invitation role"
    );
    Ok(*approved.binding())
}

fn exchange(manager: Arc<InvitationManager>, code: &[u8], approval: Approval) -> Result<Exchange> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let progress = Arc::new(Progress::default());
    let thread_progress = progress.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let socket = Arc::new(Mutex::new(None));
    let thread_socket = socket.clone();
    let worker = thread::spawn(move || -> Result<Binding> {
        let deadline = Instant::now() + ACCEPT_TIMEOUT;
        loop {
            ensure!(
                !thread_stop.load(Ordering::Acquire),
                "fixture listener cancelled"
            );
            match listener.accept() {
                Ok((stream, _)) => {
                    thread_progress.accepted.store(true, Ordering::Release);
                    *thread_socket.lock().unwrap() = Some(stream.try_clone()?);
                    // Wire::new restores blocking mode on Windows inherited
                    // accepted sockets and imposes a total read/write budget.
                    let result = server_attempt(stream, &manager, approval, &thread_progress);
                    // The cancellation clone must not keep a rejected socket
                    // alive after the gate returns (the client should see EOF).
                    thread_socket.lock().unwrap().take();
                    return result;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(Instant::now() < deadline, "fixture listener timed out");
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(error.into()),
            }
        }
    });
    let mut server = Server {
        stop,
        socket,
        worker: Some(worker),
    };
    let client = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(anyhow::Error::from)
        .and_then(|socket| client_attempt(socket, code));
    // Always finish the server BEFORE returning either endpoint's failure to
    // an assertion. Failed client connection also cancels a waiting accept.
    if !progress.accepted.load(Ordering::Acquire) && client.is_err() {
        server.cancel();
    }
    let server_result = server.join();
    Ok(Exchange {
        client,
        server: server_result,
        progress,
    })
}

fn assert_rejected_confirmation(result: Result<Binding>) {
    match result {
        Ok(_) => panic!("wrong pairing code was accepted"),
        Err(error) => assert_eq!(error.to_string(), "Peer confirmation rejected"),
    }
}

#[test]
#[ignore = "actual localhost Noise XX/SPAKE2; no trust persistence or input"]
fn random_code_requires_explicit_approval_and_cannot_be_reused() -> Result<()> {
    let _serial = TEST_SERIAL.lock().unwrap();
    let manager = Arc::new(InvitationManager::new());
    let code = manager.replace()?;
    let outcome = exchange(
        manager.clone(),
        code.as_bytes(),
        Approval::ExplicitlyApprove,
    )?;
    let client = outcome.client.context("correct-code client rejected")?;
    let server = outcome.server.context("explicit approval failed")?;
    ensure!(
        client == server,
        "approved endpoints disagreed on Noise binding"
    );
    ensure!(
        outcome
            .progress
            .cryptographically_verified
            .load(Ordering::Acquire),
        "missing cryptographic verification"
    );
    ensure!(
        manager.reserve().is_err(),
        "approved invitation was reusable"
    );
    // A real new TCP connection must also fail BEFORE any Noise work.
    let repeated = exchange(manager, code.as_bytes(), Approval::ExplicitlyApprove)?;
    ensure!(
        repeated.progress.accepted.load(Ordering::Acquire),
        "reuse did not reach TCP listener"
    );
    ensure!(
        !repeated.progress.noise_started.load(Ordering::Acquire),
        "reused invitation started Noise"
    );
    ensure!(
        repeated.client.is_err() && repeated.server.is_err(),
        "reused invitation accepted"
    );
    Ok(())
}

#[test]
#[ignore = "actual localhost repeated wrong-code attempts; no trust persistence or input"]
fn three_wrong_tcp_attempts_block_correct_fourth_before_noise() -> Result<()> {
    let _serial = TEST_SERIAL.lock().unwrap();
    let manager = Arc::new(InvitationManager::new());
    let code = manager.replace()?;
    let mut wrong = Zeroizing::new(*code.as_bytes());
    wrong[0] = if wrong[0] == b'9' { b'0' } else { wrong[0] + 1 };
    // The same manager/code survives three fresh TCP connections. A failed
    // guess cannot gain a new per-invitation budget by reconnecting.
    for _ in 0..3 {
        let outcome = exchange(manager.clone(), &wrong[..], Approval::ExplicitlyApprove)?;
        ensure!(
            outcome.progress.accepted.load(Ordering::Acquire)
                && outcome.progress.noise_started.load(Ordering::Acquire),
            "wrong-code attempt never reached PAKE"
        );
        assert_rejected_confirmation(outcome.client);
        assert_rejected_confirmation(outcome.server);
        ensure!(
            !outcome
                .progress
                .cryptographically_verified
                .load(Ordering::Acquire),
            "wrong code verified"
        );
    }
    let fourth = exchange(manager, code.as_bytes(), Approval::ExplicitlyApprove)?;
    ensure!(
        fourth.progress.accepted.load(Ordering::Acquire),
        "fourth attempt did not reconnect TCP"
    );
    ensure!(
        !fourth.progress.noise_started.load(Ordering::Acquire),
        "fourth attempt started Noise"
    );
    ensure!(fourth.client.is_err(), "fourth client was accepted");
    match fourth.server {
        Ok(_) => anyhow::bail!("fourth server attempt was accepted"),
        Err(error) => ensure!(
            error.to_string() == "Invitation attempt limit reached",
            "fourth attempt failed for the wrong reason"
        ),
    }
    Ok(())
}

#[test]
#[ignore = "actual localhost authentication then cancellation; no trust persistence or input"]
fn valid_crypto_then_cancel_invalidates_pending_approval() -> Result<()> {
    let _serial = TEST_SERIAL.lock().unwrap();
    let manager = Arc::new(InvitationManager::new());
    let code = manager.replace()?;
    let outcome = exchange(
        manager.clone(),
        code.as_bytes(),
        Approval::CancelBeforeApproval,
    )?;
    ensure!(
        outcome.client.is_ok(),
        "client crypto failed before cancellation"
    );
    ensure!(
        outcome
            .progress
            .cryptographically_verified
            .load(Ordering::Acquire),
        "server crypto failed before cancellation"
    );
    ensure!(
        outcome.server.is_err(),
        "cancelled pending result was approved"
    );
    ensure!(
        manager.reserve().is_err(),
        "cancelled invitation remained usable"
    );
    Ok(())
}
