# Release Candidate CI Gates

이 문서는 AgentDesk `main` 및 PR 레벨에서 release candidate 자격을 보증하는 3개 CI gate를 명시한다. Gate는 "이 job이 red면 릴리즈 후보가 아니다"를 의미하며, 우회 금지(branch protection에서 required로 등록 또는 자동 triage로 동등 효과를 보장).

> 상위 소스-오브-트루스: [`docs/source-of-truth.md`](../source-of-truth.md)
>
> 관련 문서: [`docs/high-risk-recovery-lane.md`](../high-risk-recovery-lane.md)

## 1. 3개 Release Gate

| Gate | ci-main.yml job | ci-pr.yml job | ci-nightly.yml 대응 | 실행 조건 |
| --- | --- | --- | --- | --- |
| **Full tests** | `full_non_pg` | `check_fast` (compile/policy only) + `test_fast` | `full_macos` + `full_windows` | main/nightly always run non-PG tests; the path-filtered PR lane is compile/policy only. |
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
  - 'src/high_risk_recovery.rs'
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

- Ordinary generated markdown freshness drift is **warning-only** for PR work.
  Stale `ARCHITECTURE.md` or `docs/generated/**` output is not equivalent to
  Full/PG/High-risk release-gate failure unless the PR is itself changing the
  generator, generated report wording, or the maintainability invariant that the
  report represents.
- `ci-pr.yml` and `ci-main.yml` run `scripts/ci-script-checks.sh`, which invokes
  `scripts/generate_inventory_docs.py` in the CI workspace. That command may
  update generated markdown locally for downstream checks, but generic markdown
  freshness drift is not the hard gate. The hard failures are the generator's
  source-of-truth invariants, such as giant-file registry drift, missing
  metadata, parse errors, or other explicitly coded maintainability errors.
- `ci-nightly.yml` runs `scripts/generate_inventory_docs.py --check` as
  `Generated docs drift (warn)` and emits a GitHub warning when inventory docs
  are stale.
- `.github/workflows/regen-docs.yml` owns the scheduled refresh path. It runs
  weekly, commits regenerated `ARCHITECTURE.md` / `docs/generated/**` output to a
  maintenance branch, and opens a reviewable PR. This keeps generated docs useful
  without forcing unrelated feature/fix PRs to carry mechanical report churn.

### Script checks Python runtime

- `scripts/ci-script-checks.sh` 는 Python 3.11+ 를 최소 런타임으로 요구한다. 이는 `tomllib` 같은 Python 3.11 표준 라이브러리 사용과 `scripts/audit_maintainability.py` 정책에 맞춘다.
- CI 의 `Script checks` 계열 job 은 `actions/setup-python` 으로 Python 3.11 을 명시적으로 설치한다.
- 로컬에서 `python3` 이 3.10 이하이면 `PYTHON=/path/to/python3.11 ./scripts/ci-script-checks.sh` 로 같은 정책을 재현한다. 지원하지 않는 Python 은 check 본문 실행 전에 명확한 오류로 실패해야 한다.

## 3. High-risk recovery lane test axes

`#1011`/`#974` 감사로그는 release gate 의 high-risk recovery lane 이 아래 **4 축**을 회귀 방지선으로 유지해야 한다고 명시한다. 레거시 SQLite 기반 `src/integration_tests/tests/high_risk_recovery.rs` 시나리오 하네스는 #3035 Phase 1 에서 제거되었으며, PG-only 회귀 보호는 `src/high_risk_recovery.rs` 로 이전된다. 아래 시나리오 매트릭스(`failure_recovery` / `outbox_boundary` / `delayed_worker` / `idle_session_cleanup` 축)는 제거된 레거시 하네스 기준 기록이며 PG 스위트 재매핑은 후속 Phase 에서 진행한다. 축별 대표 시나리오는 [`docs/high-risk-recovery-lane.md`](../high-risk-recovery-lane.md#release-gate-축-매핑) 참고.

