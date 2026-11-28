//! Windows receiver adapter. Coordinates are physical virtual-desktop pixels.
//! SendInput is subject to UIPI; successful insertion does not prove that the
//! foreground application consumed input. Secure desktops are not supported.

use super::{SCAN_TO_HID, routed::DpiScope};
use crate::{
    input::Button,
    session::{MouseSink, NativeDisplay, validate_displays},
};
use anyhow::{Result, bail, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    ptr::null_mut,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_ACCESS_DENIED, GetLastError},
    System::{
        RemoteDesktop::{
            WTS_CURRENT_SESSION, WTSActive, WTSConnectState, WTSFreeMemory,
            WTSQuerySessionInformationW,
        },
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_READOBJECTS, DESKTOP_SWITCHDESKTOP, DESKTOP_WRITEOBJECTS,
            GetThreadDesktop, GetUserObjectInformationW, HDESK, OpenInputDesktop, SetThreadDesktop,
            UOI_IO, UOI_NAME,
        },
        Threading::GetCurrentThreadId,
    },
    UI::Input::KeyboardAndMouse::*,
};

// Fits both Raw Input's 32-bit ExtraInformation and INPUT's pointer-sized field.
// This is provenance for loop rejection, never authentication or an input grant.
pub(super) const INPUT_ORIGIN_TAG: usize = 0x4352_4657;

/// Stable FNV-1a of the native Windows display name. Windows can rename a device
/// on hardware changes, so this is not a persistent hardware/EDID identity.
pub fn receiver_display_id(name: &str) -> Result<u32> {
    ensure!(!name.is_empty(), "empty native display name");
    Ok(name.bytes().fold(0x811c_9dc5u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    }))
}

fn map_displays(native: &[super::NativeDisplay]) -> Result<Vec<NativeDisplay>> {
    let mut result = Vec::with_capacity(native.len());
    let mut ids = BTreeSet::new();
    for d in native {
        let id = receiver_display_id(&d.id)?;
        ensure!(ids.insert(id), "native display identity collision");
        result.push(NativeDisplay {
            id,
            x: d.x,
            y: d.y,
            width: d.width,
            height: d.height,
        });
    }
    result.sort_by_key(|d| d.id);
    validate_displays(&result)?;
    bounds(&result)?;
    Ok(result)
}

/// Read-only inventory. Never creates an injector or changes the local cursor.
pub fn receiver_displays() -> Result<Vec<NativeDisplay>> {
    map_displays(&super::displays().map_err(anyhow::Error::msg)?)
}

fn contains(d: &NativeDisplay, x: f64, y: f64) -> bool {
    x.is_finite()
        && y.is_finite()
        && x >= d.x
        && y >= d.y
        && x < d.x + d.width
        && y < d.y + d.height
}

fn bounds(displays: &[NativeDisplay]) -> Result<[i64; 4]> {
    validate_displays(displays)?;
    let mut out = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
    for d in displays {
        let values = [d.x, d.y, d.x + d.width, d.y + d.height];
        ensure!(
            values.iter().all(|v| v.fract() == 0.0
                && *v >= f64::from(i32::MIN)
                && *v <= f64::from(i32::MAX)),
            "native desktop geometry is not integral or is out of range"
        );
        out[0] = out[0].min(values[0] as i64);
        out[1] = out[1].min(values[1] as i64);
        out[2] = out[2].max(values[2] as i64);
        out[3] = out[3].max(values[3] as i64);
    }
    ensure!(
        (1..=65536).contains(&(out[2] - out[0])) && (1..=65536).contains(&(out[3] - out[1])),
        "virtual desktop exceeds absolute input precision"
    );
    Ok(out)
}

fn absolute(displays: &[NativeDisplay], x: f64, y: f64, id: u32) -> Result<(INPUT, (f64, f64))> {
    let display = displays
        .iter()
        .find(|d| d.id == id)
        .ok_or_else(|| anyhow::anyhow!("target display is unavailable"))?;
    ensure!(
        contains(display, x, y),
        "target position does not belong to expected display"
    );
    // Select the containing physical pixel, then its normalized-bin center.
    // Floor cannot spill a fractional point at the far edge into another screen.
    let (px, py) = (x.floor() as i64, y.floor() as i64);
    let b = bounds(displays)?;
    let normalize = |p: i64, origin: i64, end: i64| {
        (((p - origin) * 2 + 1) * 65536 / (2 * (end - origin))) as i32
    };
    Ok((
        mouse(
            normalize(px, b[0], b[2]),
            normalize(py, b[1], b[3]),
            0,
            MOUSEEVENTF_MOVE
                | MOUSEEVENTF_ABSOLUTE
                | MOUSEEVENTF_VIRTUALDESK
                | MOUSEEVENTF_MOVE_NOCOALESCE,
        ),
        (px as f64, py as f64),
    ))
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: INPUT_ORIGIN_TAG,
            },
        },
    }
}

fn scan(usage: u16) -> Option<u16> {
    // PrintScreen has both a legacy SysRq scan and the canonical E0 form in
    // the capture table. Injection must choose E0 37 explicitly.
    if usage == 0x46 {
        return Some(0xe037);
    }
    SCAN_TO_HID
        .iter()
        .find_map(|&(scan, hid)| (hid == usage).then_some(scan))
}
fn media_vk(usage: u16) -> Option<u16> {
    Some(match usage {
        crate::keymap::HID_BROWSER_HOME => 0xAC,     // VK_BROWSER_HOME
        crate::keymap::HID_MUTE => 0xAD,             // VK_VOLUME_MUTE
        crate::keymap::HID_VOLUME_DOWN => 0xAE,      // VK_VOLUME_DOWN
        crate::keymap::HID_VOLUME_UP => 0xAF,        // VK_VOLUME_UP
        crate::keymap::HID_MEDIA_NEXT => 0xB0,       // VK_MEDIA_NEXT_TRACK
        crate::keymap::HID_MEDIA_PREV => 0xB1,       // VK_MEDIA_PREV_TRACK
        crate::keymap::HID_MEDIA_STOP => 0xB2,       // VK_MEDIA_STOP
        crate::keymap::HID_MEDIA_PLAY_PAUSE => 0xB3, // VK_MEDIA_PLAY_PAUSE
        crate::keymap::HID_LAUNCH_MAIL => 0xB4,      // VK_LAUNCH_MAIL
        crate::keymap::HID_LAUNCH_MEDIA => 0xB5,     // VK_LAUNCH_MEDIA_SELECT
        crate::keymap::HID_LAUNCH_APP1 => 0xB6,      // VK_LAUNCH_APP1 (Explorer)
        crate::keymap::HID_CALCULATOR => 0xB7,       // VK_LAUNCH_APP2 (Calculator)
        _ => return None,
    })
}

