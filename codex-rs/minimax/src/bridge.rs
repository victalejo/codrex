//! Translate streamed MiniMax chunks into Codrex's internal `ResponseEvent`
//! enum.
//!
//! The bridge is stateful because a streamed assistant turn is split across
//! many chunks: text deltas, partial tool-call arguments, optional reasoning
//! text, and a final usage block. Callers feed each chunk via [`ingest`] and
//! call [`finalize`] when the underlying stream ends to drain pending
//! tool-call accumulators and emit `Completed`.
//!
//! Mapping at a glance:
//! - `delta.content`           → `ResponseEvent::OutputTextDelta`
//!   (with `<think>...</think>` blocks defensively rerouted via
//!   `think_parser` to `ReasoningContentDelta`)
//! - `delta.reasoning_content` → `ResponseEvent::ReasoningContentDelta`
//! - `delta.tool_calls[]`      → `ResponseEvent::ToolCallInputDelta` per
//!   incremental fragment, plus a final `ResponseEvent::OutputItemDone`
//!   carrying a complete `ResponseItem::FunctionCall` once the call closes
//! - end of stream             → `ResponseEvent::Completed { response_id,
//!                               token_usage, end_turn }`

use crate::streaming::ChatCompletionChunk;
use crate::think_parser::ParsedSegment;
use crate::think_parser::ThinkParser;
use crate::types::Usage;
use codex_api::ResponseEvent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use std::collections::BTreeMap;
use std::time::Instant;
use tracing::info;
use uuid::Uuid;

/// Stateful translator from MiniMax stream chunks to `ResponseEvent`s.
#[derive(Debug)]
pub struct ResponseEventBridge {
    response_id: Option<String>,
    pending_calls: BTreeMap<u32, PartialCall>,
    last_usage: Option<TokenUsage>,
    final_finish_reason: Option<String>,
    think_parser: ThinkParser,
    /// Counter that grows once per reasoning-channel emission so callers can
    /// track index ordering. MiniMax doesn't surface a multi-segment index
    /// like OpenAI does, so we approximate with a monotonic counter.
    reasoning_index: i64,
    /// Run id propagated from the adapter so cost-logging events emitted
    /// here correlate with adapter-side events for the same call.
    run_id: String,
    /// Model slug, recorded for cost-logging telemetry.
    model: String,
    /// When the bridge started consuming the stream — used to compute
    /// `latency_ms` at finalize.
    started_at: Instant,
    /// Whether we've already emitted `OutputItemAdded(Message)` for this
    /// stream's assistant turn. The Codex turn loop requires an active
    /// item before any text/reasoning delta — without it, those deltas
    /// trip a debug-mode panic and an error in release. MiniMax doesn't
    /// emit a discrete "item start" event, so we synthesize one lazily on
    /// the first delta and close it in `finalize`.
    message_started: bool,
    /// Synthetic id for the wrapping message item. Reused for the
    /// matching `OutputItemDone(Message)` so callers see a coherent
    /// open/close pair.
    message_item_id: String,
    /// Accumulated visible text to populate `OutputItemDone(Message)`
    /// content. Reasoning bytes are excluded — they live in their own
    /// channel and are surfaced via `ReasoningContentDelta` events.
    accumulated_text: String,
}

impl Default for ResponseEventBridge {
    fn default() -> Self {
        Self {
            response_id: None,
            pending_calls: BTreeMap::new(),
            last_usage: None,
            final_finish_reason: None,
            think_parser: ThinkParser::default(),
            reasoning_index: 0,
            run_id: String::new(),
            model: String::new(),
            started_at: Instant::now(),
            message_started: false,
            message_item_id: String::new(),
            accumulated_text: String::new(),
        }
    }
}

#[derive(Debug, Default)]
struct PartialCall {
    call_id: String,
    name: String,
    arguments: String,
}

