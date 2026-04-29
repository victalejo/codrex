//! `codrex orchestrate` subcommand — Phase 3 commit 6 rules classifier.
//!
//! User-facing entry into the orchestrator pipeline. Phase 3 commit 6
//! adds `RulesClassifier`, so unforced invocations classify the prompt
//! against local delegation rules before deciding whether to delegate or
//! pass through unchanged.
//!
//! Pipeline at this commit:
//!
//! ```text
//!   classify prompt (flags override rules)
//!     │
//!     ├─ pass-through → log classify + decision → print prompt
//!     │
//!     └─ delegate → build DelegationSpec + apply CLI overrides
//!                    │
//!                    ▼  (log: classify with rule/override reason)
//!                  MinimaxDispatchSink.dispatch
//!     │
//!     ▼  (log: dispatch.start, dispatch.end OR clarify on ClarificationRequested)
//!   PatternAuditor.audit
//!     │
//!     ▼  (log: audit, decision)
//!   print response_text to stdout
//! ```
//!
//! Exit codes:
//!   - `0`: final verdict `Ok`
//!   - `1`: infrastructure/dispatch error (auth, transport, parser, etc.)
//!   - `2`: final verdict `Escalate` (needs user intervention)
//!   - `3`: final verdict `Drop`

use std::path::PathBuf;

use clap::Args;
use codex_orchestrator::AcceptanceCriterion;
use codex_orchestrator::ClassificationOutcome;
use codex_orchestrator::Classifier;
use codex_orchestrator::DelegationContext;
use codex_orchestrator::DelegationSpec;
use codex_orchestrator::JsonlDecisionLog;
use codex_orchestrator::MinimaxDispatchSink;
use codex_orchestrator::OrchestrateOutcome;
use codex_orchestrator::PatternAuditor;
use codex_orchestrator::RulesClassifier;
use codex_orchestrator::SpecError;
use codex_orchestrator::TestSpec;
use codex_orchestrator::run_orchestration_loop;
use codex_orchestrator::traits::DecisionLog;
use codex_orchestrator::traits::LogStage;

const DEFAULT_MINIMAX_MODEL: &str = "MiniMax-M2.7";

#[derive(Debug, Args)]
#[doc = "Orchestrate a delegated turn through the MiniMax orchestrator pipeline.

Exit codes:
  0 — final verdict Ok (response accepted)
  1 — infrastructure/dispatch error (auth failure, transport error, etc.)
  2 — final verdict Escalate (needs user intervention)
  3 — final verdict Drop (loop detected or unrecoverable)"]
pub struct OrchestrateCli {
    /// The user prompt / intent to orchestrate.
    pub prompt: String,

    /// Skip classification and force the prompt through the delegation
    /// path, even if the rules would normally pass it through.
    #[arg(long = "force-delegate", default_value_t = false, group = "classify")]
    pub force_delegate: bool,

    /// Skip the orchestrator entirely; the prompt is echoed unchanged.
    /// Mutually exclusive with `--force-delegate` and overrides any
    /// delegation rule that would otherwise match.
    #[arg(long = "no-delegate", default_value_t = false, group = "classify")]
    pub no_delegate: bool,

    /// MiniMax model slug. Defaults to `MiniMax-M2.7`.
    #[arg(long, default_value = DEFAULT_MINIMAX_MODEL)]
    pub model: String,

    /// Log directory for the JSONL decision log. Defaults to
    /// `<CODREX_HOME>/runs`. The directory is created on first write.
    #[arg(long)]
    pub log_dir: Option<PathBuf>,

    /// Maximum retry attempts on `AuditDecision::Retry` before
    /// escalating. Default 2 (set by `DelegationSpec`), capped at 10.
    #[arg(long = "max-retries")]
    pub max_retries: Option<u8>,

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
}

