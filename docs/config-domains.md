# Config Domains Taxonomy

AgentDesk has three distinct configuration domains. They share storage surfaces (YAML, `kv_meta`, JSON files) but have different owners, contracts, and audiences. Conflating them produces the kind of drift this taxonomy is meant to prevent.

This document is the operator-side index for the three domains. The precedence and persistence contract for each surface is owned by [`docs/adr-settings-precedence.md`](adr-settings-precedence.md); the canonical write paths are owned by [`docs/source-of-truth.md`](source-of-truth.md). This file links them by domain.

## The Three Domains

| Domain | Audience | Authoritative owner | Live override surface | Notes |
| --- | --- | --- | --- | --- |
| **runtime-config** | The running AgentDesk daemon (dispatch loops, queue, providers, kanban, retros) | `agentdesk.yaml` (`runtime:`, `automation:`, `review:`, `kanban:`, `database:`) | `kv_meta['runtime-config']` JSON | Live tuning surface for behavior knobs. YAML is restart baseline; `kv_meta` is the live override. Reset-on-restart is operator-controllable. |
| **dashboard** (company settings) | The dashboard UI and any caller that owns the merged company-settings JSON document | `kv_meta['settings']` | n/a — this surface IS the override | Full-replace contract. No YAML baseline. Hidden keys (e.g. `roomThemes`) survive only if callers re-merge them. |
| **bot-settings** | Discord bot bindings, agent roster, channel→agent routing, default per-bot behavior | `agentdesk.yaml` (`discord:`, `agents[].channels`) | None at runtime; defaults are read-only metadata derived from the YAML | Legacy `bot_settings.json` and `role_map.json` are migration inputs only. The dashboard "bot defaults" view is read-only over this YAML. |

A surface that doesn't fit one of the three domains (escalation override, onboarding/secrets) is a separate, dedicated store — see the surface matrix in `adr-settings-precedence.md`.

## Domain 1 — `runtime-config`

**Owner**: dashboard live-runtime controls + the running daemon.

**Authoritative writers**:

- **YAML baseline**: `~/.adk/release/config/agentdesk.yaml` under `runtime:`, `automation:`, `review:`, `kanban:`, `database:`. Edit this for restart-stable defaults.
- **Live override**: `kv_meta['runtime-config']` JSON. Written via `PUT /api/settings/runtime-config`. Cleared by `runtime.reset_overrides_on_restart=true`.
- **Hardcoded fallbacks**: ultimate defaults baked into the binary; only relevant when both YAML and override are silent.

**Read path** (precedence): hardcoded defaults → `agentdesk.yaml` → `kv_meta['runtime-config']`.

**Pitfalls**:

- Editing the dashboard does NOT update YAML — it sets a live override. To make a value the new restart baseline, also edit YAML.
- Read-only metadata such as `server_port` is exposed in this surface but is not writable.
- Per-key restart behavior (`reseed-from-yaml` vs `persist-live-override`) is reported by `/api/settings/config`. Trust that field, not local memory.

**Reference**: ADR section "Runtime config", `/api/settings/runtime-config` and `/api/settings/config` in `adr-settings-precedence.md`.

## Domain 2 — `dashboard` (company settings)

**Owner**: dashboard general settings UI.

**Authoritative writer**: `kv_meta['settings']` JSON document. Written via `PUT /api/settings`.

**Read path**: just the document. There is no YAML baseline.

**Contract**:

- **Full replace**, not patch. Callers must merge any keys they want preserved (the server cannot infer "keep `roomThemes`").
- The server strips retired legacy keys before storing.
- This domain is purely a UI-facing JSON blob; the daemon does not consume it for behavior tuning. Behavior tuning belongs in `runtime-config`.

**Pitfalls**:

- Treating `/api/settings` as a patch endpoint silently drops hidden keys.
- Adding behavior knobs here instead of `runtime-config` couples UI shape to daemon internals.

## Domain 3 — `bot-settings`

**Owner**: `agentdesk.yaml` `discord:` and `agents[].channels` blocks. Edited by operator, propagated by `scripts/deploy-release.sh`. Validated by `agentdesk config audit`.

**Authoritative writer**: `~/.adk/release/config/agentdesk.yaml`.

**Migration inputs (read-only)**:

- `~/.adk/release/config/role_map.json` — legacy channel→role map.
- `~/.adk/release/config/bot_settings.json` (and `.migrated` snapshots) — legacy default-bot table.
- DB rows mirroring agent/channel data during migration.

**Read path**: YAML wins. Legacy files and DB rows are migration inputs and are loaded only when YAML lacks an explicit value (see `discord_config_audit.rs`). The `.migrated` snapshots live under `~/.adk/release/config/.backups/YYYY-MM-DD/` per `source-of-truth.md`.

**Pitfalls**:

- Editing `bot_settings.json` directly: the change is ignored as soon as YAML re-asserts.
- Adding new bot defaults to JSON: the proper path is to add the key to YAML and let `discord_config_audit` validate.

## Cross-Domain Boundaries

- A new key MUST belong to exactly one domain. If unsure, lean toward `runtime-config` for behavior, `dashboard` for UI-only state, `bot-settings` for Discord/agent topology.
- A key must NOT appear in two YAML sections (e.g. both `runtime:` and `discord:`). Pick one home and let the consumer read from there.
- The dashboard live-override surface (`runtime-config`) is for the **daemon's** behavior. UI-only company toggles go into the `dashboard` domain. Discord/agent topology never goes into either of those — it lives in YAML.

## Code Map

For the runtime side, the relevant modules are:

- `src/services/discord_config_audit.rs` — bot-settings ingestion and YAML validation.
- `src/services/settings.rs` — runtime-config + dashboard surface plumbing.
- `src/services/mcp_config.rs` — MCP server declarations (a separate sub-vector under `agentdesk.yaml` documented in `source-of-truth.md`).
- `src/runtime_layout/paths.rs` — canonical paths used by all three domains.

## Related Documents

- [`docs/source-of-truth.md`](source-of-truth.md) — canonical file map per knowledge surface.
- [`docs/adr-settings-precedence.md`](adr-settings-precedence.md) — per-surface precedence and write-contract details.
- [`docs/memory-scope.md`](memory-scope.md) — Memento scope rules; explicitly forbids storing config-domain values in Memento.
