---
doc_type: "prd"
schema_version: "1"
status: "p0-implemented"
topic_slug: "agentdesk-observation-provider-enrichment"
topic_folder: "agentdesk-observation-provider-enrichment"
linked_spec: "./agentdesk-observation-provider-enrichment-spec.md"
generated_by: "claude"
created_at: "2026-05-02"
updated_at: "2026-05-02"
p0_pr: "https://github.com/itismyfield/AgentDesk/pull/1497"
---

# PRD: AgentDesk Observation Provider Enrichment

> status: `p0-implemented` — PR #1497 merged. P1/P2 미착수.
> linked_spec: [agentdesk-observation-provider-enrichment-spec.md](./agentdesk-observation-provider-enrichment-spec.md)

## PRD 업데이트 규칙

1. 코드 수정 최소화 + 응집도 최대화 + 재활용 최대화
2. 모호성을 제거하고 구체적으로 적는다.
3. Spec이 목표와 정렬된 경우 단계별 TASK들을 만들어 업데이트한다.
4. 코드 구현 시 Test Code나 실 검증에 대해서도 같이 포함해서 구현한다.
5. 구현하고자 하는 목표에 가장 적합한 오픈소스 라이브러리를 찾는다. 언어가 달라도 설계가 필요한 경우 PRD에 기록하여 참고한다.

---

## 배경

- 현재 상태: recommender는 Rust provider가 만든 `ctx.observations`만 소비하고, provider는 기존 DB/kv/outbox/run source만 cap 안에서 병합한다.
  - `automation-candidate-recommender.js`는 Rust routine observation provider가 만든 `ctx.observations`만 소비한다.
  - 현재 provider는 `kv_meta`의 `routine_observation:*`, `api_friction_issues`, `message_outbox`, `routine_runs`를 읽어 100개/64KB cap 안에서 observations를 만든다.
  - 신규 observation source를 추가하려면 Rust provider를 직접 수정해야 하므로 JS 루틴만으로 실험하기 어렵다.
- 문제 상황: 반복 운영 패턴과 사용자 프롬프트 패턴을 JS 루틴만으로 observation source에 안전하게 추가하기 어렵다.
  - 반복되는 운영 패턴, 사용자 프롬프트 패턴, memento/local memory 패턴이 recommender 입력으로 안정적으로 들어오지 못한다.
  - 기존 `outbox-delivery`류 observation이 daily cap을 독점하면 다른 자동화 후보가 묻힐 수 있다.
  - harvester가 직접 memento write, 파일 write, PR/카드 생성까지 수행하면 자동화 후보 수집과 실행 부작용이 섞인다.
- 왜 지금 필요한가: routine JS 런타임에는 HTTP/DB/memento bridge가 없어 JS harvester에서 observation을 주입하는 방식은 실현 불가능하다. 대신 기존 Rust provider를 보강해 grouped observation, evidence dedup, source fair merge를 적용하는 것이 유일한 실행 가능 경로다.
  - routine JS가 `POST /api/routines/observations`를 호출할 수 없음이 `src/services/routines/loader.rs`에서 확인됐다 (HTTP bridge 미설치).
  - `routine_runs` source가 raw UUID row를 반복 노출해 recommender score가 첫 tick 이후 100으로 포화되는 문제가 확인됐다.
  - recommender에 cross-tick evidence dedup이 없어 같은 근거가 tick마다 score/evidence_count를 반복 가산한다.

## 목표

- 이번 작업으로 달성해야 하는 핵심 목표: P0는 저장소/API/provider 병합 계약을 확정하고, P1/P2는 실제 API 보정 후 harvester를 추가한다.
  - P0: recommender에 stable evidence dedup을 추가하고, `routine_runs`를 grouped observation으로 개선하며, source별 fair merge cap을 적용한다. JS harvester 주입 API는 런타임 제약으로 P0 범위 밖이다.
  - P1: prompt pattern/memento source를 Rust provider 또는 service layer로 설계한다. `GET /api/routines/runs/search` `q` 필수 제약 등 실제 API 제약을 반영한다.
  - P2: optional JS quality auditor(`ctx.observations`/checkpoint만 읽는 pure consumer)를 추가하고 category 다양성을 검증한다.
