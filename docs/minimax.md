# MiniMax Provider

Codrex bundles [MiniMax](https://platform.minimax.io) as a first-class model
provider. This document covers how to authenticate, how to invoke a model,
how to switch regions, and how to read the structured cost logs.

> **Status**: Phase 2 (LITE adapter). The translator covers text messages,
> function calls, and function-call outputs natively; reasoning items,
> multimodal content, and shell-call items are stringified into `tool`
> messages with a structured warn so Phase 3 can refine. See the matrix in
> [`codex-rs/core/src/minimax_adapter.rs`](../codex-rs/core/src/minimax_adapter.rs)
> for full details.

## Setup

Generate an API key at <https://platform.minimax.io/> and export it:

```bash
# Pay-as-you-go pool (default).
export MINIMAX_API_KEY='sk-...'

# Subscription / Coding Plan pool. If both are set, Codrex prefers
# this one (configurable via `AuthPreference`).
export MINIMAX_CODING_PLAN_KEY='sk-cp-...'
```

Both keys are sent via the `Authorization: Bearer <key>` header.

## Invocation

Two equivalent forms:

```bash
# Slash syntax (provider as a prefix on --model).
codrex --model minimax/MiniMax-M2.7 "explain this repo"

# Explicit --provider flag.
codrex --provider minimax --model MiniMax-M2.7 "explain this repo"
```

The two forms are mutually exclusive when they disagree:

```bash
# Errors with: conflicting providers: --model uses prefix "minimax"
# but --provider is "openai"
codrex --model minimax/M2.7 --provider openai
```

You can also configure MiniMax in `~/.codrex/config.toml` and skip the
flags:

```toml
model = "MiniMax-M2.7"
model_provider = "minimax"
```

CLI flags take precedence over `config.toml`, which takes precedence over
the default model.

### Model strings

The Coding Plan pool we have validated against accepts these slugs:

| Model              | Notes                                       |
|--------------------|---------------------------------------------|
| `MiniMax-M2.7`     | Latest. Default in the bundled provider.    |
| `MiniMax-M2.5`     | Older tier. Lower cost, lower quality.      |
| `MiniMax-M2.1`     | Legacy.                                     |
| `MiniMax-M2`       | Base alias; routes to current M2 family.    |

`MiniMax-M2.7-highspeed` exists but requires a higher-tier plan than the
Coding Plan; selecting it returns a clear `your current token plan does
not support model` error.

## Region

By default Codrex points at the international platform:

```
https://api.minimax.io/v1
```

Users in the China region can switch to the regional endpoint with
`MINIMAX_BASE_URL`:

```bash
export MINIMAX_BASE_URL='https://api.minimaxi.com/v1'
```

The same auth keys work against both regions. The override applies to
every MiniMax request the binary makes for the lifetime of the process.

## Reasoning and `<think>` blocks

Codrex sends `reasoning_split: true` on every request. MiniMax responds
with reasoning content in a structured `reasoning_content` /
`reasoning_details[]` field, which Codrex maps to
`ResponseEvent::ReasoningContentDelta` so the TUI shows it on the
reasoning channel rather than mixed into the assistant text.

If a MiniMax tier ignores `reasoning_split` and returns
`<think>...</think>` blocks interleaved in `content`, the
[`think_parser`](../codex-rs/minimax/src/think_parser.rs) defensively
extracts those into the same reasoning channel. You should never see
`<think>` text leak into the user-visible response.

## Cost telemetry

Every MiniMax turn emits two structured `tracing::info!` events with the
same `run_id` (a fresh UUID v4 per turn). Subscribe to them via any
`tracing_subscriber` layer; see
[`docs/example-config.md`](./example-config.md) for general logging
setup.

```text
codrex.cost stage="adapter.completed" provider="minimax" model=MiniMax-M2.7
            endpoint="chat_completions" run_id=<uuid> input_tokens=… 
            output_tokens=… cached_tokens=… reasoning_tokens=…
            total_tokens=… latency_ms=…

codrex.cost stage="bridge.finalize"  ... (same fields)
```

Both fire per turn so log aggregators can verify end-to-end delivery
matches what the bridge produced.

## Adapter warnings (Phase 3 telemetry)

Variants of `ResponseItem` and `ToolSpec` that Phase 2 LITE doesn't
support natively are stringified into `tool` messages. To collect the
data that should drive Phase 3 prioritization, enable the lossy-warn
channel:

```bash
export CODREX_ADAPTER_WARN_LOSSY=1
```

Every lossy translation emits a structured warn:

```text
WARN minimax_adapter: lossy translation, refine in Phase 3
     adapter="minimax" item_type="Reasoning"
     action="stringified — Phase 3 will route via reasoning_details"
```

Default is silent.

## Troubleshooting

| Symptom                                                                   | Likely cause                          | Fix                                                                |
|---------------------------------------------------------------------------|---------------------------------------|--------------------------------------------------------------------|
| `HTTP 401`, body says `login fail: Please carry the API secret key…`      | Missing or malformed `Authorization`. | Make sure the key is exported and has no leading/trailing space.   |
| `HTTP 500`, body says `your current token plan not support model`         | Plan tier doesn't include the model.  | Use `MiniMax-M2.7` (no `-highspeed`) on the Coding Plan.            |
| `HTTP 404` from the chat completions endpoint                             | Wrong base URL.                       | Default is `https://api.minimax.io/v1`. Override with `MINIMAX_BASE_URL` if you need the China region. |
| `HTTP 429`                                                                | Rate limited (Coding Plan QPS cap).   | Slow down or switch to pay-as-you-go via `MINIMAX_API_KEY`.        |
| `unknown provider 'minimax'`                                              | Older binary without the bundled provider. | Rebuild from a tree that contains commit `b4d203b80` or newer.  |
| `--oss cannot be combined with an explicit provider`                      | Mixed flags.                          | Drop `--oss` (it's for local providers like ollama/lmstudio).      |

## Architecture pointers

- Wire-format types: [`codex-rs/minimax/src/types.rs`](../codex-rs/minimax/src/types.rs)
- HTTP + streaming clients: [`client.rs`](../codex-rs/minimax/src/client.rs), [`streaming.rs`](../codex-rs/minimax/src/streaming.rs)
- MiniMax → Codex `ResponseEvent` translator: [`bridge.rs`](../codex-rs/minimax/src/bridge.rs)
- Defensive `<think>` parser: [`think_parser.rs`](../codex-rs/minimax/src/think_parser.rs)
- Codex `Prompt` → MiniMax `ChatCompletionRequest` adapter (Phase 2 LITE):
  [`codex-rs/core/src/minimax_adapter.rs`](../codex-rs/core/src/minimax_adapter.rs)
- Provider registration: [`codex-rs/model-provider-info/src/lib.rs`](../codex-rs/model-provider-info/src/lib.rs)
- CLI flag plumbing: [`codex-rs/utils/cli/src/shared_options.rs`](../codex-rs/utils/cli/src/shared_options.rs)