/// Editing & navigation control keys that must carry an explicit Virtual-Key (`wVk`)
/// alongside their hardware `wScan` (without `KEYEVENTF_SCANCODE`). Windows TSF and
/// Chinese IMEs (Microsoft Pinyin, Sogou, WeType, QQ Pinyin) often drop or mishandle
/// `wVk = 0` scancode-only Backspace / Enter / Space / Delete events while composing
/// or immediately after committing Chinese text.
fn editing_control_vk(usage: u16) -> Option<(u16, u16, bool)> {
    Some(match usage {
        0x28 => (VK_RETURN, 0x1c, false),
        0x29 => (VK_ESCAPE, 0x01, false),
        0x2a => (VK_BACK, 0x0e, false),
        0x2b => (VK_TAB, 0x0f, false),
        0x2c => (VK_SPACE, 0x39, false),
        0x49 => (VK_INSERT, 0x52, true),
        0x4a => (VK_HOME, 0x47, true),
        0x4b => (VK_PRIOR, 0x49, true),
        0x4c => (VK_DELETE, 0x53, true),
        0x4d => (VK_END, 0x4f, true),
        0x4e => (VK_NEXT, 0x51, true),
        0x4f => (VK_RIGHT, 0x4d, true),
        0x50 => (VK_LEFT, 0x4b, true),
        0x51 => (VK_DOWN, 0x50, true),
        0x52 => (VK_UP, 0x48, true),
        _ => return None,
    })
}

fn keyboard(usage: u16, down: bool) -> Result<INPUT> {
    if let Some(vk) = media_vk(usage) {
        return Ok(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if down { 0 } else { KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: INPUT_ORIGIN_TAG,
                },
            },
        });
    }
    if let Some((vk, scan_code, extended)) = editing_control_vk(usage) {
        return Ok(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan_code,
                    dwFlags: if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }
                        | if down { 0 } else { KEYEVENTF_KEYUP },
                    time: 0,
                    dwExtraInfo: INPUT_ORIGIN_TAG,
                },
            },
        });
    }
    let scan = scan(usage).ok_or_else(|| anyhow::anyhow!("unsupported keyboard usage"))?;
    Ok(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0,
                wScan: scan & 0xff,
                dwFlags: KEYEVENTF_SCANCODE
                    | if scan & 0xe000 != 0 {
                        KEYEVENTF_EXTENDEDKEY
                    } else {
                        0
                    }
                    | if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                dwExtraInfo: INPUT_ORIGIN_TAG,
            },
        },
    })
}
fn button_input(button: Button, down: bool) -> INPUT {
    let (on, off, data) = match button {
        Button::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
        Button::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
        Button::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
        Button::Back => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 1),
        Button::Forward => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, 2),
    };
    mouse(0, 0, data, if down { on } else { off })
}
fn button_vk(button: Button) -> u16 {
    match button {
        Button::Left => VK_LBUTTON,
        Button::Right => VK_RBUTTON,
        Button::Middle => VK_MBUTTON,
        Button::Back => VK_XBUTTON1,
        Button::Forward => VK_XBUTTON2,
    }
}

struct Desktop(HDESK);
impl Drop for Desktop {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseDesktop(self.0);
            }
        }
    }
}
fn desktop_name(handle: HDESK) -> Result<String> {
    ensure!(!handle.is_null(), "local desktop is unavailable");
    let mut name = [0u16; 256];
    let mut needed = 0;
    ensure!(
        unsafe {
            GetUserObjectInformationW(
                handle,
                UOI_NAME,
                name.as_mut_ptr().cast(),
                size_of::<[u16; 256]>() as u32,
                &mut needed,
            )
        } != 0,
        "local desktop cannot be verified"
    );
    let end = name
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(|| anyhow::anyhow!("invalid local desktop name"))?;
    String::from_utf16(&name[..end]).map_err(|_| anyhow::anyhow!("invalid local desktop name"))
}
/// Returns true when the current interactive input desktop is the normal user desktop
/// (`WinSta0\Default`). Returns false when Windows is locked (`WinSta0\Winlogon`),
/// showing a UAC secure desktop, or disconnected.
pub fn is_input_desktop_default() -> bool {
    let input = Desktop(unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) });
    if input.0.is_null() {
        return false;
    }
    desktop_name(input.0)
        .map(|name| name.eq_ignore_ascii_case("Default"))
        .unwrap_or(false)
}

fn desktop_ready() -> Result<()> {
    // Never switch desktops or grant access to Winlogon/UAC. Same-name Default
    // desktops are compared inside this process's window station.
    let current_handle = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    let current = desktop_name(current_handle)?;
    ensure!(
        current.eq_ignore_ascii_case("Default"),
        "input requires the normal interactive desktop"
    );
    let input = Desktop(unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) });
    ensure!(
        desktop_name(input.0)? == current,
        "interactive desktop changed; input stopped"
    );
    let mut buffer = null_mut();
    let mut receives_input = 0i32;
    let mut needed = 0;
    ensure!(
        unsafe {
            GetUserObjectInformationW(
                current_handle,
                UOI_IO,
                (&mut receives_input as *mut i32).cast(),
                size_of::<i32>() as u32,
                &mut needed,
            )
        } != 0
            && receives_input != 0,
        "current desktop is not receiving input"
    );
    let mut bytes = 0;
    ensure!(
        unsafe {
            WTSQuerySessionInformationW(
                null_mut(),
                WTS_CURRENT_SESSION,
                WTSConnectState,
                &mut buffer,
                &mut bytes,
            )
        } != 0,
        "interactive session cannot be verified"
    );
    let active = !buffer.is_null()
        && bytes >= size_of::<i32>() as u32
        && unsafe { std::ptr::read_unaligned(buffer.cast::<i32>()) } == WTSActive;
    if !buffer.is_null() {
        unsafe { WTSFreeMemory(buffer.cast()) };
    }
    ensure!(active, "interactive session is not active");
    Ok(())
}

const PLATFORM_CACHE_TTL: Duration = Duration::from_millis(100);

