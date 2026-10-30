//! Symmetric, authenticated layout bindings for one pair session.
//!
//! The caller constructs `local_actual` from the current OS inventory and accepts
//! `peer_authenticated` only over the pinned Noise session. A Hello is metadata,
//! never authentication or input authority. Inventories are copied and validated;
//! changed displays require a fresh layout/control barrier before further input.
use crate::{
    arrangement::{Layout, Screen},
    pair_wire::{Display, Hello, validate_inventory},
    screen_runtime::{Binding, NativeRect, RuntimeLayout},
    security::Identity,
    session_metadata::SharedLayout,
};
use anyhow::{Result, ensure};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct PairLayout {
    shared: SharedLayout,
    local: Hello,
    peer: Hello,
}

fn public_identity(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn native_rect(display: &Display) -> NativeRect {
    NativeRect {
        x: display.x,
        y: display.y,
        width: display.width,
        height: display.height,
    }
}
fn validate_hello(hello: &Hello) -> Result<()> {
    ensure!(
        public_identity(&hello.device_id),
        "inventory identity must be a canonical public key"
    );
    validate_inventory(&hello.inventory)?;
    for (index, display) in hello.inventory.iter().enumerate() {
        ensure!(
            display.x + display.width > display.x && display.y + display.height > display.y,
            "native display bounds are not representable"
        );
        for other in &hello.inventory[..index] {
            // Source layouts historically use native names locally and numeric
            // IDs remotely. Reject aliases resolving to two different monitors.
            ensure!(
                display.native_id != other.wire_id.to_string()
                    && other.native_id != display.wire_id.to_string(),
                "native name aliases another display's wire token"
            );
            let overlap_x =
                display.x < other.x + other.width && other.x < display.x + display.width;
            let overlap_y =
                display.y < other.y + other.height && other.y < display.y + display.height;
            ensure!(
                !(overlap_x && overlap_y),
                "native monitor interiors overlap; local cursor mapping is ambiguous"
            );
        }
    }
    Ok(())
}
fn authenticate_inventories(identity: &Identity, local: &Hello, peer: &Hello) -> Result<()> {
    identity.validate()?;
    validate_hello(local)?;
    validate_hello(peer)?;
    ensure!(
        local.device_id == identity.public_key_hex()
            && peer.device_id == identity.peer_public_key_hex(),
        "inventory identities do not match the authenticated pair"
    );
    ensure!(
        local.inventory.len() + peer.inventory.len() <= 32,
        "paired arrangement supports at most 32 displays total"
    );
    Ok(())
}
fn find_inventory<'a>(id: &str, local: &'a Hello, peer: &'a Hello) -> Result<&'a Hello> {
    if id == local.device_id {
        Ok(local)
    } else if id == peer.device_id {
        Ok(peer)
    } else {
        anyhow::bail!("layout references an unpaired device")
    }
}
fn from_name<'a>(inventory: &'a Hello, name: &str) -> Result<&'a Display> {
    inventory
        .inventory
        .iter()
        .find(|display| display.native_id == name)
        .ok_or_else(|| anyhow::anyhow!("layout references a missing native display"))
}
fn from_runtime_name<'a>(inventory: &'a Hello, name: &str) -> Result<&'a Display> {
    let mut matching = inventory
        .inventory
        .iter()
        .filter(|display| display.native_id == name || display.wire_id.to_string() == name);
    let value = matching.next().ok_or_else(|| {
        anyhow::anyhow!("runtime binding is absent from the current native inventory")
    })?;
    ensure!(
        matching.next().is_none(),
        "runtime binding has ambiguous native identity"
    );
    Ok(value)
}
fn verify_public_runtime_identity(configured: &str, expected: &str) -> Result<()> {
    // Preserve legacy non-key aliases handled by SharedLayout::from_runtime.
    // Never reinterpret a concrete public key as an arbitrary local alias.
    if configured.len() == 64 && configured.bytes().all(|b| b.is_ascii_hexdigit()) {
        ensure!(
            configured == expected,
            "runtime public identity disagrees with authenticated peer"
        );
    }
    Ok(())
}

