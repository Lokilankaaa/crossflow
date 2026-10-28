//! Shared bounded wire fixture for actual localhost pairing experiments.
use anyhow::{Context, Result, ensure};
use crossflow_pairing_spike::Binding;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;
const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const MAX_FRAME: usize = 1024;
const PROLOGUE: &[u8] = b"Crossflow/pairing-spike/noise-xx/v1";

/// One total budget, including partial reads, the three Noise messages, PAKE,
/// and confirmations. A slow peer cannot reset the deadline per byte/frame.
pub(crate) struct Wire {
    socket: TcpStream,
    deadline: Instant,
}
impl Wire {
    pub(crate) fn new(socket: TcpStream, timeout: Duration) -> Result<Self> {
        // Windows accepted sockets inherit the nonblocking listener mode.
        socket.set_nonblocking(false)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            deadline: Instant::now() + timeout,
        })
    }
    pub(crate) fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "Pairing attempt expired")
            })
    }
    pub(crate) fn send(&mut self, payload: &[u8]) -> Result<()> {
        ensure!(
            !payload.is_empty() && payload.len() <= MAX_FRAME,
            "Invalid frame size"
        );
        let mut bytes = Vec::with_capacity(2 + payload.len());
        bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        self.write_all(&bytes).context("Send pairing frame")
    }
    pub(crate) fn receive(&mut self) -> Result<Vec<u8>> {
        let mut header = [0; 2];
        self.read_exact(&mut header)
            .context("Read pairing header")?;
        let length = u16::from_be_bytes(header) as usize;
        ensure!(length > 0 && length <= MAX_FRAME, "Invalid frame size");
        let mut payload = vec![0; length];
        self.read_exact(&mut payload)
            .context("Read pairing payload")?;
        Ok(payload)
    }
}
impl Read for Wire {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(bytes)
    }
}
impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.remaining()?;
        Ok(())
    }
}

pub(crate) fn exchange_noise(
    wire: &mut Wire,
    initiator: bool,
) -> Result<(snow::TransportState, Binding)> {
    let keypair = snow::Builder::new(PATTERN.parse()?).generate_keypair()?;
    let private = Zeroizing::new(keypair.private);
    let own_public: [u8; 32] = keypair.public.as_slice().try_into()?;
    let builder = snow::Builder::new(PATTERN.parse()?)
        .local_private_key(&private)?
        .prologue(PROLOGUE)?;
    let mut state = if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    };
    let mut buffer = [0; MAX_FRAME];
    // XX: initiator -> responder -> initiator. Handshake payloads forbidden.
    for index in 0..3 {
        if (index % 2 == 0) == initiator {
            let length = state.write_message(&[], &mut buffer)?;
            wire.send(&buffer[..length])?;
        } else {
            let frame = wire.receive()?;
            ensure!(
                state.read_message(&frame, &mut buffer)? == 0,
                "Unexpected Noise payload"
            );
        }
    }
    ensure!(state.is_handshake_finished(), "Noise XX incomplete");
    let remote: [u8; 32] = state
        .get_remote_static()
        .context("Missing remote static key")?
        .try_into()?;
    let binding = Binding {
        handshake_hash: state.get_handshake_hash().try_into()?,
        initiator_static: if initiator { own_public } else { remote },
        responder_static: if initiator { remote } else { own_public },
    };
    Ok((state.into_transport_mode()?, binding))
}

pub(crate) fn encrypted_send(
    wire: &mut Wire,
    state: &mut snow::TransportState,
    payload: &[u8],
) -> Result<()> {
    let mut encrypted = [0; MAX_FRAME];
    let length = state.write_message(payload, &mut encrypted)?;
    wire.send(&encrypted[..length])
}
pub(crate) fn encrypted_receive(
    wire: &mut Wire,
    state: &mut snow::TransportState,
) -> Result<Zeroizing<Vec<u8>>> {
    let encrypted = wire.receive()?;
    let mut plaintext = Zeroizing::new(vec![0; MAX_FRAME]);
    let length = state.read_message(&encrypted, &mut plaintext)?;
    plaintext.truncate(length);
    Ok(plaintext)
}
