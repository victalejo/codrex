//! End-to-end integration tests for the Phase 3 orchestration loop.
//!
//! Tests 1–6 exercise [`run_orchestration_loop`] with a `wiremock`
//! `MockServer` that pretends to be MiniMax. The dispatch sink is
//! redirected at the mock by setting `MINIMAX_BASE_URL` before each
//! test runs, then restored by an [`fixtures::EnvGuard`] when the test
//! finishes. Tests are serialised on `serial_test::serial(env_minimax)`
//! to avoid env-var races between tokio tasks.
//!
//! Tests 7–8 cover the two pass-through paths the rules classifier can
//! produce. The real CLI returns *before* invoking the orchestration
//! loop when classification yields `PassThrough`, so these tests
//! exercise [`classify_with_fallback`] directly with an inline rules
//! file and a disabled LLM fallback. The fidelity is 1:1 with the
//! CLI's behaviour at
//! [`cli/src/orchestrate_cmd.rs:178-180`](../../cli/src/orchestrate_cmd.rs#L178).

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use codex_orchestrator::AcceptanceCriterion;
use codex_orchestrator::ClassificationOutcome;
use codex_orchestrator::DelegationSpec;
use codex_orchestrator::InMemoryDecisionLog;
use codex_orchestrator::LlmClassification;
use codex_orchestrator::LlmClient;
use codex_orchestrator::LlmError;
use codex_orchestrator::LlmFallbackClassifier;
use codex_orchestrator::LlmFallbackConfig;
use codex_orchestrator::LogStage;
use codex_orchestrator::MinimaxDispatchSink;
use codex_orchestrator::OrchestrateOutcome;
use codex_orchestrator::PatternAuditor;
use codex_orchestrator::RulesClassifier;
use codex_orchestrator::classify_with_fallback;
use codex_orchestrator::log::RecordedEvent;
use codex_orchestrator::run_orchestration_loop;
use serial_test::serial;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

mod fixtures {
    use super::*;