impl PairLayout {
    /// Canonicalize a saved runtime layout, checking native geometry against the
    /// current local OS inventory and this session's authenticated peer Hello.
    pub fn from_runtime(
        runtime: &RuntimeLayout,
        identity: &Identity,
        local_actual: &Hello,
        peer_authenticated: &Hello,
    ) -> Result<Self> {
        authenticate_inventories(identity, local_actual, peer_authenticated)?;
        verify_public_runtime_identity(&runtime.local_device_id, &local_actual.device_id)?;
        verify_public_runtime_identity(&runtime.remote_device_id, &peer_authenticated.device_id)?;
        let mut shared = SharedLayout::from_runtime(runtime, identity)?;
        for screen in &mut shared.screens {
            let inventory = find_inventory(&screen.device_id, local_actual, peer_authenticated)?;
            let display = from_runtime_name(inventory, &screen.native_id)?;
            let binding = runtime
                .bindings
                .iter()
                .find(|binding| binding.screen_id == screen.id)
                .ok_or_else(|| anyhow::anyhow!("missing source native binding"))?;
            ensure!(
                binding.native == native_rect(display),
                "runtime native geometry changed; rebuild layout from current inventories"
            );
            screen.native_id = display.native_id.clone();
        }
        Self::from_shared(&shared, identity, local_actual, peer_authenticated)
    }

    /// Accept canonical metadata from either perspective. The source/receiver
    /// labels retain authoring orientation; neither label grants control.
    pub fn from_shared(
        shared: &SharedLayout,
        identity: &Identity,
        local_actual: &Hello,
        peer_authenticated: &Hello,
    ) -> Result<Self> {
        authenticate_inventories(identity, local_actual, peer_authenticated)?;
        shared.validate()?;
        let pair = HashSet::from([
            local_actual.device_id.as_str(),
            peer_authenticated.device_id.as_str(),
        ]);
        ensure!(
            pair.contains(shared.source_device_id.as_str())
                && pair.contains(shared.receiver_device_id.as_str()),
            "shared layout does not describe this authenticated pair"
        );
        ensure!(
            shared.screens.len()
                == local_actual.inventory.len() + peer_authenticated.inventory.len(),
            "shared layout must bind every current monitor exactly once"
        );
        let mut seen = HashSet::new();
        for screen in &shared.screens {
            let inventory = find_inventory(&screen.device_id, local_actual, peer_authenticated)?;
            let display = from_name(inventory, &screen.native_id)?;
            ensure!(
                seen.insert((&screen.device_id, &display.native_id)),
                "native monitor bound more than once"
            );
        }
        let result = Self {
            shared: shared.clone(),
            local: local_actual.clone(),
            peer: peer_authenticated.clone(),
        };
        result.runtime_for(&local_actual.device_id)?;
        result.runtime_for(&peer_authenticated.device_id)?;
        Ok(result)
    }

    pub fn shared(&self) -> &SharedLayout {
        &self.shared
    }

    /// Both perspectives use numeric sink tokens, including their local side.
    /// Arrangement coordinates never flip; only local/remote device roles change.
    pub fn runtime_for(&self, local_device_id: &str) -> Result<RuntimeLayout> {
        let local = find_inventory(local_device_id, &self.local, &self.peer)?;
        let remote = if local.device_id == self.local.device_id {
            &self.peer
        } else {
            &self.local
        };
        let mut screens = Vec::with_capacity(self.shared.screens.len());
        let mut bindings = Vec::with_capacity(self.shared.screens.len());
        for screen in &self.shared.screens {
            let inventory = find_inventory(&screen.device_id, &self.local, &self.peer)?;
            let display = from_name(inventory, &screen.native_id)?;
            screens.push(Screen {
                id: screen.id.clone(),
                device_id: screen.device_id.clone(),
                x: screen.x,
                y: screen.y,
                width: screen.width,
                height: screen.height,
            });
            bindings.push(Binding {
                screen_id: screen.id.clone(),
                native_id: display.wire_id.to_string(),
                native: native_rect(display),
            });
        }
        let result = RuntimeLayout {
            layout: Layout::new(screens)?,
            local_device_id: local.device_id.clone(),
            remote_device_id: remote.device_id.clone(),
            bindings,
        };
        result.validate()?;
        Ok(result)
    }

