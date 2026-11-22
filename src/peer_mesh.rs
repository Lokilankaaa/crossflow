//! Symmetric peer-to-peer LAN mesh runner.
//!
//! Both Windows and macOS execute this identical peer node:
//! - Enumerates active local IPv4 network channels (Wired, Wi-Fi, ZeroTier/VLAN)
//!   and persists channel + optional shared passphrase settings in `peer-config.json`.
//! - Binds a local TCP listener (all interfaces in `auto` mode, or a chosen interface
//!   when a specific channel is selected) and advertises via mDNS (`_crossflow._tcp.local.`).
//! - Browses the LAN for compatible Crossflow peers and prioritizes the best channel
//!   automatically (Direct Wired > LAN Wired > Wi-Fi > ZeroTier), with optional direct
//!   peer IP support for virtual networks where mDNS multicast does not traverse.
//! - Encrypts every session with Noise_NNpsk0 (open LAN auto-connect by default,
//!   or isolated with an optional user-configured shared passphrase).
//! - Automatically builds or restores the real multi-display layout and enables
//!   bidirectional sharing once connected unless the user explicitly paused.

use crate::{
    aux_channel::{AuxMeshHub, AuxSession},
    discovery::{self, Advertisement, Candidate, DiscoveryConfig, DiscoveryHandle},
    hotkeys::{HotkeyAction, HotkeyConfig, SharedHotkeyEngine},
    injection_worker::WorkerProxy,
    mesh_runtime::{
        HandshakedMeshPeer, MeshControl, MeshLayoutBook, MeshPacket, MeshPeerConn,
        PEER_KEEPALIVE_INTERVAL, PEER_SILENCE_TIMEOUT, canonical_screen_id, establish_mesh_peer,
        is_passphrase_mismatch_error, ordered_screens, preferred_initiator,
    },
    metrics::{Log, LogWriter},
    pair_native::{Capture, Platform, Shortcut},
    pair_source::SharedSource,
    pair_wire::Display,
    protocol::{Frame, Kind, button_from_code},
    session::{MouseSink, NativeDisplay},
    session_metadata::{SharedLayout, SharedScreen},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::{BufRead, BufReader, ErrorKind, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

pub const PEER_UI_PROTOCOL: u8 = 3;
const DEFAULT_PORT: u16 = 24800;

pub struct Options {
    pub bind: SocketAddr,
    pub device_name: Option<String>,
    pub passphrase: Option<String>,
    pub seconds: f64,
    pub max_hz: u32,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct NetworkChannelInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub ip: Option<String>,
    pub interface_name: Option<String>,
}

#[derive(Clone, Debug)]
struct LocalChannel {
    id: String,
    label: String,
    kind: &'static str,
    priority: u8,
    ip: Ipv4Addr,
    netmask: Ipv4Addr,
    interface_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualPeer {
    pub address: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "default_channel")]
    pub channel: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PeerConfig {
    #[serde(default = "default_channel")]
    channel: String,
    #[serde(default)]
    direct_peer: Option<String>,
    #[serde(default)]
    manual_peers: Vec<ManualPeer>,
    #[serde(default)]
    peer_channels: HashMap<String, String>,
    #[serde(default)]
    passphrase: Option<String>,
    #[serde(default)]
    smart_keymap: bool,
    #[serde(default = "default_true")]
    clipboard_sync: bool,
    #[serde(default = "default_true")]
    input_state_sync: bool,
    #[serde(default)]
    hotkeys: HotkeyConfig,
}

fn default_channel() -> String {
    "auto".into()
}

fn default_true() -> bool {
    true
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            direct_peer: None,
            manual_peers: Vec::new(),
            peer_channels: HashMap::new(),
            passphrase: None,
            smart_keymap: false,
            clipboard_sync: true,
            input_state_sync: true,
            hotkeys: HotkeyConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoveredPeerInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub connected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub connected: bool,
    pub focused: bool,
    pub slot: Option<usize>,
    pub address: Option<String>,
    pub active_channel_label: Option<String>,
    pub preferred_channel: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct NumberedScreenInfo {
    pub number: usize,
    pub screen_id: String,
    pub device_id: String,
    pub device_name: String,
    pub native_id: String,
}

#[derive(Serialize)]
struct PeerUiStatus {
    protocol: u8,
    state: String,
    local_device_id: String,
    local_name: String,
    auth_mode: &'static str,
    passphrase_configured: bool,
    smart_keymap: bool,
    clipboard_sync: bool,
    input_state_sync: bool,
    screen_locked: bool,
    service_mode_installed: bool,
    service_mode_active: bool,
    hotkeys: HotkeyConfig,
    selected_channel: String,
    direct_peer: Option<String>,
    active_channel_label: Option<String>,
    network_channels: Vec<NetworkChannelInfo>,
    manual_peers: Vec<ManualPeer>,
    peers: Vec<PeerInfo>,
    route_enabled: bool,
    focus_device_id: Option<String>,
    focus_screen_id: Option<String>,
    peer_device_id: Option<String>,
    peer_name: Option<String>,
    peer_address: Option<String>,
    listen_endpoint: Option<String>,
    local_displays: Vec<Display>,
    discovered_peers: Vec<DiscoveredPeerInfo>,
    shared_layout: Option<SharedLayout>,
    screen_numbers: Vec<NumberedScreenInfo>,
    transfer: Option<crate::aux_channel::TransferStatus>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum UiCommand {
    Pause,
    Resume,
    Stop,
    ApplyLayout {
        layout: SharedLayout,
    },
    SetChannel {
        channel: String,
        #[serde(default)]
        direct_peer: Option<String>,
    },
    SetPeerChannel {
        device_id: String,
        channel: String,
    },
    AddManualPeer {
        address: String,
        #[serde(default)]
        label: Option<String>,
        #[serde(default = "default_channel")]
        channel: String,
    },
    RemoveManualPeer {
        address: String,
    },
    SetPassphrase {
        passphrase: Option<String>,
    },
    SetKeymap {
        enabled: bool,
    },
    SetClipboard {
        enabled: bool,
    },
    SetInputStateSync {
        enabled: bool,
    },
    SetHotkeys {
        hotkeys: HotkeyConfig,
    },
    SetHotkeySuspended {
        suspended: bool,
    },
    SetScreenLock {
        locked: bool,
    },
    QuickJump {
        #[serde(default)]
        target_device_id: Option<String>,
    },
    QuickJumpScreen {
        screen_id: String,
    },
    SendFiles {
        #[serde(default)]
        target_device_id: Option<String>,
        paths: Vec<String>,
    },
    CancelTransfer,
}

fn app_data_root() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("HOME is unavailable")?;
        Ok(PathBuf::from(home).join("Library/Application Support/Crossflow"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let base = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?;
        Ok(PathBuf::from(base).join("Crossflow"))
    }
}

fn config_path() -> Result<PathBuf> {
    let root = app_data_root()?;
    fs::create_dir_all(&root)?;
    Ok(root.join("peer-config.json"))
}

fn ids_match(a: &str, b: &str) -> bool {
    !a.is_empty() && !b.is_empty() && (a == b || a.starts_with(b) || b.starts_with(a))
}

fn peer_channel_override(peer_channels: &HashMap<String, String>, device_id: &str) -> String {
    if let Some(ch) = peer_channels.get(device_id)
        && !ch.trim().is_empty()
    {
        return ch.trim().to_string();
    }
    for (k, v) in peer_channels {
        if ids_match(k, device_id) && !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    default_channel()
}

fn effective_channel_for_peer<'a>(config: &'a PeerConfig, device_id: &str) -> &'a str {
    if let Some(ch) = config.peer_channels.get(device_id)
        && !ch.trim().is_empty()
        && ch.trim() != "auto"
    {
        return ch.trim();
    }
    for (k, v) in &config.peer_channels {
        if ids_match(k, device_id) && !v.trim().is_empty() && v.trim() != "auto" {
            return v.trim();
        }
    }
    if config.channel.trim().is_empty() {
        "auto"
    } else {
        config.channel.trim()
    }
}

fn set_peer_channel(config: &mut PeerConfig, device_id: &str, channel: &str) {
    let key = device_id.trim();
    if key.is_empty() {
        return;
    }
    let ch = channel.trim();
    config.peer_channels.retain(|k, _| !ids_match(k, key));
    if !ch.is_empty() && ch != "auto" {
        config.peer_channels.insert(key.to_string(), ch.to_string());
    }
}

fn normalize_manual_address(raw: &str) -> Option<String> {
    let sock = parse_direct_peer(Some(raw))?;
    if sock.port() == DEFAULT_PORT {
        Some(sock.ip().to_string())
    } else {
        Some(sock.to_string())
    }
}

fn add_manual_peer(
    config: &mut PeerConfig,
    address: &str,
    label: Option<String>,
    channel: &str,
) -> bool {
    let Some(norm_addr) = normalize_manual_address(address) else {
        return false;
    };
    let clean_label = label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(48).collect::<String>());
    let clean_channel = {
        let c = channel.trim();
        if c.is_empty() {
            default_channel()
        } else {
            c.to_string()
        }
    };
    if let Some(existing) = config
        .manual_peers
        .iter_mut()
        .find(|p| normalize_manual_address(&p.address).as_deref() == Some(norm_addr.as_str()))
    {
        existing.address = norm_addr;
        existing.label = clean_label.or(existing.label.clone());
        existing.channel = clean_channel;
        return true;
    }
    if config.manual_peers.len() >= 32 {
        return false;
    }
    config.manual_peers.push(ManualPeer {
        address: norm_addr,
        label: clean_label,
        channel: clean_channel,
    });
    true
}

fn remove_manual_peer(config: &mut PeerConfig, address: &str) -> bool {
    let target = normalize_manual_address(address).unwrap_or_else(|| address.trim().to_string());
    let before = config.manual_peers.len();
    config.manual_peers.retain(|p| {
        let cur = normalize_manual_address(&p.address).unwrap_or_else(|| p.address.trim().to_string());
        cur != target
    });
    config.manual_peers.len() != before
}

fn normalize_config(cfg: &mut PeerConfig) {
    if cfg.channel.trim().is_empty() {
        cfg.channel = default_channel();
    }
    cfg.direct_peer = cfg
        .direct_peer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    // Migrate legacy single direct_peer into the multi-device manual_peers list seamlessly.
    if let Some(legacy) = cfg.direct_peer.clone() {
        let ch = cfg.channel.clone();
        let _ = add_manual_peer(cfg, &legacy, None, &ch);
    }
    // Deduplicate & sanitize existing manual_peers
    let mut cleaned = Vec::new();
    for mp in std::mem::take(&mut cfg.manual_peers) {
        if let Some(norm) = normalize_manual_address(&mp.address)
            && !cleaned
                .iter()
                .any(|e: &ManualPeer| e.address == norm)
        {
            cleaned.push(ManualPeer {
                address: norm,
                label: mp
                    .label
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.chars().take(48).collect()),
                channel: if mp.channel.trim().is_empty() {
                    default_channel()
                } else {
                    mp.channel.trim().to_string()
                },
            });
        }
    }
    cfg.manual_peers = cleaned;
    cfg.peer_channels
        .retain(|k, v| !k.trim().is_empty() && !v.trim().is_empty() && v.trim() != "auto");
    cfg.passphrase = cfg
        .passphrase
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    cfg.hotkeys = cfg.hotkeys.normalized();
}

fn load_config(env_passphrase: Option<String>) -> PeerConfig {
    let mut cfg = config_path()
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<PeerConfig>(&b).ok())
        .unwrap_or_default();
    normalize_config(&mut cfg);
    if let Some(secret) = env_passphrase
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cfg.passphrase = Some(secret.to_string());
    }
    cfg
}

