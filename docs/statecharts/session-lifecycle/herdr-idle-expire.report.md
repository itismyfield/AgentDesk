# HerdrIdleExpire — 6-hour idle end of a Herdr execution

Code SHA: `c28732c52c` (Before), `6e60666717` (Prev), `99fb38efcb` (PrevClear: idle record survives /clear).
`Expire = TRUE` models the fix (`herdr_terminate/idle_expire.rs`, `dispatched_sessions/herdr_idle.rs`,
`codex/herdr_turn/restart_intent/idle_resume.rs`); each other constant removes one guard. A new model
instead of `HerdrSession`: that module has 40+ configs and no turn, dispatch, inflight or idle-clock
variable, so extending it would change every one of their state spaces. `prov` and `gone` are chosen
at Init, so every config checks a Claude and a Codex execution, with and without its owning bot.

## Actions and code
| Action | Code |
|---|---|
| IdleKillTick | `policies/timeouts/idle-kill.js` (status idle, no dispatch, 6 h; candidates read once) → `tmux_cleanup.rs` host guard → (Before) `preserved_without_probe` + `idle_cleanup_preserved` event; (After) `herdr_idle_kill` → `expire_idle_herdr` pre-checks: dispatch, inflight row, owner runtime by row token, else `end_orphan` after the boot grace. (Prev) a Codex owner's intent was stored in the owner slot here (`record_idle_resume`), before the transition lock |
| Reserve | `terminate_scoped`: `capture_turn`, `session_transition_locks.acquire`, same-turn read, `AutoEnd::Idle` → `IdleGate::reserve` (`SELECT … FOR UPDATE`: status idle, `COALESCE(last_heartbeat, created_at)` older than the threshold, no `active_dispatch_id`), inflight re-read, `IdleGate::keep` (Codex: `idle_resume::keep` writes the thread record), `termination_fence`. The orphan path runs the same checks in `close_unrecorded`'s `holds`. A refusal writes nothing; a record already written is dropped only by the gate that wrote it (`IdleGate` drop → `idle_resume::forget`, operation compared). (Prev) a refusal cancelled the owner slot it had overwritten (`cancel_idle_resume`) |
| Close | `close_fenced` (orphan: skipped when `already_ended`), `read_end`, `AutoEnd::release` / `IdleGate::release` (row lock commit, record kept), `retire_confirmed`, `disconnect_retired_pg(.., keep_provider_session = true)` |
| EffortStart / EffortEnd | `/effort` with no turn (`effort_restart/herdr.rs` `restart`): `tmux::record` stores the owner slot before `terminate_for_http`; an end that finds the execution changed cancels only its own operation (`cancel_effort`) |
| Input / SaveInflight / TurnEnd | router intake → mailbox token, then the inflight row save; the finalizer clears both and refreshes the heartbeat |
| Input on Retired | `install_pending_pg` over a Retired row. Claude resumes the kept `claude_session_id`; Codex `launch_of` → `resume_of`: the owner slot naming that execution first, else `idle_resume::kept` for the same owner and retired nonce; `ResumeStart` checks the O cursor |
| DispatchAttach | `update_session_pg` plain `UPDATE` of `active_dispatch_id`; it waits behind the row lock (`Exclude`) |
| Elapse | `last_heartbeat` older than 6 h; a quiet long turn or a restart can leave it stale |
| Clear | `/clear` of a retired execution: `commands/control/herdr_recovery.rs:116-131` cancels the idle record of that owner and nonce (`idle_resume.rs:75` `forbid`) before `clear_retired_selector_pg`; a failed cancel leaves the clear pending. `queued_head/v2/clear.rs:219-224` cancels the owner slot's open intent. (PrevClear) only the slot and selector were cleared |
| Crash / Boot / GracePass / Register / Return / Restore | dcserver restart loses the mailbox and both locks; inflight file, pane, slot and record survive; `ORPHAN_AFTER_BOOT_SECS`; a removed bot never registers until it is added back; `restore_inflight.rs` |

## Entry points of the resume intent
| What | Where |
|---|---|
| owner slot store (`/effort`) | `commands/effort_restart/herdr.rs` `restart` → `tmux::record` → `Owner::prepare_effort` (`queued_head/v2/effort_continue.rs`) |
| owner slot cancel | `effort_restart/herdr.rs` `restart` (`tmux::cancel` on a failed end), `effort_restart/recovery.rs` → `Owner::cancel_effort` (operation compared) |
| owner slot read | `effort_restart/herdr.rs` `herdr_resume` → `BootPorts::effort_resume` (`router/message_handler/provider_dispatch.rs`) |
| idle record store | `idle_expire.rs` `IdleGate::keep` (under the transition lock and row reservation; orphan: inside `holds`) → `idle_resume::keep` |
| idle record drop | `IdleGate` drop when the end never reached `release` → `idle_resume::forget` (operation compared) |
| idle record cancel by /clear | `commands/control/herdr_recovery.rs:119` → `idle_resume.rs:75` `forbid` (nonce compared) |
| consume | `codex/herdr_turn/restart_intent.rs` `resume_of` (slot first, else `idle_resume::kept`), spent once another execution holds the row |

## Results (plain TLC 1.7.4)
| cfg | Breaks | Holds |
|---|---|---|
| Before (= no idle end) | AlarmNotRepeated, IdleEventuallyCloses | the rest |
| BeforeLiveness | IdleEventuallyCloses | TypeOK |
| Prev (`6e60666717`) | NextMessageResumes, IdleEventuallyCloses | the rest |
| After | — | all |
| MutTurn (no mailbox turn check) | NeverClosesBusy | the rest |
| MutInflight (no inflight row check) | NeverClosesBusy | the rest |
| MutIdle (reservation does not re-read status and heartbeat) | NeverClosesUsed | the rest |
| MutExclude (dispatch PATCH does not wait for the reservation) | NeverClosesBusy, NeverClosesUsed | the rest |
| MutNewThread (no Codex thread kept) | NextMessageResumes | the rest |
| MutLateKeep (Codex intent stored in the owner slot before the lock) | NextMessageResumes | the rest |
| MutOrphanCodex (no orphan end for Codex) | IdleEventuallyCloses | the rest |
| PrevClear (`99fb38efcb`: /clear keeps the idle record) | ClearNeverResumes | the rest |

Prev / MutLateKeep: `/effort` stores its intent, the idle tick overwrites the slot, `/effort` retires
the execution, the idle end is refused and cancels the slot, and the next message opens a fresh
thread. Prev / MutOrphanCodex: the owning bot is gone and its Codex pane stays open forever.
MutTurn, MutInflight, MutIdle and MutExclude are as before: a close lands under a turn, over an
open inflight row, on a pane just used, or past a dispatch that attached after the reservation.
PrevClear: an idle end keeps a Codex record, the owner clears, and its next message resumes the
cleared conversation from that record, since the clear forgot only the SQL selector and the slot.
