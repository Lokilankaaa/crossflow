//! CoreGraphics backend for the phase-zero experiment.
//!
//! Requires macOS 10.15+ and user-granted Accessibility/Input Monitoring access.
//! A successful post is not evidence of a visibly rendered cursor update.
//! `gain` maps raw Windows mouse counts linearly to logical display points; this
//! does not reproduce macOS's native acceleration curve. Only the main display
//! is supported. Call `reset()` on every new ARM/session.
//! Expanded keyboard/button methods are not enabled by their presence here.
//! The caller must explicitly select them and handle session cleanup.
//!
//! FFI/constants were checked against the MacOSX27 SDK CGEvent.h,
//! CGEventTypes.h and HIToolbox/Events.h on 2026-10-28. Public API references:
//! https://developer.apple.com/documentation/coregraphics/cgeventcreatescrollwheelevent
//! https://developer.apple.com/documentation/coregraphics/cgevent/init(keyboardeventsource:virtualkey:keydown:)
//! https://developer.apple.com/documentation/coregraphics/cgeventsourcestateid/hidsystemstate

use crate::input::Button;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::c_void;
use std::io;
use std::ptr::NonNull;

pub type AdapterError = Box<dyn Error + Send + Sync>;
const PRIVATE_EVENT_SOURCE: i32 = -1;
const HID_SYSTEM_STATE: i32 = 1;
const MOUSE_MOVED: u32 = 5;
const LEFT_MOUSE_DOWN: u32 = 1;
const LEFT_MOUSE_UP: u32 = 2;
const RIGHT_MOUSE_DOWN: u32 = 3;
const RIGHT_MOUSE_UP: u32 = 4;
const LEFT_MOUSE_DRAGGED: u32 = 6;
const RIGHT_MOUSE_DRAGGED: u32 = 7;
const OTHER_MOUSE_DOWN: u32 = 25;
const OTHER_MOUSE_UP: u32 = 26;
const OTHER_MOUSE_DRAGGED: u32 = 27;
const MOUSE_CLICK_STATE: u32 = 1;
const KEYBOARD_AUTOREPEAT: u32 = 8;
const SCROLL_UNIT_LINE: u32 = 1;
const FLAG_SHIFT: u64 = 0x20000;
const FLAG_CONTROL: u64 = 0x40000;
const FLAG_OPTION: u64 = 0x80000;
const FLAG_COMMAND: u64 = 0x100000;
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
    fn CGEventCreateKeyboardEvent(source: *mut c_void, virtual_key: u16, down: bool)
    -> *mut c_void;
    // The fixed-argument function is available from macOS 10.13 and avoids
    // architecture-specific variadic calling conventions on Apple Silicon.
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
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

/// USB HID Keyboard page -> physical ANSI position in HIToolbox/Events.h.
/// This does not translate characters or bypass the target keyboard layout/IME.
/// Caps Lock, Fn, media, keypad and international-specific keys are deliberately
/// unsupported until their lock/layout semantics are implemented and tested.
fn mac_keycode(usage: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F,
        0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    const DIGITS: [u16; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19, 0x1D];
    const FUNCTIONS: [u16; 12] = [
        0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
    ];
    Some(match usage {
        0x04..=0x1D => LETTERS[(usage - 0x04) as usize],
        0x1E..=0x27 => DIGITS[(usage - 0x1E) as usize],
        0x28 => 0x24, // Return
        0x29 => 0x35, // Escape; local emergency polling uses hardware state.
        0x2A => 0x33, // Backspace (Mac Delete)
        0x2B => 0x30, // Tab
        0x2C => 0x31, // Space
        0x2D => 0x1B, // Minus
        0x2E => 0x18, // Equal
        0x2F => 0x21, // Left bracket
        0x30 => 0x1E, // Right bracket
        0x31 => 0x2A, // ANSI backslash
        0x33 => 0x29, // Semicolon
        0x34 => 0x27, // Quote
        0x35 => 0x32, // Grave
        0x36 => 0x2B, // Comma
        0x37 => 0x2F, // Period
        0x38 => 0x2C, // Slash
        0x3A..=0x45 => FUNCTIONS[(usage - 0x3A) as usize],
        0x4A => 0x73, // Home
        0x4B => 0x74, // Page up
        0x4C => 0x75, // Forward delete
        0x4D => 0x77, // End
        0x4E => 0x79, // Page down
        0x4F => 0x7C, // Right
        0x50 => 0x7B, // Left
        0x51 => 0x7D, // Down
        0x52 => 0x7E, // Up
        0xE0 => 0x3B, // Left Control
        0xE1 => 0x38, // Left Shift
        0xE2 => 0x3A, // Left Alt -> Option
        0xE3 => 0x37, // Left GUI -> Command
        0xE4 => 0x3E, // Right Control
        0xE5 => 0x3C, // Right Shift
        0xE6 => 0x3D, // Right Alt -> Option
        0xE7 => 0x36, // Right GUI -> Command
        _ => return None,
    })
}

