# Authentication

Codrex authenticates with two kinds of providers:

1. **OpenAI** — via the upstream Codex flow. Either ChatGPT OAuth (browser)
   or an API key. Unchanged from upstream Codex.
2. **Other providers** (MiniMax today; Qwen / DeepSeek / GLM in future
   phases) — via API keys, stored in `$CODREX_HOME/auth.json` under a
   per-provider map.

This document covers both. For MiniMax-specific notes (model strings,
regions, troubleshooting), see [`docs/minimax.md`](./minimax.md).

## Quick start

```bash
# OpenAI (upstream flow, unchanged).
codrex login                              # ChatGPT OAuth (browser)
codrex login --with-api-key < ~/.openai   # API key from stdin

# MiniMax (Phase 2.5).
codrex login minimax                              # interactive prompt
codrex login minimax --with-api-key               # read key from stdin
codrex login minimax --api-key sk-cp-...          # inline (deprecated)
codrex login minimax --coding-plan                # tag as Coding Plan key

codrex login --list                       # show all configured credentials
codrex logout minimax                     # remove just MiniMax
codrex logout                             # clear the entire auth payload
```

## Credential resolution order

When the runtime needs a provider's API key, it checks sources in this
order and uses the **first match**:

1. **CLI / config** — provider-specific env keys declared in
   `model_providers.<id>.env_key` (today: `MINIMAX_API_KEY` for the
   bundled MiniMax provider).
2. **Standard provider env vars** — `MINIMAX_API_KEY` (pay-as-you-go) or
   `MINIMAX_CODING_PLAN_KEY` (Coding Plan). When both are set, the
   Coding Plan key is preferred.
3. **`auth.json`** — `$CODREX_HOME/auth.json::providers["<id>"].api_key`,
   resolved via the active credential store backend (file or keyring,
   depending on `cli_auth_credentials_store_mode`).
4. **Error** — actionable message:
   `no credentials for provider 'minimax'. Run \`codrex login minimax\`
    or set MINIMAX_API_KEY (pay-as-you-go) / MINIMAX_CODING_PLAN_KEY
    (Coding Plan).`

OpenAI uses the upstream resolution chain (`auth.json::OPENAI_API_KEY`
or `auth.json::tokens` for ChatGPT OAuth, plus the `OPENAI_API_KEY` env
var). The new `providers` map in `auth.json` does not affect the
OpenAI path.

## File format

`$CODREX_HOME/auth.json` (mode `0600`):

```json
{
  "auth_mode": "apikey",
  "OPENAI_API_KEY": "sk-...",
  "tokens": null,
  "providers": {
    "minimax": {
      "api_key": "sk-cp-...",
      "kind": "coding_plan",
      "last_verified": "2026-04-27T12:00:00Z"
    }
  }
}
```

- The OpenAI subset (`auth_mode`, `OPENAI_API_KEY`, `tokens`,
  `last_refresh`, `agent_identity`) lives at the top level for backwards
  compatibility with files written by upstream Codex.
- Other providers live under `providers.<id>`.
  - `api_key` — bearer token sent via `Authorization: Bearer <key>`.
  - `kind` — free-form string per provider. For MiniMax: `"standard"` or
    `"coding_plan"`. Other providers can ignore or repurpose it.
  - `last_verified` — ISO-8601 wallclock of the last successful
    `--test-connection` run. Set only by `codrex login --test-connection`
    or by the interactive "Test the connection?" prompt; never updated
    automatically by the runtime.

## Storage backends

The user's `cli_auth_credentials_store` setting (in `~/.codrex/config.toml`)
selects which backend persists credentials:

| Mode        | Behavior                                                       |
| ----------- | -------------------------------------------------------------- |
| `auto`      | Try the OS keyring first; fall back to `auth.json` on failure. |
| `keyring`   | OS keyring only (macOS Keychain / libsecret / Windows CM).     |
| `file`      | `auth.json` only.                                              |
| `ephemeral` | In-memory; only used by tests.                                 |

MiniMax credentials follow the same backend as OpenAI — Codrex does not
add a separate keyring integration for non-OpenAI providers. Whatever
mode the user has configured for OpenAI auth is what the multi-provider
storage uses. When the keyring backend is active, the `providers` map
lives inside the same JSON blob the keyring already stores.

## `codrex login <provider>`

Three credential-supply paths, in priority order:

1. `--with-api-key` — read the key from stdin. Recommended for scripts
   and CI: `printenv MINIMAX_API_KEY | codrex login minimax --with-api-key`.
2. `--api-key sk-...` — inline (deprecated). Stays in shell history;
   emits a stderr warning. Hidden from `--help` output.
3. **Interactive** — a hidden-input prompt (rpassword), only shown when
   stdin is a TTY. Piping a key without `--with-api-key` exits with a
   clear error rather than hanging.

Other flags:

- `--coding-plan` — tag the saved credential as a Coding Plan key
  (`kind: "coding_plan"` in `auth.json`). Without this flag the
  interactive flow asks `Is this a Coding Plan key? [y/N]:`.
- `--test-connection` — opt-in for non-interactive flows; the
  interactive flow asks `Test the connection? [Y/n]:` automatically.

When `--test-connection` runs, Codrex sends a minimal one-shot request
to the provider's chat completions endpoint and reports
`(model, latency_ms)`. **The credential is never deleted on test
failure** — transient network issues must not invalidate a
freshly-saved key. Re-run with `--test-connection` after fixing the
issue.

