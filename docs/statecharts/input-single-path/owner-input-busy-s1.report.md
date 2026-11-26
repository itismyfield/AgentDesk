# owner 입력 S1(Claude): 바로 합류 + 답 귀속 + transcript 없는 키 (#8328 1단계 설계 w4)

- 기준 코드: `82702fe252`
- 범위: 1단계를 둘로 나눈 것 가운데 **S1**(Claude 바로 합류와 답 귀속)이다. 다루는 것은 사람 입력 B 하나와 앞 턴 A 하나다.
- Codex 항상 Tab과 채널 lease·slot 공유는 S2다. S2 모델은 `OwnerInputBusySplit.tla`([보고서](owner-input-busy-split.report.md))이고, w3 상태 그대로 둔다.
- S1은 실행 용량을 새로 잡지 않으므로 이 모델에는 lease 변수가 없다(설계 문서 §1.1).
- 설계 문서: [`docs/design/8328-owner-input-busy-inject.md`](../../design/8328-owner-input-busy-inject.md)
- 모델 결과는 Rust 시험을 대신하지 않는다.

## 왜 S1을 따로 두는가

설계 리뷰 i8328-design-3의 P1-3 때문이다. 수용 기준은 "transcript 없는 정상 입력 보류 0"이다. 그런데 키 전에 source를 묻는 곳이 어댑터 말고도 둘 더 있다.
- Herdr `BusyObservation::admits`(`src/services/claude_tui/host_input/busy.rs:245-261`): 관찰한 source 경로가 요청의 transcript와 같아야 한다.
- O writer owner 경로 `Producer::wrap`(`src/services/discord/router/message_handler/provider_dispatch/owner_input/native_input.rs:71-109`): 어댑터 앞에서 `pin_source`(`src/services/discord/turn_finalizer/native_binding.rs:18-106`)를 부르고, 현재 실행의 `Bound` 기록이 없으면 실패한다.

w3 모델은 이 둘을 다루지 않았다. S1 모델은 source 결합 상태(`src`)와 보류 사유(`why`)를 변수로 두고, 세 검사를 각각 mutant로 둔다.

## 변수와 시나리오

변수:
- `src`: transcript source가 결합됐는가
- `reg`: anchor·nonce·frame hash·target incarnation이 키 전에 durable 등록됐는가
- `why`: 보류 사유
- 나머지 변수는 S2 모델과 같다

