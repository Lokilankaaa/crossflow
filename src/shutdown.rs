//! Cooperative process shutdown, separate from platform input injection.
//!
//! Hold the guard until input workers have joined and performance logs have
//! finished. Handlers only request a stop; the normal program path owns input
//! release and log finalization. This cannot clean up after SIGKILL, an aborted
//! process, a fatal crash, or an OS shutdown deadline that expires first.

use anyhow::{Context, Result, ensure};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

static INSTALLED: AtomicBool = AtomicBool::new(false);
static REQUESTED: AtomicBool = AtomicBool::new(false);
static RESTORE_FAILED: AtomicBool = AtomicBool::new(false);

/// One process-wide handler registration and its small flag bridge worker.
/// Do not replace these signal handlers or attach/detach the Windows console
/// while this guard is alive. Drop restores the prior handler configuration.
/// A restoration failure is reported to stderr and poisons future installation;
/// continued operation after that failure has no working shutdown guarantee.
pub struct Shutdown {
    registration: platform::Registration,
    quit: Arc<AtomicBool>,
    watcher: Option<JoinHandle<()>>,
}

impl Shutdown {
    pub fn install(stop: Arc<AtomicBool>) -> Result<Self> {
        ensure!(
            !RESTORE_FAILED.load(Ordering::Acquire),
            "shutdown handler restoration previously failed; cannot reinstall in this process"
        );
        ensure!(
            INSTALLED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a shutdown guard is already installed in this process"
        );
        REQUESTED.store(false, Ordering::Release);
        let mut registration = match platform::Registration::install() {
            Ok(registration) => registration,
            Err(error) => {
                if !RESTORE_FAILED.load(Ordering::Acquire) {
                    INSTALLED.store(false, Ordering::Release);
                }
                return Err(error);
            }
        };
        let quit = Arc::new(AtomicBool::new(false));
        let worker_quit = quit.clone();
        let watcher = thread::Builder::new()
            .name("crossflow-shutdown".into())
            .spawn(move || {
                while !worker_quit.load(Ordering::Acquire) {
                    if REQUESTED.load(Ordering::Acquire) {
                        stop.store(true, Ordering::Release);
                    }
                    thread::sleep(Duration::from_millis(2));
                }
            });
        match watcher {
            Ok(watcher) => Ok(Self {
                registration,
                quit,
                watcher: Some(watcher),
            }),
            Err(error) => {
                registration.cleanup_complete();
                let restored = registration.restore();
                let error = anyhow::Error::from(error).context("create shutdown flag bridge");
                match restored {
                    Ok(()) => {
                        INSTALLED.store(false, Ordering::Release);
                        Err(error)
                    }
                    Err(restore_error) => {
                        RESTORE_FAILED.store(true, Ordering::Release);
                        Err(error.context(format!(
                            "also failed to restore shutdown handlers: {restore_error:#}"
                        )))
                    }
                }
            }
        }
    }
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        // The caller's scope must place this after actual input/log cleanup.
        // A Windows close-handler thread may now allow process termination.
        self.registration.cleanup_complete();
        self.quit.store(true, Ordering::Release);
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        if let Err(error) = self.registration.restore() {
            RESTORE_FAILED.store(true, Ordering::Release);
            // This runs on the normal thread, never inside a signal handler.
            eprintln!("Could not restore shutdown handlers: {error:#}");
        } else {
            INSTALLED.store(false, Ordering::Release);
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, SetConsoleCtrlHandler,
    };

