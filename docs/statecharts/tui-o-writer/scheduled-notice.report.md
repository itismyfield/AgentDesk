# Scheduled prompt notice across the writer handoff (#8142 S3)

Model `WriterScheduledNotice.tla`, written against `c81c1920e7` plus the S3 change. It follows one scheduled prompt of one Claude channel and asks who posts its notice: the hook path, which is the live owner until the writer's frozen cohort is durable, or the writer, which reads the fire's own `isMeta` row after that. It also asks whether a deploy can roll the channel back to S2, whose ledger reader refuses that row's evidence and halts the writer.

## Behaviours

| Step | Code |
|---|---|
| `HookFire`, `HookDecide` | `tui_prompt_relay/hook_observer.rs` → `synthetic_start_wiring::post_passive_prompt_notice`. Silent once `writer_posts_input_notice` holds; otherwise it posts under `⏰`, or under `⌨️` when the row was not readable in time, or its POST fails. |
| `WriteRow` | Claude writes the fire's `user` row (`isMeta`, `scheduledTaskId`). |
| `Handoff` | `actor/notice_work.rs::start_notice_migration` records `MigrationStarted`. Rows below a source's lower are the floor, rows up to its upper are the frozen cohort, later rows are live. |
| `WriterRead` | `writer/input_notices.rs::native_input` → `Projection::project_record` → `persist_notice_evidence`. A floor row is skipped by `before_migration_floor`. |
| `Settle` | Cohort row: `input_notices/migration.rs::plan` — a legacy message under either header is debt, a proven absence posts. Live row: posts. The ledger's `Posted` outcome seals it. |
| `Restart` | The writer loses its memory and re-projects from ledger evidence (`prepare_notices`). |
| `WriterRead` (floor) | `store/mod.rs::write_ledger` → `input_notice_floor::before_scheduled_append` makes `o_store/scheduled_input_notice.floor` durable before the first `Materialized` whose row is a scheduled fire (`NoticeEntry::records_scheduled_fire`), origin bound or not. |
| `Rollback` | `scripts/deploy-release.sh` `_rollback_would_revert_o_ledger` (manual and failed-readiness rollback) and `_binary_would_revert_o_ledger` (install target): with the floor present the target must report `o_ledger_scheduled_input_notice: true`. S2 does not report it. |

## Rules

- `PostedAtMostOnce`: hook posts plus writer posts for the prompt never exceed one.
- `NoRetroactivePost`: the writer never posts a row from below the frozen lower.
- `ReplayableAfterRollback`: a channel rolled back to S2 holds no scheduled-fire evidence.
- `EventuallyPostedOnce` (liveness, weak fairness on every step but `Restart` and `Rollback`): a cohort or live prompt ends with exactly one notice, unless the channel went back to S2, which drops the fire's row by design.

## Results

Plain TLC 1.7.4, 2026-11-14, each config under one second. State counts include the `Rollback` step.

| cfg | distinct states | result |
|---|---:|---|
| WriterScheduledNoticeFixed.cfg | 140 | GREEN |
| WriterScheduledNoticeBefore.cfg | 100 | `EventuallyPostedOnce` violated: the hook is silent after the handoff and the writer drops the `isMeta` row (`c81c1920e7`) |
| WriterScheduledNoticeS3Head.cfg | 100 | `ReplayableAfterRollback` violated: the first S3 head (`63a3b017e`) appended the evidence with no floor, so the guard let S2 back in |
| WriterScheduledNoticeNoEvidenceFloor.cfg | 100 | `ReplayableAfterRollback` violated: the floor is not raised before the append |
| WriterScheduledNoticeNoRollbackGuard.cfg | 100 | `ReplayableAfterRollback` violated: the guard ignores the floor |
| WriterScheduledNoticeNoHookSuppress.cfg | 133 | `PostedAtMostOnce` violated: hook and writer both post |
| WriterScheduledNoticeNoFloor.cfg | 107 | `NoRetroactivePost` and `PostedAtMostOnce` violated |
| WriterScheduledNoticePayloadOnly.cfg | 122 | `PostedAtMostOnce` violated: the hook announced a frozen fire under `⌨️`, the writer reads no `⏰` message as absence |
| WriterScheduledNoticeNoSeal.cfg | 139 | `PostedAtMostOnce` violated: a restart posts again |
| WriterScheduledNoticeQueuedHandoff.cfg | 186 | `PostedAtMostOnce` violated: known gap, see limits |

## Counterexample replay

- Before: `HookFire`, `WriteRow`, `Handoff`, `HookDecide` (silent). Code: `synthetic_start_wiring.rs:139` returns before posting, and `extract_claude_transcript_user_prompt_with_entry_id` returns `None` for the `isMeta` row, so `Projection::project` yields nothing. Confirmed by the S2 review's ledger count (0 scheduled rows among 397 notice evidence lines).
- PayloadOnly: `HookFire`, `WriteRow`, `HookDecide` posts `⌨️`, `Handoff`, `WriterRead`, `Settle` posts. Code before the guard: `plan` compared only `input.payload` with history. Pinned by `a_frozen_scheduled_fire_the_hook_announced_under_either_header_is_not_posted_again`.
- NoHookSuppress: pinned by `a_scheduled_fire_is_announced_by_the_hook_path_only_until_the_writer_takes_notices_over`.
- NoFloor: pinned by `scheduled_fires_are_ready_only_from_the_frozen_lower_on`.
- S3Head / NoEvidenceFloor: `Handoff`, `WriterRead` appends the evidence with no floor, `Rollback` passes. Pinned by `scheduled_fire_evidence_floor_precedes_its_first_append` (store) and the scheduled-floor cases of `tests/test_deploy_o_writer_rollback_guard_6325.sh`.
- NoRollbackGuard: the floor is durable, the guard reads only the older floors. Pinned by the same shell cases (manual, failed-readiness and install-target paths).

## Limits

- `QueuedHandoff` is inherited, not introduced: when the hook decides before the handoff and the row is written after the frozen upper (an input queued behind a running turn while the channel hands over), the hook posts and the writer posts the live row. Typed inputs have the same gap. Closing it needs the writer to see the hook's announced prompt id.
- `HookDecide` is atomic. In code the owner check is followed by `direct_prompt_notice`, which re-reads the transcript every 50 ms for up to 200 ms (600 ms once the row shows a dequeue) before the POST, so a handoff can land inside a normal row wait too. The cohort's history upper is frozen at the handoff, so a hook POST made after it is outside the range even if history is read later.
- A row written after the handoff and before this build, still in retained spool when the new build boots, is read as live and posted once. Rows already past the cursor are not read again.
- The model has one prompt and one restart. Evidence completeness, binding origin, clears and ownership receipts are the existing models' subject.
- No chart was generated: the `statechart` skill is not installed on this host; TLC ran directly.
