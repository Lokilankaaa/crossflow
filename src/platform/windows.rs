//! Movement-only Windows Raw Input capture. Source cursor is never suppressed
//! or confined. No buttons or keyboard events are forwarded. The dedicated
//! process owns its mouse Raw Input registration; callbacks must be nonblocking.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    mem::{MaybeUninit, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_F8, VK_F9,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RAWMOUSE, RID_INPUT,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GWLP_USERDATA,
    GetMessageW, GetWindowLongPtrW, HWND_MESSAGE, KillTimer, MSG, RegisterClassW, SetTimer,
    SetWindowLongPtrW, TranslateMessage, UnregisterClassW, WM_CLOSE, WM_HOTKEY, WM_INPUT,
    WM_NCCREATE, WNDCLASSW,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub enum CaptureEvent {
    /// Relative device counts, before Windows pointer acceleration.
    Delta {
        dx: i32,
        dy: i32,
        captured_at: Instant,
    },
    Toggle,
    Stop,
}

#[derive(Clone, Debug, Default)]
pub struct CaptureStats {
    pub packets: u64,
    pub motion: u64,
    pub absolute: u64,
    pub device_less: u64,
    pub zero_motion: u64,
    pub non_mouse: u64,
    pub malformed: u64,
    pub read_errors: u64,
    pub elapsed: Duration,
}

#[derive(Debug, PartialEq, Eq)]
enum Packet {
    Motion(i32, i32),
    Absolute,
    DeviceLess,
    ZeroMotion,
    NonMouse,
    Malformed,
}

fn decode(raw: &RAWINPUT, bytes: u32) -> Packet {
    if bytes < size_of::<RAWINPUTHEADER>() as u32 {
        return Packet::Malformed;
    }
    if raw.header.dwType != 0 {
        return Packet::NonMouse;
    }
    let required = (size_of::<RAWINPUTHEADER>() + size_of::<RAWMOUSE>()) as u32;
    if bytes < required || raw.header.dwSize < required || raw.header.dwSize > bytes {
        return Packet::Malformed;
    }
    if raw.header.hDevice.is_null() {
        return Packet::DeviceLess;
    }
    // Mouse type and initialized byte length checked before union access.
    let mouse = unsafe { raw.data.mouse };
    if mouse.usFlags & 1 != 0 {
        return Packet::Absolute;
    }
    if mouse.lLastX == 0 && mouse.lLastY == 0 {
        return Packet::ZeroMotion;
    }
    Packet::Motion(mouse.lLastX, mouse.lLastY)
}