trait Platform: Send {
    fn invalidate_cache(&self) {}
    fn ready(&self) -> Result<()>;
    fn displays(&self) -> Result<Vec<NativeDisplay>>;
    fn cursor(&self) -> Result<(f64, f64)>;
    fn held(&self, vk: u16) -> bool;
    fn key_vk(&self, scan: u16) -> u16;
    fn post(&mut self, input: INPUT) -> Result<()>;
}
struct Native {
    last_ready: Mutex<Option<Instant>>,
    cached_displays: Mutex<Option<(Instant, Vec<NativeDisplay>)>>,
}
impl Native {
    fn new() -> Self {
        Self {
            last_ready: Mutex::new(None),
            cached_displays: Mutex::new(None),
        }
    }
}
impl Platform for Native {
    fn invalidate_cache(&self) {
        *self.last_ready.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .cached_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
    fn ready(&self) -> Result<()> {
        if self
            .last_ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|t| t.elapsed() < PLATFORM_CACHE_TTL)
        {
            return Ok(());
        }
        desktop_ready()?;
        *self.last_ready.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        Ok(())
    }
    fn displays(&self) -> Result<Vec<NativeDisplay>> {
        if let Some((t, cached)) = self
            .cached_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && t.elapsed() < PLATFORM_CACHE_TTL
        {
            return Ok(cached.clone());
        }
        let fresh = receiver_displays()?;
        *self
            .cached_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }
    fn cursor(&self) -> Result<(f64, f64)> {
        super::cursor_position().map_err(anyhow::Error::msg)
    }
    fn held(&self, vk: u16) -> bool {
        unsafe { GetAsyncKeyState(i32::from(vk)) < 0 }
    }
    fn key_vk(&self, scan: u16) -> u16 {
        let vk = unsafe { MapVirtualKeyW(u32::from(scan), MAPVK_VSC_TO_VK_EX) } as u16;
        if vk != 0 {
            vk
        } else {
            unsafe { MapVirtualKeyW(u32::from(scan & 0x00ff), MAPVK_VSC_TO_VK) as u16 }
        }
    }
    fn post(&mut self, input: INPUT) -> Result<()> {
        crate::power::wake_display_throttled();
        let _dpi = DpiScope::physical().map_err(anyhow::Error::msg)?;
        // One event per call avoids ambiguous partial batches. Even a successful
        // insertion does not establish foreground-app acceptance or visibility.
        if unsafe { SendInput(1, &input, size_of::<INPUT>() as i32) } != 1 {
            let err = unsafe { GetLastError() };
            if err == ERROR_ACCESS_DENIED {
                // Windows UIPI blocks Medium-IL SendInput when an elevated window
                // (e.g. Task Manager or Admin console) has focus. Treat as a benign
                // UIPI drop rather than crashing input-worker and tearing down sharing.
                return Ok(());
            }
            bail!(
                "Windows input insertion failed (Win32 {err}); elevated apps and secure desktops require Service Mode"
            );
        }
        Ok(())
    }
}

/// Map non-extended numpad scan codes (`0x47..=0x53`) to `VK_NUMPAD*` / `VK_DECIMAL`
/// when NumLock is active, because `MapVirtualKeyW(MAPVK_VSC_TO_VK_EX)` always returns
/// the navigation VK (`VK_HOME`/`VK_END`/etc.) regardless of NumLock state.
fn numpad_vk_when_numlock_on(scan: u16) -> Option<u16> {
    Some(match scan {
        0x47 => VK_NUMPAD7,
        0x48 => VK_NUMPAD8,
        0x49 => VK_NUMPAD9,
        0x4b => VK_NUMPAD4,
        0x4c => VK_NUMPAD5,
        0x4d => VK_NUMPAD6,
        0x4f => VK_NUMPAD1,
        0x50 => VK_NUMPAD2,
        0x51 => VK_NUMPAD3,
        0x52 => VK_NUMPAD0,
        0x53 => VK_DECIMAL,
        _ => return None,
    })
}

/// On Windows 11's secure desktop (`WinSta0\Winlogon` / `LogonUI.exe` PIN & password box),
/// `SendInput` keyboard events with `wVk = 0 | KEYEVENTF_SCANCODE` are dropped/ignored.
/// Populate explicit `wVk` alongside hardware `wScan` (clearing `KEYEVENTF_SCANCODE`) so
/// digits, letters, numpad PINs, modifiers, and symbols type cleanly into `LogonUI.exe`.
fn normalize_winlogon_keyboard_input_with_numlock(mut input: INPUT, numlock_on: bool) -> INPUT {
    if input.r#type != INPUT_KEYBOARD {
        return input;
    }
    let ki = unsafe { &mut input.Anonymous.ki };
    if ki.wVk == 0 && (ki.dwFlags & KEYEVENTF_SCANCODE) != 0 && ki.wScan != 0 {
        let extended = (ki.dwFlags & KEYEVENTF_EXTENDEDKEY) != 0;
        let mut vk = if !extended && numlock_on {
            numpad_vk_when_numlock_on(ki.wScan).unwrap_or(0)
        } else {
            0
        };
        if vk == 0 {
            let full_scan = u32::from(ki.wScan) | if extended { 0xe000 } else { 0 };
            vk = unsafe { MapVirtualKeyW(full_scan, MAPVK_VSC_TO_VK_EX) } as u16;
        }
        if vk == 0 {
            vk = unsafe { MapVirtualKeyW(u32::from(ki.wScan), MAPVK_VSC_TO_VK) } as u16;
        }
        if vk != 0 {
            ki.wVk = vk;
            ki.dwFlags &= !KEYEVENTF_SCANCODE;
        }
    }
    input
}

fn normalize_winlogon_keyboard_input(input: INPUT) -> INPUT {
    let numlock_on = (unsafe { GetKeyState(i32::from(VK_NUMLOCK)) } & 1) != 0;
    normalize_winlogon_keyboard_input_with_numlock(input, numlock_on)
}

enum DesktopCmd {
    Displays(mpsc::SyncSender<Result<Vec<NativeDisplay>>>),
    Cursor(mpsc::SyncSender<Result<(f64, f64)>>),
    Held {
        vk: u16,
        tx: mpsc::SyncSender<bool>,
    },
    Post {
        input: INPUT,
        tx: mpsc::SyncSender<Result<()>>,
    },
}

