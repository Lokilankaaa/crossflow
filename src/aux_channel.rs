//! Independent encrypted auxiliary channel for clipboard sync and streaming file transfer.
//!
//! Runs on `main_port + 1` (default 24801) using the exact same Noise_NNpsk0
//! authentication and peer identity verification as the main input session,
//! completely decoupled from the 1000Hz mouse/keyboard hot path.

use crate::{
    clipboard::{self, ClipboardItem, MAX_IMAGE_BYTES, MAX_TEXT_BYTES},
    metrics::Log,
    security::{self, SecureChannel},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CHUNK_SIZE: usize = 32 * 1024;
const MAX_FRAME_CIPHERTEXT: usize = 48 * 1024;
const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(160);
const INPUT_STATE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INPUT_STATE_ECHO_COOLDOWN: Duration = Duration::from_millis(450);
const PING_INTERVAL: Duration = Duration::from_secs(3);
const MAX_BATCH_FILES: usize = 1000;
const MAX_BATCH_BYTES: u64 = 4 * 1024 * 1024 * 1024; // 4 GiB

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TransferStatus {
    pub direction: String, // "sending" | "receiving"
    pub state: String,     // "active" | "completed" | "cancelled" | "failed"
    pub name: String,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: usize,
    pub files_total: usize,
    pub saved_dir: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AuxHeader {
    Ping,
    Clipboard {
        format: String,
        hash: String,
        len: usize,
    },
    FileBatchStart {
        batch_id: u64,
        total_files: usize,
        total_bytes: u64,
        display_name: String,
    },
    FileEntryStart {
        batch_id: u64,
        rel_path: String,
        size: u64,
    },
    FileEntryEnd {
        batch_id: u64,
        blake2s: String,
    },
    FileBatchEnd {
        batch_id: u64,
    },
    FileCancel {
        batch_id: u64,
    },
    InputState {
        caps_lock: bool,
        ime_chinese: Option<bool>,
    },
}

struct FileEntrySpec {
    source_path: PathBuf,
    rel_path: String,
    size: u64,
}

fn new_blake2s() -> Box<dyn snow::types::Hash> {
    let mut h = DefaultResolver
        .resolve_hash(&snow::params::HashChoice::Blake2s)
        .expect("BLAKE2s available");
    h.reset();
    h
}

fn finish_blake2s_hex(hasher: &mut dyn snow::types::Hash) -> String {
    let mut out = [0u8; 32];
    hasher.result(&mut out);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in out {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn default_receive_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = PathBuf::from(std::env::var_os("HOME").context("HOME unavailable")?);
    #[cfg(not(target_os = "macos"))]
    let base = PathBuf::from(std::env::var_os("USERPROFILE").context("USERPROFILE unavailable")?);
    let dir = base.join("Downloads").join("Crossflow");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn sanitize_rel_path(rel_path: &str) -> Result<PathBuf> {
    ensure!(
        !rel_path.is_empty() && rel_path.len() <= 1024 && !rel_path.contains('\0'),
        "invalid relative path"
    );
    let mut clean = PathBuf::new();
    let normalized = rel_path.replace('\\', "/");
    for segment in normalized.split('/') {
        ensure!(
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')),
            "unsafe path segment rejected"
        );
        clean.push(segment);
    }
    ensure!(
        clean.components().all(|c| matches!(c, Component::Normal(_))) && !clean.as_os_str().is_empty(),
        "non-normal path component rejected"
    );
    Ok(clean)
}

fn unique_top_level_name(dest_root: &Path, original_name: &str) -> String {
    let candidate = dest_root.join(original_name);
    if !candidate.exists() {
        return original_name.to_string();
    }
    let path = Path::new(original_name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(original_name);
    let ext = path.extension().and_then(|e| e.to_str());
    for n in 1..=9999 {
        let next = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({n}).{e}"),
            _ => format!("{stem} ({n})"),
        };
        if !dest_root.join(&next).exists() {
            return next;
        }
    }
    format!("{stem}-{}", Instant::now().elapsed().as_nanos())
}

fn collect_transfer_entries(roots: &[PathBuf]) -> Result<(Vec<FileEntrySpec>, u64, String)> {
    let mut entries = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut top_names: Vec<String> = Vec::new();
    let mut used_roots: HashSet<String> = HashSet::new();

    for root in roots {
        let Ok(meta) = fs::symlink_metadata(root) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let base_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.is_empty())
            .unwrap_or("file")
            .to_string();
        let mut unique_root = base_name.clone();
        let mut suffix = 1usize;
        while !used_roots.insert(unique_root.clone()) {
            suffix += 1;
            unique_root = format!("{base_name}_{suffix}");
        }
        top_names.push(base_name);

        if meta.is_file() {
            let size = meta.len();
            total_bytes = total_bytes.checked_add(size).context("batch size overflow")?;
            ensure!(total_bytes <= MAX_BATCH_BYTES, "文件总大小超过 4 GB 上限");
            ensure!(entries.len() < MAX_BATCH_FILES, "单次传输文件数量超过 1000 个上限");
            entries.push(FileEntrySpec {
                source_path: root.clone(),
                rel_path: unique_root,
                size,
            });
        } else if meta.is_dir() {
            let mut stack = vec![(root.clone(), unique_root)];
            while let Some((dir, prefix)) = stack.pop() {
                let Ok(read_dir) = fs::read_dir(&dir) else {
                    continue;
                };
                for item in read_dir.flatten() {
                    let path = item.path();
                    let Ok(m) = fs::symlink_metadata(&path) else {
                        continue;
                    };
                    if m.file_type().is_symlink() {
                        continue;
                    }
                    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                        continue;
                    };
                    let rel = format!("{prefix}/{name}");
                    if m.is_file() {
                        let size = m.len();
                        total_bytes = total_bytes.checked_add(size).context("batch size overflow")?;
                        ensure!(total_bytes <= MAX_BATCH_BYTES, "文件总大小超过 4 GB 上限");
                        ensure!(entries.len() < MAX_BATCH_FILES, "单次传输文件数量超过 1000 个上限");
                        entries.push(FileEntrySpec {
                            source_path: path,
                            rel_path: rel,
                            size,
                        });
                    } else if m.is_dir() {
                        stack.push((path, rel));
                    }
                }
            }
        }
    }

    ensure!(!entries.is_empty(), "没有可传输的有效文件");
    let display_name = if top_names.len() == 1 {
        top_names[0].clone()
    } else {
        format!("{} 等 {} 个项目", top_names[0], top_names.len())
    };
    Ok((entries, total_bytes, display_name))
}

fn write_encrypted(stream: &mut TcpStream, ch: &mut SecureChannel, plaintext: &[u8]) -> Result<()> {
    let ct = ch.encrypt(plaintext)?;
    ensure!(ct.len() <= MAX_FRAME_CIPHERTEXT, "aux record exceeds limit");
    let len_bytes = (ct.len() as u32).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(&ct)?;
    Ok(())
}

fn read_encrypted_blocking(stream: &mut TcpStream, ch: &mut SecureChannel) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr)?;
    let len = u32::from_be_bytes(hdr) as usize;
    ensure!(len >= 16 && len <= MAX_FRAME_CIPHERTEXT, "invalid aux record length");
    let mut ct = vec![0u8; len];
    stream.read_exact(&mut ct)?;
    ch.decrypt(&ct)
}

