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

### Verified OpenCode API constraints

Reviewed against local `opencode --version` `1.14.28` and the matching
OpenCode source snapshot on 2026-04-28.

- `/session/:sessionID/message` and `/session/:sessionID/prompt_async` validate
  request bodies as `SessionPrompt.PromptInput` without `sessionID`
  (`packages/opencode/src/server/routes/instance/session.ts:871`,
  `:909`; `packages/opencode/src/server/routes/instance/httpapi/session.ts:75`,
  `:311-318`).
- `PromptInput` supports top-level `system?: string`, `tools?: Record<string,
  boolean>`, `agent`, `model`, `format`, `variant`, and `parts`
  (`packages/opencode/src/session/prompt.ts:1677-1704`).
- OpenCode message roles are still only `user` and `assistant`
  (`packages/opencode/src/session/message.ts:140`).
- The `system` value is stored on the user message metadata and later merged
  into the LLM system prompt with the provider/agent prompt
  (`packages/opencode/src/session/prompt.ts:887-918`,
  `packages/opencode/src/session/llm.ts:99-106`).

Therefore AgentDesk should use OpenCode's top-level `system` field when the
target runtime accepts it, but must not invent unsupported
`role: "system"` / `role: "developer"` message objects or text parts.

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

That last point is an actual AgentDesk API gap, not just missing UI copy:
`detect_memory_backend()` only checks Claude/Codex before selecting the
Memento backend, and `provider_has_mcp_server(...)` returns `false` for
providers outside the Claude/Codex match arms
(`src/services/mcp_config.rs:37-46`). Until OpenCode detection is implemented,
an OpenCode-side `opencode.json` MCP entry can be present but invisible to the
memory API and related health/status surfaces.

Skill command prompting does recognize OpenCode:

- `src/services/discord/commands/skill.rs:75-85` renders an OpenCode skill
  instruction.

However, the skill path depends on the active OpenCode runtime having the
expected skill files/config available. There is no AgentDesk-level OpenCode
skill-sync or MCP-sync contract in this repo yet.

### Additional OpenCode parity gaps

The 2026-04-28 code audit found several gaps beyond prompt/output cleanup:

- OpenCode advertises `supports_resume: true` and `resume_without_reset: true`
  (`src/services/provider.rs:139-147`), but
  `src/services/opencode.rs:94-107` ignores `_session_id`, always creates a new
  OpenCode session (`src/services/opencode.rs:197-199`), then disposes and
  kills the OpenCode server after the turn (`src/services/opencode.rs:138-139`).
  Discord callers pass the session id through
  `src/services/discord/router/message_handler.rs:1461-1474` and
  `:3389-3402`, so the public capability currently overpromises.
- OpenCode simple execution does not honor the caller's cancellation token:
  `execute_command_simple_cancellable(...)` accepts `_cancel_token` but creates
  a new local token instead (`src/services/opencode.rs:40-55`). The timeout
  path in `src/services/provider_exec.rs:68-87` cancels the caller token and
  kills the PID registered on that token, so simple OpenCode timeouts can leave
  the spawned `opencode serve` path running until it exits on its own.
- OpenCode model overrides are not wired to the actual OpenCode API shape.
  AgentDesk sends `{"modelID": ...}` when creating a session
  (`src/services/opencode.rs:251-256`), but OpenCode `Session.CreateInput`
  only accepts session metadata such as `parentID`, `title`, `permission`, and
  `workspaceID` (`packages/opencode/src/session/session.ts:179-185`). The model
  belongs in `PromptInput.model` as `{ providerID, modelID }`
  (`packages/opencode/src/session/prompt.ts:1677-1688`).
- Doctor/provider health has an OpenCode check id and display label
  (`src/cli/doctor/orchestrator.rs:870-877`, `:2252-2258`), but
  `build_provider_checks(...)` only schedules Claude, Codex, Gemini, and Qwen
  (`src/cli/doctor/orchestrator.rs:903-920`).
- Runtime skill sync still targets Claude/Codex/Gemini/Qwen but not OpenCode:
  `src/runtime_layout/skill_sync.rs:569-576` falls unknown providers back to
  Codex, `src/server/routes/skills_api.rs:100-117` scans only release skills,
  Codex, and Claude roots, and `skills/manifest.json` exposes the built-in
  memory skills only to Claude/Codex. This conflicts with the Discord runtime
  scanner, which already knows OpenCode's `.opencode/skills` roots
  (`src/services/discord/mod.rs:2657-2678`).
