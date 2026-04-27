//! Live smoke test against the real MiniMax API.
//!
//! This test is **disabled by default** because it would otherwise hit
//! `api.minimax.io`, consume tokens, and break CI when MiniMax is
//! offline. Enable it locally to confirm the adapter still works
//! end-to-end with a current key after upgrades:
//!
//! ```text
//! MINIMAX_LIVE_TEST=1 \
//!   MINIMAX_API_KEY='sk-...' \
//!   cargo test -p codex-minimax --test live_smoke -- --ignored --nocapture
//! ```
//!
//! Override the model with `MINIMAX_LIVE_MODEL` (default
//! `MiniMax-M2.7`). Pick a model your plan supports — Coding Plan keys
//! cannot reach the `-highspeed` variants and will return a 500 with
//! "token plan not support model".
//!
//! The test deliberately:
//! - issues a single request, no loops, no retries
//! - asks for a fixed phrase so the assertion is stable
//! - asserts on substring (case-insensitive), not on exact tokens
//! - never asserts on token counts (those vary between runs)

use codex_minimax::AuthPreference;
use codex_minimax::MinimaxClient;
use codex_minimax::resolve_auth_from_env;
use codex_minimax::resolve_base_url;
use codex_minimax::types::ChatCompletionRequest;
use codex_minimax::types::ChatMessage;

const ENABLE_VAR: &str = "MINIMAX_LIVE_TEST";
const MODEL_OVERRIDE_VAR: &str = "MINIMAX_LIVE_MODEL";
const DEFAULT_LIVE_MODEL: &str = "MiniMax-M2.7";

fn live_enabled() -> bool {
    std::env::var(ENABLE_VAR)
        .ok()
        .is_some_and(|v| !v.trim().is_empty() && v != "0")
}

fn pick_model() -> String {
    std::env::var(MODEL_OVERRIDE_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_LIVE_MODEL.to_string())
}

#[tokio::test]
#[ignore = "live MiniMax API; opt in with MINIMAX_LIVE_TEST=1"]
async fn live_minimax_chat_completion_returns_ok() {
    if !live_enabled() {
        eprintln!(
            "{ENABLE_VAR} not set — skipping live MiniMax smoke test."
        );
        return;
    }

    let auth = resolve_auth_from_env(AuthPreference::default())
        .expect("MINIMAX_API_KEY or MINIMAX_CODING_PLAN_KEY must be set");
    let base_url = resolve_base_url();
    let client = MinimaxClient::new(reqwest::Client::new(), base_url, auth);

    let model = pick_model();
    eprintln!("MiniMax live smoke test: model={model}");

    // Pin the prompt so the assertion is stable across runs.
    let request = ChatCompletionRequest::new(
        model.clone(),
        vec![ChatMessage::user(
            "Reply with the word OK and nothing else. No punctuation.",
        )],
    );

    let response = client
        .chat_completion(&request)
        .await
        .expect("non-streaming chat completion succeeds");

    assert_eq!(response.model, model);
    assert!(!response.choices.is_empty(), "response had no choices");

    // The reply may live in `content` (when reasoning_split honored
    // returns the visible answer there) or partially leaked through
    // `<think>` despite reasoning_split being requested. Accept either —
    // we're checking the live path works, not exact framing.
    let raw = &response.choices[0].message.content;
    let cleaned = strip_think_blocks(raw);
    let needle = "ok";
    assert!(
        cleaned.to_lowercase().contains(needle),
        "expected response content (after stripping <think>) to contain {needle:?}; got {cleaned:?}"
    );

    if let Some(usage) = response.usage.as_ref() {
        eprintln!(
            "live tokens: input={} output={} total={}",
            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
        );
    }
}

/// Tiny inline `<think>...</think>` stripper for the smoke test only.
/// Production code uses `codex_minimax::think_parser::ThinkParser`.
fn strip_think_blocks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find("<think>") {
        out.push_str(&rest[..open]);
        rest = &rest[open + "<think>".len()..];
        match rest.find("</think>") {
            Some(close) => rest = &rest[close + "</think>".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}
