# Resumed execution: first input over old history

Production base: 1a1b0b1823. Model: `ClaudeReadiness.tla` through `InputPath.tla` (`ReadinessSpec`), N = 2.

A Claude execution relaunched with `--resume` idles at its composer over the transcript its earlier run left.
Its own accepted `SessionStart(resume)` is in the ledger and its launch attached the transcript, but no Stop of
this run exists yet. Before this change a queued input stayed held until a person typed a turn into the pane.

## Start state

`history` is what the earlier run left under the resumed start:

| `history` | Transcript tail when the run resumes | Shared observer | Readiness terminal |
|---|---|---|---|
| `none` | no file, or empty | Idle | not needed: the start admits an empty transcript |
| `meta` | `turn_duration`, then exit and resume metadata (`last-prompt`, `cost-state`, `tools-used`) | Idle | none: an unknown record cancels the terminal |
| `cut` | a turn the earlier run never finished, or a `permission-mode` marker last | Running or Unknown | none |

`ready-resume` (already in the model) is `none`. The new configs start at `meta` and `cut`.

## Every path that creates or reads the source

| Path | Code | Ledger effect |
|---|---|---|
| New start, resume, clear: accepted `SessionStart` of the current incarnation | `claude_tui/readiness/mod.rs:385 note_hook`, start at `:468` | `starts`, `entry.start`; a resume also records the transcript length and launch time (`:472`) |
| Stop and UserPromptSubmit of the current incarnation | `readiness/mod.rs:497` | an entry behind a recovery floor; a prompt arms `pending` |
| First observation with no entry (daemon restart, lost hook) | `readiness/mod.rs:138` | an entry behind a recovery floor at the daemon's start |
| Herdr launch or handoff from tmux, fresh or resumed | `claude/herdr_turn/handoff.rs:49 launched_source` → `discord/tui_prompt_relay/herdr_source.rs:154` (binding at `:187`) | runtime binding after the launch's own `SessionStart` |
| Herdr reattach after a daemon restart | `discord/tui_prompt_relay/herdr_source.rs:274`, then `claude/herdr_turn.rs:283 bound_source` | runtime binding only; no start, so the recovery floor applies |
| tmux launch | `claude.rs:2002` | runtime binding |
| End of a Herdr turn | `claude/herdr_turn/first_input.rs:242` | runtime binding at the turn's end |
| Source selection | `readiness/source.rs:23 Source::current` | reads marker nonce, context, `starts`, binding, entries |
| Judge | `readiness/mod.rs:84 observe`, resumed start at `:221`; `readiness/evidence.rs:58 quiet_from` | proof only |
| End of a readiness budget | `readiness/mod.rs:249 watched`, `:263 unproven`; Herdr `claude/herdr_turn/composer.rs:164`, tmux `claude_tui/input.rs:391` | none: says whether a later look can still clear the wait |

## Before and after

| Config | Rule | Before | After |
|---|---|---|---|
| `InputPathReadyResumeMetaBefore.cfg` | `ReadyPendingEventuallyExecutes`, `ReadyResumedIdleEventuallyReceives` | RED (8-step lasso, held forever) | — |
| `InputPathReadyResumeMetaAfter.cfg` | every listed rule | — | GREEN (1,190 states) |
| `InputPathReadyResumeCutBefore.cfg` | the same two liveness rules | RED | — |
| `InputPathReadyResumeCutAfter.cfg` | every listed rule | — | GREEN |
| `InputPathReadyResumePersonAfter.cfg` | every listed rule; a person types first | — | GREEN |
| `InputPathReadyResumeNoQuiet.cfg` | `ReadyNoIdleRunningInjection` | — | RED as a mutant: the start admits after a turn began |

The before counterexample is [resume-history-before.chart.mmd](resume-history-before.chart.mmd); the mutant's is
[resume-history-no-quiet.chart.mmd](resume-history-no-quiet.chart.mmd). The after run exceeds the chart budget.
`scripts/ci/statecharts_tlc.py --base 1a1b0b1823`: 65 configs, every broken rule as listed in `expected.tsv`.

