//! Platform-independent display arrangement in logical desktop coordinates.
//!
//! A device owns screens; each screen has a globally unique ID. Coordinates
//! use one common logical unit, not physical millimetres or raw mouse counts.
//! Platform adapters must perform their own display-scale conversion. Shared
//! edges must be exactly aligned: gaps are never bridged by a tolerance.
//! This module computes one handoff and never injects or warps system input.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    fn finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    pub id: String,
    pub device_id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Screen {
    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    fn contains_closed(&self, p: Point) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "screen ID must not be empty");
        ensure!(
            !self.device_id.trim().is_empty(),
            "device ID must not be empty"
        );
        ensure!(
            self.id.len() <= 128 && self.device_id.len() <= 128,
            "screen and device IDs must not exceed 128 UTF-8 bytes"
        );
        ensure!(
            [self.x, self.y, self.width, self.height]
                .iter()
                .all(|n| n.is_finite()),
            "screen geometry must be finite"
        );
        ensure!(
            self.width > 0.0 && self.height > 0.0,
            "screen dimensions must be positive"
        );
        ensure!(
            self.right().is_finite()
                && self.bottom().is_finite()
                && self.right() > self.x
                && self.bottom() > self.y,
            "screen bounds must be finite and representable"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "LayoutData")]
pub struct Layout {
    // Keep validated geometry immutable through this interface.
    screens: Vec<Screen>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutData {
    screens: Vec<Screen>,
}

impl TryFrom<LayoutData> for Layout {
    type Error = anyhow::Error;

