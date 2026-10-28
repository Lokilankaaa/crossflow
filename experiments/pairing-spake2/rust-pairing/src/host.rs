//! Reusable, attended invitation host. No UI, persistence or input processing.
//!
//! `Code` is private local display material: never log/serialize the whole event
//! stream. `Ready.input` is only a reserved listener, not an active input service.
//! Discovery advertises only the separate pairing endpoint. Every approval must
//! name the exact authenticated Pending attempt. Dropping the command sender is
//! cancellation, including during handshake and final peer confirmation.
//!
//! One manager lives across all three attempts, under the existing process-wide
//! admission gate. An invitation is never renewed by a reconnect. A successful
//! result proves the transport's mutual confirmation, not atomic durable trust.
//! The caller owns storage and any later input server. Cleanup failures produce
//! no result; a hung native worker can only be detached after a bounded wait, so
//! callers requiring process-level termination should isolate this host as well.

use crate::{
    invitation::{DisplayCode, InvitationManager},
    transport::{self, EnrolledPeer, IdentifiedSession, PeerInfo},
};
use anyhow::{Context, Result, bail, ensure};
use crossflow::discovery::{self, Advertisement, DiscoveryConfig, DiscoveryHandle, DiscoveryState};
use std::{
    io::ErrorKind,
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const LIFETIME: Duration = Duration::from_secs(120);
const TICK: Duration = Duration::from_millis(20);
const WORKER_STOP: Duration = Duration::from_secs(1);
const DISCOVERY_STOP: Duration = Duration::from_secs(2);

pub struct HostConfig {
    pub bind: SocketAddr,
    pub name: String,
    pub platform: String,
    /// Production invitations must advertise. Disabling this is restricted to
    /// loopback diagnostics and does not open a hidden LAN enrollment endpoint.
    pub advertise: bool,
}

pub enum HostCommand {
    Approve { attempt: String },
    Cancel,
}

/// Deliberately no Debug, Clone or Serialize: one variant contains the code.
pub enum HostEvent {
    Code(DisplayCode),
    Ready {
        pairing: SocketAddr,
        input: SocketAddr,
    },
    Pending {
        attempt: String,
        name: String,
        platform: String,
        fingerprint: String,
    },
    AttemptFailed {
        attempt: String,
    },
    Discovery {
        state: DiscoveryState,
    },
}

/// Reserved input listener is handed to the caller, never accepted by this host.
/// No Debug/Clone/Serialize: the enrollment includes secret identity material.
pub struct ApprovedInvitation {
    pub peer: EnrolledPeer,
    pub input_listener: TcpListener,
    pub peer_addr: SocketAddr,
}

enum Completed {
    Identified(Box<IdentifiedSession>),
    Approved(Box<EnrolledPeer>),
}

struct Flight {
    handle: JoinHandle<Result<Completed>>,
    socket: TcpStream,
}

struct Runtime {
    manager: InvitationManager,
    cancel: Arc<AtomicBool>,
    flight: Option<Flight>,
    pending: Option<IdentifiedSession>,
    discovery: Option<DiscoveryHandle>,
    deadline: Option<Instant>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            manager: InvitationManager::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            flight: None,
            pending: None,
            discovery: None,
            deadline: None,
        }
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.manager.cancel();
        self.pending.take();
        if let Some(flight) = &self.flight {
            let _ = flight.socket.shutdown(Shutdown::Both);
        }
        if let Some(discovery) = &self.discovery {
            discovery.stop();
        }
    }

    fn finish(&mut self) -> Result<()> {
        self.stop();
        let worker_ok = if let Some(flight) = self.flight.take() {
            let end = Instant::now() + WORKER_STOP;
            while !flight.handle.is_finished() && Instant::now() < end {
                thread::sleep(Duration::from_millis(5));
            }
            if flight.handle.is_finished() {
                let _ = flight.handle.join();
                true
            } else {
                false
            }
        } else {
            true
        };
        let discovery_result = self
            .discovery
            .take()
            .map(|mut d| d.wait_stopped(DISCOVERY_STOP))
            .transpose();
        ensure!(worker_ok, "Invitation worker shutdown was not confirmed");
        discovery_result.context("Invitation discovery shutdown was not confirmed")?;
        Ok(())
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs one 120-second invitation and returns at most one locally approved peer.
/// The event receiver must consume promptly: a full/disconnected bounded channel
/// aborts instead of blocking cancellation or silently dropping critical events.
pub fn serve_one(
    config: HostConfig,
    commands: Receiver<HostCommand>,
    events: SyncSender<HostEvent>,
) -> Result<ApprovedInvitation> {
    validate_config(&config)?;
    let mut runtime = Runtime::new();
    let result = run(&config, &commands, &events, &mut runtime);
    let cleanup = runtime.finish();
    match (result, cleanup) {
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(_)) => {
            Err(error.context("Invitation failed and shutdown was not confirmed"))
        }
        (Ok(_), Err(error)) => Err(error),
        (Ok(approved), Ok(())) => {
            // Cancellation/EOF received during bounded discovery shutdown still
            // prevents handing secret enrollment material to the caller.
            reject_commands(&commands)?;
            ensure!(
                runtime
                    .deadline
                    .is_some_and(|deadline| Instant::now() < deadline),
                "Invitation expired during shutdown"
            );
            Ok(approved)
        }
    }
}

