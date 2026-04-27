//! Adapter that translates Codrex's internal Responses-shaped state into
//! MiniMax chat-completions traffic and back.
//!
//! # Phase 2 LITE support matrix
//!
//! Codrex Phase 2 ships a deliberately narrow translation that covers the
//! "invoke MiniMax without orchestration" path. Variants outside the LITE
//! subset are NOT dropped — they are stringified into a `tool` message so
//! the model can still see the context, just in a degraded form. Each
//! lossy translation emits a structured `tracing::warn!` (gated by
//! `CODREX_ADAPTER_WARN_LOSSY=1`) so Phase 3 has telemetry to refine.
//!
//! ```text
//! ResponseItem variant     | Phase 2 LITE behavior          | Phase 3 target
//! -------------------------|--------------------------------|-------------------------------
//! Message (text)           | Native role/content mapping    | Native
//! Message (multimodal img) | Stringified (lossy, warn)      | Native if M2.7 vision
//! FunctionCall             | Native tool_calls              | Native
//! FunctionCallOutput       | Native role:tool + tool_call_id | Native
//! Reasoning                | Stringified (lossy, warn)      | Send as `reasoning_details`
//!                          |                                | on the prior assistant turn
//!                          |                                | (requires reasoning_split=true
//!                          |                                | and the M2.x reasoning shape)
//! LocalShellCall           | Stringified (lossy, warn)      | Native or drop w/ feature flag
//! ToolSearchCall           | Stringified (lossy, warn)      | TBD on real usage data
//! CustomToolCall           | Stringified (lossy, warn)      | TBD on real usage data
//! CustomToolCallOutput     | Stringified (lossy, warn)      | TBD on real usage data
//! ToolSearchOutput         | Stringified (lossy, warn)      | TBD on real usage data
//!
//! ToolSpec variant         | Phase 2 LITE behavior          | Phase 3 target
//! -------------------------|--------------------------------|-------------------------------
//! Function                 | Native (FunctionDefinition)    | Native
//! Namespace                | Skipped (lossy, warn)          | Flatten to function tools
//! Freeform                 | Skipped (lossy, warn)          | Native or feature-flag drop
//! ToolSearch               | Skipped (lossy, warn)          | TBD
//! LocalShell               | Skipped (lossy, warn)          | Native or feature-flag drop
//! ImageGeneration          | Skipped (lossy, warn)          | TBD
//! WebSearch                | Skipped (lossy, warn)          | TBD
//!
//! Prompt-level field       | Phase 2 LITE behavior          | Phase 3 target
//! -------------------------|--------------------------------|-------------------------------
//! base_instructions        | Native system message          | Native
//! personality              | Skipped (lossy, warn)          | Concatenate to system message
//! output_schema            | Skipped (lossy, warn)          | TBD (likely tool_choice trick)
//! parallel_tool_calls      | Ignored                        | TBD per-provider support
//! ```
//!
//! Set `CODREX_ADAPTER_WARN_LOSSY=1` to enable the structured warns and
//! collect telemetry on which variants actually appear in real workloads.
//! That data should drive the Phase 3 priorities, in order of frequency.

use std::sync::Arc;
use std::time::Instant;

use codex_api::ResponseEvent;
use codex_minimax::AuthPreference;
use codex_minimax::MinimaxClient;
use codex_minimax::ResolvedAuth;
use codex_minimax::ResponseEventBridge;
use codex_minimax::resolve_auth_from_env;
use codex_minimax::resolve_base_url;
use codex_minimax::types::ChatCompletionRequest;
use codex_minimax::types::ChatMessage;
use codex_minimax::types::FunctionDefinition;
use codex_minimax::types::Tool;
use codex_minimax::types::ToolCall;
use codex_minimax::types::ToolCallFunction;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolSpec;
use futures::StreamExt;
use tokio::sync::mpsc;
use tracing::info;
use tracing::warn;
use uuid::Uuid;

use crate::client_common::Prompt;
use crate::client_common::ResponseStream;

/// Env-var gate for the lossy-translation warning channel. When unset (the
/// default), the adapter stays silent so production stderr doesn't get
/// flooded; turn it on during Phase 3 debugging or when investigating a
/// surprising model response.
const LOSSY_WARN_ENV: &str = "CODREX_ADAPTER_WARN_LOSSY";

