//! Optional Windows `LocalSystem` input-injection service (`CrossflowInputService`).
//!
//! Architecture (identical in principle to PowerToys Mouse Without Borders Service Mode):
//! 1. `CrossflowInputService` runs in Session 0 under SCM as `NT AUTHORITY\SYSTEM`.
//! 2. It watches `WTSGetActiveConsoleSessionId()` and spawns `crossflow.exe service-worker`
//!    INTO the active interactive session with a duplicated `LocalSystem` primary token.
//! 3. `service-worker` runs inside the user's interactive session as `LocalSystem`, hosts an
//!    ACL-restricted, local-only overlapped Named Pipe (`\\.\pipe\CrossflowInputService-v1-s{sid}`)
//!    carrying the exact `worker_protocol` v3 frames, and dynamically switches its injection
//!    thread between `WinSta0\Default` and `WinSta0\Winlogon` (UAC Secure Desktop & Lock Screen).
//! 4. The normal user-session `crossflow.exe peer` connects to this pipe automatically when
//!    available, or seamlessly falls back to the portable child-process `input-worker`.

use crate::platform::windows::wide;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    ffi::c_void,
    fs,
    io::{self, Read, Write},
    mem::size_of,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_PIPE_CONNECTED,
        ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
        GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
        WAIT_OBJECT_0,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1,
        },
        DuplicateTokenEx, GetTokenInformation, SECURITY_ATTRIBUTES, SecurityIdentification,
        TOKEN_ALL_ACCESS, TOKEN_USER, TokenPrimary, TokenSessionId, TokenUser,
        SetTokenInformation,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
        },
        RemoteDesktop::{
            ProcessIdToSessionId, WTSGetActiveConsoleSessionId, WTSQueryUserToken,
        },
        Services::{
            ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
            CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx,
            RegisterServiceCtrlHandlerExW, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT,
            SC_STATUS_PROCESS_INFO, SERVICE_ACCEPT_SESSIONCHANGE, SERVICE_ACCEPT_SHUTDOWN,
            SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
            SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SESSIONCHANGE, SERVICE_CONTROL_SHUTDOWN,
            SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_NO_CHANGE,
            SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STATUS,
            SERVICE_STATUS_HANDLE, SERVICE_STATUS_PROCESS, SERVICE_STOP_PENDING, SERVICE_STOPPED,
            SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, StartServiceCtrlDispatcherW,
            StartServiceW, SetServiceStatus,
        },
        Threading::{
            CREATE_NO_WINDOW, CreateEventW, CreateProcessAsUserW, GetCurrentProcess,
            GetCurrentProcessId, GetExitCodeProcess, HIGH_PRIORITY_CLASS, INFINITE,
            OpenProcessToken, PROCESS_INFORMATION, ResetEvent, STARTUPINFOW, SetEvent,
            TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
        },
    },
};

pub const SERVICE_NAME: &str = "CrossflowInputService";
pub const SERVICE_DISPLAY_NAME: &str = "Crossflow Input Service";
pub const SERVICE_DESCRIPTION: &str = "Enables Crossflow remote mouse and keyboard control across Windows UAC prompts, elevated apps, and the lock screen.";

const STILL_ACTIVE: u32 = 259;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ServiceStatus {
    pub installed: bool,
    pub running: bool,
    pub pipe_ready: bool,
}

pub fn pipe_name_for_session(session_id: u32) -> String {
    format!(r"\\.\pipe\CrossflowInputService-v1-s{session_id}")
}

pub fn current_session_id() -> u32 {
    let mut sid: u32 = 0;
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) };
    if ok != 0 { sid } else { 0 }
}

pub fn installed_binary_path() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("Crossflow").join("crossflow.exe")
}