fn modifier_flags(keys: &BTreeSet<u16>) -> u64 {
    let mut flags = 0;
    for (left, right, mask) in [
        (0xE0, 0xE4, FLAG_CONTROL),
        (0xE1, 0xE5, FLAG_SHIFT),
        (0xE2, 0xE6, FLAG_OPTION),
        (0xE3, 0xE7, FLAG_COMMAND),
    ] {
        if keys.contains(&left) || keys.contains(&right) {
            flags |= mask;
        }
    }
    flags
}

fn mouse_button_number(button: Button) -> u32 {
    match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::Back => 3,
        Button::Forward => 4,
    }
}

fn mouse_button_event(button: Button, down: bool) -> u32 {
    match (button, down) {
        (Button::Left, true) => LEFT_MOUSE_DOWN,
        (Button::Left, false) => LEFT_MOUSE_UP,
        (Button::Right, true) => RIGHT_MOUSE_DOWN,
        (Button::Right, false) => RIGHT_MOUSE_UP,
        (_, true) => OTHER_MOUSE_DOWN,
        (_, false) => OTHER_MOUSE_UP,
    }
}

fn mouse_motion_event(buttons: &BTreeSet<Button>) -> (u32, u32) {
    // Quartz represents one button on a dragged event. Select a stable primary
    // button when more than one is down; their separate down/up states remain.
    match buttons.first().copied() {
        Some(Button::Left) => (LEFT_MOUSE_DRAGGED, 0),
        Some(Button::Right) => (RIGHT_MOUSE_DRAGGED, 1),
        Some(button) => (OTHER_MOUSE_DRAGGED, mouse_button_number(button)),
        None => (MOUSE_MOVED, 0),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct WheelRemainder {
    horizontal: i32,
    vertical: i32,
}

impl WheelRemainder {
    /// Experimental mapping: 120 source units -> one Quartz line. Apple defines
    /// wheel1 as vertical and wheel2 as horizontal. Use vertical sign unchanged
    /// and horizontal negated for Windows-right vs Quartz-left convention; the
    /// direction and natural-scrolling interaction still require physical tests.
    /// Remainders preserve sub-notch input but are discarded at session cleanup.
    fn accumulate(&mut self, horizontal: i32, vertical: i32) -> Result<(i32, i32), AdapterError> {
        if i64::from(horizontal).abs() > 32768 || i64::from(vertical).abs() > 32768 {
            return Err(failure("Scroll delta exceeds the supported bound"));
        }
        let x = self.horizontal + horizontal;
        let y = self.vertical + vertical;
        self.horizontal = x % 120;
        self.vertical = y % 120;
        Ok((y / 120, -(x / 120)))
    }
}

#[derive(Clone, Copy, Debug)]
pub enum MissingInputPermission {
    Accessibility,
    InputMonitoring,
}

impl MissingInputPermission {
    pub fn message(self) -> &'static str {
        match self {
            Self::Accessibility => {
                "Crossflow needs Accessibility permission for the receiver or its terminal host. Enable it in System Settings > Privacy & Security > Accessibility, then restart."
            }
            Self::InputMonitoring => {
                "Crossflow needs Input Monitoring permission for its local Escape stop check. Enable it in System Settings > Privacy & Security > Input Monitoring, restart, and verify holding Escape stops the receiver before testing."
            }
        }
    }
}

/// Checks this process's TCC context without prompting or creating an input owner.
/// A terminal host's successful check does not establish the packaged app's access.
pub fn preflight_input_permissions() -> Result<(), MissingInputPermission> {
    // SAFETY: read-only, no-argument preflight APIs; no permission request is made.
    if unsafe { AXIsProcessTrusted() == 0 || !CGPreflightPostEventAccess() } {
        return Err(MissingInputPermission::Accessibility);
    }
    if !unsafe { CGPreflightListenEventAccess() } {
        return Err(MissingInputPermission::InputMonitoring);
    }
    Ok(())
}

pub struct MacMouseSink {
    source: OwnedCf,
    gain: f64,
    display_id: u32,
    bounds: Bounds,
    position: CGPoint,
    owned_buttons: BTreeSet<Button>,
    owned_keys: BTreeSet<u16>,
    wheel_remainder: WheelRemainder,
}

impl MacMouseSink {
    pub fn new(gain: f64) -> Result<Self, AdapterError> {
        if !gain.is_finite() || gain <= 0.0 {
            return Err(failure("Mouse gain must be finite and positive"));
        }
        preflight_input_permissions().map_err(|permission| failure(permission.message()))?;
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
            owned_buttons: BTreeSet::new(),
            owned_keys: BTreeSet::new(),
            wheel_remainder: WheelRemainder::default(),
        };
        sink.reset()?;
        Ok(sink)
    }

    /// Rebase on arm/reconnect; never silently warp from another display.
    pub fn reset(&mut self) -> Result<(), AdapterError> {
        self.release_owned()?;
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

    /// Checks apply only to new input; release calls deliberately bypass them.
    fn check_input_ready(&self) -> Result<(), AdapterError> {
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
        Ok(())
    }

    fn post_marked(&self, event: &OwnedCf, flags: u64) {
        // SAFETY: event owns a non-null live CGEvent. Posting returns void;
        // successful return establishes neither OS acceptance nor visibility.
        unsafe {
            CGEventSetIntegerValueField(event.as_ptr(), EVENT_SOURCE_USER_DATA, ORIGIN_MARKER);
            CGEventSetFlags(event.as_ptr(), flags);
            CGEventPost(HID_EVENT_TAP, event.as_ptr());
        }
    }

    /// Return requested logical coordinates, not a display observation.
    /// CGEventPost returns void; its completion does not establish visibility.
    pub fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64), AdapterError> {
        self.check_input_ready()?;
        let point = next_point(self.position, dx, dy, self.gain, self.bounds)?;
        if point.x == self.position.x && point.y == self.position.y {
            return Ok((point.x, point.y));
        }
        let (kind, button) = mouse_motion_event(&self.owned_buttons);
        let event = OwnedCf::new(
            unsafe { CGEventCreateMouseEvent(self.source.as_ptr(), kind, point, button) },
            "CoreGraphics could not allocate a mouse event",
        )?;
        // SAFETY: this is a live owned mouse event. Metadata marks our origin
        // so a future capture adapter can reject looped-back input.
        unsafe {
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
        }
        self.post_marked(&event, modifier_flags(&self.owned_keys));
        self.position = point;
        Ok((point.x, point.y))
    }

    /// Send one physical button transition. Releases bypass emergency/display
    /// guards and are idempotent when a failed down never reached this adapter.
    pub fn button(&mut self, button: Button, down: bool) -> Result<(), AdapterError> {
        if down {
            self.check_input_ready()?;
            if self.owned_buttons.contains(&button) {
                return Err(failure("Duplicate remote mouse button down"));
            }
        } else if !self.owned_buttons.contains(&button) {
            return Ok(());
        }
        // On release, prefer the current cursor position so local takeover does
        // not jump back to a cached remote position. Failure to read it must not
        // prevent attempting the release itself.
        let position = if down {
            self.position
        } else {
            OwnedCf::new(
                unsafe { CGEventCreate(std::ptr::null_mut()) },
                "Could not read cursor during cleanup",
            )
            .ok()
            .map(|event| unsafe { CGEventGetLocation(event.as_ptr()) })
            .filter(|point| point.x.is_finite() && point.y.is_finite())
            .unwrap_or(self.position)
        };
        let event = OwnedCf::new(
            unsafe {
                CGEventCreateMouseEvent(
                    self.source.as_ptr(),
                    mouse_button_event(button, down),
                    position,
                    mouse_button_number(button),
                )
            },
            "CoreGraphics could not allocate a mouse button event",
        )?;
        // This first adapter supports individual clicks, not synthesized click
        // counts for double/triple-click selection; that requires separate tests.
        unsafe { CGEventSetIntegerValueField(event.as_ptr(), MOUSE_CLICK_STATE, 1) };
        if down {
            self.owned_buttons.insert(button);
        }
        self.post_marked(&event, modifier_flags(&self.owned_keys));
        if !down {
            self.owned_buttons.remove(&button);
        }
        Ok(())
    }

    /// Send a physical HID keyboard transition. The target layout and input
    /// method determine text. No typed content or usage values enter errors.
    /// Control remains Control; GUI maps to Command. Remapping is a core policy.
    pub fn key(&mut self, usage: u16, down: bool) -> Result<(), AdapterError> {
        if !down && !self.owned_keys.contains(&usage) {
            return Ok(());
        }
        let keycode = mac_keycode(usage)
            .ok_or_else(|| failure("Keyboard usage is unsupported by the ANSI Mac adapter"))?;
        if down {
            self.check_input_ready()?;
            if self.owned_keys.contains(&usage) {
                return Err(failure("Duplicate remote key down"));
            }
        }
        let event = OwnedCf::new(
            unsafe { CGEventCreateKeyboardEvent(self.source.as_ptr(), keycode, down) },
            "CoreGraphics could not allocate a keyboard event",
        )?;
        let mut projected = self.owned_keys.clone();
        if down {
            projected.insert(usage);
        } else {
            projected.remove(&usage);
        }
        // No synthesized repeat timer is implemented; each accepted down/up is
        // explicit. Modifier flags combine both sides and are updated on up.
        unsafe { CGEventSetIntegerValueField(event.as_ptr(), KEYBOARD_AUTOREPEAT, 0) };
        if down {
            self.owned_keys.insert(usage);
        }
        self.post_marked(&event, modifier_flags(&projected));
        if !down {
            self.owned_keys.remove(&usage);
        }
        Ok(())
    }

    /// Line-based wheel prototype, preserving fractions between wheel events.
    /// This is not touchpad gesture/momentum emulation or native-speed parity.
    pub fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<(), AdapterError> {
        self.check_input_ready()?;
        let mut next = self.wheel_remainder;
        let (mut wheel1, mut wheel2) = next.accumulate(horizontal, vertical)?;
        // Keep each event in Apple's documented typical small signed range.
        // A raw packet is bounded, so this loop performs at most 28 posts.
        while wheel1 != 0 || wheel2 != 0 {
            self.check_input_ready()?;
            let y = wheel1.clamp(-10, 10);
            let x = wheel2.clamp(-10, 10);
            let event = OwnedCf::new(
                unsafe {
                    CGEventCreateScrollWheelEvent2(
                        self.source.as_ptr(),
                        SCROLL_UNIT_LINE,
                        2,
                        y,
                        x,
                        0,
                    )
                },
                "CoreGraphics could not allocate a scroll event",
            )?;
            self.post_marked(&event, modifier_flags(&self.owned_keys));
            wheel1 -= y;
            wheel2 -= x;
        }
        self.wheel_remainder = next;
        Ok(())
    }

    /// Attempt every owned release even after an individual allocation failure.
    /// Normal keys release before modifiers, and no unowned local key/button is
    /// explicitly released. Errors retain failed ownership for a later retry.
    /// This cannot run after SIGKILL/process termination or guarantee OS receipt.
    pub fn release_owned(&mut self) -> Result<(), AdapterError> {
        self.wheel_remainder = WheelRemainder::default();
        let mut failed = false;
        for usage in self.owned_keys.iter().copied().collect::<Vec<_>>() {
            if self.key(usage, false).is_err() {
                failed = true;
            }
        }
        for button in self.owned_buttons.iter().copied().collect::<Vec<_>>() {
            if self.button(button, false).is_err() {
                failed = true;
            }
        }
        if failed {
            return Err(failure("Could not submit all owned input releases"));
        }
        Ok(())
    }

    /// Caller must poll at most every 10 ms and latch a true result. An entire
    /// press shorter than the polling gap can be missed; hold Escape.
    pub fn emergency_pressed(&self) -> bool {
        emergency_pressed()
    }
}

