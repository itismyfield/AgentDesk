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

# #5071 T4-B2a (4987 blocker B1'): the RUST half of the Rust<->Python canonical
# mutation runner. Each declared mutation edits `obligation.rs` alone and must
# turn the golden-corpus test red; the file is restored either way.
#
# It is not in `ci-script-checks.sh` because it rebuilds the crate once per
# mutation. What CI holds instead is (a) the corpus test itself in the
# `test-non-pg` obligation lane, so ANY Rust-only change to the obligation rule
# is already red there, and (b) `tests.test_reachability_canonical_equivalence`,
# which fails if a declared mutation stops anchoring on real source and would
# therefore be silently skipped. Run this before changing the framing rules.
reachability-mutation-runner:
    python3 scripts/check_reachability_canonical_equivalence.py --with-rust

# Stage 1 keeps the existing CI-safe subset. The broad non-PG sweep currently
# fails legacy/full integration route tests; see docs/ci/rust-quality-gates.md.
test-non-pg:
    # Typed KV bulk deletion must keep its array-bound payload contract covered.
    cargo test --lib engine::ops::kv_ops::tests -- --skip _pg --skip pg_ --skip postgres
    # #4878: keep the generated queue docs on the canonical thread-group contract.
    cargo test --lib server::routes::docs::inventory::endpoints::part_0 -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::task_completion_v1::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib source_registry -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib task_notification -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib delivery_lease_key -- --skip _pg --skip pg_ --skip postgres
    # #5071 T0: keep the delivery-writer/terminal-fold contract seam in a curated lane.
    cargo test --lib services::discord::session_relay_sink::delivery_orchestration_tests -- --skip _pg --skip pg_ --skip postgres
    # #5071 T4-B1 (4987 S1): the reachability library is inactive, so these are
    # its ONLY execution. verdict pins the polarity (TransportUnknown is neither
    # health nor a redelivery warrant); discovery pins fail-closed resolution and
    # same-size/different-inode divergence; tail pins the 1 MiB per-tick cap and
    # cursor identity revalidation.
    cargo test --lib services::discord::health::reachability::verdict::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::health::reachability::discovery::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::health::reachability::tail::tests -- --skip _pg --skip pg_ --skip postgres
    # #5071 T4-B2a (4987 S1, second half + blocker B1'): canonical obligation
    # framing. This lane carries the RUST half of the Rust<->Python byte-equal
    # equivalence, so a Rust-only change to the obligation rule dies here in
    # ordinary CI; the Python half is in `scripts/ci-script-checks.sh`.
    cargo test --lib services::discord::health::reachability::obligation::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib server::routes::e2e_control::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib formatting -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib delivery_record -- --skip _pg --skip pg_ --skip postgres
    # #5191: the catch-up recovery dedup must cover the dequeue→claim window,
    # including a merged head's absorbed source ids, without letting an orphaned
    # reservation suppress a genuinely unanswered message.
    cargo test --lib services::discord::recovery_known_ids::recovery_known_message_ids_tests -- --skip _pg --skip pg_ --skip postgres
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
    # #5176: cancel success is mailbox foreground release; both directions of the
    # release guard (free the zombie / never touch a live turn) stay in a lane.
    cargo test --lib services::discord::zombie_foreground_release::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib queue_marker::tests -- --skip _pg --skip pg_ --skip postgres
    # #5035: contract G is the only permission to destroy a shared queued card.
    cargo test --lib services::discord::placeholder_controller::queued_card_gate::tests -- --skip _pg --skip pg_ --skip postgres
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
    # #5175: soft-terminal delivery authority must stay bound to the turn's own
    # pre-relay inflight row. When this seam regressed to the pre-turn snapshot,
    # neither the sink nor the watcher posted the body and the delivery frontier
    # froze silently, so keep it in a curated lane.
    env -u AGENTDESK_ROOT_DIR cargo test --lib services::discord::tmux::tmux_watcher::terminal_relay_plan::soft_terminal_direct_send_authority_tests -- --skip _pg --skip pg_ --skip postgres --test-threads=1
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
    # #5188: Claude session rotation (`/clear`). All three modules guard a silent
    # permanent delivery loss, so keep them fully selected: the planner must drain
    # the frozen transcript BEFORE following the rotation (losing that is data
    # loss), a payload-adopted binding must outrank the stale launch script, and a
    # session-resetting slash control must not mint an unfinalizable inflight.
    cargo test --lib services::discord::tui_prompt_relay::session_rotation_settle::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::discord::tui_prompt_relay::injected_prompt_policy::session_resetting_lifecycle_tests -- --skip _pg --skip pg_ --skip postgres
    cargo test --lib services::tui_prompt_dedupe::session_rotation::tests -- --skip _pg --skip pg_ --skip postgres
    cargo test invariant --all-targets -- --skip _pg --skip pg_ --skip postgres
    # `ClaudeBinary` capability invariants are compile-fail doctests in src/lib.rs.
    # Filter the real rustdoc harness to this public capability contract.
    cargo test --doc ClaudeBinary

