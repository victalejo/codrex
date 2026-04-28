//! Append-only JSONL log for orchestrator decisions.
//!
//! Every stage of every delegation emits one row to
//! `<dir>/runs-YYYY-MM-DD.jsonl`. Date is in UTC and rotates at the
//! day boundary — a fresh file per UTC day. Rows are append-only,
//! one row per [`crate::traits::LogStage`] event.
//!
//! ## Wire shape
//!
//! ```jsonc
//! {
//!   "v": 1,                              // schema version, locked
//!   "ts": "2026-04-27T16:30:00.123Z",    // RFC 3339 UTC, ms precision
//!   "run_id": "019dcfb1-...",            // matches DelegationSpec.run_id
//!   "parent_run_id": null,               // Some(uuid) for nested
//!   "attempt": 0,                         // retry counter
//!   "stage": "dispatch_end",             // see LogStage variants
//!   "latency_ms": 2195,                  // wall-clock since pipeline start
//!   "payload": { /* stage-specific */ }
//! }
//! ```
//!
//! ## Concurrency
//!
//! Writes use `OpenOptions::append(true)`. POSIX guarantees atomic
//! appends for writes ≤ `PIPE_BUF` (typically 4096 bytes). Phase 3
//! payloads stay well below that — a guard in `record` truncates the
//! `payload` field with a `__truncated__: true` sentinel if a single
//! line would exceed 3 KiB. Concurrent appends from multiple
//! orchestrator runs in the same process therefore interleave at
//! line granularity without locking.
//!
//! ## Failure mode
//!
//! Logging is fire-and-forget on the orchestrator's hot path. Disk
//! errors (permission denied, ENOSPC, etc.) are surfaced via
//! `tracing::error!` and the log call returns normally. Losing a log
//! line is strictly worse than aborting a delegation, so we choose
//! to lose the line.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::DateTime;
use chrono::Utc;
use serde::Serialize;
use tracing::error;

use crate::context::DelegationContext;
use crate::traits::DecisionLog;
use crate::traits::LogStage;

const SCHEMA_VERSION: u8 = 1;
const PAYLOAD_MAX_BYTES: usize = 3 * 1024;

/// Source of "now" — abstracted so tests can pin the clock and
/// exercise day-rotation deterministically.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Default `Clock` impl that calls `chrono::Utc::now()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// File-backed JSONL log. Construct with `new` for production,
/// `with_clock` for tests that need a deterministic timestamp.
#[derive(Clone)]
pub struct JsonlDecisionLog {
    dir: PathBuf,
    clock: Arc<dyn Clock>,
}

impl JsonlDecisionLog {
    /// Construct a logger that will write to `<dir>/runs-YYYY-MM-DD.jsonl`.
    /// The directory is created on first write if it doesn't exist.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            clock: Arc::new(SystemClock),
        }
    }

    /// Test-only constructor with an injectable clock. Marked `pub` so
    /// downstream crates can build deterministic integration tests.
    pub fn with_clock(dir: PathBuf, clock: Arc<dyn Clock>) -> Self {
        Self { dir, clock }
    }

    /// Compute the file path for today's log given the configured clock.
    /// Pure function exposed for tests.
    pub fn current_path(&self) -> PathBuf {
        let now = self.clock.now();
        let stem = now.format("runs-%Y-%m-%d.jsonl").to_string();
        self.dir.join(stem)
    }

    fn write_line(&self, line: &str) -> std::io::Result<()> {
        if let Err(err) = std::fs::create_dir_all(&self.dir) {
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err);
            }
        }
        let path = self.current_path();
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Build the full payload (line + LF) in memory and emit it in
        // ONE `write_all` so concurrent appenders see line-granular
        // interleaving without partial-line corruption. POSIX
        // guarantees atomicity for a single write ≤ PIPE_BUF
        // (≥ 4096 bytes on every platform we target). PAYLOAD_MAX_BYTES
        // keeps us well under that cap; the few framing bytes around
        // the payload don't push us over.
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        file.write_all(&buf)?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct LogRow<'a> {
    v: u8,
    ts: String,
    run_id: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_run_id: Option<uuid::Uuid>,
    attempt: u8,
    stage: LogStage,
    latency_ms: u64,
    payload: &'a serde_json::Value,
}

