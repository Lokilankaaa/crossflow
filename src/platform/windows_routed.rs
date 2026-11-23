//! Local-delivery suppression for the routed sender. Raw Input remains the
//! capture source; these callbacks only make a bounded forwarding decision.
//! Windows removes low-level hooks on process exit. We deliberately do not use
//! ClipCursor, which is global state that could outlive a failed source loop.
//!
//! Microsoft documents that timed-out LL hooks can be silently removed. These
//! hooks perform no network, filesystem, queue locks or user callbacks. Live
//! Raw Input plus suppression and secure-desktop behavior still need attended
//! platform testing; this is not a guarantee about privileged applications.

use super::{CaptureEvent, CaptureStatus, STOP, TOGGLE, scan_to_hid, win_error};
use crate::input::Button;
use serde::Serialize;
use std::{
    cell::RefCell,
    collections::VecDeque,
    mem::size_of,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW},
    System::{
        LibraryLoader::GetModuleHandleW,
        RemoteDesktop::{
            NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification,
            WTSUnRegisterSessionNotification,
        },
    },
    UI::{
        HiDpi::{
            AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetAwarenessFromDpiAwarenessContext,
            GetThreadDpiAwarenessContext, SetThreadDpiAwarenessContext,
        },
        Input::KeyboardAndMouse::{GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MapVirtualKeyW},
        WindowsAndMessaging::{
            CallNextHookEx, GetClipCursor, GetCursorPos, GetPhysicalCursorPos, HHOOK,
            KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLKHF_LOWER_IL_INJECTED, LLKHF_UP,
            LLMHF_INJECTED, LLMHF_LOWER_IL_INJECTED, MSLLHOOKSTRUCT, PostMessageW,
            SetPhysicalCursorPos, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
            WH_MOUSE_LL, WM_APP, WM_COMMAND, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
            WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
            WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    },
};

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct NativeDisplay {
    pub id: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub fn cursor_position() -> Result<(f64, f64), String> {
    // The same per-monitor context is used by display inventory, the capture
    // window, reads and return warps. Do not infer a scale from display sizes.
    let _dpi = DpiScope::physical()?;
    let mut point = POINT { x: 0, y: 0 };
    if unsafe { GetPhysicalCursorPos(&mut point) } == 0 {
        return Err(win_error("GetPhysicalCursorPos"));
    }
    Ok((f64::from(point.x), f64::from(point.y)))
}

fn integer_coordinate(value: f64) -> Result<i32, String> {
    if !value.is_finite() || value < i32::MIN as f64 || value > i32::MAX as f64 {
        return Err("cursor coordinate must be finite and within the native range".into());
    }
    Ok(value.round() as i32)
}

/// Caller must clear remote focus first. Never accepts non-finite/out-of-range
/// coordinates or changes acceleration, confinement or cursor visibility.
pub fn warp_cursor(x: f64, y: f64) -> Result<(), String> {
    let (x, y) = (integer_coordinate(x)?, integer_coordinate(y)?);
    let _dpi = DpiScope::physical()?;
    if unsafe { SetPhysicalCursorPos(x, y) } == 0 {
        return Err(win_error("SetPhysicalCursorPos"));
    }
    Ok(())
}

pub(super) struct DpiScope(DPI_AWARENESS_CONTEXT);
impl DpiScope {
    pub(super) fn physical() -> Result<Self, String> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if previous.is_null() {
            return Err(win_error("SetThreadDpiAwarenessContext"));
        }
        Ok(Self(previous))
    }
}

/// Startup-only coordinate evidence. Cursor and clip reads use the caller's
/// current context; monitor inventory is explicitly in physical PMv2 pixels.
/// The cursor samples are sequential, not an atomic snapshot of moving input.
#[derive(Clone, Debug, Serialize)]
pub struct CaptureCoordinateDiagnostics {
    pub thread_awareness: i32,
    pub per_monitor_v2: bool,
    pub cursor: Result<(i32, i32), String>,
    pub physical_cursor: Result<(i32, i32), String>,
    /// Left, top, right (exclusive), bottom (exclusive). Read only: this never
    /// changes or clears another application's cursor confinement.
    pub clip_rect: Result<[i32; 4], String>,
    pub monitors: Result<Vec<NativeDisplay>, String>,
}

