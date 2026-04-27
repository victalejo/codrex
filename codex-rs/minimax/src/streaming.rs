//! SSE streaming support for MiniMax chat completions.
//!
//! MiniMax exposes the standard OpenAI-compatible SSE format
//! (`data: {...}\n\n` chunks terminated by `data: [DONE]`). This module
//! deserializes each chunk into a [`ChatCompletionChunk`] and emits them as
//! an async `Stream` of typed values.
//!
//! Translation into Codrex's internal `ResponseEvent` enum lives in
//! `bridge.rs` (added in a follow-up commit) so that this module can be
//! tested in isolation against the raw MiniMax wire format.

use crate::MinimaxClient;
use crate::MinimaxError;
use crate::types::ChatCompletionRequest;
use crate::types::Usage;
use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use serde::Deserialize;
use std::pin::Pin;

/// One incremental update in a streamed chat completion.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChatCompletionChunk {
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: u64,
    pub choices: Vec<ChunkChoice>,
    /// Usage is `null` on every chunk except the final one when MiniMax
    /// includes it. Older tiers may omit the field entirely.
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ChunkChoice {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub finish_reason: Option<String>,
    pub delta: ChunkDelta,
}

/// Incremental message delta. All fields are optional because each chunk
/// only carries the slice of state that changed.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ChunkDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    /// Populated when `reasoning_split: true` was sent on the request.
    /// Contains the streamed reasoning text for this chunk.
    #[serde(default)]
    pub reasoning_content: Option<String>,
}

/// Boxed pin'd stream alias used by the public API.
pub type ChunkStream =
    Pin<Box<dyn Stream<Item = Result<ChatCompletionChunk, MinimaxError>> + Send>>;

impl MinimaxClient {
    /// Issue a streaming chat completion. Returns a `Stream` that yields one
    /// [`ChatCompletionChunk`] per SSE `data:` line. The stream terminates
    /// after the `[DONE]` sentinel or when the underlying HTTP body ends.
    ///
    /// This method ensures `request.stream` is set to `true` regardless of
    /// what the caller passed, since a streaming endpoint that received
    /// `stream: false` would simply return a single non-SSE JSON body and
    /// callers would have no incremental updates.
    pub async fn chat_completion_stream(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChunkStream, MinimaxError> {
        let mut request = request.clone();
        request.stream = true;

        let url = format!("{}/chat/completions", self.base_url());
        let response = self
            .http_client()
            .post(&url)
            .bearer_auth(self.bearer_token())
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(MinimaxError::Status {
                status: status.as_u16(),
                body,
            });
        }

        let byte_stream = response.bytes_stream();
        let event_stream = byte_stream.eventsource();
        // Stop reading the SSE body as soon as the `[DONE]` sentinel arrives —
        // anything after it must be ignored even if the server kept the
        // connection open.
        let bounded = event_stream.take_while(|event| {
            let is_done = matches!(event, Ok(ev) if ev.data.trim() == "[DONE]");
            async move { !is_done }
        });
        let mapped = bounded.filter_map(|event| async move {
            match event {
                Ok(event) => {
                    let data = event.data;
                    if data.is_empty() {
                        return None;
                    }
                    match serde_json::from_str::<ChatCompletionChunk>(&data) {
                        Ok(chunk) => Some(Ok(chunk)),
                        Err(err) => Some(Err(MinimaxError::Decode(format!(
                            "{err}: data={data}"
                        )))),
                    }
                }
                Err(err) => Some(Err(MinimaxError::Decode(format!("sse error: {err}")))),
            }
        });

        Ok(Box::pin(mapped))
    }
}
