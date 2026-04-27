//! MiniMax provider for Codrex.
//!
//! This crate adapts the MiniMax chat completions API
//! (`https://api.minimax.io/v1/chat/completions`) into the internal
//! `ResponseEvent` stream that the rest of Codrex consumes. MiniMax exposes an
//! OpenAI-compatible chat completions endpoint, so the adapter is responsible
//! for translating between the two surface APIs:
//!
//! - **Outbound**: convert a Codex-internal `ResponsesApiRequest` into a
//!   MiniMax `chat/completions` JSON body. Always sets `reasoning_split: true`
//!   so reasoning content is surfaced as a structured field instead of being
//!   interleaved as `<think>...</think>` inside the assistant content.
//! - **Inbound**: parse SSE chunks back into `ResponseEvent` deltas, mapping
//!   `reasoning_details[]` into `ResponseEvent::ReasoningContentDelta` and
//!   `tool_calls` into the corresponding tool-call events.
//!
//! Authentication supports two key types:
//! - `MINIMAX_API_KEY` — pay-as-you-go.
//! - `MINIMAX_CODING_PLAN_KEY` — subscription-based Coding Plan pool.
//!
//! Both keys are sent via `Authorization: Bearer <key>`. The adapter prefers
//! the Coding Plan key when both are set.

#![forbid(unsafe_code)]

pub mod client;
pub mod types;

pub use client::MinimaxClient;

/// Unique identifier for the MiniMax provider in the Codrex provider registry.
pub const MINIMAX_PROVIDER_ID: &str = "minimax";

/// Human-readable name for the MiniMax provider.
pub const MINIMAX_PROVIDER_NAME: &str = "MiniMax";

/// Default base URL for the MiniMax international platform. Users in the
/// China region may want to point at `https://api.minimaxi.com/v1` via the
/// `MINIMAX_BASE_URL` environment variable instead.
pub const MINIMAX_DEFAULT_BASE_URL: &str = "https://api.minimax.io/v1";

/// Environment variable used to override the base URL at runtime.
pub const MINIMAX_BASE_URL_ENV: &str = "MINIMAX_BASE_URL";

/// Environment variable holding the standard pay-as-you-go API key.
pub const MINIMAX_API_KEY_ENV: &str = "MINIMAX_API_KEY";

/// Environment variable holding the Coding Plan subscription key.
pub const MINIMAX_CODING_PLAN_KEY_ENV: &str = "MINIMAX_CODING_PLAN_KEY";

/// Default model used when the user does not specify one explicitly. Picked
/// because it is the latest tier available on the Coding Plan pool we have
/// validated against.
pub const MINIMAX_DEFAULT_MODEL: &str = "MiniMax-M2.7";

/// Errors produced by the MiniMax adapter.
#[derive(Debug, thiserror::Error)]
pub enum MinimaxError {
    /// No API key is configured in the environment.
    #[error(
        "no MiniMax API key found. Set `{api}` (pay-as-you-go) or `{coding}` (Coding Plan)",
        api = MINIMAX_API_KEY_ENV,
        coding = MINIMAX_CODING_PLAN_KEY_ENV
    )]
    MissingApiKey,
    /// HTTP request to MiniMax failed (network, TLS, timeout, etc.).
    #[error("MiniMax request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// MiniMax returned a non-success status code.
    #[error("MiniMax returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
    /// Failed to deserialize a MiniMax response or SSE chunk.
    #[error("failed to parse MiniMax payload: {0}")]
    Decode(String),
}

/// Selects which credential to use when both `MINIMAX_API_KEY` and
/// `MINIMAX_CODING_PLAN_KEY` are set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPreference {
    /// Use the pay-as-you-go key first; fall back to Coding Plan if missing.
    PreferPayAsYouGo,
    /// Use the Coding Plan key first; fall back to pay-as-you-go if missing.
    PreferCodingPlan,
}

impl Default for AuthPreference {
    fn default() -> Self {
        Self::PreferCodingPlan
    }
}

/// Resolved authentication material for a MiniMax request.
#[derive(Debug, Clone)]
pub struct ResolvedAuth {
    /// Bearer token sent in the `Authorization` header.
    pub bearer_token: String,
    /// Which environment variable supplied the token, for diagnostics.
    pub env_var: &'static str,
}

/// Read MiniMax credentials from the process environment, honoring the given
/// preference order. Returns `MissingApiKey` if neither variable is set or
/// both are empty.
pub fn resolve_auth_from_env(preference: AuthPreference) -> Result<ResolvedAuth, MinimaxError> {
    resolve_auth_with(preference, |name| std::env::var(name).ok())
}

