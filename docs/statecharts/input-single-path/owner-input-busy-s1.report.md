# owner 입력 S1(Claude): 바로 합류 + 답 귀속 + transcript 없는 키 (#8328 1단계 설계 w4-w5)

- 기준 코드: `82702fe252`
- 범위: 1단계 S1(Claude 바로 합류와 답 귀속)이다. 다루는 것은 사람 입력 B 하나와 앞 턴 A 하나다.
- Codex 항상 Tab과 채널 lease·slot 공유는 S2이고, 모델은 `OwnerInputBusySplit.tla`([보고서](owner-input-busy-split.report.md))다.
- S1은 실행 용량을 새로 잡지 않으므로 lease 변수가 없다(설계 §1.1).
- 설계 문서: [`docs/design/8328-owner-input-busy-inject.md`](../../design/8328-owner-input-busy-inject.md)
- 모델 결과는 Rust 시험을 대신하지 않는다.

## 판 이력

- **w4**(`c603bfb08c`, cfg 24): source 결합 상태 `src`, 키 전 등록 `reg`, 보류 사유 `why`. P1-3 mutant(`AdmitsPathMatch`·`PinSourceFirst`·`RequireTranscript`).
- **S1-A fix1**(cfg 35): 구현 리뷰 review-s1a-1 P1-1을 넣었다. 시나리오 `nearmiss`는 외부 A의 프롬프트가 B의 frame과 trim 뒤에만 같고 exact로는 다르다. exact receipt가 없으니 결합 0이어야 하고(`TargetNearMiss` GREEN), class hash로 대체 결합하는 구현(`ClassHashFallback`)은 A 답을 B에 건다(`AnswerBound` RED). 다른 33개 cfg의 기대값은 그대로다.
- **w5**(cfg 33): 설계 리뷰 i8328-design-4의 P1 3건과 P2를 반영했다.
  - transcript 파일 생성(`trans`, `TuiStart`)과 AgentDesk의 source 결합(`src`, writer의 `Bound` 기록 `BindSource`)을 나눴다(P2). w4에서는 `TuiStart`가 곧바로 `src = bound`를 만들어, 결합을 빠뜨린 구현도 GREEN이 될 수 있었다.
  - 지연 pin 단일 확정점: `pin`(recorded·deferred), `sel`(BindingIntent.selected). `Select`가 `None → Some`을 한 번만 만든다(P1-3).
  - 귀속 결합: `aBound`(꼬리표 없는 opener A를 receipt로 결합), `bBound`(B 결합). A 답은 A 결합 뒤, 합류 B가 있으면 B 결합 뒤에 나간다. B 답은 B 결합 뒤에 나간다. 합류 표시는 B 결합(흡수 증거) 때 단다(P1-1).
  - source 관문 mutant 2개를 더했다: `RequireIdleObserverBeforeKey`(토큰 없는 tmux 자기 턴 observer의 transcript 선요구), `RequireBusyReadStateWithoutSource`(Herdr `Context::read`의 source 없음 → Held)(P1-2).
  - `RequireTranscript`는 토큰과 무관하게 적용한다(P2).
  - 새 liveness `NoCircularWait`: transcript가 없고 보류 조건이 없는 대기 입력은 결국 키를 받는다. 첫 입력이 transcript를 만드는 순환 대기를 잡는다.
  - 항상 허용되는 stutter `Done`을 두었다. 막힌 상태는 deadlock이 아니라 liveness 위반으로 드러난다.

## 변수와 시나리오

변수(S2 모델과 같은 것은 생략):
- `trans`: transcript 파일이 있다
- `src`: writer가 이 실행의 `Bound`를 기록했다
- `reg`: 키 전 durable 등록
- `why`: 보류 사유
- `pin`: 키 시점 pin(recorded·deferred)
- `sel`: 확정된 source
- `aBound`·`bBound`: J2 anchor 결합