| Axis | What it guards | Representative scenarios (cargo test filters) |
| --- | --- | --- |
| **Live turn 보존** | restart 직후 in-flight turn / dispatch 가 손실되거나 broken pointer 로 복원되지 않도록 | `high_risk_recovery::failure_recovery::scenario_3_restart_recovery_reconciles_broken_state`, `failure_recovery::scenario_667_restart_recovery_reconciles_duplicate_review_dispatches` |
| **Watcher reattach** | tmux 출력 watcher / deadlock watchdog 가 재시작 후 정상 재부착되고 stale 입력에 잘못 알림 보내지 않도록 | `high_risk_recovery::delayed_worker::scenario_421_deadlock_recent_output_extends_watchdog`, `delayed_worker::scenario_421_deadlock_stale_output_only_marks_suspected_deadlock`, `delayed_worker::scenario_421_long_turn_alerts_start_at_30_minutes` |
| **Dispatch/outbox idempotency** | notify outbox 가 정확히 1회 전달되고 fallback / duplicate / mixed action / completed 상태가 깨지지 않도록 | `high_risk_recovery::outbox_boundary::scenario_160_1_outbox_batch_delivers_exactly_once`, `outbox_boundary::scenario_160_2_recovery_fallback_completes_dispatch`, `outbox_boundary::scenario_160_4_outbox_processes_all_entries_including_duplicates`, `outbox_boundary::scenario_160_6_notify_success_keeps_completed_dispatch_terminal` |
| **Queue loss 방지** | boot reconcile 이 누락된 review dispatch / notify outbox / 깨진 auto-queue entry 를 backfill 하고, idle 세션 정리가 active dispatch 를 잘라먹지 않도록 | `high_risk_recovery::failure_recovery::scenario_251_boot_reconcile_backfills_missing_notify_outbox`, `failure_recovery::scenario_251_boot_reconcile_refires_missing_review_dispatch`, `failure_recovery::scenario_251_boot_reconcile_resets_broken_auto_queue_entries`, `idle_session_cleanup::scenario_492_idle_session_with_active_dispatch_uses_180_minute_safety_ttl` |

이 4 축 중 하나라도 시나리오가 0 개로 줄어들면 lane 자체가 release gate 자격을 잃는다고 본다. 새 시나리오는 위 표 + `docs/high-risk-recovery-lane.md` 동시 갱신 후 PR 에 동봉.

## 4. Resource Contention Policy

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

## 5. Triage 분류 규약

`scripts/main-ci-triage.sh` 는 `CI Main` 이 2회 연속 red일 때 test identifier 또는 `job::<name>` 단위로 ci-red 이슈를 생성/갱신한다. Release gate 별 분류 계약:

| 실패 형태 | identifier 패턴 | 재현 커맨드 (issue body 에 기록) | Follow-up owner label |
| --- | --- | --- | --- |
| Full tests 개별 케이스 red | `<mod>::<test>` (e.g. `pipeline::tests::…`) | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| PG tests 개별 케이스 red | `<mod>::…_pg_…` / `postgres_…` | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| High-risk recovery job 자체 red (로그에서 test id 추출 실패) | `job::High-risk recovery` | `_job-level failure; see failing workflow job_` | `agent:project-agentdesk` |
| High-risk recovery 개별 시나리오 red | `high_risk_recovery::<submod>::scenario_…` | `cargo test -p agentdesk <identifier> -- --exact --nocapture` | `agent:project-agentdesk` |
| 인프라 종료(job-level, test id 추출 실패 + SIGTERM/signal 15/exit 143/cancel, **real-failure 신호 없음**) | **미기록 — flaky skip** | (없음, ci-red 미승격) | (없음) |
| Job-level red + real-failure 신호(`error[E…]` / `could not compile` / `test result: FAILED` / `panicked at` / failed assertion) — SIGTERM 노이즈 혼재 여부 무관 | `job::<name>` | `_job-level failure; see failing workflow job_` | `agent:project-agentdesk` |

### SIGTERM / 인프라 종료 = flaky skip (ci-red 미승격) — #3991 / #3996

`job::<name>` 폴백은 실패 job 로그에서 `test … FAILED` assertion 을 하나도 못 뽑았을 때만 발생한다. 이 폴백 로그가 **인프라 레벨 종료** 패턴(러너 OOM/축출로 인한 `signal 15` / `SIGTERM` / `SIGKILL`, `exit 143`, GitHub Actions `The operation was canceled` / `runner has received a shutdown signal`)을 담고 있으면, 이는 코드 회귀가 아니라 flaky 러너 압박이므로 식별자를 **기록하지 않고 skip** 한다 (`log_has_infra_termination`). 따라서 2회 연속 red 여도 ci-red 이슈로 승격되지 않는다. 이 필터는 **오직 job-level 폴백에만** 적용된다 — 실제 `test … FAILED` 가 하나라도 있으면 (SIGTERM 노이즈가 같은 로그에 섞여 있어도) 그 test 식별자는 정상적으로 ci-red 승격된다.

