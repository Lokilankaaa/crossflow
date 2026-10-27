//! Pinned offline identities and fresh authenticated Noise sessions.
//!
//! Noise_KK requires both static public keys before the handshake. Transfer only
//! the intended device's identity directory through a trusted channel. No input
//! is sent or accepted as handshake payload. Caller supplies socket deadlines
//! and frames transport records; this module frames handshake records only.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const PATTERN: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";
pub const MAX_CIPHERTEXT: usize = 65_535;
pub const MAX_PLAINTEXT: usize = MAX_CIPHERTEXT - 16;
const MAX_HANDSHAKE: usize = 1_024;
const PROLOGUE: &[u8] = b"Crossflow/phase0/pinned-noise/v1/";

/// Public data only; safe for CLI output.
#[derive(Debug, Serialize)]
pub struct PairInfo {
    pub pattern: String,
    pub pair_id: String,
    pub windows_bundle: PathBuf,
    pub mac_bundle: PathBuf,
    pub windows_public_key_hex: String,
    pub mac_public_key_hex: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdentity {
    schema: u32,
    pattern: String,
    pair_id: String,
    role: String,
    private_key_hex: String,
    public_key_hex: String,
    peer_public_key_hex: String,
}

/// Intentionally not Serialize or Clone. Debug redacts the secret.
pub struct Identity {
    pub role: String,
    pub pair_id: String,
    private: [u8; 32],
    public: [u8; 32],
    peer: [u8; 32],
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("role", &self.role)
            .field("pair_id", &self.pair_id)
            .field("public_key", &encode_hex(&self.public))
            .field("peer_public_key", &encode_hex(&self.peer))
            .field("private_key", &"[REDACTED]")
            .finish()
    }
}

impl Identity {
    pub fn load(bundle: &Path) -> Result<Self> {
        let path = bundle.join("identity.json");
        let metadata = fs::metadata(&path).context("Cannot read identity bundle")?;
        ensure!(metadata.len() <= 4096, "Identity file exceeds size limit");
        let bytes = fs::read(&path).context("Cannot read identity bundle")?;
        let stored: StoredIdentity = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Malformed identity.json"))?;
        ensure!(
            stored.schema == 1 && stored.pattern == PATTERN,
            "Unsupported identity format"
        );
        ensure!(
            stored.role == "windows" || stored.role == "mac",
            "Unknown identity role"
        );
        ensure!(
            stored.pair_id.len() == 32 && stored.pair_id.bytes().all(is_lower_hex),
            "Invalid pair identifier"
        );
        let identity = Self {
            role: stored.role,
            pair_id: stored.pair_id,
            private: decode_key(&stored.private_key_hex)?,
            public: decode_key(&stored.public_key_hex)?,
            peer: decode_key(&stored.peer_public_key_hex)?,
        };
        identity.validate_keys()?;
        Ok(identity)
    }

    pub fn public_key_hex(&self) -> String {
        encode_hex(&self.public)
    }

    fn validate_keys(&self) -> Result<()> {
        let mut dh = DefaultResolver
            .resolve_dh(&snow::params::DHChoice::Curve25519)
            .context("Curve25519 unavailable in this build")?;
        dh.set(&self.private);
        ensure!(
            dh.pubkey() == self.public,
            "Public key does not match private key"
        );
        ensure!(
            self.public != self.peer,
            "Local and remote identities must differ"
        );
        let mut shared = [0u8; 32];
        dh.dh(&self.peer, &mut shared)
            .context("Invalid paired public key")?;
        ensure!(
            shared.iter().any(|v| *v != 0),
            "Invalid low-order paired public key"
        );
        Ok(())
    }
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}

fn decode_key(text: &str) -> Result<[u8; 32]> {
    ensure!(
        text.len() == 64 && text.bytes().all(is_lower_hex),
        "Invalid identity key encoding"
    );
    let mut output = [0u8; 32];
    for (index, pair) in text.as_bytes().chunks_exact(2).enumerate() {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        output[index] = digit(pair[0]) << 4 | digit(pair[1]);
    }
    Ok(output)
}

