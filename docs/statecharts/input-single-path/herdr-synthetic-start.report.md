# HerdrSyntheticStart (#8422)

Code SHA: `82b96fa94b8d5e552f55d4f9c8a80d909503755c` (#8399 S3b-1 w3). Design: [8422 design](../../design/8422-synthetic-dead-bound.design.md).

A synthetic start (HttpStart, Routine, Decision, EffortContinue) on a Herdr channel is judged before claim by
`start_waits` (owner_input.rs:770) and `start_wait::observe` (owner_input_start_wait.rs:33).
A Bound row whose execution is dead reads Held (busy/herdr.rs:304-306), so the start is held under a watch
that keeps an Unknown forever and ends a Gone without a wake (held_watch.rs:65-71).
`Fixed` is design A: observe asks the shared dead-Bound retire (the #8212 / CLI `retire::observed(Scope::Ended)` proof),
refused while a managed turn or inflight row holds the channel; a retired row reads Ready and the start cold-launches;
a permanent refusal is settled with its reason.

Rules: `SyntheticSettles` (liveness: delivered or explicitly refused), `NoAutoRetireLive`, `NoAutoRetireUnderTurn`,
`RetiredStartIsCold`, `TypeOK`, and the checker's `NoDeadlock`.

| Config | Role | Broken rules |
|---|---|---|
| HerdrSyntheticStartBefore | current code, dead Bound | SyntheticSettles |
| HerdrSyntheticStartBeforeForeign | current code, owner differs | SyntheticSettles |
| HerdrSyntheticStartFixed | design A, dead Bound | - |
| HerdrSyntheticStartFixedForeign | design A, owner differs (refused and settled) | - |
| HerdrSyntheticStartFixedLive | live idle execution, may die (unfair) | - |
| HerdrSyntheticStartFixedBusy | live busy execution | - |
| HerdrSyntheticStartFixedUnreadable | live but unreadable | - |
| HerdrSyntheticStartFixedManagedTurn | dead + managed turn (retire waits for the turn end) | - |
| HerdrSyntheticStartFixedExternalTurn | dead + non-managed inflight row (Event) | - |
| HerdrSyntheticStartRetireOnHeld | mutant: retire on any Held (candidate 3 without the proof) | NoAutoRetireLive |
| HerdrSyntheticStartRetireUnderTurn | mutant: no turn guard | NoAutoRetireUnderTurn |
| HerdrSyntheticStartGoneNoRetire | mutant: candidate 1 alone (Gone, no retire) | SyntheticSettles |
| HerdrSyntheticStartTmuxBusy | tmux Event wait, presence wake | - |
| HerdrSyntheticStartTmuxDead | tmux dead pane (Gone → Go) | - |
| HerdrSyntheticStartTmuxNoPresenceWake | mutant: no turn_presence wake | NoDeadlock, SyntheticSettles |

RED 2 → GREEN 2 (`Before`, `BeforeForeign` → `Fixed`, `FixedForeign`).
`scripts/ci/statecharts_tlc.py --base 82b96fa94b`: PASS 15/15, each config under 10 s, at most 14 distinct states.

`Before` counterexample: front arrives → `start_waits` holds it under the dead-Bound watch → the watch keeps forever.
Only the unfair operator terminate (S3b-1 retired wake) leaves that state.

Charts: [before](herdr-synthetic-start.before.chart.mmd), [fixed](herdr-synthetic-start.chart.mmd).
Not modelled: a tmux pane dying during an Event wait (checked by acceptance test T8), pane naming waits,
transient row read failures, restart-gate holds.