fn read_encrypted_nonfatal(
    stream: &mut TcpStream,
    ch: &mut SecureChannel,
) -> Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(header) as usize;
    ensure!(len >= 16 && len <= MAX_FRAME_CIPHERTEXT, "invalid aux record length");
    let mut ct = vec![0u8; len];
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let res = stream.read_exact(&mut ct);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    res?;
    Ok(Some(ch.decrypt(&ct)?))
}

fn send_clipboard_item(
    stream: &mut TcpStream,
    ch: &mut SecureChannel,
    item: &ClipboardItem,
    hash: &str,
) -> Result<()> {
    let data = item.bytes();
    let header = serde_json::to_vec(&AuxHeader::Clipboard {
        format: item.kind_str().into(),
        hash: hash.to_string(),
        len: data.len(),
    })?;
    write_encrypted(stream, ch, &header)?;
    for chunk in data.chunks(CHUNK_SIZE) {
        write_encrypted(stream, ch, chunk)?;
    }
    stream.flush()?;
    Ok(())
}

fn recv_clipboard_item(
    stream: &mut TcpStream,
    ch: &mut SecureChannel,
    format: &str,
    expected_hash: &str,
    total_len: usize,
) -> Result<ClipboardItem> {
    let limit = match format {
        "text" => MAX_TEXT_BYTES,
        "png" => MAX_IMAGE_BYTES,
        _ => bail!("unsupported clipboard format"),
    };
    ensure!(total_len > 0 && total_len <= limit, "clipboard payload exceeds limit");
    let mut buf = Vec::with_capacity(total_len);
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let read_res = (|| -> Result<()> {
        while buf.len() < total_len {
            let plain = read_encrypted_blocking(stream, ch)?;
            ensure!(buf.len() + plain.len() <= total_len, "clipboard chunk overflow");
            buf.extend_from_slice(&plain);
        }
        Ok(())
    })();
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    read_res?;

    let item = match format {
        "text" => ClipboardItem::Text(String::from_utf8(buf).context("invalid UTF-8 clipboard")?),
        "png" => ClipboardItem::Png(buf),
        _ => bail!("unsupported clipboard format"),
    };
    ensure!(item.hash_hex() == expected_hash, "clipboard checksum mismatch");
    Ok(item)
}

fn publish_transfer(slot: &Mutex<Option<(TransferStatus, Instant)>>, status: TransferStatus) {
    if let Ok(mut guard) = slot.lock() {
        *guard = Some((status, Instant::now()));
    }
}

