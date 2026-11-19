# Campaign pipeline free edit: 0단계 상태 차트 (#8249 P0)

코드 SHA는 `7ee248ddea8ac5de3991f7c8d65a9f1441fd0e0c`(origin/main)이다. 설계 r1의 기준 `a3dc4cc4a8`과 이 SHA 사이에는 #8246 한 커밋만 있고, 그 커밋은 CI 파일 7개만 바꿨다. `src/`·`migrations/`·`dashboard/src` 차이는 0이다. 따라서 설계 표 12의 좌표는 대부분 그대로 유효하다. 독립 리뷰는 `97daacf1f7`(M0~M5 착지 전)을 기준으로 읽었는데, 그 기준 탓에 생긴 P2-1("모델·host_worker 파일이 없다")은 이 SHA에서 해당하지 않는다. `CampaignNodeLifecycle.tla`, `host_worker.rs`, `stage_fsm.rs` 같은 파일은 모두 있다.

**모델 결과는 구현이나 Rust 시험의 증거가 아니다.** 이 조각은 설계의 계약을 TLC로 검사했을 뿐이고, production 코드는 0줄 바꿨다. 공개 Start·Resume·PrepareRetry는 지금도 `ExecutionUnavailable`(`control.rs:348,360`)이다. 모델의 행동은 설계 r1이 제안한 계약이며 현재 코드가 이렇게 동작한다는 뜻이 아니다.

[모델](CampaignPipeline.tla), [전체 cfg](CampaignPipeline.cfg), [차트 cfg](CampaignPipelineChart.cfg), [view](pipeline.view.mjs), [차트](pipeline.chart.mmd), [도구 보고서](pipeline.tool.report.md).

## 출발점과 범위

기존 [`CampaignNodeLifecycle`](CampaignNodeLifecycle.tla)은 고정 Stage(requirements→work→review→landing→confirm)를 다룬다. 이 모델에서 Compute·Apply·Crash, 위치(epoch) 대조, 재계산 구조를 가져왔다. 기존 [`CampaignVerification`](CampaignVerification.tla)의 FinishStage 예외(`Auth`, `FinishScopeKept`)는 P2-3 분해의 기준으로 삼았다. 두 기존 모델은 바꾸지 않았다.

단위는 **한 작업의 유한한 pipeline instance와 공통 저장·제어 경계**다. 고정 그래프 하나로 설계 V5의 모양을 모두 담았다.

- `work`에서 p10 retry self-loop(공유 예산 `q`, 소진되면 `failed`), p20 `review`로 간다. review가 삭제된 graph에서는 바로 `done`으로 간다.
- `review`에서 p10 repair로 `work`에 돌아간다. 같은 예산 `q`를 쓰므로 바깥 고리가 된다. p20은 승인 자리 `land`를 거쳐 `done`으로 간다.
- 살아 있는 모든 상태에는 p0 timeout 출구(`failed`)가 있다.
- `work`로 들어가는 전환(retry·repair)에는 승인 자리 `impl`이 붙는다.

자유 편집은 `review` 상태 삭제 한 종류로 줄였다(`SaveDelete`). 실행은 pin한 graph(`activeReview`)를 계속 쓰고, 명시적 adoption(`Resume(TRUE)`)을 거쳐야 저장 정의(`savedReview`)로 바뀐다. 추가 cfg인 `TwoJobs`와 `TwoJobsStop`에서는 두 worker와 두 작업이 같은 저장·제어 경계를 두고 경쟁한다.

## 검사 결과

공식 `scripts/ci/statecharts_tlc.py`로 **27 cfg**를 대조했다. 설계안 GREEN 13개와 결함 주입 RED 14개 모두 `expected.tsv`와 일치한다. 기본 cfg는 **102,680 distinct states**이고 invariant 12개와 liveness 1개를 검사한다. 공허한 GREEN을 피하려고 기본 cfg에 임시 부정 invariant를 걸어 다음 경로가 모두 도달 가능함을 확인했다: 성공 끝, 예산 소진 실패, FinishStage가 다음 cursor만 기록한 Stopped, adoption 뒤 실행, 명시적 복구로 생긴 새 방문, 재방문한 review의 land 승인, provisional grant, Unknown receipt.

