//! Read-only permission freshness, independent of input authority or leases.
//!
//! The owner must synchronously preflight permissions at startup/arm, then create
//! a new probe. Only the background closure calls the platform permission APIs.
//! `check` never calls that closure, takes a mutex, waits, or renews input authority.
//! Failures latch permanently; a later successful check cannot revive a probe.

use std::{
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionCheckError {
    Denied,
    Unavailable,
}

/// Fixed, redacted reasons. Neither platform errors nor permission-probe panic
/// payloads are retained or included in diagnostics returned by this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ProbeFailure {
    Denied = 1,
    Unavailable = 2,
    Stale = 3,
    Panicked = 4,
    WorkerStopped = 5,
    Stopped = 6,
    ClockInvalid = 7,
    InvalidConfig = 8,
    StartFailed = 9,
}
impl ProbeFailure {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Denied => "input permission was revoked",
            Self::Unavailable => "input permission could not be verified",
            Self::Stale => "input permission check expired",
            Self::Panicked => "input permission checker failed",
            Self::WorkerStopped => "input permission checker stopped unexpectedly",
            Self::Stopped => "input permission monitor was stopped",
            Self::ClockInvalid => "input permission clock is invalid",
            Self::InvalidConfig => "input permission monitor configuration is invalid",
            Self::StartFailed => "input permission monitor could not start",
        }
    }
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Denied,
            2 => Self::Unavailable,
            3 => Self::Stale,
            4 => Self::Panicked,
            5 => Self::WorkerStopped,
            6 => Self::Stopped,
            7 => Self::ClockInvalid,
            8 => Self::InvalidConfig,
            9 => Self::StartFailed,
            _ => Self::ClockInvalid,
        }
    }
}
impl fmt::Display for ProbeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for ProbeFailure {}

#[derive(Clone, Copy, Debug)]
pub struct ProbeConfig {
    pub poll_interval: Duration,
    pub max_success_age: Duration,
}
impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(25),
            max_success_age: Duration::from_millis(100),
        }
    }
}
impl ProbeConfig {
    fn validate(self) -> Result<Self, ProbeFailure> {
        if self.poll_interval.is_zero()
            || self.max_success_age.is_zero()
            || self.poll_interval > self.max_success_age
            || self.max_success_age > Duration::from_millis(100)
        {
            return Err(ProbeFailure::InvalidConfig);
        }
        Ok(self)
    }
}

// One atomic word prevents a successful refresh racing with a reader's stale
// check from clearing a failure or producing an inconsistent timestamp/reason.
// 60 bits of nanoseconds cover over 36 years from the probe's monotonic origin.
const REASON_SHIFT: u32 = 60;
const TIME_MASK: u64 = (1u64 << REASON_SHIFT) - 1;
type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

struct State {
    word: AtomicU64,
    max_age_ns: u64,
}
impl State {
    fn new(now: Duration, max_age: Duration) -> Result<Self, ProbeFailure> {
        Ok(Self {
            word: AtomicU64::new(timestamp(now)?),
            max_age_ns: timestamp(max_age)?,
        })
    }
    fn failure(word: u64) -> Option<ProbeFailure> {
        let reason = (word >> REASON_SHIFT) as u8;
        (reason != 0).then(|| ProbeFailure::from_code(reason))
    }
    fn latch(&self, reason: ProbeFailure) -> ProbeFailure {
        let mut old = self.word.load(Ordering::Acquire);
        loop {
            if let Some(existing) = Self::failure(old) {
                return existing;
            }
            match self.word.compare_exchange_weak(
                old,
                old | ((reason as u64) << REASON_SHIFT),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return reason,
                Err(current) => old = current,
            }
        }
    }
    /// Validate freshness and optionally publish a successful read-only probe.
    /// A late success is checked against the PREVIOUS success before publication.
    fn observe(&self, clock: &Clock, refresh: bool) -> Result<(), ProbeFailure> {
        loop {
            let old = self.word.load(Ordering::Acquire);
            if let Some(reason) = Self::failure(old) {
                return Err(reason);
            }
            let now = timestamp(clock());
            let reason = match now {
                Err(reason) => Some(reason),
                Ok(now) if now < old => Some(ProbeFailure::ClockInvalid),
                Ok(now) if now - old > self.max_age_ns => Some(ProbeFailure::Stale),
                Ok(_) => None,
            };
            let next = match reason {
                Some(reason) => old | ((reason as u64) << REASON_SHIFT),
                None if refresh => now.expect("validated monotonic timestamp"),
                None => return Ok(()),
            };
            // If a worker refreshed between the read and expiry decision, retry
            // with the newer state rather than falsely latching an old snapshot.
            if self
                .word
                .compare_exchange(old, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return reason.map_or(Ok(()), Err);
            }
        }
    }
}
fn timestamp(duration: Duration) -> Result<u64, ProbeFailure> {
    u64::try_from(duration.as_nanos())
        .ok()
        .filter(|n| *n <= TIME_MASK)
        .ok_or(ProbeFailure::ClockInvalid)
}

