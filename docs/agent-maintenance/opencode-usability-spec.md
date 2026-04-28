# OpenCode Usability Parity Spec

> Status: implementation-ready spec
>
> Last reviewed: 2026-04-28 against `origin/main` @ `fd052ce5`
>
> Scope: raise the Discord-facing OpenCode experience to the practical level
> already expected from Claude/Codex, without pretending the provider backends
> are identical.

## Problem

OpenCode is registered as a first-class provider, but its current runtime path
is not yet as defensive as the Claude/Codex paths. In the monitoring channel
case that motivated this spec, Discord-visible output included prompt/context
blocks such as authoritative instructions, tool policy, role binding, and
process narration that should have remained hidden implementation context.

This is not just a prompt-writing issue. The code paths differ:

- Claude and Codex run through mature CLI/tmux-oriented provider paths.
- OpenCode starts `opencode serve`, sends a REST prompt, reads SSE events, and
  normalizes those events into AgentDesk `StreamMessage`s.
- The shared Discord formatter currently has a Codex-specific tool-log filter,
  but no provider-neutral hidden-context sanitizer.
- The runtime MCP sync path explicitly handles Claude and Codex, while OpenCode
  currently relies on external `opencode.json` configuration.

The goal is therefore usability parity, not identical implementation.

## Current Implementation

### Provider registry and execution

OpenCode is present in the provider registry:

- `src/services/provider.rs:133` registers `id: "opencode"`.
- `src/services/provider.rs:139-143` advertises structured output, resume, and
  tool-stream support.
- `src/services/provider.rs:150-151` sets a 128k default context window and
  `managed_tmux_backend: false`.

That last point is the core architectural difference. Claude and Codex use the
managed tmux backend (`src/services/provider.rs:88`, `:109`); OpenCode does
not. The provider dispatch still routes OpenCode through the same high-level
execution switch:

- simple calls: `src/services/provider_exec.rs:124-126`
- structured/streaming calls: `src/services/provider_exec.rs:243-256`
- Discord router calls: `src/services/discord/router/message_handler.rs:1461`
  and `:3389`

### OpenCode runtime path

The OpenCode provider backend is `src/services/opencode.rs`.

The current flow is:

1. Resolve the binary from `AGENTDESK_OPENCODE_PATH` or `PATH`
   (`src/services/opencode.rs:29-38`).
2. Spawn `opencode serve --hostname 127.0.0.1 --port <N>`
   (`src/services/opencode.rs:151-166`).
3. Wait for `/global/health`, create a session, connect to `/event`, then send
   the prompt to `/session/{id}/prompt_async`
   (`src/services/opencode.rs:181-224`).
4. Read SSE lines until `session.idle` and emit `StreamMessage::Done`
   (`src/services/opencode.rs:316-390`).

The prompt is currently sent as a single text part:

- `src/services/opencode.rs:283-290` concatenates `system_prompt` and `prompt`
  as plain text.
- `_allowed_tools`, `_session_id`, `_tmux_session_name`, report channel, report
  provider, and `_compact_percent` are accepted by the function signature but
  not applied (`src/services/opencode.rs:94-107`).
- `remote_profile` is explicitly unsupported
  (`src/services/opencode.rs:109-113`).

This means OpenCode is not receiving the same role separation that Claude gets
through `--append-system-prompt`, and it is not receiving the same structured
turn wrapper that Codex gets through `compose_structured_turn_prompt`.

### OpenCode SSE handling

OpenCode currently normalizes these event shapes:

- `session.status` -> `StreamMessage::StatusUpdate`
  (`src/services/opencode.rs:423-444`)
- `part.type == "text"` -> append raw text and emit `StreamMessage::Text`
  (`src/services/opencode.rs:447-460`)
- `thinking` / `redactedThinking` -> `StreamMessage::Thinking`
  (`src/services/opencode.rs:461-468`)
- `tool-use` -> `StreamMessage::ToolUse`
  (`src/services/opencode.rs:469-479`)
