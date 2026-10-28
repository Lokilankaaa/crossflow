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
    let mut worker_timings = WorkerTimingSummary::default();
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
            worker_timings.failure_logging_declared |=
                v["data"]["worker_rpc_failure_logging"].as_bool() == Some(true);
            adapter_scopes.insert(match v["data"]["receiver_t3_t4_scope"].as_str() {
                Some("local_adapter_call") => "local_adapter_call",
                Some("worker_rpc_including_adapter_not_isolated_native_injection") => {
                    "worker_rpc_including_adapter_not_isolated_native_injection"
                }
                _ => "unspecified",
            });
        }
        if v["event"] == "worker_rpc_failure" {
            worker_timings.record_failure(v.get("reason"));
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
            worker_timings.record(v.get("worker_timing"), false);
            if let Some(d) = timing_delta(&v, "t2_ns", "t3_ns", &mut issues) {
                action_receive.push(d);
            }
            if let Some(d) = timing_delta(&v, "t3_ns", "t4_ns", &mut issues) {
                action_inject.push(d);
            }
        }
        if v["event"] == "received" {
            worker_timings.record(v.get("worker_timing"), true);
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
        "worker_timing":worker_timings.finish(),
        "injection_call":distribution(inject),"visible_smoothness":"NOT_MEASURED",
        "one_way_network_latency":"NOT_MEASURED: clocks are not synchronized"
    }))
}

/// These are elapsed durations within one machine-wide clock, never timestamps
/// to subtract from the receiver log's process-local t2/t3/t4 clock.
struct WorkerTiming {
    clock: &'static str,
    dispatch: u64,
    sink_call: u64,
    returned: u64,
    rpc: u64,
}
impl WorkerTiming {
    fn parse(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        let clock = match object.get("clock")?.as_str()? {
            "mach_continuous_time_ns" => "mach_continuous_time_ns",
            "GetTickCount64_ms_quantized" => "GetTickCount64_ms_quantized",
            "CLOCK_BOOTTIME_ns" => "CLOCK_BOOTTIME_ns",
            _ => return None,
        };
        let sequence = object.get("request_seq")?.as_u64()?;
        let dispatch = object.get("dispatch_ns")?.as_u64()?;
        let sink_call = object.get("sink_call_ns")?.as_u64()?;
        let returned = object.get("return_ns")?.as_u64()?;
        let rpc = object.get("rpc_ns")?.as_u64()?;
        if sequence == 0
            || rpc >= 30_000_000
            || dispatch.checked_add(sink_call)?.checked_add(returned)? != rpc
        {
            return None;
        }
        Some(Self {
            clock,
            dispatch,
            sink_call,
            returned,
            rpc,
        })
    }
}

