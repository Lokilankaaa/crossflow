//! Independent encrypted auxiliary channel for clipboard sync and streaming file transfer.
//!
//! Runs on `main_port + 1` (default 24801) using the exact same Noise_NNpsk0
//! authentication and peer identity verification as the main input session,
//! completely decoupled from the 1000Hz mouse/keyboard hot path.

use crate::{
    clipboard::{self, ClipboardItem, MAX_IMAGE_BYTES, MAX_TEXT_BYTES},
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

pub struct AuxSession {
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    send_tx: Sender<Vec<PathBuf>>,
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
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let transfer_slot = Arc::new(Mutex::new(None));
        let (send_tx, send_rx) = mpsc::channel();

        let worker_stop = stop.clone();
        let worker_cancel = cancel.clone();
        let worker_slot = transfer_slot.clone();
        let worker = thread::Builder::new()
            .name("crossflow-aux-channel".into())
            .spawn(move || {
                let _ = run_aux(
                    aux_listener,
                    peer_main_addr,
                    initiator,
                    &local_device_id,
                    &expected_remote_id,
                    passphrase.as_deref(),
                    &clipboard_enabled,
                    send_rx,
                    &worker_cancel,
                    &worker_slot,
                    &worker_stop,
                );
            })
            .ok();
        Self {
            stop,
            cancel,
            send_tx,
            transfer_slot,
            worker,
        }
    }

    pub fn send_files(&self, paths: Vec<PathBuf>) {
        if !paths.is_empty() {
            let _ = self.send_tx.send(paths);
        }
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

#[allow(clippy::too_many_arguments)]
fn run_aux(
    aux_listener: Option<Arc<TcpListener>>,
    peer_main_addr: SocketAddr,
    initiator: bool,
    local_device_id: &str,
    expected_remote_id: &str,
    passphrase: Option<&str>,
    clipboard_enabled: &AtomicBool,
    send_rx: Receiver<Vec<PathBuf>>,
    cancel: &AtomicBool,
    transfer_slot: &Mutex<Option<(TransferStatus, Instant)>>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut stream = if initiator {
        let target = SocketAddr::new(
            peer_main_addr.ip(),
            peer_main_addr.port().saturating_add(1),
        );
        let mut connected = None;
        for _ in 0..8 {
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
                Ok((s, addr)) if addr.ip() == peer_main_addr.ip() => {
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
    let (mut ch, remote_id) =
        security::handshake_lan_psk(&mut stream, local_device_id, passphrase, initiator)?;
    ensure!(remote_id == expected_remote_id, "aux peer identity mismatch");
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let receive_dir = default_receive_dir()?;
    let mut next_batch_id: u64 = 1;
    let mut last_token = clipboard::change_token();
    let mut last_sent_hash: Option<String> = None;
    let mut last_remote_hash: Option<String> = None;
    let mut last_poll = Instant::now();
    let mut last_ping = Instant::now();

    while !stop.load(Ordering::Acquire) {
        // 1. Explicit drag-and-drop / send_files requests from UI
        match send_rx.try_recv() {
            Ok(paths) => {
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
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => break,
        }

        // 2. Poll local clipboard changes (text, PNG screenshots, OR copied files/folders!)
        if last_poll.elapsed() >= CLIP_POLL_INTERVAL {
            last_poll = Instant::now();
            if clipboard_enabled.load(Ordering::Relaxed) {
                let token = clipboard::change_token();
                if token != last_token {
                    last_token = token;
                    if let Some(item) = clipboard::read() {
                        let hash = item.hash_hex();
                        let is_echo = last_remote_hash.as_deref() == Some(hash.as_str())
                            || last_sent_hash.as_deref() == Some(hash.as_str());
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
                            last_sent_hash = Some(hash);
                            last_remote_hash = None;
                            last_ping = Instant::now();
                        }
                    }
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

        // 4. Receive incoming remote clipboard or file batch
        if let Some(plain) = read_encrypted_nonfatal(&mut stream, &mut ch)? {
            match serde_json::from_slice::<AuxHeader>(&plain) {
                Ok(AuxHeader::Ping) | Ok(AuxHeader::FileCancel { .. }) => {}
                Ok(AuxHeader::Clipboard { format, hash, len }) => {
                    let item = recv_clipboard_item(&mut stream, &mut ch, &format, &hash, len)?;
                    if clipboard_enabled.load(Ordering::Relaxed) {
                        last_remote_hash = Some(hash);
                        last_sent_hash = None;
                        if clipboard::write(&item) {
                            last_token = clipboard::change_token();
                        }
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
                        last_remote_hash = Some(clip_item.hash_hex());
                        last_sent_hash = None;
                        if clipboard::write(&clip_item) {
                            last_token = clipboard::change_token();
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
}
