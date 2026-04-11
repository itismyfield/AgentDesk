# High-Risk Recovery Lane

고위험 회귀는 개별 함수 단위보다 상태 전이, 재시작, outbox 전달 경계, 지연된 worker 복구에서 더 자주 발생한다. 이 문서는 해당 영역을 `unit / state-transition integration / failure-recovery` 3계층으로 고정하고, 항상 실행되는 recovery lane과 남은 테스트 공백을 기록한다.

## Layer Model

| Layer | Responsibility | Primary code path | Stable command |
| --- | --- | --- | --- |
| `unit` | 파일 단위 직렬화/저장 규약, mailbox state, handoff roundtrip | `src/services/discord/inflight.rs`, `src/services/discord/handoff.rs`, `src/services/discord/channel_mailbox.rs` | 모듈별 `cargo test --bin agentdesk <filter>` |
| `state-transition integration` | DB + policy engine + dispatch 상태 전이 | `src/integration_tests.rs` | 기본 gate: `cargo test --all-targets` |
| `failure-recovery` | restart / reconcile / outbox delivery / delayed-worker recovery 경계 | `src/integration_tests/tests/high_risk_recovery.rs` | `cargo test --bin agentdesk high_risk_recovery::` |

## Recovery Lane Commands

- 전체 recovery gate: `cargo test --bin agentdesk high_risk_recovery::`
- restart / boot reconcile: `cargo test --bin agentdesk high_risk_recovery::failure_recovery::`
- outbox delivery boundary: `cargo test --bin agentdesk high_risk_recovery::outbox_boundary::`
- delayed worker / watchdog: `cargo test --bin agentdesk high_risk_recovery::delayed_worker::`

## Curated Scenarios

| Module filter | Representative tests | Why always-on |
| --- | --- | --- |
| `high_risk_recovery::failure_recovery::` | `scenario_3_restart_recovery_reconciles_broken_state`, `scenario_251_boot_reconcile_backfills_missing_notify_outbox`, `scenario_251_boot_reconcile_refires_missing_review_dispatch` | 부팅 직후 reconcile이 깨진 review pointer, 누락 outbox, 누락 review dispatch를 복구하는지 확인 |
| `high_risk_recovery::outbox_boundary::` | `scenario_160_1_outbox_batch_delivers_exactly_once`, `scenario_160_2_recovery_fallback_completes_dispatch`, `scenario_160_4_outbox_processes_all_entries_including_duplicates` | notify exactly-once, fallback completion, duplicate delivery 경계를 고정 |
| `high_risk_recovery::delayed_worker::` | `scenario_421_deadlock_recent_output_extends_watchdog`, `scenario_421_deadlock_stale_output_only_marks_suspected_deadlock`, `scenario_421_long_turn_alerts_start_at_30_minutes` | worker 지연과 최근 출력 유무에 따라 watchdog 연장/의심/알림 단계가 올바르게 분기되는지 확인 |

## P0 Coverage Inventory

| TEST_PLAN bucket | Existing coverage (code-backed) | Missing / not yet explicit |
| --- | --- | --- |
| `Restart During Active Turn` | `src/integration_tests/tests/high_risk_recovery.rs`: `scenario_3_restart_recovery_reconciles_broken_state`; `src/integration_tests/tests/high_risk_recovery.rs`: `scenario_421_deadlock_recent_output_extends_watchdog`; `src/services/discord/channel_mailbox.rs`: `recovery_kickoff_marks_recovery_until_finish_turn` | `restart_during_turn_saves_inflight_state`, `restart_recovery_resumes_completed_turn`, `restart_recovery_reattaches_watcher_if_tmux_alive`, `restart_generation_gating_skips_old_state`, `restart_pending_drains_all_turns_first`, `restart_report_saved_on_graceful_shutdown`, `restart_deferred_until_active_turn_completes` |
| `Inflight State Lifecycle` | `src/services/discord/inflight.rs`: `test_save_and_load_inflight_state`, `latest_request_owner_user_id_prefers_most_recent_state_across_providers`; `src/integration_tests/tests/high_risk_recovery.rs`: `scenario_421_deadlock_recent_output_extends_watchdog` | `inflight_save_atomic_write`, `inflight_stale_cleanup_over_5min`, `inflight_malformed_json_graceful_skip`, `inflight_provider_mismatch_skip` |
| `Handoff State` | `src/services/discord/handoff.rs`: `test_save_and_load_handoff`; `src/services/discord/recovery.rs`: `missing_session_recovery_saves_handoff_for_followup_turn`; `src/services/discord/mod.rs`: `handoff_routing_guard_rejects_wrong_agent_settings` | `handoff_dedup_prevents_double_execution`, `handoff_ttl_10min_auto_cleanup` |
| `Turn Lifecycle` | `src/services/discord/channel_mailbox.rs`: `cancel_active_turn_marks_token_without_clearing_turn_state`, `recovery_kickoff_marks_recovery_until_finish_turn`; `src/integration_tests/tests/high_risk_recovery.rs`: `scenario_421_deadlock_recent_output_extends_watchdog`, `scenario_421_long_turn_alerts_start_at_30_minutes`; `src/services/discord/mod.rs`: `recovery_known_message_ids_include_active_turn_message` | `turn_creates_placeholder_in_discord`, `turn_edits_placeholder_with_final_response`, `turn_increments_global_active_counter`, `turn_decrements_counter_on_completion` |

## Notes

- `cargo test --all-targets`는 여전히 전체 회귀 gate다. recovery lane은 이를 대체하지 않고, restart/reconcile/outbox 계열을 별도 required job으로 승격한다.
- inventory는 현재 "existing vs missing"을 분리해 기록한다. missing 항목은 lane 밖으로 숨기지 않고 문서에 남겨 다음 테스트 투자 우선순위를 고정한다.