| 설계 §8 반례 | GREEN cfg (설계안) | RED cfg (결함 주입) | 주입한 결함 | RED에서 깨지는 규칙 |
|---|---|---|---|---|
| 중첩 고리 | NestedLoop | NestedLoopBug | 바깥 repair 간선이 예산을 쓰지 않는다(SCC당 retry 하나만 확인하는 검사가 통과시킨 graph) | liveness `InstanceTerminates` |
| retry 소진 고리 | RetryExhaust | RetryExhaustBug | 소진 뒤에도 `to`로 간다(`then_to` 무시) | `RetryWithinBudget`, `InstanceTerminates` |
| 앞선 Unknown | LeadingUnknown | LeadingUnknownBug | p10이 Unknown인데 p20 True로 넘어간다 | `PriorityRespected` |
| 승인 재방문 | ApprovalRevisit | ApprovalRevisitBug | 승인을 visit 없이 gate만으로 찾는다. self-loop 재방문에서 지난 방문의 `impl` 승인을 다시 쓴다 | `ApprovalNamesExactVisit` |
| 중복 사건 | DuplicateEvent (worker 2) | DuplicateEventBug | 같은 방문의 중복 wake 두 개가 계산한 명령을 visit 대조 없이 state 이름만 보고 적용한다 | `OneCommittedExitPerVisit`, `PriorityRespected`, `TerminalNeedsEvidence` |
| 중복 사건(늦은 사건) | DuplicateEvent (LateEvents=TRUE) | LateEventBug | 이전 방문의 늦은 PASS가 현재 조건을 True로 만든다 | `ConditionFromCurrentVisit` |
| commit 전후 crash | CrashCommit | CrashCommitBug | 다음 entry intent를 전환 commit 밖에서 따로 쓴다. 둘 사이 crash로 intent를 잃는다 | liveness `InstanceTerminates` |
| 효과 영수증 유실 | ReceiptLost | ReceiptLostBug | 영수증을 잃은 Unknown launch를 미전송으로 보고 다시 개시한다 | `NoReplayAfterRestart` |
| stop 경쟁 | StopRace | StopRaceBug | commit이 lock 밖에서 읽은 control(running)을 믿는다 | `NoNewIntentAfterStop` |
| stop 경쟁(FinishStage 막힘) | StopRace | FinishBlockedBug | "stop 뒤 모든 개시 금지": frozen grant의 검증 개시도 막는다 | `FinishStageKeepsGrant` |
| stop 경쟁(FinishStage 과허용) | StopRace | FinishAllowsAllBug | "FinishStage이면 모든 action 허용": 다음 entry도 만들고 개시한다 | `NoNewIntentAfterStop`, `NoLaunchAfterFence` |
| 삭제 경쟁 | DeleteRace | DeleteRaceBug | PUT이 앞선 validate 결과를 믿는다. Prepared에서 validate한 뒤 실행이 review에 서 있는 동안 review를 삭제한다 | `NoOrphanState`, `InstanceTerminates` |
| adoption/Resume 경쟁 | AdoptResume | AdoptResumeBug | graph 세대 변경을 새 방문으로 처리하고 entry effect를 다시 발사한다 | `AdoptionDoesNotReenter` |
| 기본·차트·경쟁 | CampaignPipeline / Chart / TwoJobs / TwoJobsStop | — | 없음 | GREEN |

