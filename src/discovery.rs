//! Unauthenticated LAN discovery, separate from pairing and input sessions.
//!
//! Only `start` opens mDNS sockets. Start/stop requests and `try_snapshot` never
//! wait for network I/O. Call `wait_stopped` from a background thread when teardown
//! must be confirmed. Drop requests stop without blocking; it cannot guarantee
//! cleanup after process termination. Addresses are DNS-SD advertisements, not
//! proof of reachability or ownership. Never use `device_id` as a trusted key.
//! The bounded application cache does not bound mdns-sd's internal DNS cache.

use anyhow::{Context, Result, bail, ensure};
use mdns_sd::{
    DaemonEvent, DaemonStatus, IfKind, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent,
    ServiceInfo,
};
use serde::Serialize;
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, SocketAddr, SocketAddrV6},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const SERVICE_TYPE: &str = "_crossflow._tcp.local.";
/// Enrollment endpoints never enter the input-service browser or LanRoutes.
pub const PAIRING_SERVICE_TYPE: &str = "_crossflow-pair._tcp.local.";
pub const PAIRING_PROTOCOL_VERSION: u16 = 1;
pub const MAX_CANDIDATES: usize = 64;
pub const MAX_ADDRESSES: usize = 32;
const MAX_RECORDS: usize = 256;
pub const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(120);
const VERIFY_INTERVAL: Duration = Duration::from_secs(30);
const REFRESH_INTERVAL: Duration = Duration::from_secs(45);

/// Explicit public metadata only. Do not use an OS hostname, account name, path,
/// pairing secret, or identity public/private key as a device ID or default name.
/// `port` must describe an already-bound TCP listener owned by the caller.
#[derive(Clone, Debug)]
pub struct Advertisement {
    pub device_id: String,
    pub friendly_name: String,
    pub platform: String,
    pub protocol: u16,
    pub port: u16,
    /// Actual listener IP, not a guessed primary interface. Unspecified addresses
    /// advertise only their own family. Concrete IPv6 link-local IPs require a
    /// scope unavailable in this field and are intentionally rejected.
    pub listen_ip: IpAddr,
}