- `tool-result` -> `StreamMessage::ToolResult`
  (`src/services/opencode.rs:480-492`)
- `message.completed` -> final fallback when no text was streamed yet
  (`src/services/opencode.rs:499-518`)
- `session.idle` -> terminal completion (`src/services/opencode.rs:521-524`)
- `session.error` / `error` -> terminal error
  (`src/services/opencode.rs:526-539`)

Current gaps:

- `message.part.delta` events are not handled on `origin/main`.
- `message.part.updated` events are not handled on `origin/main`.
- repeated full text snapshots cannot be de-duplicated by part id because the
  implementation stores one `String` rather than per-part snapshots.

There is existing partial work on
`origin/codex/provider-cli-opencode-audit-20260428` that adds a
`SseMessageState`, text-part snapshots, delta handling, updated-part handling,
and regression tests. That branch is not the source of truth for this spec, but
it is a useful implementation reference.

### Discord output formatting

The common formatting entry point is:

- `src/services/discord/formatting.rs:1915`
  `format_for_discord_with_provider(...)`

Today it only applies a provider-specific filter for Codex:

- `src/services/discord/formatting.rs:1880` defines
  `filter_codex_tool_logs(...)`.
- `src/services/discord/formatting.rs:1920-1924` calls that filter only for
  `ProviderKind::Codex`.
- all providers then pass through placeholder stripping and mechanical Discord
  formatting.

There is no shared outbound sanitizer for hidden AgentDesk context blocks such
as `[Authoritative Instructions]`, `[Tool Policy]`, `[Shared Agent Rules]`,
`[Channel Role Binding]`, `[ADK API Usage]`, or the leading Discord system
context. If any provider echoes those blocks, the current formatter will
deliver them.

### MCP and skills

MCP parity is currently partial:

- Claude receives runtime MCP config through
  `src/services/mcp_config.rs:49-51`.
- Codex syncs managed MCP servers with `codex mcp add/remove`
  (`src/services/mcp_config.rs:60-110`).
- `provider_has_memento_mcp(...)` checks runtime config, Claude global config,
  and Codex config, but there is no OpenCode-specific config detector/syncer
  in `src/services/mcp_config.rs`.
- server memory API only considers Claude/Codex for memento-MCP availability
  (`src/server/routes/memory_api.rs:57-61`).

Skill command prompting does recognize OpenCode:

- `src/services/discord/commands/skill.rs:75-85` renders an OpenCode skill
  instruction.

However, the skill path depends on the active OpenCode runtime having the
expected skill files/config available. There is no AgentDesk-level OpenCode
skill-sync or MCP-sync contract in this repo yet.

## Target Experience

OpenCode should be safe and useful enough that operators can use it in Discord
without switching providers for routine inspection tasks.

P0 target:

- hidden system/developer/channel instructions never appear in Discord output;
- OpenCode final answers are delivered as final answers, not prompt prelude or
  raw process narration;
- OpenCode SSE streaming handles text deltas and full text snapshots without
  duplicate text;
- memento/Serena-style MCP availability is detectable for OpenCode, or the UI
  clearly states that AgentDesk cannot verify it yet;
- docs and tests make the remaining Claude/Codex differences explicit.

P1 target:

- OpenCode can use managed MCP sync from AgentDesk, preserving manual
  `opencode.json` fields;
- health/doctor surfaces report OpenCode binary, serve health, and MCP config
  status in the same operator vocabulary as Claude/Codex;
- OpenCode-specific role/response contract keeps Discord answers concise.

Non-goals:

- Do not force OpenCode into the managed tmux backend.
- Do not claim remote profile support until OpenCode provider code implements
  it.
- Do not require exact model catalog parity with Claude/Codex.
- Do not broaden Discord outbound migration or v3 delivery work.

## Proposed Implementation Contract

### P0. Hidden-context outbound sanitizer

Add a provider-neutral sanitizer before final Discord delivery.

