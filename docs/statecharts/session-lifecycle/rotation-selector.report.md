# Channel selector after an in-pane /clear

Code SHA: before `4f1fe5236d`, fixed `864b80ff98`.

When `/clear` is typed into a Claude pane, the hook moves the pane's runtime binding to a new session and records a rotation.
The channel's cached provider session (`cache`) and the stored selector (`sessions.claude_session_id`, `db`) must follow it before the queued Discord input's turn reads them.
The model follows one rotation, one queued input, at most one selector store outage, a user `!clear`, a newer stored selector written by another writer, and optionally one dcserver restart.

`Design` picks the settle pass:

- `before`: releasing the queue for a frozen turn comes first. The selector moves only when the rotation is retired, and the record is retired even when the store write fails. A turn start reads the cache as it is.
- `fixed`: the selector moves before the queue is released. The stored selector moves only off the old session, and a failed write keeps the rotation for the next pass. A turn start applies a rotation that is still pending.

## Rules

- `NoOldResume` (INVARIANT): the first turn after the rotation never resumes the session the pane left.
- `NoOverwrite` (INVARIANT): a user clear or a newer stored selector is not overwritten.
- `StoredSelectorFollows` (PROPERTY): once the pane rotated, the stored selector eventually leaves the old session. The settle pass and store recovery are weakly fair.

## Results

| Config | Breaks | Path |
|---|---|---|
| `SessionRotationSelectorBefore.cfg` | `NoOldResume`, `NoOverwrite`, `StoredSelectorFollows` | the queued turn starts before the settle pass and resumes the old session; a newer stored selector is overwritten; a failed store write is retired and never retried |
| `SessionRotationSelector.cfg` | nothing | 100 states |
| `SessionRotationSelectorRestart.cfg` | `NoOldResume`, `StoredSelectorFollows` | known gap: the store write fails, dcserver restarts before the retry and drops the in-memory rotation, and the next turn restores the old session from the store |

## Code map

| Model | Code |
|---|---|
| `Adopt` | `src/services/tui_prompt_dedupe/runtime_binding.rs` rotation record at hook adoption |
| `FrozenFinish` | `src/services/discord/turn_finalizer/terminal_handler.rs` mailbox release and queue eligibility after a normal finish |
| `FixedSettle` / `BeforeRelease` / `BeforeRetire` | `src/services/discord/tui_prompt_relay/session_rotation_settle.rs` `settle_one_claude_session_rotation`, `session_rotation_settle/selector.rs` |
| `db` write | `src/db/dispatched_sessions/rotation_selector.rs` conditional move |
| `TurnStart` | `src/services/discord/router/message_handler/intake_turn/session_preparation.rs` `restore` |
| `Clear` | `src/services/discord/admin_host_guard.rs` and `commands/control/native.rs` clear the cache; the stored selector is cleared and fenced |
| `DbNewer` | another writer of the stored selector, for example `/resume` (`db/dispatched_sessions.rs` `rebind_session_provider_pg`) |
| `Restart` | in-memory cache and rotation ledger are lost; the next turn restores from the store |
