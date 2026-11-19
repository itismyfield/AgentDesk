# Channel selector after an in-pane /clear

Code SHA: before `4f1fe5236d`, fix1 `c739fa4947`, fix2 `b684eb68c8`, fix3 `cc429bdea4`, fixed: this change.

When `/clear` is typed into a Claude pane, the hook moves the pane's runtime binding to a new session and records a rotation.
The channel's cached provider session (`cache`) and the stored selector (`sessions.claude_session_id`, `db`) must follow it before the queued Discord input's turn reads them.
The model follows one rotation, one queued input, at most one selector store outage, a user `!clear`, a newer stored selector written by another writer, and optionally one dcserver restart.
The constants pick the starting cache (`old`, or `none` when the channel holds no session), the pane owner (`watcher`, or `herdr` for a reconnected Herdr pane with no watcher), whether the pane-name key resolves the channel's canonical row, whether the pane may hop once more (`new` to `c`) before any Discord turn, and whether the idle loop runs on the channel's own bot.
Constants also pick whether the first hook arrived before its transcript and was adopted from the deferred queue, which leaves its entry at the queue front while the rotation blocks later adoptions; the next hook then waits behind it until the deferred retry pass may run.
A turn start is two steps under the channel transition: it reads the selector (`TurnRead`), then writes the cache and claims (`TurnFinish`). A user `!clear` takes the same transition.

`Design` picks the settle pass:

- `before`: releasing the queue for a frozen turn comes first. The selector moves only when the rotation is retired, and the record is retired even when the store write fails. A turn start reads the cache as it is.
- `fix1`: the selector moves before the queue is released and a turn start applies a pending rotation, but the stored selector moves only after the cache moved, only through the pane-name key, and only for a pane with a watcher. A missed row counts as done.
- `fix2`: as `fix1`, but the stored selector of the channel's canonical row moves off the old session even when the cache is empty, a missing row or a failed write keeps the rotation, and a watcherless Herdr pane moves its selector while its rotation stays. The settle pass ignores the channel transition, compares only with the first session, and reads the row with the idle loop's bot.
- `fix3`: as `fix2`, but the settle pass waits while a turn start holds the channel transition, a selector naming any session the pane left follows it, and a watcherless Herdr pane uses the channel's own bot runtime. The watcher branch still uses the idle loop's runtime, and a rotation holds later queued adoptions until it retires.
- `fixed`: as `fix3`, but the watcher branch uses the channel's own bot runtime too, and a watcherless pane's rotation whose selector followed stops holding later adoptions.

## Rules

- `NoOldResume` (INVARIANT): the first turn after the rotation never resumes a session the pane left.
- `NoOverwrite` (INVARIANT): a user clear or a newer stored selector is not overwritten.
- `FollowedNamesTarget` (INVARIANT): a selector marked followed names no session the pane left.
- `StoredSelectorFollows` (PROPERTY): once the pane rotated, the stored selector eventually leaves every session the pane left.
- `QueuedHookAdopts` (PROPERTY): a normal hook waiting behind the queue front is eventually adopted.
The settle pass, store recovery, the turn start's finish and the deferred retry pass are weakly fair.

## Results