/// Non-blocking status check usable by standard (non-admin) user processes.
pub fn query_status() -> ServiceStatus {
    let pipe = pipe_name_for_session(current_session_id());
    let pipe_w = wide(&pipe);
    let pipe_ready = unsafe { WaitNamedPipeW(pipe_w.as_ptr(), 1) } != 0;

    let scm = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_CONNECT) };
    if scm.is_null() {
        return ServiceStatus {
            installed: pipe_ready,
            running: pipe_ready,
            pipe_ready,
        };
    }
    let name_w = wide(SERVICE_NAME);
    let svc = unsafe { OpenServiceW(scm, name_w.as_ptr(), SERVICE_QUERY_STATUS) };
    if svc.is_null() {
        unsafe { CloseServiceHandle(scm) };
        return ServiceStatus {
            installed: false,
            running: false,
            pipe_ready,
        };
    }
    let mut ssp: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed: u32 = 0;
    let ok = unsafe {
        QueryServiceStatusEx(
            svc,
            SC_STATUS_PROCESS_INFO,
            (&mut ssp as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    };
    unsafe {
        CloseServiceHandle(svc);
        CloseServiceHandle(scm);
    }
    let running = ok != 0 && ssp.dwCurrentState == SERVICE_RUNNING;
    ServiceStatus {
        installed: true,
        running,
        pipe_ready,
    }
}

fn query_service_state(svc: SC_HANDLE) -> Option<u32> {
    let mut ssp: SERVICE_STATUS_PROCESS = unsafe { std::mem::zeroed() };
    let mut needed = 0u32;
    let ok = unsafe {
        QueryServiceStatusEx(
            svc,
            SC_STATUS_PROCESS_INFO,
            (&mut ssp as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of::<SERVICE_STATUS_PROCESS>() as u32,
            &mut needed,
        )
    };
    (ok != 0).then_some(ssp.dwCurrentState)
}

fn stop_service_handle(svc: SC_HANDLE, timeout: Duration) -> Result<()> {
    let mut status: SERVICE_STATUS = unsafe { std::mem::zeroed() };
    let ok = unsafe { ControlService(svc, SERVICE_CONTROL_STOP, &mut status) };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        if err == ERROR_SERVICE_NOT_ACTIVE {
            return Ok(());
        }
        bail!("ControlService(STOP) failed (Win32 {err})");
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if query_service_state(svc) == Some(SERVICE_STOPPED) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(80));
    }
    bail!("CrossflowInputService did not stop within timeout")
}

/// Install (or upgrade) `CrossflowInputService` as a `LocalSystem` auto-start service
/// and start it immediately. Requires Administrator privileges (elevated via UAC once).
pub fn install_service() -> Result<()> {
    let current_exe = std::env::current_exe().context("resolve current crossflow.exe path")?;
    let target_exe = installed_binary_path();
    let target_dir = target_exe
        .parent()
        .context("invalid ProgramData service path")?;
    fs::create_dir_all(target_dir).context("create ProgramData\\Crossflow directory")?;

    let scm = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_ALL_ACCESS) };
    ensure!(
        !scm.is_null(),
        "OpenSCManagerW failed (Win32 {}): Administrator privileges required",
        unsafe { GetLastError() }
    );
    let _scm_guard = ScHandleGuard(scm);

    let name_w = wide(SERVICE_NAME);
    let existing = unsafe { OpenServiceW(scm, name_w.as_ptr(), SERVICE_ALL_ACCESS) };
    if !existing.is_null() {
        let _ = stop_service_handle(existing, Duration::from_secs(5));
        unsafe { CloseServiceHandle(existing) };
        // Give the session child worker a brief moment to release the binary file lock.
        thread::sleep(Duration::from_millis(250));
    }

    if !same_path(&current_exe, &target_exe) {
        let mut last_err = None;
        for _ in 0..10 {
            match fs::copy(&current_exe, &target_exe) {
                Ok(_) => {
                    last_err = None;
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    thread::sleep(Duration::from_millis(150));
                }
            }
        }
        if let Some(e) = last_err {
            return Err(e).with_context(|| {
                format!(
                    "copy crossflow.exe to {} failed",
                    target_exe.to_string_lossy()
                )
            });
        }
    }

    let bin_cmd = format!("\"{}\" windows-service", target_exe.to_string_lossy());
    let bin_cmd_w = wide(&bin_cmd);
    let display_w = wide(SERVICE_DISPLAY_NAME);

    let svc = unsafe { OpenServiceW(scm, name_w.as_ptr(), SERVICE_ALL_ACCESS) };
    let svc = if svc.is_null() {
        let created = unsafe {
            CreateServiceW(
                scm,
                name_w.as_ptr(),
                display_w.as_ptr(),
                SERVICE_ALL_ACCESS,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                bin_cmd_w.as_ptr(),
                null(),
                null_mut(),
                null(),
                null(), // LocalSystem
                null(),
            )
        };
        ensure!(
            !created.is_null(),
            "CreateServiceW failed (Win32 {})",
            unsafe { GetLastError() }
        );
        created
    } else {
        let ok = unsafe {
            ChangeServiceConfigW(
                svc,
                SERVICE_WIN32_OWN_PROCESS,
                SERVICE_AUTO_START,
                SERVICE_ERROR_NORMAL,
                bin_cmd_w.as_ptr(),
                null(),
                null_mut(),
                null(),
                null(),
                null(),
                display_w.as_ptr(),
            )
        };
        ensure!(
            ok != 0,
            "ChangeServiceConfigW failed (Win32 {})",
            unsafe { GetLastError() }
        );
        svc
    };
    let _svc_guard = ScHandleGuard(svc);

    let mut desc_w = wide(SERVICE_DESCRIPTION);
    let mut desc = SERVICE_DESCRIPTIONW {
        lpDescription: desc_w.as_mut_ptr(),
    };
    unsafe {
        ChangeServiceConfig2W(
            svc,
            SERVICE_CONFIG_DESCRIPTION,
            (&mut desc as *mut SERVICE_DESCRIPTIONW).cast(),
        );
    }

    let start_ok = unsafe { StartServiceW(svc, 0, null()) };
    if start_ok == 0 {
        let err = unsafe { GetLastError() };
        if err != ERROR_SERVICE_ALREADY_RUNNING {
            bail!("StartServiceW failed (Win32 {err})");
        }
    }
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if query_service_state(svc) == Some(SERVICE_RUNNING) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(80));
    }
    let _ = SERVICE_NO_CHANGE;
    bail!("CrossflowInputService did not reach RUNNING state")
}

