# ExecutionAuthority — the owner refresh loop and the writer host it waits on (#7731)

Model written against AgentDesk `origin/main` `b66f40ba190f73d78ac1b5aa9e8f41a719af37e5` (`Fix = "main"`)
and the #7731 change (`Fix = "fix1"`). `Fix = "nowait"` is a mutant that skips the retirement wait for every host.

| `Fix` | Config | Bounds | Result |
|---|---|---|---|
| `main` | `ExecutionAuthorityMain.cfg` | 1 PG failure, 0 owner changes | `AdmissionResumes` breaks (exit 13) |
| `fix1` | `ExecutionAuthority.cfg` | 2 PG failures, 2 owner changes | all rules hold (exit 0) |
| `nowait` | `ExecutionAuthorityNoWait.cfg` | 2 PG failures, 2 owner changes | `NoForeignPermitHost` breaks (exit 12) |

What the model covers:

- One O channel of one provider, owned by this node, refreshed by the 2 s loop (`owner_execution::refresh`).
- A read that fails (timeout, pool, custody lock timeout) and is `temporary`, a read that returns `None` (unmanaged),
  and a read that returns a permit of the same or another owner tuple.
- The writer host and the gate it obeys: the provider lease gate (gateway boot of an unmanaged channel)
  or a permit gate (a managed channel, or `spawn_execution`). Only a closed permit gate retires its host.
- The 3 s wait for the host to retire after `execution identity changed`, ending in install or in `continue`.

What it leaves out: lease loss and re-acquisition, host crashes, other channels, the O output path.
A managed channel never becomes unmanaged (its routed `intake_outbox` rows stay).

Rules:

- `NoForeignPermitHost` (safety): a permit host never runs while the view holds another owner's permit,
  so another owner's permit is installed only after the old permit host stopped.
- `AdmissionResumes` (liveness, `<>[]Admitted`): once PG answers, live input is admitted again and stays admitted.
  `Admitted` is "no held view" (`claim_for_placement`) and "a host whose gate is owned" (`Readiness::accepts`).
- Fairness: weak fairness on the refresh tick, both wait outcomes, host retirement and `spawn_execution`.
  PG failures and owner changes are bounded (`MaxFail`, `MaxChange`), so PG answers from some point on.
  This matches production: the loop sleeps 2 s and runs again, a closed permit gate retires its host at once,
  and PG answers once the pool or lock contention clears.

Run it like this (exit 13 or 12 means a rule breaks):

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/execution-authority
$JAVA -cp $TLA2TOOLS tlc2.TLC -config ExecutionAuthorityMain.cfg ExecutionAuthority.tla
```

## main: AdmissionResumes breaks

TLC reached 10 states (6 distinct). The counterexample has 3 states and a loop:

1. Gateway boot of an unmanaged channel: no view, a host under the lease gate, input admitted.
2. One read fails: a held view with nothing retained. Admission stops.
3. PG answers `None`: the view is still there, so `execution identity changed`, then the refresh waits for the host.
4. The wait times out (the lease host never retires) and the loop is back at state 2, forever.

With 2 failures and 2 owner changes TLC also finds the owner-change entry: a lease host whose view
holds owner A sees owner B, closes A's gate and waits on the same lease host (7 states, loop back to 5).

## fix1: all rules hold

343 states generated, 162 distinct, depth 12. A host under the lease gate does not wait: the refresh installs the observation at once.
A permit host still waits until its closed gate retires it.

## nowait: NoForeignPermitHost breaks

Skipping the wait for every host installs owner B while owner A's permit host still runs (3 states: boot with A's permit host,
PG moves to B, the refresh installs B). The safety rule catches that, so `fix1` keeps the wait for permit hosts.