# PostgreSQL tests belong in the library harness. Integration and doctest targets
# are intentionally excluded; add a separate PG lane command if either gains PG coverage.
# The fixture server must be explicit; PG* variables alone are not authorization.
test-postgres:
    @test -n "${POSTGRES_TEST_DATABASE_URL_BASE:-}" || (echo "POSTGRES_TEST_DATABASE_URL_BASE must name the dedicated PostgreSQL test server with an explicit host and port" >&2; exit 1)
    cargo test --lib -- _pg pg_ postgres --nocapture --test-threads=1
    # #5356 S0: the engine wrapper's PG regression module path
    # (`engine::ops::auto_queue_ops::tests`) carries no pg-name marker, so the
    # name-filtered invocation above cannot fully select it for the test-lane
    # coverage ratchet. Select the module explicitly instead of adding it to
    # the shrink-only debt baseline.
    cargo test --lib engine::ops::auto_queue_ops::tests -- --nocapture --test-threads=1
    # #5071 T2-W S-W1: the dispatch-stamp PG regressions live in a module
    # named `tests` (hardening-audit region naming), so the path carries no
    # pg-name marker. Select the module explicitly for the coverage ratchet.
    cargo test --lib db::intake_outbox_dispatch_stamp::tests -- --nocapture --test-threads=1
    # #5356 S1: the choke-gate PG regressions live in modules named `tests`
    # (the hardening audit only recognizes that module name as a test
    # region), so their paths carry no pg-name marker either. Select each
    # module explicitly for the same coverage-ratchet reason as above.
    cargo test --lib db::auto_queue::entries::tests -- --nocapture --test-threads=1
    # #5356 S2: the cross-path advisory-order regression is PG-only and also
    # lives in a plain `tests` module, so select that module explicitly.
    cargo test --lib services::auto_queue::route::command::tests -- --nocapture --test-threads=1
    cargo test --lib services::auto_queue::route::fsm::tests -- --nocapture --test-threads=1
    cargo test --lib services::auto_queue::route::phase_gate::tests -- --nocapture --test-threads=1
    # #5356 S3: the ownership suite is a PostgreSQL-only out-of-line module;
    # keep its full module selected explicitly for the coverage ratchet.
    cargo test --lib db::dispatched_sessions::tests -- --nocapture --test-threads=1
    # #5071 T2-W S-W2: settlement regressions use the hardening-audit `tests`
    # module name, so select both no-marker modules explicitly in the PG lane.
    cargo test --lib db::intake_outbox_delivery_proof::tests -- --nocapture --test-threads=1
    cargo test --lib services::discord::turn_bridge::intake_settlement::tests -- --nocapture --test-threads=1
    cargo test --lib services::discord::runtime_bootstrap::intake_delivery_sweep::tests -- --nocapture --test-threads=1

check: fmt-check lint cargo-check test
