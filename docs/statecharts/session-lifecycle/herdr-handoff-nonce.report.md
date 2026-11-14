# Herdr handoff nonce judgment after a tmux switch

Baseline: `94194fc65b`. Companion to [HerdrHandoffLaunch](herdr-handoff-launch.report.md):
that model abstracts the launch outcome as `AlwaysFails`; this one models what decides it.

Observed (11-14 switch): a Claude channel whose last tmux execution resumed the same
transcript has a binding log whose latest record carries the previous tmux nonce, because
the writer logs nothing for a repeat of the same source (`binding_events.rs:874-882`;
only a Herdr resume counts as a new execution, `herdr_resume.rs:41-45`). The `.spawn_nonce`
marker names the later tmux execution. The legacy branch of `current_session` read every
such difference as "another execution" and refused each start, even after that tmux ended.
A Hangul logical name also failed the pinned-close name check, so the handoff close never
ran while the tmux lived.

Rule after the change: a mismatch is refused unless the marker names a tmux execution
(`.host_kind` = tmux) whose session is proven ended. Herdr writes `.host_kind` = herdr
before its nonce (`herdr_launch.rs:493-497`) and a tmux turn is refused while `.host_kind`
names another host (`host_gate.rs:20-36`), so a failed Herdr create never reads as tmux.

## Entrypoint census

`git grep -n -E 'stamp_spawn_markers\(|record_(tmux|herdr)_host_marker\(|install_herdr_execution\(|cleanup_session_temp_files|register_tmux_runtime_binding|pinned_tmux::end|observe_spawn_nonce_marker' -- src`

| Path | Code | Model value / action |
|---|---|---|
| Claude TUI tmux spawn: host marker, then nonce + context | `claude/tui_session_launch.rs:86,150`; `claude.rs:1833` | Init `marker = t1`, `hostKind = tmux` |
| Plain tmux provider spawns (nonce only) | `claude.rs:2734`; `codex.rs:1642`; `qwen.rs:62` | same tmux marker; admitted only while `.host_kind` is absent/tmux |
| Stat registration of the same source | `tui_prompt_dedupe/runtime_binding.rs:240-251` → `binding_events.rs:874-882` | `ResumeLogged = FALSE` (no record, `recorded = t0`) |
| Herdr create: row Pending, host marker, nonce | `herdr_launch.rs:480-497` | `Launch` success: `marker = h2`, `hostKind = herdr` |
| Herdr create failing after its markers | `herdr_launch.rs:529-546` (indeterminate / NotSent) | `Start = "failedcreate"` (`h1`, pane may run) |
| Herdr marker write failing during create | `herdr_launch.rs:493-508` (host marker at :493, nonce stamp at :497, then `retire_unsent`) | NOT_MODELED: host may be `herdr` while the nonce is missing or removed |
| Nonce write failure removes the nonce | `tmux_session_files.rs:346-354` | NOT_MODELED: marker absent, so the judgment refuses as before |
| kill-tmux API | `dispatched_sessions/tmux_cleanup.rs:338` | `KillTmux` (markers stay) |
| Session temp cleanup (removes `.spawn_nonce`) | `tmux_common.rs:1186-1260`; callers `tmux_reaper.rs:344,687`, `turn_lifecycle.rs:502`, `tmux_recreate.rs:59`, `clear_teardown/execution_cleanup.rs:311`, `managed_reset.rs:145`, `claude.rs:2605`, `claude/tui_session_launch.rs:57` | marker absent: never refused (unchanged, not modeled) |
| Input-owner handoff close | `queue_io/input_owner/claude_handoff.rs:138-197,241-258` → `pinned_tmux.rs:224-338` | `Begin`, `Close` (`CloseWorks`) |
| Launch-time close of a live tmux | `herdr_turn/handoff.rs:74-86` | covered by `Direct` only as a judgment before close |
| Binding judgment | `herdr_turn/handoff.rs:183-195,218-225` (`ended_tmux_execution`) | `Refused` |
| dcserver restart / boot census | markers, log and journal are files; the writer fold is rebuilt from the log | `Boot` (restart without cleanup) |
| Boot orphan sweep (removes stale session files when dead sessions were cleaned) | `watchers/lifecycle/restore.rs:690-719` → `tmux_session_files.rs:850` | NOT_MODELED |
| Retired row (#8146) | `herdr_launch.rs:440-447` `retired_hold::before_fresh` | not changed; Herdr-side proof |

Cancel, queue clear, Herdr Terminate, `/clear` and stop do not write `.host_kind` = tmux or
a tmux nonce, so none of them creates the accepted state. Input classification is untouched.

Scope: `Boot` is a restart that performs no file cleanup. Marker removal (boot orphan sweep, nonce
write failure) and partial marker writes are NOT_MODELED; the 9 cfg results do not cover them and
must not be read as a pass for the whole boot path.

## TLC result

Plain TLC 1.7.4 via `scripts/ci/statecharts_tlc.py --root <copy>` on macOS (openjdk 27),
nine configs, 4.6 s. Every expected violation matched.

| Config | NoCapture | Delivered |
|---|---|---|
| BeforeKilled (tmux killed, resume unlogged) | GREEN | RED |
| BeforeHangul (tmux alive, Hangul name) | GREEN | RED |
| BeforeLogged (resume logged, control) | GREEN | GREEN |
| FixedKilled | GREEN | GREEN |
| FixedHangul | GREEN | GREEN |
| FixedDirect (judged while tmux runs) | GREEN | not checked |
| FixedFailedCreate (Herdr marker, pane may run) | GREEN | not checked |
| NoHostCheck (mutant: liveness only) | RED | not checked |
| NoEndProof (mutant: host kind only) | RED | not checked |

Limits: `Direct` stands for a judgment without the close that `launched_source` runs before
using the binding, so it only exercises the mismatch branch. A failed Herdr create whose pane
has ended still refuses (Herdr-side end proof is outside this rule); its liveness is not claimed.
