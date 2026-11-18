# Campaign node lifecycle: start, Work, Review, Landing, confirmation

모델은 M1 head `3093a05bc934390b2702cc1714d3da7f6b23f8b7` 위에서 작성했고, 아래 source 대조는 M2 구현 작업트리 기준이다. **모델 결과는 Rust 시험 통과나 실제 이슈 완주의 증거가 아니다.** 공개 Start·Resume·Retry는 계속 `ExecutionUnavailable`이며 이 모델의 `BeginRun`·`Resume`은 공개 DTO에 없는 내부 명령이다. 기존 [실행 검증 FSM](verification.report.md), [승인 gate](approval-gate.report.md), [profile pin](profile-pin.report.md), [명령 영수증](receipt.report.md) 모델은 바꾸거나 복제하지 않았다. V phase의 개시·Unknown·두 stop의 세부는 기존 CampaignVerification 모델이 맡는다.

[모델](CampaignNodeLifecycle.tla), [전체 cfg](CampaignNodeLifecycle.cfg), [차트 cfg](CampaignNodeLifecycleChart.cfg), [view](node-lifecycle.view.mjs).

## 검사 결과

공식 `scripts/ci/statecharts_tlc.py`로 **9 cfg**를 대조한다. 정상 3개(기본·Chart·GatesOff)는 5 invariant와 liveness 1개를 검사하고, 부정 대조 6개는 지정한 규칙을 위반한다. 기본 설정은 **4,716 distinct states**, Chart 설정은 **101 distinct states**다. 기본 설정에서 완료(`stage = "complete"`), 예산 소진 실패, 그리고 "수리 1회 + stop·편집·재개 + driver 재시작 뒤 Landing 승인까지 도달"이 모두 도달 가능함을 임시 부정 invariant로 확인했다(공허한 GREEN이 아님).

| 설정 | 제거한 경계 | 예상·실제 결과 | 반례가 뜻하는 것 |
|---|---|---|---|
| 기본 / Chart / GatesOff | 없음 | GREEN | 네 자리 모두 ON·리뷰 하나만 ON·전부 OFF에서 같은 규칙이 성립 |
| Before | 단계 진입의 승인 대조(`CheckGates = FALSE`) | RED `NoUnapprovedEntry` | M2 이전 `assign_work`·`begin_review`처럼 승인 기록을 읽지 않으면 설정된 자리 없이 Work에 들어간다 |
| StaleTarget | candidate·revision 결속(`TargetBound = FALSE`) | RED `NoUnapprovedEntry` | 옛 candidate나 옛 정의 revision의 승인으로 수리 뒤 새 candidate가 진입한다 |
| Replay | 위치 대조(`PositionGuard = FALSE`) | RED `NextActionAtMostOnce` | 응답을 잃고 다시 보낸 같은 행동이 두 번 효과를 낸다 |
| BudgetIgnored | 실패 예산 정지(`BudgetStops = FALSE`) | RED `NoRetryAfterBudget` | 예산을 다 쓴 노드에 Work가 다시 배정된다 |
| KeepsLanding | 미개시 Landing admission 취소 | RED `LandingAdmissionCurrent` | stop 뒤 재개하면서 지난 epoch의 admission을 유지한다 |
| NoRecompute | 재시작 뒤 저장 상태에서 재계산(`Recompute = FALSE`) | RED liveness `ApprovedNodeMoves` | driver가 메모리의 행동만 믿으면 재시작 뒤 승인된 노드가 그 자리에 멈춘다 |

`Before`는 승인 소비가 없던 M1 head의 `stage_fsm` 진입 경로를 추상화한 설계 대조다. M1 head에는 내부 start 자체가 없어 실제 운영에서 그 진입이 일어났다는 재현이 아니다. 수정 전 RED·수정 후 GREEN의 Rust 재현은 레인 보고서에 따로 있다.

## 규칙

- `NoUnapprovedEntry` — 승인 없는 다음 단계 진입 0. Work·Review·Landing 진입 순간, 그 경계에 설정된 자리마다 **현재 candidate(수리 회차)와 현재 정의 revision**의 승인이 있어야 한다. 첫 Work는 착수·구현 두 자리를 모두 요구한다.
- `NextActionAtMostOnce` — 같은 다음 행동 두 번 실행 0. 행동은 계산된 위치(epoch)에서만 적용되고, 적용되면 epoch가 전진하므로 재전송은 효과가 없다.
- `NoRetryAfterBudget` — 실패 예산 초과 뒤 자동 재시도 0. 실패한 노드는 재개 뒤에도 실패로 남는다.
- `LandingAdmissionCurrent` — 미개시 Landing admission은 현재 epoch에만 유효하다. 재개는 다시 Landing 경계 앞에 둔다.
- `SucceededOnlyWhenConfirmed` — main 반영·AC 사실이 기록되기 전에는 완료하지 않는다.
- liveness `ApprovedNodeMoves` — 실행 중이고 실패·완료가 아니며 눈앞 경계의 승인이 갖춰진 노드는 그 자리에 영원히 머물지 않는다(다음 단계, 명시 실패, 또는 사용자의 stop으로 간다).

## 전제와 한계