    /// Build a single Server-Sent Events body with one content chunk
    /// and one usage chunk, matching the MiniMax wire shape the bridge
    /// expects.
    pub fn sse_body(content: &str) -> String {
        let content_chunk = serde_json::json!({
            "id": "resp-test",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": content}}],
        })
        .to_string();
        let usage_chunk = serde_json::json!({
            "id": "resp-test",
            "choices": [{"index": 0, "finish_reason": "stop", "delta": {}}],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3,
                "total_tokens": 8,
            },
        })
        .to_string();
        format!("data: {content_chunk}\n\ndata: {usage_chunk}\n\ndata: [DONE]\n\n")
    }

    /// Mount a sequence of canned responses on `server`. The Nth POST
    /// to `/v1/chat/completions` returns the Nth response body wrapped
    /// in a single-chunk SSE envelope. A POST past the end of the
    /// script gets a 500, so runaway dispatches surface loudly.
    /// Returns a counter the caller can inspect after the run.
    pub async fn mount_scripted_responses_async(
        server: &MockServer,
        responses: Vec<String>,
    ) -> Arc<AtomicUsize> {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_mock = Arc::clone(&counter);
        let response_count = responses.len();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_request: &Request| {
                let index = counter_for_mock.fetch_add(1, Ordering::SeqCst);
                match responses.get(index) {
                    Some(body) => ResponseTemplate::new(200)
                        .set_body_string(sse_body(body))
                        .insert_header("content-type", "text/event-stream"),
                    None => {
                        ResponseTemplate::new(500).set_body_string("scripted responses exhausted")
                    }
                }
            })
            .expect(response_count as u64)
            .mount(server)
            .await;
        counter
    }

    /// RAII guard that points the MiniMax client at a wiremock server
    /// for the duration of a test and restores the prior environment
    /// when dropped. The drop path is defensive: even if the
    /// environment was mutated mid-test, restoration never panics.
    pub struct EnvGuard {
        prev_base_url: Option<String>,
        prev_api_key: Option<String>,
    }

    impl EnvGuard {
        pub fn install(server: &MockServer) -> Self {
            let prev_base_url = std::env::var("MINIMAX_BASE_URL").ok();
            let prev_api_key = std::env::var("MINIMAX_API_KEY").ok();
            // SAFETY: tests are serialised on `#[serial(env_minimax)]`,
            // so no concurrent access to these vars is possible.
            unsafe {
                std::env::set_var("MINIMAX_BASE_URL", format!("{}/v1", server.uri()));
                std::env::set_var("MINIMAX_API_KEY", "sk-test-key");
            }
            Self {
                prev_base_url,
                prev_api_key,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same as `install` — tests are serialised on
            // `#[serial(env_minimax)]`. The std env API doesn't panic
            // in practice; double-restoration on already-removed vars
            // is a no-op.
            unsafe {
                match self.prev_base_url.take() {
                    Some(value) => std::env::set_var("MINIMAX_BASE_URL", value),
                    None => std::env::remove_var("MINIMAX_BASE_URL"),
                }
                match self.prev_api_key.take() {
                    Some(value) => std::env::set_var("MINIMAX_API_KEY", value),
                    None => std::env::remove_var("MINIMAX_API_KEY"),
                }
            }
        }
    }

    /// Bare delegation spec with a user-provided intent and default
    /// retry budget. Suitable for happy-path tests; mutate fields on
    /// the returned value for retry/forbidden/output-match scenarios.
    pub fn build_spec(intent: &str) -> DelegationSpec {
        DelegationSpec::new_bare(intent).expect("intent must be non-empty")
    }

    /// Filter `events` to those at a given stage, returning a vec of
    /// references for further inspection.
    pub fn find_events_by_stage(events: &[RecordedEvent], stage: LogStage) -> Vec<&RecordedEvent> {
        events.iter().filter(|e| e.stage == stage).collect()
    }

    /// Assert that exactly one `decision` event was logged with the
    /// expected verdict string. Panics with the full event log
    /// embedded in the message so failures debugged from CI logs alone
    /// are tractable.
    pub fn assert_decision_verdict(events: &[RecordedEvent], expected: &str) {
        let decisions = find_events_by_stage(events, LogStage::Decision);
        assert_eq!(
            decisions.len(),
            1,
            "expected exactly 1 decision event, found {}.\nFull event log:\n{:#?}",
            decisions.len(),
            events
        );
        let actual = decisions[0]
            .payload
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<missing verdict field>");
        assert_eq!(
            actual, expected,
            "decision verdict mismatch.\nFull event log:\n{:#?}",
            events
        );
    }

    /// Build a `RulesClassifier` from inline TOML for tests that need
    /// to control matching deterministically.
    pub fn inline_rules_classifier(toml: &str) -> RulesClassifier {
        RulesClassifier::from_toml_str(toml).expect("test rules toml must parse")
    }

    /// LLM client that panics on any classification call. Wraps the
    /// "fallback explicitly disabled — must not be invoked" contract
    /// in a single helper.
    struct DisabledLlmClient;

    #[async_trait]
    impl LlmClient for DisabledLlmClient {
        async fn classify(&self, _intent: &str) -> Result<LlmClassification, LlmError> {
            panic!("disabled llm fallback should not be invoked");
        }
    }

    /// Mirrors the `LlmFallback` construction in
    /// `cli/orchestrate_cmd.rs:404-407` for the "no credentials"
    /// path. If that wiring changes, this helper must follow.
    pub fn disabled_llm_fallback() -> LlmFallbackClassifier {
        LlmFallbackClassifier::new(
            Arc::new(DisabledLlmClient),
            LlmFallbackConfig {
                enabled: false,
                ..LlmFallbackConfig::default()
            },
        )
        .with_disabled_reason("no openai credentials configured")
    }
}

use fixtures::EnvGuard;
use fixtures::assert_decision_verdict;
use fixtures::build_spec;
use fixtures::disabled_llm_fallback;
use fixtures::find_events_by_stage;
use fixtures::inline_rules_classifier;
use fixtures::mount_scripted_responses_async;

// =============================================================================
// Test 1 — happy path: rule match → delegate → MiniMax responds → audit ok
// =============================================================================

#[tokio::test]
#[serial(env_minimax)]
async fn happy_delegate_returns_code() {
    let server = MockServer::start().await;
    let counter = mount_scripted_responses_async(
        &server,
        vec!["fn add(a: i32, b: i32) -> i32 { a + b }".to_string()],
    )
    .await;
    let _env = EnvGuard::install(&server);

    let spec = build_spec("implement add");
    let log = InMemoryDecisionLog::new();
    let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
    let auditor = PatternAuditor::new();

    let outcome = run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
        .await
        .expect("orchestration loop must not error");

    match &outcome {
        OrchestrateOutcome::Ok { response_text } => {
            assert!(
                response_text.contains("a + b"),
                "response_text should echo MiniMax body, got: {response_text}"
            );
        }
        other => panic!("expected Ok, got {other:?}"),
    }
    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "exactly 1 dispatch expected"
    );

    let events = log.events();
    assert_decision_verdict(&events, "ok");
    assert_eq!(
        find_events_by_stage(&events, LogStage::DispatchStart).len(),
        1
    );
    assert_eq!(
        find_events_by_stage(&events, LogStage::DispatchEnd).len(),
        1
    );
    assert_eq!(find_events_by_stage(&events, LogStage::Audit).len(), 1);
}