- `DuplicateEventBug`의 추가 위반 두 개는 같은 결함에서 나온다. 늦은 명령이 새 방문에 적용되면, 그 방문의 사실(전부 Unknown)을 평가하지 않은 채 출구를 확정한다. 그래서 우선순위도 어기고, 새 entry intent가 진행 중인 상태로 끝 상태에 닿는다.
- `DeleteRaceBug`의 liveness 위반도 같은 원인이다. 정의에서 지운 상태에 서 있는 Stopped 실행은 일반 Resume도 adoption도 할 수 없다. 설계가 말한 "readiness 사유 반환·원래 정의 복원" 경로가 필요한 지점이다.
- `TwoJobs`(stop 0, liveness 포함)와 `TwoJobsStop`(stop 1, invariant만 검사)으로 나눈 이유는 CI 시간 때문이다. stop 1회와 liveness를 함께 돌리면 맥 부하 아래 283초가 걸려 300초 한도에 닿는다.

## 규칙

INVARIANT(설계 표 11과 요청의 네 문장):

- `OneCommittedExitPerVisit` — 방문당 전환 확정 1회. 모든 commit이 방문을 바꾸므로, 확정한 명령의 visit이 현재 visit과 같다는 대조가 곧 "그 방문의 출구가 아직 소비되지 않았다"는 검사다.
- `NoOrphanState` — 고아 상태 0. 살아 있는 cursor는 참조하는 실행 pin의 graph에 있어야 한다. 편집이 일어날 수 있는 Prepared·Stopped에서는 저장 정의에도 있어야 한다.
- `NoReplayAfterRestart` — 재부팅 뒤 같은 효과를 두 번 개시하지 않는다(intent당 launch ≤ 1). 같은 전환을 다시 적용하지 않는 것은 `OneCommittedExitPerVisit`이 함께 맡는다.
- `NoNewIntentAfterStop`·`NoLaunchAfterFence`·`FinishStageKeepsGrant` — stop 속성을 세 개로 나눴다(아래 P2-3).
- `PriorityRespected`, `ApprovalNamesExactVisit`, `ConditionFromCurrentVisit`, `RetryWithinBudget`(`BudgetMonotoneWithinSeries`의 상한 쪽), `AdoptionDoesNotReenter`, `TerminalNeedsEvidence`.

PROPERTY: `InstanceTerminates` — 시작된 instance는 결국 `done` 또는 `failed`에 닿는다. 진행 가정(weak fairness)은 설계 §8 그대로다. crash·stop·편집 횟수는 유한하다. scheduler와 host는 공정하다. 사실과 승인은 결국 도착하고, 유효한 출구가 없으면 timeout이 실패 출구를 연다. Unknown은 결국 대사되고, stop은 결국 Resume된다. 종료 순위는 설계 §8의 사전식 순서다. 예산을 쓰는 간선(work self-loop, review repair)은 `used`를 늘리고, 나머지 간선은 graph 위치를 앞으로 옮긴다. 공유 예산 `q`는 작업당 하나다.

`LayoutIndependent`는 모델에 넣지 않았다. layout은 실행 변수에 아무 영향이 없도록 설계했으므로 그 행동은 stuttering이 된다.

## P2-3: stop 속성 분해

- **새 intent는 RequestStop commit부터 막는다.** 전환 commit은 lock 안에서 control을 다시 읽는다. `running`일 때만 다음 entry intent를 같은 commit에 쓴다(`NoNewIntentAfterStop`).
- **이미 전달 중인 효과는 host gate와 fence의 선후로 분류한다.** `HostLaunch`는 intent의 fence가 현재 fence와 같거나 FinishStage grant가 있을 때만 연다. launch 중(`launching`/`unknown`)이던 효과는 둘 중 하나로 정해진다. 개시가 fence보다 먼저였으면 `Receipt`로 started가 되고, fence가 먼저 닿았으면 `FenceFirst`로 미전송 취소가 된다.
- **FinishStage grant.** `RequestStop("finish")` 순간의 상태로 grant를 정한다. 아직 시작하지 않았거나 진행 중인 효과는 frozen grant를 받는다. launch 중인 효과는 provisional grant를 받는데, 늦은 started receipt가 오면 frozen으로 확정되고 fence가 먼저였으면 철회된다. grant를 받은 효과는 끝까지 실행한다(`FinishStageKeepsGrant`). 그 결과의 출구를 확정하고 다음 cursor를 기록할 수는 있다. 그러나 다음 entry intent는 만들지 않고 `entry` 표지만 남긴다(`NoNewIntentAfterStop`, `NoLaunchAfterFence`).
- **Immediate 강화.** FinishStage 도중 Immediate를 요청하면 fence를 다시 올리고 남은 grant를 모두 철회한다. 아직 시작하지 않은 intent는 `HostCancel`로, 진행 중인 효과는 `Interrupt`로 정리한다.