시나리오:
- `busy`·`i8432`·`external`: w4와 같다
- `nearmiss`: `external`과 같고, A의 프롬프트가 B의 frame과 공백만 다르다(S1-A fix1)
- `fresh`: 새 세션. 파일도 `Bound`도 없다. B의 턴이 파일을 만든다
- `rebind`: 파일은 있지만 writer가 재시작한 실행의 `Bound`를 아직 기록하지 않았다

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoDuplicate`·`NoSilentLoss` | INVARIANT | 키 효과 최대 1회, 조용한 유실 0 |
| `ReplyExactlyOnce` | INVARIANT | 답은 각각 최대 1회. 답이 게시된 B의 몫은 자기 답 XOR A 답에 합쳐짐 |
| `JoinedMarked`·`MarkOnlyJoined` | INVARIANT | 합쳐진 B에는 ➕, ➕는 합류했거나 합쳐진 B에만 |
| `AnswerBound` | INVARIANT | A 답은 `ATarget`(A anchor), B 답은 B anchor에 reply |
| `KeyBeforeLeave`·`KeyAfterRegister` | INVARIANT | 키로만 대기열을 떠나고, 키 전에 등록한다 |
| `NoSourceHold` | INVARIANT | source 미결합을 이유로 정상 입력을 보류하지 않는다 |
| `IdleDelivers`·`ReplyPosted` | PROPERTY | Idle이면 결국 주입, B의 몫은 결국 게시 |
| `NoCircularWait` | PROPERTY | transcript 없는 첫 입력도 결국 키를 받는다 |

공정성: 정상 행동과 결합 행동(`BindSource`·`Select`·`BindA`·`BindB`)에 weak fairness를 준다. `TuiAbsorb`와 mutant 행동에는 주지 않는다.

## 결과 (`statecharts_tlc.check_cfg`, 35/35 기대 집합 일치)

S1-A fix1 전후표(새 cfg 2개, 나머지 33개는 아래 w5 열 그대로):

| cfg | 수정 전 구현(class hash 대체 결합) | 수정 후 |
|---|---|---|
| `TargetNearMiss` | (없음) | GREEN |
| `TargetClassHashFallback` | (없음) | RED AnswerBound |

w4 → w5 전후표(모든 cfg에 `NoCircularWait`를 더했다):

| cfg | w4 | w5 |
|---|---|---|
| `Current8432` | RED IdleDelivers·MarkOnlyJoined·ReplyPosted | RED IdleDelivers·MarkOnlyJoined·ReplyPosted |
| `CurrentBusy` | RED MarkOnlyJoined | RED MarkOnlyJoined |
| `CurrentFresh` | (없음) | RED IdleDelivers·NoCircularWait·NoSourceHold·ReplyPosted |
| `CurrentRebind` | RED MarkOnlyJoined·NoSourceHold | RED MarkOnlyJoined·NoSourceHold |
| `Target8432` | GREEN | GREEN |
| `TargetAdmitsPathMatch` | RED NoSourceHold | RED NoSourceHold |
| `TargetBusy` | GREEN | GREEN |
| `TargetComposerNotReady` | GREEN | GREEN |
| `TargetDeferredEquality` | (없음) | RED ReplyPosted |
| `TargetDeferredEqualityRebind` | (없음) | RED ReplyPosted |
| `TargetDraftStuck` | RED IdleDelivers·ReplyPosted | RED IdleDelivers·ReplyPosted |
| `TargetExternal` | GREEN | GREEN |
| `TargetFresh` | GREEN | GREEN |
| `TargetHoldCleared` | GREEN | GREEN |
| `TargetJoinDoublePost` | RED ReplyExactlyOnce | RED ReplyExactlyOnce |
| `TargetJoinUnmarked` | RED JoinedMarked | RED JoinedMarked |
| `TargetMarkOnPaste` | RED MarkOnlyJoined | RED MarkOnlyJoined |
| `TargetNoLateBind` | (없음) | RED ReplyPosted |
| `TargetNoLateBindFresh` | (없음) | RED ReplyPosted |
| `TargetNoReference` | RED AnswerBound | RED AnswerBound |
| `TargetPinSourceFirst` | RED IdleDelivers·NoSourceHold·ReplyPosted | RED IdleDelivers·NoCircularWait·NoSourceHold·ReplyPosted |
| `TargetPinSourceFirstRebind` | RED NoSourceHold | RED NoSourceHold |
| `TargetPostOrder` | RED AnswerBound | RED AnswerBound |
| `TargetRebind` | GREEN | GREEN |
| `TargetRegisterAfterKey` | RED AnswerBound·KeyAfterRegister | RED KeyAfterRegister |
| `TargetRequireBusyReadStateWithoutSource` | (없음) | RED NoSourceHold |
| `TargetRequireIdleObserverBeforeKey` | (없음) | RED IdleDelivers·NoCircularWait·NoSourceHold·ReplyPosted |
| `TargetRequireTranscript` | RED NoSourceHold | RED NoSourceHold |
| `TargetRequireTranscriptFresh` | (없음) | RED IdleDelivers·NoCircularWait·NoSourceHold·ReplyPosted |
| `TargetResendBeforeEcho` | RED NoDuplicate | RED NoDuplicate |
| `TargetTaggedHeaderOnly` | (없음) | RED AnswerBound |
| `TargetTokenAttribution` | RED ReplyExactlyOnce·ReplyPosted | RED ReplyExactlyOnce·ReplyPosted |
| `TargetTuiDrop` | RED NoSilentLoss·ReplyPosted | RED NoSilentLoss·ReplyPosted |

합계(w5): Current RED 4, Target GREEN 7, 수용 RED 1(DraftStuck), mutant RED 21. fix1 뒤: Target GREEN 8, mutant RED 22.

mutant 뜻(w5에서 새로 넣은 것, fix1의 `ClassHashFallback` 포함):
- `ClassHashFallback`: exact receipt가 없는 행을 trim한 class hash로 등록에 잇는다. 공백만 다른 외부 A의 답이 B에 걸린다(`AnswerBound`).
- `TaggedHeaderOnly`: J2가 헤더 nonce만 읽는다. 꼬리표 없는 opener A는 결합되지 않고, A 답이 합류 B에 걸린다(`AnswerBound`).
- `RequireIdleObserverBeforeKey`: 토큰 없는 tmux 자기 턴에서 observer가 transcript를 먼저 요구한다 → `idle_unobserved`(새 세션이면 순환 대기).
- `RequireBusyReadStateWithoutSource`: Herdr `admits`가 source를 읽는 `Context::read` 상태를 요구한다.
- `DeferredEquality`: 지연 pin에도 `select_source`의 session·path 동등성을 적용한다 → 확정 0, 결합 0, 답 0(`ReplyPosted`).
- `NoLateBind`: writer의 `Bound` 기록이 끝내 오지 않는다(결합을 빠뜨린 구현). `ReplyPosted` RED. w4 모델에서는 GREEN이었다.
- `RequireTranscriptFresh`: 무토큰 새 세션에도 transcript를 요구한다(w4에서는 GREEN으로 빠져나갔다).

차트:
- [현재 새 세션 순환 대기](owner-input-busy-s1.current-fresh.chart.mmd)
- [현재 source 결합 전](owner-input-busy-s1.current-rebind.chart.mmd)
- [현재 #8432](owner-input-busy-s1.current-8432.chart.mmd)
- [목표 재바인딩](owner-input-busy-s1.target-rebind.chart.mmd)
- [목표 새 세션](owner-input-busy-s1.target-fresh.chart.mmd)
- [목표 합류](owner-input-busy-s1.target-busy-join.chart.mmd)
- [목표 #8432](owner-input-busy-s1.target-8432.chart.mmd)
- [admits 경로 일치](owner-input-busy-s1.admits-path-match.chart.mmd)
- [pin_source 선호출](owner-input-busy-s1.pin-source-first.chart.mmd)
- [idle observer 관문](owner-input-busy-s1.idle-observer-gate.chart.mmd)
- [지연 pin 동등성](owner-input-busy-s1.deferred-equality.chart.mmd)
- [헤더 nonce만](owner-input-busy-s1.tagged-header-only.chart.mmd)
- [붙여넣기 때 ➕](owner-input-busy-s1.mark-on-paste.chart.mmd)

## 읽는 법과 한계

- source mutant 다섯은 모델에서는 같은 동작이다(`src = none`이면 보류). 차이는 코드 위치뿐이라, Rust 시험은 실제 owner 진입점에서 하나씩 둔다(설계 §11 S1-B).
- custody ACK는 `Select`·`BindB` 사이의 원자적 단계로 다룬다. 확정 직후·history binding 직후·ACK 전 재시작 수렴은 S1-B Rust 시험이 맡는다.
- pane·실행 세대 교체는 키 0 대조군이다. incarnation 변수가 없으므로 Rust 시험이 맡는다.
- 두 입력이 한 턴에 합쳐지는 경우는 설계 §6.3과 S1-A 두 nonce fixture가 맡는다.

원시 로그: `/tmp/i8328d1/`(`direct-s1.out` w4, `direct-s1w5.out` w5, `s1w5-*` 차트 출력). 저장소에는 넣지 않았다.
