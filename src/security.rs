//! Pinned identities and fresh authenticated Noise sessions.
//!
//! Noise_KK requires both static public keys before the handshake. Authorized
//! pairing can construct identities in memory; legacy diagnostic bundles must
//! reach only their intended device through a trusted channel. No input is sent
//! or accepted as handshake payload. Caller supplies socket deadlines and frames
//! transport records; this module frames handshake records only.
//! Identities can be stored using Windows current-user DPAPI or macOS Keychain;
//! protected storage does not itself perform pairing or establish trust.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

pub const PATTERN: &str = "Noise_KK_25519_ChaChaPoly_BLAKE2s";
pub const MAX_CIPHERTEXT: usize = 65_535;
pub const MAX_PLAINTEXT: usize = MAX_CIPHERTEXT - 16;
const MAX_HANDSHAKE: usize = 1_024;
const PROLOGUE: &[u8] = b"Crossflow/phase0/pinned-noise/v1/";
#[cfg(not(target_os = "macos"))]
pub(crate) const PROTECTED_FILE: &str = "identity.dpapi";
#[cfg(target_os = "macos")]
pub(crate) const PROTECTED_FILE: &str = "identity.keychain";
const SEALED_HEADER: &[u8; 5] = b"CFID\x01";
const SEALED_LENGTH: usize = 5 + 1 + 32 + 32 * 3;

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

impl Drop for StoredIdentity {
    fn drop(&mut self) {
        self.private_key_hex.zeroize();
    }
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
    /// Trusted local adapter after the caller has completed short-code peer
    /// authentication AND explicit local approval. This constructor does not
    /// authenticate network data, imply peer consent, or write any files. The
    /// caller supplies the transport role and the authenticated peer static key.
    pub fn from_authorized_pairing(
        role: &str,
        pair_id: String,
        private: Zeroizing<[u8; 32]>,
        peer: [u8; 32],
    ) -> Result<Self> {
        validate_metadata(role, &pair_id)?;
        let mut dh = DefaultResolver
            .resolve_dh(&snow::params::DHChoice::Curve25519)
            .context("Curve25519 unavailable in this build")?;
        dh.set(&private[..]);
        let public: [u8; 32] = dh
            .pubkey()
            .try_into()
            .context("Invalid Curve25519 public key length")?;
        let identity = Self {
            role: role.into(),
            pair_id,
            private: *private,
            public,
            peer,
        };
        identity.validate_keys()?;
        Ok(identity)
    }