## `codrex logout <provider>`

Surgical removal: only the named provider's entry is wiped from
`auth.json::providers`. OpenAI auth and any other provider's
credentials are preserved verbatim. When the removal leaves
`auth.json` empty, the file is deleted outright.

`codrex logout` (no provider) keeps the upstream behavior of clearing
the entire auth payload (including OpenAI tokens).

When env vars are still set in the shell, the post-logout message
includes actionable guidance:

```
Note: MINIMAX_API_KEY is still set in your environment.
To fully remove credentials:
  unset MINIMAX_API_KEY    # current shell
  # then remove from ~/.zshrc, ~/.bash_profile, or wherever you set it
```

## `codrex login --list`

Shows all configured credentials in a stable, sorted table. **The API
key itself is never printed** — only the source it lives in.

```
$ codrex login --list
Provider  Type         Source                   Last verified
minimax   coding_plan  ~/.codrex/auth.json      2026-04-27
openai    api_key      env: OPENAI_API_KEY      —
```

The `Source` column distinguishes:

- An absolute path: `/Users/.../auth.json`
- A keyring backend: `keyring (macOS)` / `keyring (libsecret)` /
  `keyring (Windows credential manager)`
- A process env var: `env: VARNAME`

When no credentials are configured anywhere, the output is an
onboarding hint instead of an empty table:

```
$ codrex login --list
No credentials configured.

To get started:
  codrex login                    # OpenAI (interactive OAuth)
  codrex login minimax            # MiniMax (paste API key)
```

## File permissions

`auth.json` is **always written with mode `0o600`** (`-rw-------`) on
Unix, regardless of the storage backend (the file backend writes it
directly; the Auto backend may write it as a fallback when keyring
saves fail). When the keyring backend is active, the underlying OS
keyring API enforces its own access controls and `auth.json` may not
exist at all.

Run this if you ever see `auth.json` with looser permissions:

```bash
chmod 0600 ~/.codrex/auth.json
```

A regression test in the `codex-login` crate
(`provider_credentials_file_is_chmod_0600`) asserts the mode after
every provider write.

## Troubleshooting

| Symptom                                                                                                                  | Cause                                       | Fix                                                                                                                                  |
| ------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `no credentials for provider 'minimax'`                                                                                  | No env var, no `auth.json` entry.           | Run `codrex login minimax` or `export MINIMAX_API_KEY=...`.                                                                          |
| `unknown provider 'X' (known: minimax)`                                                                                  | Provider id is not registered.              | Today only `minimax` is bundled; OpenAI uses the no-arg `codrex login` flow.                                                         |
| `the argument '--list' cannot be used with '[PROVIDER]'`                                                                 | Used `--list` together with a provider arg. | Drop the provider; `--list` is mutually exclusive with everything else.                                                              |
| `--api-key keeps the key in shell history`                                                                               | Used the deprecated inline flag.            | Pipe instead: `printenv MINIMAX_API_KEY \| codrex login minimax --with-api-key`.                                                     |
| `auth.json permissions are 644 (expected 0600)`                                                                          | Pre-existing file with bad perms.           | `chmod 0600 ~/.codrex/auth.json`.                                                                                                    |
| Keyring save fails on macOS in CI                                                                                        | No GUI / no Keychain access.                | Set `cli_auth_credentials_store = "file"` in `~/.codrex/config.toml`. Auto mode falls back automatically when keyring is unavailable. |

## What this phase did NOT change

- **OpenAI flow is untouched.** ChatGPT OAuth, device-code auth, agent
  identity login, and the `--with-api-key` stdin path all work
  exactly as in upstream Codex. The 74 pre-existing
  `codex-login` lib tests still pass without modification.
- **No new keyring backend code.** The bundled `codex-keyring-store`
  abstraction is reused as-is. The user's existing
  `cli_auth_credentials_store` config still controls all storage.
- **No automatic credential migration.** There are no old
  multi-provider credentials to migrate — this is the first phase
  introducing the shape.
- **No env-var renames.** `MINIMAX_API_KEY` and
  `MINIMAX_CODING_PLAN_KEY` keep the same names they had in Phase 2.
- **No live API tests for auth.** Tests use temp dirs and mock servers.
  The optional `--test-connection` flow hits the real provider, but
  only when the user opts in.

## Implementation pointers

- Schema: [`codex-rs/login/src/auth/storage.rs`](../codex-rs/login/src/auth/storage.rs)
  (`AuthFile`, `ProviderCredentials`, `AuthSource`).
- Public APIs: [`codex-rs/login/src/auth/manager.rs`](../codex-rs/login/src/auth/manager.rs)
  (`save_provider_credentials`, `load_provider_credentials`,
  `remove_provider_credentials`, `list_provider_credentials`,
  `auth_source`).
- CLI handlers: [`codex-rs/cli/src/provider_login.rs`](../codex-rs/cli/src/provider_login.rs)
  (`KnownProvider`, `run_login_provider`, `run_provider_logout`,
  `run_login_list`).
- Runtime resolution chain: [`codex-rs/core/src/minimax_adapter.rs::resolve_credentials`](../codex-rs/core/src/minimax_adapter.rs).
- Cross-cutting tests: [`codex-rs/cli/tests/multi_provider_auth.rs`](../codex-rs/cli/tests/multi_provider_auth.rs).
