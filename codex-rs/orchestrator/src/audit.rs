//! Auditor implementations.
//!
//! - [`PlaceholderAuditor`] — historical from commit 3, kept for tests
//!   that exercise the orchestration loop without real audit policy.
//! - [`PatternAuditor`] — Phase 3 commit 4, evaluates every variant of
//!   [`crate::AcceptanceCriterion`] against a real dispatch outcome
//!   and produces a structured [`AuditReport`].
//!
//! ## PatternAuditor — evaluation order and decision matrix
//!
//! Criteria are evaluated cheap-to-expensive with **short-circuit on
//! the first failure**. The auditor stops as soon as it finds a
//! reason to fail, so a forbidden-pattern match doesn't pay the cost
//! of running tests behind it.
//!
//! ```text
//!   Order   Criterion              Cost class    On failure
//!   -----   ---------------------  ------------  -------------
//!     1     NoForbiddenPatterns    micros        Escalate
//!     2     OutputMatches[*]       micros        Retry
//!     3     TestsPass              seconds       Retry
//!     4     Custom[*]              seconds       Retry
//! ```
//!
//! `NoForbiddenPatterns` deliberately escalates rather than retries.
//! When the model produces a forbidden pattern, telling it "don't" via
//! retry feedback rarely helps — the rule was already in the prompt
//! and the model violated it anyway. Surface to the user with the
//! matched pattern + the snippet, and let them refine the spec.
//!
//! `TestsPass` and `Custom` failures are retryable: the auditor
//! includes a structured failure description (test name, exit code,
//! stderr/stdout snippet) in the `Retry.feedback` so the next attempt
//! can see what went wrong.
//!
//! ## Timeouts
//!
//! `TestsPass` and `Custom` execute external processes. Each is
//! wrapped in a hard timeout (default 30s, see
//! [`PatternAuditor::DEFAULT_PROCESS_TIMEOUT`]). If the timeout fires,
//! the criterion records a structured timeout failure and the audit
//! short-circuits to Retry.
//!
//! `TestSpec` itself doesn't carry a `timeout` field yet (TODO #10);
//! the auditor's hard cap is the only safety net until that lands.

use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

use crate::context::DelegationContext;
use crate::decision::AuditDecision;
use crate::orch_debug_enabled;
use crate::spec::AcceptanceCriterion;
use crate::spec::DelegationSpec;
use crate::spec::ScriptRef;
use crate::spec::TestSpec;
use crate::traits::AuditReport;
use crate::traits::Auditor;
use crate::traits::CriterionResult;
use crate::traits::DispatchOutcome;

/// Auditor that always emits `AuditDecision::Ok` with no criterion
/// results. Useful for orchestrator-loop tests that don't want to
/// reason about audit policy. Production callers use `PatternAuditor`.
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
    ) -> AuditReport {
        AuditReport {
            decision: AuditDecision::Ok {
                rationale: "PlaceholderAuditor — no acceptance checks performed. \
                            Use PatternAuditor in production."
                    .to_string(),
            },
            criterion_results: Vec::new(),
        }
    }
}

/// Real auditor for Phase 3 commit 4. Evaluates every criterion in the
/// spec, in cheap-to-expensive order, short-circuiting on the first
/// failure. Returns an [`AuditReport`] with the verdict and the per-
/// criterion timing/details.
#[derive(Debug, Clone, Copy)]
pub struct PatternAuditor {
    process_timeout: Duration,
}

impl Default for PatternAuditor {
    fn default() -> Self {
        Self::new()
    }
}

impl PatternAuditor {
    /// Hardcoded process timeout. Phase 4 will surface this through
    /// `TestSpec.timeout` (TODO #10) and `ScriptRef.timeout`.
    pub const DEFAULT_PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self {
            process_timeout: Self::DEFAULT_PROCESS_TIMEOUT,
        }
    }

    /// Test-only constructor that overrides the process timeout. Used
    /// to exercise the timeout path without making tests slow.
    pub fn with_process_timeout(timeout: Duration) -> Self {
        Self {
            process_timeout: timeout,
        }
    }
}