/// Stop and remove `CrossflowInputService` and clean up its ProgramData binary.
pub fn uninstall_service() -> Result<()> {
    let scm = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_ALL_ACCESS) };
    ensure!(
        !scm.is_null(),
        "OpenSCManagerW failed (Win32 {}): Administrator privileges required",
        unsafe { GetLastError() }
    );
    let _scm_guard = ScHandleGuard(scm);

    let name_w = wide(SERVICE_NAME);
    let svc = unsafe { OpenServiceW(scm, name_w.as_ptr(), SERVICE_ALL_ACCESS) };
    if svc.is_null() {
        let err = unsafe { GetLastError() };
        if err == ERROR_SERVICE_DOES_NOT_EXIST {
            let _ = fs::remove_file(installed_binary_path());
            return Ok(());
        }
        bail!("OpenServiceW failed (Win32 {err})");
    }
    {
        let _svc_guard = ScHandleGuard(svc);
        let _ = stop_service_handle(svc, Duration::from_secs(5));
        let ok = unsafe { DeleteService(svc) };
        ensure!(ok != 0, "DeleteService failed (Win32 {})", unsafe {
            GetLastError()
        });
    }
    thread::sleep(Duration::from_millis(250));
    let _ = fs::remove_file(installed_binary_path());
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

struct ScHandleGuard(SC_HANDLE);
impl Drop for ScHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

// ============================================================================
// Overlapped Full-Duplex Named Pipe (Zero Read/Write Deadlock!)
// ============================================================================

pub(crate) fn append_service_log(msg: &str) {
    let dir = installed_binary_path()
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\Crossflow"));
    let _ = fs::create_dir_all(&dir);
    let log_path = dir.join("service-worker.log");
    if let Ok(meta) = fs::metadata(&log_path)
        && meta.len() > 64 * 1024
    {
        let _ = fs::write(&log_path, b"");
    }
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{ts}] {msg}");
    }
}

struct SharedPipeHandle {
    handle: HANDLE,
    is_server: bool,
    close_event: EventGuard,
    closed: AtomicBool,
}
unsafe impl Send for SharedPipeHandle {}
unsafe impl Sync for SharedPipeHandle {}

impl SharedPipeHandle {
    fn new(handle: HANDLE, is_server: bool) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            handle,
            is_server,
            close_event: EventGuard::new_manual()?,
            closed: AtomicBool::new(false),
        }))
    }

    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            unsafe {
                SetEvent(self.close_event.0);
                CancelIoEx(self.handle, null());
                if self.is_server {
                    DisconnectNamedPipe(self.handle);
                }
            }
        }
    }
}