fn save_config(cfg: &PeerConfig) {
    let Ok(path) = config_path() else { return };
    let Ok(bytes) = serde_json::to_vec_pretty(cfg) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        {
            let _ = file.write_all(&bytes);
            let _ = file.flush();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = fs::write(&path, bytes);
    }
}

fn load_or_create_device_id() -> Result<String> {
    let root = app_data_root()?;
    fs::create_dir_all(&root)?;
    let path = root.join("device-id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if trimmed.len() == 64
            && trimmed
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Ok(trimmed);
        }
    }
    let mut bytes = [0u8; 32];
    DefaultResolver
        .resolve_rng()
        .context("Random generator unavailable")?
        .try_fill_bytes(&mut bytes)
        .context("Cannot generate persistent peer device ID")?;
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    fs::write(&path, format!("{id}\n"))?;
    Ok(id)
}

fn mesh_layout_path() -> Result<PathBuf> {
    let root = app_data_root()?;
    fs::create_dir_all(&root)?;
    Ok(root.join("mesh-layout.json"))
}

fn default_friendly_name() -> String {
    if let Ok(name) = std::env::var("COMPUTERNAME")
        && !name.trim().is_empty()
    {
        return name.trim().chars().take(48).collect();
    }
    if let Ok(name) = std::env::var("HOSTNAME")
        && !name.trim().is_empty()
    {
        return name.trim().chars().take(48).collect();
    }
    discovery::default_device_name().to_string()
}

fn is_benchmark_tun_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    // 198.18.0.0/15 is RFC 2544 (used by Clash/Meta fake-IP TUN adapters)
    o[0] == 198 && (o[1] == 18 || o[1] == 19)
}

fn is_cgnat_vpn_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xc0) == 64
}

fn is_direct_ethernet_ipv4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 192 && o[1] == 168 && o[2] == 50
}

fn classify_interface(name: &str, ip: Ipv4Addr) -> (&'static str, &'static str, u8) {
    let lower = name.to_ascii_lowercase();
    if lower.contains("zerotier")
        || lower.starts_with("zt")
        || lower.starts_with("feth")
        || (ip.octets()[0] == 192 && ip.octets()[1] == 168 && ip.octets()[2] == 194)
    {
        return ("zerotier", "ZeroTier 虚拟局域网", 3);
    }
    if lower.contains("tailscale")
        || lower.contains("wintun")
        || lower.contains("wireguard")
        || lower.starts_with("wg")
        || lower.starts_with("utun")
        || is_cgnat_vpn_ipv4(ip)
    {
        return ("zerotier", "虚拟局域网", 3);
    }
    if is_direct_ethernet_ipv4(ip) {
        return ("wired", "有线直连", 0);
    }
    if lower.contains("wlan")
        || lower.contains("wi-fi")
        || lower.contains("wifi")
        || lower.contains("wireless")
        || (cfg!(target_os = "macos") && lower == "en0")
    {
        return ("wifi", "Wi-Fi", 2);
    }
    if name.contains("以太网")
        || lower.contains("ethernet")
        || lower.starts_with("eth")
        || (cfg!(target_os = "macos") && lower.starts_with("en"))
    {
        return ("wired", "有线网络", 1);
    }
    ("lan", "局域网", 2)
}

fn enumerate_local_channels() -> Vec<LocalChannel> {
    let Ok(addrs) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut channels = Vec::new();
    let mut seen_ips = std::collections::HashSet::new();
    for iface in addrs {
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        let ip = v4.ip;
        if ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_multicast()
            || ip.is_link_local()
            || is_benchmark_tun_ipv4(ip)
        {
            continue;
        }
        let lower = iface.name.to_ascii_lowercase();
        if lower.contains("meta")
            || lower.contains("bluetooth")
            || iface.name.contains("蓝牙")
            || lower.contains("vethernet (wsl")
        {
            continue;
        }
        if !seen_ips.insert(ip) {
            continue;
        }
        let (kind, prefix, priority) = classify_interface(&iface.name, ip);
        channels.push(LocalChannel {
            id: ip.to_string(),
            label: format!("{prefix} · {ip}"),
            kind,
            priority,
            ip,
            netmask: v4.netmask,
            interface_name: iface.name,
        });
    }
    channels.sort_by(|a, b| (a.priority, a.ip.octets()).cmp(&(b.priority, b.ip.octets())));
    channels
}

fn channel_infos(local: &[LocalChannel]) -> Vec<NetworkChannelInfo> {
    let mut list = Vec::with_capacity(local.len() + 1);
    list.push(NetworkChannelInfo {
        id: "auto".into(),
        label: "自动选择最优".into(),
        kind: "auto".into(),
        ip: None,
        interface_name: None,
    });
    for ch in local {
        list.push(NetworkChannelInfo {
            id: ch.id.clone(),
            label: ch.label.clone(),
            kind: ch.kind.into(),
            ip: Some(ch.ip.to_string()),
            interface_name: Some(ch.interface_name.clone()),
        });
    }
    list
}

fn same_subnet(a: Ipv4Addr, b: Ipv4Addr, mask: Ipv4Addr) -> bool {
    let m = u32::from(mask);
    (u32::from(a) & m) == (u32::from(b) & m)
}

fn classify_active_channel(peer_addr: SocketAddr, local: &[LocalChannel]) -> Option<String> {
    let IpAddr::V4(v4) = peer_addr.ip() else {
        return Some("IPv6".into());
    };
    if let Some(ch) = local.iter().find(|c| same_subnet(v4, c.ip, c.netmask)) {
        let short = ch.label.split(" · ").next().unwrap_or(&ch.label);
        return Some(short.to_string());
    }
    let (_, prefix, _) = classify_interface("", v4);
    Some(prefix.to_string())
}

fn ranked_addresses(
    addresses: &[SocketAddr],
    selected_channel: &str,
    local: &[LocalChannel],
) -> Vec<SocketAddr> {
    let pinned = if selected_channel != "auto" {
        local.iter().find(|c| c.id == selected_channel)
    } else {
        None
    };

    let mut ranked: Vec<SocketAddr> = addresses
        .iter()
        .copied()
        .filter(|addr| match addr.ip() {
            IpAddr::V4(v4) => {
                if v4.is_loopback()
                    || v4.is_unspecified()
                    || v4.is_multicast()
                    || is_benchmark_tun_ipv4(v4)
                {
                    return false;
                }
                if let Some(ch) = pinned {
                    same_subnet(v4, ch.ip, ch.netmask)
                } else {
                    !v4.is_link_local()
                }
            }
            IpAddr::V6(v6) => {
                pinned.is_none()
                    && !v6.is_loopback()
                    && !v6.is_unspecified()
                    && !v6.is_multicast()
            }
        })
        .collect();

    ranked.sort_by_key(|addr| match addr.ip() {
        IpAddr::V4(v4) => {
            if let Some(ch) = local.iter().find(|c| same_subnet(v4, c.ip, c.netmask)) {
                (ch.priority, addr.to_string())
            } else if is_direct_ethernet_ipv4(v4) {
                (0u8, addr.to_string())
            } else if v4.is_private() {
                (4u8, addr.to_string())
            } else {
                (5u8, addr.to_string())
            }
        }
        IpAddr::V6(_) => (6u8, addr.to_string()),
    });
    ranked
}

fn parse_direct_peer(raw: Option<&str>) -> Option<SocketAddr> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(sock) = text.parse::<SocketAddr>() {
        return Some(sock);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, DEFAULT_PORT));
    }
    None
}

fn spawn_stdin_reader(stop: Arc<AtomicBool>) -> Receiver<UiCommand> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("crossflow-peer-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        stop.store(true, Ordering::Release);
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(cmd) = serde_json::from_str::<UiCommand>(trimmed) {
                            let is_stop = matches!(cmd, UiCommand::Stop);
                            let _ = tx.send(cmd);
                            if is_stop {
                                stop.store(true, Ordering::Release);
                                break;
                            }
                        }
                    }
                    Err(_) => {
                        stop.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        })
        .ok();
    rx
}

