//! Bounded, opt-in diagnostic records for the isolated Chrome Return experiment.
//! Recording does no log I/O and never blocks on the shared buffer. Only the
//! owning request's Drop closes collection and emits one JSON summary.

use serde::Serialize;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

const MAX_RECORDS: usize = 128;
const MAX_LABEL_BYTES: usize = 64;
const CLOCK_DOMAIN: &str = "darwin-clock-uptime-raw-v1";
static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Serialize)]
struct Metadata {
    scope: &'static str,
    driver_pid: u32,
    pid: i32,
    window_id: u32,
    attempt_id: u64,
}

#[derive(Clone, Copy, Serialize)]
struct Record {
    seq: usize,
    uptime_ns: Option<u64>,
    main_thread: bool,
    #[serde(flatten)]
    data: RecordData,
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RecordData {
    Event {
        stage: &'static str,
    },
    Begin {
        stage: &'static str,
    },
    Finish {
        stage: &'static str,
        begin_seq: Option<usize>,
        ok: bool,
    },
    Interrupted {
        stage: &'static str,
        begin_seq: Option<usize>,
    },
    WindowScan {
        count: usize,
        complete: bool,
    },
    Window {
        window_id: u32,
        minimized: Option<bool>,
        is_target: bool,
    },
}

struct State {
    records: [Option<Record>; MAX_RECORDS],
    count: usize,
    last_timestamp: Option<u64>,
}

struct Buffer {
    meta: Metadata,
    state: Mutex<State>,
    closed: AtomicBool,
    truncated: AtomicBool,
    clock_unavailable: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct Trace(Option<Arc<Buffer>>);

pub(crate) struct TraceRequest {
    trace: Trace,
}

pub(crate) struct Span {
    trace: Trace,
    stage: &'static str,
    begin_seq: Option<usize>,
    finished: bool,
}

struct Snapshot {
    meta: Metadata,
    records: [Option<Record>; MAX_RECORDS],
    count: usize,
    truncated: bool,
    clock_unavailable: bool,
}

fn enabled() -> bool {
    [
        "CUA_EXPERIMENTAL_CHROME_WINDOW_RETURN",
        "CUA_EXPERIMENTAL_CHROME_RETURN_TRACE",
    ]
    .iter()
    .all(|name| std::env::var_os(name).as_deref() == Some(std::ffi::OsStr::new("1")))
}

fn label(value: &'static str) -> Option<&'static str> {
    (!value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)))
    .then_some(value)
}

fn checked_uptime(status: i32, seconds: i64, nanos: i64) -> Option<u64> {
    if status != 0 || !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(nanos as u64)
}

fn uptime_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let status = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) };
    checked_uptime(status, time.tv_sec, time.tv_nsec)
}

impl TraceRequest {
    pub(crate) fn new(scope: &'static str, pid: i32, window_id: u32) -> Self {
        if !enabled() {
            return Self {
                trace: Trace::disabled(),
            };
        }
        let Ok(attempt_id) =
            NEXT_ATTEMPT.fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| id.checked_add(1))
        else {
            return Self {
                trace: Trace::disabled(),
            };
        };
        Self::with_metadata(
            Metadata {
                scope: label(scope).unwrap_or("invalid_scope"),
                driver_pid: std::process::id(),
                pid,
                window_id,
                attempt_id,
            },
            label(scope).is_none(),
        )
    }

    fn with_metadata(meta: Metadata, truncated: bool) -> Self {
        Self {
            trace: Trace(Some(Arc::new(Buffer {
                meta,
                state: Mutex::new(State {
                    records: [None; MAX_RECORDS],
                    count: 0,
                    last_timestamp: None,
                }),
                closed: AtomicBool::new(false),
                truncated: AtomicBool::new(truncated),
                clock_unavailable: AtomicBool::new(false),
            }))),
        }
    }

    pub(crate) fn trace(&self) -> Trace {
        self.trace.clone()
    }
}

impl Trace {
    pub(crate) fn disabled() -> Self {
        Self(None)
    }

