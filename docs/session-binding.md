# Session binding: deterministic tmux ↔ Discord channel naming

Status: foundational contract. Source: epic #2285, E1 (issue #2343).

This document is the **public, operator-facing contract** for how AgentDesk
maps tmux sessions onto Discord channels. The downstream supervisor work
(E2 discovery, E3 registry/supervisor, E4 relay refactor) all builds on the
naming convention defined here. Anything that obeys this contract can be
adopted by AgentDesk automatically — including sessions an operator creates by
hand from a terminal.

## TL;DR

```text
AgentDesk-{provider_id}-{sanitized_channel}
```

Examples:

| Channel       | Provider | Expected session name              |
| ------------- | -------- | ---------------------------------- |
| `agent-cc`    | claude   | `AgentDesk-claude-agent-cc`        |
| `dev-cdx`     | codex    | `AgentDesk-codex-dev-cdx`          |
| `research-gm` | gemini   | `AgentDesk-gemini-research-gm`     |
| `sandbox-oc`  | opencode | `AgentDesk-opencode-sandbox-oc`    |
| `sandbox-qw`  | qwen     | `AgentDesk-qwen-sandbox-qw`        |

The CLI is the source of truth — never hard-code these names in scripts.

```sh
# Print the canonical session name for a channel.
agentdesk show session-name --channel dev-cdx
# → AgentDesk-codex-dev-cdx

# Force a specific provider (when channel-suffix heuristic is ambiguous).
agentdesk show session-name --channel my-channel --provider claude
# → AgentDesk-claude-my-channel
```

## Convention details

The session name has three segments joined by `-`:

1. **Fixed prefix** `AgentDesk` (constant `TMUX_SESSION_PREFIX` in
   `src/services/provider.rs`).
2. **Provider id** — the lowercase id from the provider registry:
   `claude` | `codex` | `gemini` | `opencode` | `qwen`.
3. **Sanitized channel** — the Discord channel name / stable channel
   identifier with:
   - every character that is not `[A-Za-z0-9_-]` replaced by `-`,
   - the result prefix-truncated to **44 bytes** (UTF-8 safe),
   - a trailing `-t{thread_id}` suffix preserved across truncation so
     unified-thread guards (`is_unified_thread_channel_name_active`) can still
     extract the thread id.

There is currently **no nonce / no per-run salt**. Two distinct channels that
sanitize+truncate to the same string would collide; the channel directory
guarantees uniqueness at source, and the sanitizer is deterministic, so the
mapping `(provider, channel) → session_name` is one-to-one in practice.

The reverse function `parse_provider_and_channel_from_tmux_name` recovers
`(provider, channel_id)` losslessly for any channel id that survives
sanitize+truncate unchanged. That is the round-trip property unit tests assert
in `src/services/cluster/session_matcher.rs`.

## Provider fingerprint

Name matching is necessary but not sufficient. A session is fully matched only
when the tmux pane is also **running the expected provider**.

The pure helper `detect_provider_from_pane_command(pane_cmd: &str)` maps a
tmux pane current-command string (as reported by
`tmux display-message -p '#{pane_current_command}'`) to a `ProviderKind`.
Matching rules, in order:

1. exact match against the provider's registry `binary_name` (`codex` → Codex),
2. prefix match anchored at `-`, `_`, or `.` (`codex-cli`, `codex_v2`,
   `codex.sh` all → Codex),
3. absolute-path basename matching (`/usr/local/bin/codex` → Codex).

Bare substrings that span a word boundary (`claudio`, `codexterm`) are
**not** matched. This keeps the helper robust against Codex / Claude CLI
version drift: vendoring `codex-1.2.0` or replacing the shim with
`codex_release` continues to match without code changes, because the registry
remains the single source of truth.

## Operator playbook: pre-create a matching session

```sh
# 1. Ask AgentDesk for the canonical name.
NAME="$(agentdesk show session-name --channel <channel-id> --provider codex)"

# 2. Create the tmux session yourself.
tmux new -s "$NAME"

# 3. Inside the session, start the provider you declared.
codex
```

Once the upcoming `SessionDiscovery` loop (E2) ships, AgentDesk will enumerate
tmux sessions, run them through `SessionMatcher::match_session`, and attach a
watcher automatically — no extra "adopt this session" command. The same path
covers AgentDesk-launched sessions, monitor-pattern-triggered turns, and
operator-typed prompts.

## What this contract does **not** promise

- It does not prevent name collisions for adversarial / malformed channel ids.
  Same-host implicit trust holds; cross-host spoofing is out of scope (see the
  epic's "Migration / risk" section).
- It does not include the rollout / jsonl file format. Today both Claude and
  Codex wrappers write their structured stream to
  `runtime_root/runtime/sessions/agentdesk-<runtime>-<host>-<session>.jsonl`;
  `expected_rollout_path_for(session_name)` is the canonical helper. The path
  is session-scoped, not provider-scoped.
- It does not commit to a stable session name across major refactors. If/when
  a nonce is introduced (e.g. to support multiple AgentDesk instances on the
  same host), the CLI subcommand will continue to be the operator-facing
  source of truth — pin to `agentdesk show session-name`, not to the format.

## Cross-references

- Pure matcher + reverse function: `src/services/cluster/session_matcher.rs`.
- Naming primitive: `ProviderKind::build_tmux_session_name`
  (`src/services/provider.rs`).
- Reverse parser: `parse_provider_and_channel_from_tmux_name`
  (`src/services/provider.rs`).
- CLI: `agentdesk show session-name --channel <id> [--provider <kind>]`.
- Epic: #2285. This document: E1 (#2343). Follow-ups: E2 discovery, E3
  registry + watcher supervisor, E4 relay refactor.
