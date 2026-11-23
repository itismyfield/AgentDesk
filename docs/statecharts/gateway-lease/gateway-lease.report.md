# Gateway lease — preferred-node handback (#8351)

Code SHA: `b012692aa3a98bdc4ed4c0543a81013a5a332ff6` (Before), plus the #8351 round-2 retry (After).
Model: [GatewayLease.tla](GatewayLease.tla). One provider (Claude), two nodes: P is the preferred node and tui_o home; N is non-preferred and tui_o Foreign.

## Rules

- `LeaseReturnsToPreferred` (PROPERTY): `<>[](lease = "P")`. Restarts are bounded (`MaxRestarts = 2` each), so once they stop and P is online, the lease ends up on P and stays there.
- `ForeignQueueDrains` (PROPERTY): `[](queued => <>~queued)`. An O-channel input that N (Foreign) can only queue does not stay queued forever.
- `LeaseMatchesRuntime` (INVARIANT): the lease holder is exactly the node whose runtime is serving.

`PAcquire` (preferred-poll) is strong-fair. Between N's unlock and N's own retry the lock is free only intermittently. In code, P polls every 5 s and N's yield grace is 90 s.

## Results (TLC 1.7.4, plain `java`, WSL)

`blocker` separates the observed cause from the one this fix leaves alone. A held restore (`busy`, logged on 11-23 as `native control history busy`) clears when the history lock holder finishes. A refusal (`refused`: preparation or boot reaper) does not clear inside the process. `Refusable = FALSE` limits P's restarts to busy holds.

| cfg | Mode / Refusable | result |
|---|---|---|
| GatewayLeaseBefore | `abandon` / FALSE — `runtime_bootstrap.rs:299-301` at b012692 | RED: `LeaseReturnsToPreferred`, `ForeignQueueDrains` |
| GatewayLeaseAfter | `retry` / FALSE — round 2: a held restore is retried every 5 s | GREEN |
| GatewayLeaseAfterRefused | `retry` / TRUE — a refusal still ends the runtime | RED (known gap, left by decision; health now shows `native_boot_refused`) |
| GatewayLeaseNoYield | `noyield` / FALSE — mutation without the keepalive yield | RED: same two rules |

No chart is generated: the `statechart` skill is not installed on the WSL lane.

## Before counterexample (matches the 11-23 incident)

1. P restarts (`PRestart`). The restart leaves a native record whose boot restore refuses (`blocker = TRUE`).
2. P boots. Restore is held (`busy`), `prepare_native_boot` returns false and run_bot returns (`PRefuse` → `pRt = "dead"`). No `GATEWAY-LEASE` line, and no `waiting_providers` advertisement.
3. The cause clears (`ClearBlocker`). P never retries.
4. N restarts and boots. `yield_to_preferred_gateway` sees no waiter and acquires at once, which matches the mac-mini re-acquire at 14:54. The keepalive yield needs `node_awaits_gateway(P)`, so N holds the lease forever. The O input stays queued.

## Replay against code

| Model step | Code |
|---|---|
| PRefuse (Before) | `src/services/discord/runtime_bootstrap.rs:299-301` → `runtime_bootstrap/native_input.rs:16-35` (three `return false` arms; `native_prepare::enabled()` is always `true`, `turn_finalizer/native_prepare.rs:15`) |
| busy held, retried (After) | `runtime_bootstrap/native_input.rs` `await_native_boot` (`NativeBoot::RestoreHeld` loop, 5 s) |
| PPrepare → PWaiting | `runtime_bootstrap.rs:353` → `gateway_lease.rs:488-499` → `acquire_as_preferred_gateway` `gateway_lease.rs:229-319`; waiter publish at `:243-261` |
| PAcquire | `gateway_lease.rs:265-293` (5 s loop, `GATEWAY_PREFERENCE_POLL_INTERVAL`) |
| NStart / NYieldEnd | `gateway_lease.rs:500-529`, `yield_to_preferred_gateway` `:154-225` |
| NStandbyTake | `gateway_lease_recovery.rs:602-667` |
| NKeepaliveYield | `gateway_lease.rs:787-822` (the `preferred_gateway_is_waiting` `:118` → `should_yield_to_preferred` `:98` → `node_awaits_gateway` `cluster/intake_worker_capabilities.rs:131`) |
| Arrive/Deliver on N | O-home refusal "O channel … is served only on O home …" (Foreign queue) |

Not modeled: the handback breaker (`gateway_handback_breaker.rs`). It suppresses the keepalive yield after dense empty handbacks; on 11-23 no handback happened, so the breaker was not involved.
The preferred node's restart-pending standby (`gateway_lease.rs:304-311`) is not modeled either. That standby retries without advertising a waiter.

