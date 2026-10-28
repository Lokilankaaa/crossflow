//! Reusable, opt-in TCP pairing experiment. Never opens a listener, injects
//! input, or writes trust. Noise XX is NOT authenticated until bound SPAKE2 and
//! directional confirmation complete. Both callers must explicitly approve.
//!
//! One total deadline covers Noise, PAKE, local approval wait, Approval and Ready.
//! Network reads/writes poll cancellation in slices of at most 100 ms. Dropping
//! pending state closes the socket and erases its retained local static key.
//! The responder retains the invitation's in-flight slot through final Ready.
//!
//! This is NOT an atomic two-device commit: loss of a final Ready, a process
//! crash, or cancellation at the last boundary can leave one side with keys and
//! the other without them. Callers need a separate persistence/recovery policy.

use crate::invitation::{ApprovalHold, AttemptCheck, AttemptPermit, PendingApproval};
use crate::{AuthenticatedBinding, Binding, Pairing, Role};
use anyhow::{Context, Result, ensure};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"Crossflow/pairing-spike/noise-xx/v1";
const MAX_FRAME: usize = 1024;
const IO_SLICE: Duration = Duration::from_millis(100);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const CONTROL_VERSION: u8 = 1;

/// Begin a request using a locally supplied invitation code. This is not final
/// approval: explicitly call PendingSession::approve on BOTH endpoints later.
pub fn initiate(
    socket: TcpStream,
    code: &[u8],
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<PendingSession> {
    let mut wire = Wire::new(socket, timeout, cancel, None)?;
    ensure!(
        code.len() == crate::CODE_BYTES && code.iter().all(u8::is_ascii_digit),
        "Pairing code must contain exactly eight ASCII digits"
    );
    let (mut noise, binding, private) = exchange_noise(&mut wire, Role::Initiator)?;
    let pairing = Pairing::start(Role::Initiator, code, binding)?;
    encrypted_send(&mut wire, &mut noise, pairing.message())?;
    let peer_message = encrypted_receive(&mut wire, &mut noise)?;
    let confirmation = pairing.process(&peer_message)?;
    encrypted_send(&mut wire, &mut noise, confirmation.tag())?;
    let peer_tag = encrypted_receive(&mut wire, &mut noise)?;
    let authenticated = confirmation.verify_peer(&peer_tag)?;
    ensure!(
        *authenticated.binding() == binding,
        "Pairing binding mismatch"
    );
    wire.remaining()?;
    Ok(PendingSession {
        wire,
        noise,
        binding,
        private,
        role: Role::Initiator,
        approval: LocalApproval::Initiator(authenticated),
    })
}

/// The caller MUST reserve this permit before Noise work (normally immediately
/// after accepting a socket). Invitation checks remain active in all PAKE stages.
pub fn respond(
    socket: TcpStream,
    permit: AttemptPermit,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<PendingSession> {
    let mut wire = Wire::new(socket, timeout, cancel, Some(permit.attempt_check()))?;
    let (mut noise, binding, private) = exchange_noise(&mut wire, Role::Responder)?;
    let pairing = permit.start(binding)?;
    encrypted_send(&mut wire, &mut noise, pairing.message()?)?;
    let peer_message = encrypted_receive(&mut wire, &mut noise)?;
    let confirmation = pairing.process(&peer_message)?;
    encrypted_send(&mut wire, &mut noise, confirmation.tag()?)?;
    let peer_tag = encrypted_receive(&mut wire, &mut noise)?;
    let pending = confirmation.verify_peer(&peer_tag)?;
    ensure!(*pending.binding()? == binding, "Pairing binding mismatch");
    wire.remaining()?;
    Ok(PendingSession {
        wire,
        noise,
        binding,
        private,
        role: Role::Responder,
        approval: LocalApproval::Responder(pending),
    })
}

enum LocalApproval {
    Initiator(AuthenticatedBinding),
    Responder(PendingApproval),
}

/// Cryptographic authentication finished, but no enrollment keys are accessible.
/// No Debug/Clone/Serialize. Drop is rejection and shuts down the TCP connection.
pub struct PendingSession {
    wire: Wire,
    noise: snow::TransportState,
    binding: Binding,
    private: Zeroizing<[u8; 32]>,
    role: Role,
    approval: LocalApproval,
}

impl PendingSession {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    /// Explicit local approval followed by reciprocal Approval and Ready messages.
    /// No return value with keys exists on failure. Success is NOT bilateral
    /// durable persistence or proof that the peer received our final message.
    pub fn approve(self) -> Result<EnrolledKeys> {
        let Self {
            mut wire,
            mut noise,
            binding,
            private,
            role,
            approval,
        } = self;
        wire.remaining()?;
        match approval {
            LocalApproval::Initiator(authenticated) => {
                ensure!(
                    authenticated.role() == role && *authenticated.binding() == binding,
                    "Local authentication binding mismatch"
                );
            }
            LocalApproval::Responder(pending) => {
                let (approved, hold) = pending.approve_held()?;
                ensure!(
                    approved.role() == role && *approved.binding() == binding,
                    "Local approval binding mismatch"
                );
                wire.hold_approval(hold);
            }
        }
        for opcode in [Control::Approval, Control::Ready] {
            wire.remaining()?;
            encrypted_send(&mut wire, &mut noise, &control_frame(opcode, role))?;
            let peer = encrypted_receive(&mut wire, &mut noise)?;
            validate_control(&peer, opcode, role.peer())?;
        }
        wire.remaining()?;
        // Wire Drop closes the enrollment channel and releases the responder's
        // held invitation. No live session or input transport is handed out.
        Ok(EnrolledKeys {
            binding,
            private,
            role,
        })
    }
}

/// Keys become accessible only after the explicitly approved transport completes.
/// No Debug/Serialize/Clone. The caller owns secure persistence and erasure of
/// copies; this experiment never writes either public metadata or private keys.
pub struct EnrolledKeys {
    binding: Binding,
    private: Zeroizing<[u8; 32]>,
    role: Role,
}

impl EnrolledKeys {
    pub fn binding(&self) -> &Binding {
        &self.binding
    }

    pub fn into_parts(self) -> (Role, [u8; 16], Zeroizing<[u8; 32]>, [u8; 32]) {
        let pair_id = self.binding.handshake_hash[..16]
            .try_into()
            .expect("fixed hash length");
        let peer = match self.role {
            Role::Initiator => self.binding.responder_static,
            Role::Responder => self.binding.initiator_static,
        };
        (self.role, pair_id, self.private, peer)
    }
}

struct Wire {
    socket: TcpStream,
    deadline: Instant,
    cancel: Arc<AtomicBool>,
    attempt_check: Option<AttemptCheck>,
    approval_hold: Option<ApprovalHold>,
}

impl Wire {
    fn new(
        socket: TcpStream,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
        attempt_check: Option<AttemptCheck>,
    ) -> Result<Self> {
        // Own the socket before any fallible validation, so every rejection
        // shuts it down even when a caller retained a duplicated socket handle.
        let mut wire = Self {
            socket,
            deadline: Instant::now(),
            cancel,
            attempt_check,
            approval_hold: None,
        };
        ensure!(
            !timeout.is_zero() && timeout <= MAX_TIMEOUT,
            "Pairing timeout must be >0 and <=120 seconds"
        );
        wire.deadline = wire
            .deadline
            .checked_add(timeout)
            .context("Pairing deadline overflow")?;
        wire.remaining()?;
        // Accepted sockets can inherit a nonblocking listener's mode on Windows.
        wire.socket
            .set_nonblocking(false)
            .context("Configure pairing socket blocking mode")?;
        wire.socket
            .set_nodelay(true)
            .context("Enable pairing TCP_NODELAY")?;
        Ok(wire)
    }

    fn hold_approval(&mut self, hold: ApprovalHold) {
        // approve_held consumed active invitation state. Replace its observer
        // before the next I/O check; validating both would reject valid approval.
        self.attempt_check = None;
        self.approval_hold = Some(hold);
    }

    fn remaining(&self) -> io::Result<Duration> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                ErrorKind::ConnectionAborted,
                "Pairing cancelled",
            ));
        }
        if let Some(attempt) = &self.attempt_check {
            attempt.check().map_err(|_| {
                io::Error::new(
                    ErrorKind::ConnectionAborted,
                    "Pairing invitation revoked or expired",
                )
            })?;
        }
        if let Some(hold) = &self.approval_hold {
            hold.check().map_err(|_| {
                io::Error::new(
                    ErrorKind::ConnectionAborted,
                    "Pairing approval revoked or expired",
                )
            })?;
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| io::Error::new(ErrorKind::TimedOut, "Total pairing deadline exceeded"))
    }

    fn send(&mut self, payload: &[u8]) -> Result<()> {
        ensure!(
            !payload.is_empty() && payload.len() <= MAX_FRAME,
            "Invalid pairing frame length"
        );
        let mut frame = Vec::with_capacity(payload.len() + 2);
        frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(payload);
        self.write_all(&frame)
            .context("Write pairing frame (header and body)")?;
        self.remaining()?;
        Ok(())
    }

    fn receive(&mut self) -> Result<Vec<u8>> {
        let mut header = [0; 2];
        self.read_exact(&mut header)
            .context("Read pairing frame header")?;
        let length = u16::from_be_bytes(header) as usize;
        ensure!(
            length > 0 && length <= MAX_FRAME,
            "Invalid pairing frame length"
        );
        let mut payload = vec![0; length];
        self.read_exact(&mut payload)
            .context("Read pairing frame body")?;
        self.remaining()?;
        Ok(payload)
    }
}

