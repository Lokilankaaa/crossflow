//! Windows-only attended fixture. The "mac" identity uses WINDOWS DPAPI here,
//! never macOS Keychain. No OS capture/injection. stdout's code event is PRIVATE:
//! read it through a local pipe; never tee stdout into logs or test reports.
#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result, bail, ensure};
    use crossflow::{
        discovery::{self, Advertisement, DiscoveryConfig, DiscoveryHandle, DiscoveryState},
        metrics::LogWriter,
        security::Identity,
        session::{self, DrySink},
    };
    use crossflow_pairing_spike::{
        Role,
        invitation::InvitationManager,
        transport::{self, PeerInfo},
    };
    use serde_json::{Value, json};
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::{IpAddr, SocketAddr, TcpListener},
        path::{Component, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };
    use zeroize::Zeroizing;

    const NAME: &str = "Crossflow Mac test fixture";

    fn emit(value: Value) -> Result<()> {
        let mut output = std::io::stdout().lock();
        serde_json::to_writer(&mut output, &value)?;
        output.write_all(b"\n")?;
        output.flush()?;
        Ok(())
    }

    fn emit_private_code(code: &[u8]) -> Result<()> {
        let mut line = Zeroizing::new(Vec::with_capacity(40));
        line.extend_from_slice(b"{\"event\":\"code\",\"code\":\"");
        line.extend_from_slice(code);
        line.extend_from_slice(b"\"}\n");
        let mut output = std::io::stdout().lock();
        output.write_all(&line)?;
        output.flush()?;
        Ok(())
    }

    fn public_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    struct OwnedRoot(PathBuf);
    impl OwnedRoot {
        fn create(path: PathBuf) -> Result<Self> {
            ensure!(
                path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
                "root must be an absolute new directory"
            );
            fs::create_dir(&path).context("fixture root must not already exist")?;
            Ok(Self(path))
        }
        fn cleanup(&self) -> Result<()> {
            use std::os::windows::fs::MetadataExt;
            let metadata = fs::symlink_metadata(&self.0)?;
            ensure!(
                metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
                "unsafe fixture root"
            );
            let identity = self.0.join("mac");
            match fs::symlink_metadata(&identity) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(metadata) => {
                    ensure!(
                        metadata.is_dir() && metadata.file_attributes() & 0x400 == 0,
                        "unsafe fixture credential directory"
                    );
                }
            }
            let file = identity.join("identity.dpapi");
            match fs::symlink_metadata(&file) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file() && metadata.file_attributes() & 0x400 == 0,
                        "unsafe fixture credential file"
                    );
                    fs::remove_file(file)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            // Refuse to remove unknown contents. Never recursively delete root.
            fs::remove_dir(identity)?;
            Ok(())
        }
    }
    impl Drop for OwnedRoot {
        fn drop(&mut self) {
            let _ = self.cleanup();
        }
    }

    #[derive(Debug)]
    struct DiscoveryFailure;
    impl std::fmt::Display for DiscoveryFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("fixture discovery failed")
        }
    }
    impl std::error::Error for DiscoveryFailure {}

    struct Browser(Option<DiscoveryHandle>, bool);
    impl Browser {
        fn check(&mut self, report_state: bool) -> Result<()> {
            let Some(snapshot) = self.0.as_ref().and_then(DiscoveryHandle::try_snapshot) else {
                return Ok(());
            };
            if snapshot.state == DiscoveryState::Error {
                return Err(DiscoveryFailure.into());
            }
            // "ready" describes bound TCP listeners, not discovery delivery.
            // One separate public event makes async startup visible without
            // exposing TXT data or repeating the private invitation code.
            if report_state && !self.1 && snapshot.state != DiscoveryState::Starting {
                emit(json!({"event":"discovery_state","state":snapshot.state}))?;
                self.1 = true;
            }
            Ok(())
        }

        fn finish(&mut self) -> Result<()> {
            if let Some(handle) = &mut self.0 {
                handle.wait_stopped(Duration::from_secs(2))?;
            }
            self.0 = None;
            Ok(())
        }
    }
    impl Drop for Browser {
        fn drop(&mut self) {
            let _ = self.finish();
        }
    }

    struct Controls {
        stop: Arc<AtomicBool>,
        pending: Arc<AtomicBool>,
        approve: Arc<AtomicBool>,
        invalid: Arc<AtomicBool>,
        manager: Arc<InvitationManager>,
    }
    impl Controls {
        fn start(manager: Arc<InvitationManager>) -> Self {
            let control = Self {
                stop: Arc::new(AtomicBool::new(false)),
                pending: Arc::new(AtomicBool::new(false)),
                approve: Arc::new(AtomicBool::new(false)),
                invalid: Arc::new(AtomicBool::new(false)),
                manager,
            };
            let (stop, pending, approve, invalid, manager) = (
                control.stop.clone(),
                control.pending.clone(),
                control.approve.clone(),
                control.invalid.clone(),
                control.manager.clone(),
            );
            // Process-owned stdin reader. On lifetime expiry the process exits;
            // never wait indefinitely for a parent that keeps stdin open.
            thread::spawn(move || {
                let mut input = BufReader::new(std::io::stdin());
                loop {
                    let mut line = Vec::with_capacity(257);
                    let read = input.by_ref().take(257).read_until(b'\n', &mut line);
                    let parsed = read
                        .ok()
                        .filter(|n| *n > 0 && *n <= 256 && line.last() == Some(&b'\n'))
                        .and_then(|_| serde_json::from_slice::<Value>(&line).ok());
                    let command = parsed
                        .as_ref()
                        .and_then(Value::as_object)
                        .filter(|fields| fields.len() == 1)
                        .and_then(|fields| fields.get("op"))
                        .and_then(Value::as_str);
                    match command {
                        Some("approve") if pending.load(Ordering::Acquire) => {
                            approve.store(true, Ordering::Release);
                        }
                        Some("stop") => break,
                        _ => {
                            if !line.is_empty() {
                                invalid.store(true, Ordering::Release);
                            }
                            break;
                        }
                    }
                }
                stop.store(true, Ordering::Release);
                manager.cancel();
            });
            control
        }
    }
    impl Drop for Controls {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.manager.cancel();
        }
    }

    fn args() -> Result<(PathBuf, SocketAddr, Duration)> {
        let mut args = std::env::args().skip(1);
        let (mut root, mut bind, mut seconds) = (None, None, 180u64);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--root" if root.is_none() => {
                    root = Some(PathBuf::from(args.next().context("missing root")?))
                }
                "--bind" if bind.is_none() => {
                    bind = Some(args.next().context("missing bind")?.parse::<SocketAddr>()?)
                }
                "--seconds" => seconds = args.next().context("missing seconds")?.parse()?,
                _ => bail!(
                    "usage: pairing_fixture --root NEW_ABSOLUTE_DIRECTORY --bind LAN_IP:0 [--seconds 180]"
                ),
            }
        }
        ensure!((1..=600).contains(&seconds), "seconds must be 1..600");
        let bind = bind.context("--bind required")?;
        let local = match bind.ip() {
            IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
            IpAddr::V6(ip) => ip.is_unique_local(),
        };
        ensure!(
            local,
            "fixture requires an explicit private LAN or IPv4 link-local address"
        );
        Ok((
            root.context("--root required")?,
            bind,
            Duration::from_secs(seconds),
        ))
    }

    pub fn run() -> Result<()> {
        let (root, bind, duration) = args()?;
        let root = OwnedRoot::create(root)?;
        let pairing_listener = TcpListener::bind(bind)?;
        pairing_listener.set_nonblocking(true)?;
        let pairing_address = pairing_listener.local_addr()?;
        let input_listener = TcpListener::bind(SocketAddr::new(bind.ip(), 0))?;
        let input_address = input_listener.local_addr()?;
        let manager = Arc::new(InvitationManager::new());
        let controls = Controls::start(manager.clone());
        let started = Instant::now();
        let end = started + duration;
        let invitation_end = started + Duration::from_secs(120);
        let code = manager.replace()?;
        let public_id = discovery::new_public_id()?;
        let mut browser = Browser(
            Some(DiscoveryHandle::start_pairing(DiscoveryConfig {
                advertisement: Some(Advertisement {
                    device_id: public_id.clone(),
                    friendly_name: NAME.into(),
                    platform: "macos".into(),
                    protocol: discovery::PAIRING_PROTOCOL_VERSION,
                    port: pairing_address.port(),
                    listen_ip: pairing_address.ip(),
                }),
                local_device_id: Some(public_id),
                compatible_protocol: discovery::PAIRING_PROTOCOL_VERSION,
            })?),
            false,
        );
        ensure!(!controls.stop.load(Ordering::Acquire), "fixture cancelled");
        browser.check(false)?;
        emit_private_code(code.as_bytes())?;
        drop(code);
        emit(
            json!({"event":"ready","pairing_endpoint":pairing_address,"input_endpoint":input_address,"name":NAME,"platform":"macos"}),
        )?;

        let mut enrolled = None;
        for _ in 0..3 {
            let socket = loop {
                browser.check(true)?;
                if controls.stop.load(Ordering::Acquire)
                    || Instant::now() >= end.min(invitation_end)
                {
                    break None;
                }
                match pairing_listener.accept() {
                    Ok((socket, _)) => break Some(socket),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            let Some(socket) = socket else {
                break;
            };
            let permit = manager.reserve()?; // charge BEFORE Noise work
            let timeout = end
                .min(invitation_end)
                .saturating_duration_since(Instant::now());
            let attempt = (|| -> Result<_> {
                let pending = transport::respond(socket, permit, timeout, controls.stop.clone())?
                    .exchange_info(PeerInfo {
                    version: 1,
                    name: NAME.into(),
                    platform: "macos".into(),
                    input_port: input_address.port(),
                })?;
                browser.check(true)?;
                controls.approve.store(false, Ordering::Release);
                controls.pending.store(true, Ordering::Release);
                emit(
                    json!({"event":"pending","fingerprint":public_hex(&pending.binding().initiator_static)}),
                )?;
                while !controls.approve.load(Ordering::Acquire) {
                    browser.check(true)?;
                    ensure!(
                        !controls.stop.load(Ordering::Acquire)
                            && Instant::now() < end.min(invitation_end),
                        "fixture approval cancelled or expired"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                browser.check(true)?;
                controls.pending.store(false, Ordering::Release);
                pending.approve()
            })();
            controls.pending.store(false, Ordering::Release);
            match attempt {
                Ok(value) => {
                    enrolled = Some(value);
                    break;
                }
                Err(error) if error.is::<DiscoveryFailure>() => return Err(error),
                Err(_) if controls.stop.load(Ordering::Acquire) => break,
                Err(_) => emit(json!({"event":"error","code":"pairing_failed"}))?,
            }
        }
        browser.check(true)?;
        browser.finish()?;
        drop(pairing_listener);
        let paired = enrolled.is_some();
        if let Some(enrolled) = enrolled {
            ensure!(
                !controls.stop.load(Ordering::Acquire),
                "fixture cancelled before saving"
            );
            let (role, pair_id, private, peer) = enrolled.keys.into_parts();
            ensure!(role == Role::Responder, "fixture role mismatch");
            let identity =
                Identity::from_authorized_pairing("mac", public_hex(&pair_id), private, peer)?;
            let bundle = root.0.join("mac");
            identity.save_protected(&bundle)?;
            let writer = LogWriter::new(
                &root.0.join("input.jsonl"),
                json!({"fixture":true,"os_input":false,"protection":"windows_dpapi_not_macos_keychain"}),
            )?;
            emit(
                json!({"event":"input_ready","endpoint":input_address,"protection":"windows_dpapi","role":"mac"}),
            )?;
            let result = if let Some(left) = end
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
            {
                session::receive_on_listener(
                    &input_listener,
                    &bundle,
                    &mut DrySink::default(),
                    writer.log.clone(),
                    left.as_secs_f64(),
                    &controls.stop,
                )
            } else {
                Ok(())
            };
            let finished = writer.finish();
            result?;
            finished?;
        }
        controls.stop.store(true, Ordering::Release);
        manager.cancel();
        root.cleanup()?;
        ensure!(
            !controls.invalid.load(Ordering::Acquire),
            "invalid fixture command"
        );
        let mut report = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.0.join("result.json"))?;
        serde_json::to_writer_pretty(
            &mut report,
            &json!({"fixture":"pairing_responder","paired":paired,"os_input":false,"credentials_removed":true,"storage":"windows_dpapi_not_macos_keychain"}),
        )?;
        report.flush()?;
        emit(json!({"event":"stopped","credentials_removed":true}))
    }

    pub fn report_failure(error: &anyhow::Error) {
        let code = if error.is::<DiscoveryFailure>() {
            "discovery_failed"
        } else {
            "fixture_failed"
        };
        let _ = emit(json!({"event":"error","code":code}));
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows::run() {
        windows::report_failure(&error);
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("This fixture uses Windows DPAPI only; no macOS Keychain support is claimed.");
    std::process::exit(2);
}
