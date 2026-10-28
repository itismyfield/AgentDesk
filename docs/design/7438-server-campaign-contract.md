# 서버 캠페인 실행 계약 — F0

상위 요구사항: [#7438](https://github.com/itismyfield/AgentDesk/issues/7438).
조사 기준: `24b8a691031de5e3a24efd4d7ec18a9ba4841b08` (2026-10-28).
이 문서는 구현 계약이며 실제 실행/배포/두 저장소 완주의 증거가 아니다.

## 1. 범위와 착지 경계

묶음 1은 F0 독립 설계 검토 뒤 F1 영속 모델·API, F2 공통 프로필 인터페이스,
F8 조회 어댑터·fixture를 제공한다. 사용자 화면이나 스케줄러를 먼저 켜지 않는다.
F2a ADK, F2b CH 어댑터, F3 증거 판정, F4 TUI, F5 배정, F6 착지, F7 통합,
F8 실제 화면, F9 운영 수용은 각각 후속 구현이다. #7438 전체를 닫지 않는다.

F1의 공개 API는 prepared 정의 생성/조회/편집과 서버 조회 투영까지 제공한다.
start/resume/retry는 `execution_unavailable`로 거부하며 실제 실행 명령을 광고하지 않는다.
실행 제어의 내부 명령 처리·영속 전이는 통제된 시험에서 검증한다. 운영 런타임을 켜는
설정 토글, 가짜 성공 endpoint, 주기 tick, legacy outbox enqueue는 추가하지 않는다.
F4/F5 활성화 전에는 서버 캠페인이 카드/이슈 실행 소유권도 취득하지 않는다.

F4는 #7272 S4 착지 뒤 진행한다. `auto_queue/**`, `phase_gate.rs`, review_skip,
카드 정책 JS, 입력/turn/세션 launch/cluster/config/runtime_bootstrap/TUI-O,
배포 스크립트와 relay dead letter는 이번 묶음의 편집 대상이 아니다.
자동큐 배타 소유권 통합은 #7349 담당 코디네이터와 순서를 맞춘 후 별도로 착지한다.

## 2. 현재 코드와 재사용 경계

모든 좌표는 위 조사 SHA 기준이다. 아래 코드는 새 실행 계약을 이미 구현하지 않는다.

| 현재 책임 | 근거 | 새 코드의 경계 |
|---|---|---|
| legacy 정의/진척 통합 JSON | `src/db/campaigns.rs:45,84,144,359` | 새 server DTO를 기존 `Campaign`으로 직렬화하지 않음 |
| legacy CAS와 snapshot 10개 보존 | `src/db/campaigns.rs:505,521,564,595` | 같은 PG transaction 패턴, 실행 이력은 prune에서 분리 |
| 저장 후 자동큐 handoff | `src/server/routes/campaigns.rs:127,142,155` | 새 명령/API는 이 경로를 호출하지 않음 |
| GitHub 이슈 상태의 노드 자동 반영 | `src/db/campaigns/issue_sync.rs:99,191,220` | `auto_queue=false`만으로 격리되지 않으므로 테이블도 분리 |
| handoff 직접 SQL | `src/services/auto_queue/campaign_handoff.rs:342` | 기존 campaigns만 읽는 현 동작 유지 |
| API 인증/라우트 조합 | `src/server/routes/domains/admin.rs:16,28` | 새 router도 동일 protected API 영역에 등록 |
| 기존 멱등 저장 | `src/db/idempotency.rs:83,110,248,258` | 별도 claim commit/TTL 삭제를 재사용하지 않고 같은 명령 transaction에 receipt 저장 |
| 기존 outbox claim | `src/db/dispatches/outbox/claim.rs:15,69` | claim/TTL 재회수는 실행 개시 증거가 아님 |
| 실제 local TUI 생성 | `src/services/claude/tui_session_launch.rs:57`, `src/services/platform/tmux.rs:151` | campaign 제어 입력 없음; F4 최종 실행기 통합 필요 |
| legacy 화면의 상태 합성 | `dashboard/src/components/campaigns/campaignModel.ts:70,102` | server DTO는 stage 문자열/heartbeat를 파싱하지 않음 |
| legacy 전체 문서 PUT | `dashboard/src/api/campaigns.ts:180` | server 정의 편집과 공유하지 않음 |

현재 `/api/campaigns`와 `campaigns`/`campaign_revisions`는 legacy 정본으로 유지한다.
새 `/api/server-campaigns`와 `server_campaign_*` 테이블은 같은 canonical PG에 둔다.
새 응답의 `execution_mode`는 항상 `server`, legacy 조회 어댑터는 항상 `legacy`다.
모드 변경 명령은 없다. 정의를 복사하면 새 ID의 prepared 캠페인이며 이전 증거를 승계하지 않는다.
목록 통합은 F8에서 두 명시적 응답 타입을 합친다. 기존 API의 기본값/필드/인증/상태 코드를 바꾸지 않는다.

## 3. 식별자·영속 모델

정의 revision, 실행 control version, 실행 event sequence는 서로 다른 카운터다.
실행 이벤트가 늘었다는 이유로 사용자의 stop 대상이 달라지지 않는다.

| 레코드 | 키와 내용 | 보존/쓰기 권위 |
|---|---|---|
| Definition | campaign ID, repository, goal/title, revision, nodes, workflow/profile version | prepared/stopped에서 정의 API만 변경 |
| DefinitionRevision | campaign ID + revision, 정의 snapshot, 변경 근거 | 최신 10개 snapshot; 실행 기록은 참조하는 해시/내용을 자체 보존 |
| Run | campaign ID + run ID, control version, control state, stop intent, 대상 host/gate 집합과 fence ACK, workflow/profile pin | 명령 처리만 쓰며 성공 완료 뒤 재사용하지 않음 |
| NodeRun | run ID + node ID, stage, retry series, quality failures, review rounds, main/acceptance 사실 | 카드/이슈 종료와 독립 |
| Attempt | run/node/series/stage/attempt ID, role/session binding, definition hash, base/head | append 이력; 취소·실패를 덮어쓰지 않음 |
| Evidence | evidence ID + attempt ID, kind, binding, 원 증거/출처/수용 판정 | 제출은 주장, 채택은 서버 판정. 정의 변경으로 유효성만 바뀜 |
| Event | run ID + 단조 sequence, command ID, typed event, observed_at | append-only, snapshot prune와 무관 |
| Action | action ID, run/node/attempt, kind, control version, 대상 host/gate identity, 상태, 외부 상관 ID/관측 | 전송 전 대상 영속화, intent부터 결과 대사까지 보존 |
| Receipt | campaign ID + idempotency key, 정규화한 요청 fingerprint, status/body | 상태/events/actions와 같은 transaction, 실행 이력과 함께 보존 |

F1은 위 레코드를 서버 전용 행으로 저장한다. 별도 DB/메시지 서비스/범용 event-sourcing 엔진은 만들지 않는다.
조회 snapshot은 canonical 행에서 다시 만들 수 있다. Event는 감사/증분 전달용이며 실행 정본을 복제하는 별도 원장이 아니다.
부모 정의 revision 삭제가 run/attempt/evidence/action/receipt를 cascade 삭제하지 않게 FK를 정한다.
정의 삭제 및 실행 이력 GC는 이번 범위에 없다.

`NodeDefinition`은 ID, 제목, 요구사항 이슈 링크, 수용조건(안정 ID·설명·검증 종류), 선행 ID,
선택 그룹만 받는다. `status/stage/round/assignee/session/provider/evidence/head/auto_queue`
등 실행 필드와 unknown 필드는 400이다. 수용조건 변경에는 근거 링크와 변경 설명을 받는다.
외부 입력 검증은 HTTP 경계의 공통 validator에서 한 번 수행하며 내부에는 검증된 타입을 넘긴다.
순환은 실제 경로를, 중복/빈 AC/없는 참조/미지원 프로필은 대상과 이유 코드를 반환한다.

## 4. 상태·명령·사건

코드 enum은 `CampaignControl = Prepared | Running | Stopping | Stopped | Succeeded`다.
`Stopping`은 `StopMode = Immediate | FinishStage`, 요청 시각, 확인된 host fence,
최종 stop 확정 event 및 미대사 action을 가진다. 실패만 남아도 Succeeded가 되지 않는다.
실행의 일시 대기/진행 불가는 control state가 아니라 서버 조회 판정이다.

`Stage = Requirements | Design | DesignReview | Implementation | Review | Landing |
MainVerification | FinalVerification | Complete`를 코드 버전 `campaign-v1`로 고정한다.
수리도 Implementation/Review의 새 attempt이고 별도 workflow DSL은 없다.
`AttemptOutcome = Passed | QualityFailed | UserInterrupted | EnvironmentWait | Superseded`.
`NodeDisposition = Pending | Assigned | Running | Verifying | Failed | Blocked | Interrupted | Complete`.
`main_integrated`와 `acceptance_complete`는 별도 사실이다.

모든 명령은 멱등키를 가진다. 정의 쓰기는 expected definition revision,
내부 실행 결과는 run/attempt/control version/정의 hash를 검증한다.
stop은 expected run ID + definition revision만 검사하고 event sequence를 CAS 조건으로 쓰지 않는다.
동일 키/동일 본문은 과거 결과, 동일 키/다른 본문은 409다. 재전송 receipt 조회가 stale 검사보다 먼저다.

새 개시는 현재 run fence/version으로 검사하지만 결과는 immutable attempt/action binding으로 검사한다.
FinishStage에서는 중단 경계에서 마무리가 허용된 기존 attempt의 결과만 정상 게이트로 수용한다.
이미 시작된 외부 효과의 사실 대사는 두 stop 모드에서 허용하며, 새 단계/발사 권한을 되살리지 않는다.
진행 중 pin을 새 control version으로 변경하거나 옛 attempt 결과를 일괄 수용하지 않는다.

| 명령 | 허용 조건 | 사건/효과 |
|---|---|---|
| CreateDefinition | 새 ID, 유효 정의 | DefinitionCreated, Prepared |
| EditDefinition | Prepared/Stopped, 관련 세션 모두 Gone, 미대사 action 없음, revision 일치 | DefinitionEdited, revision 증가, 영향받은 증거 supersede |
| Start | Prepared, 프로필/실행기/게이트/소유권 통합 준비 완료 | RunStarted, Running; F1 공개 API에서는 불가 |
| RequestStop(mode) | 같은 run/정의, Running/Stopping | StopRequested, control version 증가, 새 intent 배정 즉시 금지 |
| RequestStop(Immediate) | FinishStage로 stopping | StopEscalated; 이미 취소한 시도를 복원하지 않음 |
| ConfirmStopFence | 모든 실행 가능 host가 새 fence를 확정, 이전 in-flight 개시 결과 분류 | StopConfirmed; §5의 실제 실행 경계 |
| ObserveSession/Action | 같은 incarnation/action/attempt의 직접 관측 | SessionObserved/ActionReconciled; 추측 성공 금지 |
| ConfirmStopped | 모든 관련 세션 Gone, 개시 불명/미대사 action 0, stop 확정 | RunStopped; 편집/재개 가능 |
| PrepareRetry(node) | Stopped, failed 대상 | RetryPrepared, 새 series 준비; 발사 없음, 과거 실패 보존 |
| Resume | Stopped, 정의·증거·자원/소유권 재검증 | RunResumed, control version 증가; failed는 자동 초기화하지 않음 |
| SubmitEvidence | 현재 attempt/역할/버전 binding 일치 | EvidenceSubmitted; 자체 PASS로 단계 전이 없음 |
| AcceptStageResult | F3의 결정적 증거 게이트 충족 | StageAccepted 또는 StageFailed; stopping에서는 §6 제한 |
| RecordMainIntegration | 프로필의 공통 main 관측 통과 | MainIntegrated; 후속 적격성만 해제 |
| CompleteNode/Run | 모든 필수 AC, 최종 검증까지 통과 | NodeCompleted/RunSucceeded |

서버 내부 명령은 모듈 API이며 범용 운영자 상태 수정 endpoint로 노출하지 않는다.
LLM용 claim/resume/submit API는 F4에서 session binding과 함께 붙인다.
F1 공개 제어 요청의 미구현 동작은 409와 `execution_unavailable`로 응답한다.

단계 전이는 다음 표로 고정한다. 모든 행은 현재 binding의 채택된 증거를 필요로 한다.
stopping에서는 이미 개시한 현재 단계 결과 저장까지만 허용하며 다음 행을 실행하지 않는다.

| 현재 단계 | 다음 단계 | 게이트/실패 |
|---|---|---|
| Requirements | Design | 이슈 요구·AC·검증 계획 확정 |
| Design | DesignReview 또는 Implementation | 안전/권위/동시성 계약 변경이면 독립 설계 리뷰 필수; 나머지는 명시된 계획으로 진행 |
| DesignReview | Implementation 또는 Design | 독립 CLEAN이면 구현, 차단 finding이면 설계 수리; 2라운드 미수렴은 Failed |
| Implementation | Review | exact candidate 자체 검사 완료; 잔여 로컬 검증은 리뷰와 병렬 가능 |
| Review | Landing 또는 Implementation | 독립 전체/delta 범위와 필수 로컬 검증 충족이면 착지, 차단 finding이면 원 구현 세션 수리 |
| Landing | MainVerification | 해당 candidate의 required CI/PR 권한/리뷰 모두 충족 후 merge action |
| MainVerification | FinalVerification 또는 Complete | 실제 main 포함 증거, 최종 AC 유무로 분기 |
| FinalVerification | Complete | 배포/운영 AC 모두 확인; 부분 관측은 대기 |

품질 수리 한도 3은 최초 구현 뒤 수리를 최대 3번 허용한다는 뜻이다. 차단 결과가 수리 attempt를
준비할 때 repair_cycles를 한 번 증가시키며, 3번째 수리 뒤에도 차단 결과면 Failed다.
자체 검증·리뷰의 동일 수리 attempt 실패를 중복 계수하지 않는다. 설계 리뷰 회차는 별도로 센다.

## 5. stop과 실제 실행 개시의 직렬화

`ActionState = Planned | Claimed | StartPending | Started | Reconciled | Cancelled | Unknown`.
Planned/Claimed/StartPending, 서버의 permit commit, RPC enqueue ACK는 Started 증거가 아니다.
Started는 실행기의 실제 effect initiation receipt가 있어야 한다. 결과까지 성공했다는 뜻도 아니다.

분산 host의 외부 syscall과 PG commit을 하나의 원자 트랜잭션이라고 주장하지 않는다.
단순히 PG permit을 받은 뒤 원격 launch하는 방식은 승인→전송 사이 stop을 놓치므로 채택하지 않는다.
다음 프로토콜에서 요청 접수와 stop 확정, stopped 완료를 구별한다.

1. 서버는 run 행 잠금 아래 action intent와 version을 기록한다. Claim은 작업 회수만 뜻한다.
2. 최종 실행기는 `(run, control_version)` fence와 action ID를 같은 host의 최종 개시 gate에서 확인한다.
   같은 gate 안에서 한 action의 실제 개시를 수행하고 receipt를 기록한다. 큐 삽입/SSH 송신 시점이 아니다.
   launch/deliver(new stage)/PR 생성/merge/deploy 각각의 실제 외부 호출 직전에 이 gate를 적용한다.
3. RequestStop은 run 행 잠금 아래 durable stop intent와 새 control version을 저장한다.
   새 배정/단계/action intent는 그 commit부터 거부한다. 이미 전달 중인 action은 StartPending/Unknown으로 추적한다.
4. 각 관련 host의 fence 적용은 위 최종 개시 gate와 직렬화한다. gate가 start를 먼저 실행했다면
   receipt를 반환하고, fence가 먼저라면 미개시 action을 claim 여부와 무관하게 취소한다.
   fence ACK는 큐에 넣었다는 ACK가 아니라 기존 gate 종료와 새 version 적용 완료의 증거다.
5. 서버는 모든 관련 host의 fence ACK와 이전 action 분류를 받은 뒤 같은 run lock에서 StopConfirmed를 commit한다.
   이 commit 뒤에는 오래된 발사 메시지를 보내거나 재전송해도 어떤 host도 개시할 수 없다.
   host 목록에는 intent가 배정된 host도 포함한다. stop 요청 후 새로운 host를 추가할 수 없다.
   대상 집합과 ACK는 전송 전에 저장한 action의 host/gate identity에서 복구한다.
   세션 관측이 없는 host도 제외하지 않으며, ACK/복구 확인이 없으면 stop_confirmed=false를 유지한다.
6. RequestStop API는 접수되면 202와 `stopping`, `stop_confirmed=false`를 반환할 수 있다.
   응답 유실은 같은 멱등키로 대사한다. fence 불명은 stopping/Unknown을 유지하며 확정됐다고 표시하지 않는다.
   stop 요청 접수부터 확정까지 이미 전송 중이던 동작이 먼저 gate를 넘은 경우에만 기존 실행으로 대사한다.

최종 host gate/fence는 F4의 필수 통합 산출물이다. 이번 F1은 해당 입력·영속 사건·거부 조건과
fake host의 통제된 순서 시험만 제공한다. 실제 host gate가 없는 프로필은 실행 가능 상태가 될 수 없다.
서버/host crash가 개시 receipt를 잃게 하면 action은 Unknown이다. 같은 action ID/정확한 세션 incarnation/
PR 외부 ID/배포 SHA의 관측으로만 해소하며 응답 유실을 재실행 허가로 바꾸지 않는다.
host 재부팅 시 durable fence와 action journal을 먼저 복구해야 하며, 복구 불명인 executor는 새 개시를 거부한다.
이 계약을 만족하지 못하는 기존 remote 실행 함수는 F4에서 직접 연결하지 않는다.

## 6. 중단·생존·실패 전이

`SessionObservation = Alive | Gone { host, incarnation, observed_at, evidence } |
Unknown { reason, last_observed_at }`. Gone은 도달 가능한 host에서 같은 incarnation의 종료를
확인한 관측이다. heartbeat 만료, provider 턴 완료, 호스트 불통은 Gone이 아니다.
Run에 연결된 구현/리뷰 TUI는 유휴여도 관련 세션이다. 관리 코디네이터/과거 incarnation 기록은 제외한다.
같은 역할 세션은 기본 재사용, 확인된 Gone/명시적 교체 사유 뒤 새 incarnation으로 교체한다.
Unknown 세션의 대체 발사와 정의 편집은 거부한다. 실제 stop/terminate는 기존 세션 소유권 경계를 따른다.

Immediate는 실행 중 TUI에 중단 요청을 보내고 미완수 attempt를 UserInterrupted로 닫는다.
부분 산출물은 성공 증거가 아니고 품질 실패 횟수도 늘리지 않는다. 이미 채택된 증거·main/배포 사실은 유지한다.
FinishStage는 stop 확정 전에 시작한 stage의 같은 attempt와 그 자체 검증만 허용한다.
구현 뒤 새 독립 리뷰, 리뷰 뒤 수리, CI 뒤 merge, merge 뒤 deploy는 각각 새 단계/외부 action이므로 시작하지 않는다.
끝난 stage 결과는 정상 게이트로 수용한 뒤 관련 TUI를 종료한다. 노드 전체 drain은 하지 않는다.
같은 run에 이미 시작한 외부 효과는 두 모드 모두 결과를 회수한다. 모든 관련 세션 Gone과 대사가 끝나야 Stopped다.

품질 수리 기본 한도 3회, 독립 설계 리뷰 최대 2라운드. 계수는 attempt 결과를 처음 확정하는 transaction에서만 증가한다.
중복 제출은 계수를 늘리지 않는다. 자원/쿼터/host 대기는 EnvironmentWait, 사용자 stop은 UserInterrupted다.
한도 도달은 해당 node Failed, 아직 시작하지 않은 후속은 Blocked(선행 ID), 독립 가지는 계속 진행한다.
한도 이후 자동 조사/재설계/코디네이터 dispatch를 만들지 않는다. 일반 Resume은 실패 예산을 초기화하지 않는다.
PrepareRetry는 새 series를 준비하고 이후 Resume+스케줄러만 발사한다.

## 7. 증거와 저장소 공통 인터페이스

공통 binding은 campaign/run/node/series/stage/attempt/role/session+incarnation/repository,
requirements+AC hash, workflow version, profile hash, base/candidate SHA다.
진행 중 attempt의 pin은 바꾸지 않는다. 요구사항/AC/프로필 변경은 해당 미완료 node의 현재 증거만
supersede하고 실제 main 사실은 보존한다. 간선/그룹/표시 변경만으로 코드 증거를 지우지 않는다.
완료 node에 새 요구는 후속 node로 만들며 성공 완료 캠페인은 새 정의를 복사한다.

TestEvidence는 실행 ID, 정확한 candidate, 환경 identity, 예정/실제 test 목록과 수,
passed/failed/skipped/required-skip, 로그 참조를 가진다. 실행 0건/필수 skip은 PASS가 아니다.
ReviewEvidence는 독립 session/역할, base/head, full/delta 범위, 안정 finding ID와 해소 근거,
미검증 범위, 승계한 증거와 유효 이유를 가진다. 전체 검토 없이 delta만 누적한 CLEAN은 거부한다.
MainIntegrationEvidence는 repo, target main SHA, PR/merge SHA, node별 candidate/리뷰 범위,
merge/squash/묶음 연결과 실제 ancestry/content 확인 결과를 가진다. closed/CLEAN/CI PASS/skipped는 대체 증거가 아니다.
F5와 F6은 같은 `MainIntegrationEvidence` 수용 결과를 사용한다.

F2는 다음 typed 인터페이스와 양 저장소 fixture 계약 시험만 만든다. 실제 도구 실행 어댑터는 후속이다.

| 연산/타입 | 최소 내용 |
|---|---|
| `RepositoryProfile` | `Adk` / `CookingHeart`, profile version/hash, 역할별 모델 기본값, 작업 한도 2, 실패 한도 |
| `workspace(candidate)` | editing worktree, verification checkout, 각각의 관측 SHA, repository identity |
| `verification_plan(stage, candidate)` | 필수 검사 ID/종류, 실행 환경, 필수 시험/CI/배포/운영 AC; 저장소별 명령 문자열을 도메인 FSM에 넣지 않음 |
| `resources(stage)` | host/build/editor/shared-checkout/engine/assets/MCP 요구와 대기 이유 |
| `observe(pr/ci/main/deploy)` | `Observed(evidence)` / `NotYet(reason)` / `Unknown(reason)`; 대상 identity·관측 시각 포함 |
| `ExecutionEnvironment` | OS/host/toolchain/engine version+path, assets/submodule identity, resource allocation, 실제 checkout SHA |

ADK는 격리 worktree/target/대상 Rust 시험/required CI/필요한 배포를 검증한다.
CH는 편집 worktree와 공용 검증 checkout을 분리하며 exact candidate 동기화, UE/toolchain/assets,
Monolith/에디터/PIE 조건을 증거에 담는다. `~/CookingHeart`의 조사 SHA는
`1f88667d1b3f1a430084207326f152b45e38dc2a`; `AGENTS.md:26,33,87`,
`LOCAL_SETUP.md:19`, `Scripts/lib/resolve_ue.sh:50,90`이 해당 경계다.
문서의 UE 5.8.0과 관측된 로컬 5.8.1이 다르므로 버전을 문서 상수로 추정하지 않는다.
CH 환경이 없는 fake 성공을 실제 지원으로 표시하지 않는다.
ADK 자기 배포는 action 의도→실행→재시작→부팅 시 실제 배포 SHA 관측으로 확정한다. 무응답 재배포는 없다.

## 8. legacy 권위와 릴레이 경계

묶음 1의 물리적 격리로 기존 handoff/issue-sync가 새 정의/실행을 읽거나 변경할 수 없다.
F4/F5 활성화 전 다음 실제 소비자까지 배타 소유권을 통합해야 한다. 이 절은 이번 수정 지시가 아니다.

- 같은 issue/card의 legacy dispatch/queue 존재 시 `owned_elsewhere`로 대기하며 선점하지 않는다.
- campaign/run/node/attempt가 실행 identity이고 issue/card는 링크/투영이다. 같은 issue의 복수 node를 합치지 않는다.
- task_dispatches/outbox를 재사용한다면 campaign ownership을 생성·완료·취소·재시도에 끝까지 보존한다.
- review-automation/create-pr/phase gate/review skip/품질 한도 알림/autoqueue 완료 동기화는
  campaign 소유 항목의 전이·추가 dispatch를 수행하지 않게 단일 분기 경계를 검증한다.
- 기존 카드 정책의 disabled review를 새 증거 게이트에 매핑하지 않는다.

활성화 전 대조할 실제 진입점은 다음과 같다. 생성뿐 아니라 완료·취소·재시작 replay를 포함한다.

| 진입점 | 현재 좌표 |
|---|---|
| 저장/주기/terminal handoff | `src/server/routes/campaigns.rs:159`, `src/server/mod.rs:516`, `src/kanban/hooks.rs:322` |
| 후보·enqueue·활성화 | `src/services/auto_queue/campaign_handoff.rs:154,336,381,395`, `src/services/auto_queue/fsm.rs:594,644` |
| dispatch 생성/dedup | `src/dispatch/dispatch_create.rs:583,653,1193,1269` |
| terminal·queue·phase sync | `src/dispatch/dispatch_status.rs:717,725,744,767` |
| completion hook/replay | `src/dispatch/completion_hooks.rs:12,97` |
| review skip/복원 | `src/dispatch/dispatch_status.rs:1224,1301`, `src/dispatch/dispatch_status/review_skip.rs:8` |
| 취소/queue 재활성화 | `src/dispatch/dispatch_cancel.rs:254,416` |
| 카드/큐 정책 | `policies/kanban-rules.js:230,486,596`, `policies/auto-queue.js:102,136,169,406` |
| phase gate 우회/후속 생성 | `policies/lib/auto-queue-phase-gate.js:430,601` |
| 리뷰/PR/수리/한도 알림 | `policies/review-automation.js:134,285,318,335,593,1635,1654,1786,1837,1873` |
| 직접 review 보강 | `src/kanban/transition_core.rs:215`, `src/server/routes/kanban.rs:1670`, `src/server/routes/resume.rs:456` |
| GitHub close의 직접 dispatch 취소 | `src/github/sync.rs:1093`, `src/engine/transition_executor_pg.rs:167` |
| terminal queue/rework 취소·worktree 정리 | `src/kanban/terminal_cleanup.rs:17,110`; 같은 helper를 호출하는 JS kanban facade도 포함 |

공용 dispatch 활성화 시 GitHub-sync close와 terminal-card의 실제 진입점 시험으로 campaign 소유
attempt·세션 연결·필요 worktree 보존을 확인한다. 공통 mutation/cleanup 경계의 소유권 분기를 제거하면
시험이 실패해야 하며, 일반 legacy 항목의 정리 동작은 유지한다.

`docs/relay-state-contract.md`의 #5071 단일 권위를 따른다. 캠페인의 실행 상태는 작업 공정 권위이며
Discord terminal body/relay lease/active-turn lease의 대체 권위가 아니다.
F4는 기존 입력 소유자와 세션 incarnation을 연결한다. 새 본문 전달/중복 제거/세션 강탈 경로를 만들지 않는다.

## 9. 조회 계약과 HTTP 표면

`ServerCampaignView`에는 `execution_mode`, definition revision, run/control version,
server `as_of`, last event sequence/time, control state, display state, node views,
failed/blocked 수, reasons(target 포함), allowed/rejected commands, sessions, unreconciled actions를 둔다.
표시는 서버가 다음 우선순위로 계산한다: Stopping → Stopped → Prepared → Succeeded →
실제 실행/외부 action이 있으면 Running → 자동 해제 조건이면 Waiting → 나머지 실패/차단이면 UnableToProgress.
관측 불명은 별도 Unknown 표지이며 success/editable을 합성하지 않는다.
NodeView는 stage/attempt, readiness reason, main 반영과 최종 AC 잔여, 직접 후속/실제 준비될 후속을 분리한다.
허용 명령과 거부 이유는 같은 command validator의 결과다. F1에서 start/resume/retry는 항상 미제공 이유가 보인다.

| API | 의미 |
|---|---|
| GET `/api/server-campaigns` | server 조회 목록; legacy API/목록과 독립 |
| POST `/api/server-campaigns` | 정의 생성, 멱등키, `execution_mode=server` 고정 |
| GET `/api/server-campaigns/{id}` | 정의+서버 조회 투영 |
| PUT `/api/server-campaigns/{id}/definition` | 정의만 편집, expected revision/변경 근거 |
| POST `/api/server-campaigns/{id}/commands` | 명시적 start/stop/resume/retry; 활성화 전 실행 명령 거부 |
| GET `/api/server-campaigns/{id}/events` | run/after sequence 기준 typed event 조회, 제한된 page |

400=정의/명령 형식 위반, 404=대상 없음, 409=revision/상태/멱등 본문 충돌 또는 실행 미제공.
F1은 기존 API 인증을 그대로 쓴다. 새 로그인·토큰 배포·권한 우회는 도입하지 않는다.
F8의 TS 타입/조회 어댑터/fixture는 server 응답을 그대로 전달하며 기존 UI 진입에는 연결하지 않는다.
실제 화면에서의 끊김/초안/409/stop 재전송, 케이블타이 표시 저장은 묶음 2의 수용이다.

## 10. 수용시험과 미충족 항목

| ID | 묶음 1에서 검증할 것 | 검증 수단 |
|---|---|---|
| B1-01 | legacy CRUD/CAS/누락 auto_queue 보존/history/issue-sync 불변 | 기존 PG/HTTP 시험 + server 생성 전후 legacy 원장 대조 |
| B1-02 | server 정의 실행 필드 거부, AC/순환/참조/중복, 같은 issue의 node 분리 | 실제 새 HTTP router + PG |
| B1-03 | 정의 10 revision 초과 prune 후 실행/증거/실패/action/receipt 보존 | isolated PG migrations/reconnect |
| B1-04 | 상태+event+action+receipt 원자성, 동시 동일 명령 한 번, 다른 본문 거부 | PG 경쟁/rollback/응답 유실 후 새 pool |
| B1-05 | stop 대상은 run/정의, event 증가로 stale 거부하지 않음, 강화/재시도 계수, v에서 시작한 attempt의 FinishStage(v+1) 결과 수용과 새 개시 거부 | 순수 전이표+PG command 시험 |
| B1-06 | Alive/Unknown/미대사 action이면 stopped/편집 금지 | 실제 command store + fake session observation |
| B1-07 | intent/claim/start 직전·직후 fence 주입, stop 확정 뒤 새 개시 0 | fake 최종 host gate + durable PG 사건; 실제 TUI 증거와 구분 |
| B1-08 | F1 공개 API로 start 불가, legacy outbox/dispatch 생성 0 | HTTP 요청 후 PG 상태 확인 |
| B1-09 | ADK/CH exact candidate·editing/verification 차이·자원/Unknown 공통 계약 | 두 profile fixture로 같은 계약 suite |
| B1-10 | 서버 상태/이유/명령을 TS 어댑터가 그대로 유지 | 실패+실행/실패+차단/준비/리뷰 대기/Unknown fixture |

B1-07은 세션 관측 없이 action만 배정된 host도 새 pool 복구 후 ACK 없이는 stop 확정할 수 없음을
검사한다. fake host 재시작 뒤 옛 launch 개시 0건, 개시 후 receipt 저장 전 crash의 Unknown과 재실행
거부를 확인한다. host 집합을 Alive 세션만으로 계산하거나 복구 journal을 비우는 뮤턴트가 실패해야 한다.
실제 host 재시작·최종 개시 gate 증명은 F4/F7에 남긴다.

기존 보존 시험: `postgres_campaign_concurrent_cas_and_reconnect_preserve_canonical_history_pg`,
`postgres_campaign_revision_history_stays_bounded_by_retention_pg`,
`postgres_campaign_http_auth_conflict_and_history_pg`,
`postgres_closed_issue_completes_or_skips_linked_nodes_with_the_actual_closer_pg`.
PG는 isolated `TestPostgresDb`와 serial 실행을 쓴다. 실제 목록/실행 수/실패/skip과 SHA를 기록한다.
새 PG 시험은 현 소스 기반 classifier/nextest 경로를 확인한다. 삭제된 옛 inventory를 복원하지 않는다.

미충족: 실제 host fence·세션 launch/stop·독립 역할 TUI(F4), 증거 채택 FSM(F3),
legacy 배타 소유권과 스케줄러(F5), 단독/묶음 PR main 및 배포 대사(F6),
실제 재시작·두 stop(F7), 실제 화면/케이블타이(F8), ADK/CH 실제 DAG(F9).
이 항목들은 공개 실행 활성화의 선행이며 fixture 성공으로 완료 처리하지 않는다.

구현 리뷰 단위는 (A) 도메인/영속 타입과 제어 계약, (B) PG 명령 저장+API,
(C) F2/F8 공통 인터페이스/조회로 나눈다. 각 prod 30파일/+1500 이내에서 검토하고
모두 CLEAN 뒤 묶음 1 PR 하나로 낸다. 공유 module 등록/생성물 통합은 단일 작성자가 맡는다.
설계/구현의 의미 변경은 해당 delta 리뷰, 머지 전 adk-cc 통보, i7438 단계별 DAG_ACK를 필수로 한다.
