# Phase 2 — Three-mode parallel e2e stabilization

Gate: Phase 1 counter-review clean + Phase 1 e2e probe(s) PASS.

Goal: run the same scenario battery against `pipe`, `tui`, and
`claude-e` runtimes concurrently and patch any mode-specific regression
until every scenario passes 5 consecutive runs on every mode.

## Channel layout (decision)

**Recommended: reuse the existing e2e channel pair + add one isolated
claude-e canary channel.** Rationale:

- `adk-dash-cc-e2e` (Claude provider, id `1506295332949196840`) already
  exists with the e2e role prompt, workspace, and watcher set up. Reuse
  for `tui` mode (the production-like baseline).
- Add **one new Discord channel** for `pipe` mode validation via
  announcebot admin (or reuse during a config flip on the existing
  channel). Cheap to set up because the agent definition is the same.
- Add **one more new Discord channel** for `claude-e` canary. Keeps the
  3-mode comparison fully isolated so a regression in one mode does
  not contaminate another.

Alternative (lighter weight): keep one channel and toggle
`runtime` between `pipe`, `tui`, `claude-e` across sessions. Lose
parallelism but no Discord-side setup. Acceptable for the first
scenario pass; promote to 3-channel layout for the soak phase.

## Channel provisioning steps (when 3-channel layout chosen)

1. Pick stable channel names: `adk-claude-e2e-pipe`,
   `adk-claude-e2e-tui`, `adk-claude-e2e-claude-e`.
2. Have announcebot create the channels under the AgentDesk guild's
   QA/e2e category (operator action; AgentDesk does not own that
   role by default).
3. Add 3 agent entries in operator yaml or extend the existing
   `adk-dashboard-e2e` agent with three claude channel bindings —
   one per channel — each with the appropriate `runtime` override:

   ```yaml
   - id: adk-dashboard-e2e-pipe
     ...
     channels:
       claude:
         id: '<pipe channel id>'
         runtime: pipe
   - id: adk-dashboard-e2e-tui
     ...
         runtime: tui
   - id: adk-dashboard-e2e-claude-e
     ...
         runtime: claude-e
   ```

4. `agentdesk restart-dcserver` and verify the runtime mirror logs
   show `channel_summary=...:pipe,...:tui,...:claude-e`.

## Scenario battery

Each scenario is sent to all three channels concurrently. Compare
behaviour and capture in `phase-2-e2e-results.md` (new doc).

| # | Scenario | Expected |
|---|---|---|
| S1 | Short literal echo ("Reply only with 'OK'") | Reply matches verbatim, completion-meta arrives |
| S2 | Single Bash tool call (`wc -l <file>`) | Tool use trace + correct number returned |
| S3 | Multi-tool chain (Read → Edit → confirm) | Multiple `ToolUse` StreamMessages emitted, final assistant text correct |
| S4 | Long-running tool call (~30s sleep) | Idle gate does not falsely cancel, response arrives |
| S5 | Explicit `/stop` cancel mid-turn | Cancellation propagates, no orphan child processes, channel returns to idle |
| S6 | Self-deploy recovery (deploy-release.sh during active turn) | Inflight resumes or fails cleanly with operator-visible reason |
| S7 | Follow-up turn (same session, second prompt) | `--resume <sid>` keeps context, second reply coherent |
| S8 | 24 h soak (one auto-generated turn every ~30 min) | 0 PTY/FD leaks, 0 stuck channels, 0 unexpected restarts |

## Gate criteria for Phase 3 promotion

Per mode:
- S1–S7 each pass at minimum **5 consecutive runs**.
- S8 soak completes ≥24 h with the criteria above.
- Turn-success rate variance across modes ≤ 2%.
- p95 first-token latency recorded; claude-e ≤ 1.5× the worst of
  pipe/tui (acceptable startup overhead).

When all three modes hit the gate, run a final cross-review (Codex
+ Claude general-purpose) on the cumulative diff and promote to
Phase 3 (default mode decision + operator docs).

## Rollback

Any single-mode failure → revert that channel's `runtime` line to a
known-good mode (`tui`), `agentdesk restart-dcserver`, capture the
failure mode in the decision log. Other two modes continue running.

## Observability

- `~/.adk/release/logs/dcserver.stdout.log` — primary log stream
- `agentdesk discord read <channel_id> --limit N` — verify Discord
  replies
- `ps -axf | grep -E 'claude-e|claude'` — orphan check
- `~/.claude/projects/.../session-*.jsonl` — transcript inspection
  for TUI mode

## Open questions to revisit during Phase 2

- Is the 6 s post-finalise cancel WARN log silenceable without removing
  legitimate cancel cleanup? (Codex round 4 diagnosis in flight.)
- `cache_ttl_minutes` forwarding to `claude-e` (Phase 1 known gap).
- `rate_limit_event` handling under `runtime: claude-e` (Phase 1 known
  gap).
- Default-mode decision for Phase 3 (`tui` for ops attach affordance vs.
  `claude-e` for cleanliness, or per-channel default).