#[allow(clippy::too_many_arguments)]
fn send_file_batch(
    stream: &mut TcpStream,
    ch: &mut SecureChannel,
    batch_id: u64,
    roots: &[PathBuf],
    cancel: &AtomicBool,
    stop: &AtomicBool,
    status_slot: &Mutex<Option<(TransferStatus, Instant)>>,
) -> Result<()> {
    let (entries, total_bytes, display_name) = match collect_transfer_entries(roots) {
        Ok(v) => v,
        Err(e) => {
            publish_transfer(
                status_slot,
                TransferStatus {
                    direction: "sending".into(),
                    state: "failed".into(),
                    name: "文件传输".into(),
                    bytes_done: 0,
                    bytes_total: 0,
                    files_done: 0,
                    files_total: 0,
                    saved_dir: None,
                    error: Some(format!("{e:#}")),
                },
            );
            return Ok(());
        }
    };
    cancel.store(false, Ordering::Release);
    let total_files = entries.len();
    publish_transfer(
        status_slot,
        TransferStatus {
            direction: "sending".into(),
            state: "active".into(),
            name: display_name.clone(),
            bytes_done: 0,
            bytes_total: total_bytes,
            files_done: 0,
            files_total: total_files,
            saved_dir: None,
            error: None,
        },
    );

    let start_hdr = serde_json::to_vec(&AuxHeader::FileBatchStart {
        batch_id,
        total_files,
        total_bytes,
        display_name: display_name.clone(),
    })?;
    write_encrypted(stream, ch, &start_hdr)?;
    stream.flush()?;

    let mut bytes_done: u64 = 0;
    let mut files_done: usize = 0;
    let mut last_ui = Instant::now();
    let mut buf = vec![0u8; CHUNK_SIZE];

    for entry in &entries {
        if cancel.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
            let hdr = serde_json::to_vec(&AuxHeader::FileCancel { batch_id })?;
            let _ = write_encrypted(stream, ch, &hdr);
            let _ = stream.flush();
            publish_transfer(
                status_slot,
                TransferStatus {
                    direction: "sending".into(),
                    state: "cancelled".into(),
                    name: display_name,
                    bytes_done,
                    bytes_total: total_bytes,
                    files_done,
                    files_total: total_files,
                    saved_dir: None,
                    error: None,
                },
            );
            return Ok(());
        }

        let mut file = File::open(&entry.source_path)?;
        let entry_hdr = serde_json::to_vec(&AuxHeader::FileEntryStart {
            batch_id,
            rel_path: entry.rel_path.clone(),
            size: entry.size,
        })?;
        write_encrypted(stream, ch, &entry_hdr)?;

        let mut hasher = new_blake2s();
        let mut remaining = entry.size;
        while remaining > 0 {
            if cancel.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                let hdr = serde_json::to_vec(&AuxHeader::FileCancel { batch_id })?;
                let _ = write_encrypted(stream, ch, &hdr);
                let _ = stream.flush();
                publish_transfer(
                    status_slot,
                    TransferStatus {
                        direction: "sending".into(),
                        state: "cancelled".into(),
                        name: display_name,
                        bytes_done,
                        bytes_total: total_bytes,
                        files_done,
                        files_total: total_files,
                        saved_dir: None,
                        error: None,
                    },
                );
                return Ok(());
            }
            let to_read = (remaining as usize).min(CHUNK_SIZE);
            let n = file.read(&mut buf[..to_read])?;
            ensure!(n > 0, "unexpected EOF while reading file");
            hasher.input(&buf[..n]);
            write_encrypted(stream, ch, &buf[..n])?;
            remaining -= n as u64;
            bytes_done += n as u64;
            if last_ui.elapsed() >= Duration::from_millis(90) {
                last_ui = Instant::now();
                publish_transfer(
                    status_slot,
                    TransferStatus {
                        direction: "sending".into(),
                        state: "active".into(),
                        name: display_name.clone(),
                        bytes_done,
                        bytes_total: total_bytes,
                        files_done,
                        files_total: total_files,
                        saved_dir: None,
                        error: None,
                    },
                );
            }
        }
        let digest = finish_blake2s_hex(hasher.as_mut());
        let end_hdr = serde_json::to_vec(&AuxHeader::FileEntryEnd {
            batch_id,
            blake2s: digest,
        })?;
        write_encrypted(stream, ch, &end_hdr)?;
        files_done += 1;
    }

    let batch_end = serde_json::to_vec(&AuxHeader::FileBatchEnd { batch_id })?;
    write_encrypted(stream, ch, &batch_end)?;
    stream.flush()?;

    publish_transfer(
        status_slot,
        TransferStatus {
            direction: "sending".into(),
            state: "completed".into(),
            name: display_name,
            bytes_done: total_bytes,
            bytes_total: total_bytes,
            files_done: total_files,
            files_total: total_files,
            saved_dir: None,
            error: None,
        },
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recv_file_batch(
    stream: &mut TcpStream,
    ch: &mut SecureChannel,
    dest_root: &Path,
    batch_id: u64,
    total_files: usize,
    total_bytes: u64,
    display_name: String,
    cancel: &AtomicBool,
    stop: &AtomicBool,
    status_slot: &Mutex<Option<(TransferStatus, Instant)>>,
) -> Result<Option<Vec<PathBuf>>> {
    ensure!(
        total_files > 0 && total_files <= MAX_BATCH_FILES && total_bytes <= MAX_BATCH_BYTES,
        "incoming file batch exceeds bounds"
    );
    fs::create_dir_all(dest_root)?;
    cancel.store(false, Ordering::Release);

    publish_transfer(
        status_slot,
        TransferStatus {
            direction: "receiving".into(),
            state: "active".into(),
            name: display_name.clone(),
            bytes_done: 0,
            bytes_total: total_bytes,
            files_done: 0,
            files_total: total_files,
            saved_dir: Some(dest_root.to_string_lossy().into_owned()),
            error: None,
        },
    );

    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut root_remap: HashMap<String, String> = HashMap::new();
    let mut top_paths: Vec<PathBuf> = Vec::new();
    let mut top_seen: HashSet<PathBuf> = HashSet::new();
    let mut bytes_done: u64 = 0;
    let mut files_done: usize = 0;
    let mut last_ui = Instant::now();

    let outcome = (|| -> Result<Option<Vec<PathBuf>>> {
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(None);
            }
            let plain = read_encrypted_blocking(stream, ch)?;
            match serde_json::from_slice::<AuxHeader>(&plain)? {
                AuxHeader::FileEntryStart {
                    batch_id: id,
                    rel_path,
                    size,
                } => {
                    ensure!(id == batch_id, "mismatched batch id");
                    let clean_rel = sanitize_rel_path(&rel_path)?;
                    let mut comps = clean_rel.components();
                    let first = comps
                        .next()
                        .context("missing root component")?
                        .as_os_str()
                        .to_string_lossy()
                        .into_owned();
                    let mapped_first = root_remap
                        .entry(first.clone())
                        .or_insert_with(|| unique_top_level_name(dest_root, &first))
                        .clone();
                    let mut final_path = dest_root.join(&mapped_first);
                    let top_path = final_path.clone();
                    for c in comps {
                        final_path.push(c.as_os_str());
                    }
                    if let Some(parent) = final_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let part_path = final_path.with_extension(format!(
                        "{}crossflow.part",
                        final_path
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| format!("{e}."))
                            .unwrap_or_default()
                    ));
                    let mut file = File::create(&part_path)?;
                    let mut hasher = new_blake2s();
                    let mut written: u64 = 0;
                    while written < size {
                        let chunk = read_encrypted_blocking(stream, ch)?;
                        // Sender may send a FileCancel mid-file instead of raw bytes
                        if chunk.starts_with(b"{\"kind\":\"file_cancel\"")
                            && matches!(
                                serde_json::from_slice::<AuxHeader>(&chunk),
                                Ok(AuxHeader::FileCancel { .. })
                            )
                        {
                            drop(file);
                            let _ = fs::remove_file(&part_path);
                            publish_transfer(
                                status_slot,
                                TransferStatus {
                                    direction: "receiving".into(),
                                    state: "cancelled".into(),
                                    name: display_name,
                                    bytes_done,
                                    bytes_total: total_bytes,
                                    files_done,
                                    files_total: total_files,
                                    saved_dir: Some(dest_root.to_string_lossy().into_owned()),
                                    error: None,
                                },
                            );
                            return Ok(None);
                        }
                        if written + chunk.len() as u64 > size {
                            drop(file);
                            let _ = fs::remove_file(&part_path);
                            bail!("received file chunk exceeded declared size");
                        }
                        file.write_all(&chunk)?;
                        hasher.input(&chunk);
                        written += chunk.len() as u64;
                        bytes_done += chunk.len() as u64;
                        if last_ui.elapsed() >= Duration::from_millis(90) {
                            last_ui = Instant::now();
                            publish_transfer(
                                status_slot,
                                TransferStatus {
                                    direction: "receiving".into(),
                                    state: "active".into(),
                                    name: display_name.clone(),
                                    bytes_done,
                                    bytes_total: total_bytes,
                                    files_done,
                                    files_total: total_files,
                                    saved_dir: Some(dest_root.to_string_lossy().into_owned()),
                                    error: None,
                                },
                            );
                        }
                    }
                    file.flush()?;
                    drop(file);
                    let end_plain = read_encrypted_blocking(stream, ch)?;
                    let AuxHeader::FileEntryEnd {
                        batch_id: end_id,
                        blake2s,
                    } = serde_json::from_slice::<AuxHeader>(&end_plain)?
                    else {
                        let _ = fs::remove_file(&part_path);
                        bail!("expected file_entry_end");
                    };
                    ensure!(end_id == batch_id, "mismatched batch id on entry end");
                    let actual_hash = finish_blake2s_hex(hasher.as_mut());
                    if actual_hash != blake2s {
                        let _ = fs::remove_file(&part_path);
                        bail!("file checksum mismatch");
                    }
                    fs::rename(&part_path, &final_path)?;
                    if top_seen.insert(top_path.clone()) {
                        top_paths.push(top_path);
                    }
                    files_done += 1;
                }
                AuxHeader::FileBatchEnd { batch_id: id } => {
                    ensure!(id == batch_id, "mismatched batch id on batch end");
                    publish_transfer(
                        status_slot,
                        TransferStatus {
                            direction: "receiving".into(),
                            state: "completed".into(),
                            name: display_name,
                            bytes_done: total_bytes,
                            bytes_total: total_bytes,
                            files_done,
                            files_total: total_files,
                            saved_dir: Some(dest_root.to_string_lossy().into_owned()),
                            error: None,
                        },
                    );
                    return Ok(Some(top_paths));
                }
                AuxHeader::FileCancel { .. } => {
                    publish_transfer(
                        status_slot,
                        TransferStatus {
                            direction: "receiving".into(),
                            state: "cancelled".into(),
                            name: display_name,
                            bytes_done,
                            bytes_total: total_bytes,
                            files_done,
                            files_total: total_files,
                            saved_dir: Some(dest_root.to_string_lossy().into_owned()),
                            error: None,
                        },
                    );
                    return Ok(None);
                }
                _ => bail!("unexpected header inside file batch"),
            }
        }
    })();
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    if let Err(ref e) = outcome {
        publish_transfer(
            status_slot,
            TransferStatus {
                direction: "receiving".into(),
                state: "failed".into(),
                name: "接收文件".into(),
                bytes_done,
                bytes_total: total_bytes,
                files_done,
                files_total: total_files,
                saved_dir: Some(dest_root.to_string_lossy().into_owned()),
                error: Some(format!("{e:#}")),
            },
        );
    }
    outcome
}

