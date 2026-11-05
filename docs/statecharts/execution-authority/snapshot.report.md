# Output waits and execution observation

Code baseline: `6249d8796f11b6bbddc82f1df815711b44ce4e17`.
The independent #7768 design review's slice 0/1 contract is the scope of this model.

`ReadMode` separates the old custody observation from the snapshot observation on
`Fix = fix3`. Both use the existing boot-failure, skipped-writer and gateway recovery
transitions. The older configurations keep their custody mode and existing properties.
No new temporal property is added.

`FirstPostBegins` records admission and the owner-change/environment-failure counters.
It checks the current owner, like output acquisition's fresh query, before starting.
`PgChanges` cannot overtake a held output, matching the unchanged exclusive effect guard.
Custody timeout does not increment the independent failure counter: doing that would
incorrectly exempt the bug from `OutputDelayKeepsAdmission`. Snapshot `TickFails` instead
models real pool/DB/local read failures and consumes that failure budget.

| Configuration | Rules | Result |
|---|---|---|
| ExecutionAuthorityCustody | OutputDelayKeepsAdmission | Expected violation, 47 distinct states before counterexample, depth 6 |
| ExecutionAuthoritySnapshot | OutputDelayKeepsAdmission, OutputSerializesOwner, TypeOK, NoForeignPermitHost, SameUnmanagedReadmits, TemporaryReadHolds, NoUnauthorizedUnmanagedHost, NoAmbiguousRecoveryStart | All hold, 2,180 distinct / 6,786 generated states, depth 19 |
| Full statecharts checker | Every registered rule, including historical expectations | 29 configs, matching expectations, 93.0 s |

Plain TLC 1.7.4 returns 12 for the safety violation and 0 for snapshot. The statechart
front end describes a broken-rule result as exit 1; the CI checker understands TLC's
native exit 12 and passes the registered expected violation. This is an expected RED,
not an execution failure being ignored.

## Counterexample replay against code

1. Initial owner A: a managed delivery and active owner are read by
   `tui_o/ownership/execution.rs:141-147`.
2. Prepare installs A's permit before boot restore:
   `discord/runtime_bootstrap/owner_execution.rs:70-129`.
3. StartWriter claims the channel using that permit:
   `tui_o/writer/host.rs` (`start_managed`).
4. TrackWriter publishes readiness under the permit gate:
   `tui_o/writer/host.rs` (`track` / `accepts`).
5. FirstPostBegins holds C through raw HTTP:
   `cluster/intake_router_hook/owner_record/output.rs:94-117,141-155`.
6. TickFails: old `execution.rs:134-140` times out waiting for that C; refresh's temporary
   error branch calls `suspend_execution` (`owner_execution.rs:190-213`), leaving an uncertain
   gate and Held input placement (`tui_o/cutover/intake_route.rs`, `for_input_placement`).

Slice 1 removes only step 6's custody wait. Actual effects still revalidate under C.
The model abstracts the raw wait; the actual guard has an effective upper bound of
about 65 seconds from server `idle_in_transaction_session_timeout` (`output.rs:62`).
The 60-second raw timeout is at `output.rs:147`. Neither is a durable-input completion
claim: fallback adoption and stale-route recovery still wait for C.

The model's observation action is atomic. It does not prove statement-level RR isolation,
local-file atomicity, Discord server cancellation, or all input staging latency. The
implementation keeps three SELECTs in one read-only RR snapshot; raw effect mutation
serialization is additionally checked by the required M2 code mutant.
