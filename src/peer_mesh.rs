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
    discovery::{self, Advertisement, Candidate, DiscoveryConfig, DiscoveryHandle},
    hotkeys::{HotkeyAction, HotkeyConfig, SharedHotkeyEngine},
    injection_worker::WorkerProxy,
    metrics::{Log, LogWriter},
    pair_native::{Capture, Platform, Shortcut},
    pair_session::{Command, Host, PairConnection, SessionStatus},
    pair_source::SharedSource,
    pair_wire::{Display, Hello},
    session::NativeDisplay,
    session_metadata::SharedLayout,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::{
    collections::HashMap,
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
    #[serde(default = "default_true")]
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
            smart_keymap: true,
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

fn layout_preset_path(remote_device_id: &str) -> Result<PathBuf> {
    let dir = app_data_root()?.join("peer-layouts");
    fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{remote_device_id}.json")))
}

fn load_saved_layout(remote_device_id: &str) -> Option<SharedLayout> {
    let path = layout_preset_path(remote_device_id).ok()?;
    let bytes = fs::read(path).ok()?;
    let layout: SharedLayout = serde_json::from_slice(&bytes).ok()?;
    layout.validate().ok()?;
    Some(layout)
}

fn save_layout(remote_device_id: &str, layout: &SharedLayout) {
    if layout.validate().is_err() {
        return;
    }
    if let Ok(path) = layout_preset_path(remote_device_id)
        && let Ok(bytes) = serde_json::to_vec_pretty(layout)
    {
        let _ = fs::write(path, bytes);
    }
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

fn hello(device_id: &str, platform: &Platform) -> Result<Hello> {
    let hello = Hello {
        device_id: device_id.to_string(),
        inventory: (platform.inventory)()?,
    };
    crate::pair_wire::validate_inventory(&hello.inventory)?;
    Ok(hello)
}

fn prepare_worker(inventory: &[Display], log: Log) -> Result<WorkerProxy> {
    let mut command = process::Command::new(std::env::current_exe()?);
    command.args(["input-worker", "--gain", "1"]);
    let mut sink = WorkerProxy::spawn(&mut command)?;
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

struct PeerHost<'a> {
    capture: &'a mut dyn Capture,
    source: &'a SharedSource,
    hotkey_engine: &'a SharedHotkeyEngine,
    rx: &'a Receiver<UiCommand>,
    stdout: &'a Mutex<std::io::Stdout>,
    discovery: &'a DiscoveryHandle,
    session_stop: &'a AtomicBool,
    global_stop: &'a AtomicBool,
    deadline: Instant,
    local_device_id: &'a str,
    local_name: &'a str,
    local_displays: Vec<Display>,
    config: &'a mut PeerConfig,
    smart_keymap_flag: &'a Arc<AtomicBool>,
    clipboard_sync_flag: &'a Arc<AtomicBool>,
    input_state_sync_flag: &'a Arc<AtomicBool>,
    local_has_focus: &'a Arc<AtomicBool>,
    aux: Option<&'a crate::aux_channel::AuxSession>,
    reconfigure_requested: &'a mut bool,
    network_channels: Vec<NetworkChannelInfo>,
    active_channel_label: Option<String>,
    listen_endpoint: Option<String>,
    peer_device_id: String,
    peer_name: String,
    peer_platform: String,
    peer_address: String,
    user_paused: &'a mut bool,
    screen_locked: &'a mut bool,
    pending_layout_push: &'a mut Option<(String, SharedLayout)>,
    pending_quick_jump: Option<(String, Instant)>,
    initiator: bool,
    status_seen: bool,
    auto_enable_sent: bool,
    sharing: bool,
    epoch: u64,
    pending: Option<Command>,
    last_emit: Instant,
    last_emitted_enabled: Option<bool>,
    last_emitted_focus_screen: Option<String>,
    last_emitted_peer_count: usize,
    last_emitted_locked: Option<bool>,
    last_emitted_hotkeys: Option<HotkeyConfig>,
    last_emitted_manual_peers: Option<Vec<ManualPeer>>,
    last_emitted_peer_channels: Option<HashMap<String, String>>,
}

impl PeerHost<'_> {
    fn discovered_list(&self) -> Vec<DiscoveredPeerInfo> {
        let Some(snap) = self.discovery.try_snapshot() else {
            return Vec::new();
        };
        snap.candidates
            .into_iter()
            .filter(|c| c.compatible)
            .map(|c| DiscoveredPeerInfo {
                connected: ids_match(&self.peer_device_id, &c.device_id),
                id: c.device_id,
                name: c.friendly_name,
                platform: c.platform,
            })
            .collect()
    }

    fn peers_list(&self, discovered: &[DiscoveredPeerInfo], focus_device_id: Option<&str>) -> Vec<PeerInfo> {
        let mut list = Vec::with_capacity(discovered.len() + 1);
        list.push(PeerInfo {
            id: self.peer_device_id.clone(),
            name: self.peer_name.clone(),
            platform: self.peer_platform.clone(),
            connected: true,
            focused: focus_device_id == Some(self.peer_device_id.as_str()),
            slot: Some(1),
            address: Some(self.peer_address.clone()),
            active_channel_label: self.active_channel_label.clone(),
            preferred_channel: peer_channel_override(&self.config.peer_channels, &self.peer_device_id),
        });
        for d in discovered {
            if ids_match(&self.peer_device_id, &d.id) {
                continue;
            }
            list.push(PeerInfo {
                id: d.id.clone(),
                name: d.name.clone(),
                platform: d.platform.clone(),
                connected: false,
                focused: false,
                slot: None,
                address: None,
                active_channel_label: None,
                preferred_channel: peer_channel_override(&self.config.peer_channels, &d.id),
            });
        }
        list
    }

    fn drain_ui_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(UiCommand::Stop) => {
                    self.global_stop.store(true, Ordering::Release);
                    self.session_stop.store(true, Ordering::Release);
                    self.pending = Some(Command::Stop);
                    break;
                }
                Ok(UiCommand::Pause) => {
                    *self.user_paused = true;
                    self.pending = Some(Command::Pause);
                }
                Ok(UiCommand::Resume) => {
                    *self.user_paused = false;
                    if self.status_seen {
                        self.pending = Some(Command::Enable { epoch: self.epoch });
                    }
                }
                Ok(UiCommand::ApplyLayout { layout }) => {
                    if layout.validate().is_ok() {
                        save_layout(&self.peer_device_id, &layout);
                        *self.pending_layout_push = Some((self.peer_device_id.clone(), layout));
                        self.session_stop.store(true, Ordering::Release);
                        self.pending = Some(Command::Stop);
                        break;
                    }
                }
                Ok(UiCommand::SetChannel {
                    channel,
                    direct_peer,
                }) => {
                    let clean = channel.trim();
                    self.config.channel = if clean.is_empty() {
                        default_channel()
                    } else {
                        clean.to_string()
                    };
                    self.config.direct_peer = direct_peer
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string);
                    if let Some(legacy) = self.config.direct_peer.clone() {
                        let ch = self.config.channel.clone();
                        let _ = add_manual_peer(self.config, &legacy, None, &ch);
                    }
                    save_config(self.config);
                    *self.reconfigure_requested = true;
                    self.session_stop.store(true, Ordering::Release);
                    self.pending = Some(Command::Stop);
                    break;
                }
                Ok(UiCommand::SetPeerChannel { device_id, channel }) => {
                    set_peer_channel(self.config, &device_id, &channel);
                    save_config(self.config);
                    if ids_match(&self.peer_device_id, &device_id) {
                        *self.reconfigure_requested = true;
                        self.session_stop.store(true, Ordering::Release);
                        self.pending = Some(Command::Stop);
                        break;
                    }
                }
                Ok(UiCommand::AddManualPeer {
                    address,
                    label,
                    channel,
                }) => {
                    if add_manual_peer(self.config, &address, label, &channel) {
                        save_config(self.config);
                    }
                }
                Ok(UiCommand::RemoveManualPeer { address }) => {
                    if remove_manual_peer(self.config, &address) {
                        self.config.direct_peer = None;
                        save_config(self.config);
                    }
                }
                Ok(UiCommand::SetPassphrase { passphrase }) => {
                    self.config.passphrase = passphrase
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToString::to_string);
                    save_config(self.config);
                    *self.reconfigure_requested = true;
                    self.session_stop.store(true, Ordering::Release);
                    self.pending = Some(Command::Stop);
                    break;
                }
                Ok(UiCommand::SetKeymap { enabled }) => {
                    self.config.smart_keymap = enabled;
                    self.smart_keymap_flag.store(enabled, Ordering::Relaxed);
                    save_config(self.config);
                }
                Ok(UiCommand::SetClipboard { enabled }) => {
                    self.config.clipboard_sync = enabled;
                    self.clipboard_sync_flag.store(enabled, Ordering::Relaxed);
                    save_config(self.config);
                }
                Ok(UiCommand::SetInputStateSync { enabled }) => {
                    self.config.input_state_sync = enabled;
                    self.input_state_sync_flag.store(enabled, Ordering::Relaxed);
                    save_config(self.config);
                }
                Ok(UiCommand::SetHotkeys { hotkeys }) => {
                    self.config.hotkeys = hotkeys.normalized();
                    self.hotkey_engine.update_config(&self.config.hotkeys);
                    save_config(self.config);
                }
                Ok(UiCommand::SetHotkeySuspended { suspended }) => {
                    self.hotkey_engine.set_suspended(suspended);
                }
                Ok(UiCommand::SetScreenLock { locked }) => {
                    *self.screen_locked = locked;
                    self.source.set_edge_locked(locked);
                }
                Ok(UiCommand::QuickJump { target_device_id }) => {
                    let target = self.resolve_quick_jump_target(target_device_id.as_deref());
                    self.queue_quick_jump(target);
                }
                Ok(UiCommand::SendFiles {
                    target_device_id,
                    paths,
                }) => {
                    let matches_target = target_device_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .is_none_or(|t| ids_match(&self.peer_device_id, t));
                    if matches_target && let Some(aux) = self.aux {
                        let list: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
                        aux.send_files(list);
                    }
                }
                Ok(UiCommand::CancelTransfer) => {
                    if let Some(aux) = self.aux {
                        aux.cancel_transfer();
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.global_stop.store(true, Ordering::Release);
                    self.session_stop.store(true, Ordering::Release);
                    self.pending = Some(Command::Stop);
                    break;
                }
            }
        }
    }

    fn resolve_quick_jump_target(&self, target: Option<&str>) -> String {
        match target.map(str::trim).filter(|s| !s.is_empty()) {
            None => {
                // Cycle between local and peer
                if self.source.status().remote_focus {
                    self.local_device_id.to_string()
                } else {
                    self.peer_device_id.clone()
                }
            }
            Some("local") => self.local_device_id.to_string(),
            Some(id) if id == self.local_device_id => self.local_device_id.to_string(),
            Some(_) => self.peer_device_id.clone(),
        }
    }

    fn queue_quick_jump(&mut self, target_device_id: String) {
        if !self.sharing {
            return;
        }
        self.pending_quick_jump = Some((target_device_id, Instant::now()));
        if !self.source.status().enabled {
            // Request local control authority from pair_session so QuickJump can proceed.
            self.source.signal_activity();
        }
    }

    fn process_hotkeys_and_quick_jump(&mut self, now_ns: u64) -> Result<()> {
        for action in self.hotkey_engine.drain_actions() {
            match action {
                HotkeyAction::EmergencyStop => {
                    *self.user_paused = true;
                    self.pending_quick_jump = None;
                    self.pending = Some(Command::Pause);
                }
                HotkeyAction::ToggleSharing => {
                    if self.sharing {
                        *self.user_paused = true;
                        self.pending_quick_jump = None;
                        self.pending = Some(Command::Pause);
                    } else {
                        *self.user_paused = false;
                        if self.status_seen {
                            self.pending = Some(Command::Enable { epoch: self.epoch });
                        }
                    }
                }
                HotkeyAction::ToggleLockScreen => {
                    *self.screen_locked = !*self.screen_locked;
                    self.source.set_edge_locked(*self.screen_locked);
                }
                HotkeyAction::QuickJumpNext => {
                    let target = self.resolve_quick_jump_target(None);
                    self.queue_quick_jump(target);
                }
                HotkeyAction::QuickJumpHome => {
                    self.queue_quick_jump(self.local_device_id.to_string());
                }
                HotkeyAction::QuickJumpDevice(idx) => {
                    if idx == 0 {
                        self.queue_quick_jump(self.peer_device_id.clone());
                    }
                }
            }
        }

        if let Some((target, created_at)) = self.pending_quick_jump.clone() {
            if !self.sharing || created_at.elapsed() >= Duration::from_secs(2) {
                self.pending_quick_jump = None;
            } else if !self.source.status().enabled {
                // Keep waiting briefly for local authority grant
            } else if !self.hotkey_engine.any_modifier_held()
                && !self.source.has_held_physical_input()
            {
                self.pending_quick_jump = None;
                if let Some(local_pt) = self.source.quick_jump_to_device(&target, now_ns)? {
                    let _ = self.capture.warp_local(local_pt);
                }
            }
        }
        Ok(())
    }

    fn poll_native_shortcut(&mut self) -> Result<Option<Command>> {
        let Some(shortcut) = self.capture.command() else {
            return Ok(None);
        };
        if shortcut == Shortcut::Stop {
            if let Some(reason) = self.capture.failure() {
                self.session_stop.store(true, Ordering::Release);
                anyhow::bail!(reason);
            }
            *self.user_paused = true;
            return Ok(Some(Command::Pause));
        }
        Ok(Some(match shortcut {
            Shortcut::Toggle if self.sharing => {
                *self.user_paused = true;
                Command::Pause
            }
            Shortcut::Toggle => {
                *self.user_paused = false;
                Command::Enable { epoch: self.epoch }
            }
            Shortcut::Stop => Command::Pause,
        }))
    }
}

