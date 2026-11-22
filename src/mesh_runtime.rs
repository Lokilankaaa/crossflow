//! Symmetric N-device Full Mesh runtime:
//! - Encrypted (`Noise_NNpsk0` + ChaCha20-Poly1305) non-blocking TCP transport per peer pair
//! - Concurrent inbound accept + background outbound dial (zero blocking on the 1000Hz loop)
//! - Deterministic simultaneous-dial tie-breaking (`local_id < remote_id`)
//! - 2D screen graph (`MeshLayoutBook`) supporting arbitrary N devices, multi-edge contacts & cycles,
//!   remembered screen positions per device, and Lamport revision gossip
//! - Direct 1-hop input routing (`A -> B -> C` without relay)

use crate::{
    arrangement::{Layout, Screen},
    pair_wire::Display,
    protocol::{FRAME_LEN, Frame},
    screen_runtime::{Binding, NativeRect, RuntimeLayout},
    security::{self, SecureChannel},
    session_metadata::{SharedLayout, SharedScreen},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs,
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpStream},
    path::Path,
    time::{Duration, Instant},
};

pub const MESH_MAX_RECORD_CIPHERTEXT: usize = 64 * 1024;
const TAG_FRAME: u8 = 1;
const TAG_CONTROL: u8 = 2;
const MAX_OUT_QUEUE: usize = 512;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
pub const PEER_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
pub const PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(9);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MeshControl {
    Hello {
        name: String,
        platform: String,
        displays: Vec<Display>,
        layout_revision: u64,
        #[serde(default)]
        layout_screens: Vec<SharedScreen>,
    },
    LayoutSync {
        revision: u64,
        screens: Vec<SharedScreen>,
    },
    FocusState {
        focus_device_id: Option<String>,
        focus_screen_id: Option<String>,
    },
    RevokeControl,
    KeepAlive,
}

#[derive(Debug)]
pub enum MeshPacket {
    Input(Frame),
    Control(MeshControl),
}

pub struct HandshakedMeshPeer {
    pub conn: MeshPeerConn,
    pub remote_device_id: String,
    pub peer_addr: SocketAddr,
    pub initiator: bool,
    pub name: String,
    pub platform: String,
    pub displays: Vec<Display>,
    pub layout_revision: u64,
    pub layout_screens: Vec<SharedScreen>,
}

/// Returns true ONLY for real Noise AEAD decryption / passphrase mismatch errors,
/// NEVER for TCP connect failures, busy timeouts, or connection resets.
pub fn is_passphrase_mismatch_error(error: &anyhow::Error) -> bool {
    let msg = format!("{error:#}");
    let lower = msg.to_ascii_lowercase();
    if lower.contains("timed out")
        || lower.contains("wouldblock")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("unexpected eof")
        || lower.contains("broken pipe")
        || lower.contains("aborted")
    {
        return false;
    }
    lower.contains("passphrase")
        || lower.contains("authenticate")
        || lower.contains("decrypt")
}

/// Deterministic tie-breaker when two peers dial each other simultaneously:
/// keep the connection initiated by the lexicographically smaller device ID.
pub fn preferred_initiator(local_id: &str, remote_id: &str) -> bool {
    local_id < remote_id
}

fn write_encrypted_blocking(
    stream: &mut TcpStream,
    ch: &mut SecureChannel,
    plaintext: &[u8],
) -> Result<()> {
    let ct = ch.encrypt(plaintext)?;
    ensure!(
        ct.len() <= MESH_MAX_RECORD_CIPHERTEXT,
        "mesh record exceeds ciphertext limit"
    );
    let len = (ct.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&ct)?;
    stream.flush()?;
    Ok(())
}

fn read_encrypted_blocking(stream: &mut TcpStream, ch: &mut SecureChannel) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes(hdr) as usize;
    ensure!(
        (16..=MESH_MAX_RECORD_CIPHERTEXT).contains(&len),
        "invalid mesh record length"
    );
    let mut ct = vec![0u8; len];
    stream.read_exact(&mut ct)?;
    ch.decrypt(&ct)
}

fn encode_control_plaintext(ctrl: &MeshControl) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(ctrl)?;
    let mut plain = Vec::with_capacity(1 + body.len());
    plain.push(TAG_CONTROL);
    plain.extend_from_slice(&body);
    Ok(plain)
}