fn emit_status(stdout: &Mutex<std::io::Stdout>, status: &PeerUiStatus) {
    if let Ok(line) = serde_json::to_string(status)
        && let Ok(mut out) = stdout.lock()
    {
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}


fn query_windows_service_flags() -> (bool, bool, bool) {
    #[cfg(windows)]
    {
        let st = crate::platform::windows::service::query_status();
        (st.installed, st.running, st.pipe_ready)
    }
    #[cfg(not(windows))]
    {
        (false, false, false)
    }
}

fn prepare_worker(inventory: &[Display], log: Log) -> Result<WorkerProxy> {
    #[cfg(windows)]
    let mut sink = match WorkerProxy::try_connect_windows_service() {
        Ok(s) => s,
        Err(_) => {
            let mut command = process::Command::new(std::env::current_exe()?);
            command.args(["input-worker", "--gain", "1"]);
            WorkerProxy::spawn(&mut command)?
        }
    };
    #[cfg(not(windows))]
    let mut sink = {
        let mut command = process::Command::new(std::env::current_exe()?);
        command.args(["input-worker", "--gain", "1"]);
        WorkerProxy::spawn(&mut command)?
    };
    sink.attach_failure_log(log);
    sink.set_displays(
        inventory
            .iter()
            .map(|d| NativeDisplay {
                id: d.wire_id,
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
            })
            .collect(),
    )?;
    Ok(sink)
}

struct ActiveHotkeyGuard;
impl Drop for ActiveHotkeyGuard {
    fn drop(&mut self) {
        crate::hotkeys::install_active_engine(None);
    }
}

enum HandshakeMsg {
    Ready {
        dial_key: Option<String>,
        peer: HandshakedMeshPeer,
    },
    Failed {
        dial_key: Option<String>,
        is_auth_mismatch: bool,
        message: String,
    },
}

struct ActiveMeshPeer {
    device_id: String,
    name: String,
    platform: String,
    peer_addr: SocketAddr,
    initiator: bool,
    active_channel_label: Option<String>,
    displays: Vec<Display>,
    conn: MeshPeerConn,
    aux: AuxSession,
}

struct MeshNativeIo {
    source: SharedSource,
    capture: Option<Box<dyn Capture + Send>>,
    sink: WorkerProxy,
    last_activity: u64,
    last_sink_maintain: Instant,
    last_capture_retry: Instant,
    enabled_attempted: bool,
}

impl MeshNativeIo {
    fn shutdown(mut self) {
        if let Some(mut cap) = self.capture.take() {
            let _ = cap.stop();
        }
        let _ = self.sink.shutdown();
    }
}

fn apply_inbound_frame(sink: &mut WorkerProxy, frame: &Frame) -> Result<()> {
    match frame.kind {
        Kind::Position => {
            let _ = sink.position(frame.x, frame.y, frame.display_id)?;
        }
        Kind::Move => {
            let _ = sink.move_by(frame.x, frame.y)?;
        }
        Kind::Button => {
            let button = button_from_code(frame.code)?;
            sink.button(button, frame.down)?;
        }
        Kind::Key => {
            sink.key(frame.code, frame.down)?;
        }
        Kind::Scroll => {
            sink.scroll(frame.wheel_x, frame.wheel_y)?;
        }
        _ => {}
    }
    Ok(())
}

fn remote_display_map(peers: &BTreeMap<String, ActiveMeshPeer>) -> BTreeMap<String, Vec<Display>> {
    peers
        .iter()
        .map(|(id, p)| (id.clone(), p.displays.clone()))
        .collect()
}

fn broadcast_layout_sync(
    peers: &mut BTreeMap<String, ActiveMeshPeer>,
    exclude_id: Option<&str>,
    revision: u64,
    screens: &[crate::session_metadata::SharedScreen],
) {
    let ctrl = MeshControl::LayoutSync {
        revision,
        screens: screens.to_vec(),
    };
    for (id, peer) in peers.iter_mut() {
        if exclude_id == Some(id.as_str()) {
            continue;
        }
        let _ = peer.conn.queue_control(&ctrl);
        let _ = peer.conn.flush();
    }
}

fn resolve_mesh_quick_jump_target(
    target: Option<&str>,
    local_device_id: &str,
    active_outbound_target: Option<&str>,
    peers: &BTreeMap<String, ActiveMeshPeer>,
) -> Option<String> {
    if peers.is_empty() {
        return None;
    }
    match target.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            // Cycle: Local -> Peer #1 -> Peer #2 -> ... -> Local
            let peer_ids: Vec<&String> = peers.keys().collect();
            match active_outbound_target {
                None => Some(peer_ids[0].clone()),
                Some(cur) => {
                    if let Some(pos) = peer_ids.iter().position(|id| id.as_str() == cur) {
                        if pos + 1 < peer_ids.len() {
                            Some(peer_ids[pos + 1].clone())
                        } else {
                            Some(local_device_id.to_string())
                        }
                    } else {
                        Some(local_device_id.to_string())
                    }
                }
            }
        }
        Some("local") => Some(local_device_id.to_string()),
        Some(id) if id == local_device_id => Some(local_device_id.to_string()),
        Some(id) => peers
            .keys()
            .find(|k| ids_match(k, id))
            .cloned()
            .or_else(|| peers.keys().next().cloned()),
    }
}

#[derive(Clone, Debug)]
enum QuickJumpTarget {
    Device(String),
    Screen(String),
}

fn build_numbered_screens(
    active_screens: &[SharedScreen],
    local_device_id: &str,
    local_name: &str,
    local_displays: &[Display],
    active_peers: &BTreeMap<String, ActiveMeshPeer>,
) -> Vec<NumberedScreenInfo> {
    let source_screens: Vec<SharedScreen> = if !active_screens.is_empty() {
        ordered_screens(active_screens)
    } else {
        let local_only: Vec<SharedScreen> = local_displays
            .iter()
            .map(|d| SharedScreen {
                id: canonical_screen_id(local_device_id, d.wire_id),
                device_id: local_device_id.to_string(),
                native_id: d.native_id.clone(),
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
            })
            .collect();
        ordered_screens(&local_only)
    };
    source_screens
        .into_iter()
        .enumerate()
        .map(|(idx, s)| {
            let device_name = if s.device_id == local_device_id {
                local_name.to_string()
            } else {
                active_peers
                    .get(&s.device_id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| s.device_id[..s.device_id.len().min(8)].to_string())
            };
            NumberedScreenInfo {
                number: idx + 1,
                screen_id: s.id,
                device_id: s.device_id,
                device_name,
                native_id: s.native_id,
            }
        })
        .collect()
}

fn active_transfer_snapshot(
    peers: &BTreeMap<String, ActiveMeshPeer>,
) -> Option<crate::aux_channel::TransferStatus> {
    let mut fallback = None;
    for p in peers.values() {
        if let Some(t) = p.aux.current_transfer() {
            if t.state == "active" {
                return Some(t);
            }
            if fallback.is_none() {
                fallback = Some(t);
            }
        }
    }
    fallback
}