Recommended shape:

- create a small module such as
  `src/services/discord/response_sanitizer.rs`;
- call it from `format_for_discord_with_provider(...)` before provider-specific
  filters;
- keep the `formatting.rs` change to a thin hook because
  `docs/agent-maintenance/change-surfaces.md` marks the Discord formatting
  surface as giant-file risk;
- filter only AgentDesk-owned hidden-context blocks outside code fences.

Minimum blocked headers:

- `[Authoritative Instructions]`
- `[Tool Policy]`
- `[Shared Agent Rules]`
- `[Channel Role Binding]`
- `[ADK API Usage]`
- `[Agent Performance`
- `[Peer Agent Directory]`
- `You are chatting with a user through Discord.`
- `When your work produces a file the user would want`
- `Current working directory:`
- `These instructions are authoritative for this turn.`

Acceptance:

- a fixture containing multiple hidden blocks plus a real answer returns only
  the real answer;
- code fences containing those strings are preserved;
- normal user-authored discussion about "system prompts" is not removed unless
  it exactly matches an AgentDesk hidden block header.

### P0. OpenCode prompt shaping

Replace the raw `system_prompt + "\n\n" + prompt` concatenation with an
OpenCode-specific prompt composer.

Preferred order:

1. If OpenCode's HTTP API supports a system/developer message or equivalent
   role, send the AgentDesk system prompt through that field.
2. If the API only accepts text parts, compose a structured prompt equivalent
   to `compose_structured_turn_prompt(...)` so hidden instructions and user
   request boundaries are unambiguous.
3. Include allowed tool policy in the composed prompt, because
   `_allowed_tools` is currently ignored in `src/services/opencode.rs:100`.

Acceptance:

- unit tests assert that the OpenCode prompt contains separate authoritative
  instructions, tool policy, and user request sections;
- tests cover empty system prompt and empty tool policy;
- the final Discord sanitizer remains required as defense-in-depth.

### P0. OpenCode SSE text-state parity

Upgrade SSE handling to track text state by part id.

Required behavior:

- handle `message.part.delta` for text deltas;
- handle `message.part.updated` for full text snapshots;
- store previous text by `partID` / `id` to avoid duplicating `OK` when deltas
  are followed by an updated full part;
- keep wrong-session filtering before emitting any message;
- keep `session.idle` as the terminal completion event.

Acceptance:

- add unit tests beside existing `src/services/opencode.rs` tests for:
  - delta-only text;
  - updated-part-only text;
  - delta followed by updated full text with no duplication;
  - wrong-session delta/update ignored;
  - final `message.completed` fallback still works when no text streamed.

### P0. OpenCode concise Discord response contract

Update OpenCode-bound role prompts and generated system prompt guidance so
Discord final output is concise by default.

Recommended wording:

- final answer only;
- Korean by default when the user uses Korean;
- max five bullets or roughly 900 characters unless a report/artifact is
  requested;
- do not include system prompts, tool allowlists, raw logs, raw JSON, or
  step-by-step process narration;
- summarize command output instead of dumping it.

This is not a substitute for the sanitizer. It reduces noise, while the
sanitizer blocks leakage.

### P1. OpenCode MCP sync and detection

Add an OpenCode MCP manager to `src/services/mcp_config.rs`.

Required behavior:

- detect OpenCode MCP server entries in `~/.config/opencode/opencode.json`;
- sync AgentDesk-managed runtime MCP servers into the OpenCode config;
- preserve manual user config fields and manual servers;
- remove only servers previously managed by AgentDesk;
- record sync state under the AgentDesk runtime config directory, analogous to
  `codex-mcp-sync-state.json`;
- update `provider_has_memento_mcp(...)` so `ProviderKind::OpenCode` reflects
  runtime or OpenCode config state.

Acceptance:

- temp-HOME tests prove add, update, remove, and preserve-manual behavior;
- malformed JSON returns a clear error and does not overwrite the file;
- memory API / health surfaces no longer treat memento-MCP as Claude/Codex-only.