fn decode_packet(plain: &[u8]) -> Result<MeshPacket> {
    let (&tag, body) = plain
        .split_first()
        .context("empty mesh plaintext record")?;
    match tag {
        TAG_FRAME => {
            ensure!(body.len() == FRAME_LEN, "invalid mesh input frame length");
            let mut raw = [0u8; FRAME_LEN];
            raw.copy_from_slice(body);
            Ok(MeshPacket::Input(Frame::decode(&raw)?))
        }
        TAG_CONTROL => {
            let ctrl: MeshControl = serde_json::from_slice(body)?;
            Ok(MeshPacket::Control(ctrl))
        }
        _ => bail!("unknown mesh packet tag"),
    }
}

/// Perform Noise_NNpsk0 handshake and exchange `MeshControl::Hello` on a newly
/// accepted or dialed TCP stream, returning a ready non-blocking `HandshakedMeshPeer`.
#[allow(clippy::too_many_arguments)]
pub fn establish_mesh_peer(
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    initiator: bool,
    local_device_id: &str,
    local_name: &str,
    local_displays: Vec<Display>,
    passphrase: Option<&str>,
    layout_revision: u64,
    layout_screens: Vec<SharedScreen>,
) -> Result<HandshakedMeshPeer> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;

    let (mut ch, remote_device_id) =
        security::handshake_lan_psk(&mut stream, local_device_id, passphrase, initiator)?;
    ensure!(
        remote_device_id != local_device_id,
        "mesh loopback self-connection rejected"
    );

    let local_hello = encode_control_plaintext(&MeshControl::Hello {
        name: local_name.to_string(),
        platform: std::env::consts::OS.to_string(),
        displays: local_displays,
        layout_revision,
        layout_screens,
    })?;

    let peer_plain = if initiator {
        write_encrypted_blocking(&mut stream, &mut ch, &local_hello)?;
        read_encrypted_blocking(&mut stream, &mut ch)?
    } else {
        let p = read_encrypted_blocking(&mut stream, &mut ch)?;
        write_encrypted_blocking(&mut stream, &mut ch, &local_hello)?;
        p
    };

    let MeshPacket::Control(MeshControl::Hello {
        name,
        platform,
        displays,
        layout_revision: peer_rev,
        layout_screens: peer_screens,
    }) = decode_packet(&peer_plain)?
    else {
        bail!("expected MeshControl::Hello after handshake");
    };
    crate::pair_wire::validate_inventory(&displays)?;

    stream.set_nonblocking(true)?;
    let conn = MeshPeerConn::new(stream, ch);
    Ok(HandshakedMeshPeer {
        conn,
        remote_device_id,
        peer_addr,
        initiator,
        name: if name.trim().is_empty() {
            "局域网设备".into()
        } else {
            name.trim().chars().take(48).collect()
        },
        platform,
        displays,
        layout_revision: peer_rev,
        layout_screens: peer_screens,
    })
}

/// Non-blocking encrypted framed TCP peer connection.
pub struct MeshPeerConn {
    stream: TcpStream,
    ch: SecureChannel,
    out_queue: VecDeque<Vec<u8>>,
    out_offset: usize,
    in_buf: Vec<u8>,
    last_rx: Instant,
    last_tx: Instant,
}

impl MeshPeerConn {
    fn new(stream: TcpStream, ch: SecureChannel) -> Self {
        let now = Instant::now();
        Self {
            stream,
            ch,
            out_queue: VecDeque::with_capacity(64),
            out_offset: 0,
            in_buf: Vec::with_capacity(4096),
            last_rx: now,
            last_tx: now,
        }
    }

    pub fn last_rx_elapsed(&self) -> Duration {
        self.last_rx.elapsed()
    }

    pub fn last_tx_elapsed(&self) -> Duration {
        self.last_tx.elapsed()
    }

    fn queue_plaintext(&mut self, plaintext: &[u8]) -> Result<()> {
        ensure!(
            self.out_queue.len() < MAX_OUT_QUEUE,
            "mesh peer outbound queue overflow"
        );
        let ct = self.ch.encrypt(plaintext)?;
        ensure!(
            ct.len() <= MESH_MAX_RECORD_CIPHERTEXT,
            "mesh record exceeds ciphertext limit"
        );
        let mut record = Vec::with_capacity(4 + ct.len());
        record.extend_from_slice(&(ct.len() as u32).to_be_bytes());
        record.extend_from_slice(&ct);
        self.out_queue.push_back(record);
        Ok(())
    }