#[async_trait]
impl DecisionLog for JsonlDecisionLog {
    async fn record(&self, ctx: &DelegationContext, stage: LogStage, payload: serde_json::Value) {
        let payload_serialized =
            serde_json::to_string(&payload).unwrap_or_else(|_| "null".to_string());
        let payload_owned;
        let payload_ref: &serde_json::Value = if payload_serialized.len() > PAYLOAD_MAX_BYTES {
            payload_owned = serde_json::json!({
                "__truncated__": true,
                "original_len_bytes": payload_serialized.len(),
                "preview": &payload_serialized[..256.min(payload_serialized.len())],
            });
            &payload_owned
        } else {
            &payload
        };

        let row = LogRow {
            v: SCHEMA_VERSION,
            ts: self.clock.now().to_rfc3339(),
            run_id: ctx.run_id,
            parent_run_id: ctx.parent_run_id,
            attempt: ctx.attempt,
            stage,
            latency_ms: ctx.latency_ms(),
            payload: payload_ref,
        };
        let line = match serde_json::to_string(&row) {
            Ok(s) => s,
            Err(err) => {
                error!(
                    target: "codrex::orchestrator::log",
                    error = %err,
                    stage = ?stage,
                    "failed to serialize log row; dropping event"
                );
                return;
            }
        };
        if let Err(err) = self.write_line(&line) {
            error!(
                target: "codrex::orchestrator::log",
                error = %err,
                path = %self.current_path().display(),
                stage = ?stage,
                "failed to append decision log row; dropping event"
            );
        }
    }
}

/// In-memory log used by orchestrator unit tests so they don't touch
/// disk. Records every event in insertion order; `events()` returns a
/// snapshot for assertions.
#[derive(Debug, Default, Clone)]
pub struct InMemoryDecisionLog {
    events: Arc<std::sync::Mutex<Vec<RecordedEvent>>>,
}

#[derive(Debug, Clone)]
pub struct RecordedEvent {
    pub run_id: uuid::Uuid,
    pub parent_run_id: Option<uuid::Uuid>,
    pub attempt: u8,
    pub stage: LogStage,
    pub payload: serde_json::Value,
}

impl InMemoryDecisionLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<RecordedEvent> {
        self.events.lock().expect("poisoned").clone()
    }
}

