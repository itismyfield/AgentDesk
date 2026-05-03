---
doc_type: "prd"
schema_version: "1"
status: "implemented"
topic_slug: "agentdesk-automation-candidate-pipeline"
topic_folder: "agentdesk-automation-candidate-pipeline"
linked_spec: "./agentdesk-automation-candidate-pipeline-spec.md"
generated_by: "spec-writer"
created_at: "2026-05-02"
updated_at: "2026-05-02"
---

# PRD: AgentDesk Automation Candidate Pipeline

> status: `implemented`
> linked_spec: [agentdesk-automation-candidate-pipeline-spec.md](./agentdesk-automation-candidate-pipeline-spec.md)

## PRD 업데이트 규칙

1. 코드 수정 최소화 + 응집도 최대화 + 재활용 최대화
2. 모호성을 제거하고 구체적으로 적는다.
3. Spec이 목표와 정렬된 경우 단계별 TASK들을 만들어 업데이트한다.
4. 코드 구현 시 Test Code나 실 검증에 대해서도 같이 포함해서 구현한다.
5. 구현하고자 하는 목표에 가장 적합한 오픈소스 라이브러리를 찾는다. 언어가 달라도 설계가 필요한 경우 PRD에 기록하여 참고한다.

---

## 배경

### 현재 상태

