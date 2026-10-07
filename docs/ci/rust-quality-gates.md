# Rust Quality Gates

Issue #2954 introduces a Rust-native single entrypoint without pretending the
repository is already clippy-clean.

## Entrypoints

- `just check`: local aggregate for `just fmt-check`, staged clippy,
  `cargo check --workspace --all-features --all-targets`, the existing
  non-Postgres test subset, and the targeted `ClaudeBinary` compile-fail
  doctest guard.
- `just test-postgres`: existing PostgreSQL test lane for CI jobs with a
  Postgres service.
- `just lint-strict`: target end state, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`. This is intentionally not wired into required
  CI yet.

## Current Staging

`Cargo.toml` configures eight Clippy denies: `dbg_macro`, `todo`,
`unimplemented`, `await_holding_lock`, `large_enum_variant`, `result_large_err`,
`too_many_arguments` and `type_complexity`. The authoritative staged command is
`cargo clippy --workspace --all-targets --all-features -- -W clippy::all`.
The trailing group level overrides the five group-member denies to warnings;
only `dbg_macro`, `todo` and `unimplemented` remain denied. Configured deny
entries must not be mistaken for effective hard gates under this argv.

Main CI observes this same invocation once, adding only Cargo's JSON output
format. `check_clippy_warning_count.py` counts compiler warning diagnostics
(including Rust warnings), requires successful `build-finished`, and records
SHA, runner, pinned toolchain, source and argv in the uploaded observation.
Compilation still fails the step through pipefail. Invalid observations emit an
explicit warning, never a valid zero. The count is compiler-emitted diagnostics,
not source occurrences or a deduplicated cross-target debt estimate.

Warning-total ratcheting (B2) remains deferred until a valid main Ubuntu/pinned
Rust observation establishes a reproducible baseline. No historical or local
Mac count is admitted as that baseline. Existing suppression-occurrence
ratchets do not enforce warning totals. B3 (`lint-strict` required CI) remains
deferred until warning debt is zero under the authoritative environment.

The dead-code suppression ratchet freezes the observed per-file counts in
`src/**/*.rs`, including tests: 389 suppression bodies across 179 files.
It catches `allow`/`expect`, conditional attributes and broader `unused`/
`warnings` groups using the existing Rust lexical scanner. New paths and
per-file increases fail; decreases pass. This is not permission to add more
suppression or proof that dead code was removed. Relocations require a reviewed
allowance transfer that removes the old allocation. No automatic baseline
rewrite is exposed. The baseline is suppression debt, not B2 warning debt.

`unwrap`, `expect`, and `panic` lint gates are intentionally deferred. The
current tree has many uses in tests, fixtures, and some existing runtime paths,
so enabling those lints in stage 1 would mix a broad policy decision with the
single-entrypoint work. A follow-up should decide whether the policy is
production-only, test-aware, or repo-wide before adding hard gates.

## Non-Postgres Test Scope

`just test-non-pg` keeps a targeted non-Postgres subset for local `just check`.
CI no longer runs it: on every `main` push `ci-main.yml` `full_non_pg` runs the
same adjudicated whole-library sweep as the PR `library_sweep` (non-PG `--lib`
minus `scripts/ci/non-pg-test-filter.sh`, with its own PostgreSQL), and the
`lint` job runs fmt, clippy, the policy JS tests, the non-lib side of the
recipe's `--all-targets` lines (`--bins --test '*'`), and the `ClaudeBinary`
compile-fail doctests. The nightly macOS and Windows lanes run their
`--all-targets` non-Postgres sweeps.

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
Ubuntu lint jobs (PR `Lint runner`, main `Main lint`) are the authoritative format, clippy, and
full workspace gates until remaining Unix-only tmux tests are made portable or
cfg-guarded.
`ci-nightly.yml` has the same Windows boundary today: its Windows lane runs
default-feature `cargo test --all-targets`.
`ci-main.yml` `windows_cache_warm` builds the same Windows compile on every
`main` push (`cargo check --workspace --all-targets` and `cargo test --lib
--no-run`, no tests run) and saves the registry and sccache entries the PR
Windows jobs restore. It is advisory (`continue-on-error`) and not a required
context; `scripts/check-ci-runner-hardening.sh` keeps its env, setup steps and
rust-cache inputs equal to the PR Windows jobs so both hash to the same keys.

### Local Windows GNU compile check from macOS

Use the repository's pinned Rust toolchain and install the cross compiler:

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64
cargo check --target x86_64-pc-windows-gnu --all-targets
```

`--all-targets` is load-bearing, not optional: the required lane
(`Fast check + non-PG tests (windows-latest)`) runs
`cargo check --workspace --all-targets`, and most of this repo's code lives in
`#[cfg(test)]` targets that `--lib` never compiles -- including
`src/services/discord/tmux_watcher_registry_restore_tests.rs`, which is
declared `#[cfg(test)] mod` with no unix gate and so is a Windows target only
under `--all-targets`. Pre-checking with `--lib` can pass while the required
lane fails. `--workspace` is a no-op here (`Cargo.toml` declares no
`[workspace]` section), so it is omitted above.

If bindgen cannot locate the target headers, set the target-specific include
path before retrying (adjust it to the installed MinGW sysroot):

```bash
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu="-I$(brew --prefix mingw-w64)/toolchain-x86_64/x86_64-w64-mingw32/include"
```

This checks `cfg(windows)` / `not(unix)` compilation, not native Windows
execution. GNU is not MSVC; the PR's native Windows lane is still required.

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
