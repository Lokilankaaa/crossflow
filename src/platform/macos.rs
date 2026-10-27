//! Movement-only CoreGraphics backend for the phase-zero experiment.
//!
//! Requires macOS 10.15+ and user-granted Accessibility/Input Monitoring access.
//! A successful post is not evidence of a visibly rendered cursor update.
//! `gain` maps raw Windows mouse counts linearly to logical display points; this
//! does not reproduce macOS's native acceleration curve. Only the main display
//! is supported. Call `reset()` on every new ARM/session.

use serde_json::{Value, json};
use std::error::Error;
use std::ffi::c_void;
use std::io;
use std::ptr::NonNull;

pub type AdapterError = Box<dyn Error + Send + Sync>;
const PRIVATE_EVENT_SOURCE: i32 = -1;
const COMBINED_SESSION_STATE: i32 = 0;
const MOUSE_MOVED: u32 = 5;
const HID_EVENT_TAP: u32 = 0;
const ESCAPE_KEYCODE: u16 = 53;
const MOUSE_DELTA_X: u32 = 4;
const MOUSE_DELTA_Y: u32 = 5;
const EVENT_SOURCE_USER_DATA: u32 = 42;
pub const ORIGIN_MARKER: i64 = 0x4352_464c;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayIsActive(display: u32) -> u32;
    fn CGDisplayIsAsleep(display: u32) -> u32;
    fn CGDisplayIsBuiltin(display: u32) -> u32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
    fn CGDisplayModeGetWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    fn CGDisplayModeGetRefreshRate(mode: *const c_void) -> f64;
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventSourceSetLocalEventsSuppressionInterval(source: *mut c_void, seconds: f64);
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        kind: u32,
        position: CGPoint,
        button: u32,
    ) -> *mut c_void;
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventPost(tap: u32, event: *const c_void);
    fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    // ApplicationServices Boolean is an unsigned byte, not a C int.
    fn AXIsProcessTrusted() -> u8;
}

fn failure(message: &str) -> AdapterError {
    io::Error::other(message).into()
}

/// A Create/Copy result owns exactly one Core Foundation retain.
struct OwnedCf(NonNull<c_void>);