fn lossy_warns_enabled() -> bool {
    std::env::var(LOSSY_WARN_ENV)
        .ok()
        .is_some_and(|v| !v.trim().is_empty() && v != "0")
}

/// Translate a Codex `Prompt` into a MiniMax `ChatCompletionRequest`.
///
/// This is a pure function — no I/O, no env vars beyond the lossy-warn
/// gate. Tests can call it directly to assert the wire shape.
pub fn translate_prompt(prompt: &Prompt, model: impl Into<String>) -> ChatCompletionRequest {
    let mut messages: Vec<ChatMessage> = Vec::new();

    // Base instructions translate to a leading system message.
    if !prompt.base_instructions.text.is_empty() {
        messages.push(ChatMessage::system(prompt.base_instructions.text.clone()));
    }

    // Conversation history.
    let formatted_input = prompt.get_formatted_input();
    for item in formatted_input {
        messages.extend(translate_response_item(item));
    }

    // Tools — collect Function variants natively, skip everything else with
    // a structured warn.
    let mut tools: Vec<Tool> = Vec::new();
    for spec in prompt.tools_for_translation() {
        match translate_tool(&spec) {
            Some(tool) => tools.push(tool),
            None => warn_lossy_tool(&spec),
        }
    }

    let mut request = ChatCompletionRequest::new(model, messages);
    request.tools = tools;

    // Phase 3 TODO: thread `personality`, `output_schema*` through.
    if prompt.personality.is_some() {
        warn_lossy_field("personality", "skipped in Phase 2 LITE");
    }
    if prompt.output_schema.is_some() {
        warn_lossy_field("output_schema", "skipped in Phase 2 LITE");
    }

    request
}

fn translate_response_item(item: ResponseItem) -> Vec<ChatMessage> {
    match item {
        ResponseItem::Message { role, content, .. } => {
            let (text, had_image) = flatten_content(&content);
            if had_image {
                warn_lossy_item(
                    "Message",
                    "image content stringified — multimodal lands in Phase 3",
                );
            }
            vec![ChatMessage {
                role,
                content: text,
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
            }]
        }
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => {
            // MiniMax accepts assistant messages whose only payload is
            // tool_calls; content is the empty string in that case.
            vec![ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: call_id,
                kind: "function".into(),
                function: ToolCallFunction { name, arguments },
                index: None,
            }])]
        }
        ResponseItem::FunctionCallOutput { call_id, output } => {
            let body = output
                .body
                .to_text()
                .unwrap_or_default();
            vec![ChatMessage::tool_result(call_id, body)]
        }
        ResponseItem::Reasoning { .. } => {
            warn_lossy_item(
                "Reasoning",
                "stringified as tool message — Phase 3 will route via reasoning_details",
            );
            vec![stringify_as_tool_message("reasoning", &item)]
        }
        ResponseItem::LocalShellCall { .. } => {
            warn_lossy_item(
                "LocalShellCall",
                "stringified — Phase 3 will decide native or drop",
            );
            vec![stringify_as_tool_message("local_shell_call", &item)]
        }
        ResponseItem::ToolSearchCall { .. } => {
            warn_lossy_item("ToolSearchCall", "stringified");
            vec![stringify_as_tool_message("tool_search_call", &item)]
        }
        ResponseItem::CustomToolCall { .. } => {
            warn_lossy_item("CustomToolCall", "stringified");
            vec![stringify_as_tool_message("custom_tool_call", &item)]
        }
        ResponseItem::CustomToolCallOutput { .. } => {
            warn_lossy_item("CustomToolCallOutput", "stringified");
            vec![stringify_as_tool_message("custom_tool_call_output", &item)]
        }
        ResponseItem::ToolSearchOutput { .. } => {
            warn_lossy_item("ToolSearchOutput", "stringified");
            vec![stringify_as_tool_message("tool_search_output", &item)]
        }
        // Catch-all for variants Phase 2 LITE doesn't model individually
        // (WebSearchCall, ImageGenerationCall, GhostSnapshot, Compaction,
        // Other). Same degraded path: stringify + warn so Phase 3 sees
        // them in telemetry.
        other => {
            warn_lossy_item(
                "ResponseItem(other)",
                "stringified — variant not modelled in Phase 2 LITE",
            );
            vec![stringify_as_tool_message("other", &other)]
        }
    }
}

