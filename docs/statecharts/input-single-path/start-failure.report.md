# Owner start failure slice

Historical RED code SHA: `b0bab6ebf693502f86c0b2e2c0307033fa411c29`.
Live repair code SHA: `5742a64d54e0f92c7740dcd4c767baf06f00dde2`; two-fact contract follows below.
`InputOwnership.tla` / `SFSpec` isolates this policy in the `observer` record; older slices keep their transitions.
The fixed classification is in its StartFailure comment and the fixture header. The historical typed-cause erasure at `host_refusal.rs:28–37` is covered by the live
Discord and headless entrypoint fixtures.

| cfg | Expected break | Interpretation |
|---|---|---|
| StartFailureBefore | StartFailureNoticeOnce, StartAttemptsBounded | Code-confirmed current loop |
| OpaqueNoWakeBefore | FailureEventuallyResolved | Counterfactual NotSent-only repair |
| HoldCountedBefore | HoldNeverCounted | Counterfactual bad classifier |
| TokenWakeBefore | NoStartDuringExactCleanup | Token latch precedes row cleanup |
| KnownPersistent | none | First-failure allowlist retention |
| OpaqueRepeated | none | Second-failure retention |
| OpaqueNoWakeEarlyClear | none | Cause clears before retry; no external wake |
| OpaqueNoWakeLateClear | none | Recovery after retention never replays A |
| CompletionRace | none | Duplicate wakes; effect-held ownership/reset |
| RestartClear | none | Process count reset, reserved adoption, clear, stale callback, user stop |
| HoldThenDelivered | none | Two uncounted holds, one delivery |
| RowClearDelayed | none | Failed removal keeps closing; an existing later settle removes the row |
| EarlyRetainBefore | FirstOpaqueFailureKept, EarlyClearDelivers | Opaque first failure incorrectly retained |
| ReserveResetBefore | ReserveResetsFailures | Reserve incorrectly carries prior failures |
| PolicyRowOnlyBefore | RecoveryCreditPreserved, FailureEventuallyResolved | Early policy unlink loses its wake while the exact token remains active |
| PolicyEarlyRemove | none | Independent row/token events; the last fact clears closing and wakes once |

`FirstOpaqueFailureKept` independently forbids retention or notice after one opaque failure.
`EarlyClearDelivers` requires A's payload even if a bad policy removed it from the queue.
`ReserveResetsFailures` checks the reserve state directly, before an effect-held result can hide it.
The two negative configs inject only those faults; the unmutated policy transitions stay the same.
Full checker: 83 configs, rc=0, expected/actual mismatches=0 (265.4 s, after merging main `3ae403d5ecfe8565a1d508ceff5679066ecf4ff1`).
Isolated traces reproduce EarlyClearDelivers exit 13, FirstOpaqueFailureKept exit 12 and
ReserveResetsFailures exit 12; repository model bytes remain unchanged during those runs.

Historical RED replay at the historical SHA above:
ingress admits A/B (`input_owner/ingress.rs:400–468`); wake grants A (`drive.rs:96–123,168–180`,
`v2.rs:300–313`); wrapper fails before reserve (`owner_input.rs:510–571,702–735`). The bound refusal
is now `provider_dispatch.rs:728–749`, through `claude/herdr_turn.rs:286–287` before its hook.
Generic Error renders the placeholder (`content_arms.rs:437` onward). QueueEligible completion
(`queue_io/backstop.rs:689–723`, `queue_dispatch/kickoff.rs:123`) opens a fresh pass with
`stalled=false`, regranting A for a second and third start. NotSent-only suppression
(`content_arms.rs:418–436`, `terminal_outcome_delivery.rs:247–255`) deletes the placeholder and
emits no completion; quiet recovery supplies no wake. Existing holds are uncounted.

Token release and row deletion are separate actions. `episode_identity.rs:302–320` marks the token
latch before `turn_finalizer/episode.rs:129–145` removes the row. `TokenWakeBefore` preserves that
counterexample. The policy instead clears closing and issues one wake only after the removed row
matches the captured failed row and that episode's exact token has released. The independent `unsafeGrant` witness records a grant while that
failed row remains, even if a bad wake already cleared closing. No row guard hides that witness.
Normal completion releases the token first and waits for row settlement. Policy removal
can reverse those events; its row proof waits for the existing exact token-finished latch. Known refusals without a wrapper wake directly.

