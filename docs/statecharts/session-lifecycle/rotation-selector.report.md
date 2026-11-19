# Channel selector after an in-pane /clear

Code SHA: before `4f1fe5236d`, fix1 `c739fa4947`, fixed `8be3fd921e`.

When `/clear` is typed into a Claude pane, the hook moves the pane's runtime binding to a new session and records a rotation.
The channel's cached provider session (`cache`) and the stored selector (`sessions.claude_session_id`, `db`) must follow it before the queued Discord input's turn reads them.
The model follows one rotation, one queued input, at most one selector store outage, a user `!clear`, a newer stored selector written by another writer, and optionally one dcserver restart.
The constants pick the starting cache (`old`, or `none` when the channel holds no session), the pane owner (`watcher`, or `herdr` for a reconnected Herdr pane with no watcher) and whether the pane-name key resolves the channel's canonical row.

`Design` picks the settle pass:

- `before`: releasing the queue for a frozen turn comes first. The selector moves only when the rotation is retired, and the record is retired even when the store write fails. A turn start reads the cache as it is.
- `fix1`: the selector moves before the queue is released and a turn start applies a pending rotation, but the stored selector moves only after the cache moved, only through the pane-name key, and only for a pane with a watcher. A missed row counts as done.
- `fixed`: as `fix1`, but the stored selector of the channel's canonical row moves off the old session even when the cache is empty, a missing row or a failed write keeps the rotation, and a watcherless Herdr pane moves its selector while its rotation stays.

## Rules

- `NoOldResume` (INVARIANT): the first turn after the rotation never resumes the session the pane left.
- `NoOverwrite` (INVARIANT): a user clear or a newer stored selector is not overwritten.
- `StoredSelectorFollows` (PROPERTY): once the pane rotated, the stored selector eventually leaves the old session. The settle pass and store recovery are weakly fair.

## Results

| Config | Breaks | Path |
|---|---|---|
| `SessionRotationSelectorBefore.cfg` | `NoOldResume`, `NoOverwrite`, `StoredSelectorFollows` | the queued turn starts before the settle pass and resumes the old session; a newer stored selector is overwritten; a failed store write is retired and never retried |
| `SessionRotationSelectorFix1Empty.cfg` | `NoOldResume`, `StoredSelectorFollows` | the channel holds no cached session, the settle pass retires without writing the store, and the next turn restores the old session from it |
| `SessionRotationSelectorFix1Herdr.cfg` | `StoredSelectorFollows` | a watcherless Herdr pane never gets a settle owner, so the stored selector stays old |
| `SessionRotationSelectorFix1Key.cfg` | `StoredSelectorFollows` | the pane-name key misses the canonical row and the miss counts as done |
| `SessionRotationSelector.cfg` | nothing | 636 states over every starting cache, owner and key |
| `SessionRotationSelectorRestart.cfg` | `NoOldResume`, `StoredSelectorFollows` | known gap: the store write fails, dcserver restarts before the retry and drops the in-memory rotation, and the next turn restores the old session from the store |

The committed chart is the fixed design for an empty cache, a watcherless Herdr pane and a missed pane-name key (126 states); the full fixed config exceeds the chart budget.

## Code map

| Model | Code |
|---|---|
| `Adopt` | `src/services/tui_prompt_dedupe/runtime_binding.rs` rotation record at hook adoption |
| `FrozenFinish` | `src/services/discord/turn_finalizer/terminal_handler.rs` mailbox release and queue eligibility after a normal finish |
| `WatcherSettle` / `BeforeRelease` / `BeforeRetire` | `src/services/discord/tui_prompt_relay/session_rotation_settle.rs` `settle_one_claude_session_rotation`, `session_rotation_settle/selector.rs` `follow_rotation_selector` |
| `HerdrFollow` | `session_rotation_settle/selector.rs` `follow_herdr_rotation_selector` (admitted execution nonce and the channel row's hosted record) |
| `db` write, `keyMatch` | `src/db/dispatched_sessions/rotation_selector.rs` conditional move by row id; the row is the channel's canonical row (`load_hosted_execution_pg`) |
| `TurnStart` | `src/services/discord/router/message_handler/intake_turn/session_preparation.rs` `restore` |
| `Clear` | `src/services/discord/admin_host_guard.rs` and `commands/control/native.rs` clear the cache; the stored selector is cleared and fenced |
| `DbNewer` | another writer of the stored selector, for example `/resume` (`db/dispatched_sessions.rs` `rebind_session_provider_pg`) |
| `Restart` | in-memory cache and rotation ledger are lost; the next turn restores from the store |