/// Render a `ResponseItem` we don't natively support as a JSON-stringified
/// `tool` role message. Visible, debuggable, and stable. Callers drive a
/// matching warn before invoking this.
fn stringify_as_tool_message(label: &str, item: &ResponseItem) -> ChatMessage {
    let body = serde_json::to_string(item)
        .unwrap_or_else(|err| format!("<failed to serialize {label}: {err}>"));
    ChatMessage::tool_result(format!("codrex-stringified-{label}"), body)
}

fn flatten_content(content: &[ContentItem]) -> (String, bool) {
    let mut text = String::new();
    let mut had_image = false;
    for c in content {
        match c {
            ContentItem::InputText { text: t } | ContentItem::OutputText { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentItem::InputImage { image_url, .. } => {
                had_image = true;
                if !text.is_empty() {
                    text.push('\n');
                }
                // Phase 2: keep the URL visible to the model so it at least
                // knows an image was meant to be here. Phase 3 will encode
                // properly per MiniMax's vision schema.
                text.push_str(&format!("[image:{image_url}]"));
            }
        }
    }
    (text, had_image)
}

fn translate_tool(spec: &ToolSpec) -> Option<Tool> {
    match spec {
        ToolSpec::Function(api_tool) => {
            let parameters = serde_json::to_value(&api_tool.parameters).unwrap_or(serde_json::json!({}));
            Some(Tool::function(FunctionDefinition {
                name: api_tool.name.clone(),
                description: Some(api_tool.description.clone()),
                parameters,
            }))
        }
        ToolSpec::Namespace(_)
        | ToolSpec::Freeform(_)
        | ToolSpec::ToolSearch { .. }
        | ToolSpec::LocalShell {}
        | ToolSpec::ImageGeneration { .. }
        | ToolSpec::WebSearch { .. } => None,
    }
}

fn warn_lossy_item(item_type: &'static str, action: &'static str) {
    if lossy_warns_enabled() {
        warn!(
            adapter = "minimax",
            item_type = item_type,
            action = action,
            "lossy translation, refine in Phase 3"
        );
    }
}

fn warn_lossy_field(field: &'static str, action: &'static str) {
    if lossy_warns_enabled() {
        warn!(
            adapter = "minimax",
            field = field,
            action = action,
            "lossy translation, refine in Phase 3"
        );
    }
}

fn warn_lossy_tool(spec: &ToolSpec) {
    if lossy_warns_enabled() {
        warn!(
            adapter = "minimax",
            tool_kind = spec.name(),
            action = "skipped",
            "lossy translation, refine in Phase 3"
        );
    }
}

/// Resolve credentials for a configured MiniMax provider. Honors the
/// provider's `env_key` (when set) before falling back to the standard
/// `MINIMAX_API_KEY` / `MINIMAX_CODING_PLAN_KEY` resolution.
fn resolve_credentials(provider: &ModelProviderInfo) -> Result<ResolvedAuth, CodexErr> {
    if let Some(env_key) = provider.env_key.as_deref()
        && let Ok(value) = std::env::var(env_key)
        && !value.trim().is_empty()
    {
        return Ok(ResolvedAuth {
            bearer_token: value,
            // We can't return a `&'static str` for an arbitrary user key,
            // but the diagnostic field accepts a fixed pool of names. Map
            // the standard cases and fall back to the generic placeholder.
            env_var: match env_key {
                "MINIMAX_API_KEY" => "MINIMAX_API_KEY",
                "MINIMAX_CODING_PLAN_KEY" => "MINIMAX_CODING_PLAN_KEY",
                _ => "MINIMAX_API_KEY",
            },
        });
    }
    resolve_auth_from_env(AuthPreference::default()).map_err(|err| {
        CodexErr::UnsupportedOperation(format!("MiniMax credentials unavailable: {err}"))
    })
}

fn resolve_provider_base_url(provider: &ModelProviderInfo) -> String {
    if let Some(url) = provider.base_url.as_ref()
        && !url.trim().is_empty()
    {
        return url.trim_end_matches('/').to_string();
    }
    resolve_base_url()
}

/// Stream a chat completion against the configured MiniMax provider and
/// return a `ResponseStream` that the rest of Codex consumes verbatim.
pub async fn stream_chat_completions(
    provider: &ModelProviderInfo,
    prompt: &Prompt,
    model: &str,
    http_client: reqwest::Client,
) -> CodexResult<ResponseStream> {
    let auth = resolve_credentials(provider)?;
    let base_url = resolve_provider_base_url(provider);
    let client = MinimaxClient::new(http_client, base_url, auth);

    let request = translate_prompt(prompt, model.to_string());
    let started_at = Instant::now();
    let run_id = Uuid::new_v4().to_string();
    let model_for_log = model.to_string();
    let upstream = client
        .chat_completion_stream(&request)
        .await
        .map_err(map_minimax_err)?;

    let (tx, rx) = mpsc::channel::<CodexResult<ResponseEvent>>(64);
    let bridge_run_id = run_id.clone();
    let bridge_model = model_for_log.clone();
    let bridge = Arc::new(tokio::sync::Mutex::new(
        ResponseEventBridge::with_telemetry(bridge_run_id, bridge_model, started_at),
    ));

    let bridge_handle = bridge.clone();
    let log_run_id = run_id.clone();
    let log_model = model_for_log.clone();
    tokio::spawn(async move {
        let mut upstream = upstream;
        while let Some(item) = upstream.next().await {
            match item {
                Ok(chunk) => {
                    let events = {
                        let mut guard = bridge_handle.lock().await;
                        guard.ingest(chunk)
                    };
                    for ev in events {
                        emit_cost_log_if_completed(
                            &ev,
                            &log_model,
                            &log_run_id,
                            started_at,
                        );
                        if tx.send(Ok(ev)).await.is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(map_minimax_err(err))).await;
                    return;
                }
            }
        }
        // Drain finalize events. Replace the inner bridge with a fresh
        // (telemetry-less) instance so any post-finalize ingest is a no-op
        // — finalize is a one-shot. Adapter-side cost logging also
        // inspects these events, since `Completed { token_usage }` is
        // emitted by finalize, not by per-chunk ingest.
        let final_events = {
            let mut guard = bridge_handle.lock().await;
            std::mem::replace(&mut *guard, ResponseEventBridge::new()).finalize()
        };
        for ev in final_events {
            emit_cost_log_if_completed(&ev, &log_model, &log_run_id, started_at);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
    });

    Ok(ResponseStream { rx_event: rx })
}

fn map_minimax_err(err: codex_minimax::MinimaxError) -> CodexErr {
    CodexErr::UnsupportedOperation(format!("minimax adapter: {err}"))
}

/// Adapter-side cost log emitted once per turn, when we observe the
/// terminal `Completed` event carrying a `token_usage` block. Shares the
/// generated `run_id` with the bridge-side log emitted from
/// `ResponseEventBridge::finalize`, so log aggregators can correlate.
fn emit_cost_log_if_completed(
    ev: &ResponseEvent,
    model: &str,
    run_id: &str,
    started_at: Instant,
) {
    if let ResponseEvent::Completed {
        token_usage: Some(usage),
        ..
    } = ev
    {
        let latency_ms = started_at.elapsed().as_millis() as u64;
        info!(
            stage = "adapter.completed",
            provider = "minimax",
            model = %model,
            endpoint = "chat_completions",
            run_id = %run_id,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cached_tokens = usage.cached_input_tokens,
            reasoning_tokens = usage.reasoning_output_tokens,
            total_tokens = usage.total_tokens,
            latency_ms = latency_ms,
            "codrex.cost"
        );
    }
}

/// Internal helper so the adapter can iterate over the prompt's tools
/// without exposing the field publicly outside this crate.
trait PromptToolsExt {
    fn tools_for_translation(&self) -> Vec<ToolSpec>;
}

impl PromptToolsExt for Prompt {
    fn tools_for_translation(&self) -> Vec<ToolSpec> {
        self.tools.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_tools::JsonSchema;
    use codex_tools::ResponsesApiTool;
    use pretty_assertions::assert_eq;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![ContentItem::InputText { text: text.into() }],
            phase: None,
        }
    }

    fn function_tool(name: &str) -> ResponsesApiTool {
        ResponsesApiTool {
            name: name.into(),
            description: format!("test tool {name}"),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::default(),
            output_schema: None,
        }
    }

    #[test]
    fn translate_simple_user_prompt() {
        let mut prompt = Prompt::default();
        prompt.input.push(user_message("Hello"));
        let req = translate_prompt(&prompt, "MiniMax-M2.7");
        // System message from default base instructions + user message.
        assert!(req.messages.len() >= 2);
        let user = req.messages.last().unwrap();
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "Hello");
        assert!(req.reasoning_split);
        assert!(!req.stream);
        assert_eq!(req.model, "MiniMax-M2.7");
    }

    #[test]
    fn translate_function_call_history_into_assistant_with_tool_calls() {
        let mut prompt = Prompt::default();
        prompt.input.push(user_message("weather?"));
        prompt.input.push(ResponseItem::FunctionCall {
            id: None,
            name: "get_weather".into(),
            namespace: None,
            arguments: "{\"city\":\"SF\"}".into(),
            call_id: "call_1".into(),
        });
        prompt.input.push(ResponseItem::FunctionCallOutput {
            call_id: "call_1".into(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text("17C".into()),
                success: Some(true),
            },
        });
        let req = translate_prompt(&prompt, "MiniMax-M2.7");

        // Find assistant tool-call message and the tool-result message.
        let assistant_call = req
            .messages
            .iter()
            .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
            .expect("assistant tool-call message present");
        assert_eq!(assistant_call.tool_calls[0].id, "call_1");
        assert_eq!(assistant_call.tool_calls[0].function.name, "get_weather");
        assert_eq!(
            assistant_call.tool_calls[0].function.arguments,
            "{\"city\":\"SF\"}"
        );

        let tool_result = req
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("tool-result message present");
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_result.content, "17C");
    }

    #[test]
    fn function_tool_translates_natively() {
        let mut prompt = Prompt::default();
        prompt.tools.push(ToolSpec::Function(function_tool("get_weather")));
        prompt.input.push(user_message("hi"));
        let req = translate_prompt(&prompt, "MiniMax-M2.7");
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].function.name, "get_weather");
    }

    #[test]
    fn unsupported_response_items_stringify_into_tool_messages() {
        let mut prompt = Prompt::default();
        prompt.input.push(user_message("hello"));
        prompt.input.push(ResponseItem::Reasoning {
            id: "reason-1".into(),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "I should think.".into(),
            }],
            content: None,
            encrypted_content: None,
        });
        prompt.input.push(ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: "custom-1".into(),
            name: "weird-tool".into(),
            input: "{\"x\":1}".into(),
        });
        let req = translate_prompt(&prompt, "MiniMax-M2.7");

        // Both lossy items appear as `role: tool` messages with stable
        // synthetic call ids that label the original variant.
        let synthetic_tools: Vec<&ChatMessage> = req
            .messages
            .iter()
            .filter(|m| {
                m.role == "tool"
                    && m.tool_call_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("codrex-stringified-"))
            })
            .collect();
        assert_eq!(synthetic_tools.len(), 2);
        let labels: Vec<&str> = synthetic_tools
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        assert!(labels.iter().any(|l| l.contains("reasoning")));
        assert!(labels.iter().any(|l| l.contains("custom_tool_call")));

        // Each body must be valid JSON so the model can at least parse it.
        for m in &synthetic_tools {
            let parsed: serde_json::Value =
                serde_json::from_str(&m.content).expect("stringified body is valid JSON");
            assert!(parsed.is_object());
        }
    }

    #[test]
    fn unsupported_tool_specs_are_dropped() {
        let mut prompt = Prompt::default();
        prompt.tools.push(ToolSpec::LocalShell {});
        prompt.tools.push(ToolSpec::Function(function_tool("ok")));
        prompt.input.push(user_message("hi"));
        let req = translate_prompt(&prompt, "MiniMax-M2.7");
        // LocalShell got dropped; only Function survived.
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].function.name, "ok");
    }

    /// End-to-end: build a Prompt with a single user message, run the
    /// dispatch against a wiremock server that streams a real-shape
    /// MiniMax SSE body, drain the resulting ResponseStream, and verify
    /// the translated `ResponseEvent` sequence.
    ///
    /// Also asserts that the structured cost-log fires from the adapter
    /// side (commit 7) once a Completed event with usage is delivered.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn e2e_prompt_to_response_stream_via_mock_minimax() {
        use codex_api::ResponseEvent;
        use codex_model_provider_info::ModelProviderInfo;
        use codex_model_provider_info::WireApi;
        use futures::StreamExt;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        let body = "data: {\"id\":\"resp-e2e\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
