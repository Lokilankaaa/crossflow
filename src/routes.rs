//! Bounded, unauthenticated address recovery for an already pinned peer.
//!
//! Discovery contributes routing hints only. Every returned address must pass
//! the SAME stored Noise identity handshake before input or connected state.
//! Names, discovery IDs, protocol compatibility and successful TCP connections
//! never authorize a peer. This module neither changes trust nor writes files.
//! A failed discovery worker is not restarted automatically in this version;
//! saved/previously authenticated routes keep retrying, and restarting the client
//! starts a new browser. This is not a guarantee of recovery from every network change.

use crate::discovery::{DiscoveryConfig, DiscoveryHandle, DiscoverySnapshot, DiscoveryState};
use crate::metrics::Log;
use anyhow::Result;
use serde_json::json;
use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::time::Duration;

const MAX_ROUTES: usize = 64;
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(2);

pub struct LanRoutes {
    saved: SocketAddr,
    preferred: Option<SocketAddr>,
    first: bool,
    prefer_next: bool,
    priority_turn: bool,
    saved_priority_turn: bool,
    candidates: BTreeSet<SocketAddr>,
    last_candidate: Option<SocketAddr>,
    discovery: Option<DiscoveryHandle>,
    discovery_attempted: bool,
    last_state: Option<DiscoveryState>,
    capacity_reported: bool,
    finished: bool,
}

#[derive(Default)]
struct Update {
    state: Option<DiscoveryState>,
    capacity_warning: bool,
}

impl LanRoutes {
    pub fn new(saved: SocketAddr) -> Self {
        Self {
            saved,
            preferred: None,
            first: true,
            prefer_next: false,
            priority_turn: false,
            saved_priority_turn: false,
            candidates: BTreeSet::new(),
            last_candidate: None,
            discovery: None,
            discovery_attempted: false,
            last_state: None,
            capacity_reported: false,
            finished: false,
        }
    }

    /// First call uses only the explicitly saved route. Later calls lazily start
    /// browsing and read a nonblocking snapshot; never wait for network results.
    /// Call once per connection attempt, not inside the authenticated input loop.
    pub fn next_endpoint(&mut self, log: &Log) -> SocketAddr {
        let (address, origin) = if self.first || self.finished {
            self.first = false;
            (self.saved, "saved")
        } else {
            if !self.discovery_attempted && !self.finished {
                self.discovery_attempted = true;
                match DiscoveryHandle::start(DiscoveryConfig {
                    advertisement: None,
                    local_device_id: None,
                    compatible_protocol: u16::from(crate::protocol::WIRE_VERSION),
                }) {
                    Ok(handle) => self.discovery = Some(handle),
                    Err(_) => log.write(
                        "route_discovery_error",
                        json!({"category":"start_failed","saved_route_retained":true}),
                    ),
                }
            }
            let snapshot = self
                .discovery
                .as_ref()
                .and_then(DiscoveryHandle::try_snapshot);
            let update = self.update(snapshot);
            if let Some(state) = update.state {
                log.write("route_discovery_state", json!({"state":state}));
                if matches!(state, DiscoveryState::Error | DiscoveryState::Stopped) {
                    log.write(
                        "route_discovery_error",
                        json!({"category":"unavailable","saved_route_retained":true}),
                    );
                }
            }
            if update.capacity_warning {
                log.write(
                    "route_capacity_limited",
                    json!({"maximum_routes":MAX_ROUTES}),
                );
            }
            self.choose()
        };
        log.write("route_selected", json!({"origin":origin,"address":address}));
        address
    }

    /// ONLY call after the configured pinned Noise handshake succeeds and stop /
    /// deadline checks pass. This remembers a routing hint for this process only.
    /// The caller must still reset the input source to paused on reconnect.
    pub fn authenticated(&mut self, address: SocketAddr) {
        self.preferred = Some(address);
        self.prefer_next = true;
    }

    /// Explicit bounded cleanup for the background network worker. Call off UI
    /// threads, after stopping input. Errors must not mask an earlier input error.
    pub fn finish(&mut self) -> Result<()> {
        self.finished = true;
        self.candidates.clear();
        if let Some(handle) = &mut self.discovery {
            handle.wait_stopped(SHUTDOWN_BUDGET)?;
        }
        self.discovery = None;
        Ok(())
    }

