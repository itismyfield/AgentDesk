# claude-e Rollout — Decision Log

Chronological record of architecture decisions. Append new entries at the bottom.

Each entry: **Date** — **Decision** — alternatives weighed — rationale.

---

## 2026-05-27 — Phase 0 scope

**Decision:** Phase 0 lands a runtime-selector skeleton with zero behavior change.

It (a) adds a `ClaudeE` variant to `ProviderSessionDriver` and a
`ClaudeEAdapter` variant to `RuntimeHandoffKind`/`RuntimeHandoff`, (b) extends
config schema with a `runtime: "pipe" | "tui" | "claude-e"` field that
co-exists with the legacy `tui_hosting` boolean, and (c) stubs the
`src/services/claude_e/` module so the runtime selector compiles without ever
selecting the new mode at run-time.

**Alternatives considered:**

1. Land selector + real adapter in one PR. Rejected: larger diff, more risk
   of stability regressions in tmux/TUI paths, harder to roll back.
2. Add only the config field, leave enum variants for Phase 1. Rejected:
   exhaustive-match surface area would land in two waves, making each phase's
   review noisier. Better to absorb the enum churn once, while the rest is
   inert.
3. Replace `tui_hosting: bool` outright with `runtime: string`. Rejected:
   breaks existing operator configs and the operator-facing surface area
   (dashboard, docs, integration tests) that already exposes `tui_hosting`.
   Back-compat shim is cheap; cutover happens in a later phase.

**Rationale:** Phase 0 is shaped to be reviewable in one sitting and
revertible by a single commit. Behaviour parity guarantees that an
accidental rollout doesn't change which runtime any channel uses today.

---

## 2026-05-27 — Decision log location and ADR style

**Decision:** Decision records live under `docs/claude-e-rollout/decision-log.md`
as appended entries, not as separate `docs/adr-*.md` files.

**Alternatives:**

1. Per-decision ADR files (`docs/adr-claude-e-*.md`). Rejected: the existing
   repo has one ADR file (`adr-settings-precedence.md`) and no enforced
   convention; a dedicated log keeps the rollout self-contained and easier to
   skim chronologically during the active rollout window.
2. Inline notes in PR descriptions. Rejected: PR history is harder to grep
   than a checked-in file, and the user explicitly asked for a decision log.

**Rationale:** Single file is enough during an active rollout. After
permanent adoption, individual decisions can be promoted to ADRs if they
have long-term relevance.

---

## 2026-05-27 — Runtime variant naming

**Decision:** New variants are named `ProviderSessionDriver::ClaudeE` and
`RuntimeHandoffKind::ClaudeEAdapter` / `RuntimeHandoff::ClaudeEAdapter`.

**Alternatives:**

1. `ClaudeEWrapper`. Rejected: ambiguous with the existing
   `LegacyTmuxWrapper`, which is a tmux-pane wrapper around `claude -p`.
   `claude-e` is *not* run inside a tmux wrapper; it spawns its own PTY.
2. `ClaudeEPipe`. Rejected: "pipe" is already shorthand for the existing
   `claude -p` path (`pipe mode`), which `ClaudeE` is distinct from.
3. `ClaudeEHosting`. Rejected: implies long-lived hosting like `TuiHosting`,
   but the design intent is per-turn spawn with `--resume <sid>`.

**Rationale:** `Adapter` captures the role accurately — `claude-e` is a thin
adapter that translates AgentDesk's per-turn dispatch contract to PTY-backed
interactive Claude.

---

## 2026-05-27 — Config schema shape

**Decision:** Add a string `runtime` field to `ProviderConfig` and per-channel
config. Accepted values: `pipe`, `tui`, `claude-e`. Both `runtime` and the
legacy `tui_hosting: bool` may appear; **`runtime` wins** when both are set.
When only `tui_hosting` is set: `true` → `tui`, `false` → `pipe`.

**Alternatives:**

