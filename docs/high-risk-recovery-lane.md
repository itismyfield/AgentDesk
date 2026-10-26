# High-Risk Recovery Lane

고위험 회귀는 개별 함수 단위보다 상태 전이, 재시작, outbox 전달 경계, 지연된 worker 복구에서 더 자주 발생한다. 이 문서는 해당 영역을 `unit / state-transition integration / failure-recovery` 3계층으로 고정하고, 항상 실행되는 recovery lane과 남은 테스트 공백을 기록한다.

> **#3035 Phase 1 메모:** 레거시 SQLite 기반 `src/integration_tests.rs` (삭제됨) 및 `src/integration_tests/tests/high_risk_recovery.rs` (삭제됨) 하네스(legacy-sqlite-tests 게이트, 기본 빌드 미컴파일)는 제거되었다. PG-only 회귀 보호는 `src/high_risk_recovery.rs` 로 이전되며, 아래 `scenario_*` 시나리오 표는 제거된 레거시 하네스 기준 기록(historical)이다. PG 스위트로의 시나리오 재매핑은 후속 Phase 에서 진행한다.

## Layer Model

| Layer | Responsibility | Primary code path | Stable command |
| --- | --- | --- | --- |
| `unit` | 파일 단위 직렬화/저장 규약, mailbox state | `src/services/discord/inflight.rs`, `src/services/turn_orchestrator.rs` | 모듈별 `cargo test --lib <module::path>` (예: `cargo test --lib services::discord::inflight::`) |
| `state-transition integration` | DB + policy engine + dispatch 상태 전이 | PostgreSQL이 필요한 lib 시험(`scripts/ci/pg_test_classifier.py`가 분류) | `just test-postgres` (lib 시험 전체를 PG 시험 서버에 대고 실행, `POSTGRES_TEST_DATABASE_URL_BASE` 필요) |
| `failure-recovery` | restart / reconcile / outbox delivery / delayed-worker recovery 경계 | `src/high_risk_recovery.rs` | `cargo test --lib high_risk_recovery::` |

## Recovery Lane Commands

- 전체 recovery gate: `cargo test --lib high_risk_recovery::` (CI lane의 나머지 선택자는 아래 Notes 참고)
- restart / boot reconcile: `cargo test --lib -- high_risk_recovery::boot_reconcile high_risk_recovery::restart_recovery`
- outbox delivery boundary: `cargo test --lib -- high_risk_recovery::runtime_reconcile_auto_queue_pending_delivery_orphans_requeues_notify_outbox_pg services::dispatches::outbox_queue:: services::dispatches::outbox_claiming::`
- delayed worker / watchdog: `cargo test --lib -- services::discord::health::recovery::self_watchdog services::hang_forensics::tests services::dispatch_watchdog::tests`
- 테스트는 모두 lib에 있다. `--bin agentdesk`로 돌리면 0개가 돌고 성공으로 끝나므로 쓰지 않는다. 실행 수가 0이면(`running 0 tests`) 통과가 아니라 실패로 본다.

## Curated Scenarios

| Module filter | Representative tests | Why always-on |
| --- | --- | --- |
| `high_risk_recovery::failure_recovery::` | `scenario_3_restart_recovery_reconciles_broken_state`, `scenario_251_boot_reconcile_backfills_missing_notify_outbox`, `scenario_251_boot_reconcile_refires_missing_review_dispatch` | 부팅 직후 reconcile이 깨진 review pointer, 누락 outbox, 누락 review dispatch를 복구하는지 확인 |
| `high_risk_recovery::outbox_boundary::` | `scenario_160_1_outbox_batch_delivers_exactly_once`, `scenario_160_2_recovery_fallback_completes_dispatch`, `scenario_160_4_outbox_processes_all_entries_including_duplicates` | notify exactly-once, fallback completion, duplicate delivery 경계를 고정 |
| `high_risk_recovery::delayed_worker::` | `scenario_421_deadlock_recent_output_extends_watchdog`, `scenario_421_deadlock_stale_output_only_marks_suspected_deadlock`, `scenario_421_long_turn_alerts_start_at_30_minutes` | worker 지연과 최근 출력 유무에 따라 watchdog 연장/의심/알림 단계가 올바르게 분기되는지 확인 |

## P0 Coverage Inventory