impl OwnedCf {
    fn new(pointer: *mut c_void, error: &str) -> Result<Self, AdapterError> {
        NonNull::new(pointer)
            .map(Self)
            .ok_or_else(|| failure(error))
    }
    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr()
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        // SAFETY: only non-null Create/Copy results are wrapped; this type is
        // neither Copy nor Clone. Every owned retain is released exactly once.
        unsafe { CFRelease(self.as_ptr()) };
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Bounds {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

impl Bounds {
    fn from_rect(rect: CGRect) -> Result<Self, AdapterError> {
        let bounds = Self {
            left: rect.origin.x,
            top: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        };
        if ![bounds.left, bounds.top, bounds.width, bounds.height]
            .iter()
            .all(|v| v.is_finite())
            || bounds.width < 1.0
            || bounds.height < 1.0
        {
            return Err(failure("CoreGraphics returned invalid display bounds"));
        }
        Ok(bounds)
    }
    fn contains(self, p: CGPoint) -> bool {
        p.x.is_finite()
            && p.y.is_finite()
            && p.x >= self.left
            && p.x < self.left + self.width
            && p.y >= self.top
            && p.y < self.top + self.height
    }
    fn clamp(self, p: CGPoint) -> CGPoint {
        CGPoint {
            x: p.x.clamp(self.left, self.left + self.width - 1.0),
            y: p.y.clamp(self.top, self.top + self.height - 1.0),
        }
    }
}

fn next_point(
    current: CGPoint,
    dx: f64,
    dy: f64,
    gain: f64,
    bounds: Bounds,
) -> Result<CGPoint, AdapterError> {
    if !dx.is_finite() || !dy.is_finite() || !gain.is_finite() || gain <= 0.0 {
        return Err(failure(
            "Mouse deltas must be finite and gain must be positive",
        ));
    }
    let p = CGPoint {
        x: current.x + dx * gain,
        y: current.y + dy * gain,
    };
    if !p.x.is_finite() || !p.y.is_finite() {
        return Err(failure("Scaled mouse movement overflowed"));
    }
    Ok(bounds.clamp(p))
}

pub struct MacMouseSink {
    source: OwnedCf,
    gain: f64,
    display_id: u32,
    bounds: Bounds,
    position: CGPoint,
}

impl MacMouseSink {
    pub fn new(gain: f64) -> Result<Self, AdapterError> {
        if !gain.is_finite() || gain <= 0.0 {
            return Err(failure("Mouse gain must be finite and positive"));
        }
        // SAFETY: these read-only preflight functions take no arguments. No
        // permission-prompt/request API is called by this adapter.
        let accessibility = unsafe { AXIsProcessTrusted() != 0 };
        let can_post = unsafe { CGPreflightPostEventAccess() };
        if !accessibility || !can_post {
            return Err(failure(
                "Crossflow needs Accessibility permission for the receiver or its terminal host. Enable it in System Settings > Privacy & Security > Accessibility, then restart.",
            ));
        }
        if !unsafe { CGPreflightListenEventAccess() } {
            return Err(failure(
                "Crossflow needs Input Monitoring permission for its local Escape stop check. Enable it in System Settings > Privacy & Security > Input Monitoring, restart, and verify holding Escape stops the receiver before testing.",
            ));
        }
        // SAFETY: successful creation transfers one owned retain to this type.
        let source = OwnedCf::new(
            unsafe { CGEventSourceCreate(PRIVATE_EVENT_SOURCE) },
            "CoreGraphics could not create a mouse event source",
        )?;
        unsafe { CGEventSourceSetLocalEventsSuppressionInterval(source.as_ptr(), 0.0) };
        let mut sink = Self {
            source,
            gain,
            display_id: 0,
            bounds: Bounds {
                left: 0.0,
                top: 0.0,
                width: 1.0,
                height: 1.0,
            },
            position: CGPoint::default(),
        };
        sink.reset()?;
        Ok(sink)
    }

    /// Rebase on arm/reconnect; never silently warp from another display.
    pub fn reset(&mut self) -> Result<(), AdapterError> {
        let display = unsafe { CGMainDisplayID() };
        if display == 0
            || unsafe { CGDisplayIsActive(display) == 0 || CGDisplayIsAsleep(display) != 0 }
        {
            return Err(failure(
                "No awake active main display; remote input stopped",
            ));
        }
        let bounds = Bounds::from_rect(unsafe { CGDisplayBounds(display) })?;
        let event = OwnedCf::new(
            unsafe { CGEventCreate(std::ptr::null_mut()) },
            "CoreGraphics could not read the current cursor position",
        )?;
        let position = unsafe { CGEventGetLocation(event.as_ptr()) };
        if !bounds.contains(position) {
            return Err(failure(
                "Move the local cursor onto the main display before arming this main-display-only prototype",
            ));
        }
        self.display_id = display;
        self.bounds = bounds;
        self.position = bounds.clamp(position);
        Ok(())
    }

    /// Return requested logical coordinates, not a display observation.
    /// CGEventPost returns void; its completion does not establish visibility.
    pub fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64), AdapterError> {
        if self.emergency_pressed() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Local Escape requested an emergency stop",
            )
            .into());
        }
        let display = unsafe { CGMainDisplayID() };
        if display != self.display_id
            || unsafe { CGDisplayIsActive(display) == 0 || CGDisplayIsAsleep(display) != 0 }
            || Bounds::from_rect(unsafe { CGDisplayBounds(display) })? != self.bounds
        {
            return Err(failure(
                "Main display changed or slept; stop and re-arm after checking it",
            ));
        }
        let point = next_point(self.position, dx, dy, self.gain, self.bounds)?;
        if point.x == self.position.x && point.y == self.position.y {
            return Ok((point.x, point.y));
        }
        let event = OwnedCf::new(
            unsafe { CGEventCreateMouseEvent(self.source.as_ptr(), MOUSE_MOVED, point, 0) },
            "CoreGraphics could not allocate a mouse event",
        )?;
        // SAFETY: this is a live owned mouse event. Metadata marks our origin
        // so a future capture adapter can reject looped-back input.
        unsafe {
            CGEventSetIntegerValueField(event.as_ptr(), EVENT_SOURCE_USER_DATA, ORIGIN_MARKER);
            CGEventSetIntegerValueField(
                event.as_ptr(),
                MOUSE_DELTA_X,
                (point.x - self.position.x).round() as i64,
            );
            CGEventSetIntegerValueField(
                event.as_ptr(),
                MOUSE_DELTA_Y,
                (point.y - self.position.y).round() as i64,
            );
            CGEventSetFlags(event.as_ptr(), 0);
            CGEventPost(HID_EVENT_TAP, event.as_ptr());
        }
        self.position = point;
        Ok((point.x, point.y))
    }

    /// Caller must poll at most every 10 ms and latch a true result. An entire
    /// press shorter than the polling gap can be missed; hold Escape.
    pub fn emergency_pressed(&self) -> bool {
        emergency_pressed()
    }
}

