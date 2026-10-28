use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

enum Message {
    Event(Value),
    Close,
}
#[derive(Clone)]
pub struct Log {
    tx: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
    closed: Arc<Mutex<bool>>,
    clock: Instant,
}
pub struct LogWriter {
    pub log: Log,
    worker: Option<JoinHandle<Result<()>>>,
}
impl LogWriter {
    pub fn new(path: &Path, metadata: Value) -> Result<Self> {
        if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(p)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .context("log exists or is unwritable")?;
        let (tx, rx) = sync_channel::<Message>(8192);
        let dropped = Arc::new(AtomicU64::new(0));
        let count = dropped.clone();
        let worker = thread::spawn(move || write_log(BufWriter::new(file), rx, &count, metadata));
        Ok(Self {
            log: Log {
                tx,
                dropped,
                closed: Arc::new(Mutex::new(false)),
                clock: Instant::now(),
            },
            worker: Some(worker),
        })
    }
    /// Stop accepting new events, drain all accepted events, and surface every
    /// metadata/event/trailer/flush error. Clones remain valid but become no-ops.
    pub fn finish(mut self) -> Result<()> {
        // Serialize the close marker with try_send so a concurrent producer
        // cannot enqueue an accepted event behind the terminal summary.
        *self.log.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        let sent = self.log.tx.send(Message::Close);
        if let Some(w) = self.worker.take() {
            w.join()
                .map_err(|_| anyhow::anyhow!("Performance log writer panicked"))??;
        }
        sent.context("Performance log writer stopped before finalization")?;
        Ok(())
    }
}

