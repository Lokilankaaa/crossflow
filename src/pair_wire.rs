//! Versioned records over one pinned, authenticated Noise connection.
//! This transport never grants input authority or retries uncertain input.
use crate::{
    pair_control::{RevokeReason, Side},
    protocol::{Frame, Kind},
    security::{self, Identity, SecureChannel},
    session_metadata::SharedLayout,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::{ErrorKind, Read, Write},
    net::{Shutdown, TcpStream},
    time::{Duration, Instant},
};

/// Envelope version, independent of the existing v4 72-byte input Frame.
pub const PAIR_VERSION: u16 = 1;
pub const MAX_PLAINTEXT: usize = 32 * 1024;
const MAGIC: &[u8; 4] = b"CFP1";
const HEADER: usize = 16;
const SEND_STALL: Duration = Duration::from_millis(30);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    pub native_id: String,
    pub wire_id: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub device_id: String,
    pub inventory: Vec<Display>,
}

pub fn validate_inventory(displays: &[Display]) -> Result<()> {
    ensure!(
        !displays.is_empty() && displays.len() <= 32,
        "inventory must contain 1..32 displays"
    );
    let mut names = HashSet::new();
    let mut tokens = HashSet::new();
    for display in displays {
        ensure!(
            !display.native_id.is_empty()
                && display.native_id.len() <= 256
                && !display.native_id.chars().any(char::is_control),
            "invalid native display identity"
        );
        ensure!(
            display.wire_id != 0
                && names.insert(&display.native_id)
                && tokens.insert(display.wire_id),
            "duplicate native display identity or wire token"
        );
        ensure!(
            [display.x, display.y, display.width, display.height]
                .iter()
                .all(|v| v.is_finite() && v.abs() <= 1_000_000.)
                && display.width >= 1.
                && display.height >= 1.,
            "invalid native display bounds"
        );
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    Hello(Hello),
    Layout {
        revision: u64,
        layout: SharedLayout,
    },
    /// Setup acknowledgement; pair_session validates its phase and revision.
    LayoutReady {
        revision: u64,
    },
    /// Capture and sink preparation complete; does not enable input authority.
    SessionReady {
        revision: u64,
    },
    QueryInventory,
    InventoryDone,
    Enable,
    RequestControl {
        observed_epoch: u64,
    },
    Released {
        barrier_epoch: u64,
    },
    Pause,
    ReleaseFailed,
    Revoke {
        barrier_epoch: u64,
        reason: RevokeReason,
    },
    Grant {
        source: Side,
        owner_epoch: u64,
        layout_revision: u64,
    },
    Paused {
        epoch: u64,
    },
    LayoutAccepted {
        revision: u64,
    },
    Ended {
        reason: RevokeReason,
    },
    Lease {
        token: u64,
    },
    KeepAlive,
    Input {
        source: Side,
        owner_epoch: u64,
        layout_revision: u64,
        frame: Frame,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Received {
    pub sequence: u64,
    pub message: Message,
    pub received_at: Instant,
}

fn identity_text(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn side_byte(side: Side) -> u8 {
    match side {
        Side::A => 0,
        Side::B => 1,
    }
}
fn decode_side(byte: u8) -> Result<Side> {
    match byte {
        0 => Ok(Side::A),
        1 => Ok(Side::B),
        _ => bail!("invalid authenticated side"),
    }
}
fn reason_byte(reason: RevokeReason) -> u8 {
    match reason {
        RevokeReason::Enable => 1,
        RevokeReason::Takeover => 2,
        RevokeReason::Pause => 3,
        RevokeReason::LayoutChange => 4,
        RevokeReason::Disconnect => 5,
        RevokeReason::LeaseExpired => 6,
        RevokeReason::ReleaseFailed { side: Side::A } => 7,
        RevokeReason::ReleaseFailed { side: Side::B } => 8,
    }
}
fn decode_reason(value: u8) -> Result<RevokeReason> {
    Ok(match value {
        1 => RevokeReason::Enable,
        2 => RevokeReason::Takeover,
        3 => RevokeReason::Pause,
        4 => RevokeReason::LayoutChange,
        5 => RevokeReason::Disconnect,
        6 => RevokeReason::LeaseExpired,
        7 => RevokeReason::ReleaseFailed { side: Side::A },
        8 => RevokeReason::ReleaseFailed { side: Side::B },
        _ => bail!("invalid revoke reason"),
    })
}
fn number(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().expect("checked fixed field length"))
}
fn fixed(bytes: &[u8], size: usize) -> Result<()> {
    ensure!(bytes.len() == size, "invalid pair message length");
    Ok(())
}
fn nonzero(value: u64) -> Result<()> {
    ensure!(value != 0, "zero authority/revision/token is invalid");
    Ok(())
}

impl Message {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Hello(hello) => {
                ensure!(identity_text(&hello.device_id), "invalid hello identity");
                validate_inventory(&hello.inventory)?;
            }
            Self::Layout { revision, layout } => {
                nonzero(*revision)?;
                layout.validate()?;
            }
            Self::Released { barrier_epoch } | Self::Revoke { barrier_epoch, .. } => {
                nonzero(*barrier_epoch)?
            }
            Self::Grant {
                owner_epoch,
                layout_revision,
                ..
            } => {
                nonzero(*owner_epoch)?;
                nonzero(*layout_revision)?;
            }
            Self::Paused { epoch } => nonzero(*epoch)?,
            Self::LayoutAccepted { revision } => nonzero(*revision)?,
            Self::LayoutReady { revision } | Self::SessionReady { revision } => nonzero(*revision)?,
            Self::Lease { token } => nonzero(*token)?,
            Self::Input {
                owner_epoch,
                layout_revision,
                frame,
                ..
            } => {
                nonzero(*owner_epoch)?;
                nonzero(*layout_revision)?;
                nonzero(frame.seq)?;
                ensure!(
                    matches!(
                        frame.kind,
                        Kind::Arm
                            | Kind::Move
                            | Kind::Position
                            | Kind::Button
                            | Kind::Key
                            | Kind::Scroll
                            | Kind::Pause
                            | Kind::Stop
                            | Kind::Heartbeat
                            | Kind::RoutingState
                    ),
                    "non-input frame inside duplex input envelope"
                );
                if !matches!(frame.kind, Kind::Pause | Kind::Stop) {
                    nonzero(frame.lease)?;
                }
                frame.encode()?;
            }
            _ => {}
        }
        Ok(())
    }
    fn coordinator_only(&self) -> bool {
        matches!(
            self,
            Self::Revoke { .. }
                | Self::Grant { .. }
                | Self::Paused { .. }
                | Self::LayoutAccepted { .. }
                | Self::Ended { .. }
        )
    }
}

/// Pure codec only. The application must use PairPeer for authenticated IO;
/// decoding this plaintext is not evidence of the claimed source's identity.
pub fn encode_message(sequence: u64, message: &Message) -> Result<Vec<u8>> {
    nonzero(sequence)?;
    message.validate()?;
    let mut bytes = vec![0; HEADER];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..6].copy_from_slice(&PAIR_VERSION.to_be_bytes());
    bytes[8..16].copy_from_slice(&sequence.to_be_bytes());
    bytes[6] = match message {
        Message::Hello(hello) => {
            bytes.extend(serde_json::to_vec(hello)?);
            1
        }
        Message::Layout { revision, layout } => {
            bytes.extend(revision.to_be_bytes());
            bytes.extend(serde_json::to_vec(layout)?);
            2
        }
        Message::Enable => 3,
        Message::RequestControl { observed_epoch } => {
            bytes.extend(observed_epoch.to_be_bytes());
            4
        }
        Message::Released { barrier_epoch } => {
            bytes.extend(barrier_epoch.to_be_bytes());
            5
        }
        Message::Pause => 6,
        Message::ReleaseFailed => 7,
        Message::Revoke {
            barrier_epoch,
            reason,
        } => {
            bytes.extend(barrier_epoch.to_be_bytes());
            bytes.push(reason_byte(*reason));
            8
        }
        Message::Grant {
            source,
            owner_epoch,
            layout_revision,
        } => {
            bytes.push(side_byte(*source));
            bytes.extend(owner_epoch.to_be_bytes());
            bytes.extend(layout_revision.to_be_bytes());
            9
        }
        Message::Paused { epoch } => {
            bytes.extend(epoch.to_be_bytes());
            10
        }
        Message::LayoutAccepted { revision } => {
            bytes.extend(revision.to_be_bytes());
            11
        }
        Message::Ended { reason } => {
            bytes.push(reason_byte(*reason));
            12
        }
        Message::Lease { token } => {
            bytes.extend(token.to_be_bytes());
            13
        }
        Message::KeepAlive => 14,
        Message::Input {
            source,
            owner_epoch,
            layout_revision,
            frame,
        } => {
            bytes.push(side_byte(*source));
            bytes.extend(owner_epoch.to_be_bytes());
            bytes.extend(layout_revision.to_be_bytes());
            bytes.extend(frame.encode()?);
            15
        }
        Message::LayoutReady { revision } => {
            bytes.extend(revision.to_be_bytes());
            16
        }
        Message::SessionReady { revision } => {
            bytes.extend(revision.to_be_bytes());
            17
        }
        Message::QueryInventory => 18,
        Message::InventoryDone => 19,
    };
    ensure!(
        bytes.len() <= MAX_PLAINTEXT,
        "pair message exceeds plaintext bound"
    );
    Ok(bytes)
}