enum AuxOutbound {
    Files(Vec<PathBuf>),
    Clipboard { item: ClipboardItem, hash: String },
}

const REMOTE_CLIP_ECHO_WINDOW: Duration = Duration::from_secs(2);
const PENDING_INBOUND_MAX_AGE: Duration = Duration::from_secs(8);
const AUX_RECONNECT_DELAY: Duration = Duration::from_millis(400);

#[derive(Default)]
struct SharedClipEcho {
    last_token: u64,
    pending_empty_token: Option<(u64, u8)>,
    last_remote_write: Option<(String, Instant)>,
}

/// Shared multi-peer auxiliary hub on port `main_port + 1`:
/// - Accepts inbound aux connections from arbitrary mesh peers concurrently and
///   dispatches each Noise-verified `(TcpStream, SecureChannel)` by `remote_id`.
/// - Centralizes local OS clipboard polling into ONE single reader (avoiding Win32
///   `OpenClipboard` contention across N peers) with mesh-wide BLAKE2s echo suppression.
#[derive(Clone)]
pub struct AuxMeshHub {
    pending_inbound: Arc<Mutex<HashMap<String, (TcpStream, SecureChannel, Instant)>>>,
    clip_echo: Arc<Mutex<SharedClipEcho>>,
    pending_local_clips: Arc<Mutex<std::collections::VecDeque<(ClipboardItem, String)>>>,
    has_peers: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    acceptor_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    poller_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl AuxMeshHub {
    pub fn start(
        listener: Option<Arc<TcpListener>>,
        local_device_id: String,
        passphrase: Option<String>,
        clipboard_enabled: Arc<AtomicBool>,
    ) -> Self {
        let pending_inbound: Arc<Mutex<HashMap<String, (TcpStream, SecureChannel, Instant)>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let clip_echo = Arc::new(Mutex::new(SharedClipEcho {
            last_token: clipboard::change_token(),
            pending_empty_token: None,
            last_remote_write: None,
        }));
        let pending_local_clips =
            Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(4)));
        let has_peers = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let acceptor_handle = if let Some(listener) = listener {
            let inbound = pending_inbound.clone();
            let stop_flag = stop.clone();
            thread::Builder::new()
                .name("crossflow-aux-acceptor".into())
                .spawn(move || {
                    while !stop_flag.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _addr)) => {
                                let dev = local_device_id.clone();
                                let pass = passphrase.clone();
                                let inbound_map = inbound.clone();
                                let _ = thread::Builder::new()
                                    .name("crossflow-aux-handshake".into())
                                    .spawn(move || {
                                        let _ = stream.set_nonblocking(false);
                                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                                        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                                        if let Ok((ch, remote_id)) = security::handshake_lan_psk(
                                            &mut stream,
                                            &dev,
                                            pass.as_deref(),
                                            false,
                                        ) && let Ok(mut guard) = inbound_map.lock()
                                        {
                                            guard.retain(|_, (_, _, ts)| {
                                                ts.elapsed() < PENDING_INBOUND_MAX_AGE
                                            });
                                            guard.insert(remote_id, (stream, ch, Instant::now()));
                                        }
                                    });
                            }
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(20));
                            }
                            Err(_) => break,
                        }
                    }
                })
                .ok()
        } else {
            None
        };
        // Poll the local OS clipboard on a dedicated background thread so image
        // decoding / OpenClipboard / NSPasteboard never stalls the 1000Hz input loop.
        let poller_handle = {
            let echo = clip_echo.clone();
            let queue = pending_local_clips.clone();
            let clip_flag = clipboard_enabled;
            let peers_flag = has_peers.clone();
            let stop_flag = stop.clone();
            thread::Builder::new()
                .name("crossflow-mesh-clipboard".into())
                .spawn(move || {
                    let mut last_poll = Instant::now();
                    while !stop_flag.load(Ordering::Acquire) {
                        if last_poll.elapsed() >= CLIP_POLL_INTERVAL {
                            last_poll = Instant::now();
                            if peers_flag.load(Ordering::Acquire)
                                && clip_flag.load(Ordering::Relaxed)
                                && let Some(entry) = poll_local_clipboard_inner(&echo)
                                && let Ok(mut q) = queue.lock()
                            {
                                while q.len() >= 4 {
                                    q.pop_front();
                                }
                                q.push_back(entry);
                            }
                        }
                        thread::sleep(Duration::from_millis(25));
                    }
                })
                .ok()
        };
        Self {
            pending_inbound,
            clip_echo,
            pending_local_clips,
            has_peers,
            stop,
            acceptor_handle: Arc::new(Mutex::new(acceptor_handle)),
            poller_handle: Arc::new(Mutex::new(poller_handle)),
        }
    }

    pub fn set_has_peers(&self, active: bool) {
        self.has_peers.store(active, Ordering::Release);
    }

    /// Clears transient remote-echo suppression whenever a peer connects or reconnects so
    /// re-copying the same text/image immediately syncs to the freshly connected peer.
    pub fn clear_echo_state(&self) {
        if let Ok(mut st) = self.clip_echo.lock() {
            st.last_remote_write = None;
            st.pending_empty_token = None;
        }
    }

    /// Stops the acceptor & clipboard poller and waits for both threads to exit so the
    /// listening port (`24801`) is guaranteed to be released before rebinding on wake.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut guard) = self.acceptor_handle.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
        if let Ok(mut guard) = self.poller_handle.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
        if let Ok(mut guard) = self.pending_inbound.lock() {
            guard.clear();
        }
    }

    fn take_inbound(&self, remote_id: &str) -> Option<(TcpStream, SecureChannel)> {
        let mut guard = self.pending_inbound.lock().ok()?;
        guard.retain(|_, (_, _, ts)| ts.elapsed() < PENDING_INBOUND_MAX_AGE);
        guard.remove(remote_id).map(|(s, ch, _)| (s, ch))
    }

    /// Non-blocking drain of local clipboard items discovered by the background poller.
    pub fn take_local_clipboard(&self) -> Option<(ClipboardItem, String)> {
        self.pending_local_clips.lock().ok()?.pop_front()
    }

    /// Synchronous single-poll helper (kept for direct tests).
    pub fn poll_local_clipboard(&self) -> Option<(ClipboardItem, String)> {
        poll_local_clipboard_inner(&self.clip_echo)
    }

    fn record_remote_clipboard_write(&self, hash: String) {
        if let Ok(mut st) = self.clip_echo.lock() {
            st.last_remote_write = Some((hash, Instant::now()));
            st.last_token = clipboard::change_token();
            st.pending_empty_token = None;
        }
    }
}

