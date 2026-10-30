//! Shared native-client process runner. The GUI owns this process's pipes;
//! each authenticated connection gets fresh, disabled capture/authority state.
use crate::{
    discovery::{self, Advertisement, DiscoveryConfig, DiscoveryHandle},
    injection_worker::WorkerProxy,
    metrics::{Log, LogWriter},
    pair_native::{Capture, Platform, Shortcut},
    pair_session::{Command, EstablishOutcome, Host, PairConnection, SessionStatus},
    pair_source::SharedSource,
    pair_ui::{ControlIo, Role},
    pair_wire::{Display, Hello},
    routes::LanRoutes,
    screen_runtime::RuntimeLayout,
    security::Identity,
    session::NativeDisplay,
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    io::ErrorKind,
    net::{SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub enum Mode {
    Connect {
        endpoint: SocketAddr,
        layout: RuntimeLayout,
    },
    Listen {
        bind: SocketAddr,
        discoverable: bool,
        device_name: Option<String>,
    },
}
pub struct Options {
    pub identity: PathBuf,
    pub mode: Mode,
    pub seconds: f64,
    pub max_hz: u32,
}

struct NativeHost<'a> {
    capture: &'a mut dyn Capture,
    ui: &'a mut ControlIo,
    stop: &'a AtomicBool,
    deadline: Instant,
    sharing: bool,
    epoch: u64,
    pending: Option<Command>,
}
impl NativeHost<'_> {
    fn native_command(&mut self) -> Result<Option<Command>> {
        let Some(shortcut) = self.capture.command() else { return Ok(None); };
        if shortcut == Shortcut::Stop {
            self.stop.store(true, Ordering::Release);
            if let Some(reason) = self.capture.failure() { bail!(reason); }
        }
        Ok(Some(match shortcut {
            Shortcut::Stop => Command::Stop,
            Shortcut::Toggle if self.sharing => Command::Pause,
            Shortcut::Toggle => Command::Enable { epoch: self.epoch },
        }))
    }
}
impl Host for NativeHost<'_> {
    fn pulse(&mut self, now_ns: u64) -> Result<()> {
        if Instant::now() >= self.deadline {
            self.stop.store(true, Ordering::Release);
            return Ok(());
        }
        let result = self.capture.poll(now_ns);
        if result.is_err() {
            self.stop.store(true, Ordering::Release);
        }
        result?;
        if let Some(command) = self.native_command()? {
            ensure!(self.pending.is_none() || matches!(command, Command::Stop), "native shortcut backlog");
            self.pending = Some(command);
        }
        Ok(())
    }
    fn poll_command(&mut self) -> Result<Option<Command>> {
        if let Some(command) = self.pending.take() { return Ok(Some(command)); }
        if let Some(command) = self.native_command()? { return Ok(Some(command)); }
        self.ui.poll_command()
    }
    fn publish(&mut self, status: &SessionStatus) -> Result<()> {
        self.sharing = status.endpoint.enabled;
        self.epoch = status.endpoint.epoch;
        self.ui.publish_session(status)
    }
}

fn hello(identity: &Identity, platform: &Platform) -> Result<Hello> {
    let hello = Hello {
        device_id: identity.public_key_hex(),
        inventory: (platform.inventory)()?,
    };
    crate::pair_wire::validate_inventory(&hello.inventory)?;
    Ok(hello)
}

fn prepare_worker(inventory: &[Display], log: Log) -> Result<WorkerProxy> {
    let mut command = process::Command::new(std::env::current_exe()?);
    command.args(["input-worker", "--gain", "1"]);
    let mut sink = WorkerProxy::spawn(&mut command)?;
    sink.attach_failure_log(log);
    sink.set_displays(
        inventory
            .iter()
            .map(|d| NativeDisplay {
                id: d.wire_id,
                x: d.x,
                y: d.y,
                width: d.width,
                height: d.height,
            })
            .collect(),
    )?;
    Ok(sink)
}