## Boot gate and SIGTERM (#8351 rounds 3-4)

Model: [GatewayBootShutdown.tla](GatewayBootShutdown.tla). One SIGTERM reaches a process whose Claude runtime C has no SIGTERM handler yet. C may be in the native boot gate, in the gateway lease phase after a Ready gate, or out of run_bot after a failed lease or a refusal. With `Providers = 2`, another provider O already runs its handler. While no tokio SIGTERM listener exists, the signal keeps its default action and kills the process.

Rules:
- `NoStartAfterTerm` (INVARIANT): a gate that decides after the shutdown request starts no runtime.
- `BarrierEnds` (PROPERTY): `term ~> (done # "no")`. After SIGTERM, the last slot holder exits or the default action ends the process.

| cfg | Mode / Providers / LeaseEnds | result |
|---|---|---|
| GatewayBootShutdownSingleBefore | `r3` (2b93d60 latch) / 1 / FALSE | RED: `BarrierEnds`. The latch removes the default action: after a failed lease, or in an endless preferred wait, nothing exits |
| GatewayBootShutdownSingleAfter | `b` (round 4) / 1 / FALSE | GREEN |
| GatewayBootShutdownMultiBefore | `r2` (53de918) / 2 / TRUE | RED: `NoStartAfterTerm`, `BarrierEnds` |
| GatewayBootShutdownMultiR3 | `r3` / 2 / TRUE | RED: `BarrierEnds`. The failed lease keeps the raw `fetch_sub` after O already took its slot |
| GatewayBootShutdownMultiAfter | `b` / 2 / TRUE | GREEN |
| GatewayBootShutdownMultiNoMark | mutation: the gate ignores the mark | RED: `NoStartAfterTerm` |
| GatewayBootShutdownMultiNoHandoff | mutation: C's handler hears only its own stream | RED: `BarrierEnds` |
| GatewayBootShutdownMultiRawFail | mutation: the failed lease keeps the raw `fetch_sub` | RED: `BarrierEnds` |
| GatewayBootShutdownMultiNoExit | mutation: a boot-time slot release never exits | RED: `BarrierEnds` |
| GatewayBootShutdownMultiStuckLease | `b` / 2 / FALSE | RED: `BarrierEnds`. Known gap, present since b012692: with a peer handler, preferred-poll does not read the mark and can wait forever behind a suppressed handback |

`Refusable = TRUE` in every config.

### Entry points that take SIGTERM or a barrier slot (round-4 head)

| Entry point | Code | Model |
|---|---|---|
| native gate | `runtime_bootstrap.rs:300-302` → `native_input.rs:33` reads the mark (`boot_sigterm::watch`), checked after each attempt `:101-104`, sleep raced `:137-144` | CAttempt / CWaitEnd / CRetry |
| gate stop | `boot_sigterm.rs:53-56` `stop_before_start` → `release_boot_slot` `:34-50` (CAS once; exit only for the last slot after SIGTERM) | Take |
| refused runtime | `native_input.rs:119` → `release_boot_slot` | CAttempt refused branch |
| failed gateway lease | `runtime_bootstrap.rs:367-371` → `release_boot_slot(&shared, seen())` | CLeaseFail |
| utility bot skip | `runtime_bootstrap.rs:191` raw `fetch_sub` before any SharedData (unchanged) | not modeled |
| preferred-poll wait | `gateway_lease.rs:229-319` (reads only its own `shutting_down` `:297` and the restart file) | CLeaseStart / CLeaseEnd fairness |
| SIGTERM handler install | `runtime_bootstrap.rs:333` (worker), `:409` (gateway), `:454` (standby) | CLeaseStart → started |
| SIGTERM handler | `shutdown.rs:14-54`: `legacy_sigterm` `:21`, mark `:22`, CAS + last `exit(0)` `:37-56`; signal source `:95-104` (own stream raced with the mark) | OHandle / CHandle |
| process mark | `boot_sigterm.rs:7-22` (static `watch`; registers no signal) | `mark` |
| deferred restart slot | `deferred_restart.rs:94` CAS, `:121-131` `fetch_sub`, `:195-208` rollback `fetch_add`; final `exit(0)` `spawns.rs:366-395` | not modeled (no SIGTERM) |
| per-runtime flag / shared barrier | `shared_data.rs:163-180`, `shared_state.rs:617-621`, flag writers `:728-774` | model premise |
| startup sigaction | `cli/dcserver_decision_boot.rs:68-87` (installed and restored during decision boot, before run_bot) | not modeled |