    pub fn queue_frame(&mut self, frame: &Frame) -> Result<()> {
        let encoded = frame.encode()?;
        let mut plain = [0u8; 1 + FRAME_LEN];
        plain[0] = TAG_FRAME;
        plain[1..].copy_from_slice(&encoded);
        self.queue_plaintext(&plain)
    }

    pub fn queue_control(&mut self, ctrl: &MeshControl) -> Result<()> {
        let plain = encode_control_plaintext(ctrl)?;
        self.queue_plaintext(&plain)
    }

    pub fn flush(&mut self) -> Result<()> {
        while let Some(front) = self.out_queue.front() {
            match self.stream.write(&front[self.out_offset..]) {
                Ok(0) => bail!("mesh peer closed during write"),
                Ok(n) => {
                    self.out_offset += n;
                    self.last_tx = Instant::now();
                    if self.out_offset >= front.len() {
                        self.out_queue.pop_front();
                        self.out_offset = 0;
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Flush pending outbound bytes and read all complete inbound decrypted packets.
    pub fn pump(&mut self) -> Result<Vec<MeshPacket>> {
        self.flush()?;
        let mut tmp = [0u8; 8192];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => bail!("mesh peer disconnected"),
                Ok(n) => {
                    self.last_rx = Instant::now();
                    self.in_buf.extend_from_slice(&tmp[..n]);
                    if self.in_buf.len() > MESH_MAX_RECORD_CIPHERTEXT * 4 {
                        bail!("mesh inbound buffer exceeded safety limit");
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let mut packets = Vec::new();
        while self.in_buf.len() >= 4 {
            let len = u32::from_be_bytes([
                self.in_buf[0],
                self.in_buf[1],
                self.in_buf[2],
                self.in_buf[3],
            ]) as usize;
            ensure!(
                (16..=MESH_MAX_RECORD_CIPHERTEXT).contains(&len),
                "invalid inbound mesh record length"
            );
            if self.in_buf.len() < 4 + len {
                break;
            }
            let ct = self.in_buf[4..4 + len].to_vec();
            self.in_buf.drain(..4 + len);
            let plain = self.ch.decrypt(&ct)?;
            packets.push(decode_packet(&plain)?);
        }
        Ok(packets)
    }
}

// ---------------------------------------------------------------------------
// MeshLayoutBook: N-device 2D screen graph with cycle support & Lamport gossip
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SavedMeshLayoutDisk {
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    saved_positions: HashMap<String, (f64, f64)>,
}

pub fn validate_mesh_screens(screens: &[SharedScreen]) -> Result<()> {
    ensure!(
        !screens.is_empty() && screens.len() <= 64,
        "mesh layout must contain 1..64 screens"
    );
    let mut native = HashSet::new();
    for s in screens {
        ensure!(
            !s.id.is_empty()
                && s.id.len() <= 128
                && !s.device_id.is_empty()
                && !s.native_id.is_empty()
                && s.native_id.len() <= 256
                && !s.id.chars().any(char::is_control)
                && !s.native_id.chars().any(char::is_control),
            "invalid mesh screen identity"
        );
        ensure!(
            s.width >= 1.0
                && s.height >= 1.0
                && [s.x, s.y, s.width, s.height]
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= 1_000_000.0),
            "invalid mesh screen geometry"
        );
        ensure!(
            native.insert((&s.device_id, &s.native_id)),
            "duplicate device display in mesh layout"
        );
    }
    Layout::new(
        screens
            .iter()
            .map(|s| Screen {
                id: s.id.clone(),
                device_id: s.device_id.clone(),
                x: s.x,
                y: s.y,
                width: s.width,
                height: s.height,
            })
            .collect(),
    )?;
    Ok(())
}

fn pos_key(device_id: &str, native_id: &str) -> String {
    format!("{device_id}:{native_id}")
}

fn canonical_screen_id(device_id: &str, wire_id: u32) -> String {
    let prefix = &device_id[..device_id.len().min(12)];
    format!("dev-{prefix}-d{wire_id}")
}

pub struct MeshLayoutBook {
    revision: u64,
    saved_positions: HashMap<String, (f64, f64)>,
    active_screens: Vec<SharedScreen>,
}

impl MeshLayoutBook {
    pub fn load(path: &Path) -> Self {
        let disk = fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SavedMeshLayoutDisk>(&b).ok())
            .unwrap_or_default();
        Self {
            revision: disk.revision.max(1),
            saved_positions: disk.saved_positions,
            active_screens: Vec::new(),
        }
    }

    pub fn save(&self, path: &Path) {
        let disk = SavedMeshLayoutDisk {
            revision: self.revision,
            saved_positions: self.saved_positions.clone(),
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(&disk) {
            let _ = fs::write(path, bytes);
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn active_screens(&self) -> &[SharedScreen] {
        &self.active_screens
    }

    pub fn to_shared_layout(&self, local_device_id: &str, first_peer_id: &str) -> Option<SharedLayout> {
        if self.active_screens.is_empty() {
            return None;
        }
        Some(SharedLayout {
            schema: 1,
            source_device_id: local_device_id.to_string(),
            receiver_device_id: first_peer_id.to_string(),
            screens: self.active_screens.clone(),
        })
    }

    /// Rebuild `active_screens` and a multi-device `RuntimeLayout` (`remote_device_id == "*"`)
    /// for `{local} ∪ remote_peers`, preserving saved positions whenever non-overlapping and
    /// cleanly placing any new or colliding peer to the right of the current bounding box.
    pub fn rebuild(
        &mut self,
        local_device_id: &str,
        local_displays: &[Display],
        remote_peers: &BTreeMap<String, Vec<Display>>,
    ) -> Result<RuntimeLayout> {
        let mut device_groups: Vec<(&str, &[Display])> = Vec::with_capacity(1 + remote_peers.len());
        device_groups.push((local_device_id, local_displays));
        for (peer_id, displays) in remote_peers {
            device_groups.push((peer_id.as_str(), displays.as_slice()));
        }

        let mut placed: Vec<SharedScreen> = Vec::new();
        let mut bindings: Vec<Binding> = Vec::new();

        for (dev_id, displays) in device_groups {
            if displays.is_empty() {
                continue;
            }
            // 1. Try placing all of this device's monitors at their remembered positions.
            let all_saved = displays
                .iter()
                .all(|d| self.saved_positions.contains_key(&pos_key(dev_id, &d.native_id)));
            let mut candidate_screens = Vec::with_capacity(displays.len());
            let mut candidate_bindings = Vec::with_capacity(displays.len());

            if all_saved {
                for d in displays {
                    let (sx, sy) = self.saved_positions[&pos_key(dev_id, &d.native_id)];
                    let sid = canonical_screen_id(dev_id, d.wire_id);
                    candidate_screens.push(SharedScreen {
                        id: sid.clone(),
                        device_id: dev_id.to_string(),
                        native_id: d.native_id.clone(),
                        x: sx,
                        y: sy,
                        width: d.width,
                        height: d.height,
                    });
                    candidate_bindings.push(Binding {
                        screen_id: sid,
                        native_id: d.wire_id.to_string(),
                        native: NativeRect {
                            x: d.x,
                            y: d.y,
                            width: d.width,
                            height: d.height,
                        },
                    });
                }
            }

            let mut trial = placed.clone();
            trial.extend(candidate_screens.iter().cloned());
            let saved_ok = all_saved && validate_mesh_screens(&trial).is_ok();

            if !saved_ok {
                candidate_screens.clear();
                candidate_bindings.clear();
                let min_x = displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min);
                let min_y = displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min);
                let anchor_x = placed
                    .iter()
                    .map(|s| s.x + s.width)
                    .fold(f64::NEG_INFINITY, f64::max);
                let base_x = if anchor_x.is_finite() { anchor_x } else { 0.0 };
                let base_y = 0.0;
                for d in displays {
                    let sid = canonical_screen_id(dev_id, d.wire_id);
                    let sx = base_x + (d.x - min_x);
                    let sy = base_y + (d.y - min_y);
                    candidate_screens.push(SharedScreen {
                        id: sid.clone(),
                        device_id: dev_id.to_string(),
                        native_id: d.native_id.clone(),
                        x: sx,
                        y: sy,
                        width: d.width,
                        height: d.height,
                    });
                    candidate_bindings.push(Binding {
                        screen_id: sid,
                        native_id: d.wire_id.to_string(),
                        native: NativeRect {
                            x: d.x,
                            y: d.y,
                            width: d.width,
                            height: d.height,
                        },
                    });
                }
            }

            for s in &candidate_screens {
                self.saved_positions
                    .insert(pos_key(&s.device_id, &s.native_id), (s.x, s.y));
            }
            placed.extend(candidate_screens);
            bindings.extend(candidate_bindings);
        }

        validate_mesh_screens(&placed)?;
        self.active_screens = placed.clone();
        let layout = Layout::new(
            placed
                .iter()
                .map(|s| Screen {
                    id: s.id.clone(),
                    device_id: s.device_id.clone(),
                    x: s.x,
                    y: s.y,
                    width: s.width,
                    height: s.height,
                })
                .collect(),
        )?;
        let runtime = RuntimeLayout {
            layout,
            local_device_id: local_device_id.to_string(),
            remote_device_id: "*".into(),
            bindings,
        };
        runtime.validate()?;
        Ok(runtime)
    }

    /// Apply a user-dragged layout from the UI: validates non-overlap (allowing cycles!),
    /// remembers every screen's new position, increments Lamport revision, and rebuilds RuntimeLayout.
    pub fn apply_user_layout(
        &mut self,
        screens: &[SharedScreen],
        local_device_id: &str,
        local_displays: &[Display],
        remote_peers: &BTreeMap<String, Vec<Display>>,
    ) -> Result<RuntimeLayout> {
        validate_mesh_screens(screens)?;
        for s in screens {
            self.saved_positions
                .insert(pos_key(&s.device_id, &s.native_id), (s.x, s.y));
        }
        self.revision = self.revision.saturating_add(1);
        self.rebuild(local_device_id, local_displays, remote_peers)
    }

    /// Merge a remote peer's Lamport-versioned layout update if `remote_revision > self.revision`
    /// (or if `remote_revision == self.revision` and we haven't applied a user edit yet).
    pub fn merge_remote_layout(
        &mut self,
        remote_revision: u64,
        remote_screens: &[SharedScreen],
        local_device_id: &str,
        local_displays: &[Display],
        remote_peers: &BTreeMap<String, Vec<Display>>,
    ) -> Result<Option<RuntimeLayout>> {
        if remote_revision <= self.revision || remote_screens.is_empty() {
            return Ok(None);
        }
        if validate_mesh_screens(remote_screens).is_err() {
            return Ok(None);
        }
        for s in remote_screens {
            self.saved_positions
                .insert(pos_key(&s.device_id, &s.native_id), (s.x, s.y));
        }
        self.revision = remote_revision;
        Ok(Some(self.rebuild(
            local_device_id,
            local_displays,
            remote_peers,
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arrangement::Point;
    use crate::routed_desktop::RoutedInput;
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;
    use std::thread;

    fn disp(native_id: &str, wire_id: u32, x: f64, y: f64, width: f64, height: f64) -> Display {
        Display {
            native_id: native_id.into(),
            wire_id,
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn passphrase_mismatch_classifier_ignores_timeouts_and_catches_decrypt_failures() {
        assert!(!is_passphrase_mismatch_error(&anyhow::anyhow!(
            "Read handshake: timed out"
        )));
        assert!(!is_passphrase_mismatch_error(&anyhow::anyhow!(
            "Connection refused"
        )));
        assert!(!is_passphrase_mismatch_error(&anyhow::anyhow!(
            "unexpected eof"
        )));
        assert!(is_passphrase_mismatch_error(&anyhow::anyhow!(
            "Authenticate LAN peer: Decrypt"
        )));
        assert!(is_passphrase_mismatch_error(&anyhow::anyhow!(
            "passphrase mismatch"
        )));
    }

    #[test]
    fn three_device_cycle_topology_routes_directly_a_to_b_to_c_to_a() {
        let win1 = "1".repeat(64);
        let mac = "2".repeat(64);
        let win2 = "3".repeat(64);

        let local_displays = vec![
            disp("WIN1-L", 1, 0., 0., 1920., 1080.),
            disp("WIN1-R", 2, 1920., 0., 1920., 1080.),
        ];
        let mut remotes = BTreeMap::new();
        remotes.insert(mac.clone(), vec![disp("MAC-1", 10, 0., 0., 1470., 956.)]);
        remotes.insert(win2.clone(), vec![disp("WIN2-1", 20, 0., 0., 1920., 1080.)]);

        let mut book = MeshLayoutBook {
            revision: 1,
            saved_positions: HashMap::new(),
            active_screens: Vec::new(),
        };
        let _initial = book.rebuild(&win1, &local_displays, &remotes).unwrap();

        // Arrange into a TRUE CYCLE:
        // - Mac sits directly above BOTH Win1-L and Win1-R (x: 1200..2670, y: -956..0)
        // - Win2 sits to the right of Mac AND above Win1-R (x: 2670..4590, y: -1080..0)
        // So: Win1-L <-> Mac <-> Win2 <-> Win1-R <-> Win1-L!
        let cycle_screens = vec![
            SharedScreen {
                id: canonical_screen_id(&win1, 1),
                device_id: win1.clone(),
                native_id: "WIN1-L".into(),
                x: 0.,
                y: 0.,
                width: 1920.,
                height: 1080.,
            },
            SharedScreen {
                id: canonical_screen_id(&win1, 2),
                device_id: win1.clone(),
                native_id: "WIN1-R".into(),
                x: 1920.,
                y: 0.,
                width: 1920.,
                height: 1080.,
            },
            SharedScreen {
                id: canonical_screen_id(&mac, 10),
                device_id: mac.clone(),
                native_id: "MAC-1".into(),
                x: 1200.,
                y: -956.,
                width: 1470.,
                height: 956.,
            },
            SharedScreen {
                id: canonical_screen_id(&win2, 20),
                device_id: win2.clone(),
                native_id: "WIN2-1".into(),
                x: 2670.,
                y: -1080.,
                width: 1920.,
                height: 1080.,
            },
        ];

        let runtime = book
            .apply_user_layout(&cycle_screens, &win1, &local_displays, &remotes)
            .unwrap();
        assert_eq!(book.revision(), 2);

        let mut routed = RoutedInput::new(runtime).unwrap();
        {
            let mut q = routed.input.lock().unwrap();
            q.seed_physical_held([], []).unwrap();
            q.begin_session();
        }
        routed.sync();
        assert!(routed.enable(routed.generation));

        // 1. Move up from Win1-L top edge (x=1500, y=0) -> crosses into Mac!
        routed.movement(Point { x: 1500., y: 0. }, 0., 0., 1).unwrap();
        routed.movement(Point { x: 1500., y: 0. }, 0., -50., 2).unwrap();
        assert!(routed.remote_gate.load(Ordering::Acquire));
        assert_eq!(routed.active_remote_device_id().as_deref(), Some(mac.as_str()));

        // 2. From Mac, move right across x=2670 directly into Win2 (1-hop direct switch!)
        let epoch_on_mac = routed.input.lock().unwrap().control_epoch();
        routed.movement(Point { x: 1500., y: 0. }, 1300., 0., 3).unwrap();
        assert!(routed.remote_gate.load(Ordering::Acquire));
        assert_eq!(routed.active_remote_device_id().as_deref(), Some(win2.as_str()));
        // Control epoch bumped so MeshRouter knows to Pause Mac and Arm Win2!
        assert_ne!(routed.input.lock().unwrap().control_epoch(), epoch_on_mac);

        // 3. From Win2 (x=2800, y=-50), move down past y=0 back onto Win1-R (completing the cycle!)
        let warp = routed.movement(Point { x: 1500., y: 0. }, 0., 100., 4).unwrap();
        assert!(warp.is_some());
        assert!(!routed.remote_gate.load(Ordering::Acquire));
        assert_eq!(routed.active_remote_device_id(), None);
        assert_eq!(
            routed.focus_identity(),
            (Some(win1.clone()), Some(canonical_screen_id(&win1, 2)))
        );
    }

    #[test]
    fn three_node_encrypted_mesh_handshake_and_layout_gossip_over_localhost() {
        let id_a = "a".repeat(64);
        let id_b = "b".repeat(64);
        let id_c = "c".repeat(64);
        let pass = Some("mesh-secret-123");

        let disp_a = vec![disp("A1", 1, 0., 0., 1920., 1080.)];
        let disp_b = vec![disp("B1", 2, 0., 0., 1470., 956.)];
        let disp_c = vec![disp("C1", 3, 0., 0., 2560., 1440.)];

        let listener_a = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_a = listener_a.local_addr().unwrap();

        // Spawn Node A accepting BOTH Node B and Node C concurrently!
        let a_id = id_a.clone();
        let a_disp = disp_a.clone();
        let server = thread::spawn(move || {
            let mut peers = Vec::new();
            for _ in 0..2 {
                let (stream, addr) = listener_a.accept().unwrap();
                let p = establish_mesh_peer(
                    stream,
                    addr,
                    false,
                    &a_id,
                    "Node-A",
                    a_disp.clone(),
                    pass,
                    1,
                    vec![],
                )
                .unwrap();
                peers.push(p);
            }
            peers
        });

        let sb = TcpStream::connect(addr_a).unwrap();
        let mut peer_b = establish_mesh_peer(
            sb,
            addr_a,
            true,
            &id_b,
            "Node-B",
            disp_b.clone(),
            pass,
            1,
            vec![],
        )
        .unwrap();

        let sc = TcpStream::connect(addr_a).unwrap();
        let mut peer_c = establish_mesh_peer(
            sc,
            addr_a,
            true,
            &id_c,
            "Node-C",
            disp_c.clone(),
            pass,
            1,
            vec![],
        )
        .unwrap();

        let mut a_peers = server.join().unwrap();
        assert_eq!(a_peers.len(), 2);
        assert_eq!(peer_b.remote_device_id, id_a);
        assert_eq!(peer_c.remote_device_id, id_a);

        // Node B pushes a LayoutSync revision=5 to Node A; Node A gossips to Node C!
        let updated_screens = vec![
            SharedScreen {
                id: canonical_screen_id(&id_a, 1),
                device_id: id_a.clone(),
                native_id: "A1".into(),
                x: 0.,
                y: 0.,
                width: 1920.,
                height: 1080.,
            },
            SharedScreen {
                id: canonical_screen_id(&id_b, 2),
                device_id: id_b.clone(),
                native_id: "B1".into(),
                x: 1920.,
                y: 0.,
                width: 1470.,
                height: 956.,
            },
            SharedScreen {
                id: canonical_screen_id(&id_c, 3),
                device_id: id_c.clone(),
                native_id: "C1".into(),
                x: -2560.,
                y: 0.,
                width: 2560.,
                height: 1440.,
            },
        ];
        peer_b
            .conn
            .queue_control(&MeshControl::LayoutSync {
                revision: 5,
                screens: updated_screens.clone(),
            })
            .unwrap();
        peer_b.conn.flush().unwrap();

        // A receives from B and relays LayoutSync to C
        let mut got_on_a = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !got_on_a && Instant::now() < deadline {
            for p in &mut a_peers {
                for pkt in p.conn.pump().unwrap() {
                    if let MeshPacket::Control(MeshControl::LayoutSync { revision, screens }) = pkt {
                        assert_eq!(revision, 5);
                        assert_eq!(screens.len(), 3);
                        got_on_a = true;
                    }
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(got_on_a);

        // Forward to C
        for p in &mut a_peers {
            if p.remote_device_id == id_c {
                p.conn
                    .queue_control(&MeshControl::LayoutSync {
                        revision: 5,
                        screens: updated_screens.clone(),
                    })
                    .unwrap();
                p.conn.flush().unwrap();
            }
        }
        let mut got_on_c = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !got_on_c && Instant::now() < deadline {
            for pkt in peer_c.conn.pump().unwrap() {
                if let MeshPacket::Control(MeshControl::LayoutSync { revision, screens }) = pkt {
                    assert_eq!(revision, 5);
                    assert_eq!(screens.len(), 3);
                    got_on_c = true;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(got_on_c);

        // Verify wrong passphrase is classified as auth mismatch
        let listener_bad = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr_bad = listener_bad.local_addr().unwrap();
        let a_id2 = id_a.clone();
        let a_disp2 = disp_a.clone();
        let srv2 = thread::spawn(move || {
            let (s, a) = listener_bad.accept().unwrap();
            establish_mesh_peer(s, a, false, &a_id2, "Node-A", a_disp2, Some("right-pass"), 1, vec![])
        });
        let s_bad = TcpStream::connect(addr_bad).unwrap();
        let err_client = establish_mesh_peer(
            s_bad,
            addr_bad,
            true,
            &id_b,
            "Node-B",
            disp_b,
            Some("wrong-pass"),
            1,
            vec![],
        )
        .err();
        let err_server = srv2.join().unwrap().err();
        assert!(
            err_client.as_ref().is_some_and(is_passphrase_mismatch_error)
                || err_server.as_ref().is_some_and(is_passphrase_mismatch_error)
        );
    }
}