`RowClearDelayed` begins with an unavailable store; after token release, closing prevents grants.
A finite recovery followed by an existing later settle is fair, then exact removal wakes once.
No new timer is modeled. Permanent store failure is not assumed to make progress. Fairness of
SFDrive applies only to its wake-gated action. Notice delivery does not gate row cleanup or B.
Counters saturate at start 3, notice 2 and payload 2, with no third-start guard. `starts` excludes
holds/stops and resets at an effect-held boundary; `totalStarts` counts all entries. Reserve resets
failures. Recovery liveness requires a cause that stays cleared and no user clear. Runtime counts
reset at process restart; retained custody survives. The historical `RetryBudgetSurvivesAdoption`
name checks retained custody only. Reserved adoption returns EffectOwned to NotStarted
(`v2/sources.rs:81–84`), without unsent proof; existing boot-row recovery remains in the earlier F slice.

Live contract: use `Rejected { reason: "start_rejected: …" }` with `input.attempt=None`, already
excluded from replay (`v2/retained.rs:236–248,305–311`). Keys are receipt, else `sources::identity`.
Skip `handoff_held`, ACK attemptless notice, close routine/scheduled runs; HTTP projection is follow-up.
Preclaim `NotSent("herdr_root_recovery_held")` is SchedulingHeld, uncounted, with its wake owned by S2.
Inside-start bound-gate failure is StartFailedBeforeInput regardless of string. S0 emulates only S2's frame.
The fixture uses real ingress/drive/intake/producer/bridge/listener, separate POST/delete/payload counts,
an immediate third-start witness, and payload/closed signals rather than sleeps. `live=true` activates
its live oracle; A notices and B summary are counted separately. A blocked outbox
does not hold the drive while cleanup and B payload delivery complete.

## Physical row removal contract

Paths below are relative to `src/services/discord/` at the live comparison checkpoint. Fresh birth pins must
be captured under the existing row lock and notified only after actual unlink/active rename succeeds.
No actor response is awaited while holding that lock. Missing, mismatch, failed I/O, unknown identity
and duplicate notifications cannot release another closing pin or create another wake.

| Physical point | Existing path / authority |
|---|---|
| `inflight.rs:575` | Policy API `remove_inflight_row_for_policy_op`; public JS `src/engine/ops/exec_ops.rs:313–379` |
| `inflight/circuit_removal.rs:74` | Checked delete with circuit handoff |
| `inflight/rebind_reap.rs:311` | Abandoned rebind reap |
| `inflight/rebind_reap.rs:386` | Other-provider abandoned rebind reap |
| `inflight/removal.rs:375` | Existing stale-generation invalidation |
| `inflight/removal/boot_reaper.rs:218` | Locked boot verdict for stale/provider-mismatch/malformed rows |
| `inflight/clear_store/mod.rs:79` | Ordinary receipt-protected clear |
| `inflight/clear_store/mod.rs:321` | Guarded active-path archive rename |
| `inflight/clear_store/mod.rs:483` | User-message-guarded clear |
| `inflight/clear_store/mod.rs:563` | Zero-owned clear |
| `inflight/clear_store/mod.rs:601` | Exact snapshot / path-less Herdr remove_row |
| `inflight/clear_store/mod.rs:650` | Lifecycle death-evidence clear |
| `inflight/clear_store/mod.rs:751` | After-delivery clear |
| `inflight/clear_store/mod.rs:817` | Tmux-response test seam (no current live caller) |
| `inflight/clear_store/abandon.rs:241` | Exact placeholder-abandon cleanup |
| `inflight/clear_store/identity.rs:119` | Exact finalizer CAS, marked-row late settle and other captured clears |

Correction to the initial lane table: the policy API is **not** restricted to malformed, zero-owned
or old-generation rows. It can remove a normal nonzero A, subject to its existing receipt protection,
lock and discard handoff. It belongs to the notification funnel. Its existing public JS test uses
user_msg_id=7 (`exec_ops.rs:671–768`); the live test must add matching closing / wake-once assertions.
The scoped-restore unlink (`inflight/removal/scoped_restore.rs:76`) is behind `#[cfg(test)]`
(`:3`); it is not a seventeenth production removal point and adds no live cleanup authority.

## Two-fact completion

Live code comparison SHA: `0fd788f660d61c9fdd0f46ad7825ccc7c51ff633` plus tested tree
`9d2193bccc293168fd483639a41462e8c6b38fb4`. The public policy API can remove A while its token is
active. Its early wake enters the existing busy handoff without a row and is lost before NotSent
finishes. The real barrier fixture confirms that ordering; SFRowClear now permits it and
SFEarlyWakeConsumed represents that pass. SFRelease and physical row unlink remain separate.
SFClosingComplete clears closing and wakes once only after both facts for the captured episode.

PolicyRowOnlyBefore expects RecoveryCreditPreserved and FailureEventuallyResolved to break.
PolicyEarlyRemove checks the adopted two-fact completion with the same safety/liveness rules.
TokenWakeBefore still rejects a token-only close while the row remains. NoStartDuringExactCleanup
is unchanged. Release fairness assumes the existing token-finished event eventually arrives;
a token that never releases keeps closing held without a new timeout or retry service.
