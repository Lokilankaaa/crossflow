//! Fake adapters and channels only. No OS input or global panic hook changes.
use super::*;
use std::{
    collections::BTreeSet,
    io,
    sync::{Condvar, Mutex},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Key(u16, bool),
    Button(Button, bool),
}

#[derive(Default)]
struct FaultSink {
    events: Vec<Event>,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<Button>,
    panic_down: Option<u16>,
    panic_up_once: Option<u16>,
    error_up_once: Option<u16>,
}
impl MouseSink for FaultSink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        Ok((x, y))
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        self.events.push(Event::Key(usage, down));
        if down {
            self.keys.insert(usage);
            if self.panic_down == Some(usage) {
                panic!("synthetic private panic fixture");
            }
        } else {
            if self.panic_up_once == Some(usage) {
                self.panic_up_once = None;
                panic!("synthetic release panic fixture");
            }
            if self.error_up_once == Some(usage) {
                self.error_up_once = None;
                bail!("synthetic adapter release failure");
            }
            self.keys.remove(&usage);
        }
        Ok(())
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        self.events.push(Event::Button(button, down));
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        Ok(())
    }
}

#[test]
fn down_that_posts_then_panics_is_still_owned_and_released() {
    let mut sink = FaultSink {
        panic_down: Some(0x04),
        ..FaultSink::default()
    };
    let stop = AtomicBool::new(false);
    {
        let mut guarded = Guarded(&mut sink);
        let mut owned = OwnedSink::new(&mut guarded, &stop);
        let error = owned.key(0x04, true).unwrap_err().to_string();
        assert_eq!(error, "worker adapter panicked");
        assert!(!error.contains("private"));
        owned.release_owned().unwrap();
        owned.release_owned().unwrap();
    }
    assert!(sink.keys.is_empty());
    assert_eq!(
        sink.events,
        vec![Event::Key(0x04, true), Event::Key(0x04, false)]
    );
}

#[test]
fn release_panics_and_errors_do_not_skip_other_owned_inputs_or_replay_success() {
    let mut sink = FaultSink {
        panic_up_once: Some(0x04),
        error_up_once: Some(0x05),
        ..FaultSink::default()
    };
    let stop = AtomicBool::new(false);
    {
        let mut guarded = Guarded(&mut sink);
        let mut owned = OwnedSink::new(&mut guarded, &stop);
        for usage in [0x04, 0x05, 0x06] {
            owned.key(usage, true).unwrap();
        }
        owned.button(Button::Left, true).unwrap();
        let error = owned.release_owned().unwrap_err().to_string();
        assert!(error.contains("worker adapter panicked"));
        assert!(error.contains("synthetic adapter release failure"));
        assert!(!error.contains("release panic fixture"));
        owned.release_owned().unwrap();
        owned.release_owned().unwrap();
    }
    assert!(sink.keys.is_empty() && sink.buttons.is_empty());
    assert_eq!(
        sink.events,
        vec![
            Event::Key(0x04, true),
            Event::Key(0x05, true),
            Event::Key(0x06, true),
            Event::Button(Button::Left, true),
            Event::Key(0x04, false),
            Event::Key(0x05, false),
            Event::Key(0x06, false),
            Event::Button(Button::Left, false),
            Event::Key(0x04, false),
            Event::Key(0x05, false),
        ]
    );
}

