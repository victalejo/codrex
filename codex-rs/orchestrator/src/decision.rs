//! `AuditDecision` — the verdict the auditor returns after evaluating a
//! delegated turn against its `DelegationSpec.acceptance` criteria.
//!
//! The four variants are deliberately distinct so the orchestrator's
//! retry/escalate/drop logic can match exhaustively. Adding a fifth
//! variant in the future requires the compiler-checked exhaustiveness
//! pass, which is the point.

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AuditDecision {
    /// All acceptance criteria passed. Apply the response and finish.
    /// The rationale string is for the JSONL log + user-facing summary,
    /// not for downstream control flow.
    Ok { rationale: String },

    /// Acceptance failed but the failure looks recoverable (test failed
    /// with a tractable error, output didn't match a pattern but the
    /// gist is right). Re-dispatch with `feedback` injected into the
    /// prompt, up to `DelegationSpec.max_retries` attempts.
    ///
    /// `attempt` is the 1-indexed retry number — the FIRST retry is
    /// `attempt=1`, NOT 0. The auditor doesn't manage the counter; the
    /// orchestrator's retry loop assigns it before the next dispatch.
    Retry {
        feedback: RetryFeedback,
        attempt: u8,
    },

    /// The failure looks structural (forbidden pattern matched, custom
    /// check failed irrecoverably, MiniMax asked for clarification we
    /// can't auto-resolve). Surface to the user with `reason` + a
    /// concrete `blocking_issue` they can act on, and exit non-zero.
    Escalate {
        reason: String,
        blocking_issue: String,
    },

    /// The output is unsalvageable (response was empty, dispatch
    /// errored before a real response, the spec was invalid retroactively).
    /// Log and abort silently — no user input expected.
    Drop { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryFeedback {
    pub failed_criteria: Vec<FailedCriterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailedCriterion {
    pub name: String,
    pub kind: CriterionKind,
    pub details: FailureDetails,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CriterionKind {
    OutputMatches,
    TestsPass,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailureDetails {
    OutputMatches {
        regex: String,
        output_excerpt: String,
    },
    TestsPass {
        exit_code: i32,
        stderr_excerpt: String,
        command: Vec<String>,
    },
    Custom {
        exit_code: i32,
        stderr_excerpt: String,
    },
}

impl AuditDecision {
    /// Returns `true` when the orchestrator should keep iterating (i.e.
    /// the run isn't terminal yet). Convenience for the dispatch loop.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Ok { .. } | Self::Escalate { .. } | Self::Drop { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn audit_decision_serde_round_trip_for_each_variant() {
        let retry_feedback = RetryFeedback {
            failed_criteria: vec![
                FailedCriterion {
                    name: "output_matches[0]".to_string(),
                    kind: CriterionKind::OutputMatches,
                    details: FailureDetails::OutputMatches {
                        regex: r"^DONE$".to_string(),
                        output_excerpt: "x".to_string(),
                    },
                },
                FailedCriterion {
                    name: "tests_pass".to_string(),
                    kind: CriterionKind::TestsPass,
                    details: FailureDetails::TestsPass {
                        exit_code: 1,
                        stderr_excerpt: "boom".to_string(),
                        command: vec!["pytest".to_string()],
                    },
                },
                FailedCriterion {
                    name: "custom[0:health]".to_string(),
                    kind: CriterionKind::Custom,
                    details: FailureDetails::Custom {
                        exit_code: 2,
                        stderr_excerpt: "fail".to_string(),
                    },
                },
            ],
        };
        let cases = [
            AuditDecision::Ok {
                rationale: "all checks passed".into(),
            },
            AuditDecision::Retry {
                feedback: retry_feedback,
                attempt: 1,
            },
            AuditDecision::Escalate {
                reason: "matched forbidden pattern".into(),
                blocking_issue: "response uses `unsafe`".into(),
            },
            AuditDecision::Drop {
                reason: "MiniMax returned empty body".into(),
            },
        ];
        for case in cases {
            let json = serde_json::to_string(&case).unwrap();
            let back: AuditDecision = serde_json::from_str(&json).unwrap();
            assert_eq!(back, case);
        }
    }

    #[test]
    fn audit_decision_serde_uses_snake_case_verdict_tag() {
        // Lock on-disk shape — schema migrations should be deliberate.
        let s = serde_json::to_string(&AuditDecision::Ok {
            rationale: "x".into(),
        })
        .unwrap();
        assert_eq!(s, r#"{"verdict":"ok","rationale":"x"}"#);
    }

    #[test]
    fn is_terminal_is_true_for_all_but_retry() {
        assert!(
            AuditDecision::Ok {
                rationale: "x".into()
            }
            .is_terminal()
        );
        assert!(
            AuditDecision::Escalate {
                reason: "x".into(),
                blocking_issue: "y".into()
            }
            .is_terminal()
        );
        assert!(AuditDecision::Drop { reason: "x".into() }.is_terminal());
        assert!(
            !AuditDecision::Retry {
                feedback: RetryFeedback {
                    failed_criteria: vec![],
                },
                attempt: 1
            }
            .is_terminal()
        );
    }
}
