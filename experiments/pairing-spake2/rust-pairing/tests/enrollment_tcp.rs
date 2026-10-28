//! Opt-in Windows localhost enrollment experiments, never production trust/UI.
//! Uses the transport library directly (no duplicate pairing wire protocol).
//! Both roles run on Windows: responder protection below is DPAPI, NOT a test
//! of macOS Keychain. No capture/injection or real user peer store is touched.
#![cfg(windows)]

use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    discovery,
    peers::PeerStore,
    security::{self, Identity},
};
use crossflow_pairing_spike::{
    Binding, Role,
    invitation::InvitationManager,
    transport::{self, EnrolledKeys, PendingSession},
};
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

// InvitationManager has a process-wide online-guess budget and busy gate.
// These tests consume two reserves total; the silent Noise test consumes none.
static TEST_LOCK: Mutex<()> = Mutex::new(());
const ATTEMPT: Duration = Duration::from_secs(8);
const JOIN: Duration = Duration::from_secs(10);

struct Worker<T> {
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<JoinHandle<Result<T>>>,
}
impl<T> Worker<T> {
    fn finish(&mut self, budget: Duration) -> Result<T> {
        let deadline = Instant::now() + budget;
        let handle = self.thread.as_ref().context("fixture already joined")?;
        while !handle.is_finished() {
            ensure!(
                Instant::now() < deadline,
                "fixture worker exceeded shutdown deadline"
            );
            thread::sleep(Duration::from_millis(2));
        }
        self.thread
            .take()
            .context("missing fixture worker")?
            .join()
            .map_err(|_| anyhow::anyhow!("fixture worker panicked"))?
    }
    fn cancel(&self) {
        self.stop.store(true, Ordering::Release);
        if let Some(socket) = self
            .socket
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}
impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.cancel();
        if self.thread.is_some() {
            let _ = self.finish(JOIN);
        }
    }
}