## 리뷰 4절의 빠지기 쉬운 분기

| 분기 | 모델 위치 |
|---|---|
| Start 전 `state_id=null`과 실제 Start 배정 | 초기 `state = "none"`. `BeginRun`이 저장 정의를 pin하고, 작업별 `StartJob`이 pin한 graph의 start(`work`)와 첫 intent를 배정한다. Prepared에서는 참조가 없으므로 삭제가 허용된다(`CanDelete`) |
| self-loop 재방문과 같은 state의 새 승인 | `work → work` retry도 visit을 올린다. 승인 키는 `<<job, gate, visit>>`다. 재방문에서는 새 `impl` 승인이 필요하다(ApprovalRevisitBug가 반례) |
| 명시적 복구로 생기는 새 방문과 adoption만으로 유지되는 방문 | `Resume`: 중단된 효과(`cancelled`)는 Work anchor에서 새 visit과 새 intent를 받는다(`Recover`). adoption만 할 때는 visit·사실·효과가 그대로다(AdoptResumeBug가 반례) |
| FinishStage frozen/provisional grant와 Immediate 강화 | 위 P2-3 |
| 저장 정의와 실행 pin이 다른 동안의 Resume | 일반 `Resume(FALSE)`는 `savedReview = activeReview`일 때만 된다. 다르면 `Resume(TRUE)`(adoption)만 가능하다. adoption은 모든 살아 있는 cursor가 저장 graph에 있을 때만 받아들인다. 승인은 폐기하고 fence는 올린다 |
| 다른 방문의 늦은 사건 | `LateEvent`는 원래 방문의 기록만 대사하고 현재 사실을 바꾸지 않는다(LateEventBug가 반례) |

## 설계 차이(design_delta = 1)

- **FinishStage가 기록한 다음 cursor의 "entry 미개시" 표지.** 설계 §6.3은 "선택 전환·새 cursor·다음 action 의도를 원자적으로 commit"한다고 쓴다. 리뷰 P2-3은 FinishStage에서 "다음 cursor는 기록하되 다음 작업은 enqueue하지 않는다"고 쓴다. 이 둘을 함께 만족하려면 **그 방문의 entry가 아직 개시되지 않았다**는 사실을 runtime에 영속해야 한다. 그래야 Resume이 같은 방문에서 첫 entry를 정확히 한 번 개시한다. 이 사실이 없으면 두 결과 중 하나가 된다. Resume이 그 방문을 영원히 기다리거나(CrashCommitBug와 같은 정지), 새 방문으로 처리해 재진입한다(AdoptResumeBug와 같은 위반). 모델에서는 `entry[j]`가 이 표지이고, Resume의 `Enter(j)`가 이를 소비한다. C2·C5a의 runtime 필드(설계 §6.3의 `state_visit_id` 옆)와 표 8의 "편집본 Resume"·"일반 Resume" 행에 추가할 것을 권고한다.

## 전제와 한계

- 효과 하나가 "그 방문의 entry 작업 + 검증"을 대표한다. 검증 phase 여러 개, Work/Review seal·digest, 실제 host journal 파일은 기존 `CampaignVerification`·`CampaignNodeLifecycle`(Host 부분) 모델이 맡는다.
- visit id는 0..2를 순환한다. 승인 기록은 현재 방문과 직전 방문 것만 남겨 id가 겹치지 않게 했다. 이는 유한 상태를 위한 추상화이고, 실제 승인은 불변 기록이다.
- 편집은 "점유되지 않은 review 삭제" 한 종류다. 상태 추가, 조건·우선순위 변경, 공유 예산이 서로 다른 max를 가지는 경우는 A2 validator 시험이 맡는다.
- 사용자 Stop에는 공정성이 없다. 영구 stop, 영구 무응답, 영구 Unknown은 liveness 주장 밖이다. DAG 후속 작업과 캠페인 aggregate의 Succeeded도 주장하지 않는다.
- PrepareRetry(새 series)와 Start 자리 승인은 넣지 않았다. 예산 키가 series별이므로 새 series는 새 instance로 본다.

