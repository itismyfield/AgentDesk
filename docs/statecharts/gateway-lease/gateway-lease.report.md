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
