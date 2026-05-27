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
