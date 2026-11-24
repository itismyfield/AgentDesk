# Whose transition a session-transition lease fits

Code SHA: written against `4bf7c64ec3` (#7353 KR-C10e design input); coordinates below are the KR-C10e-2 head, which moves the lock lookup into `SessionTransitionLocks` without changing a step.

Every bot runtime (`SharedData`) keeps one registry of channel transition locks.
A caller takes a channel's lock as a plain guard, as a `SessionTransitionLease`, or as an `UnplacedLease`; a lease can also stand on a guard its caller already holds.
The rule under test: **one lease fits only the transition of the `SharedData` that made it.**
The model has two runtimes, two channels and at most three lock allocations.

Constants pick what the code is compared with:

- `Wiring`: `own` gives every `SharedData` its own registry (the code); `shared` points both at one registry (hypothetical).
- `Ident`: what `matches` compares. `host` is the `SharedData` address, the channel and the lock (the code); `registry` compares the registry address instead; `lockOnly` drops the address.
- `Prune`: `deadOnly` prunes entries with no holder when a new channel is inserted (the code); `all` prunes every other entry.
- `Tokens`: `perHost` gives every runtime its own token hash (production); `same` gives both one hash (two test fixtures).

## Rules

- `LeaseFitsOnlyItsMaker`: a lease matches no other `SharedData` and no other channel.
- `LeaseFitsItsMaker`: a live lease always matches its own `SharedData` and channel.
- `BorrowedFromOwnGuard`: a borrowed lease stands on a guard its own maker took for the same channel.
- `OneHeldLockPerChannel`: one runtime's channel never has two held locks.
- `HeldLockStaysRegistered`: a held lock stays the registered one.
- `HostsHoldSeparateLocks`: two runtimes never share a lock.
- `UnplacedCoversOnlyItsMaker`: an unplaced lease covers only its maker's rows.
- `UnplacedUsesMakerRegistryAndToken`: an unplaced lease holds its maker's channel lock and its maker's token hash.
- `UnplacedCoversExactlyItsOwnerTuple`: `covers` is true exactly for that token hash and channel.
- `UnplacedHoldsCoveredHostsLock`: the runtime whose row an unplaced lease covers is the one whose lock it holds.

## Results

| Config | Wiring / Ident / Prune / Tokens | States | Breaks |
|---|---|---:|---|
| `SessionTransitionLeaseIdentity.cfg` | own / host / deadOnly / perHost (the code) | 31,481 | nothing |
| `SessionTransitionLeaseIdentityRegistryOwn.cfg` | own / registry | 31,481 | nothing |
| `SessionTransitionLeaseIdentityLockOnlyOwn.cfg` | own / lockOnly | 31,481 | nothing |
| `SessionTransitionLeaseIdentitySharedEngineHost.cfg` | shared / host | 22,361 | `BorrowedFromOwnGuard`, `HostsHoldSeparateLocks` |
| `SessionTransitionLeaseIdentityRegistryShared.cfg` | shared / registry | 22,361 | `LeaseFitsOnlyItsMaker`, `BorrowedFromOwnGuard`, `HostsHoldSeparateLocks` |
| `SessionTransitionLeaseIdentityLockOnlyShared.cfg` | shared / lockOnly | 22,361 | `LeaseFitsOnlyItsMaker`, `BorrowedFromOwnGuard`, `HostsHoldSeparateLocks` |
| `SessionTransitionLeaseIdentityPruneAll.cfg` | own / host / all | 32,521 | `LeaseFitsItsMaker`, `HeldLockStaysRegistered`, `OneHeldLockPerChannel` |
| `SessionTransitionLeaseIdentitySameToken.cfg` | own / host / deadOnly / same | 31,481 | `UnplacedCoversOnlyItsMaker`, `UnplacedHoldsCoveredHostsLock` |
| `SessionTransitionLeaseIdentityChart.cfg` | the code, one channel, two allocations | 241 | nothing |

With one registry per runtime all three ways of comparing hold every rule: the address comparison is then implied by the channel and lock comparison.
With a shared registry only the address comparison keeps `LeaseFitsOnlyItsMaker`, and even then `borrow` accepts another runtime's guard, because `borrow` compares the lock alone.

The committed chart is `SessionTransitionLeaseIdentityChart.cfg`: one channel in two runtimes, each side moving on its own.
The chart hides the steps that stay in one label: a lookup that finds no live lock, and a second handle on a lock.

## Counterexamples against the code

| Config · rule | Shortest path | In the code |
|---|---|---|
| SharedEngineHost · `BorrowedFromOwnGuard`, `HostsHoldSeparateLocks` | bot A takes the guard of c1, bot B borrows it into a lease | Not reachable: the two constructors each build a new `EngineRuntime` (`src/services/discord/runtime_bootstrap/shared_data.rs` `run_bot_build_shared_data`, `src/services/discord/shared_data.rs` `make_shared_data_for_tests_with_barrier`), and nothing clones an engine into a second `SharedData`. A borrow through another runtime looks up that runtime's own registry and fails. |
| RegistryShared, LockOnlyShared · `LeaseFitsOnlyItsMaker` | bot A takes a lease of c1; it matches bot B | Not reachable: needs the shared registry and a comparison the code does not use. |
| PruneAll · `LeaseFitsItsMaker`, `HeldLockStaysRegistered`, `OneHeldLockPerChannel` | bot A holds c1, takes c2 and loses the c1 entry, then takes c1 again | Not reachable: the prune keeps every entry with a live holder (`src/services/discord/session_transition.rs` `SessionTransitionLocks::lock`), and a guard holds its lock. |
| SameToken · `UnplacedCoversOnlyItsMaker`, `UnplacedHoldsCoveredHostsLock` | bot A takes an unplaced lease of c1; it covers bot B's row of c1 | Only between test fixtures, which share one token hash. Production launch configs are deduplicated by token (`src/services/discord/settings/read.rs` `load_discord_bot_launch_configs`). `covers` compares token hash and channel only. |

## Code map

| Model | Code |
|---|---|
| `reg`, `Fresh`, `RegAfter` | `src/services/discord/session_transition.rs` `SessionTransitionLocks::lock`: upgrade, the second upgrade under the entry, replace a dead entry, prune on a vacant insert |
| `AcquireRaw` | `shared.session_transition_locks.lock(channel).try_lock_owned()` and `.acquire(channel)` |
| `AcquireLease`, `Borrow`, `Matches` | `SessionTransitionLease::{try_acquire, borrow, matches}` |
| `AcquireUnplaced`, `Covers` | `UnplacedLease::{acquire, try_acquire, covers}` |
| `Pin`, `LockPin` | a caller that keeps the `Arc` and locks later: `src/services/discord/commands/control.rs` clear, `src/services/discord/health/o_transition.rs` |
| `Wiring = "own"` | `src/services/engine_runtime/legacy_state.rs` holds the registry, one per `EngineRuntime` |

## What the model leaves out

The three-second wait and the unbounded waiters, the owner wake after a release, process restart (leases and registries are in memory only), database row checks, and the intake worker.
Two registries racing on one dead entry are one step here; the code serializes them under the map entry.

## Run

```sh
~/.claude/skills/statechart/scripts/statechart SessionTransitionLeaseIdentity.tla \
  --config SessionTransitionLeaseIdentityChart.cfg --view transition-lease-identity.view.mjs --out /tmp/sc-transition-lease
```