## Daemon restart over the resumed session

The daemon restarts while the resumed session idles, attaches the same transcript again, and a queued input
arrives. The restart drops the start; the first observation puts a recovery floor at the transcript's end, and
nothing after it will ever be written by an idle pane. Before this change the readiness wait timed out into a
hold whose watch never ended, so the original kept the front of the queue for good.

Now a wait that ends on an unproven source ends the attempt: no key, the original retained as proven unsent,
one resend ask, the front released, and nothing resubmits it. Unproven means the transcript did not move during
the budget, the shared observer reads Idle, and no reservation, continuation fence or compact is open. A turn
still running, a person's draft and a modal are not unproven and wait as before.

| Config | Rule | Result |
|---|---|---|
| `InputPathReadyResumeRestartBefore.cfg` | `ReadyPendingEventuallySettles` | RED: held forever |
| `InputPathReadyResumeRestartAfter.cfg` | every listed rule, `ReadyRetainedOnce`, `ReadyExpiryOnlyUnproven` | GREEN |
| `InputPathReadyResumeRestartPerson.cfg` | the same; a person may type a turn after the restart | GREEN: retained, or executed after that turn ends |
| `InputPathReadyResumeRestartAnyTimeout.cfg` | `ReadyExpiryOnlyUnproven` | RED as a mutant: a boot or a running turn is retained too |
| `InputPathReadyResumeRestartGap.cfg` | `ReadyPendingEventuallyExecutes`, `ReadyResumedIdleEventuallyReceives` | RED by design: after the restart the input ends in a visible failure, it is not delivered |
| `InputPathReadyResumeRestartCut.cfg` | `ReadyPendingEventuallySettles` | RED, known limit: history cut short reads Running, which no budget tells from a live turn |

`ready-restart` is unchanged and still lets a restart at Idle rebuild readiness from the old terminal, which
the code does not do; only the `ready-resume-restart-*` modes carry the recovery floor.

## Model to code

| Model | Code |
|---|---|
| `ResumedStart` with `r.turn = 0` | `Entry::resumed_quiet`: start unconsumed, and `Tail::quiet_from` finds no turn record or `permission-mode` marker after the start's transcript length, and none stamped at or after the launch |
| `ResumedStart` replaces `observed = "Idle"` | the resumed start passes the shared-observer gate in `observe` |
| `PersonTurn` | a `user` record after the resume; the start closes and that turn needs its own approved terminal |
| `ResumeNoQuiet` | `quiet_from` removed |
| `CompactSubmit` keeps `r.turn > 0` | `compact.rs:104`: a proof resting on the resumed start alone starts no compact |
| `Restart` with `restored = "floor"` | the ledger is empty again; `observe` creates the entry behind a recovery floor (`readiness/mod.rs:138`) |
| `Unproven` | `Watched::unproven`: same source and stamp as the wait's first look, shared observer Idle, no fence, reservation or compact, and `observe` still gives no proof |
| `Expire` | `InputRefusal::Unproven` (Herdr) or `PreparationHold.unproven` (tmux) reaches `input_owner/effect.rs:176 held`, which answers `NotSent`; `effect.rs:115` then calls `retain_not_sent` |
| `Hold` stays enabled while not `Unproven` | every other timeout and refusal still answers `Completion::Held` and keeps its watch |

## Not modeled

Transcript bytes, the 256 KiB tail window, clocks, and the hook's arrival order are not in the model; the unit
tests cover them. A SessionStart the ledger refuses on a fresh launch ends the same way as the restart but
is only in the unit test. The budget itself is not modeled: `Expire` stands for a whole wait that saw the same
idle bytes from start to end. The notice post and its dedupe stay with the existing retained-input owner.
The composer capture, the draft and modal guards, and the reservation recheck are unchanged.