fn should_suppress_echo(st: &mut SharedClipEcho, hash: &str) -> bool {
    if let Some((ref remote_hash, recorded_at)) = st.last_remote_write {
        if recorded_at.elapsed() < REMOTE_CLIP_ECHO_WINDOW && remote_hash == hash {
            return true;
        }
        if recorded_at.elapsed() >= REMOTE_CLIP_ECHO_WINDOW {
            st.last_remote_write = None;
        }
    }
    false
}

fn poll_local_clipboard_inner(
    clip_echo: &Mutex<SharedClipEcho>,
) -> Option<(ClipboardItem, String)> {
    let mut st = clip_echo.lock().ok()?;
    let token = clipboard::change_token();
    let should_read = if token != st.last_token {
        st.pending_empty_token = Some((token, 0));
        st.last_token = token;
        true
    } else if let Some((tok, retries)) = st.pending_empty_token {
        if tok == token && retries < 3 {
            st.pending_empty_token = Some((tok, retries + 1));
            true
        } else {
            st.pending_empty_token = None;
            false
        }
    } else {
        false
    };
    if !should_read {
        return None;
    }
    let item = clipboard::read()?;
    st.pending_empty_token = None;
    let hash = item.hash_hex();
    // Only suppress immediate echoes of our own remote clipboard write.
    // Re-copying the same text after a peer restart increments `change_token()` and MUST sync.
    if should_suppress_echo(&mut st, &hash) {
        return None;
    }
    st.last_remote_write = None;
    Some((item, hash))
}

pub struct AuxSession {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    push_state_now: Arc<AtomicBool>,
    send_tx: Sender<AuxOutbound>,
    transfer_slot: Arc<Mutex<Option<(TransferStatus, Instant)>>>,
    worker: Option<JoinHandle<()>>,
}

impl AuxSession {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        aux_listener: Option<Arc<TcpListener>>,
        peer_main_addr: SocketAddr,
        initiator: bool,
        local_device_id: String,
        expected_remote_id: String,
        passphrase: Option<String>,
        clipboard_enabled: Arc<AtomicBool>,
        input_state_sync_enabled: Arc<AtomicBool>,
        local_has_focus: Arc<AtomicBool>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let push_state_now = Arc::new(AtomicBool::new(false));
        let transfer_slot = Arc::new(Mutex::new(None));
        let (send_tx, send_rx) = mpsc::channel();

