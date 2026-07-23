# Rust Quality Gates

Issue #2954 introduces a Rust-native single entrypoint without pretending the
repository is already clippy-clean.

## Entrypoints

- `just check`: local/CI aggregate for `just fmt-check`, staged clippy,
  `cargo check --workspace --all-features --all-targets`, the existing
  non-Postgres test subset, and the targeted `ClaudeBinary` compile-fail
  doctest guard.
- `just test-postgres`: existing PostgreSQL test lane for CI jobs with a
  Postgres service.
- `just lint-strict`: target end state, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`. This is intentionally not wired into required
  CI yet.

## Current Staging

The hard clippy gate currently denies `dbg_macro`, `todo`, and `unimplemented`
through the root `Cargo.toml` `[lints.clippy]` table. `just lint` also passes
`-W clippy::all` so CI exposes the remaining warning debt, but those warnings
are informational until the zero-warning cleanup lands. This gives CI a passing
Rust-native lint gate while the larger zero-warning cleanup is split into
reviewable follow-ups.

`unwrap`, `expect`, and `panic` lint gates are intentionally deferred. The
current tree has many uses in tests, fixtures, and some existing runtime paths,
so enabling those lints in stage 1 would mix a broad policy decision with the
single-entrypoint work. A follow-up should decide whether the policy is
production-only, test-aware, or repo-wide before adding hard gates.

## Non-Postgres Test Scope

`just test-non-pg` preserves the targeted non-Postgres subset already used by
CI on `main`: `ci-main.yml` runs it through `just check`, while the nightly
macOS and Windows lanes run their broader non-Postgres sweeps. The required PR
`check_fast` lane runs policy tests plus `just cargo-check` only, so this
longer subset does not remain on the PR critical path.

A broader sweep was attempted with:

`cargo test --workspace --all-features --all-targets -- --skip _pg --skip pg_ --skip postgres --test-threads=1`

That sweep is not CI-safe yet: it fails existing legacy/full integration,
route, config, dispatch, and engine tests that require additional environment
or database setup beyond the current fast lane. Stage 1 therefore documents the
known gap instead of silently pretending the broad sweep passes.

Follow-up split: separate deterministic unit tests from environment-dependent
integration tests, add explicit setup/feature boundaries for each group, then
replace the staged subset with a broader non-PG command in `just check`.

## Windows CI Scope

The Windows PR lane intentionally remains inline cargo commands for stage 1
instead of calling `just check`. The root `justfile` uses bash-oriented recipe
semantics, while that lane is meant to provide cross-OS compile and targeted
test signal. It also stays default-feature-only with `cargo check --workspace
--all-targets`: the retired SQLite-only feature is no longer declared in
`Cargo.toml`, and Windows remains a default-feature compile/test signal. The
Ubuntu `just check` and `lint` jobs are the authoritative format, clippy, and
full workspace gates until remaining Unix-only tmux tests are made portable or
cfg-guarded.
`ci-nightly.yml` has the same Windows boundary today: its Windows lane runs
default-feature `cargo test --all-targets`.

## Strict Clippy Debt

`cargo clippy --workspace --all-targets --all-features -- -D warnings` currently
fails with existing warnings, mostly in tests and relay/Discord code:

- unused imports/variables/assignments in doctor, dispatch outbox, onboarding,
  pipeline, route tests, and server tests
- `clippy::inconsistent_digit_grouping` in Discord/tmux test channel IDs
- `clippy::empty_line_after_outer_attr` in kanban transition tests
- `unexpected_cfgs` for `feature = "pg_integration"` not declared in
  `Cargo.toml`
- `clippy::io_other_error`, `clippy::collapsible_match`,
  `clippy::unnecessary_get_then_check`, `clippy::needless_update`,
  `clippy::useless_concat`, `clippy::redundant_closure_call`,
  `clippy::write_literal`, and `clippy::useless_vec`
- dead test helper code such as `GeminiPathOverride`

Follow-up split: first remove pure mechanical warnings in tests, then decide
whether `pg_integration` should become a real feature or a checked cfg, then
promote `just lint-strict` into `just check`.
