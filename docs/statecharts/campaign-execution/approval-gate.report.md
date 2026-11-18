# Campaign approval gate: binding, replay and omitted edits

모델은 M0 `735880f5d7359e00693476cbfc35d88013363eeb` 위에서 작성했고, 아래 source 대조는 M1 구현 작업트리(조사 시 HEAD `1b282116298d04222af306e4cd554ed0f6fca82d`) 기준이다. 승인된 범위의 정의·명령·기록·fresh edit guard가 연결됐으며, 실행 소비자는 후행 조각 책임이다. **모델 결과는 Rust/DB 시험 통과나 실제 이슈 완주의 증거가 아니다.** 현재 head의 시험·뮤턴트·정적 결과는 레인 구현 보고서에서 별도로 기록한다. 기존 [실행 검증 FSM](verification.report.md), [profile pin](profile-pin.report.md), [명령 영수증 모델](receipt.report.md)은 변경하거나 복제하지 않는다.

[모델](CampaignApprovalGate.tla), [전체 cfg](CampaignApprovalGate.cfg), [차트 cfg](CampaignApprovalGateChart.cfg), [생성 차트](approval-gate.chart.mmd), [전체 도구 보고서](approval-gate-full.tool.report.md), [차트 도구 보고서](approval-gate.tool.report.md).

## 검사 결과

최종 모델의 전체 정상 설정은 **810 distinct states**, 차트 설정은 **135 states / 346 steps / 6 chart states / 11 arrows**다. 공식 `scripts/ci/statecharts_tlc.py`로 신규 **13/13 cfg expected 일치, 26 TLC runs, 18.9초**, 미확인 규칙 0을 확인했다. 정상 2개 GREEN, 의도한 부정 대조 11개 RED다. `Before`는 아래 세 경계를 모두 제거한 설계 대조이며 **M0 main이 승인 명령을 잘못 수용한다는 재현이 아니다**. M0 공개 DTO에는 approve 자체가 없었고, M1에서 좁은 approve만 추가했다.

| 설정 | 제거한 경계 | 예상·실제 결과 | 가장 짧은 경로 |
|---|---|---|---|
| 기본 / Chart | 없음 | GREEN | 4 invariants와 NoDeadlock 통과 |
| Before | 결속·멱등·생략 편집 보호 | RED 3규칙 | 아래 세 반례를 모두 탐지 |
| NoRun / NoNode / NoGate / NoRevision | 해당 결속 좌표 | RED NoWrongBindingAccepted | 설정 → gate 열기 → 다른 key 승인, 3단계 |
| NoTargetKind / NoTargetIdentity / NoTargetHead / NoPolicy | 해당 target/policy 좌표 | RED NoWrongBindingAccepted | 설정 → gate 열기 → 다른 key 승인, 3단계 |
| Duplicate | 이미 승인된 full key 재수용 | RED SameGateAcceptedAtMostOnce | 설정 → gate 열기 → 승인 → 재수용, 4단계 |
| Omitted | 빠진 옵션을 OFF 기본값으로 덮어쓰기 | RED OmittedEditNeverClearsGate | 설정 → 옛 폼 편집, 2단계 |

`NoWrongBindingAccepted`는 요청 key와 현재 기대 key가 열린 gate의 full key와 모두 같아야 새 승인 효과가 생김을 확인한다. `SameGateAcceptedAtMostOnce`는 같은 key의 유효한 재승인이 수용 횟수를 늘리지 않음을, `OmittedEditNeverClearsGate`는 생략된 편집 입력이 설정을 끄지 않음을 확인한다. `ApprovalDefaultsOff`는 명시적인 설정 전 여섯 자리가 모두 OFF임을 확인한다. 명시된 옵션을 보내 OFF로 바꾸는 편집은 revision을 올리며 허용한다.

## 상태와 key

한 개의 관찰 대상 gate의 `absent → open → approved`와 full-key history를 투영한다. key 좌표는 run / node / slot / definition revision / target kind / target identity / target head / policy hash다. Start·Implementation·Review·Landing의 target은 Candidate(base SHA, head SHA), Merge는 PullRequest(number, head SHA), Deploy는 Main(SHA)로 시작한다. Main은 identity 좌표가 SHA이며 head 좌표는 padding이다. 다른 variant·불일치 입력까지 비교하는 것은 보수적인 입력 범위이며 실제 target shape 검증은 Rust 진입점 시험이 맡는다.

`current`는 **내부 Open 명령이 해당 run/node/slot에 등록한 gate pointer의 기대 binding**이다. 실제 checkout·attempt·PR·main·policy를 관측한 값이나 전체 캠페인의 단 하나뿐인 전역 gate를 뜻하지 않는다. 다른 자리의 기록은 독립적으로 남는다. target/policy에 대한 `ChangeCurrentBinding`은 새 내부 Open으로 pointer가 바뀐 경우를 나타내며, 외부 artifact 변화가 자동으로 서버에 관측된다는 가정은 없다. run/revision 변경은 순수 결정의 현재 state 대조로 거절한다. target A → B → A는 `RestoreTargetBinding`으로 관찰 대상 A의 full-key history를 다시 가리키되 `approved`와 수용 횟수를 보존한다. 실제 `ApprovalBook::open`도 기존 item을 `entry(...).or_insert(...)`로 남기고 pointer만 바꾼다. revision/run/policy를 과거 값으로 되돌리는 행동은 이 target 재open에 포함하지 않았다.