- `opencode serve` stdout/stderr are both discarded
  (`src/services/opencode.rs:151-166`), so failed startup or health timeout
  diagnostics lose provider-side error text that would help operators.
- Provider CLI support is split: the CLI registry/update path includes
  OpenCode (`src/cli/provider_cli/mod.rs:22`,
  `src/services/provider_cli/registry.rs:221-224`), but the server API still
  hides and rejects OpenCode because `src/server/routes/provider_cli_api.rs:25`
  hard-codes `ALL_PROVIDERS = ["codex", "claude", "gemini", "qwen"]` and
  `is_supported_provider(...)` delegates to that list
  (`src/server/routes/provider_cli_api.rs:359-360`).
- Server API docs are stale for OpenCode-capable flows: `/api/agents/setup`
  validates through `ProviderKind::from_str(...)`
  (`src/server/routes/agents_setup.rs:205`), but
  `src/server/routes/docs.rs:838-884` still documents only
  Claude/Codex/Gemini/Qwen. Provider CLI and analytics docs repeat the same
  omission (`src/server/routes/docs.rs:3076-3102`, `:3461-3486`).
- The JS embedded runtime cannot see OpenCode inflight turns:
  `src/engine/ops/exec_ops.rs:160-170` scans only
  `runtime/discord_inflight/{claude,codex,gemini,qwen}`.
- Dashboard provider selectors are inconsistent. Agent manager types include
  OpenCode (`dashboard/src/types/index.ts:37-45`), but onboarding and meeting
  surfaces restrict command/reviewer providers to Claude/Codex/Gemini/Qwen
  (`dashboard/src/components/OnboardingWizard.tsx:22`,
  `dashboard/src/components/onboardingDraft.ts:15-17`,
  `dashboard/src/components/MeetingMinutesView.tsx:56`).
- Rate-limit analytics has an explicit unsupported-provider mechanism for
  Qwen but not OpenCode (`src/server/routes/analytics.rs:21-24`), and the
  provider sort order also omits OpenCode (`src/server/routes/analytics.rs:1354-1358`).
- DB channel bindings have dedicated Claude/Codex columns but no dedicated
  OpenCode channel field. Config supports `AgentChannels.opencode`
  (`src/config.rs:245-255`), while DB sync folds OpenCode into
  `discord_channel_id` as a provider-primary channel
  (`src/db/agents.rs:325-341`, `src/db/postgres.rs:437-451`). This is workable
  for one primary non-Claude/Codex provider, but it is not full per-provider
  channel parity.
- Meeting and CLI help copy still excludes OpenCode even where parsing goes
  through `ProviderKind::from_str(...)`
  (`src/services/discord/commands/meeting_cmd.rs:10-43`,
  `src/services/discord/meeting_orchestrator.rs:1375-1432`,
  `src/cli/provider_cli/mod.rs:34-42`,
  `src/cli/dcserver.rs:468-500`).

## Target Experience

OpenCode should be safe and useful enough that operators can use it in Discord
without switching providers for routine inspection tasks.

P0 target:

- hidden system/developer/channel instructions never appear in Discord output;
- OpenCode capability metadata matches implemented behavior, especially resume,
  cancellation, and model override behavior;
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
- API, dashboard, CLI help, inflight, analytics, and skill inventory surfaces
  consistently expose OpenCode when the underlying runtime supports it.

Non-goals:

- Do not force OpenCode into the managed tmux backend.
- Do not claim remote profile support until OpenCode provider code implements
  it.
- Do not require exact model catalog parity with Claude/Codex.
- Do not broaden Discord outbound migration or v3 delivery work.

## Implementation Decisions

These decisions are locked for PR #95 so implementation does not need another
round of provider-shape interpretation.

### OpenCode model override format

Use a Qwen-like "configured default plus custom model id" experience, but keep
OpenCode's required provider/model split explicit.

Required behavior:

- `default` means AgentDesk omits the model override from the OpenCode prompt
  payload.
- non-default overrides MUST use canonical `providerID/modelID` syntax, for
  example `anthropic/claude-sonnet-4-5` or `openai/gpt-5.1`.
