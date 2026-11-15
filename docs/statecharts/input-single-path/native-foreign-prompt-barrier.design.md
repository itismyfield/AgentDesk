# Native foreign prompt barrier — 보존 포트 확인에서 중단

기준: `bad229c4e8fa4f0f219b2fe993cf01072bfff21e`. mac-1641의 barrier 결정은 수용한다. 제품 코드를 바꾸지 않은 조사/시험 계획이다. **DESIGN_DELTA: Submitted 뒤의 native 실행 불명확을 기존 보존 경로로 정상 종결하는 입구가 없다.** bad229c4e의 착지 금지를 유지한다.

## 반례

격리 fixture의 자기 UUID `N1-1` membership은 `[133,276)`, 자기 assistant end_turn은 `[276,496)`, Stop metadata는 `[496,573)`이다. 자기 strict turn_duration은 없다. 외부 human typed user `[573,795)`, 외부 assistant `[795,978)`, 외부 turn_duration `[978,1079)`가 이어진다.

영속 결과의 membership_row는 `N1-1`인데 closer_range는 외부 `[978,1079)`이며 disposition은 Executed다. 둘째 head가 0이어야 하는 부정 시험에서1이 됐다. 실제 원문은 레인 로그 `b2b3p4-design-delta-proof.json`에 있다. 그 레그의 전체 e2e는 선택3/실행3/통과2/실패1이었다. 이 설계 레그에서 재실행하지 않았다.

## 승인된 barrier 계약

native own membership 후보에 자기 strict closer가 생기기 전에 다른 user prompt가 들어오면 barrier다. 그 뒤 closer는 자기 또는 외부 레이블/ID를 달더라도 기존 own run의 결과를 만들지 않는다. barrier 이전 자기 closer로 확정한 결과는 유지한다. 정상 자기 identity의 mid-turn 입력은 barrier가 아니다.

자기 판정은 full ticket의 identity/attempt/source/episode와 이미 확인된 transcript receipt UUID의 대응을 사용해야 한다. 본문이 같거나 `origin=human`이라는 사실만으로 자기라고 판정하지 않는다. Discord message ID와 transcript UUID를 직접 비교하지 않는다. 현재 `EvidenceRegistration`은 ticket과 receipts를 운반한다(`human_frame/history/v2/native.rs:8`). 수용 직전에는 기존 registration/receipt fingerprint와 current source/actor를 다시 검사한다.

B1 oracle은 ordered records와 registration snapshot에서 run별 barrier를 접는다. 3a projection은 기존 hash pass/동일 capture에서 보존한 prompt identity fact를 같은 순서로 접는다. 원시 JSONL 재독·새 observer·공통 tracker 변경은 없다. 동일 typed 자기 identity가 확인된 mid-turn 행은 두 쪽 모두 통과시킨다. identity가 아직 확인되지 않은 행을 본문 비교만으로 자기로 승격하지 않는다. receipt 변화의 재평가는 기존 facts만 사용한다.

barrier가 결과 없음으로만 빠지면 actor는 기존 Pending으로 남는다. 그것을 mac-1641의 '불명확 정상 종결'이라고 부르지 않는다. 아래 보존 포트 공백을 먼저 해소해야 한다.

## 기존 불명확 경로와 공백