pub fn capture_coordinate_diagnostics() -> CaptureCoordinateDiagnostics {
    let context = unsafe { GetThreadDpiAwarenessContext() };
    let mut logical = POINT { x: 0, y: 0 };
    let cursor = if unsafe { GetCursorPos(&mut logical) } != 0 {
        Ok((logical.x, logical.y))
    } else {
        Err(win_error("GetCursorPos"))
    };
    let mut physical = POINT { x: 0, y: 0 };
    let physical_cursor = if unsafe { GetPhysicalCursorPos(&mut physical) } != 0 {
        Ok((physical.x, physical.y))
    } else {
        Err(win_error("GetPhysicalCursorPos"))
    };
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let clip_rect = if unsafe { GetClipCursor(&mut rect) } != 0 {
        Ok([rect.left, rect.top, rect.right, rect.bottom])
    } else {
        Err(win_error("GetClipCursor"))
    };
    CaptureCoordinateDiagnostics {
        thread_awareness: unsafe { GetAwarenessFromDpiAwarenessContext(context) },
        per_monitor_v2: unsafe {
            AreDpiAwarenessContextsEqual(context, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) != 0
        },
        cursor,
        physical_cursor,
        clip_rect,
        monitors: displays(),
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}

/// Enumerate each monitor independently in physical virtual-desktop pixels.
/// The DPI-awareness change applies only to this thread and is restored.
pub fn displays() -> Result<Vec<NativeDisplay>, String> {
    struct Enumeration {
        items: Vec<NativeDisplay>,
        error: Option<String>,
    }
    unsafe extern "system" fn monitor(
        handle: HMONITOR,
        _: HDC,
        _: *mut RECT,
        parameter: LPARAM,
    ) -> i32 {
        let state = unsafe { &mut *(parameter as *mut Enumeration) };
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if unsafe { GetMonitorInfoW(handle, &mut info.monitorInfo) } == 0 {
                return Err(win_error("GetMonitorInfoW"));
            }
            let bounds = info.monitorInfo.rcMonitor;
            let width = i64::from(bounds.right) - i64::from(bounds.left);
            let height = i64::from(bounds.bottom) - i64::from(bounds.top);
            if width <= 0 || height <= 0 || state.items.len() >= 256 {
                return Err("invalid or excessive monitor geometry".into());
            }
            let len = info
                .szDevice
                .iter()
                .position(|&v| v == 0)
                .unwrap_or(info.szDevice.len());
            let id = String::from_utf16(&info.szDevice[..len])
                .map_err(|_| "invalid monitor device name".to_owned())?;
            if id.is_empty() || state.items.iter().any(|item| item.id == id) {
                return Err("empty or duplicate monitor device name".into());
            }
            state.items.push(NativeDisplay {
                id,
                x: f64::from(bounds.left),
                y: f64::from(bounds.top),
                width: width as f64,
                height: height as f64,
            });
            Ok(())
        }));
        match result {
            Ok(Ok(())) => 1,
            Ok(Err(error)) => {
                state.error = Some(error);
                0
            }
            Err(_) => {
                state.error = Some("monitor enumeration panicked".into());
                0
            }
        }
    }
    let _dpi = DpiScope::physical()?;
    let mut state = Enumeration {
        items: Vec::new(),
        error: None,
    };
    let success = unsafe {
        EnumDisplayMonitors(
            null_mut(),
            null(),
            Some(monitor),
            &mut state as *mut _ as LPARAM,
        )
    };
    if let Some(error) = state.error {
        return Err(error);
    }
    if success == 0 {
        return Err(win_error("EnumDisplayMonitors"));
    }
    if state.items.is_empty() {
        return Err("no active Windows displays".into());
    }
    state.items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(state.items)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Decision {
    suppress: bool,
    command: Option<usize>,
}

/// Track which downs reached Windows so focus changes cannot leave a locally
/// held key/button without its release. Suppressed downs retain suppressed ups
/// even after returning focus, preventing cross-focus orphan key messages.
struct Policy {
    physical: [bool; 256],
    delivered: [bool; 256],
    suppressed: [bool; 256],
    hotkey_suppressed: [bool; 256],
}
impl Policy {
    fn new(held: impl Fn(usize) -> bool) -> Self {
        let mut physical = std::array::from_fn(held);
        // Windows hooks report left/right modifiers. Generic snapshot aliases
        // would otherwise remain held forever after the sided key is released.
        physical[0x10] = false;
        physical[0x11] = false;
        physical[0x12] = false;
        Self {
            physical,
            delivered: physical,
            suppressed: [false; 256],
            hotkey_suppressed: [false; 256],
        }
    }
    fn transition(
        &mut self,
        key: usize,
        down: bool,
        remote: bool,
        keyboard: bool,
        allow_legacy_chords: bool,
    ) -> Decision {
        if key >= self.physical.len() {
            return Decision {
                suppress: remote,
                command: None,
            };
        }
        let was_down = self.physical[key];
        self.physical[key] = down;
        let ctrl = self.physical[0x11] || self.physical[0xa2] || self.physical[0xa3];
        let alt = self.physical[0x12] || self.physical[0xa4] || self.physical[0xa5];
        if allow_legacy_chords
            && keyboard
            && down
            && !was_down
            && ctrl
            && alt
            && matches!(key, 0x20 | 0x1b)
        {
            self.suppressed[key] = true;
            return Decision {
                suppress: true,
                command: Some(if key == 0x1b { STOP } else { TOGGLE }),
            };
        }
        if down {
            if !was_down {
                self.delivered[key] = !remote;
                self.suppressed[key] = remote;
            }
            Decision {
                suppress: remote || self.suppressed[key],
                command: None,
            }
        } else {
            let suppress = self.suppressed[key] || (remote && !self.delivered[key]);
            self.delivered[key] = false;
            self.suppressed[key] = false;
            Decision {
                suppress,
                command: None,
            }
        }
    }
}