- 사용자 또는 운영 관점의 기대 효과: 자동화 후보 source가 다양해지고 반복 장애/요청 패턴이 부작용 없이 후보 근거로 축적된다.
  - 새로운 observation source 추가 비용을 Rust 코드 수정에서 JS 루틴 추가로 낮춘다.
  - 반복 장애/요청 패턴이 자동화 후보로 승격될 가능성을 높인다.
  - provider 보강은 추가 저장소나 주입 API 없이 기존 Rust 코드 수정만으로 완료할 수 있다.
  - harvester는 제안 근거만 축적하고, 실행 부작용은 recommender/agent 단계로 분리한다.

## 비목표

- 이번 단계에서 하지 않을 것:
  - JS routine에서 HTTP/DB/memento를 호출해 observation을 주입하는 방식 (런타임 제약으로 불가).
  - `POST /api/routines/observations` 주입 API 신설 (P0 범위 밖).
  - harvester가 recommendation, PR, 카드, issue를 직접 생성하지 않는다.
  - harvester가 memento `remember`/`forget`, 파일 직접 쓰기, DB 직접 우회를 수행하지 않는다.
  - observation 주입 API가 WebSocket/streaming push를 제공하지 않는다.
  - P0에서 `automation-candidate-recommender.js` 스코어링 알고리즘 자체를 바꾸지 않는다 (dedup 추가는 허용).
- 다음 단계로 미룰 것:
  - memento recall bridge가 실제 memento를 반환하도록 확장하는 작업
  - prompt-harvester의 키워드/패턴 품질 개선
  - 외부 웹자료 harvester
  - observation source별 동적 신뢰도 튜닝

## 가정 및 경쟁 해석

- 검증된 사실: 주입 API와 신규 저장소는 아직 없고, provider/run search/memory/transcript 경로는 현재 코드 기준으로 제약이 확인됐다.
  - `POST /api/routines/observations`와 `routine_observations`는 현재 코드에 없다.
  - 현재 provider 진입점은 `src/services/routines/store.rs:633`의 `fetch_recent_run_observations`다.
  - `ctx.observations` cap은 `src/services/routines/loader.rs:43`의 100개와 `src/services/routines/runtime.rs:78` 호출 경로에서 적용된다.
  - `GET /api/routines/runs/search`는 `q`가 필수라 `status=failed&limit=100`만으로는 log-harvester 요구를 만족하지 못한다: `src/server/routes/routines.rs:160`.
  - `/api/memory/recall`은 현재 memento recall bridge가 TBD이고 local memory fallback을 반환한다: `src/server/routes/memory_api.rs:113`.
  - prompt source 후보는 `/api/agents/:id/transcripts`가 실제 경로다: `src/server/routes/agents.rs:1720`.
- 아직 확인이 필요한 가정: P0는 Rust provider 보강으로 결정됐다. P1 설계에서 결정해야 할 사항만 남아 있다.
  - `routine_observations` 별도 테이블 신설은 P0에서 불필요한 것으로 결정됐다. `kv_meta routine_observation:*`를 precomputed digest surface로 활용한다.
  - memento source는 real memento bridge와 local fallback 중 무엇을 쓸지 P1 착수 전에 결정해야 한다.
  - transcript source는 `GET /api/agents/{id}/transcripts?limit=50`이 실제 경로로 확인됐지만, Rust provider 통합 방식은 P1 설계에서 결정해야 한다.
