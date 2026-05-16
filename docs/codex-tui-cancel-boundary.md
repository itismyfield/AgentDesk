# Codex TUI Cancel Boundary and Relay Suppression

Issue: #2172
Related: #2163 (generic tmux stop), #2173 (direct TUI launch parity),
#2175 / PR #2248 (Codex exec policy after direct TUI hosting),
#2249 / #2250 (cancel-aware exec follow-ups).

This ADR defines the contract for what happens when a user cancels an
in-flight Codex TUI turn — where the cancel signal stops propagating,
which side-effects fire exactly once, and what the user sees in Discord.

It complements `docs/codex-exec-policy.md`: that ADR classifies which
runtime paths may use `codex exec --json`. This one defines, for the
Direct TUI path specifically, where cancel terminates.

---

## What `/stop` actually means for Codex TUI

A `/stop` reaction or `/stop` command for a Codex Direct TUI turn must:

1. Interrupt the running Codex CLI (interactive C-c keystroke).
2. Drop the cooperative cancel flag on the shared `CancelToken`.
3. Stop relaying any further rollout output for that turn to Discord.
4. Mark the inflight turn as cancelled exactly once.

It must NOT, by default:

- Kill the tmux pane (Codex TUI is the pane foreground; killing it tears
  the session down and the next turn must respawn — see #2163).
- Kill the Codex child PGID via SIGKILL (the C-c keystroke is the
  interactive interrupt; SIGKILL is the hard-stop backstop only).

The hard-stop backstop kicks in only when the cooperative interrupt fails
to return the TUI to idle within `PROVIDER_HARD_STOP_GRACE` and the
cleanup policy is `CleanupSession`. The
`hard_stop_pid_for_unresponsive_provider` guard explicitly skips the kill
when the candidate PID is the tmux pane foreground (i.e. the Codex CLI
itself) so the pane is not collateral.

## Cancel/completion priority

A Codex TUI turn is "done" exactly when one of the following fires —
whichever comes first wins:

1. **User cancel.** `cancel_requested(cancel_token)` returns `true`.
   - This is the relay boundary. See "Relay suppression" below.
2. **Rollout terminal event.** The rollout JSONL emits an assistant
   message; the tailer drains the terminal-drain window and emits
   `StreamMessage::Done` with the captured assistant text.
3. **Codex readiness / session death.** `tmux_session_has_live_pane`
   returns `false` before the assistant text is observed. The tailer
   returns `ReadOutputResult::SessionDied` and the streaming entrypoint
   emits a "session ended before producing a response" `Done`.
4. **Assistant-response deadline (#2182 follow-up).** The tailer waited
   at EOF for `DEFAULT_ASSISTANT_RESPONSE_DEADLINE_SECS` (30 min) without
   any assistant text. The tailer emits a "timed out" `Done`.

Hooks (Stop hook from Codex hook relay #2170/PR #2184) are not in this
priority list yet because their plumbing into the tail loop has not
landed. When it does, hook-terminal events join this list between (1)
and (2): a Codex-driven stop becomes the canonical signal, and the
rollout terminal event becomes the backstop. The relay-suppression
contract below does not change.

## Relay suppression — the cancel boundary

`tail_rollout_file_until_assistant_response` is the single producer for
Codex TUI rollout-derived `StreamMessage`s. Its `Sender<StreamMessage>`
feeds the turn bridge, which relays to Discord.

The cancel boundary is enforced **at the producer**, in
`src/services/codex_tui/rollout_tail.rs`:

- Every `send` flows through `RelaySuppressionSender`. Once
  `cancel_token.cancelled` is true, every subsequent `send` is dropped
  silently.
- There is no "drain in-flight assistant text first" carve-out. A user
  cancel is a hard boundary: post-cancel rollout records (including any
  partial assistant text that arrived after the user pressed stop) are
  dropped, even if the tail has not yet observed cancel in its outer
  loop check.
- The cancel check runs both (a) at the top of the read loop (so the
  loop returns `ReadOutputResult::Cancelled` promptly) and (b) inside
  the wrapper on each `send` (so any line drained from a multi-line
  read after cancel is guaranteed not to reach the bridge).

Why the producer and not the consumer: the bridge already breaks out of
its loop on `cancel_requested`, but the rollout-tail thread continues to
run and would otherwise enqueue messages into `rx` that the bridge
either ignores (acceptable) or races to consume before exit (not
acceptable, because the consume path mutates inflight state). Dropping
at the producer is the only way to guarantee that a cancelled turn
emits zero post-cancel `StreamMessage`.

### Race: cancel vs. final completion frame

If the rollout emits a `Done` frame and the user cancels in the same
millisecond, two paths are possible:

- The `Done` was already enqueued into `rx` before the cancel flag flipped.
  The bridge may or may not consume it; the bridge's cancel-break in
  `mod.rs` makes this best-effort. Acceptable: the turn is reported as
  cancelled, no later output is emitted, and the user sees the cancel.
- The `Done` is generated AFTER the cancel flag flipped. The
  `RelaySuppressionSender` drops it. The tail returns
  `ReadOutputResult::Cancelled`. The bridge's cancel block finalises the
  turn exactly once. The user sees the cancel.

In both cases the post-cancel relay boundary holds.

### Exactly-once finalisation

The bridge's inflight state has a single cancel block (the first
`cancel_requested` arm in `turn_bridge/mod.rs` around line 1649). The
`CompletionGuard` and `InflightCleanupGuard` `Drop` impls ensure that a
panic or early return still flushes completion and clears the inflight
file. The relay-suppression contract guarantees that the
producer-side stream goes silent, so the bridge's cancel-arm
`break;` is reached at most once per turn — finalisation runs exactly
once.

## Foreground pane PID vs. wrapper child PID

The provider-CLI PID resolver in
`turn_bridge::tmux_runtime::select_provider_pid_in_pane` has two stable
contracts that interrupt routing depends on:

- **Direct TUI:** the provider CLI runs as the tmux pane foreground. The
  resolver returns `pane_pid` itself. SIGINT goes directly to the Codex
  CLI. (Test: `select_provider_pid_returns_pane_pid_when_pane_is_claude_tui`.)
- **Wrapper mode:** the pane foreground is the agentdesk wrapper; the
  Codex CLI is a child. The resolver returns the child PID. (Test:
  `select_provider_pid_still_finds_wrapped_provider_descendant`.)

Both behaviours are pinned by unit tests in `tmux_runtime.rs` and MUST
NOT regress. The TUI-mode regression guard in
`hard_stop_pid_for_unresponsive_provider` (line 436) also relies on
this: if the candidate kill PID is `pane_pid`, the hard-stop is skipped
so SIGKILL never tears down the pane underneath a cooperatively
stoppable Codex TUI.

## What this ADR explicitly does NOT do

- Migrate `execute_command_simple_with_timeout` to cancel-aware exec
  (covered by #2249 / #2250).
- Re-define the cancel infrastructure (cancel tombstones, restart-mode
  handshake, watcher replacement). Those are owned by
  `relay-state-contract.md` and the `#1222` family.
- Add Codex Stop hook integration into the cancel-priority list (will
  land when the hook-relay plumbing lands).

## Test coverage

The contract above is pinned by:

- `relay_suppression_drops_post_cancel_output` (this PR) — appends a
  post-cancel assistant text to a live rollout and asserts no
  `Text` or `Done` frame is delivered downstream.
- `select_provider_pid_returns_pane_pid_when_pane_is_claude_tui`
  (existing, #1260 / #2163) — direct-TUI pane PID resolution.
- `select_provider_pid_still_finds_wrapped_provider_descendant`
  (existing) — wrapper child PID resolution.
- `stop_active_turn_runs_interrupt_before_cancel`
  (existing, #1218) — interrupt-before-cancel ordering invariant.

Any change to the cancel boundary must update this ADR and the
corresponding tests in the same change.