struct HookState {
    policy: Policy,
    hwnd: HWND,
    focus: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: VecDeque<CaptureEvent>,
    wake_posted: bool,
    failed: bool,
    requested_stop: bool,
}
pub(super) const WM_ROUTED_INPUT: u32 = WM_APP + 0x41;
const EVENT_CAPACITY: usize = 256;

pub(super) fn abort_input() {
    HOOK_STATE.with(|slot| {
        if let Some(state) = slot.borrow().as_ref() {
            state.focus.store(false, Ordering::Release);
        }
    });
}
fn coalesce_raw_delta(prev_dx: i32, prev_dy: i32, dx: i32, dy: i32) -> Option<(i32, i32)> {
    let next_dx = prev_dx.checked_add(dx)?;
    let next_dy = prev_dy.checked_add(dy)?;
    ((-20_000..=20_000).contains(&next_dx) && (-20_000..=20_000).contains(&next_dy))
        .then_some((next_dx, next_dy))
}

impl HookState {
    fn enqueue(&mut self, event: CaptureEvent) {
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        let coalesced = if self.focus.load(Ordering::Acquire) {
            match (self.events.back_mut(), &event) {
                (
                    Some(CaptureEvent::Delta {
                        dx: prev_dx,
                        dy: prev_dy,
                        ..
                    }),
                    CaptureEvent::Delta { dx, dy, .. },
                ) => {
                    if let Some((next_dx, next_dy)) =
                        coalesce_raw_delta(*prev_dx, *prev_dy, *dx, *dy)
                    {
                        *prev_dx = next_dx;
                        *prev_dy = next_dy;
                        true
                    } else {
                        false
                    }
                }
                _ => false,
            }
        } else {
            false
        };
        if !coalesced {
            // Capacity is allocated before hooks are installed. Overflow stops the
            // entire source rather than losing a button/key release or growing.
            if self.events.len() == EVENT_CAPACITY {
                self.failed = true;
                self.focus.store(false, Ordering::Release);
                self.stop.store(true, Ordering::Release);
                return;
            }
            self.events.push_back(event);
        }
        if !self.wake_posted {
            if unsafe { PostMessageW(self.hwnd, WM_ROUTED_INPUT, 0, 0) } == 0 {
                self.failed = true;
                self.focus.store(false, Ordering::Release);
                self.stop.store(true, Ordering::Release);
            } else {
                self.wake_posted = true;
            }
        }
    }
}
thread_local! { static HOOK_STATE: RefCell<Option<HookState>> = const { RefCell::new(None) }; }

pub(super) fn enqueue_raw_move(dx: i32, dy: i32, captured_at: Instant) {
    HOOK_STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.enqueue(CaptureEvent::Delta {
                dx,
                dy,
                captured_at,
            });
        }
    });
}

pub(super) fn drain_events() -> Result<Vec<CaptureEvent>, String> {
    HOOK_STATE.with(|slot| {
        let mut value = slot.borrow_mut();
        let Some(state) = value.as_mut() else {
            return Ok(Vec::new());
        };
        state.wake_posted = false;
        if state.failed {
            return Err("routed input delivery failed; sharing stopped".into());
        }
        Ok(state.events.drain(..).collect())
    })
}

pub(super) fn take_stop_request() -> bool {
    HOOK_STATE.with(|slot| {
        slot.borrow_mut()
            .as_mut()
            .is_some_and(|state| std::mem::take(&mut state.requested_stop))
    })
}