    /// Read-only recheck before arming or after a local display notification.
    /// Order is irrelevant; native names, tokens and bounds must still match.
    pub fn validate_local_inventory(&self, current: &Hello) -> Result<()> {
        validate_hello(current)?;
        ensure!(
            current.device_id == self.local.device_id,
            "local inventory identity changed"
        );
        let expected: HashMap<_, _> = self
            .local
            .inventory
            .iter()
            .map(|d| (d.native_id.as_str(), d))
            .collect();
        ensure!(
            current.inventory.len() == expected.len()
                && current.inventory.iter().all(|d| expected
                    .get(d.native_id.as_str())
                    .is_some_and(|old| *old == d)),
            "local display inventory changed; pause and renegotiate layout"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{arrangement::Point, session_metadata::SharedScreen};
    fn identities() -> (Identity, Identity) {
        let builder = snow::Builder::new(crate::security::PATTERN.parse().unwrap());
        let a = builder.generate_keypair().unwrap();
        let b = builder.generate_keypair().unwrap();
        (
            Identity::from_authorized_pairing(
                "windows",
                "a".repeat(32),
                zeroize::Zeroizing::new(a.private.as_slice().try_into().unwrap()),
                b.public.as_slice().try_into().unwrap(),
            )
            .unwrap(),
            Identity::from_authorized_pairing(
                "mac",
                "a".repeat(32),
                zeroize::Zeroizing::new(b.private.as_slice().try_into().unwrap()),
                a.public.as_slice().try_into().unwrap(),
            )
            .unwrap(),
        )
    }
    fn display(name: &str, wire_id: u32, x: f64, y: f64, w: f64, h: f64) -> Display {
        Display {
            native_id: name.into(),
            wire_id,
            x,
            y,
            width: w,
            height: h,
        }
    }
    fn fixture() -> (Identity, Identity, Hello, Hello, SharedLayout) {
        let (a, b) = identities();
        let windows = Hello {
            device_id: a.public_key_hex(),
            inventory: vec![
                display("DISPLAY1", 1001, 0., 0., 3840., 2160.),
                display("DISPLAY2", 1002, -1920., -300., 1920., 1080.),
            ],
        };
        let mac = Hello {
            device_id: b.public_key_hex(),
            inventory: vec![
                display("7", 7, 0., 0., 1470., 956.),
                display("8", 8, 1470., -100., 1920., 1080.),
            ],
        };
        let specs = [
            ("w1", &windows, "DISPLAY1", 0., 0., 1920., 1080.),
            ("w2", &windows, "DISPLAY2", -1920., -300., 1920., 1080.),
            ("m1", &mac, "7", 1920., 0., 1470., 956.),
            ("m2", &mac, "8", 3390., -100., 1920., 1080.),
        ];
        let shared = SharedLayout {
            schema: 1,
            source_device_id: windows.device_id.clone(),
            receiver_device_id: mac.device_id.clone(),
            screens: specs
                .into_iter()
                .map(|(id, owner, native, x, y, width, height)| SharedScreen {
                    id: id.into(),
                    device_id: owner.device_id.clone(),
                    native_id: native.into(),
                    x,
                    y,
                    width,
                    height,
                })
                .collect(),
        };
        (a, b, windows, mac, shared)
    }
    #[test]
    fn both_perspectives_share_geometry_and_roundtrip_mixed_dpi() {
        let (a, b, win, mac, shared) = fixture();
        let from_win = PairLayout::from_shared(&shared, &a, &win, &mac).unwrap();
        let from_mac = PairLayout::from_shared(&shared, &b, &mac, &win).unwrap();
        let w = from_win.runtime_for(&win.device_id).unwrap();
        let m = from_mac.runtime_for(&mac.device_id).unwrap();
        assert_eq!(w.layout, m.layout);
        for (runtime, points) in [
            (
                &w,
                vec![
                    Point { x: 1234., y: 567. },
                    Point {
                        x: -1800.,
                        y: -200.,
                    },
                ],
            ),
            (
                &m,
                vec![Point { x: 400., y: 500. }, Point { x: 2000., y: -50. }],
            ),
        ] {
            for point in points {
                let focus = runtime.local_focus(point).unwrap();
                assert!(runtime.is_local(&focus));
                let back = runtime.native_point(&focus).unwrap();
                assert!((back.x - point.x).abs() < 1e-9 && (back.y - point.y).abs() < 1e-9);
            }
            assert!(
                runtime
                    .bindings
                    .iter()
                    .all(|binding| binding.native_id.parse::<u32>().unwrap() > 0)
            );
        }
        let crossed = w
            .local_exit(Point { x: 3839., y: 1000. }, 4., 0.)
            .unwrap()
            .unwrap();
        assert_eq!(crossed.screen_id, "m1");
        assert!(!w.is_local(&crossed));
        assert!(m.is_local(&crossed));
        let native = m.native_point(&crossed).unwrap();
        assert!((native.y - 500.).abs() < 1e-9);
        let back = m
            .local_exit(Point { x: 0., y: 500. }, -2., 0.)
            .unwrap()
            .unwrap();
        assert_eq!(back.screen_id, "w1");
        assert!(w.is_local(&back));
    }
    #[test]
    fn source_alias_and_tokens_canonicalize_and_reverse_again() {
        let (a, b, win, mac, shared) = fixture();
        let pair = PairLayout::from_shared(&shared, &a, &win, &mac).unwrap();
        let mut runtime = pair.runtime_for(&win.device_id).unwrap();
        runtime.local_device_id = "local".into();
        runtime.layout = Layout::new(
            runtime
                .layout
                .screens()
                .iter()
                .cloned()
                .map(|mut s| {
                    if s.device_id == win.device_id {
                        s.device_id = "local".into();
                    }
                    s
                })
                .collect(),
        )
        .unwrap();
        runtime.bindings[0].native_id = "DISPLAY1".into();
        runtime.bindings[1].native_id = "DISPLAY2".into();
        let canonical = PairLayout::from_runtime(&runtime, &a, &win, &mac).unwrap();
        assert_eq!(canonical.shared(), &shared);
        let reverse = canonical.runtime_for(&mac.device_id).unwrap();
        let reversed = PairLayout::from_runtime(&reverse, &b, &mac, &win).unwrap();
        assert_eq!(reversed.shared().screens, shared.screens);
        assert_eq!(reversed.shared().source_device_id, mac.device_id);
        assert_eq!(reversed.shared().receiver_device_id, win.device_id);
    }
    #[test]
    fn wrong_identity_stale_geometry_and_hotplug_are_rejected() {
        let (a, _, win, mac, shared) = fixture();
        let pair = PairLayout::from_shared(&shared, &a, &win, &mac).unwrap();
        let mut wrong = win.clone();
        wrong.device_id = "c".repeat(64);
        assert!(PairLayout::from_shared(&shared, &a, &wrong, &mac).is_err());
        let mut runtime = pair.runtime_for(&win.device_id).unwrap();
        runtime.bindings[0].native.width += 1.;
        assert!(PairLayout::from_runtime(&runtime, &a, &win, &mac).is_err());
        let mut reordered = win.clone();
        reordered.inventory.reverse();
        assert!(pair.validate_local_inventory(&reordered).is_ok());
        reordered.inventory[0].wire_id += 1;
        assert!(pair.validate_local_inventory(&reordered).is_err());
        let mut changed = win.clone();
        changed.inventory[0].width -= 1.;
        assert!(pair.validate_local_inventory(&changed).is_err());
        let mut missing = shared.clone();
        missing.screens.remove(1);
        assert!(PairLayout::from_shared(&missing, &a, &win, &mac).is_err());
        assert!(pair.runtime_for(&"d".repeat(64)).is_err());
    }
    #[test]
    fn duplicate_names_tokens_aliases_and_ambiguous_native_bounds_fail() {
        let (a, _, win, mac, shared) = fixture();
        for mutation in 0..5 {
            let mut bad = win.clone();
            match mutation {
                0 => bad.inventory[1].wire_id = bad.inventory[0].wire_id,
                1 => bad.inventory[1].native_id = bad.inventory[0].native_id.clone(),
                2 => bad.inventory[1].native_id = bad.inventory[0].wire_id.to_string(),
                3 => {
                    bad.inventory[1].x = 0.;
                    bad.inventory[1].y = 0.;
                }
                _ => bad.inventory[1].wire_id = 0,
            }
            assert!(PairLayout::from_shared(&shared, &a, &bad, &mac).is_err());
        }
    }
    #[test]
    fn token_namespace_is_per_device_and_total_screen_count_is_bounded() {
        let (a, _, win, mut mac, shared) = fixture();
        mac.inventory[0].wire_id = win.inventory[0].wire_id;
        assert!(PairLayout::from_shared(&shared, &a, &win, &mac).is_ok());
        let mut huge = win.clone();
        huge.inventory = (0..30)
            .map(|i| {
                display(
                    &format!("screen{i}"),
                    100 + i,
                    100. * f64::from(i),
                    0.,
                    100.,
                    100.,
                )
            })
            .collect();
        let mut bounded = shared.clone();
        bounded.screens = huge
            .inventory
            .iter()
            .enumerate()
            .map(|(i, d)| SharedScreen {
                id: format!("w{i}"),
                device_id: win.device_id.clone(),
                native_id: d.native_id.clone(),
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
            })
            .chain(mac.inventory.iter().enumerate().map(|(i, d)| SharedScreen {
                id: format!("m{i}"),
                device_id: mac.device_id.clone(),
                native_id: d.native_id.clone(),
                x: 3000. + i as f64 * 2000.,
                y: 0.,
                width: d.width,
                height: d.height,
            }))
            .collect();
        assert_eq!(bounded.screens.len(), 32);
        assert!(PairLayout::from_shared(&bounded, &a, &huge, &mac).is_ok());
        huge.inventory
            .push(display("extra", 999, 3000., 0., 100., 100.));
        let error = PairLayout::from_shared(&bounded, &a, &huge, &mac).unwrap_err();
        assert!(error.to_string().contains("32 displays total"));
    }
    #[test]
    fn canonical_layout_does_not_accept_wire_token_as_stable_name() {
        let (a, _, win, mac, mut shared) = fixture();
        shared.screens[0].native_id = win.inventory[0].wire_id.to_string();
        assert!(PairLayout::from_shared(&shared, &a, &win, &mac).is_err());
    }
}
