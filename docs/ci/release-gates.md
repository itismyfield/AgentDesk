# Release Candidate CI Gates

이 문서는 AgentDesk `main` 및 PR 레벨에서 release candidate 자격을 보증하는 3개 CI gate를 명시한다. Gate는 "이 job이 red면 릴리즈 후보가 아니다"를 의미하며, 우회 금지(branch protection에서 required로 등록 또는 자동 triage로 동등 효과를 보장).

> 상위 소스-오브-트루스: [`docs/source-of-truth.md`](../source-of-truth.md)
>
> 관련 문서: [`docs/high-risk-recovery-lane.md`](../high-risk-recovery-lane.md)

## 1. 3개 Release Gate

| Gate | ci-main.yml job | ci-pr.yml job | ci-nightly.yml 대응 | 실행 조건 |
| --- | --- | --- | --- | --- |
| **Full tests** | `full_non_pg` (line 56) | `check_fast` + `test_fast` (line 79, 115) | `full_run` | 항상 실행 (path filter 없음). PR lane은 targeted 서브셋으로 replace. |
| **PostgreSQL tests** | `postgres` (line 94) | `test_fast` PG 서비스 (line 115) | `postgres` | 항상 실행. main job은 `_pg` / `pg_` / `postgres` 필터 3회 직렬 실행. |
| **High-risk recovery** | `high-risk-recovery` (line 151) | `high-risk-recovery` (line 173) | `high_risk_recovery_full` | path filter hit 시에만 실행. nightly full job은 무조건. |

### Gate ↔ 실제 커맨드

| Gate | main 커맨드 | 재현 커맨드 (로컬) |
| --- | --- | --- |
| Full tests | `cargo test --all-targets -- --skip _pg --skip pg_ --skip postgres` | 동일 |
| PostgreSQL tests | `cargo test _pg / pg_ / postgres -- --test-threads=1` (3회) | `DATABASE_URL=... cargo test _pg -- --test-threads=1` |
| High-risk recovery | `cargo test --bin agentdesk high_risk_recovery:: -- --test-threads=1` | 동일 |

## 2. Path Filter Policy

### Always-on (필터 없음)

- **Full tests** / **PostgreSQL tests** 은 path filter 없이 `main` push 시 무조건 실행. 이 두 gate는 `changes` job의 outputs에 의존하지 않으며 `if:` 조건 없이 정의.
- 즉, 커밋이 어떤 파일만 건드리든 Full/PG는 실행되고 red면 merge 차단에 준하는 신호다.

### Conditional (`high_risk_recovery` path filter)

`high-risk-recovery` job은 `needs: changes` + `if: needs.changes.outputs.high_risk_recovery == 'true'` 로 실행되며, 필터 대상은 restart/reconcile/outbox/delayed-worker 경로 전체를 포괄한다:

```yaml
high_risk_recovery:
  - '.github/workflows/**'
  - 'policies/auto-queue.js'
  - 'policies/kanban-rules.js'
  - 'policies/timeouts.js'
  - 'policies/timeouts/**'
  - 'policies/lib/**'
  - 'policies/__tests__/**'
  - 'src/db/**'
  - 'src/dispatch/**'
  - 'src/engine/**'
  - 'src/integration_tests.rs'
  - 'src/integration_tests/tests/high_risk_recovery.rs'
  - 'src/kanban.rs'                      # 카드 상태 전이가 reconcile 입력
  - 'src/reconcile.rs'
  - 'src/server/routes/auto_queue.rs'
  - 'src/server/routes/dispatched_sessions.rs'
  - 'src/server/routes/dispatches/**'
  - 'src/services/auto_queue.rs'         # auto_queue 단일 파일 경로
  - 'src/services/auto_queue/**'         # auto_queue 하위 디렉터리
  - 'src/services/discord/**'
  - 'src/services/message_outbox.rs'     # outbox 전달 경계
  - 'src/services/platform/tmux.rs'
  - 'src/services/tmux_common.rs'
```

중요: `src/services/auto_queue.rs` (파일) 과 `src/services/auto_queue/**` (디렉터리) 는 서로 다른 경로다. 둘 다 있어야 auto_queue 변경이 recovery lane을 확실히 트리거한다. 마찬가지로 `src/kanban.rs`, `src/services/message_outbox.rs` 는 recovery 경로에 영향을 주므로 포함한다.