    pub fn load(bundle: &Path) -> Result<Self> {
        // A foreign-platform envelope is also an explicit protected-storage
        // marker. Moving it between machines must not revive legacy plaintext.
        let foreign_file = if cfg!(target_os = "macos") {
            "identity.dpapi"
        } else {
            "identity.keychain"
        };
        match fs::symlink_metadata(bundle.join(foreign_file)) {
            Ok(_) => bail!("Identity contains a foreign-platform protected record"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("Cannot inspect protected identity"),
        }
        // A present but damaged/inaccessible protected identity must fail closed;
        // do not silently fall back to an older plaintext identity beside it.
        let protected = bundle.join(PROTECTED_FILE);
        match fs::symlink_metadata(&protected) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_file(),
                    "Protected identity is not a regular file"
                );
                let sealed = read_bounded(&protected, 65_536)?;
                return Self::open_protected_record(&sealed);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("Cannot access protected identity"),
        }
        let path = bundle.join("identity.json");
        let bytes = read_bounded(&path, 4096).context("Cannot read identity bundle")?;
        let stored: StoredIdentity = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Malformed identity.json"))?;
        ensure!(
            stored.schema == 1 && stored.pattern == PATTERN,
            "Unsupported identity format"
        );
        validate_metadata(&stored.role, &stored.pair_id)?;
        let public = decode_key(&stored.public_key_hex)?;
        let peer = decode_key(&stored.peer_public_key_hex)?;
        let private = Zeroizing::new(decode_key(&stored.private_key_hex)?);
        let identity = Self {
            role: stored.role.clone(),
            pair_id: stored.pair_id.clone(),
            private: *private,
            public,
            peer,
        };
        identity.validate_keys()?;
        Ok(identity)
    }

    #[cfg(not(target_os = "macos"))]
    fn open_protected_record(sealed: &[u8]) -> Result<Self> {
        let plaintext = crate::secret_store::unprotect(sealed)?;
        Self::from_protected_record(plaintext.as_slice())
    }

    #[cfg(target_os = "macos")]
    fn open_protected_record(locator: &[u8]) -> Result<Self> {
        let reference = crate::keychain::KeychainReference::from_bytes(locator)?;
        let plaintext = crate::keychain::load(&reference)?;
        Self::from_protected_record(&plaintext)
    }

    /// Persist an already authenticated identity using the current user's OS
    /// protection. No plaintext file is written, no existing directory is
    /// overwritten, and the source identity is not deleted. This is storage,
    /// not enrollment: callers must establish peer trust before saving.
    pub fn save_protected(&self, output: &Path) -> Result<()> {
        self.validate()?;
        let plaintext = self.protected_record();
        Self::persist_protected_record(output, &plaintext)
    }

    fn protected_record(&self) -> Zeroizing<Vec<u8>> {
        let mut plaintext = Zeroizing::new(Vec::with_capacity(SEALED_LENGTH));
        plaintext.extend_from_slice(SEALED_HEADER);
        plaintext.push(if self.role == "windows" { 0 } else { 1 });
        plaintext.extend_from_slice(self.pair_id.as_bytes());
        plaintext.extend_from_slice(&self.private);
        plaintext.extend_from_slice(&self.public);
        plaintext.extend_from_slice(&self.peer);
        plaintext
    }

    #[cfg(not(target_os = "macos"))]
    fn persist_protected_record(output: &Path, plaintext: &[u8]) -> Result<()> {
        let sealed = crate::secret_store::protect(plaintext)?;
        // Protection must succeed before creating any persistent state. On an
        // unsupported platform this leaves no empty destination behind.
        private_directory(output)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(output.join(PROTECTED_FILE))
            .context("Cannot create protected identity")?;
        file.write_all(&sealed)
            .context("Cannot write protected identity")?;
        file.sync_all().context("Cannot flush protected identity")?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn persist_protected_record(output: &Path, plaintext: &[u8]) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        // Reserve a new private directory before creating any Keychain item.
        private_directory(output)?;
        let reference = match crate::keychain::store(plaintext) {
            Ok(reference) => reference,
            Err(error) => {
                // Only our newly reserved directory, and only if still empty.
                // A temporary locked Keychain must not block the next attempt.
                if fs::remove_dir(output).is_err() {
                    return Err(error).context(
                        "Keychain store failed; empty directory cleanup could not be confirmed",
                    );
                }
                return Err(error);
            }
        };
        let mut created_locator = false;
        let saved = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(output.join(PROTECTED_FILE))
                .context("Cannot create Keychain identity locator")?;
            created_locator = true;
            file.write_all(&reference.to_bytes())
                .context("Cannot write Keychain identity locator")?;
            file.sync_all()
                .context("Cannot flush Keychain identity locator")?;
            fs::File::open(output)?
                .sync_all()
                .context("Cannot flush Keychain identity directory")?;
            Ok(())
        })();
        if let Err(error) = saved {
            // Delete only the item created by this attempt. A crash between
            // Keychain add and locator commit still requires orphan recovery.
            if crate::keychain::remove(&reference).is_err() {
                return Err(error)
                    .context("Keychain locator save failed; item cleanup could not be confirmed");
            }
            if created_locator && fs::remove_file(output.join(PROTECTED_FILE)).is_err() {
                return Err(error)
                    .context("Keychain item removed; locator cleanup could not be confirmed");
            }
            if fs::remove_dir(output).is_err() {
                return Err(error)
                    .context("Keychain item removed; directory cleanup could not be confirmed");
            }
            return Err(error);
        }
        Ok(())
    }

    fn from_protected_record(record: &[u8]) -> Result<Self> {
        ensure!(
            record.len() == SEALED_LENGTH && &record[..5] == SEALED_HEADER,
            "Unsupported protected identity format"
        );
        let role = match record[5] {
            0 => "windows",
            1 => "mac",
            _ => bail!("Unknown protected identity role"),
        };
        let pair_id = &record[6..38];
        ensure!(
            pair_id.iter().copied().all(is_lower_hex),
            "Invalid protected pair identifier"
        );
        let identity = Self {
            role: role.into(),
            pair_id: String::from_utf8(pair_id.to_vec())
                .context("Invalid protected pair identifier")?,
            private: record[38..70].try_into()?,
            public: record[70..102].try_into()?,
            peer: record[102..134].try_into()?,
        };
        identity.validate_keys()?;
        Ok(identity)
    }

    pub fn public_key_hex(&self) -> String {
        encode_hex(&self.public)
    }

    pub fn peer_public_key_hex(&self) -> String {
        encode_hex(&self.peer)
    }

    /// Revalidate before creating storage; public metadata may have been changed
    /// since construction, even though the private and pinned keys are opaque.
    pub(crate) fn validate(&self) -> Result<()> {
        validate_metadata(&self.role, &self.pair_id)?;
        self.validate_keys()
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
        let mut shared = Zeroizing::new([0u8; 32]);
        dh.dh(&self.peer, shared.as_mut())
            .context("Invalid paired public key")?;
        ensure!(
            shared.iter().any(|v| *v != 0),
            "Invalid low-order paired public key"
        );
        Ok(())
    }
}

