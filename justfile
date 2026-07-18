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

# Active Claude usage compact-trigger contract.
test-active-usage-4631:
    cargo test --lib claude_compact_trigger::tests
    cargo test --lib assistant_usage_emits_complete_active_snapshot_before_done

# Stage 1 keeps the existing CI-safe subset. The broad non-PG sweep currently
# fails legacy/full integration route tests; see docs/ci/rust-quality-gates.md.
test-non-pg:
    cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib tui_task_card::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib server::routes::message_outbox::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib discord_thread_create -- --test-threads=1
    # #4599: queue reaction fallback and persisted-v1 promotion contracts.
    cargo test --lib reaction_control::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib intake_queue_transaction::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib pending_reaction_failure_adapter_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib intake_dispatch_invariant_queued_entrypoints_promote_markers -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib mailbox_reaction_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib queue_marker::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib queue_status_presentation::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::outbound::serenity_reference::tests::lifecycle_notice_nonce_is_stable_and_semantic_event_scoped -- --exact
    cargo test --lib services::discord::outbound::delivery::tests::v3_referenced_send_preserves_reference_and_dedupes -- --exact
    cargo test --lib cli::args::tests::legacy_queue_help_directs_users_to_query_without_changing_compatibility_contract
    cargo test --all-targets transition -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    cargo test --all-targets auto_queue -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets cancel -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets review_decision -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets stall_recovery -- --skip _pg --skip pg_ --skip postgres
    # Run health first so a fail-fast relay_recovery failure cannot hide it.
    python3 scripts/ci-timeout.py 900 env -u AGENTDESK_ROOT_DIR cargo test --lib health -- --skip _pg --skip pg_ --skip postgres
    env -u AGENTDESK_ROOT_DIR cargo test --lib relay_recovery -- --skip _pg --skip pg_ --skip postgres
    cargo test invariant --all-targets -- --skip _pg --skip pg_ --skip postgres
    # `ClaudeBinary` capability invariants are compile-fail doctests in src/lib.rs.
    # Filter the real rustdoc harness to this public capability contract.
    cargo test --doc ClaudeBinary

test-postgres:
    cargo test -- _pg pg_ postgres --nocapture --test-threads=1

check: fmt-check lint cargo-check test
