# Readiness wait whose transcript moves once and then stops (#8217)

Design base: `origin/main` fdd44b59c7. Implementation: ddf6590120, on the #8214 review head fc59e1199a (same judgment
files). Rows marked "base" keep the coordinates of the code before the fix; the others are of the implementation.
Model: `ReadinessRequiet.tla`, 24 configs. View: [readiness-requiet.view.mjs](readiness-requiet.view.mjs).
Design: [8217-unproven-stamp-settlement.design.md](../../design/8217-unproven-stamp-settlement.design.md).

`HerdrIdleMeta.tla` (#8214, not on main yet) fixes the transcript tail per config and has no Stamp, no budget and no
settlement, and `ClaudeReadiness.tla`'s `Unproven` reads no Stamp either. This model is the small one that adds them.

## What the model stands for

| Element | Meaning | Code it stands for |
|---|---|---|
| `restarted`, `proof = FALSE` | the daemon restarted; the ledger lost this source's evidence | `readiness/mod.rs:46-51` `Entry::restart` |
| `StartWait`, `moved = FALSE` | the wait's first look keeps the Stamp | `readiness/watched.rs:19-30` `watched`, `readiness/wait.rs:30`, `claude/herdr_turn/composer.rs:145` |
| `MetaMoves` (`neutral`) | the idle pane appends session metadata (`ai-title`); the turn state stays Idle | `tui_turn_state.rs:635` (`_ => None`, the walk goes back to the terminal) |
| `MetaMoves` (`marker`) | a lone `permission-mode`; the turn state reads Unknown | `tui_turn_state.rs:630` |
| `Timeout`, `Policy = "Before"` | base: `Watched::unproven` is false once the Stamp differs, so the hold waits | base `readiness/mod.rs:385-389`, `input.rs:391-399`, `composer.rs:192-193` |
| `held`, `Wake` | the held watch wakes the drive only when its probe reads a proof; an owned channel has no timed wake | `held_watch.rs:62-68`, `watch()` in `readiness/mod.rs`, `herdr_turn/first_input.rs:204-231`, `turn_presence/supervisor.rs:688-695` |
| `Retain`, `Notice` | proven-unsent settlement and its one resend notice | `input_owner/effect.rs:116-119`, `:184-189`, `queued_head/v2/retained.rs:166-204`, `input_owner/notice.rs:60-93` |
| `requiet`, `WindowEnds`, `Rounds` | the same wait again on a fresh Stamp, at most `Rounds` short windows | `readiness/watched.rs:9-10`, `:105-119` `Watched::expired`, `readiness/wait.rs:51-56`, `composer.rs:181-185` |
| `turn = "running"`, `"fenced"` | a turn the transcript shows; a pending, continuation or compact fence in the ledger | `readiness/watched.rs:71-104` |
| `turn = "unrecorded"`, `PersonSubmits`, `LateRecord` | a person's Enter whose hook this run lost and whose record is not written yet: only the screen shows it | `readiness/idle.rs:38-43` `shows_no_turn`, read at `readiness/watched.rs:116` |
| `restorable` (Herdr) | the restart proof over an idle screen; neutral metadata keeps it, a marker ends it | `readiness/mod.rs:317-335`, `composer.rs:170` |
| `Churn` | metadata keeps arriving in every window | `readiness/watched.rs:108-119` (the windows are counted, then the screen decides) |

Not modelled: a later input, Cancel, clear and a new incarnation (they end or replace the held front through paths this
design does not touch, design §2.3), and the composer's draft (the design reads only whether the screen is busy).

## Results

Run with `scripts/ci/statecharts_tlc.py --base origin/main` (plain TLC 1.7.4) at the implementation commit:
24/24 `ReadinessRequiet` configs match `expected.tsv` (38/38 with the #8214 model, 37.4 s). The six charted configs were also run with the statechart skill in the design leg.

| Config | Scenario | Result |
|---|---|---|
| `ReadinessRequietTmuxOnceBefore.cfg` | current code, tmux, metadata once | RED `InputEventuallyResolves` (11 states): held forever ([chart](readiness-requiet.before.chart.mmd)) |
| `ReadinessRequietTmuxOnceFixed.cfg` | design, tmux, metadata once | GREEN (11): one window, retained, noticed ([chart](readiness-requiet.fixed.chart.mmd)) |
| `ReadinessRequietHerdrOnceBefore.cfg` | current code, Herdr without a restart proof | RED `InputEventuallyResolves` ([chart](readiness-requiet.herdr-before.chart.mmd)) |
| `ReadinessRequietHerdrOnceFixed.cfg` | design, Herdr | GREEN ([chart](readiness-requiet.herdr-fixed.chart.mmd)) |
| `ReadinessRequietHerdrRestoredFixed.cfg` | design, Herdr restart proof, metadata twice, a person may submit | GREEN: delivered, or held until that turn ends |
| `ReadinessRequietTmuxUnmovedBefore.cfg` | current code, nothing moves | GREEN: the existing unmoved settlement |
| `ReadinessRequietTmuxRunningFixed.cfg`, `…HerdrRunningFixed.cfg` | design, a turn runs while metadata arrives | GREEN (33): never retained, delivered after the turn ends ([chart](readiness-requiet.running.chart.mmd)) |
| `ReadinessRequietTmuxFencedFixed.cfg` | design, a fence is open while metadata arrives | GREEN |
| `ReadinessRequietTmuxPersonFixed.cfg`, `…HerdrPersonFixed.cfg` | design, a person's unrecorded Enter | GREEN: a busy screen holds the moved branch |
| `ReadinessRequietTmuxChurnFixed.cfg`, `…HerdrChurnFixed.cfg` | design, metadata in every window | GREEN (19): settles when the windows are spent ([chart](readiness-requiet.churn.chart.mmd)) |
| `ReadinessRequietTmuxOnceStampOnceKeeps.cfg` | mutant: a Stamp that differed once keeps the hold | RED `InputEventuallyResolves` |
| `ReadinessRequietTmuxFencedIgnoreRunning.cfg` | mutant: the moved branch skips the turn and fence checks | RED `NoSettleOverOpenTurn` |
| `ReadinessRequietTmuxPersonIgnoreScreen.cfg` | mutant: the moved branch skips the screen | RED `NoRequietSettleOverBusyPane` |
| `ReadinessRequietTmuxOnceQuietSendsKey.cfg` | mutant: a quiet window sends the keys | RED `NoKeyWithoutEvidence` |
| `ReadinessRequietTmuxOnceDeliverSkipsEvidence.cfg` | mutant: the ordinary delivery drops its evidence guard | RED `NoKeyWithoutEvidence` |
| `ReadinessRequietTmuxChurnNoRoundCap.cfg` | mutant: no bound on the windows | RED `InputEventuallyResolves` |
| `ReadinessRequietTmuxChurnExhaustHolds.cfg` | mutant: spent windows hold instead of settling | RED `InputEventuallyResolves` |
| `ReadinessRequietTmuxMarkerBefore.cfg`, `…MarkerFixed.cfg` | a lone `permission-mode` arrives | RED `InputEventuallyResolves` before and after: the Unknown tail is another cause, left open (design §8) |
| `ReadinessRequietTmuxKnownGapBefore.cfg`, `…KnownGapFixed.cfg` | nothing moves, a person's unrecorded Enter | RED `UnmovedSettleSeesPane` before and after: the existing unmoved branch never reads the screen; not widened (design §8) |

Every config checks `TypeOK`, `NoKeyWithoutEvidence`, `NoSettleOverOpenTurn`, `NoRequietSettleOverBusyPane`, `OneEnd`,
`RequietIsBounded`, no deadlock, and the properties `ScenarioReached` and `InputEventuallyResolves`
(`queued ~> delivered \/ explained`). The two KnownGap configs add `UnmovedSettleSeesPane`.

## `NoKeyWithoutEvidence` reads the state

The rule was `"key" \notin bad`, and only the `QuietSendsKey` branch ever put `"key"` there, so a delivery that lost
its evidence guard stayed GREEN. It is now `keys = 0 \/ Deliverable`, read from the state; nothing in the model
withdraws the evidence once the input is done.

| Config | Rule before | Rule after |
|---|---|---|
| `ReadinessRequietTmuxOnceDeliverSkipsEvidence.cfg` (new mutant) | GREEN, 14 states: `keys = 1` with `proof = FALSE` passes | RED `NoKeyWithoutEvidence` (5 states) |
| `ReadinessRequietTmuxOnceQuietSendsKey.cfg` | RED `NoKeyWithoutEvidence` (11 states) | RED `NoKeyWithoutEvidence` (11 states) |
| the other 22 configs | as in `expected.tsv` | unchanged |

## Counterexample replayed against the code (base coordinates)

`TmuxOnceBefore`: restarted → owner input → wait starts (Stamp kept) → the pane appends metadata → the budget ends →
held, and nothing more happens. In the code: `input.rs:375` keeps the Stamp, `readiness/mod.rs:385-389` returns false
for the moved Stamp, `input.rs:392-399` returns `Held(idle_evidence_pending, unproven = false)`, `effect.rs:184-189`
maps it to `Completion::Held`, `effect.rs:116-117` holds the front, `drive.rs:113-115` starts the held watch, and the
probe (`watch()` in `readiness/mod.rs`) needs `observe()` to be `Some`, which an idle source without new activity never is.
`HerdrOnceBefore` is the same through `composer.rs:192-193` and `first_input.rs:232-240`. The existing test
`a_timed_out_hold_tells_an_unmoved_idle_source_from_a_running_turn` (`readiness_tests.rs`) asserted the first half
at base: after one `ai-title` the kept look's `unproven()` was false. It now asserts the windows and their bound.

## What the mutants say about the tests

`IgnoreRunning` over a recorded running turn is masked by the screen check in the model; it shows only over a fence,
whose screen is idle. The implementation tests therefore cover the three guards separately: a transcript that shows
a turn under an idle-looking screen, a fence, and a busy screen over an idle transcript
(`a_moved_transcript_under_a_turn_a_fence_or_a_busy_screen_keeps_the_front_waiting`, Herdr owner boundary). The entry
tests are `a_wait_whose_transcript_moved_once_ends_retained_with_one_resend_notice_pg` (Herdr) and
`an_owner_input_whose_idle_transcript_moved_once_ends_retained_without_a_key` (tmux).
