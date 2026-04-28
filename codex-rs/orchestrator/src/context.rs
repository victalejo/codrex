//! `DelegationContext` — runtime metadata that flows through the
//! orchestrator pipeline alongside a `DelegationSpec`.
//!
//! Where `DelegationSpec` describes WHAT to do, `DelegationContext`
//! tracks WHILE-DOING-IT state: identifiers for log correlation, the
//! current attempt number, the wall-clock origin so latency can be
//! computed at any stage.
//!
//! Crucially, `run_id` here is the SAME value as
//! `DelegationSpec.run_id` (the spec is the source of truth) — context
//! caches it so callers don't have to thread the spec everywhere just to
//! read the id. `parent_run_id` likewise mirrors the spec.

use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::decision::RetryFeedback;
use crate::spec::DelegationSpec;

/// Lightweight context object passed to every stage in the pipeline.
///
/// Cheaply cloneable (`Copy` on the id fields, `Instant` is also Copy).
/// `Serialize`/`Deserialize` are implemented for the JSONL log; they
/// skip `started_at` because `Instant` is process-local and not
/// serializable in any meaningful sense.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationContext {
    pub run_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,
    /// 0 on the initial dispatch, 1 on first retry, etc. The auditor
    /// reads this when constructing `AuditDecision::Retry { attempt }`.
    #[serde(default)]
    pub attempt: u8,
    /// Wall-clock origin. Skipped on the wire — `started_at` exists for
    /// `latency_since_start()` only, the JSONL log carries an explicit
    /// `started_at_unix_ms` field that's filled by the logger.
    #[serde(skip, default = "Instant::now")]
    pub started_at: Instant,
    #[serde(skip)]
    pub retry_feedback: Option<RetryFeedback>,
}

impl DelegationContext {
    /// Build a fresh context for a top-level delegation. Mirrors
    /// `DelegationSpec.run_id` and starts the latency clock now.
    pub fn for_top_level(spec: &DelegationSpec) -> Self {
        Self {
            run_id: spec.run_id,
            parent_run_id: None,
            attempt: 0,
            started_at: Instant::now(),
            retry_feedback: None,
        }
    }

    /// Build a child context for a nested delegation. Phase 4-5 will
    /// use this; Phase 3 only ever calls `for_top_level`.
    pub fn for_nested(spec: &DelegationSpec, parent: &DelegationContext) -> Self {
        Self {
            run_id: spec.run_id,
            parent_run_id: Some(parent.run_id),
            attempt: 0,
            started_at: Instant::now(),
            retry_feedback: None,
        }
    }

    /// Return a context with `attempt` advanced by 1. Called by the
    /// orchestrator's retry loop before the next dispatch.
    pub fn next_attempt(&self) -> Self {
        Self {
            attempt: self.attempt.saturating_add(1),
            ..self.clone()
        }
    }

    pub fn next_attempt_with_feedback(&self, retry_feedback: RetryFeedback) -> Self {
        Self {
            attempt: self.attempt.saturating_add(1),
            retry_feedback: Some(retry_feedback),
            ..self.clone()
        }
    }

    /// Wall-clock elapsed since `started_at`, in milliseconds.
    pub fn latency_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn spec() -> DelegationSpec {
        DelegationSpec::new_bare("hello").unwrap()
    }

    #[test]
    fn for_top_level_mirrors_spec_run_id_and_clears_parent() {
        let s = spec();
        let ctx = DelegationContext::for_top_level(&s);
        assert_eq!(ctx.run_id, s.run_id);
        assert_eq!(ctx.parent_run_id, None);
        assert_eq!(ctx.attempt, 0);
    }

    #[test]
    fn for_nested_links_parent_run_id() {
        let parent_spec = spec();
        let parent_ctx = DelegationContext::for_top_level(&parent_spec);
        let child_spec = spec();
        let child_ctx = DelegationContext::for_nested(&child_spec, &parent_ctx);
        assert_eq!(child_ctx.run_id, child_spec.run_id);
        assert_eq!(child_ctx.parent_run_id, Some(parent_ctx.run_id));
    }

    #[test]
    fn next_attempt_increments_attempt_counter() {
        let s = spec();
        let ctx = DelegationContext::for_top_level(&s);
        let r1 = ctx.next_attempt();
        let r2 = r1.next_attempt();
        assert_eq!(r1.attempt, 1);
        assert_eq!(r2.attempt, 2);
        assert_eq!(r2.run_id, ctx.run_id);
    }

    #[test]
    fn next_attempt_saturates_at_u8_max() {
        let s = spec();
        let mut ctx = DelegationContext::for_top_level(&s);
        ctx.attempt = u8::MAX;
        let next = ctx.next_attempt();
        assert_eq!(next.attempt, u8::MAX);
    }

    #[test]
    fn started_at_is_skipped_on_wire() {
        let s = spec();
        let ctx = DelegationContext::for_top_level(&s);
        let json = serde_json::to_string(&ctx).unwrap();
        assert!(!json.contains("started_at"));
        let back: DelegationContext = serde_json::from_str(&json).unwrap();
        // `started_at` defaulted to a fresh `Instant::now()` on
        // deserialize. We can't compare instants across calls, but it
        // must not have failed to parse.
        assert_eq!(back.run_id, ctx.run_id);
    }
}
