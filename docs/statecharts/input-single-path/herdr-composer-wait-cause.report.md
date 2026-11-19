# Herdr composer wait cause (#8161 S0)

Model: `HerdrComposerWaitCause.tla`. Code SHA: `a219f6c716` + S0.
Code path: `claude/herdr_turn/composer.rs` `when_idle` → `claude_tui/readiness/watched.rs` `expired`.

## Why
11-19 adk-dash-cc: after the 11:28 restart every Discord input was held as
`Claude composer held before first key: Refused(Unknown)`, 10.2 s after its first key would have gone.
That is the composer's 10 s budget ending on `Expiry::Waits`, which named no branch.
Internal turns ended twice and the hold stayed. The pane, read once (read-only), showed a background
agent's Bash permission dialog where the composer is (design W2).

## Values (from the design entry tables only)
| Variable | Values | Code |
|---|---|---|
| source | ok, none | `Source::current` at the wait's first look |
| fence | none, open | `unread_continuations`/`continuations`, `pending`, `compact_*` |
| screen | empty, unread | `composer_owner` |
| launch | before, this | binding context `created_at` vs daemon `STARTED` |

Actions: `Attempt` (the 10 s budget and its `expired` verdict), `InternalTurnEnd` (task-notification
turn; owner wake; clears no source, fence or dialog), `SourceAppears` (a launch of this run publishes
its source late), `Quiet`.

## Rules
- `KeysNeedProof`, `SettleOnlyCauseless` (invariants).
- `Resolution == <>(queued ∈ {sent, settled})` — the input is eventually sent or settled with its notice.
- `LaunchDelivers == <>(queued = sent)` (launching configs) — S0 never settles a launch of this run.

## Results (TLC 1.7.4, `scripts/ci/statecharts_tlc.py`, 11/11 as expected)
| cfg | broken | meaning |
|---|---|---|
| BeforeSourceNone | Resolution | current code holds a lost source for good |
| FixedSourceNone | - | S0 settles it with the retained notice |
| BeforeFence / FixedFence | Resolution | expected: an open fence still waits (S0 only names it) |
| BeforeUnread / FixedUnread | Resolution | expected: W2 (dialog over composer) still waits; follow-up |
| Before/FixedHealthy | - | |
| Before/FixedLaunching | - | a late source of this run still delivers |
| MutantSettleAnyLaunching | LaunchDelivers | dropping the launch guard settles an input that would deliver |

## S0b: a permission dialog over the composer
Code: `composer.rs` `when_idle` read `composer_owner` before its modal check, so a background agent's
permission prompt that replaces the composer read Unread and waited out the budget (`Waits(Observed)`).
The modal check now runs first and holds at once as `Waits(Modal)`. The held watch already existed:
`first_input.rs` records a probe (empty composer, no modal) that `held_watch.rs` reads every 2 s and that
wakes the drive once; 11-19 14:47 production delivered the 12:10 original that way after the prompt closed.

Added: `screen = dialog`, `DialogShown` (only while held), `DialogDismissed` (wakes unless mutant
`NoRewake`), invariant `DialogNamed` (a front held under a dialog says Modal).

| cfg | broken | meaning |
|---|---|---|
| BeforeDialog | DialogNamed | before: the dialog hold is named an observed wait |
| FixedDialog | - | named Modal; dismissal wakes and delivers |
| BeforeDialogStays | DialogNamed, Resolution | |
| FixedDialogStays | Resolution | expected: a prompt nobody answers keeps the input held |
| MutantNoRewakeDialog | Resolution | without the held-watch wake the dismissed dialog never delivers |

TLC 16/16 as expected.