fn emit(events: &SyncSender<HostEvent>, event: HostEvent) -> Result<()> {
    // Never format TrySendError: it owns the possibly secret event.
    events
        .try_send(event)
        .map_err(|_| anyhow::anyhow!("Invitation event channel unavailable"))
}

fn reject_commands(commands: &Receiver<HostCommand>) -> Result<()> {
    match commands.try_recv() {
        Ok(HostCommand::Cancel) | Err(TryRecvError::Disconnected) => bail!("Invitation cancelled"),
        Ok(HostCommand::Approve { .. }) => bail!("Invitation approval is not pending"),
        Err(TryRecvError::Empty) => Ok(()),
    }
}

fn validate_config(config: &HostConfig) -> Result<()> {
    PeerInfo {
        version: 1,
        name: config.name.clone(),
        platform: config.platform.clone(),
        input_port: 0,
    }
    .validate()?;
    if config.advertise {
        ensure!(
            lan_address(config.bind, true),
            "Advertised invitation must bind a private LAN or unspecified address"
        );
        if let IpAddr::V6(ip) = config.bind.ip() {
            // Advertisement currently cannot preserve a concrete link-local scope.
            ensure!(
                !ip.is_unicast_link_local(),
                "Concrete IPv6 link-local advertisement is unsupported"
            );
        }
    } else {
        ensure!(
            config.bind.ip().is_loopback(),
            "Unadvertised invitation is restricted to loopback diagnostics"
        );
    }
    Ok(())
}

fn lan_address(address: SocketAddr, unspecified: bool) -> bool {
    match address {
        SocketAddr::V4(value) => {
            (unspecified && value.ip().is_unspecified())
                || value.ip().is_private()
                || value.ip().is_link_local()
        }
        SocketAddr::V6(value) => {
            value.ip().to_ipv4_mapped().is_none()
                && ((unspecified && value.ip().is_unspecified())
                    || value.ip().is_unique_local()
                    || (value.ip().is_unicast_link_local() && value.scope_id() != 0))
        }
    }
}

