# #743 create-pr lifecycle 재설계

## Background

AgentDesk의 create-pr lifecycle(리뷰 통과 → 에이전트 dispatch로 PR 생성 → CI → 머지)에서 여러 race condition이 누적. PR #742(#701)에서 Codex adversarial review 12 라운드 중 "개별 race 패치로는 못 닫는다"는 결론 → #743으로 재설계 deferred.

## 해결 대상 Race

1. **Stale late-arriving completion**: 이전 라이프사이클의 create-pr 완료가 reopen된 카드의 새 pr_tracking을 덮어씀
2. **Pipeline-originated 멈춤**: `in_progress + pr:creating`에 남아 CI 통과 후 terminal 못 감
3. **Stale tracking defer**: `pr_tracking.state='create-pr'` row가 현재 라이프사이클 것인지 검증 없이 defer
4. **Pr_tracking row 부재**: degraded 경로(no-agent/no-repo)에서 `pr:create_failed` 마커만 있고 pr_tracking 부재 → 수동 개입으로만 복구

## 핵심 접근

**Dispatch identity 도입**: 각 create-pr dispatch에 `dispatch_generation` UUID를 발급하고 두 곳에 동시 stamp:
- `dispatch.context.dispatch_generation` (완료 페이로드)
- `pr_tracking.dispatch_generation` (카드의 현재 권위 lifecycle ID)

완료 시 두 값 비교 → 단일 stale/fresh 판정.

**축소 철학**: Atomic guarantee가 정말 필요한 fresh review-pass 경로만 Rust bridge op으로, failure/escalate/retry-lane은 기존 JS contract 그대로 유지. Codex 공격 표면 최소화.

## 데이터 모델

### pr_tracking 스키마 확장

```sql
ALTER TABLE pr_tracking ADD COLUMN dispatch_generation TEXT NOT NULL DEFAULT '';
ALTER TABLE pr_tracking ADD COLUMN review_round INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pr_tracking ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
```

- `dispatch_generation`: UUID v4. stale 판정 + row ownership primary key
- `review_round`: observability/debugging (stale 판정 보조)
- `retry_count`: 실패 재시도 횟수. 3회 도달 시 escalate

### Dispatch context stamp

```js
{
  dispatch_generation: <uuid>,
  review_round_at_dispatch: <integer>,
  sidecar_dispatch: true,
  worktree_path, worktree_branch, branch
}
```

### DB partial unique index

```sql
CREATE UNIQUE INDEX idx_single_active_create_pr
ON task_dispatches (kanban_card_id)
WHERE dispatch_type = 'create-pr' AND status IN ('pending', 'dispatched');
```

`src/dispatch/dispatch_create.rs:333` UNIQUE race retry 분기에 `"create-pr"` 추가.

### Migration pre-check (loud-fail)

Index 생성 전 중복 active row 있으면 startup panic:

```rust
let dupes: i64 = conn.query_row(
    "SELECT COUNT(*) FROM (
       SELECT kanban_card_id FROM task_dispatches
       WHERE dispatch_type='create-pr' AND status IN ('pending','dispatched')
       GROUP BY kanban_card_id HAVING COUNT(*) > 1
     )", [], |r| r.get(0))?;
if dupes > 0 {
    panic!("Migration blocked: {dupes} cards have duplicate active create-pr dispatches.");
}
```

## 배포 프리컨디션 (Zero-inflight gate)

`scripts/deploy-dev.sh` / `scripts/promote-release.sh` 시작부:

```bash
if ! curl -sf --max-time 3 "http://127.0.0.1:$DEV_PORT/api/health" > /dev/null 2>&1; then
  echo "[deploy] API not reachable — skipping zero-inflight gate"
else
  inflight=$(curl -s "http://127.0.0.1:$DEV_PORT/api/dispatches?status=pending" \
    | jq '[.dispatches[] | select(.dispatch_type=="create-pr")] | length')
  inflight_dispatched=$(curl -s "http://127.0.0.1:$DEV_PORT/api/dispatches?status=dispatched" \
    | jq '[.dispatches[] | select(.dispatch_type=="create-pr")] | length')
  if [ "$inflight" -gt 0 ] || [ "$inflight_dispatched" -gt 0 ]; then
    echo "ERROR: $inflight + $inflight_dispatched create-pr dispatches inflight. Abort."
    exit 1
  fi
fi
```