struct State<F> {
    callback: F,
    stop: Arc<AtomicBool>,
    stats: CaptureStats,
    error: Option<String>,
}
struct CallbackState<F> {
    data: RefCell<State<F>>,
    stop: Arc<AtomicBool>,
    reentrant: Cell<bool>,
}
impl<F: FnMut(CaptureEvent)> State<F> {
    fn emit(&mut self, event: CaptureEvent) {
        // A panic may not unwind over the Windows callback boundary.
        if catch_unwind(AssertUnwindSafe(|| (self.callback)(event))).is_err() {
            self.error = Some("capture callback panicked; stopped safely".into());
            self.stop.store(true, Ordering::Release);
        }
    }
    fn read(&mut self, handle: LPARAM, captured_at: Instant) {
        let mut raw = MaybeUninit::<RAWINPUT>::zeroed();
        let mut size = size_of::<RAWINPUT>() as u32;
        // Only mouse usage is registered; unexpected oversized packets fail.
        let bytes = unsafe {
            GetRawInputData(
                handle as *mut c_void,
                RID_INPUT,
                raw.as_mut_ptr().cast(),
                &mut size,
                size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        if bytes == u32::MAX {
            self.stats.read_errors += 1;
            self.error = Some(win_error("GetRawInputData"));
            self.stop.store(true, Ordering::Release);
            return;
        }
        self.stats.packets += 1;
        // All union bytes are initialized, including bytes not set by Windows.
        let raw = unsafe { raw.assume_init() };
        match decode(&raw, bytes) {
            Packet::Motion(dx, dy) => {
                self.stats.motion += 1;
                self.emit(CaptureEvent::Delta {
                    dx,
                    dy,
                    captured_at,
                });
            }
            Packet::Absolute => self.stats.absolute += 1,
            Packet::DeviceLess => self.stats.device_less += 1,
            Packet::ZeroMotion => self.stats.zero_motion += 1,
            Packet::NonMouse => self.stats.non_mouse += 1,
            Packet::Malformed => {
                self.stats.malformed += 1;
                self.error = Some("malformed Raw Input packet; stopped safely".into());
                self.stop.store(true, Ordering::Release);
            }
        }
    }
}

unsafe extern "system" fn window_proc<F: FnMut(CaptureEvent)>(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const CallbackState<F>;
    if !pointer.is_null() {
        // Box remains alive until window destruction; the loop owns access.
        // A callback that pumps native messages must not alias mutable state.
        let slot = unsafe { &*pointer };
        let Ok(mut state) = slot.data.try_borrow_mut() else {
            slot.reentrant.set(true);
            slot.stop.store(true, Ordering::Release);
            return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
        };
        match message {
            WM_INPUT => {
                let captured_at = Instant::now();
                if !state.stop.load(Ordering::Acquire) {
                    state.read(lparam, captured_at);
                }
                // Required for cleanup of foreground RIM_INPUT packets.
                drop(state);
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
            WM_HOTKEY => {
                if wparam == 1 {
                    state.emit(CaptureEvent::Toggle);
                }
                if wparam == 2 {
                    state.emit(CaptureEvent::Stop);
                    state.stop.store(true, Ordering::Release);
                }
                return 0;
            }
            WM_CLOSE => {
                state.emit(CaptureEvent::Stop);
                state.stop.store(true, Ordering::Release);
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn win_error(operation: &str) -> String {
    let code = unsafe { GetLastError() };
    format!(
        "{operation} failed: {} (Win32 {code})",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

struct ActiveGuard;
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::Release);
    }
}

struct Resources {
    name: Vec<u16>,
    instance: HINSTANCE,
    registered: bool,
    hwnd: HWND,
    raw: bool,
    hotkeys: Vec<i32>,
    timer: usize,
}
impl Drop for Resources {
    fn drop(&mut self) {
        unsafe {
            if self.timer != 0 {
                KillTimer(self.hwnd, self.timer);
            }
            for id in &self.hotkeys {
                UnregisterHotKey(self.hwnd, *id);
            }
            if self.raw {
                let remove = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 2,
                    dwFlags: RIDEV_REMOVE,
                    hwndTarget: null_mut(),
                };
                RegisterRawInputDevices(&remove, 1, size_of::<RAWINPUTDEVICE>() as u32);
            }
            if !self.hwnd.is_null() {
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
                DestroyWindow(self.hwnd);
            }
            if self.registered {
                UnregisterClassW(self.name.as_ptr(), self.instance);
            }
        }
    }
}

/// Ctrl+Alt+F8 emits Toggle; Ctrl+Alt+F9 emits Stop and ends capture.
/// External stop is checked between events with a 10 ms idle wakeup. Callbacks
/// only collect data; they must not perform network or blocking I/O. Filtering
/// device-less packets is not proof against all virtual/driver input sources.
pub fn run_capture<F: FnMut(CaptureEvent)>(
    on_event: F,
    stop: Arc<AtomicBool>,
    duration: Option<Duration>,
) -> Result<CaptureStats, String> {
    if stop.load(Ordering::Acquire) {
        return Ok(CaptureStats::default());
    }
    if duration.is_some_and(|d| d.is_zero()) {
        return Err("capture duration must be positive".into());
    }
    if ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err("only one Windows capture loop may run per process".into());
    }
    let _active = ActiveGuard;
    let state = Box::new(CallbackState {
        data: RefCell::new(State {
            callback: on_event,
            stop: stop.clone(),
            stats: CaptureStats::default(),
            error: None,
        }),
        stop,
        reentrant: Cell::new(false),
    });
    let mut resources = Resources {
        name: "CrossflowPhase0RawInput\0".encode_utf16().collect(),
        instance: unsafe { GetModuleHandleW(null()) },
        registered: false,
        hwnd: null_mut(),
        raw: false,
        hotkeys: Vec::new(),
        timer: 0,
    };
    if resources.instance.is_null() {
        return Err(win_error("GetModuleHandleW"));
    }
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc::<F>),
        hInstance: resources.instance,
        lpszClassName: resources.name.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(win_error("RegisterClassW"));
    }
    resources.registered = true;
    resources.hwnd = unsafe {
        CreateWindowExW(
            0,
            resources.name.as_ptr(),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            resources.instance,
            (&*state as *const CallbackState<F>).cast(),
        )
    };
    if resources.hwnd.is_null() {
        return Err(win_error("CreateWindowExW"));
    }
    let device = RAWINPUTDEVICE {
        usUsagePage: 1,
        usUsage: 2,
        dwFlags: RIDEV_INPUTSINK,
        hwndTarget: resources.hwnd,
    };
    if unsafe { RegisterRawInputDevices(&device, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0 {
        return Err(win_error("RegisterRawInputDevices"));
    }
    resources.raw = true;
    for (id, key) in [(1, VK_F8), (2, VK_F9)] {
        if unsafe {
            RegisterHotKey(
                resources.hwnd,
                id,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                key as u32,
            )
        } == 0
        {
            return Err(format!(
                "{}; check desktop permissions or a conflicting Ctrl+Alt+F{} binding",
                win_error("RegisterHotKey"),
                if id == 1 { 8 } else { 9 }
            ));
        }
        resources.hotkeys.push(id);
    }
    resources.timer = unsafe { SetTimer(resources.hwnd, 1, 10, None) };
    if resources.timer == 0 {
        return Err(win_error("SetTimer"));
    }
    let started = Instant::now();
    let mut message = MaybeUninit::<MSG>::zeroed();
    while !state.stop.load(Ordering::Acquire) && !duration.is_some_and(|d| started.elapsed() >= d) {
        let result = unsafe { GetMessageW(message.as_mut_ptr(), null_mut(), 0, 0) };
        if result == -1 {
            state.data.borrow_mut().error = Some(win_error("GetMessageW"));
            break;
        }
        if result == 0 {
            break;
        }
        if state.stop.load(Ordering::Acquire) || duration.is_some_and(|d| started.elapsed() >= d) {
            // GetMessage removed this packet; perform native input cleanup even
            // though the expired session must not receive another callback.
            let message = unsafe { message.assume_init_ref() };
            if message.message == WM_INPUT {
                unsafe {
                    DefWindowProcW(
                        message.hwnd,
                        message.message,
                        message.wParam,
                        message.lParam,
                    );
                }
            }
            break;
        }
        unsafe {
            TranslateMessage(message.as_ptr());
            DispatchMessageW(message.as_ptr());
        }
    }
    state.data.borrow_mut().stats.elapsed = started.elapsed();
    drop(resources);
    if state.reentrant.get() {
        return Err("reentrant capture callback detected; capture stopped".into());
    }
    let mut data = state.data.borrow_mut();
    match data.error.take() {
        Some(error) => Err(error),
        None => Ok(data.stats.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(dx: i32, dy: i32) -> RAWINPUT {
        let mut raw: RAWINPUT = unsafe { std::mem::zeroed() };
        raw.header.dwSize = (size_of::<RAWINPUTHEADER>() + size_of::<RAWMOUSE>()) as u32;
        raw.header.hDevice = std::ptr::dangling_mut::<u8>().cast();
        raw.data.mouse = RAWMOUSE {
            lLastX: dx,
            lLastY: dy,
            ..unsafe { std::mem::zeroed() }
        };
        raw
    }
    #[test]
    fn mouse_abi_and_signed_delta() {
        assert_eq!(size_of::<RAWMOUSE>(), 24);
        assert_eq!(std::mem::offset_of!(RAWMOUSE, lLastX), 12);
        let raw = packet(7, -9);
        assert_eq!(decode(&raw, raw.header.dwSize), Packet::Motion(7, -9));
    }
    #[test]
    fn filters_absolute_and_device_less() {
        let mut raw = packet(1, 2);
        raw.data.mouse.usFlags = 1;
        assert_eq!(decode(&raw, raw.header.dwSize), Packet::Absolute);
        raw.header.hDevice = null_mut();
        assert_eq!(decode(&raw, raw.header.dwSize), Packet::DeviceLess);
    }
    #[test]
    fn rejects_short_and_ignores_button_only() {
        let mut raw = packet(0, 0);
        assert_eq!(decode(&raw, raw.header.dwSize), Packet::ZeroMotion);
        assert_eq!(decode(&raw, 4), Packet::Malformed);
        raw.header.dwSize += 100;
        assert_eq!(
            decode(&raw, size_of::<RAWINPUT>() as u32),
            Packet::Malformed
        );
    }
    #[test]
    fn callback_panic_stops_before_crossing_ffi() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = State {
            callback: |_| panic!("callback failed"),
            stop: stop.clone(),
            stats: CaptureStats::default(),
            error: None,
        };
        state.emit(CaptureEvent::Toggle);
        assert!(stop.load(Ordering::Acquire));
        assert!(state.error.is_some());
    }
}