    fn try_from(value: LayoutData) -> Result<Self> {
        Self::new(value.screens)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crossing {
    pub target_screen_id: String,
    pub target_device_id: String,
    pub exit_edge: Edge,
    /// Position on the common arrangement, exactly on the shared edge.
    pub entry: Point,
    /// Same point relative to the target screen's logical origin. It may lie
    /// at width/height for entry from the right/bottom; adapters must map that
    /// boundary to their own usable pixel extent without changing this model.
    pub target_local_entry: Point,
    /// Fraction of the supplied segment consumed before crossing. Remaining
    /// motion is not silently applied; the caller can route it separately.
    pub fraction: f64,
}

impl Layout {
    pub fn new(screens: Vec<Screen>) -> Result<Self> {
        let layout = Self { screens };
        layout.validate()?;
        Ok(layout)
    }

    pub fn screens(&self) -> &[Screen] {
        &self.screens
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(!self.screens.is_empty(), "layout must contain a screen");
        ensure!(
            self.screens.len() <= 256,
            "layout supports at most 256 screens"
        );
        let mut ids = HashSet::new();
        for (index, screen) in self.screens.iter().enumerate() {
            screen.validate()?;
            ensure!(ids.insert(&screen.id), "screen IDs must be unique");
            for other in &self.screens[..index] {
                let overlap_x = screen.x < other.right() && other.x < screen.right();
                let overlap_y = screen.y < other.bottom() && other.y < screen.bottom();
                ensure!(
                    !(overlap_x && overlap_y),
                    "screen interiors must not overlap: {} and {}",
                    screen.id,
                    other.id
                );
            }
        }
        Ok(())
    }

    /// Route the first exit of a straight movement segment. `start` must be
    /// inside (or on) the named source screen, and `end` is a proposed global
    /// position, not a clamped position. Merely reaching an edge does not cross.
    ///
    /// Corner hits and endpoints of shared edge intervals do not cross. This
    /// deliberate rule prevents arbitrary choice at T-junctions and diagonal
    /// screen corners. A path that exits through a gap cannot later jump into
    /// a screen even if its endpoint lies there. Only one crossing is returned.
    pub fn crossing(
        &self,
        from_screen: &str,
        start: Point,
        end: Point,
    ) -> Result<Option<Crossing>> {
        ensure!(start.finite() && end.finite(), "movement must be finite");
        let source = self
            .screens
            .iter()
            .find(|s| s.id == from_screen)
            .ok_or_else(|| anyhow::anyhow!("unknown source screen: {from_screen}"))?;
        ensure!(
            source.contains_closed(start),
            "movement start must be on the source screen"
        );
        let dx = end.x - start.x;
        let dy = end.y - start.y;
        ensure!(dx.is_finite() && dy.is_finite(), "movement range overflow");
        let horizontal = if end.x < source.x {
            Some(((source.x - start.x) / dx, Edge::Left))
        } else if end.x > source.right() {
            Some(((source.right() - start.x) / dx, Edge::Right))
        } else {
            None
        };
        let vertical = if end.y < source.y {
            Some(((source.y - start.y) / dy, Edge::Top))
        } else if end.y > source.bottom() {
            Some(((source.bottom() - start.y) / dy, Edge::Bottom))
        } else {
            None
        };
        let (fraction, edge) = match (horizontal, vertical) {
            (None, None) => return Ok(None),
            (Some(exit), None) | (None, Some(exit)) => exit,
            (Some(a), Some(b)) if a.0 == b.0 => return Ok(None),
            (Some(a), Some(b)) => {
                if a.0 < b.0 {
                    a
                } else {
                    b
                }
            }
        };
        if !(0.0..1.0).contains(&fraction) {
            return Ok(None);
        }
        let mut entry = Point {
            x: start.x + fraction * dx,
            y: start.y + fraction * dy,
        };
        // Keep the crossed axis exact, avoiding interpolation roundoff there.
        match edge {
            Edge::Left => entry.x = source.x,
            Edge::Right => entry.x = source.right(),
            Edge::Top => entry.y = source.y,
            Edge::Bottom => entry.y = source.bottom(),
        }
        for target in &self.screens {
            if target.id == source.id {
                continue;
            }
            let shared = match edge {
                Edge::Left | Edge::Right => {
                    let aligned = if edge == Edge::Left {
                        target.right() == source.x
                    } else {
                        target.x == source.right()
                    };
                    aligned
                        && entry.y > source.y.max(target.y)
                        && entry.y < source.bottom().min(target.bottom())
                }
                Edge::Top | Edge::Bottom => {
                    let aligned = if edge == Edge::Top {
                        target.bottom() == source.y
                    } else {
                        target.y == source.bottom()
                    };
                    aligned
                        && entry.x > source.x.max(target.x)
                        && entry.x < source.right().min(target.right())
                }
            };
            if shared {
                return Ok(Some(Crossing {
                    target_screen_id: target.id.clone(),
                    target_device_id: target.device_id.clone(),
                    exit_edge: edge,
                    entry,
                    target_local_entry: Point {
                        x: entry.x - target.x,
                        y: entry.y - target.y,
                    },
                    fraction,
                }));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(id: &str, x: f64, y: f64, width: f64, height: f64) -> Screen {
        Screen {
            id: id.into(),
            device_id: format!("device-{id}"),
            x,
            y,
            width,
            height,
        }
    }

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    #[test]
    fn all_directions_preserve_parallel_position_and_device() {
        let layout = Layout::new(vec![
            screen("center", 0., 0., 100., 100.),
            screen("left", -80., -20., 80., 160.),
            screen("right", 100., -20., 80., 160.),
            screen("top", 0., -70., 100., 70.),
            screen("bottom", 0., 100., 100., 70.),
        ])
        .unwrap();
        for (end, id, edge, entry, local) in [
            (p(-50., 50.), "left", Edge::Left, p(0., 50.), p(80., 70.)),
            (p(150., 50.), "right", Edge::Right, p(100., 50.), p(0., 70.)),
            (p(50., -50.), "top", Edge::Top, p(50., 0.), p(50., 70.)),
            (
                p(50., 150.),
                "bottom",
                Edge::Bottom,
                p(50., 100.),
                p(50., 0.),
            ),
        ] {
            let crossing = layout
                .crossing("center", p(50., 50.), end)
                .unwrap()
                .unwrap();
            assert_eq!(crossing.target_screen_id, id);
            assert_eq!(crossing.target_device_id, format!("device-{id}"));
            assert_eq!(crossing.exit_edge, edge);
            assert_eq!(crossing.entry, entry);
            assert_eq!(crossing.target_local_entry, local);
            assert_eq!(crossing.fraction, 0.5);
            // A cursor exactly on its new boundary can reverse immediately.
            let reverse = layout.crossing(id, entry, p(50., 50.)).unwrap().unwrap();
            assert_eq!(reverse.target_screen_id, "center");
            assert_eq!(reverse.fraction, 0.0);
        }
    }

    #[test]
    fn partial_overlap_and_gap_do_not_warp() {
        let layout = Layout::new(vec![
            screen("a", 0., 0., 100., 100.),
            screen("b", 100., 50., 100., 100.),
        ])
        .unwrap();
        assert!(
            layout
                .crossing("a", p(50., 25.), p(150., 25.))
                .unwrap()
                .is_none()
        );
        assert!(
            layout
                .crossing("a", p(50., 75.), p(150., 75.))
                .unwrap()
                .is_some()
        );
        for y in [50., 100.] {
            assert!(
                layout
                    .crossing("a", p(50., y), p(150., y))
                    .unwrap()
                    .is_none()
            );
        }
        let gap = Layout::new(vec![
            screen("a", 0., 0., 100., 100.),
            screen("b", 100.000001, 0., 100., 100.),
        ])
        .unwrap();
        assert!(
            gap.crossing("a", p(50., 50.), p(150., 50.))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn actual_exit_not_endpoint_selects_target() {
        let layout = Layout::new(vec![
            screen("a", 0., 0., 100., 100.),
            screen("right", 100., 0., 100., 100.),
            screen("diagonal", 100., 100., 100., 100.),
        ])
        .unwrap();
        let crossing = layout
            .crossing("a", p(90., 10.), p(150., 150.))
            .unwrap()
            .unwrap();
        assert_eq!(crossing.target_screen_id, "right");
        assert!((crossing.entry.y - (10. + 140. / 6.)).abs() < 1e-10);
        assert!(
            layout
                .crossing("a", p(50., 50.), p(150., 150.))
                .unwrap()
                .is_none()
        );
        // Exits through the unconnected bottom before reaching a diagonal target.
        assert!(
            layout
                .crossing("a", p(10., 90.), p(150., 150.))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn split_edge_junction_is_not_arbitrarily_assigned() {
        let layout = Layout::new(vec![
            screen("a", 0., 0., 100., 100.),
            screen("upper", 100., 0., 100., 50.),
            screen("lower", 100., 50., 100., 50.),
        ])
        .unwrap();
        for (y, expected) in [(25., "upper"), (75., "lower")] {
            assert_eq!(
                layout
                    .crossing("a", p(50., y), p(150., y))
                    .unwrap()
                    .unwrap()
                    .target_screen_id,
                expected
            );
        }
        assert!(
            layout
                .crossing("a", p(50., 50.), p(150., 50.))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn invalid_layouts_and_movement_are_rejected() {
        assert!(Layout::new(vec![]).is_err());
        assert!(
            Layout::new(
                (0..257)
                    .map(|i| screen(&i.to_string(), i as f64 * 100., 0., 100., 100.))
                    .collect()
            )
            .is_err()
        );
        assert!(Layout::new(vec![screen(&"a".repeat(129), 0., 0., 100., 100.)]).is_err());
        for bad in [
            screen("a", 50., 0., 100., 100.),
            screen("b", 50., 0., 100., 100.),
            screen("b", 100., 0., 0., 100.),
            screen("b", 100., 0., f64::NAN, 100.),
            screen("b", f64::MAX, 0., f64::MAX, 100.),
            screen("b", f64::MAX, 0., 1., 100.),
        ] {
            assert!(Layout::new(vec![screen("a", 0., 0., 100., 100.), bad]).is_err());
        }
        let layout = Layout::new(vec![screen("a", 0., 0., 100., 100.)]).unwrap();
        assert!(
            layout
                .crossing("missing", p(50., 50.), p(60., 60.))
                .is_err()
        );
        assert!(layout.crossing("a", p(-1., 50.), p(60., 60.)).is_err());
        assert!(
            layout
                .crossing("a", p(50., 50.), p(f64::INFINITY, 60.))
                .is_err()
        );
        assert!(
            layout
                .crossing("a", p(50., 50.), p(100., 50.))
                .unwrap()
                .is_none()
        );
        assert!(
            layout
                .crossing("a", p(50., 50.), p(50., 50.))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn deserialization_cannot_bypass_validation() {
        let invalid =
            r#"{"screens":[{"id":"a","device_id":"d","x":0,"y":0,"width":0,"height":100}]}"#;
        assert!(serde_json::from_str::<Layout>(invalid).is_err());
        let layout = Layout::new(vec![screen("a", -500., -200., 320., 180.)]).unwrap();
        let encoded = serde_json::to_string(&layout).unwrap();
        assert_eq!(serde_json::from_str::<Layout>(&encoded).unwrap(), layout);
    }

    #[test]
    fn json_preserves_one_ulp_gaps_and_overlaps() {
        // Raw decimal input is intentional: parsing must not merge a one-ULP
        // gap or overlap into the exact shared edge at x=1920.
        let config = |target_x: &str| {
            format!(
                r#"{{"screens":[{{"id":"a","device_id":"win","x":0,"y":0,"width":1920,"height":1080}},{{"id":"b","device_id":"mac","x":{target_x},"y":0,"width":1920,"height":1080}}]}}"#
            )
        };
        let gap: Layout = serde_json::from_str(&config("1920.0000000000002")).unwrap();
        assert_eq!(gap.screens()[1].x.to_bits(), 1920_f64.to_bits() + 1);
        assert!(
            gap.crossing("a", p(960., 540.), p(1921., 540.))
                .unwrap()
                .is_none()
        );
        assert!(serde_json::from_str::<Layout>(&config("1919.9999999999998")).is_err());
    }

    #[test]
    fn layout_json_roundtrip_preserves_coordinate_bits() {
        let original = Layout::new(vec![screen(
            "a",
            f64::from_bits(1920_f64.to_bits() + 1),
            -f64::from_bits(1920_f64.to_bits() - 1),
            f64::from_bits(1920_f64.to_bits() - 1),
            f64::from_bits(1080_f64.to_bits() + 1),
        )])
        .unwrap();
        let encoded = serde_json::to_string(&original).unwrap();
        let decoded: Layout = serde_json::from_str(&encoded).unwrap();
        let before = &original.screens()[0];
        let after = &decoded.screens()[0];
        for (before, after) in [
            (before.x, after.x),
            (before.y, after.y),
            (before.width, after.width),
            (before.height, after.height),
        ] {
            assert_eq!(before.to_bits(), after.to_bits());
        }
    }
}
