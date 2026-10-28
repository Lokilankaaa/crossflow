//! Private bounded binary IPC for a dedicated macOS Keychain process.
//! Only the child changes its process-wide Keychain interaction policy. No shell,
//! secret argument, network transfer or OS-wide setting is involved. A killed
//! store may leave an orphan item; failure is never reported as durable success.
#[cfg(target_os = "macos")]
use crate::keychain::KeychainReference;
#[cfg(any(target_os = "macos", test))]
use crate::keychain::MAX_SECRET_BYTES;
use anyhow::Result;
#[cfg(any(target_os = "macos", test))]
use anyhow::{Context, ensure};
#[cfg(any(target_os = "macos", test))]
use std::io::{Read, Write};
#[cfg(any(target_os = "macos", test))]
use zeroize::Zeroizing;

#[cfg(any(target_os = "macos", test))]
const STORE: u8 = 1;
#[cfg(any(target_os = "macos", test))]
const LOAD: u8 = 2;
#[cfg(any(target_os = "macos", test))]
const REMOVE: u8 = 3;
#[cfg(target_os = "macos")]
const SUCCESS: u8 = 1;
#[cfg(any(target_os = "macos", test))]
const MAX_BODY: usize = MAX_SECRET_BYTES;

#[cfg(any(target_os = "macos", test))]
fn read_frame(reader: &mut impl Read) -> Result<(u8, Zeroizing<Vec<u8>>)> {
    let mut header = [0; 5];
    reader
        .read_exact(&mut header)
        .context("Missing Keychain IPC header")?;
    let len = u32::from_be_bytes(header[1..].try_into()?) as usize;
    ensure!(len <= MAX_BODY, "Oversized Keychain IPC body");
    let mut body = Zeroizing::new(vec![0; len]);
    reader
        .read_exact(&mut body)
        .context("Truncated Keychain IPC body")?;
    let mut extra = [0];
    ensure!(reader.read(&mut extra)? == 0, "Trailing Keychain IPC data");
    Ok((header[0], body))
}

#[cfg(any(target_os = "macos", test))]
fn write_frame(writer: &mut impl Write, tag: u8, body: &[u8]) -> Result<()> {
    ensure!(body.len() <= MAX_BODY, "Oversized Keychain IPC body");
    writer.write_all(&[tag])?;
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(body)?;
    writer.flush()?;
    Ok(())
}

/// Internal executable entry point. Must only be called by crossflow-keychain.
/// It changes this dedicated process's interaction policy and handles one frame.
#[cfg(target_os = "macos")]
pub fn run_worker() -> Result<()> {
    let (op, body) = read_frame(&mut std::io::stdin().lock())?;
    ensure!(
        matches!(op, STORE | LOAD | REMOVE),
        "Unknown Keychain operation"
    );
    crate::keychain::worker_initialize()?;
    let output = match op {
        STORE => Zeroizing::new(crate::keychain::worker_store(&body)?.to_bytes()),
        LOAD => crate::keychain::worker_load(&KeychainReference::from_bytes(&body)?)?,
        REMOVE => {
            crate::keychain::worker_remove(&KeychainReference::from_bytes(&body)?)?;
            Zeroizing::new(Vec::new())
        }
        _ => unreachable!(),
    };
    write_frame(&mut std::io::stdout().lock(), SUCCESS, &output)
}

#[cfg(not(target_os = "macos"))]
pub fn run_worker() -> Result<()> {
    anyhow::bail!("Native Keychain worker requires macOS")
}

