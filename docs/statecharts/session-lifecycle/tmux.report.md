# TmuxSession

The [#8125 API-thread join projection](../input-single-path/thread-ingress.report.md) adds authorization before the cold/warm launch boundary. The historical model and its known violations below remain unchanged; Herdr parent policy and child readiness belong to #8118.

Model written against AgentDesk `origin/main` `e8d94425a04867279ced4ec967e9d93df0d7ec0a` (2026-10-30). Counterexamples were replayed against the code; see `../README.md` for dispositions.

TLC reached 400 states and 820 steps. The chart draws 13 states and 24 arrows.
The chart does not draw 395 steps that stay in one chart state.
The view hides 64 steps, so the chart does not draw them.

## RowOnlyWithTurn breaks
The shortest path has 7 steps:
0. Idle
1. a message arrives: Idle
2. a message starts a turn: Turn starting
3. the server crashes: Server down, turn row on disk
4. the server boots: Restarting, turn row not picked up
5. the recovery probe fails, so the row is left: Restarting, turn row not picked up
6. watcher restore has nothing to do: Restarting, turn row not picked up
7. intake opens: Open, a turn row names a missing session

## RowNamesSession breaks
The shortest path has 7 steps:
0. Idle
1. a message arrives: Idle
2. a message starts a turn: Turn starting
3. the server crashes: Server down, turn row on disk
4. the server boots: Restarting, turn row not picked up
5. the recovery probe fails, so the row is left: Restarting, turn row not picked up
6. watcher restore has nothing to do: Restarting, turn row not picked up
7. intake opens: Open, a turn row names a missing session

## NoDeadlock breaks
The shortest path has 8 steps:
0. Idle
1. a message arrives: Idle
2. a message starts a turn: Turn starting
3. the server crashes: Server down, turn row on disk
4. the server boots: Restarting, turn row not picked up
5. the recovery probe fails, so the row is left: Restarting, turn row not picked up
6. a message arrives: Restarting, turn row not picked up
7. watcher restore has nothing to do: Restarting, turn row not picked up
8. intake opens: Open, a turn row names a missing session

## Rules that hold
PromptOnlyToLiveSession

## Namespace socket absence (code base 75726b3b1d)

`TmuxSocketBoot.tla` specializes the T-A/T-B boot path to a retained row and one waiting
input. It enumerates local/mini/WSL ownership and namespace-missing/other-path/permission/
transient/present observations. The fixed cfg sets `SocketMissingIsMissing = TRUE`.
The before cfg, retained in the implementation evidence, sets it to FALSE.

TLC 1.7.4 / Java 21, direct TLC (no Node rendering), 2026-11-04:

| Rule | Before | Fixed |
|---|---|---|
| LocalMissingUnblocked | counterexample | holds |
| ForeignProtected | holds | holds |
| LiveProtected | holds | holds |
| UncertainProtected | holds | holds |
| NoDuplicateAdmission | holds | holds |

Before: down with local row and missing namespace socket -> Boot -> Restore keeps the row
because the classifier reports ProbeFailed -> Watchers -> OpenIntake admits zero inputs.
Fixed: Restore confirms Missing, the existing missing-session disposition removes the old
row, and OpenIntake admits that input once. Fixed TLC explored 75 distinct states / 90
states generated. The complete statechart checker passed all 21 configs against expected.tsv.

Code replay (line numbers are at the base above):

1. The durable row survives the boot reaper's allowed restart-generation window
   (`inflight/removal/boot_reaper.rs:144-162`). The model starts with that retained row.
2. `platform/tmux.rs:109-121` used to classify tmux's socket ENOENT text as ProbeFailed.
   `session_inventory.rs:340-356` already checks the reported namespace path and lstat.
3. `recovery_engine/tmux_probe.rs:103-107` maps ProbeFailed to Defer;
   `restore_inflight.rs:1558-1560` continues without retiring the row.
4. `runtime_bootstrap/recovery_flush.rs:128` opens intake after the restore passes.
   `inflight/save_store.rs:1218` refuses a new turn while the old row still exists.
5. With confirmed Missing, the existing `restore_inflight.rs:1563-1625` missing-session
   notice/disposition runs before intake opens. No new deletion or kill authority is modeled.

The broad TmuxSession model still reports RowOnlyWithTurn, RowNamesSession and NoDeadlock:
its general RestoreDefer also covers real transport failures and kept host guards. Those
are intentionally not reclassified as absence. T-C (the FIFO race) is unchanged. The new
specialized model closes only the namespace-confirmed socket-absence counterexample;
it does not claim every possible deferred restore is fixed.
