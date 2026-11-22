//! Independent encrypted auxiliary channel for clipboard sync (and future file transfer).
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
use std::{
    io::{ErrorKind, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const CHUNK_SIZE: usize = 32 * 1024;
const MAX_FRAME_CIPHERTEXT: usize = 48 * 1024;
const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(160);
const PING_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AuxHeader {
    Ping,
    Clipboard {
        format: String,
        hash: String,
        len: usize,
    },
}

fn write_encrypted(stream: &mut TcpStream, ch: &mut SecureChannel, plaintext: &[u8]) -> Result<()> {
    let ct = ch.encrypt(plaintext)?;
    ensure!(ct.len() <= MAX_FRAME_CIPHERTEXT, "aux record exceeds limit");
    let len_bytes = (ct.len() as u32).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(&ct)?;
    stream.flush()?;
    Ok(())
}

/// Reads one encrypted record. Returns `Ok(None)` on read timeout / WouldBlock.
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
    // Once header bytes arrived, read body with a finite blocking budget.
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let res = stream.read_exact(&mut ct);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(120)));
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
    stream.set_read_timeout(Some(Duration::from_secs(4)))?;
    let read_res = (|| -> Result<()> {
        while buf.len() < total_len {
            let mut hdr = [0u8; 4];
            stream.read_exact(&mut hdr)?;
            let len = u32::from_be_bytes(hdr) as usize;
            ensure!(len >= 16 && len <= MAX_FRAME_CIPHERTEXT, "invalid aux chunk length");
            let mut ct = vec![0u8; len];
            stream.read_exact(&mut ct)?;
            let plain = ch.decrypt(&ct)?;
            ensure!(buf.len() + plain.len() <= total_len, "clipboard chunk overflow");
            buf.extend_from_slice(&plain);
        }
        Ok(())
    })();
    let _ = stream.set_read_timeout(Some(Duration::from_millis(120)));
    read_res?;

    let item = match format {
        "text" => ClipboardItem::Text(String::from_utf8(buf).context("invalid UTF-8 clipboard")?),
        "png" => ClipboardItem::Png(buf),
        _ => bail!("unsupported clipboard format"),
    };
    ensure!(item.hash_hex() == expected_hash, "clipboard checksum mismatch");
    Ok(item)
}

pub struct AuxSession {
    stop: Arc<AtomicBool>,
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
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("crossflow-aux-clip".into())
            .spawn(move || {
                let _ = run_aux(
                    aux_listener,
                    peer_main_addr,
                    initiator,
                    &local_device_id,
                    &expected_remote_id,
                    passphrase.as_deref(),
                    &clipboard_enabled,
                    &worker_stop,
                );
            })
            .ok();
        Self { stop, worker }
    }
}

impl Drop for AuxSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
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
    stream.set_read_timeout(Some(Duration::from_millis(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;

    let mut last_token = clipboard::change_token();
    let mut last_sent_hash: Option<String> = None;
    let mut last_remote_hash: Option<String> = None;
    let mut last_poll = Instant::now();
    let mut last_ping = Instant::now();

    while !stop.load(Ordering::Acquire) {
        // 1. Poll local clipboard changes
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
                            send_clipboard_item(&mut stream, &mut ch, &item, &hash)?;
                            last_sent_hash = Some(hash);
                            last_remote_hash = None;
                            last_ping = Instant::now();
                        }
                    }
                }
            }
        }

        // 2. Keepalive ping so dead links are detected cleanly
        if last_ping.elapsed() >= PING_INTERVAL {
            last_ping = Instant::now();
            let ping = serde_json::to_vec(&AuxHeader::Ping)?;
            write_encrypted(&mut stream, &mut ch, &ping)?;
        }

        // 3. Receive incoming remote clipboard update (120ms timeout)
        if let Some(plain) = read_encrypted_nonfatal(&mut stream, &mut ch)? {
            match serde_json::from_slice::<AuxHeader>(&plain) {
                Ok(AuxHeader::Ping) => {}
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
                Err(_) => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let (mut ch, remote) = security::handshake_lan_psk(&mut s, &id_b_srv, None, false).unwrap();
            assert_eq!(remote, "a".repeat(64));
            let hdr_bytes = read_encrypted_nonfatal(&mut s, &mut ch).unwrap().unwrap();
            let AuxHeader::Clipboard { format, hash, len } = serde_json::from_slice(&hdr_bytes).unwrap() else {
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
}
