//! Opt-in localhost experiment, not a production enrollment listener.
//! Noise XX is deliberately treated as unauthenticated until the PAKE and
//! directional confirmations finish. No trusted device is saved and no input
//! channel is opened, even when the experiment succeeds.
use anyhow::{Context, Result, ensure};
use crossflow_pairing_spike::{Binding, Pairing, Role};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const MAX_FRAME: usize = 1024;
const PROLOGUE: &[u8] = b"Crossflow/pairing-spike/noise-xx/v1";

/// One total budget, including partial reads, the three Noise messages, PAKE,
/// and confirmations. A slow peer cannot reset the deadline per byte/frame.
struct Wire {
    socket: TcpStream,
    deadline: Instant,
}
impl Wire {
    fn new(socket: TcpStream, timeout: Duration) -> Result<Self> {
        // Windows accepted sockets inherit the nonblocking listener mode.
        socket.set_nonblocking(false)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            deadline: Instant::now() + timeout,
        })
    }
    fn remaining(&self) -> std::io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "Pairing attempt expired")
            })
    }
    fn send(&mut self, payload: &[u8]) -> Result<()> {
        ensure!(
            !payload.is_empty() && payload.len() <= MAX_FRAME,
            "Invalid frame size"
        );
        let mut bytes = Vec::with_capacity(2 + payload.len());
        bytes.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        self.write_all(&bytes).context("Send pairing frame")
    }
    fn receive(&mut self) -> Result<Vec<u8>> {
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

fn exchange_noise(wire: &mut Wire, initiator: bool) -> Result<(snow::TransportState, Binding)> {
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

fn encrypted_send(wire: &mut Wire, state: &mut snow::TransportState, payload: &[u8]) -> Result<()> {
    let mut encrypted = [0; MAX_FRAME];
    let length = state.write_message(payload, &mut encrypted)?;
    wire.send(&encrypted[..length])
}
fn encrypted_receive(
    wire: &mut Wire,
    state: &mut snow::TransportState,
) -> Result<Zeroizing<Vec<u8>>> {
    let encrypted = wire.receive()?;
    let mut plaintext = Zeroizing::new(vec![0; MAX_FRAME]);
    let length = state.read_message(&encrypted, &mut plaintext)?;
    plaintext.truncate(length);
    Ok(plaintext)
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    None,
    WrongCode,
    ChangedTranscript,
    SwappedKeys,
    WrongRole,
    ReflectedTag,
    CorruptedTag,
    TruncatedMessage,
    CloseAfterNoise,
}

fn attempt(socket: TcpStream, initiator: bool, mutation: Mutation) -> Result<()> {
    let mut wire = Wire::new(socket, Duration::from_secs(3))?;
    let (mut channel, mut binding) = exchange_noise(&mut wire, initiator)?;
    if matches!(mutation, Mutation::CloseAfterNoise) {
        anyhow::bail!("Cancelled after unauthenticated Noise XX");
    }
    if matches!(mutation, Mutation::ChangedTranscript) {
        binding.handshake_hash[0] ^= 1;
    }
    if matches!(mutation, Mutation::SwappedKeys) {
        std::mem::swap(&mut binding.initiator_static, &mut binding.responder_static);
    }
    let role = if initiator && !matches!(mutation, Mutation::WrongRole) {
        Role::Initiator
    } else {
        Role::Responder
    };
    // Deliberately fixed fixture values. No real user code or persistent identity.
    let code: &[u8] = if matches!(mutation, Mutation::WrongCode) {
        b"76543210"
    } else {
        b"01234567"
    };
    let pairing = Pairing::start(role, code, binding)?;
    let message = pairing.message();
    let message = if matches!(mutation, Mutation::TruncatedMessage) {
        &message[..message.len() - 1]
    } else {
        message
    };
    encrypted_send(&mut wire, &mut channel, message)?;
    let peer_message = encrypted_receive(&mut wire, &mut channel)?;
    let confirmation = pairing.process(&peer_message)?;
    let own_tag = *confirmation.tag();
    encrypted_send(&mut wire, &mut channel, &own_tag)?;
    let mut peer_tag = encrypted_receive(&mut wire, &mut channel)?;
    if matches!(mutation, Mutation::ReflectedTag) {
        peer_tag.copy_from_slice(&own_tag);
    }
    if matches!(mutation, Mutation::CorruptedTag) {
        peer_tag[0] ^= 1;
    }
    confirmation.verify_peer(&peer_tag)?;
    wire.remaining()?;
    // Success is only a local experiment result. Bilateral durable enrollment,
    // explicit approval, rate limiting and the production trust store are absent.
    Ok(())
}

fn pair(client_mutation: Mutation) -> (Result<()>, Result<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            match listener.accept() {
                Ok((stream, _)) => return attempt(stream, false, Mutation::None),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(Instant::now() < deadline, "Listener timeout");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    });
    let client = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(anyhow::Error::from)
        .and_then(|socket| attempt(socket, true, client_mutation));
    (client, server.join().unwrap())
}

fn accept_bounded(listener: &TcpListener) -> Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(Instant::now() < deadline, "Listener timeout");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[test]
#[ignore = "explicit localhost sockets; experimental authentication only, no trust writes or OS input"]
fn actual_noise_xx_bound_short_code_exchange() {
    let (client, server) = pair(Mutation::None);
    client.expect("correct client code");
    server.expect("correct server code");
    for mutation in [
        Mutation::WrongCode,
        Mutation::ChangedTranscript,
        Mutation::SwappedKeys,
        Mutation::WrongRole,
        Mutation::TruncatedMessage,
        Mutation::CloseAfterNoise,
    ] {
        let (client, server) = pair(mutation);
        assert!(client.is_err(), "client accepted {mutation:?}");
        assert!(server.is_err(), "server accepted {mutation:?}");
        if matches!(
            mutation,
            Mutation::WrongCode | Mutation::ChangedTranscript | Mutation::SwappedKeys
        ) {
            assert_eq!(
                client.unwrap_err().to_string(),
                "Peer confirmation rejected"
            );
            assert_eq!(
                server.unwrap_err().to_string(),
                "Peer confirmation rejected"
            );
        }
    }
    for mutation in [Mutation::ReflectedTag, Mutation::CorruptedTag] {
        let (client, _server) = pair(mutation);
        // Altering the local inbound tag after the server already verified its
        // own peer need not make the server fail. It must fail this verifier.
        assert!(client.is_err(), "client accepted {mutation:?}");
    }
}

#[test]
#[ignore = "explicit localhost socket; deliberately slow fragmented peer"]
fn one_total_deadline_rejects_drip_fed_header() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let sender = std::thread::spawn(move || {
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
        socket.write_all(&[0]).unwrap();
        std::thread::sleep(Duration::from_millis(220));
        let _ = socket.write_all(&[32]);
    });
    let socket = accept_bounded(&listener).unwrap();
    let mut wire = Wire::new(socket, Duration::from_millis(100)).unwrap();
    let start = Instant::now();
    assert!(wire.receive().is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    sender.join().unwrap();
}

#[test]
#[ignore = "two actual localhost Noise XX connections with a terminating relay; no input or trust persistence"]
fn separate_noise_sessions_prevent_pake_relay_by_a_middleman() {
    let left = TcpListener::bind("127.0.0.1:0").unwrap();
    let right = TcpListener::bind("127.0.0.1:0").unwrap();
    let left_address = left.local_addr().unwrap();
    let right_address = right.local_addr().unwrap();
    let honest_server = std::thread::spawn(move || -> Result<()> {
        attempt(accept_bounded(&right)?, false, Mutation::None)
    });
    let honest_client = std::thread::spawn(move || -> Result<()> {
        let socket = TcpStream::connect_timeout(&left_address, Duration::from_secs(1))?;
        attempt(socket, true, Mutation::None)
    });
    let relay = (|| -> Result<()> {
        let mut client_wire = Wire::new(accept_bounded(&left)?, Duration::from_secs(3))?;
        let (mut client_channel, client_binding) = exchange_noise(&mut client_wire, false)?;
        let mut server_wire = Wire::new(
            TcpStream::connect_timeout(&right_address, Duration::from_secs(1))?,
            Duration::from_secs(3),
        )?;
        let (mut server_channel, server_binding) = exchange_noise(&mut server_wire, true)?;
        // The proxy successfully terminates both unauthenticated XX sessions.
        // It never supplies a code to SPAKE2: it only relays encrypted records
        // as plaintext between its two independently encrypted transports.
        ensure!(
            client_binding.handshake_hash != server_binding.handshake_hash,
            "Expected independent XX transcripts"
        );
        for _ in 0..2 {
            // PAKE message, then directional confirmation.
            let from_client = encrypted_receive(&mut client_wire, &mut client_channel)?;
            encrypted_send(&mut server_wire, &mut server_channel, &from_client)?;
            let from_server = encrypted_receive(&mut server_wire, &mut server_channel)?;
            encrypted_send(&mut client_wire, &mut client_channel, &from_server)?;
        }
        Ok(())
    })();
    let client = honest_client.join().unwrap();
    let server = honest_server.join().unwrap();
    relay.expect("proxy must actually relay both PAKE and confirmation records");
    assert_eq!(
        client.unwrap_err().to_string(),
        "Peer confirmation rejected"
    );
    assert_eq!(
        server.unwrap_err().to_string(),
        "Peer confirmation rejected"
    );
}