impl ResponseEventBridge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a bridge that records `run_id` and `model` on its cost-logging
    /// events, and uses `started_at` as the latency baseline.
    pub fn with_telemetry(
        run_id: impl Into<String>,
        model: impl Into<String>,
        started_at: Instant,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            model: model.into(),
            started_at,
            ..Self::default()
        }
    }

    /// Lazily emit `OutputItemAdded(Message{role:"assistant"})` so the
    /// turn loop has an `active_item` set before any text or reasoning
    /// delta lands. Idempotent — subsequent calls are no-ops.
    fn ensure_message_started(&mut self, out: &mut Vec<ResponseEvent>) {
        if self.message_started {
            return;
        }
        self.message_item_id = self
            .response_id
            .clone()
            .map(|id| format!("msg-{id}"))
            .unwrap_or_else(|| format!("msg-{}", Uuid::new_v4()));
        out.push(ResponseEvent::OutputItemAdded(ResponseItem::Message {
            id: Some(self.message_item_id.clone()),
            role: "assistant".into(),
            content: Vec::new(),
            phase: None,
        }));
        self.message_started = true;
    }

    /// Feed one streaming chunk; returns the events to emit immediately.
    pub fn ingest(&mut self, chunk: ChatCompletionChunk) -> Vec<ResponseEvent> {
        if self.response_id.is_none() && !chunk.id.is_empty() {
            self.response_id = Some(chunk.id.clone());
        }
        if let Some(usage) = chunk.usage.as_ref() {
            self.last_usage = Some(map_usage(usage));
        }

        let mut out: Vec<ResponseEvent> = Vec::new();
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.final_finish_reason = Some(reason);
            }
            // Structured reasoning when reasoning_split is honored.
            if let Some(reasoning) = choice.delta.reasoning_content
                && !reasoning.is_empty()
            {
                self.ensure_message_started(&mut out);
                out.push(ResponseEvent::ReasoningContentDelta {
                    delta: reasoning,
                    content_index: self.reasoning_index,
                });
                self.reasoning_index += 1;
            }
            // Plain content — defensively split out any `<think>...</think>`.
            if let Some(content) = choice.delta.content
                && !content.is_empty()
            {
                for seg in self.think_parser.push(&content) {
                    match seg {
                        ParsedSegment::Text(text) if !text.is_empty() => {
                            self.ensure_message_started(&mut out);
                            self.accumulated_text.push_str(&text);
                            out.push(ResponseEvent::OutputTextDelta(text));
                        }
                        ParsedSegment::Reasoning(text) if !text.is_empty() => {
                            self.ensure_message_started(&mut out);
                            out.push(ResponseEvent::ReasoningContentDelta {
                                delta: text,
                                content_index: self.reasoning_index,
                            });
                            self.reasoning_index += 1;
                        }
                        _ => {}
                    }
                }
            }
            // Streamed tool-call argument fragments.
            for tc in choice.delta.tool_calls {
                let entry = self.pending_calls.entry(tc.index).or_default();
                if let Some(id) = tc.id {
                    entry.call_id = id;
                }
                if let Some(func) = tc.function {
                    if let Some(name) = func.name {
                        entry.name = name;
                    }
                    if let Some(args) = func.arguments
                        && !args.is_empty()
                    {
                        entry.arguments.push_str(&args);
                        out.push(ResponseEvent::ToolCallInputDelta {
                            item_id: format!("call-{}", tc.index),
                            call_id: if entry.call_id.is_empty() {
                                None
                            } else {
                                Some(entry.call_id.clone())
                            },
                            delta: args,
                        });
                    }
                }
            }
        }
        out
    }

    /// Drain any buffered state at end of stream.
    pub fn finalize(mut self) -> Vec<ResponseEvent> {
        let mut out: Vec<ResponseEvent> = Vec::new();

        // Flush any in-flight `<think>` content.
        for seg in self.think_parser.flush() {
            match seg {
                ParsedSegment::Text(text) if !text.is_empty() => {
                    self.ensure_message_started(&mut out);
                    self.accumulated_text.push_str(&text);
                    out.push(ResponseEvent::OutputTextDelta(text));
                }
                ParsedSegment::Reasoning(text) if !text.is_empty() => {
                    self.ensure_message_started(&mut out);
                    out.push(ResponseEvent::ReasoningContentDelta {
                        delta: text,
                        content_index: self.reasoning_index,
                    });
                    self.reasoning_index += 1;
                }
                _ => {}
            }
        }

        // Close the synthetic Message wrapper before emitting any tool
        // calls or the Completed event. Mirrors OpenAI's lifecycle:
        // OutputItemAdded(Message) → deltas → OutputItemDone(Message)
        // → OutputItemDone(FunctionCall)* → Completed.
        if self.message_started {
            let content = if self.accumulated_text.is_empty() {
                Vec::new()
            } else {
                vec![ContentItem::OutputText {
                    text: std::mem::take(&mut self.accumulated_text),
                }]
            };
            out.push(ResponseEvent::OutputItemDone(ResponseItem::Message {
                id: Some(self.message_item_id.clone()),
                role: "assistant".into(),
                content,
                phase: None,
            }));
        }

        // Emit a fully-realized FunctionCall item per accumulated call.
        for (_index, call) in std::mem::take(&mut self.pending_calls) {
            if call.call_id.is_empty() && call.name.is_empty() {
                continue;
            }
            out.push(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
                id: None,
                name: call.name,
                namespace: None,
                arguments: call.arguments,
                call_id: call.call_id,
            }));
        }

        // Translate finish_reason into the optional `end_turn` flag.
        let end_turn = self
            .final_finish_reason
            .as_deref()
            .map(|reason| match reason {
                "stop" | "length" => true,
                "tool_calls" => false,
                _ => false,
            });

        // Emit structured cost-logging telemetry exactly once per stream,
        // and only when MiniMax sent a usage block. This is one of the two
        // emission points wired in commit 7 (the other lives in the
        // adapter); both share the same run_id so logs collected during a
        // delegated turn correlate cleanly.
        if let Some(usage) = self.last_usage.as_ref()
            && !self.run_id.is_empty()
        {
            let latency_ms = self.started_at.elapsed().as_millis() as u64;
            info!(
                stage = "bridge.finalize",
                provider = "minimax",
                model = %self.model,
                endpoint = "chat_completions",
                run_id = %self.run_id,
                input_tokens = usage.input_tokens,
                output_tokens = usage.output_tokens,
                cached_tokens = usage.cached_input_tokens,
                reasoning_tokens = usage.reasoning_output_tokens,
                total_tokens = usage.total_tokens,
                latency_ms = latency_ms,
                "codrex.cost"
            );
        }

        out.push(ResponseEvent::Completed {
            response_id: self.response_id.unwrap_or_default(),
            token_usage: self.last_usage,
            end_turn,
        });
        out
    }
}

