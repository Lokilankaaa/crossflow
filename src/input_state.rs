//! Cross-platform Caps Lock and Chinese/English IME state inspection & synchronization.
//!
//! - **Caps Lock**:
//!   - Windows: `GetKeyState(VK_CAPITAL)` + synthetic `VK_CAPITAL` tap via `SendInput`.
//!   - macOS: `CGEventSourceFlagsState(HIDSystemState)` + IOKit `IOHIDSetModifierLockState`.
//! - **Chinese / English IME**:
//!   - Windows: foreground `HKL` primary language + `ImmGetDefaultIMEWnd` / `WM_IME_CONTROL`
//!     (`IMC_GETOPENSTATUS`, `IMC_GETCONVERSIONMODE`, `IMC_SETOPENSTATUS`, `IMC_SETCONVERSIONMODE`)
//!     with `WM_INPUTLANGCHANGEREQUEST` fallback when switching between installed layouts.
//!   - macOS: Carbon `TISCopyCurrentKeyboardInputSource`, `TISCreateInputSourceList`,
//!     and `TISSelectInputSource`, remembering the user's preferred Chinese and English sources.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLockState {
    pub caps_lock: bool,
    pub ime_chinese: Option<bool>,
}

/// Read current local Caps Lock and Chinese/English IME state.
pub fn read_state() -> InputLockState {
    InputLockState {
        caps_lock: read_caps_lock(),
        ime_chinese: read_ime_chinese(),
    }
}

/// Apply a remote Caps Lock and/or Chinese/English IME state locally.
pub fn apply_state(target: InputLockState, sync_caps: bool, sync_ime: bool) {
    if sync_caps {
        let _ = set_caps_lock(target.caps_lock);
    }
    if sync_ime
        && let Some(want_zh) = target.ime_chinese
    {
        set_ime_chinese(want_zh);
    }
}

/// Read current hardware/OS Caps Lock lock state.
pub fn read_caps_lock() -> bool {
    platform::read_caps_lock()
}

/// Set Caps Lock lock state to `desired`. Returns `true` if the OS call succeeded.
pub fn set_caps_lock(desired: bool) -> bool {
    platform::set_caps_lock(desired)
}

/// Toggle current Caps Lock state and return the resulting state.
pub fn toggle_caps_lock() -> bool {
    let next = !read_caps_lock();
    let _ = set_caps_lock(next);
    next
}

/// Read whether the active input source / IME is currently in Chinese mode.
pub fn read_ime_chinese() -> Option<bool> {
    platform::read_ime_chinese()
}

/// Switch the active input source / IME to Chinese (`true`) or English (`false`).
pub fn set_ime_chinese(want_zh: bool) {
    platform::set_ime_chinese(want_zh);
}

/// Pure classifier for macOS TIS input source IDs and language tags (unit-tested on all platforms).
pub fn classify_macos_tis_chinese(id: &str, langs: &[String]) -> bool {
    const LATIN_LAYOUT_MARKERS: [&str; 6] = [
        ".keylayout.ABC",
        ".keylayout.US",
        ".keylayout.British",
        ".keylayout.Canadian",
        ".keylayout.Australian",
        ".keylayout.UnicodeHexInput",
    ];
    if LATIN_LAYOUT_MARKERS.iter().any(|m| id.contains(m)) {
        return false;
    }
    if langs.iter().any(|l| {
        let lower = l.to_ascii_lowercase();
        lower == "zh" || lower.starts_with("zh-") || lower.starts_with("zh_")
    }) {
        return true;
    }
    const CHINESE_ID_MARKERS: [&str; 12] = [
        "SCIM",
        "TCIM",
        "Pinyin",
        "pinyin",
        "sogou",
        "Sogou",
        "Squirrel",
        "wetype",
        "WeType",
        "baidu",
        "Shuangpin",
        "WBX",
    ];
    CHINESE_ID_MARKERS.iter().any(|m| id.contains(m))
}