impl Drop for SharedPipeHandle {
    fn drop(&mut self) {
        self.close();
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// Explicitly cancels blocked overlapped I/O on a service pipe and disconnects it so
/// detached reader/writer threads exit immediately instead of leaking the pipe handle.
pub struct PipeCloser(Arc<SharedPipeHandle>);
impl PipeCloser {
    pub fn close(&self) {
        self.0.close();
    }
}

struct EventGuard(HANDLE);
impl EventGuard {
    fn new_manual() -> io::Result<Self> {
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(h))
    }
}
impl Drop for EventGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn wait_overlapped_or_close(
    pipe: &SharedPipeHandle,
    ov: &mut OVERLAPPED,
    bytes: &mut u32,
) -> Result<(), u32> {
    let handles = [ov.hEvent, pipe.close_event.0];
    let waited = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
    if waited == WAIT_OBJECT_0 {
        if unsafe { GetOverlappedResult(pipe.handle, ov, bytes, 1) } != 0 {
            return Ok(());
        }
        return Err(unsafe { GetLastError() });
    }
    // Close signaled (or unexpected wait failure): cancel this specific overlapped I/O
    // and wait for the kernel to finish referencing `ov` before returning.
    unsafe {
        CancelIoEx(pipe.handle, ov);
        let _ = GetOverlappedResult(pipe.handle, ov, bytes, 1);
    }
    Err(ERROR_BROKEN_PIPE)
}

pub struct PipeReader {
    pipe: Arc<SharedPipeHandle>,
    event: EventGuard,
}
unsafe impl Send for PipeReader {}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pipe.closed.load(Ordering::Acquire) {
            return Ok(0);
        }
        unsafe {
            ResetEvent(self.event.0);
        }
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        let mut read_bytes: u32 = 0;
        let ok = unsafe {
            ReadFile(
                self.pipe.handle,
                buf.as_mut_ptr(),
                buf.len().min(u32::MAX as usize) as u32,
                &mut read_bytes,
                &mut ov,
            )
        };
        if ok != 0 {
            return Ok(read_bytes as usize);
        }
        let err = unsafe { GetLastError() };
        if err == ERROR_MORE_DATA {
            return Ok(read_bytes as usize);
        }
        if err == ERROR_BROKEN_PIPE {
            return Ok(0);
        }
        if err != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        match wait_overlapped_or_close(&self.pipe, &mut ov, &mut read_bytes) {
            Ok(()) => Ok(read_bytes as usize),
            Err(e) if e == ERROR_BROKEN_PIPE => Ok(0),
            Err(e) if e == ERROR_MORE_DATA => Ok(read_bytes as usize),
            Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
        }
    }
}

pub struct PipeWriter {
    pipe: Arc<SharedPipeHandle>,
    event: EventGuard,
}
unsafe impl Send for PipeWriter {}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.pipe.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "service pipe closed",
            ));
        }
        unsafe {
            ResetEvent(self.event.0);
        }
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = self.event.0;
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                self.pipe.handle,
                buf.as_ptr(),
                buf.len().min(u32::MAX as usize) as u32,
                &mut written,
                &mut ov,
            )
        };
        if ok != 0 {
            return Ok(written as usize);
        }
        let err = unsafe { GetLastError() };
        if err != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        match wait_overlapped_or_close(&self.pipe, &mut ov, &mut written) {
            Ok(()) => Ok(written as usize),
            Err(e) if e == ERROR_BROKEN_PIPE => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "service pipe closed",
            )),
            Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // Windows overlapped named pipes deliver WriteFile bytes directly to the kernel
        // buffer. Never call FlushFileBuffers here, which blocks until the peer drains.
        Ok(())
    }
}

/// Client-side connection from user-session `crossflow.exe peer` to the active
/// `LocalSystem` `service-worker` named pipe in this interactive session.
pub fn connect_service_pipe() -> Result<(PipeReader, PipeWriter, PipeCloser)> {
    let pipe_name = pipe_name_for_session(current_session_id());
    connect_named_pipe(&pipe_name)
}

