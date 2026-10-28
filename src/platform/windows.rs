//! Windows Raw Input capture. Compatibility entry points capture movement only;
//! the explicit full-input entry point also captures buttons, wheel and keys.
//! Local input is never suppressed or confined. Callbacks must be nonblocking.

use crate::input::Button;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
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
use windows_sys::Win32::Foundation::{
    ERROR_HOTKEY_ALREADY_REGISTERED, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{COLOR_WINDOW, DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetAsyncKeyState, MAPVK_VK_TO_VSC_EX, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MapVirtualKeyW, RegisterHotKey, UnregisterHotKey, VK_ESCAPE, VK_SPACE,
};
use windows_sys::Win32::UI::Input::{
    GetRawInputData, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE, RID_INPUT,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeyStatus {
    Registered,
    Conflict,
}

/// Only a known ownership conflict can degrade to buttons. Preserve every other
/// failure, including zero/unknown error codes, rather than masking OS failures.
/// Track successful registrations immediately so later failures still clean up.
fn record_hotkey_registration(
    id: i32,
    shortcut: &str,
    result: Result<(), u32>,
    registered: &mut Vec<i32>,
) -> Result<HotkeyStatus, String> {
    match result {
        Ok(()) => {
            registered.push(id);
            Ok(HotkeyStatus::Registered)
        }
        Err(ERROR_HOTKEY_ALREADY_REGISTERED) => Ok(HotkeyStatus::Conflict),
        Err(code) => Err(format!(
            "RegisterHotKey({shortcut}) failed: {} (Win32 {code})",
            std::io::Error::from_raw_os_error(code as i32)
        )),
    }
}

fn hotkey_instructions(toggle: HotkeyStatus, stop: HotkeyStatus) -> String {
    let toggle = match toggle {
        HotkeyStatus::Registered => "Ctrl + Alt + 空格：开始 / 暂停",
        HotkeyStatus::Conflict => "Ctrl + Alt + 空格：被占用，不可用；请使用按钮",
    };
    let stop = match stop {
        HotkeyStatus::Registered => "Ctrl + Alt + Esc：停止并退出",
        HotkeyStatus::Conflict => "Ctrl + Alt + Esc：被占用，不可用；请使用按钮",
    };
    format!("{toggle}\r\n{stop}\r\n随时点击“停止并退出”或关闭此窗口即可停止。")
}

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
            DisplayStatus::Active => ("正在共享输入", "暂停共享", true),
            DisplayStatus::Disconnected => ("断线暂停 · 重连后请手动开始共享", "开始共享", false),
        };
        unsafe {
            SetWindowTextW(self.status, wide(text).as_ptr());
            SetWindowTextW(self.toggle, wide(button).as_ptr());
            EnableWindow(self.toggle, i32::from(enabled));
        }
    }
}

