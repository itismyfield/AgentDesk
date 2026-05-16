# Codex Exec Policy After Direct TUI Hosting

Issue: #2175

This policy classifies every AgentDesk Codex runtime path after direct TUI
hosting. The decision point is whether the path may continue to invoke
`codex exec --json`, which is the legacy JSONL/headless contract.

## Runtime Kinds

| Runtime kind | Code path | `codex exec --json` policy | Reason |
| --- | --- | --- | --- |
| Direct TUI | `execute_streaming_local_tui_tmux` | Disallowed | This is the hosted Codex TUI rollout path. It launches `codex` directly with TUI arguments and tails Codex rollout transcripts instead of depending on JSONL stdout. |
| Legacy wrapper fallback | `execute_streaming_local_tmux` -> `codex-tmux-wrapper` | Allowed fallback | This keeps the pre-rollout tmux wrapper contract available when TUI hosting is disabled or unavailable. The wrapper still translates `codex exec --json` into AgentDesk stream events. |
| ProcessBackend | `execute_streaming_local_process_codex` -> `codex-tmux-wrapper` in pipe mode | Allowed fallback | This is the no-tmux/non-Unix fallback and uses the same wrapper JSONL translation without an interactive terminal. |
| Remote direct | `execute_streaming_remote_direct` | Not allowed now | Remote SSH execution is currently disabled and must not grow a silent `codex exec --json` implementation without a new policy decision. |
| Remote tmux | `execute_streaming_remote_tmux` | Not allowed now | Remote tmux execution is currently disabled. If it is implemented later, it should choose explicitly between direct TUI hosting and a documented fallback. |
| Direct headless | `execute_streaming_direct` | Allowed | Non-tmux structured execution that streams `StreamMessage` events from the JSONL contract. Covers both fresh one-shot runs and JSONL session resume (`codex exec resume <session> --json`) — see "Headless resume" below. |
| Simple headless | `execute_command_simple*` | Allowed (bounded) | Short-lived maintenance, probe, and batch callers parse the final assistant message from `codex exec --json`. Callers MUST use a timeout/cancellation entrypoint — see "Long-running and CI" below. |

## Decision

Direct TUI hosting is the preferred local interactive Codex path and must not
use `codex exec --json`. The remaining local wrapper and headless paths may
continue to use `codex exec --json` because they are either explicit fallback
paths or intentionally non-interactive.

This issue does not migrate launch-option parity (#2173) or typed handoff
(#2174). Those changes should keep the runtime-kind split intact: TUI hosting
uses Codex as a TUI, while fallback/headless paths use JSONL only when their
caller cannot host a TUI.

## Caller-facing interface

The `CodexRuntimeKind` enum is a *derived* observability label, not a
caller-facing API. Callers select Codex behavior by choosing one of two
discoverable entrypoints in `src/services/codex.rs`:

- `execute_command_streaming(...)` — the single entrypoint for streaming
  (interactive TUI, tmux fallback, ProcessBackend, direct headless). The
  runtime kind is derived from the session/transport shape
  (`tmux_session_name`, `session_id`, remote/local) and logged.
- `execute_command_simple_with_timeout(...)` — the single entrypoint for
  bounded, non-streaming, short-lived calls. The no-timeout variants exist
  only as internal helpers and MUST NOT be called from new code.

New Codex callers MUST pick one of these two entrypoints. Adding a third
top-level Codex entrypoint requires a follow-up ADR.

## Headless resume

`execute_streaming_direct` is permitted to issue `codex exec resume
<session> --json` when a `session_id` is supplied. Resume is an explicit
extension of the direct-headless contract, not a violation of it:

- Resume reuses the same JSONL stream contract as a fresh one-shot.
- Resume is still non-interactive and has no hosted TUI; that is what makes
  JSONL the right transport.
- Resume is logged with `runtime_kind=direct-headless` plus the existing
  `session_id` field on the streaming span so operators can distinguish
  fresh vs. resumed turns without a new runtime kind.

The previous "one-shot, non-session" wording was too narrow; the rule is
that direct headless is the non-TUI streaming JSONL path, whether the turn
is fresh or resumed.

## Long-running and CI

Headless callers (CI agents, batch probes, voice barge-in, meeting
orchestrator) can produce long-running Codex turns. The policy for these:

- Streaming callers (`execute_command_streaming` -> direct headless / wrapper
  fallback / ProcessBackend) already propagate the dispatch `CancelToken`
  to the child Codex process and tear it down on cancel.
- Non-streaming callers MUST use the timeout/cancel-aware
  `execute_command_simple_cancellable_with_options` entrypoint (the only
  helper that currently threads a `CancelToken` and kills the child PID on
  cancel). Calling `execute_command_simple` or
  `execute_command_simple_with_model` without a `CancelToken` from a
  long-running surface is a policy violation and a follow-up bug, not an
  allowed exception.

  Known follow-ups (tracked as part of this ADR rollout, not blockers for
  merging the ADR itself):

  - `execute_command_simple_with_timeout` currently spawns a worker thread
    and returns on `recv_timeout` without cancelling the worker's
    `execute_command_simple` call. Per this policy it must be reimplemented
    to thread a `CancelToken` and kill the child on timeout (mirror
    `provider_exec::execute_simple_with_timeout`).
  - Voice barge-in, voice channel text replies, and the background voice
    summary path call `execute_command_simple_with_model` and rely on
    `tokio::time::timeout` around `spawn_blocking`, which does not abort
    the spawned Codex child. These call sites must migrate to the
    cancellable entrypoint before `execute_command_simple_with_timeout` is
    fixed, otherwise they will silently keep using the unsafe helper.
- Headless CI / non-interactive batch use cases route through one of the
  two entrypoints above. There is no separate "headless CI" runtime kind:
  CI is a *caller*, not a runtime. CI-specific behavior (no TTY, fixed
  budgets, no resume) is enforced by the caller's choice of entrypoint and
  the timeout it supplies.

If a future scenario requires a runtime that is neither bounded simple nor
streaming (for example, a true long-running background JSON exec without a
session id), it requires its own runtime kind and ADR amendment before it
ships.

## Observability

Codex execution now emits an info log when a runtime path is selected:

- `provider`
- `runtime_kind`
- `uses_codex_exec_json`
- `entrypoint`

These fields are intentionally low-cardinality so operators can distinguish
direct TUI, wrapper fallback, ProcessBackend, remote, and headless execution
without changing session behavior.