#[cfg(target_os = "macos")]
pub(crate) fn store(secret: &[u8]) -> Result<KeychainReference> {
    KeychainReference::from_bytes(&request(STORE, secret)?)
}
#[cfg(target_os = "macos")]
pub(crate) fn load(reference: &KeychainReference) -> Result<Zeroizing<Vec<u8>>> {
    let bytes = request(LOAD, &reference.to_bytes())?;
    ensure!(!bytes.is_empty(), "Empty Keychain secret");
    Ok(bytes)
}
#[cfg(target_os = "macos")]
pub(crate) fn remove(reference: &KeychainReference) -> Result<()> {
    ensure!(
        request(REMOVE, &reference.to_bytes())?.is_empty(),
        "Unexpected Keychain removal response"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn request(op: u8, body: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    use std::{
        path::PathBuf,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    ensure!(body.len() <= MAX_BODY, "Oversized Keychain request");
    // Explicit absolute override is for tests/native packaging diagnostics only.
    // Normal packaged callers find the helper beside their own executable.
    let helper = match std::env::var_os("CROSSFLOW_KEYCHAIN_HELPER") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("Missing executable directory")?
            .join("crossflow-keychain"),
    };
    ensure!(
        helper.is_absolute(),
        "Keychain helper path must be absolute"
    );
    let metadata = std::fs::symlink_metadata(&helper).context("Keychain helper is missing")?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Keychain helper must be a regular executable"
    );
    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("Cannot start Keychain helper")?;
    let mut input = child.stdin.take().context("Missing Keychain input pipe")?;
    let mut output = child
        .stdout
        .take()
        .context("Missing Keychain output pipe")?;
    let owned = Zeroizing::new(body.to_vec());
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let io = thread::Builder::new()
        .name("crossflow-keychain-io".into())
        .spawn(move || {
            let result = (|| {
                write_frame(&mut input, op, &owned)?;
                drop(input); // EOF is required; the helper accepts exactly one request.
                read_frame(&mut output)
            })();
            let _ = sender.send(result);
        });
    let io = match io {
        Ok(io) => io,
        Err(error) => {
            terminate_after_failure(child)?;
            return Err(error).context("Cannot supervise Keychain helper");
        }
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => break Err(anyhow::anyhow!("Keychain helper timed out")),
            Err(error) => break Err(error.into()),
        }
    };
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            terminate_after_failure(child)?;
            return Err(error);
        }
    };
    // The dedicated helper spawns no children. Reaping it closes both pipe ends.
    // A bounded receive avoids blocking the caller if a replaced helper violates
    // that contract; it is an installed executable, not untrusted network input.
    let response = receiver.recv_timeout(Duration::from_secs(1));
    if response.is_ok() {
        let _ = io.join();
    }
    ensure!(
        status.success(),
        "Keychain helper failed; native keychain may be locked or access denied"
    );
    let (tag, bytes) = response.context("Keychain helper did not close its pipes")??;
    ensure!(tag == SUCCESS, "Invalid Keychain helper result");
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn terminate_after_failure(mut child: std::process::Child) -> Result<()> {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Never put an unbounded wait on the caller. Keep ownership in a reaper if
    // the OS cannot confirm exit; this is an error, not successful cleanup.
    let _ = thread::Builder::new()
        .name("crossflow-keychain-reaper".into())
        .spawn(move || {
            let _ = child.kill();
            let _ = child.wait();
        });
    anyhow::bail!("Keychain helper stop could not be confirmed; operation outcome is unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn ipc_rejects_oversize_truncation_and_trailing_bytes() {
        let mut oversized = vec![STORE];
        oversized.extend_from_slice(&((MAX_BODY + 1) as u32).to_be_bytes());
        assert!(read_frame(&mut Cursor::new(oversized)).is_err());
        assert!(read_frame(&mut Cursor::new([LOAD, 0, 0, 0, 1])).is_err());
        assert!(read_frame(&mut Cursor::new([LOAD, 0, 0, 0, 0, 1])).is_err());
        let mut wire = Vec::new();
        write_frame(&mut wire, REMOVE, &[]).unwrap();
        let (tag, value) = read_frame(&mut Cursor::new(wire)).unwrap();
        assert_eq!(tag, REMOVE);
        assert!(value.is_empty());
    }
}
