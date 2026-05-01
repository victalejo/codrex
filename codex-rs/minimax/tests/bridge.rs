//! Integration tests for the MiniMax → ResponseEvent bridge.
//!
//! These exercise `ResponseEventBridge::ingest` + `finalize` end-to-end
//! using synthetic chunks shaped exactly like real MiniMax SSE deltas.

use codex_api::ResponseEvent;
use codex_minimax::ResponseEventBridge;
use codex_minimax::streaming::ChatCompletionChunk;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

fn chunk_from_json(json: &str) -> ChatCompletionChunk {
    serde_json::from_str(json).unwrap_or_else(|err| panic!("bad chunk JSON: {err}: {json}"))
}

#[test]
fn maps_text_deltas_to_output_text_delta() {
    let mut bridge = ResponseEventBridge::new();
    let mut events = bridge.ingest(chunk_from_json(
        r#"{"id":"resp-1","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}"#,
    ));
    events.extend(bridge.ingest(chunk_from_json(
        r#"{"id":"resp-1","choices":[{"index":0,"finish_reason":"stop","delta":{"content":", world"}}]}"#,
    )));
    events.extend(bridge.finalize());

    // Pull out OutputTextDelta strings in order.
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|ev| match ev {
            ResponseEvent::OutputTextDelta(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["Hello", ", world"]);

    // The terminal Completed event captures the response_id.
    match events.last().expect("non-empty") {
        ResponseEvent::Completed {
            response_id,
            end_turn,
            ..
        } => {
            assert_eq!(response_id, "resp-1");
            assert_eq!(*end_turn, Some(true));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn maps_reasoning_content_to_reasoning_content_delta() {
    let mut bridge = ResponseEventBridge::new();
    let mut events = bridge.ingest(chunk_from_json(
        r#"{"id":"resp-2","choices":[{"index":0,"delta":{"reasoning_content":"Let me think."}}]}"#,
    ));
    events.extend(bridge.ingest(chunk_from_json(
        r#"{"id":"resp-2","choices":[{"index":0,"delta":{"content":"4"},"finish_reason":"stop"}]}"#,
    )));
    events.extend(bridge.finalize());

    let mut saw_reasoning = false;
    let mut saw_text = false;
    for ev in &events {
        match ev {
            ResponseEvent::ReasoningContentDelta { delta, .. } => {
                if delta == "Let me think." {
                    saw_reasoning = true;
                }
            }
            ResponseEvent::OutputTextDelta(s) if s == "4" => saw_text = true,
            _ => {}
        }
    }
    assert!(saw_reasoning, "reasoning content not surfaced");
    assert!(saw_text, "final text not surfaced");
}

#[test]
fn defensively_strips_think_tags_when_reasoning_split_not_honored() {
    let mut bridge = ResponseEventBridge::new();
    let mut events = bridge.ingest(chunk_from_json(
        r#"{"id":"resp-3","choices":[{"index":0,"delta":{"content":"<think>internal</think>visible"}}]}"#,
    ));
    events.extend(bridge.ingest(chunk_from_json(
        r#"{"id":"resp-3","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    )));
    events.extend(bridge.finalize());

    let mut text_concat = String::new();
    let mut reasoning_concat = String::new();
    for ev in &events {
        match ev {
            ResponseEvent::OutputTextDelta(s) => text_concat.push_str(s),
            ResponseEvent::ReasoningContentDelta { delta, .. } => {
                reasoning_concat.push_str(delta);
            }
            _ => {}
        }
    }
    assert_eq!(text_concat, "visible");
    assert_eq!(reasoning_concat, "internal");
}

#[test]
fn accumulates_tool_call_into_function_call_response_item() {
    let mut bridge = ResponseEventBridge::new();
    let chunks = [
        r#"{"id":"resp-4","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]}}]}"#,
        r#"{"id":"resp-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#,
        r#"{"id":"resp-4","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"SF\"}"}}]}}]}"#,
        r#"{"id":"resp-4","choices":[{"index":0,"finish_reason":"tool_calls","delta":{}}]}"#,
    ];
    let mut events: Vec<ResponseEvent> = Vec::new();
    for c in chunks {
        events.extend(bridge.ingest(chunk_from_json(c)));
    }
    events.extend(bridge.finalize());

    // We should have streamed two ToolCallInputDelta events (one per
    // arguments-bearing chunk; the empty-args chunk emits nothing).
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|ev| match ev {
            ResponseEvent::ToolCallInputDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["{\"city\":", "\"SF\"}"]);

    // And the finalized OutputItemDone carries the assembled FunctionCall.
    let final_item = events
        .iter()
        .find_map(|ev| match ev {
            ResponseEvent::OutputItemDone(item) => Some(item),
            _ => None,
        })
        .expect("OutputItemDone present");
    match final_item {
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => {
            assert_eq!(name, "get_weather");
            assert_eq!(arguments, "{\"city\":\"SF\"}");
            assert_eq!(call_id, "call_1");
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }

    // Tool call turns must NOT mark end_turn=true.
    match events.last().expect("non-empty") {
        ResponseEvent::Completed { end_turn, .. } => {
            assert_eq!(*end_turn, Some(false));
        }
        other => panic!("expected Completed last, got {other:?}"),
    }
}

#[test]
fn maps_usage_block_into_token_usage() {
    let mut bridge = ResponseEventBridge::new();
    bridge.ingest(chunk_from_json(
        r#"{"id":"resp-5","choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
    ));
    let chunk_with_usage = chunk_from_json(
        r#"{
            "id":"resp-5",
            "choices":[{"index":0,"finish_reason":"stop","delta":{}}],
            "usage":{
                "prompt_tokens":12,
                "completion_tokens":8,
                "total_tokens":20,
                "prompt_tokens_details":{"cached_tokens":4},
                "completion_tokens_details":{"reasoning_tokens":3}
            }
        }"#,
    );
    let mut events = bridge.ingest(chunk_with_usage);
    events.extend(bridge.finalize());

    let usage = match events.last().expect("non-empty") {
        ResponseEvent::Completed { token_usage, .. } => {
            token_usage.as_ref().expect("usage present").clone()
        }
        other => panic!("expected Completed, got {other:?}"),
    };
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.cached_input_tokens, 4);
    assert_eq!(usage.output_tokens, 8);
    assert_eq!(usage.reasoning_output_tokens, 3);
    assert_eq!(usage.total_tokens, 20);
}

#[test]
fn empty_stream_still_emits_completed() {
    let bridge = ResponseEventBridge::new();
    let events = bridge.finalize();
    assert_eq!(events.len(), 1);
    match &events[0] {
        ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
        } => {
            assert!(response_id.is_empty());
            assert!(token_usage.is_none());
            assert!(end_turn.is_none());
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

// Cost-logging tests live in `src/bridge.rs` as a `#[cfg(test)] mod`
// because `tracing_test` filters by the test binary's crate name; events
// from `codex_minimax::bridge` are only captured when the assertion runs
// from within the same crate.
