use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{SyncSender, sync_channel},
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
    clock: Instant,
}
pub struct LogWriter {
    pub log: Log,
    worker: Option<JoinHandle<()>>,
}
impl LogWriter {
    pub fn new(path: &Path, metadata: Value) -> Result<Self> {
        if let Some(p) = path.parent() {
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
        let worker = thread::spawn(move || {
            let mut w = BufWriter::new(file);
            let _ = writeln!(w, "{}", json!({"event":"metadata","data":metadata}));
            for msg in rx {
                match msg {
                    Message::Event(v) => {
                        if writeln!(w, "{v}").is_err() {
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Message::Close => break,
                }
            }
            let _ = writeln!(
                w,
                "{}",
                json!({"event":"log_summary","dropped_records":count.load(Ordering::Relaxed)})
            );
            let _ = w.flush();
        });
        Ok(Self {
            log: Log {
                tx,
                dropped,
                clock: Instant::now(),
            },
            worker: Some(worker),
        })
    }
    pub fn finish(mut self) {
        let _ = self.log.tx.send(Message::Close);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
impl Log {
    pub fn ns(&self) -> u64 {
        self.clock.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }
    pub fn at(&self, t: Instant) -> u64 {
        t.saturating_duration_since(self.clock).as_nanos() as u64
    }
    pub fn write(&self, event: &str, mut data: Value) {
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
    let mut cadence = vec![];
    let mut last = None;
    let mut events = 0;
    let mut rejected = 0;
    let mut dropped = 0;
    let mut malformed = 0;
    let mut complete = false;
    let mut idle_gaps = 0;
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                malformed += 1;
                continue;
            }
        };
        events += 1;
        let delta = |a: &str, b: &str| -> Option<f64> {
            let a = v[a].as_u64()?;
            let b = v[b].as_u64()?;
            b.checked_sub(a).map(|d| d as f64 / 1e6)
        };
        if v["event"] == "sent" {
            if let Some(d) = delta("t0_ns", "t1_ns") {
                src.push(d)
            }
        }
        if let Some(d) = delta("t2_ns", "t3_ns") {
            recv.push(d)
        }
        if let Some(d) = delta("t3_ns", "t4_ns") {
            inject.push(d)
        }
        if matches!(
            v["event"].as_str(),
            Some("connected" | "toggle" | "error" | "control")
        ) {
            last = None;
        }
        if v["event"] == "capture" {
            if let Some(t) = v["t0_ns"].as_u64() {
                if let Some(previous) = last {
                    if t >= previous {
                        let gap = (t - previous) as f64 / 1e6;
                        if gap <= 50.0 {
                            cadence.push(gap)
                        } else {
                            idle_gaps += 1;
                        }
                    }
                }
                last = Some(t)
            }
        }
        if v["event"] == "error" {
            rejected += 1
        }
        if v["event"] == "log_summary" {
            complete = true;
            if let Some(d) = v["dropped_records"].as_u64() {
                dropped += d;
            }
        }
    }
    Ok(
        json!({"events":events,"errors":rejected,"malformed_lines":malformed,"complete_log":complete,"dropped_log_records":if complete{json!(dropped)}else{Value::Null},"capture_idle_gaps_over_50ms":idle_gaps,"capture_interval_within_motion_bursts":distribution(cadence),"source_processing":distribution(src),"receiver_processing":distribution(recv),"injection_call":distribution(inject),"visible_smoothness":"NOT_MEASURED","one_way_network_latency":"NOT_MEASURED: clocks are not synchronized"}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
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
        writer.finish();
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
