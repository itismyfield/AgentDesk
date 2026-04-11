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
- dedicated escalation override storage

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
| Runtime config | Dashboard live-runtime controls | hardcoded defaults -> `agentdesk.yaml runtime:` -> `kv_meta['runtime-config']` override JSON | Live override applies immediately. On restart, YAML-backed keys are re-applied. Saved keys without YAML baselines persist unless `runtime.reset_overrides_on_restart=true`, in which case the whole surface resets to baseline. | `PUT /api/settings/runtime-config` replaces the override object. `GET` returns `current` + `defaults`. |
| Policy/config keys | Dashboard policy controls and automation helpers | hardcoded defaults -> YAML sections (`review:`, `runtime:`, `automation:`, `kanban:`) -> individual `kv_meta` rows | YAML-backed keys are re-seeded on restart. Hardcoded-only keys keep their DB override unless the reset flag is on. Read-only entries are surfaced as config metadata only. | `PATCH /api/settings/config` writes editable keys only. `GET` returns effective value plus baseline metadata. |
| Escalation routing | Dashboard escalation panel and Discord `!escalation` command | `escalation:` config baseline plus fallback owner/channel defaults, overridden by `kv_meta['escalation-settings-override']` | Override persists until changed back to defaults. When `runtime.reset_overrides_on_restart=true`, the stored override is cleared on boot. | `PUT /api/settings/escalation` replaces the override. Sending the default body clears the stored override. |
| Onboarding / secrets | Onboarding wizard | Dedicated onboarding keys and flows, not general settings | Persist until onboarding updates them. | Managed only through `/api/onboarding/*` and onboarding-specific helpers. |

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
- `PUT` stores the override object and mirrors scalar keys for runtime consumers
- This is a live-tuning surface, not a durable YAML editor

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

### `/api/settings/escalation`

- `GET` returns `current` and `defaults`
- Defaults come from config, not from dashboard state
- The override is a dedicated JSON document rather than a collection of unrelated `kv_meta` keys

## Operator Rules

- Changing a YAML-backed key from the dashboard is a live override, not a YAML edit.
- If a key reports `restart_behavior = reseed-from-yaml`, the dashboard value is temporary until reboot.
- If a key reports `restart_behavior = persist-live-override`, the DB override is the restart value until someone changes it or enables reset-on-restart.
- Read-only metadata such as `server_port` must not be exposed as a writable live setting.

## Local Verification Notes

Representative local checks backing this ADR:

- `/api/settings` replaces the stored JSON document instead of patch-merging it.
- `/api/settings/config` distinguishes YAML-backed keys like `merge_strategy` from hardcoded-only keys like `max_review_rounds`, and now exposes restart metadata for that difference.
- `/api/settings/runtime-config` returns merged `current` values over `defaults`, confirming the baseline/override split already used by the dashboard.

## Follow-up Boundary

Any new settings surface should not be added to the general settings UI until it declares:

- canonical owner
- precedence chain
- restart semantics
- mutability (`editable` vs read-only metadata)
- API contract shape