#[cfg(test)]
mod cost_log_tests {
    use super::*;
    use crate::streaming::ChatCompletionChunk;

    fn chunk(json: &str) -> ChatCompletionChunk {
        serde_json::from_str(json).expect("valid chunk")
    }

    /// `bridge.finalize` emits a structured `codrex.cost` log with run_id
    /// and the documented token fields when telemetry is configured.
    #[test]
    #[tracing_test::traced_test]
    fn finalize_emits_structured_cost_log_with_run_id() {
        let mut bridge =
            ResponseEventBridge::with_telemetry("test-run-id-123", "MiniMax-M2.7", Instant::now());
        bridge.ingest(chunk(
            r#"{
                "id":"resp-cost",
                "choices":[{"index":0,"finish_reason":"stop","delta":{"content":"ok"}}],
                "usage":{
                    "prompt_tokens":10,
                    "completion_tokens":5,
                    "total_tokens":15,
                    "prompt_tokens_details":{"cached_tokens":2},
                    "completion_tokens_details":{"reasoning_tokens":1}
                }
            }"#,
        ));
        let _ = bridge.finalize();

        // Field captures use the default tracing formatter: string literal
        // args render with quotes (`stage="bridge.finalize"`), `%` Display
        // args render without (`model=MiniMax-M2.7`).
        assert!(logs_contain("stage=\"bridge.finalize\""));
        assert!(logs_contain("provider=\"minimax\""));
        assert!(logs_contain("model=MiniMax-M2.7"));
        assert!(logs_contain("endpoint=\"chat_completions\""));
        assert!(logs_contain("run_id=test-run-id-123"));
        assert!(logs_contain("input_tokens=10"));
        assert!(logs_contain("output_tokens=5"));
        assert!(logs_contain("cached_tokens=2"));
        assert!(logs_contain("reasoning_tokens=1"));
        assert!(logs_contain("total_tokens=15"));
        assert!(logs_contain("latency_ms="));
        assert!(logs_contain("codrex.cost"));
    }

    /// Without telemetry configured, the cost-log channel must stay
    /// silent so production stderr doesn't get flooded.
    #[test]
    #[tracing_test::traced_test]
    fn finalize_without_telemetry_emits_no_cost_log() {
        let mut bridge = ResponseEventBridge::new();
        bridge.ingest(chunk(
            r#"{
                "id":"resp-cost-2",
                "choices":[{"index":0,"finish_reason":"stop","delta":{"content":"ok"}}],
                "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
            }"#,
        ));
        let _ = bridge.finalize();
        assert!(!logs_contain("stage=\"bridge.finalize\""));
    }

    /// Without a usage block, no cost log should fire even with telemetry.
    #[test]
    #[tracing_test::traced_test]
    fn finalize_with_telemetry_but_no_usage_emits_nothing() {
        let mut bridge =
            ResponseEventBridge::with_telemetry("no-usage-run", "MiniMax-M2.7", Instant::now());
        bridge.ingest(chunk(
            r#"{"id":"x","choices":[{"index":0,"finish_reason":"stop","delta":{}}]}"#,
        ));
        let _ = bridge.finalize();
        assert!(!logs_contain("stage=\"bridge.finalize\""));
    }
}

