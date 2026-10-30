//! Authenticated display arrangement and routing status. These messages describe
//! a session; they never grant input authority or replace Arm/Pause validation.
use crate::{
    arrangement::{Layout, Screen},
    protocol::{Frame, Kind},
    screen_runtime::RuntimeLayout,
    security::Identity,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_METADATA_BYTES: usize = 32 * 1024;
pub const PREFIX: &[u8; 4] = b"CFM4";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingState {
    pub enabled: bool,
    pub focus_screen_id: Option<String>,
    /// Local queue generation only: never transmitted or accepted from JSON.
    #[serde(skip)]
    pub source_epoch: u64,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn layout() -> SharedLayout {
        let source = "a".repeat(64);
        let receiver = "b".repeat(64);
        SharedLayout {
            schema: 1,
            source_device_id: source.clone(),
            receiver_device_id: receiver.clone(),
            screens: vec![
                SharedScreen {
                    id: "win-primary".into(),
                    device_id: source.clone(),
                    native_id: "DISPLAY1".into(),
                    x: 0.,
                    y: 0.,
                    width: 1920.,
                    height: 1080.,
                },
                SharedScreen {
                    id: "win-secondary".into(),
                    device_id: source,
                    native_id: "DISPLAY2".into(),
                    x: -1920.,
                    y: 0.,
                    width: 1920.,
                    height: 1080.,
                },
                SharedScreen {
                    id: "mac-primary".into(),
                    device_id: receiver,
                    native_id: "7".into(),
                    x: 1920.,
                    y: 0.,
                    width: 1470.,
                    height: 956.,
                },
            ],
        }
    }
    #[test]
    fn old_routing_generation_is_rejected_after_reconnect_even_with_identical_inactive_state() {
        let l = layout();
        let mut q = crate::input::SourceInputQueue::new();
        q.begin_session();
        let stale = RoutingState {
            enabled: true,
            focus_screen_id: Some("win-primary".into()),
            source_epoch: q.control_epoch(),
        };
        assert!(l.routing_matches_source(&stale, &q));
        q.end_session();
        q.begin_session();
        assert!(!q.is_active());
        assert!(!l.routing_matches_source(&stale, &q));
        let current = RoutingState {
            source_epoch: q.control_epoch(),
            ..stale
        };
        assert!(l.routing_matches_source(&current, &q));
        let json = serde_json::to_value(current).unwrap();
        assert!(json.get("source_epoch").is_none());
    }

    #[test]
    fn authenticated_arrangement_keeps_sender_and_receiver_perspectives_explicit() {
        let l = layout();
        let encoded = l.encode().unwrap();
        assert_eq!(
            SharedLayout::decode(&encoded, &l.source_device_id, &l.receiver_device_id).unwrap(),
            l
        );
        assert!(
            SharedLayout::decode(&encoded, &l.receiver_device_id, &l.source_device_id).is_err()
        );
        for (screen, input_active, device) in [
            ("win-primary", false, &l.source_device_id),
            ("win-secondary", false, &l.source_device_id),
            ("mac-primary", true, &l.receiver_device_id),
        ] {
            let routing = RoutingState {
                source_epoch: 0,
                enabled: true,
                focus_screen_id: Some(screen.into()),
            };
            assert_eq!(
                l.routing_state(&l.routing_frame(&routing).unwrap())
                    .unwrap(),
                routing
            );
            assert!(l.routing_matches_input(&routing, input_active));
            assert!(!l.routing_matches_input(&routing, !input_active));
            assert_eq!(l.focus_device(&routing), Some(device.as_str()));
        }
        assert!(l.routing_matches_input(&RoutingState::default(), false));
        assert!(!l.routing_matches_input(&RoutingState::default(), true));
        assert!(
            l.routing_frame(&RoutingState {
                source_epoch: 0,
                enabled: false,
                focus_screen_id: Some("win-primary".into())
            })
            .is_err()
        );
        assert!(
            l.routing_frame(&RoutingState {
                source_epoch: 0,
                enabled: true,
                focus_screen_id: Some("missing".into())
            })
            .is_err()
        );
    }
    #[test]
    fn shared_layout_rejects_invalid_identity_geometry_and_unbounded_payloads() {
        type Mutation = Box<dyn Fn(&mut SharedLayout)>;
        let mutations: Vec<Mutation> = vec![
            Box::new(|l| l.schema = 2),
            Box::new(|l| l.source_device_id = "local".into()),
            Box::new(|l| l.receiver_device_id = l.source_device_id.clone()),
            Box::new(|l| l.screens[0].device_id = "c".repeat(64)),
            Box::new(|l| l.screens[0].id = "x".repeat(129)),
            Box::new(|l| l.screens[0].id = "bad\nlabel".into()),
            Box::new(|l| l.screens[0].native_id = "x".repeat(257)),
            Box::new(|l| l.screens[0].native_id = "bad\tlabel".into()),
            Box::new(|l| l.screens[0].width = 0.5),
            Box::new(|l| l.screens[0].x = f64::NAN),
            Box::new(|l| l.screens[0].height = 1_000_001.),
            Box::new(|l| l.screens[1].x = 0.),
            Box::new(|l| l.screens[1].id = l.screens[0].id.clone()),
            Box::new(|l| l.screens[1].native_id = l.screens[0].native_id.clone()),
            Box::new(|l| l.screens = vec![l.screens[0].clone(); 33]),
            Box::new(|l| l.screens.clear()),
        ];
        for mutation in mutations {
            let mut l = layout();
            mutation(&mut l);
            assert!(l.encode().is_err());
        }
        let l = layout();
        let mut json = serde_json::to_value(&l).unwrap();
        json["extra"] = true.into();
        let mut bytes = PREFIX.to_vec();
        bytes.extend(serde_json::to_vec(&json).unwrap());
        assert!(SharedLayout::decode(&bytes, &l.source_device_id, &l.receiver_device_id).is_err());
        assert!(
            SharedLayout::decode(
                &vec![0; MAX_METADATA_BYTES + 1],
                &l.source_device_id,
                &l.receiver_device_id
            )
            .is_err()
        );
    }
    #[test]
    fn routing_metadata_never_arms_input_and_is_sequence_and_lease_bounded() {
        use crate::state::ReceiverState;
        let l = layout();
        let routing = RoutingState {
            source_epoch: 0,
            enabled: true,
            focus_screen_id: Some("win-primary".into()),
        };
        let mut frame = l.routing_frame(&routing).unwrap();
        frame.seq = 1;
        frame.lease = 7;
        let mut state = ReceiverState::new(150);
        state.grant(7, 0);
        state.apply(frame, 1).unwrap();
        assert!(!state.active);
        assert!(state.apply(frame, 2).is_err());
        frame.seq = 2;
        assert!(state.apply(frame, 151).is_err());
        assert!(!state.active);
        frame.code = 32;
        assert!(l.routing_state(&frame).is_err());
        frame.code = 33;
        assert!(frame.encode().is_err());
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedScreen {
    pub id: String,
    pub device_id: String,
    pub native_id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedLayout {
    pub schema: u8,
    pub source_device_id: String,
    pub receiver_device_id: String,
    pub screens: Vec<SharedScreen>,
}

fn public_identity(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl SharedLayout {
    pub fn from_runtime(layout: &RuntimeLayout, identity: &Identity) -> Result<Self> {
        layout.validate()?;
        let source_device_id = identity.public_key_hex();
        let receiver_device_id = identity.peer_public_key_hex();
        let screens = layout
            .layout
            .screens()
            .iter()
            .map(|screen| {
                let binding = layout
                    .bindings
                    .iter()
                    .find(|b| b.screen_id == screen.id)
                    .expect("validated binding");
                SharedScreen {
                    id: screen.id.clone(),
                    device_id: if screen.device_id == layout.local_device_id {
                        source_device_id.clone()
                    } else {
                        receiver_device_id.clone()
                    },
                    native_id: binding.native_id.clone(),
                    x: screen.x,
                    y: screen.y,
                    width: screen.width,
                    height: screen.height,
                }
            })
            .collect();
        let result = Self {
            schema: 1,
            source_device_id,
            receiver_device_id,
            screens,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1
                && public_identity(&self.source_device_id)
                && public_identity(&self.receiver_device_id)
                && self.source_device_id != self.receiver_device_id,
            "invalid shared layout identity or schema"
        );
        ensure!(
            !self.screens.is_empty() && self.screens.len() <= 32,
            "shared layout must contain 1..32 screens"
        );
        let mut native = HashSet::new();
        for s in &self.screens {
            ensure!(
                !s.id.is_empty()
                    && s.id.len() <= 128
                    && !s.id.chars().any(char::is_control)
                    && !s.native_id.chars().any(char::is_control),
                "invalid shared display label"
            );
            ensure!(
                s.width >= 1.0 && s.height >= 1.0,
                "shared screen dimensions below one unit"
            );
            ensure!(
                s.device_id == self.source_device_id || s.device_id == self.receiver_device_id,
                "unpaired shared layout device"
            );
            ensure!(
                !s.native_id.is_empty()
                    && s.native_id.len() <= 256
                    && native.insert((&s.device_id, &s.native_id)),
                "invalid shared native display identity"
            );
            ensure!(
                [s.x, s.y, s.width, s.height]
                    .iter()
                    .all(|v| v.is_finite() && v.abs() <= 1_000_000.0),
                "shared geometry out of bounds"
            );
        }
        ensure!(
            self.screens
                .iter()
                .any(|s| s.device_id == self.source_device_id)
                && self
                    .screens
                    .iter()
                    .any(|s| s.device_id == self.receiver_device_id),
            "both session devices need a screen"
        );
        Layout::new(
            self.screens
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
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut data = PREFIX.to_vec();
        data.extend(serde_json::to_vec(self)?);
        ensure!(
            data.len() <= MAX_METADATA_BYTES,
            "shared layout exceeds size limit"
        );
        Ok(data)
    }
    pub fn decode(data: &[u8], source: &str, receiver: &str) -> Result<Self> {
        ensure!(
            data.len() <= MAX_METADATA_BYTES && data.starts_with(PREFIX),
            "invalid shared layout header or size"
        );
        let value: Self = serde_json::from_slice(&data[PREFIX.len()..])?;
        value.validate()?;
        ensure!(
            value.source_device_id == source && value.receiver_device_id == receiver,
            "shared layout does not match authenticated devices"
        );
        Ok(value)
    }
    pub fn routing_frame(&self, state: &RoutingState) -> Result<Frame> {
        ensure!(
            state.enabled || state.focus_screen_id.is_none(),
            "paused routing cannot claim focus"
        );
        let mut frame = Frame::new(Kind::RoutingState);
        frame.down = state.enabled;
        if let Some(id) = &state.focus_screen_id {
            frame.code = u16::try_from(
                self.screens
                    .iter()
                    .position(|s| &s.id == id)
                    .ok_or_else(|| anyhow::anyhow!("unknown focus screen"))?
                    + 1,
            )?;
        }
        Ok(frame)
    }
    pub fn routing_state(&self, frame: &Frame) -> Result<RoutingState> {
        frame.encode()?;
        ensure!(frame.kind == Kind::RoutingState, "not routing metadata");
        let screen = if frame.code == 0 {
            None
        } else {
            Some(
                self.screens
                    .get(usize::from(frame.code) - 1)
                    .ok_or_else(|| anyhow::anyhow!("routing screen index out of range"))?
                    .id
                    .clone(),
            )
        };
        Ok(RoutingState {
            enabled: frame.down,
            focus_screen_id: screen,
            source_epoch: 0,
        })
    }
    pub fn focus_device(&self, state: &RoutingState) -> Option<&str> {
        state
            .focus_screen_id
            .as_ref()
            .and_then(|id| self.screens.iter().find(|s| &s.id == id))
            .map(|s| s.device_id.as_str())
    }
    pub fn routing_matches_input(&self, state: &RoutingState, input_active: bool) -> bool {
        if !state.enabled {
            return !input_active;
        }
        match self.focus_device(state) {
            Some(id) if id == self.receiver_device_id => input_active,
            Some(_) => !input_active,
            None => !input_active,
        }
    }
    pub fn routing_matches_source(
        &self,
        state: &RoutingState,
        input: &crate::input::SourceInputQueue,
    ) -> bool {
        state.source_epoch == input.control_epoch()
            && self.routing_matches_input(state, input.is_active())
    }
}