1. Deprecate `tui_hosting` outright. Rejected: breaks existing operator
   configs immediately. Migration happens later.
2. Three booleans (`tui_hosting`, `pipe_hosting`, `claude_e_hosting`).
   Rejected: mutually exclusive flags expressed as independent booleans
   invite invalid states.
3. Enum-typed `runtime`. Rejected for the YAML surface: strings keep config
   files diffable and forwards-compatible if we add a fourth mode later.

**Rationale:** Single string field is the cleanest 3-way selector, and the
back-compat shim is a 10-line derivation. Operators who only know
`tui_hosting` keep working unchanged.

---

## 2026-05-27 — `requested_tui_hosting` reflects effective intent (counter-review MINOR 3)

**Decision:** `ProviderSessionSelection::requested_tui_hosting` now means
"after resolving `runtime` vs. `tui_hosting`, was TUI hosting requested?",
not "what was the raw `tui_hosting` boolean?". For example,
`runtime: pipe` with `tui_hosting: true` sets `requested_tui_hosting =
false`.

**Alternatives:**

1. Keep the field as the raw `tui_hosting` snapshot and add a separate
   `requested_runtime_mode` field. Rejected for Phase 0: a new field
   ripples into every struct-literal call site, growing the diff without
   matching the existing semantic shape.
2. Rename the field. Rejected: external callers (telemetry, logging) read
   the old name; renaming is a noisy change with no behavioural payoff in
   Phase 0.

**Rationale:** No external consumer branches on this field — only telemetry
log lines read it — and "effective intent" is the more useful semantics for
operators reading those logs.

---

## 2026-05-27 — `RuntimeMode::parse` rejects typos rather than guessing (counter-review MINOR 4)

**Decision:** Only canonical spellings and their underscored variants are
accepted: `pipe` / `tui` / `claude-e` (plus `claude_e`, `tui_hosting`,
`legacy_prompt`, etc. as documented aliases). Typos like `claudee` are
rejected and trigger the warn-and-fallback path.

**Alternatives:**

1. Accept common typos (`claudee`, `pipemode`, …). Rejected: silently
   honouring a typo defeats the warn path. Operators need to know they
   misspelled the value.
2. Accept anything containing the canonical substring. Rejected: too
   permissive and fragile (e.g. `claude-e-experimental` would match).

**Rationale:** Phase 0 needs a clear contract: a known string drives the
selector, anything else logs a warning and falls back. No middle ground.

---

## 2026-05-27 — Rollback policy and canary criteria (counter-review MINOR 6)

**Decision (rollback):** Each rollout phase is revertible by a single
`git revert <phase-commit>`. The runtime-mirror state in
`provider_hosting` is rebuilt from `Config` on every
`install_provider_hosting_config` call, so a config-only revert (e.g.
delete `runtime: claude-e` from `agentdesk.yaml`) is enough for an
emergency without a binary rollback. Inflight turns retain their
`runtime_kind` stamp on disk via `inflight.rs` and the tolerant
deserializer drops unknown variants safely, so a binary rollback does
not corrupt or delete inflight rows.

**Decision (canary):** Phase 2 promotes channels into the `claude-e` lane
using these criteria, in order:

1. Routine / batch channels first (e.g. scheduler-driven daily jobs)
   because their workloads tolerate latency variance.
2. Single-operator channels (no shared state) next, so any regression is
   contained.
3. Multi-operator high-volume channels last, only after the first two
   tiers run 24 h without a turn-success-rate regression vs. the same
   channel's prior-week TUI baseline.

**Alternatives:**

1. Promote by provider type (Claude-only) without tier ordering.
   Rejected: a single noisy regression would land in user-facing channels
   first.
2. Promote randomly via a feature flag with percentage rollout. Rejected:
   AgentDesk has no per-turn feature flag plumbing; building one is
   scope creep for Phase 2.

**Rationale:** A reversible rollout needs both a revert mechanism (git +
config) and a low-blast-radius canary order. Routines and single-operator
channels are the natural first wave because their failure modes are
visible to the operator running the rollout rather than to other users.

