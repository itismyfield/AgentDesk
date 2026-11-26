# HerdrIdleExpire — 6-hour idle end of a Herdr execution

Code SHA: `c28732c52c` (Before). `Expire = TRUE` models the fix
(`herdr_terminate/idle_expire.rs`, `dispatched_sessions/herdr_idle.rs`); each other constant removes
one guard. A new model instead of `HerdrSession`: that module has 40+ configs and no turn, dispatch,
inflight or idle-clock variable, so extending it would change every one of their state spaces.
`prov` is chosen at Init, so every config checks a Claude and a Codex execution.

## Actions and code
| Action | Code |
|---|---|
| IdleKillTick | `policies/timeouts/idle-kill.js` (status idle, no dispatch, 6 h; candidates read once) → `tmux_cleanup.rs` host guard → (Before) `preserved_without_probe` + `idle_cleanup_preserved` event; (After) `herdr_idle_kill` → `expire_idle_herdr` pre-checks: dispatch, inflight row, owner runtime; a Codex owner records its idle resume intent (`effort_restart/herdr.rs` `record_idle_resume`) |
| Reserve | `terminate_scoped`: `capture_turn`, `session_transition_locks.acquire`, same-turn read, `AutoEnd::Idle` → `IdleGate::reserve` (`SELECT … FOR UPDATE`: status idle, `COALESCE(last_heartbeat, created_at)` older than the policy threshold, no `active_dispatch_id`), inflight re-read, `termination_fence`; a refusal cancels the intent (`cancel_idle_resume`) |
| Close | `close_fenced`, `read_end`, `AutoEnd::release` (row lock commit), `retire_confirmed`, `disconnect_retired_pg(.., keep_provider_session = true)` |
| Input / SaveInflight / TurnEnd | router intake → mailbox token, then the inflight row save; the finalizer clears both and refreshes the heartbeat |
| DispatchAttach | `update_session_pg` plain `UPDATE` of `active_dispatch_id`; it waits behind the row lock (`Exclude`) |
| Elapse | `last_heartbeat` older than 6 h; a quiet long turn or a restart can leave it stale |
| Crash / Boot / Register / Restore | dcserver restart loses the mailbox and both locks; the inflight file, pane and owner intent survive; `restore_inflight.rs` restores the turn |
| Input on Retired | `install_pending_pg` over a Retired row. Claude resumes the kept `claude_session_id`; Codex `launch_of` resumes only through the owner's intent (`herdr_resume` → `restart_intent::resume_of` → `ResumeStart`) |

`selector` (the row keeps the provider session) and `intent` (the next launch selects that native
thread) are separate: the selector alone resumes Claude but not Codex.

## Results (plain TLC 1.7.4)
| cfg | Breaks | Holds |
|---|---|---|
| Before (= no idle end) | AlarmNotRepeated, IdleEventuallyCloses | the rest |
| BeforeLiveness | IdleEventuallyCloses | TypeOK |
| After | — | all |
| MutTurn (no mailbox turn check) | NeverClosesBusy | the rest |
| MutInflight (no inflight row check) | NeverClosesBusy | the rest |
| MutIdle (reservation does not re-read status and heartbeat) | NeverClosesUsed | the rest |
| MutExclude (dispatch PATCH does not wait for the reservation) | NeverClosesBusy, NeverClosesUsed | the rest |
| MutNewThread (no Codex idle resume intent) | NextMessageResumes | the rest |

Before: the tick refuses every time and stays in "Bound · 6 h idle" forever. MutTurn: an input set the
mailbox token before its inflight row, the clock is stale, and the close lands under the turn.
MutInflight: a restart left the inflight row, the runtime registered before `restore_inflight`, and the
close lands over the open row. MutIdle: a short turn starts and ends after the tick selected the row,
and the close lands on the pane just used. MutExclude: a dispatch attaches between the reservation and
the close. MutNewThread: a Codex execution is closed and the next message starts a fresh thread.
The orphan path (no owner runtime after boot) is not modeled; its PG tests cover it.