    fn safe_stage(&self, stage: &'static str) -> &'static str {
        if let Some(stage) = label(stage) {
            return stage;
        }
        if let Some(buffer) = &self.0 {
            buffer.truncated.store(true, Ordering::Release);
        }
        "invalid_stage"
    }

    fn record_with(&self, data: RecordData, clock: impl FnOnce() -> Option<u64>) -> Option<usize> {
        let buffer = self.0.as_ref()?;
        if buffer.closed.load(Ordering::Acquire) {
            return None;
        }
        let result = catch_unwind(AssertUnwindSafe(|| {
            let mut state = match buffer.state.try_lock() {
                Ok(state) => state,
                Err(_) => {
                    buffer.truncated.store(true, Ordering::Release);
                    return None;
                }
            };
            if buffer.closed.load(Ordering::Acquire) {
                return None;
            }
            if state.count == MAX_RECORDS {
                buffer.truncated.store(true, Ordering::Release);
                return None;
            }
            let timestamp = clock();
            if timestamp.is_none() {
                buffer.clock_unavailable.store(true, Ordering::Release);
            }
            if let Some(timestamp) = timestamp {
                if state
                    .last_timestamp
                    .is_some_and(|previous| timestamp < previous)
                {
                    buffer.clock_unavailable.store(true, Ordering::Release);
                }
                state.last_timestamp = Some(
                    state
                        .last_timestamp
                        .map_or(timestamp, |previous| previous.max(timestamp)),
                );
            }
            if buffer.closed.load(Ordering::Acquire) {
                return None;
            }
            let seq = state.count;
            state.records[seq] = Some(Record {
                seq,
                uptime_ns: timestamp,
                main_thread: unsafe { libc::pthread_main_np() != 0 },
                data,
            });
            state.count += 1;
            Some(seq)
        }));
        match result {
            Ok(value) => value,
            Err(_) => {
                buffer.truncated.store(true, Ordering::Release);
                buffer.clock_unavailable.store(true, Ordering::Release);
                None
            }
        }
    }

    fn record(&self, data: RecordData) -> Option<usize> {
        self.record_with(data, uptime_ns)
    }

    pub(crate) fn event(&self, stage: &'static str) {
        self.record(RecordData::Event {
            stage: self.safe_stage(stage),
        });
    }

    pub(crate) fn begin(&self, stage: &'static str) -> Span {
        let stage = self.safe_stage(stage);
        let begin_seq = self.record(RecordData::Begin { stage });
        Span {
            trace: self.clone(),
            stage,
            begin_seq,
            finished: false,
        }
    }

    pub(crate) fn run<T, E>(
        &self,
        stage: &'static str,
        work: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let span = self.begin(stage);
        let result = work();
        span.finish(result.is_ok());
        result
    }

    pub(crate) fn window_scan(&self, count: usize, complete: bool) {
        self.record(RecordData::WindowScan { count, complete });
    }

    pub(crate) fn window(&self, id: u32, minimized: Option<bool>, is_target: bool) {
        self.record(RecordData::Window {
            window_id: id,
            minimized,
            is_target,
        });
    }

    fn close(&self) -> Option<Snapshot> {
        let buffer = self.0.as_ref()?;
        if buffer.closed.swap(true, Ordering::AcqRel) {
            return None;
        }
        let mut snapshot = Snapshot {
            meta: buffer.meta,
            records: [None; MAX_RECORDS],
            count: 0,
            truncated: buffer.truncated.load(Ordering::Acquire),
            clock_unavailable: buffer.clock_unavailable.load(Ordering::Acquire),
        };
        match buffer.state.try_lock() {
            Ok(state) => {
                snapshot.records = state.records;
                snapshot.count = state.count;
            }
            Err(TryLockError::Poisoned(error)) => {
                let state = error.into_inner();
                snapshot.records = state.records;
                snapshot.count = state.count;
                snapshot.truncated = true;
            }
            Err(TryLockError::WouldBlock) => {
                snapshot.truncated = true;
            }
        }
        Some(snapshot)
    }
}

impl Span {
    pub(crate) fn finish(mut self, ok: bool) {
        self.trace.record(RecordData::Finish {
            stage: self.stage,
            begin_seq: self.begin_seq,
            ok,
        });
        self.finished = true;
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if !self.finished {
            self.trace.record(RecordData::Interrupted {
                stage: self.stage,
                begin_seq: self.begin_seq,
            });
        }
    }
}

impl Snapshot {
    fn json(&self) -> Result<String, serde_json::Error> {
        #[derive(Serialize)]
        struct Wire<'a> {
            schema: &'static str,
            clock_domain: &'static str,
            #[serde(flatten)]
            meta: Metadata,
            collection_closed: bool,
            late_events_after_close: &'static str,
            truncated: bool,
            clock_unavailable: bool,
            records: &'a [Option<Record>],
        }
        serde_json::to_string(&Wire {
            schema: "cua.return_trace.v1",
            clock_domain: CLOCK_DOMAIN,
            meta: self.meta,
            collection_closed: true,
            late_events_after_close: "not_collected",
            truncated: self.truncated,
            clock_unavailable: self.clock_unavailable,
            records: &self.records[..self.count],
        })
    }
}

