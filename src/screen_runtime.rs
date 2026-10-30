//! Validated native-screen bindings and deterministic cross-device cursor routing.
//! No OS input or network IO belongs in this module.
use crate::arrangement::{Layout, Point, Screen};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl NativeRect {
    fn valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|n| n.is_finite() && n.abs() <= 1_000_000.0)
            && self.width >= 1.0
            && self.height >= 1.0
    }
    fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.x + self.width && p.y >= self.y && p.y < self.y + self.height
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub screen_id: String,
    pub native_id: String,
    pub native: NativeRect,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLayout {
    pub layout: Layout,
    pub local_device_id: String,
    pub remote_device_id: String,
    pub bindings: Vec<Binding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Focus {
    pub screen_id: String,
    pub point: Point,
}

impl RuntimeLayout {
    pub fn validate(&self) -> Result<()> {
        self.layout.validate()?;
        ensure!(
            !self.local_device_id.is_empty()
                && !self.remote_device_id.is_empty()
                && self.local_device_id != self.remote_device_id,
            "two distinct device identities required"
        );
        ensure!(
            self.bindings.len() == self.layout.screens().len(),
            "every screen must have one native binding"
        );
        let mut ids = HashSet::new();
        let mut native_ids = HashSet::new();
        for b in &self.bindings {
            ensure!(
                ids.insert(&b.screen_id)
                    && !b.native_id.is_empty()
                    && b.native_id.len() <= 256
                    && b.native.valid(),
                "invalid or duplicate native screen binding"
            );
            let screen = self.screen(&b.screen_id)?;
            ensure!(
                screen.device_id == self.local_device_id
                    || screen.device_id == self.remote_device_id,
                "layout contains an unpaired third device"
            );
            if screen.device_id == self.remote_device_id {
                ensure!(
                    b.native_id.parse::<u32>().is_ok_and(|id| id != 0),
                    "remote native display ID must be a nonzero integer"
                );
            }
            ensure!(
                native_ids.insert((&screen.device_id, &b.native_id)),
                "native monitor bound twice"
            );
        }
        for device in [&self.local_device_id, &self.remote_device_id] {
            ensure!(
                self.layout.screens().iter().any(|s| &s.device_id == device),
                "each device must have a screen"
            );
        }
        Ok(())
    }

    pub fn screen(&self, id: &str) -> Result<&Screen> {
        self.layout
            .screens()
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("unknown runtime screen"))
    }
    fn binding(&self, id: &str) -> Result<&Binding> {
        self.bindings
            .iter()
            .find(|b| b.screen_id == id)
            .ok_or_else(|| anyhow::anyhow!("missing native binding"))
    }
    pub fn is_local(&self, focus: &Focus) -> bool {
        self.screen(&focus.screen_id)
            .is_ok_and(|s| s.device_id == self.local_device_id)
    }
    pub fn local_focus(&self, native: Point) -> Option<Focus> {
        self.bindings.iter().find_map(|b| {
            let s = self.screen(&b.screen_id).ok()?;
            if s.device_id != self.local_device_id || !b.native.contains(native) {
                return None;
            }
            Some(Focus {
                screen_id: s.id.clone(),
                point: Point {
                    x: s.x + (native.x - b.native.x) / b.native.width * s.width,
                    y: s.y + (native.y - b.native.y) / b.native.height * s.height,
                },
            })
        })
    }
    pub fn native_point(&self, focus: &Focus) -> Result<Point> {
        let s = self.screen(&focus.screen_id)?;
        let n = &self.binding(&focus.screen_id)?.native;
        Ok(Point {
            x: n.x + ((focus.point.x - s.x) / s.width * n.width).clamp(0.0, n.width - 1.0),
            y: n.y + ((focus.point.y - s.y) / s.height * n.height).clamp(0.0, n.height - 1.0),
        })
    }
    /// Native motion owns local movement. Only an outward push on an actual
    /// native edge enters the arrangement; this avoids pre-emptive jumps.
    pub fn local_exit(&self, native: Point, dx: f64, dy: f64) -> Result<Option<Focus>> {
        let Some(mut f) = self.local_focus(native) else {
            return Ok(None);
        };
        let s = self.screen(&f.screen_id)?;
        let n = &self.binding(&f.screen_id)?.native;
        let horizontal = if dx < 0.0 && native.x <= n.x {
            f.point.x = s.x;
            true
        } else if dx > 0.0 && native.x >= n.x + n.width - 1.0 {
            f.point.x = s.right();
            true
        } else {
            false
        };
        let vertical = if dy < 0.0 && native.y <= n.y {
            f.point.y = s.y;
            true
        } else if dy > 0.0 && native.y >= n.y + n.height - 1.0 {
            f.point.y = s.bottom();
            true
        } else {
            false
        };
        if !horizontal && !vertical {
            return Ok(None);
        }
        let end = Point {
            x: f.point.x + dx * s.width / n.width,
            y: f.point.y + dy * s.height / n.height,
        };
        if self.layout.crossing(&f.screen_id, f.point, end)?.is_none() {
            return Ok(None);
        }
        Ok(Some(self.advance(
            &f,
            end.x - f.point.x,
            end.y - f.point.y,
        )?))
    }

    /// A native move can already have entered a second local monitor before
    /// Raw Input dispatch. Trace from the last observed native point so an
    /// arranged remote screen inserted between those monitors is not skipped.
    pub fn local_segment(&self, previous: Point, current: Point) -> Result<Option<Focus>> {
        let (Some(from), Some(to)) = (self.local_focus(previous), self.local_focus(current)) else {
            return Ok(None);
        };
        if from.screen_id == to.screen_id {
            return Ok(None);
        }
        let source = self.screen(&from.screen_id)?;
        let n = &self.binding(&from.screen_id)?.native;
        let dx = (current.x - previous.x) * source.width / n.width;
        let dy = (current.y - previous.y) * source.height / n.height;
        if dx.abs() > 20000.0 || dy.abs() > 20000.0 {
            return Ok(None);
        }
        let end = Point {
            x: from.point.x + dx,
            y: from.point.y + dy,
        };
        // No configured neighbor means leave the user's native desktop alone.
        if self
            .layout
            .crossing(&from.screen_id, from.point, end)?
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(self.advance(&from, dx, dy)?))
    }

    /// Carry remaining movement through adjacent screens; gaps and corners
    /// clamp to the current screen, never teleport across empty desktop.
    pub fn advance(&self, focus: &Focus, dx: f64, dy: f64) -> Result<Focus> {
        ensure!(
            dx.is_finite() && dy.is_finite() && dx.abs() <= 20000.0 && dy.abs() <= 20000.0,
            "invalid routing movement"
        );
        let mut result = focus.clone();
        let end = Point {
            x: focus.point.x + dx,
            y: focus.point.y + dy,
        };
        for _ in 0..=self.layout.screens().len() {
            if let Some(c) = self.layout.crossing(&result.screen_id, result.point, end)? {
                result = Focus {
                    screen_id: c.target_screen_id,
                    point: c.entry,
                };
            } else {
                let s = self.screen(&result.screen_id)?;
                result.point = Point {
                    x: end.x.clamp(s.x, s.right()),
                    y: end.y.clamp(s.y, s.bottom()),
                };
                return Ok(result);
            }
        }
        anyhow::bail!("screen route exceeded topology bound")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> RuntimeLayout {
        let screens = vec![
            Screen {
                id: "w1".into(),
                device_id: "local".into(),
                x: 0.,
                y: 0.,
                width: 100.,
                height: 100.,
            },
            Screen {
                id: "m1".into(),
                device_id: "mac".into(),
                x: 100.,
                y: 20.,
                width: 100.,
                height: 100.,
            },
            Screen {
                id: "m2".into(),
                device_id: "mac".into(),
                x: 200.,
                y: 20.,
                width: 100.,
                height: 100.,
            },
        ];
        let bindings = screens
            .iter()
            .enumerate()
            .map(|(i, s)| Binding {
                screen_id: s.id.clone(),
                native_id: i.to_string(),
                native: NativeRect {
                    x: if i == 2 { 200. } else { 0. },
                    y: 0.,
                    width: 200.,
                    height: 200.,
                },
            })
            .collect();
        RuntimeLayout {
            layout: Layout::new(screens).unwrap(),
            local_device_id: "local".into(),
            remote_device_id: "mac".into(),
            bindings,
        }
    }
    #[test]
    fn partial_edge_handoff_and_return_map_native_coordinates() {
        let c = config();
        c.validate().unwrap();
        assert!(
            c.local_exit(Point { x: 190., y: 100. }, 20., 0.)
                .unwrap()
                .is_none()
        );
        assert!(
            c.local_exit(Point { x: 199., y: 10. }, 10., 0.)
                .unwrap()
                .is_none()
        );
        let f = c
            .local_exit(Point { x: 199., y: 100. }, 10., 0.)
            .unwrap()
            .unwrap();
        assert_eq!(f.screen_id, "m1");
        assert_eq!(c.native_point(&f).unwrap(), Point { x: 10., y: 60. });
        let back = c.advance(&f, -10., 0.).unwrap();
        assert!(c.is_local(&back));
        assert_eq!(c.native_point(&back).unwrap(), Point { x: 190., y: 100. });
    }
    #[test]
    fn multiple_remote_monitors_and_gaps() {
        let c = config();
        let f = Focus {
            screen_id: "m1".into(),
            point: Point { x: 190., y: 50. },
        };
        let next = c.advance(&f, 30., 0.).unwrap();
        assert_eq!(next.screen_id, "m2");
        assert_eq!(c.native_point(&next).unwrap(), Point { x: 240., y: 60. });
        let outside = c.advance(&next, 500., 0.).unwrap();
        assert_eq!(outside.point.x, 300.);
    }
    #[test]
    fn rejects_unpaired_device_duplicate_binding_and_nan() {
        let mut c = config();
        c.bindings[1].native_id = c.bindings[2].native_id.clone();
        assert!(c.validate().is_err());
        let mut c = config();
        c.bindings[0].native.width = f64::NAN;
        assert!(c.validate().is_err());
        let mut c = config();
        c.remote_device_id = "other".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn remote_between_two_native_neighbors_is_not_skipped() {
        let c:RuntimeLayout=serde_json::from_str(r#"{"local_device_id":"local","remote_device_id":"mac","layout":{"screens":[{"id":"a","device_id":"local","x":0,"y":0,"width":100,"height":100},{"id":"m","device_id":"mac","x":100,"y":0,"width":100,"height":100},{"id":"b","device_id":"local","x":200,"y":0,"width":100,"height":100}]},"bindings":[{"screen_id":"a","native_id":"a","native":{"x":0,"y":0,"width":100,"height":100}},{"screen_id":"m","native_id":"1","native":{"x":0,"y":0,"width":100,"height":100}},{"screen_id":"b","native_id":"b","native":{"x":100,"y":0,"width":100,"height":100}}]}"#).unwrap();
        c.validate().unwrap();
        let f = c
            .local_segment(Point { x: 98., y: 50. }, Point { x: 103., y: 50. })
            .unwrap()
            .unwrap();
        assert_eq!(f.screen_id, "m");
        assert_eq!(c.native_point(&f).unwrap(), Point { x: 3., y: 50. });
    }
}
