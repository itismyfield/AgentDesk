# Rust Quality Gates

Issue #2954 introduces a Rust-native single entrypoint without pretending the
repository is already clippy-clean.

## Entrypoints

- `just check`: local/CI aggregate for `cargo fmt --check`, staged clippy,
  `cargo check --workspace --all-features`, and the existing non-Postgres test
  subset.
- `just test-postgres`: existing PostgreSQL test lane for CI jobs with a
  Postgres service.
- `just lint-strict`: target end state, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`. This is intentionally not wired into required
  CI yet.

## Current Staging

The hard clippy gate currently denies `dbg_macro`, `todo`, and `unimplemented`
through `Cargo.toml` plus the `just lint` command. This gives CI a passing
Rust-native lint gate while the larger zero-warning cleanup is split into
reviewable follow-ups.

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
