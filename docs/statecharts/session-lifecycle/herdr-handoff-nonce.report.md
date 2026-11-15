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

## Transcript identity after a host reboot (r2)

Baseline: `5726d52ca8`. Observed 11-15 (adk-dashboard cc, after the nonce fix): the nonce check
passed and `current_session` refused with `handoff transcript identity changed`. The recorded
transcript is the same file: same inode 1388511848, birth 10-31 16:51, first record naming
session `c45f2c1c…`, last write 11-04 18:57 before the 11-04 19:02 host boot. Its two records hold
dev 16777230 and 16777229 and the file now stats as 16777231: macOS gave the APFS volume another
device number at each boot. Nothing rewrote the file and no other execution wrote it.

The pin rule every Claude source already goes through (`claude_tui/source_verify.rs:226-292`,
`source_verify/renumber.rs:10-21`) treats the same inode on another device, pinned by a record
committed before this boot, whose first record still names the session, as the pinned file under
its new device. `current_session` compared `(dev, ino)` exactly. It now admits a mismatch only
when that rule returns `Verified` and the old tmux execution is proven ended
(`ended_tmux_execution`). Another inode, a first record of another session, a record from this boot
or a running/unknown old tmux still refuse.

Identity census (`git grep -n -E 'judge_pin\(|renumbered\(|under_new_device|file_identity|\(source\.dev, source\.ino\)|metadata\.ino\(\)' -- src`):

| Path | Code | Model |
|---|---|---|
| Stat registration records `(dev, ino)` | `binding_events.rs:808` | the recorded identity |
| Hook check records a verified identity | `claude_tui/source_verify.rs:332` → `judge_pin` | the recorded identity |
| Writer re-pins a renumbered verified pin | `binding_events.rs:768-784` → `renumber.rs:24` | not reached for an unverified legacy record |
| Host reboot renumbers the volume | OS (no AgentDesk code) | `Transcript = "renumbered"` |
| Handoff identity check | `herdr_turn/handoff.rs:162-178,208-212,230-246` | `IdentityOk` |
| Other pin checks (unchanged) | `tui_prompt_dedupe/pending.rs:309`, `source_verify.rs:399` | not changed |

A restart reads the same record, first record and file, so a second judgment matches the first
(`Boot` leaves `taken` and the transcript unchanged). Cancel, clear, Terminate and stop do not
write the binding record or the transcript identity.

| Config | NoCapture | OwnTranscript | Delivered |
|---|---|---|---|
| IdBeforeRenumbered (exact compare, observed refusal) | GREEN | GREEN | RED |
| IdFixedRenumbered | GREEN | GREEN | GREEN |
| IdFixedRenumberedAlive (journal closes the live tmux first) | GREEN | GREEN | GREEN |
| IdFixedRenumberedDirect (judged while tmux runs) | GREEN | GREEN | not checked |
| IdFixedRewritten / IdFixedForeign | GREEN | GREEN | not checked |
| IdNoEnd (mutant: no end proof) | RED | GREEN | not checked |
| IdNoInode / IdNoOwn (mutant: other inode / other session admitted) | GREEN | RED | not checked |

The nine earlier configs gained `Transcript = "same"` and the `OwnTranscript` invariant; their
results are unchanged. Eighteen configs, 7.6 s, every expected violation matched.
A rewritten transcript of the same session (another inode) stays refused, as the pin rule calls it
an anomaly; its input ends retained with the existing notice and its liveness is not claimed.

## Renumbered transcript of a Herdr execution (r3)

Review of r2 (P1-1): the renumbering admission required the tmux end proof, so a channel already
on Herdr (`.host_kind` = herdr) refused every start after a host reboot that renumbered its volume,
including a channel whose first switch the r2 fix let through. Nothing earlier re-pins its binding:
reconnect admits only a matching live Herdr execution, the O writer renumbers its own copy, and the
new SessionStart comes after the refused launch.

Rule after the change: the binding's execution is proven ended by the tmux proof, or by the
channel's hosted row naming that execution Retired with the same owner and nonce. The turn reaches
`launched_source` only for a Legacy, absent or Retired row (`herdr_turn.rs:260-275`), so a Bound or
Pending execution never meets this judgment.

End-evidence census (`git grep -n -E 'retire_pg\(|retire_confirmed\(|\.state = HostedState::Retired|retire_unsent\(|retire_own_(pending|unplaced)\(' -- src`):

| Writer of Retired | Precondition | Code |
|---|---|---|
| CAS from the same owner and nonce only | `plan_state_change` → `same_execution` | `db/dispatched_sessions/hosted_execution.rs:459-472,545-560` |
| Terminate | pane end read, then close | `herdr_terminate.rs:224-236,589-597` |
| Row end recovery | ended turn read | `recovery_engine/herdr_row_end.rs:488-493`, `herdr_row_end_live.rs:66-67` |
| Server replaced / root replaced | placed pane Missing or Exited | `recovery_engine/herdr_server_replaced.rs:265-287`, `herdr_terminate/root_replaced.rs:182-188,410,536,554` |
| Launch that never sent its create, unplaced resolution | no pane was created or placed | `herdr_launch.rs:586-608`, `herdr_launch/unplaced.rs:467-470`, `prepared.rs:25,53` |
| Herdr return, CLI | after `/exit` end, operator end check | `claude/herdr_return.rs:79-90`, `cli/herdr.rs:305-326` |
| Readers | `herdr_launch.rs:434-447` (retired hold), `host_reconcile.rs:127`, `herdr_row_end.rs:396`, `runtime_marker.rs:25`, `herdr_turn/handoff.rs` (`PriorExecution::ended`) | |

| Config | NoCapture | OwnTranscript | Delivered / DeliveredAgain |
|---|---|---|---|
| IdTmuxOnlyHerdr (r2 rule, retired Herdr execution) | GREEN | GREEN | RED Delivered |
| IdTmuxOnlyHerdrAgain (r2 rule, tmux switch then reboot) | GREEN | GREEN | RED DeliveredAgain |
| IdFixedHerdr | GREEN | GREEN | GREEN |
| IdFixedHerdrAgain / IdFixedHerdrAgainFromTmux (second reboot) | GREEN | GREEN | GREEN |
| IdFixedHerdrOther (Retired row names another nonce, h1 may run) | GREEN | GREEN | not checked |
| IdNoNonce / IdNoRetired (mutants) | RED | GREEN | not checked |

The admitted pin must be verified. A Herdr resume start logs a stat record (`binding_events.rs:874-882`
via `herdr_resume.rs:41-45`), which leaves the pin unverified until a later verified record of the
same execution (a source check or a restart's reconnect, `herdr_source.rs:318`). A channel whose
latest record is that stat still refuses with "no verified durable binding"; this rule does not
change it.

All earlier configs gained `Again = FALSE`; their results are unchanged. Twenty-six configs,
`--jobs 1`, 36.7 s, every expected violation matched.