시나리오:
- `busy`: A 실행 중, 토큰 A, source 결합됨
- `i8432`: A terminal 미제출로 토큰이 남음
- `external`: AgentDesk가 시작하지 않은 턴. anchor·토큰 없음
- `fresh`: 새 세션. 턴이 transcript를 쓰기 전까지 source가 없다
- `rebind`: A 실행 중이고 composer는 준비됐지만, writer가 재시작한 source를 아직 결합하지 않았다. `BindSource`로 나중에 결합된다

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoDuplicate` | INVARIANT | 키 효과(붙여넣기+Enter)는 최대 1회 |
| `NoSilentLoss` | INVARIANT | B는 알림 없이 사라지지 않는다 |
| `ReplyExactlyOnce` | INVARIANT | 답은 각각 최대 1회. 답이 게시된 B의 몫은 자기 답 XOR A 답에 합쳐짐 |
| `JoinedMarked` | INVARIANT | A 답에 합쳐진 B에는 합류 표시가 있다 |
| `MarkOnlyJoined` | INVARIANT | 합류 표시는 합류했거나 합쳐진 B에만 있다(P2-3) |
| `AnswerBound` | INVARIANT | A 답은 `ATarget`에, B 자기 답은 B anchor에 reply로 건다 |
| `KeyBeforeLeave` | INVARIANT | front는 키 효과로만 대기열을 떠난다 |
| `KeyAfterRegister` | INVARIANT | 키 효과 전에 durable 등록이 끝나 있다 |
| `NoSourceHold` | INVARIANT | source가 아직 결합되지 않았다는 이유로 정상 입력을 보류하지 않는다(P1-3) |
| `IdleDelivers` | PROPERTY | 대기·보류 중 B + Idle 화면이면 B는 결국 주입된다 |
| `ReplyPosted` | PROPERTY | B의 몫은 결국 게시된다 |

공정성: 정상 행동에는 weak fairness를 준다. `TuiAbsorb`와 mutant 행동에는 공정성이 없다(`RegisterLate`는 mutant 안의 후속 행동이라 공정성을 준다).

## 결과 (`statecharts_tlc.check_cfg`, 24/24 기대 집합 일치)

| cfg | 조건 | 결과 |
|---|---|---|
| `Current8432` | 현재, 남은 토큰 + Idle | **RED** `IdleDelivers`·`ReplyPosted`·`MarkOnlyJoined` |
| `CurrentBusy` | 현재, 정상 | **RED** `MarkOnlyJoined`(붙여넣기 때 바로 ➕) |
| `CurrentRebind` | 현재, source 결합 전 | **RED** `NoSourceHold`·`MarkOnlyJoined` |
| `TargetBusy`·`Target8432`·`TargetExternal` | S1 목표 | GREEN |
| `TargetFresh` | 새 세션, transcript 없음 | GREEN |
| `TargetRebind` | source 결합 전 composer 준비 | GREEN |
| `TargetHoldCleared`·`TargetComposerNotReady` | 보류 뒤 해소 | GREEN |
| `TargetDraftStuck` | 초안 영구, 알림만(U1) | RED `IdleDelivers`·`ReplyPosted`(수용) |
| `TargetAdmitsPathMatch` | mutant: Herdr `admits` 경로 일치 유지 | RED `NoSourceHold` |
| `TargetPinSourceFirst` | mutant: `Producer::wrap`의 `pin_source` 선호출, 새 세션 | RED `NoSourceHold`·`IdleDelivers`·`ReplyPosted` |
| `TargetPinSourceFirstRebind` | 같은 mutant, 재바인딩 | RED `NoSourceHold` |
| `TargetRequireTranscript` | mutant: 어댑터 transcript metadata 선확인 | RED `NoSourceHold` |
| `TargetRegisterAfterKey` | mutant: 키 뒤 등록 | RED `KeyAfterRegister`·`AnswerBound` |
| `TargetMarkOnPaste` | mutant: 붙여넣기 때 ➕ | RED `MarkOnlyJoined` |
| `TargetJoinUnmarked` | mutant: 흡수 때 ➕ 누락 | RED `JoinedMarked` |
| `TargetJoinDoublePost` | mutant: 합류 몫 이중 게시 | RED `ReplyExactlyOnce` |
| `TargetPostOrder` | mutant: 게시 순서 귀속 | RED `AnswerBound` |
| `TargetNoReference` | mutant: reference 없는 게시 | RED `AnswerBound` |
| `TargetTokenAttribution` | mutant: 토큰 보유자 귀속 | RED `ReplyExactlyOnce`·`ReplyPosted` |
| `TargetResendBeforeEcho` | mutant: echo 전 재전송 | RED `NoDuplicate` |
| `TargetTuiDrop` | mutant: TUI가 입력을 버림 | RED `NoSilentLoss`·`ReplyPosted` |

합계: GREEN 7, expected RED 17(Current 3, 수용 1, mutant 13).

차트:
- [현재 source 결합 전 반례](owner-input-busy-s1.current-rebind.chart.mmd)
- [현재 #8432 반례](owner-input-busy-s1.current-8432.chart.mmd)
- [목표 재바인딩](owner-input-busy-s1.target-rebind.chart.mmd): source 없이 붙여넣고 나중에 결합
- [목표 새 세션](owner-input-busy-s1.target-fresh.chart.mmd)
- [목표 합류](owner-input-busy-s1.target-busy-join.chart.mmd): ➕는 흡수 뒤 상태에만 있다
- [목표 #8432](owner-input-busy-s1.target-8432.chart.mmd)
- [admits 경로 일치 반례](owner-input-busy-s1.admits-path-match.chart.mmd)
- [pin_source 선호출 반례](owner-input-busy-s1.pin-source-first.chart.mmd)
- [붙여넣기 때 ➕ 반례](owner-input-busy-s1.mark-on-paste.chart.mmd)

## 읽는 법과 한계

- 세 source mutant는 모델에서는 같은 동작이다(`src = none`이면 보류). 차이는 코드 위치뿐이다. 그래서 Rust 시험은 실제 owner 진입점에서 각각 하나씩 둔다(설계 §11 S1-B 시험).
- pane 교체·실행 세대 변경은 키 0이어야 한다(대조군). 이 모델에는 incarnation 변수가 없으므로 Rust 시험이 맡는다.
- 두 입력이 한 턴에 합쳐지는 경우는 다루지 않는다. 설계 §6.3 규칙과 S1-A의 두 nonce fixture가 맡는다.

원시 로그: `/tmp/i8328d1/`(`direct-s1.out`, `s1-*` 차트 출력). 저장소에는 넣지 않았다.