fn private_directory(path: &Path) -> Result<()> {
    let builder = DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("Refusing to overwrite an existing pairing directory")
}

fn save_identity(directory: &Path, identity: &StoredIdentity) -> Result<()> {
    private_directory(directory)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(directory.join("identity.json"))
        .context("Cannot create identity")?;
    serde_json::to_writer_pretty(&mut file, identity).context("Cannot write identity")?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

/// Never overwrites existing output; a partial failure is retained for review.
pub fn create_pair(output: &Path) -> Result<PairInfo> {
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    private_directory(output)?;
    let builder = snow::Builder::new(PATTERN.parse()?);
    let windows = builder
        .generate_keypair()
        .context("Cannot generate Windows identity")?;
    let mac = builder
        .generate_keypair()
        .context("Cannot generate Mac identity")?;
    let pair_id = format!(
        "{}{}",
        encode_hex(&windows.public[..8]),
        encode_hex(&mac.public[..8])
    );
    let info = PairInfo {
        pattern: PATTERN.into(),
        pair_id: pair_id.clone(),
        windows_bundle: output.join("windows"),
        mac_bundle: output.join("mac"),
        windows_public_key_hex: encode_hex(&windows.public),
        mac_public_key_hex: encode_hex(&mac.public),
    };
    for (role, local, peer) in [("windows", &windows, &mac), ("mac", &mac, &windows)] {
        save_identity(
            &output.join(role),
            &StoredIdentity {
                schema: 1,
                pattern: PATTERN.into(),
                pair_id: pair_id.clone(),
                role: role.into(),
                private_key_hex: encode_hex(&local.private),
                public_key_hex: encode_hex(&local.public),
                peer_public_key_hex: encode_hex(&peer.public),
            },
        )?;
    }
    Ok(info)
}

fn handshake_state(identity: &Identity, initiator: bool) -> Result<snow::HandshakeState> {
    ensure!(
        identity.role == if initiator { "windows" } else { "mac" },
        "Identity has wrong endpoint role"
    );
    identity.validate_keys()?;
    let mut prologue = PROLOGUE.to_vec();
    prologue.extend_from_slice(identity.pair_id.as_bytes());
    let builder = snow::Builder::new(PATTERN.parse()?)
        .local_private_key(&identity.private)?
        .remote_public_key(&identity.peer)?
        .prologue(&prologue)?;
    Ok(if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    })
}

fn finish_handshake(state: snow::HandshakeState, identity: &Identity) -> Result<SecureChannel> {
    ensure!(state.is_handshake_finished(), "Noise handshake incomplete");
    // These are public bytes. Noise's authenticated DH proves key possession;
    // this equality check alone is not authentication.
    ensure!(
        state.get_remote_static() == Some(identity.peer.as_slice()),
        "Unexpected peer identity"
    );
    Ok(SecureChannel {
        state: state.into_transport_mode()?,
        failed: false,
    })
}

fn write_handshake_frame(stream: &mut impl Write, bytes: &[u8]) -> Result<()> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_HANDSHAKE,
        "Invalid handshake record length"
    );
    stream.write_all(&(bytes.len() as u16).to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_handshake_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header)?;
    let length = u16::from_be_bytes(header) as usize;
    ensure!(
        length > 0 && length <= MAX_HANDSHAKE,
        "Invalid handshake record length"
    );
    let mut data = vec![0; length];
    stream.read_exact(&mut data)?;
    Ok(data)
}

fn remaining_timeout(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Total handshake deadline exceeded",
            )
        })
}

/// Every partial read/write receives the remaining total budget. A peer sending
/// one byte before each socket timeout cannot extend the handshake indefinitely.
struct DeadlineIo<'a> {
    socket: &'a mut TcpStream,
    deadline: Instant,
}

impl Read for DeadlineIo<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.socket
            .set_read_timeout(Some(remaining_timeout(self.deadline)?))?;
        self.socket.read(buffer)
    }
}

impl Write for DeadlineIo<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.socket
            .set_write_timeout(Some(remaining_timeout(self.deadline)?))?;
        self.socket.write(buffer)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        remaining_timeout(self.deadline)?;
        self.socket.flush()
    }
}

