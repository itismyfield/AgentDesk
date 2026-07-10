set shell := ["bash", "-euo", "pipefail", "-c"]

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

# `-W clippy::all` reports current warning debt; only `[lints.clippy]` deny
# entries in Cargo.toml are hard gates for this staged check.
lint:
    cargo clippy --workspace --all-targets --all-features -- -W clippy::all

# Expected-failing zero-warning target; see docs/ci/rust-quality-gates.md.
lint-strict:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

cargo-check:
    cargo check --workspace --all-features --all-targets

test: test-non-pg

# Stage 1 keeps the existing CI-safe subset. The broad non-PG sweep currently
# fails legacy/full integration route tests; see docs/ci/rust-quality-gates.md.
test-non-pg:
    cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib server::routes::message_outbox::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets transition -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    cargo test --all-targets auto_queue -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets cancel -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets review_decision -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets stall_recovery -- --skip _pg --skip pg_ --skip postgres
    env -u AGENTDESK_ROOT_DIR cargo test --lib relay_recovery -- --skip _pg --skip pg_ --skip postgres
    python3 scripts/ci-timeout.py 900 env -u AGENTDESK_ROOT_DIR cargo test --lib health -- --skip _pg --skip pg_ --skip postgres
    cargo test invariant --all-targets -- --skip _pg --skip pg_ --skip postgres

test-postgres:
    cargo test -- _pg pg_ postgres --nocapture --test-threads=1

check: fmt-check lint cargo-check test
