# SessionHost

Model written against AgentDesk `origin/main` `1974cff8a67bf5d1db041abfdd4d615c518ee104` (2026-11-15), for the #7583 design leg (Fable): one Discord channel's managed provider session on the host main picks for it (`HostKind` tmux, Herdr, process), across dcserver restarts, loss of the host server, and a terminate. The proposed design (`Design = "proposed"`) removes the process host and runs Windows channels on Herdr.

TLC 1.7.4 through the `statechart` skill, 14 configs: 7 hold every rule, 7 break exactly the rules listed below. `expected.tsv` carries the same rows. No Rust was built or run; counterexamples were replayed by reading the code.

## Variables and where their values come from

| Variable | Values | Source in main |
|---|---|---|
| `kind` | tmux, herdr, process | `HostKind` (`src/domain/host_observation.rs:4-8`) |
| `Pick` | herdr, tmux, process, refuse | `TurnHost` (`src/services/turn_host.rs:17-25`), provider routing (`src/services/claude.rs:707-822`, `src/services/codex/command_streaming.rs:105-204`, `src/services/qwen.rs:339-368`) |
| `HerdrMapped` | TRUE, FALSE | `session_hosts.herdr.channels` (`src/config/session_hosts.rs:350-352`) and the provider switch `runtime.herdr_turn_enabled` / `herdr_codex_turn_enabled` (`src/services/turn_host.rs:220-247`) |
| `rec` | none, pending, bound, retired | `HostedState` Pending/Bound/Retired plus a Legacy row (`src/services/turn_host.rs:88-98`); a tmux or process inflight row counts as bound |
| `attached` | TRUE, FALSE | process registry (`src/services/session_backend.rs:341`), tmux watcher restore, Herdr restart reader (`src/services/discord/recovery_engine/herdr_reader.rs:220-228`) |
| `Platform`, `TmuxInstalled` | unix/windows, BOOLEAN | `cfg(unix)` gates and `platform::tmux::is_available` (`src/services/platform/tmux/availability.rs:110`) |

Left out and covered elsewhere: the Herdr launch steps and a lost create reply (`session-lifecycle/HerdrSession.tla`), a channel moving between tmux and Herdr (`host-migration/HostMigration.tla`), deferred tmux boot probes (`session-lifecycle/TmuxSession.tla` T-A/T-B), a provider exiting on its own.

## Rules

1. `NoKillByRestart` (invariant): a dcserver restart alone never ends a session.
2. `NoDoubleSession` (invariant): one channel never has two providers.
3. `NoOrphan` (invariant): no provider runs that no live record names, also after a terminate.
4. `NoProcessHost` (invariant): the proposed design never starts a process-hosted session.
5. `Reconnects` (liveness): after a restart the channel ends up open with its session held again or its row closed.
6. `InputAnswered` (liveness): every input is started or refused, never left waiting.
7. `HostRecovers` (liveness): a lost host server comes back without anyone's hands.

Fairness covers what dcserver and the hosts do on their own (launch, bind, reconcile, loss detection, host start); inputs, crashes, host loss and terminates are never forced. Bounds: 2 restarts, 1 host loss, 1 terminate, 3 inputs.

## Results

| Config | Platform / design / channel | States | Breaks |
|---|---|---|---|
| `SessionHostUnixTmuxCurrent` | unix, main, tmux | 560 | none |
| `SessionHostUnixTmux` | unix, proposed, tmux | 560 | none |
| `SessionHostUnixHerdrCurrent` | unix, main, Herdr | 812 | none |
| `SessionHostUnixHerdr` | unix, proposed, Herdr | 812 | none |
| `SessionHostUnixNoTmuxCurrent` | unix without tmux, main | 233 | `NoKillByRestart`, `NoOrphan`, `NoDoubleSession` |
| `SessionHostUnixNoTmux` | unix without tmux, proposed | 49 | none (input refused) |
| `SessionHostWindowsCurrent` | windows, main | 218 | `NoKillByRestart` |
| `SessionHostWindowsHerdr` | windows, proposed, Herdr | 812 | none |
| `SessionHostWindowsUnmapped` | windows, proposed, channel not mapped | 49 | none (input refused) |
| `SessionHostWindowsNoAutostart` | windows, proposed, nothing restarts a lost Herdr server | 490 | `HostRecovers` |
| `SessionHostWindowsServerInJob` | mutant: Herdr server inside dcserver's job | 1,134 | `NoKillByRestart` |
| `SessionHostRetireBeforeClose` | mutant: terminate retires the row before the close | 1,004 | `NoOrphan` |
| `SessionHostBootOpensEarly` | mutant: intake opens before boot reconcile | 980 | `Reconnects`, `InputAnswered` |
| `SessionHostSilentFallback` | mutant: proposed design keeps the process fallback | 233 | `NoProcessHost`, `NoKillByRestart`, `NoOrphan`, `NoDoubleSession` |

