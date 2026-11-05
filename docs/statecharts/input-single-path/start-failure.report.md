# Owner start failure slice

Code SHA: `b0bab6ebf693502f86c0b2e2c0307033fa411c29`; production unchanged.
`InputOwnership.tla` / `SFSpec` isolates this policy in the `observer` record; older slices keep their transitions.
The fixed classification is in its StartFailure comment and the fixture header. `host_refusal.rs:28–37`
currently erases typed causes; preserving them belongs to the live slice.

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

GREEN checks the proposed policy, not unchanged production behavior. Code-confirmed replay:
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
counterexample. The policy instead clears closing and issues one wake only when the removed row
matches the captured failed row. The independent `unsafeGrant` witness records a grant while that
failed row remains, even if a bad wake already cleared closing. No row guard hides that witness.
Normal completion waits for row settlement. Known refusals without a wrapper wake directly.

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
its future oracle; PG outbox delivery is wired in the live slice.