    fn update(&mut self, snapshot: Option<DiscoverySnapshot>) -> Update {
        if self.finished {
            self.candidates.clear();
            return Update::default();
        }
        // A busy snapshot lock is not evidence that devices disappeared.
        let Some(snapshot) = snapshot else {
            return Update::default();
        };
        let state = if self.last_state == Some(snapshot.state) {
            None
        } else {
            self.last_state = Some(snapshot.state);
            Some(snapshot.state)
        };
        if matches!(
            snapshot.state,
            DiscoveryState::Error | DiscoveryState::Stopping | DiscoveryState::Stopped
        ) {
            self.candidates.clear();
            return Update {
                state,
                capacity_warning: false,
            };
        }
        let mut addresses = BTreeSet::new();
        let mut limited = snapshot.capacity_limited;
        for candidate in snapshot.candidates {
            if !candidate.compatible
                || candidate.protocol != u16::from(crate::protocol::WIRE_VERSION)
            {
                continue;
            }
            for address in candidate.addresses {
                if let Some(address) = lan_endpoint(address) {
                    addresses.insert(address);
                    if addresses.len() > MAX_ROUTES {
                        addresses.pop_last();
                        limited = true;
                    }
                }
            }
        }
        // Replace, don't append: removed and expired advertisements cease being
        // candidates. Preserve the address cursor across snapshot refreshes.
        self.candidates = addresses;
        let capacity_warning = limited && !self.capacity_reported;
        self.capacity_reported |= limited;
        Update {
            state,
            capacity_warning,
        }
    }

    fn choose(&mut self) -> (SocketAddr, &'static str) {
        if self.prefer_next {
            self.prefer_next = false;
            self.priority_turn = false;
            if let Some(address) = self.preferred {
                return (address, "preferred");
            }
        }
        if !self.priority_turn {
            self.priority_turn = true;
            if let Some(address) = self.next_candidate() {
                return (address, "discovered");
            }
        } else {
            self.priority_turn = false;
        }
        // If preferred differs, it and saved alternate in priority slots. Every
        // second slot remains available to discovered addresses, even if saved
        // keeps failing; one advertised device cannot monopolize retries.
        if let Some(address) = self.preferred.filter(|address| *address != self.saved) {
            self.saved_priority_turn = !self.saved_priority_turn;
            if !self.saved_priority_turn {
                return (address, "preferred");
            }
        }
        (self.saved, "saved")
    }

    fn next_candidate(&mut self) -> Option<SocketAddr> {
        let eligible =
            |address: &&SocketAddr| **address != self.saved && Some(**address) != self.preferred;
        let next = self
            .candidates
            .iter()
            .filter(eligible)
            .find(|address| self.last_candidate.is_none_or(|last| **address > last))
            .or_else(|| self.candidates.iter().find(eligible))
            .copied();
        if let Some(address) = next {
            self.last_candidate = Some(address)
        }
        next
    }
}

