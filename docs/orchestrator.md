# The Codrex Orchestrator

The orchestrator is the planning-and-routing surface that lets Codrex
delegate code-writing turns to a cheaper model (MiniMax M2.7) while
keeping audit and decision logic on the side that runs your prompt.
This document is the operational reference: what the pipeline does,
how to drive it from the CLI, where the structured logs live, and
what to do when something goes wrong.

> **Status**: Phase 3 (commit 9). The pipeline is feature-complete for
> stateless flows. Stateful CLARIFY round-trips are tracked as
> [TODO #19](../TODO.md) for Phase 4.

## Table of contents

- [1. Concepts](#1-concepts)
- [2. Commands](#2-commands)
- [3. Exit codes](#3-exit-codes)
- [4. Configuration](#4-configuration)
- [5. JSONL schema](#5-jsonl-schema)
- [6. Pass-through scenarios](#6-pass-through-scenarios)
- [7. Troubleshooting](#7-troubleshooting)
- [8. Architecture pointers](#8-architecture-pointers)

---

## 1. Concepts

### 1.1 What the orchestrator is

The orchestrator is a small state machine that sits in front of a
delegate model. It receives a user intent (a prompt), decides whether
that intent is worth delegating, dispatches it to the delegate when it
is, audits the response against acceptance criteria, and emits a
final verdict.

Two principles drive the design:

- **The expensive model reasons; the cheap model implements.** A
  rules-first classifier with an LLM fallback decides which prompts
  belong to the cheap delegate (MiniMax) and which should be passed
  through to the expensive frontier model. The frontier model is
  never billed for trivially mechanical work, and the cheap model is
  never asked to design architecture.
- **Every verdict is auditable.** Every classification, dispatch,
  and audit decision is recorded as a single JSONL row in
  `~/.codrex/runs/runs-YYYY-MM-DD.jsonl`, correlated by `run_id` with
  the `codrex.cost` events the MiniMax bridge emits. You can replay
  any decision from the log alone.

### 1.2 The pipeline

```
     ┌─────────────────────┐
     │  user prompt        │
     └──────────┬──────────┘
                │
                ▼
     ┌─────────────────────┐
     │  classify           │  rules → LLM fallback
     │  (delegation_rules) │  → Delegate | PassThrough
     └──────────┬──────────┘
                │
        ┌───────┴────────┐
        │ PassThrough    │ Delegate
        │                │
        ▼                ▼
  ┌──────────┐   ┌──────────────┐
  │ exit 0   │   │  dispatch    │
  └──────────┘   │  (MiniMax)   │
                 └──────┬───────┘
                        │
                        ▼
                ┌──────────────┐
                │  audit       │  acceptance criteria:
                │              │  - no_forbidden_patterns
                │              │  - output_matches
                │              │  - tests_pass
                └──────┬───────┘
                       │
        ┌──────────────┼──────────────┬──────────┐
        │              │              │          │
        ▼              ▼              ▼          ▼
     ┌─────┐       ┌──────┐      ┌────────┐  ┌──────┐
     │ Ok  │       │Retry │      │Escalate│  │Drop  │
     │ → 0 │       │ ↺    │      │  → 2   │  │ → 3  │
     └─────┘       └──────┘      └────────┘  └──────┘
                       │
                       └──→ next attempt with
                            structured feedback,
                            up to --max-retries

         (Clarify is a fifth terminal verdict
          emitted from dispatch when the delegate
          asks a question instead of generating
          code → exit 4.)
```

Stages emit JSONL rows in order: `classify` → `dispatch_start` →
`dispatch_end` → `audit.criterion` (one per criterion) → `audit` →
`decision`. A `clarify` row is interleaved if the delegate asks a
question.

### 1.3 The retry loop

When the auditor returns `Retry`, the runner does **not** simply
re-dispatch. It first computes an **error signature** for the failure
(an aggregate of which criteria failed and how) and stores it. On the
next attempt:

- If the new failure has the **same signature** as a previous one,
  the runner concludes the delegate is stuck in a loop and converts
  `Retry` into `Drop` (exit 3).
- If the signature is **new**, the runner builds structured feedback
  ("the response failed `tests_pass`: expected exit 0, got 1; stderr
  ends with `assertion failed`") and feeds it to the next dispatch.
- After `--max-retries` distinct failures (default 2, capped at 10),
  the runner converts `Retry` into `Escalate` (exit 2).

The signature aggregate is stable across schema additions —
forward-compat for new criterion kinds is documented in
[`docs/audit-signature-forward-compat`](#) (see commit
`55fec5d48`).

### 1.4 The CLARIFY convention

When the delegate cannot produce code without more information from
the user (ambiguous requirements, missing constraints, unspecified
input shape), it does **not** invent answers and ship code. It
returns a response prefixed with `CLARIFY:` followed by a single
question.

The orchestrator detects the prefix, emits a `clarify` JSONL row,
records a `Decision` with verdict `clarify`, and exits with code 4.

> **Stateless by design (Phase 3).** The current implementation is a
> one-shot: the user reads the question on stderr, refines the
> prompt on the command line, and re-runs `codrex orchestrate` with
> the refined prompt. No state is preserved between runs.
>
> Stateful round-trips (`codrex orchestrate --resume <run_id>` or
> integration with `codrex resume`) are tracked as
> [TODO #19](../TODO.md) for Phase 4.

Concrete round-trip example:

```bash
$ codrex orchestrate "implement validate_email"
What input format should validate_email accept — a single string,
or a list? And should it return a bool or raise on invalid input?
The model needs clarification before generating code:

  Q: What input format should validate_email accept — a single string,
     or a list? And should it return a bool or raise on invalid input?

To proceed, re-run `codrex orchestrate` with the answer
incorporated into your prompt. For example:

  Original: codrex orchestrate "implement validate_email"
  Refined:  codrex orchestrate "implement validate_email using regex,
            returning bool, no external dependencies"
$ echo $?
4

$ codrex orchestrate "implement validate_email taking a single string,
  returning bool, using a basic regex, no external dependencies"
def validate_email(address: str) -> bool:
    ...
$ echo $?
0
```

The question is written to stdout so it can be piped into another
tool (`xargs`, `read`, an LLM). The framing ("the model needs
clarification…") is written to stderr so it does not pollute the
piped output.

---

## 2. Commands

The single entry point is `codrex orchestrate <prompt>`. Every flag
documented below is optional; the default flow is rules-classify →
delegate-or-passthrough → audit → exit.

### 2.1 The base command

```bash
codrex orchestrate "implement add(a, b) returning a + b"
```

- The intent is matched against `~/.codrex/delegation_rules.toml`.
- If the `implement_function` rule matches (it does, by default),
  the prompt is delegated to MiniMax M2.7.
- The response is audited against the default acceptance criteria
  (none, in this minimal example).
- On `Ok`, the response text is printed to stdout and the process
  exits 0.

### 2.2 Force or skip the classifier: `--force-delegate`, `--no-delegate`

Bypass the classifier entirely:

```bash
# Always delegate, even if no rule matches.
codrex orchestrate --force-delegate "explain this codebase"

# Never delegate; pass the prompt through unchanged.
codrex orchestrate --no-delegate "implement add(a, b)"
```

The two flags are mutually exclusive and the CLI rejects them as a
group conflict at parse time. Both override every rule and the LLM
fallback.

### 2.3 Disable the LLM fallback at runtime: `--no-llm-fallback`

By default, when no rule in `delegation_rules.toml` matches the
prompt, the orchestrator asks an LLM (OpenAI by default) to make a
binary delegate / pass-through decision. To skip that step on a
single invocation:

```bash
codrex orchestrate --no-llm-fallback "make this faster"
```

When the fallback is disabled (by flag, by config, or by missing
credentials), an unmatched prompt is passed through with reason
`llm fallback disabled`.

### 2.4 Bound the retry loop: `--max-retries`

```bash
# Allow up to 5 distinct retry signatures before escalating.
codrex orchestrate --max-retries 5 "implement parse_iso_date"
```

The default (set in `DelegationSpec`) is 2. The CLI caps the value
at 10 to prevent unbounded budget consumption from a misconfigured
script. Loop detection (same signature twice) drops independently
of the retry budget.

### 2.5 Acceptance criteria: `--forbidden`, `--require-output-match`, `--require-tests-cmd`

These flags add structured acceptance criteria to the
`DelegationSpec`. They are repeatable (except `--require-tests-cmd`)
and validated at parse time.

```bash
# Reject responses that contain raw API keys.
codrex orchestrate \
  --forbidden 'sk-[a-zA-Z0-9]{20,}' \
  --forbidden 'api_key\s*=\s*"' \
  "implement an HTTP client wrapper"

# Require the response to mention the exact function name.
codrex orchestrate \
  --require-output-match '\bvalidate_email\b' \
  "implement validate_email"

# Run the test suite after dispatch and require it to pass.
codrex orchestrate \
  --require-tests-cmd 'cargo test --package mycrate' \
  "implement parse_config"
```

`--require-tests-cmd` is shell-style split on whitespace. Quoting
beyond plain whitespace is **not** supported in Phase 3 — wrap your
command in a shell script if you need pipes, redirects, or escaped
spaces:

```bash
# Won't work: the literal '"with spaces"' is split into two args.
codrex orchestrate --require-tests-cmd 'pytest "tests/with spaces"' …

# Works: the wrapper script handles its own quoting.
codrex orchestrate --require-tests-cmd './scripts/run-tests.sh' …
```

`pytest tests/`, `cargo test`, `npm test`, and `go test ./...` are
all valid out of the box.

### 2.6 Routing: `--model`, `--log-dir`

```bash
# Delegate to a specific MiniMax model variant.
codrex orchestrate --model MiniMax-M2.5 "implement quicksort"

# Write the JSONL log somewhere other than ~/.codrex/runs.
codrex orchestrate --log-dir /tmp/codrex-debug "implement add"
```

The default model slug is `MiniMax-M2.7`. The default log directory
is `<CODREX_HOME>/runs`, which honours `CODREX_HOME` if set.

### 2.7 Globally enabling or disabling the orchestrator

There is no `--enable` / `--disable` flag on `codrex orchestrate`.
The orchestrator is invoked only when you call the `orchestrate`
subcommand; ordinary `codrex exec` and `codrex resume` flows are
untouched. To turn the LLM fallback off persistently, edit
`config.toml` (see [§4.2](#42-codrexconfigtoml-orchestratorllm_fallback)).

---

## 3. Exit codes

| Code | Verdict | When |
|------|---------|------|
| `0`  | `Ok`    | The dispatch succeeded and every acceptance criterion passed (or the prompt was passed through). The response text is on stdout. |
| `1`  | infra error | Auth failure, transport error, malformed config, or any other dispatch-level error. The runner did not produce an audit verdict. The error message is on stderr. |
| `2`  | `Escalate` | Acceptance criteria failed and either the failure was non-retryable (e.g. forbidden pattern hit) or the retry budget was exhausted. The reason and blocking issue are on stderr; partial response (if any) is on stdout. |
| `3`  | `Drop`     | Loop detected (same error signature twice in a row) or another unrecoverable condition. Reason and the repeated signature are on stderr. |
| `4`  | `Clarify`  | The delegate asked a question instead of generating code. The question is on stdout; framing instructions on how to re-run with a refined prompt are on stderr. |

> **Stdout discipline**: exit codes 0 and 4 are the only ones that
> write content the user is expected to consume. Codes 1, 2, and 3
> may write the partial response to stdout for debugging, but the
> verdict itself is always on stderr. This way `codrex orchestrate
> … > out.txt` cleanly separates response content from operational
> noise.

---

## 4. Configuration

The orchestrator reads two files under `<CODREX_HOME>` (default
`~/.codrex`):

- `delegation_rules.toml` — the rules-based classifier.
- `config.toml` — global Codrex settings, including a section for
  the orchestrator's LLM fallback.

### 4.1 `~/.codrex/delegation_rules.toml`

The rules file is shipped on first run if missing. It is plain TOML:
a `version` integer and an array of `[[rule]]` tables, evaluated in
order. The first rule whose `patterns` (regex array) match the
intent wins.

```toml
version = 1

[[rule]]
name = "implement_function"
action = "delegate"
patterns = [
  "(?i)\\bimplement\\s+(?:a\\s+|the\\s+)?function\\b",
  "(?i)\\bwrite\\s+(?:a\\s+|the\\s+)?function\\b",
]

[[rule]]
name = "design_arch"
action = "no_delegate"
patterns = [
  "(?i)\\bdesign\\s+(?:the\\s+)?(?:architecture|api|schema|system)\\b",
  "(?i)\\bhow\\s+should\\s+I\\s+structure\\b",
]
```

Schema:

| Field      | Type             | Required | Notes |
|------------|------------------|----------|-------|
| `version`  | integer          | yes      | Currently `1`. Bumped only on breaking changes. |
| `rule`     | array of tables  | yes      | At least one; matched in order. |
| `rule.name`| string           | yes      | Free-form identifier; logged in JSONL as `rule_name`. |
| `rule.action` | `"delegate"` \| `"no_delegate"` | yes | What happens on match. |
| `rule.patterns` | array of strings | yes | Each is a Rust regex. Validated at load time; an invalid pattern is a fatal config error. |

Default rules shipped (commit 6, see `codex-rs/orchestrator/src/classifier/mod.rs`):
`implement_function`, `write_tests`, `translate_code`,
`mechanical_refactor` (all `delegate`), `design_arch`,
`debug_complex`, `security`, `external_integration`
(all `no_delegate`).

### 4.2 `~/.codrex/config.toml` `[orchestrator.llm_fallback]`

```toml
[orchestrator.llm_fallback]
enabled    = true                 # default: true
provider   = "openai"             # only "openai" is supported in commit 7
model      = "gpt-5-mini"         # default; anything the API key can reach
timeout_ms = 4000                 # max wall-clock wait for the call
cache_size = 256                  # in-memory LRU for exact intent matches
```

Every key is optional. Missing keys fall back to the defaults
encoded in `LlmFallbackConfig::default()`. The `--no-llm-fallback`
CLI flag overrides `enabled = true` for a single invocation.

### 4.3 Environment variables

| Variable | Effect |
|----------|--------|
| `OPENAI_API_KEY` | Credentials for the LLM fallback classifier (default provider). When unset and no `auth.json` is found, the fallback is disabled with a non-fatal warning. |
| `MINIMAX_API_KEY` | Credentials for the MiniMax delegate. Pay-as-you-go pool. |
| `MINIMAX_CODING_PLAN_KEY` | Credentials for the MiniMax delegate, subscription / Coding Plan pool. Preferred when both are set, configurable via `AuthPreference`. |
| `CODREX_HOME` | Overrides the default `~/.codrex` directory. The rules file, runs log, and (optionally) `auth.json` are resolved relative to this. |
| `CODREX_ADAPTER_WARN_LOSSY` | When set to `1`, the MiniMax adapter logs a structured `warn` on every translator-induced loss (multimodal stringified, reasoning items dropped, etc.). Useful for tuning the LITE → FULL matrix; see [TODO #4](../TODO.md). |
| `CODREX_MINIMAX_DEBUG_WIRE` | When set to `1`, the MiniMax bridge dumps the full request and response body on any non-2xx HTTP response. Gated to avoid leaking conversation content in production stderr. |
| `CODREX_AUTH_PATH` | Overrides the path to `auth.json`. Useful for running multiple Codrex profiles side-by-side. |

### 4.4 Precedence

For each setting, the resolution order is:

1. Explicit CLI flag (e.g. `--no-llm-fallback`).
2. Environment variable (e.g. `MINIMAX_API_KEY`).
3. `config.toml` (`[orchestrator.llm_fallback]`, `[providers]`, etc.).
4. Built-in default (`LlmFallbackConfig::default()`,
   `DEFAULT_RULES_TOML`).

Credential resolution for MiniMax goes through the same chain in
[`codex-rs/core/src/minimax_adapter.rs`](../codex-rs/core/src/minimax_adapter.rs):
env → `auth.json` → error.

---

## 5. JSONL schema

Every classification, dispatch, and audit decision is appended as a
single JSON line to a daily-rotated file. The log is the canonical
record of orchestrator behaviour: every test in
`codex-rs/orchestrator/tests/` reads it back and asserts on the rows.

### 5.1 Location and rotation

- Default path: `<CODREX_HOME>/runs/runs-YYYY-MM-DD.jsonl`, where
  `YYYY-MM-DD` is local-time date when the row was written.
- A new file is opened on the first write of each calendar day.
  Files are append-only; the orchestrator never edits or rotates
  away an existing row.
- Override the directory with `--log-dir <path>` for a single run.

### 5.2 Common fields

Every row has the same envelope:

```json
{
  "ts": "2026-04-30T18:42:11.913Z",
  "run_id": "019dcfb1-7c3f-7a01-b8b0-2a1a3d4f5e6a",
  "parent_run_id": null,
  "attempt": 0,
  "stage": "dispatch_end",
  "payload": { /* stage-specific */ }
}
```

- `ts` — UTC timestamp, RFC 3339 with millisecond precision.
- `run_id` — UUID v7 for the orchestration. Matches
  `DelegationSpec.run_id` and the `run_id` on `codrex.cost` events
  emitted by the MiniMax bridge, so you can join across logs.
- `parent_run_id` — present (non-null) only when this row belongs
  to a nested delegation. Top-level rows omit the field.
- `attempt` — 0 on the first dispatch, increments on every retry.
- `stage` — see §5.3 for the exhaustive list. Snake-case;
  `audit.criterion` is the one stage with a dotted name (intentional,
  for greppability).

### 5.3 Stages and payloads

#### `classify`

Emitted once per orchestration, before dispatch (or before
pass-through). Records what the classifier decided and why.

```json
{
  "stage": "classify",
  "payload": {
    "outcome": "delegate",
    "reason": "rule matched: implement_function",
    "rule_name": "implement_function",
    "llm_model": null,
    "llm_confidence": null,
    "llm_reasoning": null,
    "llm_error": null,
    "cache_hit": false
  }
}
```

`outcome` is `delegate` or `pass_through`. The `llm_*` fields are
populated only when the rules classifier missed and the LLM fallback
ran. `cache_hit` is `true` when the LLM fallback served the answer
from its in-memory LRU.

#### `dispatch_start`

Emitted just before the runner calls the dispatch sink.

```json
{
  "stage": "dispatch_start",
  "payload": {
    "provider": "minimax",
    "model": "MiniMax-M2.7"
  }
}
```

#### `dispatch_end`

Emitted on every dispatch outcome — successful response, transport
error, or `clarify` short-circuit. Two payload shapes:

Success:

```json
{
  "stage": "dispatch_end",
  "payload": {
    "latency_ms": 4231,
    "total_tokens": 1842,
    "response_len": 612
  }
}
```

Error (non-retryable transport / dispatch failure):

```json
{
  "stage": "dispatch_end",
  "payload": {
    "error": "auth: missing MINIMAX_API_KEY"
  }
}
```

> `total_tokens` is `null` when MiniMax does not return a usage
> block (the streaming endpoint sometimes omits it). See
> [TODO #11](../TODO.md) for the proposed fix.

#### `audit.criterion`

Emitted **once per acceptance criterion**, before the aggregated
`audit` row. One per criterion, even on success — useful for
per-criterion latency dashboards.

```json
{
  "stage": "audit.criterion",
  "payload": {
    "name": "tests_pass",
    "passed": false,
    "duration_ms": 312,
    "details": {
      "exit_code": 1,
      "stderr": "test parse_config_handles_empty ... FAILED",
      "stdout": "running 1 test\nthread 'parse_config' panicked at ..."
    }
  }
}
```

#### `audit`

The aggregated audit decision. The payload is the serialised
`AuditDecision` enum — one of `Ok`, `Retry`, `Escalate`, `Drop`,
`Clarify`. Example for a `Retry`:

```json
{
  "stage": "audit",
  "payload": {
    "verdict": "retry",
    "feedback": "tests_pass failed: stderr ends with 'test parse_config_handles_empty ... FAILED'",
    "failed_criteria": [
      {
        "name": "tests_pass",
        "kind": "tests_pass",
        "details": {
          "exit_code": 1,
          "stderr_excerpt": "test parse_config_handles_empty ... FAILED"
        }
      }
    ],
    "signature": "tests_pass:exit=1"
  }
}
```

#### `decision`

The runner's terminal verdict for the orchestration (or for one
attempt, when retrying). Payload schemas by verdict:

```json
{ "stage": "decision", "payload": { "verdict": "ok" } }

{ "stage": "decision", "payload": {
    "verdict": "retry",
    "reason": "criteria_failed",
    "next_attempt": 1,
    "signature": "tests_pass:exit=1"
} }

{ "stage": "decision", "payload": {
    "verdict": "escalate",
    "reason": "max_retries_exhausted",
    "attempts_exhausted": 3
} }

{ "stage": "decision", "payload": {
    "verdict": "drop",
    "reason": "loop_detected",
    "repeated_signature": "tests_pass:exit=1",
    "attempt": 2
} }

{ "stage": "decision", "payload": {
    "verdict": "clarify",
    "question": "What input format should validate_email accept?"
} }
```

#### `clarify`

Emitted when the delegate response starts with `CLARIFY:`. Always
followed by a `decision` row with `verdict: "clarify"` and the same
question.

```json
{
  "stage": "clarify",
  "payload": {
    "question": "What input format should validate_email accept?",
    "handled": false
  }
}
```

`handled` is reserved for the Phase 4 stateful round-trip
([TODO #19](../TODO.md)) — it will flip to `true` when a future
runner consumes the question instead of exiting.

### 5.4 Correlation with `codrex.cost`

The MiniMax bridge emits a `codrex.cost` event for every successful
dispatch. The event shares `run_id` with the orchestrator log:

```bash
# All rows for a single run, across orchestrator and bridge logs.
jq -c 'select(.run_id == "019dcfb1-7c3f-7a01-b8b0-2a1a3d4f5e6a")' \
  ~/.codrex/runs/runs-2026-04-30.jsonl \
  ~/.codrex/cost/cost-2026-04-30.jsonl
```

### 5.5 Forward compatibility

The `LogStage` enum is intentionally closed (Rust `pub enum`, not
`#[non_exhaustive]`). Adding a new stage is a deliberate breaking
change and forces a `version` bump in the JSONL header (planned for
the first major release). The audit signature aggregate is
forward-compatible across new criterion kinds — see commit
`55fec5d48` for the design note.

---

## 6. Pass-through scenarios

There are five distinct paths to a pass-through, each with a
distinct `reason` string in the `classify` row. All five exit 0 and
print the prompt unchanged.

### 6.1 Rule `no_delegate` matched

The intent matched a rule whose action is `no_delegate` (e.g.
`design_arch`, `security`).

```json
{ "stage": "classify", "payload": {
    "outcome": "pass_through",
    "reason": "rule matched: design_arch",
    "rule_name": "design_arch"
} }
```

### 6.2 No rule matched + ChatGPT auth in use

Codrex detects at startup whether the OpenAI credentials are an API
key or a ChatGPT (`auth.json`) session. ChatGPT auth has an
arbitrary model whitelist that excludes `gpt-5-mini`; rather than
fail mid-run, the LLM fallback is disabled with a one-time warning.

```json
{ "stage": "classify", "payload": {
    "outcome": "pass_through",
    "reason": "no rule matched + llm fallback disabled (chatgpt auth incompatible)"
} }
```

stderr (once per process):

```
warning: orchestrator LLM fallback disabled — ChatGPT auth is
incompatible with gpt-5-mini. Set OPENAI_API_KEY to enable.
```

### 6.3 No rule matched + no credentials

Neither `OPENAI_API_KEY` nor `auth.json` is present. The fallback is
silently disabled — no warning, because this is a legitimate "I
haven't configured an LLM yet" state.

```json
{ "stage": "classify", "payload": {
    "outcome": "pass_through",
    "reason": "no rule matched + llm fallback disabled (no credentials)"
} }
```

### 6.4 No rule matched + LLM fallback unavailable

The fallback is enabled and credentials are present, but the call
itself failed (network error, timeout, OpenAI 5xx). The orchestrator
never blocks the user on a flaky classifier:

```json
{ "stage": "classify", "payload": {
    "outcome": "pass_through",
    "reason": "no rule matched + llm fallback failed",
    "llm_error": "request timed out after 4000ms"
} }
```

### 6.5 Rule `no_delegate` + `--force-delegate` override

`--force-delegate` wins over every rule, including `no_delegate`
ones. This is intentional: the flag is an override, not a hint. The
classify row records the override explicitly so audits can see the
decision was user-driven, not rule-driven.

```json
{ "stage": "classify", "payload": {
    "outcome": "delegate",
    "reason": "user-forced (--force-delegate)"
} }
```

If you need a "respect rules but skip the fallback" semantics,
combine the rules-only path with `--no-llm-fallback` instead.

---

## 7. Troubleshooting

### 7.1 ChatGPT auth: why the LLM fallback is disabled

If you see this warning:

```
warning: orchestrator LLM fallback disabled — ChatGPT auth is
incompatible with gpt-5-mini.
```

…it means Codrex found a `~/.codrex/auth.json` from a ChatGPT
session login. ChatGPT auth restricts which models the session can
reach (an OpenAI-side whitelist), and `gpt-5-mini` is not on it. The
orchestrator detects this at startup and disables the fallback to
avoid a guaranteed-fail call.

To enable the fallback, log in with an API key instead:

```bash
unset OPENAI_API_KEY     # if set to a ChatGPT-derived token
codrex login --provider openai --api-key sk-...
codrex orchestrate "implement add(a, b)"
```

Or set `OPENAI_API_KEY` in your shell directly — env wins over
`auth.json`.

### 7.2 "No credentials configured"

Both `OPENAI_API_KEY` and `auth.json` are missing. The fallback is
disabled silently and unmatched prompts pass through. To configure:

```bash
# Pay-as-you-go API key for the fallback classifier.
export OPENAI_API_KEY=sk-...

# Coding Plan or pay-as-you-go key for the MiniMax delegate.
export MINIMAX_CODING_PLAN_KEY=sk-cp-...
# or:
export MINIMAX_API_KEY=sk-...
```

### 7.3 Rule overlap: prompt matches the wrong rule

The rules are evaluated in order, first-match wins. If a prompt like
"design the auth schema" is being routed to `security` instead of
`design_arch`, you have hit a known overlap: today's `security`
pattern matches any mention of `auth` regardless of context.

Workarounds:

- Reorder `delegation_rules.toml` so the more specific rule comes
  first. Rules are matched top-to-bottom.
- Tighten the `security` patterns in your local rules file to
  require an action verb (`implement`, `store`, `validate`, …)
  alongside the `auth` mention.

The default rule set is being refined to make this overlap less
common — see [TODO #20](../TODO.md).

### 7.4 OpenAI 400 errors with ChatGPT auth

If `--no-llm-fallback` is unset and you see HTTP 400 responses from
OpenAI for the classifier call, the most likely cause is a stale
`auth.json` from a ChatGPT session. The startup detection in §7.1
catches the well-known case (`gpt-5-mini` rejected), but other
arbitrary whitelist behaviours can produce 400s for models the
session is in theory allowed to use. This is a known limitation of
ChatGPT auth, not a bug in Codrex. Use an API key.

### 7.5 MiniMax 400 / "invalid message role"

A pre-existing constraint: MiniMax rejects multi-turn conversations
with mid-conversation `system` messages, and rejects two adjacent
`system` messages even at the top. The MiniMax adapter consolidates
all system content into a single leading turn — see the dedicated
[`docs/minimax.md`](minimax.md) for the full matrix and wire probes.

### 7.6 `failed to record rollout items: thread X not found`

Pre-existing upstream noise from `codex-cli`, reproducible in
upstream `codex 0.125.0`. The rollout file is correctly persisted
to disk; the line is a race with shutdown. See
[TODO #8](../TODO.md). Safe to ignore.

### 7.7 "Tests pass but the binary still misbehaves"

`cargo test --workspace` validates the orchestrator library and CLI
parsing in-process. It does **not** rebuild `target/debug/codrex` or
`target/release/codrex`. If you run end-to-end tests that exec the
binary directly (the integration tests in
`codex-rs/cli/tests/`, or shell-driven smoke tests), rebuild
explicitly first:

```bash
cargo build -p codex-cli --bin codrex
ls -la target/debug/codrex          # confirm timestamp is fresh
./target/debug/codrex orchestrate "implement add(a, b)"
```

See [TODO #18](../TODO.md) — this is documented as a guardrail in
the verification checklists.

---

## 8. Architecture pointers

Source layout for anyone reading the code alongside this doc:

| Concern | File |
|---------|------|
| Entry point (CLI) | [`codex-rs/cli/src/orchestrate_cmd.rs`](../codex-rs/cli/src/orchestrate_cmd.rs) |
| Runner / state machine | [`codex-rs/orchestrator/src/runner.rs`](../codex-rs/orchestrator/src/runner.rs) |
| Rules-based classifier | [`codex-rs/orchestrator/src/classifier/mod.rs`](../codex-rs/orchestrator/src/classifier/mod.rs) |
| LLM fallback classifier | [`codex-rs/orchestrator/src/classifier/llm.rs`](../codex-rs/orchestrator/src/classifier/llm.rs) |
| Auditor (acceptance criteria) | [`codex-rs/orchestrator/src/audit.rs`](../codex-rs/orchestrator/src/audit.rs) |
| `AuditDecision` enum | [`codex-rs/orchestrator/src/decision.rs`](../codex-rs/orchestrator/src/decision.rs) |
| JSONL log writer | [`codex-rs/orchestrator/src/log.rs`](../codex-rs/orchestrator/src/log.rs) |
| `LogStage` / `DispatchSink` traits | [`codex-rs/orchestrator/src/traits.rs`](../codex-rs/orchestrator/src/traits.rs) |
| `DelegationSpec` / `TestSpec` | [`codex-rs/orchestrator/src/spec.rs`](../codex-rs/orchestrator/src/spec.rs) |
| MiniMax credential resolution | [`codex-rs/core/src/minimax_adapter.rs`](../codex-rs/core/src/minimax_adapter.rs) |
| MiniMax wire bridge | [`codex-rs/minimax/src/bridge.rs`](../codex-rs/minimax/src/bridge.rs) |