impl Drop for MacMouseSink {
    fn drop(&mut self) {
        // Explicit session cleanup is responsible for reporting errors. This
        // best-effort fallback also covers ordinary unwinding, never hard kills.
        let _ = self.release_owned();
    }
}

/// Does not record or inspect any other key. Physical mouse takeover
/// arbitration remains a later phase-zero task.
pub fn emergency_pressed() -> bool {
    unsafe { CGEventSourceKeyState(HID_SYSTEM_STATE, ESCAPE_KEYCODE) }
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
        "adapter": "CoreGraphics movement with opt-in experimental input actions",
        "capabilities": ["movement", "buttons", "drag", "ansi_keyboard_subset", "line_scroll"],
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

    #[test]
    fn ansi_mapping_has_distinct_physical_keys_and_rejects_unknowns() {
        let mapped: Vec<_> = (0..=u16::MAX).filter_map(mac_keycode).collect();
        assert_eq!(
            mapped.len(),
            mapped.iter().copied().collect::<BTreeSet<_>>().len()
        );
        for (hid, mac) in [
            (0x04, 0x00),
            (0x1D, 0x06),
            (0x1E, 0x12),
            (0x27, 0x1D),
            (0x28, 0x24),
            (0x29, 0x35),
            (0x2A, 0x33),
            (0x4C, 0x75),
            (0xE0, 0x3B),
            (0xE4, 0x3E),
            (0xE3, 0x37),
            (0xE7, 0x36),
        ] {
            assert_eq!(mac_keycode(hid), Some(mac));
        }
        for hid in [0, 1, 3, 0x32, 0x39, 0x46, 0x53, 0x65, 0x87, 0xE8, u16::MAX] {
            assert_eq!(mac_keycode(hid), None);
        }
    }

    #[test]
    fn modifier_flags_preserve_other_side_on_release() {
        let mut keys = BTreeSet::from([0x04, 0xE0, 0xE1, 0xE4, 0xE5]);
        assert_eq!(modifier_flags(&keys), FLAG_CONTROL | FLAG_SHIFT);
        keys.remove(&0xE0);
        keys.remove(&0xE1);
        assert_eq!(modifier_flags(&keys), FLAG_CONTROL | FLAG_SHIFT);
        keys.remove(&0xE4);
        keys.remove(&0xE5);
        assert_eq!(modifier_flags(&keys), 0);
        keys.extend([0xE2, 0xE6, 0xE3, 0xE7]);
        assert_eq!(modifier_flags(&keys), FLAG_OPTION | FLAG_COMMAND);
    }

    #[test]
    fn movement_uses_drag_type_only_for_owned_buttons() {
        assert_eq!(mouse_motion_event(&BTreeSet::new()), (MOUSE_MOVED, 0));
        for (button, kind, number) in [
            (Button::Left, LEFT_MOUSE_DRAGGED, 0),
            (Button::Right, RIGHT_MOUSE_DRAGGED, 1),
            (Button::Middle, OTHER_MOUSE_DRAGGED, 2),
            (Button::Back, OTHER_MOUSE_DRAGGED, 3),
            (Button::Forward, OTHER_MOUSE_DRAGGED, 4),
        ] {
            assert_eq!(
                mouse_motion_event(&BTreeSet::from([button])),
                (kind, number)
            );
        }
        let mut held = BTreeSet::from([Button::Left, Button::Right]);
        assert_eq!(mouse_motion_event(&held), (LEFT_MOUSE_DRAGGED, 0));
        held.remove(&Button::Left);
        assert_eq!(mouse_motion_event(&held), (RIGHT_MOUSE_DRAGGED, 1));
        assert_eq!(mouse_button_event(Button::Right, true), RIGHT_MOUSE_DOWN);
        assert_eq!(mouse_button_event(Button::Back, false), OTHER_MOUSE_UP);
    }

    #[test]
    fn wheel_preserves_signed_fractions_and_rejects_large_input_transactionally() {
        let mut wheel = WheelRemainder::default();
        assert_eq!(wheel.accumulate(60, 30).unwrap(), (0, 0));
        assert_eq!(wheel.accumulate(60, 90).unwrap(), (1, -1));
        assert_eq!(wheel, WheelRemainder::default());
        assert_eq!(wheel.accumulate(-60, -60).unwrap(), (0, 0));
        assert_eq!(wheel.accumulate(-60, -60).unwrap(), (-1, 1));
        assert_eq!(wheel.accumulate(119, -119).unwrap(), (0, 0));
        let before = wheel;
        assert!(wheel.accumulate(i32::MIN, 0).is_err());
        assert!(wheel.accumulate(0, i32::MAX).is_err());
        assert_eq!(wheel, before);
        assert_eq!(wheel.accumulate(-119, 119).unwrap(), (0, 0));
        assert_eq!(wheel, WheelRemainder::default());
    }
}