#[derive(Default)]
struct WorkerDurations {
    dispatch: Vec<f64>,
    sink_call: Vec<f64>,
    returned: Vec<f64>,
    rpc: Vec<f64>,
}
impl WorkerDurations {
    fn record(&mut self, timing: WorkerTiming) {
        self.dispatch.push(timing.dispatch as f64 / 1e6);
        self.sink_call.push(timing.sink_call as f64 / 1e6);
        self.returned.push(timing.returned as f64 / 1e6);
        self.rpc.push(timing.rpc as f64 / 1e6);
    }
    fn finish(self) -> Value {
        json!({
            "dispatch":distribution(self.dispatch),
            "sink_call":distribution(self.sink_call),
            "return":distribution(self.returned),
            "rpc":distribution(self.rpc)
        })
    }
}
#[derive(Default)]
struct WorkerTimingGroups {
    movement: WorkerDurations,
    actions: WorkerDurations,
}
#[derive(Default)]
struct WorkerTimingSummary {
    clocks: std::collections::BTreeMap<&'static str, WorkerTimingGroups>,
    valid_records: u64,
    invalid_records: u64,
    unavailable_records: u64,
    failure_logging_declared: bool,
    failures_by_reason: std::collections::BTreeMap<&'static str, u64>,
    failure_records: u64,
}
impl WorkerTimingSummary {
    fn record_failure(&mut self, reason: Option<&Value>) {
        let reason = match reason.and_then(Value::as_str) {
            Some("preflight") => "preflight",
            Some("clock") => "clock",
            Some("request_invalid") => "request_invalid",
            Some("request_queue_failed") => "request_queue_failed",
            Some("timeout") => "timeout",
            Some("response_read_failed") => "response_read_failed",
            Some("invalid_ack") => "invalid_ack",
            _ => "unknown",
        };
        self.failure_records += 1;
        *self.failures_by_reason.entry(reason).or_default() += 1;
    }
    fn record(&mut self, value: Option<&Value>, movement: bool) {
        let Some(value) = value.filter(|value| !value.is_null()) else {
            self.unavailable_records += 1;
            return;
        };
        let Some(timing) = WorkerTiming::parse(value) else {
            self.invalid_records += 1;
            return;
        };
        self.valid_records += 1;
        let group = self.clocks.entry(timing.clock).or_default();
        if movement {
            group.movement.record(timing);
        } else {
            group.actions.record(timing);
        }
    }
    fn finish(self) -> Value {
        let by_clock: serde_json::Map<String, Value> = self
            .clocks
            .into_iter()
            .map(|(clock, group)| {
                (
                    clock.to_owned(),
                    json!({"movement":group.movement.finish(), "actions":group.actions.finish()}),
                )
            })
            .collect();
        json!({
            "by_clock":by_clock,
            "valid_records":self.valid_records,
            "invalid_records":self.invalid_records,
            "unavailable_records":self.unavailable_records,
            "failure_logging_declared":self.failure_logging_declared,
            "failures_by_reason":self.failures_by_reason,
            "failure_records":self.failure_records,
            "failure_logging_status":if self.failure_logging_declared { "declared" } else { "unknown" },
            "failure_count_warning":"Missing worker_rpc_failure_logging metadata means failure coverage is unknown, not zero failures. Counts include only recorded terminal RPC failures; incomplete or dropped logs can omit failures even when logging was declared. response_read_failed combines EOF, frame decoding failure and response queue failure; it is not proof of a physical pipe closure.",
            "scope":"Durations reported only for successful actions with a matched, timely worker ACK; ACK means adapter return, not OS delivery.",
            "selection_warning":"Timed-out and failed actions are excluded from these four distributions. Successful samples cannot establish absence of stalls; inspect errors and rejection evidence separately.",
            "sink_call_scope":"Worker owned-input state handling, safety guards and adapter call; not isolated CGEventPost or visible latency.",
            "clock_warning":"Clocks are grouped separately. GetTickCount64_ms_quantized is millisecond-quantized and cannot establish high-resolution input performance. Missing/null timings are unavailable, never zero-duration samples.",
            "visible_smoothness":"NOT_MEASURED"
        })
    }
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

    fn worker_sample(clock: &str, dispatch: u64, sink: u64, returned: u64) -> Value {
        json!({"request_seq":1,"dispatch_ns":dispatch,"sink_call_ns":sink,
            "return_ns":returned,"rpc_ns":dispatch + sink + returned,"clock":clock})
    }

    fn summarize_records(mut records: Vec<Value>) -> Value {
        records.push(json!({"event":"log_summary","dropped_records":0}));
        summarize_text(
            &records
                .into_iter()
                .map(|record| format!("{record}\n"))
                .collect::<String>(),
        )
    }

    fn worker_record(event: &str, timing: Value) -> Value {
        json!({"event":event,"t2_ns":0,"t3_ns":1_000_000,"t4_ns":10_000_000,"worker_timing":timing})
    }