fn run(
    config: &HostConfig,
    commands: &Receiver<HostCommand>,
    events: &SyncSender<HostEvent>,
    runtime: &mut Runtime,
) -> Result<ApprovedInvitation> {
    reject_commands(commands)?;
    let pairing = TcpListener::bind(config.bind).context("Bind invitation listener")?;
    pairing
        .set_nonblocking(true)
        .context("Configure invitation listener")?;
    let pairing_addr = pairing.local_addr()?;
    let mut input_addr = config.bind;
    input_addr.set_port(0);
    let input_listener = TcpListener::bind(input_addr).context("Reserve input listener")?;
    let input_addr = input_listener.local_addr()?;
    let deadline = Instant::now()
        .checked_add(LIFETIME)
        .context("Invitation deadline overflow")?;
    runtime.deadline = Some(deadline);
    let code = runtime.manager.replace()?;
    if config.advertise {
        let id = discovery::new_public_id()?;
        runtime.discovery = Some(DiscoveryHandle::start_pairing(DiscoveryConfig {
            advertisement: Some(Advertisement {
                device_id: id.clone(),
                friendly_name: config.name.clone(),
                platform: config.platform.clone(),
                protocol: discovery::PAIRING_PROTOCOL_VERSION,
                port: pairing_addr.port(),
                listen_ip: pairing_addr.ip(),
            }),
            local_device_id: Some(id),
            compatible_protocol: discovery::PAIRING_PROTOCOL_VERSION,
        })?);
    }
    emit(
        events,
        HostEvent::Ready {
            pairing: pairing_addr,
            input: input_addr,
        },
    )?;
    emit(events, HostEvent::Code(code))?;
    let info = PeerInfo {
        version: 1,
        name: config.name.clone(),
        platform: config.platform.clone(),
        input_port: input_addr.port(),
    };
    let mut last_discovery = None;
    let mut attempt = String::new();
    let mut peer_addr = None;
    let mut socket_for_approval = None;
    let mut attempts = 0u8;
    let mut approving = false;

    loop {
        ensure!(Instant::now() < deadline, "Invitation expired");
        // One actor owns publication and command admission. After successful
        // Pending enqueue it sets pending before the next command read, so fast
        // readers cannot race a separate output/state transition.
        match commands.try_recv() {
            Ok(HostCommand::Cancel) | Err(TryRecvError::Disconnected) => {
                bail!("Invitation cancelled")
            }
            Ok(HostCommand::Approve { attempt: requested }) => {
                ensure!(
                    requested == attempt && !approving && runtime.pending.is_some(),
                    "Invitation approval does not match current pending attempt"
                );
                let identified = runtime.pending.take().context("No pending invitation")?;
                let socket = socket_for_approval
                    .take()
                    .context("Missing pending socket")?;
                runtime.flight = Some(Flight {
                    socket,
                    handle: thread::Builder::new()
                        .name("crossflow-invite-approve".into())
                        .spawn(move || {
                            identified
                                .approve()
                                .map(|peer| Completed::Approved(Box::new(peer)))
                        })
                        .context("Start invitation approval worker")?,
                });
                approving = true;
            }
            Err(TryRecvError::Empty) => {}
        }
        if let Some(snapshot) = runtime
            .discovery
            .as_ref()
            .and_then(DiscoveryHandle::try_snapshot)
        {
            ensure!(
                !matches!(
                    snapshot.state,
                    DiscoveryState::Error | DiscoveryState::Stopping | DiscoveryState::Stopped
                ),
                "Invitation discovery stopped or failed"
            );
            if last_discovery != Some(snapshot.state) {
                emit(
                    events,
                    HostEvent::Discovery {
                        state: snapshot.state,
                    },
                )?;
                last_discovery = Some(snapshot.state);
            }
        }
        if runtime
            .flight
            .as_ref()
            .is_some_and(|f| f.handle.is_finished())
        {
            let flight = runtime.flight.take().context("Missing invitation worker")?;
            let completed = flight
                .handle
                .join()
                .map_err(|_| anyhow::anyhow!("Invitation worker panicked"))?;
            // Admit no approval that was queued while authentication was running.
            reject_commands(commands)?;
            ensure!(Instant::now() < deadline, "Invitation expired");
            match completed {
                Ok(Completed::Identified(identified)) => {
                    let peer = identified.peer_info();
                    emit(
                        events,
                        HostEvent::Pending {
                            attempt: attempt.clone(),
                            name: peer.name.clone(),
                            platform: peer.platform.clone(),
                            fingerprint: hex(&identified.binding().initiator_static),
                        },
                    )?;
                    socket_for_approval = Some(flight.socket);
                    runtime.pending = Some(*identified);
                }
                Ok(Completed::Approved(peer)) => {
                    return Ok(ApprovedInvitation {
                        peer: *peer,
                        input_listener,
                        peer_addr: peer_addr.context("Missing authenticated peer address")?,
                    });
                }
                Err(_) => {
                    emit(
                        events,
                        HostEvent::AttemptFailed {
                            attempt: attempt.clone(),
                        },
                    )?;
                    ensure!(!approving, "Invitation approval failed");
                    ensure!(attempts < 3, "Invitation attempt limit reached");
                }
            }
        }
        // At most one accept per tick bounds flood work. Extra sockets are closed
        // without beginning Noise/PAKE or displacing an in-progress attempt.
        match pairing.accept() {
            Ok((socket, address)) => {
                let allowed = if config.advertise {
                    lan_address(address, false)
                } else {
                    address.ip().is_loopback()
                };
                if !allowed || runtime.flight.is_some() || runtime.pending.is_some() {
                    let _ = socket.shutdown(Shutdown::Both);
                } else {
                    let permit = runtime
                        .manager
                        .reserve()
                        .context("Invitation admission refused")?;
                    attempts += 1;
                    attempt = discovery::new_public_id()?;
                    peer_addr = Some(address);
                    let retained = socket
                        .try_clone()
                        .context("Retain cancellable invitation socket")?;
                    let remaining = deadline
                        .checked_duration_since(Instant::now())
                        .context("Invitation expired")?;
                    let cancel = runtime.cancel.clone();
                    let info = info.clone();
                    runtime.flight = Some(Flight {
                        socket: retained,
                        handle: thread::Builder::new()
                            .name("crossflow-invite-auth".into())
                            .spawn(move || {
                                transport::respond(socket, permit, remaining, cancel)?
                                    .exchange_info(info)
                                    .map(|session| Completed::Identified(Box::new(session)))
                            })
                            .context("Start invitation authentication worker")?,
                    });
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => bail!("Invitation accept failed"),
        }
        thread::sleep(TICK);
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(bind: &str, advertise: bool) -> HostConfig {
        HostConfig {
            bind: bind.parse().unwrap(),
            name: "Crossflow Mac test".into(),
            platform: "macos".into(),
            advertise,
        }
    }

    #[test]
    fn invitation_bind_policy_separates_lan_discovery_and_loopback_diagnostics() {
        for bind in [
            "0.0.0.0:0",
            "192.168.50.2:0",
            "169.254.1.2:0",
            "[::]:0",
            "[fd00::2]:0",
        ] {
            assert!(validate_config(&config(bind, true)).is_ok());
        }
        for bind in [
            "8.8.8.8:0",
            "127.0.0.1:0",
            "224.0.0.1:0",
            "[::1]:0",
            "[::ffff:192.168.1.2]:0",
            "[2001:db8::1]:0",
            "[fe80::1%2]:0",
        ] {
            assert!(validate_config(&config(bind, true)).is_err());
        }
        assert!(validate_config(&config("127.0.0.1:0", false)).is_ok());
        assert!(validate_config(&config("[::1]:0", false)).is_ok());
        assert!(validate_config(&config("0.0.0.0:0", false)).is_err());
        assert!(validate_config(&config("192.168.1.2:0", false)).is_err());
    }

    #[test]
    fn commands_and_event_backpressure_fail_closed_without_network() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(
            tx.send(HostCommand::Approve {
                attempt: "old".into()
            })
            .is_ok()
        );
        assert!(reject_commands(&rx).is_err());
        drop(tx);
        assert!(reject_commands(&rx).is_err());
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        emit(
            &tx,
            HostEvent::Discovery {
                state: DiscoveryState::Starting,
            },
        )
        .unwrap();
        assert!(
            emit(
                &tx,
                HostEvent::Discovery {
                    state: DiscoveryState::Browsing
                }
            )
            .is_err()
        );
    }

    struct RunningHost {
        commands: SyncSender<HostCommand>,
        events: Receiver<HostEvent>,
        handle: Option<JoinHandle<Result<ApprovedInvitation>>>,
        code: DisplayCode,
        pairing: SocketAddr,
        input: SocketAddr,
    }

    impl RunningHost {
        fn start() -> Result<Self> {
            Self::start_with(config("127.0.0.1:0", false))
        }

        fn start_with(config: HostConfig) -> Result<Self> {
            let (commands, command_rx) = std::sync::mpsc::sync_channel(8);
            let (event_tx, events) = std::sync::mpsc::sync_channel(16);
            let handle = thread::spawn(move || serve_one(config, command_rx, event_tx));
            let HostEvent::Ready { pairing, input } = events
                .recv_timeout(Duration::from_secs(3))
                .context("Host ready event")?
            else {
                bail!("Unexpected startup event");
            };
            let HostEvent::Code(code) = events
                .recv_timeout(Duration::from_secs(3))
                .context("Host private code event")?
            else {
                bail!("Unexpected code event");
            };
            Ok(Self {
                commands,
                events,
                handle: Some(handle),
                code,
                pairing,
                input,
            })
        }

        fn join(&mut self) -> Result<ApprovedInvitation> {
            let end = Instant::now() + Duration::from_secs(4);
            let handle = self.handle.as_ref().context("Host was already joined")?;
            while !handle.is_finished() {
                ensure!(Instant::now() < end, "Host test shutdown timed out");
                thread::sleep(Duration::from_millis(5));
            }
            self.handle
                .take()
                .context("Missing host thread")?
                .join()
                .map_err(|_| anyhow::anyhow!("Host test panicked"))?
        }
    }

    impl Drop for RunningHost {
        fn drop(&mut self) {
            let _ = self.commands.try_send(HostCommand::Cancel);
            if self.handle.is_some() {
                let _ = self.join();
            }
        }
    }

    #[test]
    #[ignore = "Explicit real LAN multicast test; requires CROSSFLOW_TEST_LAN_IP with a private IPv4 interface, no PAKE or OS input"]
    fn host_lan_wildcard_pairing_discovery_and_cancel() -> Result<()> {
        struct Observer(Option<DiscoveryHandle>);
        impl Observer {
            fn finish(&mut self) -> Result<()> {
                if let Some(mut handle) = self.0.take() {
                    handle.wait_stopped(DISCOVERY_STOP)?;
                }
                Ok(())
            }
        }
        impl Drop for Observer {
            fn drop(&mut self) {
                let _ = self.finish();
            }
        }

        let lan_ip: IpAddr = std::env::var("CROSSFLOW_TEST_LAN_IP")
            .context("Set CROSSFLOW_TEST_LAN_IP to the authorized local private IPv4 interface")?
            .parse()
            .context("Invalid LAN test address")?;
        ensure!(
            matches!(lan_ip, IpAddr::V4(ip) if ip.is_private() || ip.is_link_local()),
            "LAN test requires a private IPv4 interface"
        );
        let label = format!("Crossflow invite test {}", discovery::new_public_id()?);
        let mut observer = Observer(Some(DiscoveryHandle::start_pairing(DiscoveryConfig {
            advertisement: None,
            local_device_id: None,
            compatible_protocol: discovery::PAIRING_PROTOCOL_VERSION,
        })?));
        let mut config = config("0.0.0.0:0", true);
        config.name = label.clone();
        let mut host = RunningHost::start_with(config)?;
        ensure!(
            host.pairing.ip().is_unspecified() && host.pairing.is_ipv4(),
            "Host did not bind IPv4 wildcard"
        );
        // Verify private local delivery without printing the code or event.
        assert!(host.code.as_bytes().iter().all(u8::is_ascii_digit));
        let target = SocketAddr::new(lan_ip, host.pairing.port());
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(snapshot) = observer.0.as_ref().and_then(DiscoveryHandle::try_snapshot) {
                ensure!(
                    !matches!(
                        snapshot.state,
                        DiscoveryState::Error | DiscoveryState::Stopped
                    ),
                    "LAN discovery observer failed"
                );
                if let Some(candidate) = snapshot.candidates.iter().find(|candidate| {
                    candidate.friendly_name == label && candidate.addresses.contains(&target)
                }) {
                    ensure!(
                        !candidate.trusted
                            && candidate.compatible
                            && candidate.protocol == discovery::PAIRING_PROTOCOL_VERSION,
                        "Unexpected pairing discovery trust or protocol"
                    );
                    break;
                }
            }
            ensure!(
                Instant::now() < end,
                "Wildcard invitation was not discovered on the authorized LAN interface"
            );
            thread::sleep(TICK);
        }
        // No connections are made before cancellation: this test does not start
        // Noise/SPAKE2 or charge any admission reservation.
        assert!(host.commands.send(HostCommand::Cancel).is_ok());
        let error = match host.join() {
            Err(error) => error,
            Ok(_) => bail!("Cancelled invitation returned enrollment keys"),
        };
        ensure!(
            error.to_string() == "Invitation cancelled",
            "Host cancellation did not confirm clean shutdown"
        );
        ensure!(
            TcpStream::connect_timeout(&target, Duration::from_millis(300)).is_err(),
            "Cancelled advertised invitation listener remained open"
        );
        observer
            .finish()
            .context("LAN discovery observer shutdown")?;
        Ok(())
    }

    #[test]
    #[ignore = "Explicit localhost TCP test; consumes five process-wide invitation reservations, no multicast, Keychain or OS input"]
    fn host_tcp_wrong_code_budget_success_and_handshake_cancel() -> Result<()> {
        use std::io::{Read, Write};
        use zeroize::Zeroizing;
        let mut rejected = RunningHost::start()?;
        let mut wrong = Zeroizing::new(*rejected.code.as_bytes());
        wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
        for _ in 0..3 {
            let socket = TcpStream::connect_timeout(&rejected.pairing, Duration::from_secs(1))?;
            assert!(
                transport::initiate(
                    socket,
                    wrong.as_slice(),
                    Duration::from_secs(3),
                    Arc::new(AtomicBool::new(false))
                )
                .is_err(),
                "Wrong code was accepted"
            );
            assert!(matches!(
                rejected.events.recv_timeout(Duration::from_secs(3))?,
                HostEvent::AttemptFailed { .. }
            ));
        }
        assert!(
            rejected.join().is_err(),
            "Three failed attempts did not close the invitation"
        );
        assert!(
            TcpStream::connect_timeout(&rejected.pairing, Duration::from_millis(200)).is_err(),
            "Exhausted listener remained available"
        );

        let mut accepted = RunningHost::start()?;
        // A bound input port is not an input server: even TCP data queued there
        // receives no handshake or input response because host never accepts it.
        let mut reserved = TcpStream::connect_timeout(&accepted.input, Duration::from_secs(1))?;
        reserved.set_read_timeout(Some(Duration::from_millis(100)))?;
        reserved.write_all(b"reserved-port-probe")?;
        let mut one = [0u8; 1];
        let no_response = reserved
            .read(&mut one)
            .expect_err("Reserved input port unexpectedly replied");
        assert!(matches!(
            no_response.kind(),
            ErrorKind::TimedOut | ErrorKind::WouldBlock
        ));

        let socket = TcpStream::connect_timeout(&accepted.pairing, Duration::from_secs(1))?;
        let client = transport::initiate(
            socket,
            accepted.code.as_bytes(),
            Duration::from_secs(5),
            Arc::new(AtomicBool::new(false)),
        )?
        .exchange_info(PeerInfo {
            version: 1,
            name: "Crossflow Windows test".into(),
            platform: "windows".into(),
            input_port: 0,
        })?;
        assert_eq!(client.peer_info().input_port, accepted.input.port());
        let HostEvent::Pending {
            attempt,
            fingerprint,
            name,
            platform,
        } = accepted.events.recv_timeout(Duration::from_secs(3))?
        else {
            bail!("Missing authenticated pending event");
        };
        assert_eq!(fingerprint, hex(&client.binding().initiator_static));
        assert_eq!(name, "Crossflow Windows test");
        assert_eq!(platform, "windows");
        assert!(
            accepted
                .commands
                .send(HostCommand::Approve { attempt })
                .is_ok()
        );
        let client = client.approve()?;
        let server = accepted.join()?;
        assert_eq!(server.peer.keys.binding(), client.keys.binding());
        assert_eq!(server.input_listener.local_addr()?, accepted.input);
        assert_eq!(server.peer.peer_info.input_port, 0);
        drop(server);
        drop(client);
        drop(reserved);

        let mut cancelled = RunningHost::start()?;
        let mut silent = TcpStream::connect_timeout(&cancelled.pairing, Duration::from_secs(1))?;
        // Let the host reserve the fifth attempt and block on the first Noise
        // frame. No packet is sent by this client, so EOF cannot cause success.
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        assert!(cancelled.commands.send(HostCommand::Cancel).is_ok());
        assert!(cancelled.join().is_err());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "Cancellation did not interrupt the handshake promptly"
        );
        silent.set_read_timeout(Some(Duration::from_secs(1)))?;
        match silent.read(&mut one) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset
                        | ErrorKind::ConnectionAborted
                        | ErrorKind::BrokenPipe
                ) => {}
            _ => bail!("Cancelled host socket remained active"),
        }
        Ok(())
    }
}