- AgentDesk converts that value to OpenCode `PromptInput.model` as
  `{"providerID":"...","modelID":"..."}` on `/prompt_async`.
- bare model ids are rejected with a concise validation error instead of
  guessing a provider.

Rationale: Qwen can resolve model settings from `.qwen/settings.json`, while
OpenCode prompt execution needs the provider id and model id as separate API
fields.

### OpenCode allowed tools contract

Do not map AgentDesk `_allowed_tools` to OpenCode's top-level `tools` field in
P0 unless exact OpenCode permission keys are verified.

Required behavior:

- keep OpenCode outbound safety enforced by AgentDesk-side sanitization and
  command/tool execution guards;
- include a concise advisory tool policy in the OpenCode `system` prompt when
  exact tool permission mapping is unavailable;
- add `OPENCODE_SUPPORTED_ALLOWED_TOOLS` only after tests prove the mapping
  against the current OpenCode permission key names;
- do not reuse Qwen's `QWEN_SUPPORTED_ALLOWED_TOOLS` table for OpenCode.

Rationale: Qwen has an explicit AgentDesk tool compatibility table in
`src/services/qwen.rs`, but OpenCode's `tools`/permission semantics are not the
same contract.

### OpenCode channel binding model

Keep the current Qwen/Gemini-style provider-primary DB channel model for PR
#95. Do not add a dedicated OpenCode DB channel column in this implementation
wave.

Required behavior:

- YAML/config may continue to expose `AgentChannels.opencode`.
- DB sync keeps OpenCode as the provider-primary `discord_channel_id` when
  `agent.provider == "opencode"`.
- Dashboard/API UI must not imply that one DB agent can simultaneously hold an
  independent Claude channel, Codex channel, and OpenCode channel through
  dedicated columns.
- a dedicated `discord_channel_oc`-style migration is deferred until there is a
  concrete requirement for simultaneous per-provider DB bindings.

Rationale: existing DB sync already treats Gemini/OpenCode/Qwen as
provider-primary providers, while only Claude/Codex have dedicated channel
columns.

## Proposed Implementation Contract

### P0. OpenCode runtime contract correctness

Make OpenCode's advertised provider capabilities match the implemented runtime
contract.

Required behavior:

1. Either implement real OpenCode session reuse/resume or set
   `supports_resume` / `resume_without_reset` to `false` until it exists.
2. Make Discord restart/resume copy avoid promising `--resume` semantics for
   OpenCode unless the provider actually reuses an OpenCode session.
3. Pass the caller's `CancelToken` through OpenCode simple execution and
   register the spawned `opencode serve` PID on that same token.
4. Move model override handling out of session creation and into
   `/prompt_async` as OpenCode `PromptInput.model`, using the locked
   `providerID/modelID` AgentDesk syntax described above.
5. Preserve current remote-profile rejection until an explicit remote OpenCode
   execution path exists.

Acceptance:

- tests prove OpenCode simple timeout/cancel registers the child PID on the
  caller token;
- tests prove non-default OpenCode model selection accepts
  `providerID/modelID`, rejects bare model ids, and serializes as
  `{"model":{"providerID": "...", "modelID": "..."}}` in the prompt payload;
- restart/control copy no longer promises resumability when OpenCode cannot
  resume;
- provider capability tests distinguish implemented OpenCode behavior from the
  Claude/Codex/Qwen managed tmux contract.

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

Required behavior:

1. Build the `/prompt_async` JSON body with AgentDesk hidden instructions in
   OpenCode's top-level `system` field, not prepended to the first text part.
2. Keep `parts` focused on the visible user request and supported file/agent
   parts.
3. For PR #95, do not send `_allowed_tools` as OpenCode's top-level `tools`
   permission map unless exact OpenCode permission keys have been verified by
   tests. Until then, include a concise advisory tool policy in `system`,
   surface the limitation in diagnostics, and keep AgentDesk-side sanitization
   and execution guards as the enforcing layer.
4. Keep a compatibility fallback for older or drifted OpenCode runtimes: if the
   runtime rejects top-level `system`, use a structured text wrapper equivalent
   to `compose_structured_turn_prompt(...)` and log the API drift.
