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
