# Development notes

This document is the contributor-facing reference for working on the
Codrex Rust workspace. It complements the upstream codex documentation
with operational lessons specific to this fork: which test command to
run, why some upstream tests fail outside CI, and a handful of
foot-guns that bit us during Phase 3 and that we don't want to bite
the next contributor.

For user-facing documentation of the orchestrator and its CLI, see
[`orchestrator.md`](orchestrator.md).

## Verifying changes — the official test subset

The canonical verification command for any change in this fork is:

```bash
cd codex-rs
cargo test \
  -p codex-orchestrator \
  -p codex-cli \
  -p codex-config \
  -p codex-minimax \
  -p codex-login
```

This subset covers every crate Codrex has touched in Phase 2 (MiniMax
provider), Phase 2.5 (multi-provider auth), and Phase 3 (orchestrator).
On a clean tree at the time of writing, it produces ~600 passing tests
across the five crates with zero failures.

> **Why a subset and not `cargo test --workspace`?** The full workspace
> command currently fails on this fork due to **pre-existing upstream
> bugs that are unrelated to anything Codrex changed**. Specifically,
> 12 tests in `codex-app-server` assert on values that depend on the
> CI environment (skills count, PATH layout, TTY behaviour). They pass
> in a clean GitHub Actions runner and fail on most local dev boxes —
> particularly any box that has Claude plugins or extra skills
> installed. We do not want to mark those tests `#[ignore]` because
> the files are upstream and any local edit creates merge-conflict
> debt with every upstream sync. Tracked as
> [TODO #23](../TODO.md). The subset above is the exhaustive set of
> crates Codrex actively maintains; running it green is sufficient
> evidence that a change is safe.

If you specifically need to validate a change that touches behaviour
in `codex-core`, `codex-app-server`, `codex-tui`, or another crate not
listed above, run that crate's tests directly:

```bash
cargo test -p codex-core
cargo test -p codex-tui
```

…and skip the workspace command until [TODO #23](../TODO.md) is
resolved.

## Operational lessons

### Rebuild the CLI binary before any end-to-end test

`cargo test -p codex-cli` validates the library code and the clap
parsing layer in-process. **It does not rebuild the
`target/debug/codrex` binary.** If you run end-to-end tests that
actually exec the CLI — the integration tests in
`codex-rs/cli/tests/`, or any shell-driven smoke test — rebuild the
binary explicitly first and confirm the timestamp is fresh:

```bash
cargo build -p codex-cli --bin codrex
ls -la target/debug/codrex            # confirm freshly rebuilt
./target/debug/codrex orchestrate "implement add(a, b)"
```

Skipping the rebuild is the most common cause of "tests pass but the
binary still misbehaves" reports during commit verification. Tracked
as [TODO #18](../TODO.md).

### `cargo test` with pipes needs `set -o pipefail`

If you wrap `cargo test` in a pipeline (e.g. `cargo test --workspace
2>&1 | tail -80`), the exit code of the pipeline is the exit code of
the **last** command (`tail`), not of cargo. A failed test run can
look like a successful one because `tail` always returns 0.

Two safe alternatives:

```bash
# Option 1 — set pipefail in the shell so the leftmost non-zero exit
# code propagates.
set -o pipefail
cargo test --workspace 2>&1 | tee /tmp/test.log | tail -30
echo "EXIT: $?"

# Option 2 — redirect to a file and inspect the explicit exit code.
cargo test --workspace > /tmp/test.log 2>&1
echo "EXIT: $?"
tail -30 /tmp/test.log
```

The second form is preferred for background invocations because it
captures the full log unconditionally and the exit code can't be
masked. Tracked as [TODO #21](../TODO.md).

### `RUST_MIN_STACK` is set to 8 MB in `.cargo/config.toml`

The workspace's [`.cargo/config.toml`](../codex-rs/.cargo/config.toml)
sets `RUST_MIN_STACK = "8388608"` in its `[env]` block. This is
intentional and mirrors the `link-arg=/STACK:8388608` workaround that
upstream codex applies to Windows targets in the same file.

The reason is one specific upstream test
(`codex-app-server`'s `tracing_tests::turn_start_jsonrpc_span_parents_core_turn_spans`)
that consumes more than the default ~2 MB of tokio current_thread
stack in debug builds. Without the bump, it overflows and aborts
the entire `cargo test` process with `SIGABRT`. With the bump, it
passes cleanly. Threshold observed empirically between 2 and 4 MB;
8 MB is set to match the Windows linker workaround.

If you ever need to debug this — for example to verify the threshold
yourself or because a future test consumes even more — clear the env
var explicitly with `env -u RUST_MIN_STACK cargo test …`. Cargo's
`[env]` block applies even when the shell has the variable unset, so
that is the way to confirm it is being read. Tracked as
[TODO #22](../TODO.md).

### Format drift across phases

Phase 3 produced six (now seven) consecutive stashes of pure rustfmt
and clippy-style drift, none of which represent functional change.
The convention while Phase 3 was open was to push that drift onto a
labelled stash (`fmt-noise-batch-N`) and revisit at the end of the
phase, so the substantive commits stayed clean. The end-of-phase
cleanup pops every batch in order, runs `cargo fmt --all`, and
consolidates the result into a single style-only commit.

If you are mid-feature and notice formatter drift unrelated to your
change, prefer stashing it under the same naming convention rather
than mixing it into your work commit.

## Pointers

- [`orchestrator.md`](orchestrator.md) — operational reference for
  the orchestrator pipeline and CLI.
- [`minimax.md`](minimax.md) — MiniMax provider, wire constraints,
  and the `wire_probe` tool.
- [`auth.md`](auth.md) — multi-provider authentication, `auth.json`
  schema, and credential resolution.
- [`../TODO.md`](../TODO.md) — full backlog of known technical debt,
  including the items linked above.
