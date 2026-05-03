<p align="center"><strong>Codrex</strong> — a fork of OpenAI Codex with MiniMax delegation for cost-efficient code generation.</p>

<p align="center">
  GPT-5 plans and audits. MiniMax implements. Codrex handles routing, auth, and orchestration.
</p>

---

## What is Codrex?

Codrex is a fork of [OpenAI Codex](https://github.com/openai/codex) (Apache 2.0) that adds a hybrid execution model for coding tasks:

- **Architect / Auditor**: the primary model plans tasks, makes hard decisions, and reviews output.
- **Worker**: a lower-cost delegate model such as MiniMax handles mechanical implementation work.
- **Orchestrator**: `codrex orchestrate` decides when to delegate, dispatches work, and records auditable run logs.
- **Multi-provider auth**: OpenAI remains available, and Codrex adds provider-specific login flows such as MiniMax.

The goal is simple: keep expensive reasoning where it matters, and offload repetitive code generation to a cheaper model.

> **Status:** the Codrex rebrand is complete, MiniMax support is landed, and the delegation orchestrator is available for stateless flows while UX and workflow hardening continue.

## Quickstart

Codrex is currently source-first: build it locally from this repository.

```shell
git clone https://github.com/victalejo/codrex.git
cd codrex

# Install the CLI binary from the Rust workspace.
cargo install --path codex-rs/cli

# Authenticate with the provider you want to use.
codrex login
codrex login minimax

# Try a direct prompt.
codrex "explain this repository"

# Or run the delegation pipeline explicitly.
codrex orchestrate "implement add(a, b) returning a + b"
```

For OS requirements, toolchain setup, and contributor build commands, see [docs/install.md](./docs/install.md).

## Configuration

- Codrex stores its home directory under `~/.codrex/`.
- Override the location with `CODREX_HOME`.
- Legacy upstream paths remain part of the migration story: Codrex can still fall back to `~/.codex/` and `CODEX_HOME` where supported.

## Documentation

- [Installing and building](./docs/install.md)
- [Authentication](./docs/auth.md)
- [MiniMax provider](./docs/minimax.md)
- [Orchestrator](./docs/orchestrator.md)
- [Configuration](./docs/config.md)
- [Development notes](./docs/development.md)
- [Contributing](./docs/contributing.md)

## Repository layout

- `codex-rs/`: Rust workspace for the CLI, orchestrator, MiniMax provider, TUI, and shared crates.
- `codex-cli/`: JavaScript launcher and distribution packaging.
- `sdk/`: language SDKs and related examples.
- `docs/`: product docs, setup notes, and contributor references.

## Distribution

Codrex is not published to npm or Homebrew yet. The intended install targets are:

- `npm i -g codrex`
- `brew install codrex`

## Roadmap

- [x] Phase 1: rebrand binary, config dir, and user-visible strings
- [x] Phase 2: MiniMax provider
- [x] Phase 2.5: multi-provider authentication
- [ ] Phase 3: delegation orchestrator hardening and workflow polish

## License and attribution

Codrex is licensed under the [Apache License 2.0](LICENSE).

Codrex is a fork of [OpenAI Codex](https://github.com/openai/codex). Original copyright © OpenAI.

Codrex modifications and additions: Copyright © 2026 Victor Cano (IA Portafolio).

See [NOTICE](NOTICE) for full attribution.