| 현재 좌표 | 실제 계약 | late barrier에 대한 결론 |
|---|---|---|
| `discord/queue_io/input_owner/effect.rs:27` | WriteUncertain은 내용이 pane에 갔는지 불확실한 송신 결과 | 전달이 확정된 Submitted를 이 값으로 바꿀 근거가 아님 |
| 같은 파일 `:125` | Submitted이면 owner.settle(exact_attempt,true,now) | native 실행 종료보다 먼저 입력 전달만 정산함 |
| `turn_orchestrator/queued_head/v2/retained.rs:108` | active/retained exact attempt가 없으면 stale exact attempt | 이미 Submitted 정산한 attempt를 settle(false)로 되돌릴 수 없음 |
| 같은 파일 `:111` | delivered=true는 head/retained와 원본 members 제거, terminal identity 기록 | 후속 O barrier가 올 때 원본 InputAttempt가 더는 해당 위치에 없음 |
| 같은 파일 `:129` | delivered=false + active인 경우만 HeadPhase::Unknown | late Submitted에는 적용 불가 |
| 같은 파일 `:58` | tick이 살아 있는 head를 Unconfirmed retention으로 이동 | 제거된 head를 복구하지 않음. 시간을 앞으로 밀어 native 결과를 만들 수도 없음 |
| 같은 파일 `:246` | 기존 retained unconfirmed + notice enqueue 뒤 owed_to_owner=false | 기존 정상 불명확 종결은 있으나, 해당 retained entry를 만들 late-native 입구가 없음 |
| `queued_head/v2/source_admission.rs:338` | ticket_original은 items/head/retained에서만 원본 검색 | Settled bind의 receipt/source만으로 원본을 반환하지 않음 |
| 같은 파일 `:596` / `v2_types.rs:65` | Settled bind는 attempt nonce/source/시각을 보존 | full InputAttempt의 대체물이 아님 |
| `v2_types.rs:193` | AdmissionContext.original은 선택적 opaque provenance | 모든 native 입력에 존재하는 PendingQueueItem이라고 보장할 수 없음 |
| `human_frame/history/v2.rs:451` | queue terminal identity를 history settled로 투영, 상태 순위는 단조 | settled를 retained로 되돌리는 late uncertainty 입구가 아님 |
| 같은 파일 `:139` | settled passive frame의 표시 필드 compact | 원본 큐 항목/본문의 보존 저장소라고 간주할 수 없음 |
| `discord/turn_finalizer/terminal_handler.rs:378` | NativePrepared는 ticket/actor/row witness/history/cutoff 보유 | 일반 queue InputAttempt/원본 전체를 보유하는 불명확 전달 핸들이 아님 |
| `human_frame/history/v2/native.rs:78` | native disposition 5개에 generic uncertainty 없음 | 새 outcome을 선언하지 않는다. TransferredUnverified는 delivered_to_agent 증거 전용 |
| `discord/turn_finalizer/native_restore.rs:145` | outcome=None이면 drive가 그대로 반환 | barrier 뒤 정상 불명확 종결과 무기한 Pending을 구분하지 못함 |

반례 fixture 종료 뒤 queue 파일은 head=null, items=0, retained_inputs=0, terminal_ids=[81490032,81490033]이다. 레인 로그 `b2b3p4d-existing-uncertain-proof.json`에 별도로 고정했다. barrier가 더 늦게 도착하는 유효 순서도 허용되므로 이 공백을 실행 타이밍으로 피할 수 없다.

현재 불명확 경로 자체가 없다는 결론이 아니다. **native 실행 불명확을, 이미 전달 정산한 exact attempt의 원본 보존으로 넘기는 typed 입구가 부족하다.** Native outcome=None을 그대로 두거나 source 후보를 생략하기만 하면 '불명확 정상 종결' 조건을 충족하지 못한다.

새 상태/저장소/운영자 기능은 설계하지 않았다. 다음 승인 범위에는 기존 retained/unconfirmed 표현을 쓸 수 있는 exact native 보존 handoff, Submitted 전에 원본 custody를 보장하는 지점, late receipt/control/boot가 그것을 없애지 않는 순서가 필요하다. 이 부분을 문서만으로 존재한다고 선언하지 않는다.

## closer 귀속 진입점 전수표

좌표는 위 기준 head다. Git grep 원문은 레인 로그 `b2b3p4d-closer-census.txt`에 있다. 생성과 수용/복원은 구분한다.

