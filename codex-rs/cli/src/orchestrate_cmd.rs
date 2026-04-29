//! `codrex orchestrate` subcommand — Phase 3 commit 3 happy path.
//!
//! User-facing entry into the orchestrator pipeline. Phase 3 LITE
//! requires `--force-delegate` (or `--no-delegate`) — auto-classification
//! arrives in commit 6.
//!
//! Pipeline at this commit:
//!
//! ```text
//!   build DelegationSpec from --force-delegate intent
//!     │
//!     ▼  (log: classify, payload describes the force-delegate path)
//!   MinimaxDispatchSink.dispatch
//!     │
//!     ▼  (log: dispatch.start, dispatch.end OR clarify on ClarificationRequested)
//!   PlaceholderAuditor.audit  (always Ok in commit 3)
//!     │
//!     ▼  (log: audit, decision)
//!   print response_text to stdout
//! ```
//!
//! Tool execution + real audit policy land in commits 4-5.

use std::path::PathBuf;

use clap::Args;
use codex_orchestrator::AcceptanceCriterion;
use codex_orchestrator::AuditDecision;
use codex_orchestrator::DelegationContext;
use codex_orchestrator::DelegationSpec;
use codex_orchestrator::JsonlDecisionLog;
use codex_orchestrator::MinimaxDispatchSink;
use codex_orchestrator::PatternAuditor;
use codex_orchestrator::TestSpec;
use codex_orchestrator::traits::Auditor;
use codex_orchestrator::traits::DecisionLog;
use codex_orchestrator::traits::DispatchError;
use codex_orchestrator::traits::DispatchSink;
use codex_orchestrator::traits::LogStage;

const DEFAULT_MINIMAX_MODEL: &str = "MiniMax-M2.7";

#[derive(Debug, Args)]
pub struct OrchestrateCli {
    /// The user prompt / intent to orchestrate.
    pub prompt: String,

    /// Skip classification and force the prompt through the delegation
    /// path. Required in Phase 3 commit 3 — `--no-delegate` is the only
    /// other accepted control flag, and the auto classifier arrives in
    /// commit 6.
    #[arg(long = "force-delegate", default_value_t = false, group = "classify")]
    pub force_delegate: bool,

    /// Skip the orchestrator entirely; the prompt is echoed unchanged.
    /// Mutually exclusive with `--force-delegate`. Phase 3 has no
    /// auto-classifier yet, so one of the two flags is required.
    #[arg(long = "no-delegate", default_value_t = false, group = "classify")]
    pub no_delegate: bool,

    /// MiniMax model slug. Defaults to `MiniMax-M2.7`.
    #[arg(long, default_value = DEFAULT_MINIMAX_MODEL)]
    pub model: String,

    /// Log directory for the JSONL decision log. Defaults to
    /// `<CODREX_HOME>/runs`. The directory is created on first write.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Regex pattern the response must NOT match. Repeatable. Adds the
    /// pattern to `DelegationSpec.forbidden_patterns` and implicitly
    /// adds an `AcceptanceCriterion::NoForbiddenPatterns` (idempotent).
    /// Validated as a real regex at parse time.
    #[arg(long = "forbidden")]
    pub forbidden: Vec<String>,

    /// Regex pattern the response MUST match. Repeatable. Each entry
    /// adds an `AcceptanceCriterion::OutputMatches`.
    #[arg(long = "require-output-match")]
    pub require_output_match: Vec<String>,

    /// Shell-style command (split on whitespace) to run after the
    /// dispatch. Sets `expected_tests` and adds an
    /// `AcceptanceCriterion::TestsPass`. Quoting beyond plain
    /// whitespace is not supported in Phase 3 — use a wrapper script
    /// for that.
    #[arg(long = "require-tests-cmd")]
    pub require_tests_cmd: Option<String>,

    /// Maximum retry attempts on AuditDecision::Retry before
    /// escalating. Default 2 (set by DelegationSpec), capped at 10.
    #[arg(long = "max-retries")]
    pub max_retries: Option<u8>,
}