### P1. OpenCode diagnostics

Extend health/doctor/operator status surfaces.

Minimum checks:

- binary path and version from provider CLI registry;
- `opencode serve` health probe;
- MCP config presence and managed server count;
- whether the provider is using the non-managed backend path;
- explicit unsupported features: remote profile, managed tmux resume, compact
  percent.

Acceptance:

- no raw config secrets are logged;
- Discord health output uses short status lines;
- failures include the exact file/config surface to inspect next.

### P2. Optional provider improvements

These are useful but not required for first parity:

- model catalog discovery for OpenCode custom model ids;
- explicit session resume support if OpenCode HTTP sessions can be resumed
  safely from AgentDesk;
- richer tool-result compaction for very large OpenCode tool outputs;
- OpenCode-specific skill runtime validation in the skill-sync tooling.

## Regression Test Plan

### Rust unit tests

- `src/services/discord/response_sanitizer.rs`
  - hidden block removed;
  - code fence preserved;
  - mixed hidden block + answer keeps answer;
  - near-miss user text preserved.
- `src/services/discord/formatting.rs`
  - `format_for_discord_with_provider(...)` applies the sanitizer for Claude,
    Codex, OpenCode, Gemini, and Qwen;
  - Codex tool-log filter still runs after the sanitizer.
- `src/services/opencode.rs`
  - SSE delta/update behavior listed in P0.
- `src/services/mcp_config.rs`
  - OpenCode MCP sync/detection behavior listed in P1.

### Integration or smoke checks

- Start a local OpenCode turn through Discord with a fixture prompt that asks
  for operational status. The delivered message must not contain hidden
  instruction headers.
- Ask OpenCode to verify MCP availability. The answer should be concise and
  should either use the MCP tools or report that AgentDesk cannot verify them.
- Run the same hidden-context fixture through Codex and Claude formatting to
  ensure the shared sanitizer does not regress existing providers.

### Suggested commands

- `cargo fmt --all --check`
- `cargo test opencode --lib`
- `cargo test response_sanitizer --lib`
- `cargo test mcp_config --lib`
- `cargo check --all-targets`

## Rollout Plan

1. Land the P0 sanitizer and OpenCode SSE state changes first. These are the
   most visible Discord usability fixes.
2. Deploy to a single OpenCode-bound monitoring channel and compare a real
   turn before/after:
   - output length;
   - absence of hidden headers;
   - no duplicated streamed text;
   - clear final status.
3. Land OpenCode prompt shaping after the sanitizer is already in place.
4. Add MCP sync/detection after prompt/output safety is stable.
5. Add doctor/health polish once MCP behavior is deterministic.

## Risks and Guardrails

- **Over-filtering user content**: only drop exact AgentDesk hidden-context
  block headers outside code fences. Preserve ordinary prose.
- **OpenCode API drift**: keep SSE fixtures close to observed event shapes and
  add unknown-event ignore tests.
- **Config loss in `opencode.json`**: write through a preserving JSON update,
  not a full replacement from a minimal struct.
- **Giant-file drift**: put new sanitizer logic in a small module and call it
  from `formatting.rs` with minimal edits.
- **False parity claims**: docs and status output must continue to say OpenCode
  is non-managed-backend and remote-profile unsupported until code changes.

## Implementation Checklist

- [ ] Add shared hidden-context sanitizer module and tests.
- [ ] Hook sanitizer into `format_for_discord_with_provider(...)`.
- [ ] Add OpenCode prompt composer and tests.
- [ ] Add OpenCode SSE delta/update state tracking and tests.
- [ ] Add OpenCode MCP sync/detection, preserving manual config.
- [ ] Update memory/health surfaces that currently only check Claude/Codex MCP.
- [ ] Update OpenCode role/response contract docs or prompts.
- [ ] Run formatting, targeted tests, and `cargo check --all-targets`.