pub fn decode_message(bytes: &[u8]) -> Result<(u64, Message)> {
    ensure!(
        (HEADER..=MAX_PLAINTEXT).contains(&bytes.len()),
        "invalid pair message size"
    );
    ensure!(
        &bytes[..4] == MAGIC && u16::from_be_bytes(bytes[4..6].try_into().unwrap()) == PAIR_VERSION,
        "unsupported duplex envelope version (separate from input frame v4)"
    );
    ensure!(bytes[7] == 0, "pair reserved byte must be zero");
    let sequence = number(&bytes[8..16]);
    nonzero(sequence)?;
    let body = &bytes[HEADER..];
    let message = match bytes[6] {
        1 => Message::Hello(serde_json::from_slice(body)?),
        2 => {
            ensure!(body.len() > 8, "truncated shared layout");
            Message::Layout {
                revision: number(&body[..8]),
                layout: serde_json::from_slice(&body[8..])?,
            }
        }
        3 => {
            fixed(body, 0)?;
            Message::Enable
        }
        4 => {
            fixed(body, 8)?;
            Message::RequestControl {
                observed_epoch: number(body),
            }
        }
        5 => {
            fixed(body, 8)?;
            Message::Released {
                barrier_epoch: number(body),
            }
        }
        6 => {
            fixed(body, 0)?;
            Message::Pause
        }
        7 => {
            fixed(body, 0)?;
            Message::ReleaseFailed
        }
        8 => {
            fixed(body, 9)?;
            Message::Revoke {
                barrier_epoch: number(&body[..8]),
                reason: decode_reason(body[8])?,
            }
        }
        9 => {
            fixed(body, 17)?;
            Message::Grant {
                source: decode_side(body[0])?,
                owner_epoch: number(&body[1..9]),
                layout_revision: number(&body[9..17]),
            }
        }
        10 => {
            fixed(body, 8)?;
            Message::Paused {
                epoch: number(body),
            }
        }
        11 => {
            fixed(body, 8)?;
            Message::LayoutAccepted {
                revision: number(body),
            }
        }
        12 => {
            fixed(body, 1)?;
            Message::Ended {
                reason: decode_reason(body[0])?,
            }
        }
        13 => {
            fixed(body, 8)?;
            Message::Lease {
                token: number(body),
            }
        }
        14 => {
            fixed(body, 0)?;
            Message::KeepAlive
        }
        15 => {
            fixed(body, 17 + crate::protocol::FRAME_LEN)?;
            Message::Input {
                source: decode_side(body[0])?,
                owner_epoch: number(&body[1..9]),
                layout_revision: number(&body[9..17]),
                frame: Frame::decode(&body[17..])?,
            }
        }
        16 => {
            fixed(body, 8)?;
            Message::LayoutReady {
                revision: number(body),
            }
        }
        17 => {
            fixed(body, 8)?;
            Message::SessionReady {
                revision: number(body),
            }
        }
        18 => {
            fixed(body, 0)?;
            Message::QueryInventory
        }
        19 => {
            fixed(body, 0)?;
            Message::InventoryDone
        }
        _ => bail!("unknown pair message kind"),
    };
    message.validate()?;
    Ok((sequence, message))
}