pub async fn run_orchestrate(cli: OrchestrateCli) -> anyhow::Result<()> {
    if !cli.force_delegate && !cli.no_delegate {
        anyhow::bail!(
            "Phase 3 commit 3 requires one of `--force-delegate` or `--no-delegate`. \
             Auto-classification arrives in commit 6."
        );
    }
    if cli.force_delegate && cli.no_delegate {
        anyhow::bail!("`--force-delegate` and `--no-delegate` are mutually exclusive.");
    }

    let log = build_log(cli.log_dir.as_deref())?;

    if cli.no_delegate {
        // Pass-through: echo the intent unchanged. Logged for symmetry
        // so JSONL captures every orchestrate invocation, not only the
        // delegation path. We still build a DelegationSpec so the run
        // gets a real run_id (the spec auto-generates one); the
        // dispatcher / auditor are skipped entirely.
        let placeholder_spec = DelegationSpec::new_bare(&cli.prompt)?;
        let ctx = DelegationContext::for_top_level(&placeholder_spec);
        log.record(
            &ctx,
            LogStage::Classify,
            serde_json::json!({"outcome": "pass_through", "reason": "user-forced (--no-delegate)"}),
        )
        .await;
        log.record(
            &ctx,
            LogStage::Decision,
            serde_json::json!({
                "verdict": "ok",
                "rationale": "pass-through: prompt echoed without delegation"
            }),
        )
        .await;
        println!("{}", cli.prompt);
        return Ok(());
    }

    // Build the delegation spec. Phase 3 commit 4 surfaces forbidden
    // patterns, output regex, and a test command via flags so manual
    // E2E demos can exercise every audit code path. Auto-classification
    // arrives in commit 6 and replaces these with rules-driven config.
    let spec = build_delegation_spec(&cli)?;
    let ctx = DelegationContext::for_top_level(&spec);

    log.record(
        &ctx,
        LogStage::Classify,
        serde_json::json!({
            "outcome": "delegate",
            "reason": "user-forced (--force-delegate)",
            "intent": spec.intent,
        }),
    )
    .await;

    let sink = MinimaxDispatchSink::new(&cli.model);
    log.record(
        &ctx,
        LogStage::DispatchStart,
        serde_json::json!({"provider": "minimax", "model": cli.model}),
    )
    .await;

    let outcome = match sink.dispatch(&spec, &ctx).await {
        Ok(o) => o,
        Err(DispatchError::ClarificationRequested { question }) => {
            // Per the Phase 3 plan adjustment, surface CLARIFY:
            // requests with a clear "not yet implemented" hint until
            // commit 8 lands the round-trip.
            log.record(
                &ctx,
                LogStage::Clarify,
                serde_json::json!({"question": question, "handled": false}),
            )
            .await;
            log.record(
                &ctx,
                LogStage::Decision,
                serde_json::json!({
                    "verdict": "escalate",
                    "reason": "model requested clarification",
                    "blocking_issue": question,
                }),
            )
            .await;
            anyhow::bail!(
                "MiniMax requested clarification but CLARIFY: handling is not yet \
                 implemented (lands in Phase 3 commit 8).\nQuestion: {question}\n\
                 Refine your prompt and re-run."
            );
        }
        Err(other) => {
            log.record(
                &ctx,
                LogStage::DispatchEnd,
                serde_json::json!({"error": other.to_string()}),
            )
            .await;
            log.record(
                &ctx,
                LogStage::Decision,
                serde_json::json!({
                    "verdict": "drop",
                    "reason": format!("dispatch error: {other}"),
                }),
            )
            .await;
            anyhow::bail!("dispatch failed: {other}");
        }
    };

    log.record(
        &ctx,
        LogStage::DispatchEnd,
        serde_json::json!({
            "latency_ms": outcome.latency_ms,
            "total_tokens": outcome.total_tokens,
            "response_len": outcome.response_text.len(),
        }),
    )
    .await;

    let auditor = PatternAuditor::new();
    let report = auditor.audit(&spec, &ctx, &outcome).await;

    // Emit one row per criterion BEFORE the aggregated audit row.
    // Greppable as `"stage":"audit_criterion"` for dashboards of
    // "which criteria fail most".
    for cr in &report.criterion_results {
        log.record(
            &ctx,
            LogStage::AuditCriterion,
            serde_json::json!({
                "name": cr.name,
                "passed": cr.passed,
                "duration_ms": cr.duration_ms,
                "details": cr.details,
            }),
        )
        .await;
    }

    log.record(
        &ctx,
        LogStage::Audit,
        serde_json::to_value(&report.decision).unwrap_or_else(|_| serde_json::json!({})),
    )
    .await;

    let verdict_label = match &report.decision {
        AuditDecision::Ok { .. } => "ok",
        AuditDecision::Retry { .. } => "retry",
        AuditDecision::Escalate { .. } => "escalate",
        AuditDecision::Drop { .. } => "drop",
    };
    log.record(
        &ctx,
        LogStage::Decision,
        serde_json::json!({"verdict": verdict_label}),
    )
    .await;

    // Phase 3 commit 4 LITE: the orchestrate command reports the
    // verdict but doesn't yet act on Retry/Escalate/Drop (that wiring
    // arrives in commit 5). For now we print the response and exit
    // with a status code that signals the verdict to scripts.
    println!("{}", outcome.response_text);
    match report.decision {
        AuditDecision::Ok { .. } => Ok(()),
        AuditDecision::Retry { feedback, attempt } => {
            anyhow::bail!(
                "audit verdict: retry (attempt {attempt}). Retry loop arrives in commit 5.\n\
                 Feedback:\n{feedback}"
            )
        }
        AuditDecision::Escalate {
            reason,
            blocking_issue,
        } => {
            anyhow::bail!("audit verdict: escalate. {reason}\nBlocking issue: {blocking_issue}")
        }
        AuditDecision::Drop { reason } => {
            anyhow::bail!("audit verdict: drop. {reason}")
        }
    }
}