5. Never send unsupported `role: "system"` or `role: "developer"` message
   objects; OpenCode persisted message roles are only `user` and `assistant`.

Acceptance:

- unit tests assert that the OpenCode request body keeps `system` separate from
  `parts`, and never includes unsupported message role fields;
- tests assert that the user request remains in `parts` and hidden
  instructions do not get prepended to the user text;
- tests cover empty system prompt and empty tool policy;
- tests cover the documented advisory fallback for `_allowed_tools`; if exact
  permission mapping is added later, tests must also cover the successful
  OpenCode permission-map serialization path;
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
  runtime or OpenCode config state;
- update `detect_memory_backend()` so the memory API can select Memento when
  OpenCode is the configured provider with memento MCP available.

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
- include OpenCode in `build_provider_checks(...)`;
- capture bounded `opencode serve` stderr/stdout during startup and health
  failure paths;
- whether the provider is using the non-managed backend path;
- explicit unsupported features: remote profile, managed tmux resume, compact
  percent.

Acceptance:

- no raw config secrets are logged;
- Discord health output uses short status lines;
- failures include the exact file/config surface to inspect next.

### P1. OpenCode provider-surface completeness

Remove the remaining hard-coded Claude/Codex/Gemini/Qwen provider lists from
server, dashboard, CLI, and embedded runtime surfaces.

Required behavior:

1. Add a small registry-backed helper for "AgentDesk-supported CLI providers"
   instead of duplicating `["claude", "codex", "gemini", "qwen"]` lists.
   The helper should include OpenCode and derive from
   `src/services/provider.rs` where possible.
2. Update `/api/provider-cli` to include OpenCode in status, migration state,
   and action validation (`src/server/routes/provider_cli_api.rs`).
3. Update generated API docs so `/api/agents/setup`, `/api/provider-cli`,
   `/api/analytics`, and `/api/analytics/invariants` all document OpenCode
   when they accept provider filters or provider ids
   (`src/server/routes/docs.rs`).
4. Update `agentdesk.inflight.list()` to scan the provider registry or at
   least include `opencode` in `runtime/discord_inflight` discovery
   (`src/engine/ops/exec_ops.rs`).
5. Update dashboard onboarding and meeting provider selectors to include
   OpenCode, keeping type definitions and option arrays in sync
   (`dashboard/src/components/OnboardingWizard.tsx`,
   `dashboard/src/components/onboardingDraft.ts`,
   `dashboard/src/components/MeetingMinutesView.tsx`).
6. Add OpenCode to the rate-limit unsupported-provider path unless a real
   OpenCode telemetry source is implemented first
   (`src/server/routes/analytics.rs`).
7. Keep the current Qwen/Gemini-style provider-primary channel-binding model:
   OpenCode uses `discord_channel_id` when `agent.provider == "opencode"`.
   Do not add a dedicated OpenCode DB channel column in PR #95, and make UI/API
   copy explicit that independent OpenCode DB channel parity is deferred.
8. Update CLI and Discord help/error text so OpenCode is listed anywhere
   `ProviderKind::from_str(...)` would accept it.

Acceptance:

- `GET /api/provider-cli` returns an OpenCode row when no registry entry is
  present and reports OpenCode migration state when it exists;
- `PATCH /api/provider-cli/opencode` validates exactly like the other
  supported providers;
- dashboard TypeScript builds with OpenCode in onboarding and meeting provider
  selectors;
- `agentdesk.inflight.list()` includes `runtime/discord_inflight/opencode`;
- API docs no longer contradict actual `/api/agents/setup` OpenCode support;
- tests or snapshots cover all updated provider lists so future providers do
  not require another manual grep pass.

### P2. Optional provider improvements

These are useful but not required for first parity:

- model catalog discovery for OpenCode custom model ids;
- explicit session resume support if OpenCode HTTP sessions can be resumed
  safely from AgentDesk;
- richer tool-result compaction for very large OpenCode tool outputs;
- OpenCode-specific skill runtime validation in the skill-sync tooling;
- OpenCode-aware server-side skill API roots so dashboard/API skill inventory
  sees the same `.opencode/skills` content that Discord runtime scanning sees.
- registry-driven provider list tests that fail when a server/dashboard/CLI
  surface omits a provider that `ProviderKind` supports.
