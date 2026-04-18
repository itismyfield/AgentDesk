# Claude Code 2.1.104 -> 2.1.112 Review for AgentDesk

Issue: `#34`  
Date: `2026-04-18`

## Executive summary

Recommendation:

- Apply now: none
- Needs design: optional Claude reasoning-effort exposure if AgentDesk wants a first-class effort control
- Hold: tool allow/deny help-text syntax changes

The update from `2.1.104` to `2.1.112` does not force an AgentDesk compatibility change on the current execution path. The only directly observable CLI-level delta from local comparison is that Claude now advertises `xhigh` as an extra `--effort` level. AgentDesk does not currently expose or pass an effort flag, so there is no safe "just wire it in" patch without a separate UX and policy decision.

## What changed in Claude Code 2.1.112

Verified on `2026-04-18` with local CLI comparisons between:

- `claude 2.1.112`
- `npx -y @anthropic-ai/claude-code@2.1.104`

Observed top-level CLI deltas:

- `claude --help` now advertises `--effort <level>` values `low, medium, high, xhigh, max`.
- The `--allowed-tools` and `--disallowed-tools` help examples changed from `Bash(git:*)` style wording to `Bash(git *)` example wording.

Evidence:

- local `claude --help`
- local `npx -y @anthropic-ai/claude-code@2.1.104 --help`

## Current AgentDesk integration shape

AgentDesk's Claude execution path is still centered on model selection, system prompt composition, and streamed tool usage:

- `src/services/discord/router/message_handler.rs:1481` calls `claude::execute_command_streaming(...)` per turn.
- `src/services/claude.rs:501` through `src/services/claude.rs:505` only wire a `--model` override into the Claude CLI arguments.
- `src/services/discord/model_picker_interaction.rs:211` through `src/services/discord/model_picker_interaction.rs:243` only validate and persist model-picker choices, not reasoning-effort controls.

There is no existing AgentDesk surface for:

- per-channel Claude effort level
- per-dispatch Claude effort level
- policy gating for `xhigh` cost and latency tradeoffs

## Classification

### Apply now

None.

Why:

- The current Claude execution path remains valid after the update.
- No required flag migration or resume behavior change was observed.
- The observed tool-allow/deny help-text example change is documentation-level, not an integration contract change.

### Needs design

### 1. Expose Claude reasoning effort separately from model selection

Value:

- `xhigh` may be useful for expensive but high-value review or synthesis work where operators explicitly want more reasoning depth.

Effort:

- Medium to high.

Risk:

- Higher latency and cost without a clear Discord or dispatch UX can create confusing behavior and uneven operator expectations.

Why this is not an apply-now patch:

- AgentDesk currently stores only model override state, not effort state.
- Reusing `/model` to smuggle effort would be semantically wrong and create configuration debt.

Suggested follow-up:

- Only create a separate implementation card if the team wants a dedicated effort control surface with explicit policy and cost semantics.

### Hold

### 1. Help-text example syntax changes for tool allow/deny patterns

Value:

- Low.

Effort:

- Low.

Risk:

- Low, but also low payoff.

Reason:

- The observed delta is in help examples, not a verified break in AgentDesk's current Claude invocation contract.

## Conclusion

The `2.1.104 -> 2.1.112` upgrade is safe to keep, but it does not justify an immediate AgentDesk compatibility patch. The only meaningful new candidate is `xhigh` effort exposure, and that requires a deliberate product and operator-control design rather than a silent transport-layer change.
