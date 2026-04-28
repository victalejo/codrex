//! `MinimaxDispatchSink` — sends a delegated turn to MiniMax and
//! captures the response.
//!
//! ## Phase 3 commit 3 scope (intentionally narrow)
//!
//! The sink builds a **minimal** `ChatCompletionRequest` directly from
//! the `DelegationSpec` — one system message that frames the
//! delegation contract (intent + forbidden hints + utility refs +
//! CLARIFY: convention) and one user message with the bare intent.
//! No tools, no plugin context, no rollout history.
//!
//! Wiring through the full `core::minimax_adapter::stream_chat_completions`
//! path (which builds tools, applies the role/system invariants from
//! Phase 2.5, etc.) arrives in commits 4-5 when we need tool execution
//! to evaluate `TestsPass` acceptance criteria. For commit 3 the
//! happy path is "ask MiniMax a question, get an answer back".
//!
//! ## CLARIFY: detection
//!
//! Per the Phase 3 plan adjustment #2, the sink eagerly detects a
//! `CLARIFY:` prefix in MiniMax's response and surfaces it as
//! [`DispatchError::ClarificationRequested`] from commit 3 onward.
//! Commit 8 will turn that into an interactive round-trip; in commits
//! 3-7 the orchestrator surfaces the question to the user with a
//! "not yet implemented" hint and exits non-zero.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use codex_api::ResponseEvent;
use codex_config::types::AuthCredentialsStoreMode;
use codex_minimax::AuthPreference;
use codex_minimax::MinimaxClient;
use codex_minimax::ResolvedAuth;
use codex_minimax::ResponseEventBridge;
use codex_minimax::resolve_auth_from_env;
use codex_minimax::resolve_base_url;
use codex_minimax::types::ChatCompletionRequest;
use codex_minimax::types::ChatMessage;
use futures::StreamExt;
use tracing::warn;

use crate::context::DelegationContext;
use crate::decision::RetryFeedback;
use crate::orch_debug_enabled;
use crate::spec::DelegationSpec;
use crate::traits::DispatchError;
use crate::traits::DispatchOutcome;
use crate::traits::DispatchSink;

/// MiniMax-backed `DispatchSink` for the Phase 3 orchestrator.
///
/// The sink owns its own `reqwest::Client` (shared across dispatches
/// for connection pooling) and the model slug to use. Auth resolution
/// happens lazily on each dispatch — env vars first, `auth.json`
/// fallback second. The base URL respects the MiniMax-region overrides
/// the existing adapter already supports.
#[derive(Clone)]
pub struct MinimaxDispatchSink {
    http: reqwest::Client,
    model: String,
    /// Optional override for the auth backend the sink consults when
    /// reading `auth.json`. Defaults to `Auto` so it follows whichever
    /// store mode the user has configured for OpenAI auth.
    store_mode: AuthCredentialsStoreMode,
}

impl MinimaxDispatchSink {
    /// Construct a sink that targets `model` and resolves auth via the
    /// default `Auto` store mode.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            model: model.into(),
            store_mode: AuthCredentialsStoreMode::Auto,
        }
    }

    /// Test-only: inject a pre-built `reqwest::Client` (e.g. one whose
    /// requests are routed at a `wiremock::MockServer`).
    pub fn with_http_client(model: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            http,
            model: model.into(),
            store_mode: AuthCredentialsStoreMode::Auto,
        }
    }

    fn resolve_auth(&self) -> Result<ResolvedAuth, DispatchError> {
        // 1. env vars (MINIMAX_API_KEY / MINIMAX_CODING_PLAN_KEY).
        if let Ok(auth) = resolve_auth_from_env(AuthPreference::default()) {
            return Ok(auth);
        }
        // 2. auth.json fallback. Mirrors core::minimax_adapter so users
        // who ran `codrex login minimax` see the orchestrator pick up
        // their saved credentials transparently.
        if let Some(auth) = self.load_auth_from_file() {
            return Ok(auth);
        }
        Err(DispatchError::Transport(
            "no credentials for provider 'minimax'. Run `codrex login minimax` or set \
             MINIMAX_API_KEY (pay-as-you-go) / MINIMAX_CODING_PLAN_KEY (Coding Plan)."
                .to_string(),
        ))
    }

    fn load_auth_from_file(&self) -> Option<ResolvedAuth> {
        let codex_home = codex_utils_home_dir::find_codex_home().ok()?;
        let creds = codex_login::load_provider_credentials(
            codex_home.as_path(),
            self.store_mode,
            codex_minimax::MINIMAX_PROVIDER_ID,
        )
        .ok()
        .flatten()?;
        let env_var = match creds.kind.as_deref() {
            Some("coding_plan") => "MINIMAX_CODING_PLAN_KEY",
            _ => "MINIMAX_API_KEY",
        };
        Some(ResolvedAuth {
            bearer_token: creds.api_key,
            env_var,
        })
    }

    /// Build the `ChatCompletionRequest` sent to MiniMax. Pure function
    /// over the spec — exposed for tests so they can lock the wire
    /// shape without spinning up the HTTP client.
    pub fn build_request(
        &self,
        spec: &DelegationSpec,
        retry_feedback: Option<&RetryFeedback>,
    ) -> ChatCompletionRequest {
        let system_body = render_system_message(spec, retry_feedback);
        let messages = vec![
            ChatMessage::system(system_body),
            ChatMessage::user(spec.intent.clone()),
        ];
        ChatCompletionRequest::new(&self.model, messages)
    }
}