**Real-failure 우선 규약 (#3996):** 인프라 종료 skip 은 **인프라 종료가 유일한 실패 신호일 때만** 적용된다. job-level 폴백 로그에 `log_has_real_failure` 가 잡는 **결정적 실패 신호**(rustc 컴파일 에러 코드 `error[E…]`, `error: could not compile`, `test result: FAILED`, `panicked at`, failed assertion) 가 하나라도 있으면 — 같은 로그에 SIGTERM/exit 143 노이즈가 섞여 있어도 — 그 job 은 **정상 승격**된다. 즉 skip 조건은 `log_has_infra_termination && ! log_has_real_failure` 로, real 신호가 인프라 노이즈보다 항상 우선한다. 이 가드가 없으면 `test … FAILED` 를 남기지 않는 컴파일 회귀(job-level 폴백 경로)가 SIGTERM 문자열 혼재만으로 flaky 오분류되어 조용히 묻히는 false-negative 가 발생한다 (flaky 필터의 최악 실패 모드).

PR 레벨 `Fast check` (`ci-pr.yml check_fast`) 의 signal-15 는 이 triage 대상이 아니다 (triage 는 `CI Main` on `main` 만 처리). PR 측은 재실행 또는 근본완화(러너 자원/컴파일 병렬도 캡, #3658 계열)로 커버한다.

Self-test (`bash scripts/main-ci-triage.sh --self-test`) 는 위 분류가 red → red 2회 연속, recovery, existing issue comment-only, cancelled run skip, skipped lane non-closure 등 엣지 케이스 모두에서 유지됨을 검증한다. 또한 `scenario_three_gate_failures_produce_distinct_identifiers` 가 Full / PG / High-risk recovery 3개 gate 동시 실패 시 서로 다른 식별자 + 서로 다른 issue 가 생성됨을, `scenario_sigterm_job_failure_is_skipped_as_flaky` 가 2회 연속 SIGTERM job-level 실패(real 신호 없음)는 이슈화되지 않음을, `scenario_sigterm_noise_with_real_test_failure_still_creates_issue` 가 SIGTERM 노이즈가 섞여도 실제 test 실패는 여전히 이슈화됨을, `scenario_compile_error_with_sigterm_noise_still_creates_issue` (#3996) 가 SIGTERM 노이즈가 섞인 **컴파일 에러 job-level 폴백**도 real-failure 가드 덕에 ci-red 로 정상 승격됨을 확인한다.

## 6. 누가 소유하는가

- 3개 gate 의 red 신호 → `agent:project-agentdesk` label 로 자동 triage 배정.
- Gate red 가 2회 연속 재현되면 `[ci-red] <identifier> 실패 (main)` 제목의 이슈가 `ci-red` + `agent:project-agentdesk` label 로 생성/업데이트된다.
- 2회 연속 green 이면 자동 close.

## 7. 변경 이력 힌트

- #973 / #974: release gate B-12 도입.
- #1011 (이 문서): path filter gap 보강 (`src/kanban.rs`, `src/services/auto_queue.rs`, `src/services/message_outbox.rs`), triage classifier self-test 확장, 4 축 (live turn / watcher reattach / dispatch-outbox idempotency / queue loss) 명시.
- #3991: job-level 폴백에서 인프라 종료(SIGTERM/signal 15/exit 143/canceled) 로그를 flaky 로 분류해 ci-red 미승격 (`log_has_infra_termination`), self-test 2건 추가.
- #3996: flaky skip 에 real-failure 우선 가드 추가 (`log_has_real_failure`) — 인프라 종료 skip 은 `log_has_infra_termination && ! log_has_real_failure` 일 때만 적용. `error[E…]`/`could not compile`/`test result: FAILED`/`panicked at`/failed assertion 등 결정적 실패 신호가 섞이면 (SIGTERM 노이즈 무관) 정상 승격. 컴파일 회귀 job-level 폴백이 SIGTERM 혼재로 오분류되던 false-negative 차단, self-test 1건 추가 (`scenario_compile_error_with_sigterm_noise_still_creates_issue`).

## 8. Operational Post-Deploy Smoke

The CI gates above cannot use live Discord credentials, so live relay continuity
is an operational release smoke instead of a required CI job. For relay-adjacent
or restart-adjacent deploys, run the post-deploy TUI relay continuity smoke
from the release worktree after the build is ready:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py \
  --cell claude-tui \
  --confirm-live \
  --deploy-command 'AGENTDESK_DEPLOY_ALLOW_NON_MAIN=1 scripts/deploy-release.sh --skip-review'
```

The same script has CI-safe validation modes:

```bash
python3 scripts/e2e/post_deploy_relay_continuity.py --self-check
python3 scripts/e2e/post_deploy_relay_continuity.py --fixture pass
```

Full runbook:
[`docs/runbooks/post-deploy-relay-continuity-smoke.md`](../runbooks/post-deploy-relay-continuity-smoke.md).