    // Completed generations are not reset on reinstall: an old close callback
    // still unwinding must not wait for a newly installed guard's cleanup.
    static GENERATION: AtomicU64 = AtomicU64::new(0);
    static COMPLETED: AtomicU64 = AtomicU64::new(0);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetTickCount64() -> u64;
        fn Sleep(milliseconds: u32);
    }

    unsafe extern "system" fn handler(event: u32) -> i32 {
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => {
                REQUESTED.store(true, Ordering::Release);
                1
            }
            CTRL_CLOSE_EVENT => {
                let generation = GENERATION.load(Ordering::Acquire);
                REQUESTED.store(true, Ordering::Release);
                // Windows invokes this on a separate thread, not as a Unix
                // async signal. No locks, Rust allocation, logs or input calls.
                // Leave margin under the normal 5 s Windows close deadline;
                // a shorter OS deadline or scheduler stall can still preempt us.
                let began = unsafe { GetTickCount64() };
                while COMPLETED.load(Ordering::Acquire) < generation
                    && unsafe { GetTickCount64() }.wrapping_sub(began) < 3_500
                {
                    unsafe { Sleep(2) };
                }
                1
            }
            _ => 0,
        }
    }

    pub(super) struct Registration {
        generation: u64,
        registered: bool,
    }
    impl Registration {
        pub(super) fn install() -> Result<Self> {
            let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
            if unsafe { SetConsoleCtrlHandler(Some(handler), 1) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("register Windows console shutdown handler");
            }
            Ok(Self {
                generation,
                registered: true,
            })
        }
        pub(super) fn cleanup_complete(&self) {
            COMPLETED.store(self.generation, Ordering::Release);
        }
        pub(super) fn restore(&mut self) -> Result<()> {
            if self.registered {
                if unsafe { SetConsoleCtrlHandler(Some(handler), 0) } == 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("remove Windows console shutdown handler");
                }
                self.registered = false;
            }
            Ok(())
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::*;

    extern "C" fn handler(_signal: libc::c_int) {
        // AtomicBool is lock-free on the supported x86_64/arm64 targets. Do not
        // allocate, access Arc, log, lock a mutex, or call any input API here.
        REQUESTED.store(true, Ordering::Release);
    }

    pub(super) struct Registration {
        previous: Vec<(libc::c_int, libc::sigaction)>,
    }
    impl Registration {
        pub(super) fn install() -> Result<Self> {
            let mut registration = Self {
                previous: Vec::with_capacity(3),
            };
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = handler as *const () as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("initialize shutdown signal mask");
            }
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                let mut old = std::mem::MaybeUninit::<libc::sigaction>::uninit();
                if unsafe { libc::sigaction(signal, &action, old.as_mut_ptr()) } != 0 {
                    let error = anyhow::Error::from(std::io::Error::last_os_error())
                        .context(format!("install shutdown handler for signal {signal}"));
                    return match registration.restore() {
                        Ok(()) => Err(error),
                        Err(restore_error) => {
                            RESTORE_FAILED.store(true, Ordering::Release);
                            Err(error.context(format!(
                                "also failed to roll back previous handlers: {restore_error:#}"
                            )))
                        }
                    };
                }
                registration
                    .previous
                    .push((signal, unsafe { old.assume_init() }));
            }
            Ok(registration)
        }
        pub(super) fn cleanup_complete(&self) {}
        pub(super) fn restore(&mut self) -> Result<()> {
            let mut first_error = None;
            // Attempt every restoration even if an earlier one fails.
            for (signal, previous) in std::mem::take(&mut self.previous).into_iter().rev() {
                if unsafe { libc::sigaction(signal, &previous, std::ptr::null_mut()) } != 0 {
                    if first_error.is_none() {
                        first_error = Some(
                            anyhow::Error::from(std::io::Error::last_os_error())
                                .context(format!("restore prior handler for signal {signal}")),
                        );
                    }
                    self.previous.push((signal, previous));
                }
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::*;
    pub(super) struct Registration;
    impl Registration {
        pub(super) fn install() -> Result<Self> {
            anyhow::bail!("cooperative shutdown is unsupported on this platform")
        }
        pub(super) fn cleanup_complete(&self) {}
        pub(super) fn restore(&mut self) -> Result<()> {
            Ok(())
        }
    }
}
