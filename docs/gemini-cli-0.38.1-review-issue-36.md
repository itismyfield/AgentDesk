# Gemini CLI 0.38.0 -> 0.38.1 Review for AgentDesk

Issue: `#36`  
Date: `2026-04-18`

## Executive summary

Recommendation:

- Apply now: none
- Needs design: none from this patch delta alone
- Hold: keep the current exact-UUID resume prohibition intact

The `0.38.0 -> 0.38.1` update does not expose a new AgentDesk-relevant feature candidate that should be merged immediately. Local CLI comparison did not show a top-level command-surface delta, and the current AgentDesk Gemini integration should keep its strict resume-selector guardrail.

## What changed in Gemini CLI 0.38.1

Verified on `2026-04-18` with local CLI comparisons between:

- `gemini 0.38.1`
- `npx -y @google/gemini-cli@0.38.0`

Observed CLI delta:

- No top-level `gemini --help` diff was observed from the local comparison.

Evidence:

- local `gemini --version`
- local `npx -y @google/gemini-cli@0.38.0 --version`
- local `gemini --help`
- local `npx -y @google/gemini-cli@0.38.0 --help`

## Current AgentDesk integration shape

AgentDesk's Gemini path is intentionally conservative:

- `src/services/discord/router/message_handler/provider_dispatch.rs` (`execute`) calls `gemini::execute_command_streaming(...)` with a model override only.
- `src/services/gemini.rs:363` through `src/services/gemini.rs:365` explicitly reject remote-profile execution.
- `src/services/gemini.rs:367` through `src/services/gemini.rs:382` normalize and log the runtime resume selector before execution.
- `src/services/gemini.rs:1081` through `src/services/gemini.rs:1120` coerce UUID-like session identifiers to `latest` instead of passing them through as exact resume selectors.

That last guardrail is important because Gemini CLI can silently fork history if a UUID-like selector is not actually resumable.

## Classification

### Apply now

None.

Why:

- No new top-level CLI surface was observed in the patch comparison.
- The current AgentDesk integration already constrains Gemini to the supported model-plus-streaming path.
- There is no verified new feature from `0.38.1` that should change Discord UX, model selection, or session transport immediately.

### Needs design

None from this patch delta alone.

Reason:

- Any future Gemini session-management expansion should wait for a verified selector contract that is safe for automated resume behavior.

### Hold

### 1. Relaxing exact UUID resume behavior

Value:

- Potentially high if Gemini later exposes a reliable exact-resume contract.

Effort:

- Medium.

Risk:

- High. A mistaken selector can silently create a new session and break continuity expectations.

Disposition:

- Keep the current UUID-to-`latest` coercion and invalid-selector rejection logic intact.

## Conclusion

The `0.38.0 -> 0.38.1` patch is safe to keep, but it does not create an immediate AgentDesk implementation target. The important decision here is not to weaken the existing Gemini resume guardrail until upstream exposes a selector contract that can be verified safely.