/// Set finite read/write timeouts before calling. The total budget is the smaller
/// of those timeouts and three seconds, including partial IO. This switches to
/// blocking mode; the transport may switch back after success.
/// Never reuse a failed session.
pub fn handshake(
    stream: &mut TcpStream,
    identity: &Identity,
    initiator: bool,
) -> Result<SecureChannel> {
    let read_timeout = stream
        .read_timeout()?
        .context("Finite handshake read timeout required")?;
    let write_timeout = stream
        .write_timeout()?
        .context("Finite handshake write timeout required")?;
    let deadline = Instant::now() + read_timeout.min(write_timeout).min(Duration::from_secs(3));
    // Accepted sockets inherit a nonblocking listener's mode on Windows.
    stream.set_nonblocking(false)?;
    let mut state = handshake_state(identity, initiator)?;
    let mut stream = DeadlineIo {
        socket: stream,
        deadline,
    };
    let mut output = [0u8; MAX_HANDSHAKE];
    let mut payload = [0u8; MAX_HANDSHAKE];
    if initiator {
        let length = state.write_message(&[], &mut output)?;
        write_handshake_frame(&mut stream, &output[..length])?;
        let response = read_handshake_frame(&mut stream)?;
        ensure!(
            state.read_message(&response, &mut payload)? == 0,
            "Handshake payload forbidden"
        );
    } else {
        let request = read_handshake_frame(&mut stream)?;
        ensure!(
            state.read_message(&request, &mut payload)? == 0,
            "Handshake payload forbidden"
        );
        let length = state.write_message(&[], &mut output)?;
        write_handshake_frame(&mut stream, &output[..length])?;
    }
    remaining_timeout(deadline)?;
    stream.socket.set_read_timeout(Some(read_timeout))?;
    stream.socket.set_write_timeout(Some(write_timeout))?;
    finish_handshake(state, identity)
}

pub struct SecureChannel {
    state: snow::TransportState,
    failed: bool,
}

