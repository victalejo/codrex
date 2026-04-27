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

## Cerrados

_(vacío al cierre de Fase 2.5)_
