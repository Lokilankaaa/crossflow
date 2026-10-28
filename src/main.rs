use anyhow::{Result, ensure};
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
    about = "Crossflow Rust phase 0 — movement-only measurement prototype"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create paired static identities. Share only the mac bundle with the Mac.
    Pair {
        #[arg(long, default_value = ".pairing")]
        out: PathBuf,
    },
    /// Receive encrypted movement. Default is a sink without OS input injection.
    Receive {
        #[arg(long, default_value = "127.0.0.1:24800")]
        bind: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 60.0)]
        seconds: f64,
        #[arg(long)]
        inject: bool,
        #[arg(long, default_value_t = 1.0)]
        gain: f64,
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
    #[cfg(windows)]
    let _timer = TimerPeriod::start();
    match args.command {
        Command::Pair { out } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crossflow::security::create_pair(&out)?)?
            );
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
            log,
            seconds,
            inject,
            gain,
        } => {
            valid_duration(seconds)?;
            ensure!(
                gain.is_finite() && gain > 0.0 && gain <= 10.,
                "gain must be (0,10]"
            );
            let mut meta = metadata(if inject {
                "receiver_inject"
            } else {
                "receiver_dry_run"
            });
            meta["gain"] = json!(gain);
            meta["bind"] = json!(bind);
            meta["duration_seconds"] = json!(seconds);
            #[cfg(target_os = "macos")]
            {
                meta["platform"] = crossflow::platform::macos::environment();
            }
            let writer = LogWriter::new(&log, meta)?;
            let result = if inject {
                #[cfg(target_os = "macos")]
                {
                    struct Native(crossflow::platform::macos::MacMouseSink);
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
                    }
                    let mut sink = Native(
                        crossflow::platform::macos::MacMouseSink::new(gain)
                            .map_err(|e| anyhow::anyhow!(e.to_string()))?,
                    );
                    session::receive(&bind, &identity, &mut sink, writer.log.clone(), seconds)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    Err(anyhow::anyhow!(
                        "--inject requires macOS; omit it for a dry-run receiver"
                    ))
                }
            } else {
                let mut sink = session::DrySink::default();
                let r = session::receive(&bind, &identity, &mut sink, writer.log.clone(), seconds);
                eprintln!(
                    "Dry run: {} movements, final cumulative ({:.2}, {:.2})",
                    sink.moves, sink.x, sink.y
                );
                r
            };
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
            let writer = LogWriter::new(&log, meta)?;
            let motion = Arc::new(Mutex::new(Motion::default()));
            let stop = Arc::new(AtomicBool::new(false));
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
                .map_err(|_| anyhow::anyhow!("sender thread panicked"))?;
            writer.finish()?;
            capture_result?;
            result?;
        }
        Command::Capture { log, seconds } => {
            valid_duration(seconds)?;
            let writer = LogWriter::new(&log, metadata("local_capture_only"))?;
            let result = run_capture(
                Arc::new(Mutex::new(Motion::default())),
                Arc::new(AtomicBool::new(false)),
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
