# Readiness wait whose transcript moves once and then stops (#8217, design leg)

Base: `origin/main` fdd44b59c7, read together with the #8214 review head 462046ed0b (same judgment files).
Model: `ReadinessRequiet.tla`, 23 configs. View: [readiness-requiet.view.mjs](readiness-requiet.view.mjs).
Design: [8217-unproven-stamp-settlement.design.md](../../design/8217-unproven-stamp-settlement.design.md).

`HerdrIdleMeta.tla` (#8214, not on main yet) fixes the transcript tail per config and has no Stamp, no budget and no
settlement, and `ClaudeReadiness.tla`'s `Unproven` reads no Stamp either. This model is the small one that adds them.

## What the model stands for

| Element | Meaning | Code it stands for |
|---|---|---|
| `restarted`, `proof = FALSE` | the daemon restarted; the ledger lost this source's evidence | `readiness/mod.rs:44-48` `Entry::restart` |
| `StartWait`, `moved = FALSE` | the wait's first look keeps the Stamp | `readiness/mod.rs:359-369` `watched`, `claude_tui/input.rs:375`, `claude/herdr_turn/composer.rs:145` |
| `MetaMoves` (`neutral`) | the idle pane appends session metadata (`ai-title`); the turn state stays Idle | `tui_turn_state.rs:629` (`_ => None`, the walk goes back to the terminal) |
| `MetaMoves` (`marker`) | a lone `permission-mode`; the turn state reads Unknown | `tui_turn_state.rs:624` |
| `Timeout`, `Policy = "Before"` | `Watched::unproven` is false once the Stamp differs, so the hold waits | `readiness/mod.rs:385-389`, `input.rs:391-399`, `composer.rs:192-193` |
| `held`, `Wake` | the held watch wakes the drive only when its probe reads a proof; an owned channel has no timed wake | `held_watch.rs:62-68`, `readiness/mod.rs:778-799`, `herdr_turn/first_input.rs:204-231`, `turn_presence/supervisor.rs:688-695` |
| `Retain`, `Notice` | proven-unsent settlement and its one resend notice | `input_owner/effect.rs:116-119`, `:184-189`, `queued_head/v2/retained.rs:166-204`, `input_owner/notice.rs:60-93` |
| `requiet`, `WindowEnds`, `Rounds` | the design: the same wait again on a fresh Stamp, at most `Rounds` short windows | design §4 |
| `turn = "running"`, `"fenced"` | a turn the transcript shows; a pending, continuation or compact fence in the ledger | `readiness/mod.rs:390-419` |
| `turn = "unrecorded"`, `PersonSubmits`, `LateRecord` | a person's Enter whose hook this run lost and whose record is not written yet: only the screen shows it | screen check in design §4.3 |
| `restorable` (Herdr) | the restart proof over an idle screen; neutral metadata keeps it, a marker ends it | `readiness/mod.rs:315-333`, `composer.rs:166` |
| `Churn` | metadata keeps arriving in every window | design §4.2 (exhaustion) |

Not modelled: a later input, Cancel, clear and a new incarnation (they end or replace the held front through paths this
design does not touch, design §2.3), and the composer's draft (the design reads only whether the screen is busy).

## Results

Run with `scripts/ci/statecharts_tlc.py --base origin/main` (plain TLC 1.7.4): 23/23 match `expected.tsv`, 35.3 s.
The six charted configs were also run with the statechart skill.

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
| `ReadinessRequietTmuxChurnNoRoundCap.cfg` | mutant: no bound on the windows | RED `InputEventuallyResolves` |
| `ReadinessRequietTmuxChurnExhaustHolds.cfg` | mutant: spent windows hold instead of settling | RED `InputEventuallyResolves` |
| `ReadinessRequietTmuxMarkerBefore.cfg`, `…MarkerFixed.cfg` | a lone `permission-mode` arrives | RED `InputEventuallyResolves` before and after: the Unknown tail is another cause, left open (design §8) |
| `ReadinessRequietTmuxKnownGapBefore.cfg`, `…KnownGapFixed.cfg` | nothing moves, a person's unrecorded Enter | RED `UnmovedSettleSeesPane` before and after: the existing unmoved branch never reads the screen; not widened (design §8) |

Every config checks `TypeOK`, `NoKeyWithoutEvidence`, `NoSettleOverOpenTurn`, `NoRequietSettleOverBusyPane`, `OneEnd`,
`RequietIsBounded`, no deadlock, and the properties `ScenarioReached` and `InputEventuallyResolves`
(`queued ~> delivered \/ explained`). The two KnownGap configs add `UnmovedSettleSeesPane`.

## Counterexample replayed against the code

`TmuxOnceBefore`: restarted → owner input → wait starts (Stamp kept) → the pane appends metadata → the budget ends →
held, and nothing more happens. In the code: `input.rs:375` keeps the Stamp, `readiness/mod.rs:385-389` returns false
for the moved Stamp, `input.rs:392-399` returns `Held(idle_evidence_pending, unproven = false)`, `effect.rs:184-189`
maps it to `Completion::Held`, `effect.rs:116-117` holds the front, `drive.rs:113-115` starts the held watch, and the
probe `readiness/mod.rs:783-797` needs `observe()` to be `Some`, which an idle source without new activity never is.
`HerdrOnceBefore` is the same through `composer.rs:192-193` and `first_input.rs:232-240`. The existing test
`a_timed_out_hold_tells_an_unmoved_idle_source_from_a_running_turn` (`readiness_tests.rs:544-567`) asserts the first
half: after one `ai-title` the kept look's `unproven()` is false.

## What the mutants say about the tests

`IgnoreRunning` over a recorded running turn is masked by the screen check in the model; it shows only over a fence,
whose screen is idle. The implementation tests therefore cover the three guards separately: a transcript that shows
a turn under an idle-looking screen, a fence, and a busy screen over an idle transcript.