fn connect_named_pipe(pipe_name: &str) -> Result<(PipeReader, PipeWriter, PipeCloser)> {
    const ERROR_PIPE_BUSY: u32 = 231;
    let pipe_w = wide(pipe_name);
    let mut handle: HANDLE = INVALID_HANDLE_VALUE;
    let mut last_err: u32 = 0;
    for attempt in 0..4 {
        let _ = unsafe { WaitNamedPipeW(pipe_w.as_ptr(), 120) };
        handle = unsafe {
            CreateFileW(
                pipe_w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
            break;
        }
        last_err = unsafe { GetLastError() };
        if attempt < 3 && (last_err == ERROR_PIPE_BUSY || last_err == 2) {
            thread::sleep(Duration::from_millis(60));
            continue;
        }
        break;
    }
    ensure!(
        handle != INVALID_HANDLE_VALUE && !handle.is_null(),
        "connect to CrossflowInputService pipe failed (Win32 {last_err})"
    );
    let shared = match SharedPipeHandle::new(handle, false) {
        Ok(s) => s,
        Err(err) => {
            unsafe { CloseHandle(handle) };
            return Err(err.into());
        }
    };
    let reader = PipeReader {
        pipe: shared.clone(),
        event: EventGuard::new_manual()?,
    };
    let writer = PipeWriter {
        pipe: shared.clone(),
        event: EventGuard::new_manual()?,
    };
    let closer = PipeCloser(shared);
    Ok((reader, writer, closer))
}

// ============================================================================
// Interactive Session `SYSTEM` Service Worker (`crossflow.exe service-worker`)
// ============================================================================

fn session_user_sid_string(session_id: u32) -> Option<String> {
    let mut token: HANDLE = null_mut();
    if unsafe { WTSQueryUserToken(session_id, &mut token) } == 0 || token.is_null() {
        return None;
    }
    let sid_str = (|| {
        let mut needed: u32 = 0;
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed);
        }
        if needed == 0 || needed > 4096 {
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if ok == 0 {
            return None;
        }
        let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        let mut wide_ptr: *mut u16 = null_mut();
        if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut wide_ptr) } == 0
            || wide_ptr.is_null()
        {
            return None;
        }
        let mut len = 0usize;
        unsafe {
            while *wide_ptr.add(len) != 0 && len < 256 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(wide_ptr, len);
            let s = String::from_utf16_lossy(slice);
            LocalFree(wide_ptr.cast());
            Some(s)
        }
    })();
    unsafe { CloseHandle(token) };
    sid_str
}

fn pipe_sddl_for_session(session_id: u32) -> String {
    if let Some(sid) = session_user_sid_string(session_id) {
        // Strictly allow only LocalSystem, Built-in Administrators, and the active
        // interactive console user of this exact session.
        format!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})")
    } else {
        // Pre-logon fallback: LocalSystem, Administrators, and Interactive Users only.
        "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;IU)".into()
    }
}

fn create_acl_pipe_server(pipe_name: &str, sddl: &str) -> Result<HANDLE> {
    let sddl_w = wide(sddl);
    let mut sd: *mut c_void = null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl_w.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            null_mut(),
        )
    };
    ensure!(
        ok != 0 && !sd.is_null(),
        "ConvertStringSecurityDescriptorToSecurityDescriptorW failed (Win32 {})",
        unsafe { GetLastError() }
    );
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let pipe_w = wide(pipe_name);
    let handle = unsafe {
        CreateNamedPipeW(
            pipe_w.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            4,
            4096,
            4096,
            0,
            &sa,
        )
    };
    unsafe { LocalFree(sd) };
    ensure!(
        handle != INVALID_HANDLE_VALUE && !handle.is_null(),
        "CreateNamedPipeW failed (Win32 {})",
        unsafe { GetLastError() }
    );
    Ok(handle)
}

fn wait_pipe_client(handle: HANDLE, stop: &AtomicBool) -> Result<bool> {
    let ev = EventGuard::new_manual()?;
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.0;
    let ok = unsafe { ConnectNamedPipe(handle, &mut ov) };
    if ok != 0 {
        return Ok(true);
    }
    let err = unsafe { GetLastError() };
    if err == ERROR_PIPE_CONNECTED {
        return Ok(true);
    }
    if err != ERROR_IO_PENDING {
        bail!("ConnectNamedPipe failed (Win32 {err})");
    }
    loop {
        if stop.load(Ordering::Acquire) {
            unsafe { CancelIoEx(handle, &ov) };
            return Ok(false);
        }
        let w = unsafe { WaitForSingleObject(ev.0, 200) };
        if w == 0 {
            let mut transferred = 0u32;
            let res = unsafe { GetOverlappedResult(handle, &ov, &mut transferred, 0) };
            return Ok(res != 0);
        }
    }
}

