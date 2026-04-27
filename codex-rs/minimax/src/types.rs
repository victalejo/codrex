//! Wire-format types for the MiniMax chat completions API.
//!
//! These types deliberately mirror the JSON shape that
//! `https://api.minimax.io/v1/chat/completions` returns. They are an
//! implementation detail of this crate and should not leak outside the
//! adapter layer; callers consume the translated `ResponseEvent` stream.

use serde::Deserialize;
use serde::Serialize;

/// Request body for a MiniMax chat completion. Only the fields Codrex needs
/// today are modelled — additional fields can be added without breaking
/// callers because all serde-skipped fields default to `None`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Whether MiniMax should split `<think>...</think>` content out of the
    /// assistant message into a structured `reasoning_content` /
    /// `reasoning_details` field. Codrex always sets this to `true`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reasoning_split: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
}

impl ChatCompletionRequest {
    /// Build a minimal request with `reasoning_split: true` already set.
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            max_tokens: None,
            reasoning_split: true,
            stream: false,
            temperature: None,
            top_p: None,
        }
    }
}

/// A single message in the conversation. Tool call fields will be added in a
/// follow-up commit when tool calling is wired through.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// Full chat completion response returned for non-streaming requests.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub model: String,
    pub object: String,
    #[serde(default)]
    pub created: u64,
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Choice {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub finish_reason: Option<String>,
    pub message: ResponseMessage,
}

/// The assistant message returned by MiniMax. Mirrors the OpenAI chat
/// completions shape with two MiniMax-specific extensions: `reasoning_content`
/// (string) and `reasoning_details` (structured array) which are populated
/// when `reasoning_split: true` is sent on the request.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ResponseMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
    /// Plain-text reasoning summary when `reasoning_split` is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    /// Structured reasoning blocks. Each entry includes a stable `id` and a
    /// `format` such as `"MiniMax-response-v1"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<ReasoningDetail>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ReasoningDetail {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub format: String,
    #[serde(default)]
    pub index: u32,
    pub text: String,
}

/// Token usage reported by MiniMax. The two `*_details` blocks are optional
/// because earlier model tiers (M2.1, M2.5) don't always emit them.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Captured from a real MiniMax-M2.7 response with `reasoning_split=true`
    /// (see commit message of the scaffold commit for the exact curl).
    const REAL_RESPONSE_WITH_REASONING_SPLIT: &str = r#"{
        "id": "063e3070a75fda24221677e2d6e2c922",
        "choices": [
            {
                "finish_reason": "length",
                "index": 0,
                "message": {
                    "content": "",
                    "role": "assistant",
                    "name": "MiniMax AI",
                    "audio_content": "",
                    "reasoning_content": "The user asks: \"What is 2+2?\"",
                    "reasoning_details": [
                        {
                            "type": "reasoning.text",
                            "id": "reasoning-text-1",
                            "format": "MiniMax-response-v1",
                            "index": 0,
                            "text": "The user asks: \"What is 2+2?\""
                        }
                    ]
                }
            }
        ],
        "created": 1777270128,
        "model": "MiniMax-M2.7",
        "object": "chat.completion",
        "usage": {
            "total_tokens": 104,
            "prompt_tokens": 54,
            "completion_tokens": 50,
            "completion_tokens_details": {"reasoning_tokens": 50}
        },
        "input_sensitive": false,
        "output_sensitive": false,
        "base_resp": {"status_code": 0, "status_msg": ""}
    }"#;

    /// Captured from a real MiniMax-M2.5 response that emits cached_tokens.
    const REAL_RESPONSE_WITH_CACHED: &str = r#"{
        "id": "063e304648c96cba54f2ce7c099da8c3",
        "choices": [{
            "finish_reason": "length",
            "index": 0,
            "message": {
                "content": "<think>The user has just said \"hi\" which is</think>\n\n",
                "role": "assistant"
            }
        }],
        "created": 1777270086,
        "model": "MiniMax-M2.5",
        "object": "chat.completion",
        "usage": {
            "total_tokens": 49,
            "prompt_tokens": 39,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 16}
        }
    }"#;

    #[test]
    fn deserialize_response_with_reasoning_split() {
        let response: ChatCompletionResponse =
            serde_json::from_str(REAL_RESPONSE_WITH_REASONING_SPLIT).expect("parses");
        assert_eq!(response.model, "MiniMax-M2.7");
        assert_eq!(response.choices.len(), 1);
        let msg = &response.choices[0].message;
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.content, "");
        assert_eq!(
            msg.reasoning_content.as_deref(),
            Some("The user asks: \"What is 2+2?\"")
        );
        let details = msg.reasoning_details.as_ref().expect("details present");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].kind, "reasoning.text");
        assert_eq!(details[0].format, "MiniMax-response-v1");
        let usage = response.usage.expect("usage present");
        assert_eq!(usage.completion_tokens_details.unwrap().reasoning_tokens, 50);
    }

    #[test]
    fn deserialize_response_with_cached_tokens() {
        let response: ChatCompletionResponse =
            serde_json::from_str(REAL_RESPONSE_WITH_CACHED).expect("parses");
        let usage = response.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens_details.unwrap().cached_tokens, 16);
        // Without reasoning_split, content carries the <think> block.
        assert!(response.choices[0].message.content.contains("<think>"));
        assert!(response.choices[0].message.reasoning_content.is_none());
    }

    #[test]
    fn request_serializes_reasoning_split_only_when_true() {
        let req = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
        let json = serde_json::to_value(&req).expect("serializes");
        // Must include reasoning_split:true (always set by `new`).
        assert_eq!(json["reasoning_split"], serde_json::json!(true));
        // Stream is false by default → must be omitted.
        assert!(json.get("stream").is_none());
        // max_tokens / temperature / top_p must be omitted when None.
        assert!(json.get("max_tokens").is_none());
        assert!(json.get("temperature").is_none());
        assert!(json.get("top_p").is_none());
    }

    #[test]
    fn request_serializes_stream_when_set() {
        let mut req = ChatCompletionRequest::new("MiniMax-M2.7", vec![ChatMessage::user("hi")]);
        req.stream = true;
        let json = serde_json::to_value(&req).expect("serializes");
        assert_eq!(json["stream"], serde_json::json!(true));
    }
}
