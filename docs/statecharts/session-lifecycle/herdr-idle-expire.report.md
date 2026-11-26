# HerdrIdleExpire — 6-hour idle end of a Herdr execution

Code SHA: `c28732c52c` (Before). `Expire = TRUE` models the fix
(`herdr_terminate/idle_expire.rs`, `dispatched_sessions/herdr_idle.rs`); `CheckTurn`/`CheckInflight`
remove one guard each. A new model instead of `HerdrSession`: that module has 40+ configs and no turn,
dispatch, inflight or idle-clock variable, so extending it would change every one of their state spaces.

## Actions and code
| Action | Code |
|---|---|
| IdleKillTick | `policies/timeouts/idle-kill.js` (status idle, no dispatch, 6 h) → `tmux_cleanup.rs` host guard → (Before) `preserved_without_probe` + `idle_cleanup_preserved` event; (After) `herdr_idle_kill` → `expire_idle_herdr` pre-checks: dispatch, inflight row, runtime registered |
| Fence | `terminate_scoped`: `capture_turn` (any turn refuses an automatic end), `session_transition_locks.acquire`, same-turn read, `AutoEnd::Idle` inflight re-read, `termination_fence` |
| Close | row unchanged and `dispatch_holds` (no active dispatch) under the fence, `close_fenced`, `read_end`, `retire_confirmed`; then `disconnect_retired_pg(.., keep_provider_session = true)` |
| Input / SaveInflight / TurnEnd | router intake → mailbox token, then the inflight row save; finalizer clears both |
| DispatchAttach | `sessions.active_dispatch_id` written in PG without the transition lock |
| Elapse | `last_heartbeat` older than 6 h; a quiet long turn or a restart can leave it stale |
| Crash / Boot / Register / Restore | dcserver restart loses the mailbox; the inflight file and pane survive; the runtime registers, then `restore_inflight.rs` restores the turn |
| Input on Retired | `herdr_launch.rs` `install_pending_pg` over a Retired row; the kept `claude_session_id` is resumed |

## Results (plain TLC 1.7.4)
| cfg | Breaks | Holds |
|---|---|---|
| Before (= no idle end) | AlarmNotRepeated, IdleEventuallyCloses | TypeOK, NeverClosesBusy, NextMessageResumes |
| BeforeLiveness | IdleEventuallyCloses | TypeOK |
| After | — | all |
| MutTurn (no mailbox turn check) | NeverClosesBusy | the rest |
| MutInflight (no inflight row check) | NeverClosesBusy | the rest |

Before: the tick refuses every time and stays in "Bound · 6 h idle" forever. MutTurn: an input set the
mailbox token before its inflight row, the clock is stale, and the close lands under the turn.
MutInflight: a restart left the inflight row, the runtime registered before `restore_inflight`, and the
close lands over the open row. `NextMessageResumes` holds by construction (no action drops the
provider session); the code side is the `keep_provider_session = true` disconnect and its PG test.