impl Drop for TraceRequest {
    fn drop(&mut self) {
        // This owner must outlive the entire native pair/cleanup scope. Trace
        // clones (including a late main-queue callback) never flush on Drop.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            if let Some(snapshot) = self.trace.close() {
                match snapshot.json() {
                    Ok(json) => {
                        tracing::warn!(target: "cua_return_trace", payload = %json, "bounded experiment trace")
                    }
                    Err(_) => {
                        tracing::warn!(target: "cua_return_trace", attempt_id = snapshot.meta.attempt_id,
                        collection_closed = true, truncated = true, "experiment trace unavailable")
                    }
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn request() -> TraceRequest {
        TraceRequest::with_metadata(
            Metadata {
                scope: "return",
                driver_pid: 1,
                pid: 2,
                window_id: 3,
                attempt_id: 4,
            },
            false,
        )
    }

    #[test]
    fn return_trace_disabled_never_reads_clock_and_calls_work_once() {
        let trace = Trace::disabled();
        assert!(trace
            .record_with(RecordData::Event { stage: "disabled" }, || panic!(
                "clock must not run"
            ))
            .is_none());
        let calls = Cell::new(0);
        let result: Result<u32, &str> = trace.run("work", || {
            calls.set(calls.get() + 1);
            Err("original")
        });
        assert_eq!(result, Err("original"));
        assert_eq!(calls.get(), 1);
        assert!(trace.close().is_none());
    }

    #[test]
    fn return_trace_checked_clock_has_no_fallback() {
        assert_eq!(checked_uptime(0, 12, 34), Some(12_000_000_034));
        for value in [
            checked_uptime(-1, 12, 34),
            checked_uptime(0, -1, 0),
            checked_uptime(0, 0, -1),
            checked_uptime(0, 0, 1_000_000_000),
            checked_uptime(0, i64::MAX, 0),
        ] {
            assert_eq!(value, None);
        }
        let owner = request();
        let trace = owner.trace();
        trace.record_with(
            RecordData::Event {
                stage: "missing_clock",
            },
            || None,
        );
        let snapshot = trace.close().unwrap();
        assert!(snapshot.clock_unavailable);
        assert!(snapshot.records[0].unwrap().uptime_ns.is_none());
        assert!(snapshot.json().unwrap().contains(CLOCK_DOMAIN));
    }

    #[test]
    fn return_trace_buffer_is_bounded_and_closed_only_once() {
        let owner = request();
        let trace = owner.trace();
        let clocks = Cell::new(0);
        for _ in 0..MAX_RECORDS + 8 {
            trace.record_with(RecordData::Event { stage: "bounded" }, || {
                clocks.set(clocks.get() + 1);
                Some(1)
            });
        }
        assert_eq!(clocks.get(), MAX_RECORDS);
        let snapshot = trace.close().unwrap();
        assert_eq!(snapshot.count, MAX_RECORDS);
        assert!(snapshot.truncated);
        assert!(trace
            .record_with(RecordData::Event { stage: "late" }, || panic!(
                "closed clock must not run"
            ))
            .is_none());
        assert!(trace.close().is_none());
        let json = snapshot.json().unwrap();
        assert!(json.contains("\"late_events_after_close\":\"not_collected\""));
        assert!(json.len() < 32_768);
    }

    #[test]
    fn return_trace_contention_drops_diagnostic_without_waiting() {
        let owner = request();
        let trace = owner.trace();
        let buffer = trace.0.as_ref().unwrap();
        let lock = buffer.state.lock().unwrap();
        assert!(trace
            .record_with(RecordData::Event { stage: "busy" }, || panic!(
                "busy clock must not run"
            ))
            .is_none());
        drop(lock);
        let snapshot = trace.close().unwrap();
        assert!(snapshot.truncated);
        assert_eq!(snapshot.count, 0);
    }

    #[test]
    fn return_trace_backward_clock_is_marked_without_rewriting_sample() {
        let owner = request();
        let trace = owner.trace();
        trace.record_with(RecordData::Event { stage: "first" }, || Some(100));
        trace.record_with(RecordData::Event { stage: "backward" }, || Some(99));
        let snapshot = trace.close().unwrap();
        assert!(snapshot.clock_unavailable);
        assert_eq!(snapshot.records[0].unwrap().uptime_ns, Some(100));
        assert_eq!(snapshot.records[1].unwrap().uptime_ns, Some(99));
    }

    #[test]
    fn return_trace_run_preserves_error_success_and_once_only_work() {
        let owner = request();
        let trace = owner.trace();
        let calls = Cell::new(0);
        let error = Box::new(73);
        let pointer = &*error as *const i32;
        let result: Result<(), Box<i32>> = trace.run("failure", || {
            calls.set(calls.get() + 1);
            Err(error)
        });
        assert_eq!(&*result.unwrap_err() as *const i32, pointer);
        assert_eq!(
            trace.run("success", || {
                calls.set(calls.get() + 1);
                Ok::<_, ()>(42)
            }),
            Ok(42)
        );
        assert_eq!(calls.get(), 2);
        let snapshot = trace.close().unwrap();
        assert_eq!(snapshot.count, 4);
        assert!(matches!(
            snapshot.records[1].unwrap().data,
            RecordData::Finish { ok: false, .. }
        ));
        assert!(matches!(
            snapshot.records[3].unwrap().data,
            RecordData::Finish { ok: true, .. }
        ));
    }

    #[test]
    fn return_trace_unfinished_span_marks_interruption_without_swallowing_work_panic() {
        let owner = request();
        let trace = owner.trace();
        let result = catch_unwind(AssertUnwindSafe(|| {
            trace.run::<(), ()>("panic", || panic!("test work panic"))
        }));
        assert!(result.is_err());
        let snapshot = trace.close().unwrap();
        assert_eq!(snapshot.count, 2);
        assert!(matches!(
            snapshot.records[1].unwrap().data,
            RecordData::Interrupted {
                stage: "panic",
                begin_seq: Some(0)
            }
        ));
    }

    #[test]
    fn return_trace_clock_panic_and_invalid_label_are_only_diagnostic_loss() {
        let owner = request();
        let trace = owner.trace();
        trace.event("not a permitted label");
        assert!(trace
            .record_with(RecordData::Event { stage: "clock" }, || panic!(
                "test clock panic"
            ))
            .is_none());
        let snapshot = trace.close().unwrap();
        assert!(snapshot.truncated);
        assert!(snapshot.clock_unavailable);
        let json = snapshot.json().unwrap();
        assert!(!json.contains("not a permitted label"));
        assert!(json.contains("invalid_stage"));
    }

    #[test]
    fn return_trace_window_facts_preserve_unknown_without_private_text() {
        let owner = request();
        let trace = owner.trace();
        trace.window_scan(2, true);
        trace.window(3, Some(false), true);
        trace.window(5, None, false);
        let snapshot = trace.close().unwrap();
        assert_eq!(snapshot.count, 3);
        assert!(matches!(
            snapshot.records[2].unwrap().data,
            RecordData::Window {
                minimized: None,
                is_target: false,
                ..
            }
        ));
        let json = snapshot.json().unwrap();
        for forbidden in ["token", "pointer", "cookie", "title", "text"] {
            assert!(!json.contains(forbidden));
        }
    }
}
