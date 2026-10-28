use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use crossflow::{
    metrics::{LogWriter, summarize},
    session,
    state::Motion,
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[derive(Parser)]
#[command(
    version,
    about = "Crossflow Rust phase 0 — shared input and measurement prototype"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Private native-client session; inherited pipes carry controls, not input.
    #[command(hide = true)]
    DesktopSend {
        #[arg(long, required_unless_present = "peer", conflicts_with = "peer")]
        host: Option<String>,
        #[arg(long, required_unless_present = "peer", conflicts_with = "peer")]
        identity: Option<PathBuf>,
        /// App-managed trusted device ID; resolves its protected identity and route.
        #[arg(long)]
        peer: Option<String>,
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 3600.0)]
        seconds: f64,
        #[arg(long, default_value_t = 1000)]
        max_hz: u32,
    },
    /// Private inherited-pipe input owner. Never connects to the network.
    #[command(hide = true)]
    InputWorker {
        #[arg(long)]
        gain: f64,
    },
    /// Create paired static identities. Share only the mac bundle with the Mac.
    Pair {
        #[arg(long, default_value = ".pairing")]
        out: PathBuf,
    },
    /// List app-managed paired devices; no private keys or file paths are returned.
    Peers,
    /// Development migration of an existing trusted diagnostic identity.
    #[command(hide = true)]
    PeerImport {
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        endpoint: std::net::SocketAddr,
    },
    /// Browse nearby Crossflow services; discovery never grants input permission.
    Discover {
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
        /// Browse enrollment invitations, never input receiver endpoints.
        #[arg(long)]
        pairing: bool,
    },
    /// Receive encrypted movement. Default is a sink without OS input injection.
    Receive {
        #[arg(long, default_value = "127.0.0.1:24800")]
        bind: String,
        #[arg(long, required_unless_present = "peer", conflicts_with = "peer")]
        identity: Option<PathBuf>,
        /// App-managed authorized sender; no identity-file selection required.
        #[arg(long, conflicts_with = "identity")]
        peer: Option<String>,
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 60.0)]
        seconds: f64,
        #[arg(long)]
        inject: bool,
        /// Experimental keyboard/buttons/scroll injection; requires explicit OS injection.
        #[arg(long, requires = "inject")]
        inject_actions: bool,
        #[arg(long, default_value_t = 1.0)]
        gain: f64,
        /// Advertise this bound receiver on the LAN. Discovery does not grant trust.
        #[arg(long)]
        discoverable: bool,
        /// Public discovery label; defaults to a generic platform name.
        #[arg(long, requires = "discoverable")]
        device_name: Option<String>,
    },
    /// Capture Windows Raw Input; Ctrl+Alt+Space arms, Ctrl+Alt+Esc exits.
    Send {
        #[arg(long)]
        host: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 60.0)]
        seconds: f64,
        #[arg(long, default_value_t = 1000)]
        max_hz: u32,
        #[arg(long)]
        synthetic: bool,
        /// Experimental keys/buttons/wheel. Local Windows input is not suppressed.
        #[arg(long, conflicts_with = "synthetic")]
        experimental_inputs: bool,
    },
    /// Capture cadence only; does not open a network connection or inject input.
    Capture {
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
    },
    Summarize {
        path: PathBuf,
    },
    /// Validate a saved layout draft without changing any display or input.
    LayoutCheck {
        path: PathBuf,
    },
    /// Calculate one logical screen crossing without moving a system cursor.
    LayoutTrace {
        path: PathBuf,
        #[arg(long)]
        from: String,
        #[arg(long, allow_hyphen_values = true)]
        start_x: f64,
        #[arg(long, allow_hyphen_values = true)]
        start_y: f64,
        #[arg(long, allow_hyphen_values = true)]
        end_x: f64,
        #[arg(long, allow_hyphen_values = true)]
        end_y: f64,
    },
    Environment,
}

