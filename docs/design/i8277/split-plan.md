# #8277 대형 파일 분할 계획

기준: origin/main `881cc5c5d5` (2026-11-21). 대상은 이슈 본문의 10개 파일이다.
동작 변경 없이 이동만 하고, 공개 경로는 부모 모듈의 재수출로 유지한다.

## 원칙

- 조각은 부모 파일의 child 모듈로만 나눈다. 모듈 subtree가 바뀌지 않으므로
  #7353의 목표 층(`base ← db/provider ← services ← discord ← server ← cli`)에서
  각 파일이 갈 crate는 부모 모듈의 crate 그대로다.
- 시험 ID를 보존한다. inline `mod x_tests { … }`는 같은 이름의 child 파일로
  옮긴다(모듈 경로가 같아 ID가 같다). 시험 모듈을 다른 부모 아래로 옮기지 않는다.
- 층 검사(`docs/architecture/layers.md`)는 논리 모듈 단위로 역방향 edge ID를 센다.
  assembly(5) 층인 `cli`·`server`는 역방향이 있을 수 없어 prod child를 자유롭게 만든다.
  application(3)·adapter(2) 층 파일(`dispatch`·`db`·`config`·`services::routines`)은
  prod child가 부모의 역방향 edge를 새 ID로 옮기면 error다. 이 파일들은 조각 착수 때
  `python3 scripts/audit_maintainability.py --check`로 부모의 역방향 edge를 먼저 뽑고,
  그 참조를 가진 구간은 부모에 남긴다.
- 경로에 묶인 기준선(clippy allow 발생 수, destructive call site, SQL 실행 표면,
  config snapshot reader 예외, 소스 스캔 시험)이 가리키는 함수는 부모 파일에 남긴다.
  기준선 수치·항목은 이 계획의 조각에서 바꾸지 않는다.
- giant-file 기준선(`scripts/audit_maintainability_giant_baseline.toml`)을 낮추는 일과
  registry 항목 정리는 마지막 조각에서 한 번에 한다(수치 변경이라 승인 대상).
- child 하나는 prod 1,000줄 아래로 둔다(새 giant를 만들지 않는다).

## 겹침 조사 (최근 3일 `refs/review/*` 288개, 열린 PR 1건)

대상 파일을 바꾸는 미착지 ref를 `git diff origin/main...<ref>`로 찾고, 그 변경이
main에 이미 들어갔는지 역적용(`git apply --check -R`)으로 갈랐다.

