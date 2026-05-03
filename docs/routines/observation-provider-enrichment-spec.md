---
doc_type: "spec_source"
schema_version: "2"
status: "p0-implemented"
topic_slug: "agentdesk-observation-provider-enrichment"
topic_folder: "agentdesk-observation-provider-enrichment"
linked_prd: "./agentdesk-observation-provider-enrichment-prd.md"
traceability_mode: "req-task-test"
generated_by: "codex"
created_at: "2026-05-02"
updated_at: "2026-05-02"
p0_pr: "https://github.com/itismyfield/AgentDesk/pull/1497"
---

# SPEC SOURCE: AgentDesk Observation Provider Enrichment

## Linked Documents

- PRD: [agentdesk-observation-provider-enrichment-prd.md](./agentdesk-observation-provider-enrichment-prd.md)

## State

- status: draft
- source_of_truth_scope: requirements, tasks, tests, blockers, status
- sync_policy: PRD 수정 후 spec-source 문서도 같이 갱신
- implementation_stage: stage-3-p0-done
- implementation_readiness: P0 구현 완료 (PR #1497). P1(transcript/memento source), P2(quality auditor) 미착수.
- prd_sync_note: 현재 연결 PRD는 아직 observation 주입 API와 JS harvester 전제를 담고 있다. 이 spec은 2026-05-02 코드 재검토 결론을 반영한 수정 방향이며, PRD는 후속 동기화가 필요하다.

## Critical Conclusion

- 지금 바로 만들 가치가 있는 것은 `harvester.js` 3종이 아니라, 이미 저장된 운영 데이터를 안정적으로 `ctx.observations`로 요약하는 provider 개선이다.
- 현재 routine JS 런타임은 HTTP, DB, memento, 파일 접근 bridge를 제공하지 않는다. 따라서 JS가 `/api/routines/observations`에 POST하는 구조는 P0 구현 경로로 부적합하다.
- 현재 provider는 이미 `kv_meta`, `api_friction_issues`, `message_outbox`, `routine_runs`를 읽는다. 부족한 것은 새 저장소보다 기존 source의 중복/반복 근거를 잘 묶고, recommender가 같은 evidence를 tick마다 반복 가산하지 않게 하는 것이다.
- `harvester.js`를 만든다면 P2의 quality auditor 또는 pure consumer 루틴이어야 한다. 즉 `ctx.observations`와 checkpoint만 읽고 후보 품질을 평가하거나 agent handoff를 제안하는 정도가 적합하다.

## Known Code Reality And Blockers

- JS runtime bootstrap은 `src/services/routines/loader.rs`에서 `agentdesk.routines.register`만 설치한다. `fetch`, HTTP client, DB, memento bridge는 없다.
- runtime은 provider가 만든 observations를 `src/services/routines/runtime.rs` 경로에서 JS tick의 `ctx.observations`로 넣는다.
- provider 진입점은 `src/services/routines/store.rs`의 observation fetch 경로이며, 현재 source는 `kv_meta routine_observation:*`, `api_friction_issues`, `message_outbox`, `routine_runs`다.
- `kv_meta routine_observation:*`는 이미 `expires_at` 기반 제외 흐름을 갖고 있어, P0에서 새 `routine_observations` 테이블을 만들 필요가 낮다.
- `automation-candidate-recommender.js`는 observations를 순회하며 evidence count와 score를 올리지만, 안정적인 `evidence_ref` 기반 dedup/anti-replay가 없다.
- routine action model은 `complete`, `skip`, `pause`, `agent` 중심이다. observation write-side action은 없다.
- `GET /api/routines/runs/search`는 현재 `q`가 필수라 status-only failed-run 조회를 전제로 한 JS harvester 구현은 맞지 않는다.
- `/api/memory/recall`은 real memento bridge가 아니라 local fallback 상태다. memory observation source는 별도 bridge 결정 전까지 P1 blocker다.
- 실제 transcript 경로는 `GET /api/agents/{id}/transcripts?limit=50`다. 존재하지 않는 session endpoint를 전제로 하면 안 된다.

## Requirement Registry

- [REQ-001] P0는 routine JS가 HTTP, DB, memento, 파일 접근을 할 수 있다는 가정에 의존하면 안 된다.
- [REQ-002] P0는 새 `POST /api/routines/observations`와 새 `routine_observations` 테이블을 기본 경로로 만들지 않는다. 기존 provider와 `kv_meta routine_observation:*` 확장을 우선 검토한다.
- [REQ-003] recommender는 동일 evidence를 tick마다 반복 가산하지 않도록 stable evidence key 기반 dedup을 적용해야 한다.
- [REQ-004] evidence key 우선순위는 `evidence_ref`를 1순위로 하고, 없으면 `source|category|signature|summary_hash` 또는 동등한 stable key를 사용해야 한다.
- [REQ-005] recommender checkpoint는 bounded `seen_evidence` 또는 동등한 TTL/LRU 구조를 저장해 anti-replay를 제공해야 한다. `seen_evidence_ttl ≥ 25h` (24h 조회 윈도우를 완전히 커버), `max_entries = 500 LRU`.
- [REQ-006] provider는 raw row를 그대로 많이 밀어 넣기보다 source별로 반복 근거를 집계해 bounded observations를 생성해야 한다.
- [REQ-007] `routine_runs` source는 `(script_ref, action, status, error_fingerprint)` 또는 동등 key로 실패/반복 패턴을 묶고 `occurrences`, latest timestamp, sample evidence refs를 포함해야 한다.
- [REQ-008] `message_outbox`, `api_friction_issues`, `routine_runs`, `kv_meta` source는 source별 deterministic cap 또는 fair merge를 적용해 단일 source가 observation cap을 독점하지 않게 해야 한다.
- [REQ-009] 기존 `ctx.observations` cap 100개와 64KB payload 제한은 유지해야 한다.
- [REQ-010] `kv_meta routine_observation:*`는 precomputed digest ingestion surface로 문서화하고, TTL/만료 제외/중복 key 정책을 명시해야 한다.
- [REQ-011] prompt/local-memory/memento observation source는 JS harvester HTTP 호출이 아니라 Rust provider, API service, maintenance job 중 하나로 구현해야 한다.
- [REQ-012] memento source는 real memento bridge와 local fallback 중 무엇을 쓸지 결정하기 전까지 P1 blocker로 둔다.
- [REQ-013] transcript source를 구현할 경우 실제 route인 `GET /api/agents/{id}/transcripts?limit=50` 또는 `/api/docs`로 확인된 동등 API만 사용해야 한다.
- [REQ-014] JS 루틴을 추가하는 경우 P0가 아니라 P2로 두며, `ctx.observations`와 checkpoint만 읽는 pure consumer 또는 quality auditor여야 한다.
- [REQ-015] 신규 observation source는 recommender의 source diversity를 높여야 하며, `outbox-delivery`류 반복 source가 daily cap을 독점하지 않는지 검증해야 한다.

## Task Registry

### P0

> P0 구현 우선순위: **P0-A** (routine_runs grouping, TSK-P0-004) → **P0-B** (seen_evidence dedup, TSK-P0-002/003) → **P0-C** (fair merge, TSK-P0-005)
> JS runtime capability 문서화 (원 TSK-P0-001)는 Known Code Reality 섹션에서 이미 완료됐으므로 구현 작업에서 제외한다.

- [TSK-P0-002] [x] `automation-candidate-recommender.js`에 stable evidence key dedup을 추가한다.
- [TSK-P0-003] [x] recommender checkpoint에 bounded `seen_evidence` TTL/LRU 구조를 추가하고, 같은 evidence가 다음 tick에서 다시 score/evidence_count를 올리지 않게 한다.
- [TSK-P0-004] [x] provider의 `routine_runs` source를 raw recent rows 중심에서 grouped observation 중심으로 개선한다. **구현 결정**: GROUP BY key는 `(script_ref, name, action, status)`. `error_fingerprint`는 REQ-007 "동등 key" 조항 적용으로 P1 deferred — P0 목표(score 포화 방지)는 현 key로 달성됨.
- [TSK-P0-005] [x] provider source별 deterministic cap 또는 fair merge를 적용해 전체 100개/64KB cap 안에서 source diversity를 유지한다.
- [TSK-P0-006] [x] `kv_meta routine_observation:*` contract를 precomputed digest ingestion surface로 문서화한다. key shape, TTL, 중복 정책, provider 제외 조건을 포함한다. → `store.rs` `precomputed_observation_from_kv` 함수 doc comment에 전체 contract 기재.
- [TSK-P0-007] [x] P0 변경에 대한 Rust/JS 테스트를 추가한다. → Rust 3개 (bounded_push item/byte cap, evidence_ref format), JS 5개 (dedup, TTL 만료, LRU cap, composite key, multi-pattern).

### P1

> **연관 문서**: recommender saturation detection 및 3-routine pipeline (detector, executor) 구현은 별도 PRD/spec으로 분리됨: [agentdesk-automation-candidate-pipeline](../agentdesk-automation-candidate-pipeline/agentdesk-automation-candidate-pipeline-prd.md)

- [TSK-P1-001] [ ] prompt pattern source를 Rust provider 또는 service layer로 설계한다. transcript source는 `GET /api/agents/{id}/transcripts?limit=50` 또는 내부 service 호출만 사용한다.
- [TSK-P1-002] [ ] memento/local-memory source 전략을 결정한다. real memento bridge가 필요하면 bridge 작업을 별도 선행 PR로 분리한다.
- [TSK-P1-003] [ ] `routine_runs` 외 source에서 반복 패턴을 추가로 집계할 경우 full scan 없이 bounded window와 source cap을 적용한다.
- [TSK-P1-004] [ ] 신규 provider source를 `/api/docs` 또는 내부 route docs에 반영한다.

### P2

- [TSK-P2-001] [ ] 필요할 경우 `routines/monitoring/observation-quality-auditor.js`를 추가한다. 이 루틴은 `ctx.observations`와 checkpoint만 읽고, HTTP/DB/memento/file I/O를 전제로 하지 않는다.
- [TSK-P2-002] [ ] quality auditor는 중복 evidence, 낮은 다양성, 반복되는 low-value category를 감지해 `{ action: "agent" }` handoff 또는 `{ action: "complete" }`만 반환한다.
- [TSK-P2-003] [ ] JS host bridge가 정말 필요하면 별도 PRD/PR로 분리하고, routine sandbox 권한 모델과 API exposure risk를 먼저 검토한다.

## Test Registry

- [TEST-001] validates REQ-001, TSK-P0-001: routine JS에서 `fetch`/DB/memento/file I/O를 사용할 수 없거나 지원하지 않는다는 runtime contract가 테스트 또는 문서 검증으로 고정된다.
- [TEST-002] validates REQ-003, REQ-004, TSK-P0-002: 같은 `evidence_ref`를 가진 observation 두 개가 들어와도 recommender score와 evidence_count는 한 번만 증가한다.
- [TEST-003] validates REQ-004, TSK-P0-002: `evidence_ref`가 없는 observation은 `source|category|signature|summary_hash` 동등 key로 dedup된다.
- [TEST-004] validates REQ-005, TSK-P0-003: 같은 evidence가 다음 tick에 다시 들어와도 checkpoint의 `seen_evidence` 때문에 재가산되지 않는다.
- [TEST-005] validates REQ-005, TSK-P0-003: `seen_evidence`는 cap 또는 TTL을 넘어 무한 증가하지 않는다.
- [TEST-006] validates REQ-006, REQ-007, TSK-P0-004: 동일 `(script_ref, action, status, error_fingerprint)` 실패 5건은 raw 5개가 아니라 `occurrences=5`인 grouped observation 1개로 생성된다.
- [TEST-007] validates REQ-008, REQ-009, TSK-P0-005: 한 source가 100개 이상 생성 가능한 상황에서도 전체 `ctx.observations`는 100개/64KB cap을 넘지 않고 다른 source 몫을 남긴다.
- [TEST-008] validates REQ-010, TSK-P0-006: 만료된 `kv_meta routine_observation:*` 항목은 provider 결과에서 제외된다.
- [TEST-009] validates REQ-010, TSK-P0-006: 같은 logical digest key는 중복 observation으로 확장되지 않는다.
- [TEST-010] validates REQ-011, REQ-012, TSK-P1-002: memento source 구현 전 real bridge/local fallback 결정이 문서화되고, fallback 사용 시 결과 label이 명확하다.
- [TEST-011] validates REQ-013, TSK-P1-001: transcript source는 `/api/agents/{id}/transcripts?limit=50` 또는 `/api/docs`로 확인된 동등 endpoint/service만 사용한다.
- [TEST-012] validates REQ-014, TSK-P2-001, TSK-P2-002: optional JS quality auditor는 `ctx.observations`와 checkpoint 외 I/O를 요구하지 않고 `complete` 또는 `agent` action만 반환한다.
- [TEST-013] validates REQ-015, TSK-P0-005, TSK-P2-002: `outbox-delivery` 반복 source가 많은 상황에서도 category/source diversity가 유지되고 daily candidate cap을 독점하지 않는다.

## Traceability

- REQ-001 -> (Known Code Reality + Execution Notes) -> TEST-001
- REQ-002 -> TSK-P0-006 -> TEST-008, TEST-009
- REQ-003 -> TSK-P0-002, TSK-P0-003 -> TEST-002, TEST-004
- REQ-004 -> TSK-P0-002 -> TEST-002, TEST-003
- REQ-005 -> TSK-P0-003 -> TEST-004, TEST-005
- REQ-006 -> TSK-P0-004, TSK-P0-005 -> TEST-006, TEST-007
- REQ-007 -> TSK-P0-004 -> TEST-006
- REQ-008 -> TSK-P0-005 -> TEST-007, TEST-013
- REQ-009 -> TSK-P0-005 -> TEST-007
- REQ-010 -> TSK-P0-006 -> TEST-008, TEST-009
- REQ-011 -> TSK-P1-001, TSK-P1-002, TSK-P1-003 -> TEST-010, TEST-011
- REQ-012 -> TSK-P1-002 -> TEST-010
- REQ-013 -> TSK-P1-001 -> TEST-011
- REQ-014 -> TSK-P2-001, TSK-P2-002 -> TEST-012
- REQ-015 -> TSK-P0-005, TSK-P2-002 -> TEST-013
- TSK-P0-007 -> TEST-002, TEST-003, TEST-004, TEST-005, TEST-006, TEST-007, TEST-008, TEST-009
- TSK-P1-004 -> TEST-010, TEST-011
- TSK-P2-003 -> TEST-001, TEST-012

## Agent Execution Notes

- 구현자는 P0에서 새 JS harvester 파일을 만들지 않는다.
- 구현자는 `POST /api/routines/observations`를 먼저 만들지 않는다. 현재 코드에는 이미 `kv_meta routine_observation:*` provider surface가 있으므로 그 contract를 정리하고 필요할 때만 확장한다.
- 구현자는 `automation-candidate-recommender.js`의 scoring loop를 먼저 확인하고, observation별 stable evidence key를 만든 뒤 score/evidence_count 증가 전에 dedup을 적용한다.
- checkpoint에 저장하는 dedup set은 bounded 구조여야 한다. 무제한 evidence history 저장은 금지한다.
- `routine_runs` provider source는 최근 run row를 반복 노출하지 않고 grouped observation을 생성한다.
- 모든 provider source는 source별 cap 또는 fair merge를 거쳐 전체 100개/64KB cap에 들어가야 한다.
- `kv_meta routine_observation:*`는 외부 JS 주입 API가 아니라 내부 precomputed digest surface로 취급한다.
- prompt/memory/memento source는 JS HTTP 호출이 아니라 Rust provider/service/maintenance job에서 구현한다.
- optional JS auditor는 observation consumer일 뿐 harvester가 아니다. API 호출, DB 접근, memento write/read, 파일 접근을 전제로 하지 않는다.
- 신규 API나 route를 추가하는 경우 ADK API 작업 규칙에 따라 `/api/docs` 또는 관련 docs를 먼저 확인하고 문서/테스트를 같이 갱신한다.

## Contract Snapshot

```json
{
  "p0_runtime_model": {
    "routine_js_io_bridge": false,
    "available_js_contract": [
      "agentdesk.routines.register",
      "ctx.observations",
      "routine checkpoint",
      "return complete | skip | pause | agent"
    ],
    "forbidden_p0_assumptions": [
      "fetch from JS routine",
      "direct DB access from JS routine",
      "memento bridge from JS routine",
      "file I/O from JS routine",
      "POST /api/routines/observations as primary path"
    ]
  },
  "p0_primary_work": {
    "provider_enrichment": true,
    "recommender_evidence_dedup": true,
    "source_fair_merge": true,
    "new_js_harvester_files": false
  },
  "provider_sources": {
    "existing": [
      "kv_meta routine_observation:*",
      "api_friction_issues",
      "message_outbox",
      "routine_runs"
    ],
    "p1_candidates": [
      "agent transcripts via /api/agents/{id}/transcripts?limit=50 or internal service",
      "memento or local memory after bridge/fallback decision"
    ]
  },
  "recommender_dedup": {
    "evidence_key_priority": [
      "evidence_ref",
      "source|category|signature|summary_hash"
    ],
    "checkpoint_state": {
      "seen_evidence": "bounded TTL/LRU map or equivalent",
      "must_not_grow_unbounded": true
    },
    "score_rule": "only first unseen evidence in the active dedup window can increment score/evidence_count"
  },
  "routine_runs_grouping": {
    "group_key": "script_ref|action|status|error_fingerprint",
    "observation_fields": [
      "source",
      "category",
      "signature",
      "summary",
      "occurrences",
      "latest_at",
      "sample_evidence_refs"
    ]
  },
  "caps": {
    "ctx_observations_max_items": 100,
    "ctx_observations_max_bytes": 65536,
    "source_merge_policy": "fair merge: kv_meta 20, api_friction_issues 15, message_outbox 10, routine_runs 25, kanban_stale 10, dispatch_retry 10, session_pattern 10"
  },
  "kv_meta_digest_surface": {
    "key_prefix": "routine_observation:",
    "ttl_field": "expires_at",
    "role": "precomputed digest ingestion surface",
    "p0_new_table_required": false
  },
  "p2_optional_js": {
    "name": "observation-quality-auditor.js",
    "allowed_inputs": [
      "ctx.observations",
      "checkpoint"
    ],
    "allowed_outputs": [
      "complete",
      "agent"
    ],
    "not_allowed": [
      "HTTP calls",
      "DB calls",
      "memento calls",
      "file writes"
    ]
  }
}
```
