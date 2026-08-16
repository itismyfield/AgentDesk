# Execution Identity Fence — Legacy → Observe → Enforce Promotion Criteria

Source issue: #5399 (item 2 — "Enforce 승격 판정식 명문화"). Related: #5071 T3-A1
(#5398) landed the fence; #5411 made `Legacy` skip the marker read.

Last refreshed: 2026-08-17

> **Every threshold in [Promotion Formula](#promotion-formula) is a PROPOSAL.
> 운영자 확정 필요 — nothing here is ratified, and this is not an instruction to
> change production today.** The non-numeric parts are read off the code.

## Scope

`runtime.execution_identity_mode` (`config::ExecutionIdentityMode`) is the
three-stage rollout switch for the #5071 T3 execution-identity fence. This
runbook covers the **Legacy → Observe → Enforce** promotion decision only; the
cutover is [`execution-identity-enforce-rollout.md`](execution-identity-enforce-rollout.md)
and the operator-API tension is
[`execution-identity-manual-recovery-under-enforce.md`](execution-identity-manual-recovery-under-enforce.md).
`tmux_watcher_registry::execution_identity_mode` resolves
`config_live_reload::current()` per decision and falls back to the compiled-in
`Legacy`, so **a mode change applies without a dcserver restart**, at the next
fenced decision.

## What Observe Actually Records

This is the inventory the formula may depend on. Nothing outside it exists as a
rollout signal today.

Two call sites consult the fence — both capture a `WatcherIdentityFence` and pass
it to `TmuxWatcherRegistry::under_identity_fence`:

| `site` label | Call site | Registry helper |
|---|---|---|
| `relay_recovery_dead_frontier_cancel` | `relay_recovery::apply` (`DEAD_FRONTIER_CANCEL_IDENTITY_SITE`) | `cancel_and_remove_channel_if_current` |
| `tui_direct_stale_foreign_cancel` | `tui_direct_pending_start::submit_stale_foreign_inflight_cancel` (`STALE_FOREIGN_CANCEL_IDENTITY_SITE`) | `remove_tmux_session_if_current` |

`execution_identity::record_incarnation_observation` is the only writer of the
three `AtomicU64` in `OBSERVATION_COUNTERS`; its only caller is
`execution_identity::destruction_permitted_under_identity`. It is a no-op unless
`ExecutionIdentityMode::records_identity_observations()` — true for `Observe` and
`Enforce`, false for `Legacy`.

| Outcome | Counter | Log level | `counter` field |
|---|---|---|---|
| `Match` | `matched` | `debug!` | `execution_identity_nonce_match` |
| `Mismatch` | `mismatched` | `info!` | `execution_identity_nonce_mismatch` |
| `Unknown` | `unknown` | `debug!` | `execution_identity_nonce_unknown` |

All three carry `site` and `session_key` on the module's default target,
`agentdesk::services::discord::tmux::execution_identity` (mounted under `tmux` by
a `#[path]` declaration in `services::discord::tmux`).

Four constraints on that evidence:

1. **No production readout of the counters.**
   `execution_identity::observation_counts` is `#[cfg(test)]` — no route, health
   field, log line, or CLI prints the totals in a release build. #5399 item 1
   tracks the exposure decision,
   [`docs/design/5399-observe-exposure-options.md`](../design/5399-observe-exposure-options.md).
2. **`Match` and `Unknown` are invisible under the default filter.**
   `logging::tracing_env_filter` adds an `agentdesk=info` directive on top of
   `EnvFilter::from_default_env()`, so only the mismatch line survives. **The
   denominator is not observable today** — see [Measuring It](#measuring-it).
3. **The counters are process-local and non-durable.** `OBSERVATION_COUNTERS` is
   a `OnceLock` static: never persisted, never reset by an API, zero in every new
   process. A restart rebases numerator and denominator, and a multi-runtime or
   multi-node deployment holds one set per process.
4. **The binding conjunct has no counter.**
   `WatcherIdentityFence::permits_pinned_binding` logs
   `execution_identity_binding_mismatch` at `info!` on the
   `agentdesk::services::discord::tmux_watcher_registry` target and moves no
   `AtomicU64`. Only `tui_direct_pending_start` calls `with_pinned_binding`;
   `relay_recovery::apply` pins nothing, because
   `cancel_and_remove_channel_if_current` already compares channel binding,
   session name and output path in every mode.

`Observe` never denies (`denies_on_incarnation_mismatch()` is false), so every
destructive outcome under `Observe` is identical to `Legacy`. The only runtime
differences are the two marker reads (`consults_spawn_nonce()` becomes true) and
the counters/logs above. `Observe` restores nothing: the two #5067 in-flight
emission fences T3-A1 deleted are gone in every mode.

## When Observations Actually Fire

Both sites are rare by construction — the dominant risk to any sample floor.
Counters move only when a decision reaches the registry CAS, since
`destruction_permitted_under_identity` runs inside
`WatcherIdentityFence::permits_destruction`, called only from the two locked CAS
cores. A fence captured and then abandoned before the CAS (gate denial, mailbox
episode change, failed pin commit) reads the marker and records **nothing**.

- `relay_recovery_dead_frontier_cancel` needs
  `relay_recovery::relay_frontier_dead_reattach_owner` to return `Some`
  (`RelayStallState::TmuxAliveRelayDead`, `desynced`, `tmux_alive == Some(true)`,
  `watcher_attached`, `watcher_owns_live_relay`, `last_relay_offset == 0`) and
  `episode.is_none()`, which restricts it to `RelayRecoveryApplySource::Manual` —
  an operator API call. **This site does not fire on its own.**
- `tui_direct_stale_foreign_cancel` fires from the
  `WaitOutcome::BackstopForeignInflightLive` arm of the
  `tui_direct_pending_start` worker, after `destructive_cancel_gate::evaluate`
  allows it and the #4020 positive-stale age gate
  (`STALE_FOREIGN_INFLIGHT_MIN_AGE_SECS`) has passed.

So a calendar-only window can end with a **zero-sample** counter set, trivially
satisfying "mismatch == 0" while proving nothing. The formula therefore requires
a positive sample floor and treats a zero-sample window as NO-GO.

## Promotion Formula

Promote only when **every** clause holds. P1–P2 are structural; P3–P6 carry the
proposed numbers.

**P1 — Observe is actually running.** `execution_identity_mode: observe` is live
for every dcserver process in scope, each up continuously for the whole window.
Any restart inside the window resets the counters and restarts the window
(constraint 3).

**P2 — The observation surface is readable.** The Match/Unknown denominator is
obtainable for the whole window. Until #5399 item 1 lands, P2 is **unmet by
construction** on a default-configured host; see [Measuring It](#measuring-it).

**P3 — Observation window.** _Proposal: 7 full KST days_ — 운영자 확정 필요. The
shape (not the number) matches the sibling criterion in
[`dispatch-delivery-cutover-rollback.md`](dispatch-delivery-cutover-rollback.md);
nothing about this fence argues for a different unit.

**P4 — Sample floor.** _Proposal: ≥ 20 recorded observations, of which ≥ 5 carry
`site = tui_direct_stale_foreign_cancel`_ — 운영자 확정 필요. The total guards
against the zero-sample trap; the per-site minimum is needed because
`relay_recovery_dead_frontier_cancel` only fires on operator traffic, so a
total-only floor could be met entirely by deliberate API calls while the one
automatically-firing site stayed unexercised. If the floor is not reached
organically, extend the window or manufacture samples on the dedicated E2E
channels (see
[`post-deploy-relay-continuity-smoke.md`](post-deploy-relay-continuity-smoke.md)),
marking them as such at sign-off — they prove the fence is wired, not that
production traffic is clean.

**P5 — Mismatch condition.** `mismatched == 0` **and** `unknown == 0` **and**
zero `execution_identity_binding_mismatch` lines for the window.

`unknown` belongs in the zero condition, not an advisory: `Enforce` permits only
`IncarnationObservation::Match`, so a nonzero `unknown` under Observe is an exact
prediction of a deny. Each one is a marker-absent session, and clearing it is the
[Enforce rollout runbook](execution-identity-enforce-rollout.md)'s pre-flight
work. _Proposal: strict zero with no per-event justification escape_ — 운영자
확정 필요. The sibling dispatch runbook allows "tied to a concrete id and
explicitly justified"; that escape is deliberately **not** proposed here, because
a justified mismatch under Observe still becomes an unconditional deny under
Enforce.

**P6 — Sign-off.** One reviewer sign-off recording the window bounds, the
observed totals, and the measurement mechanism used.

## Measuring It

Preferred: whatever readout #5399 item 1 lands — provided it carries `site`. P4's
floor is per-site, and the three `OBSERVATION_COUNTERS` totals do not carry
attribution (`record_incarnation_observation` passes `site` to the log line only),
so a readout of those totals alone gives P4's denominator and not its
`tui_direct_stale_foreign_cancel` minimum. Until then:

**Interim A — mismatch-only, from the dcserver log.** Only `info` lines survive
the default filter, so this yields a numerator with per-session attribution and
**no denominator**: it can falsify P5 but cannot satisfy P4. The log is
`$RUNTIME_ROOT/logs/dcserver.stdout.log`
(`cli::dcserver::dcserver_stdout_log_path`; `$RUNTIME_ROOT` is
`$AGENTDESK_ROOT_DIR` or `~/.adk/release`, per `config::runtime_root`).

```bash
ADK_LOG="${AGENTDESK_ROOT_DIR:-$HOME/.adk/release}/logs/dcserver.stdout.log"
grep -c 'execution_identity_nonce_mismatch'   "$ADK_LOG"   # both sites
grep -c 'execution_identity_binding_mismatch' "$ADK_LOG"   # tui_direct only
grep -E 'execution_identity_(nonce|binding)_mismatch' "$ADK_LOG"  # attribution
```

`logging::dcserver_log_max_bytes` / `dcserver_log_max_files` default to 100 MiB ×
10 files (`AGENTDESK_DCSERVER_LOG_MAX_BYTES` / `AGENTDESK_DCSERVER_LOG_MAX_FILES`
override). A 7-day window on a busy host can outlive that retention — confirm the
oldest retained rotation predates the window start before trusting a zero, and
widen the grep to the rotated siblings.

**Interim B — raising the module to `debug`.** `tracing_env_filter` calls
`EnvFilter::from_default_env()` and then `add_directive("agentdesk=info")`, so a
blanket `RUST_LOG=agentdesk=debug` is re-pinned to `info`. A module-scoped
directive (`RUST_LOG=agentdesk::services::discord::tmux::execution_identity=debug`)
is more specific and is the form to try. **That precedence is not proven by any
test in this repository**, and `logging::init_dcserver_tracing` builds the filter
once behind a `OnceLock`, so the env var only takes effect at process start.
Verify empirically on a scratch dcserver that an `execution_identity_nonce_match`
line appears and record the check at sign-off; if it does not, P2 is unmet and
the promotion waits on item 1.

## Rollback

Set `execution_identity_mode` back to `observe` (keeps counters and marker reads)
or `legacy` (stops both, per #5411) in the live `agentdesk.yaml`. The next fenced
decision already uses it — no restart, no in-flight turn disturbed. Restart the
window from zero: `Observe` and `Enforce` share the counter path, so totals
accumulated under `Enforce` are indistinguishable from Observe-era ones. If the
rollback was triggered by a deny that stranded a session, recover via
[`execution-identity-manual-recovery-under-enforce.md`](execution-identity-manual-recovery-under-enforce.md),
not this document.

## What a Passed Formula Does Not Establish

The formula gates a rollout, not a correctness proof. Two identity-specific
non-guarantees, both declared in code: **no row-generation identity** — every
conjunct is a value comparison, and `WatcherIdentityFence` declares that a row
replaced and then re-admitted with all pinned values restored is
indistinguishable from one that never moved (`Enforce` permits that removal;
fixed by
`value_cas_declared_non_guarantee_readmitted_identical_row_passes_enforce`); and
**not an emission lease, not linearizable** — a `Match` says the session name
still denotes the captured spawn, says nothing about a terminal POST the *same*
incarnation has in flight, and the in-lock re-read shares no lock with the spawn
path's marker rename. For what the switch leaves untouched (the other 14 registry
removals, the `tmux_kill`/`process_kill` families, non-unix hosts) see
[the rollout runbook's coverage section](execution-identity-enforce-rollout.md#what-enforce-does-not-cover).

## Sign-Off

- Author: #5399 item 2, 2026-08-17 — thresholds proposed, not ratified.
- Threshold owner: pending (운영자 확정 필요 for P3, P4, P5).
- GO reviewer: pending.