impl Drop for Identity {
    fn drop(&mut self) {
        self.private.zeroize();
    }
}

fn read_bounded(path: &Path, limit: usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut file = fs::File::open(path).context("Cannot read identity")?;
    ensure!(file.metadata()?.is_file(), "Identity is not a regular file");
    // Read into a fixed allocation rather than read_to_end: no growth or
    // reallocations can leave freed plaintext outside Zeroizing's ownership.
    let mut bytes = Zeroizing::new(vec![0; limit + 1]);
    let mut length = 0;
    while length < bytes.len() {
        match file.read(&mut bytes[length..]) {
            Ok(0) => break,
            Ok(count) => length += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("Cannot read identity"),
        }
    }
    ensure!(length <= limit, "Identity file exceeds size limit");
    bytes.truncate(length);
    Ok(bytes)
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn validate_metadata(role: &str, pair_id: &str) -> Result<()> {
    ensure!(role == "windows" || role == "mac", "Unknown identity role");
    ensure!(
        pair_id.len() == 32 && pair_id.bytes().all(is_lower_hex),
        "Invalid pair identifier"
    );
    Ok(())
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
    // Keep the wire format while avoiding a separate tiny header write. This
    // reduces small-packet waiting; it does not diagnose any LAN failure.
    let mut frame = Vec::with_capacity(bytes.len() + 2);
    frame.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    frame.extend_from_slice(bytes);
    stream
        .write_all(&frame)
        .context("Write handshake frame (header and body)")?;
    stream.flush().context("Flush handshake frame")?;
    Ok(())
}

fn read_handshake_frame(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut header = [0u8; 2];
    stream
        .read_exact(&mut header)
        .context("Read handshake header")?;
    let length = u16::from_be_bytes(header) as usize;
    ensure!(
        length > 0 && length <= MAX_HANDSHAKE,
        "Invalid handshake record length"
    );
    let mut data = vec![0; length];
    stream
        .read_exact(&mut data)
        .context("Read handshake body")?;
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
    stream
        .set_nonblocking(false)
        .context("Set blocking handshake mode")?;
    stream
        .set_nodelay(true)
        .context("Enable TCP_NODELAY before handshake")?;
    let mut state = handshake_state(identity, initiator).context("Initialize Noise handshake")?;
    let mut stream = DeadlineIo {
        socket: stream,
        deadline,
    };
    let mut output = [0u8; MAX_HANDSHAKE];
    let mut payload = [0u8; MAX_HANDSHAKE];
    if initiator {
        let length = state
            .write_message(&[], &mut output)
            .context("Create Noise request")?;
        write_handshake_frame(&mut stream, &output[..length]).context("Send Noise request")?;
        let response = read_handshake_frame(&mut stream).context("Receive Noise response")?;
        ensure!(
            state
                .read_message(&response, &mut payload)
                .context("Authenticate Noise response")?
                == 0,
            "Handshake payload forbidden"
        );
    } else {
        let request = read_handshake_frame(&mut stream).context("Receive Noise request")?;
        ensure!(
            state
                .read_message(&request, &mut payload)
                .context("Authenticate Noise request")?
                == 0,
            "Handshake payload forbidden"
        );
        let length = state
            .write_message(&[], &mut output)
            .context("Create Noise response")?;
        write_handshake_frame(&mut stream, &output[..length]).context("Send Noise response")?;
    }
    remaining_timeout(deadline).context("Finish handshake within total deadline")?;
    stream
        .socket
        .set_read_timeout(Some(read_timeout))
        .context("Restore read timeout after handshake")?;
    stream
        .socket
        .set_write_timeout(Some(write_timeout))
        .context("Restore write timeout after handshake")?;
    finish_handshake(state, identity).context("Finish local Noise handshake")
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
    fn authorized_identity_derives_public_key_and_reuses_pinned_protocol() {
        let (a, b) = identities();
        let local = Identity::from_authorized_pairing(
            "windows",
            a.pair_id.clone(),
            Zeroizing::new(a.private),
            a.peer,
        )
        .unwrap();
        let remote = Identity::from_authorized_pairing(
            "mac",
            b.pair_id.clone(),
            Zeroizing::new(b.private),
            b.peer,
        )
        .unwrap();
        assert_eq!(local.public, a.public);
        assert_eq!(remote.public, b.public);
        let (mut tx, mut rx) = channels(&local, &remote).unwrap();
        assert_eq!(
            rx.decrypt(&tx.encrypt(b"authorized in memory").unwrap())
                .unwrap(),
            b"authorized in memory"
        );
        let private_hex = Zeroizing::new(encode_hex(&a.private));
        let debug = format!("{local:?}");
        assert!(debug.contains("[REDACTED]") && !debug.contains(private_hex.as_str()));
    }

    #[test]
    fn authorized_identity_rejects_invalid_metadata_self_and_low_order_keys() {
        let (a, _) = identities();
        for role in ["", "Windows", "linux", "windows\n"] {
            assert!(
                Identity::from_authorized_pairing(
                    role,
                    a.pair_id.clone(),
                    Zeroizing::new(a.private),
                    a.peer
                )
                .is_err()
            );
        }
        for pair_id in [
            String::new(),
            "a".repeat(31),
            "a".repeat(33),
            "A".repeat(32),
            "g".repeat(32),
        ] {
            assert!(
                Identity::from_authorized_pairing(
                    "windows",
                    pair_id,
                    Zeroizing::new(a.private),
                    a.peer
                )
                .is_err()
            );
        }
        let mut one = [0; 32];
        one[0] = 1;
        for peer in [[0; 32], one, a.public] {
            assert!(
                Identity::from_authorized_pairing(
                    "windows",
                    a.pair_id.clone(),
                    Zeroizing::new(a.private),
                    peer
                )
                .is_err()
            );
        }
    }

    #[test]
    fn portable_protected_record_validates_both_platform_roles() {
        let (a, b) = identities();
        for identity in [&a, &b] {
            let record = identity.protected_record();
            let loaded = Identity::from_protected_record(&record).unwrap();
            assert_eq!(loaded.role, identity.role);
            assert_eq!(loaded.public, identity.public);
            assert_eq!(loaded.peer, identity.peer);
            for offset in [0, 5, 6, 38, 70, 102] {
                let mut corrupted = Zeroizing::new(record.to_vec());
                if offset == 102 {
                    corrupted[102..134].fill(0);
                } else {
                    corrupted[offset] ^= 0xff;
                }
                assert!(Identity::from_protected_record(&corrupted).is_err());
            }
            assert!(Identity::from_protected_record(&record[..record.len() - 1]).is_err());
        }
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
    fn framing_errors_identify_header_body_and_preserve_io_cause() {
        for (bytes, context) in [
            (vec![0], "Read handshake header"),
            (vec![0, 2, 3], "Read handshake body"),
        ] {
            let error = read_handshake_frame(&mut Cursor::new(bytes)).unwrap_err();
            assert_eq!(error.to_string(), context);
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }
        struct FailedWrite;
        impl Write for FailedWrite {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "test deadline",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_handshake_frame(&mut FailedWrite, b"abc").unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "Write handshake frame (header and body): test deadline"
        );
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn handshake_header_and_body_share_one_write_and_keep_wire_format() {
        #[derive(Default)]
        struct Recorder {
            writes: Vec<Vec<u8>>,
            flushed: bool,
        }
        impl Write for Recorder {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.writes.push(bytes.to_vec());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushed = true;
                Ok(())
            }
        }
        let mut recorder = Recorder::default();
        write_handshake_frame(&mut recorder, b"abc").unwrap();
        assert_eq!(recorder.writes, vec![vec![0, 3, b'a', b'b', b'c']]);
        assert!(recorder.flushed);
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
        let foreign = info.windows_bundle.join(if cfg!(target_os = "macos") {
            "identity.dpapi"
        } else {
            "identity.keychain"
        });
        fs::write(&foreign, b"foreign protected identity marker").unwrap();
        assert!(Identity::load(&info.windows_bundle).is_err());
        fs::remove_file(foreign).unwrap();
        assert!(Identity::load(&info.windows_bundle).is_ok());
        fs::remove_file(info.windows_bundle.join("identity.json")).unwrap();
        fs::remove_file(info.mac_bundle.join("identity.json")).unwrap();
        fs::remove_dir(info.windows_bundle).unwrap();
        fs::remove_dir(info.mac_bundle).unwrap();
        fs::remove_dir(path).unwrap();
    }

    #[cfg(windows)]
    fn protected_test_directory() -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "crossflow-protected-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "explicit native macOS Keychain test; creates and deletes an isolated item; never injects input"]
    fn macos_keychain_identity_store_reopens_and_authenticates() -> Result<()> {
        let (a, b) = identities();
        let root = std::env::temp_dir().canonicalize()?.join(format!(
            "crossflow-keychain-identity-{}",
            crate::discovery::new_public_id()?
        ));
        fs::create_dir(&root)?;
        let store = crate::peers::PeerStore::at(root.join("peers"));
        let peer_id = b.peer_public_key_hex();
        let bundle = root.join("peers").join(&peer_id);
        let checked = (|| -> Result<()> {
            // Reject an identity belonging to the wrong platform before save.
            ensure!(
                store
                    .remember_authorized(identities().0, "Wrong role", "127.0.0.1:5555".parse()?)
                    .is_err(),
                "Wrong local role accepted"
            );
            let saved = store.remember_authorized_receiver(b, "Keychain test Windows")?;
            ensure!(
                saved.last_endpoint.is_none(),
                "Receiver-only peer acquired a false route"
            );
            let reopened = crate::peers::PeerStore::at(root.join("peers"));
            ensure!(
                reopened.list()?.peers == vec![saved],
                "Stored peer list mismatch"
            );
            let (_, resolved) = reopened.resolve(&peer_id)?;
            let loaded = Identity::load(&resolved)?;
            ensure!(loaded.role == "mac", "Wrong loaded role");
            ensure!(
                !resolved.join("identity.json").exists(),
                "Unexpected plaintext identity"
            );
            ensure!(
                loaded.save_protected(&resolved).is_err(),
                "Existing identity overwritten"
            );
            let (mut tx, mut rx) = channels(&a, &loaded)?;
            ensure!(
                rx.decrypt(&tx.encrypt(b"keychain restored identity")?)?
                    == b"keychain restored identity",
                "Restored Noise identity failed"
            );
            let locator = fs::read(resolved.join(PROTECTED_FILE))?;
            fs::write(resolved.join(PROTECTED_FILE), b"invalid keychain locator")?;
            let rejected = Identity::load(&resolved).is_err();
            fs::write(resolved.join(PROTECTED_FILE), &locator)?;
            ensure!(rejected, "Invalid locator accepted");
            Ok(())
        })();
        // Exact cleanup even after an assertion failure; never sweep Keychain.
        let cleanup = (|| -> Result<()> {
            if bundle.join(PROTECTED_FILE).is_file() {
                let reference = crate::keychain::KeychainReference::from_bytes(&fs::read(
                    bundle.join(PROTECTED_FILE),
                )?)?;
                crate::keychain::remove(&reference)?;
                fs::remove_file(bundle.join(PROTECTED_FILE))?;
            }
            for name in ["peer.json", "peer.pending.json"] {
                let path = bundle.join(name);
                if path.is_file() {
                    fs::remove_file(path)?;
                }
            }
            if bundle.is_dir() {
                fs::remove_dir(&bundle)?;
            }
            if root.join("peers").is_dir() {
                fs::remove_dir(root.join("peers"))?;
            }
            fs::remove_dir(&root)?;
            Ok(())
        })();
        cleanup.context("Native Keychain fixture cleanup failed")?;
        checked
    }

    #[test]
    #[cfg(windows)]
    fn protected_identity_reloads_into_existing_authenticated_protocol() {
        let (a, b) = identities();
        let path = protected_test_directory();
        a.save_protected(&path).unwrap();
        assert!(!path.join("identity.json").exists());
        let stored = fs::read(path.join(PROTECTED_FILE)).unwrap();
        assert!(
            !stored
                .windows(a.private.len())
                .any(|part| part == a.private)
        );
        let loaded = Identity::load(&path).unwrap();
        assert_eq!(loaded.public, a.public);
        assert_eq!(loaded.peer, a.peer);
        assert_eq!(loaded.pair_id, a.pair_id);
        let (mut tx, mut rx) = channels(&loaded, &b).unwrap();
        assert_eq!(
            rx.decrypt(&tx.encrypt(b"protected identity").unwrap())
                .unwrap(),
            b"protected identity"
        );
        assert!(a.save_protected(&path).is_err());
        assert_eq!(fs::read(path.join(PROTECTED_FILE)).unwrap(), stored);
        fs::remove_file(path.join(PROTECTED_FILE)).unwrap();
        fs::remove_dir(path).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn damaged_protected_identity_does_not_fall_back_to_plaintext() {
        let root = protected_test_directory();
        let pair = create_pair(&root).unwrap();
        assert!(Identity::load(&pair.windows_bundle).is_ok());
        let protected = pair.windows_bundle.join(PROTECTED_FILE);
        fs::write(&protected, b"truncated-invalid-dpapi").unwrap();
        assert!(Identity::load(&pair.windows_bundle).is_err());
        fs::remove_file(&protected).unwrap();
        fs::create_dir(&protected).unwrap();
        assert!(Identity::load(&pair.windows_bundle).is_err());
        fs::remove_dir(&protected).unwrap();
        assert!(Identity::load(&pair.windows_bundle).is_ok());
        for directory in [&pair.windows_bundle, &pair.mac_bundle] {
            fs::remove_file(directory.join("identity.json")).unwrap();
            fs::remove_dir(directory).unwrap();
        }
        fs::remove_dir(root).unwrap();
    }

    #[test]
    #[cfg(windows)]
    fn valid_os_protection_does_not_bypass_identity_validation() {
        let (a, _) = identities();
        let path = protected_test_directory();
        a.save_protected(&path).unwrap();
        let file = path.join(PROTECTED_FILE);
        let protected = fs::read(&file).unwrap();
        let opened = crate::secret_store::unprotect(&protected).unwrap();
        for offset in [0, 5, 6, 38, 70, 102] {
            let mut record = Zeroizing::new(opened.as_slice().to_vec());
            if offset == 102 {
                record[102..134].fill(0);
            } else {
                record[offset] ^= 0xff;
            }
            let invalid = crate::secret_store::protect(&record).unwrap();
            fs::write(&file, invalid).unwrap();
            assert!(
                Identity::load(&path).is_err(),
                "invalid field at offset {offset}"
            );
        }
        fs::write(&file, vec![0; 65_537]).unwrap();
        assert!(Identity::load(&path).is_err());
        fs::remove_file(&file).unwrap();
        fs::remove_dir(path).unwrap();
    }

    #[test]
    #[cfg(windows)]
    #[ignore = "opens localhost sockets; uses generated test identities and current-user DPAPI"]
    fn protected_identities_authenticate_over_real_tcp() {
        let (a, b) = identities();
        let first = protected_test_directory();
        let second = protected_test_directory();
        a.save_protected(&first).unwrap();
        b.save_protected(&second).unwrap();
        let (client, server) = tcp_exchange(
            Identity::load(&first).unwrap(),
            Identity::load(&second).unwrap(),
        );
        for directory in [&first, &second] {
            fs::remove_file(directory.join(PROTECTED_FILE)).unwrap();
            fs::remove_dir(directory).unwrap();
        }
        assert_eq!(client.unwrap(), b"accepted");
        server.unwrap();
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
            ensure!(socket.nodelay()?, "Handshake did not enable TCP_NODELAY");
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
            ensure!(socket.nodelay()?, "Handshake did not enable TCP_NODELAY");
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
            let error = result
                .err()
                .expect("Drip-fed handshake unexpectedly succeeded");
            assert!(format!("{error:#}").contains("Read handshake body"));
            assert!(error.downcast_ref::<std::io::Error>().is_some());
            began.elapsed()
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
        let elapsed = worker.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(300),
            "Drip-fed handshake exceeded total deadline tolerance: {elapsed:?}"
        );
    }
}