/// One owner, one outbound record. Partial writes are never replaced, merged,
/// retransmitted or carried to another connection. Every IO/protocol failure
/// poisons this instance. Caller must release local owned input on any error.
/// KeepAlive is liveness only; caller enforces authority and receive leases.
pub struct PairPeer {
    socket: TcpStream,
    secure: SecureChannel,
    local_id: String,
    remote_id: String,
    local_side: Side,
    sequence_out: u64,
    sequence_in: u64,
    output: Vec<u8>,
    offset: usize,
    output_started: Instant,
    pending_kind: Option<PendingKind>,
    input: Vec<u8>,
    input_started: Option<Instant>,
    hello_queued: bool,
    hello_sent: bool,
    hello_received: bool,
    layout_sent: bool,
    layout_received: bool,
    terminal: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKind {
    Hello,
    Input,
    Control,
}

impl PairPeer {
    /// Runs only the existing paired Noise handshake, with a total finite
    /// budget. No plaintext compatibility path or automatic reconnect exists.
    /// The legacy Identity role must match this transport direction ("windows"
    /// initiates, "mac" responds); it does not determine input ownership or A/B.
    pub fn handshake(
        mut socket: TcpStream,
        identity: &Identity,
        initiator: bool,
        timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(2),
            "handshake budget must be 0..2 seconds"
        );
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        let secure = security::handshake(&mut socket, identity, initiator)?;
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        let local_id = identity.public_key_hex();
        let remote_id = identity.peer_public_key_hex();
        ensure!(
            local_id != remote_id,
            "paired devices must have distinct identities"
        );
        let local_side = if local_id < remote_id {
            Side::A
        } else {
            Side::B
        };
        Ok(Self {
            socket,
            secure,
            local_id,
            remote_id,
            local_side,
            sequence_out: 0,
            sequence_in: 0,
            output: vec![],
            offset: 0,
            output_started: Instant::now(),
            pending_kind: None,
            input: vec![],
            input_started: None,
            hello_queued: false,
            hello_sent: false,
            hello_received: false,
            layout_sent: false,
            layout_received: false,
            terminal: false,
        })
    }