fn accept(listener: &TcpListener, stop: &AtomicBool) -> Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        ensure!(!stop.load(Ordering::Acquire), "fixture accept cancelled");
        match listener.accept() {
            Ok((socket, _)) => {
                // Windows sockets inherit the listener's nonblocking setting.
                socket.set_nonblocking(false)?;
                return Ok(socket);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                ensure!(Instant::now() < deadline, "fixture accept timed out");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct PendingPair {
    client: PendingSession,
    server: Worker<EnrolledKeys>,
    decision: Sender<bool>,
    binding: Binding,
}

fn authenticate_pair() -> Result<PendingPair> {
    let manager = InvitationManager::new();
    let code = manager.replace()?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (decision, command) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let socket = Arc::new(Mutex::new(None));
    let thread_socket = socket.clone();
    let handle = thread::spawn(move || -> Result<EnrolledKeys> {
        let result = (|| {
            let stream = accept(&listener, &thread_stop)?;
            *thread_socket.lock().unwrap() = Some(stream.try_clone()?);
            let permit = manager.reserve()?; // consumes budget before Noise
            let pending = transport::respond(stream, permit, ATTEMPT, thread_stop)?;
            ready_tx
                .send(*pending.binding())
                .context("fixture ready channel closed")?;
            if command
                .recv_timeout(Duration::from_secs(4))
                .context("fixture approval was not supplied")?
            {
                pending.approve()
            } else {
                drop(pending); // local decline: no approval/Ready/key output
                bail!("Fixture responder declined approval")
            }
        })();
        thread_socket.lock().unwrap().take();
        result
    });
    let mut server = Worker {
        stop,
        socket,
        thread: Some(handle),
    };
    let client_result = TcpStream::connect_timeout(&address, Duration::from_secs(1))
        .map_err(anyhow::Error::from)
        .and_then(|socket| {
            transport::initiate(
                socket,
                code.as_bytes(),
                ATTEMPT,
                Arc::new(AtomicBool::new(false)),
            )
        });
    let client = match client_result {
        Ok(client) => client,
        Err(error) => {
            server.cancel();
            let _ = server.finish(JOIN);
            return Err(error).context("fixture initiator authentication failed");
        }
    };
    let binding = ready_rx
        .recv_timeout(Duration::from_secs(2))
        .context("responder did not reach PendingSession")?;
    ensure!(
        client.binding() == &binding,
        "authenticated pending channel bindings differ"
    );
    Ok(PendingPair {
        client,
        server,
        decision,
        binding,
    })
}

struct Evidence {
    root: PathBuf,
    bundles: Vec<PathBuf>,
    cleaned: bool,
}
impl Evidence {
    fn new(label: &str) -> Result<Self> {
        let root = std::env::current_dir()?.join("runs").join(format!(
            "enrollment-{label}-{}",
            discovery::new_public_id()?
        ));
        fs::create_dir_all(root.parent().context("evidence parent missing")?)?;
        fs::create_dir(&root)?;
        eprintln!("Enrollment fixture evidence: {}", root.display());
        Ok(Self {
            root,
            bundles: vec![],
            cleaned: false,
        })
    }
    fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        let mut removed = Vec::new();
        let mut failed = false;
        for bundle in &self.bundles {
            ensure!(
                bundle.starts_with(&self.root),
                "fixture cleanup escaped evidence root"
            );
            // Exact generated fixture files only, including partial writes.
            // No recursive delete and no access to the user's real peer store.
            for name in [
                "identity.dpapi",
                "identity.json",
                "peer.json",
                "peer.pending.json",
            ] {
                let path = bundle.join(name);
                match fs::remove_file(&path) {
                    Ok(()) => removed.push(path.strip_prefix(&self.root)?.display().to_string()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => failed = true,
                }
            }
            if let Err(error) = fs::remove_dir(bundle)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failed = true;
            }
        }
        if let Err(error) = fs::remove_dir(self.root.join("peers"))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            failed = true;
        }
        fs::write(
            self.root.join("key-cleanup.txt"),
            format!(
                "cleanup_succeeded: {}\nExact generated paths removed:\n{}\nFile deletion does not guarantee secure media erasure.\n",
                !failed,
                removed.join("\n")
            ),
        )?;
        ensure!(!failed, "generated fixture key cleanup failed");
        self.cleaned = true;
        Ok(())
    }
    fn report(&self, ok: bool, detail: &str) -> Result<()> {
        fs::write(
            self.root.join("report.txt"),
            format!(
                "ok: {ok}\nactual_os_input: false\nnetwork: localhost only\nreal_user_store_accessed: false\n{detail}\n"
            ),
        )?;
        Ok(())
    }
}
impl Drop for Evidence {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn identity(keys: EnrolledKeys, expected_role: Role) -> Result<Identity> {
    let (role, pair_id, private, peer) = keys.into_parts();
    ensure!(
        role == expected_role,
        "enrollment role differs from expected fixture role"
    );
    Identity::from_authorized_pairing(
        match role {
            Role::Initiator => "windows",
            Role::Responder => "mac",
        },
        hex(&pair_id),
        private,
        peer,
    )
}

fn verify_existing_noise_kk(
    client: Identity,
    server_identity: Identity,
    listener: TcpListener,
) -> Result<()> {
    const REQUEST: &[u8] = b"Crossflow enrollment fixture request";
    const RESPONSE: &[u8] = b"Crossflow enrollment fixture response";
    let address = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let socket = Arc::new(Mutex::new(None));
    let thread_socket = socket.clone();
    let thread = thread::spawn(move || -> Result<()> {
        let result = (|| {
            let mut stream = accept(&listener, &thread_stop)?;
            *thread_socket.lock().unwrap() = Some(stream.try_clone()?);
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut channel = security::handshake(&mut stream, &server_identity, false)?;
            // Fixed-size encrypted fixture payloads, not a second input protocol.
            let mut encrypted = vec![0; REQUEST.len() + 16];
            stream.read_exact(&mut encrypted)?;
            ensure!(
                channel.decrypt(&encrypted)?.as_slice() == REQUEST,
                "KK responder payload mismatch"
            );
            stream.write_all(&channel.encrypt(RESPONSE)?)?;
            Ok(())
        })();
        thread_socket.lock().unwrap().take();
        result
    });
    let mut server = Worker {
        stop,
        socket,
        thread: Some(thread),
    };
    let client_result = (|| -> Result<()> {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let mut channel = security::handshake(&mut stream, &client, true)?;
        stream.write_all(&channel.encrypt(REQUEST)?)?;
        let mut encrypted = vec![0; RESPONSE.len() + 16];
        stream.read_exact(&mut encrypted)?;
        ensure!(
            channel.decrypt(&encrypted)?.as_slice() == RESPONSE,
            "KK initiator payload mismatch"
        );
        Ok(())
    })();
    if client_result.is_err() {
        server.cancel();
    }
    let server_result = server.finish(JOIN);
    client_result.and(server_result)
}

#[test]
#[ignore = "actual localhost pairing + Windows DPAPI fixture + current Noise_KK; no OS input"]
fn approved_local_keys_persist_without_plaintext_and_authenticate_current_protocol() -> Result<()> {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut evidence = Evidence::new("success")?;
    let result = (|| -> Result<()> {
        let PendingPair {
            client,
            mut server,
            decision,
            binding,
        } = authenticate_pair()?;
        decision
            .send(true)
            .context("responder approval channel closed")?;
        let client_keys = client.approve();
        if client_keys.is_err() {
            server.cancel();
        }
        let server_keys = server.finish(JOIN);
        let client_keys = client_keys.context("initiator explicit approval/Ready failed")?;
        let server_keys = server_keys.context("responder explicit approval/Ready failed")?;
        ensure!(
            client_keys.binding() == &binding && server_keys.binding() == &binding,
            "enrolled binding changed"
        );
        let windows = identity(client_keys, Role::Initiator)?;
        let responder = identity(server_keys, Role::Responder)?;
        ensure!(
            windows.public_key_hex() == hex(&binding.initiator_static)
                && responder.public_key_hex() == hex(&binding.responder_static),
            "enrolled private keys do not match authenticated Noise static identities"
        );
        ensure!(
            windows.pair_id == responder.pair_id,
            "enrolled pair IDs differ"
        );
        ensure!(
            windows.public_key_hex() == responder.peer_public_key_hex()
                && responder.public_key_hex() == windows.peer_public_key_hex(),
            "local/peer public identities disagree"
        );
        let expected_windows_public = windows.public_key_hex();
        let peer_id = windows.peer_public_key_hex();
        let store = PeerStore::at(evidence.root.join("peers"));
        let kk_listener = TcpListener::bind("127.0.0.1:0")?;
        let windows_bundle = evidence.root.join("peers").join(&peer_id);
        evidence.bundles.push(windows_bundle.clone());
        let saved =
            store.remember_authorized(windows, "Enrollment fixture", kk_listener.local_addr()?)?;
        let (resolved, path) = store.resolve(&saved.id)?;
        ensure!(
            resolved == saved && path == windows_bundle,
            "saved peer did not resolve consistently"
        );
        ensure!(
            path.join("identity.dpapi").is_file() && !path.join("identity.json").exists(),
            "Windows fixture identity was not protected-only"
        );
        let reloaded_windows = Identity::load(&path)?;
        ensure!(
            reloaded_windows.public_key_hex() == expected_windows_public,
            "Windows identity changed during persistence"
        );
        let responder_bundle = evidence.root.join("mac-role-windows-protection");
        evidence.bundles.push(responder_bundle.clone());
        // This is a mac ROLE protected by Windows DPAPI solely for this test.
        // It does not implement or claim to exercise macOS Keychain storage.
        responder.save_protected(&responder_bundle)?;
        ensure!(
            !responder_bundle.join("identity.json").exists(),
            "responder fixture wrote plaintext identity"
        );
        let reloaded_responder = Identity::load(&responder_bundle)?;
        ensure!(
            reloaded_responder.public_key_hex() == responder.public_key_hex(),
            "responder identity changed during persistence"
        );
        verify_existing_noise_kk(reloaded_windows, reloaded_responder, kk_listener)
    })();
    let cleanup = evidence.cleanup();
    evidence.report(result.is_ok() && cleanup.is_ok(),
        "pairing: real Noise_XX + SPAKE2 + both explicit approvals + Ready\nidentity_transfer: none; locally generated keys\npersistence: Windows temporary PeerStore and Windows-protected responder-role fixture\nplaintext_identity_files: none expected\nprotocol_check: existing Noise_KK and bidirectional fixed encrypted payload\nmac_keychain_tested: false\natomic_bilateral_commit_claimed: false")?;
    result.and(cleanup)
}

#[test]
#[ignore = "actual localhost pending pairing then peer decline; no persistence or input"]
fn one_sided_approval_with_peer_drop_does_not_return_keys() -> Result<()> {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut evidence = Evidence::new("decline")?;
    let result = (|| -> Result<()> {
        let PendingPair {
            client,
            mut server,
            decision,
            ..
        } = authenticate_pair()?;
        // Both sides already authenticated; neither received keys yet. The
        // responder explicitly declines instead of sending Approval or Ready.
        decision
            .send(false)
            .context("responder decline channel closed")?;
        let client_result = client.approve();
        let server_result = server.finish(JOIN);
        ensure!(
            client_result.is_err(),
            "initiator obtained keys without peer approval"
        );
        match server_result {
            Ok(_) => bail!("declining responder obtained keys"),
            Err(error) => ensure!(
                error.to_string() == "Fixture responder declined approval",
                "responder failed before the intended decline"
            ),
        }
        Ok(())
    })();
    let cleanup = evidence.cleanup();
    evidence.report(result.is_ok() && cleanup.is_ok(),
        "both_endpoints_reached_pending: required\nresponder_explicit_action: decline/drop before Approval\nenrolled_keys_returned: none expected\ntrust_written: false\nlimitation: does not claim atomic success after both endpoints already sent Ready")?;
    result.and(cleanup)
}

#[test]
#[ignore = "actual silent localhost Noise handshake cancellation; no persistence or input"]
fn silent_handshake_cancel_returns_within_one_second() -> Result<()> {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut evidence = Evidence::new("cancel")?;
    let result = (|| -> Result<u128> {
        let manager = InvitationManager::new();
        let display = manager.replace()?;
        let code = Zeroizing::new(*display.as_bytes());
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let socket = Arc::new(Mutex::new(None));
        let thread_socket = socket.clone();
        let handle = thread::spawn(move || -> Result<()> {
            let result = (|| -> Result<()> {
                let stream = TcpStream::connect_timeout(&address, Duration::from_secs(1))?;
                *thread_socket.lock().unwrap() = Some(stream.try_clone()?);
                ensure!(
                    transport::initiate(stream, &code[..], ATTEMPT, thread_stop).is_err(),
                    "silent unauthenticated endpoint returned a pending session"
                );
                Ok(())
            })();
            thread_socket.lock().unwrap().take();
            result
        });
        let mut worker = Worker {
            stop: stop.clone(),
            socket,
            thread: Some(handle),
        };
        let mut silent = accept(&listener, &AtomicBool::new(false))?;
        silent.set_read_timeout(Some(Duration::from_secs(1)))?;
        silent.read_exact(&mut [0; 1])?; // prove Noise has started; never reply
        let began = Instant::now();
        stop.store(true, Ordering::Release);
        worker.finish(Duration::from_secs(1))?;
        let elapsed = began.elapsed().as_millis();
        // Keep the silent socket alive until AFTER exit: EOF cannot explain it.
        drop(silent);
        Ok(elapsed)
    })();
    let cleanup = evidence.cleanup();
    evidence.report(result.is_ok() && cleanup.is_ok(), &format!(
        "cancel_budget_ms: 1000\nobserved_cancel_ms: {}\ninvitation_reserves_consumed: 0\ntrust_written: false", result.as_ref().map(|v| v.to_string()).unwrap_or_else(|_| "failed".into())
    ))?;
    result.map(|_| ()).and(cleanup)
}