struct DesktopActor {
    name: String,
    tx: Option<mpsc::SyncSender<DesktopCmd>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl DesktopActor {
    fn spawn(name: String, hdesk: HDESK) -> Result<Self> {
        let raw_desk = hdesk as usize;
        let is_winlogon = name.eq_ignore_ascii_case("Winlogon");
        let (tx, rx) = mpsc::sync_channel::<DesktopCmd>(4);
        let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<()>>(1);
        let thread_name = format!("crossflow-svc-{}", name.to_ascii_lowercase());
        let handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                crate::power::configure_realtime_thread(true);
                let hdesk = raw_desk as HDESK;
                // SetThreadDesktop MUST be the very first Win32 USER/GDI call on a clean
                // thread so Windows never rejects it with ERROR_BUSY (170).
                if unsafe { SetThreadDesktop(hdesk) } == 0 {
                    let err = unsafe { GetLastError() };
                    unsafe { CloseDesktop(hdesk) };
                    let _ = ready_tx.send(Err(anyhow::anyhow!(
                        "SetThreadDesktop failed (Win32 {err})"
                    )));
                    return;
                }
                let _dpi = DpiScope::physical().ok();
                let _ = ready_tx.send(Ok(()));

                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        DesktopCmd::Displays(reply) => {
                            let _ = reply.send(receiver_displays());
                        }
                        DesktopCmd::Cursor(reply) => {
                            let res = super::cursor_position()
                                .map_err(anyhow::Error::msg)
                                .or_else(|err| {
                                    // Fallback to logical GetCursorPos or primary display center
                                    // while Winlogon's lock-screen curtain / sleeping display
                                    // temporarily refuses GetPhysicalCursorPos.
                                    let mut pt = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
                                    if unsafe {
                                        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(
                                            &mut pt,
                                        )
                                    } != 0
                                    {
                                        Ok((f64::from(pt.x), f64::from(pt.y)))
                                    } else if is_winlogon {
                                        receiver_displays().map(|ds| {
                                            ds.first()
                                                .map(|d| (d.x + d.width * 0.5, d.y + d.height * 0.5))
                                                .unwrap_or((0.0, 0.0))
                                        })
                                    } else {
                                        Err(err)
                                    }
                                });
                            let _ = reply.send(res);
                        }
                        DesktopCmd::Held { vk, tx } => {
                            let held = unsafe { GetAsyncKeyState(i32::from(vk)) < 0 };
                            let _ = tx.send(held);
                        }
                        DesktopCmd::Post { input, tx } => {
                            crate::power::wake_display_throttled();
                            let effective = if is_winlogon {
                                normalize_winlogon_keyboard_input(input)
                            } else {
                                input
                            };
                            let sent =
                                unsafe { SendInput(1, &effective, size_of::<INPUT>() as i32) };
                            let res = if sent == 1 {
                                Ok(())
                            } else {
                                let err = unsafe { GetLastError() };
                                Err(anyhow::anyhow!(
                                    "Windows service SendInput failed (Win32 {err})"
                                ))
                            };
                            let _ = tx.send(res);
                        }
                    }
                }
                unsafe { CloseDesktop(hdesk) };
            })?;
        match ready_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Ok(())) => Ok(Self {
                name,
                tx: Some(tx),
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                drop(tx);
                let _ = handle.join();
                bail!("service desktop worker thread startup timed out")
            }
        }
    }

    fn send(&self, cmd: DesktopCmd) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("service desktop worker closed"))?
            .send(cmd)
            .map_err(|_| anyhow::anyhow!("service desktop worker stopped"))
    }
}

impl Drop for DesktopActor {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn open_active_input_desktop() -> Result<(String, HDESK)> {
    const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
    const MIN_RIGHTS: u32 = DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS | DESKTOP_SWITCHDESKTOP;
    let mut hdesk: HDESK = null_mut();
    for attempt in 0..5 {
        hdesk = unsafe { OpenInputDesktop(0, 0, MAXIMUM_ALLOWED) };
        if hdesk.is_null() {
            hdesk = unsafe { OpenInputDesktop(0, 0, MIN_RIGHTS) };
        }
        if !hdesk.is_null() {
            break;
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    ensure!(
        !hdesk.is_null(),
        "OpenInputDesktop failed in service worker (Win32 {})",
        unsafe { GetLastError() }
    );
    let name = match desktop_name(hdesk) {
        Ok(n) => n,
        Err(e) => {
            unsafe { CloseDesktop(hdesk) };
            return Err(e);
        }
    };
    let allowed = name.eq_ignore_ascii_case("Default") || name.eq_ignore_ascii_case("Winlogon");
    if !allowed {
        unsafe { CloseDesktop(hdesk) };
        bail!("unsupported interactive desktop for service input: {name}");
    }
    Ok((name, hdesk))
}

/// Elevated `LocalSystem` platform backend used exclusively inside `CrossflowInputService`'s
/// interactive session worker (`crossflow.exe service-worker`). Spawns a clean per-desktop
/// thread whenever the active input desktop transitions (`WinSta0\Default` <-> `WinSta0\Winlogon`)
/// so `SetThreadDesktop` never hits `ERROR_BUSY (170)` and remote mouse/keyboard control work
/// seamlessly across UAC prompts, elevated Administrator applications, and the Windows lock screen.
struct NativeService {
    actor: Mutex<Option<DesktopActor>>,
    last_sync: Mutex<Option<Instant>>,
    cached_displays: Mutex<Option<(Instant, Vec<NativeDisplay>)>>,
}
unsafe impl Send for NativeService {}

impl NativeService {
    fn new() -> Self {
        Self {
            actor: Mutex::new(None),
            last_sync: Mutex::new(None),
            cached_displays: Mutex::new(None),
        }
    }

    fn sync_input_desktop_throttled(&self, force: bool) -> Result<()> {
        if !force
            && self
                .last_sync
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some_and(|t| t.elapsed() < PLATFORM_CACHE_TTL)
            && self
                .actor
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        {
            return Ok(());
        }
        let (name, hdesk) = open_active_input_desktop()?;
        let mut guard = self.actor.lock().unwrap_or_else(|e| e.into_inner());
        let same = guard
            .as_ref()
            .is_some_and(|cur| cur.name.eq_ignore_ascii_case(&name));
        if same {
            unsafe { CloseDesktop(hdesk) };
            *self.last_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
            return Ok(());
        }
        // Drop the old desktop actor first so its handle closes cleanly, then attach a
        // brand-new thread to the new input desktop (Default <-> Winlogon).
        *guard = None;
        let fresh = DesktopActor::spawn(name, hdesk)?;
        *guard = Some(fresh);
        *self.last_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        Ok(())
    }

    fn call_actor<T>(&self, build: impl FnOnce(mpsc::SyncSender<T>) -> DesktopCmd) -> Result<T> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let guard = self.actor.lock().unwrap_or_else(|e| e.into_inner());
            let actor = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("service desktop actor unavailable"))?;
            actor.send(build(reply_tx))?;
        }
        reply_rx
            .recv_timeout(Duration::from_millis(200))
            .map_err(|_| anyhow::anyhow!("service desktop actor timed out"))
    }
}