| Config | Breaks | Path |
|---|---|---|
| `SessionRotationSelectorBefore.cfg` | `NoOldResume`, `NoOverwrite`, `StoredSelectorFollows` | the queued turn starts before the settle pass and resumes the old session; a newer stored selector is overwritten; a failed store write is retired and never retried |
| `SessionRotationSelectorFix1Empty.cfg` | `NoOldResume`, `StoredSelectorFollows` | the channel holds no cached session, the settle pass retires without writing the store, and the next turn restores the old session from it |
| `SessionRotationSelectorFix1Herdr.cfg` | `StoredSelectorFollows` | a watcherless Herdr pane never gets a settle owner, so the stored selector stays old |
| `SessionRotationSelectorFix1Key.cfg` | `NoOldResume`, `StoredSelectorFollows` | the pane-name key misses the canonical row and the miss counts as done; a turn start that read before the settle pass writes the old session back |
| `SessionRotationSelectorFix2Transition.cfg` | `NoOldResume` | a turn start reads the old stored selector, the settle pass moves it and retires, and the turn start writes the old session into the cache |
| `SessionRotationSelectorFix2Hop.cfg` | `NoOldResume`, `StoredSelectorFollows` | a watcherless Herdr pane follows `new`, hops to `c`, and the selector naming `new` is kept as if a user chose it |
| `SessionRotationSelectorFix2Bot.cfg` | `StoredSelectorFollows` | the idle loop runs on another bot, whose token finds no row for the channel |
| `SessionRotationSelectorFix3Runtime.cfg` | `StoredSelectorFollows` | the idle loop runs on another bot; its watcher branch reads that bot's row, counts the miss as done and retires, so the channel bot's stored selector stays old |
| `SessionRotationSelectorFix3Queue.cfg` | `QueuedHookAdopts` | the first hook was adopted from the queue and its entry stays at the front while the watcherless pane's rotation stays, so the next hook waits forever |
| `SessionRotationSelector.cfg` | nothing | 10,724 states over every starting cache, owner, key, hop, bot and queue |
| `SessionRotationSelectorRestart.cfg` | `NoOldResume`, `StoredSelectorFollows` | known gap: the store write fails, dcserver restarts before the retry and drops the in-memory rotation, and the next turn restores the old session from the store |

The committed chart is the fixed design for a watcherless Herdr pane of another bot's channel whose first hook was queued, followed by a second hook; the full fixed config exceeds the chart budget.

## Code map

| Model | Code |
|---|---|
| `Adopt` | `src/services/tui_prompt_dedupe/runtime_binding.rs` rotation record at hook adoption |
| `FrozenFinish` | `src/services/discord/turn_finalizer/terminal_handler.rs` mailbox release and queue eligibility after a normal finish |
| `WatcherSettle` / `BeforeRelease` / `BeforeRetire` | `src/services/discord/tui_prompt_relay/session_rotation_settle.rs` `settle_one_claude_session_rotation`, `session_rotation_settle/selector.rs` `follow_rotation_selector` |
| `HerdrFollow` | `session_rotation_settle/selector.rs` `follow_herdr_rotation_selector` (admitted execution nonce and the channel row's hosted record) |
| `db` write, `keyMatch` | `src/db/dispatched_sessions/rotation_selector.rs` conditional move by row id; the row is the channel's canonical row (`load_hosted_execution_pg`) |
| `TurnRead` / `TurnFinish` | the intake's channel transition (`router/turn_start.rs` `IntakeRuntimeTransition`) around `intake_turn/session_preparation.rs` `restore` and the mailbox claim |
| `Free` | `session_rotation_settle/selector.rs` `follow_channel_selector` try-locks `session_transition_lock` |
| `Hook`, `Left` | `tui_prompt_dedupe/session_rotation.rs` `record_claude_session_rotation` keeps `left_session_ids` |
| `pending`, `front`, `Retry`, `Blocks` | `claude_tui/hook_server/adoption_retry.rs` deferred queue; `claude_rotation_blocks_adoption` decides whether a rotation holds it |
| `bot`, `RowFound` | `selector.rs` `channel_runtime` (`HealthRegistry::shared_for_provider_on_channel`) for both the watcher and the Herdr branch |
| `Clear` | `src/services/discord/admin_host_guard.rs` and `commands/control/native.rs` clear the cache; the stored selector is cleared and fenced |
| `DbNewer` | another writer of the stored selector, for example `/resume` (`db/dispatched_sessions.rs` `rebind_session_provider_pg`) |
| `Restart` | in-memory cache and rotation ledger are lost; the next turn restores from the store |
