# Rust Quality Gates

Issue #2954 introduces a Rust-native single entrypoint without pretending the
repository is already clippy-clean.

## Entrypoints

- `just check`: local aggregate for `just fmt-check`, staged clippy,
  `cargo check --workspace --all-features --all-targets` and `just test`.
- `just test`: the CI nextest run, `cargo nextest run --lib --profile ci`, over every
  lib test; the PostgreSQL ones need a dedicated test server.
- `just test-non-pg`: without PostgreSQL, the lib tests the source PG classifier leaves
  out through the native CI path, then every non-lib test and the `ClaudeBinary`
  compile-fail doctests.
- `just test-postgres`: every lib test against an explicit
  `POSTGRES_TEST_DATABASE_URL_BASE` server.
- `just lint-strict`: target end state, `cargo clippy --workspace --all-targets
  --all-features -- -D warnings`. This is intentionally not wired into required
  CI yet.

## Current Staging

`Cargo.toml` configures eleven deny entries: the `correctness` group and ten
individual lints, `dbg_macro`, `todo`, `unimplemented`, `await_holding_lock`,
`drop_non_drop`, `mistyped_literal_suffixes`, `large_enum_variant`,
`result_large_err`, `too_many_arguments` and `type_complexity`.
The authoritative staged command is
`cargo clippy --workspace --all-targets --all-features -- -W clippy::all`.
The trailing group level lowers the Cargo levels of group-member denies;
the four structural complexity lints remain warnings.

Effective denies for lib/bin under the staged command: `dbg_macro`, `todo`,
`unimplemented`, `await_holding_lock`, `drop_non_drop`, `mistyped_literal_suffixes`
and the `correctness` group.

The first three are outside `clippy::all`. For the other entries, `src/lib.rs`
and `src/main.rs` redeclare crate-level `#![deny(clippy::<lint>)]` attributes,
which keep them denied under this argv. These attributes cover the lib/bin
crates, including their unit tests; separate integration-test crates are not
covered by those attributes. The library retains existing individual correctness
allows for `absurd_extreme_comparisons` and `overly_complex_bool_expr`; the group
deny does not remove them. Configured Cargo deny entries alone must not be
mistaken for effective hard gates under this argv.

Main and PR lint run this same invocation once, adding only Cargo's JSON output
format. `check_clippy_warning_count.py` counts compiler warning diagnostics
(including Rust warnings), requires successful `build-finished`, and records
SHA, runner, actual rustc/Clippy versions, host, source and the protected argv.
Compilation still fails the step through pipefail. Invalid observations fail
the gate, never producing a valid zero. Main uploads the observation. The count
is compiler-emitted diagnostics, not source occurrences or a deduplicated
cross-target debt estimate.

The warning-total gate uses `scripts/clippy_warning_baseline.json`: **290**
diagnostics, measured locally on WSL at
`6330b17f93a7852c42580e65df8f0c90752a1765` after reducing the previous 342-warning
ceiling. The observation records its local WSL source, commit SHA and diagnostics
digest; it does not claim a CI run or uploaded artifact. The environment remains
Rust 1.94.1, x86_64 Linux and the staged argv above; local Mac totals cannot
substitute for it. Main and PR CI measure again and reject warning growth or
invalid/mismatched observations. PR CI observes the merge ref with main, so the
combined change needs a fresh observation when it lands. The existing required
`Lint` mirror owns PR enforcement; no required context is added.

Lower the baseline with a valid observation when warning debt decreases.
Admission compares the committed PR base (main uses its first parent) and
rejects baseline increases or environment changes. A toolchain/environment
upgrade requires a separately reviewed rollout. Existing suppression-occurrence
ratchets remain separate from warning totals. B3 (`lint-strict` required CI) remains
deferred until warning debt is zero under the authoritative environment.

The dead-code suppression ratchet freezes the observed per-file counts in
`src/**/*.rs`, including tests: 389 suppression bodies across 179 files.
It catches `allow`/`expect`, conditional attributes and broader `unused`/
`warnings` groups using the existing Rust lexical scanner. New paths and
per-file increases fail. Inner attributes have a separately measured 34-body
baseline, so replacing an outer suppression with an inner one cannot broaden
its scope under an unchanged total. Decreases require lowering the matching
allocation; admission rejects baseline increases against the committed base.
This is not permission to add more suppression or proof that dead code was
removed. Relocations to a new path require a separately reviewed admission
change that removes the old allocation; the default gate rejects new allocations.
No automatic baseline
rewrite is exposed. The baseline is suppression debt, not B2 warning debt.

`unwrap`, `expect`, and `panic` lint gates are intentionally deferred. The
current tree has many uses in tests, fixtures, and some existing runtime paths,
so enabling those lints in stage 1 would mix a broad policy decision with the
single-entrypoint work. A follow-up should decide whether the policy is
production-only, test-aware, or repo-wide before adding hard gates.

## Non-Postgres Test Scope

`just test-non-pg` runs the classifier's non-PG lib tests locally the way the native
nightly does. CI does not call it: on `main` push and dispatch, every nightly, and on PRs whose
`rust_tests` filter is on, `rust_build` archives the lib tests and plans every listed test
over six `rust_tests` workers, each with its own PostgreSQL service, and
`test_evidence` audits that every planned test ran. PR, main and nightly keep the same
three jobs; `scripts/check-ci-runner-hardening.sh` allows them to differ only in
`needs`, `if`, the PR base comparison and the main `nextest-baseline` upload.

The `lint` job runs fmt, clippy, the policy JS tests, the non-lib side of the
recipe's `--all-targets` lines (`--bins --test '*'`), and the `ClaudeBinary`
compile-fail doctests. The nightly macOS and Windows lanes run
`scripts/ci/nextest-native.sh`: native source compare, plan, preflight, run
record and audit on one worker. They plan the non-PG lib tests that
`scripts/ci/pg_test_classifier.py` selects, then run the non-lib targets with
`cargo test` and the non-Postgres skips.

The macOS-only `herdr_loaded_image_macos_` tests compile on no PR runner.
`scripts/ci/herdr_loaded_image_macos_ids.txt` lists them and a static test pins it
to the macOS source inventory. A local Mac run checks that exactly those ran:
`python3 scripts/run_test_lane.py --lane macos-loaded-image --expected-ids scripts/ci/herdr_loaded_image_macos_ids.txt -- env -u AGENTDESK_ROOT_DIR cargo test --lib herdr_loaded_image_macos_ -- --test-threads=1`.

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
default-feature lib tests through nextest and the other targets with `cargo test`.
`ci-windows-cache-warm.yml` `windows_cache_warm` builds the same Windows compile
when a `main` push changes `Cargo.lock`, `Cargo.toml`, `rust-toolchain.toml` or
the cargo config, and once a day inside the 7-day cache eviction window
(`cargo check --workspace --all-targets` and `cargo test --lib --no-run`, no
tests run). It saves the registry and sccache entries the PR Windows jobs
restore. A day without a warm only slows those jobs; it never decides them. It
is advisory (`continue-on-error`) and not a required context;
`scripts/check-ci-runner-hardening.sh` keeps its env, setup steps and
rust-cache inputs equal to the PR Windows jobs so both hash to the same keys,
and rejects a second warm job in `ci-main.yml`.

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
