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
| New start, resume, clear: accepted `SessionStart` of the current incarnation | `claude_tui/readiness/mod.rs:316 note_hook`, start at `:399` | `starts`, `entry.start`; a resume also records the transcript length and launch time (`:409`) |
| Stop and UserPromptSubmit of the current incarnation | `readiness/mod.rs:427` | an entry behind a recovery floor; a prompt arms `pending` |
| First observation with no entry (daemon restart, lost hook) | `readiness/mod.rs:137` | an entry behind a recovery floor at the daemon's start |
| Herdr launch or handoff from tmux, fresh or resumed | `claude/herdr_turn/handoff.rs:49 launched_source` → `discord/tui_prompt_relay/herdr_source.rs:154` (binding at `:187`) | runtime binding after the launch's own `SessionStart` |
| Herdr reattach after a daemon restart | `discord/tui_prompt_relay/herdr_source.rs:274` | runtime binding only; no start, so the recovery floor applies |
| tmux launch | `claude.rs:2002` | runtime binding |
| End of a Herdr turn | `claude/herdr_turn/first_input.rs:242` | runtime binding at the turn's end |
| Source selection | `readiness/source.rs:23 Source::current` | reads marker nonce, context, `starts`, binding, entries |
| Judge | `readiness/mod.rs:84 observe`, resumed start at `:221`; `readiness/evidence.rs:58 quiet_from` | proof only |

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

## Model to code

| Model | Code |
|---|---|
| `ResumedStart` with `r.turn = 0` | `Entry::resumed_quiet`: start unconsumed, and `Tail::quiet_from` finds no turn record or `permission-mode` marker after the start's transcript length, and none stamped at or after the launch |
| `ResumedStart` replaces `observed = "Idle"` | the resumed start passes the shared-observer gate in `observe` |
| `PersonTurn` | a `user` record after the resume; the start closes and that turn needs its own approved terminal |
| `ResumeNoQuiet` | `quiet_from` removed |
| `CompactSubmit` keeps `r.turn > 0` | `compact.rs:104`: a proof resting on the resumed start alone starts no compact |

## Not modeled

Transcript bytes, the 256 KiB tail window, clocks, and the hook's arrival order are not in the model; the unit
tests cover them. A daemon restart between the resume and the first input drops the start, and the existing
recovery floor then applies as before (`ready-restart` does not compose with these configs).
The composer capture, the draft and modal guards, and the reservation recheck are unchanged.