/// Same as [`resolve_auth_from_env`] but parameterized over a getter so tests
/// can supply a mock without mutating process-wide state.
pub fn resolve_auth_with<G>(
    preference: AuthPreference,
    getter: G,
) -> Result<ResolvedAuth, MinimaxError>
where
    G: Fn(&str) -> Option<String>,
{
    let read = |name: &str| getter(name).filter(|v| !v.trim().is_empty());

    let payg = read(MINIMAX_API_KEY_ENV);
    let coding = read(MINIMAX_CODING_PLAN_KEY_ENV);

    let (bearer_token, env_var) = match preference {
        AuthPreference::PreferCodingPlan => match (coding, payg) {
            (Some(token), _) => (token, MINIMAX_CODING_PLAN_KEY_ENV),
            (None, Some(token)) => (token, MINIMAX_API_KEY_ENV),
            (None, None) => return Err(MinimaxError::MissingApiKey),
        },
        AuthPreference::PreferPayAsYouGo => match (payg, coding) {
            (Some(token), _) => (token, MINIMAX_API_KEY_ENV),
            (None, Some(token)) => (token, MINIMAX_CODING_PLAN_KEY_ENV),
            (None, None) => return Err(MinimaxError::MissingApiKey),
        },
    };

    Ok(ResolvedAuth {
        bearer_token,
        env_var,
    })
}

/// Resolve the base URL: honor `MINIMAX_BASE_URL` if set, otherwise return
/// the default. The returned URL is guaranteed not to end in a trailing slash
/// so callers can append `/chat/completions` directly.
pub fn resolve_base_url() -> String {
    resolve_base_url_with(|name| std::env::var(name).ok())
}

/// Same as [`resolve_base_url`] but parameterized over a getter for tests.
pub fn resolve_base_url_with<G>(getter: G) -> String
where
    G: Fn(&str) -> Option<String>,
{
    let raw = getter(MINIMAX_BASE_URL_ENV)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| MINIMAX_DEFAULT_BASE_URL.to_string());
    raw.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn env_from(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<&'static str, &'static str> = pairs.iter().copied().collect();
        move |name| map.get(name).map(|v| (*v).to_string())
    }

    #[test]
    fn prefers_coding_plan_when_both_set() {
        let auth = resolve_auth_with(
            AuthPreference::PreferCodingPlan,
            env_from(&[
                (MINIMAX_API_KEY_ENV, "payg-key"),
                (MINIMAX_CODING_PLAN_KEY_ENV, "coding-key"),
            ]),
        )
        .expect("auth resolves");
        assert_eq!(auth.bearer_token, "coding-key");
        assert_eq!(auth.env_var, MINIMAX_CODING_PLAN_KEY_ENV);
    }

    #[test]
    fn prefers_payg_when_requested() {
        let auth = resolve_auth_with(
            AuthPreference::PreferPayAsYouGo,
            env_from(&[
                (MINIMAX_API_KEY_ENV, "payg-key"),
                (MINIMAX_CODING_PLAN_KEY_ENV, "coding-key"),
            ]),
        )
        .expect("auth resolves");
        assert_eq!(auth.bearer_token, "payg-key");
        assert_eq!(auth.env_var, MINIMAX_API_KEY_ENV);
    }

    #[test]
    fn falls_back_to_other_key_when_preferred_missing() {
        let auth = resolve_auth_with(
            AuthPreference::PreferCodingPlan,
            env_from(&[(MINIMAX_API_KEY_ENV, "payg-only")]),
        )
        .expect("auth resolves");
        assert_eq!(auth.bearer_token, "payg-only");
        assert_eq!(auth.env_var, MINIMAX_API_KEY_ENV);
    }

    #[test]
    fn errors_when_neither_key_set() {
        let err = resolve_auth_with(AuthPreference::PreferCodingPlan, env_from(&[]))
            .expect_err("missing key");
        assert!(matches!(err, MinimaxError::MissingApiKey));
    }

    #[test]
    fn treats_blank_keys_as_missing() {
        let err = resolve_auth_with(
            AuthPreference::PreferCodingPlan,
            env_from(&[
                (MINIMAX_API_KEY_ENV, "   "),
                (MINIMAX_CODING_PLAN_KEY_ENV, ""),
            ]),
        )
        .expect_err("blank keys are missing");
        assert!(matches!(err, MinimaxError::MissingApiKey));
    }

    #[test]
    fn base_url_returns_default_when_unset() {
        assert_eq!(resolve_base_url_with(env_from(&[])), MINIMAX_DEFAULT_BASE_URL);
    }

    #[test]
    fn base_url_strips_trailing_slash() {
        let url = resolve_base_url_with(env_from(&[(
            MINIMAX_BASE_URL_ENV,
            "https://api.minimaxi.com/v1/",
        )]));
        assert_eq!(url, "https://api.minimaxi.com/v1");
    }

    #[test]
    fn base_url_uses_override_when_set() {
        let url = resolve_base_url_with(env_from(&[(
            MINIMAX_BASE_URL_ENV,
            "https://api.minimaxi.com/v1",
        )]));
        assert_eq!(url, "https://api.minimaxi.com/v1");
    }
}