- 검토했지만 채택하지 않은 해석 / 대안: recommender 직접 수집, harvester 직접 실행 부작용, full scan 방식은 범위와 안정성 때문에 제외한다.
  - recommender가 직접 memento/log/transcript를 읽는 방식은 단일 책임을 깨고 실패 격리를 어렵게 하므로 채택하지 않는다.
  - harvester가 바로 PR/카드를 생성하는 방식은 부작용이 커서 observation 주입 단계에서는 채택하지 않는다.
  - 전체 로그/전체 memory full scan은 비용과 개인정보/노이즈 위험 때문에 채택하지 않는다.

## 사용자 시나리오 _(선택 — 인프라/플랫폼 PRD는 생략 가능)_

- 누가 사용하는가: AgentDesk 운영자와 자동화 후보 recommender를 쓰는 에이전트
- 어떤 상황에서 사용하는가: 반복 장애, 반복 프롬프트, 반복 루틴 실패가 누적되지만 자동화 후보로 잘 올라오지 않는 상황
- 기대하는 결과는 무엇인가: bounded evidence가 `ctx.observations`에 들어가고, recommender가 더 다양한 카테고리의 자동화 후보를 제안한다.

## 요구사항

### 기능 요구사항

- [ ] `routine_runs` source는 `(script_ref, action, status, error_fingerprint)` 기준으로 grouped observation을 생성한다. `occurrences`, `latest_at`, `sample_evidence_refs` 포함.
- [ ] recommender는 stable `evidence_ref` 또는 `source|category|signature|summary_hash` 기반 dedup으로 동일 evidence를 tick마다 반복 가산하지 않는다.
- [ ] recommender checkpoint는 `seen_evidence_ttl ≥ 25h`, `max_entries = 500 LRU`의 bounded dedup 구조를 유지한다.
- [ ] provider source별 fair merge cap을 적용한다: `kv_meta` 20, `api_friction_issues` 20, `message_outbox` 20, `routine_runs` 40, 전체 100개/64KB cap 유지.
- [ ] P1 source (transcript, memento)는 실제 존재하는 Rust provider/service 경로를 사용한다. 존재하지 않는 경로는 blocker로 남긴다.

### 비기능 요구사항 _(구체적인 기준이 있을 때만 작성)_

- 성능: provider tick에서 source별 cap 후 전체 100개/64KB cap을 넘기지 않는다.
- 안정성: `routine_runs` grouping은 DB 오류 시 빈 observations로 graceful fallback한다.
- 유지보수성: 신규 provider source 추가 시 recommender scoring contract를 바꾸지 않는다.
- 보안/운영: P1 이후 transcript source는 내부 service 호출 또는 인증된 API만 사용한다.

## 제약사항 및 의존성

- 언어 / 런타임 / 프레임워크 제약:
  - Rust server는 기존 axum/sqlx/PostgreSQL 경로를 사용한다.
  - JS harvester는 기존 routine runtime과 checkpoint 모델을 사용한다.
- 외부 API 또는 서비스 의존성:
  - P1 memento source는 `/api/memory/recall` 또는 memento bridge 상태에 의존한다.
  - P2 prompt source는 `/api/agents/:id/transcripts`에 의존한다.
- 환경변수 / 설정 의존성:
  - routines가 enabled이고 PostgreSQL pool이 있어야 한다.
  - 별도 API token은 P0 범위에서 만들지 않는다.
- 호환성 또는 버전 제약:
  - 기존 `ctx.observations` JSON contract, category/signature/weight 필드를 깨면 안 된다.

## 참고 오픈소스 및 타 언어 레퍼런스

- 검토한 오픈소스 라이브러리 및 저장소 URL: 신규 라이브러리는 채택하지 않고 기존 `sqlx`, `axum`, `serde`, PostgreSQL `ON CONFLICT`/TTL query 패턴을 재사용한다.
  - 신규 라이브러리는 필요 없다. 기존 `sqlx`, `axum`, `serde`, PostgreSQL `ON CONFLICT`/TTL query 패턴을 재사용한다.
- 선택 또는 제외 이유:
  - cron/queue 라이브러리를 추가하지 않는다. 기존 routine scheduler와 checkpoint가 충분하다.
  - vector DB나 full-text engine을 추가하지 않는다. P0는 bounded structured observation 저장이 목표다.
