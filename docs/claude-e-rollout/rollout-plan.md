# claude-e Rollout — Phased Plan

Goal: deliver three Claude runtimes (`pipe` / `tui` / `claude-e`), all
config-selectable per-channel and globally, to a stable steady state in
Discord e2e as fast as possible — without deleting the existing tmux
wrapper or TUI hosting code paths.

Each phase ends with a counter-review (Codex + Claude cross-check). Reviews
repeat until "clean" (zero blocking findings). Decisions taken without user
input are recorded in `decision-log.md`.

## Phase 0 — Skeleton & decision log (this PR)

- `ProviderSessionDriver::ClaudeE` + `RuntimeHandoffKind::ClaudeEAdapter`
- `RuntimeHandoff::ClaudeEAdapter { output_path, session_name, last_offset }`
- Config: `runtime: "pipe" | "tui" | "claude-e"` with `tui_hosting`
  back-compat
- `src/services/claude_e/` module skeleton (compiles, never selected at
  runtime)
- Decision log + rollout plan
- **Gate:** workspace builds, all existing tests pass, no observable
  runtime behavior change

## Phase 1 — Adapter implementation + Discord e2e

- `claude_e::process` PTY spawn (binary discovery, args, env)
- `claude_e::jsonl_parser` (validate parity with current transcript parser
  per Decision Log entry to come)
- `claude_e::cancellation` (SIGINT → SIGKILL cascade, child Claude + MCP
  reaped)
- Adapter wires `RuntimeHandoff::ClaudeEAdapter` through `turn_bridge` so
  `runtime: claude-e` channels actually dispatch via `claude-e`.
- **Experiments in PR:**
  - Hook policy: confirm claude-e `--settings` injection can be disabled or
    coexist non-conflictingly with the AgentDesk hook bundle.
  - Parser equivalence: diff TUI transcript JSONL against `claude-e`
    `--output-format stream-json` for the same prompt; reuse parser or add
    a thin adapter.
  - Cancel cascade: kill -9 sweep verifies no orphan claude / MCP children.
- **Gate (any failure blocks Phase 2):**
  - hook conflict count = 0 in soak
  - parser equivalence confirmed or thin adapter added
  - cancel leak = 0
  - 24 h soak with no PTY/FD leak
  - one full turn dispatched in the e2e channel using `runtime: claude-e`
    returns a coherent transcript

## Phase 2 — Three-way parallel e2e

- Channel set: 1+ channel per mode (`pipe`, `tui`, `claude-e`). Use
  `agentdesk send-to-agent` / announcebot admin to provision the
  additional channels if needed.
- Same scenario battery run against each mode. Track turn-success rate,
  p95 launch latency, cancel cleanliness.
- Issues fixed in-PR until 5 consecutive runs pass on every mode.
- **Gate:** turn-success rate within ±2% across modes, no mode regresses
  vs. its baseline, recovery paths exercised on each.

## Phase 3 — Permanent operation

- Default runtime decision recorded in decision log with justification
  (latency, stability, hook compatibility, observability).
- Operator docs updated: how to flip a channel between modes.
- Modes other than the default remain selectable; **no code deletion**.
- Final cross-review (Codex + Claude). After clean: PR(s) merged to main.

## Rollback matrix

Each phase is reversible in three layers, in order of decreasing blast
radius:

| Layer | Trigger | Action | Recovery time |
|---|---|---|---|
| Config-only | A `runtime` value misbehaves on one channel | Remove the `runtime` line (or set it back to `tui` / `pipe`) and reload config | Seconds — `install_provider_hosting_config` rebuilds the mirrors on next config read |
| Binary, single-commit | A phase regresses behaviour systemically | `git revert <phase-commit>` and redeploy via `scripts/deploy-release.sh` | Minutes — re-deploy + dcserver restart |
| Binary, full rollback | Multiple phases must be unwound | `git revert` each phase commit in reverse order | Tens of minutes |

Cross-phase invariants that must survive a rollback:

- **Inflight rows** stamp `runtime_kind` strings; the tolerant deserializer
  drops unknown variants safely, so a newer-binary row never breaks an
  older binary.
- **`tui_hosting` semantics** never change: a binary rollback leaves
  channels routing through whatever `tui_hosting` says.
- **No code deletion**: all three runtime branches stay reachable
  regardless of which phase you roll back to.

## Counter-review protocol

For each phase PR:

1. Spawn a Codex `codex:codex-rescue` agent with the diff + Phase context.
2. Spawn a Claude `general-purpose` reviewer agent with the same diff.
3. Both must produce a written verdict with severity-tagged findings.
4. Repeat until both return zero blocking findings.
5. Findings and resolutions appended to decision log.

### Round budget and parallel work policy

Counter-reviews can run long. To keep the rollout moving without
abandoning the "review-clean before merge" gate:

- Reviews run **in parallel** with the next phase's preparation work
  whenever the next-phase prep is read-only or behaviour-inert (e.g.
  installing the `claude-e` binary, capturing JSONL samples, drafting
  designs).
- If a Codex round exceeds **10 minutes** without an interim finding
  visible in the log, the operator may proceed with the next-phase
  *preparation* work while the review keeps running. Code edits that
  affect the current PR's behaviour must still wait for the round to
  complete.
- A round is considered **clean** when both reviewers return PASS with
  zero BLOCKING + zero new MAJOR. Round-N is short-circuitable if (a)
  one reviewer already returned PASS-CLEAN and (b) the operator has
  directly verified the other reviewer's previous round-(N-1) findings
  are fixed (tests, fmt, build proof appended to decision log).
- Round 3+ defaults to **single Codex pass** unless round 2 surfaced
  net-new findings beyond round 1; the Claude reviewer rejoins only if
  Codex flags something.

The decision log records when the short-circuit clause was used and
why.