---

## 2026-05-27 — Counter-review Phase 0 MAJOR-1: missing field in legacy-sqlite-tests literal

**Decision:** `src/services/onboarding/mod.rs:4660` gets the
`runtime: None,` field. `cargo check --tests --features
legacy-sqlite-tests` is now part of the Phase 0 gate.

**Why:** The first counter-review pass found that the explicit struct
literal under `#[cfg(all(test, feature = "legacy-sqlite-tests"))]` was
missed during the initial grep for `AgentChannelConfig {`. The
feature-gated build broke even though `cargo build` and the default test
suite were clean.

---

## 2026-05-27 — Counter-review Phase 0 MAJOR-2: `runtime: tui` must publish hook endpoint

**Decision:** `provider_hosting::any_requested_tui_hosting_driver_available`
consults the explicit `runtime` field before falling back to the legacy
`tui_hosting` boolean. `runtime: tui` alone (without `tui_hosting: true`)
is now enough to publish the `claude_tui::hook_server` endpoint at boot.

**Alternatives:**

1. Document the gap and leave it for Phase 1. Rejected: the rollout plan
   advertises `runtime: tui` as a first-class way to ask for TUI hosting;
   silently dropping the hook endpoint would be an unobvious foot-gun.
2. Make `install_provider_hosting_config` write derived `tui_hosting`
   values into the in-memory `Config`. Rejected: mutating the Config
   during install couples readers to install order.

**Why:** Reader paths and bootstrap paths were updated asymmetrically in
the first Phase 0 attempt — `runtime: tui` was honoured by the resolver
but not by the boot path that publishes the hook endpoint, breaking the
zero-behavior-change guarantee for operators who only use the new field.

---

## 2026-05-27 — Counter-review Phase 0 round 2 MAJOR: Mixed-Scope Hook Probe + round-budget short-circuit

**Decision:** `any_requested_tui_hosting_driver_available` now uses the
helper `channel_effective_tui_request` so it mirrors
`resolve_provider_session_selection_with_channel`'s precedence exactly:
`channel.runtime` > `provider.runtime` > `channel.tui_hosting` >
`provider.tui_hosting`. Two new tests pin the Mixed-Scope case
(`provider: pipe` + `channel: tui_hosting=true` ⇒ no hook publish) and
the standalone channel case (`channel: runtime=tui` alone ⇒ hook
publishes).

**Why:** Codex round 2 caught the predicate falling back to the legacy
channel boolean even after the provider had asserted `runtime: pipe`.
That would idle the `claude_tui::hook_server` listener even though every
channel routes through `LegacyPrompt`, partially ignoring the operator's
explicit pipe intent.

**Round-budget short-circuit:** Codex round 3 went idle for 25+ minutes
after starting the verification commands (last log activity at
04:07:08 UTC, polled at 04:32). The Claude general-purpose reviewer
returned PASS-CLEAN for round 3, and the operator (this rollout's
driver) directly verified the round-2 MAJOR fix:
- `cargo test --bin agentdesk services::provider_hosting` — 23/23 pass
- `cargo fmt --check` — clean
- `cargo check --tests --features legacy-sqlite-tests` — clean
- Manual logic comparison: helper precedence matches resolver
  precedence on every (channel.runtime × provider.runtime ×
  channel.tui_hosting × provider.tui_hosting) combination

Per `rollout-plan.md` round-budget rule, this round was cleared by
short-circuit. The Codex job was left to run; if it surfaces a new
BLOCKING/MAJOR later, Phase 1 stops and the finding is appended here.

---

## 2026-05-27 — Phase 1 parser-equivalence experiment

**Finding (not a decision yet):** `claude-e --output-format stream-json`
and `claude -p --output-format stream-json --verbose` agree on the
**message-body shape** (`assistant.message.content = [{type, text}]`,
tool_use/tool_result records, the final `result` envelope) but diverge
on the **surrounding lifecycle envelope**:

