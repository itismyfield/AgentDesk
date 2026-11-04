# Input owner entry notes

These tables describe input readiness, launch outcomes and adjacent entry points
as of `2b17275c67`.

## Readiness and launch outcomes

| Entry or outcome | Behavior | Code |
| --- | --- | --- |
| Claude cold first prompt | `FreshTurn` readiness calls the pane readiness wait with `transcript_path = None`. The readiness timeout is 120 seconds per attempt, at most two attempts with a 5 second backoff between them. This is a readiness budget of 120 × 2 + 5 seconds; the separate busy-transcript wait does not make this a 900 second cold-readiness wait. | [first_input::submit/submit_using](../../src/services/claude/first_input.rs), [readiness constants and wait_for_prompt_ready](../../src/services/claude_tui/input.rs), [attempt/backoff constants](../../src/services/claude.rs) |
| Codex Herdr Enter | `run_herdr` reaches the common `Plan::step` Enter arm via `run_on`. It sleeps for `PROMPT_INPUT_BEFORE_ENTER_SETTLE` (200 ms), checks cancellation again, then sends Enter. The adapter has a settle delay. | [Codex host_input](../../src/services/codex_tui/host_input.rs) |
| Unconsumed launch hook | While a hook is waiting, `launch_refuses_live` refuses a live-session first-prompt route. `launch_hook` takes the hook once; dropping its launch guard clears an unused hook. An unused hook is not proof that an owned submission ran. | [owner_input launch scope](../../src/services/claude_tui/owner_input.rs), [unconsumed_launch_hook_is_not_owned](../../src/services/discord/router/message_handler/provider_dispatch/owner_input_tests.rs) |
| Owner-settled Claude first prompt returns an error | `OwnedLaunch::Settled` returns directly from the fresh lifecycle. The driver does not resend, observe the response or tear down the session/owner marker after this result. A pre-owner legacy failure follows the existing teardown policy; Held, Guarded and fence refusals retain their existing behavior. | [fresh_lifecycle::run](../../src/services/claude_tui/hosting/fresh_lifecycle.rs), [first_input::submit_owned](../../src/services/claude/first_input.rs) |

## Adjacent entries

| Entry | Response and state ownership | Code |
| --- | --- | --- |
| `agentdesk.turn.start` JavaScript host call | Schedules `start_agent_handoff_turn` on the main runtime and immediately returns JSON with `ok: true`, `status: dispatched`, and `to_agent_id`. Actual handoff success is logged with `response.to_value()`; failures log the HTTP status and error. The synchronous JavaScript response acknowledges dispatch, not input delivery. | [turn_ops::start_turn_raw](../../src/engine/ops/turn_ops.rs) |
| `CancelQueuedPrimaryMessage` | The mailbox request is routed through `v2::control::route` when a V2 owner is adopted; that arm calls the owner's `cancel`. Without a V2 owner, the legacy mailbox arm removes the primary message and persists or restores its queue. This is separate from cancelling the active turn. | [mailbox request and legacy arm](../../src/services/turn_orchestrator.rs), [V2 control route](../../src/services/turn_orchestrator/queued_head/v2/control.rs) |
