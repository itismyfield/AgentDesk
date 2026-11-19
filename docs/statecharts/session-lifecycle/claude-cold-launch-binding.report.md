# Herdr Claude cold launch: launch Pending, first turn read, row Bound

Code: `b54787cb19`. A small standalone model of one Herdr Claude cold launch. It does not extend `HerdrSession`.

## Result

Plain TLC 1.7.4 through `scripts/ci/statecharts_tlc.py` on a scoped root: 3/3 configs matched their expected sets, 6.2 s.
The `statechart` wrapper was not run, so no `.chart.mmd` exists for this model.

| Config | Result | Meaning |
|---|---|---|
| Before | RED: FirstTurnBound, LaunchSettles | the log and row resolve only after the first turn's read ends; an early end leaves both Pending |
| After | GREEN | the transcript exists, so the Pending resolves and the row turns Bound before the read starts |
| NoBind | RED: FirstTurnBound, LaunchSettles | mutant: resolve the log before the read but skip the row bind |

`FirstTurnBound`: while the first turn is read, the log names its source and the row is Bound. Presence reads a Herdr channel Bound only from a Bound row.
`LaunchSettles`: a launched binding ends resolved and Bound. It may keep waiting only when no transcript was ever written.

## Counterexample against the code

Before: `attach_launched_herdr_source` (`herdr_source.rs:154`) runs the Stat of an unwritten transcript, and the log appends Pending(startup) (`binding_events.rs` plan, `(None, None)`).
The row stays Pending (`baseline` none). Hooks of the launch session reach `observe_launch_hook` (`adoption_retry.rs:248`).
`reclaim_with_current_prompt` promotes only an unverified registered Source, so the launch's own Pending stays.
The prompt writes the transcript, and `first_input::observe` reads the turn.
Only after the read returns does it register again and call `bind_once_logged`.
A read woken by completion cleanup without a terminal, or one that fails, returns before that, so both stay Pending.

## Entry points and values

| Value | Source |
|---|---|
| log none → pending | `attach_launched_herdr_source` → `register_launched_tmux_runtime_binding_under_source_authority` (Stat, `CauseSource::Launch`) |
| log pending → resolved | Stat of an existing file with a matching Pending (`binding_events.rs` plan, `(Some, Some)`) |
| row pending → bound | `bind_once_logged` (`herdr_turn.rs`) or the next turn's attach |
| read ends | `first_input::observe`: early (no file), `unconfirmed && woken`, read error `?`, normal end |
| launch hooks | `observation_ingress.rs:197` same-session branch → `observe_launch_hook` |

Assumptions: the DB bind succeeds. The transcript appears only while the turn waits for it. In-pane `/clear` adoption is outside the model; its path does not change.
