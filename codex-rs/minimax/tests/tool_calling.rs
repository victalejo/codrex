//! Integration tests for MiniMax tool calling.
//!
//! Covers both directions:
//! - Outbound: the `tools[]` and `tool_choice` fields serialize into the
//!   shape MiniMax (and OpenAI) expect.
//! - Inbound: tool-call responses (non-streaming) and tool-call streaming
//!   chunks deserialize into the typed `ToolCall` / `ToolCallChunk` shape.
//! - Round-trip: a `ChatMessage::tool_result` follow-up serializes with
//!   `role: "tool"` and the original `tool_call_id`.

use codex_minimax::MinimaxClient;
use codex_minimax::ResolvedAuth;
use codex_minimax::streaming::ChunkDelta;
use codex_minimax::types::ChatCompletionRequest;
use codex_minimax::types::ChatCompletionResponse;
use codex_minimax::types::ChatMessage;
use codex_minimax::types::FunctionDefinition;
use codex_minimax::types::Tool;
use codex_minimax::types::ToolChoice;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn make_client(server: &MockServer) -> MinimaxClient {
    let auth = ResolvedAuth {
        bearer_token: "test-token".to_string(),
        env_var: "MINIMAX_API_KEY",
    };
    MinimaxClient::new(reqwest::Client::new(), server.uri(), auth)
}

fn weather_tool() -> Tool {
    Tool::function(FunctionDefinition {
        name: "get_weather".to_string(),
        description: Some("Get the current weather for a city.".to_string()),
        parameters: json!({
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }),
    })
}

#[tokio::test]
async fn outbound_tools_serialize_in_openai_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x", "model": "MiniMax-M2.7", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ok"}}]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let mut request = ChatCompletionRequest::new(
        "MiniMax-M2.7",
        vec![ChatMessage::user("What is the weather in SF?")],
    );
    request.tools = vec![weather_tool()];
    request.tool_choice = Some(ToolChoice::auto());
    client.chat_completion(&request).await.expect("ok");

    let received: Vec<Request> = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("get_weather"));
    assert_eq!(
        body["tools"][0]["function"]["description"],
        json!("Get the current weather for a city.")
    );
    assert_eq!(
        body["tools"][0]["function"]["parameters"]["properties"]["city"]["type"],
        json!("string")
    );
    assert_eq!(body["tool_choice"], json!("auto"));
}

#[tokio::test]
async fn outbound_tool_choice_force_serializes_as_object() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x", "model": "MiniMax-M2.7", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "ok"}}]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let mut request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    request.tools = vec![weather_tool()];
    request.tool_choice = Some(ToolChoice::force("get_weather"));
    client.chat_completion(&request).await.expect("ok");

    let received: Vec<Request> = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&received[0].body).expect("json");
    assert_eq!(body["tool_choice"]["type"], json!("function"));
    assert_eq!(
        body["tool_choice"]["function"]["name"],
        json!("get_weather")
    );
}

#[tokio::test]
async fn inbound_tool_call_response_deserializes() {
    let server = MockServer::start().await;
    // Real-shape MiniMax tool-call response captured during validation.
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "063e307410a6e2d9840887d6e48d6d31",
            "model": "MiniMax-M2.7",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_function_0tldhb4zjgq7_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\": \"SF\"}"
                        },
                        "index": 0
                    }]
                }
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let mut request =
        ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("weather?")]);
    request.tools = vec![weather_tool()];
    request.tool_choice = Some(ToolChoice::auto());
    let response: ChatCompletionResponse = client.chat_completion(&request).await.expect("ok");

    let choice = &response.choices[0];
    assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
    let calls = &choice.message.tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "call_function_0tldhb4zjgq7_1");
    assert_eq!(calls[0].kind, "function");
    assert_eq!(calls[0].function.name, "get_weather");
    let args: Value = serde_json::from_str(&calls[0].function.arguments).expect("args parse");
    assert_eq!(args["city"], json!("SF"));
}

