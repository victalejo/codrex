//! Integration tests for the non-streaming MiniMax chat completions client.
//!
//! Every test runs against a `wiremock::MockServer` so no network calls hit
//! the real MiniMax platform. A separate, env-gated smoke test covers the
//! live endpoint in commit 8.

use codex_minimax::MinimaxClient;
use codex_minimax::ResolvedAuth;
use codex_minimax::types::ChatCompletionRequest;
use codex_minimax::types::ChatMessage;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn make_client(server: &MockServer) -> MinimaxClient {
    let auth = ResolvedAuth {
        bearer_token: "test-token".to_string(),
        env_var: "MINIMAX_API_KEY",
    };
    MinimaxClient::new(reqwest::Client::new(), server.uri(), auth)
}

/// Real-shape MiniMax-M2.7 response with `reasoning_split` enabled.
fn sample_reasoning_split_response() -> Value {
    json!({
        "id": "test-123",
        "model": "MiniMax-M2.7",
        "object": "chat.completion",
        "created": 1_777_270_128_u64,
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": "4",
                "reasoning_content": "User asks 2+2; answer is 4.",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-text-1",
                    "format": "MiniMax-response-v1",
                    "index": 0,
                    "text": "User asks 2+2; answer is 4."
                }]
            }
        }],
        "usage": {
            "prompt_tokens": 12,
            "completion_tokens": 8,
            "total_tokens": 20,
            "prompt_tokens_details": {"cached_tokens": 4},
            "completion_tokens_details": {"reasoning_tokens": 6}
        }
    })
}

#[tokio::test]
async fn chat_completion_happy_path_with_reasoning_split() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("Authorization", "Bearer test-token"))
        .and(header("Content-Type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_reasoning_split_response()))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request =
        ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("What is 2+2?")]);
    let response = client.chat_completion(&request).await.expect("ok response");

    assert_eq!(response.model, "MiniMax-M2.7");
    let msg = &response.choices[0].message;
    assert_eq!(msg.content, "4");
    assert_eq!(
        msg.reasoning_content.as_deref(),
        Some("User asks 2+2; answer is 4.")
    );
    let details = msg.reasoning_details.as_ref().expect("details present");
    assert_eq!(details[0].format, "MiniMax-response-v1");
    let usage = response.usage.expect("usage present");
    assert_eq!(usage.prompt_tokens, 12);
    assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 4);
}

#[tokio::test]
async fn chat_completion_sends_reasoning_split_true_in_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(sample_reasoning_split_response()))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    client.chat_completion(&request).await.expect("ok");

    let received: Vec<Request> = server.received_requests().await.expect("requests recorded");
    assert_eq!(received.len(), 1);
    let body: Value = serde_json::from_slice(&received[0].body).expect("request body is JSON");
    assert_eq!(body["reasoning_split"], json!(true));
    assert_eq!(body["model"], json!("MiniMax-M2.7"));
    assert_eq!(body["messages"][0]["role"], json!("user"));
    assert_eq!(body["messages"][0]["content"], json!("hi"));
    // Stream defaults to false → must be omitted from the wire body so we
    // don't accidentally request a streaming response.
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn chat_completion_propagates_5xx_with_body() {
    let server = MockServer::start().await;
    let error_body = json!({
        "type": "error",
        "error": {
            "type": "server_error",
            "message": "your current token plan not support model, MiniMax-M2.7-highspeed (2061)",
            "http_code": "500"
        }
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(500).set_body_json(error_body))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request =
        ChatCompletionRequest::new("MiniMax-M2.7-highspeed", vec![ChatMessage::user("hi")]);
    let err = client
        .chat_completion(&request)
        .await
        .expect_err("server error");
    match err {
        codex_minimax::MinimaxError::Status { status, body } => {
            assert_eq!(status, 500);
            assert!(body.contains("token plan not support model"));
        }
        other => panic!("expected MinimaxError::Status, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_completion_propagates_401_unauthorized() {
    let server = MockServer::start().await;
    let error_body = json!({
        "type": "error",
        "error": {
            "type": "authorized_error",
            "message": "login fail: Please carry the API secret key in the 'Authorization' field of the request header (1004)",
            "http_code": "401"
        }
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(error_body))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    let err = client.chat_completion(&request).await.expect_err("401");
    match err {
        codex_minimax::MinimaxError::Status { status, .. } => assert_eq!(status, 401),
        other => panic!("expected MinimaxError::Status, got {other:?}"),
    }
}

#[tokio::test]
async fn chat_completion_rejects_stream_true_at_non_streaming_endpoint() {
    let server = MockServer::start().await;
    // Mount nothing — request must fail before any HTTP traffic.

    let client = make_client(&server);
    let mut request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    request.stream = true;
    let err = client
        .chat_completion(&request)
        .await
        .expect_err("misuse error");
    assert!(matches!(err, codex_minimax::MinimaxError::Decode(_)));
}

#[tokio::test]
async fn chat_completion_decode_error_includes_body_for_diagnostics() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not-json-at-all"))
        .expect(1)
        .mount(&server)
        .await;

    let client = make_client(&server);
    let request = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
    let err = client
        .chat_completion(&request)
        .await
        .expect_err("decode err");
    match err {
        codex_minimax::MinimaxError::Decode(msg) => {
            assert!(msg.contains("body="));
            assert!(msg.contains("not-json-at-all"));
        }
        other => panic!("expected Decode error, got {other:?}"),
    }
}
