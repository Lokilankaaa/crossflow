//! Private one-shot Windows enrollment worker. Code travels only on inherited
//! stdin, never argv/stdout/stderr. This executable captures no input and never
//! automatically connects the resulting input session. Local persistence and
//! the peer's persistence are not atomic; failure never deletes existing records.

#[cfg(windows)]
mod windows {
    use crossflow::{peers::PeerStore, security::Identity};
    use crossflow_pairing_spike::{
        Role,
        transport::{self, PeerInfo},
    };
    use serde::{Deserialize, Serialize};
    use std::{
        io::{Read, Write},
        net::{IpAddr, SocketAddr, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU8, Ordering},
            mpsc::{self, Receiver, SyncSender},
        },
        thread,
        time::{Duration, Instant},
    };
    use zeroize::Zeroizing;

    const MAX_LINE: usize = 2048;
    const TOTAL: Duration = Duration::from_secs(120);
    const WAITING: u8 = 0;
    const CONNECTING: u8 = 1;
    const PENDING: u8 = 2;
    const APPROVING: u8 = 3;
    const TERMINAL: u8 = 4;
    const PUBLISHING: u8 = 5;
    const APPROVAL_DURING_PUBLICATION: u8 = 6;

    #[derive(Clone, Copy, Debug)]
    #[repr(u8)]
    enum Failure {
        InvalidCommand = 1,
        InvalidState,
        InvalidEndpoint,
        Cancelled,
        Timeout,
        ConnectionFailed,
        AuthenticationFailed,
        InvalidPeerInfo,
        ApprovalFailed,
        StorageFailed,
        OutputFailed,
        Internal,
    }
    impl Failure {
        fn name(self) -> &'static str {
            match self {
                Self::InvalidCommand => "invalid_command",
                Self::InvalidState => "invalid_state",
                Self::InvalidEndpoint => "invalid_endpoint",
                Self::Cancelled => "cancelled",
                Self::Timeout => "timeout",
                Self::ConnectionFailed => "connection_failed",
                Self::AuthenticationFailed => "authentication_failed",
                Self::InvalidPeerInfo => "invalid_peer_info",
                Self::ApprovalFailed => "approval_failed",
                Self::StorageFailed => "storage_failed",
                Self::OutputFailed => "output_failed",
                Self::Internal => "internal_error",
            }
        }
        fn from_tag(tag: u8) -> Self {
            match tag {
                1 => Self::InvalidCommand,
                2 => Self::InvalidState,
                3 => Self::InvalidEndpoint,
                4 => Self::Cancelled,
                5 => Self::Timeout,
                6 => Self::ConnectionFailed,
                7 => Self::AuthenticationFailed,
                8 => Self::InvalidPeerInfo,
                9 => Self::ApprovalFailed,
                10 => Self::StorageFailed,
                11 => Self::OutputFailed,
                _ => Self::Internal,
            }
        }
    }

    struct Shared {
        phase: AtomicU8,
        cancel: Arc<AtomicBool>,
        failure: AtomicU8,
    }
    impl Shared {
        fn receive_approval(&self) -> Result<(), Failure> {
            loop {
                let phase = self.phase.load(Ordering::Acquire);
                let next = match phase {
                    PENDING => APPROVING,
                    // A peer can read the written JSON before stdout.flush()
                    // returns. Hold its approval until publication is acknowledged.
                    PUBLISHING => APPROVAL_DURING_PUBLICATION,
                    _ => return Err(Failure::InvalidState),
                };
                if self
                    .phase
                    .compare_exchange(phase, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(());
                }
            }
        }

        fn finish_publication(&self) -> bool {
            loop {
                let phase = self.phase.load(Ordering::Acquire);
                let next = match phase {
                    PUBLISHING => PENDING,
                    APPROVAL_DURING_PUBLICATION => APPROVING,
                    _ => return false,
                };
                if self
                    .phase
                    .compare_exchange(phase, next, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return true;
                }
            }
        }

        fn abort(&self, failure: Failure) {
            let _ = self.failure.compare_exchange(
                0,
                failure as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.cancel.store(true, Ordering::Release);
            self.phase.store(TERMINAL, Ordering::Release);
        }
        fn error_or(&self, fallback: Failure) -> Failure {
            let failure = self.failure.load(Ordering::Acquire);
            if failure == 0 {
                fallback
            } else {
                Failure::from_tag(failure)
            }
        }
        fn check(&self, deadline: Instant) -> Result<Duration, Failure> {
            if self.cancel.load(Ordering::Acquire) {
                return Err(self.error_or(Failure::Cancelled));
            }
            if let Some(left) = deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
            {
                Ok(left)
            } else {
                self.abort(Failure::Timeout);
                Err(Failure::Timeout)
            }
        }
    }

    // Borrow directly from the zeroizing input buffer, avoiding owned serde
    // Strings/Values containing the code. Escapes are rejected before parsing.
    #[derive(Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
    enum CommandView<'a> {
        Begin { endpoint: &'a str, code: &'a str },
        Approve {},
        Cancel {},
    }
    enum Command {
        Begin {
            endpoint: SocketAddr,
            code: Zeroizing<[u8; 8]>,
        },
        Approve,
        Cancel,
    }

    fn parse_command(bytes: &[u8]) -> Result<Command, Failure> {
        if !bytes.is_ascii() || bytes.contains(&b'\\') {
            return Err(Failure::InvalidCommand);
        }
        let command: CommandView<'_> =
            serde_json::from_slice(bytes).map_err(|_| Failure::InvalidCommand)?;
        match command {
            CommandView::Begin { endpoint, code } => {
                let endpoint: SocketAddr =
                    endpoint.parse().map_err(|_| Failure::InvalidEndpoint)?;
                if !lan_endpoint(endpoint) {
                    return Err(Failure::InvalidEndpoint);
                }
                let input = code.as_bytes();
                if input.len() != 8 || !input.iter().all(u8::is_ascii_digit) {
                    return Err(Failure::InvalidCommand);
                }
                let mut code = Zeroizing::new([0; 8]);
                code.copy_from_slice(input);
                Ok(Command::Begin { endpoint, code })
            }
            CommandView::Approve {} => Ok(Command::Approve),
            CommandView::Cancel {} => Ok(Command::Cancel),
        }
    }

    fn lan_endpoint(endpoint: SocketAddr) -> bool {
        if endpoint.port() == 0 {
            return false;
        }
        match endpoint.ip() {
            IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
            IpAddr::V6(ip) => {
                if ip.to_ipv4_mapped().is_some() {
                    return false;
                }
                let SocketAddr::V6(endpoint) = endpoint else {
                    return false;
                };
                ip.segments()[0] & 0xfe00 == 0xfc00
                    || (ip.segments()[0] & 0xffc0 == 0xfe80 && endpoint.scope_id() != 0)
            }
        }
    }

    fn read_command(reader: &mut impl Read) -> Result<Option<Command>, Failure> {
        // Fixed capacity: no secret-bearing reallocations. Do not use a JSON
        // Value, String, or application BufReader as an intermediate code buffer.
        let mut line = Zeroizing::new(vec![0u8; MAX_LINE]);
        let mut used = 0;
        loop {
            if used == line.len() {
                return Err(Failure::InvalidCommand);
            }
            match reader.read(&mut line[used..used + 1]) {
                Ok(0) if used == 0 => return Ok(None),
                Ok(0) => return Err(Failure::InvalidCommand),
                Ok(_) => {
                    used += 1;
                    if line[used - 1] == b'\n' {
                        return parse_command(&line[..used]).map(Some);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Failure::InvalidCommand),
            }
        }
    }

    fn read_inputs(shared: Arc<Shared>, tx: SyncSender<Command>) {
        let input = std::io::stdin();
        let mut input = input.lock();
        loop {
            let command = match read_command(&mut input) {
                Ok(Some(command)) => command,
                Ok(None) => {
                    if shared.phase.load(Ordering::Acquire) != TERMINAL {
                        shared.abort(Failure::Cancelled);
                    }
                    return;
                }
                Err(error) => {
                    if shared.phase.load(Ordering::Acquire) != TERMINAL {
                        shared.abort(error);
                    }
                    return;
                }
            };
            if shared.phase.load(Ordering::Acquire) == TERMINAL {
                return;
            }
            let transition = match &command {
                Command::Begin { .. } => shared
                    .phase
                    .compare_exchange(WAITING, CONNECTING, Ordering::AcqRel, Ordering::Acquire)
                    .map(|_| ())
                    .map_err(|_| Failure::InvalidState),
                Command::Approve => shared.receive_approval(),
                Command::Cancel => {
                    shared.abort(Failure::Cancelled);
                    return;
                }
            };
            if transition.is_err() {
                shared.abort(Failure::InvalidState);
                return;
            }
            if tx.try_send(command).is_err() {
                shared.abort(Failure::InvalidState);
                return;
            }
        }
    }

    #[derive(Serialize)]
    #[serde(tag = "event", rename_all = "snake_case")]
    enum Event {
        Pending {
            name: String,
            platform: String,
            fingerprint: String,
        },
        Saved {
            id: String,
        },
        Error {
            error: &'static str,
        },
    }
    struct Output {
        event: Event,
        acknowledgment: SyncSender<bool>,
    }

    fn write_outputs(shared: Arc<Shared>, rx: Receiver<Output>) {
        let output = std::io::stdout();
        write_outputs_to(shared, rx, output.lock());
    }

    fn write_outputs_to(shared: Arc<Shared>, rx: Receiver<Output>, mut output: impl Write) {
        for item in rx {
            let publishing = matches!(item.event, Event::Pending { .. });
            // The publication boundary is after all crypto/metadata checks and
            // immediately before pending output. Never open this gate while
            // connecting, and never use a queued approval unless flush succeeds.
            if publishing
                && shared
                    .phase
                    .compare_exchange(CONNECTING, PUBLISHING, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
            {
                let _ = item.acknowledgment.try_send(false);
                continue;
            }
            let encoded = serde_json::to_vec(&item.event);
            let written = encoded
                .ok()
                .filter(|bytes| bytes.len() < MAX_LINE)
                .is_some_and(|mut bytes| {
                    bytes.push(b'\n');
                    output
                        .write_all(&bytes)
                        .and_then(|_| output.flush())
                        .is_ok()
                });
            if !written {
                shared.abort(Failure::OutputFailed);
                let _ = item.acknowledgment.try_send(false);
                break;
            }
            if publishing && !shared.finish_publication() {
                let _ = item.acknowledgment.try_send(false);
                continue;
            }
            let _ = item.acknowledgment.try_send(true);
        }
    }

    fn emit(
        tx: &SyncSender<Output>,
        event: Event,
        deadline: Instant,
        shared: Option<&Shared>,
    ) -> Result<(), Failure> {
        let (acknowledgment, ack) = mpsc::sync_channel(1);
        tx.try_send(Output {
            event,
            acknowledgment,
        })
        .map_err(|_| Failure::OutputFailed)?;
        // Pipe writes happen on a separate bounded writer, so a stopped reader
        // cannot keep this one-shot child alive beyond its output budget.
        let end = deadline.min(Instant::now() + Duration::from_secs(2));
        loop {
            if let Some(shared) = shared {
                shared.check(deadline)?;
            }
            let budget = end
                .checked_duration_since(Instant::now())
                .ok_or(Failure::OutputFailed)?;
            match ack.recv_timeout(budget.min(Duration::from_millis(20))) {
                Ok(true) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                _ => {
                    return Err(shared.map_or(Failure::OutputFailed, |state| {
                        state.error_or(Failure::OutputFailed)
                    }));
                }
            }
        }
    }

    fn next_command(
        rx: &Receiver<Command>,
        shared: &Shared,
        deadline: Instant,
    ) -> Result<Command, Failure> {
        loop {
            let budget = shared.check(deadline)?.min(Duration::from_millis(20));
            match rx.recv_timeout(budget) {
                Ok(command) => return Ok(command),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(shared.error_or(Failure::Cancelled));
                }
            }
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn stage_error(shared: &Shared, deadline: Instant, fallback: Failure) -> Failure {
        shared
            .check(deadline)
            .err()
            .unwrap_or_else(|| shared.error_or(fallback))
    }

    fn enroll(
        rx: &Receiver<Command>,
        output: &SyncSender<Output>,
        shared: &Shared,
        deadline: Instant,
    ) -> Result<(), Failure> {
        let Command::Begin { endpoint, code } = next_command(rx, shared, deadline)? else {
            return Err(Failure::InvalidState);
        };
        let budget = shared.check(deadline)?.min(Duration::from_millis(750));
        let socket = TcpStream::connect_timeout(&endpoint, budget)
            .map_err(|_| stage_error(shared, deadline, Failure::ConnectionFailed))?;
        let mut actual_peer = socket.peer_addr().map_err(|_| Failure::ConnectionFailed)?;
        let budget = shared.check(deadline)?;
        let pending = transport::initiate(socket, code.as_slice(), budget, shared.cancel.clone());
        drop(code);
        let pending =
            pending.map_err(|_| stage_error(shared, deadline, Failure::AuthenticationFailed))?;
        shared.check(deadline)?;
        let identified = pending
            .exchange_info(PeerInfo {
                version: 1,
                name: "Crossflow Windows PC".into(),
                platform: "windows".into(),
                input_port: 0,
            })
            .map_err(|_| stage_error(shared, deadline, Failure::InvalidPeerInfo))?;
        let peer = identified.peer_info();
        if peer.platform != "macos" || peer.input_port == 0 {
            return Err(Failure::InvalidPeerInfo);
        }
        actual_peer.set_port(peer.input_port);
        let fingerprint = hex(&identified.binding().responder_static);
        shared.check(deadline)?;
        emit(
            output,
            Event::Pending {
                name: peer.name.clone(),
                platform: peer.platform.clone(),
                fingerprint: fingerprint.clone(),
            },
            deadline,
            Some(shared),
        )?;
        if !matches!(next_command(rx, shared, deadline)?, Command::Approve) {
            return Err(Failure::InvalidState);
        }
        shared.check(deadline)?;
        let enrolled = identified
            .approve()
            .map_err(|_| stage_error(shared, deadline, Failure::ApprovalFailed))?;
        shared.check(deadline)?;
        let (role, pair_id, private, peer_public) = enrolled.keys.into_parts();
        if role != Role::Initiator || hex(&peer_public) != fingerprint {
            return Err(Failure::Internal);
        }
        let identity =
            Identity::from_authorized_pairing("windows", hex(&pair_id), private, peer_public)
                .map_err(|_| Failure::StorageFailed)?;
        let store = PeerStore::default_store().map_err(|_| Failure::StorageFailed)?;
        shared.check(deadline)?;
        let saved = store
            .remember_authorized(identity, &enrolled.peer_info.name, actual_peer)
            .map_err(|_| Failure::StorageFailed)?;
        // Cancellation during OS persistence may leave this newly committed
        // record. Never delete existing records to disguise a partial outcome.
        shared.check(deadline)?;
        if saved.id != fingerprint {
            return Err(Failure::Internal);
        }
        shared.phase.store(TERMINAL, Ordering::Release);
        emit(
            output,
            Event::Saved { id: saved.id },
            deadline,
            Some(shared),
        )
    }

    pub fn run() -> bool {
        let deadline = Instant::now() + TOTAL;
        let shared = Arc::new(Shared {
            phase: AtomicU8::new(WAITING),
            cancel: Arc::new(AtomicBool::new(false)),
            failure: AtomicU8::new(0),
        });
        let (output_tx, output_rx) = mpsc::sync_channel(2);
        let writer_shared = shared.clone();
        let writer = thread::Builder::new()
            .name("enroll-output".into())
            .spawn(move || write_outputs(writer_shared, output_rx));
        if writer.is_err() {
            return false;
        }
        let result = if std::env::args_os().count() != 1 {
            Err(Failure::InvalidCommand)
        } else {
            let (commands_tx, commands_rx) = mpsc::sync_channel(2);
            let reader_shared = shared.clone();
            match thread::Builder::new()
                .name("enroll-input".into())
                .spawn(move || read_inputs(reader_shared, commands_tx))
            {
                Ok(_) => enroll(&commands_rx, &output_tx, &shared, deadline),
                Err(_) => Err(Failure::Internal),
            }
        };
        if let Err(error) = result {
            let error = shared.error_or(error);
            shared.abort(error);
            let _ = emit(
                &output_tx,
                Event::Error {
                    error: error.name(),
                },
                Instant::now() + Duration::from_millis(500),
                None,
            );
            false
        } else {
            true
        }
        // Blocking stdin/stdout helpers are private to this process and are not
        // joined. main exits the one-shot child after the terminal event/timeout.
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn strict_commands_do_not_accept_unknown_fields_escapes_or_non_lan_routes() {
            for input in [
                br#"{"op":"begin","endpoint":"127.0.0.1:1","code":"01234567"}"#.as_slice(),
                br#"{"op":"begin","endpoint":"8.8.8.8:1","code":"01234567"}"#,
                br#"{"op":"begin","endpoint":"[::ffff:192.168.1.1]:1","code":"01234567"}"#,
                br#"{"op":"begin","endpoint":"192.168.1.1:1","code":"0123456x"}"#,
                br#"{"op":"begin","endpoint":"192.168.1.1:1","code":"\u00301234567"}"#,
                br#"{"op":"approve","unknown":true}"#,
                br#"{"op":"approve","code":"01234567"}"#,
                br#"{"op":"approve","code":null}"#,
            ] {
                assert!(parse_command(input).is_err());
            }
            let input = Zeroizing::new(
                br#"{"op":"begin","endpoint":"192.168.1.1:1234","code":"01234567"}"#.to_vec(),
            );
            let parsed = parse_command(&input).unwrap();
            let Command::Begin { code, .. } = parsed else {
                panic!("Unexpected command kind")
            };
            assert!(code.as_slice() == b"01234567");
        }

        #[test]
        fn lines_are_bounded_and_require_newline() {
            assert!(read_command(&mut std::io::Cursor::new(vec![b' '; MAX_LINE + 1])).is_err());
            assert!(read_command(&mut std::io::Cursor::new(b"{\"op\":\"approve\"}")).is_err());
            assert!(matches!(
                read_command(&mut std::io::Cursor::new(b"{\"op\":\"approve\"}\n")),
                Ok(Some(Command::Approve))
            ));
        }

        struct BlockingFlush {
            visible: SyncSender<()>,
            release: Receiver<()>,
            fail: bool,
        }
        impl Write for BlockingFlush {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.visible
                    .send(())
                    .map_err(|_| std::io::Error::other("Fixture closed"))?;
                self.release
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| std::io::Error::other("Fixture release timeout"))?;
                if self.fail {
                    Err(std::io::Error::other("Fixture output failure"))
                } else {
                    Ok(())
                }
            }
        }

        #[test]
        fn publication_boundary_handles_immediate_approval_without_blocking_cancellation() {
            for mode in 0..3 {
                let shared = Arc::new(Shared {
                    phase: AtomicU8::new(CONNECTING),
                    cancel: Arc::new(AtomicBool::new(false)),
                    failure: AtomicU8::new(0),
                });
                // Entire cryptographic phase rejects preemptive approval.
                assert!(shared.receive_approval().is_err());
                let (output_tx, output_rx) = mpsc::sync_channel(1);
                let (visible, visibility) = mpsc::sync_channel(1);
                let (release, released) = mpsc::sync_channel(1);
                let (acknowledgment, ack) = mpsc::sync_channel(1);
                let writer_shared = shared.clone();
                let writer = thread::spawn(move || {
                    write_outputs_to(
                        writer_shared,
                        output_rx,
                        BlockingFlush {
                            visible,
                            release: released,
                            fail: mode == 2,
                        },
                    )
                });
                assert!(
                    output_tx
                        .send(Output {
                            event: Event::Pending {
                                name: "Mac".into(),
                                platform: "macos".into(),
                                fingerprint: "a".repeat(64),
                            },
                            acknowledgment,
                        })
                        .is_ok()
                );
                visibility.recv_timeout(Duration::from_secs(1)).unwrap();
                // All bytes have been written, but flush hasn't returned. A real
                // UI can already have read pending and sent its explicit approval.
                shared.receive_approval().unwrap();
                assert!(
                    shared.receive_approval().is_err(),
                    "Duplicate approval was accepted"
                );
                assert!(matches!(ack.try_recv(), Err(mpsc::TryRecvError::Empty)));
                if mode == 1 {
                    shared.abort(Failure::Cancelled);
                    assert!(shared.cancel.load(Ordering::Acquire));
                }
                release.send(()).unwrap();
                assert_eq!(ack.recv_timeout(Duration::from_secs(1)).unwrap(), mode == 0);
                assert_eq!(
                    shared.phase.load(Ordering::Acquire),
                    if mode == 0 { APPROVING } else { TERMINAL }
                );
                drop(output_tx);
                writer.join().unwrap();
            }
        }
    }
}

fn main() {
    #[cfg(windows)]
    let success = windows::run();
    #[cfg(not(windows))]
    let success = {
        println!("{{\"event\":\"error\",\"error\":\"unsupported\"}}");
        false
    };
    std::process::exit(if success { 0 } else { 1 });
}
