# ADR: Settings Precedence And Persistence

Status: accepted
Date: 2026-04-11

## Context

AgentDesk settings already span multiple storage surfaces:

- `agentdesk.yaml`
- `kv_meta['settings']`
- `kv_meta['runtime-config']`
- whitelisted individual `kv_meta` keys
- onboarding-specific keys and flows

The problem is not that there are multiple stores. The problem is that operators and dashboard code need an explicit contract for each surface:

- who owns the canonical write path
- which baseline wins on restart
- whether a live override persists or gets re-seeded
- whether the surface is full-replace, patch, or derived metadata

Trying to force everything into one physical store would hide these differences and push risk into ad hoc merge logic.

## Decision

We keep per-surface storage, but every settings surface must declare:

1. canonical owner
2. precedence chain
3. persistence and restart semantics
4. write contract
5. whether the surface is editable or read-only metadata

## Surface Matrix

| Surface | Canonical owner | Precedence / baseline | Persistence / restart | Write contract |
| --- | --- | --- | --- | --- |
| Company settings JSON | Dashboard general settings UI and any caller that owns the merged JSON document | No YAML baseline. `kv_meta['settings']` is the canonical document. | Persists until explicitly replaced. Restart does not rebuild it from YAML. | `PUT /api/settings` is full replace. Callers must merge hidden keys themselves. |
| Runtime config | Dashboard live-runtime controls | hardcoded defaults -> `agentdesk.yaml runtime:` -> `kv_meta['runtime-config']` override JSON | Explicit live overrides apply immediately and survive restart ahead of YAML unless `runtime.reset_overrides_on_restart=true`. Non-explicit saved values rebase onto YAML/defaults. | `PUT /api/settings/runtime-config` fully replaces the object. Supplied `__runtimeConfigExplicitKeys` metadata is authoritative, including an empty list; without it, every known body key is explicit. `GET` returns `current` + `defaults` + `explicit_keys`. |
| Policy/config keys | Dashboard policy controls and automation helpers | hardcoded defaults -> YAML sections (`review:`, `runtime:`, `kanban:`) -> individual `kv_meta` rows | YAML-backed keys are re-seeded on restart. Hardcoded-only keys keep their DB override unless the reset flag is on. Read-only entries are surfaced as config metadata only. | `PATCH /api/settings/config` writes editable keys only. `GET` returns effective value plus baseline metadata. |
| Onboarding / secrets | Onboarding wizard | Dedicated onboarding keys and flows, not general settings | Persist until onboarding updates them. | Managed only through `/api/onboarding/*` and onboarding-specific helpers. |

### Runtime-config keys resolved in Rust

<!-- BEGIN GENERATED: runtime setting precedence -->
<!-- Generated from the key table in src/services/settings/resolve; regenerate: ADK_WRITE_SETTINGS_PRECEDENCE_DOC=1 cargo test --lib services::settings::resolve:: -->

Rust readers resolve these `runtime-config` keys with `settings::resolve`: the persisted value when it outranks YAML, else the node's YAML snapshot, else the default.

| `runtime-config` field | YAML field | Default | Persisted value outranks YAML when |
|---|---|---|---|
| `dispatchRateLimitGateEnabled` | `runtime.dispatch_rate_limit_gate_enabled` | `true` | it is listed in `__runtimeConfigExplicitKeys` or differs from the default |
| `dispatchRateLimitGateDangerPct` | `runtime.dispatch_rate_limit_gate_danger_pct` | `100` | it is listed in `__runtimeConfigExplicitKeys` or differs from the default |
| `rateLimitStaleSec` | `runtime.rate_limit_stale_sec` | `600` | it is listed in `__runtimeConfigExplicitKeys` or differs from the default |
| `maxEntryRetries` | `runtime.max_entry_retries` | `3` | it has the expected JSON type |
<!-- END GENERATED: runtime setting precedence -->

Policy `agentdesk.config.get` reads only the persisted layer: a legacy scalar `kv_meta` row, else the raw `runtime-config` field, else `null`. It has no YAML or default layer, so each policy applies its own fallback.

## Seeding Ownership

- Only the node that owns shared configuration sync seeds YAML into `kv_meta` at boot: a node with `cluster.enabled: false`, or the `cluster.role: hub` node (`shared_config_sync_enabled`). It writes the config-key seeds, `server_port`, the `runtime-config` blob and the repo and agent rows (`startup_reseed`).
- Runner and auto nodes never seed. Their `startup_reseed` fails until `server_port` and `runtime-config` exist, so the kv layer every node resolves against is the one the hub seeded.
- The YAML layer stays per node: each node resolves against its own YAML snapshot. A hub-seeded value that outranks YAML under the key's rule therefore applies on every node, while a seeded gate default defers to each node's own YAML.

## Representative Contracts

### `/api/settings`

- Canonical store: `kv_meta['settings']`
- No YAML baseline
- Full-replace contract
- Server strips retired legacy keys before storing
- Callers must re-send merged JSON if they want hidden keys such as `roomThemes` to survive

### `/api/settings/runtime-config`

- `GET` returns:
  - `defaults`: hardcoded + YAML baseline
  - `current`: defaults with saved override merged on top
- `PUT` fully replaces the stored override object. When `__runtimeConfigExplicitKeys` is present, the server honors that exact known-key list, including an intentionally empty list from the dashboard. When metadata is absent (for example, CLI or plain REST), every known config key present in the body is recorded as an explicit override; unknown JSON fields never gain override authority.
- `agentdesk config get` includes the reserved metadata in its current-value object so piping that JSON back to `agentdesk config set` preserves the existing `explicit_keys` list.
- This is a live-tuning surface, not a durable YAML editor
- Rust readers resolve a key through `settings::resolve`, which returns the value with its `source` (`kv_override`, `yaml` or `default`); `/api/health/detail` shows `enabled_source` and `danger_pct_source` in `rate_limit_dispatch_gate`.

### `/api/settings/config`

- `GET` returns per-key metadata:
  - `value`: effective current value
  - `baseline`: restart baseline for the key
  - `baseline_source`: `yaml`, `hardcoded`, or `config`
  - `override_active`: whether the effective value currently differs from baseline
  - `editable`: whether the API accepts writes for this key
  - `restart_behavior`: machine-readable restart rule such as `reseed-from-yaml` or `persist-live-override`
- `PATCH` writes only editable whitelisted keys
- `server_port` is intentionally read-only metadata even though it is visible in this surface

## Operator Rules

- Changing a YAML-backed key from the dashboard is a live override, not a YAML edit.
- If a key reports `restart_behavior = reseed-from-yaml`, the dashboard value is temporary until reboot.
- If a key reports `restart_behavior = persist-live-override`, the DB override is the restart value until someone changes it or enables reset-on-restart.
- Read-only metadata such as `server_port` must not be exposed as a writable live setting.

## Local Verification Notes

Representative local checks backing this ADR:

- `/api/settings` replaces the stored JSON document instead of patch-merging it.
- `/api/settings/config` distinguishes YAML-backed keys from hardcoded-only keys, and now exposes restart metadata for that difference.
- `/api/settings/runtime-config` returns merged `current` values over `defaults`, confirming the baseline/override split already used by the dashboard.

## Follow-up Boundary

Any new settings surface should not be added to the general settings UI until it declares:

- canonical owner
- precedence chain
- restart semantics
- mutability (`editable` vs read-only metadata)
- API contract shape
