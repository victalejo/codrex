//! Codrex Phase 3 orchestration layer.
//!
//! The orchestrator decides whether a user prompt should be **delegated**
//! to a secondary model (Phase 3 ships MiniMax as the only delegate
//! target) or passed through to the default agent. Each delegation runs
//! through a four-stage pipeline:
//!
//! 1. **Classify** — turn the prompt into a `DelegationSpec` or decide
//!    to pass through. The classifier is hybrid: rules first
//!    (`~/.codrex/delegation_rules.toml`) → LLM fallback when rules
//!    don't match.
//! 2. **Dispatch** — send the spec to the delegate model and capture
//!    its response (text + applied diffs + tool calls).
//! 3. **Audit** — match the response against the spec's
//!    `acceptance` criteria and decide
//!    [`AuditDecision::Ok`]/`Retry`/`Escalate`/`Drop`.
//! 4. **Log** — append a structured JSONL entry per stage to
//!    `~/.codrex/runs/runs-YYYY-MM-DD.jsonl` so the full lifecycle is
//!    inspectable post-hoc.
//!
//! # Phase 3 LITE scope (this crate at commit 1)
//!
//! Only the type system + trait skeletons land here. Implementations
//! arrive in subsequent commits:
//!
//! ```text
//! Commit | Lands
//! -------|----------------------------------------------------------
//! 1      | This crate: types, trait stubs, validation, debug toggle
//! 2      | JsonlDecisionLog + daily rotation + concurrent-write tests
//! 3      | `codrex orchestrate` subcommand happy path (--force-delegate)
//! 4      | Auditor (forbidden patterns + tests + acceptance criteria)
//! 5      | Retry/Escalate/Drop decisions wired into the orchestrate loop
//! 6      | RulesClassifier + ~/.codrex/delegation_rules.toml
//! 7      | LlmClassifier fallback (OpenAI structured-output)
//! 8      | CLARIFY: convention with user round-trip
//! 9      | Integration tests + docs/orchestrator.md
//! ```
//!
//! # Debug toggle
//!
//! `CODREX_ORCH_DEBUG=1` enables structured stderr dumps of the spec,
//! classifier decision, and audit decision at each stage. Use during
//! local debugging — it's gated to keep production stderr quiet. See
//! [`orch_debug_enabled`].

pub mod context;
pub mod decision;
pub mod error;
pub mod log;
pub mod spec;
pub mod traits;

pub use context::DelegationContext;
pub use decision::AuditDecision;
pub use error::SpecError;
pub use log::Clock;
pub use log::InMemoryDecisionLog;
pub use log::JsonlDecisionLog;
pub use log::SystemClock;
pub use spec::AcceptanceCriterion;
pub use spec::DelegationSpec;
pub use spec::ScriptRef;
pub use spec::TestSpec;
pub use spec::UtilRef;
pub use traits::Auditor;
pub use traits::Classifier;
pub use traits::ClassificationOutcome;
pub use traits::DecisionLog;
pub use traits::DispatchOutcome;
pub use traits::DispatchSink;
pub use traits::LogStage;

/// Returns whether `CODREX_ORCH_DEBUG=1` is set in the environment.
///
/// When true, orchestrator stages emit structured stderr dumps in
/// addition to their normal `tracing` output. Mirrors the gate already
/// established by `CODREX_MINIMAX_DEBUG_WIRE=1` in the MiniMax adapter,
/// so a single mental model applies across debug toggles.
pub fn orch_debug_enabled() -> bool {
    std::env::var("CODREX_ORCH_DEBUG")
        .ok()
        .is_some_and(|v| !v.trim().is_empty() && v != "0")
}