impl Platform for NativeService {
    fn invalidate_cache(&self) {
        *self.last_sync.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .cached_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
    fn ready(&self) -> Result<()> {
        self.sync_input_desktop_throttled(false)
    }
    fn displays(&self) -> Result<Vec<NativeDisplay>> {
        if let Some((t, cached)) = self
            .cached_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && t.elapsed() < PLATFORM_CACHE_TTL
        {
            return Ok(cached.clone());
        }
        self.sync_input_desktop_throttled(false)?;
        let fresh = self.call_actor(DesktopCmd::Displays)??;
        *self
            .cached_displays
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), fresh.clone()));
        Ok(fresh)
    }
    fn cursor(&self) -> Result<(f64, f64)> {
        self.sync_input_desktop_throttled(false)?;
        self.call_actor(DesktopCmd::Cursor)?
    }
    fn held(&self, vk: u16) -> bool {
        if self.sync_input_desktop_throttled(false).is_err() {
            return false;
        }
        self.call_actor(|tx| DesktopCmd::Held { vk, tx })
            .unwrap_or(false)
    }
    fn key_vk(&self, scan: u16) -> u16 {
        let vk = unsafe { MapVirtualKeyW(u32::from(scan), MAPVK_VSC_TO_VK_EX) } as u16;
        if vk != 0 {
            vk
        } else {
            unsafe { MapVirtualKeyW(u32::from(scan & 0x00ff), MAPVK_VSC_TO_VK) as u16 }
        }
    }
    fn post(&mut self, input: INPUT) -> Result<()> {
        self.sync_input_desktop_throttled(false)?;
        if self.call_actor(|tx| DesktopCmd::Post { input, tx })?.is_ok() {
            return Ok(());
        }
        // Desktop may have just transitioned (Default <-> Winlogon): resync onto a fresh
        // desktop actor, wake the display, and retry once.
        crate::power::wake_display_now();
        self.sync_input_desktop_throttled(true)?;
        self.call_actor(|tx| DesktopCmd::Post { input, tx })?
    }
}

const RECENT_RELEASE_GRACE: Duration = Duration::from_millis(250);

pub struct WindowsMouseSink {
    platform: Box<dyn Platform>,
    emergency: Arc<AtomicBool>,
    gain: f64,
    displays: Vec<NativeDisplay>,
    position: (f64, f64),
    failed: bool,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<Button>,
    recently_released_keys: BTreeMap<u16, Instant>,
    recently_released_buttons: BTreeMap<Button, Instant>,
}
impl WindowsMouseSink {
    /// `emergency` is a latched local physical-hotkey/stop signal. The caller
    /// must create a fresh latch only after an explicit new receiver session.
    pub fn new(gain: f64, emergency: Arc<AtomicBool>) -> Result<Self> {
        ensure!(
            gain.is_finite() && gain > 0.0,
            "mouse gain must be finite and positive"
        );
        let mut sink = Self {
            platform: Box::new(Native::new()),
            emergency,
            gain,
            displays: Vec::new(),
            position: (0.0, 0.0),
            failed: true,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            recently_released_keys: BTreeMap::new(),
            recently_released_buttons: BTreeMap::new(),
        };
        sink.reset()?;
        Ok(sink)
    }

