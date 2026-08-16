# Observe-Mode Exposure Options for the Execution Identity Fence

Source issue: #5399 (item 1 — "Observe 관측 노출"). Related: #5071 T3-A1 (#5398),
#5396 P2-7, #5411.

Last refreshed: 2026-08-17

> **Decision proposal, not an implementation.** It states the current exposure
> surface, compares three fixes, and recommends one. The PR that adds this
> document changes no code. The decision owner picks an option (or rejects all
> three) and files the implementation slice.

## Why This Blocks Something

[`docs/runbooks/execution-identity-promotion-criteria.md`](../runbooks/execution-identity-promotion-criteria.md)
defines the Legacy → Observe → Enforce promotion formula. Two clauses cannot be
evaluated on a default-configured dcserver: **P4 (sample floor)** needs the total
number of comparisons — the `Match` and `Unknown` outcomes, i.e. the denominator
— **and a per-`site` split of it**, because P4's floor is "≥ N observations, of
which ≥ M carry `site = tui_direct_stale_foreign_cancel`"; and **P5 (zero
condition)** needs `unknown == 0`, `Unknown` being exactly the outcome that
predicts an `Enforce` deny for a marker-absent session. Both are invisible. Until
an option below lands, an Observe window can only falsify the promotion, never
satisfy it.

## Current State

`execution_identity::OBSERVATION_COUNTERS` is a `OnceLock` static holding three
`AtomicU64` (`matched`, `mismatched`, `unknown`), written only by
`execution_identity::record_incarnation_observation`, whose only caller is
`execution_identity::destruction_permitted_under_identity`; recording is gated on
`ExecutionIdentityMode::records_identity_observations()`, so `Observe` and
`Enforce` count and `Legacy` does not. `execution_identity::observation_counts` —
the only reader — is `#[cfg(test)]`, so **there is no production readout: no
route, health field, log line, or CLI.** The counters are also process-local and
never persisted, so each restart rebases them to zero.

The logs: `Match` → `debug!` `execution_identity_nonce_match`; `Mismatch` →
`info!` `execution_identity_nonce_mismatch`; `Unknown` → `debug!`
`execution_identity_nonce_unknown`; target
`agentdesk::services::discord::tmux::execution_identity`, fields `site`
(`relay_recovery_dead_frontier_cancel` / `tui_direct_stale_foreign_cancel`) and
`session_key`. `logging::tracing_env_filter` adds an `agentdesk=info` directive
on top of `EnvFilter::from_default_env()`, so only the mismatch line ships.
**The counters carry no attribution and the visible log carries no denominator** —
that is the exact shape of the gap. Separately,
`WatcherIdentityFence::permits_pinned_binding` logs
`execution_identity_binding_mismatch` at `info!` on the
`agentdesk::services::discord::tmux_watcher_registry` target and moves no counter
at all; Options A and C do not cover that conjunct, B could.