The unix tmux and unix Herdr pairs reach the same states under main and the proposed design: the default for those channels does not change.

Charts: [windows, main](session-host-windows-current.chart.mmd), [windows, unmapped channel refused](session-host-windows-unmapped.chart.mmd), [Herdr server in the job](session-host-windows-server-in-job.chart.mmd). Configs above 500 states with no broken rule have no chart.

## Counterexamples replayed against the code

### Windows, main: `NoKillByRestart`

0. No session → 1. a message arrives → 2. a turn launches a new session (process) → 3. dcserver stops: no session.

| Step | Code |
|---|---|
| 2 | A managed session on non-unix always takes the process path: `src/services/claude.rs:807-821`, `src/services/codex/command_streaming.rs:182-203`, `src/services/qwen.rs:357-368`. The wrapper is dcserver's child (`src/services/session_backend.rs:229-260`) and is found only through the in-memory registry (`:341`). |
| 3 | dcserver puts itself in a kill-on-close job before starting any child (`src/cli/dcserver.rs:882`, `src/services/platform/windows_job.rs:35,52`), so its exit ends the wrapper and the provider. After boot the registry is empty and the row reads as dead (`src/services/session_backend.rs:526-536`). |

Confirmed. This is the gap the issue names.

### Unix without tmux, main: `NoOrphan`, `NoDoubleSession`

… 2. a turn launches a new session (process) → 4. dcserver stops: an unreachable provider still runs → 6. boot reconcile → 7. a turn launches a new session while the old provider runs.

| Step | Code |
|---|---|
| 2 | tmux missing demotes the turn to the process path (`src/services/claude.rs:770-805`, guard `src/services/claude/backend_routing.rs:58-66`). |
| 4 | The wrapper leads its own process group (`src/services/session_backend.rs:240-244`), so stopping dcserver does not signal it; its stdin reader ends at EOF but the process stays while the provider runs (`src/services/tmux_wrapper.rs:375-383`). |
| 6-7 | Nothing can name the old wrapper after boot (`src/services/session_backend.rs:341`); the next turn starts a second wrapper with `--resume`. `backend_routing.rs:55-57` records the double-resume risk as accepted. |

Confirmed by reading; not reproduced on a host. Only a unix host with no tmux reaches it.

### Mutants (design guards, not main)

- `server_in_job`: the Windows job has no breakaway limit (`src/services/platform/windows_job.rs:35`), so a Herdr server that dcserver starts as an ordinary child dies with dcserver and takes every pane with it. The design must start it outside the job; whether Herdr's own automatic startup leaves the job is unmeasured ([U] in the design).
- `retire_before_close`: a crash between the retire and the close leaves a pane no row names. The design keeps main's order, close first and retire on its acknowledgement (`src/services/termination_audit/host_terminate.rs:116-133`).
- `boot_opens_early`: a live session that boot reconcile did not pick up is never held again, so its input waits forever. The design keeps the Herdr restart reader before intake (`src/services/discord/tui_prompt_relay/rehydration.rs:270,668`).
- `silent_fallback`: keeping any call into the process path brings back every break of the unix-without-tmux config.

### Windows, proposed, no autostart: `HostRecovers`

After the Herdr server is lost (a Herdr update or stop: Windows has no live handoff), inputs are refused with an error and nothing brings the server back. main restarts it on unix (`src/services/discord/recovery_engine/herdr_server_replaced.rs:41`, `src/services/session_host/herdr/autostart/spawn.rs`); the Windows design needs the same start path, outside the job.

## Assumptions

- A Herdr pane outlives dcserver on Windows when its server runs outside dcserver's job ("Local persistent sessions continue running after the client detaches", Herdr v0.9.5 Windows docs). Not measured on Windows.
- The Herdr restart reader works over a new API connection per call; it needs no terminal attach or server handoff (`src/services/session_host/herdr/transport.rs:1-2`).
- Loss detection is modeled as one step that always succeeds; main's own unclear-read paths keep a row instead (HerdrSession and the herdr-root models cover them).

## Run

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/session-host-fable
~/.claude/skills/statechart/scripts/statechart SessionHost.tla --config SessionHostWindowsHerdr.cfg --view session-host.view.mjs --out /tmp/sc-session-host
```