        let worker_stop = stop.clone();
        let worker_cancel = cancel.clone();
        let worker_push = push_state_now.clone();
        let worker_slot = transfer_slot.clone();
        let worker = thread::Builder::new()
            .name("crossflow-aux-channel".into())
            .spawn(move || {
                let peer_aux_addr = SocketAddr::new(
                    peer_main_addr.ip(),
                    peer_main_addr.port().saturating_add(1),
                );
                while !worker_stop.load(Ordering::Acquire) {
                    let mut was_connected = false;
                    let _ = run_aux(
                        aux_listener.clone(),
                        None,
                        peer_aux_addr,
                        initiator,
                        &local_device_id,
                        &expected_remote_id,
                        passphrase.as_deref(),
                        &clipboard_enabled,
                        &input_state_sync_enabled,
                        &local_has_focus,
                        &worker_push,
                        &send_rx,
                        &worker_cancel,
                        &worker_slot,
                        &worker_stop,
                        None,
                        &mut was_connected,
                    );
                    if !sleep_unless_stopped(&worker_stop, AUX_RECONNECT_DELAY) {
                        break;
                    }
                }
            })
            .ok();
        Self {
            stop,
            cancel,
            push_state_now,
            send_tx,
            transfer_slot,
            worker,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_mesh_peer(
        hub: AuxMeshHub,
        peer_aux_addr: SocketAddr,
        initiator: bool,
        local_device_id: String,
        expected_remote_id: String,
        passphrase: Option<String>,
        clipboard_enabled: Arc<AtomicBool>,
        input_state_sync_enabled: Arc<AtomicBool>,
        local_has_focus: Arc<AtomicBool>,
        log: Option<Log>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let push_state_now = Arc::new(AtomicBool::new(false));
        let transfer_slot = Arc::new(Mutex::new(None));
        let (send_tx, send_rx) = mpsc::channel();

        let worker_stop = stop.clone();
        let worker_cancel = cancel.clone();
        let worker_push = push_state_now.clone();
        let worker_slot = transfer_slot.clone();
        let worker = thread::Builder::new()
            .name("crossflow-mesh-aux".into())
            .spawn(move || {
                // Self-healing loop: if the auxiliary TCP channel drops during sleep/wake or
                // reconnect races while the main 24800 input channel stays up, automatically
                // re-establish 24801 so clipboard, IME, and file sync never silently die.
                let mut last_disc_log: Option<Instant> = None;
                while !worker_stop.load(Ordering::Acquire) {
                    let mut was_connected = false;
                    let res = run_aux(
                        None,
                        Some(hub.clone()),
                        peer_aux_addr,
                        initiator,
                        &local_device_id,
                        &expected_remote_id,
                        passphrase.as_deref(),
                        &clipboard_enabled,
                        &input_state_sync_enabled,
                        &local_has_focus,
                        &worker_push,
                        &send_rx,
                        &worker_cancel,
                        &worker_slot,
                        &worker_stop,
                        log.as_ref(),
                        &mut was_connected,
                    );
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let should_log = was_connected
                        || last_disc_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(30));
                    if should_log && let Some(ref log) = log {
                        last_disc_log = Some(Instant::now());
                        let peer_tag = &expected_remote_id[..expected_remote_id.len().min(8)];
                        let reason = match res {
                            Ok(()) => "closed".to_string(),
                            Err(e) => format!("{e:#}"),
                        };
                        log.write(
                            "aux_disconnected",
                            serde_json::json!({
                                "peer": peer_tag,
                                "initiator": initiator,
                                "target": peer_aux_addr.to_string(),
                                "reason": reason,
                            }),
                        );
                    }
                    if !sleep_unless_stopped(&worker_stop, AUX_RECONNECT_DELAY) {
                        break;
                    }
                }
            })
            .ok();
        Self {
            stop,
            cancel,
            push_state_now,
            send_tx,
            transfer_slot,
            worker,
        }
    }

    pub fn push_input_state_now(&self) {
        self.push_state_now.store(true, Ordering::Release);
    }

    pub fn send_files(&self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            let _ = self.send_tx.send(AuxOutbound::Files(paths));
        }
    }

    pub fn push_clipboard(&self, item: ClipboardItem, hash: String) {
        let _ = self.send_tx.send(AuxOutbound::Clipboard { item, hash });
    }

    pub fn cancel_transfer(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    pub fn current_transfer(&self) -> Option<TransferStatus> {
        let guard = self.transfer_slot.lock().ok()?;
        let (status, updated) = guard.as_ref()?;
        // Auto-expire finished banners after 10 seconds so UI stays clean.
        if status.state != "active" && updated.elapsed() >= Duration::from_secs(10) {
            return None;
        }
        Some(status.clone())
    }
}

impl Drop for AuxSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.cancel.store(true, Ordering::Release);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) -> bool {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn run_aux(
    aux_listener: Option<Arc<TcpListener>>,
    mesh_hub: Option<AuxMeshHub>,
    peer_aux_addr: SocketAddr,
    initiator: bool,
    local_device_id: &str,
    expected_remote_id: &str,
    passphrase: Option<&str>,
    clipboard_enabled: &AtomicBool,
    input_state_sync_enabled: &AtomicBool,
    local_has_focus: &AtomicBool,
    push_state_now: &AtomicBool,
    send_rx: &Receiver<AuxOutbound>,
    cancel: &AtomicBool,
    transfer_slot: &Mutex<Option<(TransferStatus, Instant)>>,
    stop: &AtomicBool,
    log: Option<&Log>,
    was_connected: &mut bool,
) -> Result<()> {
    let (mut stream, mut ch) = if let Some(ref hub) = mesh_hub
        && !initiator
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut accepted = None;
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            if let Some(pair) = hub.take_inbound(expected_remote_id) {
                accepted = Some(pair);
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        let Some(pair) = accepted else {
            return Ok(());
        };
        pair
    } else {
        let mut stream = if initiator {
            let target = peer_aux_addr;
            let mut connected = None;
            for _ in 0..10 {
                if stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                if let Ok(s) = TcpStream::connect_timeout(&target, Duration::from_millis(350)) {
                    connected = Some(s);
                    break;
                }
                thread::sleep(Duration::from_millis(150));
            }
            let Some(s) = connected else {
                return Ok(());
            };
            s
        } else {
            let Some(listener) = aux_listener else {
                return Ok(());
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut accepted = None;
            while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((s, addr)) if addr.ip() == peer_aux_addr.ip() => {
                        accepted = Some(s);
                        break;
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(30));
                    }
                    Err(_) => return Ok(()),
                }
            }
            let Some(s) = accepted else {
                return Ok(());
            };
            s
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let (ch, remote_id) =
            security::handshake_lan_psk(&mut stream, local_device_id, passphrase, initiator)?;
        ensure!(remote_id == expected_remote_id, "aux peer identity mismatch");
        (stream, ch)
    };
    *was_connected = true;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let peer_tag = &expected_remote_id[..expected_remote_id.len().min(8)];
    if let Some(log) = log {
        log.write(
            "aux_connected",
            serde_json::json!({
                "peer": peer_tag,
                "initiator": initiator,
                "addr": peer_aux_addr.to_string(),
            }),
        );
    }

    let receive_dir = default_receive_dir()?;
    let mut next_batch_id: u64 = 1;
    let mut last_token = clipboard::change_token();
    let mut pending_empty_token: Option<(u64, u8)> = None;
    let mut last_remote_write: Option<(String, Instant)> = None;
    let mut last_poll = Instant::now();
    let mut last_input_state_poll = Instant::now();
    let mut last_local_input_state: Option<crate::input_state::InputLockState> = None;
    let mut suppress_input_sync_until = Instant::now();
    let mut last_ping = Instant::now();