#[derive(Clone, Debug)]
pub enum CaptureEvent {
    /// Relative device counts, before Windows pointer acceleration.
    Delta {
        dx: i32,
        dy: i32,
        captured_at: Instant,
    },
    /// Initial physical held state; emitted only by the full-input entry point.
    Snapshot {
        buttons: Vec<Button>,
        keys: Vec<u16>,
    },
    Button {
        button: Button,
        down: bool,
        captured_at: Instant,
    },
    Key {
        usage: u16,
        down: bool,
        captured_at: Instant,
    },
    Scroll {
        horizontal: i32,
        vertical: i32,
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
    pub keyboard: u64,
    pub buttons: u64,
    pub wheel: u64,
    pub unsupported_keys: u64,
    pub overruns: u64,
    pub local_shortcuts: u64,
    pub elapsed: Duration,
}

// Scan Set 1 to HID page 0x07. This is a physical-position map, not text.
// Microsoft reference: https://learn.microsoft.com/windows/win32/inputdev/about-keyboard-input
const SCAN_TO_HID: &[(u16, u16)] = &[
    (0x01, 0x29),
    (0x02, 0x1e),
    (0x03, 0x1f),
    (0x04, 0x20),
    (0x05, 0x21),
    (0x06, 0x22),
    (0x07, 0x23),
    (0x08, 0x24),
    (0x09, 0x25),
    (0x0a, 0x26),
    (0x0b, 0x27),
    (0x0c, 0x2d),
    (0x0d, 0x2e),
    (0x0e, 0x2a),
    (0x0f, 0x2b),
    (0x10, 0x14),
    (0x11, 0x1a),
    (0x12, 0x08),
    (0x13, 0x15),
    (0x14, 0x17),
    (0x15, 0x1c),
    (0x16, 0x18),
    (0x17, 0x0c),
    (0x18, 0x12),
    (0x19, 0x13),
    (0x1a, 0x2f),
    (0x1b, 0x30),
    (0x1c, 0x28),
    (0x1d, 0xe0),
    (0x1e, 0x04),
    (0x1f, 0x16),
    (0x20, 0x07),
    (0x21, 0x09),
    (0x22, 0x0a),
    (0x23, 0x0b),
    (0x24, 0x0d),
    (0x25, 0x0e),
    (0x26, 0x0f),
    (0x27, 0x33),
    (0x28, 0x34),
    (0x29, 0x35),
    (0x2a, 0xe1),
    (0x2b, 0x31),
    (0x2c, 0x1d),
    (0x2d, 0x1b),
    (0x2e, 0x06),
    (0x2f, 0x19),
    (0x30, 0x05),
    (0x31, 0x11),
    (0x32, 0x10),
    (0x33, 0x36),
    (0x34, 0x37),
    (0x35, 0x38),
    (0x36, 0xe5),
    (0x37, 0x55),
    (0x38, 0xe2),
    (0x39, 0x2c),
    (0x3a, 0x39),
    (0x3b, 0x3a),
    (0x3c, 0x3b),
    (0x3d, 0x3c),
    (0x3e, 0x3d),
    (0x3f, 0x3e),
    (0x40, 0x3f),
    (0x41, 0x40),
    (0x42, 0x41),
    (0x43, 0x42),
    (0x44, 0x43),
    (0x45, 0x53),
    (0x46, 0x47),
    (0x47, 0x5f),
    (0x48, 0x60),
    (0x49, 0x61),
    (0x4a, 0x56),
    (0x4b, 0x5c),
    (0x4c, 0x5d),
    (0x4d, 0x5e),
    (0x4e, 0x57),
    (0x4f, 0x59),
    (0x50, 0x5a),
    (0x51, 0x5b),
    (0x52, 0x62),
    (0x53, 0x63),
    (0x54, 0x46),
    (0x56, 0x64),
    (0x57, 0x44),
    (0x58, 0x45),
    (0xe01c, 0x58),
    (0xe01d, 0xe4),
    (0xe035, 0x54),
    (0xe037, 0x46),
    (0xe038, 0xe6),
    (0xe045, 0x53),
    (0xe047, 0x4a),
    (0xe048, 0x52),
    (0xe049, 0x4b),
    (0xe04b, 0x50),
    (0xe04d, 0x4f),
    (0xe04f, 0x4d),
    (0xe050, 0x51),
    (0xe051, 0x4e),
    (0xe052, 0x49),
    (0xe053, 0x4c),
    (0xe05b, 0xe3),
    (0xe05c, 0xe7),
    (0xe05d, 0x65),
];
fn scan_to_hid(scan: u16) -> Option<u16> {
    SCAN_TO_HID
        .iter()
        .find_map(|&(code, usage)| (code == scan).then_some(usage))
}

#[derive(Debug, PartialEq, Eq)]
enum InputPacket {
    Mouse {
        dx: i32,
        dy: i32,
        flags: u16,
        wheel: i32,
    },
    Key {
        usage: u16,
        down: bool,
    },
    Absolute,
    DeviceLess,
    Other,
    UnsupportedKey,
    Malformed,
    Overrun,
}

fn decode_input(raw: &RAWINPUT, bytes: u32, map_vk: impl Fn(u16) -> u16) -> InputPacket {
    if bytes < size_of::<RAWINPUTHEADER>() as u32 {
        return InputPacket::Malformed;
    }
    let payload = match raw.header.dwType {
        0 => size_of::<RAWMOUSE>(),
        1 => size_of::<RAWKEYBOARD>(),
        _ => return InputPacket::Other,
    };
    let required = (size_of::<RAWINPUTHEADER>() + payload) as u32;
    if bytes < required || raw.header.dwSize < required || raw.header.dwSize > bytes {
        return InputPacket::Malformed;
    }
    if raw.header.hDevice.is_null() {
        return InputPacket::DeviceLess;
    }
    if raw.header.dwType == 0 {
        let mouse = unsafe { raw.data.mouse };
        if mouse.usFlags & 1 != 0 {
            return InputPacket::Absolute;
        }
        let buttons = unsafe { mouse.Anonymous.Anonymous };
        if buttons.usButtonFlags & !0x0fff != 0 {
            return InputPacket::Malformed;
        }
        return InputPacket::Mouse {
            dx: mouse.lLastX,
            dy: mouse.lLastY,
            flags: buttons.usButtonFlags,
            wheel: buttons.usButtonData as i16 as i32,
        };
    }
    let keyboard = unsafe { raw.data.keyboard };
    if keyboard.MakeCode == 0xff {
        return InputPacket::Overrun;
    }
    if keyboard.Reserved != 0 || keyboard.Flags & !7 != 0 || keyboard.Flags & 6 == 6 {
        return InputPacket::Malformed;
    }
    if keyboard.VKey >= 0xff || keyboard.VKey == 0x13 {
        return InputPacket::UnsupportedKey;
    }
    // E1 Pause has no ordinary, consistent break sequence. Do not manufacture
    // a held key from it. Multimedia/vendor/IME keys are also unsupported here.
    if keyboard.Flags & 4 != 0 {
        return InputPacket::UnsupportedKey;
    }
    let scan = if keyboard.MakeCode == 0 {
        map_vk(keyboard.VKey)
    } else {
        if keyboard.MakeCode > 0x7f {
            return InputPacket::Malformed;
        }
        keyboard.MakeCode | if keyboard.Flags & 2 != 0 { 0xe000 } else { 0 }
    };
    match scan_to_hid(scan) {
        Some(usage) => InputPacket::Key {
            usage,
            down: keyboard.Flags & 1 == 0,
        },
        None => InputPacket::UnsupportedKey,
    }
}

const BUTTON_FLAGS: [(Button, u16, u16); 5] = [
    (Button::Left, 1, 2),
    (Button::Right, 4, 8),
    (Button::Middle, 16, 32),
    (Button::Back, 64, 128),
    (Button::Forward, 256, 512),
];
const BUTTON_VKS: [(Button, i32); 5] = [
    (Button::Left, 1),
    (Button::Right, 2),
    (Button::Middle, 4),
    (Button::Back, 5),
    (Button::Forward, 6),
];

fn physical_snapshot(
    mut held: impl FnMut(i32) -> bool,
    map_vk: impl Fn(u16) -> u16,
) -> (Vec<Button>, Vec<u16>) {
    let buttons = BUTTON_VKS
        .iter()
        .filter_map(|&(button, vk)| held(vk).then_some(button))
        .collect();
    let mut keys = BTreeSet::new();
    for vk in 8..=254u16 {
        // Skip aggregate Shift/Ctrl/Alt: the individual sides are sampled below.
        if matches!(vk, 0x10..=0x13) || !held(vk as i32) {
            continue;
        }
        if let Some(usage) = scan_to_hid(map_vk(vk)) {
            keys.insert(usage);
        }
    }
    (buttons, keys.into_iter().collect())
}

#[derive(Default)]
struct ShortcutFilter {
    held: BTreeSet<u16>,
    suppressed: BTreeSet<u16>,
}
impl ShortcutFilter {
    fn observe(&mut self, usage: u16, down: bool) -> bool {
        let chord = (self.held.contains(&0xe0) || self.held.contains(&0xe4))
            && (self.held.contains(&0xe2) || self.held.contains(&0xe6));
        let suppress = if down && chord && matches!(usage, 0x2c | 0x29) {
            self.suppressed.insert(usage);
            true
        } else if !down {
            self.suppressed.remove(&usage)
        } else {
            self.suppressed.contains(&usage)
        };
        if down {
            self.held.insert(usage);
        } else {
            self.held.remove(&usage);
        }
        suppress
    }
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
    full_input: bool,
    shortcuts: ShortcutFilter,
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
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        // A panic may not unwind over the Windows callback boundary.
        if catch_unwind(AssertUnwindSafe(|| (self.callback)(event))).is_err() {
            self.error = Some("capture callback panicked; stopped safely".into());
            self.stop.store(true, Ordering::Release);
        }
    }
    fn read(&mut self, handle: LPARAM, captured_at: Instant) {
        let mut raw = MaybeUninit::<RAWINPUT>::zeroed();
        let mut size = size_of::<RAWINPUT>() as u32;
        // RAWINPUT covers the registered mouse and optional keyboard payloads;
        // unexpected oversized packets fail rather than reading partial data.
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
        if self.full_input {
            let packet = decode_input(&raw, bytes, |vk| unsafe {
                MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) as u16
            });
            self.input_packet(packet, captured_at);
            return;
        }
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
    fn input_packet(&mut self, packet: InputPacket, captured_at: Instant) {
        match packet {
            InputPacket::Mouse {
                dx,
                dy,
                flags,
                wheel,
            } => {
                // A raw packet has one timestamp and no finer ordering. Use an
                // explicit movement -> button transitions -> wheel convention.
                if dx != 0 || dy != 0 {
                    self.stats.motion += 1;
                    self.emit(CaptureEvent::Delta {
                        dx,
                        dy,
                        captured_at,
                    });
                } else {
                    self.stats.zero_motion += 1;
                }
                for (button, down_flag, up_flag) in BUTTON_FLAGS {
                    for (flag, down) in [(down_flag, true), (up_flag, false)] {
                        if flags & flag != 0 {
                            self.stats.buttons += 1;
                            self.emit(CaptureEvent::Button {
                                button,
                                down,
                                captured_at,
                            });
                        }
                    }
                }
                if flags & 0x0c00 != 0 && wheel != 0 {
                    self.stats.wheel += 1;
                    self.emit(CaptureEvent::Scroll {
                        horizontal: if flags & 0x0800 != 0 { wheel } else { 0 },
                        vertical: if flags & 0x0400 != 0 { wheel } else { 0 },
                        captured_at,
                    });
                }
            }
            InputPacket::Key { usage, down } => {
                self.stats.keyboard += 1;
                if self.shortcuts.observe(usage, down) {
                    self.stats.local_shortcuts += 1;
                } else {
                    self.emit(CaptureEvent::Key {
                        usage,
                        down,
                        captured_at,
                    });
                }
            }
            InputPacket::Absolute => self.stats.absolute += 1,
            InputPacket::DeviceLess => self.stats.device_less += 1,
            InputPacket::Other => self.stats.non_mouse += 1,
            InputPacket::UnsupportedKey => self.stats.unsupported_keys += 1,
            InputPacket::Malformed | InputPacket::Overrun => {
                if packet == InputPacket::Overrun {
                    self.stats.overruns += 1;
                } else {
                    self.stats.malformed += 1;
                }
                self.error = Some("invalid or overrun Raw Input packet; stopped safely".into());
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
    raw_keyboard: bool,
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
            if self.raw_keyboard {
                let remove = RAWINPUTDEVICE {
                    usUsagePage: 1,
                    usUsage: 6,
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
/// Conflicting hotkeys are marked unavailable; buttons/window-close still work.
/// External stop is checked between events with a 10 ms idle wakeup. Callbacks
/// only collect data; they must not perform network or blocking I/O. Filtering
/// device-less packets is not proof against all virtual/driver input sources.
pub fn run_capture_with_status<F: FnMut(CaptureEvent), S: FnMut() -> CaptureStatus>(
    on_event: F,
    status_fn: S,
    stop: Arc<AtomicBool>,
    duration: Option<Duration>,
) -> Result<CaptureStats, String> {
    run_capture_mode(on_event, status_fn, stop, duration, false)
}

/// Experimental physical mouse/keyboard capture. Local events also reach local
/// applications. Snapshot, Button, Key and Scroll are emitted only in this mode.
/// The caller must seed physical state before enabling its shared input queue.
pub fn run_input_capture_with_status<F: FnMut(CaptureEvent), S: FnMut() -> CaptureStatus>(
    on_event: F,
    status_fn: S,
    stop: Arc<AtomicBool>,
    duration: Option<Duration>,
) -> Result<CaptureStats, String> {
    run_capture_mode(on_event, status_fn, stop, duration, true)
}

fn run_capture_mode<F: FnMut(CaptureEvent), S: FnMut() -> CaptureStatus>(
    on_event: F,
    status_fn: S,
    stop: Arc<AtomicBool>,
    duration: Option<Duration>,
    full_input: bool,
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
            full_input,
            shortcuts: ShortcutFilter::default(),
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
        raw_keyboard: false,
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
            320,
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
    let mode_note = if full_input {
        "实验模式：键盘/点击也会转发；Windows 本地输入仍生效。"
    } else {
        "仅共享移动，不转发点击；本机光标仍会移动。"
    };
    // Fill this only after registration results are known. Never advertise a
    // shortcut as usable when another application owns it.
    let instruction_control = make_control("STATIC", "", 0, 20, 60, 410, 140, 11)?;
    let toggle = make_control(
        "BUTTON",
        "开始共享",
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        20,
        220,
        180,
        34,
        TOGGLE,
    )?;
    make_control(
        "BUTTON",
        "停止并退出",
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        220,
        220,
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
    if full_input {
        let keyboard = RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 6,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: resources.hwnd,
        };
        if unsafe { RegisterRawInputDevices(&keyboard, 1, size_of::<RAWINPUTDEVICE>() as u32) } == 0
        {
            return Err(win_error("RegisterRawInputDevices(keyboard)"));
        }
        resources.raw_keyboard = true;
    }
    let mut hotkey_states = [HotkeyStatus::Conflict; 2];
    for (state, (id, key, shortcut)) in hotkey_states.iter_mut().zip([
        (TOGGLE as i32, VK_SPACE, "Ctrl+Alt+Space"),
        (STOP as i32, VK_ESCAPE, "Ctrl+Alt+Escape"),
    ]) {
        let result = if unsafe {
            RegisterHotKey(
                resources.hwnd,
                id,
                MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
                key as u32,
            )
        } == 0
        {
            // Read the error exactly once before any formatting or other APIs.
            Err(unsafe { GetLastError() })
        } else {
            Ok(())
        };
        *state = record_hotkey_registration(id, shortcut, result, &mut resources.hotkeys)?;
        if *state == HotkeyStatus::Conflict {
            eprintln!(
                "{shortcut} is unavailable (already registered by another application). Use the Crossflow buttons; Stop or closing its window always exits."
            );
        }
    }
    let instructions = format!(
        "{}\r\n{mode_note}\r\n{lifetime}",
        hotkey_instructions(hotkey_states[0], hotkey_states[1]),
    );
    if unsafe { SetWindowTextW(instruction_control, wide(&instructions).as_ptr()) } == 0 {
        return Err(win_error("SetWindowTextW(shortcut instructions)"));
    }
    if full_input {
        // Registration is already live: messages continue queuing while this
        // bounded startup snapshot is sampled. Never use the unreliable low
        // "pressed since last call" bit, and never log the actual held keys.
        let (buttons, keys) = physical_snapshot(
            |vk| unsafe { GetAsyncKeyState(vk) < 0 },
            |vk| unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) as u16 },
        );
        let mut state = state.data.borrow_mut();
        state.shortcuts.held.extend(keys.iter().copied());
        state.emit(CaptureEvent::Snapshot { buttons, keys });
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

    #[test]
    fn hotkey_conflicts_keep_only_successful_registrations_for_cleanup() {
        for toggle_conflict in [false, true] {
            for stop_conflict in [false, true] {
                let mut registered = Vec::new();
                let mut states = Vec::new();
                for (id, conflict) in [(TOGGLE, toggle_conflict), (STOP, stop_conflict)] {
                    states.push(
                        record_hotkey_registration(
                            id as i32,
                            "test shortcut",
                            if conflict {
                                Err(ERROR_HOTKEY_ALREADY_REGISTERED)
                            } else {
                                Ok(())
                            },
                            &mut registered,
                        )
                        .unwrap(),
                    );
                }
                let expected: Vec<i32> = [(TOGGLE, toggle_conflict), (STOP, stop_conflict)]
                    .into_iter()
                    .filter_map(|(id, conflict)| (!conflict).then_some(id as i32))
                    .collect();
                assert_eq!(registered, expected);
                assert_eq!(states[0] == HotkeyStatus::Conflict, toggle_conflict);
                assert_eq!(states[1] == HotkeyStatus::Conflict, stop_conflict);
                let instructions = hotkey_instructions(states[0], states[1]);
                assert_eq!(
                    instructions.contains("空格：被占用，不可用"),
                    toggle_conflict
                );
                assert_eq!(instructions.contains("Esc：被占用，不可用"), stop_conflict);
                assert_eq!(instructions.contains("空格：开始 / 暂停"), !toggle_conflict);
                assert_eq!(instructions.contains("Esc：停止并退出"), !stop_conflict);
                assert!(instructions.contains("点击“停止并退出”或关闭此窗口"));
            }
        }
    }

    #[test]
    fn non_conflict_hotkey_errors_fail_without_losing_cleanup_or_error_code() {
        // Includes absent/unknown codes, access denied, invalid parameters/window,
        // and resource exhaustion. Only the explicit conflict code may degrade.
        for code in [0, 5, 87, 1400, 1450, u32::MAX] {
            let mut registered = Vec::new();
            record_hotkey_registration(TOGGLE as i32, "Ctrl+Alt+Space", Ok(()), &mut registered)
                .unwrap();
            let error = record_hotkey_registration(
                STOP as i32,
                "Ctrl+Alt+Escape",
                Err(code),
                &mut registered,
            )
            .unwrap_err();
            assert!(error.contains("RegisterHotKey(Ctrl+Alt+Escape)"));
            assert!(error.contains(&format!("(Win32 {code})")));
            assert_eq!(registered, vec![TOGGLE as i32]);
        }
    }

    #[test]
    fn unavailable_shortcuts_do_not_arm_or_disable_local_stop() {
        for status in [
            CaptureStatus::Disconnected,
            CaptureStatus::Ready,
            CaptureStatus::Active,
        ] {
            let events = RefCell::new(Vec::new());
            let stop = Arc::new(AtomicBool::new(false));
            let mut state = State {
                callback: |event| events.borrow_mut().push(event),
                status_fn: || status,
                connected_once: false,
                displayed: None,
                refreshed_at: None,
                stop: stop.clone(),
                stats: CaptureStats::default(),
                error: None,
                full_input: false,
                shortcuts: ShortcutFilter::default(),
            };
            let mut registered = Vec::new();
            for id in [TOGGLE, STOP] {
                record_hotkey_registration(
                    id as i32,
                    "test shortcut",
                    Err(ERROR_HOTKEY_ALREADY_REGISTERED),
                    &mut registered,
                )
                .unwrap();
            }
            state.refresh(true);
            assert!(
                events.borrow().is_empty(),
                "registration/refresh must not arm"
            );
            assert!(!stop.load(Ordering::Acquire));
            // Both the Stop button and WM_CLOSE route to this exact command,
            // independent of any successful global hotkey registration.
            state.command(STOP);
            assert!(stop.load(Ordering::Acquire));
            assert!(matches!(events.borrow().as_slice(), [CaptureEvent::Stop]));
        }
    }

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
            full_input: false,
            shortcuts: ShortcutFilter::default(),
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
            full_input: false,
            shortcuts: ShortcutFilter::default(),
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

    fn keyboard(scan: u16, flags: u16) -> RAWINPUT {
        let mut raw = packet(0, 0);
        raw.header.dwType = 1;
        raw.header.dwSize = (size_of::<RAWINPUTHEADER>() + size_of::<RAWKEYBOARD>()) as u32;
        raw.data.keyboard = RAWKEYBOARD {
            MakeCode: scan,
            Flags: flags,
            VKey: 0x41,
            ..Default::default()
        };
        raw
    }
    #[test]
    fn physical_keyboard_mapping_preserves_sides_navigation_and_fkeys() {
        for (scan, usage) in [
            (0x1e, 0x04),
            (0x10, 0x14),
            (0x3b, 0x3a),
            (0x58, 0x45),
            (0x1d, 0xe0),
            (0xe01d, 0xe4),
            (0x2a, 0xe1),
            (0x36, 0xe5),
            (0x38, 0xe2),
            (0xe038, 0xe6),
            (0xe05b, 0xe3),
            (0xe05c, 0xe7),
            (0xe048, 0x52),
            (0x48, 0x60),
            (0xe01c, 0x58),
            (0x1c, 0x28),
            (0xe045, 0x53),
        ] {
            assert_eq!(scan_to_hid(scan), Some(usage));
        }
        assert_eq!(scan_to_hid(0xe12d), None);
        assert_eq!(
            scan_to_hid(0xe02a),
            None,
            "fake extended Shift is not a real modifier"
        );
        let scans: BTreeSet<_> = SCAN_TO_HID.iter().map(|&(scan, _)| scan).collect();
        assert_eq!(scans.len(), SCAN_TO_HID.len());
    }
    #[test]
    fn keyboard_decode_filters_source_overrun_and_unsupported_e1() {
        let raw = keyboard(0x1d, 3);
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0),
            InputPacket::Key {
                usage: 0xe4,
                down: false
            }
        );
        let mut raw = keyboard(0x1e, 0);
        raw.header.hDevice = null_mut();
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0),
            InputPacket::DeviceLess
        );
        let raw = keyboard(0xff, 0);
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0),
            InputPacket::Overrun
        );
        let raw = keyboard(0x1d, 4);
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0),
            InputPacket::UnsupportedKey
        );
        let mut pause = keyboard(0x45, 0);
        pause.data.keyboard.VKey = 0x13;
        assert_eq!(
            decode_input(&pause, pause.header.dwSize, |_| 0),
            InputPacket::UnsupportedKey
        );
        let raw = keyboard(0x1d, 6);
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0),
            InputPacket::Malformed
        );
        let raw = keyboard(0, 0);
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0xe01c),
            InputPacket::Key {
                usage: 0x58,
                down: true
            }
        );
        assert_eq!(
            decode_input(&raw, raw.header.dwSize - 1, |_| 0),
            InputPacket::Malformed
        );
    }
    #[test]
    fn mouse_packet_keeps_buttons_and_signed_subnotch_scroll() {
        let mut raw = packet(12, -7);
        raw.data.mouse.Anonymous.Anonymous.usButtonFlags = 1 | 2 | 0x0800;
        raw.data.mouse.Anonymous.Anonymous.usButtonData = (-30i16) as u16;
        assert_eq!(
            decode_input(&raw, raw.header.dwSize, |_| 0),
            InputPacket::Mouse {
                dx: 12,
                dy: -7,
                flags: 1 | 2 | 0x0800,
                wheel: -30
            }
        );
        assert_eq!(
            decode(&raw, raw.header.dwSize),
            Packet::Motion(12, -7),
            "compatibility decoder remains movement-only"
        );
    }
    #[test]
    fn snapshot_uses_physical_buttons_and_distinct_modifier_sides() {
        let (buttons, keys) = physical_snapshot(
            |vk| matches!(vk, 1 | 6 | 0x10 | 0xa0 | 0xa1 | 0xa3 | 0xa5),
            |vk| match vk {
                0x10 => panic!("aggregate modifier must be ignored"),
                0xa0 => 0x2a,
                0xa1 => 0x36,
                0xa3 => 0xe01d,
                0xa5 => 0xe038,
                _ => 0,
            },
        );
        assert_eq!(buttons, vec![Button::Left, Button::Forward]);
        assert_eq!(keys, vec![0xe1, 0xe4, 0xe5, 0xe6]);
    }
    #[test]
    fn shortcut_trigger_up_remains_suppressed_after_modifiers_release() {
        for trigger in [0x2c, 0x29] {
            let mut filter = ShortcutFilter::default();
            assert!(!filter.observe(0xe0, true));
            assert!(!filter.observe(0xe6, true));
            assert!(filter.observe(trigger, true));
            assert!(!filter.observe(0xe0, false));
            assert!(!filter.observe(0xe6, false));
            assert!(
                filter.observe(trigger, true),
                "typematic trigger remains local"
            );
            assert!(filter.observe(trigger, false));
            assert!(
                !filter.observe(trigger, true),
                "ordinary next press is forwarded"
            );
            assert!(!filter.observe(trigger, false));
        }
    }
    #[test]
    fn full_packet_callbacks_are_ordered_and_overrun_stops() {
        let events = RefCell::new(Vec::new());
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = State {
            callback: |event| events.borrow_mut().push(event),
            status_fn: || CaptureStatus::Ready,
            connected_once: false,
            displayed: None,
            refreshed_at: None,
            stop: stop.clone(),
            stats: CaptureStats::default(),
            error: None,
            full_input: true,
            shortcuts: ShortcutFilter::default(),
        };
        let at = Instant::now();
        state.input_packet(
            InputPacket::Mouse {
                dx: 3,
                dy: 4,
                flags: 1 | 2 | 0x400,
                wheel: -120,
            },
            at,
        );
        assert!(matches!(events.borrow().as_slice(),[
            CaptureEvent::Delta {dx:3,dy:4,captured_at:a},
            CaptureEvent::Button {button:Button::Left,down:true,captured_at:b},
            CaptureEvent::Button {button:Button::Left,down:false,captured_at:c},
            CaptureEvent::Scroll {horizontal:0,vertical:-120,captured_at:d},
        ] if *a==at && *b==at && *c==at && *d==at));
        state.input_packet(InputPacket::Overrun, at);
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(state.stats.overruns, 1);
        state.emit(CaptureEvent::Key {
            usage: 0x04,
            down: true,
            captured_at: at,
        });
        assert_eq!(
            events.borrow().len(),
            4,
            "nothing emitted after fail-closed stop"
        );
    }
}