fn serve_one_pipe_client(
    handle: HANDLE,
    mut build_sink: impl FnMut() -> Result<Box<dyn crate::session::MouseSink>>,
) {
    let shared = match SharedPipeHandle::new(handle, true) {
        Ok(s) => s,
        Err(err) => {
            append_service_log(&format!("pipe handle init failed: {err}"));
            unsafe {
                DisconnectNamedPipe(handle);
                CloseHandle(handle);
            }
            return;
        }
    };
    let reader = match EventGuard::new_manual() {
        Ok(event) => PipeReader {
            pipe: shared.clone(),
            event,
        },
        Err(err) => {
            append_service_log(&format!("pipe reader event failed: {err}"));
            shared.close();
            return;
        }
    };
    let writer = match EventGuard::new_manual() {
        Ok(event) => PipeWriter {
            pipe: shared.clone(),
            event,
        },
        Err(err) => {
            append_service_log(&format!("pipe writer event failed: {err}"));
            shared.close();
            return;
        }
    };

    match build_sink() {
        Ok(mut sink) => {
            let worker_res = crate::injection_worker::run_worker(reader, writer, sink.as_mut());
            let _ = sink.release_owned();
            if let Err(err) = worker_res {
                append_service_log(&format!("run_worker ended with error: {err:#}"));
            }
        }
        Err(err) => {
            append_service_log(&format!("new_service sink init failed: {err:#}"));
            drop(reader);
            drop(writer);
        }
    }

    // Explicitly wake and cancel the detached reader/writer threads spawned inside
    // `run_worker` so they drop their `Arc<SharedPipeHandle>` and release the pipe
    // handle immediately instead of deadlocking future clients with ERROR_PIPE_BUSY.
    shared.close();
    let weak: Weak<SharedPipeHandle> = Arc::downgrade(&shared);
    drop(shared);
    let wait_until = Instant::now() + Duration::from_millis(250);
    while weak.upgrade().is_some() && Instant::now() < wait_until {
        thread::sleep(Duration::from_millis(2));
    }
}

/// Runs inside the active interactive session as `NT AUTHORITY\SYSTEM`.
/// Accepts one authenticated local client at a time over the session's ACL-locked pipe,
/// injects input across both `WinSta0\Default` and `WinSta0\Winlogon` (UAC & Lock Screen),
/// and guarantees full `release_owned()` cleanup whenever a client disconnects.
pub fn run_service_worker(session_id: u32) -> Result<()> {
    crate::power::configure_realtime_thread(true);
    let stop = Arc::new(AtomicBool::new(false));
    let pipe_name = pipe_name_for_session(session_id);
    append_service_log(&format!("service-worker started for session {session_id}"));

    while !stop.load(Ordering::Acquire) {
        let sddl = pipe_sddl_for_session(session_id);
        let handle = match create_acl_pipe_server(&pipe_name, &sddl) {
            Ok(h) => h,
            Err(err) => {
                append_service_log(&format!("create_acl_pipe_server failed: {err:#}"));
                thread::sleep(Duration::from_millis(300));
                continue;
            }
        };
        let connected = match wait_pipe_client(handle, &stop) {
            Ok(true) => true,
            Ok(false) => {
                unsafe { CloseHandle(handle) };
                break;
            }
            Err(err) => {
                append_service_log(&format!("wait_pipe_client failed: {err:#}"));
                unsafe { CloseHandle(handle) };
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        if !connected {
            unsafe { CloseHandle(handle) };
            continue;
        }
        serve_one_pipe_client(handle, || {
            let emergency = Arc::new(AtomicBool::new(false));
            let sink = crate::platform::windows::WindowsMouseSink::new_service(1.0, emergency)?;
            Ok(Box::new(sink))
        });
    }
    Ok(())
}

// ============================================================================
// Session-0 Windows Service Host (`crossflow.exe windows-service`)
// ============================================================================

static SERVICE_STOP: AtomicBool = AtomicBool::new(false);
static SERVICE_SESSION_CHANGED: AtomicBool = AtomicBool::new(false);
static SERVICE_STATUS_HANDLE_RAW: AtomicU32 = AtomicU32::new(0);
// Store pointer-sized handle cleanly via AtomicUsize
static SERVICE_STATUS_PTR: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn report_scm_status(state: u32, exit_code: u32, wait_hint_ms: u32) {
    let raw = SERVICE_STATUS_PTR.load(Ordering::Acquire);
    if raw == 0 {
        return;
    }
    let handle = raw as SERVICE_STATUS_HANDLE;
    let controls = if state == SERVICE_RUNNING {
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_SESSIONCHANGE
    } else {
        0
    };
    let mut status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: controls,
        dwWin32ExitCode: exit_code,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: wait_hint_ms,
    };
    unsafe {
        SetServiceStatus(handle, &mut status);
    }
    let _ = SERVICE_STATUS_HANDLE_RAW.load(Ordering::Relaxed);
}

unsafe extern "system" fn service_ctrl_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            SERVICE_STOP.store(true, Ordering::Release);
            report_scm_status(SERVICE_STOP_PENDING, 0, 3000);
            0
        }
        SERVICE_CONTROL_SESSIONCHANGE => {
            SERVICE_SESSION_CHANGED.store(true, Ordering::Release);
            0
        }
        SERVICE_CONTROL_INTERROGATE => 0,
        _ => 0,
    }
}

