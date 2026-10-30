# HostMigration
<!-- doc-path-refs: proposed session_handoff.rs herdr_return.rs -->

Model written against AgentDesk `feat/i7503-claude-resume-both` `db99888121` (2026-10-30; base `origin/main` `e8d94425a0`), for #7503: a Claude channel moving tmux → Herdr and back (Return) with the same session id. Every counterexample was replayed step by step against the code by reading it (no build or test run); see the table at the end.

Three configs:

- `HostMigration.cfg`: the forward turn may start fresh (force_fresh, no transcript, or another account, `herdr_turn.rs:495-511`). Chart `migration.chart.mmd`.
- `HostMigrationIdOnly.cfg`: the channel may be registered by id only, with no name or alias. Chart `migration-idonly.chart.mmd`.
- `HostMigrationClean.cfg`: neither. All rules hold. Chart `migration-clean.chart.mmd`. The mutants run on this config.

## What the model holds

| Model action | Code (`db99888121`) |
|---|---|
| Restart (mapping may change; processes and rows survive; the listing is rebuilt; an id-only channel loses its name) | `migration.rs:208-235` (Bound local Claude may lose its mapping, Pending may not), `turn_host.rs:157-167` |
| Queued input waits while the channel has no name | `nameless_gate.rs:22-26,184-195`, `adk_session.rs:68-82` |
| ForwardEndsTmux, ForwardLaunchResume, ForwardLaunchFresh, Attach | `herdr_turn.rs:538-549` (`/exit` only when resuming), `:560-563` (auth record), `:571-576`; `session_handoff.rs:60-84` |
| ReturnEndsHerdr, ReturnRetires, ReturnReleases, ReturnMarksTmux | `herdr_return.rs:58-70` (hold, old tmux must have ended), `:71` + `:113-117` (auth record must name the session), `:72-80`, `:83`, `:88` |
| Return is due / finished | `turn_host.rs:102-121` (`return_plan`), `:126-129` (`returned`: marker tmux and not listed) |
| TmuxLaunch | `claude.rs:1770,1831-1864`, `host_gate.rs:53-61`, `tui_session_launch.rs:91-100` |
| Herdr answers (session written back) | `channel_writeback.rs:218-222` |

Left out: the O cursor check (`session_handoff.rs:153-163`, an error before any effect), the Herdr endpoint becoming unreachable (transient `Unknown`), /clear, Codex, other nodes, and a provider exiting on its own.

## Rules

1. `OneProviderPerSession`: one Claude session id has at most one live provider.
2. `ResumesSameSession`: a move resumes the session the leaving host ran, never another one.
3. `NoDeadlock`: a waiting input can always be served. A turn that fails changes nothing and stays pending, which is the same as the user sending it again.
4. `RecordMatchesProcesses`: a Pending/Bound row owns the channel's only live Claude, and a Legacy/Retired row runs none on Herdr.

## Results, `HostMigration.cfg`

TLC reached 264 states and 540 steps. The chart draws 14 states and 18 arrows.
The chart does not draw 317 steps that stay in one chart state.
The view hides 10 steps, so the chart does not draw them.

### RecordMatchesProcesses breaks
The shortest path has 3 steps:
0. On tmux, Claude running (set to tmux)
1. the server restarts, maybe with the other host set: On tmux, Claude running (set to Herdr)
2. you send a message: On tmux, Claude running (set to Herdr)
3. Herdr launches a fresh session [no session to resume]: Herdr launch sent, not bound yet, old tmux Claude still runs

### NoDeadlock breaks
The shortest path has 7 steps:
0. On tmux, Claude running (set to tmux)
1. the server restarts, maybe with the other host set: On tmux, Claude running (set to Herdr)
2. you send a message: On tmux, Claude running (set to Herdr)
3. Herdr launches a fresh session [no session to resume]: Herdr launch sent, not bound yet, old tmux Claude still runs
4. the launch reports its start and binds the Herdr row: On Herdr, Claude running (set to Herdr), old tmux Claude still runs
5. the server restarts, maybe with the other host set: On Herdr, Claude running (set to tmux), old tmux Claude still runs
6. you send a message: On Herdr, Claude running (set to tmux), old tmux Claude still runs
7. you send a message: On Herdr, Claude running (set to tmux), old tmux Claude still runs

### Rules that hold
OneProviderPerSession, ResumesSameSession

## Results, `HostMigrationIdOnly.cfg`

TLC reached 290 states and 626 steps. The chart draws 12 states and 17 arrows.
The chart does not draw 373 steps that stay in one chart state.
The view hides 11 steps, so the chart does not draw them.