#[async_trait]
impl DecisionLog for InMemoryDecisionLog {
    async fn record(&self, ctx: &DelegationContext, stage: LogStage, payload: serde_json::Value) {
        self.events.lock().expect("poisoned").push(RecordedEvent {
            run_id: ctx.run_id,
            parent_run_id: ctx.parent_run_id,
            attempt: ctx.attempt,
            stage,
            payload,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DelegationSpec;
    use chrono::TimeZone;
    use pretty_assertions::assert_eq;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Test clock that returns whatever time has been pinned via
    /// `set`, defaulting to UNIX epoch on construction.
    #[derive(Debug)]
    struct FixedClock(Mutex<DateTime<Utc>>);

    impl FixedClock {
        fn at(when: DateTime<Utc>) -> Arc<Self> {
            Arc::new(Self(Mutex::new(when)))
        }
        fn set(&self, when: DateTime<Utc>) {
            *self.0.lock().unwrap() = when;
        }
    }

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    fn ctx() -> DelegationContext {
        let spec = DelegationSpec::new_bare("hello").unwrap();
        DelegationContext::for_top_level(&spec)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn writes_one_line_per_record_call() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::at(Utc.with_ymd_and_hms(2026, 4, 27, 16, 0, 0).unwrap());
        let log = JsonlDecisionLog::with_clock(dir.path().to_path_buf(), clock);

        let c = ctx();
        log.record(&c, LogStage::Classify, serde_json::json!({"ok":true}))
            .await;
        log.record(&c, LogStage::DispatchStart, serde_json::json!({}))
            .await;
        log.record(&c, LogStage::DispatchEnd, serde_json::json!({"tokens":42}))
            .await;

        let path = dir.path().join("runs-2026-04-27.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(v["v"], 1);
            assert_eq!(v["run_id"], serde_json::json!(c.run_id.to_string()));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rotates_to_a_new_file_when_the_utc_day_advances() {
        let dir = TempDir::new().unwrap();
        let clock = FixedClock::at(Utc.with_ymd_and_hms(2026, 4, 27, 23, 59, 0).unwrap());
        let log = JsonlDecisionLog::with_clock(dir.path().to_path_buf(), clock.clone());

        let c = ctx();
        log.record(&c, LogStage::Classify, serde_json::json!({}))
            .await;

        clock.set(Utc.with_ymd_and_hms(2026, 4, 28, 0, 1, 0).unwrap());
        log.record(&c, LogStage::DispatchEnd, serde_json::json!({}))
            .await;

        let day1 = dir.path().join("runs-2026-04-27.jsonl");
        let day2 = dir.path().join("runs-2026-04-28.jsonl");
        assert!(day1.exists() && day2.exists());
        assert_eq!(std::fs::read_to_string(&day1).unwrap().lines().count(), 1);
        assert_eq!(std::fs::read_to_string(&day2).unwrap().lines().count(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writes_do_not_corrupt_lines() {
        let dir = TempDir::new().unwrap();
        let log = Arc::new(JsonlDecisionLog::new(dir.path().to_path_buf()));

        let mut handles = Vec::new();
        for i in 0..50 {
            let log = Arc::clone(&log);
            handles.push(tokio::spawn(async move {
                let c = ctx();
                log.record(&c, LogStage::DispatchStart, serde_json::json!({"i": i}))
                    .await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let path = log.current_path();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 50, "every line must be present");
        for line in lines {
            // The proof of "no corruption" is that every line parses as
            // valid JSON. Interleaved bytes from concurrent writers
            // would tip serde over.
            let _: serde_json::Value =
                serde_json::from_str(line).expect("every line must be valid JSON");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn truncates_oversized_payloads_with_sentinel() {
        let dir = TempDir::new().unwrap();
        let log = JsonlDecisionLog::new(dir.path().to_path_buf());
        let huge = "x".repeat(PAYLOAD_MAX_BYTES + 100);
        log.record(
            &ctx(),
            LogStage::DispatchEnd,
            serde_json::json!({"blob": huge}),
        )
        .await;

        let content = std::fs::read_to_string(log.current_path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(v["payload"]["__truncated__"], serde_json::json!(true));
        assert!(v["payload"]["original_len_bytes"].as_u64().unwrap() > PAYLOAD_MAX_BYTES as u64);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn record_does_not_panic_on_unwriteable_dir() {
        // Path that cannot exist as a directory (parent is a file).
        let tmp = TempDir::new().unwrap();
        let blocking_file = tmp.path().join("file");
        std::fs::write(&blocking_file, b"x").unwrap();
        let bad_dir = blocking_file.join("under_a_file");
        let log = JsonlDecisionLog::new(bad_dir);
        // Must not panic — fire-and-forget contract.
        log.record(&ctx(), LogStage::Classify, serde_json::json!({}))
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn in_memory_log_records_every_event_in_order() {
        let log = InMemoryDecisionLog::new();
        let c = ctx();
        log.record(&c, LogStage::Classify, serde_json::json!({"a":1}))
            .await;
        log.record(&c, LogStage::Audit, serde_json::json!({"b":2}))
            .await;
        let events = log.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].stage, LogStage::Classify);
        assert_eq!(events[1].stage, LogStage::Audit);
        assert_eq!(events[0].payload, serde_json::json!({"a":1}));
    }

    #[test]
    fn current_path_is_a_pure_function_of_clock_state() {
        let dir = PathBuf::from("/tmp/x");
        let clock = FixedClock::at(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        let log = JsonlDecisionLog::with_clock(dir, clock);
        assert_eq!(
            log.current_path(),
            PathBuf::from("/tmp/x/runs-2026-01-01.jsonl")
        );
    }
}