struct ExitGuard(Arc<State>);
impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.latch(ProbeFailure::WorkerStopped);
    }
}

pub struct PermissionProbe {
    state: Arc<State>,
    clock: Clock,
    stop: Sender<()>,
}
impl PermissionProbe {
    /// Call immediately after a synchronous startup/arm preflight succeeds.
    /// The closure must only inspect permissions, own no borrowed native input
    /// pointers, and never produce input or extend an input lease. It may outlive
    /// this handle briefly if an OS permission API is blocked during Drop.
    pub fn start_after_success<F>(check: F, config: ProbeConfig) -> Result<Self, ProbeFailure>
    where
        F: Fn() -> Result<(), PermissionCheckError> + Send + 'static,
    {
        let origin = Instant::now();
        Self::start_with_clock(check, config, Arc::new(move || origin.elapsed()))
    }
    fn start_with_clock<F>(
        check: F,
        config: ProbeConfig,
        clock: Clock,
    ) -> Result<Self, ProbeFailure>
    where
        F: Fn() -> Result<(), PermissionCheckError> + Send + 'static,
    {
        let config = config.validate()?;
        let state = Arc::new(State::new(clock(), config.max_success_age)?);
        let (stop, stopped) = mpsc::channel();
        let worker_state = state.clone();
        let worker_clock = clock.clone();
        // The detached thread owns only its read-only checker and shared atomic
        // state. Never join it while releasing remotely owned keys/buttons.
        thread::Builder::new()
            .name("crossflow-permission-probe".into())
            .spawn(move || {
                let _exit = ExitGuard(worker_state.clone());
                loop {
                    if worker_state.observe(&worker_clock, false).is_err() {
                        return;
                    }
                    let started = Instant::now();
                    match catch_unwind(AssertUnwindSafe(&check)) {
                        Ok(Ok(())) => {
                            if worker_state.observe(&worker_clock, true).is_err() {
                                return;
                            }
                        }
                        Ok(Err(PermissionCheckError::Denied)) => {
                            worker_state.latch(ProbeFailure::Denied);
                            return;
                        }
                        Ok(Err(PermissionCheckError::Unavailable)) => {
                            worker_state.latch(ProbeFailure::Unavailable);
                            return;
                        }
                        Err(_) => {
                            worker_state.latch(ProbeFailure::Panicked);
                            return;
                        }
                    }
                    match stopped
                        .recv_timeout(config.poll_interval.saturating_sub(started.elapsed()))
                    {
                        Err(RecvTimeoutError::Timeout) => {}
                        _ => {
                            worker_state.latch(ProbeFailure::Stopped);
                            return;
                        }
                    }
                }
            })
            .map_err(|_| ProbeFailure::StartFailed)?;
        Ok(Self { state, clock, stop })
    }
    /// Fast input-thread check: atomic state and monotonic time only. A fresh
    /// permission result is necessary, but never sufficient, to authorize input.
    pub fn check(&self) -> Result<(), ProbeFailure> {
        self.state.observe(&self.clock, false)
    }
}
impl Drop for PermissionProbe {
    fn drop(&mut self) {
        self.state.latch(ProbeFailure::Stopped);
        let _ = self.stop.send(());
        // Intentionally no join: the read-only OS query may be blocked.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn clock() -> (Arc<AtomicU64>, Clock) {
        let value = Arc::new(AtomicU64::new(0));
        let reading = value.clone();
        (
            value,
            Arc::new(move || Duration::from_millis(reading.load(Ordering::Acquire))),
        )
    }
    fn state() -> State {
        State::new(Duration::ZERO, Duration::from_millis(100)).unwrap()
    }
    fn await_failure(probe: &PermissionProbe) -> ProbeFailure {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Err(reason) = probe.check() {
                return reason;
            }
            assert!(Instant::now() < until, "checker did not finish");
            thread::yield_now();
        }
    }
    #[test]
    fn stale_boundary_is_bounded_and_never_auto_resumes() {
        let (time, clock) = clock();
        let state = state();
        time.store(100, Ordering::Release);
        assert_eq!(state.observe(&clock, false), Ok(()));
        time.store(101, Ordering::Release);
        assert_eq!(state.observe(&clock, false), Err(ProbeFailure::Stale));
        assert_eq!(state.observe(&clock, true), Err(ProbeFailure::Stale));
        assert_eq!(state.latch(ProbeFailure::Stopped), ProbeFailure::Stale);
    }
    #[test]
    fn successful_refresh_advances_only_permission_freshness() {
        let (time, clock) = clock();
        let state = state();
        time.store(75, Ordering::Release);
        assert_eq!(state.observe(&clock, true), Ok(()));
        time.store(175, Ordering::Release);
        assert_eq!(state.observe(&clock, false), Ok(()));
        time.store(176, Ordering::Release);
        assert_eq!(state.observe(&clock, false), Err(ProbeFailure::Stale));
    }
    #[test]
    fn late_background_success_cannot_mask_stall_even_without_input_polling() {
        let (time, clock) = clock();
        let state = state();
        time.store(101, Ordering::Release);
        assert_eq!(state.observe(&clock, true), Err(ProbeFailure::Stale));
    }
    #[test]
    fn explicit_rebuild_is_required_after_revocation() {
        let (time, clock) = clock();
        let state = state();
        assert_eq!(state.latch(ProbeFailure::Denied), ProbeFailure::Denied);
        time.store(10, Ordering::Release);
        assert_eq!(state.observe(&clock, true), Err(ProbeFailure::Denied));
        let replacement = State::new(clock(), Duration::from_millis(100)).unwrap();
        assert_eq!(replacement.observe(&clock, false), Ok(()));
    }
    #[test]
    fn clock_regression_and_overflow_fail_closed() {
        let (time, clock) = clock();
        let state = state();
        time.store(10, Ordering::Release);
        state.observe(&clock, true).unwrap();
        time.store(9, Ordering::Release);
        assert_eq!(
            state.observe(&clock, false),
            Err(ProbeFailure::ClockInvalid)
        );
        assert!(State::new(Duration::MAX, Duration::from_millis(100)).is_err());
    }
    #[test]
    fn invalid_config_cannot_weaken_maximum_age() {
        for config in [
            ProbeConfig {
                poll_interval: Duration::ZERO,
                ..Default::default()
            },
            ProbeConfig {
                max_success_age: Duration::from_millis(101),
                ..Default::default()
            },
            ProbeConfig {
                max_success_age: Duration::from_millis(20),
                ..Default::default()
            },
        ] {
            assert_eq!(config.validate().unwrap_err(), ProbeFailure::InvalidConfig);
        }
    }
    #[test]
    fn background_revocation_and_unavailability_latch_redacted_reasons() {
        for (error, reason) in [
            (PermissionCheckError::Denied, ProbeFailure::Denied),
            (PermissionCheckError::Unavailable, ProbeFailure::Unavailable),
        ] {
            let (_, clock) = clock();
            let calls = Arc::new(AtomicUsize::new(0));
            let invoked = calls.clone();
            let probe = PermissionProbe::start_with_clock(
                move || {
                    invoked.fetch_add(1, Ordering::AcqRel);
                    Err(error)
                },
                ProbeConfig::default(),
                clock,
            )
            .unwrap();
            assert_eq!(await_failure(&probe), reason);
            assert_eq!(probe.check(), Err(reason));
            assert_eq!(calls.load(Ordering::Acquire), 1);
        }
    }
    #[test]
    fn background_panic_is_terminal_and_returns_no_payload() {
        let (_, clock) = clock();
        let probe = PermissionProbe::start_with_clock(
            || panic!("test-only internal detail"),
            ProbeConfig::default(),
            clock,
        )
        .unwrap();
        assert_eq!(await_failure(&probe), ProbeFailure::Panicked);
        assert_eq!(
            probe.check().unwrap_err().to_string(),
            "input permission checker failed"
        );
    }
    #[test]
    fn unexpected_worker_exit_latches_failure() {
        let state = Arc::new(state());
        {
            let _guard = ExitGuard(state.clone());
        }
        assert_eq!(
            State::failure(state.word.load(Ordering::Acquire)),
            Some(ProbeFailure::WorkerStopped)
        );
    }
    #[test]
    fn blocked_checker_expires_and_drop_does_not_wait_for_it() {
        let (time, clock) = clock();
        let (entered, entry) = mpsc::channel();
        let (unblock, blocked) = mpsc::channel();
        let (exited, exit) = mpsc::channel();
        let probe = PermissionProbe::start_with_clock(
            move || {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                exited.send(()).unwrap();
                Ok(())
            },
            ProbeConfig::default(),
            clock.clone(),
        )
        .unwrap();
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        time.store(101, Ordering::Release);
        assert_eq!(probe.check(), Err(ProbeFailure::Stale));
        let shared = probe.state.clone();
        let started = Instant::now();
        drop(probe);
        assert!(started.elapsed() < Duration::from_millis(100));
        unblock.send(()).unwrap();
        exit.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(shared.observe(&clock, true), Err(ProbeFailure::Stale));
    }
    #[test]
    fn drop_is_terminal_even_if_inflight_probe_later_succeeds() {
        let (_, clock) = clock();
        let (entered, entry) = mpsc::channel();
        let (unblock, blocked) = mpsc::channel();
        let probe = PermissionProbe::start_with_clock(
            move || {
                entered.send(()).unwrap();
                blocked.recv().unwrap();
                Ok(())
            },
            ProbeConfig::default(),
            clock.clone(),
        )
        .unwrap();
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        let shared = probe.state.clone();
        drop(probe);
        assert_eq!(shared.observe(&clock, false), Err(ProbeFailure::Stopped));
        unblock.send(()).unwrap();
        assert_eq!(shared.observe(&clock, true), Err(ProbeFailure::Stopped));
    }
}
