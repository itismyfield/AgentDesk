//! #1073 (#905-4): Discord bot flow integration test harness.
//!
//! Structure
//! =========
//! - [`mock_discord`] — mock HTTP layer for the outbound Discord transport.
//!   Records every `post_message` call and can return configurable errors so
//!   duplicate-send / retry paths can be exercised deterministically.
//! - [`harness`] — [`TestHarness`] bundling the mock transport, an optional
//!   ephemeral PostgreSQL database, and an isolated tmux namespace rooted at
//!   `/tmp/r4-tmux-<uuid>` so concurrent runs never stomp the shared tmux
//!   socket.
//! - [`scenarios`] — three DoD scenarios (duplicate relay / session kill on
//!   restart / cross-watcher race). Each scenario is written against the
//!   narrowest API surface the harness actually needs so the whole suite
//!   stays under the 10-second-per-test budget and avoids real network I/O.
//!
//! Why this is its own submodule
//! -----------------------------
//! The per-file `mod tests {}` blocks in `services/discord/*` can only
//! exercise private state one file at a time. The flow-level invariants we
//! want to pin here (dedupe across watchers, queue preservation across
//! restart, claim/replace race) span several modules, so we place them under
//! `integration_tests::discord_flow` alongside the pre-existing
//! `high_risk_recovery` lane.
//!
//! Running locally
//! ---------------
//! - `cargo test --bin agentdesk integration_tests::tests::discord_flow`
//! - The Postgres scenario is gated on `POSTGRES_TEST_DATABASE_URL_BASE` (or
//!   a reachable default `postgres://<user>@localhost:5432`). When the env
//!   is missing the harness still provides the full mock Discord + tmux
//!   namespace pair; the scenario that needs Postgres is marked `#[ignore]`
//!   via [`requires_pg`] so CI can opt in via `--ignored`.

// The parent file wires this via `#[path = "../discord_flow/mod.rs"]` so
// the actual source tree lives at `src/integration_tests/discord_flow/`
// per the #1073 spec, even though the module is hung under the
// `integration_tests::tests::discord_flow` path. We therefore need
// explicit `#[path]` overrides on the submodules, otherwise Rust would
// look for them relative to the `integration_tests::tests::` dir.
#[path = "harness.rs"]
pub(super) mod harness;
#[path = "mock_discord.rs"]
pub(super) mod mock_discord;
#[path = "scenarios.rs"]
pub(super) mod scenarios;
