# Scheduled prompt notice across the writer handoff (#8142 S3)

Model `WriterScheduledNotice.tla`, written against `c81c1920e7` plus the S3 change. It follows one scheduled prompt of one Claude channel and asks who posts its notice: the hook path, which is the live owner until the writer's frozen cohort is durable, or the writer, which reads the fire's own `isMeta` row after that.

## Behaviours

| Step | Code |
|---|---|
| `HookFire`, `HookDecide` | `tui_prompt_relay/hook_observer.rs` → `synthetic_start_wiring::post_passive_prompt_notice`. Silent once `writer_posts_input_notice` holds; otherwise it posts under `⏰`, or under `⌨️` when the row was not readable in time, or its POST fails. |
| `WriteRow` | Claude writes the fire's `user` row (`isMeta`, `scheduledTaskId`). |
| `Handoff` | `actor/notice_work.rs::start_notice_migration` records `MigrationStarted`. Rows below a source's lower are the floor, rows up to its upper are the frozen cohort, later rows are live. |
| `WriterRead` | `writer/input_notices.rs::native_input` → `Projection::project_record` → `persist_notice_evidence`. A floor row is skipped by `before_migration_floor`. |
| `Settle` | Cohort row: `input_notices/migration.rs::plan` — a legacy message under either header is debt, a proven absence posts. Live row: posts. The ledger's `Posted` outcome seals it. |
| `Restart` | The writer loses its memory and re-projects from ledger evidence (`prepare_notices`). |

## Rules

- `PostedAtMostOnce`: hook posts plus writer posts for the prompt never exceed one.
- `NoRetroactivePost`: the writer never posts a row from below the frozen lower.
- `EventuallyPostedOnce` (liveness, weak fairness on every step but `Restart`): a cohort or live prompt ends with exactly one notice.

## Results

Plain TLC 1.7.4, 2026-11-14, each config under one second.

| cfg | distinct states | result |
|---|---:|---|
| WriterScheduledNoticeFixed.cfg | 84 | GREEN |
| WriterScheduledNoticeBefore.cfg | 50 | `EventuallyPostedOnce` violated: the hook is silent after the handoff and the writer drops the `isMeta` row (`c81c1920e7`) |
| WriterScheduledNoticeNoHookSuppress.cfg | 79 | `PostedAtMostOnce` violated: hook and writer both post |
| WriterScheduledNoticeNoFloor.cfg | 64 | `NoRetroactivePost` and `PostedAtMostOnce` violated |
| WriterScheduledNoticePayloadOnly.cfg | 71 | `PostedAtMostOnce` violated: the hook announced a frozen fire under `⌨️`, the writer reads no `⏰` message as absence |
| WriterScheduledNoticeNoSeal.cfg | 83 | `PostedAtMostOnce` violated: a restart posts again |
| WriterScheduledNoticeQueuedHandoff.cfg | 107 | `PostedAtMostOnce` violated: known gap, see limits |

## Counterexample replay

- Before: `HookFire`, `WriteRow`, `Handoff`, `HookDecide` (silent). Code: `synthetic_start_wiring.rs:139` returns before posting, and `extract_claude_transcript_user_prompt_with_entry_id` returns `None` for the `isMeta` row, so `Projection::project` yields nothing. Confirmed by the S2 review's ledger count (0 scheduled rows among 397 notice evidence lines).
- PayloadOnly: `HookFire`, `WriteRow`, `HookDecide` posts `⌨️`, `Handoff`, `WriterRead`, `Settle` posts. Code before the guard: `plan` compared only `input.payload` with history. Pinned by `a_frozen_scheduled_fire_the_hook_announced_under_either_header_is_not_posted_again`.
- NoHookSuppress: pinned by `a_scheduled_fire_is_announced_by_the_hook_path_only_until_the_writer_takes_notices_over`.
- NoFloor: pinned by `scheduled_fires_are_ready_only_from_the_frozen_lower_on`.

## Limits

- `QueuedHandoff` is inherited, not introduced: when the hook decides before the handoff and the row is written after the frozen upper (an input queued behind a running turn while the channel hands over), the hook posts and the writer posts the live row. Typed inputs have the same gap. Closing it needs the writer to see the hook's announced prompt id.
- `HookDecide` is atomic; in code the check and the POST are a few milliseconds apart, and a cohort reconciliation reading history inside that window sees no message.
- A row written after the handoff and before this build, still in retained spool when the new build boots, is read as live and posted once. Rows already past the cursor are not read again.
- The model has one prompt and one restart. Evidence completeness, binding origin, clears and ownership receipts are the existing models' subject.
- No chart was generated: the `statechart` skill is not installed on this host; TLC ran directly.