M1의 `context`는 현재 definition revision·run ID·node 존재·Running/stop·required slot을 확인한다. target의 실제 head/PR/main 관측과 policy hash의 실제 snapshot 대조는 하지 않는다. **M2/M4/M6/M8의 producer와 최종 consumer는 실제 관측을 같은 binding과 대조하고, 변화한 target/policy를 새 gate pointer로 등록한 뒤 그 binding의 승인만 소비해야 한다.** 등록 pointer와 같은 승인이라는 사실만으로 현재 artifact가 검증됐다고 주장하지 않는다.

승인 provenance는 모델에서 유효한 같은 입력이라는 추상 전제다. Rust 구현은 동일 binding·동일 `ForumApproval`을 no-op으로 처리하고 최초 source/evidence/recorded_at을 보존하며 다른 evidence는 충돌로 거절한다. 서버가 고정하는 출처는 `CoordinatorRecordedForum`이다. `ForumApproval::validate`는 빈 decision ref와 HTTPS/host가 없는 response ref를 거절하고 response_at은 typed timestamp로 받는다. 원문 조회·작성자 인증·실제 선택·시각의 사실 확인은 수행하지 않는다. approve 명령의 선택과 코디네이터가 확인해 넣은 원문 위치·시각을 기록하는 계약이며 서버의 포럼 사용자 직접 인증으로 표현하지 않는다.

## 진입점 전수표

아래 좌표는 M1 구현 작업트리 기준이다. 내부 pointer의 현재성과 외부 artifact 관측을 구분한다.

| 진입점·소비자 | 현재 코드 좌표 | 모델·구현 연결 |
|---|---|---|
| 정의 생성·옛 정의 복원 | `src/server/routes/server_campaigns/mod.rs:115`, `dto.rs:14`, `domain/campaign/definition.rs:16,22,27`, `workflow_options.rs:14` | 옵션 누락은 None으로 보존하고 effective options는 여섯 자리 OFF. 명시 object는 여섯 bool을 모두 요구하므로 부분 누락을 OFF로 채우지 않음 |
| 정의 편집 | `server_campaigns/mod.rs:136`, `dto.rs:22`, `services/campaign_execution/control.rs:301,306,308,339`, `decision_commands.rs:16` | fresh edit는 revision/edit guard 다음에 옵션 None을 거절하고, 명시 편집만 검증·revision 증가로 진행 |
| 공개 commands endpoint | `server_campaigns/mod.rs:161`, `dto.rs:31,38,68,100` | flat Approve만 boxed domain binding/evidence로 변환. Open은 공개 DTO에 없고 `domain/campaign/model.rs:368`의 내부 명령 |
| 명령 서비스 dispatch | `services/campaign_execution/commands.rs:43,63,71,95,106`, `control.rs:293,295,297` | prepare 뒤 live state의 순수 child 결정. child가 변경 없음이면 기존 unchanged 경로라 새 event·기록 효과 0 |
| 현재 run/revision/slot | `services/campaign_execution/decision_commands.rs:27,28,31,36,46,49,69` | 이미 승인된 gate의 fresh 재승인도 context를 먼저 검사. required OFF·중단 상태의 fresh Open/Approve를 거절 |
| target 형식·full key | `domain/campaign/decision_item.rs:12,20,78,104,108` | gate별 target variant와 필수값을 검증. full binding hash를 item ID로, node/slot hash를 run 내부 pointer ID로 사용 |
| gate 열기·A→B→A | `decision_item.rs:129,138,141,145`, `decision_commands.rs:75` | 같은 pointer면 no-op. 새 pointer는 기존 full-key item을 보존하므로 A 재open이 승인 기록을 초기화하지 않음 |
| 명시 승인·중복·출처 | `decision_item.rs:149,158,162,165,172`, `decision_commands.rs:83` | pointer·저장 binding 대조 뒤 동일 evidence는 no-op, 다른 evidence는 Conflict. 최초 승인에만 source/evidence/recorded_at 기록 |
| 실제 target/policy 관측 | `decision_commands.rs:27`, `decision_item.rs:78,104`의 검사 범위 | M1은 등록 pointer 결속만 검사. 실제 source/current target·policy를 확인해 등록하고 마지막에 같은 binding을 소비하는 producer/final consumer는 M2/M4/M6/M8 계약 |
| 직렬화된 현재 상태 관측 | `db/server_campaigns/command_transaction.rs:71,78,109` | campaign/run lock 뒤 get_in. 두 fresh 호출은 이 경계에서 순서대로 같은 기록을 결정하므로 모델은 원자적인 승인 행동을 사용 |
| 같은 key의 receipt 재생 | `command_transaction.rs:80,89,92`, `commands.rs:72` | 기존 성공 응답은 live state 검사 전에 재생할 수 있음. 상태 변경·신규 승인 수용은 0이며 ApproveGate 새 효과와 구분 |
| 상태/receipt 영속화 | `command_transaction.rs:171,199,213`, `commands.rs:59`, `domain/campaign/model.rs:298` | 기존 state write·receipt·commit 경계로 Run JSON의 approval book을 저장. 새 DB schema 없음 |
| 옛 Run 복원 | `db/server_campaigns/read.rs:55,68`, `domain/campaign/model.rs:298` | gate book 없는 데이터는 empty로 복원. 누락 데이터를 이미 승인한 gate로 추측하지 않음 |
| 공개 실행 불가 | `services/campaign_execution/control.rs:42,345,349,353,359`, `view.rs:198,268` | 기존 Unavailable 유지. 승인 기록만으로 Start/Resume/Retry를 열지 않으며 실제 소비는 M2 이후 책임 |