| 구분 | 좌표 | 행동 |
|---|---|---|
| 공통 O 경계 | `tui_o/shadow/identity.rs:71`, `shadow/seal.rs:103` | strict E를 Idle로 분류, Open 안 Prompt는 재개방 안 함. 변경 금지 |
| B1 경계 운반 | `tui_o/writer/input_facts.rs:494`, `:620` | tracker close를 Ordered::Closed로 운반 |
| B1 batch 입구 | 같은 파일 `:583` | authenticated full capture → native::fold. 현재 test/unix oracle |
| B1 가입 | `input_facts/native/tests.rs:681` | Opened에 membership/run 연결 |
| B1 closer 귀속 생성 | 같은 파일 `:721` | Closed와 기존 member.run 일치로 Evidence 생성, closer_range는 `:735` |
| 3a compact 입구 | `input_facts/native.rs:238` | facts를 registration snapshot으로 재평가 |
| 3a 경계 운반 | 같은 파일 `:293`, `:325`, `:331` | tracker 결과를 Opened/Closed edge로 운반 |
| 3a closer 귀속 생성 | 같은 파일 `:446`, `:486`, `:500` | run membership에 closed range 귀속 |
| live admission | `discord/turn_finalizer/native_supply.rs:199`, `:237` | exact ticket/target/current evidence → NativePrepared.admit |
| source/range 검증 | 같은 파일 `:455`, `:498` | membership/closer range 정합 검증. foreign prompt barrier 검사는 현재 없음 |
| boot fresh proof | 같은 파일 `:291` | 같은 source_evidence로 Outcome 생성. 별도 우회 경로를 두면 안 됨 |
| live result 저장 | `discord/turn_finalizer/terminal_handler.rs:645`, `:669` | Evidence를 Outcome으로 복사하고 stamped CAS 저장 |
| history CAS | `human_frame/history/v2/native.rs:566` | full registration/receipt와 결과 admission 검사, closer 의미 생성은 아님 |
| saved boot 소비 | `discord/turn_finalizer/native_restore.rs:145`, `:382`, `:436` | 저장 결과/closer 범위 검증·정리; 새로운 실행 결과 생성 아님 |
| queue remove 결과 | `input_facts/native.rs:588`, `native/tests.rs:824` | withdraw/transferred는 closer_range=None. foreign prompt를 이 결과로 바꾸지 않음 |

공통 native policy의 Cancel/Terminate/clear/stop/operator release는 명시 control proof의 종결이며 source closer 귀속과 다르다. barrier가 이 기존 권한을 만들어내거나 지우지 않는다.

## 모델 계획 — 실행하지 않음

변경 대상은 기존 NativeInputAttribution/NativeInputPreparation이다. 추가 행동 후보는 OwnEndTurn, StopMetadataOnly, OwnMidTurnPrompt, ForeignHumanPrompt, ForeignCloser, LateOwnCloser, ExistingUncertainHandoff다. 마지막 행동의 production 포트가 현재 없으므로 모델에 성공 전이를 만들어 GREEN으로 둘 수 없다.

필요 Before/Fixed 쌍:

- `NativeForeignPromptBarrierBefore/Fixed`: 실제 own end_turn→Stop-only→foreign typed→foreign closer의 NoForeignExecuted 반례.
- `NativeForeignPromptLateOwnBefore/Fixed`: barrier 뒤 late own closer도 과거 own 결과를 만들지 않음.
- `NativeOwnMidTurnBefore/Fixed`: 자기 typed identity의 합류 뒤 자기 closer는 결국 정산1.
- `NativeBarrierCustodyBefore/Fixed`: 기존 불명확 보존 handoff 뒤 original 보존, 주입0, 정상 불명확 종결과 Pending 구분, crash/boot에서도 유지.

불변식은 NoForeignExecuted, NoSilentLoss, NoDuplicateInjection이다. 자기 closer가 barrier보다 먼저 도착하면 EventuallySettled를 유지한다. barrier 뒤는 실행 성공으로 바꾸지 않고 기존 불명확 처리의 실제 종결 지점으로 EventuallyUncertain을 증명해야 한다. 무한 Pending을 Uncertain이라고 이름만 바꾸거나 fairness로 입력 순서를 제외하지 않는다.

이번 레그의 TLC는 NOT_RUN이다. 모델/cfg/expected.tsv를 고치지 않았다. 이전126cfg GREEN은 새 반례와 위 handoff를 증명하지 않는다.