/// Fresh, public, per-run discovery identity. It has no relationship to keys,
/// pairing IDs, account names, hostnames or persistent device identity.
pub fn new_public_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    DefaultResolver
        .resolve_rng()
        .context("Discovery random source unavailable")?
        .try_fill_bytes(&mut bytes)
        .context("Cannot generate public discovery ID")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn default_device_name() -> &'static str {
    match std::env::consts::OS {
        "windows" => "Crossflow Windows PC",
        "macos" => "Crossflow Mac",
        "linux" => "Crossflow Linux PC",
        _ => "Crossflow device",
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    pub advertisement: Option<Advertisement>,
    pub local_device_id: Option<String>,
    pub compatible_protocol: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub device_id: String,
    pub friendly_name: String,
    pub platform: String,
    pub protocol: u16,
    pub compatible: bool,
    pub trusted: bool,
    /// Numeric IPv6 scope IDs survive serialization, e.g. `[fe80::1%7]:24800`.
    pub addresses: Vec<SocketAddr>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryState {
    Starting,
    Browsing,
    Stopping,
    Stopped,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiscoverySnapshot {
    pub state: DiscoveryState,
    pub candidates: Vec<Candidate>,
    /// Latched for this discovery run if any candidate/record was omitted at capacity.
    pub capacity_limited: bool,
    /// Fixed local error category; never echoes untrusted TXT contents.
    pub error: Option<String>,
}

pub struct DiscoveryHandle {
    stop: Arc<AtomicBool>,
    snapshot: Arc<Mutex<DiscoverySnapshot>>,
    worker: Option<JoinHandle<()>>,
}

impl DiscoveryHandle {
    pub fn start(config: DiscoveryConfig) -> Result<Self> {
        Self::start_service(config, SERVICE_TYPE)
    }

    /// Browse/publish only short-code enrollment endpoints. Addresses and names
    /// remain untrusted; this does not start pairing or grant input permission.
    pub fn start_pairing(config: DiscoveryConfig) -> Result<Self> {
        Self::start_service(config, PAIRING_SERVICE_TYPE)
    }

    fn start_service(config: DiscoveryConfig, service_type: &'static str) -> Result<Self> {
        ensure!(config.compatible_protocol != 0, "protocol must be nonzero");
        if let Some(id) = &config.local_device_id {
            validate_id(id)?;
        }
        if let Some(ad) = &config.advertisement {
            ad.validate()?;
            if let Some(id) = &config.local_device_id {
                ensure!(id == &ad.device_id, "local discovery IDs must agree");
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(DiscoverySnapshot {
            state: DiscoveryState::Starting,
            candidates: Vec::new(),
            capacity_limited: false,
            error: None,
        }));
        let worker_stop = stop.clone();
        let worker_snapshot = snapshot.clone();
        let worker = thread::Builder::new()
            .name("crossflow-discovery".into())
            .spawn(move || {
                run(config, service_type, &worker_stop, &worker_snapshot);
            })?;
        Ok(Self {
            stop,
            snapshot,
            worker: Some(worker),
        })
    }

    /// Latest whole snapshot; `None` means briefly busy, not an empty device list.
    pub fn try_snapshot(&self) -> Option<DiscoverySnapshot> {
        self.snapshot.try_lock().ok().map(|v| v.clone())
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Bounded confirmation. Do not call this waiting method on a UI thread.
    pub fn wait_stopped(&mut self, timeout: Duration) -> Result<()> {
        self.stop();
        let start = Instant::now();
        while self.worker.as_ref().is_some_and(|h| !h.is_finished()) {
            ensure!(start.elapsed() < timeout, "discovery shutdown timed out");
            thread::sleep(Duration::from_millis(2));
        }
        if let Some(worker) = self.worker.take() {
            ensure!(worker.join().is_ok(), "discovery worker failed");
        }
        let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        ensure!(
            snapshot.state == DiscoveryState::Stopped,
            "discovery did not stop cleanly"
        );
        Ok(())
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 32
            && id
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "invalid discovery device ID"
    );
    Ok(())
}

impl Advertisement {
    fn validate(&self) -> Result<()> {
        self.validate_metadata()?;
        ensure!(
            self.listen_ip.is_unspecified() || (endpoint(self.listen_ip, 0, self.port).is_some()),
            "Discovery requires a LAN listener; loopback, invalid and scoped link-local binds cannot be advertised"
        );
        Ok(())
    }

    fn validate_metadata(&self) -> Result<()> {
        validate_id(&self.device_id)?;
        ensure!(
            !self.friendly_name.trim().is_empty()
                && self.friendly_name.len() <= 96
                && !self.friendly_name.chars().any(|c| c.is_control()
                    || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
            "invalid discovery friendly name"
        );
        ensure!(
            matches!(
                self.platform.as_str(),
                "windows" | "macos" | "linux" | "ios" | "ipados"
            ),
            "invalid discovery platform"
        );
        ensure!(
            self.protocol != 0 && self.port != 0,
            "invalid discovery protocol or port"
        );
        Ok(())
    }

    #[cfg(test)]
    fn service(&self) -> Result<ServiceInfo> {
        self.service_for(SERVICE_TYPE)
    }

    fn service_for(&self, service_type: &'static str) -> Result<ServiceInfo> {
        self.validate()?;
        let protocol = self.protocol.to_string();
        let properties = [
            ("schema", "1"),
            ("id", self.device_id.as_str()),
            ("name", self.friendly_name.as_str()),
            ("platform", self.platform.as_str()),
            ("protocol", protocol.as_str()),
        ];
        // Opaque ID avoids broadcasting machine/account hostnames.
        let address = if self.listen_ip.is_unspecified() {
            String::new()
        } else {
            self.listen_ip.to_string()
        };
        let mut service = ServiceInfo::new(
            service_type,
            &format!("cf-{}", self.device_id),
            &format!("cf-{}.local.", self.device_id),
            address.as_str(),
            self.port,
            &properties[..],
        )?;
        if self.listen_ip.is_unspecified() {
            service = service.enable_addr_auto();
        }
        // Do not auto-populate other addresses on a multi-address interface.
        service.set_interfaces(vec![self.interface_selector()]);
        Ok(service)
    }

    fn interface_selector(&self) -> IfKind {
        if self.listen_ip.is_unspecified() {
            if self.listen_ip.is_ipv4() {
                IfKind::IPv4
            } else {
                IfKind::IPv6
            }
        } else {
            IfKind::Addr(self.listen_ip)
        }
    }
}

fn endpoint(ip: IpAddr, scope: u32, port: u16) -> Option<SocketAddr> {
    if port == 0 || ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return None;
    }
    match ip {
        IpAddr::V4(ip) if ip.is_broadcast() || ip.octets()[0] == 0 => None,
        IpAddr::V4(ip) => Some(SocketAddr::new(ip.into(), port)),
        IpAddr::V6(ip) => {
            // Reject mapped IPv4 to avoid bypassing the IPv4 address checks.
            if ip.to_ipv4_mapped().is_some() {
                return None;
            }
            let link_local = (ip.segments()[0] & 0xffc0) == 0xfe80;
            if link_local && scope == 0 {
                return None;
            }
            Some(SocketAddrV6::new(ip, port, 0, if link_local { scope } else { 0 }).into())
        }
    }
}

#[cfg(test)]
fn parse(service: &ResolvedService, compatible_protocol: u16) -> Result<Candidate> {
    parse_for(service, compatible_protocol, SERVICE_TYPE)
}

fn parse_for(
    service: &ResolvedService,
    compatible_protocol: u16,
    service_type: &str,
) -> Result<Candidate> {
    ensure!(
        service.ty_domain.eq_ignore_ascii_case(service_type)
            && service.fullname.len() <= 255
            && service
                .fullname
                .to_ascii_lowercase()
                .ends_with(service_type),
        "invalid discovery service"
    );
    let value = |key| -> Result<&str> {
        let raw = service
            .get_property_val(key)
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("missing discovery property"))?;
        std::str::from_utf8(raw).map_err(|_| anyhow::anyhow!("invalid discovery text"))
    };
    ensure!(value("schema")? == "1", "unsupported discovery schema");
    let ad = Advertisement {
        device_id: value("id")?.into(),
        friendly_name: value("name")?.into(),
        platform: value("platform")?.into(),
        protocol: value("protocol")?
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid discovery protocol"))?,
        port: service.port,
        // Incoming metadata has no authority over our local listener policy.
        listen_ip: IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
    };
    ad.validate_metadata()?;
    let mut addresses = BTreeSet::new();
    for ip in &service.addresses {
        let scope = match ip {
            ScopedIp::V6(v6) => v6.scope_id().index,
            _ => 0,
        };
        if let Some(addr) = endpoint(ip.to_ip_addr(), scope, service.port) {
            addresses.insert(addr);
        }
    }
    ensure!(!addresses.is_empty(), "discovery has no usable address");
    Ok(Candidate {
        device_id: ad.device_id,
        friendly_name: ad.friendly_name,
        platform: ad.platform,
        protocol: ad.protocol,
        compatible: ad.protocol == compatible_protocol,
        trusted: false,
        addresses: addresses.into_iter().take(MAX_ADDRESSES).collect(),
    })
}

struct Record {
    candidate: Candidate,
    seen: Instant,
}
struct Cache {
    records: BTreeMap<String, Record>,
    capacity_limited: bool,
    service_type: &'static str,
}
impl Default for Cache {
    fn default() -> Self {
        Self {
            records: BTreeMap::new(),
            capacity_limited: false,
            service_type: SERVICE_TYPE,
        }
    }
}

impl Cache {
    fn resolved(
        &mut self,
        service: &ResolvedService,
        protocol: u16,
        own: Option<&str>,
        now: Instant,
    ) {
        let key = service.fullname.to_ascii_lowercase();
        let Ok(candidate) = parse_for(service, protocol, self.service_type) else {
            // An invalid update must not leave a previous apparently-valid entry.
            self.records.remove(&key);
            return;
        };
        if own == Some(candidate.device_id.as_str()) {
            self.records.remove(&key);
            return;
        }
        if !self.records.contains_key(&key) && self.records.len() >= MAX_RECORDS {
            self.capacity_limited = true;
            return;
        }
        let ids: BTreeSet<_> = self
            .records
            .values()
            .map(|r| r.candidate.device_id.as_str())
            .collect();
        if !ids.contains(candidate.device_id.as_str()) && ids.len() >= MAX_CANDIDATES {
            self.capacity_limited = true;
            return;
        }
        self.records.insert(
            key,
            Record {
                candidate,
                seen: now,
            },
        );
    }

    fn removed(&mut self, fullname: &str) {
        self.records.remove(&fullname.to_ascii_lowercase());
    }
    fn expire(&mut self, now: Instant) {
        self.records
            .retain(|_, r| now.saturating_duration_since(r.seen) < CANDIDATE_TIMEOUT);
    }

    fn candidates(&self) -> Vec<Candidate> {
        let mut merged: BTreeMap<String, Candidate> = BTreeMap::new();
        let mut conflicts = BTreeSet::new();
        for record in self.records.values() {
            let candidate = &record.candidate;
            if let Some(old) = merged.get_mut(&candidate.device_id) {
                if old.friendly_name != candidate.friendly_name
                    || old.platform != candidate.platform
                    || old.protocol != candidate.protocol
                {
                    conflicts.insert(candidate.device_id.clone());
                    continue;
                }
                old.addresses.extend(&candidate.addresses);
                old.addresses.sort_unstable();
                old.addresses.dedup();
                old.addresses.truncate(MAX_ADDRESSES);
            } else {
                merged.insert(candidate.device_id.clone(), candidate.clone());
            }
        }
        // An unauthenticated duplicate ID with conflicting metadata is ambiguous.
        for id in conflicts {
            merged.remove(&id);
        }
        merged.into_values().collect()
    }
}

fn publish(
    shared: &Mutex<DiscoverySnapshot>,
    state: DiscoveryState,
    cache: &Cache,
    error: Option<&str>,
) {
    *shared.lock().unwrap_or_else(|e| e.into_inner()) = DiscoverySnapshot {
        state,
        candidates: cache.candidates(),
        capacity_limited: cache.capacity_limited,
        error: error.map(str::to_owned),
    };
}

fn shutdown(daemon: &ServiceDaemon, fullname: Option<&str>, service_type: &str) -> Result<()> {
    if let Some(name) = fullname {
        let _ = daemon.unregister(name);
    }
    let _ = daemon.stop_browse(service_type);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match daemon.shutdown() {
            Ok(response) => {
                ensure!(
                    matches!(
                        response.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                        Ok(DaemonStatus::Shutdown)
                    ),
                    "discovery daemon shutdown unconfirmed"
                );
                return Ok(());
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(_) => bail!("discovery daemon shutdown failed"),
        }
    }
}

fn run(
    config: DiscoveryConfig,
    service_type: &'static str,
    stop: &AtomicBool,
    shared: &Mutex<DiscoverySnapshot>,
) {
    let mut cache = Cache {
        service_type,
        ..Cache::default()
    };
    if stop.load(Ordering::Acquire) {
        publish(shared, DiscoveryState::Stopped, &cache, None);
        return;
    }
    let Ok(daemon) = ServiceDaemon::new() else {
        publish(
            shared,
            DiscoveryState::Error,
            &cache,
            Some("mdns_start_failed"),
        );
        return;
    };
    let own = config.local_device_id.as_deref().or_else(|| {
        config
            .advertisement
            .as_ref()
            .map(|ad| ad.device_id.as_str())
    });
    let mut fullname = None;
    let result = (|| -> Result<()> {
        let monitor = daemon.monitor()?;
        daemon.set_ip_check_interval(5)?;
        let mut events = daemon.browse(service_type)?;
        if let Some(ad) = &config.advertisement {
            let service = ad.service_for(service_type)?;
            fullname = Some(service.get_fullname().to_owned());
            daemon.register(service)?;
        }
        let mut verified = Instant::now();
        let mut refreshed = Instant::now();
        publish(shared, DiscoveryState::Browsing, &cache, None);
        while !stop.load(Ordering::Acquire) {
            // Bound work per cycle so noisy LAN traffic cannot starve stop.
            for _ in 0..64 {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                match events.try_recv() {
                    Ok(ServiceEvent::ServiceResolved(service)) => {
                        cache.resolved(&service, config.compatible_protocol, own, Instant::now())
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, fullname)) => cache.removed(&fullname),
                    Ok(_) => {}
                    Err(mdns_sd::TryRecvError::Empty) => break,
                    Err(mdns_sd::TryRecvError::Disconnected) => bail!("browse stopped"),
                }
            }
            for _ in 0..64 {
                match monitor.try_recv() {
                    Ok(DaemonEvent::Error(_)) => bail!("daemon error"),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            let now = Instant::now();
            cache.expire(now);
            if now.duration_since(verified) >= VERIFY_INTERVAL {
                // Nonresponsive records are flushed by mdns-sd after 10 seconds.
                // Queue pressure is harmless: expiry remains a hard local bound.
                for name in cache.records.keys() {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = daemon.verify(name.clone(), Duration::from_secs(10));
                }
                verified = now;
            }
            if now.duration_since(refreshed) >= REFRESH_INTERVAL {
                // Re-browse provides current DNS cache/address updates even when
                // an unchanged peer does not emit another resolved event.
                daemon.stop_browse(service_type)?;
                events = daemon.browse(service_type)?;
                refreshed = now;
            }
            publish(shared, DiscoveryState::Browsing, &cache, None);
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    })();
    publish(shared, DiscoveryState::Stopping, &cache, None);
    let cleanup = shutdown(&daemon, fullname.as_deref(), service_type);
    cache.records.clear();
    if cleanup.is_err() {
        publish(
            shared,
            DiscoveryState::Error,
            &cache,
            Some("mdns_shutdown_unconfirmed"),
        );
    } else if result.is_err() {
        publish(
            shared,
            DiscoveryState::Error,
            &cache,
            Some("mdns_operation_failed"),
        );
    } else {
        publish(shared, DiscoveryState::Stopped, &cache, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mdns_sd::IntoTxtProperties;

    fn service(id: u32, alias: &str, ip: &str) -> ResolvedService {
        let ad = Advertisement {
            device_id: format!("{id:032x}"),
            friendly_name: "测试设备".into(),
            platform: "windows".into(),
            protocol: 2,
            port: 24800,
            listen_ip: "0.0.0.0".parse().unwrap(),
        };
        let mut service = ad.service().unwrap().as_resolved_service();
        service.fullname = format!("{alias}.{SERVICE_TYPE}");
        service
            .addresses
            .insert(ip.parse::<IpAddr>().unwrap().into());
        service
    }

    #[test]
    fn pairing_and_input_service_caches_are_disjoint() {
        let input = service(1, "input", "192.168.1.2");
        let mut pairing = input.clone();
        pairing.ty_domain = PAIRING_SERVICE_TYPE.into();
        pairing.fullname = format!("pairing.{PAIRING_SERVICE_TYPE}");
        assert!(parse_for(&pairing, 2, PAIRING_SERVICE_TYPE).is_ok());
        assert!(parse_for(&pairing, 2, SERVICE_TYPE).is_err());
        assert!(parse_for(&input, 2, PAIRING_SERVICE_TYPE).is_err());
        let mut input_cache = Cache::default();
        input_cache.resolved(&pairing, 2, None, Instant::now());
        assert!(input_cache.candidates().is_empty());
        let mut pair_cache = Cache {
            service_type: PAIRING_SERVICE_TYPE,
            ..Cache::default()
        };
        pair_cache.resolved(&input, 2, None, Instant::now());
        assert!(pair_cache.candidates().is_empty());
        pair_cache.resolved(&pairing, 2, None, Instant::now());
        assert_eq!(pair_cache.candidates().len(), 1);
        assert!(!pair_cache.candidates()[0].trusted);
    }

    #[test]
    fn validates_advertisement_and_compatible_untrusted_candidates() {
        let good = service(1, "one", "192.168.1.2");
        let parsed = parse(&good, 2).unwrap();
        assert!(parsed.compatible);
        assert!(!parsed.trusted);
        assert!(!parse(&good, 1).unwrap().compatible);
        let mut bad = good.clone();
        bad.port = 0;
        assert!(parse(&bad, 2).is_err());
        bad = good.clone();
        bad.ty_domain = "_other._tcp.local.".into();
        assert!(parse(&bad, 2).is_err());
        bad = good.clone();
        bad.txt_properties = (&[("schema", "1"), ("id", "not-an-id")][..]).into_txt_properties();
        assert!(parse(&bad, 2).is_err());
        let mut ad = Advertisement {
            device_id: "a".repeat(32),
            friendly_name: "\u{202e}hidden".into(),
            platform: "macos".into(),
            protocol: 2,
            port: 1,
            listen_ip: "0.0.0.0".parse().unwrap(),
        };
        assert!(ad.validate().is_err());
        ad.friendly_name = "Mac".into();
        assert!(ad.validate().is_ok());
        assert!(validate_id(&"A".repeat(32)).is_err());
    }

    #[test]
    fn concrete_bind_never_adds_another_address_or_family() {
        let mut ad = Advertisement {
            device_id: "a".repeat(32),
            friendly_name: "Crossflow test".into(),
            platform: "windows".into(),
            protocol: 2,
            port: 41234,
            listen_ip: "192.168.50.2".parse().unwrap(),
        };
        for address in ["192.168.50.2", "2001:db8::2"] {
            ad.listen_ip = address.parse().unwrap();
            let service = ad.service().unwrap();
            assert!(!service.is_addr_auto());
            assert_eq!(service.get_addresses().len(), 1);
            assert!(service.get_addresses().contains(&ad.listen_ip));
            assert_eq!(service.get_port(), 41234);
            assert!(matches!(ad.interface_selector(), IfKind::Addr(ip) if ip == ad.listen_ip));
        }
        ad.listen_ip = "0.0.0.0".parse().unwrap();
        assert!(ad.service().unwrap().is_addr_auto());
        assert!(matches!(ad.interface_selector(), IfKind::IPv4));
        ad.listen_ip = "::".parse().unwrap();
        assert!(ad.service().unwrap().is_addr_auto());
        assert!(matches!(ad.interface_selector(), IfKind::IPv6));
        for address in [
            "127.0.0.1",
            "::1",
            "255.255.255.255",
            "224.0.0.1",
            "ff02::1",
            "::ffff:192.168.50.2",
            "fe80::2",
        ] {
            ad.listen_ip = address.parse().unwrap();
            assert!(ad.service().is_err(), "{address}");
        }
    }

    #[test]
    fn public_ids_are_fresh_and_do_not_require_identity_material() {
        let mut ids = BTreeSet::new();
        for _ in 0..32 {
            let id = new_public_id().unwrap();
            validate_id(&id).unwrap();
            assert!(ids.insert(id));
        }
        assert!(matches!(
            default_device_name(),
            "Crossflow Windows PC" | "Crossflow Mac" | "Crossflow Linux PC" | "Crossflow device"
        ));
    }

    #[test]
    fn explicit_shutdown_wait_is_bounded_and_reports_worker_failure() {
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(DiscoverySnapshot {
            state: DiscoveryState::Starting,
            candidates: vec![],
            capacity_limited: false,
            error: None,
        }));
        let worker_snapshot = snapshot.clone();
        let release = Arc::new(AtomicBool::new(false));
        let worker_release = release.clone();
        let worker = thread::spawn(move || {
            while !worker_release.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            worker_snapshot.lock().unwrap().state = DiscoveryState::Error;
        });
        let mut handle = DiscoveryHandle {
            stop,
            snapshot,
            worker: Some(worker),
        };
        let began = Instant::now();
        let result = handle.wait_stopped(Duration::from_millis(5));
        release.store(true, Ordering::Release);
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(began.elapsed() < Duration::from_millis(500));
        assert!(
            handle
                .wait_stopped(Duration::from_secs(1))
                .unwrap_err()
                .to_string()
                .contains("did not stop cleanly")
        );
    }

    #[test]
    fn scoped_addresses_are_connection_ready_but_not_reachability_claims() {
        let ip = "fe80::123".parse().unwrap();
        assert!(endpoint(ip, 0, 24800).is_none());
        assert_eq!(
            endpoint(ip, 7, 24800).unwrap().to_string(),
            "[fe80::123%7]:24800"
        );
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "ff02::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(endpoint(ip.parse().unwrap(), 1, 24800).is_none(), "{ip}");
        }
    }

    #[test]
    fn merges_interfaces_removes_alias_and_expires() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let one = service(1, "one", "192.168.1.2");
        let two = service(1, "two", "10.0.0.2");
        cache.resolved(&one, 2, None, now);
        cache.resolved(&one, 2, None, now);
        cache.resolved(&two, 2, None, now);
        assert_eq!(cache.candidates().len(), 1);
        assert_eq!(cache.candidates()[0].addresses.len(), 2);
        cache.removed(&one.fullname.to_uppercase());
        assert_eq!(cache.candidates()[0].addresses.len(), 1);
        cache.expire(now + CANDIDATE_TIMEOUT - Duration::from_nanos(1));
        assert_eq!(cache.candidates().len(), 1);
        cache.expire(now + CANDIDATE_TIMEOUT);
        assert!(cache.candidates().is_empty());
    }

    #[test]
    fn update_replaces_addresses_invalid_update_removes_and_own_is_hidden() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let one = service(1, "one", "192.168.1.2");
        cache.resolved(&one, 2, Some(&format!("{:032x}", 1)), now);
        assert!(cache.candidates().is_empty());
        cache.resolved(&one, 2, None, now);
        let mut changed = service(1, "one", "10.0.0.2");
        cache.resolved(&changed, 2, None, now);
        assert_eq!(
            cache.candidates()[0].addresses,
            ["10.0.0.2:24800".parse().unwrap()]
        );
        changed.port = 0;
        cache.resolved(&changed, 2, None, now);
        assert!(cache.candidates().is_empty());
    }

    #[test]
    fn candidates_records_and_addresses_are_bounded() {
        let now = Instant::now();
        let mut cache = Cache::default();
        for id in 1..=100 {
            cache.resolved(&service(id, &format!("peer{id}"), "10.0.0.2"), 2, None, now);
        }
        assert_eq!(cache.candidates().len(), MAX_CANDIDATES);
        assert!(cache.capacity_limited);
        for n in 1..=300 {
            cache.resolved(&service(1, &format!("alias{n}"), "10.0.0.2"), 2, None, now);
        }
        assert_eq!(cache.records.len(), MAX_RECORDS);
        let mut many = service(1, "many", "10.0.0.2");
        for n in 1..=200 {
            many.addresses
                .insert(format!("10.1.1.{n}").parse::<IpAddr>().unwrap().into());
        }
        assert_eq!(parse(&many, 2).unwrap().addresses.len(), MAX_ADDRESSES);
    }

    #[test]
    fn conflicting_ids_are_not_presented_as_one_known_device() {
        let now = Instant::now();
        let mut cache = Cache::default();
        let one = service(1, "one", "192.168.1.2");
        let mut two = service(1, "two", "10.0.0.2");
        two.txt_properties = (&[
            ("schema", "1"),
            ("id", "00000000000000000000000000000001"),
            ("name", "Other"),
            ("platform", "macos"),
            ("protocol", "2"),
        ][..])
            .into_txt_properties();
        cache.resolved(&one, 2, None, now);
        cache.resolved(&two, 2, None, now);
        assert!(cache.candidates().is_empty());
        cache.removed(&two.fullname);
        assert_eq!(cache.candidates().len(), 1);
    }

    #[test]
    #[ignore = "opens real LAN multicast sockets; explicitly opt in on an authorized test network"]
    fn real_pairing_mdns_publish_and_browse() -> Result<()> {
        let ip: std::net::Ipv4Addr = std::env::var("CROSSFLOW_TEST_LAN_IP")
            .context("set CROSSFLOW_TEST_LAN_IP to an authorized local RFC1918 address")?
            .parse()?;
        ensure!(ip.is_private(), "explicit private LAN address required");
        let listener = std::net::TcpListener::bind((ip, 0))?;
        let address = listener.local_addr()?;
        let id = new_public_id()?;
        let mut publisher = DiscoveryHandle::start_pairing(DiscoveryConfig {
            advertisement: Some(Advertisement {
                device_id: id.clone(),
                friendly_name: "Crossflow pairing discovery test".into(),
                platform: "macos".into(),
                protocol: PAIRING_PROTOCOL_VERSION,
                port: address.port(),
                listen_ip: address.ip(),
            }),
            local_device_id: Some(id.clone()),
            compatible_protocol: PAIRING_PROTOCOL_VERSION,
        })?;
        let mut browser = DiscoveryHandle::start_pairing(DiscoveryConfig {
            advertisement: None,
            local_device_id: None,
            compatible_protocol: PAIRING_PROTOCOL_VERSION,
        })?;
        let start = Instant::now();
        let mut found = false;
        let mut publish_snapshot = None;
        let mut browse_snapshot = None;
        while start.elapsed() < Duration::from_secs(10) {
            if let Some(snapshot) = publisher.try_snapshot() {
                publish_snapshot = Some(snapshot);
            }
            if let Some(snapshot) = browser.try_snapshot() {
                found = snapshot.candidates.iter().any(|c| {
                    c.device_id == id
                        && !c.trusted
                        && c.compatible
                        && c.addresses.contains(&address)
                });
                browse_snapshot = Some(snapshot);
            }
            if found
                || publish_snapshot.as_ref().is_some_and(|s| s.error.is_some())
                || browse_snapshot.as_ref().is_some_and(|s| s.error.is_some())
            {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        eprintln!(
            "pairing mDNS publisher={publish_snapshot:?}; browser={browse_snapshot:?}; found={found}"
        );
        let publisher_stop = publisher.wait_stopped(Duration::from_secs(3));
        let browser_stop = browser.wait_stopped(Duration::from_secs(3));
        publisher_stop?;
        browser_stop?;
        ensure!(
            found,
            "pairing service was not discovered on the authorized LAN interface"
        );
        Ok(())
    }

    #[test]
    #[ignore = "opens real LAN multicast sockets; explicitly opt in on an authorized test network"]
    fn real_mdns_publish_and_browse() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let id = format!("{:032x}", std::process::id());
        let mut publisher = DiscoveryHandle::start(DiscoveryConfig {
            advertisement: Some(Advertisement {
                device_id: id.clone(),
                friendly_name: "Crossflow discovery test".into(),
                platform: "windows".into(),
                protocol: 2,
                port: listener.local_addr().unwrap().port(),
                listen_ip: listener.local_addr().unwrap().ip(),
            }),
            local_device_id: Some(id.clone()),
            compatible_protocol: 2,
        })
        .unwrap();
        let mut browser = DiscoveryHandle::start(DiscoveryConfig {
            advertisement: None,
            local_device_id: None,
            compatible_protocol: 2,
        })
        .unwrap();
        let start = Instant::now();
        let mut found = false;
        while start.elapsed() < Duration::from_secs(10) {
            if browser
                .try_snapshot()
                .is_some_and(|s| s.candidates.iter().any(|c| c.device_id == id && !c.trusted))
            {
                found = true;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        publisher.wait_stopped(Duration::from_secs(3)).unwrap();
        browser.wait_stopped(Duration::from_secs(3)).unwrap();
        assert!(
            found,
            "no local mDNS result; multicast/firewall/interface support needs inspection"
        );
    }
}
