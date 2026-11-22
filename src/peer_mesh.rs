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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PeerConfig {
    #[serde(default = "default_channel")]
    channel: String,
    #[serde(default)]
    direct_peer: Option<String>,
    #[serde(default)]
    passphrase: Option<String>,
    #[serde(default = "default_true")]
    smart_keymap: bool,
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
            passphrase: None,
            smart_keymap: true,
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

#[derive(Serialize)]
struct PeerUiStatus {
    protocol: u8,
    state: String,
    local_device_id: String,
    local_name: String,
    auth_mode: &'static str,
    passphrase_configured: bool,
    smart_keymap: bool,
    selected_channel: String,
    direct_peer: Option<String>,
    active_channel_label: Option<String>,
    network_channels: Vec<NetworkChannelInfo>,
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
    SetPassphrase {
        passphrase: Option<String>,
    },
    SetKeymap {
        enabled: bool,
    },
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

fn load_config(env_passphrase: Option<String>) -> PeerConfig {
    let mut cfg = config_path()
        .ok()
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<PeerConfig>(&b).ok())
        .unwrap_or_default();
    if cfg.channel.trim().is_empty() {
        cfg.channel = default_channel();
    }
    cfg.direct_peer = cfg
        .direct_peer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    cfg.passphrase = cfg
        .passphrase
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
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

struct PeerHost<'a> {
    capture: &'a mut dyn Capture,
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
    reconfigure_requested: &'a mut bool,
    network_channels: Vec<NetworkChannelInfo>,
    active_channel_label: Option<String>,
    listen_endpoint: Option<String>,
    peer_device_id: String,
    peer_name: String,
    peer_address: String,
    user_paused: &'a mut bool,
    pending_layout_push: &'a mut Option<(String, SharedLayout)>,
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
                connected: self.peer_device_id.starts_with(&c.device_id),
                id: c.device_id,
                name: c.friendly_name,
                platform: c.platform,
            })
            .collect()
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
                    save_config(self.config);
                    *self.reconfigure_requested = true;
                    self.session_stop.store(true, Ordering::Release);
                    self.pending = Some(Command::Stop);
                    break;
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
        let discovered = self.discovered_list();
        let state_changed = self.last_emitted_enabled != Some(status.endpoint.enabled)
            || self.last_emitted_focus_screen != status.focus_screen_id
            || self.last_emitted_peer_count != discovered.len();
        if state_changed
            || self.last_emit.elapsed() >= Duration::from_millis(250)
            || self.pending.is_some()
        {
            self.last_emit = Instant::now();
            self.last_emitted_enabled = Some(status.endpoint.enabled);
            self.last_emitted_focus_screen = status.focus_screen_id.clone();
            self.last_emitted_peer_count = discovered.len();
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
                    selected_channel: self.config.channel.clone(),
                    direct_peer: self.config.direct_peer.clone(),
                    active_channel_label: self.active_channel_label.clone(),
                    network_channels: self.network_channels.clone(),
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
    let smart_keymap_flag = Arc::new(AtomicBool::new(config.smart_keymap));
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
                    selected_channel: config.channel.clone(),
                    direct_peer: config.direct_peer.clone(),
                    active_channel_label: None,
                    network_channels: channel_infos(&local_channels),
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
                            save_config(&config);
                            reconfigure_requested = true;
                            break;
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
                            selected_channel: config.channel.clone(),
                            direct_peer: config.direct_peer.clone(),
                            active_channel_label: None,
                            network_channels: channel_infos(&local_channels),
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
                        for addr in
                            ranked_addresses(&candidate.addresses, &config.channel, &local_channels)
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

                    // Direct peer IP fallback (useful for ZeroTier / cross-subnet links)
                    if connected.is_none()
                        && let Some(direct_addr) = parse_direct_peer(config.direct_peer.as_deref())
                        && let Ok(stream) =
                            TcpStream::connect_timeout(&direct_addr, Duration::from_millis(300))
                    {
                        connected = Some((stream, true, None, None));
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
                connection.set_key_translator(crate::keymap::KeyTranslator::new(
                    smart_keymap_flag.clone(),
                    different_os,
                ));
                let source = SharedSource::new(connection.runtime().clone())?;
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

                let run_result = connection.run(
                    &source,
                    &mut sink,
                    &mut PeerHost {
                        capture: capture.as_mut(),
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
                        reconfigure_requested: &mut reconfigure_requested,
                        network_channels: channel_infos(&local_channels),
                        active_channel_label,
                        listen_endpoint: listen_endpoint.clone(),
                        peer_device_id: remote_id.clone(),
                        peer_name: peer_name.clone(),
                        peer_address: peer_address.clone(),
                        user_paused: &mut user_paused,
                        pending_layout_push: &mut pending_layout_push,
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
            selected_channel: config.channel,
            direct_peer: config.direct_peer,
            active_channel_label: None,
            network_channels: vec![],
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
}