pub fn run(
    options: Options,
    writer: LogWriter,
    stop: Arc<AtomicBool>,
    platform: Platform,
) -> Result<()> {
    ensure!(
        options.seconds.is_finite() && options.seconds > 0. && options.seconds <= 86400.,
        "invalid peer duration"
    );
    ensure!((60..=8000).contains(&options.max_hz), "invalid max_hz");

    let stdout = Mutex::new(std::io::stdout());
    let local_device_id = load_or_create_device_id()?;
    let local_discovery_id = local_device_id[..32].to_string();
    let local_name = options
        .device_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(default_friendly_name);
    let mut config = load_config(options.passphrase);
    let hotkey_engine = SharedHotkeyEngine::new(&config.hotkeys);
    crate::hotkeys::install_active_engine(Some(hotkey_engine.clone()));
    let _hotkey_guard = ActiveHotkeyGuard;
    let smart_keymap_flag = Arc::new(AtomicBool::new(config.smart_keymap));
    let clipboard_sync_flag = Arc::new(AtomicBool::new(config.clipboard_sync));
    let input_state_sync_flag = Arc::new(AtomicBool::new(config.input_state_sync));
    let local_has_focus = Arc::new(AtomicBool::new(true));
    let _keep_awake = crate::power::KeepAwakeGuard::acquire();

    let layout_file = mesh_layout_path()?;
    let mut layout_book = MeshLayoutBook::load(&layout_file);

    #[cfg(target_os = "macos")]
    {
        if let Err(permission) = crate::platform::macos::preflight_input_permissions() {
            let local_channels = enumerate_local_channels();
            let has_pass = config.passphrase.is_some();
            emit_status(
                &stdout,
                &PeerUiStatus {
                    protocol: PEER_UI_PROTOCOL,
                    state: "startup_rejected".into(),
                    local_device_id,
                    local_name,
                    auth_mode: if has_pass { "passphrase" } else { "open" },
                    passphrase_configured: has_pass,
                    smart_keymap: config.smart_keymap,
                    clipboard_sync: config.clipboard_sync,
                    input_state_sync: config.input_state_sync,
                    screen_locked: false,
                    service_mode_installed: false,
                    service_mode_active: false,
                    hotkeys: config.hotkeys.clone(),
                    selected_channel: config.channel.clone(),
                    direct_peer: config.direct_peer.clone(),
                    active_channel_label: None,
                    network_channels: channel_infos(&local_channels),
                    manual_peers: config.manual_peers.clone(),
                    peers: vec![],
                    route_enabled: false,
                    focus_device_id: None,
                    focus_screen_id: None,
                    peer_device_id: None,
                    peer_name: None,
                    peer_address: None,
                    listen_endpoint: None,
                    local_displays: vec![],
                    discovered_peers: vec![],
                    shared_layout: None,
                    screen_numbers: vec![],
                    transfer: None,
                    error: Some(permission.message().to_string()),
                },
            );
            writer.finish()?;
            anyhow::bail!("{}", permission.message());
        }
    }

    let rx = spawn_stdin_reader(stop.clone());
    let log = writer.log.clone();
    let deadline = Instant::now() + Duration::from_secs_f64(options.seconds);
    let mut user_paused = false;
    let mut screen_locked = false;
    let motion_interval = Duration::from_secs_f64(1.0 / f64::from(options.max_hz));

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            let mut local_channels = enumerate_local_channels();
            let mut last_channel_refresh = Instant::now();
            if config.channel != "auto" && !local_channels.iter().any(|c| c.id == config.channel) {
                config.channel = default_channel();
            }

            let bind_ip: IpAddr = if config.channel == "auto" {
                options.bind.ip()
            } else {
                config
                    .channel
                    .parse::<Ipv4Addr>()
                    .map(IpAddr::V4)
                    .unwrap_or_else(|_| options.bind.ip())
            };
            let bind_addr = SocketAddr::new(bind_ip, options.bind.port());
            let listener = TcpListener::bind(bind_addr)
                .or_else(|_| TcpListener::bind(SocketAddr::new(bind_ip, 0)))
                .or_else(|_| TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)))?;
            listener.set_nonblocking(true)?;
            let bound = listener.local_addr()?;
            let listen_endpoint = Some(bound.to_string());
            let aux_listener = TcpListener::bind(SocketAddr::new(
                bound.ip(),
                bound.port().saturating_add(1),
            ))
            .ok()
            .and_then(|l| {
                l.set_nonblocking(true).ok()?;
                Some(Arc::new(l))
            });
            let aux_hub = AuxMeshHub::start(
                aux_listener,
                local_device_id.clone(),
                config.passphrase.clone(),
                clipboard_sync_flag.clone(),
            );
            let channel_cache = Arc::new(Mutex::new(local_channels.clone()));
            let service_flags_cache = Arc::new(Mutex::new(query_windows_service_flags()));
            let channel_watch_stop = Arc::new(AtomicBool::new(false));
            {
                let cache = channel_cache.clone();
                let svc_cache = service_flags_cache.clone();
                let watch_stop = channel_watch_stop.clone();
                let _ = thread::Builder::new()
                    .name("crossflow-channel-watch".into())
                    .spawn(move || {
                        let mut last_ch = Instant::now();
                        let mut last_svc = Instant::now() - Duration::from_secs(2);
                        while !watch_stop.load(Ordering::Acquire) {
                            if last_ch.elapsed() >= Duration::from_secs(4) {
                                last_ch = Instant::now();
                                let refreshed = enumerate_local_channels();
                                if let Ok(mut slot) = cache.lock() {
                                    *slot = refreshed;
                                }
                            }
                            if last_svc.elapsed() >= Duration::from_millis(1200) {
                                last_svc = Instant::now();
                                let flags = query_windows_service_flags();
                                if let Ok(mut slot) = svc_cache.lock() {
                                    *slot = flags;
                                }
                            }
                            thread::sleep(Duration::from_millis(200));
                        }
                    });
            }
            let mut service_mode_flags = query_windows_service_flags();
            let mut last_service_check = Instant::now();

            let mut discovery = DiscoveryHandle::start(DiscoveryConfig {
                local_device_id: Some(local_discovery_id.clone()),
                advertisement: Some(Advertisement {
                    device_id: local_discovery_id.clone(),
                    friendly_name: local_name.clone(),
                    platform: std::env::consts::OS.into(),
                    protocol: u16::from(crate::protocol::WIRE_VERSION),
                    port: bound.port(),
                    listen_ip: bound.ip(),
                }),
                compatible_protocol: u16::from(crate::protocol::WIRE_VERSION),
            })?;

            let mut local_displays = (platform.inventory)().unwrap_or_default();
            let mut last_display_refresh = Instant::now();
            let (hs_tx, hs_rx) = mpsc::channel::<HandshakeMsg>();
            let mut in_flight_dials: HashSet<String> = HashSet::new();
            let mut dial_backoff: HashMap<String, Instant> = HashMap::new();
            let mut active_peers: BTreeMap<String, ActiveMeshPeer> = BTreeMap::new();
            let mut native_io: Option<MeshNativeIo> = None;

            let mut active_outbound_target: Option<String> = None;
            let mut inbound_owner: Option<String> = None;
            let mut remote_focus_state: (Option<String>, Option<String>) = (None, None);
            let mut last_broadcast_focus: (Option<String>, Option<String>) = (None, None);
            let mut pending_quick_jump: Option<(QuickJumpTarget, Instant)> = None;
            let mut numbered_screens: Vec<NumberedScreenInfo> = build_numbered_screens(
                layout_book.active_screens(),
                &local_device_id,
                &local_name,
                &local_displays,
                &active_peers,
            );
            hotkey_engine.set_screen_count(numbered_screens.len());
            let mut screen_numbers_dirty = false;

            let mut reconfigure_requested = false;
            let mut last_auth_error: Option<String> = None;
            let mut last_dial_scan = Instant::now() - Duration::from_secs(2);
            let mut last_motion_send = Instant::now() - motion_interval;
            let mut last_status_emit = Instant::now() - Duration::from_secs(1);
            let mut last_status_sig = String::new();
            let started_at = Instant::now();

            while !stop.load(Ordering::Acquire)
                && !reconfigure_requested
                && Instant::now() < deadline
            {
                let mut did_work = false;

                if last_channel_refresh.elapsed() >= Duration::from_secs(3) {
                    last_channel_refresh = Instant::now();
                    if let Ok(cached) = channel_cache.try_lock() {
                        local_channels = cached.clone();
                    }
                }
                if last_service_check.elapsed() >= Duration::from_millis(800) {
                    last_service_check = Instant::now();
                    if let Ok(cached) = service_flags_cache.try_lock() {
                        service_mode_flags = *cached;
                    }
                    // Hot-upgrade portable worker onto LocalSystem service pipe as soon as
                    // CrossflowInputService becomes available (zero restart required!).
                    if service_mode_flags.2
                        && inbound_owner.is_none()
                        && let Some(ref mut io) = native_io
                        && !io.sink.is_service_backed()
                        && let Ok(new_sink) = prepare_worker(&local_displays, log.clone())
                    {
                        if new_sink.is_service_backed() {
                            let mut old = std::mem::replace(&mut io.sink, new_sink);
                            let _ = old.shutdown();
                        } else {
                            let mut unused = new_sink;
                            let _ = unused.shutdown();
                        }
                    }
                }
                if active_peers.is_empty() && last_display_refresh.elapsed() >= Duration::from_secs(2)
                {
                    last_display_refresh = Instant::now();
                    if let Ok(inv) = (platform.inventory)()
                        && !inv.is_empty()
                    {
                        local_displays = inv;
                        screen_numbers_dirty = true;
                    }
                }

                // 1. Drain UI commands
                loop {
                    match rx.try_recv() {
                        Ok(UiCommand::Stop) | Err(TryRecvError::Disconnected) => {
                            stop.store(true, Ordering::Release);
                            break;
                        }
                        Ok(UiCommand::Pause) => {
                            user_paused = true;
                            pending_quick_jump = None;
                            if let Some(ref io) = native_io {
                                io.source.pause_mesh();
                            }
                            if let Some(prev) = active_outbound_target.take()
                                && let Some(p) = active_peers.get_mut(&prev)
                            {
                                let _ = p.conn.queue_frame(&Frame::new(Kind::Pause));
                                let _ = p.conn.flush();
                            }
                        }
                        Ok(UiCommand::Resume) => {
                            user_paused = false;
                            if let Some(ref mut io) = native_io
                                && io.source.status().ready
                                && io.source.enable_mesh().is_ok()
                            {
                                io.enabled_attempted = true;
                            }
                        }
                        Ok(UiCommand::ApplyLayout { layout }) => {
                            let remotes = remote_display_map(&active_peers);
                            if let Ok(new_rt) = layout_book.apply_user_layout(
                                &layout.screens,
                                &local_device_id,
                                &local_displays,
                                &remotes,
                            ) {
                                layout_book.save(&layout_file);
                                screen_numbers_dirty = true;
                                if let Some(ref io) = native_io {
                                    let _ = io.source.update_runtime(new_rt);
                                }
                                broadcast_layout_sync(
                                    &mut active_peers,
                                    None,
                                    layout_book.revision(),
                                    layout_book.active_screens(),
                                );
                            }
                        }
                        Ok(UiCommand::SetChannel {
                            channel,
                            direct_peer,
                        }) => {
                            let clean = channel.trim();
                            config.channel = if clean.is_empty() {
                                default_channel()
                            } else {
                                clean.to_string()
                            };
                            config.direct_peer = direct_peer
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(ToString::to_string);
                            if let Some(legacy) = config.direct_peer.clone() {
                                let ch = config.channel.clone();
                                let _ = add_manual_peer(&mut config, &legacy, None, &ch);
                            }
                            save_config(&config);
                            reconfigure_requested = true;
                            break;
                        }
                        Ok(UiCommand::SetPeerChannel { device_id, channel }) => {
                            set_peer_channel(&mut config, &device_id, &channel);
                            save_config(&config);
                            // Disconnect ONLY that specific peer so it immediately reconnects on its new channel
                            let matching: Vec<String> = active_peers
                                .keys()
                                .filter(|k| ids_match(k, &device_id))
                                .cloned()
                                .collect();
                            for id in matching {
                                active_peers.remove(&id);
                                if active_outbound_target.as_deref() == Some(id.as_str()) {
                                    active_outbound_target = None;
                                }
                                if inbound_owner.as_deref() == Some(id.as_str()) {
                                    inbound_owner = None;
                                }
                            }
                            dial_backoff.retain(|k, _| !ids_match(k, &device_id));
                            last_dial_scan = Instant::now() - Duration::from_secs(2);
                        }
                        Ok(UiCommand::AddManualPeer {
                            address,
                            label,
                            channel,
                        }) => {
                            if add_manual_peer(&mut config, &address, label, &channel) {
                                save_config(&config);
                                dial_backoff.clear();
                                last_dial_scan = Instant::now() - Duration::from_secs(2);
                            }
                        }
                        Ok(UiCommand::RemoveManualPeer { address }) => {
                            if remove_manual_peer(&mut config, &address) {
                                config.direct_peer = None;
                                save_config(&config);
                            }
                        }
                        Ok(UiCommand::SetPassphrase { passphrase }) => {
                            config.passphrase = passphrase
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(ToString::to_string);
                            save_config(&config);
                            last_auth_error = None;
                            reconfigure_requested = true;
                            break;
                        }
                        Ok(UiCommand::SetKeymap { enabled }) => {
                            config.smart_keymap = enabled;
                            smart_keymap_flag.store(enabled, Ordering::Relaxed);
                            save_config(&config);
                        }
                        Ok(UiCommand::SetClipboard { enabled }) => {
                            config.clipboard_sync = enabled;
                            clipboard_sync_flag.store(enabled, Ordering::Relaxed);
                            save_config(&config);
                        }
                        Ok(UiCommand::SetInputStateSync { enabled }) => {
                            config.input_state_sync = enabled;
                            input_state_sync_flag.store(enabled, Ordering::Relaxed);
                            save_config(&config);
                        }
                        Ok(UiCommand::SetHotkeys { hotkeys }) => {
                            config.hotkeys = hotkeys.normalized();
                            hotkey_engine.update_config(&config.hotkeys);
                            save_config(&config);
                        }
                        Ok(UiCommand::SetHotkeySuspended { suspended }) => {
                            hotkey_engine.set_suspended(suspended);
                        }
                        Ok(UiCommand::SetScreenLock { locked }) => {
                            screen_locked = locked;
                            if let Some(ref io) = native_io {
                                io.source.set_edge_locked(locked);
                            }
                        }
                        Ok(UiCommand::QuickJump { target_device_id }) => {
                            if !user_paused
                                && let Some(target) = resolve_mesh_quick_jump_target(
                                    target_device_id.as_deref(),
                                    &local_device_id,
                                    active_outbound_target.as_deref(),
                                    &active_peers,
                                )
                            {
                                pending_quick_jump =
                                    Some((QuickJumpTarget::Device(target), Instant::now()));
                            }
                        }
                        Ok(UiCommand::QuickJumpScreen { screen_id }) => {
                            let clean = screen_id.trim();
                            if !user_paused && !clean.is_empty() {
                                pending_quick_jump = Some((
                                    QuickJumpTarget::Screen(clean.to_string()),
                                    Instant::now(),
                                ));
                            }
                        }
                        Ok(UiCommand::SendFiles {
                            target_device_id,
                            paths,
                        }) => {
                            let list: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
                            if !list.is_empty() {
                                let target = target_device_id
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|s| !s.is_empty());
                                for (id, peer) in &active_peers {
                                    if target.is_none_or(|t| ids_match(id, t)) {
                                        peer.aux.send_files(list.clone());
                                    }
                                }
                            }
                        }
                        Ok(UiCommand::CancelTransfer) => {
                            for peer in active_peers.values() {
                                peer.aux.cancel_transfer();
                            }
                        }
                        Err(TryRecvError::Empty) => break,
                    }
                }
                if stop.load(Ordering::Acquire) || reconfigure_requested {
                    break;
                }

                // 2. Always accept inbound TCP connections on 24800 (even while already connected to N peers!)
                loop {
                    match listener.accept() {
                        Ok((stream, addr)) => {
                            let tx = hs_tx.clone();
                            let dev_id = local_device_id.clone();
                            let dev_name = local_name.clone();
                            let disp = local_displays.clone();
                            let pass = config.passphrase.clone();
                            let rev = layout_book.revision();
                            let screens = layout_book.active_screens().to_vec();
                            let _ = thread::Builder::new()
                                .name("crossflow-mesh-accept".into())
                                .spawn(move || {
                                    match establish_mesh_peer(
                                        stream,
                                        addr,
                                        false,
                                        &dev_id,
                                        &dev_name,
                                        disp,
                                        pass.as_deref(),
                                        rev,
                                        screens,
                                    ) {
                                        Ok(peer) => {
                                            let _ = tx.send(HandshakeMsg::Ready {
                                                dial_key: None,
                                                peer,
                                            });
                                        }
                                        Err(e) => {
                                            let _ = tx.send(HandshakeMsg::Failed {
                                                dial_key: None,
                                                is_auth_mismatch: is_passphrase_mismatch_error(&e),
                                                message: format!("{e:#}"),
                                            });
                                        }
                                    }
                                });
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }

                // 3. Background dial discovered peers & manual peer IPs not yet connected
                let candidates: Vec<Candidate> = discovery
                    .try_snapshot()
                    .map(|s| s.candidates.into_iter().filter(|c| c.compatible).collect())
                    .unwrap_or_default();

                if last_dial_scan.elapsed() >= Duration::from_millis(650) {
                    last_dial_scan = Instant::now();
                    let now_inst = Instant::now();
                    let fallback_dial = started_at.elapsed() >= Duration::from_millis(1800);

                    for c in &candidates {
                        if active_peers.keys().any(|id| ids_match(id, &c.device_id))
                            || in_flight_dials.contains(&c.device_id)
                            || dial_backoff
                                .get(&c.device_id)
                                .is_some_and(|until| now_inst < *until)
                        {
                            continue;
                        }
                        if !(local_discovery_id < c.device_id || fallback_dial) {
                            continue;
                        }
                        let eff_ch = effective_channel_for_peer(&config, &c.device_id);
                        let addrs = ranked_addresses(&c.addresses, eff_ch, &local_channels);
                        if addrs.is_empty() {
                            continue;
                        }
                        let dial_key = c.device_id.clone();
                        in_flight_dials.insert(dial_key.clone());
                        let tx = hs_tx.clone();
                        let dev_id = local_device_id.clone();
                        let dev_name = local_name.clone();
                        let disp = local_displays.clone();
                        let pass = config.passphrase.clone();
                        let rev = layout_book.revision();
                        let screens = layout_book.active_screens().to_vec();
                        let _ = thread::Builder::new()
                            .name("crossflow-mesh-dial".into())
                            .spawn(move || {
                                let mut last_err = None;
                                for addr in addrs {
                                    match TcpStream::connect_timeout(
                                        &addr,
                                        Duration::from_millis(280),
                                    ) {
                                        Ok(stream) => {
                                            match establish_mesh_peer(
                                                stream,
                                                addr,
                                                true,
                                                &dev_id,
                                                &dev_name,
                                                disp.clone(),
                                                pass.as_deref(),
                                                rev,
                                                screens.clone(),
                                            ) {
                                                Ok(peer) => {
                                                    let _ = tx.send(HandshakeMsg::Ready {
                                                        dial_key: Some(dial_key),
                                                        peer,
                                                    });
                                                    return;
                                                }
                                                Err(e) => {
                                                    last_err = Some(e);
                                                }
                                            }
                                        }
                                        Err(e) => last_err = Some(e.into()),
                                    }
                                }
                                let err = last_err
                                    .unwrap_or_else(|| anyhow::anyhow!("peer unreachable"));
                                let _ = tx.send(HandshakeMsg::Failed {
                                    dial_key: Some(dial_key),
                                    is_auth_mismatch: is_passphrase_mismatch_error(&err),
                                    message: format!("{err:#}"),
                                });
                            });
                    }

                    // Manual static peer IP list
                    for mp in &config.manual_peers {
                        let Some(target) = parse_direct_peer(Some(&mp.address)) else {
                            continue;
                        };
                        let key = format!("manual:{}", target);
                        if active_peers.values().any(|p| p.peer_addr.ip() == target.ip())
                            || in_flight_dials.contains(&key)
                            || dial_backoff
                                .get(&key)
                                .is_some_and(|until| now_inst < *until)
                        {
                            continue;
                        }
                        in_flight_dials.insert(key.clone());
                        let tx = hs_tx.clone();
                        let dev_id = local_device_id.clone();
                        let dev_name = local_name.clone();
                        let disp = local_displays.clone();
                        let pass = config.passphrase.clone();
                        let rev = layout_book.revision();
                        let screens = layout_book.active_screens().to_vec();
                        let _ = thread::Builder::new()
                            .name("crossflow-mesh-dial-manual".into())
                            .spawn(move || {
                                match TcpStream::connect_timeout(
                                    &target,
                                    Duration::from_millis(300),
                                )
                                .map_err(anyhow::Error::from)
                                .and_then(|stream| {
                                    establish_mesh_peer(
                                        stream,
                                        target,
                                        true,
                                        &dev_id,
                                        &dev_name,
                                        disp,
                                        pass.as_deref(),
                                        rev,
                                        screens,
                                    )
                                }) {
                                    Ok(peer) => {
                                        let _ = tx.send(HandshakeMsg::Ready {
                                            dial_key: Some(key),
                                            peer,
                                        });
                                    }
                                    Err(e) => {
                                        let _ = tx.send(HandshakeMsg::Failed {
                                            dial_key: Some(key),
                                            is_auth_mismatch: is_passphrase_mismatch_error(&e),
                                            message: format!("{e:#}"),
                                        });
                                    }
                                }
                            });
                    }
                }

                // 4. Integrate newly handshaked peers
                let mut topology_changed = false;
                while let Ok(msg) = hs_rx.try_recv() {
                    did_work = true;
                    match msg {
                        HandshakeMsg::Failed {
                            dial_key,
                            is_auth_mismatch,
                            message,
                        } => {
                            if let Some(k) = dial_key {
                                in_flight_dials.remove(&k);
                                let wait = if is_auth_mismatch {
                                    Duration::from_millis(2500)
                                } else {
                                    Duration::from_millis(1200)
                                };
                                dial_backoff.insert(k, Instant::now() + wait);
                            }
                            log.write("mesh_handshake_failed", serde_json::json!({"auth": is_auth_mismatch, "reason": message}));
                            if is_auth_mismatch {
                                last_auth_error =
                                    Some("等待密码匹配：请确认各台设备的密码配置一致。".into());
                            }
                        }
                        HandshakeMsg::Ready { dial_key, peer } => {
                            if let Some(k) = dial_key {
                                in_flight_dials.remove(&k);
                                dial_backoff.remove(&k);
                            }
                            last_auth_error = None;
                            let rid = peer.remote_device_id.clone();
                            let keep_new = match active_peers.get(&rid) {
                                None => true,
                                Some(existing) => {
                                    let want_init = preferred_initiator(&local_device_id, &rid);
                                    peer.initiator == want_init && existing.initiator != want_init
                                }
                            };
                            if !keep_new {
                                continue;
                            }
                            let aux_initiator = preferred_initiator(&local_device_id, &rid);
                            let aux = AuxSession::spawn_mesh_peer(
                                aux_hub.clone(),
                                peer.peer_addr,
                                aux_initiator,
                                local_device_id.clone(),
                                rid.clone(),
                                config.passphrase.clone(),
                                clipboard_sync_flag.clone(),
                                input_state_sync_flag.clone(),
                                local_has_focus.clone(),
                            );
                            let active_ch =
                                classify_active_channel(peer.peer_addr, &local_channels);
                            let peer_rev = peer.layout_revision;
                            let peer_screens = peer.layout_screens;
                            active_peers.insert(
                                rid.clone(),
                                ActiveMeshPeer {
                                    device_id: rid.clone(),
                                    name: peer.name,
                                    platform: peer.platform,
                                    peer_addr: peer.peer_addr,
                                    initiator: peer.initiator,
                                    active_channel_label: active_ch,
                                    displays: peer.displays,
                                    conn: peer.conn,
                                    aux,
                                },
                            );
                            let remotes = remote_display_map(&active_peers);
                            let adopted = layout_book
                                .merge_remote_layout(
                                    peer_rev,
                                    &peer_screens,
                                    &local_device_id,
                                    &local_displays,
                                    &remotes,
                                )
                                .ok()
                                .flatten();
                            if adopted.is_some() {
                                layout_book.save(&layout_file);
                                broadcast_layout_sync(
                                    &mut active_peers,
                                    Some(&rid),
                                    layout_book.revision(),
                                    layout_book.active_screens(),
                                );
                            }
                            topology_changed = true;
                        }
                    }
                }

                // 5. Pump all connected peers (non-blocking read/write + keepalive)
                let mut dead_peers: Vec<(String, &'static str)> = Vec::new();
                let peer_ids: Vec<String> = active_peers.keys().cloned().collect();
                let mut inbound_layout_updates: Vec<(String, u64, Vec<crate::session_metadata::SharedScreen>)> = Vec::new();
                let mut revoke_outbound_from: Vec<String> = Vec::new();

                for pid in peer_ids {
                    let Some(peer) = active_peers.get_mut(&pid) else {
                        continue;
                    };
                    if peer.conn.last_tx_elapsed() >= PEER_KEEPALIVE_INTERVAL {
                        let _ = peer.conn.queue_control(&MeshControl::KeepAlive);
                    }
                    if peer.conn.last_rx_elapsed() >= PEER_SILENCE_TIMEOUT {
                        dead_peers.push((pid, "silence_timeout"));
                        continue;
                    }
                    match peer.conn.pump() {
                        Ok(packets) => {
                            if !packets.is_empty() {
                                did_work = true;
                            }
                            for pkt in packets {
                                match pkt {
                                    MeshPacket::Input(frame) => {
                                        if let Some(ref mut io) = native_io {
                                            match frame.kind {
                                                Kind::Arm => {
                                                    if let Some(old_owner) = inbound_owner.take()
                                                        && old_owner != pid
                                                    {
                                                        revoke_outbound_from.push(old_owner);
                                                    }
                                                    let _ = io.sink.reset();
                                                    inbound_owner = Some(pid.clone());
                                                }
                                                Kind::Pause => {
                                                    if inbound_owner.as_deref() == Some(pid.as_str())
                                                    {
                                                        let _ = io.sink.release_owned();
                                                        inbound_owner = None;
                                                    }
                                                }
                                                _ => {
                                                    if inbound_owner.as_deref() == Some(pid.as_str())
                                                    {
                                                        if let Err(err) =
                                                            apply_inbound_frame(&mut io.sink, &frame)
                                                        {
                                                            log.write(
                                                                "mesh_inbound_frame_err",
                                                                serde_json::json!({
                                                                    "kind": format!("{:?}", frame.kind),
                                                                    "reason": format!("{err:#}"),
                                                                }),
                                                            );
                                                            let _ = io.sink.release_owned();
                                                            inbound_owner = None;
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    MeshPacket::Control(ctrl) => match ctrl {
                                        MeshControl::LayoutSync { revision, screens } => {
                                            inbound_layout_updates
                                                .push((pid.clone(), revision, screens));
                                        }
                                        MeshControl::FocusState {
                                            focus_device_id,
                                            focus_screen_id,
                                        } => {
                                            if active_outbound_target.is_none() {
                                                remote_focus_state =
                                                    (focus_device_id, focus_screen_id);
                                            }
                                        }
                                        MeshControl::RevokeControl => {
                                            log.write(
                                                "mesh_revoke_control",
                                                serde_json::json!({
                                                    "direction": "rx",
                                                    "peer": &pid[..pid.len().min(8)],
                                                }),
                                            );
                                            if active_outbound_target.as_deref()
                                                == Some(pid.as_str())
                                            {
                                                if let Some(ref mut io) = native_io
                                                    && let Ok(Some(pt)) =
                                                        io.source.force_return_local_with_clock(|| log.ns())
                                                    && let Some(ref mut cap) = io.capture
                                                {
                                                    let _ = cap.warp_local(pt);
                                                }
                                                active_outbound_target = None;
                                            }
                                        }
                                        MeshControl::KeepAlive | MeshControl::Hello { .. } => {}
                                    },
                                }
                            }
                        }
                        Err(_) => {
                            dead_peers.push((pid, "pump_error"));
                        }
                    }
                }

                // Notify displaced inbound owners
                for old_id in revoke_outbound_from {
                    if let Some(p) = active_peers.get_mut(&old_id) {
                        let _ = p.conn.queue_control(&MeshControl::RevokeControl);
                        let _ = p.conn.flush();
                    }
                }

                // Merge any received LayoutSync updates
                for (from_id, rev, screens) in inbound_layout_updates {
                    let remotes = remote_display_map(&active_peers);
                    if let Ok(Some(new_rt)) = layout_book.merge_remote_layout(
                        rev,
                        &screens,
                        &local_device_id,
                        &local_displays,
                        &remotes,
                    ) {
                        layout_book.save(&layout_file);
                        screen_numbers_dirty = true;
                        if let Some(ref io) = native_io {
                            let _ = io.source.update_runtime(new_rt);
                        }
                        broadcast_layout_sync(
                            &mut active_peers,
                            Some(&from_id),
                            layout_book.revision(),
                            layout_book.active_screens(),
                        );
                    }
                }

                // Clean up disconnected peers without disrupting remaining peers!
                for (dead, reason) in dead_peers {
                    log.write(
                        "mesh_peer_dropped",
                        serde_json::json!({
                            "peer": &dead[..dead.len().min(8)],
                            "reason": reason,
                        }),
                    );
                    active_peers.remove(&dead);
                    if active_outbound_target.as_deref() == Some(dead.as_str()) {
                        active_outbound_target = None;
                        if let Some(ref mut io) = native_io
                            && let Ok(Some(pt)) =
                                io.source.force_return_local_with_clock(|| log.ns())
                            && let Some(ref mut cap) = io.capture
                        {
                            let _ = cap.warp_local(pt);
                        }
                    }
                    if inbound_owner.as_deref() == Some(dead.as_str()) {
                        inbound_owner = None;
                        if let Some(ref mut io) = native_io {
                            let _ = io.sink.release_owned();
                        }
                    }
                    topology_changed = true;
                }
                aux_hub.set_has_peers(!active_peers.is_empty());

                // 6. Start, hot-update, or stop native Capture + WorkerProxy
                if active_peers.is_empty() {
                    if let Some(io) = native_io.take() {
                        io.shutdown();
                    }
                    active_outbound_target = None;
                    inbound_owner = None;
                    remote_focus_state = (None, None);
                } else if topology_changed || native_io.is_none() {
                    let remotes = remote_display_map(&active_peers);
                    if let Ok(rt) =
                        layout_book.rebuild(&local_device_id, &local_displays, &remotes)
                    {
                        layout_book.save(&layout_file);
                        if let Some(ref io) = native_io {
                            let _ = io.source.update_runtime(rt);
                        } else if let Ok(source) = SharedSource::new(rt) {
                            source.set_edge_locked(screen_locked);
                            hotkey_engine.reset_modifiers();
                            let _ = hotkey_engine.drain_actions();
                            if let Ok(mut sink) = prepare_worker(&local_displays, log.clone()) {
                                match (platform.capture)(source.clone(), log.clone()) {
                                    Ok(capture) => {
                                        let last_activity = source.activity_counter();
                                        native_io = Some(MeshNativeIo {
                                            source,
                                            capture: Some(capture),
                                            sink,
                                            last_activity,
                                            last_sink_maintain: Instant::now(),
                                            last_capture_retry: Instant::now(),
                                            enabled_attempted: false,
                                        });
                                    }
                                    Err(_) if sink.is_service_backed() => {
                                        // Lock screen / secure desktop: keep elevated service sink
                                        // alive so remote peers can still control and unlock!
                                        let last_activity = source.activity_counter();
                                        native_io = Some(MeshNativeIo {
                                            source,
                                            capture: None,
                                            sink,
                                            last_activity,
                                            last_sink_maintain: Instant::now(),
                                            last_capture_retry: Instant::now(),
                                            enabled_attempted: false,
                                        });
                                    }
                                    Err(_) => {
                                        let _ = sink.shutdown();
                                    }
                                }
                            }
                        }
                        broadcast_layout_sync(
                            &mut active_peers,
                            None,
                            layout_book.revision(),
                            layout_book.active_screens(),
                        );
                        screen_numbers_dirty = true;
                    }
                }
                if screen_numbers_dirty || topology_changed {
                    screen_numbers_dirty = false;
                    let active_slice = if active_peers.is_empty() {
                        &[][..]
                    } else {
                        layout_book.active_screens()
                    };
                    numbered_screens = build_numbered_screens(
                        active_slice,
                        &local_device_id,
                        &local_name,
                        &local_displays,
                        &active_peers,
                    );
                    hotkey_engine.set_screen_count(numbered_screens.len());
                }

                // 7. Poll native capture, process custom hotkeys / QuickJump, route 1-hop frames!
                let mut restart_native: Option<&'static str> = None;
                if let Some(ref mut io) = native_io {
                    // Retry starting capture if we are currently in receive-only service mode (e.g. after unlock)
                    if io.capture.is_none()
                        && io.last_capture_retry.elapsed() >= Duration::from_millis(1200)
                    {
                        io.last_capture_retry = Instant::now();
                        let rt = io.source.runtime();
                        if let Ok(fresh_source) = SharedSource::new(rt) {
                            fresh_source.set_edge_locked(screen_locked);
                            if let Ok(cap) = (platform.capture)(fresh_source.clone(), log.clone()) {
                                io.last_activity = fresh_source.activity_counter();
                                io.source = fresh_source;
                                io.capture = Some(cap);
                                io.enabled_attempted = false;
                            }
                        }
                    }

                    let mut capture_failed = false;
                    if let Some(ref mut cap) = io.capture {
                        if let Err(err) = cap.poll(log.ns()) {
                            log.write(
                                "mesh_native_restart",
                                serde_json::json!({"reason": "capture_poll_err", "detail": format!("{err:#}")}),
                            );
                            capture_failed = true;
                        } else if let Some(failure) = io.source.status().failure {
                            log.write(
                                "mesh_native_restart",
                                serde_json::json!({"reason": "source_failure", "detail": format!("{failure:?}")}),
                            );
                            capture_failed = true;
                        } else if let Some(failure) = cap.failure() {
                            log.write(
                                "mesh_native_restart",
                                serde_json::json!({"reason": "capture_failure", "detail": failure}),
                            );
                            capture_failed = true;
                        }
                    }

                    if capture_failed {
                        if io.sink.is_service_backed() {
                            // Keep LocalSystem service sink alive across lock-screen transitions!
                            if let Some(mut old_cap) = io.capture.take() {
                                let _ = old_cap.stop();
                            }
                            if let Some(prev) = active_outbound_target.take()
                                && let Some(p) = active_peers.get_mut(&prev)
                            {
                                let _ = p.conn.queue_frame(&Frame::new(Kind::Pause));
                                let _ = p.conn.flush();
                            }
                            io.last_capture_retry = Instant::now();
                        } else {
                            restart_native = Some("capture_failed");
                        }
                    } else if let Some(ref mut cap) = io.capture {
                        if let Some(sc) = cap.command() {
                            match sc {
                                Shortcut::Stop => {
                                    user_paused = true;
                                    io.source.pause_mesh();
                                }
                                Shortcut::Toggle => {
                                    user_paused = !user_paused;
                                    if user_paused {
                                        io.source.pause_mesh();
                                    }
                                }
                            }
                        }

                        // Auto-enable once capture snapshot is ready unless user paused
                        let st = io.source.status();
                        if !user_paused && st.ready && !st.enabled {
                            if io.source.enable_mesh().is_ok() {
                                io.enabled_attempted = true;
                            }
                        } else if user_paused && st.enabled {
                            io.source.pause_mesh();
                        }

                        // Drain custom hotkeys
                        for action in hotkey_engine.drain_actions() {
                            did_work = true;
                            match action {
                                HotkeyAction::EmergencyStop => {
                                    user_paused = true;
                                    pending_quick_jump = None;
                                    io.source.pause_mesh();
                                    let _ = io.sink.release_owned();
                                    if let Some(prev) = active_outbound_target.take()
                                        && let Some(p) = active_peers.get_mut(&prev)
                                    {
                                        let _ = p.conn.queue_frame(&Frame::new(Kind::Pause));
                                        let _ = p.conn.flush();
                                    }
                                }
                                HotkeyAction::ToggleSharing => {
                                    user_paused = !user_paused;
                                    pending_quick_jump = None;
                                    if user_paused {
                                        io.source.pause_mesh();
                                        if let Some(prev) = active_outbound_target.take()
                                            && let Some(p) = active_peers.get_mut(&prev)
                                        {
                                            let _ = p.conn.queue_frame(&Frame::new(Kind::Pause));
                                            let _ = p.conn.flush();
                                        }
                                    } else if io.source.status().ready {
                                        let _ = io.source.enable_mesh();
                                    }
                                }
                                HotkeyAction::ToggleLockScreen => {
                                    screen_locked = !screen_locked;
                                    io.source.set_edge_locked(screen_locked);
                                }
                                HotkeyAction::QuickJumpNext => {
                                    if !user_paused
                                        && let Some(t) = resolve_mesh_quick_jump_target(
                                            None,
                                            &local_device_id,
                                            active_outbound_target.as_deref(),
                                            &active_peers,
                                        )
                                    {
                                        pending_quick_jump =
                                            Some((QuickJumpTarget::Device(t), Instant::now()));
                                    }
                                }
                                HotkeyAction::QuickJumpHome => {
                                    if !user_paused {
                                        pending_quick_jump = Some((
                                            QuickJumpTarget::Device(local_device_id.clone()),
                                            Instant::now(),
                                        ));
                                    }
                                }
                                HotkeyAction::QuickJumpDevice(idx) => {
                                    if !user_paused
                                        && let Some(id) = active_peers.keys().nth(idx).cloned()
                                    {
                                        pending_quick_jump =
                                            Some((QuickJumpTarget::Device(id), Instant::now()));
                                    }
                                }
                                HotkeyAction::QuickJumpScreen(idx) => {
                                    if !user_paused
                                        && let Some(info) = numbered_screens.get(idx)
                                    {
                                        pending_quick_jump = Some((
                                            QuickJumpTarget::Screen(info.screen_id.clone()),
                                            Instant::now(),
                                        ));
                                    }
                                }
                            }
                        }

                        // Execute latched QuickJump once modifiers & physical keys are clear
                        if let Some((target, created_at)) = pending_quick_jump.clone() {
                            if user_paused || created_at.elapsed() >= Duration::from_secs(2) {
                                pending_quick_jump = None;
                            } else if io.source.status().enabled
                                && !hotkey_engine.any_modifier_held()
                                && !io.source.has_held_physical_input()
                            {
                                pending_quick_jump = None;
                                let warp = match target {
                                    QuickJumpTarget::Device(dev_id) => io
                                        .source
                                        .quick_jump_to_device_with_clock(&dev_id, || log.ns()),
                                    QuickJumpTarget::Screen(screen_id) => io
                                        .source
                                        .quick_jump_to_screen_with_clock(&screen_id, || log.ns()),
                                };
                                if let Ok(Some(local_pt)) = warp {
                                    let _ = cap.warp_local(local_pt);
                                }
                            }
                        }

                        // Local physical activity revokes inbound remote injection cleanly
                        let act = io.source.activity_counter();
                        if act != io.last_activity {
                            io.last_activity = act;
                            if let Some(owner_id) = inbound_owner.take() {
                                log.write(
                                    "mesh_revoke_control",
                                    serde_json::json!({
                                        "direction": "tx",
                                        "peer": &owner_id[..owner_id.len().min(8)],
                                    }),
                                );
                                let _ = io.sink.release_owned();
                                if let Some(p) = active_peers.get_mut(&owner_id) {
                                    let _ = p.conn.queue_control(&MeshControl::RevokeControl);
                                    let _ = p.conn.flush();
                                }
                            }
                        }

                        // Drain outbound frames and route directly 1-hop to target peer!
                        let mut drained = 0usize;
                        while drained < 32 {
                            let allow_motion = last_motion_send.elapsed() >= motion_interval;
                            let next = match io
                                .source
                                .next_mesh_frame_with_clock(|| log.ns(), allow_motion)
                            {
                                Ok(v) => v,
                                Err(err) => {
                                    log.write(
                                        "mesh_native_restart",
                                        serde_json::json!({"reason": "next_frame_err", "detail": format!("{err:#}")}),
                                    );
                                    restart_native = Some("next_frame_err");
                                    break;
                                }
                            };
                            let Some((target, frame)) = next else {
                                break;
                            };
                            did_work = true;
                            drained += 1;
                            if matches!(frame.kind, Kind::Move | Kind::Position) {
                                last_motion_send = Instant::now();
                            }

                            if active_outbound_target != target {
                                if let Some(old_id) = active_outbound_target.take()
                                    && let Some(old_peer) = active_peers.get_mut(&old_id)
                                {
                                    let _ = old_peer.conn.queue_frame(&Frame::new(Kind::Pause));
                                    let _ = old_peer.conn.flush();
                                }
                                if let Some(ref new_id) = target
                                    && let Some(new_peer) = active_peers.get(new_id)
                                {
                                    new_peer.aux.push_input_state_now();
                                }
                                active_outbound_target = target.clone();
                            }

                            if let Some(ref tid) = active_outbound_target
                                && let Some(peer) = active_peers.get_mut(tid)
                            {
                                let _ = peer.conn.queue_frame(&frame);
                                let _ = peer.conn.flush();
                            }
                        }
                    }

                    // Keep input-worker heartbeat healthy (even when capture is paused on lock screen!)
                    if restart_native.is_none()
                        && io.last_sink_maintain.elapsed() >= Duration::from_millis(15)
                    {
                        io.last_sink_maintain = Instant::now();
                        if let Err(err) = io.sink.maintain() {
                            log.write(
                                "mesh_native_restart",
                                serde_json::json!({"reason": "sink_maintain_err", "detail": format!("{err:#}")}),
                            );
                            restart_native = Some("sink_maintain_err");
                        }
                    }
                }

                if restart_native.is_some() {
                    if let Some(io) = native_io.take() {
                        io.shutdown();
                    }
                    if let Some(prev) = active_outbound_target.take()
                        && let Some(p) = active_peers.get_mut(&prev)
                    {
                        let _ = p.conn.queue_frame(&Frame::new(Kind::Pause));
                        let _ = p.conn.flush();
                    }
                    inbound_owner = None;
                    if let Ok(inv) = (platform.inventory)()
                        && !inv.is_empty()
                    {
                        local_displays = inv;
                    }
                }

                // 8. Central mesh clipboard broadcast (non-blocking drain from background poller)
                if !active_peers.is_empty() {
                    while let Some((item, hash)) = aux_hub.take_local_clipboard() {
                        did_work = true;
                        for peer in active_peers.values() {
                            peer.aux.push_clipboard(item.clone(), hash.clone());
                        }
                    }
                }

                // 9. Track local_has_focus & broadcast FocusState changes
                let (focus_dev, focus_scr) = if let Some(ref io) = native_io
                    && !user_paused
                    && io.source.status().enabled
                {
                    if active_outbound_target.is_some() {
                        io.source.focus_identity()
                    } else if inbound_owner.is_some() {
                        remote_focus_state.clone()
                    } else {
                        let local_f = io.source.focus_identity();
                        if local_f.0.is_some() {
                            local_f
                        } else {
                            remote_focus_state.clone()
                        }
                    }
                } else {
                    (None, None)
                };
                let is_local_focus_now = focus_dev.as_deref() == Some(local_device_id.as_str())
                    || (active_outbound_target.is_none() && inbound_owner.is_none());
                local_has_focus.store(is_local_focus_now, Ordering::Release);

                if !active_peers.is_empty()
                    && (focus_dev.as_deref() == Some(local_device_id.as_str())
                        || active_outbound_target.is_some())
                    && (focus_dev.clone(), focus_scr.clone()) != last_broadcast_focus
                {
                    last_broadcast_focus = (focus_dev.clone(), focus_scr.clone());
                    let ctrl = MeshControl::FocusState {
                        focus_device_id: focus_dev.clone(),
                        focus_screen_id: focus_scr.clone(),
                    };
                    for peer in active_peers.values_mut() {
                        let _ = peer.conn.queue_control(&ctrl);
                        let _ = peer.conn.flush();
                    }
                }

                // 10. Emit unified multi-device PeerUiStatus
                let transfer = active_transfer_snapshot(&active_peers);
                let sharing_now = !active_peers.is_empty() && !user_paused;
                let state_str = if active_peers.is_empty() {
                    "searching"
                } else if user_paused {
                    "paused"
                } else {
                    "sharing"
                };

                let mut peers_list: Vec<PeerInfo> = Vec::with_capacity(active_peers.len() + candidates.len());
                for (idx, p) in active_peers.values().enumerate() {
                    peers_list.push(PeerInfo {
                        id: p.device_id.clone(),
                        name: p.name.clone(),
                        platform: p.platform.clone(),
                        connected: true,
                        focused: focus_dev.as_deref() == Some(p.device_id.as_str()),
                        slot: Some(idx + 1),
                        address: Some(p.peer_addr.to_string()),
                        active_channel_label: p.active_channel_label.clone(),
                        preferred_channel: peer_channel_override(&config.peer_channels, &p.device_id),
                    });
                }
                for c in &candidates {
                    if active_peers.keys().any(|id| ids_match(id, &c.device_id)) {
                        continue;
                    }
                    peers_list.push(PeerInfo {
                        id: c.device_id.clone(),
                        name: c.friendly_name.clone(),
                        platform: c.platform.clone(),
                        connected: false,
                        focused: false,
                        slot: None,
                        address: None,
                        active_channel_label: None,
                        preferred_channel: peer_channel_override(&config.peer_channels, &c.device_id),
                    });
                }

                let service_mode_installed = service_mode_flags.0;
                let service_mode_active = native_io
                    .as_ref()
                    .map(|io| io.sink.is_service_backed())
                    .unwrap_or(service_mode_flags.1 && service_mode_flags.2);
                let sig = format!(
                    "{state_str}|{:?}|{:?}|{screen_locked}|{service_mode_installed}|{service_mode_active}|{}|{}|{}|{:?}",
                    focus_dev,
                    focus_scr,
                    peers_list.len(),
                    config.manual_peers.len(),
                    layout_book.revision(),
                    last_auth_error
                );
                if sig != last_status_sig
                    || last_status_emit.elapsed() >= Duration::from_millis(250)
                    || transfer.as_ref().is_some_and(|t| t.state == "active")
                {
                    last_status_sig = sig;
                    last_status_emit = Instant::now();
                    let first_peer = active_peers.values().next();
                    let shared_layout = first_peer.and_then(|fp| {
                        layout_book.to_shared_layout(&local_device_id, &fp.device_id)
                    });
                    let discovered_compat: Vec<DiscoveredPeerInfo> = peers_list
                        .iter()
                        .map(|p| DiscoveredPeerInfo {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            platform: p.platform.clone(),
                            connected: p.connected,
                        })
                        .collect();
                    let has_pass = config.passphrase.is_some();
                    emit_status(
                        &stdout,
                        &PeerUiStatus {
                            protocol: PEER_UI_PROTOCOL,
                            state: state_str.into(),
                            local_device_id: local_device_id.clone(),
                            local_name: local_name.clone(),
                            auth_mode: if has_pass { "passphrase" } else { "open" },
                            passphrase_configured: has_pass,
                            smart_keymap: config.smart_keymap,
                            clipboard_sync: config.clipboard_sync,
                            input_state_sync: config.input_state_sync,
                            screen_locked,
                            service_mode_installed,
                            service_mode_active,
                            hotkeys: config.hotkeys.clone(),
                            selected_channel: config.channel.clone(),
                            direct_peer: config.direct_peer.clone(),
                            active_channel_label: first_peer
                                .and_then(|p| p.active_channel_label.clone()),
                            network_channels: channel_infos(&local_channels),
                            manual_peers: config.manual_peers.clone(),
                            peers: peers_list,
                            route_enabled: sharing_now,
                            focus_device_id: focus_dev,
                            focus_screen_id: focus_scr,
                            peer_device_id: first_peer.map(|p| p.device_id.clone()),
                            peer_name: first_peer.map(|p| p.name.clone()),
                            peer_address: first_peer.map(|p| p.peer_addr.to_string()),
                            listen_endpoint: listen_endpoint.clone(),
                            local_displays: local_displays.clone(),
                            discovered_peers: discovered_compat,
                            shared_layout,
                            screen_numbers: numbered_screens.clone(),
                            transfer,
                            error: if active_peers.is_empty() {
                                last_auth_error.clone()
                            } else {
                                None
                            },
                        },
                    );
                }

                if !did_work {
                    thread::sleep(Duration::from_millis(if active_peers.is_empty() {
                        20
                    } else {
                        1
                    }));
                }
            }

            if let Some(io) = native_io.take() {
                io.shutdown();
            }
            channel_watch_stop.store(true, Ordering::Release);
            aux_hub.stop();
            discovery.stop();
            let _ = discovery.wait_stopped(Duration::from_secs(2));
        }
        Ok(())
    })();

    stop.store(true, Ordering::Release);
    let has_pass = config.passphrase.is_some();
    emit_status(
        &stdout,
        &PeerUiStatus {
            protocol: PEER_UI_PROTOCOL,
            state: if result.is_ok() {
                "stopped".into()
            } else {
                "error".into()
            },
            local_device_id,
            local_name,
            auth_mode: if has_pass { "passphrase" } else { "open" },
            passphrase_configured: has_pass,
            smart_keymap: config.smart_keymap,
            clipboard_sync: config.clipboard_sync,
            input_state_sync: config.input_state_sync,
            screen_locked,
            service_mode_installed: false,
            service_mode_active: false,
            hotkeys: config.hotkeys,
            selected_channel: config.channel,
            direct_peer: config.direct_peer,
            active_channel_label: None,
            network_channels: vec![],
            manual_peers: config.manual_peers,
            peers: vec![],
            route_enabled: false,
            focus_device_id: None,
            focus_screen_id: None,
            peer_device_id: None,
            peer_name: None,
            peer_address: None,
            listen_endpoint: None,
            local_displays: vec![],
            discovered_peers: vec![],
            shared_layout: None,
            screen_numbers: vec![],
            transfer: None,
            error: result.as_ref().err().map(|e| format!("{e:#}")),
        },
    );
    writer.finish()?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddrV4;

    fn sample_channels() -> Vec<LocalChannel> {
        vec![
            LocalChannel {
                id: "192.168.50.1".into(),
                label: "有线直连 · 192.168.50.1".into(),
                kind: "wired",
                priority: 0,
                ip: Ipv4Addr::new(192, 168, 50, 1),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                interface_name: "以太网 3".into(),
            },
            LocalChannel {
                id: "192.168.1.164".into(),
                label: "Wi-Fi · 192.168.1.164".into(),
                kind: "wifi",
                priority: 2,
                ip: Ipv4Addr::new(192, 168, 1, 164),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                interface_name: "WLAN".into(),
            },
            LocalChannel {
                id: "192.168.194.10".into(),
                label: "ZeroTier 虚拟局域网 · 192.168.194.10".into(),
                kind: "zerotier",
                priority: 3,
                ip: Ipv4Addr::new(192, 168, 194, 10),
                netmask: Ipv4Addr::new(255, 255, 255, 0),
                interface_name: "ZeroTier One".into(),
            },
        ]
    }

    #[test]
    fn auto_channel_ranks_wired_ahead_of_wifi_and_zerotier_and_filters_tun() {
        let local = sample_channels();
        let wifi = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 188), 24800));
        let wired = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 2), 24800));
        let zt = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 194, 59), 24800));
        let tun = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(198, 18, 0, 1), 24800));
        let loopback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 24800));

        let auto_ranked = ranked_addresses(&[loopback, tun, zt, wifi, wired], "auto", &local);
        assert_eq!(auto_ranked, vec![wired, wifi, zt]);
    }

    #[test]
    fn pinned_channel_restricts_dialing_to_matching_subnet() {
        let local = sample_channels();
        let wifi = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 188), 24800));
        let wired = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 2), 24800));
        let zt = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 194, 59), 24800));

        assert_eq!(
            ranked_addresses(&[zt, wifi, wired], "192.168.194.10", &local),
            vec![zt]
        );
        assert_eq!(
            ranked_addresses(&[zt, wifi, wired], "192.168.1.164", &local),
            vec![wifi]
        );
        assert_eq!(
            ranked_addresses(&[zt, wifi, wired], "192.168.50.1", &local),
            vec![wired]
        );
    }

    #[test]
    fn direct_peer_parser_accepts_bare_ip_or_host_port() {
        assert_eq!(
            parse_direct_peer(Some("192.168.194.59")),
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(192, 168, 194, 59),
                24800
            )))
        );
        assert_eq!(
            parse_direct_peer(Some("192.168.194.59:25000")),
            Some(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::new(192, 168, 194, 59),
                25000
            )))
        );
        assert_eq!(parse_direct_peer(Some("")), None);
    }

    #[test]
    fn peer_channel_override_supersedes_global_channel_and_matches_prefixes() {
        let mut cfg = PeerConfig::default();
        cfg.channel = "192.168.1.164".into();
        let full_id = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let prefix = &full_id[..32];

        assert_eq!(effective_channel_for_peer(&cfg, full_id), "192.168.1.164");
        set_peer_channel(&mut cfg, prefix, "192.168.50.1");
        assert_eq!(effective_channel_for_peer(&cfg, full_id), "192.168.50.1");
        assert_eq!(
            peer_channel_override(&cfg.peer_channels, full_id),
            "192.168.50.1"
        );
        // Clearing back to "auto" reverts to global default
        set_peer_channel(&mut cfg, full_id, "auto");
        assert_eq!(effective_channel_for_peer(&cfg, prefix), "192.168.1.164");
        assert_eq!(peer_channel_override(&cfg.peer_channels, prefix), "auto");
    }

    #[test]
    fn legacy_direct_peer_migrates_and_manual_peers_dedup_and_remove() {
        let mut cfg = PeerConfig {
            direct_peer: Some("192.168.194.59:24800".into()),
            ..PeerConfig::default()
        };
        normalize_config(&mut cfg);
        assert_eq!(cfg.manual_peers.len(), 1);
        assert_eq!(cfg.manual_peers[0].address, "192.168.194.59");

        assert!(add_manual_peer(
            &mut cfg,
            "192.168.194.59",
            Some("Mac Studio".into()),
            "auto"
        ));
        assert_eq!(cfg.manual_peers.len(), 1);
        assert_eq!(cfg.manual_peers[0].label.as_deref(), Some("Mac Studio"));

        assert!(add_manual_peer(
            &mut cfg,
            "10.0.0.12:25000",
            Some("Office PC".into()),
            "192.168.1.164"
        ));
        assert_eq!(cfg.manual_peers.len(), 2);
        assert!(!add_manual_peer(&mut cfg, "not-an-ip", None, "auto"));

        assert!(remove_manual_peer(&mut cfg, "192.168.194.59:24800"));
        assert_eq!(cfg.manual_peers.len(), 1);
        assert_eq!(cfg.manual_peers[0].address, "10.0.0.12:25000");
    }
}