- liveness는 서버 driver(`Compute`·`Apply`·`Ack`)와 외부 응답(`Outcome`·`Confirmed`)에 weak fairness를 둔다. 즉 **시도 결과와 main·AC 사실이 결국 도착한다는 전제**에서만 성립한다. 자원이 영구히 없거나 관측이 Unknown이면 기다리는 것이 맞고, 그 경우는 이 모델의 주장 밖이다. 승인에는 공정성을 두지 않는다. 사용자가 승인을 영원히 보류하면 노드는 정상적으로 기다린다.
- 승인은 (자리, 수리 회차, 정의 revision) 좌표로만 추상화했다. run·node·gate·target·policy의 여덟 좌표 결속은 [승인 gate 모델](approval-gate.report.md)이 맡는다.
- `Confirmed`는 main 반영·AC 완료·현재 증거 세 사실을 하나로 묶은 외부 사건이다. 이 사실의 생산자(머지·배포 승인 소비 포함)는 M7·M8이며, M2 코드에는 그 생산자가 없다. 따라서 **M2만으로는 운영에서 완료에 도달할 수 없다.**
- `Stop`은 열린 시도를 중단시키고 조용해진 상태까지를 한 단계로 접었다. fence·ack·세션 Gone 대사는 기존 모델과 `control.rs`의 quiescent 검사가 맡는다. 재개는 Work·Review에 있던 노드를 같은 수리 회차의 새 Work로 돌리고 미개시 Landing admission에서는 현재 승인을 다시 확인한다. 품질 실패 수는 늘지 않는다.
- 노드 하나만 모델링했다. DAG 순서·작업 2개·선택 설계 단계·수동 retry 계열은 범위 밖이다.
- `statechart` wrapper가 WSL에 없어 차트 그림(`.chart.mmd`)과 도구 보고서는 만들지 않았다(`sc=not-run`). view와 Chart cfg는 맥에서 그릴 수 있게 남겼다.

## 진입점 전수표

좌표는 M2 구현 작업트리 기준이다.

| 진입점·소비자 | 코드 좌표 | 모델 행동·구현 연결 |
|---|---|---|
| 공개 Start·Resume·Retry | `control.rs:346`, `control.rs:350`, `view.rs:203`, `dto.rs:38` | 모델 밖. 그대로 `ExecutionUnavailable`. 승인·수명 상태가 공개 실행을 열지 않는다 |
| 내부 수명 명령 dispatch | `control.rs:362`, `lifecycle.rs:379` | `BeginRun`·`Resume`·`Apply(land/complete)`. 변경 없음은 기존 unchanged 경로(event 0) |
| run 시작 | `lifecycle.rs:182` | `BeginRun`. Prepared에서만 새 run을 만들고, 같은 ID 재전송은 무변화, 다른 ID는 거절 |
| Work 진입(첫 Work·수리 Work) | `stage_fsm.rs:145,172`, `lifecycle.rs:113,95,102` | `Apply(assign)`. Requirements 또는 Implementation에서만. 선행 노드 main 반영, 착수(첫 Work)·구현 자리의 exact 승인 |
| 승인 대조 | `lifecycle.rs:43`, `lifecycle.rs:78` | `Passes`. 현재 pointer의 revision·candidate(base/head)·pin hash·승인 기록을 모두 대조 |
| Review 진입 | `stage_fsm.rs:355`, `stage_fsm.rs:389` | `Apply(review)`. 봉인된 출력과 pin에 결속한 리뷰 자리 승인. 인계한 candidate를 fence에 기록 |
| 단계 결과·실패 예산 | `stage_fsm.rs` `accept_stage_result`, `failure_budget.rs:13` | `Apply(accept)`. 기존 코드 그대로. 확정 품질 실패만 세고 예산 소진은 Failed |
| 예산 소진 뒤 재배정 차단 | `stage_fsm.rs:149` | `Wanted`의 failed 분기. 기존 거절을 유지 |
| Landing 승인(admission) | `lifecycle.rs:308`, `lifecycle.rs:149` | `Apply(land)`. 리뷰를 통과한 그 candidate가 현재 정의에 결속돼 있고 착지 자리 승인이 있어야 한다 |
| 확인·완료 | `lifecycle.rs:340`, `lifecycle.rs:160,178` | `Apply(complete)`. 세 사실과 노드가 시작한 시도·action·세션의 종결을 요구. 마지막 노드에서 Succeeded |
| 재개 | `lifecycle.rs:217`, `control.rs:47` | `Resume`. Stopped이고 조용할 때만. fence 세대 전진, 중단된 Work·Review는 새 Work로, 실패 노드는 그대로 |
| 다음 행동 계산 | `scheduler.rs:23`, `domain/campaign/lifecycle.rs` `StepKey` | `Compute`·`Wanted`. 저장 상태만 읽는 순수 함수. 위치(epoch)가 시도·launch ID를 정한다 |
| 재전송 무효 | `stage_fsm.rs` `assign_work`·`begin_review`의 기존 시도 분기, `lifecycle.rs:319` | `PositionGuard`. 같은 시도 ID·같은 위치의 재전송은 무변화 |
| stop·편집 | `control.rs` `RequestStop`·`EditDefinition` | `Stop`·`Edit`. 기존 코드 그대로. 편집은 revision을 올려 옛 승인을 무효로 만든다 |

`rg`로 `Command::Start|Resume|PrepareRetry`, `stage_fsm::assign_work|begin_review|accept_stage_result`, `lifecycle::`, `scheduler::next_action`의 호출부를 확인했다. `next_action`의 운영 호출자는 아직 없다(M3 host worker가 첫 소비자). 내부 명령은 `commands::execute`로만 들어오며 HTTP route는 닫힌 `PublicCommand`만 변환한다.

## 재실행

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
python3 scripts/ci/statecharts_tlc.py --base <M1 head> --head HEAD
```

README 색인은 `index.d/campaign-node-lifecycle.md`와 `python3 scripts/ci/statechart_index.py`로만 생성했다.