fn load_layout(path: &std::path::Path) -> Result<crossflow::arrangement::Layout> {
    use std::io::Read;
    // Bound reads before parsing untrusted imported layout files.
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "layout file exceeds 1 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}
fn metadata(mode: &str) -> serde_json::Value {
    json!({
        "mode":mode,"version":env!("CARGO_PKG_VERSION"),
        "os":std::env::consts::OS,"arch":std::env::consts::ARCH,
        "build_revision":env!("CROSSFLOW_BUILD_REVISION"),
        "build_dirty":env!("CROSSFLOW_BUILD_DIRTY"),
        "build_compiler":env!("CROSSFLOW_BUILD_COMPILER"),
        "build_unix_seconds":env!("CROSSFLOW_BUILD_UNIX_SECONDS"),
        "run_started_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|d|d.as_millis()),
        "transport":"Noise_KK_25519_ChaChaPoly_BLAKE2s over TCP_NODELAY",
        "wire_version":crossflow::protocol::WIRE_VERSION,
        "movement_only":true,"visible_metrics":"not measured by software timestamps"
    })
}
fn valid_duration(seconds: f64) -> Result<()> {
    ensure!(
        seconds.is_finite() && seconds > 0.0 && seconds <= 3600.0,
        "seconds must be >0 and <=3600"
    );
    Ok(())
}

#[cfg(windows)]
struct TimerPeriod;
#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}
#[cfg(windows)]
impl TimerPeriod {
    fn start() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}
#[cfg(windows)]
impl Drop for TimerPeriod {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let stop = Arc::new(AtomicBool::new(false));
    // Keep the handler alive until workers, adapters and log writers finish.
    let _shutdown = if matches!(
        &args.command,
        Command::Receive { .. }
            | Command::Send { .. }
            | Command::Capture { .. }
            | Command::DesktopSend { .. }
            | Command::Discover { .. }
    ) {
        Some(crossflow::shutdown::Shutdown::install(stop.clone())?)
    } else {
        None
    };
    #[cfg(windows)]
    let _timer = TimerPeriod::start();
    match args.command {
        Command::DesktopSend {
            host,
            identity,
            peer,
            log,
            seconds,
            max_hz,
        } => {
            let discover_peer = peer.is_some();
            let (host, identity) = if let Some(id) = peer {
                let (device, bundle) =
                    crossflow::peers::PeerStore::default_store()?.resolve(&id)?;
                let endpoint = device.last_endpoint.context(
                    "Saved peer has no input listening endpoint; this record only authorizes incoming input",
                )?;
                (endpoint.to_string(), bundle)
            } else {
                (
                    host.context("Target endpoint required")?,
                    identity.context("Identity required")?,
                )
            };
            let mut meta = metadata("native_client_sender");
            meta["host"] = json!(host);
            meta["desktop_control_protocol"] = json!(1);
            meta["duration_seconds"] = json!(seconds);
            meta["max_hz"] = json!(max_hz);
            meta["peer_route_discovery"] = json!(discover_peer);
            let writer = LogWriter::new(&log, meta)?;
            #[cfg(windows)]
            crossflow::desktop::run(
                crossflow::desktop::Options {
                    host,
                    identity,
                    discover_peer,
                    seconds,
                    max_hz,
                },
                writer,
                stop,
            )?;
            #[cfg(not(windows))]
            {
                let _ = (host, identity, seconds, max_hz, discover_peer);
                writer.finish()?;
                anyhow::bail!("native client sending currently requires Windows");
            }
        }
        Command::InputWorker { gain } => {
            // This private child owns its process: do not print panic payloads
            // (which could contain input) or block cleanup on stderr output.
            std::panic::set_hook(Box::new(|_| {}));
            ensure!(
                gain.is_finite() && gain > 0.0 && gain <= 10.0,
                "invalid worker gain"
            );
            #[cfg(target_os = "macos")]
            crossflow::injection_worker::run_native(gain)?;
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("native input worker requires macOS");
        }
        Command::Pair { out } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crossflow::security::create_pair(&out)?)?
            );
        }
        Command::Peers => {
            let devices = crossflow::peers::PeerStore::default_store()?.list()?;
            println!("{}", serde_json::to_string_pretty(&devices)?);
        }
        Command::PeerImport {
            identity,
            name,
            endpoint,
        } => {
            let device = crossflow::peers::PeerStore::default_store()?
                .remember_existing(&identity, &name, endpoint)?;
            println!("{}", serde_json::to_string_pretty(&device)?);
        }
        Command::Discover { seconds, pairing } => {
            ensure!(
                seconds.is_finite() && seconds > 0.0 && seconds <= 60.0,
                "discovery duration must be >0 and <=60 seconds"
            );
            let config = crossflow::discovery::DiscoveryConfig {
                advertisement: None,
                local_device_id: None,
                compatible_protocol: if pairing {
                    crossflow::discovery::PAIRING_PROTOCOL_VERSION
                } else {
                    crossflow::protocol::WIRE_VERSION.into()
                },
            };
            let mut discovery = if pairing {
                crossflow::discovery::DiscoveryHandle::start_pairing(config)?
            } else {
                crossflow::discovery::DiscoveryHandle::start(config)?
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
            let mut snapshot = None;
            while !stop.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                if let Some(current) = discovery.try_snapshot() {
                    snapshot = Some(current);
                }
                thread::sleep(std::time::Duration::from_millis(100));
            }
            if let Some(current) = discovery.try_snapshot() {
                snapshot = Some(current);
            }
            discovery.stop();
            let stopped = discovery.wait_stopped(std::time::Duration::from_secs(3));
            let snapshot =
                snapshot.ok_or_else(|| anyhow::anyhow!("discovery state unavailable"))?;
            let failed = snapshot.error.is_some();
            println!("{}", serde_json::to_string_pretty(&snapshot)?);
            stopped?;
            ensure!(!failed, "local discovery failed; see reported status");
        }
        Command::Summarize { path } => {
            println!("{}", serde_json::to_string_pretty(&summarize(&path)?)?)
        }
        Command::LayoutCheck { path } => {
            let layout = load_layout(&path)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "valid":true,"screens":layout.screens().len(),
                    "applied_to_session":false,"layout":layout
                }))?
            );
        }
        Command::LayoutTrace {
            path,
            from,
            start_x,
            start_y,
            end_x,
            end_y,
        } => {
            use crossflow::arrangement::Point;
            let layout = load_layout(&path)?;
            let crossing = layout.crossing(
                &from,
                Point {
                    x: start_x,
                    y: start_y,
                },
                Point { x: end_x, y: end_y },
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "crossing":crossing,"system_input_injected":false
                }))?
            );
        }
        Command::Environment => {
            #[allow(unused_mut)]
            let mut info = metadata("environment");
            #[cfg(target_os = "macos")]
            {
                info["platform"] = crossflow::platform::macos::environment();
            }
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        Command::Receive {
            bind,
            identity,
            peer,
            log,
            seconds,
            inject,
            inject_actions,
            gain,
            discoverable,
            device_name,
        } => {
            valid_duration(seconds)?;
            let identity = if let Some(peer) = peer {
                crossflow::peers::PeerStore::default_store()?
                    .resolve(&peer)?
                    .1
            } else {
                identity.context("Receiver requires an authorized identity")?
            };
            ensure!(
                gain.is_finite() && gain > 0.0 && gain <= 10.,
                "gain must be (0,10]"
            );
            let mut meta = metadata(if inject {
                "receiver_inject"
            } else {
                "receiver_dry_run"
            });
            let discovery_options =
                discoverable.then_some(session::ReceiveDiscoveryOptions { device_name });
            meta["discoverable_requested"] = json!(discoverable);
            meta["gain"] = json!(gain);
            meta["bind"] = json!(bind);
            meta["duration_seconds"] = json!(seconds);
            meta["experimental_input_actions"] = json!(inject_actions);
            meta["movement_only"] = json!(inject && !inject_actions);
            meta["input_worker"] = json!(inject && inject_actions);
            meta["input_worker_protocol_version"] = json!(if inject && inject_actions {
                Some(crossflow::worker_protocol::VERSION)
            } else {
                None
            });
            meta["worker_rpc_failure_logging"] = json!(inject && inject_actions);
            meta["input_worker_ack"] = json!("adapter_return_only_not_os_or_display");
            meta["receiver_t3_t4_scope"] = json!(if inject && inject_actions {
                "worker_rpc_including_adapter_not_isolated_native_injection"
            } else {
                "local_adapter_call"
            });
            #[cfg(target_os = "macos")]
            {
                meta["platform"] = crossflow::platform::macos::environment();
            }
            let writer = LogWriter::new(&log, meta)?;
            let result = if inject {
                #[cfg(target_os = "macos")]
                {
                    (|| -> Result<()> {
                        if inject_actions {
                            let mut command = std::process::Command::new(std::env::current_exe()?);
                            command
                                .arg("input-worker")
                                .arg("--gain")
                                .arg(gain.to_string());
                            let mut sink =
                                crossflow::injection_worker::WorkerProxy::spawn(&mut command)?;
                            sink.attach_failure_log(writer.log.clone());
                            let result = session::receive_with_discovery(
                                &bind,
                                &identity,
                                &mut sink,
                                writer.log.clone(),
                                seconds,
                                &stop,
                                discovery_options.as_ref(),
                            );
                            // Discovery failure must not skip the input owner's
                            // explicit cleanup ACK and process-exit attempt.
                            let cleanup = sink.shutdown();
                            if let Err(error) = &cleanup {
                                writer.log.write("error", json!({"stage":"input_worker_shutdown","message":format!("{error:#}"),"receiver_error":result.as_ref().err().map(|e|format!("{e:#}"))}));
                            }
                            return match (result, cleanup) {
                                (Err(error), _) => Err(error),
                                (Ok(()), cleanup) => cleanup,
                            };
                        }
                        struct Native(crossflow::platform::macos::MacMouseSink, bool);
                        impl session::MouseSink for Native {
                            fn reset(&mut self) -> Result<()> {
                                self.0.reset().map_err(|e| anyhow::anyhow!(e.to_string()))
                            }
                            fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
                                self.0
                                    .move_by(x, y)
                                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                            }
                            fn emergency_pressed(&self) -> bool {
                                self.0.emergency_pressed()
                            }
                            fn button(
                                &mut self,
                                button: crossflow::input::Button,
                                down: bool,
                            ) -> Result<()> {
                                if !self.1 {
                                    ensure!(!down, "button injection requires --inject-actions");
                                    return Ok(());
                                }
                                self.0
                                    .button(button, down)
                                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                            }
                            fn key(&mut self, usage: u16, down: bool) -> Result<()> {
                                if !self.1 {
                                    ensure!(!down, "keyboard injection requires --inject-actions");
                                    return Ok(());
                                }
                                self.0
                                    .key(usage, down)
                                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                            }
                            fn scroll(&mut self, horizontal: i32, vertical: i32) -> Result<()> {
                                ensure!(self.1, "scroll injection requires --inject-actions");
                                self.0
                                    .scroll(horizontal, vertical)
                                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                            }
                            fn release_owned(&mut self) -> Result<()> {
                                self.0
                                    .release_owned()
                                    .map_err(|e| anyhow::anyhow!(e.to_string()))
                            }
                        }
                        let mut sink = Native(
                            crossflow::platform::macos::MacMouseSink::new(gain)
                                .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                            inject_actions,
                        );
                        session::receive_with_discovery(
                            &bind,
                            &identity,
                            &mut sink,
                            writer.log.clone(),
                            seconds,
                            &stop,
                            discovery_options.as_ref(),
                        )
                    })()
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Err(anyhow::anyhow!(
                        "--inject requires macOS; omit it for a dry-run receiver"
                    ))
                }
            } else {
                let mut sink = session::DrySink::default();
                let r = session::receive_with_discovery(
                    &bind,
                    &identity,
                    &mut sink,
                    writer.log.clone(),
                    seconds,
                    &stop,
                    discovery_options.as_ref(),
                );
                eprintln!(
                    "Dry run: {} movements, final cumulative ({:.2}, {:.2})",
                    sink.moves, sink.x, sink.y
                );
                r
            };
            if let Err(error) = &result {
                writer.log.write(
                    "error",
                    json!({"stage":"receiver_exit","message":format!("{error:#}")}),
                );
            }
            writer.finish()?;
            result?;
        }
        Command::Send {
            host,
            identity,
            log,
            seconds,
            max_hz,
            synthetic,
            experimental_inputs,
        } => {
            valid_duration(seconds)?;
            let mut meta = metadata(if synthetic {
                "synthetic_sender"
            } else {
                "raw_input_sender"
            });
            meta["max_hz"] = json!(max_hz);
            meta["host"] = json!(host);
            meta["duration_seconds"] = json!(seconds);
            meta["experimental_input_actions"] = json!(experimental_inputs);
            meta["movement_only"] = json!(!experimental_inputs);
            let writer = LogWriter::new(&log, meta)?;
            if experimental_inputs {
                let result =
                    run_ordered_sender(host, identity, seconds, max_hz, writer.log.clone(), stop);
                writer.finish()?;
                result?;
            } else {
                let motion = Arc::new(Mutex::new(Motion::default()));
                let m = motion.clone();
                let s = stop.clone();
                let l = writer.log.clone();
                let worker = thread::spawn(move || {
                    let result = session::send(
                        session::SendOptions {
                            host: &host,
                            bundle: &identity,
                            seconds,
                            max_hz,
                            synthetic,
                        },
                        m,
                        s.clone(),
                        l,
                    );
                    s.store(true, Ordering::Relaxed);
                    result
                });
                let capture_result = if !synthetic {
                    run_capture(motion, stop.clone(), writer.log.clone(), seconds, false)
                } else {
                    Ok(())
                };
                if capture_result.is_err() {
                    stop.store(true, Ordering::Relaxed)
                }
                let result = worker
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("sender thread panicked")));
                if let Err(error) = &result {
                    writer.log.write(
                        "error",
                        json!({"stage":"sender_exit","message":format!("{error:#}")}),
                    );
                }
                writer.finish()?;
                capture_result?;
                result?;
            }
        }
        Command::Capture { log, seconds } => {
            valid_duration(seconds)?;
            let writer = LogWriter::new(&log, metadata("local_capture_only"))?;
            let result = run_capture(
                Arc::new(Mutex::new(Motion::default())),
                stop,
                writer.log.clone(),
                seconds,
                true,
            );
            writer.finish()?;
            result?;
        }
    }
    Ok(())
}