// =============================================================================
// Test 2 — retry: attempt 0 fails output_match, attempt 1 passes
// =============================================================================

#[tokio::test]
#[serial(env_minimax)]
async fn retry_succeeds_on_attempt_1() {
    let server = MockServer::start().await;
    // Attempt 0 lacks "recursive" → output_match fails → retry.
    // Attempt 1 contains "recursive" → output_match passes → ok.
    let counter = mount_scripted_responses_async(
        &server,
        vec![
            "iteration of foo using a for loop".to_string(),
            "recursive helper that calls itself with n-1".to_string(),
        ],
    )
    .await;
    let _env = EnvGuard::install(&server);

    let mut spec = build_spec("implement fibonacci recursively");
    spec.acceptance =
        vec![AcceptanceCriterion::output_matches(r"\brecursive\b").expect("regex must compile")];
    spec.max_retries = 2;
    let log = InMemoryDecisionLog::new();
    let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
    let auditor = PatternAuditor::new();

    let outcome = run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
        .await
        .expect("orchestration loop must not error");

    match &outcome {
        OrchestrateOutcome::Ok { response_text } => {
            assert!(response_text.contains("recursive"));
        }
        other => panic!("expected Ok after retry, got {other:?}"),
    }
    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "exactly 2 dispatches expected"
    );

    let events = log.events();
    assert_eq!(
        find_events_by_stage(&events, LogStage::DispatchStart).len(),
        2,
        "two dispatch_start rows (attempts 0, 1)"
    );
    assert_eq!(find_events_by_stage(&events, LogStage::Audit).len(), 2);

    let decisions = find_events_by_stage(&events, LogStage::Decision);
    assert_eq!(decisions.len(), 2, "retry decision then ok decision");
    assert_eq!(
        decisions[0]
            .payload
            .get("verdict")
            .and_then(serde_json::Value::as_str),
        Some("retry")
    );
    assert_eq!(
        decisions[1]
            .payload
            .get("verdict")
            .and_then(serde_json::Value::as_str),
        Some("ok")
    );
}

// =============================================================================
// Test 3 — max_retries exhausted: 3 attempts with distinct signatures
// =============================================================================

#[tokio::test]
#[serial(env_minimax)]
async fn max_retries_exhausted_escalates() {
    let server = MockServer::start().await;
    // Three responses that all fail output_match on different excerpts
    // → three distinct error signatures → no loop drop, just exhaust.
    // max_retries=2 → attempts 0, 1, 2 (3 total) before Escalate.
    let counter = mount_scripted_responses_async(
        &server,
        vec![
            "alpha branch implementation v1".to_string(),
            "beta variant of the same logic".to_string(),
            "gamma rewrite ignoring the spec entirely".to_string(),
        ],
    )
    .await;
    let _env = EnvGuard::install(&server);

    let mut spec = build_spec("implement parse_iso_date");
    spec.acceptance =
        vec![AcceptanceCriterion::output_matches(r"parse_iso_date").expect("regex must compile")];
    spec.max_retries = 2;
    let log = InMemoryDecisionLog::new();
    let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
    let auditor = PatternAuditor::new();

    let outcome = run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
        .await
        .expect("orchestration loop must not error");

    match &outcome {
        OrchestrateOutcome::Escalate {
            reason,
            attempts_exhausted,
            ..
        } => {
            assert_eq!(reason, "max_retries_exhausted");
            assert_eq!(*attempts_exhausted, Some(3));
        }
        other => panic!("expected Escalate after exhausted retries, got {other:?}"),
    }
    assert_eq!(outcome.exit_code(), 2);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        3,
        "three dispatches expected"
    );

    let events = log.events();
    assert_eq!(
        find_events_by_stage(&events, LogStage::DispatchStart).len(),
        3
    );
    let decisions = find_events_by_stage(&events, LogStage::Decision);
    let final_decision = decisions
        .last()
        .expect("at least one decision event expected");
    assert_eq!(
        final_decision
            .payload
            .get("verdict")
            .and_then(serde_json::Value::as_str),
        Some("escalate"),
    );
    assert_eq!(
        final_decision
            .payload
            .get("attempts_exhausted")
            .and_then(serde_json::Value::as_u64),
        Some(3),
    );
}

// =============================================================================
// Test 4 — loop detected: 2 attempts with identical signatures → Drop
// =============================================================================