    /// Elevated `LocalSystem` service sink supporting `WinSta0\Default`, UAC
    /// Secure Desktop (`WinSta0\Winlogon`), elevated Admin windows, and Lock Screen.
    pub fn new_service(gain: f64, emergency: Arc<AtomicBool>) -> Result<Self> {
        ensure!(
            gain.is_finite() && gain > 0.0,
            "mouse gain must be finite and positive"
        );
        let mut sink = Self {
            platform: Box::new(NativeService::new()),
            emergency,
            gain,
            displays: Vec::new(),
            position: (0.0, 0.0),
            failed: true,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            recently_released_keys: BTreeMap::new(),
            recently_released_buttons: BTreeMap::new(),
        };
        sink.reset()?;
        Ok(sink)
    }
    fn emergency_local(&self) -> bool {
        // Supplement the physical-hook latch. Do not interpret our own remote
        // modifiers/Escape as physical emergency input; GetAsyncKeyState alone
        // cannot distinguish their provenance.
        let local = |vk, usage| !self.keys.contains(&usage) && self.platform.held(vk);
        let pressed = (local(VK_LCONTROL, 0xe0) || local(VK_RCONTROL, 0xe4))
            && (local(VK_LMENU, 0xe2) || local(VK_RMENU, 0xe6))
            && local(VK_ESCAPE, 0x29);
        if pressed {
            self.emergency.store(true, Ordering::Release);
        }
        self.emergency.load(Ordering::Acquire)
    }
    fn checked(&mut self) -> Result<()> {
        let result = (|| {
            ensure!(
                !self.failed,
                "input adapter requires a fresh arm after failure"
            );
            ensure!(
                !self.emergency_local(),
                "local emergency requested input stop"
            );
            self.platform.ready()?;
            ensure!(
                self.platform.displays()? == self.displays,
                "display topology changed; input stopped"
            );
            Ok(())
        })();
        self.finish(result)
    }
    fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.failed = true;
            if self.release_owned().is_err() {
                bail!("input stopped; owned input release could not be confirmed");
            }
        }
        result
    }
    fn send(&mut self, input: INPUT) -> Result<()> {
        let result = self.platform.post(input);
        self.finish(result)
    }
    fn position_checked(&mut self, x: f64, y: f64, id: u32) -> Result<(f64, f64)> {
        let packet = absolute(&self.displays, x, y, id);
        let (input, point) = self.finish(packet)?;
        self.send(input)?;
        self.position = point;
        Ok(point)
    }
}
impl MouseSink for WindowsMouseSink {
    fn displays(&self) -> Result<Vec<NativeDisplay>> {
        self.platform.displays()
    }
    fn maintain(&mut self) -> Result<()> {
        self.platform.invalidate_cache();
        self.checked()
    }
    fn reset(&mut self) -> Result<()> {
        self.failed = true;
        self.release_owned()?;
        self.recently_released_keys.clear();
        self.recently_released_buttons.clear();
        self.platform.invalidate_cache();
        ensure!(
            !self.emergency_local(),
            "local emergency requested input stop"
        );
        self.platform.ready()?;
        let displays = self.platform.displays()?;
        bounds(&displays)?;
        let position = self.platform.cursor()?;
        ensure!(
            displays.iter().any(|d| contains(d, position.0, position.1)),
            "local cursor is not on an active display"
        );
        self.displays = displays;
        self.position = position;
        self.failed = false;
        Ok(())
    }
    fn move_by(&mut self, dx: f64, dy: f64) -> Result<(f64, f64)> {
        self.checked()?;
        let x = self.position.0 + dx * self.gain;
        let y = self.position.1 + dy * self.gain;
        let target = self
            .displays
            .iter()
            .find(|d| contains(d, x, y))
            .map(|d| d.id);
        let Some(id) = target else {
            return self.finish(Err(anyhow::anyhow!(
                "relative input leaves active display geometry"
            )));
        };
        let result = self.position_checked(x, y, id)?;
        // Preserve fractional gain between relative packets even though the
        // native event targets one integral pixel.
        self.position = (x, y);
        Ok(result)
    }
    fn position(&mut self, x: f64, y: f64, id: u32) -> Result<(f64, f64)> {
        self.checked()?;
        self.position_checked(x, y, id)
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        if !down && !self.buttons.contains(&button) {
            return Ok(());
        }
        if down {
            self.checked()?;
            let own_recent_release = self
                .recently_released_buttons
                .get(&button)
                .is_some_and(|t| t.elapsed() < RECENT_RELEASE_GRACE);
            let valid = !self.buttons.contains(&button)
                && (own_recent_release || !self.platform.held(button_vk(button)));
            self.finish(if valid {
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "button is already held locally or remotely"
                ))
            })?;
            self.recently_released_buttons.remove(&button);
            self.buttons.insert(button);
        }
        self.send(button_input(button, down))?;
        if !down {
            self.buttons.remove(&button);
            self.recently_released_buttons
                .insert(button, Instant::now());
        }
        Ok(())
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        if usage == crate::keymap::HID_FN_GLOBE {
            // Apple Fn/Globe has no Windows hardware scan code; treat as a benign no-op
            // so pressing Fn on Mac while controlling Windows never aborts injection.
            return Ok(());
        }
        if !down && !self.keys.contains(&usage) {
            return Ok(());
        }
        let packet = match keyboard(usage, down) {
            Ok(input) => input,
            Err(_) if crate::input::is_keyboard_usage(usage) => {
                // Valid HID keyboard usage with no Windows scan/VK equivalent (e.g. macOS
                // Fn/Globe 0xA4, Keypad = 0x67, Pause 0x48): treat as a benign no-op so it
                // never crashes the injection worker or bounces cursor focus!
                if !down {
                    self.keys.remove(&usage);
                }
                return Ok(());
            }
            Err(err) => return self.finish(Err(err)),
        };
        let input = packet;
        if down {
            self.checked()?;
            if self.keys.contains(&usage) {
                // Already owned by this remote session: allow genuine key auto-repeat
                // for repeatable keys (Backspace, Delete, Arrows, letters, etc.) and
                // treat modifier/lock repeats as benign no-ops.
                if !crate::input::is_repeatable_key(usage) {
                    return Ok(());
                }
            } else {
                let vk = media_vk(usage)
                    .or_else(|| editing_control_vk(usage).map(|(vk, _, _)| vk))
                    .unwrap_or_else(|| self.platform.key_vk(scan(usage).expect("validated usage")));
                // Rapidly tapping Backspace / keys over Wi-Fi bursts can re-press a key
                // milliseconds after our own SendInput(KEYUP), before Windows CSRSS / IME
                // clears GetAsyncKeyState. Allow re-press within RECENT_RELEASE_GRACE.
                let own_recent_release = self
                    .recently_released_keys
                    .get(&usage)
                    .is_some_and(|t| t.elapsed() < RECENT_RELEASE_GRACE);
                let valid = vk != 0 && (own_recent_release || !self.platform.held(vk));
                self.finish(if valid {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "key is unsupported or already held locally or remotely"
                    ))
                })?;
                self.recently_released_keys.remove(&usage);
                self.keys.insert(usage);
            }
        }
        self.send(input)?;
        if !down {
            self.keys.remove(&usage);
            self.recently_released_keys.insert(usage, Instant::now());
        }
        Ok(())
    }
    fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
        self.checked()?;
        for (value, flag) in [
            (horizontal, MOUSEEVENTF_HWHEEL),
            (vertical, MOUSEEVENTF_WHEEL),
        ] {
            if value != 0 {
                self.send(mouse(0, 0, value as u32, flag))?;
            }
        }
        Ok(())
    }
    fn release_owned(&mut self) -> Result<()> {
        let mut failed = false;
        for usage in self.keys.clone() {
            if keyboard(usage, false)
                .and_then(|input| self.platform.post(input))
                .is_ok()
            {
                self.keys.remove(&usage);
            } else {
                failed = true;
            }
        }
        for button in self.buttons.clone() {
            if self.platform.post(button_input(button, false)).is_ok() {
                self.buttons.remove(&button);
            } else {
                failed = true;
            }
        }
        if failed {
            self.failed = true;
        }
        ensure!(!failed, "owned input release could not be confirmed");
        Ok(())
    }
    fn emergency_pressed(&self) -> bool {
        self.failed || self.emergency_local()
    }
}
impl Drop for WindowsMouseSink {
    fn drop(&mut self) {
        let _ = self.release_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    fn layout() -> Vec<NativeDisplay> {
        vec![
            NativeDisplay {
                id: 1,
                x: 0.,
                y: 0.,
                width: 3840.,
                height: 2160.,
            },
            NativeDisplay {
                id: 2,
                x: -1440.,
                y: -89.,
                width: 1440.,
                height: 2560.,
            },
        ]
    }
    #[test]
    fn absolute_normalization_uses_entire_negative_origin_desktop_without_pixel_drift() {
        let screens = layout();
        let b = bounds(&screens).unwrap();
        for display in &screens {
            for (x, y) in [
                (display.x, display.y),
                (
                    display.x + display.width - 0.1,
                    display.y + display.height - 0.1,
                ),
                (display.x + 42.5, display.y + 20.3),
            ] {
                let (input, point) = absolute(&screens, x, y, display.id).unwrap();
                let m = unsafe { input.Anonymous.mi };
                assert_eq!(
                    ((i64::from(m.dx) * (b[2] - b[0])) / 65536 + b[0]) as f64,
                    point.0
                );
                assert_eq!(
                    ((i64::from(m.dy) * (b[3] - b[1])) / 65536 + b[1]) as f64,
                    point.1
                );
                assert_eq!(m.dwExtraInfo, INPUT_ORIGIN_TAG);
                assert_eq!(
                    m.dwFlags & (MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
                    MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
                );
            }
        }
        // Every primary-screen pixel must remain addressable, not only corners.
        for x in 0..3840 {
            let (input, _) = absolute(&screens, f64::from(x), 1080., 1).unwrap();
            let m = unsafe { input.Anonymous.mi };
            assert_eq!(i64::from(m.dx) * (b[2] - b[0]) / 65536 + b[0], i64::from(x));
        }
    }
    #[test]
    fn absolute_rejects_gaps_wrong_display_nonfinite_and_exclusive_edge() {
        for (x, y, id) in [
            (100., -1., 1),
            (3840., 100., 1),
            (-10., 100., 1),
            (f64::NAN, 0., 1),
            (0., f64::INFINITY, 1),
            (10., 10., 55),
        ] {
            assert!(absolute(&layout(), x, y, id).is_err());
        }
        let wide = vec![NativeDisplay {
            id: 1,
            x: 0.,
            y: 0.,
            width: 65537.,
            height: 1.,
        }];
        assert!(bounds(&wide).is_err());
    }
    #[test]
    fn native_name_ids_are_stable_order_independent_and_collisions_rejected() {
        let native = |id: &str| super::super::NativeDisplay {
            id: id.into(),
            x: 0.,
            y: 0.,
            width: 100.,
            height: 100.,
        };
        let a = native(r"\\.\DISPLAY1");
        let b = native(r"\\.\DISPLAY2");
        assert_eq!(
            map_displays(&[a.clone(), b.clone()]).unwrap(),
            map_displays(&[b, a]).unwrap()
        );
        assert_ne!(
            receiver_display_id(r"\\.\DISPLAY1").unwrap(),
            receiver_display_id(r"\\.\DISPLAY2").unwrap()
        );
        // Known FNV-1a collision: detection must not silently alias two screens.
        assert_eq!(
            receiver_display_id("costarring").unwrap(),
            receiver_display_id("liquid").unwrap()
        );
        assert!(map_displays(&[native("costarring"), native("liquid")]).is_err());
    }
    #[test]
    fn keyboard_and_buttons_preserve_physical_sides_release_flags_and_origin() {
        for &(capture_scan, usage) in SCAN_TO_HID {
            let down = keyboard(usage, true).unwrap();
            let up = keyboard(usage, false).unwrap();
            let d = unsafe { down.Anonymous.ki };
            let u = unsafe { up.Anonymous.ki };
            assert_eq!(super::super::scan_to_hid(scan(usage).unwrap()), Some(usage));
            assert_eq!(u.wScan, d.wScan);
            assert_eq!(u.dwFlags, d.dwFlags | KEYEVENTF_KEYUP);
            assert_eq!(d.dwExtraInfo, INPUT_ORIGIN_TAG);
            if let Some((expected_vk, expected_scan, expected_ext)) = editing_control_vk(usage) {
                assert_eq!(d.wVk, expected_vk);
                assert_eq!(d.wScan, expected_scan);
                assert_eq!(d.dwFlags & KEYEVENTF_SCANCODE, 0);
                assert_eq!(d.dwFlags & KEYEVENTF_EXTENDEDKEY != 0, expected_ext);
            } else {
                assert_eq!(d.wVk, 0);
                assert_ne!(d.dwFlags & KEYEVENTF_SCANCODE, 0);
                if capture_scan & 0xe000 != 0 && usage != 0x53 {
                    assert_ne!(d.dwFlags & KEYEVENTF_EXTENDEDKEY, 0);
                }
            }
        }
        assert_eq!(scan(0x46), Some(0xe037));
        assert!(keyboard(0x48, true).is_err()); // Pause/E1 intentionally unsupported.
        for button in [
            Button::Left,
            Button::Right,
            Button::Middle,
            Button::Back,
            Button::Forward,
        ] {
            let down = unsafe { button_input(button, true).Anonymous.mi };
            let up = unsafe { button_input(button, false).Anonymous.mi };
            assert_ne!(down.dwFlags, up.dwFlags);
            assert_eq!(down.mouseData, up.mouseData);
            assert_eq!(up.dwExtraInfo, INPUT_ORIGIN_TAG);
        }
    }

    struct State {
        events: Vec<INPUT>,
        failures: VecDeque<bool>,
        ready: bool,
        screens: Vec<NativeDisplay>,
        held: BTreeSet<u16>,
    }
    struct Fake(Arc<Mutex<State>>);
    impl Platform for Fake {
        fn ready(&self) -> Result<()> {
            ensure!(self.0.lock().unwrap().ready, "fake desktop unavailable");
            Ok(())
        }
        fn displays(&self) -> Result<Vec<NativeDisplay>> {
            Ok(self.0.lock().unwrap().screens.clone())
        }
        fn cursor(&self) -> Result<(f64, f64)> {
            Ok((10., 10.))
        }
        fn held(&self, vk: u16) -> bool {
            self.0.lock().unwrap().held.contains(&vk)
        }
        fn key_vk(&self, scan: u16) -> u16 {
            scan
        } // Synthetic keys, no OS lookup.
        fn post(&mut self, input: INPUT) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            state.events.push(input);
            ensure!(
                !state.failures.pop_front().unwrap_or(false),
                "fake insertion failed"
            );
            Ok(())
        }
    }
    fn fake() -> (WindowsMouseSink, Arc<Mutex<State>>) {
        let state = Arc::new(Mutex::new(State {
            events: vec![],
            failures: VecDeque::new(),
            ready: true,
            screens: layout(),
            held: BTreeSet::new(),
        }));
        let mut sink = WindowsMouseSink {
            platform: Box::new(Fake(state.clone())),
            emergency: Arc::new(AtomicBool::new(false)),
            gain: 1.,
            displays: vec![],
            position: (0., 0.),
            failed: true,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            recently_released_keys: BTreeMap::new(),
            recently_released_buttons: BTreeMap::new(),
        };
        sink.reset().unwrap();
        (sink, state)
    }
    #[test]
    fn release_owned_bypasses_stop_and_desktop_guards_and_never_releases_unowned() {
        let (mut sink, state) = fake();
        sink.key(0x04, true).unwrap();
        sink.button(Button::Back, true).unwrap();
        state.lock().unwrap().ready = false;
        sink.emergency.store(true, Ordering::Release);
        sink.key(0x05, false).unwrap();
        sink.button(Button::Left, false).unwrap();
        assert_eq!(state.lock().unwrap().events.len(), 2);
        sink.release_owned().unwrap();
        assert!(sink.keys.is_empty() && sink.buttons.is_empty());
        assert_eq!(state.lock().unwrap().events.len(), 4);
        sink.release_owned().unwrap();
        assert_eq!(state.lock().unwrap().events.len(), 4);
        assert!(sink.reset().is_err());
    }
    #[test]
    fn failed_insertion_latches_and_failed_cleanup_retains_owned_for_retry() {
        let (mut sink, state) = fake();
        state.lock().unwrap().failures.extend([true, true]);
        assert!(sink.key(0x04, true).is_err());
        assert!(sink.failed);
        assert!(sink.keys.contains(&0x04));
        assert_eq!(state.lock().unwrap().events.len(), 2);
        sink.release_owned().unwrap();
        assert!(sink.keys.is_empty());
        assert!(sink.position(20., 20., 1).is_err());
        assert_eq!(state.lock().unwrap().events.len(), 3);
    }
    #[test]
    fn topology_change_stops_and_releases_owned_before_further_input() {
        let (mut sink, state) = fake();
        sink.button(Button::Left, true).unwrap();
        state.lock().unwrap().screens[0].height = 1080.;
        assert!(sink.position(30., 30., 1).is_err());
        assert!(sink.failed);
        assert!(sink.buttons.is_empty());
        let state = state.lock().unwrap();
        assert_eq!(state.events.len(), 2);
        assert_eq!(
            unsafe { state.events[1].Anonymous.mi }.dwFlags,
            MOUSEEVENTF_LEFTUP
        );
    }
    #[test]
    fn local_held_inputs_cannot_be_adopted_or_released_by_receiver() {
        let (mut sink, state) = fake();
        state.lock().unwrap().held.insert(0x1e);
        assert!(sink.key(0x04, true).is_err());
        assert!(sink.keys.is_empty());
        assert!(state.lock().unwrap().events.is_empty());
        let (mut sink, state) = fake();
        state.lock().unwrap().held.insert(VK_LBUTTON);
        assert!(sink.button(Button::Left, true).is_err());
        assert!(sink.buttons.is_empty());
        assert!(state.lock().unwrap().events.is_empty());
    }
    #[test]
    fn local_emergency_latches_but_owned_remote_chord_is_not_misclassified() {
        let (sink, state) = fake();
        state
            .lock()
            .unwrap()
            .held
            .extend([VK_LCONTROL, VK_LMENU, VK_ESCAPE]);
        assert!(sink.emergency_pressed());
        state.lock().unwrap().held.clear();
        assert!(sink.emergency_pressed());
        let (mut sink, state) = fake();
        sink.keys.extend([0xe0, 0xe2, 0x29]);
        state
            .lock()
            .unwrap()
            .held
            .extend([VK_LCONTROL, VK_LMENU, VK_ESCAPE]);
        assert!(!sink.emergency_pressed());
    }
    #[test]
    fn fractional_relative_gain_accumulates_and_scroll_keeps_signed_raw_units() {
        let (mut sink, state) = fake();
        sink.gain = 0.25;
        for _ in 0..4 {
            sink.move_by(1., 0.).unwrap();
        }
        assert_eq!(sink.position, (11., 10.));
        sink.scroll(-15, 120).unwrap();
        let state = state.lock().unwrap();
        let h = unsafe { state.events[4].Anonymous.mi };
        let v = unsafe { state.events[5].Anonymous.mi };
        assert_eq!(h.dwFlags, MOUSEEVENTF_HWHEEL);
        assert_eq!(h.mouseData as i32, -15);
        assert_eq!(v.dwFlags, MOUSEEVENTF_WHEEL);
        assert_eq!(v.mouseData as i32, 120);
    }
    #[test]
    fn owned_repeatable_keys_auto_repeat_while_modifiers_do_not() {
        let (mut sink, state) = fake();
        sink.key(0x2a, true).unwrap(); // Backspace down
        state.lock().unwrap().held.insert(0x0e);
        sink.key(0x2a, true).unwrap(); // Backspace auto-repeat
        sink.key(0x2a, false).unwrap(); // Backspace up
        assert_eq!(state.lock().unwrap().events.len(), 3);
        sink.key(0xe1, true).unwrap(); // Left Shift down
        sink.key(0xe1, true).unwrap(); // Left Shift repeat -> benign no-op
        assert_eq!(state.lock().unwrap().events.len(), 4);
    }
    #[test]
    fn rapid_backspace_repress_tolerates_stale_os_async_key_state_after_own_release() {
        let (mut sink, state) = fake();
        // Tap 1: Backspace down + up; simulate Windows CSRSS/IME leaving VK_BACK stale-held
        sink.key(0x2a, true).unwrap();
        sink.key(0x2a, false).unwrap();
        state.lock().unwrap().held.insert(VK_BACK);
        // Tap 2 immediately after our own release must succeed instead of failing the worker
        sink.key(0x2a, true).unwrap();
        sink.key(0x2a, false).unwrap();
        assert_eq!(state.lock().unwrap().events.len(), 4);
        // Verify Backspace emits explicit VK_BACK + scan 0x0e without KEYEVENTF_SCANCODE
        let ki = unsafe { state.lock().unwrap().events[0].Anonymous.ki };
        assert_eq!(ki.wVk, VK_BACK);
        assert_eq!(ki.wScan, 0x0e);
        assert_eq!(ki.dwFlags & KEYEVENTF_SCANCODE, 0);
    }
    #[test]
    fn winlogon_keyboard_input_populates_virtual_key_and_clears_scancode_flag() {
        // Letters ('A' = HID 0x04), top-row digits ('1' = HID 0x1e, '0' = HID 0x27),
        // and Left Shift (HID 0xe1) must receive explicit wVk with KEYEVENTF_SCANCODE cleared.
        for (usage, expected_vk) in [
            (0x04u16, 0x41u16), // 'A'
            (0x1e, 0x31),       // '1'
            (0x27, 0x30),       // '0'
            (0xe1, VK_LSHIFT),  // Left Shift
        ] {
            let raw = keyboard(usage, true).unwrap();
            let norm = normalize_winlogon_keyboard_input_with_numlock(raw, true);
            let ki = unsafe { norm.Anonymous.ki };
            assert_eq!(ki.wVk, expected_vk, "usage 0x{usage:02x}");
            assert_ne!(ki.wScan, 0, "usage 0x{usage:02x} must keep hardware scan");
            assert_eq!(ki.dwFlags & KEYEVENTF_SCANCODE, 0);
            assert_eq!(ki.dwFlags & KEYEVENTF_KEYUP, 0);
            assert_eq!(ki.dwExtraInfo, INPUT_ORIGIN_TAG);

            let raw_up = keyboard(usage, false).unwrap();
            let norm_up = normalize_winlogon_keyboard_input_with_numlock(raw_up, true);
            let ki_up = unsafe { norm_up.Anonymous.ki };
            assert_eq!(ki_up.wVk, expected_vk);
            assert_eq!(ki_up.dwFlags & KEYEVENTF_SCANCODE, 0);
            assert_ne!(ki_up.dwFlags & KEYEVENTF_KEYUP, 0);
        }

        // Numpad '1' (HID 0x59 -> scan 0x4f): VK_NUMPAD1 when NumLock is ON, VK_END when OFF.
        let kp1 = keyboard(0x59, true).unwrap();
        let kp1_on = unsafe {
            normalize_winlogon_keyboard_input_with_numlock(kp1, true)
                .Anonymous
                .ki
        };
        assert_eq!(kp1_on.wVk, VK_NUMPAD1);
        assert_eq!(kp1_on.wScan, 0x4f);
        assert_eq!(kp1_on.dwFlags & KEYEVENTF_SCANCODE, 0);

        let kp1_off = unsafe {
            normalize_winlogon_keyboard_input_with_numlock(kp1, false)
                .Anonymous
                .ki
        };
        assert_eq!(kp1_off.wVk, VK_END);
        assert_eq!(kp1_off.dwFlags & KEYEVENTF_SCANCODE, 0);
    }
}