fn run_ordered_sender(
    host: String,
    identity: PathBuf,
    seconds: f64,
    max_hz: u32,
    log: crossflow::metrics::Log,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    #[cfg(windows)]
    {
        use crossflow::input::{CaptureOutcome, SourceInputQueue};
        use crossflow::platform::windows::{
            CaptureEvent, CaptureStatus, run_input_capture_with_status,
        };
        eprintln!(
            "Experimental keyboard/buttons/wheel enabled. Windows local input is NOT suppressed; start with a DrySink receiver. Sharing remains paused until you explicitly enable it."
        );
        let input = Arc::new(Mutex::new(SourceInputQueue::new()));
        let network_input = input.clone();
        let network_stop = stop.clone();
        let network_log = log.clone();
        let worker = thread::spawn(move || {
            let result = session::send_ordered(
                session::SendOptions {
                    host: &host,
                    bundle: &identity,
                    seconds,
                    max_hz,
                    synthetic: false,
                },
                network_input,
                network_stop.clone(),
                network_log,
            );
            network_stop.store(true, Ordering::Release);
            result
        });
        let event_input = input.clone();
        let event_stop = stop.clone();
        let state_input = input.clone();
        let result = run_input_capture_with_status(
            move |event| {
                let mut source = event_input.lock().unwrap();
                let outcome = match event {
                    CaptureEvent::Delta {
                        dx,
                        dy,
                        captured_at,
                    } => {
                        let t0 = log.at(captured_at);
                        log.write("capture", json!({"t0_ns":t0}));
                        source.move_by(dx as f64, dy as f64, t0)
                    }
                    CaptureEvent::Button {
                        button,
                        down,
                        captured_at,
                    } => source.button(button, down, log.at(captured_at)),
                    CaptureEvent::Key {
                        usage,
                        down,
                        captured_at,
                    } => source.key(usage, down, log.at(captured_at)),
                    CaptureEvent::Scroll {
                        horizontal,
                        vertical,
                        captured_at,
                    } => source.scroll(horizontal, vertical, log.at(captured_at)),
                    CaptureEvent::Snapshot { buttons, keys } => {
                        if source.seed_physical_held(buttons, keys).is_err() {
                            source.capture_reset();
                            log.write("error", json!({"stage":"physical_snapshot","message":"Could not initialize paused physical input state"}));
                            event_stop.store(true, Ordering::Release);
                        }
                        CaptureOutcome::Ignored
                    }
                    CaptureEvent::Toggle => {
                        if source.is_active() {
                            source.pause();
                        } else {
                            source.arm();
                        }
                        log.write("toggle", json!({"active":source.is_active()}));
                        CaptureOutcome::Ignored
                    }
                    CaptureEvent::Stop => {
                        source.pause();
                        event_stop.store(true, Ordering::Release);
                        CaptureOutcome::Ignored
                    }
                };
                if let CaptureOutcome::Paused(reason) = outcome {
                    log.write(
                        "input_queue_paused",
                        json!({"reason":format!("{reason:?}")}),
                    );
                }
            },
            move || {
                let source = state_input.lock().unwrap();
                if !source.is_connected() {
                    CaptureStatus::Disconnected
                } else if source.is_active() {
                    CaptureStatus::Active
                } else {
                    CaptureStatus::Ready
                }
            },
            stop.clone(),
            Some(std::time::Duration::from_secs_f64(seconds)),
        );
        stop.store(true, Ordering::Release);
        input
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .capture_reset();
        let network = worker
            .join()
            .map_err(|_| anyhow::anyhow!("ordered sender thread panicked"))?;
        result.map_err(|e| anyhow::anyhow!(e))?;
        network
    }
    #[cfg(not(windows))]
    {
        let _ = (host, identity, seconds, max_hz, log, stop);
        anyhow::bail!("experimental physical input capture currently requires Windows")
    }
}