### NoDeadlock breaks
The shortest path has 7 steps:
0. On tmux, no Claude running (set to tmux)
1. the server restarts, maybe with the other host set: On tmux, no Claude running (set to Herdr)
2. you send a message: On tmux, no Claude running (set to Herdr)
3. Herdr launches with --resume of the same session: Herdr launch sent, not bound yet
4. the launch reports its start and binds the Herdr row: On Herdr, Claude running (set to Herdr)
5. the server restarts, maybe with the other host set: On Herdr, Claude running (set to tmux), input waits, channel has no name
6. you send a message: On Herdr, Claude running (set to tmux), input waits, channel has no name
7. you send a message: On Herdr, Claude running (set to tmux), input waits, channel has no name

### Rules that hold
OneProviderPerSession, ResumesSameSession, RecordMatchesProcesses

## Results, `HostMigrationClean.cfg`

TLC reached 151 states and 319 steps. The chart draws 11 states and 13 arrows.
The chart does not draw 188 steps that stay in one chart state.
The view hides 5 steps, so the chart does not draw them.

### Rules that hold
OneProviderPerSession, ResumesSameSession, RecordMatchesProcesses, NoDeadlock

## Mutants (each removes one code guard; run on `HostMigrationClean.cfg` unless noted)

| Mutant (`Mut`) | Code guard removed | Rules that break |
|---|---|---|
| `noExitBeforeLaunch` | `herdr_turn.rs:544-549`: the resume launch no longer ends the old tmux first | OneProviderPerSession, RecordMatchesProcesses, NoDeadlock |
| `retireBeforeExit` | `herdr_return.rs:72-80`: retire without a confirmed `/exit` | RecordMatchesProcesses, OneProviderPerSession |
| `returnedIgnoresMarker` | `turn_host.rs:126-129`: "finished" without the tmux marker | NoDeadlock (tmux refuses on the Herdr marker, no Return is due) |
| `noAuthCheck` (`AllowFresh = TRUE`) | `herdr_return.rs:113-117`: resume without the auth record naming the session | ResumesSameSession (a fresh Herdr session B that never wrote back; Return resumes the old A) |
| `noTmuxCheck` (both configs) | `herdr_return.rs:66-70` | none: in the model a tmux that still runs never shares the Herdr session id, so this guard only blocks (see C2) |

Command for a mutant: copy a config, set `Mut = "<name>"`, and run the command below with that config.

## Counterexamples replayed against the code

| ID | Rule, path | Replay | Verdict |
|---|---|---|---|
| C1 | RecordMatchesProcesses, `HostMigration.cfg` steps 1-3 | Restart with the Herdr mapping; shutdown keeps tmux (`runtime_bootstrap/shutdown.rs`). First input → `for_turn` Herdr → `launched_source`: `resumed_session` answers `None` (`herdr_turn.rs:499-511`), a fresh id (`:540-543`), no `end_by_exit` (`:544`), launch (`:571`). Nothing else ends the legacy tmux: the reaper kills dead panes only (`tmux_reaper.rs:406-410`). | Confirmed by code. Already in main (main never ended the legacy tmux); not new in #7503 |
| C2 | NoDeadlock, `HostMigration.cfg` steps 4-7 | Continuing C1: attach (`herdr_turn.rs:576`), restart without the mapping, accepted for a Bound row (`migration.rs:223-234`). Each input: `for_turn` → `Return` (`turn_host.rs:157-162`) → `herdr_return.rs:66-68` `tmux {logical} still runs`, an error before any effect. No step ever ends that tmux (see C1). The only way out is to put the Herdr mapping back or end the tmux Claude by hand. | Confirmed by code. New in #7503. Reviews A1/A3 covered "a fresh launch skips `/exit`" but not this Return block |
| C3 | NoDeadlock, `HostMigrationIdOnly.cfg` steps 5-7 | Id-only registration; after a restart `core.sessions` has no name; `build_adk_session_key` returns `None` (`adk_session.rs:68-82`), so `returns_to_tmux` sees no row and plans no Return (`nameless_gate.rs`, lines 184-195 on the branch), and the gate keeps holding (`:22-26,33`). | Confirmed by code. Same as review B2 P1-1 (repair B-2, not in `db99888121`) |
| C4 | (as a mutant only) ResumesSameSession without `herdr_return.rs:113-117` | With the guard: a fresh Herdr session B whose first turn did not write back before the mapping went (`channel_writeback.rs:218-222` runs only at completion). The auth record names B (`herdr_turn.rs:562-564`), so Return refuses A on every turn (`herdr_return.rs:113-117`). | Not checked: whether inflight restart recovery finishes that turn and writes B back is not read. Not counted |
| — | `auth_session` record wiped by the reaper before Return | Rejected: the reaper's teardown gate keeps a session whose row is Herdr (`host_teardown_gate.rs:20-27`, `tmux_reaper.rs:447-458`) | Model too broad (not in the final model) |

## Run

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/host-migration
~/.claude/skills/statechart/scripts/statechart HostMigration.tla --config HostMigration.cfg --view migration.view.mjs --out /tmp/sc-hm
```

The view hides only the end-state step `Done`.