/// Regression tests for the synthesized message-item lifecycle. Without
/// these events the Codex turn loop trips the `error_or_panic` guard at
/// `core/src/util.rs:97` ("OutputTextDelta/ReasoningRawContentDelta
/// without active item") on the first delta of a real MiniMax stream.
/// Bug surfaced live during Phase 2.5 validation.
#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::streaming::ChatCompletionChunk;

    fn chunk(json: &str) -> ChatCompletionChunk {
        serde_json::from_str(json).expect("valid chunk")
    }

    fn is_message_added(ev: &ResponseEvent) -> bool {
        matches!(
            ev,
            ResponseEvent::OutputItemAdded(ResponseItem::Message {
                role,
                ..
            }) if role == "assistant"
        )
    }
    fn is_message_done(ev: &ResponseEvent) -> bool {
        matches!(
            ev,
            ResponseEvent::OutputItemDone(ResponseItem::Message {
                role,
                ..
            }) if role == "assistant"
        )
    }

    #[test]
    fn first_text_delta_is_preceded_by_output_item_added_message() {
        let mut bridge = ResponseEventBridge::new();
        let events = bridge.ingest(chunk(
            r#"{"id":"resp-1","choices":[{"index":0,"delta":{"content":"Hello"}}]}"#,
        ));
        assert!(
            events.first().is_some_and(is_message_added),
            "expected OutputItemAdded(Message) before first text delta; got {:?}",
            events
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ResponseEvent::OutputTextDelta(t) if t == "Hello")),
            "text delta missing"
        );
    }

    #[test]
    fn first_reasoning_delta_is_preceded_by_output_item_added_message() {
        let mut bridge = ResponseEventBridge::new();
        let events = bridge.ingest(chunk(
            r#"{"id":"resp-1","choices":[{"index":0,"delta":{"reasoning_content":"thinking..."}}]}"#,
        ));
        assert!(
            events.first().is_some_and(is_message_added),
            "expected OutputItemAdded(Message) before first reasoning delta; got {:?}",
            events
        );
    }

    #[test]
    fn output_item_added_emitted_only_once_across_chunks() {
        let mut bridge = ResponseEventBridge::new();
        let mut all = bridge.ingest(chunk(
            r#"{"id":"resp-1","choices":[{"index":0,"delta":{"content":"Hel"}}]}"#,
        ));
        all.extend(bridge.ingest(chunk(
            r#"{"id":"resp-1","choices":[{"index":0,"delta":{"content":"lo"}}]}"#,
        )));
        all.extend(bridge.ingest(chunk(
            r#"{"id":"resp-1","choices":[{"index":0,"delta":{"reasoning_content":"think"}}]}"#,
        )));
        let count = all.iter().filter(|e| is_message_added(e)).count();
        assert_eq!(
            count, 1,
            "OutputItemAdded(Message) must be emitted exactly once per stream"
        );
    }

    #[test]
    fn finalize_closes_message_with_accumulated_text() {
        let mut bridge = ResponseEventBridge::new();
        let _ = bridge.ingest(chunk(
            r#"{"id":"resp-1","choices":[{"index":0,"delta":{"content":"OK"}}]}"#,
        ));
        let final_events = bridge.finalize();
        let done = final_events
            .iter()
            .find(|e| is_message_done(e))
            .expect("OutputItemDone(Message) emitted at finalize");
        if let ResponseEvent::OutputItemDone(ResponseItem::Message { content, id, .. }) = done {
            assert!(id.is_some(), "message item should carry a synthesized id");
            assert_eq!(content.len(), 1);
            assert!(matches!(
                &content[0],
                ContentItem::OutputText { text } if text == "OK"
            ));
        }
        // OutputItemDone(Message) must come BEFORE Completed.
        let done_idx = final_events.iter().position(is_message_done).unwrap();
        let completed_idx = final_events
            .iter()
            .position(|e| matches!(e, ResponseEvent::Completed { .. }))
            .unwrap();
        assert!(
            done_idx < completed_idx,
            "OutputItemDone(Message) must precede Completed"
        );
    }

    #[test]
    fn finalize_skips_message_close_when_no_deltas() {
        let bridge = ResponseEventBridge::new();
        let final_events = bridge.finalize();
        // Streams with zero content (just usage / finish_reason) should
        // not invent a synthetic Message item.
        assert!(
            !final_events.iter().any(is_message_done),
            "no Message lifecycle should be synthesized when nothing was emitted"
        );
        assert!(
            final_events
                .iter()
                .any(|e| matches!(e, ResponseEvent::Completed { .. })),
            "Completed should still fire"
        );
    }
}

fn map_usage(usage: &Usage) -> TokenUsage {
    let cached = usage
        .prompt_tokens_details
        .as_ref()
        .map(|d| d.cached_tokens)
        .unwrap_or(0);
    let reasoning = usage
        .completion_tokens_details
        .as_ref()
        .map(|d| d.reasoning_tokens)
        .unwrap_or(0);
    TokenUsage {
        input_tokens: usage.prompt_tokens as i64,
        cached_input_tokens: cached as i64,
        output_tokens: usage.completion_tokens as i64,
        reasoning_output_tokens: reasoning as i64,
        total_tokens: usage.total_tokens as i64,
    }
}