fn build_delegation_spec(cli: &OrchestrateCli) -> anyhow::Result<DelegationSpec> {
    let mut spec = DelegationSpec::new_bare(&cli.prompt)?;
    spec.forbidden_patterns = cli.forbidden.clone();
    if let Some(max_retries) = cli.max_retries {
        spec.max_retries = max_retries;
    }
    let mut acceptance: Vec<AcceptanceCriterion> = Vec::new();
    if !cli.forbidden.is_empty() {
        acceptance.push(AcceptanceCriterion::NoForbiddenPatterns);
    }
    for regex in &cli.require_output_match {
        acceptance.push(AcceptanceCriterion::OutputMatches {
            regex: regex.clone(),
        });
    }
    if let Some(cmd_str) = cli.require_tests_cmd.as_deref()
        && !cmd_str.trim().is_empty()
    {
        let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
        if !parts.is_empty() {
            spec.expected_tests = Some(TestSpec {
                command: parts,
                working_dir: None,
            });
            acceptance.push(AcceptanceCriterion::TestsPass);
        }
    }
    spec.acceptance = acceptance;
    spec.validate()
        .map_err(|e| anyhow::anyhow!("invalid spec built from flags: {e}"))?;
    Ok(spec)
}

fn build_log(custom_dir: Option<&std::path::Path>) -> anyhow::Result<JsonlDecisionLog> {
    let dir = match custom_dir {
        Some(p) => p.to_path_buf(),
        None => {
            let home = codex_utils_home_dir::find_codex_home()
                .map_err(|e| anyhow::anyhow!("failed to resolve CODREX_HOME: {e}"))?;
            home.as_path().join("runs")
        }
    };
    Ok(JsonlDecisionLog::new(dir))
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        orchestrate: OrchestrateCli,
    }

    #[test]
    fn max_retries_flag_overrides_delegation_spec() {
        let cli = TestCli::parse_from([
            "codrex",
            "do work",
            "--force-delegate",
            "--max-retries",
            "5",
        ])
        .orchestrate;

        let spec = build_delegation_spec(&cli).expect("spec should build from CLI");

        assert_eq!(spec.max_retries, 5);
    }

    #[test]
    fn max_retries_defaults_to_delegation_spec_default_when_flag_absent() {
        let cli = TestCli::parse_from(["codrex", "do work", "--force-delegate"]).orchestrate;

        let spec = build_delegation_spec(&cli).expect("spec should build from CLI");

        // DEFAULT_MAX_RETRIES is private in codex-orchestrator; keep this
        // aligned with DelegationSpec::new_bare's documented default.
        assert_eq!(spec.max_retries, 2);
    }
}
