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
    # #4878: keep the generated queue docs on the canonical thread-group contract.
    cargo test --lib server::routes::docs::inventory::endpoints::part_0 -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::task_completion_v1::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib delivery_lease_key -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib server::routes::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib formatting -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib delivery_record -- --skip _pg --skip pg_ --skip postgres
    # #4911: a winner-bound current-generation frontier must never delete a losing anchor.
    cargo test --lib services::discord::tmux::placeholder_suppression::evidence::tests -- --skip _pg --skip pg_ --skip postgres
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::watcher_lifecycle::tests::tests::turn_starts_reuse_healthy_runtime_path_incumbent_after_handoff -- --exact
    cargo test --lib server::claude_oauth_usage_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib tui_task_card::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib server::routes::message_outbox::tests -- --skip _pg --skip pg_ --skip postgres
    # Keep the non-PostgreSQL unit tests covered after outbox_claiming's PG split.
    cargo test --lib services::dispatches::outbox_claiming::tests -- --skip _pg --skip pg_ --skip postgres
    # Keep the non-PostgreSQL unit tests covered after delivery guard's PG split.
    cargo test --lib services::dispatches::discord_delivery::guard::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib discord_thread_create -- --test-threads=1
    # #4599: queue reaction fallback and persisted-v1 promotion contracts.
    cargo test --lib reaction_control::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib intake_queue_transaction::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib pending_reaction_failure_adapter_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib intake_dispatch_invariant_queued_entrypoints_promote_markers -- --skip _pg --skip pg_ --skip postgres
    # #5040: telemetry-only owner planning cannot fence an unopted live-local channel.
    cargo test --lib services::discord::router::intake_dispatch::tests::telemetry_only_unopted -- --skip _pg --skip pg_ --skip postgres
    # #4788: raw attachment preparation must remain behind local admission.
    cargo test --lib attachment -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib mailbox_reaction_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib queue_marker::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib queue_status_presentation::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib status_panel_singleton_store -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib busy_followup_retry_store -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::claude_tui::input::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::tmux_common::sentinel_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::turn_bridge::followup_requeue::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::turn_bridge::terminal_outcome_delivery::busy_followup_retry::tests -- --skip _pg --skip pg_ --skip postgres
    # #4259/#5014: keep exact bridge-entry and destructive-commit modules in a curated lane.
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::destructive_commit::tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::save_store::bridge_entry_guard_tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::save_store::identity_gate::bridge_entry::tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::inflight::save_store::identity_gate::claude_e_stamp::tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::bridge_entry_persist::tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::current_message_anchor::tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::guards::tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_bridge::stream_loop::tool_arms::authority_tests -- --test-threads=1
    cargo test --lib services::discord::gateway::tests -- --skip _pg --skip pg_ --skip postgres
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::gateway::outbound_messages::classified_edit_tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::intake_dispatch::queued::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::router::message_handler::intake_turn::placeholder_handoff::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::completion_admission::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::completion_admission_actor::tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::cleanup::tests::late_already_finalized_cleanup_releases_mailbox_and_rearms_once_4906 -- --exact --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::turn_finalizer::cleanup::tests::mailbox_release_backstop_coalesces_duplicate_arms_and_eventually_fires_4906 -- --exact --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::tmux_watcher::placeholder_reclaim::redrive_reclaim_e2e_tests::live_tmux_redrive_reclaim_cycle_terminates_4299 -- --exact --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::recovery_engine::runtime::reregister_ledger_reseed_tests -- --test-threads=1
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::placeholder_sweeper::abandon_guard::tests -- --test-threads=1
    # #4892: keep the live panel and spinner-merged latest-tool contracts in the retained lane.
    env -u AGENTDESK_ROOT_DIR cargo test --lib placeholder_live_events -- --skip _pg --skip pg_ --skip postgres
    env -u AGENTDESK_ROOT_DIR cargo test --lib single_message_panel::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::outbound::serenity_reference::tests::lifecycle_notice_nonce_is_stable_and_semantic_event_scoped -- --exact
    cargo test --lib services::discord::outbound::delivery::tests::v3_referenced_send_preserves_reference_and_dedupes -- --exact
    # #4913 GO-A1: retain canonical Discord identity validation, collision, and observability contracts.
    cargo test --lib canonical_identity::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib session_canonical_identity::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::observability::metrics::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::observability::turn_lifecycle::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::observability::recovery_audit::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib cli::args::tests::legacy_queue_help_directs_users_to_query_without_changing_compatibility_contract
    cargo test --all-targets transition -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    cargo test --all-targets auto_queue -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets cancel -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets review_decision -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets stall_recovery -- --skip _pg --skip pg_ --skip postgres
    cargo test --all-targets routines -- --skip _pg --skip pg_ --skip postgres
    # Run health first so a fail-fast relay_recovery failure cannot hide it.
    python3 scripts/ci-timeout.py 900 env -u AGENTDESK_ROOT_DIR cargo test --lib health -- --skip _pg --skip pg_ --skip postgres
    env -u AGENTDESK_ROOT_DIR cargo test --lib relay_recovery -- --skip _pg --skip pg_ --skip postgres
    # #4874: keep the local-model durable-queue wake production E2E fully selected.
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tui_prompt_relay::local_model_queue_wake_e2e -- --skip _pg --skip pg_ --skip postgres --test-threads=1
    # #4875: keep the Claude catalog and picker test modules fully selected.
    cargo test --lib services::discord::model_catalog -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::commands::model_ui::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::runtime_bootstrap::shutdown::lifecycle_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test invariant --all-targets -- --skip _pg --skip pg_ --skip postgres
    # `ClaudeBinary` capability invariants are compile-fail doctests in src/lib.rs.
    # Filter the real rustdoc harness to this public capability contract.
    cargo test --doc ClaudeBinary

# PostgreSQL tests belong in the library harness. Integration and doctest targets
# are intentionally excluded; add a separate PG lane command if either gains PG coverage.
test-postgres:
    cargo test --lib -- _pg pg_ postgres --nocapture --test-threads=1

check: fmt-check lint cargo-check test
