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
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_ESCAPE,
    VK_SPACE,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RAWMOUSE, RID_INPUT,
    RIDEV_INPUTSINK, RIDEV_REMOVE, RegisterRawInputDevices,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetMessageW, GetWindowLongPtrW, IDC_ARROW, KillTimer,
    LoadCursorW, MSG, RegisterClassW, SW_SHOWNOACTIVATE, SendMessageW, SetTimer, SetWindowLongPtrW,
    SetWindowTextW, ShowWindow, TranslateMessage, UnregisterClassW, WM_CLOSE, WM_COMMAND,
    WM_HOTKEY, WM_INPUT, WM_NCCREATE, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD,
    WS_MINIMIZEBOX, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
const TOGGLE: usize = 1;
const STOP: usize = 2;

/// Snapshot of the real session state, supplied by the shared core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    CaptureOnly,
    Disconnected,
    Ready,
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayStatus {
    CaptureOnly,
    Waiting,
    Ready,
    Active,
    Disconnected,
}

fn display_status(status: CaptureStatus, connected_once: &mut bool) -> DisplayStatus {
    match status {
        CaptureStatus::CaptureOnly => DisplayStatus::CaptureOnly,
        CaptureStatus::Disconnected if *connected_once => DisplayStatus::Disconnected,
        CaptureStatus::Disconnected => DisplayStatus::Waiting,
        CaptureStatus::Ready => {
            *connected_once = true;
            DisplayStatus::Ready
        }
        CaptureStatus::Active => {
            *connected_once = true;
            DisplayStatus::Active
        }
    }
}

#[derive(Clone, Copy)]
struct Controls {
    status: HWND,
    toggle: HWND,
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

impl Controls {
    fn render(self, status: DisplayStatus) {
        let (text, button, enabled) = match status {
            DisplayStatus::CaptureOnly => ("仅采集测试 · 不向其他设备共享", "开始共享", false),
            DisplayStatus::Waiting => ("等待连接 · 连接成功后可开始共享", "开始共享", false),
            DisplayStatus::Ready => ("已连接 · 等待开启", "开始共享", true),
            DisplayStatus::Active => ("正在共享 · 移动鼠标可控制 Mac", "暂停共享", true),
            DisplayStatus::Disconnected => ("断线暂停 · 重连后请手动开始共享", "开始共享", false),
        };
        unsafe {
            SetWindowTextW(self.status, wide(text).as_ptr());
            SetWindowTextW(self.toggle, wide(button).as_ptr());
            EnableWindow(self.toggle, i32::from(enabled));
        }
    }
}

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

struct State<F, S> {
    callback: F,
    status_fn: S,
    connected_once: bool,
    displayed: Option<DisplayStatus>,
    refreshed_at: Option<Instant>,
    stop: Arc<AtomicBool>,
    stats: CaptureStats,
    error: Option<String>,
}
struct CallbackState<F, S> {
    data: RefCell<State<F, S>>,
    controls: Cell<Option<Controls>>,
    stop: Arc<AtomicBool>,
    reentrant: Cell<bool>,
}
impl<F: FnMut(CaptureEvent), S: FnMut() -> CaptureStatus> State<F, S> {
    fn status(&mut self) -> CaptureStatus {
        match catch_unwind(AssertUnwindSafe(|| (self.status_fn)())) {
            Ok(status) => status,
            Err(_) => {
                self.error = Some("status callback panicked; stopped safely".into());
                self.stop.store(true, Ordering::Release);
                CaptureStatus::Disconnected
            }
        }
    }

    fn command(&mut self, command: usize) {
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        if command == STOP {
            self.emit(CaptureEvent::Stop);
            self.stop.store(true, Ordering::Release);
        } else if command == TOGGLE
            && matches!(self.status(), CaptureStatus::Ready | CaptureStatus::Active)
        {
            // The callback must also check connection state under the core lock.
            self.emit(CaptureEvent::Toggle);
        }
    }