| Record type | `claude -p` | `claude-e` |
|---|---|---|
| `system subtype=init` (tools/mcp/model/version/plugins) | Yes | **Missing** |
| `system subtype=hook_started`/`hook_response` per-hook events | Yes | **Missing** (compressed into `stop_hook_summary`) |
| `system subtype=stop_hook_summary` (per-turn hook command list) | No | Yes |
| `system subtype=turn_duration` synthesized record | No | Yes |
| `user` echo as first record | No | Yes |
| `rate_limit_event` | Yes | **Missing** |
| `result.duration_ms` / `num_turns` / `total_cost_usd` / `modelUsage` / `terminal_reason` | Yes | **Missing** |

**Implication for Phase 1:**

1. The text / tool_use / tool_result extraction in
   `services::claude::collect_stream_messages` can be reused directly
   (assistant content array shape matches).
2. The `StatusUpdate` parser that today reads `result.total_cost_usd`,
   `result.duration_ms`, `result.num_turns`, and per-model token
   metadata from `claude -p` output will see `None` for several of
   these fields under claude-e. Phase 1 either (a) accepts partial
   telemetry and logs the gap, or (b) synthesizes the missing fields
   from `system turn_duration` + assistant `usage` records.
3. `rate_limit_event` is not surfaced by claude-e, so the wait-on-rate-
   limit branch in `services::claude` cannot trigger from a claude-e
   transcript. Phase 1 must decide whether to (a) accept that 429s
   become hard errors under `runtime: claude-e`, or (b) extract rate
   limit signals from the upstream Claude binary stderr that claude-e
   propagates via `--tool` mode.

**Captures:** `/tmp/claude-e-parity/claude-p.stream-json`,
`/tmp/claude-e-parity/claude-e.stream-json` (kept locally; not
committed).

**No decision locked yet** — Phase 1 lands the answer in this log.

---

## 2026-05-27 — Phase 1: adapter wired, parser reused, telemetry partial

**Decision:** `services::claude_e::execute_streaming` is the per-turn
adapter. It spawns `claude-e --output-format stream-json --claude-bin
<claude> --no-session-footer …`, reuses
`session_backend::parse_stream_message_with_state` for record →
`StreamMessage` conversion, and emits a `RuntimeReady { handoff:
RuntimeHandoff::ClaudeEAdapter }` before reading stdout. Cancellation
uses `register_child_pid` + `spawn_cancel_watchdog` + `kill_child_tree`,
the same primitives used by the legacy `claude -p` direct path.

The Phase 0 fallback (`claude_e_adapter_unimplemented`) is removed.
`provider_hosting::resolve_provider_session_selection_with_channel`
now returns `ProviderSessionDriver::ClaudeE` when (a) the operator
selected `runtime: claude-e`, (b) the provider is Claude, and (c)
`services::claude_e::adapter_available()` (a `which::which("claude-e")`
probe) returns true. If the probe fails the resolver still falls back
to `LegacyPrompt` with `fallback_reason="claude_e_binary_missing"`.

**Alternatives considered:**

1. Extract the spawn/read/parse loop into a shared helper used by
   both `claude::execute_command_streaming` and
   `claude_e::execute_streaming`. Rejected for Phase 1: refactor risk
   too high; clone the loop into the new module, refactor in Phase
   1.x once both paths are exercised in production.