fn with_policy(f: impl FnOnce(&mut HookState) -> Decision) -> bool {
    HOOK_STATE.with(|slot| {
        let Ok(mut value) = slot.try_borrow_mut() else {
            return false;
        };
        let Some(state) = value.as_mut() else {
            return false;
        };
        let result = catch_unwind(AssertUnwindSafe(|| f(state)));
        match result {
            Ok(decision) => {
                if let Some(command) = decision.command {
                    // Release local delivery synchronously before the command
                    // reaches the queue; the core owns remote release/pause.
                    state.focus.store(false, Ordering::Release);
                    if command == STOP {
                        state.requested_stop = true;
                        state.stop.store(true, Ordering::Release);
                    }
                    if unsafe { PostMessageW(state.hwnd, WM_COMMAND, command, 0) } == 0 {
                        state.stop.store(true, Ordering::Release);
                    }
                }
                decision.suppress
            }
            Err(_) => {
                state.focus.store(false, Ordering::Release);
                state.stop.store(true, Ordering::Release);
                false
            }
        }
    })
}

unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const INPUT_ORIGIN_TAG: usize = 0x4352_4F53_5346_4C4F;
    if code >= 0 && lparam != 0 {
        let input = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let is_media_or_browser_vk = matches!(input.vkCode, 0xA6 | 0xA7 | 0xAD..=0xB1 | 0xB3);
        // OEM laptop Fn-key daemons, mouse vendor utilities (Logitech/Razer sending
        // VK_BROWSER_BACK / VK_BROWSER_FORWARD), and Windows HID Input Service often
        // deliver with LLKHF_INJECTED set. Accept unless tagged with Crossflow's own origin.
        let accept = (input.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) == 0)
            || (is_media_or_browser_vk && input.dwExtraInfo != INPUT_ORIGIN_TAG);
        if accept {
            let suppress = with_policy(|state| {
                let remote =
                    state.focus.load(Ordering::Acquire) && !state.stop.load(Ordering::Acquire);
                let down = input.flags & LLKHF_UP == 0;
                let vk = input.vkCode as usize;
                let browser_mouse_btn = match input.vkCode {
                    0xA6 => Some(Button::Back),
                    0xA7 => Some(Button::Forward),
                    _ => None,
                };
                // Map multimedia virtual keys and sided modifiers directly so Right Shift
                // (which Windows WH_KEYBOARD_LL often flags with LLKHF_EXTENDED -> 0xE036)
                // and OEM media keys with scanCode == 0 never get dropped.
                let direct_usage = match input.vkCode {
                    0x14 => Some(crate::keymap::HID_CAPS_LOCK),
                    0x5B => Some(0xE3), // VK_LWIN
                    0x5C => Some(0xE7), // VK_RWIN
                    0x7C..=0x87 => Some(0x68 + (input.vkCode - 0x7C) as u16), // VK_F13..VK_F24
                    0xA0 => Some(0xE1), // VK_LSHIFT
                    0xA1 => Some(0xE5), // VK_RSHIFT
                    0xA2 => Some(0xE0), // VK_LCONTROL
                    0xA3 => Some(0xE4), // VK_RCONTROL
                    0xA4 => Some(0xE2), // VK_LMENU
                    0xA5 => Some(0xE6), // VK_RMENU
                    0xAD => Some(crate::keymap::HID_MUTE),
                    0xAE => Some(crate::keymap::HID_VOLUME_DOWN),
                    0xAF => Some(crate::keymap::HID_VOLUME_UP),
                    0xB0 => Some(crate::keymap::HID_MEDIA_NEXT),
                    0xB1 => Some(crate::keymap::HID_MEDIA_PREV),
                    0xB3 => Some(crate::keymap::HID_MEDIA_PLAY_PAUSE),
                    _ => None,
                };
                let resolved_usage = if let Some(u) = direct_usage {
                    Some(u)
                } else if browser_mouse_btn.is_none() && input.vkCode < 0xff && input.vkCode != 0x13 {
                    let scan = if input.scanCode == 0 {
                        (unsafe { MapVirtualKeyW(input.vkCode, MAPVK_VK_TO_VSC_EX) }) as u16
                    } else if input.scanCode <= 0x7f {
                        input.scanCode as u16
                            | if input.flags & LLKHF_EXTENDED != 0 {
                                0xe000
                            } else {
                                0
                            }
                    } else {
                        0
                    };
                    scan_to_hid(scan).or_else(|| {
                        let fallback =
                            (unsafe { MapVirtualKeyW(input.vkCode, MAPVK_VK_TO_VSC_EX) }) as u16;
                        scan_to_hid(fallback)
                    })
                } else {
                    None
                };

                let has_engine = crate::hotkeys::has_active_engine();
                if has_engine && vk < state.policy.hotkey_suppressed.len() {
                    if state.policy.hotkey_suppressed[vk] {
                        state.policy.physical[vk] = down;
                        if !down {
                            state.policy.hotkey_suppressed[vk] = false;
                            state.policy.delivered[vk] = false;
                            state.policy.suppressed[vk] = false;
                        }
                        return Decision {
                            suppress: true,
                            command: None,
                        };
                    }
                    let was_down = state.policy.physical[vk];
                    if down && !was_down && !matches!(resolved_usage, Some(0xE0..=0xE7)) {
                        sync_windows_hotkey_modifiers(&state.policy, remote);
                    }
                    if let Some(btn) = browser_mouse_btn
                        && (!down || !was_down)
                        && crate::hotkeys::active_observe_button(btn, down).is_some()
                    {
                        state.policy.physical[vk] = true;
                        state.policy.hotkey_suppressed[vk] = true;
                        return Decision {
                            suppress: true,
                            command: None,
                        };
                    }
                    if let Some(usage) = resolved_usage
                        && (!down || !was_down)
                        && crate::hotkeys::active_observe_key(usage, down).is_some()
                    {
                        state.policy.physical[vk] = true;
                        state.policy.hotkey_suppressed[vk] = true;
                        return Decision {
                            suppress: true,
                            command: None,
                        };
                    }
                }

                let decision = state
                    .policy
                    .transition(vk, down, remote, true, !has_engine);
                if let Some(usage) = resolved_usage {
                    state.enqueue(CaptureEvent::Key {
                        usage,
                        down,
                        captured_at: Instant::now(),
                        remote_at_capture: Some(remote),
                    });
                }
                decision
            });
            if suppress {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

fn modifier_physically_down(policy: &Policy, remote: bool, vks: &[usize]) -> bool {
    vks.iter().any(|&vk| {
        if remote || policy.suppressed[vk] {
            policy.physical[vk]
        } else {
            unsafe { GetAsyncKeyState(vk as i32) < 0 }
        }
    })
}

fn sync_windows_hotkey_modifiers(policy: &Policy, remote: bool) {
    // While remote focus is active (or for modifiers whose key-down was suppressed
    // during remote focus), Win32 GetAsyncKeyState stays false because WH_KEYBOARD_LL
    // swallowed local delivery. Read true hardware state from `policy.physical` there!
    let ctrl = modifier_physically_down(policy, remote, &[0x11, 0xA2, 0xA3]);
    let shift = modifier_physically_down(policy, remote, &[0x10, 0xA0, 0xA1]);
    let alt = modifier_physically_down(policy, remote, &[0x12, 0xA4, 0xA5]);
    let meta = modifier_physically_down(policy, remote, &[0x5B, 0x5C]);
    crate::hotkeys::active_sync_modifiers(ctrl, shift, alt, meta);
}

fn mouse_button(message: u32, data: u32) -> Option<(usize, bool)> {
    match message {
        WM_LBUTTONDOWN => Some((1, true)),
        WM_LBUTTONUP => Some((1, false)),
        WM_RBUTTONDOWN => Some((2, true)),
        WM_RBUTTONUP => Some((2, false)),
        WM_MBUTTONDOWN => Some((4, true)),
        WM_MBUTTONUP => Some((4, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => match data >> 16 {
            1 => Some((5, message == WM_XBUTTONDOWN)),
            2 => Some((6, message == WM_XBUTTONDOWN)),
            _ => None,
        },
        _ => None,
    }
}

unsafe extern "system" fn mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const INPUT_ORIGIN_TAG: usize = 0x4352_4F53_5346_4C4F;
    if code >= 0 && lparam != 0 {
        let input = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
        let msg = wparam as u32;
        let is_aux_mouse = matches!(
            msg,
            WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP
        );
        // Accept physical events AND vendor-driver-remapped side/middle mouse buttons
        // (Logitech G HUB / Razer / etc.) unless tagged with Crossflow's own origin.
        let accept = (input.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0)
            || (is_aux_mouse && input.dwExtraInfo != INPUT_ORIGIN_TAG);
        if accept {
            let suppress = with_policy(|state| {
                let remote =
                    state.focus.load(Ordering::Acquire) && !state.stop.load(Ordering::Acquire);
                if let Some((button_vk, down)) = mouse_button(msg, input.mouseData) {
                    let button = match button_vk {
                        1 => Button::Left,
                        2 => Button::Right,
                        4 => Button::Middle,
                        5 => Button::Back,
                        _ => Button::Forward,
                    };
                    if crate::hotkeys::has_active_engine()
                        && button_vk < state.policy.hotkey_suppressed.len()
                    {
                        if state.policy.hotkey_suppressed[button_vk] {
                            state.policy.physical[button_vk] = down;
                            if !down {
                                state.policy.hotkey_suppressed[button_vk] = false;
                                state.policy.delivered[button_vk] = false;
                                state.policy.suppressed[button_vk] = false;
                            }
                            return Decision {
                                suppress: true,
                                command: None,
                            };
                        }
                        let was_down = state.policy.physical[button_vk];
                        if down && !was_down {
                            sync_windows_hotkey_modifiers(&state.policy, remote);
                        }
                        if (!down || !was_down)
                            && crate::hotkeys::active_observe_button(button, down).is_some()
                        {
                            state.policy.physical[button_vk] = true;
                            state.policy.hotkey_suppressed[button_vk] = true;
                            return Decision {
                                suppress: true,
                                command: None,
                            };
                        }
                    }
                    let decision = state.policy.transition(button_vk, down, remote, false, false);
                    state.enqueue(CaptureEvent::Button {
                        button,
                        down,
                        captured_at: Instant::now(),
                        remote_at_capture: Some(remote),
                    });
                    decision
                } else {
                    if matches!(wparam as u32, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
                        let delta = (input.mouseData >> 16) as i16 as i32;
                        if delta != 0 {
                            state.enqueue(CaptureEvent::Scroll {
                                horizontal: if wparam as u32 == WM_MOUSEHWHEEL {
                                    delta
                                } else {
                                    0
                                },
                                vertical: if wparam as u32 == WM_MOUSEWHEEL {
                                    delta
                                } else {
                                    0
                                },
                                captured_at: Instant::now(),
                                remote_at_capture: Some(remote),
                            });
                        }
                    }
                    Decision {
                        suppress: remote
                            && matches!(
                                wparam as u32,
                                WM_MOUSEMOVE
                                    | WM_MOUSEWHEEL
                                    | WM_MOUSEHWHEEL
                                    | WM_XBUTTONDOWN
                                    | WM_XBUTTONUP
                            ),
                        command: None,
                    }
                }
            });
            if suppress {
                return 1;
            }
        }
    }
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

pub(super) struct RoutedHooks {
    mouse: HHOOK,
    keyboard: HHOOK,
    focus: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    notified_window: HWND,
}
impl RoutedHooks {
    pub(super) fn install(
        hwnd: HWND,
        focus: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        focus.store(false, Ordering::Release);
        let policy = Policy::new(|vk| unsafe { GetAsyncKeyState(vk as i32) < 0 });
        HOOK_STATE.with(|slot| {
            let mut value = slot.borrow_mut();
            if value.is_some() {
                return Err("routed hooks already installed on this thread".to_owned());
            }
            *value = Some(HookState {
                policy,
                hwnd,
                focus: focus.clone(),
                stop: stop.clone(),
                events: VecDeque::with_capacity(EVENT_CAPACITY),
                wake_posted: false,
                failed: false,
                requested_stop: false,
            });
            Ok(())
        })?;
        let mut hooks = Self {
            mouse: null_mut(),
            keyboard: null_mut(),
            focus,
            stop,
            notified_window: null_mut(),
        };
        let instance = unsafe { GetModuleHandleW(null()) };
        if instance.is_null() {
            return Err(win_error("GetModuleHandleW(hooks)"));
        }
        hooks.mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), instance, 0) };
        if hooks.mouse.is_null() {
            return Err(win_error("SetWindowsHookExW(mouse)"));
        }
        hooks.keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard), instance, 0) };
        if hooks.keyboard.is_null() {
            return Err(win_error("SetWindowsHookExW(keyboard)"));
        }
        if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } == 0 {
            return Err(win_error("WTSRegisterSessionNotification"));
        }
        hooks.notified_window = hwnd;
        Ok(hooks)
    }
    pub(super) fn maintain(&self, status: CaptureStatus) {
        if status != CaptureStatus::Active || self.stop.load(Ordering::Acquire) {
            self.focus.store(false, Ordering::Release);
        }
    }
    pub(super) fn snapshot(&self) -> (Vec<Button>, Vec<u16>) {
        let held: [bool; 256] =
            std::array::from_fn(|vk| unsafe { GetAsyncKeyState(vk as i32) < 0 });
        HOOK_STATE.with(|slot| {
            if let Some(state) = slot.borrow_mut().as_mut() {
                state.policy = Policy::new(|vk| held[vk]);
            }
        });
        super::physical_snapshot(
            |vk| held[vk as usize],
            |vk| unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) as u16 },
        )
    }
    pub(super) fn error(&self) -> Option<String> {
        HOOK_STATE.with(|slot| {
            slot.borrow().as_ref().and_then(|state| {
                state
                    .failed
                    .then(|| "routed input delivery failed; sharing stopped".into())
            })
        })
    }
}
impl Drop for RoutedHooks {
    fn drop(&mut self) {
        self.focus.store(false, Ordering::Release);
        // Clear callback state even if a native unhook reports failure. Such a
        // leftover hook then always passes through until process termination.
        HOOK_STATE.with(|slot| {
            *slot.borrow_mut() = None;
        });
        unsafe {
            if !self.notified_window.is_null() {
                WTSUnRegisterSessionNotification(self.notified_window);
            }
            if !self.keyboard.is_null() {
                UnhookWindowsHookEx(self.keyboard);
            }
            if !self.mouse.is_null() {
                UnhookWindowsHookEx(self.mouse);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_down_and_up_stay_suppressed_across_focus_return() {
        for key in [1, 2, 4, 5, 6, 0x41, 0xa2] {
            let mut policy = Policy::new(|_| false);
            assert!(policy.transition(key, true, true, key > 6, true).suppress);
            assert!(policy.transition(key, false, false, key > 6, true).suppress);
            assert!(!policy.transition(key, true, false, key > 6, true).suppress);
            assert!(!policy.transition(key, false, false, key > 6, true).suppress);
        }
    }
    #[test]
    fn preheld_local_release_is_not_lost_when_focus_changes() {
        for key in [1, 0x41, 0xa2] {
            let mut policy = Policy::new(|vk| vk == key);
            assert!(policy.transition(key, true, true, key > 6, true).suppress);
            assert!(!policy.transition(key, false, true, key > 6, true).suppress);
        }
    }
    #[test]
    fn emergency_chords_work_even_with_suppressed_modifiers_and_do_not_repeat() {
        for (key, command) in [(0x20, TOGGLE), (0x1b, STOP)] {
            let mut policy = Policy::new(|_| false);
            assert!(policy.transition(0xa2, true, true, true, true).suppress);
            assert!(policy.transition(0xa4, true, true, true, true).suppress);
            assert_eq!(
                policy.transition(key, true, true, true, true),
                Decision {
                    suppress: true,
                    command: Some(command)
                }
            );
            assert_eq!(
                policy.transition(key, true, false, true, true),
                Decision {
                    suppress: true,
                    command: None
                }
            );
            assert!(policy.transition(key, false, false, true, true).suppress);
            assert!(policy.transition(0xa2, false, false, true, true).suppress);
            assert!(policy.transition(0xa4, false, false, true, true).suppress);
        }
    }
    #[test]
    fn local_chords_are_consumed_once_without_stranding_local_modifiers() {
        let mut policy = Policy::new(|_| false);
        assert!(!policy.transition(0xa3, true, false, true, true).suppress);
        assert!(!policy.transition(0xa5, true, false, true, true).suppress);
        assert_eq!(
            policy.transition(0x20, true, false, true, true).command,
            Some(TOGGLE)
        );
        assert!(policy.transition(0x20, false, false, true, true).suppress);
        assert!(!policy.transition(0xa3, false, false, true, true).suppress);
        assert!(!policy.transition(0xa5, false, false, true, true).suppress);
    }
    #[test]
    fn physical_context_is_nested_and_restored_after_unwind_without_input() {
        // An isolated thread changes only its DPI context. No hooks, windows,
        // cursor reads/warps or synthetic input are involved in this test.
        std::thread::spawn(|| {
            let original = unsafe { GetThreadDpiAwarenessContext() };
            let unwound = catch_unwind(|| {
                let _outer = DpiScope::physical().unwrap();
                assert_ne!(
                    unsafe {
                        AreDpiAwarenessContextsEqual(
                            GetThreadDpiAwarenessContext(),
                            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                        )
                    },
                    0
                );
                {
                    let _inner = DpiScope::physical().unwrap();
                }
                assert_ne!(
                    unsafe {
                        AreDpiAwarenessContextsEqual(
                            GetThreadDpiAwarenessContext(),
                            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                        )
                    },
                    0
                );
                panic!("exercise DPI restoration on failure");
            });
            assert!(unwound.is_err());
            assert_ne!(
                unsafe { AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), original) },
                0
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn coordinates_reject_nonfinite_and_overflow_before_native_call() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            i32::MAX as f64 + 1.0,
            i32::MIN as f64 - 1.0,
        ] {
            assert!(integer_coordinate(value).is_err());
        }
        assert_eq!(integer_coordinate(-1920.25).unwrap(), -1920);
        assert_eq!(integer_coordinate(100.75).unwrap(), 101);
    }
    #[test]
    fn mouse_buttons_include_both_extended_buttons_without_aliasing() {
        assert_eq!(mouse_button(WM_XBUTTONDOWN, 1 << 16), Some((5, true)));
        assert_eq!(mouse_button(WM_XBUTTONUP, 2 << 16), Some((6, false)));
        assert_eq!(mouse_button(WM_XBUTTONDOWN, 3 << 16), None);
        assert_eq!(mouse_button(WM_MOUSEWHEEL, 0), None);
    }
    #[test]
    fn event_ring_overflow_stops_and_releases_local_gate_without_dropping_one_release() {
        let focus = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = HookState {
            policy: Policy::new(|_| false),
            hwnd: null_mut(),
            focus: focus.clone(),
            stop: stop.clone(),
            events: VecDeque::with_capacity(EVENT_CAPACITY),
            wake_posted: true,
            failed: false,
            requested_stop: false,
        };
        for _ in 0..EVENT_CAPACITY {
            state.enqueue(CaptureEvent::Key {
                usage: 4,
                down: true,
                captured_at: Instant::now(),
                remote_at_capture: Some(true),
            });
        }
        assert_eq!(state.events.len(), EVENT_CAPACITY);
        assert!(!stop.load(Ordering::Acquire));
        state.enqueue(CaptureEvent::Key {
            usage: 4,
            down: false,
            captured_at: Instant::now(),
            remote_at_capture: Some(true),
        });
        assert!(state.failed);
        assert!(stop.load(Ordering::Acquire));
        assert!(!focus.load(Ordering::Acquire));
        assert_eq!(state.events.len(), EVENT_CAPACITY);
    }
    #[test]
    fn pause_disconnect_and_drop_clear_gate_without_native_handles() {
        let focus = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let hooks = RoutedHooks {
            mouse: null_mut(),
            keyboard: null_mut(),
            focus: focus.clone(),
            stop: stop.clone(),
            notified_window: null_mut(),
        };
        for status in [
            CaptureStatus::Ready,
            CaptureStatus::Disconnected,
            CaptureStatus::CaptureOnly,
        ] {
            focus.store(true, Ordering::Release);
            hooks.maintain(status);
            assert!(!focus.load(Ordering::Acquire));
        }
        focus.store(true, Ordering::Release);
        hooks.maintain(CaptureStatus::Active);
        assert!(focus.load(Ordering::Acquire));
        stop.store(true, Ordering::Release);
        hooks.maintain(CaptureStatus::Active);
        assert!(!focus.load(Ordering::Acquire));
        focus.store(true, Ordering::Release);
        drop(hooks);
        assert!(!focus.load(Ordering::Acquire));
    }
    #[test]
    fn raw_deltas_coalesce_while_remote_focus_is_active_without_displacing_controls() {
        let focus = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = HookState {
            policy: Policy::new(|_| false),
            hwnd: null_mut(),
            focus: focus.clone(),
            stop: stop.clone(),
            events: VecDeque::with_capacity(EVENT_CAPACITY),
            wake_posted: true,
            failed: false,
            requested_stop: false,
        };
        let t1 = Instant::now();
        state.enqueue(CaptureEvent::Delta {
            dx: 3,
            dy: -4,
            captured_at: t1,
        });
        state.enqueue(CaptureEvent::Delta {
            dx: 7,
            dy: 9,
            captured_at: Instant::now(),
        });
        assert_eq!(state.events.len(), 1);
        assert!(matches!(
            state.events.back(),
            Some(CaptureEvent::Delta { dx: 10, dy: 5, .. })
        ));
        state.enqueue(CaptureEvent::Key {
            usage: 4,
            down: true,
            captured_at: Instant::now(),
            remote_at_capture: Some(true),
        });
        state.enqueue(CaptureEvent::Delta {
            dx: -2,
            dy: 1,
            captured_at: Instant::now(),
        });
        assert_eq!(state.events.len(), 3);
        // When focus is local, adjacent raw deltas remain distinct.
        focus.store(false, Ordering::Release);
        state.enqueue(CaptureEvent::Delta {
            dx: 5,
            dy: 6,
            captured_at: Instant::now(),
        });
        assert_eq!(state.events.len(), 4);
    }
}