struct ChannelRead {
    receive: Receiver<[u8; FRAME_LEN]>,
    pending: Option<([u8; FRAME_LEN], usize)>,
    finished: mpsc::Sender<()>,
}
impl Read for ChannelRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pending.is_none() {
            self.pending = match self.receive.recv() {
                Ok(bytes) => Some((bytes, 0)),
                Err(_) => return Ok(0),
            };
        }
        let (bytes, offset) = self.pending.as_mut().unwrap();
        let count = (FRAME_LEN - *offset).min(out.len());
        out[..count].copy_from_slice(&bytes[*offset..*offset + count]);
        *offset += count;
        if *offset == FRAME_LEN {
            self.pending = None;
        }
        Ok(count)
    }
}
impl Drop for ChannelRead {
    fn drop(&mut self) {
        let _ = self.finished.send(());
    }
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}
impl Gate {
    fn unblock(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct UnblockOnDrop(Arc<Gate>);
impl Drop for UnblockOnDrop {
    fn drop(&mut self) {
        self.0.unblock();
    }
}

struct BlockingWrite {
    gate: Arc<Gate>,
    blocked: mpsc::Sender<()>,
    acks: mpsc::Sender<Message>,
    finished: mpsc::Sender<()>,
}
impl Write for BlockingWrite {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let bytes: &[u8; FRAME_LEN] = bytes
            .try_into()
            .map_err(|_| io::Error::other("unexpected fake frame length"))?;
        let message =
            Message::decode(bytes).map_err(|_| io::Error::other("invalid fake response"))?;
        if message.seq == 4 {
            let _ = self.blocked.send(());
            let mut open = self.gate.open.lock().unwrap();
            while !*open {
                open = self.gate.changed.wait(open).unwrap();
            }
        }
        self.acks
            .send(message)
            .map_err(|_| io::Error::other("fake parent stopped reading"))?;
        Ok(FRAME_LEN)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for BlockingWrite {
    fn drop(&mut self) {
        let _ = self.finished.send(());
    }
}

struct RecordingSink {
    state: Arc<Mutex<FaultSink>>,
    released_ns: Arc<AtomicU64>,
}
impl MouseSink for RecordingSink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        Ok((x, y))
    }
    fn key(&mut self, usage: u16, down: bool) -> Result<()> {
        self.state.lock().unwrap().key(usage, down)
    }
    fn button(&mut self, button: Button, down: bool) -> Result<()> {
        self.state.lock().unwrap().button(button, down)?;
        if !down {
            self.released_ns.store(continuous_ns()?, Ordering::Release);
        }
        Ok(())
    }
}

#[test]
fn blocked_response_writer_does_not_delay_lease_cleanup_or_worker_return() {
    let (requests, request_read) = mpsc::channel();
    let (acks, ack_read) = mpsc::channel();
    let (blocked, blocked_read) = mpsc::channel();
    let (reader_finished, reader_done) = mpsc::channel();
    let (writer_finished, writer_done) = mpsc::channel();
    let gate = Arc::new(Gate::default());
    let unblock = UnblockOnDrop(gate.clone());
    let state = Arc::new(Mutex::new(FaultSink::default()));
    let released_ns = Arc::new(AtomicU64::new(0));
    let sink = RecordingSink {
        state: state.clone(),
        released_ns: released_ns.clone(),
    };
    let input = ChannelRead {
        receive: request_read,
        pending: None,
        finished: reader_finished,
    };
    let output = BlockingWrite {
        gate: gate.clone(),
        blocked,
        acks,
        finished: writer_finished,
    };
    let (finished, done) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut sink = sink;
        let result = run_worker(input, output, &mut sink);
        let _ = finished.send(result.map_err(|error| error.to_string()));
    });
    assert_eq!(
        ack_read.recv_timeout(Duration::from_secs(2)).unwrap().op,
        Op::Ready
    );
    // Send only after the previous ACK; stamp immediately before sending.
    for (seq, op) in [(1, Op::Reset), (2, Op::Key), (3, Op::Button)] {
        let mut message = Message::new(op);
        message.seq = seq;
        message.epoch = 1;
        if op == Op::Key {
            message.code = 0x04;
            message.down = true;
        }
        if op == Op::Button {
            message.code = button_code(Button::Left);
            message.down = true;
        }
        message.sent_ns = continuous_ns().unwrap();
        requests.send(message.encode().unwrap()).unwrap();
        let ack = ack_read.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!((ack.op, ack.seq), (Op::Completed, seq));
    }
    {
        let snapshot = state.lock().unwrap();
        assert!(snapshot.keys.contains(&0x04) && snapshot.buttons.contains(&Button::Left));
    }
    let mut heartbeat = Message::new(Op::Heartbeat);
    heartbeat.seq = 4;
    heartbeat.epoch = 1;
    heartbeat.sent_ns = continuous_ns().unwrap();
    requests.send(heartbeat.encode().unwrap()).unwrap();
    blocked_read.recv_timeout(Duration::from_secs(2)).unwrap();
    let error = done
        .recv_timeout(Duration::from_secs(2))
        .expect("worker waited on blocked response IO")
        .unwrap_err();
    assert!(error.contains("worker lease ended"), "{error}");
    assert!(
        !*gate.open.lock().unwrap(),
        "writer must still be blocked when worker returns"
    );
    let release_time = released_ns.load(Ordering::Acquire);
    let elapsed = release_time.checked_sub(heartbeat.sent_ns).unwrap();
    assert!(elapsed >= crate::worker_protocol::ACTIVE_LEASE_NS);
    assert!(
        elapsed < 2_000_000_000,
        "cleanup did not complete within the test bound"
    );
    {
        let snapshot = state.lock().unwrap();
        assert!(snapshot.keys.is_empty() && snapshot.buttons.is_empty());
        assert_eq!(
            snapshot.events,
            vec![
                Event::Key(0x04, true),
                Event::Button(Button::Left, true),
                Event::Key(0x04, false),
                Event::Button(Button::Left, false)
            ]
        );
    }
    worker.join().unwrap();
    drop(requests); // Unblock the detached reader; do not leak test IO threads.
    drop(unblock); // Production exits its process instead; test explicitly opens it.
    reader_done.recv_timeout(Duration::from_secs(2)).unwrap();
    writer_done.recv_timeout(Duration::from_secs(2)).unwrap();
}

