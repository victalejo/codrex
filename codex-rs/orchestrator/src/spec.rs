//! `DelegationSpec` and supporting value types.
//!
//! A spec is the structured contract handed from the classifier to the
//! dispatcher. It carries:
//!
//! - **What to do** (`intent`, `acceptance`, `expected_tests`)
//! - **What NOT to do** (`forbidden_patterns`)
//! - **What to reuse** (`utility_refs`)
//! - **How forgiving to be** (`max_retries`)
//!
//! Plus the run identity (`run_id`, `parent_run_id`, `created_at`) used
//! by the JSONL log to tie every stage of a delegation together.
//!
//! ## Validation
//!
//! `DelegationSpec` enforces invariants at construction time via
//! [`DelegationSpec::validate`]. This includes:
//!
//! - Non-empty intent.
//! - Every entry in `forbidden_patterns` compiles as a regex.
//! - Every `AcceptanceCriterion::OutputMatches` regex compiles.
//! - `AcceptanceCriterion::TestsPass` only valid when
//!   `expected_tests.is_some()`.
//! - `AcceptanceCriterion::NoForbiddenPatterns` only valid when
//!   `forbidden_patterns` is non-empty.
//! - `AcceptanceCriterion::Custom { name }` is non-empty.
//! - `max_retries <= 10` (sanity bound).
//!
//! Failing fast at parse keeps invalid specs out of the dispatch loop —
//! the audit phase can rely on every regex being already-validated.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::error::SpecError;

const DEFAULT_MAX_RETRIES: u8 = 2;
const MAX_ALLOWED_RETRIES: u8 = 10;

/// Structured plan for a single delegation. Built by the classifier,
/// consumed by dispatch + audit, logged in JSONL.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DelegationSpec {
    pub run_id: Uuid,
    /// `None` for a top-level delegation. `Some(parent.run_id)` when this
    /// delegation was spawned by another delegation (Phase 4-5
    /// nested orchestration). Phase 3 always sets this to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<Uuid>,

    /// The user-facing prompt or condensed intent. The dispatcher uses
    /// this verbatim as the user message of the delegated turn.
    pub intent: String,

    /// Acceptance criteria the auditor uses to decide
    /// [`crate::AuditDecision`]. Empty list ⇒ no audit signal beyond
    /// dispatch success (Phase 3 commit 3 happy-path mode).
    #[serde(default)]
    pub acceptance: Vec<AcceptanceCriterion>,

    /// Regex patterns the response must NOT match. Validated at
    /// construction time so the audit phase doesn't need to handle
    /// compile errors. Empty when no forbidden constraints apply.
    #[serde(default)]
    pub forbidden_patterns: Vec<String>,

    /// Optional test command to run after applying the response. Phase
    /// 3 LITE shape; Phase 4 will extend with `timeout`,
    /// `expected_exit_code`, structured-output parsers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_tests: Option<TestSpec>,

    /// Suggested code to reuse — names + signatures of utilities the
    /// delegate should prefer over re-implementing.
    #[serde(default)]
    pub utility_refs: Vec<UtilRef>,

    /// Maximum retry attempts on `AuditDecision::Retry`. Default 2.
    /// Bounded at 10 to prevent runaway delegations.
    #[serde(default = "default_max_retries")]
    pub max_retries: u8,

    pub created_at: SystemTime,
}

fn default_max_retries() -> u8 {
    DEFAULT_MAX_RETRIES
}