    #[test]
    fn worker_durations_separate_clocks_and_actions_from_movement() {
        let result = summarize_records(vec![
            worker_record(
                "received",
                worker_sample("mach_continuous_time_ns", 1_000_000, 2_000_000, 3_000_000),
            ),
            worker_record(
                "received",
                worker_sample("mach_continuous_time_ns", 2_000_000, 3_000_000, 4_000_000),
            ),
            worker_record(
                "input_applied",
                worker_sample("mach_continuous_time_ns", 1_000_000, 1_000_000, 1_000_000),
            ),
            worker_record(
                "input_applied",
                worker_sample("GetTickCount64_ms_quantized", 1_000_000, 0, 0),
            ),
            worker_record(
                "received",
                worker_sample("CLOCK_BOOTTIME_ns", 500_000, 500_000, 500_000),
            ),
        ]);
        let worker = &result["worker_timing"];
        assert_eq!(worker["valid_records"], 5);
        assert_eq!(worker["invalid_records"], 0);
        assert_eq!(worker["unavailable_records"], 0);
        assert_eq!(worker["by_clock"].as_object().unwrap().len(), 3);
        let mac = &worker["by_clock"]["mach_continuous_time_ns"];
        assert_eq!(mac["movement"]["dispatch"]["samples"], 2);
        assert_eq!(mac["movement"]["dispatch"]["p50_ms"], 1.0);
        assert_eq!(mac["movement"]["sink_call"]["max_ms"], 3.0);
        assert_eq!(mac["movement"]["return"]["max_ms"], 4.0);
        assert_eq!(mac["movement"]["rpc"]["p95_ms"], 9.0);
        assert_eq!(mac["actions"]["rpc"]["max_ms"], 3.0);
        let windows = &worker["by_clock"]["GetTickCount64_ms_quantized"];
        assert!(windows["movement"]["rpc"].is_null());
        assert_eq!(windows["actions"]["sink_call"]["max_ms"], 0.0);
        assert_eq!(
            worker["by_clock"]["CLOCK_BOOTTIME_ns"]["movement"]["rpc"]["max_ms"],
            1.5
        );
        // The old t3..t4 measurement stays independent of worker components.
        assert_eq!(result["injection_call"]["samples"], 3);
        assert_eq!(result["injection_call"]["max_ms"], 9.0);
        assert_eq!(result["action_adapter_call"]["samples"], 2);
    }

    #[test]
    fn missing_worker_timings_are_unavailable_not_zero_or_corrupt() {
        let result = summarize_records(vec![
            json!({"event":"received","t2_ns":0,"t3_ns":1,"t4_ns":2}),
            worker_record("input_applied", Value::Null),
            // Timings on unrelated events are not acknowledged input samples.
            worker_record("error", worker_sample("mach_continuous_time_ns", 1, 2, 3)),
        ]);
        assert_eq!(result["worker_timing"]["unavailable_records"], 2);
        assert_eq!(result["worker_timing"]["invalid_records"], 0);
        assert_eq!(result["worker_timing"]["valid_records"], 0);
        assert_eq!(result["worker_timing"]["by_clock"], json!({}));
        assert_eq!(result["errors"], 1);
        assert_eq!(result["injection_call"]["samples"], 1);
        assert_eq!(result["action_adapter_call"]["samples"], 1);
    }

    #[test]
    fn malformed_worker_timings_are_rejected_as_whole_records() {
        let valid = worker_sample("mach_continuous_time_ns", 1, 2, 3);
        let mut malformed = vec![json!([]), json!(false), json!("timing"), json!({})];
        for field in [
            "request_seq",
            "dispatch_ns",
            "sink_call_ns",
            "return_ns",
            "rpc_ns",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            malformed.push(missing);
            for invalid in [Value::Null, json!("1"), json!(-1), json!(1.0)] {
                let mut record = valid.clone();
                record[field] = invalid;
                malformed.push(record);
            }
        }
        for (field, invalid) in [
            ("request_seq", json!(0)),
            ("clock", json!("untrusted-clock")),
            ("clock", Value::Null),
            ("clock", json!(1)),
            ("rpc_ns", json!(7)),
        ] {
            let mut record = valid.clone();
            record[field] = invalid;
            malformed.push(record);
        }
        let mut missing_clock = valid.clone();
        missing_clock.as_object_mut().unwrap().remove("clock");
        malformed.push(missing_clock);
        malformed.push(worker_sample(
            "CLOCK_BOOTTIME_ns",
            10_000_000,
            10_000_000,
            10_000_000,
        ));
        malformed.push(json!({"request_seq":1,"dispatch_ns":u64::MAX,"sink_call_ns":1,"return_ns":0,"rpc_ns":0,"clock":"mach_continuous_time_ns"}));
        malformed.push(json!({"request_seq":1,"dispatch_ns":u64::MAX-1,"sink_call_ns":1,"return_ns":1,"rpc_ns":0,"clock":"mach_continuous_time_ns"}));
        let count = malformed.len();
        let records = malformed
            .into_iter()
            .map(|timing| worker_record("received", timing))
            .collect();
        let result = summarize_records(records);
        assert_eq!(result["worker_timing"]["invalid_records"], count);
        assert_eq!(result["worker_timing"]["valid_records"], 0);
        assert_eq!(result["worker_timing"]["unavailable_records"], 0);
        assert_eq!(result["worker_timing"]["by_clock"], json!({}));
        assert_eq!(result["injection_call"]["samples"], count);
    }

