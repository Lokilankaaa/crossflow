//! Real isolated process recovery using only a recording fake input sink.
//! First: cargo build --example worker_fixture --locked
//! Then: cargo test --test injection_worker_process -- --ignored --test-threads=1
//! Optional CROSSFLOW_WORKER_FIXTURE selects a prebuilt fixture executable.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    root: PathBuf,
    executable: PathBuf,
    passed: bool,
}
impl Fixture {
    fn new(label: &str) -> Result<Self> {
        let executable = std::env::var_os("CROSSFLOW_WORKER_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("target/debug/examples")
                    .join(if cfg!(windows) {
                        "worker_fixture.exe"
                    } else {
                        "worker_fixture"
                    })
            });
        ensure!(
            executable.is_file(),
            "build the fake worker first: cargo build --example worker_fixture --locked"
        );
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("runs")
            .join(format!(
                "injection-worker-{label}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
            ));
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            executable,
            passed: false,
        })
    }
    fn command(&self, mode: &str) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .arg(mode)
            .arg(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW, fake fixture only.
        }
        command
    }
    fn events(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("events"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
    fn wait_for(&self, label: &str, mut predicate: impl FnMut() -> bool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            ensure!(
                !self.root.join("worker-watchdog").exists()
                    && !self.root.join("parent-watchdog").exists(),
                "fixture safety watchdog fired; not a successful worker cleanup"
            );
            if Instant::now() >= deadline {
                bail!(
                    "timed out waiting for {label}; fixture retained at {}",
                    self.root.display()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
    fn held(&self) -> Result<()> {
        self.wait_for("acknowledged held key and button", || {
            self.root.join("parent-ready").exists()
        })?;
        let events = self.events();
        ensure!(
            events.contains(&"key-down".into()) && events.contains(&"button-down".into()),
            "fixture did not actually inject the held fake pair"
        );
        Ok(())
    }
    fn observe_worker(&self) -> Result<WorkerExit> {
        self.wait_for("worker process identifier", || {
            fs::read_to_string(self.root.join("worker-pid"))
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some()
        })?;
        let pid = fs::read_to_string(self.root.join("worker-pid"))?.parse()?;
        let observed = WorkerExit::open(pid, &self.executable)?;
        fs::write(self.root.join("allow-parent-input"), b"observer-ready")?;
        Ok(observed)
    }
    fn released_and_returned(&self) -> Result<()> {
        self.wait_for("owned release and worker loop return", || {
            self.root.join("worker-exit").exists()
        })?;
        let events = self.events();
        for (down, up) in [("key-down", "key-up"), ("button-down", "button-up")] {
            let pressed = events
                .iter()
                .position(|event| event == down)
                .context("missing fake Down")?;
            let released = events
                .iter()
                .position(|event| event == up)
                .context("missing owned Up")?;
            ensure!(released > pressed, "release preceded ownership");
            ensure!(
                events.iter().filter(|event| event.as_str() == up).count() == 1,
                "duplicate release/replay"
            );
        }
        ensure!(
            !self.root.join("worker-watchdog").exists(),
            "watchdog exit is not cleanup"
        );
        Ok(())
    }
}

#[cfg(windows)]
struct WorkerExit(std::os::windows::io::OwnedHandle);
#[cfg(windows)]
impl WorkerExit {
    fn open(pid: u32, executable: &std::path::Path) -> Result<Self> {
        use std::os::windows::{
            ffi::OsStringExt,
            io::{AsRawHandle, FromRawHandle, OwnedHandle},
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            QueryFullProcessImageNameW,
        };
        // SAFETY: read-only query/synchronization access to the child PID from
        // this fixture's private directory. Never obtains termination access.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        ensure!(!handle.is_null(), "could not observe worker OS process");
        // SAFETY: OpenProcess returned a new owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut image = vec![0u16; 32768];
        let mut length = image.len() as u32;
        // SAFETY: valid owned process handle and writable bounded UTF-16 buffer.
        ensure!(
            unsafe {
                QueryFullProcessImageNameW(
                    handle.as_raw_handle(),
                    0,
                    image.as_mut_ptr(),
                    &mut length,
                )
            } != 0,
            "worker image query failed"
        );
        let actual = PathBuf::from(std::ffi::OsString::from_wide(&image[..length as usize]))
            .canonicalize()?;
        ensure!(
            actual == executable.canonicalize()?,
            "worker PID did not identify this fixture executable"
        );
        Ok(Self(handle))
    }
    fn wait(&self) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
        };
        // SAFETY: the read-only handle remains owned for this bounded wait.
        ensure!(
            unsafe { WaitForSingleObject(self.0.as_raw_handle(), 5000) } == WAIT_OBJECT_0,
            "worker returned but its OS process did not exit"
        );
        Ok(())
    }
}

#[cfg(unix)]
struct WorkerExit(libc::pid_t);
#[cfg(unix)]
impl WorkerExit {
    fn open(pid: u32, _: &std::path::Path) -> Result<Self> {
        let pid = libc::pid_t::try_from(pid)?;
        ensure!(pid > 0, "invalid fixture worker PID");
        // SAFETY: signal zero only checks existence; it sends no signal.
        ensure!(
            unsafe { libc::kill(pid, 0) } == 0,
            "fixture worker was not alive at observation"
        );
        Ok(Self(pid))
    }
    fn wait(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // A zombie still exists: it cannot satisfy this check. For the
            // killed-parent case the system must also reap the orphan; delayed
            // reaping fails explicitly rather than being reported as success.
            // SAFETY: signal zero is read-only and this PID came from our child.
            if unsafe { libc::kill(self.0, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "worker OS process still exists or awaits reaping"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if self.passed {
            let _ = fs::remove_dir_all(&self.root);
        }
        // On failure retain the unique directory for diagnostics. Each helper
        // self-exits at 12s even if broken; never terminate an unowned bare PID.
    }
}
struct OwnedChild(Child);
impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self> {
        Ok(Self(command.spawn()?))
    }
    fn finish(&mut self) -> Result<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait()? {
                return Ok(status);
            }
            ensure!(
                Instant::now() < deadline,
                "owned fixture child did not exit"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "explicit fake three-process crash recovery; build worker_fixture first"]
fn killed_parent_releases_held_pair_in_surviving_worker() -> Result<()> {
    let mut fixture = Fixture::new("kill")?;
    let mut parent = OwnedChild::spawn(fixture.command("parent").arg("healthy"))?;
    let observed = fixture.observe_worker()?;
    fixture.held()?;
    parent.0.kill()?;
    let status = parent.finish()?;
    ensure!(!status.success(), "parent was not forcibly stopped");
    fixture.released_and_returned()?;
    observed.wait()?;
    fixture.passed = true;
    Ok(())
}

#[test]
#[ignore = "explicit fake lease expiration; build worker_fixture first"]
fn alive_but_stalled_parent_cannot_keep_owned_inputs_held() -> Result<()> {
    let mut fixture = Fixture::new("stalled")?;
    let mut parent = OwnedChild::spawn(fixture.command("parent").arg("hold"))?;
    let observed = fixture.observe_worker()?;
    fixture.held()?;
    fixture.released_and_returned()?;
    #[cfg(windows)]
    observed.wait()?;
    ensure!(
        parent.0.try_wait()?.is_none(),
        "fixture parent exited instead of remaining stalled"
    );
    parent.0.kill()?;
    parent.finish()?;
    // On Unix a stalled parent cannot reap its exited child. Verify release
    // while it is alive, then require actual OS disappearance after reparenting.
    #[cfg(unix)]
    observed.wait()?;
    fixture.passed = true;
    Ok(())
}

#[test]
#[ignore = "explicit fake healthy heartbeat and shutdown; build worker_fixture first"]
fn healthy_parent_maintains_then_releases_without_replay() -> Result<()> {
    let mut fixture = Fixture::new("healthy")?;
    let mut parent = OwnedChild::spawn(fixture.command("parent").arg("healthy"))?;
    let observed = fixture.observe_worker()?;
    fixture.held()?;
    // Exercise several active lease periods, not merely an immediate shutdown.
    thread::sleep(Duration::from_millis(450));
    ensure!(
        parent.0.try_wait()?.is_none(),
        "healthy fixture parent unexpectedly stopped"
    );
    ensure!(
        !fixture
            .events()
            .iter()
            .any(|event| event == "key-up" || event == "button-up"),
        "worker expired despite execution-loop heartbeats"
    );
    fs::write(fixture.root.join("stop-parent"), b"stop")?;
    ensure!(parent.finish()?.success(), "healthy parent shutdown failed");
    fixture.released_and_returned()?;
    observed.wait()?;
    ensure!(
        fs::read_to_string(fixture.root.join("worker-exit"))? == "ok",
        "healthy worker did not complete its Shutdown acknowledgement"
    );
    fixture.passed = true;
    Ok(())
}

#[test]
#[ignore = "explicit fake truncated pipe frame; build worker_fixture first"]
fn partial_frame_eof_exits_without_injecting_input() -> Result<()> {
    let mut fixture = Fixture::new("partial-eof")?;
    let mut worker = OwnedChild::spawn(fixture.command("worker").stdin(Stdio::piped()))?;
    let mut input = worker.0.stdin.take().context("missing worker stdin")?;
    input.write_all(b"C")?;
    drop(input);
    ensure!(
        !worker.finish()?.success(),
        "truncated frame must not be successful"
    );
    ensure!(
        fixture.root.join("worker-exit").exists(),
        "worker exited without finishing run_worker"
    );
    ensure!(
        !fixture.events().iter().any(|event| matches!(
            event.as_str(),
            "key-down" | "button-down" | "move" | "scroll"
        )),
        "truncated frame reached fake adapter"
    );
    ensure!(
        !fixture.root.join("worker-watchdog").exists(),
        "watchdog exit is not EOF handling"
    );
    fixture.passed = true;
    Ok(())
}

#[test]
#[ignore = "explicit fake ACK pipe failure; build worker_fixture first"]
fn acknowledgement_failure_releases_inputs_after_adapter_return() -> Result<()> {
    let mut fixture = Fixture::new("ack-failure")?;
    let mut parent = OwnedChild::spawn(fixture.command("parent").arg("ack-failure"))?;
    let observed = fixture.observe_worker()?;
    fixture.held()?;
    ensure!(
        parent.finish()?.success(),
        "parent did not observe expected terminal ACK failure"
    );
    ensure!(
        fixture.root.join("parent-ack-error").exists(),
        "parent missed ACK failure"
    );
    fixture.released_and_returned()?;
    observed.wait()?;
    ensure!(
        fs::read_to_string(fixture.root.join("worker-exit"))? == "error",
        "worker ignored its broken response pipe"
    );
    let events = fixture.events();
    let moved = events
        .iter()
        .position(|event| event == "move")
        .context("Move never reached adapter before ACK fault")?;
    let released = events
        .iter()
        .position(|event| event == "key-up")
        .context("no cleanup")?;
    ensure!(
        moved < released,
        "test did not exercise ACK failure after adapter return"
    );
    fixture.passed = true;
    Ok(())
}