fn run_capture(
    motion: Arc<Mutex<Motion>>,
    stop: Arc<AtomicBool>,
    log: crossflow::metrics::Log,
    seconds: f64,
    capture_only: bool,
) -> Result<()> {
    #[cfg(windows)]
    {
        use crossflow::platform::windows::{CaptureEvent, CaptureStatus, run_capture_with_status};
        let callback_stop = stop.clone();
        let status_motion = motion.clone();
        let stats = run_capture_with_status(
            move |event| match event {
                CaptureEvent::Delta {
                    dx,
                    dy,
                    captured_at,
                } => {
                    let t0 = log.at(captured_at);
                    log.write("capture", json!({"t0_ns":t0}));
                    if !capture_only {
                        motion.lock().unwrap().add(dx as f64, dy as f64, t0);
                    }
                }
                CaptureEvent::Toggle => {
                    if !capture_only {
                        let active = motion.lock().unwrap().toggle();
                        eprintln!(
                            "Remote movement {}",
                            if active { "ACTIVE" } else { "PAUSED" }
                        );
                        log.write("toggle", json!({"active":active}));
                    }
                }
                CaptureEvent::Stop => {
                    callback_stop.store(true, Ordering::Relaxed);
                }
                // Expanded capture is opt-in and is not registered by this path.
                _ => {}
            },
            move || {
                if capture_only {
                    return CaptureStatus::CaptureOnly;
                }
                let state = status_motion.lock().unwrap();
                if !state.connected {
                    CaptureStatus::Disconnected
                } else if state.active {
                    CaptureStatus::Active
                } else {
                    CaptureStatus::Ready
                }
            },
            stop,
            Some(std::time::Duration::from_secs_f64(seconds)),
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        eprintln!("Capture: {stats:?}");
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (motion, stop, log, seconds, capture_only);
        anyhow::bail!(
            "physical capture currently requires Windows; --synthetic is available for diagnostics"
        )
    }
}

#[cfg(test)]
mod discovery_cli_tests {
    use super::*;

    #[test]
    fn receiver_peer_mode_requires_one_identity_source_and_never_enables_injection() {
        let args = Args::try_parse_from([
            "crossflow",
            "receive",
            "--peer",
            &"a".repeat(64),
            "--log",
            "test.jsonl",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Command::Receive {
                identity: None,
                peer: Some(_),
                inject: false,
                ..
            }
        ));
        assert!(Args::try_parse_from(["crossflow", "receive", "--log", "test.jsonl"]).is_err());
        assert!(
            Args::try_parse_from([
                "crossflow",
                "receive",
                "--peer",
                &"a".repeat(64),
                "--identity",
                "legacy",
                "--log",
                "test.jsonl"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "crossflow",
                "receive",
                "--identity",
                "legacy",
                "--log",
                "test.jsonl"
            ])
            .is_ok()
        );
    }

    #[test]
    fn pairing_discovery_is_explicit_and_input_remains_default() {
        assert!(matches!(
            Args::try_parse_from(["crossflow", "discover"])
                .unwrap()
                .command,
            Command::Discover { pairing: false, .. }
        ));
        assert!(matches!(
            Args::try_parse_from(["crossflow", "discover", "--pairing"])
                .unwrap()
                .command,
            Command::Discover { pairing: true, .. }
        ));
    }

    #[test]
    fn desktop_peer_mode_excludes_manual_identity_and_host() {
        let base = [
            "crossflow",
            "desktop-send",
            "--peer",
            "0011",
            "--log",
            "run.jsonl",
        ];
        assert!(Args::try_parse_from(base).is_ok());
        for extra in [["--host", "127.0.0.1:24800"], ["--identity", "identity"]] {
            let mut args = base.to_vec();
            args.extend(extra);
            assert!(Args::try_parse_from(args).is_err());
        }
        assert!(Args::try_parse_from(["crossflow", "desktop-send", "--log", "run.jsonl"]).is_err());
        assert!(
            Args::try_parse_from([
                "crossflow",
                "desktop-send",
                "--host",
                "127.0.0.1:24800",
                "--log",
                "run.jsonl"
            ])
            .is_err()
        );
        assert!(
            Args::try_parse_from([
                "crossflow",
                "desktop-send",
                "--host",
                "127.0.0.1:24800",
                "--identity",
                "identity",
                "--log",
                "run.jsonl"
            ])
            .is_ok()
        );
    }

    #[test]
    fn discovery_is_explicit_and_name_cannot_enable_it_implicitly() {
        let base = [
            "crossflow",
            "receive",
            "--identity",
            "identity",
            "--log",
            "run.jsonl",
        ];
        let parsed = Args::try_parse_from(base).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Receive {
                discoverable: false,
                device_name: None,
                ..
            }
        ));
        let mut named = base.to_vec();
        named.extend(["--device-name", "Public device"]);
        assert!(Args::try_parse_from(&named).is_err());
        named.push("--discoverable");
        let parsed = Args::try_parse_from(named).unwrap();
        assert!(
            matches!(parsed.command, Command::Receive { discoverable: true, device_name: Some(name), .. } if name == "Public device")
        );
    }
}