#[tokio::test]
#[serial(env_minimax)]
async fn loop_detected_drops() {
    let server = MockServer::start().await;
    // Identical responses → identical FailedCriterion details →
    // identical signatures → loop detection short-circuits before
    // exhausting retries.
    let identical = "the model insists on the same wrong answer".to_string();
    let counter =
        mount_scripted_responses_async(&server, vec![identical.clone(), identical.clone()]).await;
    let _env = EnvGuard::install(&server);

    let mut spec = build_spec("implement the right thing");
    spec.acceptance =
        vec![AcceptanceCriterion::output_matches(r"correct").expect("regex must compile")];
    spec.max_retries = 5; // generous budget so the trip is via loop, not exhaust
    let log = InMemoryDecisionLog::new();
    let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
    let auditor = PatternAuditor::new();

    let outcome = run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
        .await
        .expect("orchestration loop must not error");

    match &outcome {
        OrchestrateOutcome::Drop {
            reason,
            repeated_signature,
        } => {
            assert_eq!(reason, "loop_detected");
            assert!(
                repeated_signature.is_some(),
                "Drop on loop must include the repeated signature"
            );
        }
        other => panic!("expected Drop on loop, got {other:?}"),
    }
    assert_eq!(outcome.exit_code(), 3);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "loop should drop after the second matching signature, not retry to 3"
    );

    let events = log.events();
    let decisions = find_events_by_stage(&events, LogStage::Decision);
    let final_decision = decisions
        .last()
        .expect("at least one decision event expected");
    assert_eq!(
        final_decision
            .payload
            .get("verdict")
            .and_then(serde_json::Value::as_str),
        Some("drop"),
    );
    assert!(
        final_decision.payload.get("repeated_signature").is_some(),
        "drop payload must include the repeated_signature field for analysis",
    );
}

// =============================================================================
// Test 5 — forbidden pattern: immediate Escalate, no retry
// =============================================================================

#[tokio::test]
#[serial(env_minimax)]
async fn forbidden_pattern_escalates_immediately() {
    let server = MockServer::start().await;
    let counter = mount_scripted_responses_async(
        &server,
        vec!["fn add(a, b) { /* TODO: implement */ }".to_string()],
    )
    .await;
    let _env = EnvGuard::install(&server);

    let mut spec = build_spec("implement add");
    spec.set_forbidden_patterns(vec!["TODO"])
        .expect("forbidden pattern must compile");
    spec.acceptance = vec![AcceptanceCriterion::NoForbiddenPatterns];
    spec.max_retries = 5; // budget exists but should be unused
    let log = InMemoryDecisionLog::new();
    let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
    let auditor = PatternAuditor::new();

    let outcome = run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
        .await
        .expect("orchestration loop must not error");

    match &outcome {
        OrchestrateOutcome::Escalate { blocking_issue, .. } => {
            assert!(
                blocking_issue.contains("forbidden") || blocking_issue.contains("TODO"),
                "blocking_issue should mention the matched pattern, got: {blocking_issue}"
            );
        }
        other => panic!("expected Escalate immediately, got {other:?}"),
    }
    assert_eq!(outcome.exit_code(), 2);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "forbidden patterns must escalate without retrying"
    );

    let events = log.events();
    assert_eq!(
        find_events_by_stage(&events, LogStage::DispatchStart).len(),
        1,
        "exactly one dispatch — no retry on forbidden",
    );
    assert_decision_verdict(&events, "escalate");
}

// =============================================================================
// Test 6 — CLARIFY: model asks question, exit 4, framing on stderr
// =============================================================================

#[tokio::test]
#[serial(env_minimax)]
async fn clarify_response_exits_4() {
    let server = MockServer::start().await;
    let counter = mount_scripted_responses_async(
        &server,
        vec!["CLARIFY: should validate_email accept a list or a single string?".to_string()],
    )
    .await;
    let _env = EnvGuard::install(&server);

    let spec = build_spec("implement validate_email");
    let log = InMemoryDecisionLog::new();
    let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
    let auditor = PatternAuditor::new();

    let outcome = run_orchestration_loop(&spec, "MiniMax-M2.7", &sink, &auditor, &log)
        .await
        .expect("orchestration loop must not error");

    match &outcome {
        OrchestrateOutcome::Clarify { question } => {
            assert_eq!(
                question, "should validate_email accept a list or a single string?",
                "question must be the body of the CLARIFY: prefix, trimmed"
            );
        }
        other => panic!("expected Clarify, got {other:?}"),
    }
    assert_eq!(outcome.exit_code(), 4);
    assert_eq!(counter.load(Ordering::SeqCst), 1);

    let events = log.events();
    assert_eq!(
        find_events_by_stage(&events, LogStage::Clarify).len(),
        1,
        "a clarify stage row must be present alongside the decision",
    );
    assert_decision_verdict(&events, "clarify");
}