#[tokio::test]
async fn round_trip_tool_result_message_serializes_with_tool_role() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x", "model": "MiniMax-M2.7", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop",
                "message": {"role": "assistant", "content": "Done."}}]
        })))
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request = ChatCompletionRequest::new(
        "MiniMax-M2.7",
        vec![
            ChatMessage::user("weather in SF?"),
            ChatMessage::assistant_tool_calls(vec![codex_minimax::types::ToolCall {
                id: "call_42".to_string(),
                kind: "function".to_string(),
                function: codex_minimax::types::ToolCallFunction {
                    name: "get_weather".to_string(),
                    arguments: r#"{"city":"SF"}"#.to_string(),
                },
                index: Some(0),
            }]),
            ChatMessage::tool_result("call_42", r#"{"temperature_c": 17}"#),
        ],
    );
    client.chat_completion(&request).await.expect("ok");

    let received: Vec<Request> = server.received_requests().await.expect("recorded");
    let body: Value = serde_json::from_slice(&received[0].body).expect("json");
    let messages = body["messages"].as_array().expect("array");
    assert_eq!(messages.len(), 3);

    // Assistant message carries tool_calls and an empty content string.
    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(messages[1]["content"], json!(""));
    assert_eq!(messages[1]["tool_calls"][0]["id"], json!("call_42"));
    assert_eq!(
        messages[1]["tool_calls"][0]["function"]["name"],
        json!("get_weather")
    );

    // Tool result reply carries role=tool + tool_call_id matching the call.
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[2]["tool_call_id"], json!("call_42"));
    assert_eq!(messages[2]["content"], json!("{\"temperature_c\": 17}"));
}

#[tokio::test]
async fn streaming_tool_call_chunks_accumulate_by_index() {
    let server = MockServer::start().await;
    let body = "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\"}}]}}]}\n\n\
data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"SF\\\"}\"}}]}}]}\n\n\
data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"finish_reason\":\"tool_calls\",\"delta\":{}}]}\n\n\
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

    let client = make_client(&server);
    let mut request =
        ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("weather?")]);
    request.tools = vec![weather_tool()];
    let mut stream = client
        .chat_completion_stream(&request)
        .await
        .expect("stream opens");

    // Accumulator: (index -> (id, name, args)).
    let mut accum: Vec<(u32, String, String, String)> = Vec::new();
    let mut last_finish: Option<String> = None;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk parses");
        for choice in &chunk.choices {
            if let Some(reason) = choice.finish_reason.as_ref() {
                last_finish = Some(reason.clone());
            }
            apply_tool_call_deltas(&choice.delta, &mut accum);
        }
    }

    assert_eq!(last_finish.as_deref(), Some("tool_calls"));
    assert_eq!(accum.len(), 1);
    let (idx, id, name, args) = &accum[0];
    assert_eq!(*idx, 0);
    assert_eq!(id, "call_1");
    assert_eq!(name, "get_weather");
    let parsed: Value = serde_json::from_str(args).expect("args parse");
    assert_eq!(parsed["city"], json!("SF"));
}

fn apply_tool_call_deltas(delta: &ChunkDelta, accum: &mut Vec<(u32, String, String, String)>) {
    for tc in &delta.tool_calls {
        if let Some(slot) = accum.iter_mut().find(|(i, ..)| *i == tc.index) {
            if let Some(id) = tc.id.as_deref() {
                slot.1 = id.to_string();
            }
            if let Some(func) = tc.function.as_ref() {
                if let Some(name) = func.name.as_deref() {
                    slot.2 = name.to_string();
                }
                if let Some(args) = func.arguments.as_deref() {
                    slot.3.push_str(args);
                }
            }
        } else {
            let id = tc.id.clone().unwrap_or_default();
            let (name, args) = match tc.function.as_ref() {
                Some(f) => (
                    f.name.clone().unwrap_or_default(),
                    f.arguments.clone().unwrap_or_default(),
                ),
                None => (String::new(), String::new()),
            };
            accum.push((tc.index, id, name, args));
        }
    }
}
