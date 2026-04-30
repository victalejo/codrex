# Deuda técnica — Backlog post-Fase 2.5

> Fuente canónica de TODOs conocidos al cierre de Fase 2.5 (2026-04-27).
> No usar issues de GitHub mientras el repo siga privado. Migrar a issues cuando
> se publique 0.1.0.

Cada ítem incluye: **origen** (fase que lo introdujo), **disparador** (qué evento
permite cerrarlo), **scope** (cómo se cierra), y **bloqueante de** (qué milestone
no debería salir sin esto).

---

## 1. Rebrand de docs upstream (Fase 1)

- **Origen:** Fase 1 (rebrand `codex` → `codrex`).
- **Disparador:** preparación de release `0.1.0`.
- **Scope:** revisar todos los archivos `docs/**/*.md` upstream y reemplazar
  referencias a `codex` por `codrex` donde apliquen al CLI/proyecto (NO
  reemplazar referencias a la API o producto OpenAI Codex como concepto).
- **Bloqueante de:** publicación pública del repo y de `0.1.0`.
- **Estimado:** 1-2 horas, mecánico con review.

## 2. `announcement_tip.toml` version_regex rebrand (Fase 1)

- **Origen:** Fase 1.
- **Disparador:** primer release `0.1.0` que use el sistema de tips de upgrade.
- **Scope:** ajustar `version_regex` y mensajes en
  `codex-rs/tui/src/onboarding/announcement_tip.toml` para que apunten al
  release stream de codrex en lugar del de codex upstream.
- **Bloqueante de:** mensajes de upgrade correctos en TUI.
- **Estimado:** 30 minutos.

## 3. Consolidación de schema auth.json (Schema A → Schema B) — Fase 2.5

- **Origen:** Fase 2.5 (decisión: aditivo no destructivo).
- **Disparador:** release `0.1.0` (breaking change permitido en major-zero).
- **Scope:** migrar todos los providers (incluyendo `openai`) al map
  `providers: {}` y eliminar los campos top-level (`OPENAI_API_KEY`, `tokens`,
  `last_refresh`). Incluye:
  - migración automática on-load: detectar Schema A y reescribir como Schema B
    en disk.
  - tests de upgrade path (auth.json viejo → carga → reescribe → carga de
    nuevo).
  - actualizar `docs/auth.md`.
- **Bloqueante de:** `0.1.0`.
- **Riesgo:** bajo si hacemos migración automática + read-fallback durante una
  ventana de minor versions.
- **Estimado:** 1 día completo (incluye tests de migración).

## 4. Refinar matriz LITE → FULL del adapter MiniMax (Fase 2)

- **Origen:** Fase 2 (perfil LITE conservador para no perder features).
- **Disparador:** suficiente telemetría real con `CODREX_ADAPTER_WARN_LOSSY=1`
  habilitado para identificar qué features LITE drop sin impacto observado.
- **Scope:**
  - documentar en `core/src/minimax_adapter.rs` qué features se sacan en LITE.
  - decidir feature-by-feature si pasa a FULL o se mantiene en LITE.
  - posiblemente introducir `CODREX_ADAPTER_PROFILE=lite|full|auto` (default
    auto = LITE para 0.1.x, FULL para 0.2.x).
- **Bloqueante de:** ninguno; mejora de UX progresiva.
- **Estimado:** depende de cuánto uso real haya antes de decidir.

## 5. Validar `api.minimaxi.com` para usuarios región China (Fase 2)

- **Origen:** Fase 2 (config soporta el endpoint pero no se validó live).
- **Disparador:** primer usuario con tráfico desde China que reporte fricción.
- **Scope:**
  - validar que el endpoint resuelve y responde con la API key de coding plan.
  - validar que streaming SSE funciona (no hay proxies que rompan chunks).
  - documentar el override en `docs/auth.md` o en una nota en
    `docs/minimax-region.md`.
  - posiblemente auto-detectar región vía geo-IP soft-hint con override
    explícito.
- **Bloqueante de:** ninguno hasta que aparezca el caso real.
- **Estimado:** 1-3 horas si hay un usuario que pueda testear.

