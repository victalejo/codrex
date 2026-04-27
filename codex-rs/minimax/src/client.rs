//! HTTP client for the MiniMax chat completions endpoint.
//!
//! This module is intentionally thin: it knows how to send a single request
//! to `<base_url>/chat/completions`, attach the bearer-token header, and
//! deserialize the response. Streaming and tool calling are layered on top
//! in subsequent modules.

use crate::MINIMAX_DEFAULT_MODEL;
use crate::MinimaxError;
use crate::ResolvedAuth;
use crate::types::ChatCompletionRequest;
use crate::types::ChatCompletionResponse;

/// HTTP client for MiniMax chat completions.
#[derive(Debug, Clone)]
pub struct MinimaxClient {
    http: reqwest::Client,
    base_url: String,
    auth: ResolvedAuth,
}

impl MinimaxClient {
    /// Build a client that uses the supplied `reqwest::Client` for outbound
    /// requests. `base_url` should be the platform root without a trailing
    /// slash (e.g. `https://api.minimax.io/v1`).
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, auth: ResolvedAuth) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            auth,
        }
    }

    /// Default model used when the caller does not specify one.
    pub fn default_model(&self) -> &'static str {
        MINIMAX_DEFAULT_MODEL
    }

    /// The configured base URL, useful for diagnostics and logging.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Borrow the underlying HTTP client. Used by the streaming module so
    /// that streaming and non-streaming requests reuse the same connection
    /// pool and TLS configuration.
    pub(crate) fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    /// Borrow the bearer token used for the `Authorization` header.
    pub(crate) fn bearer_token(&self) -> &str {
        &self.auth.bearer_token
    }

    /// Issue a non-streaming chat completion. Streaming requests go through
    /// a separate entry point (added in a follow-up commit).
    pub async fn chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, MinimaxError> {
        if request.stream {
            return Err(MinimaxError::Decode(
                "non-streaming endpoint received stream=true; use the streaming client instead"
                    .to_string(),
            ));
        }

        let url = format!("{}/chat/completions", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.auth.bearer_token)
            .header("Content-Type", "application/json")
            .json(request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|_| String::new());
            return Err(MinimaxError::Status {
                status: status.as_u16(),
                body,
            });
        }

        let body = response.text().await?;
        let parsed: ChatCompletionResponse = serde_json::from_str(&body)
            .map_err(|err| MinimaxError::Decode(format!("{err}: body={body}")))?;
        Ok(parsed)
    }
}
