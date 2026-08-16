# Execution Identity Fence — Enforce Rollout Runbook

Source issue: #5399 (item 3 — "Enforce 롤아웃 runbook"). Related: #5071 T3-A1
(#5398) landed the fence; #5411 made `Legacy` skip the marker read.

Last refreshed: 2026-08-17

> Promotion into this cutover is gated by
> [`execution-identity-promotion-criteria.md`](execution-identity-promotion-criteria.md).
> Do not run this runbook until that formula passes with sign-off.

## Scope

`runtime.execution_identity_mode: enforce` makes exactly two automatic
watcher-registry removals refuse unless every pinned value still equals the live
one. Nothing else changes. This runbook is the fail-closed contract for that
flip, the pre-flight it requires, and the way back.

## The Fail-Closed Rule

`execution_identity::destruction_permitted_under_identity` permits a fenced
removal under `Enforce` only on `IncarnationObservation::Match` — a readable
nonce on **both** sides comparing byte-equal. Everything else denies:

| Captured | Live | Observation | `Enforce` |
|---|---|---|---|
| `Some(a)` | `Some(a)` | `Match` | permit |
| `Some(a)` | `Some(b)` | `Mismatch` | **deny** |
| `Some(a)` | `None` | `Unknown` | **deny** |
| `None` | any | `Unknown` | **deny** |

Fixed by `absent_spawn_nonce_is_never_observed_as_a_match` and
`only_enforce_denies_and_only_on_a_non_matching_incarnation`.

**Absence is permanent.** No production code writes a `.spawn_nonce` for an
already-running session: the only writers are the five provider spawn sites that
call `discord::stamp_spawn_markers` (two in `services::claude`, two in
`services::codex`, one in `services::qwen`), while restart adoption
(`watchers::lifecycle::restore::restore_tmux_watchers`), manual rebind
(`recovery_engine::manual_rebind`), and every recovery path write none. A session
whose marker is absent when Enforce goes live stays `Unknown` — and therefore
denied — for the rest of that tmux session's life, and the automatic repair that
would normally recycle it is exactly what the deny blocks.

## The Three Marker-Absent Categories

All three produce the same state (`read_spawn_nonce` returns `None`) and the same
outcome; the remedy in step 3 is the same for all three, so nothing downstream
depends on telling them apart. They differ only in how they arise and in whether
they leave any trace at all — see step 2 for what is measurable.

### A — Never written under this runtime root

`tmux_common::session_temp_path` builds the marker path under
`agentdesk_temp_dir()` from `session_temp_prefix(name)`, and that prefix embeds a
hash over two inputs: the runtime root (`current_tmux_owner_marker()`, built from
`config::runtime_root`) and the host namespace
(`tmux_common::host_temp_namespace`). Two ways in: the tmux session was created
outside the five spawn sites, so no marker was ever minted; or it was spawned
under a **different runtime root or host namespace** (dev↔release switch,
`AGENTDESK_ROOT_DIR` change, hostname change) and is now read under the current
one — the prefix differs, so `tmux_common::resolve_session_temp_path` misses at
the persistent path and also at the legacy `$TMPDIR` fallback.

This is the issue's "A0 이전 배포 생존 세션". The marker long predates the fence
(#3087 added it for the status panel), so age alone is not the discriminator —
namespace and spawn provenance are.

### B — Write failed at spawn

`tmux_session_files::write_spawn_nonce` is deliberately fail-to-absent: on any
error it removes both the temp sibling **and any pre-existing destination**, so a
failed respawn leaves no readable nonce rather than the prior spawn's stale one.
`stamp_spawn_markers` propagates that error and every spawn site only logs and
continues — the spawn is not aborted.

**Observability gap:** the two `services::claude` sites log through
`claude::debug_log`, gated on `DEBUG_ENABLED` and writing to
`$RUNTIME_ROOT/debug/claude.log`, **not** the dcserver log (`services::codex` and
`services::qwen` use `tracing::warn!`). A Claude marker-write failure is
therefore invisible in normal operation — "no warning in the dcserver log" is not
evidence that category B is empty for Claude sessions.

### C — Marker deleted while the session outlives it

`tmux_common::cleanup_session_temp_files` sweeps `spawn_nonce` (it is in its
`EXTS` list) from both the persistent and the legacy `/tmp` location, on teardown
paths such as `commands::control::reset_managed_process_session` and
`recreate_tmux_session`. Those normally kill the tmux session too — but a partial
teardown leaves a live session with no marker, and nothing re-mints it.

## What a Deny Actually Does

A deny is a refusal, never a partial mutation. Both sites run their flock-held
pin verification before the registry CAS, and after T3-A1 those callbacks are
pure (`|_| Ok(CommitEvidence::…)`): `inflight::commit_destructive_cancel_locked`
verifies identity, `updated_at` and `save_generation` and returns an outcome
without writing the row. So the guarantee is a **no-write** one, and it is about
this path: **a deny does not change the inflight row, the registry entry, or the
handle's `cancel` value.**

That is not a liveness statement about the watcher. The CAS conjunct is
`Arc::ptr_eq(&entry.cancel, expected_cancel)` — pointer identity, never the
boolean — so a pin still compares equal to a handle whose `cancel` another path
already stored `true` on in place, leaving the entry registered. Production paths
that do exactly that exist. **This runbook does not enumerate them**, and the
absence of a list here is not a claim that there is only one, or that any
particular other path removes the entry first. After a deny you may state that
this path wrote nothing; you may **not** state that the incumbent watcher is
still relaying.

**`relay_recovery_dead_frontier_cancel`** — `cancel_and_remove_channel_if_current`
returns `false`, logging `relay recovery skipped finalizer after committed
cancel; expected watcher was not current` at `warn`, skipping both
`finalize_cancelled_watcher_owner_turn` and
`inflight::clear_lifecycle_inflight_state_if_matches_identity_after_death_evidence`,
then falling through to `reattach_apply::apply_rebind`. The recovery attempt
degrades to the non-destructive rebind rather than failing outright. This site is
reachable only from `RelayRecoveryApplySource::Manual` — see
[`execution-identity-manual-recovery-under-enforce.md`](execution-identity-manual-recovery-under-enforce.md).

**`tui_direct_stale_foreign_cancel`** — `remove_tmux_session_if_current` returns
`None`, logging `tui_direct_pending_start: stale FOREIGN cancel committed but
watcher incarnation changed; finalizer skipped` at `info` and returning `false`.
The `pinned.cancel.store(true, …)` in `submit_stale_foreign_inflight_cancel` sits
**after** that `is_none()` early return, so this path leaves the registration and
the handle's `cancel` value as it found them.
`demote_stale_foreign_inflight_if_current` then returns `false`, the caller's
`reclaim_orphan_fn` reports `ReclaimStaleForeignOutcome::None`, and the worker
falls into bounded escalation instead of re-evaluating. After
`PENDING_START_MAX_BACKSTOP_CYCLES` it takes the ABORT branch
(`event = tui_direct_pending_start.backstop_abort_foreign_inflight_live`): the
synthetic turn-start claim is dropped, the anchor keeps its `⏳`, and reconcile
lands `✅` via the prior owner's completion or `⚠` via the TTL fallback.

The submitted input is not retracted — the prompt reached the pane before this
branch, and the ABORT branch's own comment records that only the synthetic
ownership claim is dropped while "the watcher/bridge still relays its output".
That last clause holds for as long as the incumbent watcher is alive, which this
path neither disturbs nor guarantees: per the pointer-CAS note above, a deny is
consistent with a watcher some other path has already cancelled in place.
Ownership bookkeeping is what degrades by construction; output delivery degrades
only in that residual case, and the `⚠` TTL fallback is where it surfaces.

**This is the primary regression signal after the flip.** A rise in
`backstop_abort_foreign_inflight_live` on channels that previously self-healed is
the signature of a marker-absent session hitting the fence.

## Pre-Rollout Checklist

### 1. Confirm the promotion formula passed

Clauses P1–P6 with sign-off. P5 requires `unknown == 0`, and that count *is* the
measured marker-absent population.

### 2. Marker-absent live sessions: what you can and cannot measure

**There is no procedure here that enumerates them, and #5399 adds none.**
This step tells you what the fence's `Unknown` actually is, and where the rule
that decides it lives — not how to reproduce that rule by hand.

**What `Unknown` is.** `read_spawn_nonce` returning `None`, which is three
conditions, only the first of which a filename glob sees:

1. `tmux_common::resolve_session_temp_path` finds neither the **exact**
   persistent path nor the **exact** legacy `$TMPDIR` path. The path is exact,
   not suffixed, and its prefix carries a hash over the runtime root and host
   namespace (category A above). So a suffix glob (`*-<session>.spawn_nonce`)
   matches a marker minted under a **stale prefix** the reader will never open,
   and reports it `ok`.
2. `std::fs::read_to_string` on that one path fails. `resolve_session_temp_path`
   returns the persistent path as soon as it *exists*; `read_spawn_nonce` then
   reads **only** that path and does **not** fall back to `$TMPDIR`. An existing
   but unreadable persistent marker is `None` even when a readable legacy one
   sits beside it.
3. The content is empty after `str::trim`. Presence of the file is not presence
   of a nonce.

#### 2a. The path rule is the code — do not re-derive it

The source of truth for the marker path is `tmux_common::session_temp_prefix`
(what is hashed and how the prefix is assembled) together with
`session_temp_path` / `resolve_session_temp_path` (the two candidate paths and
their order), over the two prefix inputs `tmux_common::current_tmux_owner_marker`
(via `config::runtime_root`) and `tmux_common::host_temp_namespace`. Read those
when you need the rule.

**Do not re-derive the prefix by hand.** Each input has its own normalization
and fallback behaviour on blank, whitespace-only, and unset values, and a shell
re-implementation that diverges in any of those edge cases yields a different
path — which turns a healthy session into a false `ABSENT` and sends you to
step 3 to recycle a session that never needed it. A prior revision of this
runbook carried such a derivation and it was wrong in exactly that way.

#### 2b. Nothing exposes the resolved path either

`read_spawn_nonce` returns the marker's **content**, never the path it resolved,
so no caller of it can print one. `execution_identity::observation_counts` is
`#[cfg(test)]`, and no production log line, API, or CLI prints a resolved
`.spawn_nonce` path. There is no existing tool that calls the derivation for you,
and adding a reader or an exposure endpoint is code, out of scope for a docs
change.

**Carry this as a declared limitation into the sign-off:** at pre-flight time the
marker-absent population is **unmeasured**. The measurement that does exist is
the fence's own `Observe`-mode `unknown` counter (promotion clause P5), and it is
a count, not a session list — so it tells you *whether* the population is empty,
never *which* sessions are in it.

#### 2c. Given a marker path, the ok/ABSENT decision

This part is a predicate on file content, not a derivation, so it is safe to
reproduce. If you hold the exact path — obtained from the code itself, never
re-derived by hand or by glob — `read_spawn_nonce`'s verdict is:

```bash
probe() {  # $1 = the exact path resolve_session_temp_path would have returned
  [ -e "$1" ] || { echo "ABSENT  (no marker at that path)"; return; }
  raw="$(cat "$1" 2>/dev/null)" || { echo "ABSENT  (unreadable: $1)"; return; }
  if [ -z "$(printf '%s' "$raw" | tr -d '[:space:]')" ]; then
    echo "ABSENT  (empty after trim: $1)"
  else
    echo "ok      $1"
  fi
}
```

The whitespace-stripped comparison is the emptiness predicate only, not the
value: `read_spawn_nonce` decides on `str::trim`, and the two differ only for
content with interior whitespace, which no minted nonce has
(`write_spawn_nonce` writes a `Uuid::simple` hex string).

An `ok` here proves `read_spawn_nonce` would have returned `Some` **at the
instant of the check, for that path** — not that the marker will equal a future
capture (a respawn in between re-mints it — fine, both sides re-read), and not
which of A/B/C applies, which it need not, because the remedy is identical.

### 3. Clear every marker-absent session you do identify, before the flip

The **only** way to give a live session a readable nonce is another provider
spawn; there is no backfill tool, and adding one is out of scope for #5399. Any
session you have established to be marker-absent must be either **recycled** —
reset the session so the next turn
spawns fresh and `stamp_spawn_markers` mints a nonce
(`commands::control::reset_managed_process_session` / `recreate_tmux_session`,
both unfenced in every mode) — or **accepted**, knowingly left with its two
fenced repair paths disabled for the rest of that session's life, with session
name and owning channel recorded.

Do **not** hand-write a `.spawn_nonce`. The nonce is the identity of a specific
spawn; a fabricated value makes the fence certify a spawn that never happened,
which is strictly worse than the deny.

### 4. Confirm hot-reload works on the target host

`execution_identity_mode` reads `config_live_reload::current()` on every fenced
decision, so flip and revert both apply without a restart. Verify hot-reload
actually works here (edit an unrelated live-reload key and confirm it takes
effect) — a host where it is broken turns the revert into a restart.

### 5. Record the baseline

For the window immediately before the flip: the count of
`tui_direct_pending_start.backstop_abort_foreign_inflight_live` events; the count
of `relay recovery skipped finalizer after committed cancel` warnings (a rate,
not a boolean — a plain pointer/binding miss also produces it); and step 2's
limitation, restated for the record: no marker-absent session list was produced,
because no procedure produces one.

## The Flip

1. Set `runtime.execution_identity_mode: enforce` in the live `agentdesk.yaml`.
2. Do not restart; the next fenced decision reads the new value. Note there is
   **no cheap production proof that the flip took effect** — a
   `POST /api/channels/{id}/relay-recovery` dry run (the JSON body field
   `"apply": false`, which is also the default; see
   [the manual-recovery runbook](execution-identity-manual-recovery-under-enforce.md#how-to-actually-set-apply))
   returns `mode: "dry_run"` from `run_relay_recovery` before any fence is
   captured, so it confirms the decision shape and nothing about the mode, which
   is only observable when a fenced decision reaches a registry CAS. Confirm the
   config value and rely on step 3 for behaviour.
3. Watch the two signals from step 5 for the first full turn cycle on the busiest
   channel before widening.

## Reverting

Set the mode back to `observe` (keeps counters and marker reads) or `legacy`
(stops both, per #5411). The next fenced decision uses it.

**The revert restores the pre-Enforce destructive outcome, not the pre-T3-A1
system.** T3-A1 deleted the two #5067 in-flight emission fences in every mode and
no value of this switch brings them back; that baseline needs a revert of #5398 —
both `config::ExecutionIdentityMode` and
`tmux_watcher_registry::execution_identity_mode` say so in their docs. A revert
also does not repair what a deny already stranded: a session that took the abort
branch has dropped its synthetic claim, and the anchor reconcile runs on its own
path.

## Non-Unix Hosts

`services::discord::tmux` is `#[cfg(unix)]`, so `tmux_watcher_registry` carries a
`#[cfg(not(unix))]` shim pair: `capture_session_spawn_nonce` returns `None`, and
`destruction_permitted_under_identity` reduces to
`!mode.denies_on_incarnation_mismatch()`. So under `Enforce` a non-unix host
**refuses both fenced paths unconditionally**, and under `Observe` it records
**nothing** — the shim never calls `record_incarnation_observation`, so a window
observed there yields zero mismatches *and* zero samples, which the promotion
formula's P4 floor exists to reject. Both are stated on
`ExecutionIdentityMode::Enforce` and on the shim itself.

## What Enforce Does Not Cover

- **14 of the 16 production registry removals.** The `registry_remove` category
  of `scripts/destructive_call_site_baseline.json` records 16 removals across 10
  files; the fence covers one in `relay_recovery/apply.rs` and one in
  `tui_direct_pending_start.rs`. Every other entry is unchanged in every mode —
  including the **second** `relay_recovery/apply.rs` removal, the
  `shared.tmux_watchers.remove(&channel)` in the idle-tmux-reattach branch of the
  same `ReattachWatcher` arm, which cancels a watcher with no identity conjunct
  at all.
- **Every `tmux_kill`, `process_kill`, and unfenced `watcher_cancel` site** in
  that same baseline.
- **The A → B → A readmission** and **same-incarnation emission races** — see the
  promotion runbook's
  [non-guarantees](execution-identity-promotion-criteria.md#what-a-passed-formula-does-not-establish).
- **A pre-flight enumeration of marker-absent live sessions.** Per step 2 there
  is none, and this runbook does not substitute a hand derivation for it.

## Sign-Off

- Author: #5399 item 3, 2026-08-17 — procedure documented; no GO recorded.
- Pre-flight owner: pending.
- GO reviewer: pending.
