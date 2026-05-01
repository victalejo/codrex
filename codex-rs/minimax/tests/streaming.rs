//! Integration tests for the streaming MiniMax chat completions client.
//!
//! These tests use `wiremock` to serve canned SSE bodies and verify that
//! the resulting `ChatCompletionChunk` stream:
//!  1. forces `stream: true` on the outbound request body
//!  2. correctly parses each `data:` chunk into a typed value
//!  3. terminates on `[DONE]` and on body close
//!  4. surfaces malformed chunks as `MinimaxError::Decode` while continuing
//!     to deliver subsequent valid chunks

use codex_minimax::MinimaxClient;
use codex_minimax::ResolvedAuth;
use codex_minimax::types::ChatCompletionRequest;
use codex_minimax::types::ChatMessage;
use futures::StreamExt;
use serde_json::Value;
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

/// Build a real-shape SSE body. The format here mirrors the live MiniMax
/// stream we captured during validation: standard `data: {...}\n\n` framing
/// followed by `data: [DONE]\n\n`.
fn sse(body: &[&str]) -> String {
    let mut out = String::new();
    for chunk in body {
        out.push_str("data: ");
        out.push_str(chunk);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out
}

#[tokio::test]
async fn stream_yields_text_deltas_in_order() {
    let server = MockServer::start().await;
    let body = sse(&[
        r#"{"id":"x","model":"MiniMax-M2.7","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}"#,
        r#"{"id":"x","model":"MiniMax-M2.7","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":", "}}]}"#,
        r#"{"id":"x","model":"MiniMax-M2.7","object":"chat.completion.chunk","choices":[{"index":0,"finish_reason":"stop","delta":{"content":"world"}}]}"#,
    ]);
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
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    let mut stream = client
        .chat_completion_stream(&request)
        .await
        .expect("stream opens");

    let mut text = String::new();
    let mut last_finish: Option<String> = None;
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk parses");
        for choice in &chunk.choices {
            if let Some(delta) = choice.delta.content.as_ref() {
                text.push_str(delta);
            }
            if let Some(reason) = choice.finish_reason.as_ref() {
                last_finish = Some(reason.clone());
            }
        }
    }
    assert_eq!(text, "Hello, world");
    assert_eq!(last_finish.as_deref(), Some("stop"));
}

#[tokio::test]
async fn stream_forces_stream_true_on_outbound_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(&[
                    r#"{"id":"x","choices":[{"index":0,"finish_reason":"stop","delta":{}}]}"#,
                ])),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    // Build a request with stream=false explicitly — the streaming client
    // must override it to true, otherwise MiniMax would send back a single
    // non-SSE body and there would be no incremental updates.
    let mut request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    request.stream = false;
    let mut stream = client
        .chat_completion_stream(&request)
        .await
        .expect("stream opens");
    while stream.next().await.is_some() {}

    let received: Vec<Request> = server.received_requests().await.expect("requests recorded");
    let body: Value = serde_json::from_slice(&received[0].body).expect("request body is JSON");
    assert_eq!(body["stream"], serde_json::json!(true));
    assert_eq!(body["reasoning_split"], serde_json::json!(true));
}

#[tokio::test]
async fn stream_emits_reasoning_content_when_split_enabled() {
    let server = MockServer::start().await;
    let body = sse(&[
        r#"{"id":"x","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"User asks 2+2."}}]}"#,
        r#"{"id":"x","choices":[{"index":0,"delta":{"reasoning_content":" Answer: 4."}}]}"#,
        r#"{"id":"x","choices":[{"index":0,"delta":{"content":"4"},"finish_reason":"stop"}]}"#,
    ]);
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
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("2+2?")]);
    let mut stream = client
        .chat_completion_stream(&request)
        .await
        .expect("stream opens");

    let mut reasoning = String::new();
    let mut content = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk parses");
        for choice in &chunk.choices {
            if let Some(r) = choice.delta.reasoning_content.as_ref() {
                reasoning.push_str(r);
            }
            if let Some(c) = choice.delta.content.as_ref() {
                content.push_str(c);
            }
        }
    }
    assert_eq!(reasoning, "User asks 2+2. Answer: 4.");
    assert_eq!(content, "4");
}

#[tokio::test]
async fn stream_terminates_on_done_sentinel() {
    let server = MockServer::start().await;
    // Body that includes [DONE] in the middle: anything after it must be
    // ignored.
    let mut body = String::new();
    body.push_str("data: ");
    body.push_str(r#"{"id":"x","choices":[{"index":0,"delta":{"content":"a"}}]}"#);
    body.push_str("\n\n");
    body.push_str("data: [DONE]\n\n");
    body.push_str("data: ");
    body.push_str(r#"{"id":"x","choices":[{"index":0,"delta":{"content":"SHOULD-NOT-APPEAR"}}]}"#);
    body.push_str("\n\n");

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
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    let mut stream = client
        .chat_completion_stream(&request)
        .await
        .expect("stream opens");
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        let chunk = item.expect("chunk parses");
        for choice in &chunk.choices {
            if let Some(c) = choice.delta.content.as_ref() {
                text.push_str(c);
            }
        }
    }
    assert_eq!(text, "a");
}

#[tokio::test]
async fn stream_propagates_5xx_before_streaming() {
    let server = MockServer::start().await;
    let error_body = serde_json::json!({
        "type": "error",
        "error": {"type": "server_error", "message": "boom", "http_code": "500"}
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(error_body))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    match client.chat_completion_stream(&request).await {
        Err(codex_minimax::MinimaxError::Status { status, body }) => {
            assert_eq!(status, 500);
            assert!(body.contains("boom"));
        }
        Err(other) => panic!("expected Status, got {other:?}"),
        Ok(_) => panic!("expected error, got Ok stream"),
    }
}

#[tokio::test]
async fn stream_surfaces_malformed_chunk_but_keeps_running() {
    let server = MockServer::start().await;
    let body = sse(&[
        r#"{"id":"x","choices":[{"index":0,"delta":{"content":"good "}}]}"#,
        r#"{not-valid-json"#,
        r#"{"id":"x","choices":[{"index":0,"finish_reason":"stop","delta":{"content":"after"}}]}"#,
    ]);
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
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    let mut stream = client
        .chat_completion_stream(&request)
        .await
        .expect("stream opens");

    let mut good_count = 0;
    let mut bad_count = 0;
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                good_count += 1;
                for choice in &chunk.choices {
                    if let Some(c) = choice.delta.content.as_ref() {
                        text.push_str(c);
                    }
                }
            }
            Err(codex_minimax::MinimaxError::Decode(_)) => bad_count += 1,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!(good_count, 2);
    assert_eq!(bad_count, 1);
    assert_eq!(text, "good after");
}
