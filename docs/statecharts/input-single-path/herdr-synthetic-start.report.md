# HerdrSyntheticStart (#8422, rev2 + impl-w2)

Code SHA: `82b96fa94b8d5e552f55d4f9c8a80d909503755c` (#8399 S3b-1 w3). Design: [8422 design rev2](../../design/8422-synthetic-dead-bound.design.md).

A synthetic start (HttpStart, Routine, Decision, EffortContinue) on a Herdr channel is judged before claim by
`start_waits` (owner_input.rs:770) and `start_wait::observe` (owner_input_start_wait.rs:33).
A Bound row whose execution is dead reads Held (Claude, busy/herdr.rs:304-306) or `Activity::Unknown`
(Codex early return, owner_input_start_wait.rs:67-76; unbound.rs:328). The start is held under a watch
that keeps an Unknown forever and ends a Gone without a wake (held_watch.rs:65-71).
A person's input reaches #8212's recovery after claim (Claude only, Scope::Missing).

`Fixed` is design rev2: every provider's Unknown Bound read asks the shared dead-Bound recovery, with the owner compared
with the asking provider and the CLI `retire::observed(Scope::Ended)` proof. The recovery holds the channel transition
lease from its last no-turn read to the CAS (`retiring` phase; a person borrows its own guard), and passes only an
exact `Ok(None)` inflight read. A retired row reads Ready and the start cold-launches. A permanent refusal settles Failed.
`TurnStart` is another start registering its token; it must take the transition lock first (turn_start.rs:151).
`InflightStart` is a conservative model of a watcher creating an inflight row alone; the real Herdr paths are design
8.2 (#7 is unreachable on Herdr, #8 the monitor auto-turn takes the lease).

impl-w2 splits the physical execution (`exec`) from a Codex rollout's last turn (`rollout`): a closed
rollout reads Idle whatever happened to the execution. `Fixed` joins a Codex Ready/Busy with the exact execution's
liveness; mutant `unknownOnly` is impl-w1 (recovery on Unknown alone). A follow-up into a dead Bound execution fails its
start (Failed), so a delivery needs a live existing execution or a new cold one (`DeliveredNeedsExecution`).

Rules: `SyntheticSettles` (delivered or explicitly refused), `OwnedStartDelivered` (a start whose owner matches is
delivered), `NoAutoRetireLive`, `NoAutoRetireUnderTurn` (no Bound retired while a token or an unproven inflight read
holds the channel), `RetiredStartIsCold`, `RejectedIsFailed`, `TypeOK`, and the checker's `NoDeadlock`.

| Config | Role | Broken rules |
|---|---|---|
| Before, BeforeShell, BeforeCodex, BeforeForeign | current code, synthetic | SyntheticSettles, OwnedStartDelivered |
| BeforePersonShell | current code, person, lone root shell | SyntheticSettles, OwnedStartDelivered |
| BeforePersonMissing | current code, person, missing pane (#8212) | - |
| Fixed, FixedShell, FixedCodex, FixedCodexShell, FixedForeign | rev2 | - |
| FixedPersonMissing, FixedPersonShell, FixedPersonRace | rev2, person with borrowed lease | - |
| FixedLive, FixedBusy, FixedUnreadable, FixedCodexLive, FixedCodexUnreadable | live or unreadable: never retired | - |
| FixedManagedTurn, FixedExternalTurn, FixedInflightUnreadable | turn, inflight row, inflight read error | - |
| FixedRace, FixedRaceLive | token registration race around the CAS | - |
| FixedInflightRace, FixedPersonInflightRace | a watcher's lone inflight row racing the CAS | - |
| CodexEarlyReturn | mutant: Codex returns before the recovery | SyntheticSettles, OwnedStartDelivered |
| CodexClosedRolloutBefore | current code, Codex closed rollout Idle on a dead execution | OwnedStartDelivered |
| CodexClosedRolloutUnknownOnly, CodexClosedRolloutLiveUnknownOnly | impl-w1: Idle read then (or already) dead | OwnedStartDelivered |
| FixedCodexClosedRollout, FixedCodexClosedRolloutShell, FixedCodexClosedRolloutLive | impl-w2: liveness joined | - |
| OwnerClaudeConst | mutant: owner compared with "claude" | OwnedStartDelivered |
| NoLease | mutant: no lease between read and CAS | NoAutoRetireUnderTurn |
| InflightNoLease | mutant: the lone inflight row skips the lease | NoAutoRetireUnderTurn |
| NoBorrow | mutant: person re-acquires its own lock | SyntheticSettles, OwnedStartDelivered |
| RejectAsCompleted | mutant: refusal settled Completed | RejectedIsFailed |
| OkFlatten | mutant: inflight read error passes as no row | NoAutoRetireUnderTurn |
| RetireOnHeld | mutant: retire without the exact proof | NoAutoRetireLive |
| RetireUnderTurn | mutant: no turn guard | NoAutoRetireUnderTurn |
| GoneNoRetire | mutant: candidate 1 alone | SyntheticSettles, OwnedStartDelivered |
| TmuxBusy, TmuxDead | tmux Event wait / dead pane | - |
| TmuxNoPresenceWake | mutant: no turn_presence wake | NoDeadlock, SyntheticSettles, OwnedStartDelivered |

RED 5 → GREEN 5 (Before, BeforeShell, BeforeCodex, BeforeForeign, BeforePersonShell →
Fixed, FixedShell, FixedCodex, FixedForeign, FixedPersonShell).
`scripts/ci/statecharts_tlc.py --base 82b96fa94b --jobs 4`: PASS 45/45 (80.1 s total).
impl-w2 RED → GREEN: CodexClosedRolloutUnknownOnly → FixedCodexClosedRollout (and the Shell/Live pair).

`NoLease` shortest counterexample: front arrives → `start_waits` begins the retire without a lease →
another start registers its token → the CAS retires a Bound that a turn holds.

Charts: [before](herdr-synthetic-start.before.chart.mmd), [fixed](herdr-synthetic-start.chart.mmd),
[no lease](herdr-synthetic-start.nolease.chart.mmd).
Not modelled: a tmux pane dying during an Event wait (acceptance test T8 fixes the watcher → unbound Idle → WakeOwner
order), the audit's per-call proofs (design 8.2), pane naming waits,
transient row read failures, restart-gate holds, receipt persistence across restart (acceptance test T7).