/// Does not record or inspect any other key. Physical mouse takeover
/// arbitration remains a later phase-zero task.
pub fn emergency_pressed() -> bool {
    unsafe { CGEventSourceKeyState(COMBINED_SESSION_STATE, ESCAPE_KEYCODE) }
}

/// Read metadata only. A mode refresh value is not a measured refresh rate.
pub fn environment() -> Value {
    let hardware_model = std::process::Command::new("/usr/sbin/sysctl")
        .args(["-n", "hw.model"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    let mut ids = [0u32; 32];
    let mut count = 0u32;
    let result = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    let main = unsafe { CGMainDisplayID() };
    let mut displays = Vec::new();
    if result == 0 {
        for &id in ids.iter().take((count as usize).min(ids.len())) {
            let rect = unsafe { CGDisplayBounds(id) };
            let mut data = json!({
                "id": id, "main": id == main,
                "built_in": unsafe { CGDisplayIsBuiltin(id) != 0 },
                "asleep": unsafe { CGDisplayIsAsleep(id) != 0 },
                "bounds_points": [rect.origin.x, rect.origin.y, rect.size.width, rect.size.height],
                "mode_refresh_hz": null, "mode_refresh_status": "unknown"
            });
            if let Ok(mode) =
                OwnedCf::new(unsafe { CGDisplayCopyDisplayMode(id) }, "no display mode")
            {
                let rate = unsafe { CGDisplayModeGetRefreshRate(mode.as_ptr()) };
                data["mode_size"] = json!(unsafe {
                    [
                        CGDisplayModeGetWidth(mode.as_ptr()),
                        CGDisplayModeGetHeight(mode.as_ptr()),
                    ]
                });
                data["pixel_size"] = json!(unsafe {
                    [
                        CGDisplayModeGetPixelWidth(mode.as_ptr()),
                        CGDisplayModeGetPixelHeight(mode.as_ptr()),
                    ]
                });
                if rate.is_finite() && rate > 0.0 {
                    data["mode_refresh_hz"] = json!(rate);
                    data["mode_refresh_status"] = json!("declared by CoreGraphics; not measured");
                }
            }
            displays.push(data);
        }
    }
    json!({
        "adapter": "CoreGraphics movement-only",
        "hardware_model": hardware_model,
        "architecture": std::env::consts::ARCH,
        "permissions": {
            "accessibility": unsafe { AXIsProcessTrusted() != 0 },
            "post_event": unsafe { CGPreflightPostEventAccess() },
            "listen_event": unsafe { CGPreflightListenEventAccess() }
        },
        "display_scope": "main display only", "coordinate_space": "logical display points",
        "displays": displays,
        "display_query_error": if result == 0 { None } else { Some(result) },
        "actual_refresh_hz": null,
        "actual_refresh_status": "not measured; use display settings and external observation",
        "local_takeover": "hold Escape; physical mouse takeover is not yet implemented"
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bounds() -> Bounds {
        Bounds {
            left: -100.0,
            top: 20.0,
            width: 200.0,
            height: 100.0,
        }
    }

    #[test]
    fn coregraphics_structs_match_64_bit_c_abi() {
        assert_eq!(std::mem::size_of::<CGPoint>(), 16);
        assert_eq!(std::mem::size_of::<CGRect>(), 32);
    }
    #[test]
    fn clamp_handles_negative_origin_and_exclusive_edges() {
        let p = bounds().clamp(CGPoint {
            x: 200.0,
            y: -100.0,
        });
        assert_eq!((p.x, p.y), (99.0, 20.0));
        assert!(!bounds().contains(CGPoint { x: 100.0, y: 50.0 }));
    }
    #[test]
    fn gain_preserves_subpoint_motion_and_bounds() {
        let origin = CGPoint { x: 0.0, y: 50.0 };
        let p = next_point(origin, 0.25, -0.5, 2.0, bounds()).unwrap();
        assert_eq!((p.x, p.y), (0.5, 49.0));
        let p = next_point(origin, 500.0, -500.0, 1.0, bounds()).unwrap();
        assert_eq!((p.x, p.y), (99.0, 20.0));
    }
    #[test]
    fn invalid_inputs_never_reach_ffi() {
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(next_point(CGPoint::default(), value, 0.0, 1.0, bounds()).is_err());
        }
        assert!(next_point(CGPoint::default(), f64::MAX, 0.0, 2.0, bounds()).is_err());
        assert!(Bounds::from_rect(CGRect::default()).is_err());
    }
}
