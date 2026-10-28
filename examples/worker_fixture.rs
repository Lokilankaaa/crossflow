//! Isolated fake-input process fixture; never imports a native input adapter.
//! Build explicitly with `cargo build --example worker_fixture --locked`.
use anyhow::{Context, Result, bail, ensure};
use crossflow::{
    injection_worker::{WorkerProxy, run_worker, stdio_files},
    input::Button,
    metrics::LogWriter,
    session::MouseSink,
    worker_protocol::FRAME_LEN,
};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

fn append(root: &Path, event: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("events"))?;
    writeln!(file, "{event}")?;
    file.flush()?;
    Ok(())
}

struct RecordingSink(PathBuf);
struct FailAfterBytes<W> {
    writer: W,
    remaining: usize,
}
impl<W: Write> Write for FailAfterBytes<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "fixture ACK failure",
            ));
        }
        let length = bytes.len().min(self.remaining);
        let written = self.writer.write(&bytes[..length])?;
        self.remaining -= written;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}
impl MouseSink for RecordingSink {
    fn reset(&mut self) -> Result<()> {
        append(&self.0, "reset")
    }
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        append(&self.0, "move")?;
        Ok((x, y))
    }
    fn key(&mut self, _: u16, down: bool) -> Result<()> {
        append(&self.0, if down { "key-down" } else { "key-up" })
    }
    fn button(&mut self, _: Button, down: bool) -> Result<()> {
        append(&self.0, if down { "button-down" } else { "button-up" })
    }
    fn scroll(&mut self, _: i32, _: i32) -> Result<()> {
        append(&self.0, "scroll")
    }
    // Intentionally does not synthesize Up: the worker must track ownership and
    // explicitly release each held key/button through the adapter methods.
    fn release_owned(&mut self) -> Result<()> {
        append(&self.0, "adapter-release-owned")
    }
}

fn run(mode: &str, root: &Path, behavior: &str) -> Result<()> {
    ensure!(root.is_dir(), "fixture directory must already exist");
    let watchdog_root = root.to_owned();
    let watchdog_mode = mode.to_owned();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(12));
        let _ = fs::write(
            watchdog_root.join(format!("{watchdog_mode}-watchdog")),
            b"timeout",
        );
        eprintln!("fake worker fixture exceeded its safety deadline");
        std::process::exit(90);
    });
    match mode {
        "worker" => {
            // A caught adapter panic must not disclose its payload through the
            // default hook before run_worker converts it to a generic error.
            std::panic::set_hook(Box::new(|_| {}));
            let (reader, writer) = stdio_files()?;
            let writer: Box<dyn Write + Send> = if behavior == "fail-ack" {
                // Ready + Reset + key Down + button Down are acknowledged.
                // Fail the subsequent Move ACK after the fake adapter returned.
                Box::new(FailAfterBytes {
                    writer,
                    remaining: 4 * FRAME_LEN,
                })
            } else {
                Box::new(writer)
            };
            let result = run_worker(reader, writer, &mut RecordingSink(root.to_owned()));
            // Recording exit only after run_worker has attempted its cleanup.
            fs::write(
                root.join("worker-exit"),
                if result.is_ok() { "ok" } else { "error" },
            )?;
            result
        }
        "parent" => {
            let mut command = Command::new(std::env::current_exe()?);
            command.arg("worker").arg(root);
            if behavior == "ack-failure" {
                command.arg("fail-ack");
            }
            let mut proxy = WorkerProxy::spawn(&mut command)?;
            let writer = LogWriter::new(
                &root.join("worker-rpc.jsonl"),
                serde_json::json!({"worker_rpc_failure_logging":true}),
            )?;
            proxy.attach_failure_log(writer.log.clone());
            fs::write(root.join("worker-pid"), proxy.pid().to_string())?;
            // Let the outer test acquire a read-only OS process handle before
            // any action can trigger a rapid terminal worker exit.
            while !root.join("allow-parent-input").exists() {
                thread::sleep(Duration::from_millis(2));
            }
            proxy.reset()?;
            proxy.key(4, true)?;
            let key_timing = proxy.take_timing().context("key timing missing")?;
            ensure!(proxy.take_timing().is_none(), "timing was not consumed");
            proxy.button(Button::Left, true)?;
            let button_timing = proxy.take_timing().context("button timing missing")?;
            ensure!(
                key_timing.request_seq + 1 == button_timing.request_seq,
                "timing belongs to another RPC"
            );
            fs::write(
                root.join("timings.json"),
                serde_json::to_vec(&[key_timing, button_timing])?,
            )?;
            fs::write(root.join("parent-ready"), b"held-pair-acknowledged")?;
            match behavior {
                "ack-failure" => {
                    ensure!(
                        proxy.move_by(1.0, 1.0).is_err(),
                        "fixture ACK failure was not terminal"
                    );
                    ensure!(
                        proxy.take_timing().is_none(),
                        "failed RPC exposed old action timing"
                    );
                    ensure!(
                        proxy.move_by(1.0, 1.0).is_err(),
                        "terminal RPC was replayed"
                    );
                    fs::write(root.join("parent-ack-error"), b"terminal")?;
                }
                "hold" => {
                    // The process remains alive, but the receiving execution
                    // loop makes no progress and must not renew the worker lease.
                    thread::sleep(Duration::from_secs(9));
                }
                "healthy" => {
                    while !root.join("stop-parent").exists() {
                        proxy.maintain()?;
                        thread::sleep(Duration::from_millis(5));
                    }
                    proxy.key(4, false)?;
                    proxy.button(Button::Left, false)?;
                    proxy.shutdown()?;
                    ensure!(
                        proxy.take_timing().is_none(),
                        "control RPC exposed old action timing"
                    );
                }
                _ => bail!("unknown fixture parent behavior"),
            }
            drop(proxy);
            writer.finish()?;
            fs::write(root.join("parent-exit"), b"ok")?;
            Ok(())
        }
        _ => bail!("unknown fixture mode"),
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let result = (|| {
        let mode = args
            .get(1)
            .and_then(|s| s.to_str())
            .context("fixture mode missing")?;
        let root = PathBuf::from(args.get(2).context("fixture directory missing")?);
        let behavior = args.get(3).and_then(|s| s.to_str()).unwrap_or("healthy");
        run(mode, &root, behavior)
    })();
    if let Err(error) = &result {
        eprintln!("fake worker fixture: {error:#}");
    }
    // run_worker may leave a pipe reader blocked until process teardown. Do not
    // wait for detached IO or the safety-watchdog threads on either exit path.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}