`rg`로 Create/Edit/CommandRequest/PublicCommand와 create/execute/apply_in·control dispatch·read/write 진입점을 확인했다. HTTP create/edit/commands는 모두 기존 명령 서비스의 같은 transaction 경계를 사용한다. M1에서 공개 approve와 내부 open을 연결했으며, gate 승인을 실제 실행 권위로 소비하는 producer/consumer는 아직 없다.

### Rust 시험과 legacy receipt의 대조

현재 추가된 수용 시험의 역할은 아래와 같다. 이는 시험 소스를 대조한 목록이며 실행·통과 수는 레인 구현 보고서의 현재 head 증거를 따른다.

| 시험 | 확인할 경계 |
|---|---|
| `decision_commands_tests.rs`의 `approve_requires_every_binding_coordinate_to_match_the_current_gate` | 여덟 좌표별 불일치 거절과 원래 binding의 정상 수용 |
| `same_gate_repeated_approval_preserves_the_first_record_without_another_event` | 다른 idempotency key·늦은 관측 시각의 재승인도 최초 source/evidence/recorded_at·state·event count 보존 |
| `target_change_blocks_old_approval_and_reopening_a_keeps_its_full_key_history` | 새 내부 Open으로 B를 등록하면 A 거절, A 재open 때 두 history item과 최초 승인 보존 |
| `fresh_approval_rechecks_the_current_run_and_revision_even_after_approval` | Approved 분기보다 현재 run/revision 대조가 먼저임을 검증 |
| `a_changed_forum_response_conflicts_with_an_approved_gate` | decision_ref/response_ref/response_at 중 하나가 다른 evidence를 각각 Conflict로 거절 |
| `omitted_workflow_edit_cannot_switch_off_configured_gates_but_explicit_off_can` | 실제 None인 생략 편집을 거절하고 명시 OFF 편집은 수용. 유효한 change reason으로 다른 편집 guard의 우연한 거절을 피함 |
| `server_campaigns/decision_tests.rs`의 DTO 두 시험 | flat 공개 승인 전체 매핑과 internal Open·가짜 source·혼합 target의 closed serde 거절 |

legacy receipt는 이 fresh 검사와 다른 경계다. `command_transaction.rs:80~107`은 같은 key/fingerprint의 저장 응답을 live state 읽기(:109) 전에 재생한다. `control::decide_with_receipt`도 `control.rs:203~219`에서 같은 fingerprint를 확인한 뒤 현재 state를 그대로 두고 events/records 0으로 반환한다. 따라서 revision이나 target pointer가 달라진 뒤 과거 성공 응답을 다시 받는 것은 가능하지만 새 gate 승인 수용이나 현재 실행 허가가 아니다. 과거 성공 receipt를 최종 consumer의 현재 승인 대조 대신 사용하면 안 된다. 같은 key에 다른 command/evidence를 보내면 fingerprint 충돌이고, 새 key로 다시 승인하면 위 context·book 검사를 모두 거친다. 옛 폼의 같은 명령 replay는 이 기존 계약을 보존하며, 옵션 생략 거절은 fresh edit에만 적용한다.

## 한계와 재실행

RED 경로는 약화한 논리 경계의 부정 대조이며 실제 source 뮤턴트와 동일하지 않다. 특히 NoTargetKind의 잘못된 variant 입력은 Rust의 gate/target 형식 검증이 먼저 거절할 수 있다. full-key hash·저장 binding equality·shape 검사에 중복 보호가 있으므로 source 원인 제거 뮤턴트는 Rust 시험에서 별도로 확인해야 한다. API DTO·hash bytes·출처 필드·DB JSON 복원은 모델 범위 밖이다. 등록 pointer 변경 뒤 기존 승인을 새 등록 대상의 권위로 쓰지 않는 경계를 검사하며, 실제 artifact 변경의 관측과 최종 Work/Review/merge/deploy 소비는 후행 모델·시험 책임이다. 공개 execution Unavailable은 기존 코드 좌표로 확인했으며 이번 모델은 실행 FSM을 추가하지 않았다.

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
