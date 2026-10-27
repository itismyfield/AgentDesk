set shell := ["bash", "-euo", "pipefail", "-c"]

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# `-W clippy::all` lowers Cargo group-member denies; crate attributes in src/lib.rs and
# src/main.rs keep the risk lints and the correctness group denied. See docs/ci/rust-quality-gates.md.
lint:
    cargo clippy --workspace --all-targets --all-features -- -W clippy::all

# Expected-failing zero-warning target; see docs/ci/rust-quality-gates.md.
lint-strict:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

cargo-check:
    cargo check --workspace --all-features --all-targets

# The CI nextest run: every lib test, PostgreSQL ones included.
test:
    cargo nextest run --lib --profile ci

# Active Claude usage compact-trigger contract.
test-active-usage-4631:
    cargo test --lib claude_compact_trigger::tests
    cargo test --lib assistant_usage_emits_complete_active_snapshot_before_done

# Mutate the Rust framing rules against the golden corpus.
reachability-mutation-runner:
    python3 scripts/check_reachability_canonical_equivalence.py --with-rust

# Lib tests the source PG classifier leaves out, through the native CI run (plan, run, audit),
# then every non-lib test and the ClaudeBinary compile-fail doctests.
test-non-pg:
    rm -rf target/nextest-local target/nextest/ci
    RUNNER_TEMP="$PWD/target/nextest-local" GITHUB_WORKSPACE="$PWD" bash scripts/ci/nextest-native.sh \
        "$(uname -s | sed 's/^Darwin$/macos/; s/^Linux$/linux/')" local-non-pg pg-classifier
    cargo test --bins --test '*' -- --skip _pg --skip pg_ --skip postgres
    cargo test --doc ClaudeBinary

# Every lib test against the dedicated PostgreSQL test server, which must be explicit;
# PG* variables alone are not authorization.
test-postgres:
    @test -n "${POSTGRES_TEST_DATABASE_URL_BASE:-}" || (echo "POSTGRES_TEST_DATABASE_URL_BASE must name the dedicated PostgreSQL test server with an explicit host and port" >&2; exit 1)
    cargo nextest run --lib

check: fmt-check lint cargo-check test
