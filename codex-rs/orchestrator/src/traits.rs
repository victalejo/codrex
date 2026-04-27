//! Trait stubs for the orchestrator pipeline.
//!
//! Phase 3 commit 1 lands the **shapes** only. Each trait gets a
//! concrete implementation in a later commit:
//!
//! ```text
//! Trait          | Lands in commit
//! ---------------|---------------------------------------------------
//! Classifier     | 6 (RulesClassifier) + 7 (LlmClassifier)
//! DispatchSink   | 3 (MinimaxDispatchSink, --force-delegate)
//! Auditor        | 4 (PatternAuditor)
//! DecisionLog    | 2 (JsonlDecisionLog)
//! ```
//!
//! Defining the traits this early forces commit-1 to settle the
//! interfaces the rest of the phase will speak. Concrete implementations
//! arrive behind these traits, so the orchestrator's main loop (commit
//! 3+) can be written against the abstract pipeline and tested with
//! mocks without committing to a particular classifier or auditor.

use async_trait::async_trait;
use serde::Deserialize;
use serde::Serialize;

use crate::context::DelegationContext;
use crate::decision::AuditDecision;
use crate::spec::DelegationSpec;

/// Result of running a classifier against a user prompt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ClassificationOutcome {
    /// Build a delegated turn from `spec`. The orchestrator hands this
    /// to a `DispatchSink`.
    Delegate { spec: DelegationSpec },
    /// Don't delegate; run the prompt through the default agent path.
    /// The carrier `reason` is logged so we can later analyze why
    /// classification declined a delegation.
    PassThrough { reason: String },
}

/// Decides whether a user prompt should be delegated and, if so, with
/// what spec.
#[async_trait]
pub trait Classifier: Send + Sync {
    async fn classify(&self, prompt: &str) -> ClassificationOutcome;
}

/// Result of dispatching a delegated turn to a model.
///
/// Phase 3 commit 3 captures only the textual response; commits 4+ will
/// extend with applied diffs / tool-call traces / per-step latency once
/// we wire `run_turn` reuse and need that data for audit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DispatchOutcome {
    pub response_text: String,
    /// Total wall-clock for the dispatch (model API call + streaming
    /// drain). Useful for the JSONL log even when the model itself
    /// reports its own latency.
    pub latency_ms: u64,
    /// `Some` when the model emitted token-usage metadata; orchestrator
    /// telemetry correlates this with the spec's `run_id` so cost
    /// attribution stays honest across delegations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

/// Sends a delegated turn to a model and waits for the result.
///
/// Implementations decide which model: Phase 3 ships
/// `MinimaxDispatchSink` (commit 3) — Phase 4-5 may add OpenAI sinks
/// for nested delegations or specialized sinks for offline replay.
#[async_trait]
pub trait DispatchSink: Send + Sync {
    async fn dispatch(
        &self,
        spec: &DelegationSpec,
        ctx: &DelegationContext,
    ) -> Result<DispatchOutcome, DispatchError>;
}

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("model returned an error: {0}")]
    Model(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// The model returned a `CLARIFY: ...` prefixed response asking for
    /// disambiguation. Phase 3 commits 3-7 surface this as a non-zero
    /// exit; commit 8 implements the round-trip.
    #[error("model requested clarification: {question}")]
    ClarificationRequested { question: String },
}

/// Evaluates a `DispatchOutcome` against a `DelegationSpec` and decides
/// the next step.
///
/// Phase 3 commit 4 ships `PatternAuditor` (handles forbidden_patterns
/// + simple acceptance criteria). Commit 5 adds the test runner branch.
#[async_trait]
pub trait Auditor: Send + Sync {
    async fn audit(
        &self,
        spec: &DelegationSpec,
        ctx: &DelegationContext,
        outcome: &DispatchOutcome,
    ) -> AuditDecision;
}

/// Append-only structured event log for a single delegation pipeline.
///
/// Implementations control the storage backend; Phase 3 ships
/// `JsonlDecisionLog` (commit 2) writing to
/// `~/.codrex/runs/runs-YYYY-MM-DD.jsonl`. The tests-friendly
/// `InMemoryDecisionLog` exists alongside for unit tests of the
/// orchestrator loop without touching disk.
///
/// Logging is fire-and-forget on the orchestrator's hot path: log
/// failures must not break the delegation. Implementations should
/// surface their own `tracing::error!` on failure but always return
/// `Ok(())` to the caller.
#[async_trait]
pub trait DecisionLog: Send + Sync {
    /// Record one event in the lifecycle of a delegation. The
    /// implementation is responsible for serializing `payload` (it's
    /// already a `serde_json::Value` so the trait stays object-safe and
    /// the caller chooses the structure per-stage).
    async fn record(&self, ctx: &DelegationContext, stage: LogStage, payload: serde_json::Value);
}

/// Pipeline stage tags the JSONL log uses to scope a payload.
///
/// One stage per row in the JSONL file makes log analysis trivial: grep
/// by `"stage":"audit"` and you have every audit decision across every
/// run. Adding a new stage is intentional — touching this enum forces a
/// schema-version bump in the JSONL header.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogStage {
    Classify,
    DispatchStart,
    DispatchEnd,
    Audit,
    Decision,
    Clarify,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn log_stage_serde_uses_snake_case() {
        let s = serde_json::to_string(&LogStage::DispatchStart).unwrap();
        assert_eq!(s, r#""dispatch_start""#);
    }

    #[test]
    fn classification_outcome_serde_round_trip() {
        let cases = [
            ClassificationOutcome::Delegate {
                spec: crate::DelegationSpec::new_bare("intent").unwrap(),
            },
            ClassificationOutcome::PassThrough {
                reason: "no rules matched".into(),
            },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).unwrap();
            let back: ClassificationOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(back, case);
        }
    }

    #[test]
    fn dispatch_outcome_serde_round_trip() {
        let outcome = DispatchOutcome {
            response_text: "ok".into(),
            latency_ms: 1234,
            total_tokens: Some(42),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let back: DispatchOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, outcome);
    }
}