- `automation-candidate-recommender.js`는 매 tick bounded observations를 checkpoint에 누적하고, score ≥ 80이면 `action:"agent"` escalation을 보낸다.
- P0에서 seen_evidence 기반 dedup(TTL 25h, LRU 500)이 구현됐다(PR #1497).
- recommender는 escalation 후 무한 cooldown까지 기다리며, score 포화(saturation) 이후 새 신호가 없으면 영원히 동일 observation만 반복 처리한다.
- 현재 pipeline의 종점은 `action:"agent"` 하나뿐이다. agent가 후보를 받아 검토했는지, 승인했는지, 실제 자동화 카드/이슈가 생성됐는지 추적할 방법이 없다.

### 문제 상황

1. **Saturation(포화) 문제**: recommender가 score ≥ 80 도달 후 cooldown 내에는 재escalation 없이 동일 evidence를 계속 가산한다. 새로운 observation이 없으면 `scored=0` 상태가 반복되어 루프가 사실상 정지한다. autoresearch의 "running_min 곡선 평탄화 = saturation 신호" 와 동일한 현상이다.
2. **후보 흐름의 단절**: recommender → agent escalation 이후 다음 단계(detector, executor)가 없어 자동화 후보가 사람 메모나 Discord 대화에서 소실된다.
3. **자동화 실행 추적 불가**: 후보가 승인됐더라도 GitHub 이슈나 Kanban 카드가 생성됐는지 recommender checkpoint에 반영되지 않는다.

### 선행 연구 분석: autoresearch (karpathy/autoresearch)

> **핵심 발견**: autoresearch에는 programmatic saturation detection이 **없다**. 루프, EMA 스무딩, fast-fail, results.tsv 공유 매체의 패턴만 존재하며 포화 복구는 사람/agent가 직접 판단한다. 따라서 우리의 saturation detection + re-optimization은 이 프로젝트를 참고해 **직접 설계하는 것**이다.

**채택하는 패턴**:
- **NEVER STOP 원칙** (`program.md`): `while True` 루프는 외부에서 멈추지 않는 한 계속 실행된다. 우리도 recommender가 `complete`를 반환하더라도 런타임이 다음 tick을 예약하므로 동일하게 동작한다.
- **EMA 스무딩** (`train.py`, `ema_beta=0.9`): `smooth = 0.9 * smooth + 0.1 * current` 로 step-to-step 노이즈를 제거. 우리는 이 공식을 `scored` 카운터 스무딩에 그대로 적용한다: `ema_scored = 0.9 * ema_scored + 0.1 * scored`.
- **Fast-fail** (`train.py`): `if isnan(loss) or loss > 100: exit(1)`. 정상 경로를 기다리지 않고 즉시 실패 처리. 우리는 `scored === 0 AND deduped === total_obs` (모든 observation이 dedup됨)인 tick이 `FAST_FAIL_TICKS`(기본 2) 연속이면 saturation threshold를 기다리지 않고 즉시 reopt를 트리거한다.
- **results.tsv 공유 매체**: `commit_hash \t val_bpb \t memory_gb \t keep|discard \t description` 형식의 탭 구분 로그. 우리는 kv_meta(`routine_observation:recommender_tick:{yyyymmdd}`)를 동일한 역할로 사용한다. tick마다 `{scored, deduped, ema_scored, best_score, reopt_count}` 를 기록하면 detector가 이 history를 읽어 saturation 판단을 검증할 수 있다.
- **단일 지표 비교** (`results.tsv`의 `val_bpb` 비교): keep/discard는 이전 best 대비 개선 여부 하나로 결정. 우리 detector의 품질 게이트도 단일 score threshold로 통과/거절을 판정한다.

**채택하지 않는 것**:
- autoresearch의 re-optimization 전략은 "더 열심히 생각하라"(read papers, try radical changes)는 인간 지시이므로 알고리즘으로 직접 옮길 수 없다. 대신 우리는 **partial seen_evidence reset + diversity mode**를 발명한다.
- autoresearch는 5분 TIME_BUDGET 고정이지만 우리 루틴은 `@every 1m` cadence이므로 시간 예산 개념 대신 **tick 카운터** 기반 saturation window를 사용한다.

---

## 목표

이번 작업으로 달성해야 하는 핵심 목표: recommender 루프가 saturation 후에도 계속 돌고, 후보 흐름이 detector → executor로 연결되어 자동화 카드/이슈 생성까지 추적된다.

### P0-E: Recommender Saturation Detection & Re-optimization Loop

**Saturation 감지 (2-tier)**:
- **Fast-fail tier**: `scored === 0 AND deduped === total_obs` (전체 observation이 dedup됨)가 `FAST_FAIL_TICKS`(기본 2) 연속이면 reopt 즉시 트리거. autoresearch의 `if isnan(loss): exit(1)` 에 해당.
- **EMA tier**: `ema_scored = 0.9 * ema_scored + 0.1 * scored_this_tick` (autoresearch `ema_beta=0.9` 그대로). `ema_scored < EMA_SATURATION_THRESHOLD`(기본 0.3)가 `SATURATION_TICKS`(기본 5) 연속이면 reopt 트리거.

**Re-optimization (발명)**:
- Partial seen_evidence reset: `REOPT_WINDOW_MS`(기본 12h) 이전 항목만 삭제. 최근 12h 이내는 보존.
- Diversity mode 활성화: `DIVERSITY_BOOST_TICKS`(기본 10) 동안 underrepresented source category 우선 scoring.
- `reopt_count` 증가, `saturation_ticks` 리셋.

**Tick Log (results.tsv 아날로그)**:
- 매 tick 결과를 `routine_observation:recommender_tick:{yyyymmdd_hhmm}` kv_meta(TTL 48h)에 기록: `{scored, deduped, ema_scored, best_candidate_score, reopt_count, saturation_ticks}`.
- detector가 이 history를 읽어 포화 추이를 검증하거나 사람이 `analysis.ipynb`처럼 시각화할 수 있다.

**NEVER STOP**: tick이 `complete`를 반환해도 runtime이 다음 tick을 예약한다. reopt 이후에도 루프는 계속된다.

### P1: Automation Candidate Detector

- `automation-candidate-detector.js`: recommender escalation 이후 `routine_observation:candidate_review:{signature}` kv_meta 항목을 `ctx.observations`로 수신하는 pure consumer JS routine.
- 품질 게이트: evidence_age, score 기준치, diversity, cooldown 만료 여부를 검증하고, 통과 시 `routine_observation:candidate_approved:{signature}` kv_meta 항목을 기록할 것을 agent에 요청한다.
- 재추천 방지: 이미 approved/dispatched 된 후보는 scoring에서 제외.

### P1: Automation Executor

- `automation-executor.js`: `routine_observation:candidate_approved:{signature}` kv_meta 항목을 `ctx.observations`로 수신.
- 승인된 후보마다 GitHub Issue 생성 + Kanban 카드 생성 prompt를 `action:"agent"`로 emit.
- 실행 후 `routine_observation:candidate_dispatched:{signature}` kv_meta 항목(TTL 7d)을 기록해 executor 중복 방지.
- dispatched 항목은 recommender의 suppression/inventory와 연동해 재추천을 차단한다.

---

## 비목표

- P0-E 구현 시 recommender 외부 API 호출 또는 DB 직접 쓰기 없음 (QuickJS sandbox 제약 유지).
- detector/executor가 직접 GitHub API 호출 불가. `action:"agent"` prompt를 통해 외부 agent에게 위임.
- auto-implement: 사람 승인 없이 코드 작성, PR 생성, 서비스 재시작 자동 수행 불가.
- seen_evidence 전체 reset 불가 (P0-E re-optimization은 partial reset만 허용).

---

## 가정 및 경쌍 해석

### QuickJS 샌드박스 계약 (변경 없음)

- JS routine에서 접근 가능한 것: `agentdesk.routines.register`, `ctx.observations`, `ctx.checkpoint`, `ctx.automationInventory`, `ctx.limits`, `ctx.now`
- 반환 가능한 action: `complete`, `skip`, `pause`, `agent`
- HTTP, DB, memento, file bridge 없음

### kv_meta 파이프라인 계약

| kv_meta 키 | 생산자 | 소비자 | TTL |
|---|---|---|---|
| `routine_observation:candidate_review:{signature}` | LLM agent (recommender escalation 수신 후) | detector (ctx.observations 경유) | 48h |
| `routine_observation:candidate_approved:{signature}` | LLM agent (detector approval prompt 수신 후) | executor (ctx.observations 경유) | 72h |
| `routine_observation:candidate_dispatched:{signature}` | LLM agent (executor dispatch prompt 수신 후) | recommender suppression, detector exclusion | 7d |

### Saturation 파라미터 기본값

| 파라미터 | 기본값 | 설명 |
|---|---|---|
| `SATURATION_TICKS` | 5 | 연속 scored=0 또는 delta < threshold인 tick 수 |
| `SATURATION_DELTA_THRESHOLD` | 1.0 | score delta가 이 값 미만이면 saturation 카운터 증가 |
| `REOPT_WINDOW_MS` | 12 * 3600 * 1000 | Partial reset 기준: 이 시간보다 오래된 seen_evidence 항목 제거 |
| `DIVERSITY_BOOST_TICKS` | 10 | re-optimization 진입 후 diversity mode를 유지할 tick 수 |

### 의존 관계

- 이 PRD는 PR #1497(P0) 및 PR #101(원 recommender)이 merged 또는 active인 상태를 전제한다.
- detector와 executor는 Rust observation provider가 `candidate_review:*`/`candidate_approved:*` kv_meta 키를 `ctx.observations`에 포함하는 것을 전제한다.
- 현재 provider는 `kv_meta routine_observation:*` 전체를 읽으므로 추가 Rust 코드 없이 kv_meta 키만 쓰면 된다.

---

## 참고 문서

- [agentdesk-automation-candidate-recommender-prd.md](../agentdesk-automation-candidate-recommender/agentdesk-automation-candidate-recommender-prd.md) — 원 recommender PRD (implemented_in_pr)
- [agentdesk-observation-provider-enrichment-prd.md](../agentdesk-observation-provider-enrichment/agentdesk-observation-provider-enrichment-prd.md) — P0 enrichment PRD (p0-implemented)
- [agentdesk-observation-provider-enrichment-spec.md](../agentdesk-observation-provider-enrichment/agentdesk-observation-provider-enrichment-spec.md) — P0 enrichment spec (p0-implemented)