/// Preparation has no input authority. Keep the existing client's status pipe
/// responsive while bounded OS/handshake calls run on a temporary thread.
fn prepare<T: Send>(
    ui: &mut ControlIo,
    stop: &AtomicBool,
    deadline: Instant,
    state: &str,
    work: impl FnOnce() -> Result<T> + Send,
) -> Result<T> {
    ensure!(
        !stop.load(Ordering::Acquire) && Instant::now() < deadline,
        "pair preparation cancelled"
    );
    thread::scope(|scope| {
        let worker = scope.spawn(work);
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                stop.store(true, Ordering::Release);
            }
            if ui.publish_state(state, None).is_err() {
                stop.store(true, Ordering::Release);
            }
            match ui.poll_command() {
                Ok(Some(Command::Stop)) => {
                    stop.store(true, Ordering::Release);
                }
                Err(_) => {
                    stop.store(true, Ordering::Release);
                }
                _ => {} // A stale start while preparing cannot arm the session.
            }
            thread::sleep(Duration::from_millis(20));
        }
        let result = worker
            .join()
            .map_err(|_| anyhow::anyhow!("pair preparation panicked"))?;
        // Return any created resource even after cancellation so the caller
        // can obtain explicit cleanup confirmation. The UI retains IO errors.
        result
    })
}

pub fn run(
    options: Options,
    writer: LogWriter,
    stop: Arc<AtomicBool>,
    platform: Platform,
) -> Result<()> {
    ensure!(
        options.seconds.is_finite() && options.seconds > 0. && options.seconds <= 3600.,
        "invalid pair duration"
    );
    ensure!(
        (60..=8000).contains(&options.max_hz),
        "invalid pair input rate"
    );
    let identity = Identity::load(&options.identity)?;
    let (input, output) = crate::injection_worker::stdio_files()?;
    let listener = match &options.mode {
        Mode::Listen { bind, .. } => {
            let listener = TcpListener::bind(bind)?;
            listener.set_nonblocking(true)?;
            Some(listener)
        }
        _ => None,
    };
    let bound = listener.as_ref().map(TcpListener::local_addr).transpose()?;
    let role = if listener.is_some() {
        Role::Receiver
    } else {
        Role::Sender
    };
    let mut ui = ControlIo::start(
        input,
        output,
        stop.clone(),
        role,
        identity.public_key_hex(),
        bound.map(|b| b.to_string()),
    )?;
    let log = writer.log.clone();
    let deadline = Instant::now() + Duration::from_secs_f64(options.seconds);
    let mut routes = match options.mode {
        Mode::Connect { endpoint, .. } => Some(LanRoutes::new(endpoint)),
        _ => None,
    };
    let mut discovery = None;
    let result = (|| -> Result<()> {
        if let (
            Some(bound),
            Mode::Listen {
                discoverable: true,
                device_name,
                ..
            },
        ) = (bound, &options.mode)
        {
            let advertisement = Advertisement {
                device_id: discovery::new_public_id()?,
                friendly_name: device_name
                    .clone()
                    .unwrap_or_else(|| discovery::default_device_name().into()),
                platform: std::env::consts::OS.into(),
                protocol: u16::from(crate::protocol::WIRE_VERSION),
                port: bound.port(),
                listen_ip: bound.ip(),
            };
            discovery = Some(DiscoveryHandle::start(DiscoveryConfig {
                local_device_id: Some(advertisement.device_id.clone()),
                advertisement: Some(advertisement),
                compatible_protocol: u16::from(crate::protocol::WIRE_VERSION),
            })?);
        }
        let state = if listener.is_some() {
            "listening"
        } else {
            "connecting"
        };
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            ui.publish_state(state, None)?;
            if matches!(ui.poll_command()?, Some(Command::Stop)) {
                break;
            }
            let stream = if let Some(listener) = &listener {
                match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                }
            } else {
                let address = routes
                    .as_mut()
                    .context("missing pair routes")?
                    .next_endpoint(&log);
                match TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
                    Ok(stream) => stream,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            };
            let address = stream.peer_addr()?;
            let local = hello(&identity, &platform)?;
            let proposal = match &options.mode {
                Mode::Connect { layout, .. } => Some(layout.clone()),
                _ => None,
            };
            let established = prepare(&mut ui, &stop, deadline, state, || {
                PairConnection::establish(
                    stream,
                    &identity,
                    listener.is_none(),
                    local.clone(),
                    proposal,
                    &stop,
                    log.clone(),
                )
            });
            let mut connection = match established {
                Ok(EstablishOutcome::InventoryServed) => continue,
                Ok(EstablishOutcome::Session(connection)) => connection,
                Err(error) if stop.load(Ordering::Acquire) => {
                    log.write(
                        "pair_setup_cancelled",
                        serde_json::json!({"reason":format!("{error:#}")}),
                    );
                    break;
                }
                Err(error) => {
                    log.write(
                        "pair_setup_failed",
                        serde_json::json!({"reason":format!("{error:#}")}),
                    );
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                break;
            }
            if let Some(routes) = &mut routes {
                routes.authenticated(address);
            }
            connection.set_max_hz(options.max_hz)?;
            let source = SharedSource::new(connection.runtime().clone())?;
            // Worker startup precedes capture so its bounded startup wait cannot
            // expire an already-running capture's owner lease.
            let mut sink = prepare(&mut ui, &stop, deadline, "starting", || {
                prepare_worker(&local.inventory, log.clone())
            })?;
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                sink.shutdown()?;
                break;
            }
            let capture_result = prepare(&mut ui, &stop, deadline, "starting", || {
                (platform.capture)(source.clone(), log.clone())
            });
            let mut capture = match capture_result {
                Ok(capture) => capture,
                Err(error) => {
                    let cleanup = sink.shutdown();
                    return Err(error.context(format!(
                        "capture startup failed; input-owner cleanup: {}",
                        if cleanup.is_ok() {
                            "confirmed"
                        } else {
                            "unconfirmed"
                        }
                    )));
                }
            };
            if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
                let capture_cleanup = capture.stop();
                let sink_cleanup = sink.shutdown();
                capture_cleanup?;
                sink_cleanup?;
                break;
            }
            let session = connection.run(
                &source,
                &mut sink,
                &mut NativeHost {
                    capture: capture.as_mut(),
                    ui: &mut ui,
                    stop: &stop,
                    deadline,
                    sharing: false,
                    epoch: 0,
                    pending: None,
                },
                &stop,
                log.clone(),
            );
            // All cleanup attempts execute even if an earlier one failed.
            let capture_cleanup = capture.stop();
            let sink_cleanup = sink.shutdown();
            if capture_cleanup.is_err() || sink_cleanup.is_err() {
                log.write("pair_cleanup_failed", serde_json::json!({"capture_confirmed":capture_cleanup.is_ok(),"worker_confirmed":sink_cleanup.is_ok()}));
                bail!("pair cleanup unconfirmed; restart the client before sharing again");
            }
            if let Err(error) = session {
                log.write(
                    "pair_session_failed",
                    serde_json::json!({"reason":format!("{error:#}"),"cleanup_confirmed":true}),
                );
                if stop.load(Ordering::Acquire) {
                    return Err(error);
                }
            }
            // A replacement endpoint is disabled. Neither this loop nor status
            // publication authorizes sharing after reconnection.
        }
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    let route_cleanup = routes.as_mut().map(LanRoutes::finish).transpose();
    let discovery_cleanup = discovery
        .as_mut()
        .map(|d| d.wait_stopped(Duration::from_secs(3)))
        .transpose();
    let cleanup = result
        .and(route_cleanup.map(|_| ()))
        .and(discovery_cleanup.map(|_| ()));
    let logged = writer.finish();
    ui.finish(cleanup.and(logged))
}