struct SessionChild {
    session_id: u32,
    process: HANDLE,
}
impl SessionChild {
    fn is_running(&self) -> bool {
        let mut code: u32 = 0;
        unsafe { GetExitCodeProcess(self.process, &mut code) != 0 && code == STILL_ACTIVE }
    }
    fn terminate(mut self) {
        if !self.process.is_null() {
            unsafe {
                TerminateProcess(self.process, 0);
                WaitForSingleObject(self.process, 1500);
                CloseHandle(self.process);
            }
            self.process = null_mut();
        }
    }
}
impl Drop for SessionChild {
    fn drop(&mut self) {
        if !self.process.is_null() {
            unsafe {
                TerminateProcess(self.process, 0);
                CloseHandle(self.process);
            }
            self.process = null_mut();
        }
    }
}

fn spawn_session_worker(session_id: u32) -> Result<SessionChild> {
    let exe = std::env::current_exe().context("resolve service exe path")?;
    let exe_w = wide(&exe.to_string_lossy());
    let mut cmd_w = wide(&format!(
        "\"{}\" service-worker --session {session_id}",
        exe.to_string_lossy()
    ));
    let mut desktop_w = wide(r"winsta0\default");

    let mut sys_token: HANDLE = null_mut();
    ensure!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut sys_token) } != 0
            && !sys_token.is_null(),
        "OpenProcessToken failed (Win32 {})",
        unsafe { GetLastError() }
    );
    let mut dup_token: HANDLE = null_mut();
    let dup_ok = unsafe {
        DuplicateTokenEx(
            sys_token,
            TOKEN_ALL_ACCESS,
            null(),
            SecurityIdentification,
            TokenPrimary,
            &mut dup_token,
        )
    };
    unsafe { CloseHandle(sys_token) };
    ensure!(
        dup_ok != 0 && !dup_token.is_null(),
        "DuplicateTokenEx failed (Win32 {})",
        unsafe { GetLastError() }
    );

    let set_ok = unsafe {
        SetTokenInformation(
            dup_token,
            TokenSessionId,
            (&session_id as *const u32).cast(),
            size_of::<u32>() as u32,
        )
    };
    if set_ok == 0 {
        let err = unsafe { GetLastError() };
        unsafe { CloseHandle(dup_token) };
        bail!("SetTokenInformation(TokenSessionId) failed (Win32 {err})");
    }

    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop_w.as_mut_ptr();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    let ok = unsafe {
        CreateProcessAsUserW(
            dup_token,
            exe_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_NO_WINDOW | HIGH_PRIORITY_CLASS,
            null(),
            null(),
            &si,
            &mut pi,
        )
    };
    unsafe { CloseHandle(dup_token) };
    ensure!(
        ok != 0 && !pi.hProcess.is_null(),
        "CreateProcessAsUserW failed (Win32 {})",
        unsafe { GetLastError() }
    );
    if !pi.hThread.is_null() {
        unsafe { CloseHandle(pi.hThread) };
    }
    Ok(SessionChild {
        session_id,
        process: pi.hProcess,
    })
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut *mut u16) {
    let name_w = wide(SERVICE_NAME);
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(name_w.as_ptr(), Some(service_ctrl_handler), null_mut())
    };
    if handle.is_null() {
        return;
    }
    SERVICE_STATUS_PTR.store(handle as usize, Ordering::Release);
    SERVICE_STOP.store(false, Ordering::Release);
    report_scm_status(SERVICE_START_PENDING, 0, 2000);
    report_scm_status(SERVICE_RUNNING, 0, 0);

    let mut child: Option<SessionChild> = None;
    while !SERVICE_STOP.load(Ordering::Acquire) {
        let _ = SERVICE_SESSION_CHANGED.swap(false, Ordering::AcqRel);
        let active_sid = unsafe { WTSGetActiveConsoleSessionId() };
        if active_sid != 0xFFFF_FFFF {
            let need_spawn = match &child {
                None => true,
                Some(c) => c.session_id != active_sid || !c.is_running(),
            };
            if need_spawn {
                if let Some(old) = child.take() {
                    old.terminate();
                }
                if let Ok(spawned) = spawn_session_worker(active_sid) {
                    child = Some(spawned);
                }
            }
        }
        for _ in 0..5 {
            if SERVICE_STOP.load(Ordering::Acquire)
                || SERVICE_SESSION_CHANGED.load(Ordering::Acquire)
            {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    if let Some(c) = child.take() {
        c.terminate();
    }
    report_scm_status(SERVICE_STOPPED, 0, 0);
}

/// Entry point invoked by Windows SCM when starting `CrossflowInputService`.
pub fn run_windows_service() -> Result<()> {
    let mut name_w = wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name_w.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: null_mut(),
            lpServiceProc: None,
        },
    ];
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    ensure!(
        ok != 0,
        "StartServiceCtrlDispatcherW failed (Win32 {})",
        unsafe { GetLastError() }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_pipe_name_is_scoped_per_interactive_session() {
        assert_eq!(
            pipe_name_for_session(1),
            r"\\.\pipe\CrossflowInputService-v1-s1"
        );
        assert_ne!(pipe_name_for_session(1), pipe_name_for_session(2));
    }

    #[test]
    fn installed_binary_path_targets_program_data_crossflow() {
        let p = installed_binary_path();
        assert!(p.ends_with(Path::new("Crossflow").join("crossflow.exe")));
    }

    #[test]
    fn query_status_is_non_panicking_for_standard_users() {
        let st = query_status();
        if !st.installed {
            assert!(!st.running);
        }
    }

    #[test]
    fn service_pipe_reconnects_cleanly_after_shutdown_without_busy_deadlock() {
        use crate::input::Button;
        use crate::session::{MouseSink, NativeDisplay};

        struct DummySink;
        impl MouseSink for DummySink {
            fn displays(&self) -> Result<Vec<NativeDisplay>> {
                Ok(vec![NativeDisplay {
                    id: 1,
                    x: 0.0,
                    y: 0.0,
                    width: 1920.0,
                    height: 1080.0,
                }])
            }
            fn reset(&mut self) -> Result<()> {
                Ok(())
            }
            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                Ok((x, y))
            }
            fn position(&mut self, x: f64, y: f64, _display_id: u32) -> Result<(f64, f64)> {
                Ok((x, y))
            }
            fn button(&mut self, _button: Button, _down: bool) -> Result<()> {
                Ok(())
            }
            fn key(&mut self, _usage: u16, _down: bool) -> Result<()> {
                Ok(())
            }
            fn scroll(&mut self, _horizontal: i32, _vertical: i32) -> Result<()> {
                Ok(())
            }
            fn release_owned(&mut self) -> Result<()> {
                Ok(())
            }
            fn emergency_pressed(&self) -> bool {
                false
            }
        }

        let pipe_name = format!(
            r"\\.\pipe\CrossflowTestPipe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server_pipe = pipe_name.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();

        let server = thread::spawn(move || {
            let pipe_w = wide(&server_pipe);
            for _ in 0..3 {
                let handle = unsafe {
                    CreateNamedPipeW(
                        pipe_w.as_ptr(),
                        PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                        1,
                        4096,
                        4096,
                        0,
                        null(),
                    )
                };
                assert!(
                    handle != INVALID_HANDLE_VALUE && !handle.is_null(),
                    "CreateNamedPipeW failed with Win32 {}",
                    unsafe { GetLastError() }
                );
                let connected = wait_pipe_client(handle, &server_stop).unwrap();
                assert!(connected);
                serve_one_pipe_client(handle, || Ok(Box::new(DummySink)));
            }
        });

        // Give server thread a brief moment to create the first pipe instance.
        thread::sleep(Duration::from_millis(20));

        // Connect -> reset -> key -> shutdown -> reconnect 3 consecutive times on a
        // nMaxInstances=1 pipe. Before the PipeCloser fix, iteration 2 deadlocked forever
        // with ERROR_PIPE_BUSY (231).
        for round in 0..3 {
            let (reader, writer, closer) = connect_named_pipe(&pipe_name)
                .unwrap_or_else(|e| panic!("round {round} connect failed: {e:#}"));
            let mut proxy = crate::injection_worker::WorkerProxy::from_test_pipe(
                reader, writer, closer,
            )
            .unwrap_or_else(|e| panic!("round {round} ready failed: {e:#}"));
            proxy.reset().unwrap();
            proxy.position(100.0, 200.0, 1).unwrap();
            proxy.key(0x04, true).unwrap();
            proxy.key(0x04, false).unwrap();
            proxy.shutdown().unwrap();
            drop(proxy);
            thread::sleep(Duration::from_millis(15));
        }

        stop.store(true, Ordering::Release);
        server.join().unwrap();
    }
}
