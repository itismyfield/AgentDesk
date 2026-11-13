# Native input attribution — 상태 차트 검증

Code SHA: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`.
설계 모델이며 생산 코드를 고치거나 Rust·herdr 실기 회귀를 실행하지 않았다.
[설계](native-attribution.design.md), [진입점 전수표](native-attribution.entrypoints.md),
[생성한 수정 전 반례 그림](native-attribution.chart.mmd), [생성 보고서](native-attribution.current.report.md),
[수동 작성한 수정 후 계약 요약](native-attribution.design.chart.mmd).

## 상태 차트

기존 `input-single-path` 영역에 별도 작은 `NativeInputAttribution.tla`를 추가했다.
`InputOwnership.tla`의 native busy/idle은 원본의 내부 enqueue/dequeue·membership·strict closer를 구분하지 않는다.
기존 모델을 더 큰 다목적 FSM으로 확장하지 않고 같은 색인·영역에서 원본 두 개, 외부 native run 하나,
최대 프로세스 재시작 1회와 execution incarnation 교체 1회를 정밀화한다. 기존 모델/규칙은 변경하지 않는다.

값 범위와 호출/선언 전수의 정본은 [진입점 표](native-attribution.entrypoints.md)다.

| 모델 값 | 해당 전수표 행/코드 | 추상화 |
|---|---|---|
| Ids=1,2; lease=0/1/2; rows/pending은 Ids 부분집합 | lease/anchor 표, `turn_orchestrator.rs:1936`, `:1989`, `:2015`, `:2042` | 원본 1과 뒤 head 2, 0은 lease 없음. 실제 message-id 0을 주입하지 않음 |
| stage Pending/Injected/Waiting/Dequeued/Running/AssistantEnd/Withdrawn | O queued stage와 `queued::classify`; 입력 등록·strict closer 표 | 저장된 원본, 받은 입력, 실행 소속, 본문 종료, 철회를 구별 |
| native Idle/External/Own, members/ran/closed | `InputFacts::mark`, `TurnTracker::observe` | External도 member로 가입한 ADK 입력을 실행할 수 있음 |
| nativeQueue = empty/External/1, pc=0..10 | enqueue/dequeue/remove/command 진입점 | 외부 enqueue를 먼저 소비한 뒤 ADK FIFO를 읽는 fixture. 모든 FIFO permutation의 parser 정확성은 구현 시험 |
| result/proof None/Complete/Withdrawn/Cancelled/OperatorReleased; count=0/1/2 | finalizer TerminalEvent, 기존 V2 original + proposed result 계약 | Complete 외 결과를 성공으로 바꾸지 않음, 2는 중복 mutant 도달값 |
| generation=1,2; incarnation=1,2; inputInc/proofInc=0,1,2 | restore/boot, SourceId와 V2 Source::validate | 프로세스 boot와 provider execution은 별도. old evidence의 새 input 차용 금지 |
| known/booted/restarted/replaced/wake/outsideQueued, foreignBody/staleReleased/warned | restore·queue kick·source 검증·진단된 body 축적 | boolean은 해당 분기 관측; warned는 ticket별 1회 표식(분 단위 clock은 범위 밖) |
| Mode/Scenario/Legacy 상수 | Current/설계/관문 제거 실험과 Queue/MidTurn/Withdraw fixture | 운영 configuration enum이 아니라 검증 설정 |

## 지킬 규칙

| 종류 | 이름 | 사용자 규칙 |
|---|---|---|
| INVARIANT | CompletionExactlyOnce | 결과가 확정된 AgentDesk 입력은 결과 기록 횟수가 정확히 1회 |
| INVARIANT | NoForeignEnd | 실행 member였고 그 run이 닫힌 입력만 Complete |
| INVARIANT | NoSilentLoss | 각 원본은 durable pending 또는 명시 결과로 남음 |
| INVARIANT | NoForeignResponse | 앞선 외부 턴 본문을 자기 입력 응답에 축적하지 않음 |
| INVARIANT | NoSuccessorRelease, NoSourceBorrow | 옛 episode/source 증거로 후속 lease를 정리하지 않음 |
| PROPERTY | IdleEventuallyReleased | 관측 가능한 native idle에서 결국 lease·inflight가 빔 |
| PROPERTY | HeadEventuallyInjected | 뒤의 원래 head가 결국 주입됨 |
| PROPERTY | EveryInputEventuallySettled | 각 원본이 결국 명시 결과를 얻음 |

모든 PROPERTY는 실제 SPECIFICATION fairness를 포함하고 SYMMETRY를 쓰지 않는다.
WF는 native record 도착, source replay, 원본 결과 저장·lease CAS·row 정리·wake가 계속 실행 기회를 얻는다는 가정이다.
운영자 release·Cancel 같은 선택 행동은 공정성으로 강제하지 않아 수동 복구 없는 정체도 잡는다.
`warned` 변경은 lease·proof·result를 바꾸지 않는다. assistant end_turn과 strict closer도 별도 행동이다.

## RED / GREEN 및 관문 제거

아래 표는 repository TLC checker가 실제로 깨진 규칙 집합과 `expected.tsv`를 비교할 대상이다.
`Fixed*`는 제안 계약의 GREEN이며 생산 구현의 GREEN이 아니다. 전체 실행의 수치/rc는 레인 보고서와 원시 로그에 적는다.

| cfg 접미사 | safety | liveness | 판정 |
|---|---|---|---|
| CurrentQueue | NoForeignResponse | 세 PROPERTY 모두 | 수정 전 RED (실제 19:02 사례) |
| FixedQueue | 없음 | 없음 | 수정 후 GREEN |
| CurrentMidTurn | 없음 | 세 PROPERTY 모두 | 수정 전 RED (실제 20:57 사례) |
| FixedMidTurn | 없음 | 없음 | 수정 후 GREEN |
| CurrentWithdraw | 없음 | 세 PROPERTY 모두 | 수정 전 누락 계약 RED; 실기 재현 미실행 |
| FixedWithdraw | 없음 | 없음 | 수정 후 GREEN |
| FixedBootLegacy | 없음 | 없음 | 구형 귀속을 같은 source에서 재구성하는 전제 아래 GREEN |
| Unavailable | 없음 | 세 PROPERTY 모두 | 영구 증거 불가독의 한계 RED; 이를 GREEN으로 숨기지 않음 |
| ForeignEnd | NoForeignEnd | 없음 | membership 관문 제거 RED |
| NoFence | NoForeignEnd, NoSuccessorRelease, NoSourceBorrow | 없음 | old input/source fence 제거 RED |
| NoWake | 없음 | HeadEventuallyInjected, EveryInputEventuallySettled | wake 제거 RED |
| NoWithdraw | 없음 | 세 PROPERTY 모두 | withdraw 결과 제거 RED |
| DropWithdraw | NoSilentLoss | 세 PROPERTY 모두 | 원본을 조용히 지움 RED |
| DoubleSettle | CompletionExactlyOnce | 없음 | durable 결과 멱등성 제거 RED |

Unavailable은 restart 뒤 같은 source를 영원히 읽을 수 없는 비교 설정이다. 알고 있는 native 상태를
메모리에 남겨서 idle이 보이더라도 귀속 증거가 복원되지 않는 반례를 보인다. 이 입력을 성공으로 처분하는
시간 정책은 없다. 실제 GREEN의 진행성 전제는 증거·store의 eventual readability다.

## 반례와 코드 대조

1. CurrentQueue: 외부 enqueue/시작 → ADK 주입/enqueue → 외부 assistant end에서 NoForeignResponse.
   외부 strict closer → ADK dequeue/user → 자기 assistant end/strict closer를 지나서도 lease·row가 남는다.
   진단 §1의 byte offset/UTC 관측과 일치한다. 코드에서는 `intake_turn.rs:2237`이 O에도 TB를 띄우고,
   `input_facts.rs:355`의 O events가 입력별 finalizer로 연결되지 않으며 `carried_turn_close.rs:163`은 현 세대를 제외한다.
   정확한 TB 내부 보류 predicate는 미확정이다. 모델의 Current는 이 **실패한 incident**의 관측을 추상화하며
   정상 TB 턴도 항상 멈춘다고 주장하지 않는다.
2. CurrentMidTurn: native external run에 입력이 member로 들어가도 Current에서는 O proof가 finalizer로 전달되지 않는다.
   이슈 21:1x 댓글의 재발과 일치한다. `queued.rs:147`의 absorption 처리는 이미 있지만 aggregate input facts일 뿐이다.
3. CurrentWithdraw: `queued.rs:198`은 remove를 분류하고 aggregate owed를 줄인다. 현재 O fact에는
   입력별 Withdrawn settlement를 호출하는 경로가 없다. TB stop identity는 remove를 읽기도 한다
   (`turn_bridge/tmux_runtime/claude_stop_identity.rs:330`) — 따라서 “코드 어디에도 withdraw 처리가 없다”고 단정하지 않는다.
   이 cfg는 필요한 계약의 부재를 추상화한 비교이며 기존 TB에서 실제 무한 대기를 재현한 증거는 아니다.
   이슈 DoD 2의 실제 연결 fixture가 구현 레그에서 판정한다.
4. 관문 제거 여섯 설정은 **생산 코드의 기존 결함 주장과 다르다**. 설계에서 해당 관문이 필요한 이유를 보인다.
   0개 production 변이를 실행했다. 구현에서는 같은 guard를 실제로 제거한 mutant RED가 필요하다.

생성한 CurrentQueue 그림은 도구의 500-state 예산을 넘어 **반례 경로만** 보여 준다.
그림에서 보이지 않는 모든 경로를 검증에서 생략한 것은 아니다. TLC는 도달 상태 전체를 검사한다.
수정 후 수동 요약 그림은 Restart/ExecutionReplaced의 세부 분기와 head의 두 번째 closer를 접어서 그렸다.
그 분기들은 모델 Next와 checker에 남아 있다. generated report의 경로 번호와 실제 시점 대조는 위 1~3이다.

## 구현 시험으로 넘긴 경계와 모델 한계

- 정해진 외부 enqueue/FIFO fixture의 올바른 분류·유일한 attempt 바인딩은 O/history 포트 가정이다.
  같은 본문 twin, 외부 queued notification·FIFO popAll/removal 선후 모든 조합은 Rust fixture가 증명한다.
- 이 모델은 영속 결과를 한 atomic transition으로 둔다. 실제 V2 저장·fsync·lease와 파일 사이 crash는 atomic하지 않다.
  결과-before-release, result 보존, CAS 실패 debt 재발견을 실제 store/latch 시험으로 검증해야 한다.
- OperatorRelease/AdjacentEnd는 이미 권한·exact execution 조건을 만족한 기존 조작의 **결과**다.
  Cancel 요청만으로 native를 idle로 만드는 가정을 제품에 옮기지 않는다. Stop/Terminate의 권한·프로세스 증거는 #8128 접점 시험이다.
- incarnation 1/2와 ticket 1/2는 전체 identity tuple의 추상화다. 같은 numeric pane·같은 message-id에 다른 nonce,
  old schema None, generation wrap/GC 후 late event를 실제 identity API로 따로 시험한다.
- WARN 분 단위 경계/로그 sink·배송 POST/receipt·Discord body/card/footer는 모델 밖이며 main 설계의 DoD 표가 맡는다.
  O-delivery가 입력 release를 결정한다는 가정은 넣지 않았다. proof stamp는 native 출처이지 배송 receipt가 아니다.

재현(원본 worktree의 커밋 뒤 변경 모델 14 cfg를 모두 선택):

```sh
unset TMUX TMUX_PANE
JAVA=/opt/homebrew/opt/openjdk/bin/java \
TLA2TOOLS=/Users/itismyfield/.local/share/tla2tools/tla2tools-1.7.4.jar \
python3 scripts/ci/statecharts_tlc.py --base 33d1f4b5a9dce7023e2d0014e45a05ed6c530bba --head HEAD --jobs 2
```

반례 그림은 `.codex/skills/statechart/scripts/statechart NativeInputAttribution.tla`에
`--config NativeAttributionCurrentQueue.cfg --view native-attribution.view.mjs --out <새 디렉터리>`로 생성한다.
wrapper rc=1은 예상 RED다. repository checker는 예상 RED까지 정확히 맞아야 전체 rc=0이다.