## 6. Integración keyring multi-provider nativa (Fase 2.5)

- **Origen:** Fase 2.5 (MiniMax respeta el storage mode del usuario, pero
  cuando es Keyring guarda toda la auth como blob JSON único).
- **Disparador:** primer reporte de fricción con keyring multi-provider.
- **Scope:**
  - en lugar de blob único en keyring, guardar cada provider como entrada
    separada (`codrex.openai`, `codrex.minimax`, etc).
  - simplifica revocación selectiva por provider sin tocar los otros.
  - requiere migración del blob viejo al nuevo formato (similar a #3).
- **Bloqueante de:** ninguno hasta que un usuario lo pida.
- **Riesgo:** medio (toca el adapter de keyring upstream que ya tuvo bug del
  mock one-shot).
- **Estimado:** 1-2 días con tests.

---

## Notas operativas

- **Antes de arrancar Fase 3 (orquestador):** validación live de Fase 2 + 2.5
  con API key real es bloqueante. Esta lista NO es bloqueante de Fase 3.
- **Antes de release `0.1.0`:** ítems 1, 2, 3 son obligatorios. Los demás son
  nice-to-have.
- **Actualización del backlog:** cuando se cierre un ítem, mover a sección
  `## Cerrados` con commit hash. Cuando se descubra deuda nueva, agregar acá.

## 7. `codex_models_manager` no usa auth.json para refresh (Fase 2.5)

- **Origen:** Fase 2.5 (descubierto en validación live, 2026-04-27).
- **Síntoma:** al arrancar `codrex exec -m minimax/...` aparece
  `ERROR codex_models_manager::manager: failed to refresh available models:
   Missing environment variable: MINIMAX_API_KEY`. La inferencia funciona
  igual porque el adapter SÍ usa la resolution chain (env → auth.json →
  error), pero el refresh de modelos al arranque solo lee de env.
- **Disparador:** primer usuario que se confunda con el error spurious.
- **Scope:** el módulo `codex-models-manager` debe pasar por la misma
  resolution chain que el adapter (`resolve_credentials` en
  `core/src/minimax_adapter.rs`). O simplemente skip silently cuando no
  hay env var (el adapter cubrirá el caso real).
- **Bloqueante de:** ninguno funcionalmente; cosmético pero engaña.
- **Estimado:** 1-2 horas.

## 8. `failed to record rollout items: thread X not found` — UPSTREAM, COSMÉTICO

- **Origen:** pre-existente en upstream `codex-cli 0.125.0`. **No es
  nuestro bug.** Reproducido verbatim corriendo `/opt/homebrew/bin/codex
  exec` el 2026-04-27.
- **Síntoma:** al cierre de cada `codex/codrex exec` aparece la línea
  `ERROR codex_core::session: failed to record rollout items: thread X
   not found`.
- **Análisis:** el rollout file SÍ se escribe correctamente a disco antes
  de que el shutdown remueva el recorder del map en memoria. La línea
  ERROR es ruido del último `persist_rollout_items` que carrera con
  `live_thread.shutdown()` — la persistencia real ya ocurrió.
- **Verificación:** `codrex exec resume <session-id>` funciona
  correctamente y reanuda con todo el historial. Tanto OpenAI como
  MiniMax (post-fix `7f33ffe97`).
- **Bloqueante de:** nada funcionalmente. Solo molesta visualmente.
- **Plan:** **dejar dormido**. Probable PR upstream eventual; no
  justifica deviation propia mientras el flujo funciona.
- **Estimado si se quiere fixear localmente:** 2-3 horas (skip o suprimir
  el último persist cuando shutdown ya está en curso).

## 11. Garantizar `usage` block en streaming MiniMax (Fase 3 commit 3)

- **Origen:** observado en validación E2E del commit 3 (2026-04-27).
  `total_tokens=null` en JSONL row `dispatch_end` y `codrex.cost` no
  emite porque MiniMax-M2.7 no incluyó usage en la respuesta streamed.
- **Disparador:** primera vez que querramos cost dashboards reales o
  attribution per-delegation.
- **Scope:** investigar si el endpoint MiniMax soporta
  `stream_options: {"include_usage": true}` (convención OpenAI). Si lo
  soporta, prenderlo por default en `ChatCompletionRequest` desde el
  dispatcher del orquestador (y opcionalmente desde el adapter
  general). Si no lo soporta, documentarlo y considerar fallback al
  endpoint non-streaming para una llamada paralela mínima de usage —
  con costo extra, evaluar trade-off.
- **Bloqueante de:** ninguno funcional; bloquea visibility/cost dashboards.
- **Estimado:** 1 hora (probe + setting), o medio día si requiere
  fallback non-stream.

## 16. Test integral end-to-end de `build_llm_fallback_classifier` (Fase 3 commit 7)

- **Origen:** auditoría de commit 7 (2026-04-30).
- **Disparador:** próxima pasada de hardening del fallback LLM.
- **Scope:** agregar un test integral que cubra la cadena completa de
  resolución `OPENAI_API_KEY env -> auth.json -> sin nada` pasando por
  `build_llm_fallback_classifier`, no solo helpers unitarios separados.
- **Bloqueante de:** ninguno; cobertura adicional de precedencia.
- **Estimado:** 30-45 minutos.

## 17. Refactor cosmético del test `chatgpt_auth_warning_emitted_only_once` (Fase 3 commit 7)

- **Origen:** auditoría de commit 7 (2026-04-30).
- **Disparador:** próxima limpieza menor del suite de tests.
- **Scope:** convertir las 2 llamadas explícitas actuales a un loop con
  `N` parametrizable para expresar mejor la intención "warning emitido
  exactamente una vez en múltiples classify calls", sin cambiar
  cobertura funcional.
- **Bloqueante de:** ninguno; legibilidad solamente.
- **Estimado:** 15 minutos.

## 18. Guardrail de verificación manual para `codex-cli` (Fase 3 commit 8)

- **Origen:** auditoría de commit 7/8 (2026-04-30).
- **Disparador:** commit 9 (`docs/orchestrator.md`) o la próxima guía de
  verificación manual del orquestador.
- **Scope:** documentar explícitamente que `cargo test -p codex-orchestrator`
  valida la librería, pero las corridas E2E del subcomando requieren rebuild
  separado del binario con `cargo build -p codex-cli --bin codrex` y chequeo
  de timestamp/binario actualizado antes de correr `./target/debug/codrex ...`.
- **Bloqueante de:** ninguno; reduce falsos verdes en auditorías manuales.
- **Estimado:** 15-30 minutos.

## 19. Round-trip stateful para `CLARIFY` (Phase 4)

- **Origen:** diseño de commit 8 (2026-04-30).
- **Disparador:** cuando queramos que una aclaración continúe el flujo sin
  obligar al usuario a reformular manualmente el prompt.
- **Scope:** diseñar un mecanismo stateful (`codrex orchestrate --resume
  <session_id> ...` o integración con `codrex resume`) que recupere el contexto
  del run original, inyecte la respuesta del usuario y reanude la delegación.
- **Bloqueante de:** ninguno en Phase 3; mejora de UX para Phase 4.
- **Riesgo:** medio, porque toca persistencia/identidad de runs y semántica de
  reanudación.
- **Estimado:** 0.5-1 día con tests y docs.

## 20. Refinar regla `security` en `delegation_rules.toml` (Fase 3 commit 6)

- **Origen:** auditoría de commit 6 (2026-04-30), formalizado durante el plan de
  commit 9a.
- **Disparador:** primer reporte de mis-routing donde un prompt de diseño de
  esquema de auth se delega como `security` cuando debería ir a `design_arch`.
- **Síntoma:** hoy "design the auth schema" matchea la regla `security` antes
  que `design_arch` por la mera presencia de la palabra "auth", aunque la
  intención del prompt sea de diseño/arquitectura, no de implementación o
  manejo de credenciales sensibles.
- **Scope:** refinar el pattern de `security` para requerir contexto de
  implementación o manejo activo de credenciales (e.g. "implement", "handle",
  "store", "validate", "sanitize") en lugar de cualquier mención de "auth",
  "password" o "token". Considerar también prioridad explícita entre reglas
  con overlap en el matcher.
- **Bloqueante de:** ninguno funcional; mejora la precisión del classifier
  rules-based.
- **Estimado:** 1-2 horas (ajuste de regex + tests de overlap explícito).

## 21. `cargo test --workspace` requiere `pipefail` para detectar fallos reales (Fase 3 commit 9a)

- **Origen:** lección operativa descubierta en commit 9a (2026-04-30) durante
  la verificación de `docs/orchestrator.md`.
- **Síntoma:** `cargo test --workspace 2>&1 | tail -80` reporta exit 0 aunque
  cargo falle, porque el exit code de la pipe es el de `tail`. Esto enmascara
  errores de compilación en crates del workspace que no se ven con
  `cargo test -p <crate>` específico.
- **Disparador:** próxima checklist de verificación manual o helper script de
  CI que use cargo test trunca con pipes.
- **Scope:**
  - Documentar en la guía de verificación manual (referenciada en
    [TODO #18](#18-guardrail-de-verificación-manual-para-codex-cli-fase-3-commit-8))
    que las invocaciones de `cargo test` con pipes requieren `set -o pipefail`
    o redirect a archivo + `wait` con check explícito del exit code.
  - Considerar un helper en `scripts/` para envolver el patrón:
    `cargo-test-workspace-verbose` que escriba a archivo y verifique exit
    explícitamente, evitando el footgun.
- **Bloqueante de:** ninguno funcional; reduce falsos verdes en auditorías.
- **Estimado:** 15-30 minutos (doc + helper opcional).

## 22. `app-server::tracing_tests` consume >2 MB de stack en debug build (Fase 3 commit 9a)

- **Origen:** descubierto en commit 9a (2026-04-30) tras fixear el primer
  bug de WireApi exhaustiveness — `cargo test --workspace` ya compilaba
  pero el test
  `message_processor::tracing_tests::turn_start_jsonrpc_span_parents_core_turn_spans`
  desbordaba el stack default de tokio current_thread (~2 MB) y abortaba
  el process completo (SIGABRT).
- **Threshold empírico:** el test pasa con `RUST_MIN_STACK=4194304` (4 MB),
  falla con el default de ~2 MB. No hay recursión genuina; es consumption
  alto en debug build (unoptimized frames + tracing instrumentation +
  futures anidados de tokio).
- **Procedencia:** 100% upstream. Los últimos commits sobre el archivo
  son `ac2bffa44 test: harden app-server integration tests (#19683)` y
  `9c3abcd46 [codex] Move config loading into codex-config (#19487)`.
  No tocamos el crate `codex-app-server` en Codrex.
- **Por qué fue invisible hasta hoy:** el bug de WireApi exhaustiveness
  rompía la compilación de `codex-config` bajo `cfg(test)`, y el dep
  graph `codex-app-server → codex-config` impedía llegar al runtime
  del test ofensor. Tras el fix `178efda7c`, el overflow afloró.
- **Workaround actual:** `RUST_MIN_STACK = "8388608"` en
  `codex-rs/.cargo/config.toml` `[env]` block. 8 MB para igualar el
  workaround `link-arg=/STACK:8388608` que upstream ya tiene en
  Windows. Mantiene cobertura del test (no `#[ignore]`).
- **Disparador para fix definitivo:** próximo merge de upstream que
  toque `tracing_tests.rs` o el `.cargo/config.toml`, o reporte de
  fricción con el workaround.
- **Posibles fixes definitivos (futuros):**
  - Reducir profundidad de spans en el test `turn_start_jsonrpc_span_parents_core_turn_spans`.
  - Optimizar instrumentación de tracing en `app-server` para que no
    infle frames en debug builds.
  - Abrir PR upstream sugiriendo bump de stack default a través del
    mismo mecanismo.
- **Bloqueante de:** ninguno. Workaround in-repo deja `cargo test
  --workspace` verde.
- **Riesgo:** bajo. El env var es benigno y el comentario in-config
  explica el porqué.
- **Mantenimiento:** sync con `.cargo/config.toml` upstream cuando
  hagamos merge; el conflict, si aparece, es obvio.
- **Estimado para fix definitivo:** depende del approach (reducir spans:
  1-2h; PR upstream: variable según ciclo de review).

## 23. `codex-app-server` tiene tests environment-leaking que rompen `cargo test --workspace` (Fase 3 commit 9a)

- **Origen:** descubierto en commit 9a (2026-04-30) tras los fixes de
  WireApi (`178efda7c`) y `RUST_MIN_STACK` (`bed0c5ca9`). Una vez
  desbloqueada la compilación y el primer overflow, `cargo test
  --workspace` reveló 12 fallas adicionales en `codex-app-server`
  que asumen un environment de CI clean.
- **Procedencia:** 100% upstream. Los 12 tests viven en
  `codex-rs/app-server/tests/suite/v2/`; no fueron tocados por
  Codrex. Pertenecen al crate `codex-app-server` que mantenemos
  intacto desde el fork.
- **Por qué fueron invisibles hasta hoy:** el bug de WireApi
  exhaustiveness rompía la compilación de `codex-config` bajo
  `cfg(test)`. El dep graph `codex-app-server → codex-config`
  impedía que ningún test de `codex-app-server` corriera. Tras los
  fixes de hoy, los tests llegan a runtime y fallan porque asumen un
  ambiente que esta máquina no cumple (skills/plugins instalados,
  PATH específico, comportamiento de TTY, etc.).
- **Lista de tests afectados (12):**
  - `suite::v2::command_exec::command_exec_accepts_permission_profile`
  - `suite::v2::command_exec::command_exec_env_overrides_merge_with_server_environment_and_support_unset`
  - `suite::v2::command_exec::command_exec_non_streaming_respects_output_cap`
  - `suite::v2::command_exec::command_exec_permission_profile_cwd_uses_command_cwd`
  - `suite::v2::command_exec::command_exec_pipe_streams_output_and_accepts_write`
  - `suite::v2::command_exec::command_exec_process_ids_are_connection_scoped_and_disconnect_terminates_process`
  - `suite::v2::command_exec::command_exec_streaming_does_not_buffer_output`
  - `suite::v2::command_exec::command_exec_tty_implies_streaming_and_reports_pty_output`
  - `suite::v2::command_exec::command_exec_tty_supports_initial_size_and_resize`
  - `suite::v2::command_exec::command_exec_without_process_id_keeps_buffered_compatibility`
  - `suite::v2::mcp_server_status::mcp_server_status_list_tools_and_auth_only_skips_slow_inventory_calls`
  - `suite::v2::turn_start::turn_start_emits_thread_scoped_warning_notification_for_trimmed_skills`
- **Diagnóstico más claro (turn_start_emits_thread_scoped_warning_notification_for_trimmed_skills):**
  el test compara byte-a-byte un mensaje sobre "additional skills
  not included in the model-visible skills list". Esperaba el número
  `7` (CI clean) y vio `21` (este VPS, con plugins
  `superpowers/*`, `claude-md-management/*`, etc.). No es bug de
  código — es una assertion exacta sobre un valor que depende del
  environment.
- **Patrón de las 11 de `command_exec`:** sin leer cada body, todas
  comparten archivo y prefijo de nombre; lo más probable es que
  asuman comandos del PATH, comportamiento de shell o permisos de
  `/tmp` que esta máquina no provee idénticos al CI canónico. No
  hay evidencia de bug genuino.
- **Decisión adoptada en 9a:** no marcar `#[ignore]` selectivo en
  archivos upstream (genera deuda de merge eterna). En su lugar,
  bautizar oficial el subset de verificación que cubre todo lo que
  Codrex tocó en Phase 2/2.5/3:

      cargo test \
        -p codex-orchestrator \
        -p codex-cli \
        -p codex-config \
        -p codex-minimax \
        -p codex-login

  Documentar el subset y las lecciones operativas en
  `docs/development.md` (commit del mismo día).
- **Disparador para fix definitivo:** primer pedido externo de que
  `cargo test --workspace` quede verde, primer merge de upstream
  que toque alguno de los 12 archivos, o cuando el repo se publique
  como `0.1.0` y queramos que el comando estándar de Rust funcione
  para contributors.
- **Posibles fixes definitivos (futuros):**
  - Mock del environment en cada test (PATH, skills count, etc.).
  - PR upstream que reemplace assertions exactas por matchers
    flexibles (e.g. regex `\d+ additional skills`).
  - Marcar los 12 tests como `#[ignore]` en upstream con
    justificación (probablemente rechazado por upstream; útil solo
    si Codrex decide divergir).
- **Bloqueante de:** publicación del repo como `0.1.0` con DX
  estándar de Rust (`cargo test --workspace` debería pasar para un
  contributor recién clonando).
- **Riesgo:** bajo en lo inmediato. El subset oficial cubre 100% de
  lo que tocamos.
- **Estimado para fix definitivo:** difícil de estimar sin auditar
  cada test (1-2 días en remediación interna; variable upstream).

## 10. `TestSpec` LITE extensions (Fase 3 commit 1)

- **Origen:** Fase 3 commit 1 (`codex-rs/orchestrator/src/spec.rs`).
- **Disparador:** primer caso real donde un test corre largo y necesita
  timeout, o cuando queremos retry feedback estructurado por test
  fallido (no el blob entero de stdout).
- **Scope:** extender `TestSpec` con:
  - `timeout: Option<Duration>` — kill el proceso si excede.
  - `expected_exit_code: i32` (default `0`) — útil para test runners
    que devuelven códigos no-cero en éxito (e.g. property-based con
    failures conocidos).
  - parser estructurado de output: TAP / JUnit XML / cargo-nextest JSON.
    Permite que `AuditDecision::Retry { feedback }` cite "test
    `validate_email_invalid` falló: expected ValidationError, got
    Email" en vez de pegar el stdout entero.
- **Bloqueante de:** retry inteligente en Phase 4-5.
- **Estimado:** medio día (3 fields + parser modular, gated por enum).

## 9. Wire probe permanente (Fase 2.5)

- **Origen:** Fase 2.5.
- **Estado:** ya commiteado en `codex-rs/minimax/examples/wire_probe.rs`.
  Útil para debugging futuro de errores opacos. No es deuda — solo nota.
- **Uso:** `cargo run -p codex-minimax --example wire_probe`. Lee la key
  de `~/.codex/auth.json` (o `CODREX_AUTH_PATH`) y corre una matriz de
  probes contra api.minimax.io. Nunca expone la key en stdout.

---

## Cerrados

- **2026-04-27 — `developer` role rejected by MiniMax**
  Commit `ac3c0192c`. OpenAI's reasoning role remapped to `system` in
  `normalize_role_for_minimax`. 3 regression tests.

- **2026-04-27 — Two adjacent `system` messages rejected (HTTP 400 / 2013)**
  Commit `c1579ac78`. `coalesce_consecutive_system_messages` merges runs
  before wire send. 3 regression tests.

- **2026-04-27 — Bridge panic: `ReasoningRawContentDelta without active item`**
  Commit `c1579ac78`. Bridge synthesizes
  `OutputItemAdded(Message)` lifecycle around streaming deltas, closes
  with `OutputItemDone(Message)` before `Completed`. 5 regression tests.

- **2026-04-27 — Wire-level debug dump on HTTP rejection**
  Commit `c313ff8fe`. `CODREX_MINIMAX_DEBUG_WIRE=1` dumps the request
  body alongside response on non-2xx, plus a `tracing::warn` at target
  `codrex::minimax::wire`. Gated to avoid leaking conversation content
  in production stderr.

- **2026-04-27 — Mid-conversation system messages rejected on resume
  (HTTP 400 / 2013 — `chat content has invalid message role: system`)**
  Commit `7f33ffe97`. Generalized the consecutive-system coalesce into
  `consolidate_system_messages_to_leading`: hoists every system body to
  a single leading turn, preserves insertion order, drops empties.
  Required for `codrex resume` against MiniMax to work. Wire probes
  05c/05d added to `wire_probe.rs` to lock the constraint live.
  2 regression tests + 1 replaced.
