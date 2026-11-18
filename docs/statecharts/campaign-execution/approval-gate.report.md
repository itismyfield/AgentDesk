# Campaign approval gate: binding, replay and omitted edits

코드 기준은 M0 `735880f5d7359e00693476cbfc35d88013363eeb`다. **이 산출물은 M1 구현 전 모델이며 prod 구현/수용 완료 증거가 아니다.** `control.rs` child dispatch·편집 guard 등의 FILES 승인이 필요해 구현을 시작하지 않은 상태에서 작성했다. 아래 신규 API·저장 shape는 예정 계약이다. 기존 [실행 검증 FSM](verification.report.md), [profile pin](profile-pin.report.md), [명령 영수증 모델](receipt.report.md)은 변경하거나 복제하지 않는다.

[모델](CampaignApprovalGate.tla), [전체 cfg](CampaignApprovalGate.cfg), [차트 cfg](CampaignApprovalGateChart.cfg), [생성 차트](approval-gate.chart.mmd), [전체 도구 보고서](approval-gate-full.tool.report.md), [차트 도구 보고서](approval-gate.tool.report.md).

## 검사 결과

최종 모델의 전체 정상 설정은 **810 distinct states**, 차트 설정은 **135 states / 346 steps / 6 chart states / 11 arrows**다. 공식 `scripts/ci/statecharts_tlc.py`로 신규 **13/13 cfg expected 일치, 26 TLC runs, 18.9초**, 미확인 규칙 0을 확인했다. 정상 2개 GREEN, 의도한 부정 대조 11개 RED다. `Before`는 아래 세 경계를 모두 제거한 설계 대조이며 **기존 main이 승인 명령을 잘못 수용한다는 재현이 아니다**. 기존 공개 DTO에는 approve 자체가 아직 없다.

| 설정 | 제거한 경계 | 예상·실제 결과 | 가장 짧은 경로 |
|---|---|---|---|
| 기본 / Chart | 없음 | GREEN | 4 invariants와 NoDeadlock 통과 |
| Before | 결속·멱등·생략 편집 보호 | RED 3규칙 | 아래 세 반례를 모두 탐지 |
| NoRun / NoNode / NoGate / NoRevision | 해당 결속 좌표 | RED NoWrongBindingAccepted | 설정 → gate 열기 → 다른 key 승인, 3단계 |
| NoTargetKind / NoTargetIdentity / NoTargetHead / NoPolicy | 해당 target/policy 좌표 | RED NoWrongBindingAccepted | 설정 → gate 열기 → 다른 key 승인, 3단계 |
| Duplicate | 이미 승인된 full key 재수용 | RED SameGateAcceptedAtMostOnce | 설정 → gate 열기 → 승인 → 재수용, 4단계 |
| Omitted | 빠진 옵션을 OFF 기본값으로 덮어쓰기 | RED OmittedEditNeverClearsGate | 설정 → 옛 폼 편집, 2단계 |

`NoWrongBindingAccepted`는 요청 key와 현재 기대 key가 열린 gate의 full key와 모두 같아야 새 승인 효과가 생김을 확인한다. `SameGateAcceptedAtMostOnce`는 같은 key의 유효한 재승인이 수용 횟수를 늘리지 않음을, `OmittedEditNeverClearsGate`는 생략된 편집 입력이 설정을 끄지 않음을 확인한다. `ApprovalDefaultsOff`는 명시적인 설정 전 여섯 자리가 모두 OFF임을 확인한다. 명시된 옵션을 보내 OFF로 바꾸는 편집은 revision을 올리며 허용한다.

## 상태와 예정 key

한 개의 관찰 대상 gate의 `absent → open → approved`와 full-key history를 투영한다. key 좌표는 run / node / slot / definition revision / target kind / target identity / target head / policy hash다. Start·Implementation·Review·Landing의 target은 Candidate(base SHA, head SHA), Merge는 PullRequest(number, head SHA), Deploy는 Main(SHA)로 시작한다. Main은 identity 좌표가 SHA이며 head 좌표는 padding이다. 다른 variant·불일치 입력까지 비교하는 것은 보수적인 입력 범위이며 실제 target shape 검증은 Rust 진입점 시험이 맡는다.

`current`는 관찰 대상 승인 동작의 현재 기대 binding이며 전체 캠페인의 단 하나뿐인 전역 gate를 뜻하지 않는다. 다른 자리의 기록은 독립적으로 남는다. `ChangeCurrentBinding`은 각 결속 좌표의 불일치를 검사한다. target A → B → A는 `RestoreTargetBinding`으로 관찰 대상 A의 full-key history를 다시 가리키되 `approved`와 수용 횟수를 보존한다. 이 때문에 A를 다시 열어도 한 번 수용된 승인이 초기화되지 않는다. revision/run/policy를 과거 값으로 되돌리는 행동은 이 재open에 포함하지 않았다.

승인 provenance는 유효한 같은 입력이라는 추상 전제다. 예정 구현은 동일 binding·동일 provenance의 재승인을 no-op으로 처리하고 기록을 보존하며 다른 provenance는 충돌로 거절한다. 포럼 원문 위치·시각·선택을 코디네이터가 확인한 출처임을 저장하고 서버의 포럼 사용자 직접 인증으로 표현하지 않는다. provenance 필드·직렬화·위조 방지 검사는 아직 이 모델에서 증명하지 않았으며 구현 후 Rust 시험으로 확인해야 한다.

## 진입점 전수표