pub async fn run_orchestrate(cli: OrchestrateCli) -> anyhow::Result<OrchestrateOutcome> {
    if cli.force_delegate && cli.no_delegate {
        anyhow::bail!("`--force-delegate` and `--no-delegate` are mutually exclusive.");
    }

    let log = build_log(cli.log_dir.as_deref())?;

    if cli.no_delegate {
        return pass_through(
            &cli.prompt,
            "user-forced (--no-delegate)".to_string(),
            None,
            &log,
        )
        .await;
    }

    let spec = if cli.force_delegate {
        let spec = build_delegation_spec(&cli)?;
        log_classification(
            &DelegationContext::for_top_level(&spec),
            "delegate",
            "user-forced (--force-delegate)".to_string(),
            None,
            &log,
        )
        .await;
        spec
    } else {
        let codex_home = codex_utils_home_dir::find_codex_home()
            .map_err(|e| anyhow::anyhow!("failed to resolve CODREX_HOME: {e}"))?;
        let classifier = RulesClassifier::from_default_path(codex_home.as_path())?;
        match classifier.classify(&cli.prompt).await {
            ClassificationOutcome::Delegate {
                spec,
                reason,
                rule_name,
            } => {
                let spec = build_delegation_spec_from_base(spec, &cli)?;
                log_classification(
                    &DelegationContext::for_top_level(&spec),
                    "delegate",
                    reason,
                    rule_name,
                    &log,
                )
                .await;
                spec
            }
            ClassificationOutcome::PassThrough { reason, rule_name } => {
                return pass_through(&cli.prompt, reason, rule_name, &log).await;
            }
        }
    };

    let sink = MinimaxDispatchSink::new(&cli.model);
    let auditor = PatternAuditor::new();
    let outcome = run_orchestration_loop(&spec, &cli.model, &sink, &auditor, &log)
        .await
        .map_err(anyhow::Error::msg)?;
    match &outcome {
        OrchestrateOutcome::Ok { response_text } => {
            println!("{response_text}");
        }
        OrchestrateOutcome::Escalate {
            reason,
            blocking_issue,
            attempts_exhausted,
        } => {
            eprintln!("audit verdict: escalate. {reason}");
            eprintln!("blocking_issue: {blocking_issue}");
            if let Some(attempts_exhausted) = attempts_exhausted {
                eprintln!("attempts_exhausted: {attempts_exhausted}");
            }
        }
        OrchestrateOutcome::Drop {
            reason,
            repeated_signature,
        } => {
            eprintln!("audit verdict: drop. {reason}");
            if let Some(repeated_signature) = repeated_signature {
                eprintln!("repeated_signature: {repeated_signature}");
            }
        }
    }

    Ok(outcome)
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

fn build_delegation_spec(cli: &OrchestrateCli) -> anyhow::Result<DelegationSpec> {
    build_delegation_spec_from_base(DelegationSpec::new_bare(&cli.prompt)?, cli)
}

fn build_delegation_spec_from_base(
    mut spec: DelegationSpec,
    cli: &OrchestrateCli,
) -> anyhow::Result<DelegationSpec> {
    if let Some(max_retries) = cli.max_retries {
        spec.max_retries = max_retries;
    }
    spec.set_forbidden_patterns(cli.forbidden.clone())
        .map_err(|e| anyhow::anyhow!("invalid spec built from flags: {e}"))?;

    let mut acceptance: Vec<AcceptanceCriterion> = Vec::new();
    if !cli.forbidden.is_empty() {
        acceptance.push(AcceptanceCriterion::NoForbiddenPatterns);
    }
    for (index, regex) in cli.require_output_match.iter().enumerate() {
        let criterion = AcceptanceCriterion::output_matches(regex).map_err(|source| {
            anyhow::anyhow!(
                "invalid spec built from flags: {}",
                SpecError::InvalidOutputMatchesRegex { index, source }
            )
        })?;
        acceptance.push(criterion);
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

async fn pass_through(
    prompt: &str,
    reason: String,
    rule_name: Option<String>,
    log: &JsonlDecisionLog,
) -> anyhow::Result<OrchestrateOutcome> {
    // Pass-through: echo the intent unchanged. Logged for symmetry so
    // JSONL captures every orchestrate invocation, not only the
    // delegation path. We still build a DelegationSpec so the run gets
    // a real run_id; the dispatcher and auditor are skipped entirely.
    let placeholder_spec = DelegationSpec::new_bare(prompt)?;
    let ctx = DelegationContext::for_top_level(&placeholder_spec);
    log_classification(&ctx, "pass_through", reason.clone(), rule_name, log).await;
    log.record(
        &ctx,
        LogStage::Decision,
        serde_json::json!({
            "verdict": "ok",
            "rationale": format!("pass-through: {reason}")
        }),
    )
    .await;
    println!("{prompt}");
    Ok(OrchestrateOutcome::Ok {
        response_text: prompt.to_string(),
    })
}

async fn log_classification(
    ctx: &DelegationContext,
    outcome: &str,
    reason: String,
    rule_name: Option<String>,
    log: &JsonlDecisionLog,
) {
    let mut payload = serde_json::json!({
        "outcome": outcome,
        "reason": reason,
    });
    if let Some(rule_name) = rule_name
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "rule_name".to_string(),
            serde_json::Value::String(rule_name),
        );
    }
    log.record(ctx, LogStage::Classify, payload).await;
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use pretty_assertions::assert_eq;

    use super::OrchestrateCli;
    use super::build_delegation_spec;
    use codex_orchestrator::DelegationSpec;

    #[derive(Debug, Parser)]
    struct ParsedOrchestrateCli {
        #[command(flatten)]
        cli: OrchestrateCli,
    }

    fn parse_cli(args: &[&str]) -> OrchestrateCli {
        ParsedOrchestrateCli::try_parse_from(args)
            .expect("orchestrate args should parse")
            .cli
    }

    #[test]
    fn build_spec_uses_max_retries_from_flag() {
        let cli = parse_cli(&["codrex", "prompt", "--force-delegate", "--max-retries", "5"]);

        let spec = build_delegation_spec(&cli).expect("spec should build");

        assert_eq!(spec.max_retries, 5);
    }

    #[test]
    fn build_spec_keeps_default_max_retries_without_flag() {
        let cli = parse_cli(&["codrex", "prompt", "--force-delegate"]);

        let spec = build_delegation_spec(&cli).expect("spec should build");
        let default_spec =
            DelegationSpec::new_bare("prompt").expect("bare spec should use the default retries");

        assert_eq!(spec.max_retries, default_spec.max_retries);
    }
}
