//! Unauthenticated LAN discovery, separate from pairing and input sessions.
//!
//! Only `start` opens mDNS sockets. Start/stop requests and `try_snapshot` never
//! wait for network I/O. Call `wait_stopped` from a background thread when teardown
//! must be confirmed. Drop requests stop without blocking; it cannot guarantee
//! cleanup after process termination. Addresses are DNS-SD advertisements, not
//! proof of reachability or ownership. Never use `device_id` as a trusted key.
//! The bounded application cache does not bound mdns-sd's internal DNS cache.

use anyhow::{Result, bail, ensure};
use mdns_sd::{
    DaemonEvent, DaemonStatus, ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo,
};
use serde::Serialize;
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
                run(config, &worker_stop, &worker_snapshot);
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

    fn service(&self) -> Result<ServiceInfo> {
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
        Ok(ServiceInfo::new(
            SERVICE_TYPE,
            &format!("cf-{}", self.device_id),
            &format!("cf-{}.local.", self.device_id),
            "",
            self.port,
            &properties[..],
        )?
        .enable_addr_auto())
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

fn parse(service: &ResolvedService, compatible_protocol: u16) -> Result<Candidate> {
    ensure!(
        service.ty_domain.eq_ignore_ascii_case(SERVICE_TYPE)
            && service.fullname.len() <= 255
            && service
                .fullname
                .to_ascii_lowercase()
                .ends_with(SERVICE_TYPE),
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
    };
    ad.validate()?;
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
#[derive(Default)]
struct Cache {
    records: BTreeMap<String, Record>,
    capacity_limited: bool,
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
        let Ok(candidate) = parse(service, protocol) else {
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

fn shutdown(daemon: &ServiceDaemon, fullname: Option<&str>) -> Result<()> {
    if let Some(name) = fullname {
        let _ = daemon.unregister(name);
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
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

fn run(config: DiscoveryConfig, stop: &AtomicBool, shared: &Mutex<DiscoverySnapshot>) {
    let mut cache = Cache::default();
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
        let mut events = daemon.browse(SERVICE_TYPE)?;
        if let Some(ad) = &config.advertisement {
            let service = ad.service()?;
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
                daemon.stop_browse(SERVICE_TYPE)?;
                events = daemon.browse(SERVICE_TYPE)?;
                refreshed = now;
            }
            publish(shared, DiscoveryState::Browsing, &cache, None);
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    })();
    publish(shared, DiscoveryState::Stopping, &cache, None);
    let cleanup = shutdown(&daemon, fullname.as_deref());
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
        };
        let mut service = ad.service().unwrap().as_resolved_service();
        service.fullname = format!("{alias}.{SERVICE_TYPE}");
        service
            .addresses
            .insert(ip.parse::<IpAddr>().unwrap().into());
        service
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
        };
        assert!(ad.validate().is_err());
        ad.friendly_name = "Mac".into();
        assert!(ad.validate().is_ok());
        assert!(validate_id(&"A".repeat(32)).is_err());
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
