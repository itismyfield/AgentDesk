# Memento Memory Scope Contract

This document pins down which kinds of facts belong in `scope: permanent` vs `scope: workspace` (a.k.a. session) in the Memento MCP store, and what must never be written to Memento at all.

It is the working contract for the rule referenced from `_shared.prompt.md` and from [`docs/source-of-truth.md`](source-of-truth.md) (the Memento workspace memory row, previously placeholder under issue 910-6).

## TL;DR

- `permanent` is for durable, identity-level knowledge about the user, their environment, and long-lived decisions.
- `workspace` (session) is for project-, task-, or turn-scoped state that should not pollute the next unrelated session.
- If a fact has a canonical home in a tracked file (prompt, runtime YAML, policy, doc), Memento MUST NOT mirror it. Re-read the file instead.
- When unsure, write `workspace`. Promote to `permanent` only after the fact has survived multiple sessions and is not file-canonical.

## Scope: permanent

Use `scope: permanent` only for fragments that meet ALL of:

1. The fact is about the user, their long-term preferences, or their environment (not about a specific repo branch or campaign).
2. The fact is expected to remain true across sessions for weeks or longer.
3. There is no canonical file in any of the source-of-truth surfaces that already encodes it.

### Allowed permanent categories

- **User identity / preferences**: name, contact channels, working hours, communication style preferences, language preferences.
- **Long-term decisions**: architectural choices the user has explicitly committed to ("we standardize on Postgres", "all retros use the timeline shape"), only after the decision is also reflected in code or docs and the Memento entry is the human-friendly summary.
- **Environment facts unlikely to change**: machine names, primary repo paths, hardware quirks, "this user uses RTK".
- **Personal context**: family member names/roles for the family-counsel agent, ongoing personal projects the user has named, hobbies they reference repeatedly.
- **Procedural knowledge from resolved errors**: a `procedure` fragment summarizing how a recurring class of issue was fixed (only after `resolutionStatus = resolved`).

### Forbidden permanent categories

Even if the content feels durable, do NOT store the following as `permanent`:

- Anything that has a canonical file: prompts, `_shared.prompt.md` rules, policy YAML, `agentdesk.yaml` settings, ADR text, skill SKILL.md content, or any path listed in `docs/source-of-truth.md`. Memento is not a cache of repo files.
- Code rules / coding style rules — those live in `agents/_shared.prompt.md` and per-agent prompts.
- Configuration values — those live in `agentdesk.yaml`, `kv_meta`, or the dashboard surfaces (see `docs/config-domains.md`).
- Tool invocation lists, MCP server lists, model names — derived from runtime config.
- Per-issue progress notes, "the campaign for #1100 is at step 4" — that is workspace scope.
- Speculative or inferred facts (`assertionStatus = inferred`) — keep them workspace until verified.

## Scope: workspace (session)

Use `scope: workspace` (the default for ambiguous cases) for:

- Active task state: which issue is in progress, which subtask is next, what file is being edited.
- Turn-local hypotheses, partial findings, "I tried X, it failed because Y" — until the cause is confirmed.
- Project-specific context that only matters for one repo or one campaign worktree.
- Recall hints for the current session ("the user is debugging the codex tmux wrapper today").
- Cross-turn handoff notes between the planning, debugging, and verification phases of one task.

Workspace fragments naturally rotate or expire. They are not promoted to permanent automatically; promotion requires explicit `amend` after the fact has been re-validated.

## Forbidden in Memento entirely

Some content must not be stored in Memento at any scope, because it has authoritative file-level homes:

