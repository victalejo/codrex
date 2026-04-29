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
use std::sync::Arc;
use std::time::Duration;

use clap::Args;
use codex_config::CloudRequirementsLoader;
use codex_config::LoaderOverrides;
use codex_config::NoopThreadConfigLoader;
use codex_config::config_toml::ConfigToml;
use codex_config::loader::load_config_layers_state;
use codex_config::types::AuthCredentialsStoreMode;
use codex_exec_server::LOCAL_FS;
use codex_orchestrator::AcceptanceCriterion;
use codex_orchestrator::ClassificationTrace;
use codex_orchestrator::Classifier;
use codex_orchestrator::DEFAULT_LLM_FALLBACK_CACHE_SIZE;
use codex_orchestrator::DEFAULT_LLM_FALLBACK_MODEL;
use codex_orchestrator::DEFAULT_LLM_FALLBACK_PROVIDER;
use codex_orchestrator::DEFAULT_LLM_FALLBACK_TIMEOUT;
use codex_orchestrator::DelegationContext;
use codex_orchestrator::DelegationSpec;
use codex_orchestrator::JsonlDecisionLog;
use codex_orchestrator::LlmClient;
use codex_orchestrator::LlmFallbackClassifier;
use codex_orchestrator::LlmFallbackConfig;
use codex_orchestrator::MinimaxDispatchSink;
use codex_orchestrator::OpenAiLlmClient;
use codex_orchestrator::OrchestrateOutcome;
use codex_orchestrator::PatternAuditor;
use codex_orchestrator::RulesClassifier;
use codex_orchestrator::SpecError;
use codex_orchestrator::TestSpec;
use codex_orchestrator::classify_with_fallback;
use codex_orchestrator::load_openai_auth;
use codex_orchestrator::run_orchestration_loop;
use codex_orchestrator::traits::DecisionLog;
use codex_orchestrator::traits::LogStage;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_cli::CliConfigOverrides;

const DEFAULT_MINIMAX_MODEL: &str = "MiniMax-M2.7";

#[derive(Debug, Clone)]
struct OrchestrateRuntimeConfig {
    codex_home: PathBuf,
    cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    llm_fallback: LlmFallbackConfig,
}

struct DisabledLlmClient;

#[derive(Debug, Args)]
#[doc = "Orchestrate a delegated turn through the MiniMax orchestrator pipeline.

Exit codes:
  0 — final verdict Ok (response accepted)
  1 — infrastructure/dispatch error (auth failure, transport error, etc.)
  2 — final verdict Escalate (needs user intervention)
  3 — final verdict Drop (loop detected or unrecoverable)"]