Volume is low by construction: the counters move only when a decision reaches the
registry CAS, and both fenced sites are rare — see the promotion runbook's
[When Observations Actually Fire](../runbooks/execution-identity-promotion-criteria.md#when-observations-actually-fire).

## Option A — Promote `Match` and `Unknown` to `info!`

Change the two `tracing::debug!` calls in `record_incarnation_observation`;
nothing else moves. Gives the full outcome distribution under the shipped filter,
with `site` and `session_key` on every line, so a nonzero `unknown` names the
exact sessions to clear before an Enforce flip.

Costs: log volume at `info` — bounded by the rarity above, though unbounded in
principle if either site ever looped (neither does today;
`tui_direct_pending_start` is bounded by `PENDING_START_MAX_BACKSTOP_CYCLES` and
the relay-recovery site is operator-triggered). Counting means grepping a rotated
log (`logging::dcserver_log_max_bytes` / `dcserver_log_max_files`, default
100 MiB × 10), so a long window can outlive retention. The promotion is permanent
unless a follow-up demotes it. Does not cover the binding conjunct.

## Option B — Publish the counters on the health API

Make `observation_counts` non-test and surface the three totals through
`GET /api/health/detail` (`health_api::health_detail_handler` →
`health_response(&state, true)`).

Gives exact totals in one poll, no log-retention dependency, machine readable.
It is also the only option that can close the binding-conjunct gap, by adding a
counter `permits_pinned_binding` currently does not move.

**Required scope, not an optional extra: split the counters per `site`.**
`IncarnationObservationCounters` is three bare `AtomicU64` (`matched`,
`mismatched`, `unknown`) and `record_incarnation_observation` takes `site` only
to put it on the log line — it never reaches a counter. So a B that publishes
`observation_counts()` as it stands gives P4's **total** and none of its
`site = tui_direct_stale_foreign_cancel` floor, and P4 exists to reject exactly
the window a total-only readout cannot rule out: one met entirely by
operator-triggered `relay_recovery_dead_frontier_cancel` calls while the
automatically-firing site stayed unexercised. Per-site counters are therefore
inside B's scope if B is to satisfy P4 on its own; without them B is a partial
answer to P4 and no answer to P5.

Costs: the counters are process-local and non-durable, so the readout is
meaningless without a companion "counting since" epoch in the same object — a
restart otherwise resets numerator and denominator mid-window with no way for the
reader to tell. No per-session attribution at any granularity:
`tui_direct_stale_foreign_cancel: {unknown: 3}` still does not say which three
sessions, which is what P5's follow-up needs, so B alone sends the operator back
to A's logs.
Widest surface: `/api/health/detail` is a control endpoint
(`local_or_configured_control_endpoint_allowed`) and a new field there is a
compatibility commitment — a *new route* would additionally need registering in
`src/server/routes/docs.rs` for `scripts/check_api_docs_coverage.py`, while a
field on the existing route does not trip that gate. And dropping `#[cfg(test)]`
makes the counters a production surface later work must keep meaningful.

## Option C — Document a log-filter escape hatch

No code change; document that an operator raises the module to `debug` for the
window.

Zero implementation cost and no permanent surface — but the mechanism is
unverified. `logging::tracing_env_filter` calls `EnvFilter::from_default_env()`
and then `add_directive("agentdesk=info")`, so a blanket
`RUST_LOG=agentdesk=debug` is re-pinned to `info`. A module-scoped directive
(`RUST_LOG=agentdesk::services::discord::tmux::execution_identity=debug`) is more
specific and should survive, but **no test in this repository proves that
precedence**, and it rests on `tracing-subscriber` internals rather than AgentDesk
code. `logging::init_dcserver_tracing` also builds the filter once behind a
`OnceLock`, so this is a restart-scoped env change that cannot be flipped on a
running dcserver the way `execution_identity_mode` can — and the evidence then
depends on an operator having set it correctly for the whole window, with no
after-the-fact way to detect that they did not.

## Comparison

| | A — `info` | B — health readout | C — filter guidance |
|---|---|---|---|
| Gives a denominator | yes | yes | if the directive works |
| P4's per-`site` floor | yes (`site` on every line) | only if B also splits the counters per `site` | if the directive works |
| Per-session attribution | yes | no | yes |
| Survives a restart | yes (on-disk log) | no (needs an epoch field) | yes |
| Covers the binding conjunct | no | possible | no |
| Code change | 2 lines | field + readout plumbing | none |
| Restart to enable | no | no | yes |
| Permanent new surface | log volume | API field | none |
| Verified against this repo | yes | yes | **no** |

## Recommendation

**Option A.**

The formula's binding constraint is not "how many" but "which". P5 requires
`unknown == 0`, and every `unknown` is a specific marker-absent session the
[Enforce rollout runbook](../runbooks/execution-identity-enforce-rollout.md) must
clear by name. Only the log lines carry `site` and `session_key`. So Option B as
its counters stand today satisfies neither clause outright: it gives P4's total
but not P4's per-`site` floor, and it gives P5 no way to name the sessions it
must clear. B closes P4 only if per-site counters are built with it, and it would
in practice be paired with A regardless. A is also the only option whose cost is bounded by something
already verified — the two sites are rare, and the event that would add an `info`
line is precisely the event the rollout exists to observe. C is excluded because
its central mechanism is unverified here and because a restart-scoped env var is
the wrong control for a window the mode switch itself starts and stops without a
restart. If a restart-independent total is also wanted, B is a reasonable
follow-up — but not without the counter-epoch field, and it does not remove the
need for A.

Out of scope for the resulting slice: splitting the counters per `site` or adding
one for the binding conjunct. Not because the formula does not want the split —
P4 requires it — but because A satisfies P4 from the log line's own `site` field
without touching a counter; the split becomes mandatory only for a B that is
meant to stand alone. Also out of scope: making the counters durable or
cluster-aggregated, and any change to
what the fence decides — every option here is observation-only, and
`ExecutionIdentityMode::denies_on_incarnation_mismatch` is untouched.
