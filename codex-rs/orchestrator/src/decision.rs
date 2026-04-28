//! `AuditDecision` — the verdict the auditor returns after evaluating a
//! delegated turn against its `DelegationSpec.acceptance` criteria.
//!
//! The four variants are deliberately distinct so the orchestrator's
//! retry/escalate/drop logic can match exhaustively. Adding a fifth
//! variant in the future requires the compiler-checked exhaustiveness
//! pass, which is the point.

use regex::Regex;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

const SIGNATURE_PREVIEW_LIMIT: usize = 200;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryFeedback {
    pub failed_criteria: Vec<FailedCriterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedCriterion {
    pub name: String,
    pub kind: CriterionKind,
    pub details: FailureDetails,
}

impl FailedCriterion {
    pub fn error_signature(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.name.as_bytes());
        hasher.update(b"|");
        hasher.update(self.kind.kind_id().as_bytes());
        hasher.update(b"|");
        hasher.update(self.normalized_failure_details().as_bytes());
        let hash = hasher.finalize();
        hash.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn normalized_failure_details(&self) -> String {
        match &self.details {
            FailureDetails::OutputMatches {
                regex,
                output_excerpt,
            } => format!(
                "regex={},output_excerpt={}",
                regex,
                normalize_signature_excerpt(output_excerpt),
            ),
            FailureDetails::TestsPass {
                exit_code,
                stderr_excerpt,
                command,
            } => {
                let normalized_command = command
                    .iter()
                    .map(|part| normalize_signature_path(part))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!(
                    "exit_code={exit_code},stderr_excerpt={},command={}",
                    normalize_signature_excerpt(stderr_excerpt),
                    normalized_command,
                )
            }
            FailureDetails::Custom {
                exit_code,
                stderr_excerpt,
            } => format!(
                "exit_code={exit_code},stderr_excerpt={}",
                normalize_signature_excerpt(stderr_excerpt),
            ),
        }
    }
}

fn normalize_signature_excerpt(input: &str) -> String {
    let normalized = input.replace('\n', " ").replace('\r', " ");
    let normalized = normalize_signature_paths(&normalized);
    let normalized = normalize_signature_noise(&normalized);
    normalized.chars().take(SIGNATURE_PREVIEW_LIMIT).collect()
}

fn normalize_signature_paths(text: &str) -> String {
    text.replace('\\', "/")
}

fn normalize_signature_noise(text: &str) -> String {
    let mut value = text.to_string();
    let uuid_re = Regex::new(
        r"\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b",
    )
    .expect("uuid regex");
    value = uuid_re.replace_all(&value, "[uuid]").to_string();
    let pid_re = Regex::new(r"(?i)\b(pid|process id|process)\s*[:=]?\s*\d+\b").expect("pid regex");
    value = pid_re.replace_all(&value, "[pid]").to_string();
    let timestamp_re =
        Regex::new(r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?\b")
            .expect("timestamp regex");
    timestamp_re.replace_all(&value, "[ts]").to_string()
}

fn normalize_signature_path(path: &str) -> String {
    normalize_signature_paths(&normalize_signature_noise(path))
}

impl CriterionKind {
    fn kind_id(&self) -> &'static str {
        match self {
            Self::OutputMatches => "output_matches",
            Self::TestsPass => "tests_pass",
            Self::Custom => "custom",
        }
    }
}

pub fn error_signature_for_retry_feedback(feedback: &RetryFeedback) -> Option<String> {
    feedback
        .failed_criteria
        .first()
        .map(FailedCriterion::error_signature)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CriterionKind {
    OutputMatches,
    TestsPass,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    fn failed_criterion_signature_is_stable_for_same_inputs() {
        let c1 = FailedCriterion {
            name: "output_matches[0]".to_string(),
            kind: CriterionKind::OutputMatches,
            details: FailureDetails::OutputMatches {
                regex: r"^DONE$".to_string(),
                output_excerpt: "abc 123".to_string(),
            },
        };
        let c2 = FailedCriterion {
            name: "output_matches[0]".to_string(),
            kind: CriterionKind::OutputMatches,
            details: FailureDetails::OutputMatches {
                regex: r"^DONE$".to_string(),
                output_excerpt: "abc 123".to_string(),
            },
        };
        assert_eq!(c1.error_signature(), c2.error_signature());
    }

    #[test]
    fn failed_criterion_signature_changes_on_failure_details_change() {
        let c1 = FailedCriterion {
            name: "output_matches[0]".to_string(),
            kind: CriterionKind::OutputMatches,
            details: FailureDetails::OutputMatches {
                regex: r"^DONE$".to_string(),
                output_excerpt: "abc 123".to_string(),
            },
        };
        let c2 = FailedCriterion {
            name: "output_matches[0]".to_string(),
            kind: CriterionKind::OutputMatches,
            details: FailureDetails::OutputMatches {
                regex: r"^DONE$".to_string(),
                output_excerpt: "abc 999".to_string(),
            },
        };
        assert_ne!(c1.error_signature(), c2.error_signature());
    }

    #[test]
    fn feedback_signature_uses_first_failed_criterion_only() {
        let feedback = RetryFeedback {
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
                        stderr_excerpt: "z".to_string(),
                        command: vec!["python3".to_string()],
                    },
                },
            ],
        };
        assert_eq!(
            error_signature_for_retry_feedback(&feedback).as_deref(),
            Some(feedback.failed_criteria[0].error_signature().as_str())
        );
    }

    #[test]
    fn normalize_signature_removes_uuid_like_noise() {
        let c = FailedCriterion {
            name: "tests_pass".to_string(),
            kind: CriterionKind::TestsPass,
            details: FailureDetails::TestsPass {
                exit_code: 1,
                stderr_excerpt:
                    "error at 7f3a4b89-1f3c-4f7c-9bc5-1a2b3c4d5e6f pid=1234 2026-04-27T10:00:00Z"
                        .to_string(),
                command: vec!["pytest".into()],
            },
        };
        let signature = c.error_signature();
        let second = FailedCriterion {
            name: "tests_pass".to_string(),
            kind: CriterionKind::TestsPass,
            details: FailureDetails::TestsPass {
                exit_code: 1,
                stderr_excerpt:
                    "error at 11111111-1111-1111-1111-111111111111 pid=9999 2026-04-28T11:00:00Z"
                        .to_string(),
                command: vec!["pytest".into()],
            },
        };
        assert_eq!(signature, second.error_signature());
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