## 진입점 전수표

좌표는 `7ee248ddea` 기준이다. 설계 표 12의 행에 리뷰 P2-2의 Stage 소비, binding digest, 별도 영속화 경계를 더했다. 줄 번호가 기호가 아니라 주석이나 괄호를 가리키던 4곳은 바로잡았다(★). S=`src/services/campaign_execution`, D=`src/domain/campaign`, DC=`dashboard/src/components/campaigns`.

| # | 경계 | 현재 좌표 | 모델의 값·행동 |
|---|---|---|---|
| 1 | 보호 API 등록 | `src/server/routes/domains/admin.rs:32,54` | 모델 밖(권한). 모든 명령의 입구 |
| 2 | 생성·전체 정의 PUT·명령 | `src/server/routes/server_campaigns/mod.rs:115,136,161` | `Validate`·`SaveDelete`(PUT), `BeginRun`·`RequestStop`·`Resume`(명령) |
| 3 | 목록·상세·사건 | `server_campaigns/mod.rs:78,101,181` | 읽기 전용. 상태 변화 없음 |
| 4 | DTO 입력·명령 변환 | `server_campaigns/dto.rs:14,22,38,67` | `Resume(adopt)`의 adopt 인자가 들어갈 자리. 공개 Start/Resume은 아직 닫혀 있다 |
| 5 | 오류·receipt 응답 | `server_campaigns/protocol.rs:57,123,126,129` | 모델 밖. 거절(409/422)은 행동의 enabling 조건이 거짓인 경우에 해당 |
| 6 | 공통 명령 진입 | S/`commands.rs:20,43,63` | 모든 저장 행동의 원자 단위(한 TLA 단계 = 한 트랜잭션) |
| 7 | 생성·편집 reducer | S/`control.rs:254,302` | `SaveDelete`. 최신 상태에서 다시 검증(`CanDelete`) |
| 8 | edit_guard·quiescent·공개 실행 gate | S/`control.rs:47,57,346`(Unavailable `:348,360`) | `SaveDelete`·`Resume`의 `control ∈ {prepared, stopped} ∧ Quiet` |
| 9 | 정의·DAG·hash | D/`definition.rs:116,159,204`(`profile_hash :131`, `graph_changed :225`) | `savedReview`·`activeReview`의 차이가 실행 hash 변경 |
| 10 | DB lock·상태·receipt | `src/db/server_campaigns/command_transaction.rs:48,156,199` | `Commit`·`SaveDelete`·`Resume`이 lock 안에서 다시 읽는 것 |
| 11 | 불변 사실·pin·snapshot 저장 | `src/db/server_campaigns/write.rs:10,48,66,106` | `activeReview`(pin)와 `eff` 기록 보존. `PinAndFactsImmutable`은 이 모델에서 값 불변으로만 나타난다 |
| 12 | 재구성·worker 읽기·event | `src/db/server_campaigns/read.rs:61,76,85,99,115,142` | `Compute`의 lock 밖 읽기(`get_for_worker :99`) |
| 13 | 영속 스키마 | `migrations/postgres/0161_server_campaigns.sql:6,28★,36,99` | `state`·`visit`·`entry`·`used`의 저장 자리(신규 열은 B2/C1) |
| 14 | runtime·attempt·FinishGrant | D/`model.rs:185,266,305,348`(`Stage :43`, `NodeRuntime.stage :306`) | `state`(cursor), `visit`, `grant`, `entry`(design_delta) |
| 15 | StepKey·action ID·await | D/`lifecycle.rs:9,18,50` | `Compute`의 `[j, v, s, ch]`. action ID에 visit이 들어가야 한다 |
| 16 | rules·프로필 | S/`pipeline_rules.rs:26`; S/`runtime_profile.rs:91`; S/`adk_profile.rs:83` | 모델 밖. `BeginRun`이 pin하는 입력 |
| 17 | execution pin | D/`execution_pin.rs:92,112` | `BeginRun`의 pin, `Resume`의 재검사(graph adoption과 별도) |
| 18 | 다음 행동 | S/`scheduler.rs:23` | `Raw`·`Choice`(순수 evaluator) |
| 19 | Work/Review·직접 stage 쓰기 | S/`stage_fsm.rs:96,228,277,391,660,813,821` | `Commit`(공통 전환 경계로 모을 직접 cursor 쓰기 4곳: `:228,391,813,821`) |
| 20 | BeginRun·Resume·입장·완료 | S/`lifecycle.rs:179,215,295,306,338`(직접 쓰기 `:254,295`) | `BeginRun`, `Resume`(`Recover`·`Enter`), `Commit`(`advance :285`) |
| 21 | stop·fence·정산 | S/`control.rs:369,603,631`(강화 `:383`, grant 고정 `:402`, stop intent `:412`, provisional `:430`) | `RequestStop(m)`·`FreezeGrant`·`Settle` |
| 22 | 품질 예산 | S/`failure_budget.rs:13` | `used`, `Counted`, `RetryWithinBudget` |
| 23 | 승인 writer | S/`decision_commands.rs:16,27,60` | `Approve(j, g)`. 키 `<<job, gate, visit>>` |
| 24 | 리뷰·검증 binding | D/`review_gate.rs:109,421,494`; D/`verification_run.rs:130,429,626` | `eff`(effect가 대표). 방문 결속은 C1 |
| 25 | 증거 수용·평가 | D/`evidence.rs:217`; D/`evidence_gate.rs:101,116` | `Observe`·`LateEvent`, `facts`의 U/T/F |
| 26 | 리뷰 dispatch·결과 | S/`review_dispatch.rs:87,211`; S/`review_wait.rs:19,41` | `HostLaunch`·`Finish`(review 방문) |
| 27 | 검증 driver·명령 | S/`verification_driver.rs:151,346`; S/`verification_commands.rs:47` | `HostLaunch`·`Finish`. FinishStage grant 소비 |
| 28 | PR/main/deploy 관측 | S/`adk_observe.rs:173,261,341`; S/`main_integration.rs:175` | `Observe`(서버가 받아들인 관측만 사실이 된다) |
| 29 | worker·boot·효과 개시 | S/`host_worker.rs:75,152,292,345,519` | `Compute`·`Commit`·`Drop`(`poll :152`, `advance :292`), `Crash` |
| 30 | envelope·journal·최종 gate | S/`effect_envelope.rs:117`; S/`host_gate.rs:342,554,643`(FinishStage 예외 `check :511`, `:528-529`) | `GateOpen`·`HostLaunch`·`HostCancel`·`Reconcile`(`recover_journal :342`) |
| 31 | 조회·명령 가능 여부 | S/`view.rs:72,189,211` | `Resume(TRUE/FALSE)`의 enabling 조건을 노출할 자리(C6) |
| 32 | 외부 adapter 노출 | `src/lib.rs:303` | 모델 밖 |
| 33 | dashboard 엄격 decoder | `dashboard/src/api/serverCampaigns.ts:10,26,73,93` | 모델 밖(A3b) |
| 34 | dashboard 읽기·쓰기 | `serverCampaigns.ts:195,207,249,253,262,266` | `SaveDelete` 호출자 |
| 35 | form·명령·복사 | DC/`ServerCampaignForm.tsx:148`; DC/`CampaignsPanel.tsx:148,197,273` | `Validate`→`SaveDelete` 순서의 UI 측 |
| 36 | 상세·라벨·제어·poll·holds | DC/`ServerCampaignDetail.tsx:50`; DC/`serverCampaignModel.ts:38`; DC/`ServerCampaignControls.tsx:51`; DC/`useServerCampaignReads.ts:17`; DC/`useServerCampaignHolds.ts:10` | 모델 밖(표시) |
| 37 | legacy 원장 writer | `src/server/routes/campaigns.rs:195,210,223,266`; `src/db/campaigns.rs:518,545,576` | 모델 밖. server 원장과 따로이고, 실행 증거를 이어받지 않는다 |
| 38 | legacy sync·자동큐 | `src/github/sync/campaign_nodes.rs:17`; `src/db/campaigns/issue_sync.rs:163,191,271,329`; `src/services/auto_queue/campaign_handoff.rs:124,324,446` | 모델 밖 |
| 39 | 별도 legacy pipeline API | `src/server/routes/domains/ops.rs:106`; `src/server/routes/pipeline.rs:92,112,129,279,317` | 모델 밖 |
| 40 | legacy UI 저장·카드 추가 | `dashboard/src/api/campaigns.ts:188`; DC/`QueueAddToCampaign.tsx:22,29` | 모델 밖 |
| 41 | layout | DC/`campaignViewStorage.ts:5,83,92,108`; DC/`CampaignRelationsEditor.tsx:213` | 실행 변수 없음(`LayoutIndependent` = stuttering) |
| 42 | 공개 API inventory | `src/server/routes/docs/inventory/endpoints/server_campaigns.rs:4` | 모델 밖 |
| 43 | Stage 소비: `TestEvidence::validate_execution_subject` | D/`profile.rs:450`(stage 대조 `:457`) | `ConditionFromCurrentVisit`. stage 대조 대신 (state, visit, graph pin) 대조 |
| 44 | seal의 binding digest | D/`candidate_seal.rs:46`(`digest_of :78`) | 모델 밖(과거 digest 재현 decoder, C1). `PinAndFactsImmutable` |
| 45 | begin_review의 digest 재검사 | S/`stage_fsm.rs:325★` | `Commit` 앞의 보호 가드(모델에서는 `Decidable`에 포함) |
| 46 | 증거 저장 | `src/db/server_campaigns/evidence_store.rs:89` | `facts`의 영속 기록 |
| 47 | host journal | S/`host_journal.rs:31★`(`write_durable :105`) | `eff.st`의 `launching/unknown`, `Crash`·`Reconcile` |
| 48 | 이력 정리 | `src/services/maintenance/jobs/db_retention/record_caps.rs:98★` | pipeline snapshot은 run과 함께 보존한다. 정리 대상인 표시 이력을 복구 근거로 쓰지 않는다(모델 밖) |
| 49 | 승인 gate·binding | D/`workflow_options.rs:5`; D/`decision_item.rs:20,60`; S/`lifecycle.rs:43,78,95` | `Approve`, `Exact`, `Approved` |
| 50 | 직접 cursor 초기화 | D/`model.rs:351`(`Stage::Requirements` 기본값) | `state = "none"`(null start)으로 바꿀 자리 |

`rg`로 `.stage =`의 쓰기 위치를 확인했다. 시험 밖에서는 `stage_fsm.rs:228,391,813,821`, `lifecycle.rs:254,295`, `model.rs:351` 7곳이고, D/에는 쓰기가 없다. 이 7곳이 C2의 공통 전환 경계로 모일 대상이다.

## 재실행

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
python3 scripts/ci/statecharts_tlc.py --base 7ee248ddea --head HEAD --jobs 2
cd docs/statecharts/campaign-execution
~/.claude/skills/statechart/scripts/statechart CampaignPipeline.tla --config CampaignPipelineChart.cfg --view pipeline.view.mjs --out /tmp/sc-pipeline
```

README 색인은 `index.d/campaign-pipeline.md`와 `python3 scripts/ci/statechart_index.py`로만 생성했다.
