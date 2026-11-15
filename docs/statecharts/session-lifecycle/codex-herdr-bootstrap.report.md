# CodexHerdrBootstrap

Model written against AgentDesk `3fa2ab95c8068bb0c4a58bc742f65f0db80c5a71` (2026-11-15, issue #8181 design leg 1).
Counterexamples were replayed against that code; dispositions are below.

The Codex canary channel (`codex_tui::canary`, channel 1509350778043895902) runs on Herdr. Its hosted row
is Retired, its spawn nonce marker is gone, and the binding log still ends its codex ownership records with
a tmux-era `verified` context. Herdr launches write no ownership record (`source_policy` None), so that pin
is permanent.

## Why a new module

`HerdrSession` (Claude pane and row lifecycle) and `WriterWake` (writer park, boot custody, restart end) share
one `vars` tuple across 26 and 11 configs. This behaviour needs the verified pin, the spawn nonce marker,
O writer resumption, intake placement and the turn host gate together, and touches neither model's existing
variables. A separate module keeps every existing config unchanged. The `StartLaunch` edge of `HerdrSession`
(Retired row CAS to Pending) is the edge this model gates; see `herdr.report.md`.

## Configs

| cfg | Fix | old tmux may run | result |
|---|---|---|---|
| `CodexHerdrBootstrapBefore.cfg` | before | no | HeldStarts, WriterReady break (RED: permanent refusal) |
| `CodexHerdrBootstrapDesign.cfg` | design | no | all rules hold, including the three liveness properties |
| `CodexHerdrBootstrapDesignLive.cfg` | design | yes or no | all safety rules hold |
| `CodexHerdrBootstrapMutNoVacancy.cfg` | design without the vacancy checks | yes or no | NoDualExecution, SourceInBoundary break |
| `CodexHerdrBootstrapMutPinDrop.cfg` | ignore the pin on a Herdr channel (rejected alternative) | yes or no | NoDualExecution, ResumedOnlyWithEvidence, SourceInBoundary break |

`scripts/ci/statecharts_tlc.py --base 3fa2ab95c8` passed all five against `expected.tsv` (5.3 s total).
Design and DesignLive reach 684 and 708 states, over the 500-state chart budget, so they have no chart.

## Before: HeldStarts and WriterReady break

```
0. writer booting
1. input arrives (held: writer not ready)
2. dcserver restart
3. boot reaches first_drain; writer blocked by the old pin; cleanup done; no marker; input held
   ... stays forever
```

Replay: `actor.rs:357-361` resumes only while `sources_allowed()`; with `owed_delivery_ready` false
(`codex_verified/provenance/admission.rs:49-52`) that is `deliver.rs:140-148`, which needs
`codex_verified_channel_delivery_allowed`. `verified_panes` adds the canary pane even with no runtime binding
(`codex_policy.rs:331-342`), and `disposition` with an absent marker returns VerifiedUnavailable from the
durable canary pin (`codex_policy.rs:44-58`). Placement holds the input (`intake_route/input.rs:97-110`,
reason `fresh_evidence_missing`; fresh admission is tmux-only, `absent_open.rs:62`, `codex_absence.rs:20`).
A turn that does reach the host is refused by `turn_host.rs:308-309` before the launch that would stamp a new
nonce (`herdr_launch.rs:495-499`). Matches the live log `phase=discovery reason=fresh_evidence_missing`.
Confirmed.

## MutNoVacancy: NoDualExecution and SourceInBoundary break

Bootstrap without the pinned tmux target vacancy check launches while the old tmux execution runs; the new
marker makes the pane Legacy and the old source is published. Replay: `herdr_launch.rs:495-503` stamps the
spawn marker before `runtime_marker::clear_fresh` checks tmux presence, and that check runs only when a
runtime-kind marker exists. Confirmed for the bootstrap path; the same ordering on the ordinary writer-ready
path is pre-existing and not widened here.

## MutPinDrop: three safety rules break

Ignoring the pin when the channel is Herdr-configured makes the writer ready with no current evidence
(ResumedOnlyWithEvidence), lets a still running tmux source publish (SourceInBoundary), and lets the ordinary
launch path start a second execution (NoDualExecution). This is why the design keeps the pin and adds an
admission instead.

## Fairness

`SF` on Resume (the actor loop polls repeatedly) and on Unplaced (the resolver polls repeatedly); `WF` on the
other system steps. Cancel, Retire, MarkerLost, TmuxEnds and Restart are environment steps without fairness.
Without `SF` on Resume, a bound new incarnation that retires and loses its marker before the actor's next poll
returns the channel to the bootstrap state with the writer blocked; the next input bootstraps again
(design residual R1).

## Not modelled

The headless turn/start path (`headless_turn.rs:159` -> `turn_host.rs:191` executor_not_wired, issue #8125),
non-canary channels (their `verified_panes` is empty without a runtime binding), Legacy or missing rows
(tmux-era rows with no Herdr ownership), and more than one input at a time.