fn write_log(
    mut output: impl Write,
    events: Receiver<Message>,
    dropped: &AtomicU64,
    metadata: Value,
) -> Result<()> {
    writeln!(output, "{}", json!({"event":"metadata","data":metadata}))
        .context("Cannot write performance log metadata")?;
    for event in events {
        match event {
            Message::Event(value) => {
                writeln!(output, "{value}").context("Cannot write performance log event")?;
            }
            Message::Close => {
                writeln!(
                    output,
                    "{}",
                    json!({"event":"log_summary","dropped_records":dropped.load(Ordering::Relaxed)})
                )
                .context("Cannot write performance log trailer")?;
                output.flush().context("Cannot flush performance log")?;
                return Ok(());
            }
        }
    }
    bail!("Performance log was not explicitly finalized")
}
impl Log {
    pub fn ns(&self) -> u64 {
        self.clock.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }
    pub fn at(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.clock).as_nanos() as u64
    }
    pub fn write(&self, event: &str, mut data: Value) {
        let closed = self.closed.lock().unwrap_or_else(|e| e.into_inner());
        if *closed {
            return;
        }
        if let Some(o) = data.as_object_mut() {
            o.insert("event".into(), event.into());
            o.insert("local_ns".into(), self.ns().into());
        }
        if self.tx.try_send(Message::Event(data)).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn distribution(mut v: Vec<f64>) -> Value {
    if v.is_empty() {
        return Value::Null;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let p = |q: f64| {
        v[((v.len() as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(v.len() - 1)]
    };
    json!({"samples":v.len(),"p50_ms":p(0.5),"p95_ms":p(0.95),"p99_ms":p(0.99),"max_ms":v[v.len()-1]})
}
pub fn summarize(path: &Path) -> Result<Value> {
    let mut src = vec![];
    let mut recv = vec![];
    let mut inject = vec![];
    let mut action_source = vec![];
    let mut action_receive = vec![];
    let mut action_inject = vec![];
    let mut transport = vec![];
    let mut adapter_scopes = std::collections::BTreeSet::new();
    let mut queued_moves = 0u64;
    let mut cadence_raw = vec![];
    let mut cadence_bursts = vec![];
    let mut last = None;
    let mut events = 0;
    let mut rejected = 0;
    let mut dropped = 0;
    let mut malformed = 0;
    let mut last_is_terminal = false;
    let mut terminal_summaries = 0;
    let mut unterminated = 0;
    let mut long_gaps = 0;
    let mut issues = TimingIssues::default();
    let mut last_local_ns = None;
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? != 0 {
        last_is_terminal = false;
        let newline_terminated = line.ends_with(b"\n");
        if !newline_terminated {
            unterminated += 1;
        }
        let parsed = serde_json::from_slice::<Value>(&line);
        line.clear();
        let v = match parsed {
            Ok(v) if v.is_object() && v["event"].is_string() => v,
            Err(_) => {
                malformed += 1;
                continue;
            }
            _ => {
                malformed += 1;
                continue;
            }
        };
        events += 1;
        if v["event"] == "metadata" {
            adapter_scopes.insert(match v["data"]["receiver_t3_t4_scope"].as_str() {
                Some("local_adapter_call") => "local_adapter_call",
                Some("worker_rpc_including_adapter_not_isolated_native_injection") => {
                    "worker_rpc_including_adapter_not_isolated_native_injection"
                }
                _ => "unspecified",
            });
        }
        if v.get("local_ns").is_some() {
            if let Some(now) = timestamp(&v, "local_ns", &mut issues) {
                if last_local_ns.is_some_and(|last| now < last) {
                    issues.nonmonotonic_local_timestamps += 1;
                } else {
                    last_local_ns = Some(now);
                }
            }
        }
        if v["event"] == "sent" {
            queued_moves += 1;
            if let Some(d) = timing_delta(&v, "t0_ns", "t1_ns", &mut issues) {
                src.push(d)
            }
        }
        if v["event"] == "transport_summary" {
            transport.push(v.clone());
        }
        if v["event"] == "input_queued" {
            if let Some(d) = timing_delta(&v, "t0_ns", "t1_ns", &mut issues) {
                action_source.push(d);
            }
        }
        if v["event"] == "input_applied" {
            if let Some(d) = timing_delta(&v, "t2_ns", "t3_ns", &mut issues) {
                action_receive.push(d);
            }
            if let Some(d) = timing_delta(&v, "t3_ns", "t4_ns", &mut issues) {
                action_inject.push(d);
            }
        }
        if v["event"] == "received" {
            if let Some(d) = timing_delta(&v, "t2_ns", "t3_ns", &mut issues) {
                recv.push(d)
            }
            if let Some(d) = timing_delta(&v, "t3_ns", "t4_ns", &mut issues) {
                inject.push(d)
            }
        }
        if is_capture_boundary(&v) {
            last = None;
        }
        if v["event"] == "capture" {
            if let Some(t) = timestamp(&v, "t0_ns", &mut issues) {
                if let Some(previous) = last {
                    if t >= previous {
                        let gap = (t - previous) as f64 / 1e6;
                        cadence_raw.push(gap);
                        if gap <= 50.0 {
                            cadence_bursts.push(gap)
                        } else {
                            long_gaps += 1;
                        }
                    } else {
                        issues.nonmonotonic_capture_timestamps += 1;
                        continue;
                    }
                }
                last = Some(t)
            }
        }
        if v["event"] == "error" {
            rejected += 1
        }
        if v["event"] == "log_summary" {
            terminal_summaries += 1;
            if let Some(d) = v["dropped_records"].as_u64() {
                dropped = d;
                last_is_terminal = newline_terminated;
            } else {
                malformed += 1;
            }
        }
    }
    let complete =
        last_is_terminal && terminal_summaries == 1 && malformed == 0 && unterminated == 0;
    Ok(json!({
        "events":events,"errors":rejected,"malformed_lines":malformed,
        "unterminated_records":unterminated,"terminal_summaries":terminal_summaries,
        "complete_log":complete,"dropped_log_records":if complete{json!(dropped)}else{Value::Null},
        "capture_interval_all_in_session":distribution(cadence_raw),
        "capture_interval_within_motion_bursts":distribution(cadence_bursts),
        "capture_gap_over_50ms_count":long_gaps,
        "capture_idle_gaps_over_50ms":long_gaps,
        "capture_interval_filter":{
            "motion_burst_max_gap_ms":50.0,
            "raw_includes_long_gaps":true,
            "heartbeat_resets_cadence":false,
            "warning":"Cannot distinguish voluntary idle from input stalls without independent motion ground truth. The legacy capture_idle_gaps_over_50ms field does not establish that these gaps were idle."
        },
        "timing_issues":issues,
        "queued_move_records":queued_moves,
        "transport_attempts":transport,
        "send_evidence":"sent records mark local queueing, not socket completion or receiver acknowledgement. Missing transport summaries mean socket completion is unknown; socket completion never proves remote application.",
        "source_processing":distribution(src),"receiver_processing":distribution(recv),
        "action_source_processing":distribution(action_source),
        "action_receiver_processing":distribution(action_receive),
        "action_adapter_call":distribution(action_inject),
        "receiver_adapter_timing_scopes":adapter_scopes,
        "adapter_timing_warning":"injection_call and action_adapter_call measure t3..t4. With an input worker this includes pipe scheduling and ACK wait, not isolated native injection. Missing or mixed scopes cannot establish isolated injection timing.",
        "injection_call":distribution(inject),"visible_smoothness":"NOT_MEASURED",
        "one_way_network_latency":"NOT_MEASURED: clocks are not synchronized"
    }))
}

#[derive(Default, serde::Serialize)]
struct TimingIssues {
    missing_values: u64,
    malformed_values: u64,
    invalid_order: u64,
    nonmonotonic_capture_timestamps: u64,
    nonmonotonic_local_timestamps: u64,
}

fn timestamp(value: &Value, name: &str, issues: &mut TimingIssues) -> Option<u64> {
    match value.get(name) {
        None => {
            issues.missing_values += 1;
            None
        }
        Some(v) => match v.as_u64() {
            Some(t) => Some(t),
            None => {
                issues.malformed_values += 1;
                None
            }
        },
    }
}

fn timing_delta(value: &Value, start: &str, end: &str, issues: &mut TimingIssues) -> Option<f64> {
    let a = timestamp(value, start, issues);
    let b = timestamp(value, end, issues);
    match (a, b) {
        (Some(a), Some(b)) if b >= a => Some((b - a) as f64 / 1e6),
        (Some(_), Some(_)) => {
            issues.invalid_order += 1;
            None
        }
        _ => None,
    }
}

fn is_capture_boundary(record: &Value) -> bool {
    matches!(
        record["event"].as_str(),
        Some("connected" | "disconnected" | "toggle" | "error" | "emergency_stop")
    ) || (record["event"] == "control"
        && matches!(
            record["kind"].as_str(),
            Some("Arm" | "Pause" | "Stop" | "arm" | "pause" | "stop")
        ))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn summarize_text(text: &str) -> Value {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "crossflow-summary-{}-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
        drop(file);
        let result = summarize(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn action_timings_stay_separate_from_cursor_movement() {
        let result = summarize_text(concat!(
            "{\"event\":\"input_queued\",\"t0_ns\":100,\"t1_ns\":200}\n",
            "{\"event\":\"input_applied\",\"t2_ns\":10,\"t3_ns\":30,\"t4_ns\":40}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n"
        ));
        assert_eq!(result["action_source_processing"]["samples"], 1);
        assert_eq!(result["action_receiver_processing"]["samples"], 1);
        assert_eq!(result["action_adapter_call"]["samples"], 1);
        assert!(result["source_processing"].is_null());
        assert!(result["injection_call"].is_null());
        assert_eq!(result["queued_move_records"], 0);
    }

    #[test]
    fn worker_timing_scope_survives_summary_without_claiming_native_latency() {
        let result = summarize_text(concat!(
            "{\"event\":\"metadata\",\"data\":{\"receiver_t3_t4_scope\":\"worker_rpc_including_adapter_not_isolated_native_injection\"}}\n",
            "{\"event\":\"received\",\"t2_ns\":10,\"t3_ns\":30,\"t4_ns\":40}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n"
        ));
        assert_eq!(
            result["receiver_adapter_timing_scopes"],
            json!(["worker_rpc_including_adapter_not_isolated_native_injection"])
        );
        assert_eq!(result["injection_call"]["samples"], 1);
        assert_eq!(result["visible_smoothness"], "NOT_MEASURED");
    }

    #[test]
    fn legacy_sent_records_do_not_claim_network_delivery() {
        let result = summarize_text(concat!(
            "{\"event\":\"sent\",\"t0_ns\":1,\"t1_ns\":2}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n"
        ));
        assert_eq!(result["queued_move_records"], 1);
        assert_eq!(result["transport_attempts"], json!([]));
        assert_eq!(result["source_processing"]["samples"], 1);
        assert!(
            result["send_evidence"]
                .as_str()
                .unwrap()
                .contains("unknown")
        );
    }

    #[test]
    fn raw_capture_intervals_preserve_long_gaps() {
        let result = summarize_text(concat!(
            "{\"event\":\"capture\",\"t0_ns\":1000000}\n",
            "{\"event\":\"capture\",\"t0_ns\":81000000}\n",
            "{\"event\":\"capture\",\"t0_ns\":82000000}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n"
        ));
        assert_eq!(result["capture_interval_all_in_session"]["samples"], 2);
        assert_eq!(result["capture_interval_all_in_session"]["max_ms"], 80.0);
        assert_eq!(
            result["capture_interval_within_motion_bursts"]["samples"],
            1
        );
        assert_eq!(
            result["capture_interval_within_motion_bursts"]["max_ms"],
            1.0
        );
        assert_eq!(result["capture_gap_over_50ms_count"], 1);
        assert_eq!(result["visible_smoothness"], "NOT_MEASURED");
    }

    #[test]
    fn heartbeat_does_not_reset_capture_cadence() {
        let result = summarize_text(concat!(
            "{\"event\":\"capture\",\"t0_ns\":1}\n",
            "{\"event\":\"control\",\"kind\":\"Heartbeat\"}\n",
            "{\"event\":\"capture\",\"t0_ns\":1000001}\n",
            "{\"event\":\"control\",\"kind\":\"Pause\"}\n",
            "{\"event\":\"capture\",\"t0_ns\":90000001}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n"
        ));
        assert_eq!(result["capture_interval_all_in_session"]["samples"], 1);
        assert_eq!(result["capture_interval_all_in_session"]["max_ms"], 1.0);
    }

    #[test]
    fn corruption_after_trailer_is_incomplete() {
        let result = summarize_text("{\"event\":\"log_summary\",\"dropped_records\":0}\n{corrupt");
        assert_eq!(result["complete_log"], false);
        assert_eq!(result["malformed_lines"], 1);
        assert_eq!(result["unterminated_records"], 1);
        assert!(result["dropped_log_records"].is_null());
    }

    #[test]
    fn only_a_single_valid_newline_terminated_final_trailer_is_complete() {
        for text in [
            "{\"event\":\"log_summary\",\"dropped_records\":0}",
            "{\"event\":\"log_summary\"}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":-1}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n{\"event\":\"capture\",\"t0_ns\":1}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n{\"event\":\"log_summary\",\"dropped_records\":0}\n",
            "not-json\n{\"event\":\"log_summary\",\"dropped_records\":0}\n",
        ] {
            assert_eq!(
                summarize_text(text)["complete_log"],
                false,
                "Accepted incomplete log: {text}"
            );
        }
    }

    #[test]
    fn invalid_order_and_nonmonotonic_timing_are_counted() {
        let result = summarize_text(concat!(
            "{\"event\":\"sent\",\"t0_ns\":20,\"t1_ns\":10}\n",
            "{\"event\":\"received\",\"t2_ns\":100,\"t3_ns\":99,\"t4_ns\":98}\n",
            "{\"event\":\"capture\",\"t0_ns\":10,\"local_ns\":20}\n",
            "{\"event\":\"capture\",\"t0_ns\":9,\"local_ns\":19}\n",
            "{\"event\":\"capture\",\"t0_ns\":\"bad\"}\n",
            "{\"event\":\"sent\",\"t1_ns\":10}\n",
            "{\"event\":\"log_summary\",\"dropped_records\":0}\n"
        ));
        assert_eq!(result["timing_issues"]["invalid_order"], 3);
        assert_eq!(
            result["timing_issues"]["nonmonotonic_capture_timestamps"],
            1
        );
        assert_eq!(result["timing_issues"]["nonmonotonic_local_timestamps"], 1);
        assert_eq!(result["timing_issues"]["malformed_values"], 1);
        assert_eq!(result["timing_issues"]["missing_values"], 1);
        assert!(result["source_processing"].is_null());
        assert!(result["receiver_processing"].is_null());
        assert!(result["injection_call"].is_null());
    }

    #[test]
    fn metadata_and_flush_write_errors_are_reported() {
        struct FailingOutput {
            fail_write: bool,
        }
        impl Write for FailingOutput {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                if self.fail_write {
                    Err(std::io::Error::other("test write failure"))
                } else {
                    Ok(buffer.len())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("test flush failure"))
            }
        }
        for fail_write in [true, false] {
            let (tx, rx) = sync_channel(1);
            tx.send(Message::Close).unwrap();
            let error = write_log(
                FailingOutput { fail_write },
                rx,
                &AtomicU64::new(0),
                json!({}),
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(if fail_write { "metadata" } else { "flush" })
            );
        }
    }

    #[test]
    fn finish_surfaces_worker_failure() {
        let (tx, rx) = sync_channel(1);
        drop(rx);
        let writer = LogWriter {
            log: Log {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
                closed: Arc::new(Mutex::new(false)),
                clock: Instant::now(),
            },
            worker: Some(thread::spawn(|| bail!("test writer failure"))),
        };
        assert!(
            writer
                .finish()
                .unwrap_err()
                .to_string()
                .contains("test writer failure")
        );
    }
    #[test]
    fn nearest_rank() {
        let d = distribution(vec![1., 2., 3., 4., 5.]);
        assert_eq!(d["p95_ms"], 5.);
        assert_eq!(d["p50_ms"], 3.);
        assert!(distribution(vec![]).is_null());
    }
    #[test]
    fn finish_works_with_live_clone_and_marks_complete() {
        let dir = std::env::temp_dir().join(format!(
            "crossflow-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("log.jsonl");
        let writer = LogWriter::new(&path, json!({"test":true})).unwrap();
        let clone = writer.log.clone();
        clone.write("sent", json!({"t0_ns":100,"t1_ns":1100}));
        writer.finish().unwrap();
        let result = summarize(&path).unwrap();
        assert_eq!(result["complete_log"], true);
        assert_eq!(result["source_processing"]["samples"], 1);
        drop(clone);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn truncated_log_does_not_claim_zero_loss() {
        let dir = std::env::temp_dir().join(format!(
            "crossflow-truncated-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("log.jsonl");
        std::fs::write(&path, "{\"event\":\"metadata\"}\n{bad\n").unwrap();
        let result = summarize(&path).unwrap();
        assert_eq!(result["complete_log"], false);
        assert!(result["dropped_log_records"].is_null());
        assert_eq!(result["malformed_lines"], 1);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
