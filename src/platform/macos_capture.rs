//! macOS source capture, independent of transport and UI.
//!
//! Session event taps expose Quartz relative deltas, NOT guaranteed unaccelerated
//! HID counts. Coordinates are global logical points. Each accepted event is also
//! a local-activity notification. We reject Crossflow's tag and nonzero source
//! PIDs; this is loop prevention, not a security proof of hardware provenance.
//!
//! Call `set_remote_focus(true)` at least every 50 ms while forwarding. Its 100 ms
//! lease fails open locally and permanently stops this capture on expiry. Drain
//! events frequently; overflow stops capture rather than dropping key releases.
//! On *any* stop, the integration must release remotely owned input and relinquish
//! control. This module never injects keys or changes system permissions.

use crate::input::Button;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

pub const ORIGIN_MARKER: i64 = 0x4352_464c;
const CAPACITY: usize = 512;
const LEASE_NS: u64 = 400_000_000;
const STALL_NS: u64 = 1_500_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CaptureKind {
    Delta {
        dx: f64,
        dy: f64,
    },
    Button {
        button: Button,
        down: bool,
    },
    Key {
        usage: u16,
        down: bool,
    },
    /// Windows-compatible wheel units (120/line); Quartz left is negated.
    Scroll {
        horizontal: i32,
        vertical: i32,
    },
    /// Unsupported physical key; local delivery continues, remote focus stops.
    UnsupportedKey {
        keycode: u16,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct CapturedEvent {
    pub kind: CaptureKind,
    pub captured_at: Instant,
    pub native_x: f64,
    pub native_y: f64,
    /// Quartz's own nanosecond timestamp; do not subtract from Instant directly.
    pub native_timestamp_ns: u64,
    pub modifiers: u64,
    pub remote_focus: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HeldSnapshot {
    pub keys: Vec<u16>,
    pub buttons: Vec<Button>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SourceCounts {
    pub physical_candidate: u64,
    pub own_injected: u64,
    pub other_process: u64,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Requested = 1,
    EmergencyShortcut,
    QueueOverflow,
    TapDisabled,
    PermissionsLost,
    SleepOrStall,
    FocusLeaseExpired,
    UnsupportedInput,
    CallbackPanicked,
    SetupFailed,
    FocusRace,
}
impl StopReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Requested => "native capture stopped (requested)",
            Self::EmergencyShortcut => "local emergency shortcut",
            Self::QueueOverflow => "native capture stopped (queue overflow)",
            Self::TapDisabled => "native capture stopped (event tap disabled by macOS)",
            Self::PermissionsLost => "native capture stopped (permissions lost or probe stale)",
            Self::SleepOrStall => "native capture stopped (run loop stall or sleep)",
            Self::FocusLeaseExpired => "native capture stopped (focus lease expired)",
            Self::UnsupportedInput => "native capture stopped (unsupported input)",
            Self::CallbackPanicked => "native capture stopped (callback panicked)",
            Self::SetupFailed => "native capture stopped (setup failed)",
            Self::FocusRace => "native capture stopped (focus handoff race)",
        }
    }
}
fn reason(value: u8) -> Option<StopReason> {
    Some(match value {
        1 => StopReason::Requested,
        2 => StopReason::EmergencyShortcut,
        3 => StopReason::QueueOverflow,
        4 => StopReason::TapDisabled,
        5 => StopReason::PermissionsLost,
        6 => StopReason::SleepOrStall,
        7 => StopReason::FocusLeaseExpired,
        8 => StopReason::UnsupportedInput,
        9 => StopReason::CallbackPanicked,
        10 => StopReason::SetupFailed,
        11 => StopReason::FocusRace,
        _ => return None,
    })
}

/// Private SPSC ring: only the tap thread pushes; only &mut MacCapture pops.
/// Slots are published with release/acquire and never concurrently accessed.
struct Ring<T: Copy> {
    slots: [UnsafeCell<MaybeUninit<T>>; CAPACITY],
    write: AtomicUsize,
    read: AtomicUsize,
}
unsafe impl<T: Copy + Send> Sync for Ring<T> {}
impl<T: Copy> Ring<T> {
    fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }
    // Safety: there must be exactly one producer.
    unsafe fn push(&self, value: T) -> bool {
        let write = self.write.load(Ordering::Relaxed);
        if write.wrapping_sub(self.read.load(Ordering::Acquire)) >= CAPACITY {
            return false;
        }
        unsafe { (*self.slots[write % CAPACITY].get()).write(value) };
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }
    // Safety: there must be exactly one consumer.
    unsafe fn pop(&self) -> Option<T> {
        let read = self.read.load(Ordering::Relaxed);
        if read == self.write.load(Ordering::Acquire) {
            return None;
        }
        let value = unsafe { (*self.slots[read % CAPACITY].get()).assume_init_read() };
        self.read.store(read.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct Shared {
    routing: AtomicBool,
    queue: Ring<CapturedEvent>,
    stop: AtomicU8,
    remote_until: AtomicU64,
    loop_tick: AtomicU64,
    held: AtomicUsize,
    activity: AtomicU64,
    own_injected: AtomicU64,
    other_process: AtomicU64,
    physical_candidate: AtomicU64,
}
impl Shared {
    fn new() -> Self {
        Self {
            routing: AtomicBool::new(false),
            queue: Ring::new(),
            stop: AtomicU8::new(0),
            remote_until: AtomicU64::new(0),
            loop_tick: AtomicU64::new(0),
            held: AtomicUsize::new(0),
            activity: AtomicU64::new(0),
            own_injected: AtomicU64::new(0),
            other_process: AtomicU64::new(0),
            physical_candidate: AtomicU64::new(0),
        }
    }
    fn stop(&self, why: StopReason) {
        self.remote_until.store(0, Ordering::SeqCst);
        let _ = self
            .stop
            .compare_exchange(0, why as u8, Ordering::SeqCst, Ordering::SeqCst);
    }
    fn remote(&self, now: u64) -> bool {
        if self.stop.load(Ordering::SeqCst) != 0 {
            return false;
        }
        if now.saturating_sub(self.loop_tick.load(Ordering::Acquire)) > STALL_NS {
            self.stop(StopReason::SleepOrStall);
            return false;
        }
        let until = self.remote_until.load(Ordering::SeqCst);
        if until != 0 && now >= until {
            self.stop(StopReason::FocusLeaseExpired);
            return false;
        }
        until != 0
    }
    fn focus(&self, remote: bool, now: u64) -> bool {
        self.focus_inner(remote, now, false)
    }
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn focus_allow_held(&self, remote: bool, now: u64) -> bool {
        self.focus_inner(remote, now, true)
    }
    fn focus_inner(&self, remote: bool, now: u64, allow_held: bool) -> bool {
        if !remote {
            self.remote_until.store(0, Ordering::SeqCst);
            return true;
        }
        // Serialize only the bounded routing decision, never wait in the tap.
        // If a physical press is in flight the caller can retry on its next poll.
        let Some(_routing) = self.try_route() else {
            return false;
        };
        self.remote(now);
        if self.stop.load(Ordering::SeqCst) != 0 {
            return false;
        }
        let until = self.remote_until.load(Ordering::SeqCst);
        if until != 0 && now >= until {
            self.stop(StopReason::FocusLeaseExpired);
            return false;
        }
        // Do not strand a key/button which was delivered to a local application
        // on edge drag; explicit QuickJump safely passes `allow_held = true`
        // because `route_press` delivers pre-held local releases back to local macOS.
        if !allow_held && until == 0 && self.held.load(Ordering::SeqCst) != 0 {
            return false;
        }
        self.remote_until
            .store(now.saturating_add(LEASE_NS), Ordering::SeqCst);
        self.stop.load(Ordering::SeqCst) == 0
    }
    fn try_route(&self) -> Option<RoutingGuard<'_>> {
        self.routing
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| RoutingGuard(&self.routing))
    }
}

struct RoutingGuard<'a>(&'a AtomicBool);
impl Drop for RoutingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Physical position mapping matching the Mac sink (including Caps Lock, Fn/Globe,
/// Insert/Help, Numpad, and F13–F20).
pub fn hid_usage(keycode: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u16; 10] = [18, 19, 20, 21, 23, 22, 26, 28, 25, 29];
    const FUNCTIONS: [u16; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    const HIGH_FUNCTIONS: [u16; 8] = [105, 107, 113, 106, 64, 79, 80, 90]; // F13..F20
    if let Some(index) = LETTERS.iter().position(|v| *v == keycode) {
        return Some(4 + index as u16);
    }
    if let Some(index) = DIGITS.iter().position(|v| *v == keycode) {
        return Some(0x1e + index as u16);
    }
    if let Some(index) = FUNCTIONS.iter().position(|v| *v == keycode) {
        return Some(0x3a + index as u16);
    }
    if let Some(index) = HIGH_FUNCTIONS.iter().position(|v| *v == keycode) {
        return Some(0x68 + index as u16);
    }
    Some(match keycode {
        36 => 0x28,
        53 => 0x29,
        51 => 0x2a,
        48 => 0x2b,
        49 => 0x2c,
        27 => 0x2d,
        24 => 0x2e,
        33 => 0x2f,
        30 => 0x30,
        42 => 0x31,
        41 => 0x33,
        39 => 0x34,
        50 => 0x35,
        43 => 0x36,
        47 => 0x37,
        44 => 0x38,
        57 => crate::keymap::HID_CAPS_LOCK,
        114 => 0x49,
        115 => 0x4a,
        116 => 0x4b,
        117 => 0x4c,
        119 => 0x4d,
        121 => 0x4e,
        124 => 0x4f,
        123 => 0x50,
        125 => 0x51,
        126 => 0x52,
        71 => 0x53,
        75 => 0x54,
        67 => 0x55,
        78 => 0x56,
        69 => 0x57,
        76 => 0x58,
        83 => 0x59,
        84 => 0x5a,
        85 => 0x5b,
        86 => 0x5c,
        87 => 0x5d,
        88 => 0x5e,
        89 => 0x5f,
        91 => 0x60,
        92 => 0x61,
        82 => 0x62,
        65 => 0x63,
        10 => 0x64,
        81 => 0x67,
        63 => crate::keymap::HID_FN_GLOBE,
        59 => 0xe0,
        56 => 0xe1,
        58 => 0xe2,
        55 => 0xe3,
        62 => 0xe4,
        60 => 0xe5,
        61 => 0xe6,
        54 => 0xe7,
        _ => return None,
    })
}
#[cfg(target_os = "macos")]
fn button(number: usize) -> Option<Button> {
    [
        Button::Left,
        Button::Right,
        Button::Middle,
        Button::Back,
        Button::Forward,
    ]
    .get(number)
    .copied()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum SysDefinedMedia {
    Key {
        key_code: u16,
        usage: u16,
        down: bool,
    },
    SuppressRepeat,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn decode_sysdefined_fields(subtype: i16, data1: u64) -> Option<SysDefinedMedia> {
    if subtype != 8 {
        return None;
    }
    let key_code = ((data1 >> 16) & 0xFFFF) as u16;
    let key_flags = (data1 & 0xFFFF) as u16;
    let key_state = (key_flags & 0xFF00) >> 8;
    let is_repeat = (key_flags & 0x01) != 0;
    let usage = match key_code {
        0 => crate::keymap::HID_VOLUME_UP,
        1 => crate::keymap::HID_VOLUME_DOWN,
        7 => crate::keymap::HID_MUTE,
        16 => crate::keymap::HID_MEDIA_PLAY_PAUSE,
        17 | 19 => crate::keymap::HID_MEDIA_NEXT,
        18 | 20 => crate::keymap::HID_MEDIA_PREV,
        _ => return None,
    };
    if is_repeat && !matches!(key_code, 0 | 1) {
        return Some(SysDefinedMedia::SuppressRepeat);
    }
    let down = match key_state {
        0x0A => true,
        0x0B => false,
        _ => return None,
    };
    Some(SysDefinedMedia::Key {
        key_code,
        usage,
        down,
    })
}

fn physical_source(marker: i64, pid: i64) -> bool {
    marker != ORIGIN_MARKER && pid == 0
}
// Apple IOHIDFamily/IOHIDSystem/IOKit/hidsystem/IOLLEvent.h device flags.
// Reading the event's bits avoids a WindowServer query from the tap callback.
fn modifier_down(key: u16, flags: u64) -> Option<bool> {
    let mask = match key {
        59 => 1,
        56 => 2,
        60 => 4,
        55 => 8,
        54 => 16,
        58 => 32,
        61 => 64,
        62 => 8192,
        63 => 0x0080_0000, // kCGEventFlagMaskSecondaryFn (Fn / Globe)
        _ => return None,
    };
    Some(flags & mask != 0)
}
fn wheel(horizontal: f64, vertical: f64) -> Option<(f64, f64)> {
    let x = -horizontal * 120.0;
    let y = vertical * 120.0;
    if !x.is_finite() || !y.is_finite() || x.abs() > 32768.0 || y.abs() > 32768.0 {
        return None;
    }
    Some((x, y))
}

#[derive(Default)]
struct WheelRemainder {
    x: f64,
    y: f64,
}
impl WheelRemainder {
    fn take(&mut self, h: f64, v: f64) -> Option<(i32, i32)> {
        let (x, y) = wheel(h, v)?;
        self.x += x;
        self.y += y;
        let output = (self.x.trunc() as i32, self.y.trunc() as i32);
        self.x -= f64::from(output.0);
        self.y -= f64::from(output.1);
        Some(output)
    }
}

fn route_press(owned: &mut bool, down: bool, remote: bool) -> bool {
    if down {
        *owned = remote;
        remote
    } else {
        let suppress = *owned;
        *owned = false;
        suppress
    }
}
fn drain_press(owned: &mut bool, down: bool, repeat: bool) -> bool {
    if down && !repeat {
        *owned = false;
        return false;
    }
    let suppress = *owned;
    if !down {
        *owned = false;
    }
    suppress
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use crate::permission_probe::{PermissionCheckError, PermissionProbe, ProbeConfig};
    use std::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    type Ptr = *mut c_void;
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGEventTapCreate(
            location: u32,
            place: u32,
            options: u32,
            mask: u64,
            callback: extern "C" fn(Ptr, u32, Ptr, Ptr) -> Ptr,
            info: Ptr,
        ) -> Ptr;
        fn CGEventTapEnable(tap: Ptr, enable: bool);
        fn CGEventTapIsEnabled(tap: Ptr) -> bool;
        fn CGEventGetIntegerValueField(event: Ptr, field: u32) -> i64;
        fn CGEventGetDoubleValueField(event: Ptr, field: u32) -> f64;
        fn CGEventGetFlags(event: Ptr) -> u64;
        fn CGEventGetLocation(event: *const c_void) -> Point;
        fn CGEventGetTimestamp(event: Ptr) -> u64;
        fn CGEventSourceKeyState(state: i32, key: u16) -> bool;
        fn CGEventSourceButtonState(state: i32, button: u32) -> bool;
        fn CGPreflightListenEventAccess() -> bool;
        fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
        fn CGMainDisplayID() -> u32;
        fn CGDisplayHideCursor(display: u32) -> i32;
        fn CGDisplayShowCursor(display: u32) -> i32;
        fn CGCursorIsVisible() -> bool;
        fn CGSMainConnectionID() -> u32;
        fn CGSSetConnectionProperty(cid: u32, target_cid: u32, key: Ptr, value: Ptr) -> i32;
    }
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Ptr;
        fn sel_registerName(name: *const std::ffi::c_char) -> Ptr;
        fn objc_autoreleasePoolPush() -> Ptr;
        fn objc_autoreleasePoolPop(pool: Ptr);
        fn objc_msgSend();
    }
    pub(super) fn decode_sysdefined_media(event: Ptr) -> Option<SysDefinedMedia> {
        unsafe {
            let pool = objc_autoreleasePoolPush();
            let result = (|| -> Option<SysDefinedMedia> {
                let cls = objc_getClass(c"NSEvent".as_ptr());
                if cls.is_null() {
                    return None;
                }
                let sel_from: extern "C" fn(Ptr, Ptr, Ptr) -> Ptr =
                    std::mem::transmute(objc_msgSend as *const ());
                let ns_ev = sel_from(cls, sel_registerName(c"eventWithCGEvent:".as_ptr()), event);
                if ns_ev.is_null() {
                    return None;
                }
                let get_i16: extern "C" fn(Ptr, Ptr) -> i16 =
                    std::mem::transmute(objc_msgSend as *const ());
                let subtype = get_i16(ns_ev, sel_registerName(c"subtype".as_ptr()));
                let get_isize: extern "C" fn(Ptr, Ptr) -> isize =
                    std::mem::transmute(objc_msgSend as *const ());
                let data1 = get_isize(ns_ev, sel_registerName(c"data1".as_ptr())) as u64;
                decode_sysdefined_fields(subtype, data1)
            })();
            objc_autoreleasePoolPop(pool);
            result
        }
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: Ptr;
        static kCFBooleanTrue: Ptr;
        fn CFStringCreateWithCString(alloc: Ptr, c_str: *const std::ffi::c_char, encoding: u32) -> Ptr;
        fn CFMachPortCreateRunLoopSource(allocator: Ptr, port: Ptr, order: isize) -> Ptr;
        fn CFMachPortInvalidate(port: Ptr);
        fn CFRunLoopGetCurrent() -> Ptr;
        fn CFRunLoopAddSource(loop_: Ptr, source: Ptr, mode: Ptr);
        fn CFRunLoopRemoveSource(loop_: Ptr, source: Ptr, mode: Ptr);
        fn CFRunLoopRunInMode(mode: Ptr, seconds: f64, return_after_source: bool) -> i32;
        fn CFRelease(object: *const c_void);
    }
    unsafe extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    #[derive(Clone, Copy)]
    struct Clock {
        numer: u32,
        denom: u32,
    }
    impl Clock {
        fn new() -> anyhow::Result<Self> {
            let mut info = Timebase::default();
            anyhow::ensure!(
                unsafe { mach_timebase_info(&mut info) } == 0 && info.denom != 0,
                "Cannot read monotonic timebase"
            );
            Ok(Self {
                numer: info.numer,
                denom: info.denom,
            })
        }
        fn now(self) -> u64 {
            ((unsafe { mach_continuous_time() } as u128 * self.numer as u128) / self.denom as u128)
                .min(u64::MAX as u128) as u64
        }
    }
    fn permissions() -> bool {
        unsafe { AXIsProcessTrusted() != 0 && CGPreflightListenEventAccess() }
    }

    #[derive(Default)]
    struct CursorState {
        bg_enabled: bool,
        suppressed: bool,
        disassociated: bool,
        hide_count: u32,
    }

    #[derive(Default)]
    struct CursorSuppression(Mutex<CursorState>);

    impl CursorSuppression {
        fn ensure_background_cursor(state: &mut CursorState) {
            if state.bg_enabled {
                return;
            }
            // macOS silently ignores CGDisplayHideCursor from background helper
            // processes unless SetsCursorInBackground is enabled on the CGS connection.
            const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
            let key = unsafe {
                CFStringCreateWithCString(
                    std::ptr::null_mut(),
                    c"SetsCursorInBackground".as_ptr(),
                    K_CF_STRING_ENCODING_UTF8,
                )
            };
            if !key.is_null() {
                unsafe {
                    let cid = CGSMainConnectionID();
                    let _ = CGSSetConnectionProperty(cid, cid, key, kCFBooleanTrue);
                    CFRelease(key);
                }
                state.bg_enabled = true;
            }
        }

        fn set_suppressed(&self, suppress: bool) {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if suppress {
                Self::ensure_background_cursor(&mut state);
                state.suppressed = true;
                // Never call CGWarpMouseCursorPosition here: warping injects a massive
                // synthetic kCGMouseEventDeltaX/Y spike on the next move that bounces
                // remote focus straight back to Mac. Disassociation + HID tap keeps the
                // cursor completely frozen in place with pure physical deltas.
                if !state.disassociated && unsafe { CGAssociateMouseAndMouseCursorPosition(0) } == 0
                {
                    state.disassociated = true;
                }
                if state.hide_count == 0
                    && unsafe { CGDisplayHideCursor(CGMainDisplayID()) } == 0
                {
                    state.hide_count = 1;
                }
            } else {
                state.suppressed = false;
                while state.hide_count > 0 {
                    let _ = unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
                    state.hide_count -= 1;
                }
                if state.disassociated {
                    let _ = unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
                    state.disassociated = false;
                }
            }
        }

        /// Re-assert cursor hiding & disassociation (without ever warping!) if macOS
        /// WindowServer un-hid the cursor while remote focus is active.
        fn enforce_hidden(&self) {
            let Ok(mut state) = self.0.try_lock() else {
                return;
            };
            if !state.suppressed {
                return;
            }
            if state.hide_count < 4 && unsafe { CGCursorIsVisible() } {
                Self::ensure_background_cursor(&mut state);
                let _ = unsafe { CGAssociateMouseAndMouseCursorPosition(0) };
                state.disassociated = true;
                if unsafe { CGDisplayHideCursor(CGMainDisplayID()) } == 0 {
                    state.hide_count += 1;
                }
            }
        }
    }

    pub struct MacCapture {
        shared: Arc<Shared>,
        cursor: Arc<CursorSuppression>,
        worker: Option<JoinHandle<()>>,
        clock: Clock,
        pub initial_held: HeldSnapshot,
    }
    impl MacCapture {
        pub fn start() -> anyhow::Result<Self> {
            anyhow::ensure!(
                permissions(),
                "Mac capture requires Accessibility and Input Monitoring permissions"
            );
            let clock = Clock::new()?;
            let shared = Arc::new(Shared::new());
            let cursor = Arc::new(CursorSuppression::default());
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);
            let state = Arc::clone(&shared);
            let worker_cursor = Arc::clone(&cursor);
            let worker = thread::Builder::new()
                .name("crossflow-mac-input".into())
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        run(
                            Arc::clone(&state),
                            Arc::clone(&worker_cursor),
                            clock,
                            ready_tx,
                        )
                    }));
                    worker_cursor.set_suppressed(false);
                    if result.is_err() {
                        state.stop(StopReason::CallbackPanicked);
                    }
                })?;
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(initial_held)) => Ok(Self {
                    shared,
                    cursor,
                    worker: Some(worker),
                    clock,
                    initial_held,
                }),
                result => {
                    shared.stop(StopReason::SetupFailed);
                    cursor.set_suppressed(false);
                    // Do not block the caller forever on a stalled OS permission RPC.
                    if worker.is_finished() {
                        let _ = worker.join();
                    }
                    match result {
                        Ok(Err(e)) => Err(anyhow::anyhow!(e)),
                        _ => Err(anyhow::anyhow!(
                            "Mac event tap did not start within five seconds"
                        )),
                    }
                }
            }
        }
        /// Must be called on the sole consumer. After a stop no queued event may
        /// be replayed, even if it was captured before the failure.
        pub fn try_next(&mut self) -> Option<CapturedEvent> {
            if self.stop_reason().is_some() {
                return None;
            }
            let event = unsafe { self.shared.queue.pop() };
            // A producer failure during the pop invalidates even older events.
            if self.stop_reason().is_some() {
                None
            } else {
                event
            }
        }
        /// Place the cursor once after returning focus locally. This is an OS
        /// cursor warp, not an injected movement event. Failure stops capture;
        /// the integration must release remote ownership and must not replay it.
        pub fn warp_local(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
            let result = super::super::macos::warp_local_cursor(x, y, || {
                anyhow::ensure!(self.stop_reason().is_none(), "Mac capture is stopped");
                anyhow::ensure!(
                    self.shared.remote_until.load(Ordering::SeqCst) == 0,
                    "Return focus locally before cursor placement"
                );
                Ok(())
            });
            if result.is_err() {
                self.shared.stop(StopReason::UnsupportedInput);
            }
            result
        }
        /// False means focus was rejected (held local input, expired lease, stop).
        pub fn set_remote_focus(&self, remote: bool) -> bool {
            self.set_remote_focus_inner(remote, false)
        }
        /// Like `set_remote_focus`, but permits immediate QuickJump focus handoff
        /// while modifier/prefix keys are still physically held (`route_press` keeps
        /// pre-held local releases routed back to local macOS without stranding them).
        pub fn set_remote_focus_allow_held(&self, remote: bool) -> bool {
            self.set_remote_focus_inner(remote, true)
        }
        fn set_remote_focus_inner(&self, remote: bool, allow_held: bool) -> bool {
            let mut accepted = if allow_held {
                self.shared.focus_allow_held(remote, self.clock.now())
            } else {
                self.shared.focus(remote, self.clock.now())
            };
            if remote && !accepted && self.shared.stop.load(Ordering::SeqCst) == 0 {
                for _ in 0..32 {
                    std::hint::spin_loop();
                    accepted = if allow_held {
                        self.shared.focus_allow_held(true, self.clock.now())
                    } else {
                        self.shared.focus(true, self.clock.now())
                    };
                    if accepted || self.shared.stop.load(Ordering::SeqCst) != 0 {
                        break;
                    }
                }
                // Self-heal stale `held` count (e.g. if macOS Secure Input, Spotlight,
                // or Cmd+Tab swallowed a key-up) after confirming hardware has nothing held.
                if !allow_held
                    && !accepted
                    && self.shared.stop.load(Ordering::SeqCst) == 0
                    && self.shared.held.load(Ordering::SeqCst) != 0
                    && !any_hardware_input_held()
                {
                    self.shared.held.store(0, Ordering::SeqCst);
                    accepted = self.shared.focus(true, self.clock.now());
                }
            }
            if !remote {
                self.cursor.set_suppressed(false);
            } else if accepted {
                self.cursor.set_suppressed(true);
            }
            accepted
        }
        pub fn stop_reason(&self) -> Option<StopReason> {
            self.shared.remote(self.clock.now());
            reason(self.shared.stop.load(Ordering::SeqCst))
        }
        pub fn activity_count(&self) -> u64 {
            self.shared.activity.load(Ordering::Acquire)
        }
        /// Aggregate provenance only; never records keycodes, text or event data.
        pub fn source_counts(&self) -> SourceCounts {
            SourceCounts {
                physical_candidate: self.shared.physical_candidate.load(Ordering::Relaxed),
                own_injected: self.shared.own_injected.load(Ordering::Relaxed),
                other_process: self.shared.other_process.load(Ordering::Relaxed),
            }
        }
        pub fn stop(&self) {
            self.shared.stop(StopReason::Requested);
            self.cursor.set_suppressed(false);
        }
    }
    impl Drop for MacCapture {
        fn drop(&mut self) {
            self.stop();
            if let Some(worker) = self.worker.take() {
                // The callback consults stop before suppression even if the OS
                // stalls the run-loop owner's permission check. Avoid a UI hang.
                if worker.is_finished() {
                    let _ = worker.join();
                }
            }
        }
    }
    fn any_hardware_input_held() -> bool {
        for key in 0..128u16 {
            if key == 57 {
                // Caps Lock is a toggle lock on macOS, not a held key.
                continue;
            }
            if hid_usage(key).is_some() && unsafe { CGEventSourceKeyState(1, key) } {
                return true;
            }
        }
        for number in 0..5u32 {
            if unsafe { CGEventSourceButtonState(1, number) } {
                return true;
            }
        }
        false
    }

    struct Context {
        shared: Arc<Shared>,
        cursor: Arc<CursorSuppression>,
        clock: Clock,
        tap_port: Ptr,
        media_tap_port: Ptr,
        keys: [bool; 128],
        buttons: [bool; 5],
        remote_keys: [bool; 128],
        remote_buttons: [bool; 5],
        remote_media_keys: u32,
        hotkey_suppressed: [bool; 128],
        hotkey_suppressed_buttons: [bool; 5],
        wheel: WheelRemainder,
        permission: Option<PermissionProbe>,
    }
    // Callback state lives on the run-loop thread until the tap is invalidated.
    extern "C" fn callback(_proxy: Ptr, kind: u32, event: Ptr, info: Ptr) -> Ptr {
        if info.is_null() {
            return event;
        }
        let state = unsafe { &mut *info.cast::<Context>() };
        match catch_unwind(AssertUnwindSafe(|| handle(state, kind, event))) {
            Ok(suppress) if suppress => std::ptr::null_mut(),
            Ok(_) => event,
            Err(_) => {
                state.shared.stop(StopReason::CallbackPanicked);
                event
            }
        }
    }
    fn handle(state: &mut Context, kind: u32, event: Ptr) -> bool {
        if kind == u32::MAX || kind == u32::MAX - 1 {
            // macOS sends kCGEventTapDisabledByTimeout (u32::MAX - 1) on transient
            // scheduler or WindowServer stalls. Re-enable immediately per Apple docs;
            // only stop if the primary tap refuses to re-enable (e.g. revoked permission).
            if !state.media_tap_port.is_null() && state.shared.stop.load(Ordering::SeqCst) == 0 {
                unsafe { CGEventTapEnable(state.media_tap_port, true) };
            }
            if !state.tap_port.is_null() && state.shared.stop.load(Ordering::SeqCst) == 0 {
                unsafe { CGEventTapEnable(state.tap_port, true) };
                if unsafe { CGEventTapIsEnabled(state.tap_port) } {
                    return false;
                }
            }
            state.shared.stop(StopReason::TapDisabled);
            return false;
        }
        if event.is_null() {
            return false;
        }
        if state
            .permission
            .as_ref()
            .is_none_or(|probe| probe.check().is_err())
        {
            state.shared.stop(StopReason::PermissionsLost);
        }
        let marker = unsafe { CGEventGetIntegerValueField(event, 42) };
        let pid = unsafe { CGEventGetIntegerValueField(event, 41) };
        let accepted_source = if kind == 14 {
            // NX_SYSDEFINED media/volume events (Apple Silicon function row, Control Strip,
            // keyboard helpers) may carry a non-zero system/helper PID; only reject our own
            // marker or our own PID to prevent feedback loops.
            marker != ORIGIN_MARKER && pid != i64::from(std::process::id())
        } else {
            physical_source(marker, pid)
        };
        if !accepted_source {
            if marker == ORIGIN_MARKER {
                state.shared.own_injected.fetch_add(1, Ordering::Relaxed);
            } else {
                state.shared.other_process.fetch_add(1, Ordering::Relaxed);
            }
            return false;
        }
        state
            .shared
            .physical_candidate
            .fetch_add(1, Ordering::Relaxed);
        let shared = Arc::clone(&state.shared);
        let mut guard = shared.try_route();
        if guard.is_none() {
            for _ in 0..64 {
                std::hint::spin_loop();
                guard = shared.try_route();
                if guard.is_some() {
                    break;
                }
            }
        }
        let Some(_routing) = guard else {
            // A concurrent focus handoff must not split a local press/release.
            // Fail open for fresh input, preserving suppressed-release ownership.
            shared.stop(StopReason::FocusRace);
            return drain_release(state, kind, event);
        };
        let now = state.clock.now();
        let remote = state.shared.remote(now);
        if state.shared.stop.load(Ordering::SeqCst) != 0 {
            return drain_release(state, kind, event);
        }
        let raw_point = unsafe { CGEventGetLocation(event) };
        let point = if raw_point.x.is_finite() && raw_point.y.is_finite() {
            raw_point
        } else if kind == 14 {
            Point::default()
        } else {
            state.shared.stop(StopReason::UnsupportedInput);
            return drain_release(state, kind, event);
        };
        let flags = unsafe { CGEventGetFlags(event) };
        // Reconcile stale modifier bits only while local (when remote is active,
        // suppressed FlagsChanged events may not update WindowServer's modifier mask,
        // while our HID event tap observes every physical key-down/up directly).
        if !remote {
            for &mod_key in &[59usize, 56, 60, 55, 54, 58, 61, 62, 63] {
                if state.keys[mod_key] && modifier_down(mod_key as u16, flags) == Some(false) {
                    update_held(&state.shared, &mut state.keys[mod_key], false);
                    state.remote_keys[mod_key] = false;
                    if let Some(u) = hid_usage(mod_key as u16) {
                        crate::hotkeys::active_observe_key(u, false);
                    }
                }
            }
        }
        let mut suppress = remote;
        let captured_kind = match kind {
            5 | 6 | 7 | 27 => {
                if kind == 5 {
                    // Plain MouseMoved (not Dragged): no mouse button is physically held.
                    for b in 0..5 {
                        if state.buttons[b] {
                            update_held(&state.shared, &mut state.buttons[b], false);
                            state.remote_buttons[b] = false;
                            if let Some(btn) = button(b) {
                                crate::hotkeys::active_observe_button(btn, false);
                            }
                        }
                    }
                }
                if remote {
                    // Re-hide if WindowServer un-hid the cursor, without ever warping.
                    state.cursor.enforce_hidden();
                }
                let dx = unsafe { CGEventGetDoubleValueField(event, 4) };
                let dy = unsafe { CGEventGetDoubleValueField(event, 5) };
                if !dx.is_finite() || !dy.is_finite() || dx.abs() > 20000.0 || dy.abs() > 20000.0 {
                    state.shared.stop(StopReason::UnsupportedInput);
                    return false;
                }
                if dx == 0.0 && dy == 0.0 {
                    return remote;
                }
                CaptureKind::Delta { dx, dy }
            }
            1 | 2 | 3 | 4 | 25 | 26 => {
                let number = unsafe { CGEventGetIntegerValueField(event, 3) } as usize;
                let Some(button) = button(number) else {
                    if remote {
                        state.shared.stop(StopReason::UnsupportedInput);
                    }
                    return false;
                };
                let down = matches!(kind, 1 | 3 | 25);
                if crate::hotkeys::has_active_engine() {
                    if state.hotkey_suppressed_buttons[number] {
                        if !down {
                            state.hotkey_suppressed_buttons[number] = false;
                        }
                        return true;
                    }
                    if crate::hotkeys::active_observe_button(button, down).is_some() {
                        state.hotkey_suppressed_buttons[number] = true;
                        return true;
                    }
                }
                update_held(&state.shared, &mut state.buttons[number], down);
                suppress = route_press(&mut state.remote_buttons[number], down, remote);
                CaptureKind::Button { button, down }
            }
            10..=12 => {
                let key = unsafe { CGEventGetIntegerValueField(event, 9) };
                if !(0..128).contains(&key) {
                    if !remote && (kind == 10 || kind == 12) {
                        state.shared.activity.fetch_add(1, Ordering::Release);
                    }
                    return false;
                }
                // Caps Lock on macOS fires a single FlagsChanged event (toggle semantics, no
                // guaranteed release). Never count it in `held`; when remote, forward a clean tap.
                if key == 57 {
                    state.shared.activity.fetch_add(1, Ordering::Release);
                    if !remote {
                        return false;
                    }
                    let ts = unsafe { CGEventGetTimestamp(event) };
                    let captured_at = Instant::now();
                    for tap_down in [true, false] {
                        let ev = CapturedEvent {
                            kind: CaptureKind::Key {
                                usage: crate::keymap::HID_CAPS_LOCK,
                                down: tap_down,
                            },
                            captured_at,
                            native_x: point.x,
                            native_y: point.y,
                            native_timestamp_ns: ts,
                            modifiers: flags,
                            remote_focus: true,
                        };
                        if !unsafe { state.shared.queue.push(ev) } {
                            state.shared.stop(StopReason::QueueOverflow);
                            break;
                        }
                    }
                    return true;
                }
                let index = key as usize;
                let down = if kind == 12 {
                    modifier_down(key as u16, flags).unwrap_or(false)
                } else {
                    kind == 10
                };
                let Some(usage) = hid_usage(key as u16) else {
                    if !remote && (kind == 10 || kind == 12) {
                        state.shared.activity.fetch_add(1, Ordering::Release);
                    }
                    return false;
                };
                let is_repeat = kind == 10 && unsafe { CGEventGetIntegerValueField(event, 8) } != 0;
                if crate::hotkeys::has_active_engine() {
                    if state.hotkey_suppressed[index] {
                        if !down {
                            state.hotkey_suppressed[index] = false;
                        }
                        return true;
                    }
                    if !is_repeat && crate::hotkeys::active_observe_key(usage, down).is_some() {
                        state.hotkey_suppressed[index] = true;
                        return true;
                    }
                } else if key == 53 && down && flags & 0x40000 != 0 && flags & 0x80000 != 0 {
                    state.shared.stop(StopReason::EmergencyShortcut);
                    return false;
                }
                if is_repeat {
                    if remote
                        && state.remote_keys[index]
                        && crate::input::is_repeatable_key(usage)
                    {
                        suppress = true;
                        CaptureKind::Key { usage, down: true }
                    } else {
                        return state.remote_keys[index];
                    }
                } else {
                    update_held(&state.shared, &mut state.keys[index], down);
                    suppress = route_press(&mut state.remote_keys[index], down, remote);
                    CaptureKind::Key { usage, down }
                }
            }
            14 => {
                // NX_SYSDEFINED media keys (Volume Up/Down, Mute, Play/Pause, Next/Prev)
                match decode_sysdefined_media(event) {
                    Some(SysDefinedMedia::Key {
                        key_code,
                        usage,
                        down,
                    }) => {
                        state.shared.activity.fetch_add(1, Ordering::Release);
                        let bit = if key_code < 32 { 1u32 << key_code } else { 0 };
                        if down {
                            if !remote {
                                return false;
                            }
                            state.remote_media_keys |= bit;
                            suppress = true;
                        } else {
                            let was_remote = (state.remote_media_keys & bit) != 0;
                            state.remote_media_keys &= !bit;
                            if !remote && !was_remote {
                                return false;
                            }
                            suppress = true;
                            if !remote {
                                return true;
                            }
                        }
                        CaptureKind::Key { usage, down }
                    }
                    Some(SysDefinedMedia::SuppressRepeat) => {
                        return remote;
                    }
                    None => return false,
                }
            }
            22 => {
                // Fixed-point line deltas retain trackpad/sub-notch precision;
                // do not label this a raw wheel/HID measurement.
                let h = unsafe { CGEventGetDoubleValueField(event, 94) };
                let v = unsafe { CGEventGetDoubleValueField(event, 93) };
                let Some((horizontal, vertical)) = state.wheel.take(h, v) else {
                    state.shared.stop(StopReason::UnsupportedInput);
                    return false;
                };
                if horizontal == 0 && vertical == 0 {
                    return remote;
                }
                CaptureKind::Scroll {
                    horizontal,
                    vertical,
                }
            }
            _ => return false,
        };
        let value = CapturedEvent {
            kind: captured_kind,
            captured_at: Instant::now(),
            native_x: point.x,
            native_y: point.y,
            native_timestamp_ns: unsafe { CGEventGetTimestamp(event) },
            modifiers: flags,
            remote_focus: remote,
        };
        state.shared.activity.fetch_add(1, Ordering::Release);
        if !unsafe { state.shared.queue.push(value) } {
            state.shared.stop(StopReason::QueueOverflow);
            // A release for an already suppressed down must still be consumed.
            if matches!(
                captured_kind,
                CaptureKind::Key { down: false, .. } | CaptureKind::Button { down: false, .. }
            ) {
                return suppress;
            }
            // This fresh down will be delivered locally: do not swallow its up.
            match captured_kind {
                CaptureKind::Key { down: true, .. } if kind != 14 => {
                    let key = unsafe { CGEventGetIntegerValueField(event, 9) } as usize;
                    if key < state.remote_keys.len() {
                        state.remote_keys[key] = false;
                    }
                }
                CaptureKind::Button { down: true, .. } => {
                    let key = unsafe { CGEventGetIntegerValueField(event, 3) } as usize;
                    if key < state.remote_buttons.len() {
                        state.remote_buttons[key] = false;
                    }
                }
                _ => {}
            }
            return false;
        }
        suppress
    }
    /// After an ordinary stop, only finish the ownership of suppressed presses.
    /// Fresh input goes local immediately. If macOS disables the tap/revokes its
    /// permission it cannot intercept any event; that OS boundary is unavoidable.
    fn drain_release(state: &mut Context, kind: u32, event: Ptr) -> bool {
        match kind {
            10..=12 => {
                let key = unsafe { CGEventGetIntegerValueField(event, 9) };
                if !(0..128).contains(&key) {
                    return false;
                }
                let key = key as usize;
                let down = if kind == 12 {
                    modifier_down(key as u16, unsafe { CGEventGetFlags(event) }).unwrap_or(false)
                } else {
                    kind == 10
                };
                let repeat = kind == 10 && unsafe { CGEventGetIntegerValueField(event, 8) } != 0;
                drain_press(&mut state.remote_keys[key], down, repeat)
            }
            1 | 3 | 25 => {
                let key = unsafe { CGEventGetIntegerValueField(event, 3) } as usize;
                if key < 5 {
                    state.remote_buttons[key] = false;
                }
                false
            }
            2 | 4 | 26 => {
                let key = unsafe { CGEventGetIntegerValueField(event, 3) } as usize;
                if key >= 5 {
                    return false;
                }
                let owned = state.remote_buttons[key];
                state.remote_buttons[key] = false;
                owned
            }
            _ => false,
        }
    }
    fn update_held(shared: &Shared, current: &mut bool, down: bool) {
        if *current != down {
            *current = down;
            if down {
                shared.held.fetch_add(1, Ordering::SeqCst);
            } else {
                shared.held.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
    struct Tap {
        port: Ptr,
        source: Ptr,
        loop_: Ptr,
    }
    impl Drop for Tap {
        fn drop(&mut self) {
            unsafe {
                CGEventTapEnable(self.port, false);
                CFMachPortInvalidate(self.port);
                if !self.source.is_null() {
                    CFRunLoopRemoveSource(self.loop_, self.source, kCFRunLoopDefaultMode);
                    CFRelease(self.source);
                }
                CFRelease(self.port);
            }
        }
    }
    fn run(
        shared: Arc<Shared>,
        cursor: Arc<CursorSuppression>,
        clock: Clock,
        ready: mpsc::SyncSender<Result<HeldSnapshot, String>>,
    ) {
        let mut state = Box::new(Context {
            shared: Arc::clone(&shared),
            cursor: Arc::clone(&cursor),
            clock,
            tap_port: std::ptr::null_mut(),
            media_tap_port: std::ptr::null_mut(),
            keys: [false; 128],
            buttons: [false; 5],
            remote_keys: [false; 128],
            remote_buttons: [false; 5],
            remote_media_keys: 0,
            hotkey_suppressed: [false; 128],
            hotkey_suppressed_buttons: [false; 5],
            wheel: WheelRemainder::default(),
            permission: None,
        });
        let mask = [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14, 22, 25, 26, 27]
            .iter()
            .fold(0u64, |m, t| m | (1u64 << t));
        // Prefer kCGHIDEventTap (0) so suppressed mouse/trackpad events are swallowed
        // BEFORE WindowServer updates the local cursor; fall back to kCGSessionEventTap (1)
        // if HID tap is unavailable under the current permissions.
        let hid_port = unsafe {
            CGEventTapCreate(
                0,
                0,
                0,
                mask,
                callback,
                (&mut *state as *mut Context).cast(),
            )
        };
        let port = if !hid_port.is_null() {
            hid_port
        } else {
            unsafe {
                CGEventTapCreate(
                    1,
                    0,
                    0,
                    mask,
                    callback,
                    (&mut *state as *mut Context).cast(),
                )
            }
        };
        if port.is_null() {
            shared.stop(StopReason::SetupFailed);
            let _ = ready.send(Err(
                "Cannot create Mac event tap; check Accessibility/Input Monitoring".into(),
            ));
            return;
        }
        state.tap_port = port;
        // Keep the new tap disabled while startup permission/key queries run so
        // WindowServer cannot time out an un-pumped tap before CFRunLoop starts.
        unsafe { CGEventTapEnable(port, false) };
        let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null_mut(), port, 0) };
        let loop_ = unsafe { CFRunLoopGetCurrent() };
        let tap = Tap {
            port,
            source,
            loop_,
        };
        if source.is_null() {
            shared.stop(StopReason::SetupFailed);
            let _ = ready.send(Err("Cannot create Mac event run-loop source".into()));
            return;
        }
        // On Apple Silicon and modern macOS, NX_SYSDEFINED media/volume keys (type 14)
        // from the built-in function row / Control Strip often enter at kCGSessionEventTap
        // rather than kCGHIDEventTap. Attach a lightweight session-level tap for type 14
        // whenever the primary tap is kCGHIDEventTap so volume/media keys are intercepted
        // and suppressed cleanly before reaching BezelServices/coreaudiod.
        let media_tap = if !hid_port.is_null() {
            let media_port = unsafe {
                CGEventTapCreate(
                    1,
                    0,
                    0,
                    1u64 << 14,
                    callback,
                    (&mut *state as *mut Context).cast(),
                )
            };
            if !media_port.is_null() {
                unsafe { CGEventTapEnable(media_port, false) };
                let media_source =
                    unsafe { CFMachPortCreateRunLoopSource(std::ptr::null_mut(), media_port, 0) };
                if !media_source.is_null() {
                    state.media_tap_port = media_port;
                    Some(Tap {
                        port: media_port,
                        source: media_source,
                        loop_,
                    })
                } else {
                    unsafe {
                        CFMachPortInvalidate(media_port);
                        CFRelease(media_port);
                    }
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        // Startup queries are allowed before dispatch starts. Once the tap is
        // active, only the separate shared permission-probe worker makes RPCs.
        if !permissions() {
            shared.stop(StopReason::PermissionsLost);
            let _ = ready.send(Err("Mac capture permissions changed during startup".into()));
            return;
        }
        match PermissionProbe::start_after_success(
            || {
                if permissions() {
                    Ok(())
                } else {
                    Err(PermissionCheckError::Denied)
                }
            },
            ProbeConfig::default(),
        ) {
            Ok(probe) => state.permission = Some(probe),
            Err(error) => {
                shared.stop(StopReason::SetupFailed);
                let _ = ready.send(Err(error.to_string()));
                return;
            }
        }
        // Sample currently held input BEFORE enabling the tap so WindowServer
        // never sends events to an enabled tap whose run loop is not pumping yet.
        let mut snapshot = HeldSnapshot::default();
        for key in 0..128 {
            if key == 57 {
                // Caps Lock is a toggle lock on macOS, never a held physical key.
                continue;
            }
            let Some(usage) = hid_usage(key as u16) else {
                continue;
            };
            state.keys[key] = unsafe { CGEventSourceKeyState(1, key as u16) };
            if state.keys[key] {
                shared.held.fetch_add(1, Ordering::SeqCst);
                snapshot.keys.push(usage);
            }
        }
        for number in 0..5 {
            state.buttons[number] = unsafe { CGEventSourceButtonState(1, number as u32) };
            if state.buttons[number] {
                shared.held.fetch_add(1, Ordering::SeqCst);
                snapshot.buttons.push(button(number).unwrap());
            }
        }
        let ready_now = clock.now();
        shared.loop_tick.store(ready_now, Ordering::Release);
        unsafe {
            CFRunLoopAddSource(loop_, source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
            if let Some(ref mt) = media_tap {
                CFRunLoopAddSource(loop_, mt.source, kCFRunLoopDefaultMode);
                CGEventTapEnable(mt.port, true);
            }
        }
        shared.remote(ready_now);
        if state
            .permission
            .as_ref()
            .is_none_or(|probe| probe.check().is_err())
        {
            shared.stop(StopReason::PermissionsLost);
        }
        if !unsafe { CGEventTapIsEnabled(port) } {
            unsafe { CGEventTapEnable(port, true) };
            if !unsafe { CGEventTapIsEnabled(port) } {
                shared.stop(StopReason::TapDisabled);
            }
        }
        if shared.stop.load(Ordering::SeqCst) != 0 {
            let _ = ready.send(Err(
                "Mac capture stopped during initial input sampling".into()
            ));
            return;
        }
        if ready.send(Ok(snapshot)).is_err() {
            shared.stop(StopReason::Requested);
        }
        loop {
            let now = clock.now();
            if !shared.remote(now) {
                // Restore local cursor visibility and association promptly if
                // focus lease expires or capture stops while the session thread stalls.
                cursor.set_suppressed(false);
            }
            if state
                .permission
                .as_ref()
                .is_none_or(|probe| probe.check().is_err())
            {
                shared.stop(StopReason::PermissionsLost);
            }
            if shared.stop.load(Ordering::SeqCst) != 0 {
                // Stop read-only checks; retain only release ownership cleanup.
                cursor.set_suppressed(false);
                state.permission.take();
                if !state.remote_keys.iter().any(|v| *v) && !state.remote_buttons.iter().any(|v| *v)
                {
                    break;
                }
                if !unsafe { CGEventTapIsEnabled(port) } {
                    break;
                }
                unsafe {
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.005, true);
                }
                continue;
            }
            shared.loop_tick.store(now, Ordering::Release);
            if !unsafe { CGEventTapIsEnabled(port) } {
                unsafe { CGEventTapEnable(port, true) };
                if !unsafe { CGEventTapIsEnabled(port) } {
                    shared.stop(StopReason::TapDisabled);
                    break;
                }
            }
            if let Some(ref mt) = media_tap
                && !unsafe { CGEventTapIsEnabled(mt.port) }
            {
                unsafe { CGEventTapEnable(mt.port, true) };
            }
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.005, true);
            }
        }
        drop(media_tap);
        drop(tap); // Invalidate before freeing callback context.
        cursor.set_suppressed(false);
    }
}
#[cfg(target_os = "macos")]
pub use native::MacCapture;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping_is_unique_and_modifiers_preserve_side() {
        let mut values = std::collections::BTreeSet::new();
        for key in 0..128 {
            if let Some(usage) = hid_usage(key) {
                assert!(crate::input::is_keyboard_usage(usage));
                assert!(values.insert(usage));
            }
        }
        assert_eq!(hid_usage(0), Some(4));
        assert_eq!(hid_usage(62), Some(0xe4));
        assert_eq!(hid_usage(59), Some(0xe0));
        assert_eq!(hid_usage(57), Some(crate::keymap::HID_CAPS_LOCK));
        assert_eq!(hid_usage(63), Some(crate::keymap::HID_FN_GLOBE));
        assert_eq!(hid_usage(82), Some(0x62)); // Keypad 0
        assert_eq!(hid_usage(127), None);
    }
    #[test]
    fn own_and_foreign_injection_are_not_activity() {
        assert!(!physical_source(ORIGIN_MARKER, 0));
        assert!(!physical_source(0, 123));
        assert!(physical_source(0, 0));
    }
    #[test]
    fn bounded_ring_preserves_order_and_refuses_overwrite() {
        let q = Ring::new();
        for i in 0..CAPACITY {
            assert!(unsafe { q.push(i) });
        }
        assert!(!unsafe { q.push(9999) });
        for i in 0..CAPACITY {
            assert_eq!(unsafe { q.pop() }, Some(i));
        }
        assert!(unsafe { q.pop() }.is_none());
        assert!(unsafe { q.push(7) });
        assert_eq!(unsafe { q.pop() }, Some(7));
    }
    #[test]
    fn spsc_publication_survives_concurrent_wraparound() {
        let queue = std::sync::Arc::new(Ring::new());
        let producer = queue.clone();
        let worker = std::thread::spawn(move || {
            for value in 0..50_000 {
                while !unsafe { producer.push(value) } {
                    std::thread::yield_now();
                }
            }
        });
        for expected in 0..50_000 {
            loop {
                if let Some(value) = unsafe { queue.pop() } {
                    assert_eq!(value, expected);
                    break;
                }
                std::thread::yield_now();
            }
        }
        worker.join().unwrap();
    }
    #[test]
    fn remote_lease_cannot_restart_after_expiry_or_stop() {
        let s = Shared::new();
        s.loop_tick.store(1, Ordering::Relaxed);
        assert!(s.focus(true, 1));
        assert!(s.remote(2));
        assert!(!s.remote(LEASE_NS + 1));
        assert_eq!(
            reason(s.stop.load(Ordering::Relaxed)),
            Some(StopReason::FocusLeaseExpired)
        );
        assert!(!s.focus(true, LEASE_NS + 2));
        assert!(s.focus(false, LEASE_NS + 3));
    }
    #[test]
    fn locally_held_key_blocks_focus_and_stall_fails_open() {
        let s = Shared::new();
        s.held.store(1, Ordering::Relaxed);
        assert!(!s.focus(true, 1));
        s.held.store(0, Ordering::Relaxed);
        assert!(s.focus(true, 1));
        assert!(!s.remote(STALL_NS + 1));
        assert_eq!(
            reason(s.stop.load(Ordering::Relaxed)),
            Some(StopReason::SleepOrStall)
        );
    }
    #[test]
    fn in_flight_local_press_cannot_race_remote_focus() {
        let s = Shared::new();
        let callback = s.try_route().unwrap();
        assert!(!s.focus(true, 1));
        assert_eq!(s.remote_until.load(Ordering::SeqCst), 0);
        // The callback delivers this press locally before publishing its state.
        s.held.store(1, Ordering::SeqCst);
        drop(callback);
        assert!(!s.focus(true, 2));
        s.held.store(0, Ordering::SeqCst);
        assert!(s.focus(true, 3));
    }
    #[test]
    fn focus_after_capture_stall_cannot_start_a_new_lease() {
        let s = Shared::new();
        assert!(!s.focus(true, STALL_NS + 1));
        assert_eq!(
            reason(s.stop.load(Ordering::SeqCst)),
            Some(StopReason::SleepOrStall)
        );
        assert_eq!(s.remote_until.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn scroll_units_and_bounds() {
        assert_eq!(wheel(1.0, 0.5), Some((-120.0, 60.0)));
        assert_eq!(wheel(f64::NAN, 1.0), None);
        assert_eq!(wheel(1000.0, 0.0), None);
    }
    #[test]
    fn modifier_flags_distinguish_both_sides_without_system_query() {
        assert_eq!(modifier_down(59, 0x40000 | 8192), Some(false));
        assert_eq!(modifier_down(62, 0x40000 | 8192), Some(true));
        assert_eq!(modifier_down(56, 0x20000 | 2 | 4), Some(true));
        assert_eq!(modifier_down(60, 0x20000 | 2), Some(false));
    }
    #[test]
    fn ownership_survives_focus_change_and_stop() {
        let mut owned = false;
        assert!(!route_press(&mut owned, true, false));
        assert!(!route_press(&mut owned, false, true)); // local down has local up
        assert!(route_press(&mut owned, true, true));
        assert!(route_press(&mut owned, false, false)); // remote down still consumes up
        assert!(route_press(&mut owned, true, true));
        assert!(drain_press(&mut owned, true, true)); // repeat after stop
        assert!(drain_press(&mut owned, false, false));
        assert!(!drain_press(&mut owned, true, false));
        assert!(!drain_press(&mut owned, false, false));
    }
    #[test]
    fn fractional_wheel_motion_is_retained() {
        let mut wheel = WheelRemainder::default();
        let total: i32 = (0..10).map(|_| wheel.take(0.0, 0.005).unwrap().1).sum();
        assert!((f64::from(total) + wheel.y - 6.0).abs() < 1e-8);
    }
    #[test]
    fn sysdefined_media_fields_decode_volume_mute_and_media_keys() {
        assert_eq!(
            decode_sysdefined_fields(8, (0u64 << 16) | (0x0A << 8)),
            Some(SysDefinedMedia::Key {
                key_code: 0,
                usage: crate::keymap::HID_VOLUME_UP,
                down: true,
            })
        );
        assert_eq!(
            decode_sysdefined_fields(8, (1u64 << 16) | (0x0A << 8) | 0x01),
            Some(SysDefinedMedia::Key {
                key_code: 1,
                usage: crate::keymap::HID_VOLUME_DOWN,
                down: true,
            })
        );
        assert_eq!(
            decode_sysdefined_fields(8, (7u64 << 16) | (0x0A << 8) | 0x01),
            Some(SysDefinedMedia::SuppressRepeat)
        );
        assert_eq!(
            decode_sysdefined_fields(8, (19u64 << 16) | (0x0B << 8)),
            Some(SysDefinedMedia::Key {
                key_code: 19,
                usage: crate::keymap::HID_MEDIA_NEXT,
                down: false,
            })
        );
        assert_eq!(
            decode_sysdefined_fields(8, (20u64 << 16) | (0x0A << 8)),
            Some(SysDefinedMedia::Key {
                key_code: 20,
                usage: crate::keymap::HID_MEDIA_PREV,
                down: true,
            })
        );
        assert_eq!(decode_sysdefined_fields(7, (0u64 << 16) | (0x0A << 8)), None);
    }
}