impl Host for PeerHost<'_> {
    fn pulse(&mut self, now_ns: u64) -> Result<()> {
        if Instant::now() >= self.deadline {
            self.global_stop.store(true, Ordering::Release);
            self.session_stop.store(true, Ordering::Release);
            return Ok(());
        }
        self.drain_ui_commands();
        let result = self.capture.poll(now_ns);
        if result.is_err() {
            self.session_stop.store(true, Ordering::Release);
        }
        result?;
        self.process_hotkeys_and_quick_jump(now_ns)?;
        if let Some(cmd) = self.poll_native_shortcut()? {
            self.pending = Some(cmd);
        }
        Ok(())
    }

    fn poll_command(&mut self) -> Result<Option<Command>> {
        if let Some(cmd) = self.pending.take() {
            return Ok(Some(cmd));
        }
        self.drain_ui_commands();
        if let Some(cmd) = self.pending.take() {
            return Ok(Some(cmd));
        }
        self.poll_native_shortcut()
    }

    fn publish(&mut self, status: &SessionStatus) -> Result<()> {
        self.status_seen = true;
        self.sharing = status.endpoint.enabled;
        if self.sharing {
            *self.user_paused = false;
        }
        self.epoch = status.endpoint.epoch;
        if self.initiator
            && !self.auto_enable_sent
            && !*self.user_paused
            && status.capture_ready
            && !self.sharing
        {
            self.auto_enable_sent = true;
            self.pending = Some(Command::Enable { epoch: self.epoch });
        }
        // Track focus ownership and immediately push Caps Lock + IME state when
        // cursor leaves this device for the peer (ignore transient None during handoff).
        if let Some(focus_id) = status.focus_device_id.as_deref() {
            let now_local_focus = focus_id == self.local_device_id;
            let prev_local_focus = self.local_has_focus.swap(now_local_focus, Ordering::AcqRel);
            if prev_local_focus
                && !now_local_focus
                && let Some(aux) = self.aux
            {
                aux.push_input_state_now();
            }
        }
        let discovered = self.discovered_list();
        let transfer = self.aux.and_then(|a| a.current_transfer());
        let state_changed = self.last_emitted_enabled != Some(status.endpoint.enabled)
            || self.last_emitted_focus_screen != status.focus_screen_id
            || self.last_emitted_peer_count != discovered.len()
            || self.last_emitted_locked != Some(*self.screen_locked)
            || self.last_emitted_hotkeys.as_ref() != Some(&self.config.hotkeys)
            || self.last_emitted_manual_peers.as_ref() != Some(&self.config.manual_peers)
            || self.last_emitted_peer_channels.as_ref() != Some(&self.config.peer_channels)
            || transfer.as_ref().is_some_and(|t| t.state == "active");
        if state_changed
            || self.last_emit.elapsed() >= Duration::from_millis(250)
            || self.pending.is_some()
        {
            self.last_emit = Instant::now();
            self.last_emitted_enabled = Some(status.endpoint.enabled);
            self.last_emitted_focus_screen = status.focus_screen_id.clone();
            self.last_emitted_peer_count = discovered.len();
            self.last_emitted_locked = Some(*self.screen_locked);
            self.last_emitted_hotkeys = Some(self.config.hotkeys.clone());
            self.last_emitted_manual_peers = Some(self.config.manual_peers.clone());
            self.last_emitted_peer_channels = Some(self.config.peer_channels.clone());
            let peers = self.peers_list(&discovered, status.focus_device_id.as_deref());
            let has_pass = self.config.passphrase.is_some();
            emit_status(
                self.stdout,
                &PeerUiStatus {
                    protocol: PEER_UI_PROTOCOL,
                    state: if status.endpoint.enabled {
                        "sharing".into()
                    } else {
                        "paused".into()
                    },
                    local_device_id: self.local_device_id.into(),
                    local_name: self.local_name.into(),
                    auth_mode: if has_pass { "passphrase" } else { "open" },
                    passphrase_configured: has_pass,
                    smart_keymap: self.config.smart_keymap,
                    clipboard_sync: self.config.clipboard_sync,
                    input_state_sync: self.config.input_state_sync,
                    screen_locked: *self.screen_locked,
                    hotkeys: self.config.hotkeys.clone(),
                    selected_channel: self.config.channel.clone(),
                    direct_peer: self.config.direct_peer.clone(),
                    active_channel_label: self.active_channel_label.clone(),
                    network_channels: self.network_channels.clone(),
                    manual_peers: self.config.manual_peers.clone(),
                    peers,
                    route_enabled: status.endpoint.enabled,
                    focus_device_id: status.focus_device_id.clone(),
                    focus_screen_id: status.focus_screen_id.clone(),
                    peer_device_id: Some(self.peer_device_id.clone()),
                    peer_name: Some(self.peer_name.clone()),
                    peer_address: Some(self.peer_address.clone()),
                    listen_endpoint: self.listen_endpoint.clone(),
                    local_displays: self.local_displays.clone(),
                    discovered_peers: discovered,
                    shared_layout: Some(status.shared_layout.clone()),
                    transfer,
                    error: None,
                },
            );
        }
        Ok(())
    }
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
    let _keep_awake = crate::power::KeepAwakeGuard::acquire();

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
    let mut pending_layout_push: Option<(String, SharedLayout)> = None;

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            let mut local_channels = enumerate_local_channels();
            let mut last_channel_refresh = Instant::now();
            if config.channel != "auto" && !local_channels.iter().any(|c| c.id == config.channel) {
                // Selected IP disappeared (e.g. Ethernet unplugged): fall back gracefully to auto.
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

            let mut reconfigure_requested = false;
            let mut last_error: Option<String> = None;
            let mut auth_backoff_until = Instant::now();
            let mut last_status_emit = Instant::now() - Duration::from_secs(1);
            let mut last_dial_attempt = Instant::now() - Duration::from_secs(1);
            let mut searching_since = Instant::now();

            while !stop.load(Ordering::Acquire)
                && !reconfigure_requested
                && Instant::now() < deadline
            {
                if last_channel_refresh.elapsed() >= Duration::from_secs(3) {
                    last_channel_refresh = Instant::now();
                    local_channels = enumerate_local_channels();
                }

                // Drain UI commands while searching
                loop {
                    match rx.try_recv() {
                        Ok(UiCommand::Stop) | Err(TryRecvError::Disconnected) => {
                            stop.store(true, Ordering::Release);
                            break;
                        }
                        Ok(UiCommand::Pause) => user_paused = true,
                        Ok(UiCommand::Resume) => user_paused = false,
                        Ok(UiCommand::ApplyLayout { layout }) => {
                            if layout.validate().is_ok() {
                                let target = if layout.source_device_id == local_device_id {
                                    layout.receiver_device_id.clone()
                                } else {
                                    layout.source_device_id.clone()
                                };
                                save_layout(&target, &layout);
                                pending_layout_push = Some((target, layout));
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
                            last_dial_attempt = Instant::now() - Duration::from_secs(2);
                        }
                        Ok(UiCommand::AddManualPeer {
                            address,
                            label,
                            channel,
                        }) => {
                            if add_manual_peer(&mut config, &address, label, &channel) {
                                save_config(&config);
                                last_dial_attempt = Instant::now() - Duration::from_secs(2);
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
                        }
                        Ok(
                            UiCommand::QuickJump { .. }
                            | UiCommand::SendFiles { .. }
                            | UiCommand::CancelTransfer,
                        ) => {}
                        Err(TryRecvError::Empty) => break,
                    }
                }
                if stop.load(Ordering::Acquire) || reconfigure_requested {
                    break;
                }

                let candidates: Vec<Candidate> = discovery
                    .try_snapshot()
                    .map(|s| s.candidates.into_iter().filter(|c| c.compatible).collect())
                    .unwrap_or_default();

                if last_status_emit.elapsed() >= Duration::from_millis(250) {
                    last_status_emit = Instant::now();
                    let local_displays = (platform.inventory)().unwrap_or_default();
                    let has_pass = config.passphrase.is_some();
                    emit_status(
                        &stdout,
                        &PeerUiStatus {
                            protocol: PEER_UI_PROTOCOL,
                            state: "searching".into(),
                            local_device_id: local_device_id.clone(),
                            local_name: local_name.clone(),
                            auth_mode: if has_pass { "passphrase" } else { "open" },
                            passphrase_configured: has_pass,
                            smart_keymap: config.smart_keymap,
                            clipboard_sync: config.clipboard_sync,
                            input_state_sync: config.input_state_sync,
                            screen_locked,
                            hotkeys: config.hotkeys.clone(),
                            selected_channel: config.channel.clone(),
                            direct_peer: config.direct_peer.clone(),
                            active_channel_label: None,
                            network_channels: channel_infos(&local_channels),
                            manual_peers: config.manual_peers.clone(),
                            peers: candidates
                                .iter()
                                .map(|c| PeerInfo {
                                    id: c.device_id.clone(),
                                    name: c.friendly_name.clone(),
                                    platform: c.platform.clone(),
                                    connected: false,
                                    focused: false,
                                    slot: None,
                                    address: None,
                                    active_channel_label: None,
                                    preferred_channel: peer_channel_override(
                                        &config.peer_channels,
                                        &c.device_id,
                                    ),
                                })
                                .collect(),
                            route_enabled: false,
                            focus_device_id: None,
                            focus_screen_id: None,
                            peer_device_id: None,
                            peer_name: None,
                            peer_address: None,
                            listen_endpoint: listen_endpoint.clone(),
                            local_displays,
                            discovered_peers: candidates
                                .iter()
                                .map(|c| DiscoveredPeerInfo {
                                    id: c.device_id.clone(),
                                    name: c.friendly_name.clone(),
                                    platform: c.platform.clone(),
                                    connected: false,
                                })
                                .collect(),
                            shared_layout: None,
                            transfer: None,
                            error: last_error.clone(),
                        },
                    );
                }

                // 1. Accept inbound connection (unless pushing layout)
                let mut connected: Option<(TcpStream, bool, Option<String>, Option<String>)> = None;
                if pending_layout_push.is_none() {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            connected = Some((stream, false, None, None));
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e.into()),
                    }
                }

                // 2. Auto-dial discovered peer OR configured direct peer (e.g. ZeroTier IP)
                let fallback_dial = searching_since.elapsed() >= Duration::from_millis(1500);
                let in_auth_backoff = Instant::now() < auth_backoff_until;
                let can_dial = !in_auth_backoff
                    && (pending_layout_push.is_some()
                        || last_dial_attempt.elapsed()
                            >= Duration::from_millis(if fallback_dial { 1200 } else { 350 }));
                if connected.is_none() && can_dial {
                    last_dial_attempt = Instant::now();
                    for candidate in &candidates {
                        let has_push = pending_layout_push
                            .as_ref()
                            .is_some_and(|(id, _)| id.starts_with(&candidate.device_id));
                        let should_dial =
                            has_push || local_discovery_id < candidate.device_id || fallback_dial;
                        if !should_dial {
                            continue;
                        }
                        let eff_channel =
                            effective_channel_for_peer(&config, &candidate.device_id);
                        for addr in
                            ranked_addresses(&candidate.addresses, eff_channel, &local_channels)
                        {
                            if let Ok(stream) =
                                TcpStream::connect_timeout(&addr, Duration::from_millis(250))
                            {
                                connected = Some((
                                    stream,
                                    true,
                                    Some(candidate.friendly_name.clone()),
                                    Some(candidate.device_id.clone()),
                                ));
                                break;
                            }
                        }
                        if connected.is_some() {
                            break;
                        }
                    }

                    // Manual peer IP list fallback (ZeroTier / cross-subnet static peers)
                    if connected.is_none() {
                        for mp in &config.manual_peers {
                            if let Some(direct_addr) = parse_direct_peer(Some(&mp.address))
                                && let Ok(stream) = TcpStream::connect_timeout(
                                    &direct_addr,
                                    Duration::from_millis(280),
                                )
                            {
                                connected = Some((stream, true, mp.label.clone(), None));
                                break;
                            }
                        }
                    }
                }

                // Fallback accept if layout push hasn't dialed yet
                if connected.is_none() && pending_layout_push.is_some() {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            connected = Some((stream, false, None, None));
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e.into()),
                    }
                }

                let Some((stream, initiator, known_name, known_discovery_id)) = connected else {
                    thread::sleep(Duration::from_millis(25));
                    continue;
                };

                let peer_sock = stream.peer_addr().ok();
                let peer_address = peer_sock
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "unknown".into());
                let active_channel_label =
                    peer_sock.and_then(|a| classify_active_channel(a, &local_channels));
                let local_hello = hello(&local_device_id, &platform)?;
                let preferred_layout = if initiator {
                    if let Some((_, layout)) = pending_layout_push.take() {
                        Some(layout)
                    } else {
                        known_discovery_id
                            .as_deref()
                            .and_then(find_saved_layout_by_prefix)
                    }
                } else {
                    None
                };

                let session_stop = Arc::new(AtomicBool::new(false));
                let established = PairConnection::establish_peer(
                    stream,
                    &local_device_id,
                    config.passphrase.as_deref(),
                    initiator,
                    local_hello.clone(),
                    preferred_layout,
                    &stop,
                    log.clone(),
                );
                let mut connection = match established {
                    Ok(conn) => {
                        last_error = None;
                        conn
                    }
                    Err(error) => {
                        let msg = format!("{error:#}");
                        log.write("peer_setup_failed", serde_json::json!({"reason": msg}));
                        let is_auth_mismatch = msg.contains("passphrase")
                            || msg.contains("Authenticate")
                            || msg.contains("Receive LAN Noise")
                            || msg.contains("Read handshake");
                        if is_auth_mismatch {
                            last_error = Some("等待密码匹配：请确认两台设备的密码配置一致。".into());
                            auth_backoff_until = Instant::now() + Duration::from_millis(2500);
                        } else {
                            last_error = Some(msg);
                            auth_backoff_until = Instant::now() + Duration::from_millis(1000);
                        }
                        searching_since = Instant::now();
                        thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                };

                let remote_id = connection.remote_device_id().to_string();
                save_layout(&remote_id, connection.shared_layout());

                let peer_name = known_name
                    .or_else(|| {
                        candidates
                            .iter()
                            .find(|c| remote_id.starts_with(&c.device_id))
                            .map(|c| c.friendly_name.clone())
                    })
                    .unwrap_or_else(|| "局域网设备".into());

                let known_platform = candidates
                    .iter()
                    .find(|c| remote_id.starts_with(&c.device_id))
                    .map(|c| c.platform.as_str());
                let remote_os = infer_peer_os(known_platform, connection.shared_layout(), &remote_id);
                let different_os = remote_os != std::env::consts::OS;

                connection.set_max_hz(options.max_hz)?;
                // Preserve native Ctrl and Cmd/Win keys so macOS Ctrl shortcuts stay intact.
                let _ = (remote_os, different_os);
                connection.set_key_translator(crate::keymap::KeyTranslator::passthrough());
                let source = SharedSource::new(connection.runtime().clone())?;
                source.set_edge_locked(screen_locked);
                hotkey_engine.reset_modifiers();
                let _ = hotkey_engine.drain_actions();
                let mut sink = match prepare_worker(&local_hello.inventory, log.clone()) {
                    Ok(s) => s,
                    Err(e) => {
                        last_error = Some(format!("输入服务启动失败: {e:#}"));
                        thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                };
                let mut capture = match (platform.capture)(source.clone(), log.clone()) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = sink.shutdown();
                        last_error = Some(format!("输入采集启动失败: {e:#}"));
                        thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                };

                let local_has_focus = Arc::new(AtomicBool::new(true));
                let aux_session = peer_sock.map(|sock| {
                    crate::aux_channel::AuxSession::spawn(
                        aux_listener.clone(),
                        sock,
                        initiator,
                        local_device_id.clone(),
                        remote_id.clone(),
                        config.passphrase.clone(),
                        clipboard_sync_flag.clone(),
                        input_state_sync_flag.clone(),
                        local_has_focus.clone(),
                    )
                });

                let run_result = connection.run(
                    &source,
                    &mut sink,
                    &mut PeerHost {
                        capture: capture.as_mut(),
                        source: &source,
                        hotkey_engine: &hotkey_engine,
                        rx: &rx,
                        stdout: &stdout,
                        discovery: &discovery,
                        session_stop: &session_stop,
                        global_stop: &stop,
                        deadline,
                        local_device_id: &local_device_id,
                        local_name: &local_name,
                        local_displays: local_hello.inventory.clone(),
                        config: &mut config,
                        smart_keymap_flag: &smart_keymap_flag,
                        clipboard_sync_flag: &clipboard_sync_flag,
                        input_state_sync_flag: &input_state_sync_flag,
                        local_has_focus: &local_has_focus,
                        aux: aux_session.as_ref(),
                        reconfigure_requested: &mut reconfigure_requested,
                        network_channels: channel_infos(&local_channels),
                        active_channel_label,
                        listen_endpoint: listen_endpoint.clone(),
                        peer_device_id: remote_id.clone(),
                        peer_name: peer_name.clone(),
                        peer_platform: remote_os.to_string(),
                        peer_address: peer_address.clone(),
                        user_paused: &mut user_paused,
                        screen_locked: &mut screen_locked,
                        pending_layout_push: &mut pending_layout_push,
                        pending_quick_jump: None,
                        initiator,
                        status_seen: false,
                        auto_enable_sent: false,
                        sharing: false,
                        epoch: 0,
                        pending: None,
                        last_emit: Instant::now() - Duration::from_secs(1),
                        last_emitted_enabled: None,
                        last_emitted_focus_screen: None,
                        last_emitted_peer_count: usize::MAX,
                        last_emitted_locked: None,
                        last_emitted_hotkeys: None,
                        last_emitted_manual_peers: None,
                        last_emitted_peer_channels: None,
                    },
                    &session_stop,
                    log.clone(),
                );

                let capture_cleanup = capture.stop();
                let sink_cleanup = sink.shutdown();
                if capture_cleanup.is_err() || sink_cleanup.is_err() {
                    // After OS suspend / lid-close / display power transitions, the
                    // child worker may have already exited. Log and recover automatically
                    // rather than permanently terminating the peer service.
                    log.write(
                        "peer_cleanup_recovered",
                        serde_json::json!({
                            "capture_confirmed": capture_cleanup.is_ok(),
                            "worker_confirmed": sink_cleanup.is_ok()
                        }),
                    );
                    if capture_cleanup.is_err() {
                        anyhow::bail!("capture cleanup unconfirmed; restart client");
                    }
                    searching_since = Instant::now();
                    thread::sleep(Duration::from_millis(350));
                    continue;
                }
                if let Err(error) = run_result {
                    let reason = format!("{error:#}");
                    log.write(
                        "peer_session_ended",
                        serde_json::json!({"reason": reason, "cleanup_confirmed": true}),
                    );
                    if pending_layout_push.is_none()
                        && !reconfigure_requested
                        && !stop.load(Ordering::Acquire)
                    {
                        last_error = None;
                    }
                }
                searching_since = Instant::now();
                if reconfigure_requested {
                    break;
                }
                let pause_ms = if pending_layout_push.is_some() { 40 } else { 200 };
                thread::sleep(Duration::from_millis(pause_ms));
            }

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
            transfer: None,
            error: result.as_ref().err().map(|e| format!("{e:#}")),
        },
    );
    writer.finish()?;
    result
}

fn infer_peer_os(known_platform: Option<&str>, layout: &SharedLayout, remote_id: &str) -> &'static str {
    match known_platform {
        Some("windows") => return "windows",
        Some("macos") => return "macos",
        _ => {}
    }
    let has_win_display = layout.screens.iter().any(|s| {
        s.device_id == remote_id
            && (s.native_id.starts_with(r"\\.\") || s.native_id.contains("DISPLAY"))
    });
    if has_win_display { "windows" } else { "macos" }
}

fn find_saved_layout_by_prefix(prefix: &str) -> Option<SharedLayout> {
    let dir = app_data_root().ok()?.join("peer-layouts");
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".json")
            && stem.starts_with(prefix)
            && stem.len() == 64
        {
            return load_saved_layout(stem);
        }
    }
    None
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
