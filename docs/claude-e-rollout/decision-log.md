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