    while !stop.load(Ordering::Acquire) {
        // 1. Explicit drag-and-drop / send_files / mesh-broadcast clipboard requests
        match send_rx.try_recv() {
            Ok(AuxOutbound::Files(paths)) => {
                let batch_id = next_batch_id;
                next_batch_id += 1;
                send_file_batch(
                    &mut stream,
                    &mut ch,
                    batch_id,
                    &paths,
                    cancel,
                    stop,
                    transfer_slot,
                )?;
                last_ping = Instant::now();
            }
            Ok(AuxOutbound::Clipboard { item, hash }) => {
                let kind = item.kind_str();
                let bytes = item.bytes().len();
                match item {
                    ClipboardItem::Text(_) | ClipboardItem::Png(_) => {
                        send_clipboard_item(&mut stream, &mut ch, &item, &hash)?;
                    }
                    ClipboardItem::Files(paths) => {
                        let batch_id = next_batch_id;
                        next_batch_id += 1;
                        send_file_batch(
                            &mut stream,
                            &mut ch,
                            batch_id,
                            &paths,
                            cancel,
                            stop,
                            transfer_slot,
                        )?;
                    }
                }
                if let Some(log) = log {
                    log.write(
                        "aux_clip_sent",
                        serde_json::json!({
                            "peer": peer_tag,
                            "kind": kind,
                            "bytes": bytes,
                        }),
                    );
                }
                last_remote_write = None;
                last_ping = Instant::now();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }

        // 2. Poll local clipboard changes in standalone (non-mesh) mode;
        //    in mesh mode, AuxMeshHub polls once centrally and fans out via AuxOutbound::Clipboard.
        if mesh_hub.is_none() && last_poll.elapsed() >= CLIP_POLL_INTERVAL {
            last_poll = Instant::now();
            if clipboard_enabled.load(Ordering::Relaxed) {
                let token = clipboard::change_token();
                let should_read = if token != last_token {
                    pending_empty_token = Some((token, 0));
                    last_token = token;
                    true
                } else if let Some((tok, retries)) = pending_empty_token {
                    if tok == token && retries < 3 {
                        pending_empty_token = Some((tok, retries + 1));
                        true
                    } else {
                        pending_empty_token = None;
                        false
                    }
                } else {
                    false
                };
                if should_read {
                    if let Some(item) = clipboard::read() {
                        pending_empty_token = None;
                        let hash = item.hash_hex();
                        let is_echo = last_remote_write.as_ref().is_some_and(|(h, ts)| {
                            ts.elapsed() < REMOTE_CLIP_ECHO_WINDOW && h == &hash
                        });
                        if !is_echo {
                            match item {
                                ClipboardItem::Text(_) | ClipboardItem::Png(_) => {
                                    send_clipboard_item(&mut stream, &mut ch, &item, &hash)?;
                                }
                                ClipboardItem::Files(paths) => {
                                    let batch_id = next_batch_id;
                                    next_batch_id += 1;
                                    send_file_batch(
                                        &mut stream,
                                        &mut ch,
                                        batch_id,
                                        &paths,
                                        cancel,
                                        stop,
                                        transfer_slot,
                                    )?;
                                }
                            }
                            last_remote_write = None;
                            last_ping = Instant::now();
                        }
                    }
                }
            }
        }

        // 2b. Sync Caps Lock and Chinese/English IME state (on focus handoff or live change)
        let force_state_push = push_state_now.swap(false, Ordering::AcqRel);
        if input_state_sync_enabled.load(Ordering::Relaxed)
            && (force_state_push || last_input_state_poll.elapsed() >= INPUT_STATE_POLL_INTERVAL)
        {
            last_input_state_poll = Instant::now();
            if force_state_push {
                let st = crate::input_state::read_state();
                last_local_input_state = Some(st);
                let msg = serde_json::to_vec(&AuxHeader::InputState {
                    caps_lock: st.caps_lock,
                    ime_chinese: st.ime_chinese,
                })?;
                write_encrypted(&mut stream, &mut ch, &msg)?;
                stream.flush()?;
                last_ping = Instant::now();
            } else if local_has_focus.load(Ordering::Acquire)
                && Instant::now() >= suppress_input_sync_until
            {
                let st = crate::input_state::read_state();
                let changed = last_local_input_state.is_some_and(|prev| prev != st);
                last_local_input_state = Some(st);
                if changed {
                    let msg = serde_json::to_vec(&AuxHeader::InputState {
                        caps_lock: st.caps_lock,
                        ime_chinese: st.ime_chinese,
                    })?;
                    write_encrypted(&mut stream, &mut ch, &msg)?;
                    stream.flush()?;
                    last_ping = Instant::now();
                }
            }
        }

        // 3. Keepalive ping
        if last_ping.elapsed() >= PING_INTERVAL {
            last_ping = Instant::now();
            let ping = serde_json::to_vec(&AuxHeader::Ping)?;
            write_encrypted(&mut stream, &mut ch, &ping)?;
            stream.flush()?;
        }

        // 4. Receive incoming remote clipboard, file batch, or Caps/IME state
        if let Some(plain) = read_encrypted_nonfatal(&mut stream, &mut ch)? {
            match serde_json::from_slice::<AuxHeader>(&plain) {
                Ok(AuxHeader::Ping) | Ok(AuxHeader::FileCancel { .. }) => {}
                Ok(AuxHeader::InputState {
                    caps_lock,
                    ime_chinese,
                }) => {
                    if input_state_sync_enabled.load(Ordering::Relaxed) {
                        crate::input_state::apply_state(
                            crate::input_state::InputLockState {
                                caps_lock,
                                ime_chinese,
                            },
                            true,
                            true,
                        );
                        suppress_input_sync_until = Instant::now() + INPUT_STATE_ECHO_COOLDOWN;
                        last_local_input_state = Some(crate::input_state::read_state());
                    }
                }
                Ok(AuxHeader::Clipboard { format, hash, len }) => {
                    let item = recv_clipboard_item(&mut stream, &mut ch, &format, &hash, len)?;
                    let mut write_ok = false;
                    if clipboard_enabled.load(Ordering::Relaxed) {
                        last_remote_write = Some((hash.clone(), Instant::now()));
                        write_ok = clipboard::write(&item);
                        if write_ok {
                            last_token = clipboard::change_token();
                            if let Some(ref hub) = mesh_hub {
                                hub.record_remote_clipboard_write(hash);
                            }
                        }
                    }
                    if let Some(log) = log {
                        log.write(
                            "aux_clip_recv",
                            serde_json::json!({
                                "peer": peer_tag,
                                "kind": format,
                                "bytes": len,
                                "write_ok": write_ok,
                            }),
                        );
                    }
                }
                Ok(AuxHeader::FileBatchStart {
                    batch_id,
                    total_files,
                    total_bytes,
                    display_name,
                }) => {
                    if let Some(saved_paths) = recv_file_batch(
                        &mut stream,
                        &mut ch,
                        &receive_dir,
                        batch_id,
                        total_files,
                        total_bytes,
                        display_name,
                        cancel,
                        stop,
                        transfer_slot,
                    )? && !saved_paths.is_empty()
                    {
                        let clip_item = ClipboardItem::Files(saved_paths);
                        let hash = clip_item.hash_hex();
                        last_remote_write = Some((hash.clone(), Instant::now()));
                        if clipboard::write(&clip_item) {
                            last_token = clipboard::change_token();
                            if let Some(ref hub) = mesh_hub {
                                hub.record_remote_clipboard_write(hash);
                            }
                        }
                    }
                }
                _ => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rel_path_blocks_traversal_and_invalid_segments() {
        assert!(sanitize_rel_path("../secret.txt").is_err());
        assert!(sanitize_rel_path("a/../../b").is_err());
        assert!(sanitize_rel_path("/etc/passwd").is_err());
        assert!(sanitize_rel_path("C:/Windows/win.ini").is_err());
        assert!(sanitize_rel_path("folder/bad:name.txt").is_err());
        assert_eq!(
            sanitize_rel_path("docs/report.pdf").unwrap(),
            PathBuf::from("docs").join("report.pdf")
        );
    }

    #[test]
    fn encrypted_clipboard_text_and_png_roundtrip_over_aux_channel() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let id_a = "a".repeat(64);
        let id_b = "b".repeat(64);
        let id_b_srv = id_b.clone();
        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let (mut ch, remote) =
                security::handshake_lan_psk(&mut s, &id_b_srv, None, false).unwrap();
            assert_eq!(remote, "a".repeat(64));
            let hdr_bytes = read_encrypted_nonfatal(&mut s, &mut ch).unwrap().unwrap();
            let AuxHeader::Clipboard { format, hash, len } =
                serde_json::from_slice(&hdr_bytes).unwrap()
            else {
                panic!("expected clipboard header");
            };
            recv_clipboard_item(&mut s, &mut ch, &format, &hash, len).unwrap()
        });

        let mut client = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        let (mut ch, remote) = security::handshake_lan_psk(&mut client, &id_a, None, true).unwrap();
        assert_eq!(remote, id_b);
        let item = ClipboardItem::Text("Hello Crossflow 剪贴板同步 🚀".repeat(2000));
        let hash = item.hash_hex();
        send_clipboard_item(&mut client, &mut ch, &item, &hash).unwrap();

        let received = server.join().unwrap();
        assert_eq!(received, item);
    }

    #[test]
    fn encrypted_file_and_directory_batch_roundtrip_with_blake2s_verification() {
        let tmp = std::env::temp_dir().join(format!("cf-xfer-test-{}", std::process::id()));
        let src_dir = tmp.join("src");
        let dst_dir = tmp.join("dst");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(src_dir.join("nested")).unwrap();
        let payload_a = vec![0x5Au8; 100_000]; // spans 4 chunks
        let payload_b = b"Crossflow nested file content".to_vec();
        fs::write(src_dir.join("big.bin"), &payload_a).unwrap();
        fs::write(src_dir.join("nested").join("note.txt"), &payload_b).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let id_a = "1".repeat(64);
        let id_b = "2".repeat(64);
        let id_b_srv = id_b.clone();
        let dst_clone = dst_dir.clone();

        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            s.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let (mut ch, _) = security::handshake_lan_psk(&mut s, &id_b_srv, None, false).unwrap();
            let hdr_bytes = read_encrypted_blocking(&mut s, &mut ch).unwrap();
            let AuxHeader::FileBatchStart {
                batch_id,
                total_files,
                total_bytes,
                display_name,
            } = serde_json::from_slice(&hdr_bytes).unwrap()
            else {
                panic!("expected FileBatchStart");
            };
            let cancel = AtomicBool::new(false);
            let stop = AtomicBool::new(false);
            let slot = Mutex::new(None);
            recv_file_batch(
                &mut s,
                &mut ch,
                &dst_clone,
                batch_id,
                total_files,
                total_bytes,
                display_name,
                &cancel,
                &stop,
                &slot,
            )
            .unwrap()
            .unwrap()
        });