impl DelegationSpec {
    /// Convenience constructor for the most common shape: a bare intent
    /// with default retry budget and no acceptance criteria. Suitable
    /// for `--force-delegate` happy-path runs in commit 3.
    pub fn new_bare(intent: impl Into<String>) -> Result<Self, SpecError> {
        let spec = Self {
            run_id: Uuid::new_v4(),
            parent_run_id: None,
            intent: intent.into(),
            acceptance: Vec::new(),
            forbidden_patterns: Vec::new(),
            expected_tests: None,
            utility_refs: Vec::new(),
            max_retries: DEFAULT_MAX_RETRIES,
            created_at: SystemTime::now(),
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Run all parse-time invariants. Returns the first error
    /// encountered so callers see a single clear failure message rather
    /// than a list to triage.
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.intent.trim().is_empty() {
            return Err(SpecError::EmptyIntent);
        }
        if self.max_retries > MAX_ALLOWED_RETRIES {
            return Err(SpecError::MaxRetriesTooLarge(self.max_retries));
        }
        for (idx, pat) in self.forbidden_patterns.iter().enumerate() {
            regex::Regex::new(pat).map_err(|source| SpecError::InvalidForbiddenPattern {
                index: idx,
                source,
            })?;
        }
        for (idx, util) in self.utility_refs.iter().enumerate() {
            if util.symbol.trim().is_empty() {
                return Err(SpecError::EmptyUtilSymbol(idx));
            }
        }
        for (idx, criterion) in self.acceptance.iter().enumerate() {
            self.validate_criterion(idx, criterion)?;
        }
        if let Some(tests) = &self.expected_tests
            && tests.command.is_empty()
        {
            // Defensive: an empty command vector is a programming error
            // upstream — the LLM/rules emitter should never produce it.
            // Treat it as TestsPassWithoutExpectedTests-equivalent.
            return Err(SpecError::TestsPassWithoutExpectedTests(usize::MAX));
        }
        Ok(())
    }

    fn validate_criterion(
        &self,
        idx: usize,
        criterion: &AcceptanceCriterion,
    ) -> Result<(), SpecError> {
        match criterion {
            AcceptanceCriterion::OutputMatches { regex } => {
                regex::Regex::new(regex).map_err(|source| {
                    SpecError::InvalidOutputMatchesRegex {
                        index: idx,
                        source,
                    }
                })?;
            }
            AcceptanceCriterion::TestsPass => {
                if self.expected_tests.is_none() {
                    return Err(SpecError::TestsPassWithoutExpectedTests(idx));
                }
            }
            AcceptanceCriterion::NoForbiddenPatterns => {
                if self.forbidden_patterns.is_empty() {
                    return Err(SpecError::NoForbiddenPatternsWithoutPatterns(idx));
                }
            }
            AcceptanceCriterion::Custom { name, check } => {
                if name.trim().is_empty() {
                    return Err(SpecError::EmptyCustomName(idx));
                }
                if check.path.as_os_str().is_empty() {
                    return Err(SpecError::EmptyScriptPath);
                }
            }
        }
        Ok(())
    }
}

/// One acceptance condition the auditor must observe before declaring
/// `AuditDecision::Ok`. Multiple criteria combine as AND.
///
/// `TestsPass` and `NoForbiddenPatterns` reference data on the parent
/// `DelegationSpec` (`expected_tests`, `forbidden_patterns`); the spec
/// validator enforces the dependency at construction time so the audit
/// phase can assume both are populated when the criterion is present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcceptanceCriterion {
    /// The response (text + applied diffs concatenated) must match this
    /// regex. The pattern is validated at parse time.
    OutputMatches { regex: String },
    /// Run `DelegationSpec.expected_tests` and require exit 0. Requires
    /// `expected_tests.is_some()` on the parent spec.
    TestsPass,
    /// No forbidden_patterns regex matches the response. Requires
    /// `forbidden_patterns` non-empty on the parent spec.
    NoForbiddenPatterns,
    /// Run a user-supplied script; pass on exit 0. The auditor inherits
    /// the orchestrator's working dir unless the script changes it
    /// itself. Phase 3 LITE assumes the script is sandbox-friendly.
    Custom { name: String, check: ScriptRef },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UtilRef {
    pub path: PathBuf,
    pub symbol: String,
    /// Free-form signature hint surfaced to the delegate model
    /// ("fn validate_email(s: &str) -> Result<Email, ValidationError>").
    /// Optional because not every reference has a tractable signature
    /// (e.g. macros, complex generics).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Test command the auditor runs to evaluate `TestsPass`.
///
/// **Phase 3 LITE shape.** TODO #10: Phase 4 will extend with
/// `timeout: Option<Duration>`, `expected_exit_code: i32`, and a
/// structured-output parser (TAP / JUnit XML / cargo-nextest JSON) so
/// retries can include "which test failed" feedback rather than the
/// whole stdout/stderr blob.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestSpec {
    /// Argv-style: `command[0]` is the binary, the rest are args.
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
}

/// Reference to an executable a `Custom` acceptance criterion uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScriptRef {
    pub path: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn sample_spec() -> DelegationSpec {
        DelegationSpec {
            run_id: Uuid::nil(),
            parent_run_id: None,
            intent: "implement validate_email".into(),
            acceptance: vec![
                AcceptanceCriterion::OutputMatches {
                    regex: r"^OK".into(),
                },
                AcceptanceCriterion::NoForbiddenPatterns,
                AcceptanceCriterion::TestsPass,
            ],
            forbidden_patterns: vec!["unsafe".into(), r"std::mem::transmute".into()],
            expected_tests: Some(TestSpec {
                command: vec!["cargo".into(), "test".into()],
                working_dir: Some(PathBuf::from("/tmp")),
            }),
            utility_refs: vec![UtilRef {
                path: PathBuf::from("src/email.rs"),
                symbol: "Email".into(),
                signature: Some("struct Email(String)".into()),
            }],
            max_retries: 3,
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn spec_serde_round_trip_preserves_all_fields() {
        let spec = sample_spec();
        let json = serde_json::to_string(&spec).expect("serialize");
        let back: DelegationSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, spec);
    }

    #[test]
    fn new_bare_succeeds_for_a_simple_intent() {
        let spec = DelegationSpec::new_bare("do the thing").unwrap();
        assert_eq!(spec.intent, "do the thing");
        assert_eq!(spec.max_retries, DEFAULT_MAX_RETRIES);
        assert!(spec.acceptance.is_empty());
        assert!(spec.forbidden_patterns.is_empty());
    }

    #[test]
    fn new_bare_rejects_empty_intent() {
        let err = DelegationSpec::new_bare("   ").unwrap_err();
        assert!(matches!(err, SpecError::EmptyIntent));
    }

    #[test]
    fn validate_rejects_invalid_forbidden_regex() {
        let mut spec = sample_spec();
        spec.forbidden_patterns.push("(".into()); // unbalanced paren
        let err = spec.validate().unwrap_err();
        assert!(matches!(
            err,
            SpecError::InvalidForbiddenPattern { index: 2, .. }
        ));
    }

    #[test]
    fn validate_rejects_tests_pass_without_expected_tests() {
        let mut spec = sample_spec();
        spec.expected_tests = None;
        let err = spec.validate().unwrap_err();
        assert!(matches!(
            err,
            SpecError::TestsPassWithoutExpectedTests(_)
        ));
    }

    #[test]
    fn validate_rejects_no_forbidden_patterns_when_list_empty() {
        let mut spec = sample_spec();
        spec.forbidden_patterns.clear();
        let err = spec.validate().unwrap_err();
        assert!(matches!(
            err,
            SpecError::NoForbiddenPatternsWithoutPatterns(_)
        ));
    }

    #[test]
    fn validate_rejects_invalid_output_matches_regex() {
        let mut spec = sample_spec();
        spec.acceptance[0] = AcceptanceCriterion::OutputMatches {
            regex: "[invalid".into(),
        };
        let err = spec.validate().unwrap_err();
        assert!(matches!(
            err,
            SpecError::InvalidOutputMatchesRegex { index: 0, .. }
        ));
    }

    #[test]
    fn validate_rejects_max_retries_above_bound() {
        let mut spec = sample_spec();
        spec.max_retries = 11;
        let err = spec.validate().unwrap_err();
        assert!(matches!(err, SpecError::MaxRetriesTooLarge(11)));
    }

    #[test]
    fn validate_rejects_empty_custom_criterion_name() {
        let mut spec = sample_spec();
        spec.acceptance.push(AcceptanceCriterion::Custom {
            name: "  ".into(),
            check: ScriptRef {
                path: PathBuf::from("/bin/true"),
                args: Vec::new(),
            },
        });
        let err = spec.validate().unwrap_err();
        assert!(matches!(err, SpecError::EmptyCustomName(_)));
    }

    #[test]
    fn validate_rejects_empty_util_ref_symbol() {
        let mut spec = sample_spec();
        spec.utility_refs.push(UtilRef {
            path: PathBuf::from("src/foo.rs"),
            symbol: String::new(),
            signature: None,
        });
        let err = spec.validate().unwrap_err();
        assert!(matches!(err, SpecError::EmptyUtilSymbol(_)));
    }

    #[test]
    fn acceptance_criterion_serde_uses_snake_case_kind_tag() {
        // Lock the on-disk shape so future schema versions migrate
        // intentionally rather than by accident.
        let json = serde_json::to_string(&AcceptanceCriterion::TestsPass).unwrap();
        assert_eq!(json, r#"{"kind":"tests_pass"}"#);

        let json = serde_json::to_string(&AcceptanceCriterion::OutputMatches {
            regex: "x".into(),
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"output_matches","regex":"x"}"#);
    }
}