fn render_system_message(spec: &DelegationSpec, retry_feedback: Option<&RetryFeedback>) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(
        s,
        "You are MiniMax, executing a delegated task as part of the Codrex orchestrator (run_id={}).",
        spec.run_id
    );
    let _ = writeln!(s, "Respond with a complete answer to the intent below.");
    if !spec.forbidden_patterns.is_empty() {
        let _ = writeln!(s, "\nAVOID the following patterns in your output:");
        for pat in &spec.forbidden_patterns {
            let _ = writeln!(s, "  - {pat}");
        }
    }
    if !spec.utility_refs.is_empty() {
        let _ = writeln!(
            s,
            "\nWhen possible, REUSE these existing utilities rather than re-implementing:"
        );
        for u in &spec.utility_refs {
            let sig = u
                .signature
                .as_deref()
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            let _ = writeln!(s, "  - {} (`{}`{sig})", u.symbol, u.path.display());
        }
    }
    let _ = writeln!(
        s,
        "\nIf you need disambiguation BEFORE attempting the task, prefix your response with \
         `CLARIFY:` followed by ONE concrete question. Otherwise complete the task."
    );
    if let Some(feedback) = retry_feedback
        && !feedback.failed_criteria.is_empty()
    {
        let _ = writeln!(
            s,
            "\nPrevious attempt failed acceptance criteria. Address these failures:"
        );
        let _ = writeln!(s, "{}", render_retry_feedback(feedback));
    }
    s
}

fn render_retry_feedback(feedback: &RetryFeedback) -> String {
    use std::fmt::Write;

    let mut s = String::new();
    let mut idx = 1usize;
    for criterion in &feedback.failed_criteria {
        let kind = match criterion.kind {
            crate::decision::CriterionKind::OutputMatches => "output_matches",
            crate::decision::CriterionKind::TestsPass => "tests_pass",
            crate::decision::CriterionKind::Custom => "custom",
        };
        let _ = writeln!(s, "{idx}. {} ({kind})", criterion.name);
        match &criterion.details {
            crate::decision::FailureDetails::OutputMatches {
                regex,
                output_excerpt,
            } => {
                let _ = writeln!(s, "   regex: {regex}");
                let _ = writeln!(
                    s,
                    "   output_excerpt: {}",
                    output_excerpt.replace('\n', " ")
                );
            }
            crate::decision::FailureDetails::TestsPass {
                exit_code,
                stderr_excerpt,
                command,
            } => {
                let _ = writeln!(s, "   command: {}", command.join(" "));
                let _ = writeln!(s, "   exit_code: {exit_code}");
                let _ = writeln!(
                    s,
                    "   stderr_excerpt: {}",
                    stderr_excerpt.replace('\n', " ")
                );
            }
            crate::decision::FailureDetails::Custom {
                exit_code,
                stderr_excerpt,
            } => {
                let _ = writeln!(s, "   exit_code: {exit_code}");
                let _ = writeln!(
                    s,
                    "   stderr_excerpt: {}",
                    stderr_excerpt.replace('\n', " ")
                );
            }
        }
        idx = idx.saturating_add(1);
    }
    s
}