        let mut client = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        client.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let (mut ch, _) = security::handshake_lan_psk(&mut client, &id_a, None, true).unwrap();
        let cancel = AtomicBool::new(false);
        let stop = AtomicBool::new(false);
        let slot = Mutex::new(None);
        send_file_batch(
            &mut client,
            &mut ch,
            1,
            &[src_dir.join("big.bin"), src_dir.join("nested")],
            &cancel,
            &stop,
            &slot,
        )
        .unwrap();

        let saved = server.join().unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(fs::read(dst_dir.join("big.bin")).unwrap(), payload_a);
        assert_eq!(
            fs::read(dst_dir.join("nested").join("note.txt")).unwrap(),
            payload_b
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn remote_clipboard_echo_is_time_bounded_and_recopy_is_allowed() {
        let mut st = SharedClipEcho::default();
        let hash = ClipboardItem::Text("hello crossflow".into()).hash_hex();
        // Fresh local copy is never suppressed
        assert!(!should_suppress_echo(&mut st, &hash));
        // Immediate echo after remote write IS suppressed
        st.last_remote_write = Some((hash.clone(), Instant::now()));
        assert!(should_suppress_echo(&mut st, &hash));
        // Different content is never suppressed even within the echo window
        let other = ClipboardItem::Text("different".into()).hash_hex();
        assert!(!should_suppress_echo(&mut st, &other));
        // Expired remote write allows re-copying the exact same text
        st.last_remote_write = Some((hash.clone(), Instant::now() - Duration::from_secs(3)));
        assert!(!should_suppress_echo(&mut st, &hash));
        assert!(st.last_remote_write.is_none());
    }
}