/// Inventory query used by the actual arrangement editor. This authenticates
/// the saved peer, but never creates capture, an input worker or a grant.
pub fn query(
    endpoint: SocketAddr,
    identity: &Identity,
    platform: &Platform,
    stop: &AtomicBool,
    log: &Log,
) -> Result<Vec<NativeDisplay>> {
    let mut routes = LanRoutes::new(endpoint);
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = (|| {
        let local = hello(identity, platform)?;
        let mut last_error = anyhow::anyhow!("paired display query cancelled");
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            let address = routes.next_endpoint(log);
            let result = TcpStream::connect_timeout(&address, Duration::from_millis(250))
                .map_err(anyhow::Error::from)
                .and_then(|stream| {
                    crate::pair_session::query_inventory(
                        stream,
                        identity,
                        local.clone(),
                        stop,
                        log.clone(),
                    )
                });
            match result {
                Ok(displays) => {
                    return Ok(displays
                        .into_iter()
                        .map(|d| NativeDisplay {
                            id: d.wire_id,
                            x: d.x,
                            y: d.y,
                            width: d.width,
                            height: d.height,
                        })
                        .collect());
                }
                Err(error) => last_error = error,
            }
            thread::sleep(Duration::from_millis(50));
        }
        Err(last_error)
    })();
    let cleanup = routes.finish();
    cleanup?;
    result
}
