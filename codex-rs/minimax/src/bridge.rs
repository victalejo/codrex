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
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use std::collections::BTreeMap;
use std::time::Instant;
use tracing::info;

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
                            out.push(ResponseEvent::OutputTextDelta(text));
                        }
                        ParsedSegment::Reasoning(text) if !text.is_empty() => {
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
                    out.push(ResponseEvent::OutputTextDelta(text));
                }
                ParsedSegment::Reasoning(text) if !text.is_empty() => {
                    out.push(ResponseEvent::ReasoningContentDelta {
                        delta: text,
                        content_index: self.reasoning_index,
                    });
                    self.reasoning_index += 1;
                }
                _ => {}
            }
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
        let end_turn = self.final_finish_reason.as_deref().map(|reason| match reason {
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
        let mut bridge = ResponseEventBridge::with_telemetry(
            "test-run-id-123",
            "MiniMax-M2.7",
            Instant::now(),
        );
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
        let mut bridge = ResponseEventBridge::with_telemetry(
            "no-usage-run",
            "MiniMax-M2.7",
            Instant::now(),
        );
        bridge.ingest(chunk(
            r#"{"id":"x","choices":[{"index":0,"finish_reason":"stop","delta":{}}]}"#,
        ));
        let _ = bridge.finalize();
        assert!(!logs_contain("stage=\"bridge.finalize\""));
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