data: {\"id\":\"resp-e2e\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\", world\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2,\"total_tokens\":6}}\n\n\
data: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .expect(1)
            .mount(&server)
            .await;

        // Provider info pointing at the mock server.
        let provider = ModelProviderInfo {
            name: "MiniMax".into(),
            base_url: Some(server.uri()),
            // Empty env_key forces the resolver to fall back to the
            // shared MINIMAX_API_KEY / MINIMAX_CODING_PLAN_KEY pool.
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            auth: None,
            aws: None,
            wire_api: WireApi::ChatCompletions,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: None,
            stream_max_retries: None,
            stream_idle_timeout_ms: None,
            websocket_connect_timeout_ms: None,
            requires_openai_auth: false,
            supports_websockets: false,
        };

        // Inject auth via process env. `--test-threads=1` is the cargo
        // default for unit-test binaries, so the mutation below is safe
        // for the duration of this test.
        // SAFETY: serial test execution + scope-limited env mutation.
        unsafe {
            std::env::set_var("MINIMAX_API_KEY", "e2e-token");
        }

        let mut prompt = Prompt::default();
        prompt.input.push(user_message("Hi"));

        let stream = stream_chat_completions(
            &provider,
            &prompt,
            "MiniMax-M2.7",
            reqwest::Client::new(),
        )
        .await
        .expect("stream opens");

        let mut text = String::new();
        let mut completed: Option<i64> = None;
        let mut stream = stream;
        while let Some(item) = stream.next().await {
            match item.expect("event ok") {
                ResponseEvent::OutputTextDelta(delta) => text.push_str(&delta),
                ResponseEvent::Completed { token_usage, .. } => {
                    if let Some(usage) = token_usage {
                        completed = Some(usage.total_tokens);
                    }
                }
                _ => {}
            }
        }

        unsafe {
            std::env::remove_var("MINIMAX_API_KEY");
        }

        assert_eq!(text, "Hello, world");
        assert_eq!(completed, Some(6));

        // Cost log assertions: adapter-side fires when a Completed with
        // usage is delivered, bridge-side fires on finalize. Both share
        // the same generated run_id so log aggregators can correlate.
        assert!(logs_contain("stage=\"adapter.completed\""));
        assert!(logs_contain("stage=\"bridge.finalize\""));
        assert!(logs_contain("provider=\"minimax\""));
        assert!(logs_contain("model=MiniMax-M2.7"));
        assert!(logs_contain("input_tokens=4"));
        assert!(logs_contain("output_tokens=2"));
        assert!(logs_contain("total_tokens=6"));
        assert!(logs_contain("codrex.cost"));
    }

    #[test]
    fn multimodal_message_falls_back_to_image_marker() {
        let mut prompt = Prompt::default();
        prompt.input.push(ResponseItem::Message {
            id: None,
            role: "user".into(),
            content: vec![
                ContentItem::InputText {
                    text: "describe".into(),
                },
                ContentItem::InputImage {
                    image_url: "https://example.com/cat.png".into(),
                    detail: None,
                },
            ],
            phase: None,
        });
        let req = translate_prompt(&prompt, "MiniMax-M2.7");
        let user = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .expect("user message present");
        assert!(user.content.contains("describe"));
        assert!(user.content.contains("[image:https://example.com/cat.png]"));
    }
}