- dedicated OpenCode DB channel column/API fields if AgentDesk later needs
  simultaneous Claude/Codex/OpenCode channel bindings for one DB agent.

## Regression Test Plan

## PR #95 Implementation Status

Implemented in this PR:

- OpenCode runtime capability metadata now reflects current behavior:
  `supports_resume` and `resume_without_reset` are false until real OpenCode
  session reuse exists.
- OpenCode simple execution passes the caller cancellation token through the
  runtime path and registers the spawned `opencode serve` process on that
  token.
- OpenCode model override uses AgentDesk's locked `providerID/modelID` syntax
  and serializes the split pair as OpenCode `PromptInput.model` on
  `/prompt_async`; bare model ids are rejected.
- OpenCode prompt shaping keeps hidden/system guidance in the top-level
  `system` field and leaves visible user text in `parts`.
- AgentDesk `_allowed_tools` is not mapped to OpenCode `tools` in P0; OpenCode
  receives a concise advisory in `system` until exact permission-key mapping is
  verified.
- OpenCode SSE handling tracks text state by part id, supports delta/update
  events, filters wrong-session events, and avoids duplicate text when a full
  update follows streamed deltas.
- Final Discord formatting now runs a provider-neutral hidden-context sanitizer
  before provider-specific output filtering.

Not implemented in this PR:

- OpenCode MCP sync/detection and `provider_has_memento_mcp(...)` integration.
- Provider CLI/API/dashboard provider-surface completeness.
- Exact OpenCode permission-key serialization for AgentDesk allowed tools.
- Runtime fallback when an older or drifted OpenCode build rejects top-level
  `system`; this remains a follow-up unless observed API drift requires it.

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
- `src/server/routes/provider_cli_api.rs`
  - OpenCode included in provider status and patch validation.
- `src/server/routes/docs.rs`
  - OpenCode appears in provider enum/help text for supported API endpoints.
- `src/engine/ops/exec_ops.rs`
  - OpenCode inflight files appear in `agentdesk.inflight.list()`.
- `src/server/routes/analytics.rs`
  - OpenCode appears as unsupported for rate-limit telemetry when recently
    used, or has real telemetry if implemented.

### Integration or smoke checks

- Start a local OpenCode turn through Discord with a fixture prompt that asks
  for operational status. The delivered message must not contain hidden
  instruction headers.
- Ask OpenCode to verify MCP availability. The answer should be concise and
  should either use the MCP tools or report that AgentDesk cannot verify them.
- Run the same hidden-context fixture through Codex and Claude formatting to
  ensure the shared sanitizer does not regress existing providers.
- Open `/api/provider-cli` and the dashboard provider selectors and verify
  OpenCode appears anywhere provider operations are offered.

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
4. Add provider-surface completeness fixes for API docs, provider-cli API,
   dashboard selectors, inflight listing, and analytics. These changes are
   mostly low-risk and prevent operators from thinking OpenCode is unsupported
   when the runtime can handle it.
5. Add MCP sync/detection after prompt/output safety is stable.
6. Add doctor/health polish once MCP behavior is deterministic.

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

- [x] Add shared hidden-context sanitizer module and tests.
- [x] Hook sanitizer into `format_for_discord_with_provider(...)`.
- [x] Add OpenCode prompt composer and tests.
- [x] Add OpenCode SSE delta/update state tracking and tests.
- [x] Correct OpenCode provider capability metadata for non-resume behavior.
- [x] Add OpenCode `providerID/modelID` override validation and prompt
      serialization.
- [ ] Add OpenCode MCP sync/detection, preserving manual config.
- [ ] Update memory/health surfaces that currently only check Claude/Codex MCP.
- [ ] Include OpenCode in provider-cli server API status/action validation.
- [ ] Update API docs for OpenCode-capable provider params.
- [ ] Include OpenCode in `agentdesk.inflight.list()`.
- [ ] Include OpenCode in dashboard onboarding and meeting provider selectors.
- [ ] Include OpenCode in rate-limit unsupported-provider analytics or add real
      telemetry.
- [ ] Preserve and document the provider-primary DB channel-binding model for
      OpenCode.
- [ ] Update CLI/Discord help text that lists supported providers.
- [ ] Update OpenCode role/response contract docs or prompts.
- [x] Run formatting, targeted tests, and `cargo check --all-targets`.