fn timing_pair(op: Op) -> (Message, Message) {
    let mut request = Message::new(op);
    request.seq = 7;
    request.epoch = 3;
    request.sent_ns = 100;
    if op == Op::Key {
        request.code = 0x04;
    }
    if op == Op::Scroll {
        request.a = 120;
    }
    let mut response = Message::new(Op::Completed);
    response.seq = request.seq;
    response.epoch = request.epoch;
    response.call_start_ns = 200;
    response.call_end_ns = 300;
    response.sent_ns = 400;
    (request, response)
}

#[test]
fn action_timing_intervals_partition_rpc_and_allow_zero_length_stages() {
    for op in [Op::Move, Op::Button, Op::Key, Op::Scroll] {
        let (request, mut response) = timing_pair(op);
        let timing = timing_for_ack(&request, &response, 500).unwrap().unwrap();
        assert_eq!(timing.request_seq, request.seq);
        assert_eq!(
            (
                timing.dispatch_ns,
                timing.sink_call_ns,
                timing.return_ns,
                timing.rpc_ns
            ),
            (100, 100, 200, 400)
        );
        assert_eq!(
            timing.dispatch_ns + timing.sink_call_ns + timing.return_ns,
            timing.rpc_ns
        );
        response.call_start_ns = request.sent_ns;
        response.call_end_ns = request.sent_ns;
        response.sent_ns = request.sent_ns;
        let zero = timing_for_ack(&request, &response, request.sent_ns)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                zero.dispatch_ns,
                zero.sink_call_ns,
                zero.return_ns,
                zero.rpc_ns
            ),
            (0, 0, 0, 0)
        );
        let last_valid = request.sent_ns + crate::worker_protocol::MAX_REQUEST_AGE_NS - 1;
        assert!(timing_for_ack(&request, &response, last_valid).is_ok());
    }
}

#[test]
fn ack_timing_rejects_mismatched_identity_future_reversal_and_expiry() {
    for case in 0..11 {
        let (mut request, mut response) = timing_pair(Op::Move);
        let mut received = 500;
        match case {
            0 => request.sent_ns = 201,        // issued after call start
            1 => response.call_start_ns = 301, // start after end
            2 => response.call_end_ns = 401,   // end after response send
            3 => response.sent_ns = 501,       // response sent in receiver's future
            4 => received = 99,                // receipt before issuance
            5 => response.seq += 1,
            6 => response.epoch += 1,
            7 => response.op = Op::Failed,
            8 => received = request.sent_ns + crate::worker_protocol::MAX_REQUEST_AGE_NS,
            9 => received = request.sent_ns + crate::worker_protocol::MAX_REQUEST_AGE_NS + 1,
            _ => response.sent_ns = 99, // response before request
        }
        assert!(
            timing_for_ack(&request, &response, received).is_err(),
            "invalid timing case {case}"
        );
    }
}