#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr::null_mut;
    use std::thread;
    use std::time::Duration;
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, GetKeyboardLayout, GetKeyboardLayoutList, INPUT, INPUT_0, INPUT_KEYBOARD,
        KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_CAPITAL,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, MSG, PM_NOREMOVE, PeekMessageW,
        PostMessageW, SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    };

    const WM_INPUTLANGCHANGEREQUEST: u32 = 0x0050;
    const WM_IME_CONTROL: u32 = 0x0283;
    const IMC_GETCONVERSIONMODE: usize = 0x0001;
    const IMC_SETCONVERSIONMODE: usize = 0x0002;
    const IMC_GETOPENSTATUS: usize = 0x0005;
    const IMC_SETOPENSTATUS: usize = 0x0006;
    const IME_CMODE_NATIVE: usize = 0x0001;
    const IME_CMODE_FULLSHAPE: usize = 0x0008;
    const LANG_CHINESE_PRIMARY: u16 = 0x04;
    const LANG_ENGLISH_PRIMARY: u16 = 0x09;
    const INPUT_ORIGIN_TAG: usize = 0x4352_4F53_5346_4C4F;

    #[link(name = "imm32")]
    unsafe extern "system" {
        fn ImmGetDefaultIMEWnd(hwnd: *mut c_void) -> *mut c_void;
    }

    pub fn read_caps_lock() -> bool {
        unsafe {
            // Pump message queue state and attach briefly to foreground thread so
            // GetKeyState(VK_CAPITAL) returns the true live global Caps Lock toggle
            // rather than a stale background-thread snapshot.
            let mut msg: MSG = std::mem::zeroed();
            let _ = PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE);
            let cur_tid = GetCurrentThreadId();
            let hwnd = GetForegroundWindow();
            let fg_tid = if !hwnd.is_null() {
                GetWindowThreadProcessId(hwnd, null_mut())
            } else {
                0
            };
            let attached = fg_tid != 0 && fg_tid != cur_tid && AttachThreadInput(cur_tid, fg_tid, 1) != 0;
            let state = (GetKeyState(VK_CAPITAL as i32) & 1) != 0;
            if attached {
                let _ = AttachThreadInput(cur_tid, fg_tid, 0);
            }
            state
        }
    }

    pub fn set_caps_lock(desired: bool) -> bool {
        if read_caps_lock() == desired {
            return true;
        }
        let inputs = [
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_CAPITAL,
                        wScan: 0x3A,
                        dwFlags: 0,
                        time: 0,
                        dwExtraInfo: INPUT_ORIGIN_TAG,
                    },
                },
            },
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_CAPITAL,
                        wScan: 0x3A,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: INPUT_ORIGIN_TAG,
                    },
                },
            },
        ];
        let sent = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32) };
        sent == inputs.len() as u32
    }

    fn primary_lang(hkl: *mut c_void) -> u16 {
        ((hkl as usize) & 0x03FF) as u16
    }

    fn query_ime_control(ime_wnd: *mut c_void, cmd: usize, arg: isize) -> Option<usize> {
        let mut out: usize = 0;
        let ok = unsafe {
            SendMessageTimeoutW(
                ime_wnd,
                WM_IME_CONTROL,
                cmd,
                arg,
                SMTO_ABORTIFHUNG,
                50,
                &mut out,
            )
        };
        (ok != 0).then_some(out)
    }

    fn installed_hkls() -> Vec<*mut c_void> {
        let count = unsafe { GetKeyboardLayoutList(0, null_mut()) };
        if count <= 0 || count > 64 {
            return Vec::new();
        }
        let mut list = vec![null_mut(); count as usize];
        let got = unsafe { GetKeyboardLayoutList(count, list.as_mut_ptr()) };
        if got <= 0 {
            return Vec::new();
        }
        list.truncate(got as usize);
        list
    }

    pub fn read_ime_chinese() -> Option<bool> {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            return None;
        }
        let tid = unsafe { GetWindowThreadProcessId(hwnd, null_mut()) };
        if tid == 0 {
            return None;
        }
        let hkl = unsafe { GetKeyboardLayout(tid) };
        if hkl.is_null() {
            return None;
        }
        if primary_lang(hkl) != LANG_CHINESE_PRIMARY {
            return Some(false);
        }
        let ime_wnd = unsafe { ImmGetDefaultIMEWnd(hwnd) };
        if ime_wnd.is_null() {
            return Some(true);
        }
        let open = query_ime_control(ime_wnd, IMC_GETOPENSTATUS, 0)?;
        let conv = query_ime_control(ime_wnd, IMC_GETCONVERSIONMODE, 0)?;
        Some(open != 0 && (conv & IME_CMODE_NATIVE) != 0)
    }

    pub fn set_ime_chinese(want_zh: bool) {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            return;
        }
        let tid = unsafe { GetWindowThreadProcessId(hwnd, null_mut()) };
        if tid == 0 {
            return;
        }
        let hkl = unsafe { GetKeyboardLayout(tid) };
        if hkl.is_null() {
            return;
        }
        let current_primary = primary_lang(hkl);
        let hkls = installed_hkls();
        let zh_hkl = hkls
            .iter()
            .copied()
            .find(|&h| primary_lang(h) == LANG_CHINESE_PRIMARY);
        let en_hkl = hkls
            .iter()
            .copied()
            .find(|&h| primary_lang(h) == LANG_ENGLISH_PRIMARY);

        if want_zh {
            if current_primary != LANG_CHINESE_PRIMARY {
                if let Some(target_zh) = zh_hkl {
                    unsafe {
                        PostMessageW(hwnd, WM_INPUTLANGCHANGEREQUEST, 0, target_zh as isize);
                    }
                    thread::sleep(Duration::from_millis(35));
                } else {
                    return;
                }
            }
            let ime_wnd = unsafe { ImmGetDefaultIMEWnd(hwnd) };
            if !ime_wnd.is_null() {
                let _ = query_ime_control(ime_wnd, IMC_SETOPENSTATUS, 1);
                let cur_conv = query_ime_control(ime_wnd, IMC_GETCONVERSIONMODE, 0).unwrap_or(0);
                let next_conv = (cur_conv | IME_CMODE_NATIVE) as isize;
                let _ = query_ime_control(ime_wnd, IMC_SETCONVERSIONMODE, next_conv);
            }
        } else {
            if current_primary != LANG_CHINESE_PRIMARY {
                return;
            }
            let ime_wnd = unsafe { ImmGetDefaultIMEWnd(hwnd) };
            if !ime_wnd.is_null() {
                let cur_conv = query_ime_control(ime_wnd, IMC_GETCONVERSIONMODE, 0).unwrap_or(0);
                let next_conv = (cur_conv & !(IME_CMODE_NATIVE | IME_CMODE_FULLSHAPE)) as isize;
                let _ = query_ime_control(ime_wnd, IMC_SETCONVERSIONMODE, next_conv);
                let _ = query_ime_control(ime_wnd, IMC_SETOPENSTATUS, 0);
            }
            thread::sleep(Duration::from_millis(20));
            if read_ime_chinese() == Some(true)
                && let Some(target_en) = en_hkl
            {
                unsafe {
                    PostMessageW(hwnd, WM_INPUTLANGCHANGEREQUEST, 0, target_en as isize);
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_op_in_unsafe_fn)]
mod platform {
    use super::classify_macos_tis_chinese;
    use std::ffi::{CStr, c_char, c_void};
    use std::ptr::null;
    use std::sync::Mutex;

    const K_IOHID_PARAM_CONNECT_TYPE: u32 = 1;
    const K_IOHID_CAPS_LOCK_STATE: i32 = 1;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    static PREFERRED_SOURCES: Mutex<(Option<String>, Option<String>)> = Mutex::new((None, None));

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
        fn IOServiceOpen(service: u32, owning_task: u32, connect_type: u32, connect: *mut u32) -> i32;
        fn IOServiceClose(connect: u32) -> i32;
        fn IOObjectRelease(object: u32) -> i32;
        fn IOHIDGetModifierLockState(handle: u32, selector: i32, state: *mut bool) -> i32;
        fn IOHIDSetModifierLockState(handle: u32, selector: i32, state: bool) -> i32;
    }

    unsafe extern "C" {
        static mach_task_self_: u32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(cf: *const c_void);
        fn CFEqual(cf1: *const c_void, cf2: *const c_void) -> bool;
        fn CFBooleanGetValue(boolean: *const c_void) -> bool;
        fn CFGetTypeID(cf: *const c_void) -> usize;
        fn CFArrayGetTypeID() -> usize;
        fn CFDictionaryGetTypeID() -> usize;
        fn CFStringGetTypeID() -> usize;
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, idx: isize) -> *const c_void;
        fn CFDictionaryGetValue(dict: *const c_void, key: *const c_void) -> *const c_void;
        fn CFStringCreateWithCString(alloc: *const c_void, c_str: *const c_char, encoding: u32) -> *mut c_void;
        fn CFStringGetLength(string: *const c_void) -> isize;
        fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
        fn CFStringGetCString(
            string: *const c_void,
            buffer: *mut c_char,
            buffer_size: isize,
            encoding: u32,
        ) -> bool;
        fn CFPreferencesAppSynchronize(app_id: *const c_void) -> bool;
        fn CFPreferencesCopyAppValue(key: *const c_void, app_id: *const c_void) -> *mut c_void;
    }

    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C" {
        static kTISPropertyInputSourceID: *const c_void;
        static kTISPropertyInputSourceCategory: *const c_void;
        static kTISCategoryKeyboardInputSource: *const c_void;
        static kTISPropertyInputSourceIsSelectCapable: *const c_void;
        static kTISPropertyInputSourceIsEnabled: *const c_void;
        static kTISPropertyInputSourceLanguages: *const c_void;

        fn TISCopyCurrentKeyboardInputSource() -> *mut c_void;
        fn TISCreateInputSourceList(properties: *const c_void, include_all: bool) -> *mut c_void;
        fn TISGetInputSourceProperty(source: *const c_void, key: *const c_void) -> *const c_void;
        fn TISSelectInputSource(source: *const c_void) -> i32;
    }

    pub fn read_caps_lock() -> bool {
        unsafe {
            // Use IOHIDGetModifierLockState rather than CGEventSourceFlagsState: on
            // macOS Sequoia, CGEventSourceFlagsState does not reflect Caps Lock set
            // via IOHIDSetModifierLockState.
            let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
            if matching.is_null() {
                return false;
            }
            let svc = IOServiceGetMatchingService(0, matching);
            if svc == 0 {
                return false;
            }
            let mut conn: u32 = 0;
            let mut state = false;
            if IOServiceOpen(svc, mach_task_self_, K_IOHID_PARAM_CONNECT_TYPE, &mut conn) == 0
                && conn != 0
            {
                let _ = IOHIDGetModifierLockState(conn, K_IOHID_CAPS_LOCK_STATE, &mut state);
                IOServiceClose(conn);
            }
            IOObjectRelease(svc);
            state
        }
    }

    pub fn set_caps_lock(desired: bool) -> bool {
        unsafe {
            let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
            if matching.is_null() {
                return false;
            }
            let svc = IOServiceGetMatchingService(0, matching);
            if svc == 0 {
                return false;
            }
            let mut conn: u32 = 0;
            let kr_open = IOServiceOpen(svc, mach_task_self_, K_IOHID_PARAM_CONNECT_TYPE, &mut conn);
            let ok = if kr_open == 0 && conn != 0 {
                let kr_set = IOHIDSetModifierLockState(conn, K_IOHID_CAPS_LOCK_STATE, desired);
                IOServiceClose(conn);
                kr_set == 0
            } else {
                false
            };
            IOObjectRelease(svc);
            ok
        }
    }

    unsafe fn cfstring_to_string(cf: *const c_void) -> Option<String> {
        if cf.is_null() {
            return None;
        }
        let len = CFStringGetLength(cf);
        let max = CFStringGetMaximumSizeForEncoding(len, K_CF_STRING_ENCODING_UTF8) + 1;
        if max <= 1 || max > 4096 {
            return None;
        }
        let mut buf = vec![0u8; max as usize];
        if !CFStringGetCString(
            cf,
            buf.as_mut_ptr() as *mut c_char,
            max,
            K_CF_STRING_ENCODING_UTF8,
        ) {
            return None;
        }
        let cstr = CStr::from_ptr(buf.as_ptr() as *const c_char);
        cstr.to_str().ok().map(ToString::to_string)
    }

    unsafe fn source_id_and_langs(src: *const c_void) -> Option<(String, Vec<String>)> {
        let id = cfstring_to_string(TISGetInputSourceProperty(src, kTISPropertyInputSourceID))?;
        let mut langs = Vec::new();
        let arr = TISGetInputSourceProperty(src, kTISPropertyInputSourceLanguages);
        if !arr.is_null() {
            let count = CFArrayGetCount(arr);
            for i in 0..count.min(32) {
                if let Some(s) = cfstring_to_string(CFArrayGetValueAtIndex(arr, i)) {
                    langs.push(s);
                }
            }
        }
        Some((id, langs))
    }

    unsafe fn cf_str(s: &std::ffi::CStr) -> *mut c_void {
        CFStringCreateWithCString(null(), s.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    }

    unsafe fn dict_string(dict: *const c_void, key: &std::ffi::CStr) -> Option<String> {
        let k = cf_str(key);
        if k.is_null() {
            return None;
        }
        let val = CFDictionaryGetValue(dict, k);
        CFRelease(k);
        if val.is_null() || CFGetTypeID(val) != CFStringGetTypeID() {
            return None;
        }
        cfstring_to_string(val)
    }

    /// Reads the live system-wide selected input source directly from `cfprefsd`
    /// (`com.apple.HIToolbox` -> `AppleSelectedInputSources`), avoiding the stale
    /// in-process cache of `TISCopyCurrentKeyboardInputSource` on non-runloop threads.
    unsafe fn read_live_hitoolbox_chinese() -> Option<bool> {
        let app_id = cf_str(c"com.apple.HIToolbox");
        let key = cf_str(c"AppleSelectedInputSources");
        if app_id.is_null() || key.is_null() {
            if !app_id.is_null() {
                CFRelease(app_id);
            }
            if !key.is_null() {
                CFRelease(key);
            }
            return None;
        }
        let _ = CFPreferencesAppSynchronize(app_id);
        let arr = CFPreferencesCopyAppValue(key, app_id);
        CFRelease(key);
        CFRelease(app_id);
        if arr.is_null() {
            return None;
        }
        let result = if CFGetTypeID(arr) == CFArrayGetTypeID() {
            let count = CFArrayGetCount(arr);
            let mut found = None;
            for i in 0..count {
                let item = CFArrayGetValueAtIndex(arr, i);
                if item.is_null() || CFGetTypeID(item) != CFDictionaryGetTypeID() {
                    continue;
                }
                let kind = dict_string(item, c"InputSourceKind").unwrap_or_default();
                if kind == "Non Keyboard Input Method" {
                    continue;
                }
                if let Some(mode) = dict_string(item, c"Input Mode") {
                    let is_zh = classify_macos_tis_chinese(&mode, &[]);
                    if let Ok(mut pref) = PREFERRED_SOURCES.lock() {
                        if is_zh {
                            pref.1 = Some(mode);
                        } else {
                            pref.0 = Some(mode);
                        }
                    }
                    found = Some(is_zh);
                    break;
                }
                if let Some(layout_name) = dict_string(item, c"KeyboardLayout Name") {
                    let pseudo_id = format!("com.apple.keylayout.{layout_name}");
                    let is_zh = classify_macos_tis_chinese(&pseudo_id, &[]);
                    if let Ok(mut pref) = PREFERRED_SOURCES.lock() {
                        if !is_zh {
                            pref.0 = Some(pseudo_id);
                        }
                    }
                    found = Some(is_zh);
                    break;
                }
                if let Some(bundle) = dict_string(item, c"Bundle ID") {
                    found = Some(classify_macos_tis_chinese(&bundle, &[]));
                    break;
                }
            }
            found
        } else {
            None
        };
        CFRelease(arr);
        result
    }

    pub fn read_ime_chinese() -> Option<bool> {
        unsafe {
            if let Some(live) = read_live_hitoolbox_chinese() {
                return Some(live);
            }
            let cur = TISCopyCurrentKeyboardInputSource();
            if cur.is_null() {
                return None;
            }
            let parsed = source_id_and_langs(cur);
            CFRelease(cur);
            let (id, langs) = parsed?;
            let is_zh = classify_macos_tis_chinese(&id, &langs);
            if let Ok(mut pref) = PREFERRED_SOURCES.lock() {
                if is_zh {
                    pref.1 = Some(id);
                } else {
                    pref.0 = Some(id);
                }
            }
            Some(is_zh)
        }
    }

    pub fn set_ime_chinese(want_zh: bool) {
        if read_ime_chinese() == Some(want_zh) {
            return;
        }
        let (pref_en, pref_zh) = PREFERRED_SOURCES
            .lock()
            .map(|g| g.clone())
            .unwrap_or((None, None));
        unsafe {
            let list = TISCreateInputSourceList(null(), false);
            if list.is_null() {
                return;
            }
            let count = CFArrayGetCount(list);
            let mut exact: *const c_void = null();
            let mut abc_fallback: *const c_void = null();
            let mut first_match: *const c_void = null();

            for i in 0..count {
                let src = CFArrayGetValueAtIndex(list, i);
                if src.is_null() {
                    continue;
                }
                let cat = TISGetInputSourceProperty(src, kTISPropertyInputSourceCategory);
                if cat.is_null() || !CFEqual(cat, kTISCategoryKeyboardInputSource) {
                    continue;
                }
                let sel = TISGetInputSourceProperty(src, kTISPropertyInputSourceIsSelectCapable);
                if sel.is_null() || !CFBooleanGetValue(sel) {
                    continue;
                }
                let en = TISGetInputSourceProperty(src, kTISPropertyInputSourceIsEnabled);
                if !en.is_null() && !CFBooleanGetValue(en) {
                    continue;
                }
                let Some((id, langs)) = source_id_and_langs(src) else {
                    continue;
                };
                let is_zh = classify_macos_tis_chinese(&id, &langs);
                if is_zh != want_zh {
                    continue;
                }
                if want_zh {
                    if pref_zh.as_deref() == Some(id.as_str()) {
                        exact = src;
                        break;
                    }
                    if first_match.is_null() {
                        first_match = src;
                    }
                } else {
                    if pref_en.as_deref() == Some(id.as_str()) {
                        exact = src;
                        break;
                    }
                    if (id == "com.apple.keylayout.ABC" || id == "com.apple.keylayout.US")
                        && abc_fallback.is_null()
                    {
                        abc_fallback = src;
                    }
                    if first_match.is_null() {
                        first_match = src;
                    }
                }
            }

            let chosen = if !exact.is_null() {
                exact
            } else if !abc_fallback.is_null() {
                abc_fallback
            } else {
                first_match
            };
            if !chosen.is_null() {
                let _ = TISSelectInputSource(chosen);
            }
            CFRelease(list);
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    pub fn read_caps_lock() -> bool {
        false
    }
    pub fn set_caps_lock(_desired: bool) -> bool {
        false
    }
    pub fn read_ime_chinese() -> Option<bool> {
        None
    }
    pub fn set_ime_chinese(_want_zh: bool) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_macos_chinese_and_english_sources() {
        assert!(classify_macos_tis_chinese(
            "com.apple.inputmethod.SCIM.ITABC",
            &["zh-Hans".into()]
        ));
        assert!(classify_macos_tis_chinese(
            "com.sogou.inputmethod.sogou.pinyin",
            &["zh-Hans".into()]
        ));
        assert!(classify_macos_tis_chinese(
            "com.tencent.inputmethod.wetype.pinyin",
            &[]
        ));
        assert!(!classify_macos_tis_chinese(
            "com.apple.keylayout.ABC",
            &["en".into(), "zh-Hans".into()]
        ));
        assert!(!classify_macos_tis_chinese(
            "com.apple.keylayout.US",
            &["en".into()]
        ));
    }

    #[test]
    fn read_state_does_not_panic() {
        let _ = read_state();
    }
}