### Generated docs / architecture drift

- `scripts/generate_inventory_docs.py --check` 은 `Script checks` job 마지막 단계에서 하드 블록 (ci-main `scripts` step, ci-pr `scripts` step).
- 이 drift 검증이 red 면 Full/PG/High-risk 와 동일하게 release gate 위반으로 간주한다.

## 3. Resource Contention Policy

`PostgreSQL tests` 와 `High-risk recovery` 는 공유 리소스(동일 Postgres 서비스 컨테이너)를 사용하므로 다음 정책을 조합한다.

### Serial execution

- `postgres` job: `cargo test _pg -- --test-threads=1` / `cargo test pg_ -- --test-threads=1` / `cargo test postgres -- --test-threads=1` — 모두 **단일 스레드** 강제.
- `high-risk-recovery` job: `cargo test --bin agentdesk high_risk_recovery:: -- --test-threads=1` — 동일.
- 이유: PG 테스트는 같은 `postgres` DB 인스턴스 위에서 CREATE/DROP DATABASE 로 격리하므로, parallel 실행 시 테스트 간 lifecycle race 가 재현되는 사고가 #973/#974 에서 확인됨.

### Fixture isolation

- `PgRecoveryTestDatabase::create` 는 test마다 `agentdesk_pg_recovery_<uuid>` 데이터베이스를 신규 생성 → 독립 pool → drop 순으로 정리.
- `crate::db::postgres::lock_test_lifecycle()` lifecycle guard 로 동시 create/drop 직렬화.
- `seed_*` 헬퍼는 in-memory SQLite `test_db()` fixture 를 사용 — PG 가 필요 없는 recovery 시나리오는 PG 서비스와 독립.

### Pool sizing

- Recovery test의 `pg_recovery_test_config` 는 `pool_max = 1` 로 설정. 단일 connection 으로 startup reconcile 이 runtime pool 을 점유하지 않고 completion 되는지 검증 (`scenario_969_pg_boot_reconcile_uses_startup_pool_without_pool_timeout_logs`).

## 4. Triage 분류 규약

`scripts/main-ci-triage.sh` 는 `CI Main` 이 2회 연속 red일 때 test identifier 또는 `job::<name>` 단위로 ci-red 이슈를 생성/갱신한다. Release gate 별 분류 계약:

| 실패 형태 | identifier 패턴 | 재현 커맨드 (issue body 에 기록) | Follow-up owner label |
| --- | --- | --- | --- |
| Full tests 개별 케이스 red | `<mod>::<test>` (e.g. `pipeline::tests::…`) | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| PG tests 개별 케이스 red | `<mod>::…_pg_…` / `postgres_…` | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| High-risk recovery job 자체 red (로그에서 test id 추출 실패) | `job::High-risk recovery` | `_job-level failure; see failing workflow job_` | `agent:project-agentdesk` |
| High-risk recovery 개별 시나리오 red | `high_risk_recovery::<submod>::scenario_…` | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |

Self-test (`bash scripts/main-ci-triage.sh --self-test`) 는 위 분류가 red → red 2회 연속, recovery, existing issue comment-only, cancelled run skip, skipped lane non-closure 등 엣지 케이스 모두에서 유지됨을 검증한다. 또한 `scenario_three_gate_failures_produce_distinct_identifiers` 가 Full / PG / High-risk recovery 3개 gate 동시 실패 시 서로 다른 식별자 + 서로 다른 issue 가 생성됨을 확인한다.

## 5. 누가 소유하는가

- 3개 gate 의 red 신호 → `agent:project-agentdesk` label 로 자동 triage 배정.
- Gate red 가 2회 연속 재현되면 `[ci-red] <identifier> 실패 (main)` 제목의 이슈가 `ci-red` + `agent:project-agentdesk` label 로 생성/업데이트된다.
- 2회 연속 green 이면 자동 close.

## 6. 변경 이력 힌트

- #973 / #974: release gate B-12 도입.
- #1011 (이 문서): path filter gap 보강 (`src/kanban.rs`, `src/services/auto_queue.rs`, `src/services/message_outbox.rs`), triage classifier self-test 확장.