#[test]
fn control_ack_has_no_action_timing_and_rejects_nonzero_stages() {
    for op in [Op::Reset, Op::Heartbeat, Op::Release, Op::Shutdown] {
        let (request, mut response) = timing_pair(op);
        response.call_start_ns = 0;
        response.call_end_ns = 0;
        assert!(timing_for_ack(&request, &response, 500).unwrap().is_none());
        for (start, end) in [(0, 1), (1, 0), (1, 1)] {
            response.call_start_ns = start;
            response.call_end_ns = end;
            assert!(timing_for_ack(&request, &response, 500).is_err());
        }
    }
}

struct DelayedSink {
    observed: Arc<Mutex<Option<(u64, u64, Duration)>>>,
}
impl MouseSink for DelayedSink {
    fn move_by(&mut self, x: f64, y: f64) -> Result<(f64, f64)> {
        let start = continuous_ns()?;
        let wall = Instant::now();
        thread::sleep(Duration::from_millis(20));
        let elapsed = wall.elapsed();
        let end = continuous_ns()?;
        *self.observed.lock().unwrap() = Some((start, end, elapsed));
        Ok((x, y))
    }
}

#[test]
fn worker_ack_timestamps_enclose_delayed_sink_call_without_claiming_proxy_latency() {
    let (requests, request_read) = mpsc::channel();
    let (acks, ack_read) = mpsc::channel();
    let (blocked, _blocked_read) = mpsc::channel();
    let (reader_finished, reader_done) = mpsc::channel();
    let (writer_finished, writer_done) = mpsc::channel();
    let gate = Arc::new(Gate::default());
    gate.unblock();
    let input = ChannelRead {
        receive: request_read,
        pending: None,
        finished: reader_finished,
    };
    let output = BlockingWrite {
        gate,
        blocked,
        acks,
        finished: writer_finished,
    };
    let observed = Arc::new(Mutex::new(None));
    let mut sink = DelayedSink {
        observed: observed.clone(),
    };
    let (finished, done) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ = finished.send(run_worker(input, output, &mut sink));
    });
    assert_eq!(
        ack_read.recv_timeout(Duration::from_secs(2)).unwrap().op,
        Op::Ready
    );
    for (seq, op) in [(1, Op::Reset), (2, Op::Move), (3, Op::Shutdown)] {
        let mut request = Message::new(op);
        request.seq = seq;
        request.epoch = 1;
        if op == Op::Move {
            request.x = 1.0;
            request.y = -1.0;
        }
        request.sent_ns = continuous_ns().unwrap();
        requests.send(request.encode().unwrap()).unwrap();
        let response = ack_read.recv_timeout(Duration::from_secs(2)).unwrap();
        let received = continuous_ns().unwrap();
        assert_eq!(
            (response.op, response.seq, response.epoch),
            (Op::Completed, seq, 1)
        );
        if op == Op::Move {
            let (inner_start, inner_end, wall_elapsed) = observed.lock().unwrap().unwrap();
            assert!(wall_elapsed >= Duration::from_millis(20));
            assert!(request.sent_ns <= response.call_start_ns);
            assert!(response.call_start_ns <= inner_start && inner_start <= inner_end);
            assert!(inner_end <= response.call_end_ns && response.call_end_ns <= response.sent_ns);
            assert!(response.sent_ns <= received);
            if cfg!(windows) {
                // GetTickCount64 is quantized; a 20ms sleep is not asserted to
                // equal 20ms in this clock, nor to fit the proxy's 30ms budget.
                for time in [
                    inner_start,
                    inner_end,
                    response.call_start_ns,
                    response.call_end_ns,
                ] {
                    assert_eq!(time % 1_000_000, 0);
                }
            }
            assert_eq!((response.x, response.y), (1.0, -1.0));
        } else {
            assert_eq!((response.call_start_ns, response.call_end_ns), (0, 0));
        }
    }
    done.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    worker.join().unwrap();
    drop(requests);
    reader_done.recv_timeout(Duration::from_secs(2)).unwrap();
    writer_done.recv_timeout(Duration::from_secs(2)).unwrap();
}