API 미응답 시 gate skip(복구 배포 false-block 방지). `$DEV_PORT` 할당을 스크립트 상단으로 이동해 `set -euo pipefail` 하에서도 bind됨 보장.

## Contract 보존 매트릭스

| # | Contract | 출처 | 보존 방식 |
|---|---|---|---|
| C1 | `upsertPrTracking` facade COALESCE | `00-pr-tracking.js:36` | signature 불변, 새 컬럼은 Rust bridge op만 write |
| C2 | `create_dispatch_core` 자체 tx | `dispatch_create.rs:160,579` | `_on_conn` variant 분리 + thin shim |
| C3 | `processTrackedMergeQueue` terminal only | `merge-automation.js:1711` | handoffCreatePr가 status 안 건드림 |
| C4 | `markPrCreateFailed`는 terminal force 전이 | `review-automation.js:772` | JS orchestration 유지 (setStatus 포함) |
| C5 | dispatch active dedupe | `dispatch_create.rs:80,213` | handoffCreatePr **idempotent reuse**. reseedPrTracking이 cancel 동반 |
| C6 | degraded 경로 pr_tracking 미생성 | `deploy-pipeline.js:332` | JS 전용 처리, pr_tracking 건드리지 않음 |
| C7 | escalation = state + notification 짝 | `ci-recovery.js:332,465` | 기존 JS `escalateToManualIntervention` 재사용 |
| C8 | `OnDispatchCompleted` success-only | `dispatch_status.rs` | failure는 JS catch에서 bridge op 호출 |

## Rust bridge ops

### 2개 bridge + 1 helper

#### handoffCreatePr(cardId, payload) → { dispatch_id, generation, reused }

Fresh review-pass 경로 전용 (review-automation, deploy-pipeline). Single transaction:

1. `lookup_active_dispatch_id("create-pr")` 있으면 기존 id + 기존 generation 반환 (idempotent reuse, C5)
2. 없으면:
   - `INSERT OR REPLACE pr_tracking` (state='create-pr', new uuid, round, head_sha, retry_count=0)
   - `create_dispatch_core_on_conn(&tx, ..., "create-pr", context_with_stamp)`
   - `UPDATE kanban_cards SET blocked_reason='pr:creating'`
3. commit

#### reseedPrTracking(cardId) → { generation }

Single transaction:

1. active create-pr dispatch 있으면 `cancel_dispatch_and_reset_auto_queue_on_conn(&tx, id, "superseded_by_reseed")` (C5)
2. `UPDATE pr_tracking SET dispatch_generation=new_uuid, review_round=current, head_sha=latest, state='create-pr', retry_count=0, last_error=NULL`

#### recordPrCreateFailure(cardId, error, stamp_gen_or_null) → { retry_count, escalated }

Thin helper, JS orchestration 안에서 호출:

1. Stale guard: `stamp_gen != null AND stamp_gen != current_gen` → noop
2. pr_tracking row 있음 → `UPDATE retry_count = retry_count + 1, last_error=?`
   row 없음 (null-stamp, handoff rollback 경로) → `INSERT retry_count=1`
3. `retry_count >= 3` → `state='escalated'`
4. blocked_reason 미변경 (JS orchestration이 별도 세팅)

UPDATE+SELECT 패턴 (SQLite RETURNING 의존 회피).

## dispatch_create 리팩토링

`src/dispatch/dispatch_create.rs` 2개 함수를 `_on_conn` variant로 분리:

```rust
pub fn create_dispatch_core_on_conn(conn: &Connection, ...) -> Result<...>
pub fn apply_dispatch_attached_intents_on_conn(conn: &Connection, ...) -> Result<...>

// 기존 진입점은 thin shim
pub fn create_dispatch_core_with_options(db: &Db, ...) -> Result<...> {
    let mut conn = db.lock()?;
    let tx = conn.transaction()?;
    let r = create_dispatch_core_on_conn(&tx, ...)?;
    tx.commit()?;
    Ok(r)
}
```

외부 caller signature 불변. `handoffCreatePr`만 외부 tx에서 on_conn variant 호출.

## JS facade 원칙

`policies/00-pr-tracking.js`:

- `loadTrackedPrForCard` SELECT 확장: `dispatch_generation, review_round, retry_count` 포함 (read only)
- `upsertPrTracking` signature **변경 없음** — 새 컬럼 write 불가
- 새 컬럼 write는 Rust bridge op 전용
- Error seed path는 새 컬럼 건드리지 않음 (COALESCE가 기존 값 보존)

## Stale 판정

### Success (onDispatchCompleted)

```js
var stampGen = dispatch.context && dispatch.context.dispatch_generation;
if (!stampGen) {
  agentdesk.log.error("[review] stamp missing — reseeding");
  agentdesk.reviewAutomation.reseedPrTracking(cardId);
  return;
}
var tracking = loadTrackedPrForCard(cardId);
if (!tracking || tracking.dispatch_generation !== stampGen) return;  // stale noop
// proceed: pr_tracking → 'wait-ci', status transition
```

### Failure (JS orchestration)

```js
function markPrCreateFailed(cardId, error, stampGen) {
  // stampGen은 null 가능 (pre-handoff failure)
  var terminalTarget = resolveTerminalTarget(card);

  // 1. 먼저 retry row seed/retry++ (C4 literal: row before terminal)
  var result = agentdesk.reviewAutomation.recordPrCreateFailure(cardId, error, stampGen);

  // 2. Terminalize
  agentdesk.kanban.setStatus(cardId, terminalTarget, true);

  // 3. Blocked reason (setStatus가 clear하므로 뒤에 set)
  if (result.escalated) {
    agentdesk.kanban.setBlockedReason(cardId, 'pr:create_failed_escalated:max_retries');
    escalateToManualIntervention(cardId, 'create-pr max retries');  // C7
  } else {
    agentdesk.kanban.setBlockedReason(cardId, 'pr:create_failed:' + truncate(error, 120));
  }
}
```

Crash safety: 모든 중간 step 실패에서 retry loop가 tracking에서 복구.

## Success 상태 전이표 (kanban_cards.status 기준)

| 현재 status | 현재 blocked_reason | 전이 | 결과 status | 결과 blocked_reason | 용도 |
|---|---|---|---|---|---|
| `in_progress` | `pr:creating` | `setStatus(reviewPassTarget, force=true)` | `reviewPassTarget` | `ci:waiting` | pipeline origin |
| `reviewing`/`completeReviewState` | (empty) | `setStatus(reviewPassTarget)` 비강제 | `reviewPassTarget` | `ci:waiting` | fresh review pass |
| `reviewPassTarget` (이미) | `pr:creating` | status 유지, `blocked_reason`만 | (변화 없음) | `ci:waiting` | retry lane success (C3) |

`force=true` 범위: pipeline-originated 1 case 전용.

## onCardTerminal 정밀화 (merge-automation.js:41-93)

```js
onCardTerminal: function(payload) {
  var card = loadCardRow(cardId);
  var tracking = loadTrackedPrForCard(cardId);

  // degraded (C6 literal: pr_tracking 건드리지 않음)
  if (card.blocked_reason && card.blocked_reason.indexOf("pr:create_failed") === 0) {
    if (!tracking && card.blocked_reason.indexOf("_escalated:") < 0) {
      agentdesk.kanban.setBlockedReason(cardId, 'pr:create_failed_escalated:no_tracking');
      escalateToManualIntervention(cardId, 'pr_tracking row missing at terminal');
    }
    return;  // 기존 prefix 체크가 'pr:create_failed_escalated:*'도 cover
  }

  if (tracking && tracking.state === 'create-pr') {
    // 신규: stale generation 체크
    var activeGen = loadActiveCreatePrGeneration(cardId);
    if (tracking.dispatch_generation && activeGen
        && tracking.dispatch_generation !== activeGen) {
      agentdesk.reviewAutomation.reseedPrTracking(cardId);  // 내부 active cancel (C5)
      return;
    }
    // 신규: head_sha divergence
    var latestHead = loadLatestWorkHeadSha(cardId);
    if (tracking.head_sha && latestHead && tracking.head_sha !== latestHead) {
      agentdesk.reviewAutomation.reseedPrTracking(cardId);
      return;
    }
    return;  // 정상 defer
  }
  // 기존 merge 로직
}
```

`loadActiveCreatePrGeneration`: `json_extract(context, '$.dispatch_generation')` 사용 (SQLite JSON1).

## benign marker 편입

`src/manual_intervention.rs:1-8`:

```rust
const BENIGN_BLOCKED_REASON_PREFIXES: &[&str] = &[
    "ci:waiting", "ci:running", "ci:rerunning", "ci:rework",
    "deploy:waiting", "deploy:deploying:",
    "pr:creating",  // +추가
];
```

`policies/00-escalation.js:5-12`: 동일 추가.

`pr:create_failed*`, `pr:create_failed_escalated*`는 benign 아님.

## Retry lane (tryCreateTrackedPr)

**범위 밖 — 건드리지 않음**. `tryCreateTrackedPr` (`merge-automation.js:705-738`)는:

- 동기적 gh CLI 기반 PR 생성/조회 (dispatch 미생성)
- `pr-always` 모드 + direct-merge fallback 등 여러 caller
- Signature: `{ ok, pr.number, head_sha }` 동기 반환
- Dispatch stamp 불필요

`handoffCreatePr`은 fresh review-pass 경로 전용.

## 커밋 계획 (단일 PR, 7 커밋)

| # | 범위 | 커버 테스트 |
|---|---|---|
| 1 | 스키마 마이그레이션 + partial unique index + pre-check + benign marker (Rust+JS) + rollout scripts zero-inflight gate | 스키마, migration panic, benign 분류, script gate |
| 2 | `create_dispatch_core_on_conn` + `apply_dispatch_attached_intents_on_conn` variant 분리 (기존 shim 유지) | 회귀 |
| 3 | 2 bridge ops + 1 helper (handoffCreatePr idempotent reuse, reseedPrTracking cancel+update, recordPrCreateFailure INSERT-if-missing) | bridge op atomic, stale guard, retry count, active cancel |
| 4 | `00-pr-tracking.js` SELECT 확장 | facade read |
| 5 | `review-automation.js` / `deploy-pipeline.js` handoff 경유 리팩토링 + stale guard + markPrCreateFailed orchestration (record→setStatus→blocked_reason→escalate) + missing stamp reseed | reopen_race, same_round_late, pipeline_to_ci, parallel_creators, missing_stamp_reseed, markfailed_orchestration_order, retry_incremented_on_null_stamp |
| 6 | `merge-automation.js` onCardTerminal 정밀화 (degraded JS-only + stale generation/head reseed) | divergent_head_sha, no_tracking_escalates, reseed_cancels_active, escalated_defer_via_prefix |
| 7 | `dispatch_create.rs:333` UNIQUE race retry에 `"create-pr"` 추가 | parallel_dispatch_race |

## Test matrix

- `scenario_743_reopen_race_preserves_generation`
- `scenario_743_same_round_late_completion_no_new_work`
- `scenario_743_pipeline_originated_to_ci_success`
- `scenario_743_parallel_create_pr_creators`
- `scenario_743_handoff_idempotent_reuse`
- `scenario_743_divergent_head_sha_reseed`
- `scenario_743_no_tracking_with_create_failed_escalates`
- `scenario_743_pr_creating_not_manual_intervention`
- `scenario_743_retry_count_increments_and_escalates` (state='escalated' + blocked_reason='pr:create_failed_escalated:max_retries' 명시 assert)
- `scenario_743_reseed_resets_retry_count`
- `scenario_743_migration_blocks_on_duplicate_active`
- `scenario_743_failure_path_stale_guard`
- `scenario_743_missing_stamp_triggers_reseed`
- `scenario_743_markfailed_order_seeds_retry_row_first`
- `scenario_743_retry_incremented_on_null_stamp`

## Remaining risks

1. **SQLite RETURNING 미사용** — UPDATE+SELECT로 구현 (레포 현 방식과 일치)
2. **JSON1 `json_extract` 의존** — 이미 레포 여러 곳 사용 중(`auto-queue.js:37`, `kanban.rs:203`), 문제 없음
3. **Migration 롤백** — ADD COLUMN drop은 SQLite에서 불편. feature flag로 bridge op 비활성화 가능

## v1~v8 설계 여정 요약

7 라운드 Codex adversarial review를 거침. 주요 pivot:
- **v2 (critical 1 대응)**: `review_round` 단독 → `dispatch_generation` UUID 도입
- **v4→v5 (high 3 대응)**: atomic bridge op 범위를 happy path만으로 축소, failure/escalate는 기존 JS contract 보존
- **v7→v8**: 존재하지 않는 hook leak을 방어하려던 오판 되돌림

최종 v8: IMPLEMENT 판정.