- 타 언어 참고 구현 또는 설계 포인트:
  - event ingestion API는 append/upsert + TTL + bounded consumer contract를 분리하는 구조를 따른다.

## 구현 및 영향 범위

- 수정 대상 파일 / 모듈: routine provider/store, routine routes/docs/tests, migration/schema, P1/P2 monitoring JS 루틴이 대상이다.
  - `src/services/routines/store.rs`
  - `src/services/routines/runtime.rs`
  - `src/services/routines/loader.rs`
  - `src/server/routes/routines.rs`
  - `src/server/routes/docs.rs`
  - `src/db/schema.rs` 또는 PostgreSQL migration 위치
  - `routines/monitoring/*.js` (P1/P2)
- 제외 범위: recommender scoring rewrite, direct memento write, direct DB/file access from JS, external web harvester는 제외한다.
  - recommender scoring rewrite
  - direct memento write
  - direct DB access from JS
  - external web harvester
- 회귀 위험이 있는 기존 동작:
  - `ctx.observations` cap, payload byte cap, existing provider source ordering
  - routine run search API의 `q` 필수 contract
  - API docs tests under `src/server/routes/routes_tests.rs`

## 단계별 TASK

> 우선순위: P0 (필수) → P1 (중요) → P2 (개선)
> 각 TASK의 선행조건·완료 정의는 복잡한 경우에만 작성한다.

### P0

> 구현 순서: **P0-A** (routine_runs grouping) → **P0-B** (evidence dedup) → **P0-C** (fair merge)

- [P0-A] [ ] provider의 `routine_runs` source를 raw recent rows에서 grouped observation으로 개선한다. group_key: `(script_ref, action, status, error_fingerprint)`, `occurrences`/`latest_at`/`sample_evidence_refs` 포함.
- [P0-B] [ ] `automation-candidate-recommender.js`에 stable evidence key dedup을 추가한다. `evidence_ref` 1순위, `source|category|signature|summary_hash` 2순위.
- [P0-B] [ ] recommender checkpoint에 bounded `seen_evidence` TTL/LRU를 추가한다. `seen_evidence_ttl ≥ 25h`, `max_entries = 500 LRU`.
- [P0-C] [ ] provider source별 fair merge cap을 적용한다: `kv_meta` 20, `api_friction_issues` 20, `message_outbox` 20, `routine_runs` 40, 전체 100개/64KB cap 유지.
- [ ] P0 변경에 대한 Rust/JS 테스트를 추가한다.

### P1

- [ ] prompt pattern source를 Rust provider 또는 service layer로 설계한다. transcript source는 `GET /api/agents/{id}/transcripts?limit=50` 또는 내부 service 호출만 사용한다.
- [ ] memento/local-memory source 전략을 결정한다. real memento bridge가 필요하면 bridge 작업을 별도 선행 PR로 분리한다.
- [ ] `routine_runs` 외 source에서 반복 패턴을 추가로 집계할 경우 full scan 없이 bounded window와 source cap을 적용한다.
- [ ] 신규 provider source를 `/api/docs` 또는 내부 route docs에 반영한다.

### P2

- [ ] 필요할 경우 `routines/monitoring/observation-quality-auditor.js`를 추가한다. 이 루틴은 `ctx.observations`와 checkpoint만 읽고, HTTP/DB/memento/file I/O를 전제로 하지 않는다.
- [ ] quality auditor는 중복 evidence, 낮은 다양성, 반복되는 low-value category를 감지해 `{ action: "agent" }` handoff 또는 `{ action: "complete" }`만 반환한다.
- [ ] JS host bridge가 정말 필요하면 별도 PRD/PR로 분리하고, routine sandbox 권한 모델과 API exposure risk를 먼저 검토한다.

## 검증 계획

### Test Code