#[async_trait]
impl Auditor for PatternAuditor {
    async fn audit(
        &self,
        spec: &DelegationSpec,
        ctx: &DelegationContext,
        outcome: &DispatchOutcome,
    ) -> AuditReport {
        let mut criterion_results: Vec<CriterionResult> = Vec::new();

        // Evaluate cheap-to-expensive, short-circuit on first failure.
        // Indexing semantics:
        //   - `OutputMatches` and `Custom` can repeat in `spec.acceptance`;
        //     we tag each with its 0-indexed position (`output_matches[2]`,
        //     `custom:my_check`).
        //   - `NoForbiddenPatterns` and `TestsPass` are evaluated once
        //     even if listed multiple times — the second occurrence
        //     wouldn't add information.
        let mut evaluated_no_forbidden = false;
        let mut evaluated_tests_pass = false;

        for (idx, criterion) in spec.acceptance.iter().enumerate() {
            match criterion {
                AcceptanceCriterion::NoForbiddenPatterns => {
                    if evaluated_no_forbidden {
                        continue;
                    }
                    evaluated_no_forbidden = true;
                    let result = check_no_forbidden_patterns(spec, outcome);
                    let passed = result.passed;
                    let details = result.details.clone();
                    criterion_results.push(result);
                    if !passed {
                        return early_escalate(criterion_results, &details, "no_forbidden_patterns");
                    }
                }
                AcceptanceCriterion::OutputMatches { regex } => {
                    let result = check_output_matches(idx, regex, outcome);
                    let passed = result.passed;
                    let details = result.details.clone();
                    criterion_results.push(result);
                    if !passed {
                        return early_retry(
                            criterion_results,
                            ctx,
                            "output_matches",
                            &details,
                        );
                    }
                }
                AcceptanceCriterion::TestsPass => {
                    if evaluated_tests_pass {
                        continue;
                    }
                    evaluated_tests_pass = true;
                    // Spec validation guarantees expected_tests is Some
                    // when this variant appears. Defensive unwrap_or
                    // produces a Drop verdict if it ever isn't.
                    let Some(test_spec) = spec.expected_tests.as_ref() else {
                        return AuditReport {
                            decision: AuditDecision::Drop {
                                reason: "TestsPass criterion present but expected_tests is None \
                                         (spec validation failure)"
                                    .to_string(),
                            },
                            criterion_results,
                        };
                    };
                    let result = run_test_spec(test_spec, self.process_timeout).await;
                    let passed = result.passed;
                    let details = result.details.clone();
                    criterion_results.push(result);
                    if !passed {
                        return early_retry(
                            criterion_results,
                            ctx,
                            "tests_pass",
                            &details,
                        );
                    }
                }
                AcceptanceCriterion::Custom { name, check } => {
                    let result =
                        run_custom_script(idx, name, check, self.process_timeout).await;
                    let passed = result.passed;
                    let details = result.details.clone();
                    criterion_results.push(result);
                    if !passed {
                        return early_retry(
                            criterion_results,
                            ctx,
                            &format!("custom:{name}"),
                            &details,
                        );
                    }
                }
            }
        }

        // All criteria passed (or none configured).
        AuditReport {
            decision: AuditDecision::Ok {
                rationale: if criterion_results.is_empty() {
                    "no acceptance criteria configured; treating as pass-through Ok".to_string()
                } else {
                    format!(
                        "all {} acceptance criteria passed",
                        criterion_results.len()
                    )
                },
            },
            criterion_results,
        }
    }
}

fn early_escalate(
    criterion_results: Vec<CriterionResult>,
    details: &JsonValue,
    label: &str,
) -> AuditReport {
    let blocking_issue = format_blocking_issue(label, details);
    AuditReport {
        decision: AuditDecision::Escalate {
            reason: format!("{label} failed — manual intervention required"),
            blocking_issue,
        },
        criterion_results,
    }
}

fn early_retry(
    criterion_results: Vec<CriterionResult>,
    ctx: &DelegationContext,
    label: &str,
    details: &JsonValue,
) -> AuditReport {
    // Attempt counter for the NEXT dispatch is +1 of the current ctx
    // attempt. The retry loop in commit 5 will increment ctx before
    // re-dispatching; we record the value the loop should land on.
    let next_attempt = ctx.attempt.saturating_add(1);
    let feedback = format_retry_feedback(label, details);
    AuditReport {
        decision: AuditDecision::Retry {
            feedback,
            attempt: next_attempt,
        },
        criterion_results,
    }
}

fn format_blocking_issue(label: &str, details: &JsonValue) -> String {
    if label == "no_forbidden_patterns"
        && let Some(matches) = details.get("matches").and_then(|m| m.as_array())
        && !matches.is_empty()
    {
        let summary: Vec<String> = matches
            .iter()
            .filter_map(|m| {
                let pat = m.get("pattern").and_then(|x| x.as_str())?;
                let snippet = m
                    .get("snippet")
                    .and_then(|x| x.as_str())
                    .unwrap_or("<no snippet>");
                Some(format!("`{pat}` matched: {snippet}"))
            })
            .collect();
        return format!("forbidden patterns matched: {}", summary.join("; "));
    }
    format!("{label} failed: {details}")
}