pub struct OrchestrateCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

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

    /// Disable the commit-7 OpenAI fallback classifier, even when no
    /// rule matches and OpenAI credentials are available.
    #[arg(long = "no-llm-fallback", default_value_t = false)]
    pub no_llm_fallback: bool,

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
    let trace = if cli.force_delegate || cli.no_delegate {
        classify_from_cli(&cli, &DisabledRulesClassifier, &disabled_llm_fallback()).await?
    } else {
        let runtime = load_runtime_config(&cli).await?;
        let rules = RulesClassifier::from_default_path(runtime.codex_home.as_path())?;
        let llm = build_llm_fallback_classifier(&runtime)?;
        classify_from_cli(&cli, &rules, &llm).await?
    };

    let spec = match &trace.outcome {
        codex_orchestrator::ClassificationOutcome::Delegate { spec, .. } => {
            let spec = build_delegation_spec_from_base(spec.clone(), &cli)?;
            log_classification(&DelegationContext::for_top_level(&spec), &trace, &log).await;
            spec
        }
        codex_orchestrator::ClassificationOutcome::PassThrough { .. } => {
            return pass_through(&cli.prompt, &trace, &log).await;
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

struct DisabledRulesClassifier;

#[async_trait::async_trait]
impl Classifier for DisabledRulesClassifier {
    async fn classify(&self, _prompt: &str) -> codex_orchestrator::ClassificationOutcome {
        panic!("rules classifier should not be called");
    }
}

#[async_trait::async_trait]
impl LlmClient for DisabledLlmClient {
    async fn classify(
        &self,
        _intent: &str,
    ) -> Result<codex_orchestrator::LlmClassification, codex_orchestrator::LlmError> {
        panic!("llm fallback should not be called");
    }
}

fn disabled_llm_fallback() -> LlmFallbackClassifier {
    LlmFallbackClassifier::new(
        Arc::new(DisabledLlmClient),
        LlmFallbackConfig {
            enabled: false,
            ..LlmFallbackConfig::default()
        },
    )
    .with_disabled_reason("llm fallback disabled")
}

async fn classify_from_cli<C: Classifier + ?Sized>(
    cli: &OrchestrateCli,
    rules: &C,
    llm_fallback: &LlmFallbackClassifier,
) -> anyhow::Result<ClassificationTrace> {
    if cli.force_delegate {
        return Ok(ClassificationTrace {
            outcome: codex_orchestrator::ClassificationOutcome::Delegate {
                spec: build_delegation_spec(cli)?,
                reason: "user-forced (--force-delegate)".to_string(),
                rule_name: None,
            },
            llm_model: None,
            llm_confidence: None,
            llm_reasoning: None,
            llm_error: None,
            cache_hit: false,
        });
    }

    if cli.no_delegate {
        return Ok(ClassificationTrace {
            outcome: codex_orchestrator::ClassificationOutcome::PassThrough {
                reason: "user-forced (--no-delegate)".to_string(),
                rule_name: None,
            },
            llm_model: None,
            llm_confidence: None,
            llm_reasoning: None,
            llm_error: None,
            cache_hit: false,
        });
    }

    Ok(classify_with_fallback(rules, llm_fallback, &cli.prompt).await)
}

fn configured_llm_fallback_enabled(config_toml: &ConfigToml, cli: &OrchestrateCli) -> bool {
    config_toml
        .orchestrator
        .as_ref()
        .and_then(|config| config.llm_fallback.as_ref())
        .and_then(|config| config.enabled)
        .unwrap_or(true)
        && !cli.no_llm_fallback
}

async fn load_runtime_config(cli: &OrchestrateCli) -> anyhow::Result<OrchestrateRuntimeConfig> {
    let cli_overrides = cli
        .config_overrides
        .parse_overrides()
        .map_err(|error| anyhow::anyhow!("Error parsing -c overrides: {error}"))?;
    let codex_home = codex_utils_home_dir::find_codex_home()
        .map_err(|error| anyhow::anyhow!("failed to resolve CODREX_HOME: {error}"))?;
    let cwd = AbsolutePathBuf::current_dir()
        .map_err(|error| anyhow::anyhow!("failed to resolve current directory: {error}"))?;
    let layers = load_config_layers_state(
        LOCAL_FS.as_ref(),
        codex_home.as_path(),
        Some(cwd),
        &cli_overrides,
        LoaderOverrides::default(),
        CloudRequirementsLoader::default(),
        &NoopThreadConfigLoader,
    )
    .await
    .map_err(|error| anyhow::anyhow!("Error loading configuration: {error}"))?;
    let effective = layers.effective_config();
    let config_toml: ConfigToml = effective
        .try_into()
        .map_err(|error| anyhow::anyhow!("invalid configuration: {error}"))?;

    Ok(OrchestrateRuntimeConfig {
        codex_home: codex_home.to_path_buf(),
        cli_auth_credentials_store_mode: config_toml
            .cli_auth_credentials_store
            .unwrap_or(AuthCredentialsStoreMode::Auto),
        llm_fallback: LlmFallbackConfig {
            provider: config_toml
                .orchestrator
                .as_ref()
                .and_then(|config| config.llm_fallback.as_ref())
                .and_then(|config| config.provider.clone())
                .unwrap_or_else(|| DEFAULT_LLM_FALLBACK_PROVIDER.to_string()),
            model: config_toml
                .orchestrator
                .as_ref()
                .and_then(|config| config.llm_fallback.as_ref())
                .and_then(|config| config.model.clone())
                .unwrap_or_else(|| DEFAULT_LLM_FALLBACK_MODEL.to_string()),
            timeout: Duration::from_millis(
                config_toml
                    .orchestrator
                    .as_ref()
                    .and_then(|config| config.llm_fallback.as_ref())
                    .and_then(|config| config.timeout_ms)
                    .unwrap_or(DEFAULT_LLM_FALLBACK_TIMEOUT.as_millis() as u64),
            ),
            cache_size: config_toml
                .orchestrator
                .as_ref()
                .and_then(|config| config.llm_fallback.as_ref())
                .and_then(|config| config.cache_size)
                .unwrap_or(DEFAULT_LLM_FALLBACK_CACHE_SIZE),
            enabled: configured_llm_fallback_enabled(&config_toml, cli),
        },
    })
}

fn build_llm_fallback_classifier(
    runtime: &OrchestrateRuntimeConfig,
) -> anyhow::Result<LlmFallbackClassifier> {
    let mut config = runtime.llm_fallback.clone();
    if !config.enabled {
        return Ok(disabled_llm_fallback());
    }
    if config.provider != DEFAULT_LLM_FALLBACK_PROVIDER {
        anyhow::bail!(
            "unsupported llm fallback provider '{}' (commit 7 supports only '{}')",
            config.provider,
            DEFAULT_LLM_FALLBACK_PROVIDER
        );
    }

    match load_openai_auth(
        runtime.codex_home.as_path(),
        runtime.cli_auth_credentials_store_mode,
    ) {
        Ok(Some(auth)) => {
            let client = OpenAiLlmClient::new(config.model.clone(), &auth)
                .map_err(|error| anyhow::anyhow!("failed to initialize llm fallback: {error}"))?;
            Ok(LlmFallbackClassifier::new(Arc::new(client), config))
        }
        Ok(None) => {
            config.enabled = false;
            Ok(
                LlmFallbackClassifier::new(Arc::new(DisabledLlmClient), config)
                    .with_disabled_reason("no openai credentials configured"),
            )
        }
        Err(error) => {
            config.enabled = false;
            Ok(
                LlmFallbackClassifier::new(Arc::new(DisabledLlmClient), config)
                    .with_disabled_reason(error.to_string()),
            )
        }
    }
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
    trace: &ClassificationTrace,
    log: &JsonlDecisionLog,
) -> anyhow::Result<OrchestrateOutcome> {
    // Pass-through: echo the intent unchanged. Logged for symmetry so
    // JSONL captures every orchestrate invocation, not only the
    // delegation path. We still build a DelegationSpec so the run gets
    // a real run_id; the dispatcher and auditor are skipped entirely.
    let placeholder_spec = DelegationSpec::new_bare(prompt)?;
    let ctx = DelegationContext::for_top_level(&placeholder_spec);
    log_classification(&ctx, trace, log).await;
    log.record(
        &ctx,
        LogStage::Decision,
        serde_json::json!({
            "verdict": "ok",
            "rationale": format!("pass-through: {}", trace.reason())
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
    trace: &ClassificationTrace,
    log: &JsonlDecisionLog,
) {
    let outcome = match &trace.outcome {
        codex_orchestrator::ClassificationOutcome::Delegate { .. } => "delegate",
        codex_orchestrator::ClassificationOutcome::PassThrough { .. } => "pass_through",
    };
    let mut payload = serde_json::json!({
        "outcome": outcome,
        "reason": trace.reason(),
    });
    if let Some(rule_name) = trace.rule_name()
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "rule_name".to_string(),
            serde_json::Value::String(rule_name.to_string()),
        );
    }
    if let Some(llm_model) = trace.llm_model.as_ref()
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "llm_model".to_string(),
            serde_json::Value::String(llm_model.clone()),
        );
        obj.insert(
            "cache_hit".to_string(),
            serde_json::Value::Bool(trace.cache_hit),
        );
    }
    if let Some(llm_confidence) = trace.llm_confidence
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "llm_confidence".to_string(),
            serde_json::Value::from(llm_confidence),
        );
    }
    if let Some(llm_reasoning) = trace.llm_reasoning.as_ref()
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "llm_reasoning".to_string(),
            serde_json::Value::String(llm_reasoning.clone()),
        );
    }
    if let Some(llm_error) = trace.llm_error.as_ref()
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert(
            "llm_error".to_string(),
            serde_json::Value::String(llm_error.clone()),
        );
    }
    log.record(ctx, LogStage::Classify, payload).await;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use async_trait::async_trait;
    use clap::Parser;
    use codex_orchestrator::Classifier;
    use codex_orchestrator::LlmClassification;
    use codex_orchestrator::LlmClient;
    use codex_orchestrator::LlmError;
    use codex_orchestrator::LlmFallbackClassifier;
    use codex_orchestrator::LlmFallbackConfig;
    use pretty_assertions::assert_eq;

    use super::OrchestrateCli;
    use super::build_delegation_spec;
    use super::classify_from_cli;
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

    #[derive(Debug)]
    struct CountingClassifier {
        calls: Arc<AtomicUsize>,
    }

    impl CountingClassifier {
        fn new() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Classifier for CountingClassifier {
        async fn classify(&self, prompt: &str) -> codex_orchestrator::ClassificationOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            codex_orchestrator::ClassificationOutcome::PassThrough {
                reason: format!("unexpected classifier call for {prompt}"),
                rule_name: None,
            }
        }
    }

    #[derive(Debug)]
    struct PanicLlmClient;

    #[async_trait]
    impl LlmClient for PanicLlmClient {
        async fn classify(&self, _intent: &str) -> Result<LlmClassification, LlmError> {
            panic!("llm fallback should not be called");
        }
    }

    fn disabled_llm_classifier() -> LlmFallbackClassifier {
        LlmFallbackClassifier::new(
            Arc::new(PanicLlmClient),
            LlmFallbackConfig {
                provider: "openai".to_string(),
                model: "gpt-5-mini".to_string(),
                timeout: Duration::from_secs(1),
                cache_size: 16,
                enabled: false,
            },
        )
    }

    #[derive(Debug)]
    struct StaticRuleClassifier {
        calls: Arc<AtomicUsize>,
        outcome: codex_orchestrator::ClassificationOutcome,
    }

    impl StaticRuleClassifier {
        fn delegate(prompt: &str) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                outcome: codex_orchestrator::ClassificationOutcome::Delegate {
                    spec: DelegationSpec::new_bare(prompt).expect("delegate spec should build"),
                    reason: "matched rule 'implement_function'".to_string(),
                    rule_name: Some("implement_function".to_string()),
                },
            }
        }

        fn no_match() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                outcome: codex_orchestrator::ClassificationOutcome::PassThrough {
                    reason: "no rule matched (LLM fallback in commit 7)".to_string(),
                    rule_name: None,
                },
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Classifier for StaticRuleClassifier {
        async fn classify(&self, _prompt: &str) -> codex_orchestrator::ClassificationOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.outcome.clone()
        }
    }

    #[derive(Debug)]
    struct CountingLlmClient {
        calls: Arc<AtomicUsize>,
        result: Result<LlmClassification, LlmError>,
    }

    impl CountingLlmClient {
        fn delegate() -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                result: Ok(LlmClassification {
                    should_delegate: true,
                    confidence: 0.91,
                    reasoning: "mechanical code conversion".to_string(),
                }),
            }
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmClient for CountingLlmClient {
        async fn classify(&self, _intent: &str) -> Result<LlmClassification, LlmError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    fn enabled_llm_classifier(client: Arc<dyn LlmClient>) -> LlmFallbackClassifier {
        LlmFallbackClassifier::new(
            client,
            LlmFallbackConfig {
                provider: "openai".to_string(),
                model: "gpt-5-mini".to_string(),
                timeout: Duration::from_secs(1),
                cache_size: 16,
                enabled: true,
            },
        )
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

    #[test]
    fn parse_cli_accepts_no_llm_fallback() {
        let cli = parse_cli(&["codrex", "prompt", "--no-llm-fallback"]);

        assert!(cli.no_llm_fallback);
    }

    #[tokio::test]
    async fn force_delegate_skips_classifiers_entirely() {
        let cli = parse_cli(&["codrex", "prompt", "--force-delegate"]);
        let rules = CountingClassifier::new();
        let llm = disabled_llm_classifier();

        let trace = classify_from_cli(&cli, &rules, &llm)
            .await
            .expect("classification should succeed");

        assert!(matches!(
            trace.outcome,
            codex_orchestrator::ClassificationOutcome::Delegate { .. }
        ));
        assert_eq!(trace.reason(), "user-forced (--force-delegate)");
        assert_eq!(rules.call_count(), 0);
    }

    #[tokio::test]
    async fn no_delegate_skips_classifiers_entirely() {
        let cli = parse_cli(&["codrex", "prompt", "--no-delegate"]);
        let rules = CountingClassifier::new();
        let llm = disabled_llm_classifier();

        let trace = classify_from_cli(&cli, &rules, &llm)
            .await
            .expect("classification should succeed");

        assert!(matches!(
            trace.outcome,
            codex_orchestrator::ClassificationOutcome::PassThrough { .. }
        ));
        assert_eq!(trace.reason(), "user-forced (--no-delegate)");
        assert_eq!(rules.call_count(), 0);
    }

    #[tokio::test]
    async fn rules_match_skips_llm_fallback() {
        let cli = parse_cli(&["codrex", "implement validate_email"]);
        let rules = StaticRuleClassifier::delegate("implement validate_email");
        let llm_client = Arc::new(CountingLlmClient::delegate());
        let llm = enabled_llm_classifier(llm_client.clone());

        let trace = classify_from_cli(&cli, &rules, &llm)
            .await
            .expect("classification should succeed");

        assert!(matches!(
            trace.outcome,
            codex_orchestrator::ClassificationOutcome::Delegate { .. }
        ));
        assert_eq!(trace.reason(), "matched rule 'implement_function'");
        assert_eq!(rules.call_count(), 1);
        assert_eq!(llm_client.call_count(), 0);
    }

    #[tokio::test]
    async fn no_match_triggers_llm_fallback() {
        let cli = parse_cli(&["codrex", "convert this XML config to YAML format"]);
        let rules = StaticRuleClassifier::no_match();
        let llm_client = Arc::new(CountingLlmClient::delegate());
        let llm = enabled_llm_classifier(llm_client.clone());

        let trace = classify_from_cli(&cli, &rules, &llm)
            .await
            .expect("classification should succeed");

        assert!(matches!(
            trace.outcome,
            codex_orchestrator::ClassificationOutcome::Delegate { .. }
        ));
        assert_eq!(trace.reason(), "llm fallback");
        assert_eq!(rules.call_count(), 1);
        assert_eq!(llm_client.call_count(), 1);
    }
}
