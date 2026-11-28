//! Cross-platform system keepalive and display-wake helpers.
//!
//! Allows displays to turn off normally for power saving while keeping the
//! network/peer service alive, and wakes sleeping displays immediately when
//! remote mouse or keyboard input arrives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static LAST_WAKE_MS: AtomicU64 = AtomicU64::new(0);
const WAKE_THROTTLE_MS: u64 = 500;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn should_wake_now() -> bool {
    let now = now_ms();
    let prev = LAST_WAKE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < WAKE_THROTTLE_MS {
        return false;
    }
    LAST_WAKE_MS
        .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

/// Holds a system-sleep prevention assertion (still permits display sleep)
/// for the lifetime of the peer mesh process.
pub struct KeepAwakeGuard {
    #[cfg(target_os = "macos")]
    assertion_id: Option<u32>,
    #[cfg(windows)]
    active: bool,
}

impl KeepAwakeGuard {
    pub fn acquire() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                assertion_id: macos_impl::acquire_system_keepalive(),
            }
        }
        #[cfg(windows)]
        {
            windows_impl::set_system_required(true);
            Self { active: true }
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            Self {}
        }
    }
}

impl Drop for KeepAwakeGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if let Some(id) = self.assertion_id.take() {
                macos_impl::release_assertion(id);
            }
        }
        #[cfg(windows)]
        {
            if self.active {
                windows_impl::set_system_required(false);
                self.active = false;
            }
        }
    }
}

/// Immediately wakes a sleeping/black local display on control handoff (`Kind::Arm`).
/// Uses a short 200ms debounce so burst reconnects don't spam `powerd`, while
/// guaranteeing the display turns on immediately when the cursor enters this machine.
pub fn wake_display_now() {
    let now = now_ms();
    let prev = LAST_WAKE_MS.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < 200 {
        return;
    }
    LAST_WAKE_MS.store(now, Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    macos_impl::wake_display();
    #[cfg(windows)]
    windows_impl::wake_display();
}

/// Wakes a sleeping local display when remote mouse/keyboard input arrives.
/// Rate-limited so calling on the 1000Hz injection hot path is essentially free.
#[inline]
pub fn wake_display_throttled() {
    if !should_wake_now() {
        return;
    }
    #[cfg(target_os = "macos")]
    macos_impl::wake_display();
    #[cfg(windows)]
    windows_impl::wake_display();
}

#[cfg(target_os = "macos")]
mod macos_impl {
    use std::ffi::{c_char, c_void};
    use std::sync::atomic::{AtomicU32, Ordering};

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_IOPM_ASSERTION_LEVEL_ON: u32 = 255;
    const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;

    static LAST_USER_ACTIVITY_ID: AtomicU32 = AtomicU32::new(0);

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: *mut c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: *const c_void,
            level: u32,
            name: *const c_void,
            assertion_id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(assertion_id: u32) -> i32;
        fn IOPMAssertionDeclareUserActivity(
            assertion_name: *const c_void,
            user_type: u32,
            assertion_id: *mut u32,
        ) -> i32;
    }

    pub fn acquire_system_keepalive() -> Option<u32> {
        unsafe {
            let kind = CFStringCreateWithCString(
                std::ptr::null_mut(),
                c"PreventUserIdleSystemSleep".as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            );
            let reason = CFStringCreateWithCString(
                std::ptr::null_mut(),
                c"Crossflow LAN peer keepalive".as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            );
            if kind.is_null() || reason.is_null() {
                if !kind.is_null() {
                    CFRelease(kind);
                }
                if !reason.is_null() {
                    CFRelease(reason);
                }
                return None;
            }
            let mut id: u32 = 0;
            let rc = IOPMAssertionCreateWithName(kind, K_IOPM_ASSERTION_LEVEL_ON, reason, &mut id);
            CFRelease(kind);
            CFRelease(reason);
            (rc == 0).then_some(id)
        }
    }

    pub fn release_assertion(id: u32) {
        unsafe {
            let _ = IOPMAssertionRelease(id);
        }
    }

    /// Declares local user activity unconditionally (matching `caffeinate -u`) so
    /// Apple Silicon Macs with black/locked/dark-wake displays turn on immediately,
    /// even when `CGDisplayIsAsleep(CGMainDisplayID())` misreports 0 or no active displays.
    pub fn wake_display() {
        unsafe {
            let reason = CFStringCreateWithCString(
                std::ptr::null_mut(),
                c"Crossflow remote input wake".as_ptr(),
                K_CF_STRING_ENCODING_UTF8,
            );
            if reason.is_null() {
                return;
            }
            let mut id: u32 = LAST_USER_ACTIVITY_ID.load(Ordering::Relaxed);
            if IOPMAssertionDeclareUserActivity(reason, K_IOPM_USER_ACTIVE_LOCAL, &mut id) == 0 {
                LAST_USER_ACTIVITY_ID.store(id, Ordering::Relaxed);
            } else {
                // Retry with a fresh assertion slot if powerd retired the previous ID.
                let mut fresh_id: u32 = 0;
                if IOPMAssertionDeclareUserActivity(reason, K_IOPM_USER_ACTIVE_LOCAL, &mut fresh_id)
                    == 0
                {
                    LAST_USER_ACTIVITY_ID.store(fresh_id, Ordering::Relaxed);
                }
            }
            CFRelease(reason);
        }
    }
}

#[cfg(windows)]
mod windows_impl {
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
    const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
    const ES_CONTINUOUS: u32 = 0x8000_0000;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetThreadExecutionState(es_flags: u32) -> u32;
    }

    pub fn set_system_required(enable: bool) {
        unsafe {
            let flags = if enable {
                ES_CONTINUOUS | ES_SYSTEM_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            let _ = SetThreadExecutionState(flags);
        }
    }

    pub fn wake_display() {
        unsafe {
            let _ = SetThreadExecutionState(ES_DISPLAY_REQUIRED);
        }
    }
}