- P0 acceptance를 닫는 Test Code / 실 검증 매핑: API validation, TTL cleanup, provider cap, idempotency를 테스트와 실 주입으로 닫는다.
  - API validation: 51개 요청, 513B summary, 정상 50개 요청
  - TTL: 만료 observation provider 제외 및 cleanup 확인
  - cap: 기존 source 90개 + injected 20개에서 100개 초과하지 않음
  - idempotency: 동일 `(source, signature)` 재주입 시 중복 row가 늘지 않음
- 추가하거나 수정할 테스트: provider 단위 테스트, API docs/route 테스트, P1 이후 routine JS policy 테스트를 추가한다.
  - `src/services/routines/store.rs` provider 단위 테스트
  - `src/server/routes/routes_tests.rs` API docs/route 테스트
  - P1 이후 `npm run test:policies` 또는 routine JS 테스트
- 커버해야 하는 핵심 케이스:
  - DB 오류 시 recommender tick이 빈 observations로 계속 진행
  - source 필드를 recommender가 특별 취급하지 않음

### 실 검증

- 직접 실행 또는 수동 검증 절차: `/api/docs` 노출 확인, 샘플 observation 주입, 다음 recommender tick 포함 여부를 확인한다.
  - 로컬 서버에서 `/api/docs/automation/routines`에 endpoint 노출 확인
  - `POST /api/routines/observations`로 샘플 1건 주입
  - 다음 routine tick에서 recommender의 `ctx.observations`에 포함되는지 로그 확인
- 기대 결과:
  - API는 `{ accepted: 1 }`을 반환한다.
  - recommender는 기존 scoring contract로 후보 evidence를 누적한다.

### 회귀 검증

- 영향 범위 기준 회귀 확인 항목:
  - `cargo fmt --all --check`
  - `cargo check --all-targets`
  - `npm run test:policies`
  - routine provider tests
  - API docs route tests

## 리스크 및 대응

| 심각도 | 리스크 | 대응 방안 |
|---|---|---|
| 높음 | localhost-only API가 외부 노출 경로에 붙으면 임의 observation 주입 가능 | route mount와 bind/address 가정을 문서화하고, 외부 노출 표면에는 붙이지 않는다 |
| 높음 | harvester가 full scan 또는 write-side 도구를 호출해 비용/부작용 증가 | bounded window, checkpoint dedup, 금지 행동 테스트를 둔다 |
| 중간 | 기존 provider cap을 넘겨 recommender prompt가 커짐 | 기존 100개/64KB cap을 통합 배열에 그대로 적용한다 |
| 중간 | memento recall API가 실제 memento를 반환하지 않아 P1 품질 저하 | P1 선행조건으로 memento bridge 또는 local fallback 전략을 결정한다 |
| 낮음 | 테이블이 TTL cleanup 실패로 누적됨 | provider tick 시작 시 cleanup하고 인덱스를 둔다 |

## 배포 / 마이그레이션 / 롤백 _(선택 — 해당 없으면 생략)_

- 배포 순서:
  - migration 적용 → API/provider 배포 → docs 확인 → harvester JS 별도 활성화
- 데이터 또는 설정 마이그레이션 필요 여부:
  - `routine_observations` 테이블을 선택하면 PostgreSQL migration이 필요하다.
- 롤백 방법:
  - harvester routines 비활성화 → API/provider 코드 revert → 필요 시 테이블 drop migration 또는 unused table 유지
- 배포 후 확인할 지표:
  - routine tick observation count
  - recommender category 분포
  - harvester run failure count
  - `routine_observations` row count와 expired row count

## 완료 기준

- [ ] P0 API가 검증된 contract로 동작한다.
- [ ] Provider가 injected observations를 기존 cap 안에서 병합한다.
- [ ] TTL/중복/validation 테스트가 통과한다.
- [ ] API docs와 실 검증 절차가 갱신된다.
- [ ] P1/P2 blocker가 문서에 남아 있거나 해결됐다.
- 리뷰어 / 승인자: kunkunGames