#[async_trait]
impl DispatchSink for MinimaxDispatchSink {
    async fn dispatch(
        &self,
        spec: &DelegationSpec,
        ctx: &DelegationContext,
    ) -> Result<DispatchOutcome, DispatchError> {
        let started = Instant::now();
        let auth = self.resolve_auth()?;
        let base_url = resolve_base_url();
        let client = MinimaxClient::new(self.http.clone(), base_url, auth);
        let request = self.build_request(spec, ctx.retry_feedback.as_ref());

        if orch_debug_enabled() {
            eprintln!(
                "[codrex/orch] dispatch run_id={} model={} (system_len={} user_len={})",
                ctx.run_id,
                self.model,
                request
                    .messages
                    .first()
                    .map(|m| m.content.len())
                    .unwrap_or(0),
                request
                    .messages
                    .last()
                    .map(|m| m.content.len())
                    .unwrap_or(0),
            );
        }

        let bridge = Arc::new(tokio::sync::Mutex::new(
            ResponseEventBridge::with_telemetry(
                ctx.run_id.to_string(),
                self.model.clone(),
                started,
            ),
        ));

        let mut stream = client
            .chat_completion_stream(&request)
            .await
            .map_err(|e| DispatchError::Transport(e.to_string()))?;

        let mut accumulated_text = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| DispatchError::Transport(e.to_string()))?;
            let events = {
                let mut guard = bridge.lock().await;
                guard.ingest(chunk)
            };
            for ev in events {
                if let ResponseEvent::OutputTextDelta(t) = ev {
                    accumulated_text.push_str(&t);
                }
            }
        }

        // Drain bridge: surfaces the Completed event with token usage
        // and emits the structured `codrex.cost` log keyed by run_id —
        // this is the day-one telemetry tie-in the Phase 3 plan asks
        // for. The cost log lands automatically because we built the
        // bridge with `with_telemetry(run_id, model, started)`.
        let final_events = {
            let bridge = Arc::try_unwrap(bridge)
                .map_err(|_| {
                    DispatchError::Transport(
                        "bridge had outstanding references; cannot finalize".to_string(),
                    )
                })?
                .into_inner();
            bridge.finalize()
        };
        let mut total_tokens: Option<u64> = None;
        for ev in final_events {
            match ev {
                ResponseEvent::OutputTextDelta(t) => accumulated_text.push_str(&t),
                ResponseEvent::Completed { token_usage, .. } => {
                    total_tokens = token_usage.map(|u| u.total_tokens.max(0) as u64);
                }
                _ => {}
            }
        }

        // Eagerly detect MiniMax requesting clarification. The full
        // round-trip lands in commit 8; commits 3-7 surface this as a
        // dispatch error so the orchestrator exits non-zero with a
        // clear message rather than silently treating the question as
        // a normal answer.
        let trimmed = accumulated_text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("CLARIFY:") {
            return Err(DispatchError::ClarificationRequested {
                question: rest.trim().to_string(),
            });
        }

        if accumulated_text.trim().is_empty() {
            // Empty response is degenerate but not a transport error.
            // Surface it with a clear message; the orchestrator treats
            // this as `AuditDecision::Drop` upstream.
            warn!(
                target: "codrex::orchestrator::dispatch",
                run_id = %ctx.run_id,
                "MiniMax returned empty response body"
            );
        }

        Ok(DispatchOutcome {
            response_text: accumulated_text,
            latency_ms: started.elapsed().as_millis() as u64,
            total_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DelegationContext;
    use crate::DelegationSpec;
    use pretty_assertions::assert_eq;
    use wiremock::MockServer;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    fn spec_with_intent(intent: &str) -> DelegationSpec {
        DelegationSpec::new_bare(intent).unwrap()
    }

    fn ctx(spec: &DelegationSpec) -> DelegationContext {
        DelegationContext::for_top_level(spec)
    }

    #[test]
    fn build_request_emits_one_system_and_one_user_message() {
        let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
        let spec = spec_with_intent("explain validate_email in python");
        let req = sink.build_request(&spec, None);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
        assert_eq!(req.messages[1].content, "explain validate_email in python");
        assert_eq!(req.model, "MiniMax-M2.7");
        assert!(req.tools.is_empty());
    }

    #[test]
    fn system_message_includes_clarify_convention() {
        let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
        let spec = spec_with_intent("do x");
        let req = sink.build_request(&spec, None);
        assert!(req.messages[0].content.contains("CLARIFY:"));
    }

    #[test]
    fn system_message_lists_forbidden_patterns_when_present() {
        let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
        let mut spec = spec_with_intent("do x");
        spec.set_forbidden_patterns(["unsafe", r"std::mem::transmute"])
            .unwrap();
        let req = sink.build_request(&spec, None);
        assert!(req.messages[0].content.contains("AVOID"));
        assert!(req.messages[0].content.contains("unsafe"));
        assert!(req.messages[0].content.contains("std::mem::transmute"));
    }

    #[test]
    fn system_message_includes_retry_feedback_when_present() {
        use crate::decision::CriterionKind;
        use crate::decision::FailedCriterion;
        use crate::decision::FailureDetails;
        use crate::decision::RetryFeedback;

        let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
        let spec = spec_with_intent("do x");
        let feedback = RetryFeedback {
            failed_criteria: vec![FailedCriterion {
                name: "output_matches[0]".to_string(),
                kind: CriterionKind::OutputMatches,
                details: FailureDetails::OutputMatches {
                    regex: r"^DONE$".to_string(),
                    output_excerpt: "not done".to_string(),
                },
            }],
        };
        let req = sink.build_request(&spec, Some(&feedback));
        assert!(
            req.messages[0]
                .content
                .contains("Previous attempt failed acceptance criteria")
        );
        assert!(req.messages[0].content.contains("output_matches[0]"));
        assert!(req.messages[0].content.contains("regex: ^DONE$"));
    }

    fn nonstreaming_chat_response_body(content: &str) -> String {
        // Streaming endpoint normally yields SSE chunks. For tests we
        // emit a single complete chunk + [DONE] sentinel that the
        // bridge accumulates into one OutputTextDelta.
        format!(
            "data: {{\"id\":\"resp-test\",\"object\":\"chat.completion.chunk\",\
             \"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{}\"}}}}]}}\n\n\
             data: {{\"id\":\"resp-test\",\"choices\":[{{\"index\":0,\"finish_reason\":\"stop\",\
             \"delta\":{{}}}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":3,\
             \"total_tokens\":8}}}}\n\n\
             data: [DONE]\n\n",
            content.replace('"', "\\\"")
        )
    }

    /// End-to-end test against a `wiremock` MockServer pretending to be
    /// MiniMax. Verifies that:
    ///   * The dispatcher hits `/v1/chat/completions`.
    ///   * Auth from MINIMAX_API_KEY env var is used.
    ///   * The accumulated response text is returned in
    ///     `DispatchOutcome.response_text`.
    ///   * `total_tokens` is captured from the usage block.
    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn dispatch_returns_accumulated_response_text() {
        let server = MockServer::start().await;
        let body = nonstreaming_chat_response_body("Hello, world.");
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;

        // Force the resolve_base_url() to return the mock server URL by
        // setting the env override the codex-minimax crate honors.
        // SAFETY: tests run on a single thread (current_thread runtime
        // not configured here) so cross-test env races are tolerable.
        // Real fix lives in codex-minimax; out of scope for this commit.
        unsafe { std::env::set_var("MINIMAX_BASE_URL", format!("{}/v1", server.uri())) };
        unsafe { std::env::set_var("MINIMAX_API_KEY", "sk-test-key") };

        let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
        let spec = spec_with_intent("ping");
        let outcome = sink
            .dispatch(&spec, &ctx(&spec))
            .await
            .expect("dispatch ok");
        assert_eq!(outcome.response_text, "Hello, world.");
        assert_eq!(outcome.total_tokens, Some(8));
        assert!(outcome.latency_ms < 30_000);

        unsafe {
            std::env::remove_var("MINIMAX_BASE_URL");
            std::env::remove_var("MINIMAX_API_KEY");
        }
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn dispatch_surfaces_clarify_prefix_as_clarification_error() {
        let server = MockServer::start().await;
        let body = nonstreaming_chat_response_body(
            "CLARIFY: which language should the function be written in?",
        );
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("MINIMAX_BASE_URL", format!("{}/v1", server.uri()));
            std::env::set_var("MINIMAX_API_KEY", "sk-test-key");
        }

        let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
        let spec = spec_with_intent("implement a function");
        let err = sink
            .dispatch(&spec, &ctx(&spec))
            .await
            .expect_err("clarification expected");
        match err {
            DispatchError::ClarificationRequested { question } => {
                assert!(question.contains("which language"));
            }
            other => panic!("expected ClarificationRequested, got {other:?}"),
        }

        unsafe {
            std::env::remove_var("MINIMAX_BASE_URL");
            std::env::remove_var("MINIMAX_API_KEY");
        }
    }

    #[tokio::test]
    #[serial_test::serial(env_minimax)]
    async fn dispatch_returns_transport_error_when_no_credentials() {
        // Ensure no env vars override and no auth.json is read.
        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
            std::env::remove_var("MINIMAX_CODING_PLAN_KEY");
            // Point CODREX_HOME at a directory that exists but has no
            // auth.json — the file fallback returns None cleanly.
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CODREX_HOME", tmp.path());
            let sink = MinimaxDispatchSink::new("MiniMax-M2.7");
            let spec = spec_with_intent("hi");
            let err = sink
                .dispatch(&spec, &ctx(&spec))
                .await
                .expect_err("expected transport error");
            assert!(
                matches!(err, DispatchError::Transport(msg) if msg.contains("codrex login minimax"))
            );
            std::env::remove_var("CODREX_HOME");
            drop(tmp);
        }
    }
}
