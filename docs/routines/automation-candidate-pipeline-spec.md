---
doc_type: "spec_source"
schema_version: "2"
status: "implemented"
topic_slug: "agentdesk-automation-candidate-pipeline"
topic_folder: "agentdesk-automation-candidate-pipeline"
linked_prd: "./agentdesk-automation-candidate-pipeline-prd.md"
traceability_mode: "req-task-test"
generated_by: "spec-writer"
created_at: "2026-05-02"
updated_at: "2026-05-02"
---

# SPEC SOURCE: AgentDesk Automation Candidate Pipeline

## Linked Documents

- PRD: [agentdesk-automation-candidate-pipeline-prd.md](./agentdesk-automation-candidate-pipeline-prd.md)
- 원 Recommender PRD: [agentdesk-automation-candidate-recommender-prd.md](../agentdesk-automation-candidate-recommender/agentdesk-automation-candidate-recommender-prd.md)
- Enrichment Spec (P0): [agentdesk-observation-provider-enrichment-spec.md](../agentdesk-observation-provider-enrichment/agentdesk-observation-provider-enrichment-spec.md)

## State

- status: implemented
- source_of_truth_scope: requirements, tasks, tests, status
- sync_policy: PRD 수정 후 spec-source 문서도 같이 갱신
- implementation_stage: implemented (PR #1497)

---

## Requirement Registry

### P0-E: Recommender Saturation Detection & Re-optimization

> **설계 근거**: autoresearch(`karpathy/autoresearch`)는 programmatic saturation detection이 없다. 루프(`while True`), EMA loss 스무딩(`ema_beta=0.9`), fast-fail(`isnan or loss>100`), `results.tsv` 공유 매체 패턴만 존재하며, 포화 복구는 인간/agent의 직접 판단이다. 아래 REQ는 이 패턴들을 기반으로 **직접 설계한** saturation detection + re-optimization이다.

- [REQ-P0E-001] recommender checkpoint는 saturation 추적 필드를 가져야 한다:
  - `ema_scored: number` (EMA-smoothed scored 카운터, 초기값=0, `ema_beta=0.9`)
  - `saturation_ticks: number` (연속 포화 tick 카운터, 초기값=0)
  - `fast_fail_ticks: number` (모든 obs가 dedup된 연속 tick 카운터, 초기값=0)
  - `reopt_count: number` (re-optimization 실행 횟수, 초기값=0)
  - `diversity_mode_ticks_remaining: number` (diversity mode 잔여 tick, 초기값=0)
  - `last_reopt_at: string|null` (마지막 reopt ISO timestamp)

- [REQ-P0E-002] 매 tick 종료 후 EMA 업데이트:
  ```
  ema_scored = EMA_BETA * ema_scored + (1 - EMA_BETA) * scored_this_tick
  ```
  `EMA_BETA = 0.9` (autoresearch `ema_beta=0.9` 그대로). 첫 tick은 debiasing 없이 그대로 설정.

- [REQ-P0E-003] **Fast-fail tier** (autoresearch `if isnan(loss): exit(1)` 대응):
  - `scored === 0 AND deduped === total_obs_this_tick AND total_obs_this_tick > 0`이면 `fast_fail_ticks += 1`, 아니면 `fast_fail_ticks = 0`.
  - `fast_fail_ticks >= FAST_FAIL_TICKS`(기본 2)이면 즉시 reopt 트리거 (EMA tier 대기 없음).

- [REQ-P0E-004] **EMA saturation tier**:
  - `ema_scored < EMA_SATURATION_THRESHOLD`(기본 0.3)이면 `saturation_ticks += 1`, 아니면 `saturation_ticks = 0`.
  - `saturation_ticks >= SATURATION_TICKS`(기본 5)이면 reopt 트리거.

- [REQ-P0E-005] **Re-optimization** (partial reset + diversity mode):
  - `seen_evidence`에서 현재 시각 기준 `REOPT_WINDOW_MS`(기본 12h) 이전에 기록된 항목만 삭제. 최근 12h 이내 항목은 보존.
  - `diversity_mode_ticks_remaining = DIVERSITY_BOOST_TICKS`(기본 10).
  - `saturation_ticks = 0`, `fast_fail_ticks = 0`, `reopt_count += 1`, `last_reopt_at = nowStr`.

- [REQ-P0E-006] `diversity_mode_ticks_remaining > 0`인 동안:
  - 직전 `DIVERSITY_LOOKBACK_TICKS`(기본 5) tick에서 scored 횟수가 가장 적은 source category를 먼저 처리.
  - 처리 후 `diversity_mode_ticks_remaining -= 1`.

- [REQ-P0E-007] **Tick log** (autoresearch `results.tsv` 아날로그):
  - 매 tick 결과를 `routine_observation:recommender_tick:{yyyymmdd_hhmm}` kv_meta(TTL 48h)에 기록. 단, 이 kv_meta 쓰기는 JS routine이 직접 할 수 없으므로 `result` 반환값에 포함시켜 Rust runtime이 기록한다.
  - 포함 필드: `scored`, `deduped`, `ema_scored`, `best_candidate_score`, `reopt_count`, `saturation_ticks`, `fast_fail_ticks`.
  - **구현 주의**: JS가 kv_meta를 직접 쓸 수 없으므로 P0-E에서는 `scoring_summary` 문자열에 포함하는 것으로 갈음하고, Rust-side tick log는 P1 Rust task로 분리한다.

- [REQ-P0E-008] scoring_summary에 `ema_scored`, `reopt_count`, `saturation_ticks`, `fast_fail_ticks` 값을 포함해야 한다.
  - 예: `scored=N deduped=M ema_scored=0.12 reopt_count=2 saturation_ticks=3 fast_fail_ticks=0`

- [REQ-P0E-009] NEVER STOP: saturation → reopt → 탐색 재개 루프는 외부 개입 없이 indefinitely 반복된다. tick이 `complete`를 반환하더라도 다음 tick이 예약된다.

### P1: kv_meta Pipeline Contract

- [REQ-P1-001] recommender가 `action:"agent"`를 emit한 이후, LLM agent는 후보 검토 결과를 `routine_observation:candidate_review:{signature}` kv_meta 키(TTL 48h)로 기록해야 한다. 이 키가 없으면 detector는 해당 후보를 볼 수 없다.
- [REQ-P1-002] detector가 품질 게이트를 통과시키면, LLM agent는 `routine_observation:candidate_approved:{signature}` kv_meta 키(TTL 72h)를 기록해야 한다.
- [REQ-P1-003] executor가 dispatch 완료 후, LLM agent는 `routine_observation:candidate_dispatched:{signature}` kv_meta 키(TTL 7d)를 기록해야 한다.
- [REQ-P1-004] `candidate_dispatched:*` kv_meta 항목은 recommender의 suppressions 또는 automationInventory와 연동되어 재추천을 방지해야 한다.
- [REQ-P1-005] detector와 executor는 JS routine sandbox 제약을 준수해야 한다: HTTP/DB/memento/file bridge 없음. `ctx.observations`, `ctx.checkpoint`, `action:"agent"` 반환만 가능.

### P1: Automation Candidate Detector

- [REQ-P1-006] `automation-candidate-detector.js`는 `ctx.observations`에서 `source_kind:"candidate_review"` 또는 `category:"candidate_review"`에 해당하는 항목을 필터링해야 한다.
- [REQ-P1-007] 품질 게이트 통과 조건: (1) `evidence_age` ≤ 48h, (2) candidate score ≥ `DETECTOR_SCORE_THRESHOLD`(기본 80), (3) 동일 signature의 `approved` 또는 `dispatched` 항목이 이미 ctx.observations에 있으면 skip.
- [REQ-P1-008] 품질 게이트 통과 시 detector는 `action:"agent"` prompt를 생성해야 한다. prompt는 candidate signature, score, evidence 요약, "이 후보를 승인하려면 `candidate_approved:{signature}` kv_meta 키를 TTL 72h로 기록하라"는 명시적 지시를 포함해야 한다.
- [REQ-P1-009] detector checkpoint는 이미 처리한 후보의 `seen_candidates` Set(TTL 72h)을 유지해 같은 review 항목을 중복 emit하지 않아야 한다.
- [REQ-P1-010] 품질 게이트 실패 시 reject 이유를 `result`에 기록하고 `action:"complete"`를 반환해야 한다.

### P1: Automation Executor

- [REQ-P1-011] `automation-executor.js`는 `ctx.observations`에서 `source_kind:"candidate_approved"` 또는 `category:"candidate_approved"`에 해당하는 항목을 필터링해야 한다.
- [REQ-P1-012] `candidate_dispatched:*` 항목이 이미 ctx.observations에 있는 signature는 처리하지 않아야 한다 (중복 방지).
- [REQ-P1-013] 처리 대상 후보마다 `action:"agent"` prompt를 emit해야 한다. prompt는 다음을 포함해야 한다: (1) GitHub Issue 생성 요청 (title, body, labels 포함), (2) Kanban 카드 생성 요청 (board, column, description 포함), (3) "완료 후 `candidate_dispatched:{signature}` kv_meta 키를 TTL 7d로 기록하라"는 명시적 지시.
- [REQ-P1-014] executor checkpoint는 이미 dispatch한 후보의 `dispatched_signatures` Set(TTL 7d)을 유지해 중복 dispatch를 방지해야 한다.
- [REQ-P1-015] 처리한 후보가 없으면 `action:"complete"`를 반환하고 다음 틱을 기다린다.

---

## Task Registry

### Phase P0-E: Recommender Saturation Detection & Re-optimization

- [TSK-P0E-001] **Constants 추가**: `EMA_BETA=0.9`, `EMA_SATURATION_THRESHOLD=0.3`, `SATURATION_TICKS=5`, `FAST_FAIL_TICKS=2`, `REOPT_WINDOW_MS=12*3600*1000`, `DIVERSITY_BOOST_TICKS=10`, `DIVERSITY_LOOKBACK_TICKS=5`.

- [TSK-P0E-002] **`emptyCheckpoint()` 확장**:
  - 신규 필드: `ema_scored: 0`, `saturation_ticks: 0`, `fast_fail_ticks: 0`, `reopt_count: 0`, `diversity_mode_ticks_remaining: 0`, `last_reopt_at: null`
  - `loadCheckpoint()`에서 backward-compat 기본값 처리 (기존 checkpoint에 필드 없으면 0).

- [TSK-P0E-003] **`updateEmaScored(cp, scoredThisTick)`** 헬퍼 구현:
  ```js
  cp.ema_scored = EMA_BETA * cp.ema_scored + (1 - EMA_BETA) * scoredThisTick;
  ```

- [TSK-P0E-004] **`updateFastFailTicks(cp, scoredThisTick, totalObsThisTick)`** 헬퍼 구현:
  - `scored === 0 AND total > 0`이면 `fast_fail_ticks += 1`, 아니면 `fast_fail_ticks = 0`.

- [TSK-P0E-005] **`updateSaturationTicks(cp)`** 헬퍼 구현:
  - `ema_scored < EMA_SATURATION_THRESHOLD`이면 `saturation_ticks += 1`, 아니면 `saturation_ticks = 0`.

- [TSK-P0E-006] **`shouldTriggerReopt(cp): boolean`** 헬퍼 구현:
  - `fast_fail_ticks >= FAST_FAIL_TICKS OR saturation_ticks >= SATURATION_TICKS` 이면 `true`.

- [TSK-P0E-007] **`triggerReopt(cp, nowStr)`** 헬퍼 구현:
  - `partialResetSeenEvidence(cp, nowStr, REOPT_WINDOW_MS)` 호출.
  - `diversity_mode_ticks_remaining = DIVERSITY_BOOST_TICKS`.
  - `saturation_ticks = 0`, `fast_fail_ticks = 0`, `reopt_count += 1`, `last_reopt_at = nowStr`.

- [TSK-P0E-008] **`partialResetSeenEvidence(cp, nowStr, windowMs)`** 헬퍼 구현:
  - `seen_evidence`에서 `(nowMs - seenMs) > windowMs`인 항목만 삭제.

- [TSK-P0E-009] **`scoreObservations()` 수정**: `diversity_mode_ticks_remaining > 0`인 경우 source category별 최근 DIVERSITY_LOOKBACK_TICKS tick scored 횟수를 집계 (checkpoint `stats.category_scored_history`에 저장), underrepresented category 우선 처리.

- [TSK-P0E-010] **main tick 순서 수정**:
  ```
  1. pruneSeenEvidence / pruneExpiredSuppressions
  2. scoreObservations → {scored, deduped, total_obs, ...}
  3. updateEmaScored(cp, scored)
  4. updateFastFailTicks(cp, scored, total_obs)
  5. updateSaturationTicks(cp)
  6. if shouldTriggerReopt(cp): triggerReopt(cp, nowStr)
  7. scoring_summary 포함: scored/deduped/ema_scored/reopt_count/saturation_ticks/fast_fail_ticks
  ```

- [TSK-P0E-011] **JS 테스트** (`policies/__tests__/recommender-saturation.test.js`):
  - fast-fail: 2틱 all-dedup → `reopt_count === 1`
  - EMA tier: ema_scored < 0.3 연속 5틱 → `reopt_count === 1`
  - EMA 계산: scored=1 → `ema_scored ≈ 0.1`
  - partial reset: 12h 이전 항목만 제거, 최근 12h 이내 보존
  - diversity mode: 10틱 후 `diversity_mode_ticks_remaining === 0`
  - scored > 0 → `saturation_ticks === 0`
  - scoring_summary 파싱: `ema_scored=`, `reopt_count=`, `fast_fail_ticks=` 포함 확인

### Phase P1: Automation Candidate Detector

- [TSK-P1D-001] `policies/__tests__/support/routine-harness.js` 재활용 (기존 파일 변경 없음).
- [TSK-P1D-002] `routines/monitoring/automation-candidate-detector.js` 생성: `agentdesk.routines.register` 계약 준수, `ctx.observations` 필터링, 품질 게이트 구현.
- [TSK-P1D-003] detector checkpoint schema 구현: `seen_candidates: {}` (signature → first_seen_at ISO string, TTL 72h), `pending_reviews: []`, `dispatched_ref: []`.
- [TSK-P1D-004] 품질 게이트 구현: evidence_age 계산, score 확인, approved/dispatched 존재 여부 확인.
- [TSK-P1D-005] approval prompt 생성: candidate signature, score, evidence 요약, kv_meta 기록 지시 포함.
- [TSK-P1D-006] JS 테스트 추가 (`policies/__tests__/detector.test.js`): (1) 게이트 통과 시 action:"agent" 확인, (2) 이미 approved 후보 skip 확인, (3) evidence_age 초과 시 reject 확인, (4) 중복 emit 방지 확인.

### Phase P1: Automation Executor

- [TSK-P1E-001] `routines/monitoring/automation-executor.js` 생성: `agentdesk.routines.register` 계약 준수, `candidate_approved:*` 필터링.
- [TSK-P1E-002] executor checkpoint schema 구현: `dispatched_signatures: {}` (signature → dispatched_at ISO string, TTL 7d).
- [TSK-P1E-003] dispatch 중복 방지: `candidate_dispatched:*` ctx.observations 확인 + checkpoint `dispatched_signatures` 확인.
- [TSK-P1E-004] dispatch prompt 구현: GitHub Issue 생성 요청, Kanban 카드 생성 요청, `candidate_dispatched:{signature}` kv_meta 기록 지시 포함.
- [TSK-P1E-005] JS 테스트 추가 (`policies/__tests__/executor.test.js`): (1) approved 후보 수신 시 action:"agent" 확인, (2) dispatched 후보 skip 확인, (3) 후보 없으면 action:"complete" 확인.

---

## Test Plan

### Unit Tests (JS, `node:test`)

| 파일 | 테스트 | 검증 대상 |
|---|---|---|
| `recommender-saturation.test.js` | fast_fail: 2틱 all-dedup → reopt | fast_fail_ticks 카운터, reopt_count=1 |
| `recommender-saturation.test.js` | EMA tier: ema_scored < 0.3 × 5틱 → reopt | saturation_ticks 카운터, reopt_count |
| `recommender-saturation.test.js` | EMA 계산 정확도: scored=1 후 ema값 확인 | `ema = 0.9*0 + 0.1*1 = 0.1` |
| `recommender-saturation.test.js` | reopt 후 partial reset: 12h 이전만 삭제 | seen_evidence REOPT_WINDOW 기준 분리 |
| `recommender-saturation.test.js` | diversity mode 감소: 10 → 9 → … → 0 | diversity_mode_ticks_remaining 카운터 |
| `recommender-saturation.test.js` | scored > 0이면 saturation_ticks 리셋 | saturation_ticks = 0 |
| `recommender-saturation.test.js` | scoring_summary에 ema_scored 포함 | summary 파싱 |
| `detector.test.js` | 게이트 통과 | action === "agent" |
| `detector.test.js` | approved 후보 skip | action === "complete" |
| `detector.test.js` | evidence_age 초과 reject | action === "complete", reject 이유 |
| `detector.test.js` | 중복 emit 방지 (seen_candidates) | 2번째 tick에서 emit 없음 |
| `executor.test.js` | approved 수신 → agent | action === "agent", prompt 내용 |
| `executor.test.js` | dispatched 중복 skip | action === "complete" |
| `executor.test.js` | 후보 없음 | action === "complete" |

### Rust Integration Tests

- P0-E는 JS-only 변경이므로 Rust 신규 테스트 없음.
- detector/executor kv_meta 키 포맷이 provider에서 올바르게 필터링되는지는 P1 구현 시 `store.rs` 통합 테스트로 검증.

---

## kv_meta Key Format Reference

```
routine_observation:candidate_review:{signature}         TTL: 48h  생산자: LLM agent (recommender 수신 후)
routine_observation:candidate_approved:{signature}       TTL: 72h  생산자: LLM agent (detector 승인 후)
routine_observation:candidate_dispatched:{signature}     TTL: 7d   생산자: LLM agent (executor dispatch 후)
routine_observation:recommender_tick:{yyyymmdd_hhmm}     TTL: 48h  생산자: Rust runtime (P1), JS result (P0-E 임시)
```

`{signature}` = recommender candidate의 `pattern_id` 또는 stable signature 문자열.

### Tick Log 필드 (results.tsv 아날로그)

autoresearch의 `results.tsv`(`commit_hash \t val_bpb \t memory_gb \t keep|discard \t description`)에서 영감을 받은 tick-by-tick 진행 기록:

```json
{
  "tick_at": "2026-05-02T10:01:00Z",
  "scored": 2,
  "deduped": 8,
  "total_obs": 10,
  "ema_scored": 0.27,
  "best_candidate_score": 85,
  "reopt_count": 1,
  "saturation_ticks": 3,
  "fast_fail_ticks": 0,
  "diversity_mode_remaining": 7
}
```

---

## Saturation Detection Flow Diagram

```
[autoresearch 영감: while True + ema_beta=0.9 + fast-fail(isnan)]

tick N:
  score observations → {scored, deduped, total_obs}

  // EMA update (autoresearch ema_beta=0.9 그대로)
  ema_scored = 0.9 * ema_scored + 0.1 * scored

  // Fast-fail tier (autoresearch "if isnan: exit(1)" 대응, 발명)
  if scored==0 AND deduped==total_obs AND total_obs>0:
    fast_fail_ticks++
  else:
    fast_fail_ticks = 0

  // EMA saturation tier (발명)
  if ema_scored < EMA_SATURATION_THRESHOLD(0.3):
    saturation_ticks++
  else:
    saturation_ticks = 0

  // Reopt trigger
  if fast_fail_ticks >= FAST_FAIL_TICKS(2)
     OR saturation_ticks >= SATURATION_TICKS(5):
    partialResetSeenEvidence(REOPT_WINDOW_MS=12h)  // 발명
    diversity_mode_ticks_remaining = DIVERSITY_BOOST_TICKS(10)
    fast_fail_ticks = 0; saturation_ticks = 0; reopt_count++

tick N+1..N+10:
  diversity mode: underrepresented source 우선
  diversity_mode_ticks_remaining--

tick N+11:
  normal mode 재개
  NEVER STOP: 다음 saturation까지 카운트 재시작

// Tick log (autoresearch results.tsv 아날로그, 발명)
// scoring_summary에 포함: scored=N deduped=M ema_scored=0.xx reopt_count=K ...
```

---

## Open Questions

- [ ] `candidate_review` kv_meta 항목의 source_kind 필드명을 현재 provider가 어떻게 분류하는가? Rust provider에서 `source_kind` 필드 확인 필요.
- [ ] diversity mode에서 source category 우선순위 집계를 checkpoint에 저장할지 tick-local 변수로 처리할지 결정 필요.
- [ ] executor prompt에서 GitHub repo/Kanban board 정보를 ctx.observations에서 읽을지, 상수로 hardcode할지 결정 필요 (P1 구현 시 결정).