    fn refresh(&mut self, force: bool) -> Option<DisplayStatus> {
        if !force
            && self
                .refreshed_at
                .is_some_and(|at| at.elapsed() < Duration::from_millis(100))
        {
            return None;
        }
        self.refreshed_at = Some(Instant::now());
        let current = self.status();
        let display = display_status(current, &mut self.connected_once);
        if self.displayed == Some(display) {
            return None;
        }
        self.displayed = Some(display);
        Some(display)
    }
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

unsafe extern "system" fn window_proc<F: FnMut(CaptureEvent), S: FnMut() -> CaptureStatus>(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
    }
    // Native painting/text updates may synchronously reenter the window proc.
    // They do not need the capture state and must not trip its borrow guard.
    if !matches!(
        message,
        WM_INPUT | WM_HOTKEY | WM_COMMAND | WM_CLOSE | WM_TIMER
    ) {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const CallbackState<F, S>;
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
            WM_HOTKEY => state.command(wparam),
            WM_COMMAND if (wparam >> 16) == 0 => state.command(wparam & 0xffff),
            WM_CLOSE => state.command(STOP),
            WM_TIMER => {}
            _ => {}
        }
        let display = state.refresh(message != WM_TIMER);
        drop(state);
        if let (Some(controls), Some(display)) = (slot.controls.get(), display) {
            controls.render(display);
        }
        return 0;
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

/// Capture-only compatibility entry point. The window cannot arm sharing.
pub fn run_capture<F: FnMut(CaptureEvent)>(
    on_event: F,
    stop: Arc<AtomicBool>,
    duration: Option<Duration>,
) -> Result<CaptureStats, String> {
    run_capture_with_status(on_event, || CaptureStatus::CaptureOnly, stop, duration)
}

/// Ctrl+Alt+Space requests Toggle; Ctrl+Alt+Escape emits Stop and ends capture.
/// A visible control window provides the same actions and real session status.
/// External stop is checked between events with a 10 ms idle wakeup. Callbacks
/// only collect data; they must not perform network or blocking I/O. Filtering
/// device-less packets is not proof against all virtual/driver input sources.
pub fn run_capture_with_status<F: FnMut(CaptureEvent), S: FnMut() -> CaptureStatus>(
    on_event: F,
    status_fn: S,
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
            status_fn,
            connected_once: false,
            displayed: None,
            refreshed_at: None,
            stop: stop.clone(),
            stats: CaptureStats::default(),
            error: None,
        }),
        controls: Cell::new(None),
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
        lpfnWndProc: Some(window_proc::<F, S>),
        hInstance: resources.instance,
        lpszClassName: resources.name.as_ptr(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: (COLOR_WINDOW + 1) as usize as _,
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
            wide("Crossflow · 鼠标共享").as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            460,
            278,
            null_mut(),
            null_mut(),
            resources.instance,
            (&*state as *const CallbackState<F, S>).cast(),
        )
    };
    if resources.hwnd.is_null() {
        return Err(win_error("CreateWindowExW"));
    }
    let make_control = |class: &str, text: &str, style, x, y, width, height, id: usize| {
        let control = unsafe {
            CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                width,
                height,
                resources.hwnd,
                id as _,
                resources.instance,
                null(),
            )
        };
        if control.is_null() {
            return Err(win_error("CreateWindowExW(control)"));
        }
        unsafe {
            SendMessageW(
                control,
                WM_SETFONT,
                GetStockObject(DEFAULT_GUI_FONT) as usize,
                1,
            );
        }
        Ok(control)
    };
    let status = make_control("STATIC", "等待连接", 0, 20, 20, 410, 28, 10)?;
    let lifetime = duration.map_or_else(
        || "关闭此窗口也会安全停止。".to_owned(),
        |duration| format!("本次试验将在 {:.0} 秒后自动结束。", duration.as_secs_f64()),
    );
    let instructions = format!(
        "Ctrl + Alt + 空格：开始 / 暂停\r\nCtrl + Alt + Esc：停止并退出\r\n仅共享移动，不转发点击；本机光标仍会移动。\r\n{lifetime}"
    );
    make_control("STATIC", &instructions, 0, 20, 60, 410, 100, 11)?;
    let toggle = make_control(
        "BUTTON",
        "开始共享",
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        20,
        180,
        180,
        34,
        TOGGLE,
    )?;
    make_control(
        "BUTTON",
        "停止并退出",
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        220,
        180,
        180,
        34,
        STOP,
    )?;
    let controls = Controls { status, toggle };
    state.controls.set(Some(controls));
    let display = state.data.borrow_mut().refresh(true);
    if let Some(display) = display {
        controls.render(display);
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
    for (id, key) in [(TOGGLE as i32, VK_SPACE), (STOP as i32, VK_ESCAPE)] {
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
                "{}; check desktop permissions or a conflicting Ctrl+Alt+{} binding",
                win_error("RegisterHotKey"),
                if id == TOGGLE as i32 {
                    "Space"
                } else {
                    "Escape"
                }
            ));
        }
        resources.hotkeys.push(id);
    }
    resources.timer = unsafe { SetTimer(resources.hwnd, 1, 10, None) };
    if resources.timer == 0 {
        return Err(win_error("SetTimer"));
    }
    // Display once without taking focus; never force activation on status updates.
    unsafe {
        ShowWindow(resources.hwnd, SW_SHOWNOACTIVATE);
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
            status_fn: || CaptureStatus::Ready,
            connected_once: false,
            displayed: None,
            refreshed_at: None,
            stop: stop.clone(),
            stats: CaptureStats::default(),
            error: None,
        };
        state.emit(CaptureEvent::Toggle);
        assert!(stop.load(Ordering::Acquire));
        assert!(state.error.is_some());
    }

    #[test]
    fn connection_history_changes_status_without_arming() {
        let mut seen = false;
        assert_eq!(
            display_status(CaptureStatus::Disconnected, &mut seen),
            DisplayStatus::Waiting
        );
        assert_eq!(
            display_status(CaptureStatus::Ready, &mut seen),
            DisplayStatus::Ready
        );
        assert_eq!(
            display_status(CaptureStatus::Active, &mut seen),
            DisplayStatus::Active
        );
        assert_eq!(
            display_status(CaptureStatus::Disconnected, &mut seen),
            DisplayStatus::Disconnected
        );
        assert_eq!(
            display_status(CaptureStatus::Ready, &mut seen),
            DisplayStatus::Ready
        );
    }

    #[test]
    fn toggle_uses_current_connection_and_close_stops() {
        let status = Cell::new(CaptureStatus::Disconnected);
        let events = RefCell::new(Vec::new());
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = State {
            callback: |event| events.borrow_mut().push(event),
            status_fn: || status.get(),
            connected_once: false,
            displayed: None,
            refreshed_at: None,
            stop: stop.clone(),
            stats: CaptureStats::default(),
            error: None,
        };
        state.command(TOGGLE);
        assert!(events.borrow().is_empty());
        status.set(CaptureStatus::Ready);
        state.command(TOGGLE);
        assert!(matches!(events.borrow().as_slice(), [CaptureEvent::Toggle]));
        status.set(CaptureStatus::Disconnected);
        state.command(TOGGLE);
        assert_eq!(events.borrow().len(), 1);
        state.command(STOP);
        assert!(stop.load(Ordering::Acquire));
        assert!(matches!(events.borrow().last(), Some(CaptureEvent::Stop)));
        state.command(TOGGLE);
        assert_eq!(events.borrow().len(), 2);
    }
}
