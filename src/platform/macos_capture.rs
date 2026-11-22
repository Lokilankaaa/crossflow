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
        // Do not strand a key/button which was delivered to a local application.
        if until == 0 && self.held.load(Ordering::SeqCst) != 0 {
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

/// Physical position mapping, deliberately matching the existing Mac sink.
/// Caps Lock, Fn, media and keypad lock semantics remain unsupported.
pub fn hid_usage(keycode: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u16; 10] = [18, 19, 20, 21, 23, 22, 26, 28, 25, 29];
    const FUNCTIONS: [u16; 12] = [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111];
    if let Some(index) = LETTERS.iter().position(|v| *v == keycode) {
        return Some(4 + index as u16);
    }
    if let Some(index) = DIGITS.iter().position(|v| *v == keycode) {
        return Some(0x1e + index as u16);
    }
    if let Some(index) = FUNCTIONS.iter().position(|v| *v == keycode) {
        return Some(0x3a + index as u16);
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
        115 => 0x4a,
        116 => 0x4b,
        117 => 0x4c,
        119 => 0x4d,
        121 => 0x4e,
        124 => 0x4f,
        123 => 0x50,
        125 => 0x51,
        126 => 0x52,
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
    #[derive(Clone, Copy)]
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
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: Ptr;
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
        disassociated: bool,
        hidden: bool,
    }

    #[derive(Default)]
    struct CursorSuppression(Mutex<CursorState>);

    impl CursorSuppression {
        fn set_suppressed(&self, suppress: bool) {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            if suppress {
                if !state.disassociated && unsafe { CGAssociateMouseAndMouseCursorPosition(0) } == 0
                {
                    state.disassociated = true;
                }
                if !state.hidden && unsafe { CGDisplayHideCursor(CGMainDisplayID()) } == 0 {
                    state.hidden = true;
                }
            } else {
                if state.hidden {
                    let _ = unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
                    state.hidden = false;
                }
                if state.disassociated {
                    let _ = unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
                    state.disassociated = false;
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
            let accepted = self.shared.focus(remote, self.clock.now());
            self.cursor.set_suppressed(remote && accepted);
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
    struct Context {
        shared: Arc<Shared>,
        clock: Clock,
        keys: [bool; 128],
        buttons: [bool; 5],
        remote_keys: [bool; 128],
        remote_buttons: [bool; 5],
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
        if !physical_source(marker, pid) {
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
        let Some(_routing) = shared.try_route() else {
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
        let point = unsafe { CGEventGetLocation(event) };
        if !point.x.is_finite() || !point.y.is_finite() {
            state.shared.stop(StopReason::UnsupportedInput);
            return drain_release(state, kind, event);
        }
        let flags = unsafe { CGEventGetFlags(event) };
        let mut suppress = remote;
        let captured_kind = match kind {
            5 | 6 | 7 | 27 => {
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
                let index = key as usize;
                let down = if kind == 12 {
                    modifier_down(key as u16, flags).unwrap_or(false)
                } else {
                    kind == 10
                };
                if key == 53 && down && flags & 0x40000 != 0 && flags & 0x80000 != 0 {
                    state.shared.stop(StopReason::EmergencyShortcut);
                    return false;
                }
                let Some(usage) = hid_usage(key as u16) else {
                    if !remote && (kind == 10 || kind == 12) {
                        state.shared.activity.fetch_add(1, Ordering::Release);
                    }
                    return false;
                };
                if kind == 10 && unsafe { CGEventGetIntegerValueField(event, 8) } != 0 {
                    return state.remote_keys[index];
                }
                update_held(&state.shared, &mut state.keys[index], down);
                suppress = route_press(&mut state.remote_keys[index], down, remote);
                CaptureKind::Key { usage, down }
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
                CaptureKind::Key { down: true, .. } => {
                    let key = unsafe { CGEventGetIntegerValueField(event, 9) } as usize;
                    state.remote_keys[key] = false;
                }
                CaptureKind::Button { down: true, .. } => {
                    let key = unsafe { CGEventGetIntegerValueField(event, 3) } as usize;
                    state.remote_buttons[key] = false;
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
            clock,
            keys: [false; 128],
            buttons: [false; 5],
            remote_keys: [false; 128],
            remote_buttons: [false; 5],
            wheel: WheelRemainder::default(),
            permission: None,
        });
        let mask = [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 22, 25, 26, 27]
            .iter()
            .fold(0u64, |m, t| m | (1u64 << t));
        // Session/head/default permits suppression without requiring a root HID tap.
        let port = unsafe {
            CGEventTapCreate(
                1,
                0,
                0,
                mask,
                callback,
                (&mut *state as *mut Context).cast(),
            )
        };
        if port.is_null() {
            shared.stop(StopReason::SetupFailed);
            let _ = ready.send(Err(
                "Cannot create Mac event tap; check Accessibility/Input Monitoring".into(),
            ));
            return;
        }
        // Keep the new tap disabled while startup permission RPCs may block.
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
        shared.loop_tick.store(clock.now(), Ordering::Release);
        unsafe {
            CFRunLoopAddSource(loop_, source, kCFRunLoopDefaultMode);
            CGEventTapEnable(port, true);
        }
        // Enable interception BEFORE sampling held input. A press after its
        // key was sampled can now only be delivered through our callback, so
        // no local down can disappear into a snapshot-to-enable gap. The run
        // loop has not dispatched callbacks yet; this thread owns the arrays.
        let mut snapshot = HeldSnapshot::default();
        for key in 0..128 {
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
        // Refresh loop_tick after sampling 128 keys so startup latency is never
        // misclassified as a run-loop stall.
        let ready_now = clock.now();
        shared.loop_tick.store(ready_now, Ordering::Release);
        shared.remote(ready_now);
        if state
            .permission
            .as_ref()
            .is_none_or(|probe| probe.check().is_err())
        {
            shared.stop(StopReason::PermissionsLost);
        }
        if !unsafe { CGEventTapIsEnabled(port) } {
            shared.stop(StopReason::TapDisabled);
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
                shared.stop(StopReason::TapDisabled);
                break;
            }
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.005, true);
            }
        }
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
        assert_eq!(hid_usage(57), None);
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
}