## 시험 계획 — 구현하지 않음

| 예정 이름/기존 이름 | 실제 입구 | 단언 | 원인 뮤턴트 |
|---|---|---|---|
| `native_8148_activation_isolated_herdr_stop_only_preserves_original_pg` 유지 | activation_e2e.rs:475 → 실제 O offer/actor/listener | 원본 보존, Executed0, 둘째 head0, 불명확 보존 후 Pending과 구분 | barrier 제거 |
| `native_8148_barrier_own_strict_closer_before_foreign_prompt_pg` | O capture→admit | 자기 결과1, 뒤 외부3턴이 원 결과/응답에 합쳐지지 않음 | 자기 closer까지 차단 |
| `native_8148_barrier_own_midturn_identity_is_not_foreign_pg` | receipt/full-ticket 재평가→fold/projection→admit | 자기 identity 합류를 통과, 결과1 | 모든 prompt에 barrier |
| `native_8148_barrier_late_own_closer_stays_uncertain_pg` | foreign prompt→later own closer→typed 공급 | 어느 closer도 기존 own 결과를 만들지 않음, 원본 보존 | late own ID로 barrier 해제 |
| `native_8148_barrier_agent_manager_human_three_runs_pg` | 실제 O capture 및 격리 Herdr | 12:12 입력 뒤 human typed3턴의 positive/negative 분리 | first foreign close를 own으로 채택 |
| `native_8148_barrier_projection_matches_batch_at_every_prefix` | B1 Fixture::read/native::fold ↔ Projection::evidence | 각 prefix, receipt 변화, same-body 다른 identity, late closer의 동일 결과/불명확 판정. raw 재독0 | 한쪽 barrier만 제거 |
| `native_8148_barrier_after_submitted_retains_exact_original_pg` | 실제 pump Submitted→owner.settle→late O barrier | 기존 exact 원본/멤버가 보존되고 재주입0. stale exact attempt를 성공으로 무시하지 않음 | 보존 실패 무시 |

각 시험은 구현 레그에서 RED→GREEN과 production 원인 뮤턴트를 실행한다. 이름만 있는 계획을 실행 시험으로 세지 않는다. uncertain handoff가 승인되기 전 기대 성공 전이를 구현하거나 시험하지 않는다.

## 크기 재추정과 중단

fold/projection barrier와 typed identity 재검사만은 prod +70~130 정도로 추정한다(B1 oracle/test 제외). 앞 조각4 총 추정 +570~950에 이를 더하면 +640~1080이다. 그러나 **불명확 보존 handoff의 추가 계약/코드가 이 숫자에 포함돼 있지 않으므로 전체 상한 충족 추정이 아니다.**

현재 eabdd166d..bad229c4e의 실측은 prod3/+83이다. 이후 actual+remaining +1100 정지선을 유지한다. 보존 포트 승인 뒤 추가 파일·순증을 다시 추정해야 하며, 지금 +1100 미만이라고 확정하지 않는다. 수치보다 mac-1641의 '기존 경로가 모자라면 DESIGN_DELTA' 중단 조건을 적용했다.

## 리뷰 집중 항목

- 경계 시각: barrier 전 own closer만 정산, barrier 뒤 late own/foreign closer 모두 거절.
- 동시성: full identity와 receipt stamp 재평가, Submitted→barrier 및 barrier→effect-ack 두 순서의 custody 보존.
- 조용한 유실: outcome=None/해시/terminal ID만으로 full original 보존과 불명확 정상 종결을 선언하지 않음.
- 공통 O 의미: shadow tracker/notice 수정0, source reopen/open/byte 추가0을 구현 레그에서 실측.
- 동등성: B1 oracle과 3a projection은 모든 prefix/receipt snapshot에서 같은 barrier 판정이어야 함.
- 잘못된 PASS: 제품 변경0, 모델 변경0, TLC/새 시험 NOT_RUN. 이전126cfg와 기존9개 진단을 이번 설계 검증으로 재표기하지 않음.