    /// Symmetric LAN peer handshake using Noise_NNpsk0 (open LAN by default, or
    /// isolated with an optional shared passphrase). No identity files needed.
    pub fn handshake_lan_psk(
        mut socket: TcpStream,
        local_id: &str,
        passphrase: Option<&str>,
        initiator: bool,
        timeout: Duration,
    ) -> Result<Self> {
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(2),
            "handshake budget must be 0..2 seconds"
        );
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        let (secure, remote_id) =
            security::handshake_lan_psk(&mut socket, local_id, passphrase, initiator)?;
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        ensure!(
            local_id != remote_id,
            "peer devices must have distinct identities"
        );
        let local_side = if local_id < remote_id.as_str() {
            Side::A
        } else {
            Side::B
        };
        Ok(Self {
            socket,
            secure,
            local_id: local_id.to_string(),
            remote_id,
            local_side,
            sequence_out: 0,
            sequence_in: 0,
            output: vec![],
            offset: 0,
            output_started: Instant::now(),
            pending_kind: None,
            input: vec![],
            input_started: None,
            hello_queued: false,
            hello_sent: false,
            hello_received: false,
            layout_sent: false,
            layout_received: false,
            terminal: false,
        })
    }
    pub fn local_device_id(&self) -> &str {
        &self.local_id
    }
    pub fn remote_device_id(&self) -> &str {
        &self.remote_id
    }
    pub fn local_side(&self) -> Side {
        self.local_side
    }
    pub fn remote_side(&self) -> Side {
        if self.local_side == Side::A {
            Side::B
        } else {
            Side::A
        }
    }
    pub fn pending(&self) -> bool {
        self.offset < self.output.len()
    }
    pub fn pending_input(&self) -> bool {
        self.pending() && self.pending_kind == Some(PendingKind::Input)
    }
    pub fn terminal(&self) -> bool {
        self.terminal
    }
    pub fn hello_complete(&self) -> bool {
        self.hello_sent && self.hello_received && !self.terminal
    }

    /// A full outbound slot is a terminal overflow, not a retry queue. Inspect
    /// pending() before submitting. Queue acceptance is NOT delivery evidence.
    pub fn queue(&mut self, message: Message) -> Result<()> {
        let result = self.queue_checked(message);
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn queue_checked(&mut self, message: Message) -> Result<()> {
        ensure!(!self.terminal, "pair connection is terminal");
        ensure!(!self.pending(), "pair outbound slot occupied");
        self.validate_direction(&message, true)?;
        let sequence = self
            .sequence_out
            .checked_add(1)
            .context("pair sequence exhausted")?;
        let plain = encode_message(sequence, &message)?;
        let cipher = self.secure.encrypt(&plain)?;
        self.output.clear();
        self.output.extend((cipher.len() as u16).to_be_bytes());
        self.output.extend(cipher);
        self.offset = 0;
        self.output_started = Instant::now();
        self.sequence_out = sequence;
        self.pending_kind = Some(match message {
            Message::Hello(_) => PendingKind::Hello,
            Message::Input { .. } => PendingKind::Input,
            _ => PendingKind::Control,
        });
        if matches!(message, Message::Hello(_)) {
            self.hello_queued = true;
        }
        if matches!(message, Message::Layout { .. }) {
            self.layout_sent = true;
        }
        Ok(())
    }
    fn validate_direction(&self, message: &Message, outbound: bool) -> Result<()> {
        let side = if outbound {
            self.local_side()
        } else {
            self.remote_side()
        };
        let identity = if outbound {
            self.local_device_id()
        } else {
            self.remote_device_id()
        };
        if let Message::Hello(hello) = message {
            ensure!(
                !(if outbound {
                    self.hello_queued
                } else {
                    self.hello_received
                }),
                "duplicate hello"
            );
            ensure!(
                hello.device_id == identity,
                "hello does not match authenticated identity"
            );
        } else {
            ensure!(
                self.hello_sent && self.hello_received,
                "both authenticated hellos must precede other records"
            );
        }
        if message.coordinator_only() {
            ensure!(side == Side::A, "authority record from non-coordinator");
        }
        if let Message::Input { source, .. } = message {
            ensure!(
                *source == side,
                "input source does not match authenticated side"
            );
        }
        if let Message::Layout { layout, .. } = message {
            ensure!(
                !(if outbound {
                    self.layout_sent
                } else {
                    self.layout_received
                }),
                "shared layout may be supplied once per direction"
            );
            let ids = [
                layout.source_device_id.as_str(),
                layout.receiver_device_id.as_str(),
            ];
            ensure!(
                ids.contains(&self.local_device_id()) && ids.contains(&self.remote_device_id()),
                "layout contains unauthenticated devices"
            );
        }
        Ok(())
    }

    /// Nonblocking bounded work: <=32KiB read, <=64 decrypted records, <=64KiB
    /// retained backlog. Partial inbound records must complete within 150ms.
    pub fn pump(&mut self) -> Result<Vec<Received>> {
        let result = self.pump_checked();
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn pump_checked(&mut self) -> Result<Vec<Received>> {
        self.flush_checked()?;
        self.read_checked()
    }
    /// Complete only the current record; useful for a final query response.
    /// This has the same bounded send deadline and never reads/discards replies.
    pub fn flush_pending(&mut self) -> Result<()> {
        let result = self.flush_checked();
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn flush_checked(&mut self) -> Result<()> {
        ensure!(!self.terminal, "pair connection is terminal");
        while self.pending() {
            ensure!(
                self.output_started.elapsed() < SEND_STALL,
                "pair send stalled; pending input discarded"
            );
            match self.socket.write(&self.output[self.offset..]) {
                Ok(0) => bail!("pair disconnected during write"),
                Ok(size) => self.offset += size,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        if !self.pending() {
            if self.pending_kind == Some(PendingKind::Hello) {
                self.hello_sent = true;
            }
            self.pending_kind = None;
            self.output.clear();
            self.offset = 0;
        }
        Ok(())
    }
    fn read_checked(&mut self) -> Result<Vec<Received>> {
        let mut bytes = [0; 4096];
        for _ in 0..8 {
            match self.socket.read(&mut bytes) {
                Ok(0) => bail!("pair disconnected; unprocessed records discarded"),
                Ok(size) => {
                    self.input_started.get_or_insert_with(Instant::now);
                    self.input.extend_from_slice(&bytes[..size]);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(
            self.input.len() <= 65536,
            "pair receive backlog exceeded bound"
        );
        ensure!(
            self.input_started
                .is_none_or(|at| at.elapsed() < Duration::from_millis(150)),
            "pair receive record stalled"
        );
        let received_at = Instant::now();
        let mut messages = vec![];
        let mut consumed = 0;
        while self.input.len() - consumed >= 2 && messages.len() < 64 {
            let size = usize::from(u16::from_be_bytes(
                self.input[consumed..consumed + 2].try_into().unwrap(),
            ));
            ensure!(
                (HEADER + 16..=MAX_PLAINTEXT + 16).contains(&size),
                "pair ciphertext length invalid"
            );
            if self.input.len() - consumed < size + 2 {
                break;
            }
            let plain = self
                .secure
                .decrypt(&self.input[consumed + 2..consumed + 2 + size])?;
            let (sequence, message) = decode_message(&plain)?;
            ensure!(
                self.sequence_in.checked_add(1) == Some(sequence),
                "duplicate or reordered pair record"
            );
            self.validate_direction(&message, false)?;
            self.sequence_in = sequence;
            if matches!(message, Message::Hello(_)) {
                self.hello_received = true;
            }
            if matches!(message, Message::Layout { .. }) {
                self.layout_received = true;
            }
            messages.push(Received {
                sequence,
                message,
                received_at,
            });
            consumed += size + 2;
        }
        self.input.drain(..consumed);
        if self.input.is_empty() {
            self.input_started = None;
        }
        Ok(messages)
    }
    /// Terminal local stop. Never flushes pending bytes or recreates a channel.
    pub fn abort(&mut self) {
        self.terminal = true;
        self.output.clear();
        self.offset = 0;
        self.pending_kind = None;
        self.input.clear();
        self.input_started = None;
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}
impl Drop for PairPeer {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{input::Button, session::MouseSink, session_metadata::SharedScreen};
    use std::{
        fs,
        net::TcpListener,
        path::PathBuf,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn hello(id: &str) -> Hello {
        Hello {
            device_id: id.into(),
            inventory: vec![Display {
                native_id: "display-primary".into(),
                wire_id: 7,
                x: -100.,
                y: 0.,
                width: 1920.,
                height: 1080.,
            }],
        }
    }
    fn layout(a: &str, b: &str) -> SharedLayout {
        SharedLayout {
            schema: 1,
            source_device_id: a.into(),
            receiver_device_id: b.into(),
            screens: vec![
                SharedScreen {
                    id: "a-screen".into(),
                    device_id: a.into(),
                    native_id: "display-primary".into(),
                    x: 0.,
                    y: 0.,
                    width: 1920.,
                    height: 1080.,
                },
                SharedScreen {
                    id: "b-screen".into(),
                    device_id: b.into(),
                    native_id: "display-primary".into(),
                    x: 1920.,
                    y: 0.,
                    width: 1920.,
                    height: 1080.,
                },
            ],
        }
    }
    fn action(side: Side, seq: u64, kind: Kind) -> Message {
        let mut frame = Frame::new(kind);
        frame.seq = seq;
        frame.lease = 11;
        match kind {
            Kind::Position => {
                frame.display_id = 7;
                frame.x = 10.;
                frame.y = 20.;
            }
            Kind::Key => {
                frame.code = 0x04;
                frame.down = true;
            }
            _ => {}
        }
        Message::Input {
            source: side,
            owner_epoch: if side == Side::A { 1 } else { 2 },
            layout_revision: 7,
            frame,
        }
    }
    fn examples() -> Vec<Message> {
        vec![
            Message::Hello(hello(&"a".repeat(64))),
            Message::Layout {
                revision: 7,
                layout: layout(&"a".repeat(64), &"b".repeat(64)),
            },
            Message::Enable,
            Message::RequestControl { observed_epoch: 0 },
            Message::Released { barrier_epoch: 1 },
            Message::Pause,
            Message::ReleaseFailed,
            Message::Revoke {
                barrier_epoch: 2,
                reason: RevokeReason::Takeover,
            },
            Message::Grant {
                source: Side::B,
                owner_epoch: 2,
                layout_revision: 7,
            },
            Message::Paused { epoch: 2 },
            Message::LayoutAccepted { revision: 7 },
            Message::Ended {
                reason: RevokeReason::ReleaseFailed { side: Side::B },
            },
            Message::Lease { token: 11 },
            Message::KeepAlive,
            Message::LayoutReady { revision: 7 },
            Message::SessionReady { revision: 7 },
            action(Side::A, 1, Kind::Position),
            action(Side::B, 2, Kind::Key),
        ]
    }
    #[test]
    fn codec_roundtrips_all_strict_typed_messages_separate_from_v4_input() {
        for message in examples() {
            let bytes = encode_message(123, &message).unwrap();
            assert_eq!(&bytes[..4], b"CFP1");
            assert_eq!(&bytes[4..6], &PAIR_VERSION.to_be_bytes());
            assert_eq!(decode_message(&bytes).unwrap(), (123, message));
            assert!(decode_message(&bytes[..HEADER - 1]).is_err());
            let mut invalid = bytes.clone();
            invalid[4..6].copy_from_slice(&4u16.to_be_bytes());
            assert!(decode_message(&invalid).is_err());
            invalid = bytes.clone();
            invalid[7] = 1;
            assert!(decode_message(&invalid).is_err());
            invalid = bytes.clone();
            invalid[6] = 255;
            assert!(decode_message(&invalid).is_err());
            invalid = bytes.clone();
            invalid[8..16].fill(0);
            assert!(decode_message(&invalid).is_err());
        }
        assert!(decode_message(&Frame::new(Kind::Pause).encode().unwrap()).is_err());
        for message in examples()
            .into_iter()
            .filter(|m| !matches!(m, Message::Hello(_) | Message::Layout { .. }))
        {
            let mut bytes = encode_message(1, &message).unwrap();
            bytes.push(0);
            assert!(decode_message(&bytes).is_err());
        }
    }
    #[test]
    fn malformed_inventory_layout_input_and_unknown_json_fields_are_rejected() {
        let good = hello(&"a".repeat(64));
        let mut bad = good.clone();
        bad.inventory[0].wire_id = 0;
        assert!(encode_message(1, &Message::Hello(bad)).is_err());
        let mut bad = good.clone();
        bad.inventory = vec![bad.inventory[0].clone(); 33];
        assert!(validate_inventory(&bad.inventory).is_err());
        for invalid in ["", "bad\nname"] {
            let mut bad = good.clone();
            bad.inventory[0].native_id = invalid.into();
            assert!(validate_inventory(&bad.inventory).is_err());
        }
        for invalid in [0., 0.5, f64::INFINITY, f64::NAN] {
            let mut bad = good.clone();
            bad.inventory[0].width = invalid;
            assert!(validate_inventory(&bad.inventory).is_err());
        }
        let mut bad = good.clone();
        bad.inventory.push(bad.inventory[0].clone());
        assert!(validate_inventory(&bad.inventory).is_err());
        let mut bytes = encode_message(1, &Message::Hello(good.clone())).unwrap();
        let mut body = serde_json::to_value(&good).unwrap();
        body["untrusted_extra"] = true.into();
        bytes.truncate(HEADER);
        bytes.extend(serde_json::to_vec(&body).unwrap());
        assert!(decode_message(&bytes).is_err());
        let mut body = serde_json::to_value(&good).unwrap();
        body["inventory"][0]["extra"] = true.into();
        bytes.truncate(HEADER);
        bytes.extend(serde_json::to_vec(&body).unwrap());
        assert!(decode_message(&bytes).is_err());
        assert!(decode_message(&vec![0; MAX_PLAINTEXT + 1]).is_err());
        for message in [
            Message::Lease { token: 0 },
            Message::Grant {
                source: Side::A,
                owner_epoch: 0,
                layout_revision: 7,
            },
            Message::Released { barrier_epoch: 0 },
            Message::Layout {
                revision: 0,
                layout: layout(&"a".repeat(64), &"b".repeat(64)),
            },
            action(Side::A, 1, Kind::Lease),
        ] {
            assert!(encode_message(1, &message).is_err());
        }
        let mut bytes = encode_message(1, &action(Side::A, 1, Kind::Position)).unwrap();
        bytes[HEADER] = 2;
        assert!(decode_message(&bytes).is_err());
        let mut bytes = encode_message(1, &action(Side::A, 1, Kind::Position)).unwrap();
        bytes[HEADER + 17 + 4] = 3;
        assert!(decode_message(&bytes).is_err());
    }

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn peers(reverse: bool) -> Result<(PairPeer, PairPeer, Fixture)> {
        let path = std::env::current_dir()?.join("runs").join(format!(
            "pair-wire-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let fixture = Fixture(path);
        security::create_pair(&fixture.0)?;
        let mut first = Identity::load(&fixture.0.join(if reverse { "mac" } else { "windows" }))?;
        let mut second = Identity::load(&fixture.0.join(if reverse { "windows" } else { "mac" }))?;
        if (first.public_key_hex() < second.public_key_hex()) == reverse {
            std::mem::swap(&mut first, &mut second);
        }
        // Existing identity.role names encode TCP roles, not OS or input
        // ownership. Change these test-only in-memory role labels while keeping
        // exactly the same pinned keys, pair id, and Noise verification path.
        first.role = "windows".into();
        second.role = "mac".into();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let worker = thread::spawn(move || -> Result<PairPeer> {
            let (socket, _) = listener.accept()?;
            PairPeer::handshake(socket, &second, false, Duration::from_secs(2))
        });
        let first = PairPeer::handshake(
            TcpStream::connect(address)?,
            &first,
            true,
            Duration::from_secs(2),
        )?;
        assert_eq!(first.local_side(), if reverse { Side::B } else { Side::A });
        let second = worker.join().expect("handshake worker panicked")?;
        if first.local_side() == Side::A {
            Ok((first, second, fixture))
        } else {
            Ok((second, first, fixture))
        }
    }
    fn exchange_hello(a: &mut PairPeer, b: &mut PairPeer) -> Result<()> {
        a.queue(Message::Hello(hello(a.local_device_id())))?;
        b.queue(Message::Hello(hello(b.local_device_id())))?;
        let deadline = Instant::now() + Duration::from_secs(1);
        while !a.hello_complete() || !b.hello_complete() {
            for received in a.pump()?.into_iter().chain(b.pump()?) {
                ensure!(matches!(received.message, Message::Hello(_)));
            }
            ensure!(Instant::now() < deadline, "hello exchange timed out");
            thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    fn transfer(from: &mut PairPeer, to: &mut PairPeer, message: Message) -> Result<Received> {
        from.queue(message.clone())?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            ensure!(from.pump()?.is_empty(), "unexpected reverse message");
            let mut received = to.pump()?;
            if !received.is_empty() {
                ensure!(
                    received.len() == 1 && received[0].message == message,
                    "record order/content changed"
                );
                return Ok(received.remove(0));
            }
            ensure!(Instant::now() < deadline, "record delivery timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn raw_record(peer: &mut PairPeer, plain: &[u8], corrupt: bool) -> Result<()> {
        let mut cipher = peer.secure.encrypt(plain)?;
        if corrupt {
            *cipher.last_mut().unwrap() ^= 1;
        }
        let mut record = Vec::new();
        record.extend((cipher.len() as u16).to_be_bytes());
        record.extend(cipher);
        peer.socket.set_nonblocking(false)?;
        peer.socket
            .set_write_timeout(Some(Duration::from_secs(1)))?;
        peer.socket.write_all(&record)?;
        peer.socket.set_nonblocking(true)?;
        Ok(())
    }
    fn expect_terminal(peer: &mut PairPeer) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match peer.pump() {
                Err(_) => {
                    ensure!(peer.terminal());
                    ensure!(peer.pump().is_err());
                    return Ok(());
                }
                Ok(messages) => ensure!(messages.is_empty(), "invalid record escaped validation"),
            }
            ensure!(Instant::now() < deadline, "invalid peer remained connected");
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[derive(Default)]
    struct MockSink(Vec<&'static str>);
    impl MouseSink for MockSink {
        fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
            self.0.push("move");
            Ok((x, y))
        }
        fn position(&mut self, x: f64, y: f64, _id: u32) -> Result<(f64, f64)> {
            self.0.push("position");
            Ok((x, y))
        }
        fn key(&mut self, _usage: u16, _down: bool) -> Result<()> {
            self.0.push("key");
            Ok(())
        }
        fn button(&mut self, _button: Button, _down: bool) -> Result<()> {
            self.0.push("button");
            Ok(())
        }
        fn release_owned(&mut self) -> Result<()> {
            self.0.push("release");
            Ok(())
        }
    }
    fn apply_mock(sink: &mut MockSink, received: Received) -> Result<()> {
        let Message::Input { frame, .. } = received.message else {
            bail!("expected input");
        };
        match frame.kind {
            Kind::Position => {
                sink.position(frame.x, frame.y, frame.display_id)?;
            }
            Kind::Key => sink.key(frame.code, frame.down)?,
            Kind::Pause => sink.release_owned()?,
            _ => bail!("unexpected mock action"),
        }
        Ok(())
    }
    #[test]
    #[ignore = "opens encrypted localhost sockets; mock sink only"]
    fn authenticated_one_socket_duplex_preserves_input_control_order_for_both_sources() -> Result<()>
    {
        for reverse in [false, true] {
            let (mut a, mut b, _fixture) = peers(reverse)?;
            assert_eq!(a.local_side(), Side::A);
            assert_eq!(b.local_side(), Side::B);
            assert_eq!(a.local_device_id(), b.remote_device_id());
            exchange_hello(&mut a, &mut b)?;
            let shared = layout(a.local_device_id(), b.local_device_id());
            transfer(
                &mut a,
                &mut b,
                Message::Layout {
                    revision: 7,
                    layout: shared,
                },
            )?;
            transfer(&mut b, &mut a, Message::Enable)?;
            transfer(
                &mut a,
                &mut b,
                Message::Revoke {
                    barrier_epoch: 1,
                    reason: RevokeReason::Enable,
                },
            )?;
            transfer(&mut b, &mut a, Message::Released { barrier_epoch: 1 })?;
            transfer(
                &mut a,
                &mut b,
                Message::Grant {
                    source: Side::A,
                    owner_epoch: 1,
                    layout_revision: 7,
                },
            )?;
            let mut target_b = MockSink::default();
            let mut target_a = MockSink::default();
            apply_mock(
                &mut target_b,
                transfer(&mut a, &mut b, action(Side::A, 1, Kind::Position))?,
            )?;
            apply_mock(
                &mut target_b,
                transfer(&mut a, &mut b, action(Side::A, 2, Kind::Key))?,
            )?;
            apply_mock(
                &mut target_b,
                transfer(&mut a, &mut b, action(Side::A, 3, Kind::Pause))?,
            )?;
            transfer(
                &mut b,
                &mut a,
                Message::RequestControl { observed_epoch: 1 },
            )?;
            transfer(
                &mut a,
                &mut b,
                Message::Revoke {
                    barrier_epoch: 2,
                    reason: RevokeReason::Takeover,
                },
            )?;
            transfer(&mut b, &mut a, Message::Released { barrier_epoch: 2 })?;
            transfer(
                &mut a,
                &mut b,
                Message::Grant {
                    source: Side::B,
                    owner_epoch: 2,
                    layout_revision: 7,
                },
            )?;
            apply_mock(
                &mut target_a,
                transfer(&mut b, &mut a, action(Side::B, 1, Kind::Position))?,
            )?;
            apply_mock(
                &mut target_a,
                transfer(&mut b, &mut a, action(Side::B, 2, Kind::Key))?,
            )?;
            apply_mock(
                &mut target_a,
                transfer(&mut b, &mut a, action(Side::B, 3, Kind::Pause))?,
            )?;
            assert_eq!(target_a.0, vec!["position", "key", "release"]);
            assert_eq!(target_b.0, target_a.0);
            transfer(&mut b, &mut a, Message::KeepAlive)?;
            transfer(&mut a, &mut b, Message::Lease { token: 99 })?;
            assert!(!a.pending() && !b.pending());
        }
        Ok(())
    }
    #[test]
    #[ignore = "opens encrypted localhost sockets; malformed authenticated records only"]
    fn authenticated_malformed_wrong_role_source_and_reordered_records_fail_closed() -> Result<()> {
        for case in 0..7 {
            let (mut a, mut b, _fixture) = peers(false)?;
            exchange_hello(&mut a, &mut b)?;
            let mut message = match case {
                0 => action(Side::A, 1, Kind::Position),
                1 => Message::Grant {
                    source: Side::B,
                    owner_epoch: 1,
                    layout_revision: 7,
                },
                2 => Message::Hello(hello(b.local_device_id())),
                _ => Message::KeepAlive,
            };
            if case == 5 {
                message = Message::Layout {
                    revision: 7,
                    layout: layout(&"a".repeat(64), &"b".repeat(64)),
                };
            }
            let seq = b.sequence_out + if case == 3 { 2 } else { 1 };
            let mut plain = encode_message(seq, &message)?;
            if case == 6 {
                plain[7] = 1;
            }
            raw_record(&mut b, &plain, case == 4)?;
            expect_terminal(&mut a)?;
        }
        Ok(())
    }
    #[test]
    #[ignore = "opens encrypted localhost sockets; malformed hello only"]
    fn identity_bound_hello_is_required_before_control_and_cannot_be_duplicated() -> Result<()> {
        for case in 0..3 {
            let (mut a, mut b, _fixture) = peers(false)?;
            let message = match case {
                0 => Message::KeepAlive,
                1 => Message::Hello(hello(a.local_device_id())),
                _ => Message::Hello(hello(b.local_device_id())),
            };
            // Even valid peer identity does not allow Grant before BOTH hellos.
            raw_record(&mut b, &encode_message(1, &message)?, false)?;
            if case == 2 {
                let end = Instant::now() + Duration::from_secs(1);
                loop {
                    if !a.pump()?.is_empty() {
                        break;
                    }
                    ensure!(Instant::now() < end);
                    thread::sleep(Duration::from_millis(1));
                }
                raw_record(
                    &mut b,
                    &encode_message(
                        2,
                        &Message::Grant {
                            source: Side::A,
                            owner_epoch: 1,
                            layout_revision: 7,
                        },
                    )?,
                    false,
                )?;
            }
            expect_terminal(&mut a)?;
        }
        Ok(())
    }
    #[test]
    #[ignore = "opens encrypted localhost sockets; no input injection"]
    fn queue_overflow_stop_and_send_stall_discard_unsent_input_without_replay() -> Result<()> {
        for case in 0..3 {
            let (mut a, mut b, _fixture) = peers(false)?;
            exchange_hello(&mut a, &mut b)?;
            a.queue(action(Side::A, 1, Kind::Position))?;
            assert!(a.pending_input());
            match case {
                0 => {
                    assert!(a.queue(Message::Pause).is_err());
                }
                1 => a.abort(),
                _ => {
                    a.output_started = Instant::now() - SEND_STALL;
                    assert!(a.pump().is_err());
                }
            }
            assert!(a.terminal());
            assert!(!a.pending());
            expect_terminal(&mut b)?;
        }
        Ok(())
    }
    #[test]
    #[ignore = "opens encrypted localhost sockets; partial records only"]
    fn oversized_or_incomplete_records_have_bounded_terminal_failure() -> Result<()> {
        for oversized in [false, true] {
            let (mut a, mut b, _fixture) = peers(false)?;
            exchange_hello(&mut a, &mut b)?;
            b.socket.set_nonblocking(false)?;
            b.socket
                .write_all(if oversized { &[0xff, 0xff] } else { &[0] })?;
            b.socket.set_nonblocking(true)?;
            if !oversized {
                let end = Instant::now() + Duration::from_secs(1);
                while a.input.is_empty() {
                    assert!(a.pump()?.is_empty());
                    ensure!(Instant::now() < end);
                    thread::sleep(Duration::from_millis(1));
                }
                a.input_started = Some(Instant::now() - Duration::from_millis(151));
            }
            expect_terminal(&mut a)?;
        }
        Ok(())
    }
}