    #[test]
    fn worker_ack_durations_never_claim_visible_or_native_injection_latency() {
        let result = summarize_records(vec![
            json!({"event":"metadata","data":{"receiver_t3_t4_scope":"worker_rpc_including_adapter_not_isolated_native_injection"}}),
            worker_record(
                "received",
                worker_sample("GetTickCount64_ms_quantized", 0, 0, 0),
            ),
            json!({"event":"error","message":"ACK timed out"}),
        ]);
        assert_eq!(result["worker_timing"]["valid_records"], 1);
        assert_eq!(result["errors"], 1);
        assert_eq!(result["visible_smoothness"], "NOT_MEASURED");
        assert_eq!(
            result["worker_timing"]["visible_smoothness"],
            "NOT_MEASURED"
        );
        assert!(
            result["one_way_network_latency"]
                .as_str()
                .unwrap()
                .starts_with("NOT_MEASURED")
        );
        assert!(result["capture_interval_all_in_session"].is_null());
        assert_eq!(
            result["receiver_adapter_timing_scopes"],
            json!(["worker_rpc_including_adapter_not_isolated_native_injection"])
        );
        assert!(
            result["worker_timing"]["sink_call_scope"]
                .as_str()
                .unwrap()
                .contains("not isolated CGEventPost")
        );
        assert!(
            result["worker_timing"]["clock_warning"]
                .as_str()
                .unwrap()
                .contains("millisecond-quantized")
        );
        assert!(
            result["worker_timing"]["selection_warning"]
                .as_str()
                .unwrap()
                .contains("cannot establish absence of stalls")
        );
    }

    #[test]
    fn worker_rpc_failures_are_counted_separately_and_legacy_coverage_is_unknown() {
        let sample = worker_sample("mach_continuous_time_ns", 1, 2, 3);
        let result = summarize_records(vec![
            json!({"event":"metadata","data":{"worker_rpc_failure_logging":true}}),
            worker_record("received", sample.clone()),
            json!({"event":"worker_rpc_failure","reason":"timeout","request_seq":2,"kind":"Move","result":"uncertain_no_replay","worker_timing":sample}),
            json!({"event":"worker_rpc_failure","reason":"invalid_ack","request_seq":3,"kind":"Key","result":"uncertain_no_replay"}),
            json!({"event":"worker_rpc_failure","reason":"untrusted-arbitrary-reason"}),
        ]);
        let worker = &result["worker_timing"];
        assert_eq!(worker["failure_logging_declared"], true);
        assert_eq!(worker["failure_logging_status"], "declared");
        assert_eq!(worker["failure_records"], 3);
        assert_eq!(
            worker["failures_by_reason"],
            json!({"timeout":1,"invalid_ack":1,"unknown":1})
        );
        assert_eq!(worker["valid_records"], 1);
        assert_eq!(
            worker["by_clock"]["mach_continuous_time_ns"]["movement"]["rpc"]["samples"],
            1
        );
        assert!(worker["by_clock"]["mach_continuous_time_ns"]["actions"]["rpc"].is_null());
        for metadata in [
            json!({}),
            json!({"worker_rpc_failure_logging":false}),
            json!({"worker_rpc_failure_logging":"true"}),
        ] {
            let legacy = summarize_records(vec![json!({"event":"metadata","data":metadata})]);
            assert_eq!(legacy["worker_timing"]["failure_logging_declared"], false);
            assert_eq!(legacy["worker_timing"]["failure_logging_status"], "unknown");
            assert_eq!(legacy["worker_timing"]["failure_records"], 0);
            assert!(
                legacy["worker_timing"]["failure_count_warning"]
                    .as_str()
                    .unwrap()
                    .contains("unknown, not zero failures")
            );
        }
        let undeclared = summarize_records(vec![
            json!({"event":"worker_rpc_failure","reason":"timeout"}),
        ]);
        assert_eq!(undeclared["worker_timing"]["failure_records"], 1);
        assert_eq!(
            undeclared["worker_timing"]["failure_logging_status"],
            "unknown"
        );
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