아래 기존 좌표는 M0 head 기준이다. 신규 행은 source가 아직 없어 좌표를 만들지 않았다.

| 진입점·소비자 | 현재 코드 좌표 | 모델·구현 연결 |
|---|---|---|
| 정의 생성 | `src/server/routes/server_campaigns/mod.rs:115`, `dto.rs:10`, `domain/campaign/definition.rs:15` | 없는 옵션은 여섯 자리 OFF로 복원할 예정. ConfigureGates 전 초기 상태 |
| 정의 편집 | `server_campaigns/mod.rs:136`, `dto.rs:18`, `services/campaign_execution/control.rs:292,299,329` | 기존 revision/edit guard·검증·증가 경계. 명시 옵션 누락의 fresh edit를 거절하는 child 호출이 필요하며 FILES 승인 대기 |
| 공개 commands endpoint | `server_campaigns/mod.rs:161`, `dto.rs:27,34,55` | narrow Approve DTO 추가 예정. Open은 공개 DTO에 넣지 않고 내부 명령만 사용 |
| 명령 서비스 dispatch | `services/campaign_execution/commands.rs:43,63,71,95,106` | 기존 prepare 이후 live state 안에서 새 child로 Open/Approve를 결정하고 기존 stage/receipt 경계를 재사용할 예정 |
| gate 정의·명령·기록 | 신규 `domain/campaign/workflow_options.rs`, `decision_item.rs`, 기존 `model.rs:276,364,529` | six-slot 옵션, full key·history/current pointer·승인 출처, Open/Approve/Event 저장을 계획. 아직 구현 없음 |
| gate 순수 결정 | 신규 `services/campaign_execution/decision_commands.rs`, 기존 `control.rs:286` | OpenGate·ApproveGate·EditWithOmittedOptions의 실제 guard 예정. required slot OFF면 열지 않고 full-key history를 보존 |
| 직렬화된 현재 상태 관측 | `db/server_campaigns/command_transaction.rs:71,78,109` | campaign/run lock 뒤 get_in. 두 fresh 호출은 이 경계에서 순서대로 같은 기록을 결정하므로 모델은 원자적인 승인 행동을 사용 |
| 같은 key의 receipt 재생 | `command_transaction.rs:80,89,92`, `commands.rs:72` | 기존 성공 응답은 live state 검사 전에 재생할 수 있음. 상태 변경·신규 승인 수용은 0이며 ApproveGate 새 효과와 구분 |
| 상태/receipt 영속화 | `command_transaction.rs:171,199,213`, `commands.rs:59` | 기존 state write·receipt·commit 경계. gate book을 Run JSON에 저장할 예정이며 DB schema를 새로 만들지 않음 |
| 옛 Run 복원 | `db/server_campaigns/read.rs:55,68`, `domain/campaign/model.rs:276` | gate book 없는 데이터는 empty로 복원할 예정. 누락 데이터를 이미 승인한 gate로 추측하지 않음 |
| 공개 실행 불가 | `services/campaign_execution/control.rs:41,335,339,343`, `view.rs:198,268` | 기존 Unavailable 유지. 승인 기록만으로 Start/Resume/Retry를 열지 않으며 실제 소비는 M2 이후 책임 |

`rg`로 Create/Edit/CommandRequest/PublicCommand와 create/execute/apply_in·control dispatch·read/write 진입점을 확인했다. 기존 HTTP create/edit/commands는 모두 명령 서비스의 같은 transaction 경계를 사용한다. base에는 approve/open 명령이나 gate consumer가 없다. 이 표의 예정 접점을 추가할 때 실제 코드 좌표와 반례의 실행 경로를 다시 대조해야 한다.

## 한계와 재실행

RED 경로는 약화한 예정 설계의 부정 대조다. API DTO·hash bytes·출처 필드·DB JSON 복원과 root가 구현할 실제 guard는 아직 재현하지 않았다. approval 후 current target이 변하면 기존 기록을 새 대상의 권위로 쓰지 않아야 한다는 전제를 검사하며, 최종 Work/Review/merge/deploy 소비는 후행 모델·시험 책임이다. 공개 execution Unavailable은 기존 코드 좌표로 확인했으며 이번 모델은 실행 FSM을 추가하지 않았다.

프로세스 crash/DB commit/stop·host 효과는 기존 모델 경계이며 추가 권한이 없다. 사용자가 승인을 영원히 보류해도 정상 대기이므로 liveness를 주장하지 않는다. 차트는 같은 label 안의 유효한 재승인 no-op·거절된 생략 편집·stutter를 숨긴다. 전체 설정은 500-state 그림 상한을 넘으므로 full 보고서만 있으며 차트 설정은 대표 slot 하나로 같은 행동을 그린다.

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
~/.claude/skills/statechart/scripts/statechart \
  docs/statecharts/campaign-execution/CampaignApprovalGate.tla \
  --config docs/statecharts/campaign-execution/CampaignApprovalGateChart.cfg \
  --view docs/statecharts/campaign-execution/approval-gate.view.mjs \
  --out /tmp/campaign-approval-gate-new
```

전체 검사는 기본 cfg, 부정 대조는 각 suffix cfg를 사용한다. 정상 exit 0 / 부정 대조 exit 1이다. README는 `index.d/campaign-approval-gate.md`를 쓰고 `python3 scripts/ci/statechart_index.py`로만 생성했다. raw log·최종 공식 checker JSON 및 source SHA256은 레인 `i7438/m1/logs`와 `model-evidence.md`에 보존한다.
