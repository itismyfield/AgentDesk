# Peer deploy restart during a gateway yield wait

Code SHA: `315143ba6b6f19d21e29d1db7d7ec4e53e787ad8` (main) and the fix branch `fix/i8176`.

The model follows one non-preferred cluster peer (the mac-mini) right after it restarted itself, and one
cluster deploy request to it. The peer boots into the preferred-gateway yield wait
(`yield_to_preferred_gateway`, up to `gateway_yield_grace_secs` = 90s), then runs with the marker poller
that persists `restart_pending` and exits. It can also write its own UUID restart request (standby
promotion or a self-fence handback). The deploy writes its nonce into `restart_pending`, waits up to
20s for `restart_persisted.<nonce>`, and on timeout disposes its own marker, so the later 30s wait can no
longer be answered.

`Mode` picks the yield wait:

- `main`: the wait spawns no poller and ignores `restart_pending`, so a request that lands in it is
  not seen until the wait ends.
- `fixed`: the wait stands down to standby as soon as a request with a nonce is pending, skipping the
  lease attempt; standby spawns the poller.
- `anyproof` (mutant of fixed): the deploy accepts any persisted proof, including the peer's own UUID one.

One tick is about 10s. The yield wait is `Y` = 3 ticks and the deploy budget `B` = 2; faster steps
(poller, relaunch, acknowledgement) run before time passes.

## Rules

- `AckRequiresOwnProof` (INVARIANT): the deploy is acknowledged only by the proof of its own nonce.
- `DoneRunsNewBinary` (INVARIANT): a finished deploy leaves the peer on the staged binary.
- `DeployConfirmed` (PROPERTY): an issued deploy request is acknowledged, whatever restart the peer was in.
- `RestartsSettle` (PROPERTY): restarts stop; eventually no request is pending and the peer is not exiting.

## Results

| Config | Breaks | Path |
|---|---|---|
| `PeerDeployRestartMain.cfg` | `DeployConfirmed` | the peer boots into the yield wait, the deploy writes its request, two ticks pass and the deploy's timeout clears the request before the peer leaves the wait |
| `PeerDeployRestart.cfg` | nothing | 61 distinct states |
| `PeerDeployRestartAnyProof.cfg` | `AckRequiresOwnProof` | the peer writes and persists its own restart, then a deploy request is acknowledged by that UUID proof |

The chart is the `fixed` run.

## Code map

| Model | Code |
|---|---|
| `peer = yield` | `src/services/discord/runtime_bootstrap/gateway_lease.rs` `yield_to_preferred_gateway` (non-preferred arm of `run_bot_acquire_gateway_lease`) |
| `YieldStandsDown` | the same wait returning `YieldEnd::RestartPending` → `Ok(None)` → `GatewayLeaseOutcome::Standby` |
| `peer = up` | `start_standby_runtime` / gateway start, both spawning `run_bot_spawn_deferred_restart_poller` (`spawns.rs`) |
| `PollerConsumes` | `spawns.rs` poller: `commit_deferred_restart_sentinel` → `identity_safe_dispose_restart_marker` → `exit(0)` |
| `SelfRequest` | `gateway_lease_recovery.rs` `attempt_clean_standby_promotion` (UUID nonce) and `gateway_lease.rs` `self_fence_gateway` (UUID nonce) |
| `DeployRequest` / `Ack` / `Timeout` | `scripts/_defaults.sh` `request_restart_drain_mode_or_fail` (20s, `clear_restart_drain_mode` on timeout) and `wait_for_restart_persistence_or_fail` (30s) |
| `Swap` | `scripts/deploy-release.sh` bootout/bootstrap after the durability gate |

## Left out

- The deploy refusing outright (`refused:restart-lease-held`) when the peer's own UUID request still holds
  `restart_pending`: the model only issues a deploy request while no marker is pending. That refusal is
  the existing marker lease and its window is the few seconds of a self restart.
- Every provider has its own yield wait and poller; the model folds them into one peer. The final
  provider writes the proof.
- The preferred node keeps advertising itself as a waiter while it holds the gateway, so every boot of
  the non-preferred peer spends the full 90s in the yield wait; this is why every cluster deploy hit it.
- A cancelled request after the peer stood down leaves it in standby; its lease retry may later promote
  with one more restart when the preferred node does not hold the lease.
