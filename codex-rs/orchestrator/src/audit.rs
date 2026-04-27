//! Auditor implementations.
//!
//! Phase 3 commit 3 ships only [`PlaceholderAuditor`] which always
//! returns [`AuditDecision::Ok`]. Real audit logic (forbidden patterns,
//! test runners, custom scripts) lands in commit 4 as `PatternAuditor`.
//!
//! The placeholder exists so commit 3 can wire the full pipeline
//! end-to-end before the audit phase has actual policy. Without it the
//! orchestrate subcommand couldn't return — there'd be no way to
//! transition past `Audit` into `Decision`. Keeping the trait stable
//! means commit 4 swaps the auditor without touching the loop.

use async_trait::async_trait;

use crate::context::DelegationContext;
use crate::decision::AuditDecision;
use crate::spec::DelegationSpec;
use crate::traits::Auditor;
use crate::traits::DispatchOutcome;

/// Auditor that always emits `AuditDecision::Ok`. Phase 3 commit 3
/// only — replaced in commit 4 with the real `PatternAuditor`.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlaceholderAuditor;

impl PlaceholderAuditor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Auditor for PlaceholderAuditor {
    async fn audit(
        &self,
        _spec: &DelegationSpec,
        _ctx: &DelegationContext,
        _outcome: &DispatchOutcome,
    ) -> AuditDecision {
        AuditDecision::Ok {
            rationale: "phase 3 LITE: PlaceholderAuditor — no acceptance checks performed. \
                        Real audit logic arrives in commit 4 (PatternAuditor)."
                .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::DelegationSpec;

    #[tokio::test(flavor = "current_thread")]
    async fn placeholder_auditor_always_returns_ok() {
        let spec = DelegationSpec::new_bare("hi").unwrap();
        let ctx = DelegationContext::for_top_level(&spec);
        let outcome = DispatchOutcome {
            response_text: "result".into(),
            latency_ms: 10,
            total_tokens: None,
        };
        let decision = PlaceholderAuditor::new().audit(&spec, &ctx, &outcome).await;
        assert!(matches!(decision, AuditDecision::Ok { .. }));
    }
}
