# Codex TUI rehydration idempotency (#5755)

Base: `08ee40ee4e9be20a4be7a9322a7e632b7ded77bc`.
The issue's measured revision `54cee749fefe691d992d5a9f8abebf6377b0be42`
has identical versions of `rehydration.rs`, `codex_tui_restore.rs`,
`guarded_persist.rs`, and `tui_prompt_dedupe/runtime_binding.rs`.

The periodic caller is `codex_idle_rollout::spawn_codex_idle_rollout_relay`.
It invokes `rehydrate_existing_codex_tui_bindings`, whose transaction's `Some`
result causes a success log and an insertion into the pass-local claimed-path
set. `None` continues that session's loop; it does not trigger another restore
or fallback. Existing bindings already contribute to the initial claimed set.

At this base, the transaction already uses the per-source authority lock and
returns an existing, present-file binding with `replace = false`. That still
produces `Some`, so the success log repeats. The corresponding dedupe channel
registration changes the channel mirror (and a DM channel marker), not an
inflight row. The direct-resume watcher fallback also reuses the existing
source binding. Therefore the issue's observed log frequency alone does not
establish a durable row deletion or prove the cause of `Missing`.

The direct-resume helper is consumed by startup recovery in
`watchers/lifecycle/restore.rs`. A live, unpaused watcher with the same output
path is skipped before its commit step. At commit,
`try_claim_watcher_with_thread_parent` atomically refuses an existing equivalent
watcher; a paused/cancelled watcher or a changed output path can cancel and
replace it, after which a supervised watcher is spawned. Those asynchronous
lifecycle effects are real, but the periodic rehydration transaction's `Some`
result does not invoke that startup path. Its owner-registry repair only
updates the restored-owner map and preserves an existing live watcher.

The repair makes an unchanged binding a no-op under that same authority lock.
An unambiguous live marker can replace a previous rollout even while its file
still exists. A changed relay namespace repairs its path while retaining the
same rollout cursor. Eviction remains recoverable, including a recreated pane
that resumes the same rollout. No permanent "already restored" flag is used.
Stale, duplicate, or foreign claimed markers cannot replace a valid binding.

The regression fixture seeds an active inflight row and compares the entire
persisted row, including its identity and offsets, before and after the second
rehydration. Concurrent calls must install and report a recovery once. These
checks cover local source/binding behavior; they do not simulate Discord
delivery or establish why an already-missing row disappeared.

After the coordinated deployment, observe a Codex turn for at least 30 minutes:

- With no rollout/pane change, successful rehydration logs converge to zero per
  hour and healthy streaming produces no `outcome=Missing` skips.
- Status panel updates follow real tool activity throughout the turn.
- A genuine rollout replacement or recreated pane recovers and then settles.

This live observation is pending deployment and cannot be replaced with a unit
test result. The T5 `Missing -> Suppressed` rule, cohort configuration, and turn
lease/finalizer behavior are outside this change.