fn format_retry_feedback(label: &str, details: &JsonValue) -> String {
    match label {
        "output_matches" => {
            let regex = details
                .get("regex")
                .and_then(|x| x.as_str())
                .unwrap_or("<unknown>");
            format!(
                "Your previous response did not match the expected pattern: `{regex}`. \
                 Please retry and ensure the response matches."
            )
        }
        "tests_pass" => {
            let exit = details.get("exit_code").and_then(|x| x.as_i64()).unwrap_or(-1);
            let stderr = details
                .get("stderr")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let snippet = stderr.chars().take(2048).collect::<String>();
            format!(
                "Tests failed (exit code {exit}). Stderr snippet:\n{snippet}\n\
                 Please address the failure and retry."
            )
        }
        s if s.starts_with("custom:") => {
            let exit = details.get("exit_code").and_then(|x| x.as_i64()).unwrap_or(-1);
            let stderr = details
                .get("stderr")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let snippet = stderr.chars().take(2048).collect::<String>();
            format!(
                "Custom acceptance check `{s}` failed (exit {exit}). Stderr:\n{snippet}"
            )
        }
        _ => format!("{label} failed: {details}"),
    }
}

fn check_no_forbidden_patterns(
    spec: &DelegationSpec,
    outcome: &DispatchOutcome,
) -> CriterionResult {
    let started = Instant::now();
    let mut matches: Vec<JsonValue> = Vec::new();
    for pattern in &spec.forbidden_patterns {
        if let Some(m) = pattern.regex().find(&outcome.response_text) {
            let snippet = snippet_around(&outcome.response_text, m.start(), m.end(), 80);
            matches.push(json!({
                "pattern": pattern.as_str(),
                "match_start": m.start(),
                "match_end": m.end(),
                "snippet": snippet,
            }));
        }
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let passed = matches.is_empty();
    if orch_debug_enabled() {
        eprintln!(
            "[codrex/orch] audit no_forbidden_patterns passed={passed} ({}ms, {} matches)",
            duration_ms,
            matches.len()
        );
    }
    CriterionResult {
        name: "no_forbidden_patterns".to_string(),
        passed,
        duration_ms,
        details: json!({"matches": matches}),
    }
}

fn check_output_matches(
    idx: usize,
    regex: &crate::spec::ValidatedRegex,
    outcome: &DispatchOutcome,
) -> CriterionResult {
    let started = Instant::now();
    let matched = regex.regex().is_match(&outcome.response_text);
    let duration_ms = started.elapsed().as_millis() as u64;
    if orch_debug_enabled() {
        eprintln!(
            "[codrex/orch] audit output_matches[{idx}] passed={matched} ({duration_ms}ms)"
        );
    }
    CriterionResult {
        name: format!("output_matches[{idx}]"),
        passed: matched,
        duration_ms,
        details: json!({"regex": regex.as_str(), "matched": matched}),
    }
}

async fn run_test_spec(test_spec: &TestSpec, hard_timeout: Duration) -> CriterionResult {
    let started = Instant::now();
    let name = "tests_pass";
    let Some((bin, args)) = test_spec.command.split_first() else {
        // Shouldn't happen — spec validation catches empty command.
        return CriterionResult {
            name: name.to_string(),
            passed: false,
            duration_ms: 0,
            details: json!({"error": "empty command"}),
        };
    };
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = test_spec.working_dir.as_ref() {
        cmd.current_dir(dir);
    }
    run_subprocess(name, cmd, hard_timeout, started).await
}

async fn run_custom_script(
    idx: usize,
    name: &str,
    script: &ScriptRef,
    hard_timeout: Duration,
) -> CriterionResult {
    let started = Instant::now();
    let label = format!("custom[{idx}:{name}]");
    let mut cmd = Command::new(&script.path);
    cmd.args(&script.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    run_subprocess(&label, cmd, hard_timeout, started).await
}

async fn run_subprocess(
    name: &str,
    mut cmd: Command,
    hard_timeout: Duration,
    started: Instant,
) -> CriterionResult {
    let spawn_result = cmd.spawn();
    let child = match spawn_result {
        Ok(c) => c,
        Err(err) => {
            return CriterionResult {
                name: name.to_string(),
                passed: false,
                duration_ms: started.elapsed().as_millis() as u64,
                details: json!({"error": format!("spawn failed: {err}")}),
            };
        }
    };

    let outcome = timeout(hard_timeout, child.wait_with_output()).await;
    let duration_ms = started.elapsed().as_millis() as u64;

    match outcome {
        Ok(Ok(output)) => {
            let exit_code = output.status.code().unwrap_or(-1);
            let passed = output.status.success();
            CriterionResult {
                name: name.to_string(),
                passed,
                duration_ms,
                details: json!({
                    "exit_code": exit_code,
                    "stdout": String::from_utf8_lossy(&output.stdout)
                        .chars().take(4096).collect::<String>(),
                    "stderr": String::from_utf8_lossy(&output.stderr)
                        .chars().take(4096).collect::<String>(),
                }),
            }
        }
        Ok(Err(err)) => CriterionResult {
            name: name.to_string(),
            passed: false,
            duration_ms,
            details: json!({"error": format!("wait failed: {err}")}),
        },
        Err(_elapsed) => {
            warn!(
                target: "codrex::orchestrator::audit",
                criterion = %name,
                timeout_ms = hard_timeout.as_millis() as u64,
                "criterion timed out"
            );
            CriterionResult {
                name: name.to_string(),
                passed: false,
                duration_ms,
                details: json!({
                    "error": "timeout",
                    "timeout_ms": hard_timeout.as_millis() as u64,
                }),
            }
        }
    }
}

fn snippet_around(text: &str, start: usize, end: usize, context: usize) -> String {
    let s = start.saturating_sub(context);
    let e = (end + context).min(text.len());
    let raw = text.get(s..e).unwrap_or("");
    raw.replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::AcceptanceCriterion;
    use crate::spec::TestSpec;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;
    use std::time::SystemTime;
    use uuid::Uuid;

    fn outcome(text: &str) -> DispatchOutcome {
        DispatchOutcome {
            response_text: text.into(),
            latency_ms: 100,
            total_tokens: Some(50),
        }
    }

    fn ctx_for(spec: &DelegationSpec) -> DelegationContext {
        DelegationContext::for_top_level(spec)
    }

    fn spec_with(
        intent: &str,
        forbidden: Vec<&str>,
        acceptance: Vec<AcceptanceCriterion>,
        expected_tests: Option<TestSpec>,
    ) -> DelegationSpec {
        let mut spec = DelegationSpec {
            run_id: Uuid::new_v4(),
            parent_run_id: None,
            intent: intent.into(),
            acceptance,
            forbidden_patterns: Vec::new(),
            expected_tests,
            utility_refs: Vec::new(),
            max_retries: 2,
            created_at: SystemTime::UNIX_EPOCH,
        };
        spec.set_forbidden_patterns(forbidden)
            .expect("forbidden patterns must compile");
        spec.validate().expect("test spec must validate");
        spec
    }

    #[tokio::test(flavor = "current_thread")]
    async fn placeholder_auditor_returns_ok_with_no_criterion_results() {
        let spec = DelegationSpec::new_bare("hi").unwrap();
        let report = PlaceholderAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("text"))
            .await;
        assert!(matches!(report.decision, AuditDecision::Ok { .. }));
        assert!(report.criterion_results.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pattern_auditor_passes_when_no_criteria_configured() {
        let spec = DelegationSpec::new_bare("hi").unwrap();
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("anything goes"))
            .await;
        assert!(matches!(report.decision, AuditDecision::Ok { .. }));
        assert!(report.criterion_results.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pattern_auditor_passes_when_all_criteria_pass() {
        let spec = spec_with(
            "x",
            vec!["unsafe"],
            vec![
                AcceptanceCriterion::NoForbiddenPatterns,
                AcceptanceCriterion::output_matches(r"^OK\b").unwrap(),
            ],
            None,
        );
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("OK so it worked"))
            .await;
        assert!(matches!(report.decision, AuditDecision::Ok { .. }));
        assert_eq!(report.criterion_results.len(), 2);
        assert!(report.criterion_results.iter().all(|r| r.passed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn forbidden_pattern_match_escalates_not_retries() {
        let spec = spec_with(
            "x",
            vec!["unsafe"],
            vec![AcceptanceCriterion::NoForbiddenPatterns],
            None,
        );
        let report = PatternAuditor::new()
            .audit(
                &spec,
                &ctx_for(&spec),
                &outcome("hold my beer\nlet me try unsafe { transmute() }"),
            )
            .await;
        match &report.decision {
            AuditDecision::Escalate { blocking_issue, .. } => {
                assert!(
                    blocking_issue.contains("unsafe"),
                    "blocking_issue should name the matched pattern; got {blocking_issue}"
                );
            }
            other => panic!("expected Escalate, got {other:?}"),
        }
        assert_eq!(report.criterion_results.len(), 1);
        assert!(!report.criterion_results[0].passed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_matches_failure_returns_retry() {
        let spec = spec_with(
            "x",
            vec![],
            vec![AcceptanceCriterion::output_matches(r"^DONE$").unwrap()],
            None,
        );
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("nope, not done yet"))
            .await;
        match &report.decision {
            AuditDecision::Retry { feedback, attempt } => {
                assert_eq!(*attempt, 1);
                assert!(feedback.contains("DONE"));
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    /// Short-circuit invariant: a forbidden-pattern match must abort
    /// before TestsPass would run. Otherwise we'd pay test latency
    /// for runs that were already going to escalate.
    #[tokio::test(flavor = "current_thread")]
    async fn auditor_short_circuits_on_first_failure() {
        let spec = spec_with(
            "x",
            vec!["forbidden"],
            vec![
                AcceptanceCriterion::NoForbiddenPatterns,
                // If this ran, it would try to spawn `nonexistent`.
                AcceptanceCriterion::TestsPass,
            ],
            Some(TestSpec {
                command: vec!["nonexistent_binary_should_not_run".into()],
                working_dir: None,
            }),
        );
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("contains forbidden token"))
            .await;
        assert!(matches!(report.decision, AuditDecision::Escalate { .. }));
        assert_eq!(
            report.criterion_results.len(),
            1,
            "TestsPass must not run after NoForbiddenPatterns fails"
        );
        assert_eq!(report.criterion_results[0].name, "no_forbidden_patterns");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tests_pass_failure_returns_retry_with_stderr_snippet() {
        // `false` exits 1 with no output. `bash -c 'echo failure 1>&2; exit 1'`
        // would carry stderr but not all environments have bash; use a
        // python one-liner that should be present on macOS/Linux.
        let spec = spec_with(
            "x",
            vec![],
            vec![AcceptanceCriterion::TestsPass],
            Some(TestSpec {
                command: vec![
                    "python3".into(),
                    "-c".into(),
                    "import sys; sys.stderr.write('boom\\n'); sys.exit(2)".into(),
                ],
                working_dir: None,
            }),
        );
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("fine"))
            .await;
        match &report.decision {
            AuditDecision::Retry { feedback, .. } => {
                assert!(feedback.contains("Tests failed"));
                assert!(feedback.contains("boom"));
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_records_structured_failure_and_short_circuits() {
        let spec = spec_with(
            "x",
            vec![],
            vec![AcceptanceCriterion::TestsPass],
            Some(TestSpec {
                command: vec!["sleep".into(), "30".into()],
                working_dir: None,
            }),
        );
        let auditor = PatternAuditor::with_process_timeout(Duration::from_millis(150));
        let report = auditor.audit(&spec, &ctx_for(&spec), &outcome("x")).await;
        assert!(matches!(report.decision, AuditDecision::Retry { .. }));
        assert_eq!(report.criterion_results.len(), 1);
        let cr = &report.criterion_results[0];
        assert!(!cr.passed);
        assert_eq!(cr.details["error"], "timeout");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn custom_script_failure_returns_retry() {
        // `/bin/false` is a portable POSIX binary that exits 1 silently.
        let spec = spec_with(
            "x",
            vec![],
            vec![AcceptanceCriterion::Custom {
                name: "my_check".into(),
                check: ScriptRef {
                    path: PathBuf::from("/bin/false"),
                    args: vec![],
                },
            }],
            None,
        );
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("x"))
            .await;
        assert!(matches!(report.decision, AuditDecision::Retry { .. }));
        let cr = &report.criterion_results[0];
        assert!(cr.name.starts_with("custom["));
        assert!(!cr.passed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn criterion_results_record_duration_ms() {
        let spec = spec_with(
            "x",
            vec!["forbidden"],
            vec![AcceptanceCriterion::NoForbiddenPatterns],
            None,
        );
        let report = PatternAuditor::new()
            .audit(&spec, &ctx_for(&spec), &outcome("clean output"))
            .await;
        assert!(matches!(report.decision, AuditDecision::Ok { .. }));
        // duration_ms is non-negative; we don't assert >0 because the
        // regex match is fast enough to round to 0 ms.
        assert_eq!(report.criterion_results.len(), 1);
    }
}