| Content kind | Authoritative location | Reference |
| --- | --- | --- |
| Shared agent rules (Code Principles, Memory, Verification, etc.) | `~/ObsidianVault/RemoteVault/adk-config/agents/_shared.prompt.md` | source-of-truth row "Shared prompt" |
| Per-agent identity / persona | `~/ObsidianVault/RemoteVault/adk-config/agents/<role>.prompt.md` | source-of-truth row "Per-agent prompt files" |
| Runtime / Discord / database config | `~/.adk/release/config/agentdesk.yaml` | source-of-truth rows "Runtime baseline config", "Discord bot bindings", "Operational database routing" |
| Live runtime overrides | `kv_meta['runtime-config']` | `docs/adr-settings-precedence.md` and `docs/config-domains.md` |
| Policy hooks / pipeline | `policies/*.js`, `policies/default-pipeline.yaml` | source-of-truth rows "Policy hooks" / "Default pipeline" |
| Skill body content | `~/ObsidianVault/RemoteVault/99_Skills/<skill>/SKILL.md` | source-of-truth row "Per-skill packages" |
| Memory tier content (SAM, SAK, LTM, workspace MEMORY.md) | `~/.adk/release/config/memories/*` and `workspace-memory-md/<agent>.md` | source-of-truth rows "Shared agent knowledge / SAM / LTM / Workspace MEMORY" |
| MCP server declarations | `agentdesk.yaml mcp_servers:` | source-of-truth row "MCP server declarations" |
| LaunchAgent shape | generator (`src/cli/init.rs`) + `launchd.env` | source-of-truth row "macOS release LaunchAgent" |

If a Memento search returns one of the above as a fragment, treat the file as authoritative and migrate / delete the duplicate. The file-canonical content overrides the Memento copy.

## Decision Procedure

When deciding where (or whether) to call `remember`, walk this checklist:

1. Is this content duplicated from a file in the table above? → DO NOT remember.
2. Is this a turn-local hypothesis or in-progress observation? → `workspace`.
3. Is this a project-specific or campaign-specific fact? → `workspace`.
4. Is this a user-level preference, identity fact, or resolved long-term procedure with no canonical file? → `permanent`.
5. Unsure? → `workspace`.

Promotion (`workspace → permanent`) is explicit, never automatic. Use `amend` after the fact has been validated across at least one additional session.

## Permanent-Scope Audit (issue #1100)

A fragment-level audit of existing `scope: permanent` entries was NOT performed inline in this commit (no Memento fetch was issued). The audit categories below are the candidate buckets that future audit passes should sweep, ordered by likely yield. They are derived from the forbidden table above and from observed past patterns:

1. **Code style / "bouncing fixes" rules** — anything that paraphrases the `_shared.prompt.md` Code Principles, DRY rule, Discord Response Style, or Tool Output Efficiency sections. Migrate by deleting the Memento fragment and pointing back at the prompt file.
2. **Runtime config restatements** — fragments that record port numbers, DB paths, model strings, or MCP server lists. Delete; re-derive from `agentdesk.yaml` when needed.
3. **Skill recipes** — fragments duplicating SKILL.md instructions for `memory-merge`, `skill-sync`, `agentdesk-restart`, etc. Delete; rely on the skill body.
4. **Per-issue campaign notes** — fragments tagged with stale issue IDs that were promoted to `permanent` instead of remaining `workspace`. Demote to `workspace` (or `forget` if the issue is closed).
5. **Inferred / unverified claims** — fragments with `assertionStatus = inferred` that were never amended to `verified`. Demote to `workspace` or `forget`.
6. **Cache copies of source-of-truth** — fragments that summarize `docs/source-of-truth.md` rows. If the summary is genuinely user-facing, mark the fragment with a `cache` note linking back to the doc; otherwise delete.

The audit pass itself is owned by the agent that next runs `mcp__memento__memory_consolidate` with `scope=permanent` selected; no automated migration is required by this issue.

## Related Documents

- [`docs/source-of-truth.md`](source-of-truth.md) — canonical file map for all forbidden Memento content.
- [`docs/config-domains.md`](config-domains.md) — runtime-config / dashboard / bot-settings owner split.
- [`docs/adr-settings-precedence.md`](adr-settings-precedence.md) — settings store precedence and persistence rules.
- `agents/_shared.prompt.md` — operator-side Memory section that references this document.