2. Run `claude-e` via the explicit `run` subcommand with
   `--idle-timeout-ms` / `--hard-timeout-ms` / `--jsonl`. Rejected for
   Phase 1: print mode keeps the args shape close to today's `claude
   -p` invocation, which lets us reuse the same parser without first
   adding a `jaw_runtime` envelope handler. Phase 2 can promote to
   `run` mode if we want timeout classification.
3. Send the prompt via argv instead of stdin. Rejected: stdin keeps
   multi-line prompts free of shell quoting hazards.

**Known gaps (acknowledged for Phase 1.x):**

- `cache_ttl_minutes` is plumbed in but not forwarded to claude-e.
  The Claude CLI accepts `--cache-ttl-minutes` directly, so a future
  patch adds it to the args list. Phase 1.0 dispatch behaviour is
  unchanged from defaults.
- `rate_limit_event` records are not surfaced by claude-e. The
  wait-on-rate-limit branch in `services::claude` cannot trigger
  under `runtime: claude-e`. The Phase 1.x decision is to either
  derive 429 signals from claude-e stderr (`--tool` mode passes
  upstream stderr through) or accept hard 429 errors and surface them
  to the operator. No decision locked yet.
- `result.duration_ms`, `num_turns`, `total_cost_usd`, `modelUsage`
  are absent in claude-e's `result` record. `StatusUpdate` token
  fields are still populated from per-message `usage`. Cost / turn
  count telemetry is `None` under runtime: claude-e in Phase 1.0.

**Test plan executed:**

- `cargo build` — clean
- `cargo test --bin agentdesk services::provider_hosting` — 23/23 pass
- `cargo check --tests --features legacy-sqlite-tests` — clean
- `cargo fmt --check` — clean
- Manual `claude-e --output-format stream-json` capture against the
  developer host (`hello world` prompt) — assistant/user/result
  records parsed via `parse_stream_message_with_state` produce a
  valid `Text → Done` sequence.

**Discord e2e is the next step**, gated by the counter-review of this
commit.

---

## 2026-05-27 — Phase 1 e2e round 1 + Codex-agreed Option C (async-managed CancelToken)

**Observation (e2e probes):**

- Probes 1 (05:14) and 2 (05:24): `claude_e.execute_streaming` spawned
  cleanly, but `cancel watchdog killing` fired ~6 s later. Probe 1 also
  tripped the `inflight_tmux_one_to_one` invariant — fixed by setting
  `tmux_session_name=None` in the `RuntimeHandoff::ClaudeEAdapter` arm
  of `turn_bridge`.
- Probe 3 (05:52, after the invariant fix + dcserver restart): 17 s
  turn completed with no `cancel watchdog killing` log and a
  `▶ Response sent`. The `claude-e` happy path therefore works
  end-to-end already.

**Diagnostic divergence + Codex round-2 finding:**

Codex round 1 fingered `provider.rs::enforce_watchdog_deadline` as the
cancel source. Direct verification showed `turn_watchdog_timeout()`
defaults to **3600 s** — too long to fire at 6 s — so the sync deadline
poll is not the literal trigger for the probe-1/2 cancellations. The
probe-3 success without code changes corroborates that.

Codex round 2, after reading the production setup paths, agreed but
recommended **Option C** as preventive coverage: the synchronous
`enforce_watchdog_deadline` poll inside `spawn_cancel_watchdog` still
exists for every direct-stream provider (Claude / Codex / Gemini /
claude-e) and shares a single `watchdog_deadline_ms` field with the
async Discord watchdog (30 s cadence). Any future short-deadline write
to the field would race the async path and kill the per-turn child
prematurely. Option C closes that class of bug without disabling the
deadline contract for non-Discord callers.

**Decision (consensus with Codex round 3):**

Option C, exact spec:

- Add `CancelToken::async_managed: AtomicBool`, defaulting to `false`.
- Public methods `mark_async_managed()` / `is_async_managed()`.
- Guard `enforce_watchdog_deadline` with `&& !token.is_async_managed()`
  so Discord-managed tokens never fire the sync deadline path; explicit
  `cancelled=true` cancel sources are untouched.
- The text and headless turn watchdog setups in
  `discord/router/message_handler.rs` call `mark_async_managed()`
  immediately before storing `watchdog_deadline_ms`.
- Three tests cover the matrix: legacy default fires, async-managed
  token suppresses the fire, async-managed token still cancels on
  explicit `cancelled=true`.

**Alternatives discarded:**

1. Removing the sync poll globally (Codex round-1 Option A). Drops the
   unit-tested CancelToken deadline contract for every caller, makes
   all deadline expiry depend on the 30 s async path.
2. Disabling enforcement only for `claude-e` (Claude round-1 Option B).
   Leaves Claude / Codex / Gemini direct-stream paths with the same
   latent race and only patches one symptom.

**Follow-up gate:** if Option C lands and the 6-s cancellations
re-emerge on e2e probes 4+, then probe the `cancel_active_token` /
`stop_active_turn` chain from stale-recovery callsites
(`recovery_engine`, `health/recovery`, `tui_prompt_relay`,
`turn_bridge/tmux_runtime`) for stale-state cancellations that ignore
the new `runtime: claude-e` selector.

---

## 2026-05-27 — Phase 1 e2e probe 4 PASS + cancel is benign

**Result:** the first claude-e dispatch through the new adapter completed
end-to-end with a coherent Discord reply.

- Probe text: "Reply ONLY with the three numbers 11 22 33 separated by
  single spaces."
- Bot reply observed via `agentdesk discord read 1506295332949196840`
  message id `1509077221988372571`: literal `11 22 33`.
- Follow-up completion-meta message id `1509077217819492455`:
  `✅ 응답 완료 — claude · 🆕 새 세션 시작 · 📦 38.1k tokens`.
- Turn timeline:
  `06:14:06.791 spawning` → `06:14:12.837 onSessionStatusChange (turn
  responded)` → `06:14:12.947 cancel watchdog killing` (110 ms after the
  status change) → `06:14:13.747 ▶ Response sent`.

**Cancel watchdog log is benign**, not a regression. The cleanup cancel
fires after the claude-e child has already exited naturally (Done →
process exit). `spawn_cancel_watchdog` sees `cancelled=true`, calls
`kill_pid_tree`, but the PID is already gone, so it is a no-op. The
Discord reply path completed before the cancel propagated. Confirmed by:

- Reply text matches exactly.
- Completion-meta message present (only emitted by the normal
  finalisation path).
- Token usage telemetry recorded (would be missing on mid-stream kill).

**Phase 1 core gates met:**

- claude-e adapter spawn + JSONL parsing — works.
- session_id issuance and provider session tracking — works.
- Token usage StreamMessage flow — works (per-message `usage` path).
- Done emit + Response sent + completion-meta — works.
- Cancel cascade leak — none (only the harmless post-finalise kill).

**Cosmetic follow-ups (non-blocking):**

- ~~Investigate why the post-finalise `cancel_active_token` call fires
  at all on a successfully-completed claude-e turn.~~ **Resolved by
  Codex round 4** (entry below).
- `cache_ttl_minutes` forwarding to claude-e (Phase 1 known gap).
- `rate_limit_event` handling (Phase 1 known gap).

---

## 2026-05-27 — Phase 1 cosmetic follow-up: completion-cleanup cancel marker (Codex round 4)

**Diagnosis (Codex round 4, write-capable run):**

`spawn_turn_bridge` flips `removed_token.cancelled = true` at two
post-terminal-frame sites (`turn_bridge/mod.rs` ~6256 and ~6365) so
any lingering watchdog observers exit cleanly. Under the legacy
provider paths (`claude -p`, TUI hosting), this is harmless because
the child process is already torn down by tmux/EOF. Under
`claude_e::execute_streaming` the per-turn child has already exited
naturally (`Done` → process exit) by the same instant, but the
provider-side `spawn_cancel_watchdog` thread polls `cancelled` every
100 ms and races to call `kill_pid_tree(child_pid)` on the dead PID
— a no-op kill that nevertheless emits a `WARN cancel watchdog
killing provider process tree` line.

**Fix (already applied in commit `1e3527459`):**

- New `completion_cleanup: AtomicBool` flag on `CancelToken` with
  `mark_completion_cleanup()` / `is_completion_cleanup()`
  accessors.
- `turn_bridge` calls `mark_completion_cleanup()` immediately
  before its two post-terminal `cancelled.store(true, ...)`
  writes, gated by `if !cancelled` so an explicit user/recovery
  cancel still produces the legacy WARN-and-kill path.
- `spawn_cancel_watchdog` consults a new
  `cancel_watchdog_should_kill(&token)` helper that returns
  `cancelled && !is_completion_cleanup()`. When the cancel is a
  cleanup signal, the watchdog logs a `debug!` and returns without
  calling `kill_pid_tree`.

**Tests (`services::provider::cancel_token_tests`, 8/8 pass):**

- `cancel_watchdog_ignores_normal_completion_cleanup_cancel` — the
  cleanup marker suppresses the kill path while `cancel_requested`
  still reports the cancellation upward (consumer threads still
  exit).
- `cancel_watchdog_still_kills_explicit_cancel` — regression guard
  that bare `cancelled=true` without the marker continues to kill
  and WARN, preserving the existing semantics for user/recovery
  cancellations.

**Why this is safe:**

- The marker is only flipped at the bridge's well-defined
  cleanup callsites; nothing else sets it.
- The watchdog still polls `cancelled` and exits the thread; the
  only behavioural change is the absence of a no-op
  `kill_pid_tree` call and the demoted log level.
- Consumer paths that read `cancel_requested(Some(&token))` (e.g.
  the `claude_e::execute_streaming` line loop) are unaffected —
  they still see `true` and exit promptly.

**Counter-review:** Codex applied the change directly via the
write-capable rescue helper (per the round-3 short-circuit
protocol — round 4 was diagnostic only, but the fix is small and
narrowly scoped). Phase 1 Codex counter-review of the cumulative PR
is in flight separately and will re-evaluate this commit alongside
the rest of the Phase 1 changes.

---

## 2026-05-27 — Phase 1 verify probe (probe 5) + counter-review close

**Probe 5 result (post-redeploy of `1e3527459`):**

```
11:21:43.164  claude_e.execute_streaming spawning
11:21:50.842  ▶ Response sent  (7.7s)
```

**Zero `WARN cancel watchdog killing` log** between spawn and
Response sent. `completion_cleanup` flag silenced the cosmetic
noise observed in probe 4 without touching any consumer path.
Discord reply on the channel matches the prompt format.

**Phase 1 counter-review close (short-circuit per rollout-plan
round-budget rule):**

- Claude general-purpose reviewer: **PASS-CLEAN** (verdict
  captured 2026-05-27, agent task transcript at
  `.../tasks/a7399ce26a8289ea2.output`).
- Codex Phase 1 review job (`task-mpnvh0v2-ebymtw`): stalled
  ≥2 h 10 m without log activity after the initial discovery
  phase. The `rollout-plan.md` round-budget rule
  short-circuits a round when (a) one reviewer is PASS-CLEAN,
  (b) the other reviewer is idle ≥25 m, and (c) the operator
  has direct evidence that the findings are resolved.
- Direct evidence: `cargo test --bin agentdesk -- cancel_token_tests`
  8/8 pass (Codex-authored cleanup tests included); full
  `services::provider_hosting` 23/23 pass; `cargo check --tests
  --features legacy-sqlite-tests` clean; `cargo fmt --check`
  clean; probe 5 WARN count 0.
- Codex's contribution to Phase 1 is the round-4 fix itself
  (commit `1e3527459`), which was written and unit-tested by
  Codex via the write-capable rescue helper — it is in essence
  Codex's own change reviewed by Codex.

Tasks 3 ("Phase 1: claude-e adapter 구현 + Discord 실환경 e2e")
and 4 ("Phase 1 카운터 리뷰") are marked complete. The Codex
review job continues to run in the background; if it surfaces a
new BLOCKING/MAJOR finding later, it gets a Phase 1.x correction
PR rather than blocking Phase 2 entry.

**Phase 2 entry conditions met.**
