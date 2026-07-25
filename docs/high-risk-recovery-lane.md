# High-Risk Recovery Lane

고위험 회귀는 개별 함수 단위보다 상태 전이, 재시작, outbox 전달 경계에서 더 자주 발생한다. 이 문서는 현재 required recovery lane이 실제로 실행하는 PostgreSQL 기반 테스트와 그 책임을 고정한다.

## Layer Model

| Layer | Responsibility | Primary code path | Stable command |
| --- | --- | --- | --- |
| `unit` | 파일 단위 직렬화/저장 규약, mailbox state, handoff roundtrip | `src/services/discord/inflight.rs`, `src/services/discord/handoff.rs`, `src/services/discord/channel_mailbox.rs` | 모듈별 `cargo test --lib <filter>` |
| `state-transition integration` | DB + policy engine + dispatch 상태 전이 | `src/integration_tests.rs` 및 각 서비스의 `#[cfg(test)]` 모듈 | 기본 gate: `cargo test --all-targets` |
| `failure-recovery` | PostgreSQL restart / reconcile / outbox delivery 경계 | `src/high_risk_recovery.rs` | `cargo test --lib high_risk_recovery:: -- --test-threads=1` |

## Recovery Lane Commands

- 전체 recovery gate: `cargo test --lib high_risk_recovery:: -- --test-threads=1`
- 개별 재현은 아래 full test name에 `--exact`를 붙인다.

## Current Recovery Tests

| Full test name | Guarded recovery boundary |
| --- | --- |
| `high_risk_recovery::boot_reconcile_pg_resets_stale_runtime_rows` | 부팅 reconcile이 stale dispatch runtime row를 안전한 상태로 되돌리는지 검증 |
| `high_risk_recovery::restart_recovery_does_not_repost_prior_typed_dispatch_delivery` | 재시작 후 이미 기록된 typed dispatch delivery를 다시 게시하지 않는지 검증 |
| `high_risk_recovery::runtime_reconcile_auto_queue_pending_delivery_orphans_requeues_notify_outbox` | runtime reconcile이 pending-delivery orphan을 notify outbox로 재큐잉하는지 검증 |
| `high_risk_recovery::boot_reconcile_pg_refires_missing_review_dispatch` | 부팅 reconcile이 누락된 review dispatch를 다시 생성하는지 검증 |
| `high_risk_recovery::completed_queue_review_drift_reconcile_promotes_only_stale_done_entries` | completed queue drift reconcile이 stale done entry만 승격하는지 검증 |

## Release gate 축 매핑

[`docs/ci/release-gates.md`](./ci/release-gates.md#3-high-risk-recovery-lane-test-axes)의 요약과 동기화한다. 현재 5개 테스트가 실제 보장하는 축만 열거하며, 제거된 legacy `scenario_*` 이름을 coverage로 계산하지 않는다.

| Axis | Anchored tests |
| --- | --- |
| Restart/runtime state repair | `boot_reconcile_pg_resets_stale_runtime_rows`, `completed_queue_review_drift_reconcile_promotes_only_stale_done_entries` |
| Dispatch delivery idempotency | `restart_recovery_does_not_repost_prior_typed_dispatch_delivery` |
| Dispatch/outbox loss prevention | `runtime_reconcile_auto_queue_pending_delivery_orphans_requeues_notify_outbox`, `boot_reconcile_pg_refires_missing_review_dispatch` |

새 시나리오는 위 축 중 하나에 귀속시키고 이 표와 `release-gates.md`를 동시에 갱신한다. watcher reattach나 delayed-worker watchdog처럼 현재 이 파일에 없는 경계는 다른 테스트 모듈이 소유하며, 이 lane의 5개 테스트가 대신 보장한다고 문서화하지 않는다.

## Notes

- `cargo test --all-targets`는 여전히 전체 회귀 gate다. recovery lane은 이를 대체하지 않고 PostgreSQL restart/reconcile/outbox 경계를 별도 required job으로 승격한다.
- `src/high_risk_recovery.rs`의 테스트 수와 이름은 `cargo test --lib high_risk_recovery:: -- --list`로 확인한다. 필터가 0개 테스트를 선택하면 lane drift로 취급한다.
