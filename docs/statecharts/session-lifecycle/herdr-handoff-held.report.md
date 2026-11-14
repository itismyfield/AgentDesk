# Handoff journal that outlives its channel's Herdr mapping

Baseline: `1a1b0b1823be8bb25fc9e5ab4be4a914e8d814d8`. Companion to
[HerdrHandoffLaunch](herdr-handoff-launch.report.md): that model starts from a
launch that fails; this one starts from a journal no launch will ever consume.

Observed start state: `unfinished_handoff` with `ended=true`, `failures=0`, its
original still the queue front, and boot config no longer routing the channel to
Herdr. Every look answered `NotSent("claude_handoff_held")` and a one-second timer
woke the next look, without a count, a deadline or a notice.

Decision: boot config is the handoff target. A journal on a channel that left
Herdr is dropped when its old pane ended or its original is no longer queued, and
the front takes the ordinary path. A hold no later look of the journal clears
(the channel left Herdr before the old pane ended, or a mapped channel's host
refuses an ended journal) spends the journal's existing launch budget: durable
count, `1 << failures` second deadline, and on the fifth the existing
start-failure retention with its notice. A mapped channel whose host is Herdr
keeps the journal until the close and the Herdr reserve, unchanged. A mapped
channel's unended journal whose host refuses keeps custody as a failed close
does: the durable count grows its backoff to 32 seconds and nothing is retained.

## Cause on the baseline

| Step | Code | Effect |
|---|---|---|
| Journal selects the handoff path | `queue_io/input_owner/claude_handoff.rs:37-39,54`; `judge` `:223-228` skips the mapping check when a journal exists | an unmapped channel is still judged as a handoff |
| Hold without a count | `:58-70` (session key unresolved, fixed `1`) and `:72-83` (host not Herdr, the journal's unchanged `failures`); `hold` `:286-300` only withdraws the grant | `failures` stays 0, no deadline, no retention |
| One-second wake | `schedule` `:301-361`: `1_000 << failures` ms, then `recover` and `owner_wake` `:354-355`; `recover` `:416` schedules the same key again | the drive (`drive.rs:100-104`) repeats the same front forever |

## Journal lifecycle census

`git grep -n -E 'begin_handoff|finish_handoff|abandon_handoff|handoff_held|handoff_start_failed|handoff_failed|handoff_ended|unfinished_handoff|\.handoff\(|claude_handoff::(prepare|recover|retry_start)' -- src`
(tests excluded); coordinates are the fixed tree's.

| Entry | Code | Journal effect |
|---|---|---|
| Begin / reuse | `queued_head/v2/claude_handoff.rs` `begin_handoff`; `input_owner/claude_handoff.rs` `prepare` | created `ended=false, failures=0` for a Legacy or absent row on a mapped channel |
| Pin / old pane ended / close failure | `handoff_pin`, `handoff_ended`, `handoff_failed` | exact pane, `ended=true`, uncapped close count with one retry notice |
| Channel left Herdr (new) | `abandon_handoff`; `prepare` before any judgement, `recover` at boot and on each timer | dropped when ended or its original is gone; otherwise kept |
| Counted hold (new) | `handoff_held`; `prepare` `spend` | count + deadline; the fifth retains the front through `start_failed` |
| Refused host, old pane standing (changed) | `handoff_failed`; `prepare` | counted as a close failure: backoff instead of a one-second hold, custody kept |
| Launch failure before reserve | `handoff_start_failed`; `provider_dispatch/start_failure.rs` | count + deadline, fifth retained |
| Retention releases the journal | `queued_head/v2/start_failure.rs` `start_failed` | cleared when ended or its original left the queue |
| Herdr reserve | `queued_head/v2.rs` `place` | consumed with the attempt; any other host is refused while a journal exists |
| Bound row / input-free finish | `prepare`, `recover`; `finish_handoff` | cleared once its original is no longer queued |
| Boot census and adoption | `pending_queue_persistence/queued_recovery.rs`; `queued_head/compat.rs`; `provider_dispatch/owner_input.rs` `recover` | a journal alone selects the actor; values restored as saved |
| Cancel / clear / clear waiting | `queued_head/v2/control.rs` | cleared with its original or with the waiting items |
| Herdr mapping change | boot config only (`config/session_hosts.rs` `has_herdr_endpoint`) | no journal write; the next `prepare` or `recover` reads the new mapping |

## Model and bounds

`HerdrHandoffHeld.tla`, two originals, `Cap = 5`, restored `failures` in `0..Cap`.
`Mapped` is constant for a run, as boot config is for a process. `wait` counts the
persisted deadline down; a wake before it holds without a count and is not
modelled as a step. Publication of each snapshot succeeds and the outbox
eventually posts. The session-key-unresolved hold of a mapped channel, a journal
naming another session key, and the transition-lock, active-turn and close-retry
holds of a real handoff are outside this model; their wakes are the existing ones.

| Rule | Meaning |
|---|---|
| `BoundedHolds` (safety) | the timer re-holds one front at most `Cap` times |
| `OnlyEndedLeftReleased` (safety) | only an ended journal of a channel that left Herdr is dropped with its input queued |
| `EndedLeftDelivers` (safety) | that drop never costs the input |
| `InProgressDelivers` (safety) | a real handoff is never cut short into a retained input |
| `InputOnce` (safety) | no original is delivered or retained twice |
| `FrontProgress` (liveness) | every front is delivered, or retained and told |

## TLC result

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py`; twelve configs, every
expected violation matched. InProgressRefused stays RED by decision: an unended
handoff of a mapped channel is never retained, so a host that refuses forever
holds its front forever, at the 32 second backoff a persistent close failure has.

| Config | Start | Safety | Front delivered or visibly retained |
|---|---|---|---|
| Before | left Herdr, ended | RED BoundedHolds | RED FrontProgress |
| BeforeUnended | left Herdr, unended | RED BoundedHolds | RED FrontProgress |
| BeforeRefused | mapped, host refuses | RED BoundedHolds | RED FrontProgress |
| Fixed | left Herdr, ended | GREEN | GREEN (delivered) |
| FixedUnended | left Herdr, unended | GREEN | GREEN (retained and told) |
| FixedRefused | mapped, host refuses | GREEN | GREEN (retained and told) |
| InProgress | mapped, Herdr host, unended | GREEN | GREEN (closed, then delivered) |
| InProgressRefused | mapped, host refuses, unended | RED BoundedHolds (custody kept) | RED FrontProgress |
| NoRelease (mutant) | left Herdr, ended | RED EndedLeftDelivers | GREEN |
| NoCount (mutant) | left Herdr, unended | RED BoundedHolds | RED FrontProgress |
| AlwaysLeft (mutant) | mapped, Herdr host, ended | RED OnlyEndedLeftReleased | GREEN |
| ReleaseUnended (mutant) | left Herdr, unended | RED OnlyEndedLeftReleased | GREEN |
