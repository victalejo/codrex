<p align="center"><strong>Codrex</strong> — a fork of OpenAI Codex with MiniMax delegation for cost-efficient code generation.</p>

<p align="center">
  GPT-5 plans and audits. MiniMax M2 implements. ~12× cheaper for mechanical code work.
</p>

---

## What is Codrex?

Codrex is a fork of [OpenAI Codex](https://github.com/openai/codex) (Apache 2.0) that introduces a hybrid execution model:

- **Architect / Auditor** — the primary model (GPT-5 / Codex) plans tasks, generates structured specs, and audits implementation output.
- **Worker** — [MiniMax M2](https://www.minimaxi.com/) receives the spec and writes the code.
- **Configurable delegation** — `--delegation-mode {auto|always|never}` lets you control when delegation happens.

The goal: keep GPT-5's reasoning where it matters (planning, review, hard decisions) and offload mechanical implementation to a model that's an order of magnitude cheaper.

> **Status:** early development. Phase 1 (rebrand) complete. Phases 2 (MiniMax provider) and 3 (delegation orchestrator) in progress. See [the project roadmap](#roadmap).

---

## Quickstart

### Local install (build from source)

Codrex is not yet published to npm or Homebrew. Build from source:

```shell
# Clone the repo
git clone https://github.com/victalejo/codrex.git
cd codrex

# Build and install the CLI binary
cargo install --path codex-rs/cli

# Run it
codrex
```

### Configuration

- Config directory: `~/.codrex/` (with automatic fallback read from `~/.codex/` for migration from upstream Codex).
- Override with the `CODREX_HOME` environment variable (the legacy `CODEX_HOME` is honored as a fallback).

## Distribution (coming soon)

Once the MVP is stable, Codrex will be published to:

- npm: `npm i -g codrex`
- Homebrew: `brew install codrex`

## Roadmap

- [x] **Phase 1** — Rebrand binary, config dir, and user-visible strings
- [ ] **Phase 2** — MiniMax provider (pay-as-you-go API + Coding Plan key)
- [ ] **Phase 3** — Delegation orchestrator (SpecGenerator → Worker → Auditor loop)

## Docs

- [**Contributing**](./docs/contributing.md)
- [**Installing & building**](./docs/install.md)

## License & Attribution

Codrex is licensed under the [Apache License 2.0](LICENSE).

Codrex is a fork of [OpenAI Codex](https://github.com/openai/codex). Original copyright © OpenAI.

Codrex modifications and additions: Copyright © 2026 Victor Cano (IA Portafolio).

See [NOTICE](NOTICE) for full attribution.