| 대상 | 살아 있는 겹침 | 이미 착지 |
|---|---|---|
| `src/services/routines/store.rs` | PR #8291 `land-batch-1120a5`(#8219 포함, +43/−88, `store/` child 3개 추가) | — |
| `src/services/routines/loader.rs` | 없음(같은 디렉터리의 `mod.rs`·`agent_executor`는 #8291이 바꾼다) | — |
| `src/db/postgres.rs` | 없음(child `postgres/agent_sync.rs`는 #8291이 바꾼다) | #8264, #8207, batch-1118b·1119c |
| `src/config.rs` | 없음(child `config/session_hosts.rs`는 `i8280-s2-1`이 바꾼다) | #8157, #8217 |
| `src/server/mod.rs` | 없음 | #8157, #7438 |
| `src/server/routes/health_api.rs` | 없음 | #8157 |
| `src/cli/doctor/orchestrator.rs` | 없음 | #8157 |
| `src/dispatch/dispatch_context.rs` | 없음 | — |
| `src/db/auto_queue/phase_gates.rs` | 없음 | — |

- #7353 S4(db·server·config) 조각: 리뷰 ref가 아직 없다(S1 `land-i7353s1`만 착지).
  `config.rs`와 `db/postgres.rs`는 S4가 시작되면 겹치므로 착수 때 소유를 다시 확인한다.
- #7651 R1~R7, #8141, `land-batch-1120b`: 대상 10개 파일과 그 child 디렉터리를 바꾸지 않는다.
- #8216 루틴 스택: #8219가 `store.rs`를 바꾼다. 다른 ref는 없다.

## 책임 지도와 조각

줄 수는 `wc -l`이고 괄호 안은 prod 구간(끝의 inline 시험 모듈 제외)이다.

### 2. `src/db/auto_queue/phase_gates.rs` — 3,001 (prod 969) · adapter · 겹침 0

- 970–1262 `current_batch_phase_pg_tests`, 1263–3002 `reconcile_phase_gate_pg_tests`
  → 같은 이름의 child 시험 파일(기존 `phase_gates/` 디렉터리).
- 결과 예상 969줄. prod 이동이 없어 층 edge가 바뀌지 않는다. **조각 S2**.

### 3. `src/services/routines/loader.rs` — 3,266 (prod 968) · adapter · 겹침 0

- 969–3267 `tests` → child `tests`(기존 `loader/` 디렉터리). 결과 예상 968줄. **조각 S2**.

### 4. `src/server/routes/health_api.rs` — 3,180 (prod 1,193) · assembly · 겹침 0

- 1194–3169 `tests` → child `tests`. 결과 예상 약 1,200줄. **조각 S2**.
- 후속(선택): `stale_mailbox_repair_handler`(403줄) → child `stale_mailbox_repair`. **조각 S6**.

### 5. `src/db/postgres.rs` — 3,537 (prod 1,389) · adapter · 직접 겹침 0

- 1390–3538 `tests` → child `tests`. 결과 예상 약 1,390줄. **조각 S2**.
- 후속: 629–1389의 `#[cfg(test)]` 시험 DB 수명 헬퍼(생성·소유 등록·drop) →
  child `test_database`, 부모에서 `pub(crate) use`. 결과 예상 약 650줄. **조각 S6**
  (#8291 착지 뒤, #7353 S4 db 조각과 소유 확인 뒤).

### 6. `src/cli/doctor/orchestrator.rs` — 3,839 (prod 약 3,680) · assembly · 겹침 0

| 구간 | 책임 | 옮길 곳 |
|---|---|---|
| 29–506 | `Check`·`FixAction` 모델과 표시 | child `check_model` |
| 507–845 | qwen 설정·auth·runtime 점검 | child `qwen_checks` |
| 849–1248, 2086–2286 | provider 점검(opencode, binding, CLI) | child `provider_checks` |
| 1259–1538 | 권한·connector 점검, JSON 보고 조립 | child `report_build` |
| 1539–1820 | safe fix 적용·service fix | child `fix_apply` |
| 1821–2085 | Discord bot 점검 | child `discord_checks` |
| 2287–2558 | runtime path·server·degraded 점검 | child `runtime_checks` |
| 2559–2806, 2978–3058 | service manager 점검(OS별) | child `service_manager` |
| 2807–2977 | fd headroom 점검 | 기존 child `fd_usage` 옆 `fd_headroom` |
| 3069–3380 | DB 무결성·PG 연결·stale DB 파일 | child `db_checks` |
| 3381–3618 | GitHub registry·disk 사용량 | child `disk_checks` |
| 3619–3681 | `cmd_doctor` 진입점 | 남김 |
| 3682–3837 | `profile_filter_tests` | 같은 이름의 child 시험 파일 |

결과 예상: 부모 약 400줄. child가 11개라 **조각 S3(모델·provider·qwen·보고)**,
**조각 S4(나머지 점검)** 둘로 나눈다. 경로에 묶인 기준선은 S1과 같은 방식으로 먼저 센다.

### 7. `src/dispatch/dispatch_context.rs` — 3,949 (prod 2,809) · application · 겹침 0

| 구간 | 책임 | 옮길 곳 |
|---|---|---|
| 1–346 | 실행 target·sandbox·counter-model·세션 전략 | 남김 |
| 347–563 | 커밋 제목의 이슈 참조 판정 | child `issue_reference` |
| 564–864, 1998–2091 | worktree 소속·clean probe | child `worktree_probe` |
| 865–1385 | review target context 주입·카드 worktree | 남김 |
| 1386–2525 | PG review target 해석(work dispatch, PR tracking, repo head) | child `review_target`(2개로 분할) |
| 2526–2809 | `build_review_context` | 남김 |
| 2810–3950 | 시험 모듈 4개 | 같은 이름의 child 시험 파일 4개 |

결과 예상: 부모 약 1,200줄. **조각 S5**. 착수 전에 `dispatch::dispatch_context`의
역방향 edge를 뽑아 그 참조가 있는 함수는 부모에 남긴다.

### 8. `src/server/mod.rs` — 2,785 (prod 약 1,980) · assembly · 겹침 0

| 구간 | 책임 | 옮길 곳 |
|---|---|---|
| 1–395 | 모듈 선언·`run` 조립 | 남김 |
| 396–911 | policy tick loop·hook 실행 기록 | child `policy_tick` |
| 912–1380 | Claude/OpenAI/Codex rate limit 조회 | child `rate_limit_fetch` |
| 1381–1514 | GitHub sync loop | child `github_sync` |
| 1515–2351 | message outbox drain·재시도 | child `message_outbox_drain` |
| 2361–2587 | routine runtime loop | child `routine_runtime` |
| inline 시험 모듈 6개(약 800줄) | — | 같은 이름의 child 시험 파일(부모 아래 선언 유지) |

결과 예상: 부모 약 450줄. **조각 S7**. 시험 모듈은 `server::<이름>` 경로를 지키려고
부모에 선언을 남기고, 옮긴 함수는 시험 파일의 import만 바꾼다.

### 9. `src/services/routines/store.rs` — 4,285 (prod 3,607) · adapter · **겹침 있음**

| 구간 | 책임 | 옮길 곳 |
|---|---|---|
| 1–679 | 관측값 계산·record 타입·삭제 gate | child `records`(타입), `observation_values` |
| 680–3461 | `impl RoutineStore`(2,782줄) | 책임별 `impl` 블록 child: `observations`(1280–1982), `lifecycle`(attach·patch·resume·delete), `runs`(retry·interrupt·recover·insert) |
| 3608–4285 | `tests` | child `tests` |

**조각 S8a(시험·관측), S8b(lifecycle·runs)**. PR #8291(#8219)이 착지한 뒤에 시작한다.

### 10. `src/config.rs` — 3,006 (inline 시험 약 900) · adapter(DTO는 vocabulary) · #7353 S4와 조정

- inline 시험 모듈 7개 → 같은 이름의 child 시험 파일. 시험 env lock(2767–2970) →
  기존 child `test_env` 쪽. prod 이동이 없는 부분이라 먼저 할 수 있다. **조각 S9a**.
- DTO 묶음(server·discord·agent channel·cluster·review/kanban·memory·routines·onboarding)
  → child `schema` 아래 모듈, 부모에서 `pub use`. `config`의 역방향 edge가 child로
  옮겨 가므로 #7353 S4의 config 조각과 소유·층 규칙을 맞춘 뒤 한다. **조각 S9b**.

## 순서

CLI 이관기가 삭제되어 조각 S1은 분할 대상에서 빠졌다.

| 조각 | 파일 | 새 파일 | 전제 |
|---|---|---|---|
| S2 | `phase_gates.rs`·`loader.rs`·`health_api.rs`·`postgres.rs`의 시험 모듈 | 5 | 없음 |
| S3·S4 | `cli/doctor/orchestrator.rs` | 6 + 6 | 없음 |
| S5 | `dispatch/dispatch_context.rs` | 8 | 역방향 edge 선검사 |
| S6 | `health_api.rs` handler, `postgres.rs` 시험 DB 헬퍼 | 2 | #8291 착지, #7353 S4 db 소유 확인 |
| S7 | `server/mod.rs` | 11 | 없음 |
| S8a·S8b | `routines/store.rs` | 4 + 3 | #8291 착지 |
| S9a·S9b | `config.rs` | 7 + 약 8 | #7353 S4 config 조정 |
| S10 | giant 기준선 낮추기·registry 정리 | 0 | 수치 변경 승인 |

모든 조각은 prod 30파일·순증 +1500 안이다(이동은 순증이 child의 `use` 줄뿐이다).