// =============================================================================
// Test 7 — rule no_delegate: classifier returns PassThrough, dispatch never runs
// =============================================================================

#[tokio::test]
async fn rule_no_delegate_skips_dispatch() {
    let rules_toml = r#"
version = 1

[[rule]]
name = "design_arch"
action = "no_delegate"
patterns = ["(?i)\\bdesign\\s+(?:the\\s+)?(?:architecture|api|schema|system)\\b"]
"#;
    let rules = inline_rules_classifier(rules_toml);
    let llm_fallback = disabled_llm_fallback();

    // The pattern requires `design [the] (architecture|api|schema|system)`
    // with one of those four tokens immediately following. "Design the api"
    // exercises the no_delegate path cleanly; "Design the auth schema"
    // would not match (the prompt has "auth" between "the" and "schema",
    // tracked separately as TODO #20 — not what this test is about).
    let trace = classify_with_fallback(&rules, &llm_fallback, "Design the api").await;

    match trace.outcome {
        ClassificationOutcome::PassThrough { .. } => {}
        ClassificationOutcome::Delegate { .. } => {
            panic!(
                "expected PassThrough on no_delegate rule match; got Delegate.\n\
                 reason: {}",
                trace.reason()
            );
        }
    }
    assert_eq!(
        trace.rule_name(),
        Some("design_arch"),
        "rule_name should record the matched rule for audit visibility",
    );
    assert!(
        trace.reason().contains("design_arch"),
        "reason should reference the matched rule, got: {}",
        trace.reason(),
    );
    // No mock MiniMax server was started — proving by absence that
    // dispatch never runs in this path.
}

// =============================================================================
// Test 8 — no rule match + no creds: pass-through with the distinctive reason
// =============================================================================

#[tokio::test]
async fn no_match_no_creds_passes_through() {
    let rules_toml = r#"
version = 1

[[rule]]
name = "implement_function"
action = "delegate"
patterns = ["(?i)\\bimplement\\s+(?:a\\s+|the\\s+)?function\\b"]
"#;
    let rules = inline_rules_classifier(rules_toml);
    let llm_fallback = disabled_llm_fallback();

    // Prompt that won't hit any rule above; with the LLM fallback
    // disabled (no credentials), classification must pass through.
    let trace = classify_with_fallback(&rules, &llm_fallback, "summarise this codebase").await;

    match trace.outcome {
        ClassificationOutcome::PassThrough { .. } => {}
        ClassificationOutcome::Delegate { .. } => {
            panic!(
                "expected PassThrough when no rule matches + fallback disabled; got Delegate.\n\
                 reason: {}",
                trace.reason()
            );
        }
    }
    // The "no openai credentials configured" string we pass to
    // `with_disabled_reason` is recognised internally and rewritten to
    // the canonical user-facing constant
    // `NO_CREDENTIALS_DISABLED_REASON = "llm fallback disabled (no credentials)"`,
    // then concatenated with "no rule matched + " by `classify_with_fallback`.
    // This locks the distinctive reason added in commit `092f86b8b` —
    // "(no credentials)" must remain visible so users can tell this case
    // apart from a transport-error fallback miss.
    let reason = trace.reason();
    assert!(
        reason.contains("no rule matched"),
        "reason must indicate rules-classifier missed, got: {reason}",
    );
    assert!(
        reason.contains("no credentials"),
        "reason must surface the no-credentials explanation distinctively, got: {reason}",
    );
    assert_eq!(
        trace.rule_name(),
        None,
        "no rule matched, so rule_name must be absent",
    );
}

// =============================================================================
// Bonus — lock the OrchestrateOutcome -> exit code mapping main.rs depends on
// =============================================================================

#[test]
fn outcome_exit_codes_match_main_translation() {
    // If a future variant is added without an `exit_code` arm,
    // main.rs's mapping silently breaks. This test pins the contract.
    assert_eq!(
        OrchestrateOutcome::Ok {
            response_text: String::new()
        }
        .exit_code(),
        0,
    );
    assert_eq!(
        OrchestrateOutcome::Clarify {
            question: String::new()
        }
        .exit_code(),
        4,
    );
    assert_eq!(
        OrchestrateOutcome::Escalate {
            reason: String::new(),
            blocking_issue: String::new(),
            attempts_exhausted: None,
        }
        .exit_code(),
        2,
    );
    assert_eq!(
        OrchestrateOutcome::Drop {
            reason: String::new(),
            repeated_signature: None,
        }
        .exit_code(),
        3,
    );
}
