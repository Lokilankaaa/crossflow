//! Symmetric peer-to-peer LAN mesh runner.
//!
//! Both Windows and macOS execute this identical peer node:
//! - Binds a local TCP listener and advertises via mDNS (`_crossflow._tcp.local.`).
//! - Browses the LAN for other compatible Crossflow peers.
//! - Uses deterministic ID tie-breaking (smaller discovery ID dials; larger waits)
//!   so two peers never collide, while any peer editing the arrangement can
//!   proactively push its updated SharedLayout over a fast reconnect.
//! - Encrypts every session with Noise_NNpsk0 (open LAN auto-connect by default,
//!   or isolated with an optional user-supplied shared passphrase).
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
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
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

pub struct Options {
    pub bind: SocketAddr,
    pub device_name: Option<String>,
    pub passphrase: Option<String>,
    pub seconds: f64,
    pub max_hz: u32,
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
    ApplyLayout { layout: SharedLayout },
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

fn is_direct_lan_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let octets = ip.octets();
    // Prioritize point-to-point direct ethernet (192.168.50.x or link-local 169.254.x.x)
    (octets[0] == 192 && octets[1] == 168 && octets[2] == 50)
        || (octets[0] == 169 && octets[1] == 254)
}

fn ranked_addresses(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut ranked: Vec<SocketAddr> = addresses
        .iter()
        .copied()
        .filter(|addr| match addr.ip() {
            IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified() && !v4.is_multicast(),
            IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified() && !v6.is_multicast(),
        })
        .collect();
    ranked.sort_by_key(|addr| match addr.ip() {
        IpAddr::V4(v4) if is_direct_lan_ipv4(v4) => (0u8, addr.to_string()),
        IpAddr::V4(v4) if v4.is_private() => (1u8, addr.to_string()),
        IpAddr::V4(_) => (2u8, addr.to_string()),
        IpAddr::V6(_) => (3u8, addr.to_string()),
    });
    ranked
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
                        // Parent GUI closed its pipe -> stop cleanly.
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
    auth_mode: &'static str,
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
                        // Peacefully end this session so we immediately reconnect as initiator
                        // and push the updated layout to the peer in ~50ms.
                        self.session_stop.store(true, Ordering::Release);
                        self.pending = Some(Command::Stop);
                        break;
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

    fn poll_native_shortcut(&mut self) -> Result<Option<Command>> {
        let Some(shortcut) = self.capture.command() else {
            return Ok(None);
        };
        if shortcut == Shortcut::Stop {
            if let Some(reason) = self.capture.failure() {
                // Capture fault ends only this session; outer loop stays alive and recovers.
                self.session_stop.store(true, Ordering::Release);
                anyhow::bail!(reason);
            }
            // User hit Escape / Ctrl+Alt+Esc emergency shortcut: pause sharing immediately
            // without killing the peer service so user can resume cleanly when ready.
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
        // Initiator auto-enables sharing on initial ready unless user explicitly paused
        // (avoids simultaneous enable races while bringing both peers straight into sharing).
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
        if state_changed || self.last_emit.elapsed() >= Duration::from_millis(250) || self.pending.is_some() {
            self.last_emit = Instant::now();
            self.last_emitted_enabled = Some(status.endpoint.enabled);
            self.last_emitted_focus_screen = status.focus_screen_id.clone();
            self.last_emitted_peer_count = discovered.len();
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
                    auth_mode: self.auth_mode,
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
    let passphrase = options
        .passphrase
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let auth_mode = if passphrase.is_some() {
        "passphrase"
    } else {
        "open"
    };

    #[cfg(target_os = "macos")]
    {
        if let Err(permission) = crate::platform::macos::preflight_input_permissions() {
            emit_status(
                &stdout,
                &PeerUiStatus {
                    protocol: PEER_UI_PROTOCOL,
                    state: "startup_rejected".into(),
                    local_device_id,
                    local_name,
                    auth_mode,
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

    let listener = TcpListener::bind(options.bind).or_else(|_| {
        TcpListener::bind(SocketAddr::new(options.bind.ip(), 0))
    })?;
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

    let rx = spawn_stdin_reader(stop.clone());
    let log = writer.log.clone();
    let deadline = Instant::now() + Duration::from_secs_f64(options.seconds);
    let mut user_paused = false;
    let mut pending_layout_push: Option<(String, SharedLayout)> = None;
    let mut last_error: Option<String> = None;
    let mut last_status_emit = Instant::now() - Duration::from_secs(1);
    let mut last_dial_attempt = Instant::now() - Duration::from_secs(1);
    let mut searching_since = Instant::now();

    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            // Drain UI commands while idle/searching
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
                    Err(TryRecvError::Empty) => break,
                }
            }
            if stop.load(Ordering::Acquire) {
                break;
            }

            let candidates: Vec<Candidate> = discovery
                .try_snapshot()
                .map(|s| s.candidates.into_iter().filter(|c| c.compatible).collect())
                .unwrap_or_default();

            if last_status_emit.elapsed() >= Duration::from_millis(250) {
                last_status_emit = Instant::now();
                let local_displays = (platform.inventory)().unwrap_or_default();
                emit_status(
                    &stdout,
                    &PeerUiStatus {
                        protocol: PEER_UI_PROTOCOL,
                        state: "searching".into(),
                        local_device_id: local_device_id.clone(),
                        local_name: local_name.clone(),
                        auth_mode,
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

            // 1. Check inbound connection first (unless we are actively pushing a layout edit)
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

            // 2. Otherwise auto-dial discovered LAN peer if we win tie-break, have a layout push,
            //    or have waited >1.5s without an inbound connection (handles asymmetric firewalls).
            let fallback_dial = searching_since.elapsed() >= Duration::from_millis(1500);
            let can_dial = pending_layout_push.is_some()
                || last_dial_attempt.elapsed() >= Duration::from_millis(if fallback_dial { 900 } else { 350 });
            if connected.is_none() && can_dial {
                last_dial_attempt = Instant::now();
                for candidate in &candidates {
                    let has_push = pending_layout_push
                        .as_ref()
                        .is_some_and(|(id, _)| id.starts_with(&candidate.device_id));
                    let should_dial = has_push || local_discovery_id < candidate.device_id || fallback_dial;
                    if !should_dial {
                        continue;
                    }
                    for addr in ranked_addresses(&candidate.addresses) {
                        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(250)) {
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
            }

            // Fallback: if layout push didn't dial yet, still accept inbound connections so we never deadlock
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

            let peer_address = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "unknown".into());
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
                passphrase.as_deref(),
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
                    last_error = Some(if msg.contains("passphrase") || msg.contains("Authenticate") {
                        "连接验证失败：请确认两台设备的共享密码一致。".into()
                    } else {
                        msg
                    });
                    thread::sleep(Duration::from_millis(250));
                    continue;
                }
            };

            // Save accepted layout on BOTH sides so either side remembers the latest arrangement!
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

            connection.set_max_hz(options.max_hz)?;
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
                    auth_mode,
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
                log.write(
                    "peer_cleanup_failed",
                    serde_json::json!({
                        "capture_confirmed": capture_cleanup.is_ok(),
                        "worker_confirmed": sink_cleanup.is_ok()
                    }),
                );
                anyhow::bail!("cleanup unconfirmed; restart client");
            }
            if let Err(error) = run_result {
                let reason = format!("{error:#}");
                log.write(
                    "peer_session_ended",
                    serde_json::json!({"reason": reason, "cleanup_confirmed": true}),
                );
                if pending_layout_push.is_none() && !stop.load(Ordering::Acquire) {
                    last_error = None; // Normal disconnect/reconnect; auto-recover silently.
                }
            }
            // Brief backoff before re-discovering/reconnecting (faster for layout push).
            searching_since = Instant::now();
            let pause_ms = if pending_layout_push.is_some() { 40 } else { 200 };
            thread::sleep(Duration::from_millis(pause_ms));
        }
        Ok(())
    })();

    stop.store(true, Ordering::Release);
    discovery.stop();
    let _ = discovery.wait_stopped(Duration::from_secs(2));
    emit_status(
        &stdout,
        &PeerUiStatus {
            protocol: PEER_UI_PROTOCOL,
            state: if result.is_ok() { "stopped".into() } else { "error".into() },
            local_device_id,
            local_name,
            auth_mode,
            route_enabled: false,
            focus_device_id: None,
            focus_screen_id: None,
            peer_device_id: None,
            peer_name: None,
            peer_address: None,
            listen_endpoint,
            local_displays: vec![],
            discovered_peers: vec![],
            shared_layout: None,
            error: result.as_ref().err().map(|e| format!("{e:#}")),
        },
    );
    writer.finish()?;
    result
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
    use std::net::{Ipv4Addr, SocketAddrV4};

    #[test]
    fn direct_ethernet_route_ranks_ahead_of_wifi_and_ipv6() {
        let wifi = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 188), 24800));
        let wired = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 168, 50, 2), 24800));
        let loopback = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 24800));
        let ranked = ranked_addresses(&[loopback, wifi, wired]);
        assert_eq!(ranked, vec![wired, wifi]);
    }
}