/// Conservative automatic-recovery policy, intentionally stricter than the
/// discovery display parser. No hostname resolution or IPv4-mapped exceptions.
fn lan_endpoint(address: SocketAddr) -> Option<SocketAddr> {
    if address.port() == 0 {
        return None;
    }
    match address.ip() {
        IpAddr::V4(ip) if (ip.is_private() || ip.is_link_local()) && !ip.is_broadcast() => {
            Some(SocketAddr::new(ip.into(), address.port()))
        }
        IpAddr::V6(ip) if ip.to_ipv4_mapped().is_none() => {
            let SocketAddr::V6(v6) = address else {
                unreachable!()
            };
            let unique_local = ip.segments()[0] & 0xfe00 == 0xfc00;
            let link_local = ip.segments()[0] & 0xffc0 == 0xfe80;
            if unique_local {
                Some(SocketAddrV6::new(ip, address.port(), 0, 0).into())
            } else if link_local && v6.scope_id() != 0 {
                Some(SocketAddrV6::new(ip, address.port(), 0, v6.scope_id()).into())
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Candidate;

    fn addr(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }
    fn snapshot(addresses: &[&str]) -> DiscoverySnapshot {
        DiscoverySnapshot {
            state: DiscoveryState::Browsing,
            candidates: vec![Candidate {
                device_id: "untrusted-id".into(),
                friendly_name: "untrusted-name".into(),
                platform: "macos".into(),
                protocol: u16::from(crate::protocol::WIRE_VERSION),
                compatible: true,
                trusted: false,
                addresses: addresses.iter().map(|value| addr(value)).collect(),
            }],
            capacity_limited: false,
            error: None,
        }
    }

    #[test]
    fn strict_lan_policy_rejects_public_loopback_and_mapped_addresses() {
        for input in [
            "8.8.8.8:1",
            "100.64.0.1:1",
            "192.0.2.1:1",
            "127.0.0.2:1",
            "0.0.0.0:1",
            "224.0.0.1:1",
            "255.255.255.255:1",
            "[::1]:1",
            "[::]:1",
            "[ff02::1]:1",
            "[2001:db8::1]:1",
            "[::ffff:192.168.1.1]:1",
            "[::ffff:127.0.0.1]:1",
            "[fe80::1]:1",
            "10.0.0.1:0",
        ] {
            assert!(
                lan_endpoint(addr(input)).is_none(),
                "Rejected address was allowed: {input}"
            );
        }
        for input in [
            "10.0.0.1:1",
            "172.16.0.1:1",
            "192.168.1.1:1",
            "169.254.1.1:1",
            "[fd00::1]:1",
            "[fc00::1]:1",
            "[fe80::1%7]:1",
        ] {
            assert_eq!(lan_endpoint(addr(input)), Some(addr(input)));
        }
        assert!(lan_endpoint(addr("172.32.0.1:1")).is_none());
    }

    #[test]
    fn compatible_is_a_hint_not_trust_and_protocol_must_match() {
        let mut routes = LanRoutes::new(addr("127.0.0.1:5"));
        let mut s = snapshot(&["192.168.1.2:5", "8.8.8.8:5"]);
        routes.update(Some(s.clone()));
        assert_eq!(routes.candidates.len(), 1);
        s.candidates[0].compatible = false;
        routes.update(Some(s.clone()));
        assert!(routes.candidates.is_empty());
        s.candidates[0].compatible = true;
        s.candidates[0].protocol += 1;
        s.candidates[0].trusted = true;
        routes.update(Some(s));
        assert!(routes.candidates.is_empty());
    }

    #[test]
    fn rotation_preserves_saved_and_candidates_despite_snapshot_refresh() {
        let saved = addr("192.168.1.1:5");
        let mut routes = LanRoutes::new(saved);
        let s = snapshot(&["192.168.1.2:5", "192.168.1.3:5", "192.168.1.2:5"]);
        routes.update(Some(s.clone()));
        assert_eq!(routes.choose(), (addr("192.168.1.2:5"), "discovered"));
        routes.update(Some(s.clone()));
        assert_eq!(routes.choose(), (saved, "saved"));
        routes.update(Some(s.clone()));
        assert_eq!(routes.choose(), (addr("192.168.1.3:5"), "discovered"));
        assert_eq!(routes.choose().0, saved);
        routes.update(Some(s));
        assert_eq!(routes.choose().0, addr("192.168.1.2:5"));
    }

    #[test]
    fn authenticated_route_stays_a_process_hint_and_does_not_starve_others() {
        let saved = addr("192.168.1.1:5");
        let preferred = addr("192.168.1.2:5");
        let candidate = addr("192.168.1.3:5");
        let mut routes = LanRoutes::new(saved);
        routes.update(Some(snapshot(&[
            "192.168.1.1:5",
            "192.168.1.2:5",
            "192.168.1.3:5",
        ])));
        routes.authenticated(preferred);
        assert_eq!(routes.choose(), (preferred, "preferred"));
        let sequence: Vec<_> = (0..8).map(|_| routes.choose().0).collect();
        assert_eq!(sequence.iter().filter(|a| **a == candidate).count(), 4);
        assert_eq!(sequence.iter().filter(|a| **a == saved).count(), 2);
        assert_eq!(sequence.iter().filter(|a| **a == preferred).count(), 2);
    }

    #[test]
    fn busy_retains_snapshot_but_errors_stop_and_removal_clear_routes() {
        let mut routes = LanRoutes::new(addr("192.168.1.1:5"));
        routes.update(Some(snapshot(&["192.168.1.2:5"])));
        routes.update(None);
        assert_eq!(routes.candidates.len(), 1);
        routes.update(Some(snapshot(&[])));
        assert!(routes.candidates.is_empty());
        for state in [
            DiscoveryState::Error,
            DiscoveryState::Stopping,
            DiscoveryState::Stopped,
        ] {
            routes.update(Some(snapshot(&["192.168.1.2:5"])));
            let mut s = snapshot(&["192.168.1.2:5"]);
            s.state = state;
            routes.update(Some(s));
            assert!(routes.candidates.is_empty());
            assert_eq!(routes.choose().0, routes.saved);
        }
    }

    #[test]
    fn bounded_unique_addresses_warn_once() {
        let mut routes = LanRoutes::new(addr("192.168.1.1:5"));
        let mut s = snapshot(&[]);
        s.candidates[0].addresses = (1..=100)
            .map(|port| addr(&format!("192.168.1.2:{port}")))
            .collect();
        assert!(routes.update(Some(s.clone())).capacity_warning);
        assert_eq!(routes.candidates.len(), MAX_ROUTES);
        assert!(!routes.update(Some(s)).capacity_warning);
    }

    #[test]
    fn first_saved_attempt_and_unused_shutdown_do_not_start_discovery() {
        let path = std::env::temp_dir().join(format!(
            "crossflow-routes-{}.jsonl",
            crate::discovery::new_public_id().unwrap()
        ));
        let writer = crate::metrics::LogWriter::new(&path, json!({"test":"routes"})).unwrap();
        let saved = addr("127.0.0.1:5");
        let mut routes = LanRoutes::new(saved);
        assert_eq!(routes.next_endpoint(&writer.log), saved);
        assert!(!routes.discovery_attempted && routes.discovery.is_none());
        routes.finish().unwrap();
        routes.update(Some(snapshot(&["192.168.1.2:5"])));
        assert!(routes.candidates.is_empty());
        assert_eq!(routes.next_endpoint(&writer.log), saved);
        assert!(!routes.discovery_attempted && routes.discovery.is_none());
        writer.finish().unwrap();
        let _ = std::fs::remove_file(path);
    }
}
