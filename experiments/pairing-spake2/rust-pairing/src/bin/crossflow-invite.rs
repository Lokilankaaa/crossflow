//! Private macOS invitation process. UI owns display and explicit approval.
//! Codes travel on inherited stdout only, never arguments or diagnostic logs.
//! This process saves an authorized peer; it never starts input injection.
#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result, bail, ensure};
    use crossflow::{peers::PeerStore, security::Identity};
    use crossflow_pairing_spike::{
        Role,
        host::{self, HostCommand, HostConfig, HostEvent},
        transport::PeerInfo,
    };
    use serde::Deserialize;
    use serde_json::json;
    use std::{
        io::{BufRead, BufReader, Read, Write},
        sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
        thread,
        time::{Duration, Instant},
    };
    use zeroize::Zeroizing;

    #[derive(Deserialize)]
    #[serde(tag = "op", deny_unknown_fields)]
    enum Command {
        #[serde(rename = "begin")]
        Begin { name: String },
        #[serde(rename = "approve")]
        Approve { attempt: String },
        #[serde(rename = "cancel")]
        Cancel,
    }

    struct Output(Zeroizing<Vec<u8>>, SyncSender<bool>);
    fn emit(output: &SyncSender<Output>, bytes: Zeroizing<Vec<u8>>) -> Result<()> {
        ensure!(bytes.len() <= 2048, "Invitation output exceeded limit");
        let (ack, done) = mpsc::sync_channel(1);
        output
            .try_send(Output(bytes, ack))
            .map_err(|_| anyhow::anyhow!("Invitation output unavailable"))?;
        ensure!(
            done.recv_timeout(Duration::from_secs(2)).unwrap_or(false),
            "Invitation output did not complete"
        );
        Ok(())
    }
    fn emit_json(output: &SyncSender<Output>, value: serde_json::Value) -> Result<()> {
        let mut bytes = Zeroizing::new(serde_json::to_vec(&value)?);
        bytes.push(b'\n');
        emit(output, bytes)
    }

    fn controls() -> Result<Receiver<Command>> {
        let (sender, receiver) = mpsc::sync_channel(8);
        thread::Builder::new()
            .name("crossflow-invite-control".into())
            .spawn(move || {
                let mut input = BufReader::new(std::io::stdin());
                loop {
                    let mut bytes = Zeroizing::new(Vec::with_capacity(1025));
                    let parsed = input
                        .by_ref()
                        .take(1025)
                        .read_until(b'\n', &mut bytes)
                        .ok()
                        .filter(|n| *n > 0 && *n <= 1024 && bytes.last() == Some(&b'\n'))
                        .and_then(|_| serde_json::from_slice::<Command>(&bytes).ok());
                    let Some(command) = parsed else {
                        break;
                    };
                    let cancel = matches!(command, Command::Cancel);
                    if sender.try_send(command).is_err() || cancel {
                        break;
                    }
                }
                // EOF, invalid input and saturation all close the receiver; the main
                // loop cancels rather than interpreting a partial command as consent.
            })?;
        Ok(receiver)
    }

    fn output() -> Result<SyncSender<Output>> {
        let (sender, receiver) = mpsc::sync_channel::<Output>(4);
        thread::Builder::new()
            .name("crossflow-invite-output".into())
            .spawn(move || {
                let mut stream = std::io::stdout().lock();
                while let Ok(Output(bytes, ack)) = receiver.recv() {
                    let ok = stream
                        .write_all(&bytes)
                        .and_then(|_| stream.flush())
                        .is_ok();
                    let _ = ack.try_send(ok);
                    if !ok {
                        break;
                    }
                }
            })?;
        Ok(sender)
    }

    fn public_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn run() -> Result<()> {
        ensure!(
            std::env::args_os().len() == 1,
            "Invitation worker takes no arguments"
        );
        let commands = controls()?;
        let Command::Begin { name } = commands
            .recv_timeout(Duration::from_secs(15))
            .context("Missing invitation start")?
        else {
            bail!("Invitation must start with begin");
        };
        PeerInfo {
            version: 1,
            name: name.clone(),
            platform: "macos".into(),
            input_port: 0,
        }
        .validate()?;
        let helper = std::env::current_exe()?
            .parent()
            .context("Missing worker directory")?
            .join("crossflow-keychain");
        ensure!(helper.is_file(), "Packaged Keychain helper is missing");
        let output = output()?;
        let (control_tx, control_rx) = mpsc::sync_channel(8);
        let (events_tx, events_rx) = mpsc::sync_channel(8);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let host = thread::Builder::new()
            .name("crossflow-invitation-host".into())
            .spawn(move || {
                let result = host::serve_one(
                    HostConfig {
                        bind: "0.0.0.0:0".parse().expect("constant bind"),
                        name,
                        platform: "macos".into(),
                        advertise: true,
                    },
                    control_rx,
                    events_tx,
                );
                let _ = finished_tx.send(result);
            })?;
        let deadline = Instant::now() + Duration::from_secs(125);
        let exchange = (|| -> Result<_> {
            loop {
                ensure!(Instant::now() < deadline, "Invitation timed out");
                loop {
                    match commands.try_recv() {
                        Ok(Command::Approve { attempt }) => {
                            ensure!(
                                attempt.len() == 32
                                    && attempt
                                        .bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                                "Invalid approval token"
                            );
                            control_tx
                                .try_send(HostCommand::Approve { attempt })
                                .map_err(|_| anyhow::anyhow!("Invitation control unavailable"))?;
                        }
                        Ok(Command::Cancel) | Err(TryRecvError::Disconnected) => {
                            bail!("Invitation cancelled")
                        }
                        Ok(Command::Begin { .. }) => bail!("Duplicate invitation start"),
                        Err(TryRecvError::Empty) => break,
                    }
                }
                match events_rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(HostEvent::Code(code)) => {
                        // Do not convert the short code to an owned String/JSON Value.
                        let mut bytes = Zeroizing::new(Vec::with_capacity(80));
                        bytes.extend_from_slice(
                            b"{\"event\":\"code\",\"lifetime_seconds\":120,\"code\":\"",
                        );
                        bytes.extend_from_slice(code.as_bytes());
                        bytes.extend_from_slice(b"\"}\n");
                        emit(&output, bytes)?;
                    }
                    Ok(HostEvent::Ready { pairing, input }) => emit_json(
                        &output,
                        json!({"event":"invitation_ready","pairing_endpoint":pairing,"reserved_input_endpoint":input,"input_ready":false}),
                    )?,
                    Ok(HostEvent::Pending {
                        attempt,
                        name,
                        platform,
                        fingerprint,
                    }) => emit_json(
                        &output,
                        json!({"event":"pending","attempt":attempt,"name":name,"platform":platform,"fingerprint":fingerprint}),
                    )?,
                    Ok(HostEvent::AttemptFailed { attempt }) => {
                        emit_json(&output, json!({"event":"attempt_failed","attempt":attempt}))?
                    }
                    Ok(HostEvent::Discovery { state }) => {
                        emit_json(&output, json!({"event":"discovery","state":state}))?
                    }
                    Err(_) => {}
                }
                match finished_rx.try_recv() {
                    Ok(result) => return result,
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => bail!("Invitation host exited unexpectedly"),
                }
            }
        })();
        if exchange.is_err() {
            let _ = control_tx.try_send(HostCommand::Cancel);
            drop(control_tx);
            let _ = finished_rx.recv_timeout(Duration::from_secs(5));
            // Process exit terminates any remaining worker threads. No OS input
            // process was launched and no identity has been saved on this path.
            return exchange.map(|_| ());
        }
        let approved = exchange?;
        host.join()
            .map_err(|_| anyhow::anyhow!("Invitation host panicked"))?;
        ensure!(
            matches!(commands.try_recv(), Err(TryRecvError::Empty)),
            "Invitation ended before saving"
        );
        ensure!(
            approved.peer.peer_info.platform == "windows",
            "This receiver expects a Windows input peer"
        );
        let peer_name = approved.peer.peer_info.name;
        let peer_port = approved.peer.peer_info.input_port;
        let (role, pair_id, private, peer) = approved.peer.keys.into_parts();
        ensure!(role == Role::Responder, "Invitation role mismatch");
        let identity =
            Identity::from_authorized_pairing("mac", public_hex(&pair_id), private, peer)?;
        let store = PeerStore::default_store()?;
        let saved = if peer_port == 0 {
            store.remember_authorized_receiver(identity, &peer_name)?
        } else {
            let mut endpoint = approved.peer_addr;
            endpoint.set_port(peer_port);
            store.remember_authorized(identity, &peer_name, endpoint)?
        };
        // Deliberately no "input_ready": releasing the reserved socket does not
        // start a receiver. The native client must explicitly start its session.
        let reserved_port = approved.input_listener.local_addr()?.port();
        drop(approved.input_listener);
        emit_json(
            &output,
            json!({"event":"saved","id":saved.id,"suggested_receive_port":reserved_port,"input_ready":false}),
        )
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    let ok = macos::run().is_ok();
    #[cfg(not(target_os = "macos"))]
    let ok = false;
    std::process::exit(if ok { 0 } else { 1 });
}