impl Read for Wire {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        loop {
            self.socket
                .set_read_timeout(Some(self.remaining()?.min(IO_SLICE)))?;
            match self.socket.read(output) {
                Ok(count) => {
                    self.remaining()?;
                    return Ok(count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
}

impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.socket
                .set_write_timeout(Some(self.remaining()?.min(IO_SLICE)))?;
            match self.socket.write(bytes) {
                Ok(count) => {
                    self.remaining()?;
                    return Ok(count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining().map(|_| ())
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

fn exchange_noise(
    wire: &mut Wire,
    role: Role,
) -> Result<(snow::TransportState, Binding, Zeroizing<[u8; 32]>)> {
    wire.remaining()?;
    let generated = snow::Builder::new(PATTERN.parse()?).generate_keypair()?;
    let generated_private = Zeroizing::new(generated.private);
    ensure!(
        generated_private.len() == 32,
        "Invalid generated static key length"
    );
    let mut private = Zeroizing::new([0u8; 32]);
    private.copy_from_slice(&generated_private);
    drop(generated_private);
    let own: [u8; 32] = generated
        .public
        .as_slice()
        .try_into()
        .context("Invalid generated public key length")?;
    let builder = snow::Builder::new(PATTERN.parse()?)
        .local_private_key(private.as_slice())?
        .prologue(PROLOGUE)?;
    let initiator = role == Role::Initiator;
    let mut state = if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    };
    let mut buffer = Zeroizing::new([0; MAX_FRAME]);
    for index in 0..3 {
        wire.remaining()?;
        if (index % 2 == 0) == initiator {
            let length = state.write_message(&[], buffer.as_mut_slice())?;
            wire.send(&buffer[..length])?;
        } else {
            let frame = wire.receive()?;
            ensure!(
                state.read_message(&frame, buffer.as_mut_slice())? == 0,
                "Noise handshake payload forbidden"
            );
        }
    }
    ensure!(state.is_handshake_finished(), "Noise XX incomplete");
    let peer: [u8; 32] = state
        .get_remote_static()
        .context("Missing Noise peer static key")?
        .try_into()?;
    let binding = Binding {
        handshake_hash: state.get_handshake_hash().try_into()?,
        initiator_static: if initiator { own } else { peer },
        responder_static: if initiator { peer } else { own },
    };
    binding.validate()?;
    wire.remaining()?;
    Ok((state.into_transport_mode()?, binding, private))
}

fn encrypted_send(wire: &mut Wire, noise: &mut snow::TransportState, payload: &[u8]) -> Result<()> {
    wire.remaining()?;
    ensure!(
        payload.len() <= MAX_FRAME - 16,
        "Pairing plaintext exceeds frame limit"
    );
    let mut buffer = [0; MAX_FRAME];
    let length = noise.write_message(payload, &mut buffer)?;
    wire.send(&buffer[..length])
}

fn encrypted_receive(
    wire: &mut Wire,
    noise: &mut snow::TransportState,
) -> Result<Zeroizing<Vec<u8>>> {
    let frame = wire.receive()?;
    let mut plaintext = Zeroizing::new(vec![0; MAX_FRAME]);
    let length = noise.read_message(&frame, &mut plaintext)?;
    plaintext.truncate(length);
    wire.remaining()?;
    Ok(plaintext)
}

#[derive(Clone, Copy)]
enum Control {
    Approval = 1,
    Ready = 2,
}

fn control_frame(opcode: Control, role: Role) -> [u8; 3] {
    [CONTROL_VERSION, opcode as u8, role.byte()]
}

fn validate_control(payload: &[u8], opcode: Control, role: Role) -> Result<()> {
    ensure!(
        payload == control_frame(opcode, role),
        "Unexpected pairing approval stage, role or version"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_require_exact_phase_role_version_and_length() {
        assert!(validate_control(&[1, 1, 1], Control::Approval, Role::Responder).is_ok());
        assert!(validate_control(&[1, 2, 0], Control::Ready, Role::Initiator).is_ok());
        for invalid in [
            &[][..],
            &[1, 1][..],
            &[1, 1, 1, 0][..],
            &[2, 1, 1][..],
            &[1, 2, 1][..],
            &[1, 1, 0][..],
            &[1, 1, 2][..],
        ] {
            assert!(validate_control(invalid, Control::Approval, Role::Responder).is_err());
        }
    }

    #[test]
    fn public_handoff_types_are_send_without_unsafe_overrides() {
        fn send<T: Send>() {}
        send::<PendingSession>();
        send::<EnrolledKeys>();
    }
}