impl SecureChannel {
    /// Ciphertext only; caller must add a bounded record length prefix.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        ensure!(!self.failed, "Secure channel no longer usable");
        ensure!(
            plaintext.len() <= MAX_PLAINTEXT,
            "Plaintext exceeds Noise record limit"
        );
        let mut output = vec![0; plaintext.len() + 16];
        match self.state.write_message(plaintext, &mut output) {
            Ok(length) => {
                output.truncate(length);
                Ok(output)
            }
            Err(error) => {
                self.failed = true;
                Err(error).context("Noise encryption failed")
            }
        }
    }

    /// Authentication, replay or length failure permanently poisons this channel.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        ensure!(!self.failed, "Secure channel no longer usable");
        if !(16..=MAX_CIPHERTEXT).contains(&ciphertext.len()) {
            self.failed = true;
            bail!("Invalid Noise ciphertext length");
        }
        let mut output = vec![0; ciphertext.len() - 16];
        match self.state.read_message(ciphertext, &mut output) {
            Ok(length) => {
                output.truncate(length);
                Ok(output)
            }
            Err(error) => {
                self.failed = true;
                Err(error).context("Noise authentication failed; disconnect required")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn identities() -> (Identity, Identity) {
        let builder = snow::Builder::new(PATTERN.parse().unwrap());
        let a = builder.generate_keypair().unwrap();
        let b = builder.generate_keypair().unwrap();
        let pair_id = format!(
            "{}{}",
            encode_hex(&a.public[..8]),
            encode_hex(&b.public[..8])
        );
        (
            Identity {
                role: "windows".into(),
                pair_id: pair_id.clone(),
                private: a.private.as_slice().try_into().unwrap(),
                public: a.public.as_slice().try_into().unwrap(),
                peer: b.public.as_slice().try_into().unwrap(),
            },
            Identity {
                role: "mac".into(),
                pair_id,
                private: b.private.as_slice().try_into().unwrap(),
                public: b.public.as_slice().try_into().unwrap(),
                peer: a.public.as_slice().try_into().unwrap(),
            },
        )
    }

    fn channels(a: &Identity, b: &Identity) -> Result<(SecureChannel, SecureChannel)> {
        let mut initiator = handshake_state(a, true)?;
        let mut responder = handshake_state(b, false)?;
        let mut message = [0u8; MAX_HANDSHAKE];
        let mut payload = [0u8; MAX_HANDSHAKE];
        let length = initiator.write_message(&[], &mut message)?;
        ensure!(
            responder.read_message(&message[..length], &mut payload)? == 0,
            "payload"
        );
        let length = responder.write_message(&[], &mut message)?;
        ensure!(
            initiator.read_message(&message[..length], &mut payload)? == 0,
            "payload"
        );
        Ok((
            finish_handshake(initiator, a)?,
            finish_handshake(responder, b)?,
        ))
    }

    #[test]
    fn paired_peers_exchange_both_directions() {
        let (a, b) = identities();
        let (mut tx, mut rx) = channels(&a, &b).unwrap();
        let cipher = tx.encrypt(b"movement").unwrap();
        assert_eq!(rx.decrypt(&cipher).unwrap(), b"movement");
        assert_eq!(
            tx.decrypt(&rx.encrypt(b"lease").unwrap()).unwrap(),
            b"lease"
        );
    }

    #[test]
    fn wrong_peer_rejected_during_handshake() {
        let (a, b) = identities();
        let (_, stranger) = identities();
        assert!(channels(&a, &stranger).is_err());
        let (stranger, _) = identities();
        assert!(channels(&stranger, &b).is_err());
    }

    #[test]
    fn wrong_pin_rejected_even_with_same_pair_id() {
        let (a, mut b) = identities();
        let (stranger, _) = identities();
        b.peer = stranger.public;
        assert!(channels(&a, &b).is_err());
    }

    #[test]
    fn corruption_poisoned_channel_never_accepts_following_valid_data() {
        let (a, b) = identities();
        let (mut tx, mut rx) = channels(&a, &b).unwrap();
        let mut bad = tx.encrypt(b"movement").unwrap();
        bad[0] ^= 1;
        assert!(rx.decrypt(&bad).is_err());
        assert!(rx.decrypt(&tx.encrypt(b"valid next").unwrap()).is_err());
    }

    #[test]
    fn replay_is_rejected() {
        let (a, b) = identities();
        let (mut tx, mut rx) = channels(&a, &b).unwrap();
        let cipher = tx.encrypt(b"movement").unwrap();
        assert!(rx.decrypt(&cipher).is_ok());
        assert!(rx.decrypt(&cipher).is_err());
    }

    #[test]
    fn fresh_session_rejects_old_ciphertext() {
        let (a, b) = identities();
        let (mut old_tx, _) = channels(&a, &b).unwrap();
        let old = old_tx.encrypt(b"old movement").unwrap();
        let (mut new_tx, mut new_rx) = channels(&a, &b).unwrap();
        assert_ne!(old, new_tx.encrypt(b"old movement").unwrap());
        assert!(new_rx.decrypt(&old).is_err());
    }

    #[test]
    fn role_and_identity_validation_fail_closed() {
        let (mut a, _) = identities();
        assert!(handshake_state(&a, false).is_err());
        a.peer = [0; 32];
        assert!(a.validate_keys().is_err());
        assert!(decode_key("bad").is_err());
    }

    #[test]
    fn framing_bounds_and_truncation_rejected() {
        assert!(read_handshake_frame(&mut Cursor::new([0, 0])).is_err());
        assert!(read_handshake_frame(&mut Cursor::new([255, 255])).is_err());
        assert!(read_handshake_frame(&mut Cursor::new([0, 2, 3])).is_err());
        let mut wire = vec![];
        write_handshake_frame(&mut wire, b"abc").unwrap();
        assert_eq!(
            read_handshake_frame(&mut Cursor::new(wire)).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn expired_total_deadline_has_no_remaining_io_budget() {
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            remaining_timeout(past).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
        let future = Instant::now() + Duration::from_secs(1);
        let remaining = remaining_timeout(future).unwrap();
        assert!(remaining > Duration::ZERO && remaining <= Duration::from_secs(1));
    }

    #[test]
    fn size_limits_apply_before_crypto() {
        let (a, b) = identities();
        let (mut tx, mut rx) = channels(&a, &b).unwrap();
        assert!(tx.encrypt(&vec![0; MAX_PLAINTEXT + 1]).is_err());
        assert!(rx.decrypt(&[0; 15]).is_err());
    }

    #[test]
    fn generated_files_roundtrip_and_never_overwrite() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "crossflow-security-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let info = create_pair(&path).unwrap();
        let a = Identity::load(&info.windows_bundle).unwrap();
        let b = Identity::load(&info.mac_bundle).unwrap();
        assert!(channels(&a, &b).is_ok());
        assert!(!serde_json::to_string(&info).unwrap().contains("private"));
        assert!(!format!("{a:?}").contains(&encode_hex(&a.private)));
        assert!(create_pair(&path).is_err());
        fs::remove_file(info.windows_bundle.join("identity.json")).unwrap();
        fs::remove_file(info.mac_bundle.join("identity.json")).unwrap();
        fs::remove_dir(info.windows_bundle).unwrap();
        fs::remove_dir(info.mac_bundle).unwrap();
        fs::remove_dir(path).unwrap();
    }

    fn tcp_exchange(client: Identity, server: Identity) -> (Result<Vec<u8>>, Result<()>) {
        use std::net::TcpListener;
        use std::thread;
        use std::time::{Duration, Instant};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || -> Result<()> {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        ensure!(Instant::now() < deadline, "Test accept timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            socket.set_read_timeout(Some(Duration::from_secs(3)))?;
            socket.set_write_timeout(Some(Duration::from_secs(3)))?;
            let mut channel = handshake(&mut socket, &server, false)?;
            let encrypted = read_handshake_frame(&mut socket)?;
            ensure!(
                channel.decrypt(&encrypted)? == b"probe",
                "Unexpected test payload"
            );
            write_handshake_frame(&mut socket, &channel.encrypt(b"accepted")?)?;
            Ok(())
        });
        let client_result = (|| -> Result<Vec<u8>> {
            let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
            socket.set_read_timeout(Some(Duration::from_secs(3)))?;
            socket.set_write_timeout(Some(Duration::from_secs(3)))?;
            let mut channel = handshake(&mut socket, &client, true)?;
            write_handshake_frame(&mut socket, &channel.encrypt(b"probe")?)?;
            channel.decrypt(&read_handshake_frame(&mut socket)?)
        })();
        (client_result, worker.join().expect("Test server panicked"))
    }

    #[test]
    #[ignore = "Requires permission to connect localhost sockets; run explicitly with --ignored"]
    fn tcp_handshake_and_encrypted_roundtrip() {
        let (a, b) = identities();
        let (client, server) = tcp_exchange(a, b);
        assert!(
            server.is_ok(),
            "Server handshake/roundtrip failed: {server:?}"
        );
        assert_eq!(client.unwrap(), b"accepted");
    }

    #[test]
    #[ignore = "Requires permission to connect localhost sockets; run explicitly with --ignored"]
    fn tcp_wrong_identity_never_reaches_application_data() {
        let (a, mut b) = identities();
        let (stranger, _) = identities();
        b.peer = stranger.public;
        let (client, server) = tcp_exchange(a, b);
        assert!(client.is_err());
        assert!(server.is_err());
    }

    #[test]
    #[ignore = "Requires permission to connect localhost sockets; run explicitly with --ignored"]
    fn tcp_partial_reads_cannot_extend_total_handshake_budget() {
        use std::net::TcpListener;
        use std::thread;
        let (_, server) = identities();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let began = Instant::now();
            let result = handshake(&mut socket, &server, false);
            (result.is_err(), began.elapsed())
        });
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        socket.write_all(&[0, 48]).unwrap();
        for _ in 0..16 {
            if socket.write_all(&[0]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        drop(socket);
        let (failed, elapsed) = worker.join().unwrap();
        assert!(failed);
        assert!(
            elapsed < Duration::from_millis(300),
            "Drip-fed handshake exceeded total deadline tolerance: {elapsed:?}"
        );
    }
}
