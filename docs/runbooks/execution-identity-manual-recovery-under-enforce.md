# Manual Relay Recovery Under Enforce

Source issue: #5399 (item 4 — "`RelayRecoveryApplySource::Manual` fence 적용
문면"). Related: #5071 T3-A1 (#5398); design `design-t3t4-final.md` §5.1,
referenced from that PR's description.

Last refreshed: 2026-08-17

## The Apparent Contradiction

The T3/T4 design's §5.1 says the identity fence has **no effect on operator
paths**. The code says `RelayRecoveryApplySource::Manual` — the operator API —
reaches a fenced call site and is refused under `Enforce`. Both are true, about
different sets of "operator path". This document fixes which is true in which
scope, so a future reader does not resolve the tension by weakening either one.

## True: §5.1's Scope Is Process/Session Reset

`commands::control::reset_managed_process_session` (removes and terminates the
process session handle — or `process::kill_pid_tree` on a lingering pid — records
a tmux exit reason, `platform::tmux::kill_session`, then
`tmux_common::cleanup_session_temp_files`) and its hard-reset sibling
`recreate_tmux_session` never touch the fence, in any mode. Neither calls
`TmuxWatcherRegistry::under_identity_fence`, and neither appears in the
`registry_remove` category of `scripts/destructive_call_site_baseline.json` —
they are `tmux_kill`/`process_kill` sites. An operator who resets a session has
the same authority under `Enforce` as under `Legacy`. This is also why the reset
family is the recommended lever in the
[Enforce rollout runbook](execution-identity-enforce-rollout.md#3-clear-every-absent-session-before-the-flip):
it is the only operator action that both clears a stuck session and causes the
next spawn to mint a fresh `.spawn_nonce`.

## Also True: The Manual Apply Source Is Fenced

`relay_recovery::apply::apply_relay_recovery_decision` captures a
`WatcherIdentityFence` (`DEAD_FRONTIER_CANCEL_IDENTITY_SITE`) and routes the
dead-frontier destructive cancel through
`under_identity_fence(...).with_terminal_delivery_fence(...).cancel_and_remove_channel_if_current(...)`
(the delivery binder is required since #5071 relay-tail S4 r2). Under
`Enforce`, an absent or changed `.spawn_nonce` denies that removal.

That branch is guarded by `episode.is_none()`, and **only Manual can satisfy it**
for the `ReattachWatcher` action:

- `relay_recovery_auto_heal_apply::apply_relay_recovery_plan_with_seams` calls
  `circuit_breaker::should_use_durable_circuit(action, source)`, which is
  `action == ReattachWatcher && source != Manual`.
- For `ProbeAutoHeal` and `StallWatchdog` that is true, so an episode reservation
  runs and **every** non-`Reserved` outcome (`Open`, `StaleIdentity`,
  `MissingInflight`, `IoError`) returns early with `skipped: true` before
  `apply_relay_recovery_decision` is reached. A `Reserved` outcome passes
  `Some(episode)`, failing the `episode.is_none()` guard.
- For `Manual` the predicate is false, no episode is reserved, and
  `reserved_episode` stays `None`.

So the fenced dead-frontier cancel is **exclusively operator-triggered**. Its
entry point is `POST /api/channels/{id}/relay-recovery`
(`health_api::relay_recovery_handler` → `health::recovery::handle_relay_recovery`
→ `relay_recovery::run_relay_recovery`, which hard-codes
`RelayRecoveryApplySource::Manual`) with `apply` set to `true`.

### How to actually set `apply`

**`apply` is a field of the JSON request body. It is not a query parameter.**
`relay_recovery_handler` takes `Path(channel_id)` and `body: Bytes` and extracts
no `Query` at all: an empty body becomes `RelayRecoveryRequest::default()`, and a
non-empty one goes through `serde_json::from_str::<RelayRecoveryRequest>`, whose
shape is `{ provider: Option<String>, apply: bool }` with `#[serde(default)]` on
`apply`. So a missing body, an empty body, a body without the field, and
`{"apply": false}` all mean the same thing — **dry run** — and a `?apply=true`
query string is silently ignored, leaving the call a dry run that applies
nothing. A malformed body is a `400 invalid request: …` rather than a dry run.

```bash
API="http://127.0.0.1:<config.server.port>/api/channels/<channel_id>/relay-recovery"

# Dry run — plan + evidence only. run_relay_recovery returns
# {"mode":"dry_run","applied":false,…} before any fence is captured.
curl -sS -X POST "$API" -H 'Content-Type: application/json' -d '{"apply":false}'

# Apply — the ONLY form that reaches the fenced dead-frontier cancel.
curl -sS -X POST "$API" -H 'Content-Type: application/json' -d '{"apply":true}'

# Optional provider filter (same field set, either mode).
curl -sS -X POST "$API" -H 'Content-Type: application/json' \
  -d '{"provider":"codex","apply":true}'
```

Tell the two apart in the response, not in the request you think you sent: only
an apply leaves the `!apply` early return in `run_relay_recovery`, so a body that
comes back `"mode":"dry_run"` with `"applied":false` did **not** run the fenced
path regardless of the URL. The route is a control endpoint
(`local_or_configured_control_endpoint_allowed`) — a non-loopback caller with no
`server.auth_token` configured gets `403 auth_token required for non-loopback
host`, which is also not a dry run.

The design's vocabulary explains the collision: §5.1 described operator **reset**
authority, while T3-A1 fenced two **automatic watcher-registry removals** — and
one of those is reachable only from an operator API. "Automatic" there means "not
a human-authored cancel decision": the removal is planned by
`plan_relay_recovery` from health evidence, and the operator only authorizes the
plan. The tension is a naming overlap, not a behavioural surprise.

## Resolution

| Claim | Scope in which it is true |
|---|---|
| "operator 경로 영향 없음" (§5.1) | Process/tmux reset: `reset_managed_process_session`, `recreate_tmux_session`, and every `tmux_kill`/`process_kill` site. Unfenced in all modes. |
| "Manual is refused under Enforce" | The dead-frontier destructive watcher cancel in `relay_recovery::apply`, reachable only via `POST /api/channels/{id}/relay-recovery` with a JSON body of `{"apply": true}`. Fenced; denies on a non-`Match` nonce. |

Neither generalizes. In particular:

- `POST /api/inflight/rebind` (`health_api::rebind_inflight_handler` →
  `recovery_engine::rebind_inflight_for_channel`) is a separate operator path
  with its own `registry_remove` entry and **no** fence. `Enforce` does not
  affect it.
- The idle-tmux-reattach branch of the same `ReattachWatcher` arm — also
  Manual-only, since it too requires `episode.is_none()` — performs
  `shared.tmux_watchers.remove(&channel)` unfenced. Even within one Manual apply,
  only one of the two destructive branches is fenced.

## What a Denied Manual Apply Looks Like

The apply does not fail. `cancel_and_remove_channel_if_current` returns `false`,
which logs `relay recovery skipped finalizer after committed cancel; expected
watcher was not current` at `warn`, skips
`finalize_cancelled_watcher_owner_turn` and
`inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence`,
and falls through to `reattach_apply::apply_rebind`. The HTTP response therefore
reports the **rebind** outcome — there is no dedicated status string for "denied
by the identity fence".

To tell a fence deny from an ordinary pointer/binding miss, correlate with the
log: only a fence deny is accompanied by an `execution_identity_nonce_mismatch`
(`info`) line for `site = relay_recovery_dead_frontier_cancel`, or — for the
marker-absent case — an `execution_identity_nonce_unknown` line, which is `debug`
and therefore **invisible under the default `agentdesk=info` filter**. A
marker-absent deny is silent today; see
[`docs/design/5399-observe-exposure-options.md`](../design/5399-observe-exposure-options.md).

Nothing is half-applied: `inflight::commit_destructive_cancel_locked` runs before
the CAS and, after T3-A1, its callback is pure — it verifies identity,
`updated_at` and `save_generation` and returns an outcome without writing the
row, so the inflight row, the watcher registration and `cancel` are all unchanged
by a denied apply.

## Recovering Manually While Enforce Is Live

**1. Prefer the unfenced operator paths.** If the goal is to unstick a channel
rather than to run the dead-frontier cancel specifically, use a path the fence
does not touch: `POST /api/inflight/rebind` for a rebind/adoption, or the session
reset family for a hard reset — which additionally re-mints `.spawn_nonce` on the
next spawn. This is the recommended route: no config change, no window with the
fence off.

**2. Diagnose with a dry run first.** `apply` defaults to `false` — an omitted
body is enough — and the call returns `mode: "dry_run"`, `applied: false`, with
the planned action plus evidence (see
[How to actually set `apply`](#how-to-actually-set-apply); the field is in the
JSON body, so a `?apply=true` URL also lands here). Use it to confirm the
decision really is the dead-frontier shape —
`relay_recovery::relay_frontier_dead_reattach_owner` requires
`RelayStallState::TmuxAliveRelayDead`, `desynced`, `tmux_alive == Some(true)`,
`watcher_attached`, `watcher_owns_live_relay`, and `last_relay_offset == 0` —
before concluding that the fence is what is in the way.

**3. Temporarily demote the switch.** `execution_identity_mode` reads
`config_live_reload::current()` per decision, so: set
`runtime.execution_identity_mode: observe` in the live `agentdesk.yaml` (no
restart); run the Manual apply, which under `Observe` records but never denies;
set it back to `enforce`. This is a real hole in the guarantee for the duration,
and it is deliberate — the switch is a rollout control, not a security boundary.
Log the window bounds and affected channel, and prefer option 1 whenever it
achieves the same outcome.

**4. What is not available.** There is no per-request fence override, no `force`
parameter on the endpoint, and no way to write a `.spawn_nonce` for a live
session outside a provider spawn. Do not fabricate a marker by hand — a
hand-written nonce makes the fence certify a spawn that never happened.

## Sign-Off

- Author: #5399 item 4, 2026-08-17 — interpretation recorded; no code or design
  change proposed.
- Design-doc owner: pending — §5.1's wording is left as-is. A future revision
  should narrow "operator 경로" to "process/session reset" rather than delete the
  claim.