| TEST_PLAN bucket | Existing coverage (code-backed) | Missing / not yet explicit |
| --- | --- | --- |
| `Restart During Active Turn` | `src/high_risk_recovery.rs`: `restart_recovery_does_not_repost_prior_typed_dispatch_delivery_pg`, `boot_reconcile_pg_resets_stale_runtime_rows`; `src/services/turn_orchestrator/recovery_kickoff_tests.rs`: `empty_slot_kickoff_installs_and_releases_its_own_episode` | `restart_during_turn_saves_inflight_state`, `restart_recovery_resumes_completed_turn`, `restart_recovery_reattaches_watcher_if_tmux_alive`, `restart_generation_gating_skips_old_state`, `restart_pending_drains_all_turns_first`, `restart_report_saved_on_graceful_shutdown`, `restart_deferred_until_active_turn_completes` |
| `Inflight State Lifecycle` | `src/services/discord/inflight/host_locator.rs`: `known_locator_survives_save_and_load`; `src/services/discord/inflight/removal.rs`: `boot_reaper_retires_foreign_malformed_and_out_of_window_rows` | `inflight_save_atomic_write`, `inflight_stale_cleanup_over_5min`, `inflight_malformed_json_graceful_skip`, `inflight_provider_mismatch_skip` |
| `Handoff State` | 없음. durable handoff 기록은 폐기됐다(#2613) | `handoff_dedup_prevents_double_execution`, `handoff_ttl_10min_auto_cleanup` |
| `Turn Lifecycle` | `src/services/turn_orchestrator.rs`: `cancel_active_turn_with_reason_writes_label_and_flips_atomically`; `src/services/turn_orchestrator/recovery_kickoff_tests.rs`: `kickoff_then_try_start_preserves_the_recovery_claim`; `src/services/discord/recovery_known_ids.rs`: `active_turn_outranks_a_stale_queue_entry_for_the_same_id` | `turn_creates_placeholder_in_discord`, `turn_edits_placeholder_with_final_response`, `turn_increments_global_active_counter`, `turn_decrements_counter_on_completion` |

## Release gate 축 매핑

`#1011` release gate hardening 감사로그에 따라 recovery lane 은 아래 4 축 중 하나라도 0 개 시나리오가 되면 release gate 자격을 잃는다. [`docs/ci/release-gates.md`](./ci/release-gates.md#3-high-risk-recovery-lane-test-axes) 의 요약과 동기화한다.

| Axis | Module filter | Anchored scenarios |
| --- | --- | --- |
| Live turn 보존 | `high_risk_recovery::failure_recovery::` | `scenario_3_restart_recovery_reconciles_broken_state`, `scenario_667_restart_recovery_reconciles_duplicate_review_dispatches` |
| Watcher reattach | `high_risk_recovery::delayed_worker::` | `scenario_421_deadlock_recent_output_extends_watchdog`, `scenario_421_deadlock_stale_output_only_marks_suspected_deadlock`, `scenario_421_long_turn_alerts_start_at_30_minutes` |
| Dispatch/outbox idempotency | `high_risk_recovery::outbox_boundary::` | `scenario_160_1_outbox_batch_delivers_exactly_once`, `scenario_160_2_recovery_fallback_completes_dispatch`, `scenario_160_4_outbox_processes_all_entries_including_duplicates`, `scenario_160_6_notify_success_keeps_completed_dispatch_terminal` |
| Queue loss 방지 | `high_risk_recovery::failure_recovery::` + `idle_session_cleanup::` | `scenario_251_boot_reconcile_backfills_missing_notify_outbox`, `scenario_251_boot_reconcile_refires_missing_review_dispatch`, `scenario_251_boot_reconcile_resets_broken_auto_queue_entries`, `scenario_492_idle_session_with_active_dispatch_uses_180_minute_safety_ttl` |

새 시나리오는 위 4 축 중 하나에 귀속시키고 표 + release-gates.md 를 동시 갱신.

## Notes

- PR은 모든 lib 시험을 nextest 한 번으로 돌리고 `High-risk recovery` context는 그 실행의 판정이다. 이 lane의 선택자는 `scripts/ci/targeted-test-selectors.txt`의 `High-risk recovery` 절에 두며, 계획된 시험과 맞지 않으면 build·evidence job이 실패한다. main은 아직 별도 `high-risk-recovery` job을 돌린다.
- inventory는 현재 "existing vs missing"을 분리해 기록한다. missing 항목은 lane 밖으로 숨기지 않고 문서에 남겨 다음 테스트 투자 우선순위를 고정한다.
