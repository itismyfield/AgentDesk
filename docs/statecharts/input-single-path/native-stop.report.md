# InputNativeStop — 외부 Claude 턴에 넣은 자기 입력의 중단

이전 P1 판독 결함의 코드 좌표 기준 SHA: `f332bd53918fcd3ac9867034f9323b42bf0d8cda`, 병합한 main `dc5c742e40079a724a557ed29378cba6c7dbd75f` (#7788 S2/#7824).
최초 fixed는 이 기준 코드에서 지적된 remove-only 흡수와 자동 compact summary 판독을 수리하는 계약이다. 구현 시험의 성공을 뜻하지 않는다.
후속 전체 검증은 `caf61e3801f5cdc8091c0268be94b66333eb4830`(병합한 main `d79d8f6a60e703fc685424103b5d9715d7840455`, #7827) 트리에서 수행했다. 위 f332 SHA는 원래 반례의 코드 좌표 기준이며, 그 main 병합 시점에는 이 모델·cfg·view·차트의 내용은 바꾸지 않았다.
기존 Before 두 설정은 처음 설계한 receipt/N1/N2 연결을 각각 끄는 역사적 결함 설정이며, absorption/summary Before는 기준 SHA에서 확인한 P1-1/P1-2 판독 결함을 분리한다. 이번 birth-child 판독의 기준 SHA는 `bda08f189c775de190d5be53a053f9ba31d94fd0`이다. EpisodeBefore는 이 SHA의 복구 child 거부를 durable ledger가 우회하는 CI 반례를 분리한다. RestoredBefore는 결정2의 restored token에 persisted child 근거가 없는 H1/H2 거절을 분리한다. 실행 당시 git HEAD와 working Rust SHA는 레인 `s1-9-model-work.json`에 기록했다. 모델은 수리 계약을 검사하며 해당 Rust 스냅샷의 구현 시험 성공을 뜻하지 않는다. `InputOwnership.tla`는 이 변경에서 수정하지 않는다.

| 설정 | `Connected` | `RunningReceipt` | `AbsorptionEvidence` | `CompactSummaryNeutral` | `EpisodeChecked` | `RestoredChecked` | exit | 상태 / 전이 | 깨진 규칙 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| `InputNativeStop.cfg` | TRUE | TRUE | TRUE | TRUE | TRUE | TRUE | 0 | 443 / 1246 | 없음 |
| `InputNativeStopBefore.cfg` | FALSE | TRUE | TRUE | TRUE | TRUE | TRUE | 1 | 443 / 1246 | `OwnInputStops` |
| `InputNativeStopRunningBefore.cfg` | TRUE | FALSE | TRUE | TRUE | TRUE | TRUE | 1 | 443 / 1246 | `OwnInputStops` |
| `InputNativeStopAbsorptionBefore.cfg` | TRUE | TRUE | FALSE | TRUE | TRUE | TRUE | 1 | 443 / 1246 | `NoForeignEscape`, `OwnInputStops` |
| `InputNativeStopSummaryBefore.cfg` | TRUE | TRUE | TRUE | FALSE | TRUE | TRUE | 1 | 443 / 1246 | `OwnInputStops` |
| `InputNativeStopEpisodeBefore.cfg` | TRUE | TRUE | TRUE | TRUE | FALSE | TRUE | 1 | 443 / 1246 | `NoForeignEscape`, `NoOtherEpisodeEscape`, `NoOtherParentEscape` |
| `InputNativeStopRestoredBefore.cfg` | TRUE | TRUE | TRUE | TRUE | TRUE | FALSE | 1 | 443 / 1246 | `NoUnprovenRestoredChildEscape` |

## 차트와 그림의 범위

- [고친 설정](native-stop.chart.mmd): 19상태/62화살표.
- [N1/N2 연결 누락](native-stop-before.chart.mmd): 20상태/64화살표.
- [running receipt 연결 누락](native-stop-running-before.chart.mmd): 20상태/63화살표.
- [Command 없는 absorption을 놓침](native-stop-absorption-before.chart.mmd): 21상태/65화살표.
- [compact summary를 foreign user로 오판](native-stop-summary-before.chart.mmd): 20상태/64화살표.
- [다른 birth child를 ledger가 다시 승인](native-stop-episode-before.chart.mmd): 19상태/62화살표.
- [restored token이 child 근거 없는 ledger를 승인](native-stop-restored-before.chart.mmd): 19상태/62화살표.

TLC는 각 설정에서 같은 443상태/1246전이를 검사한다. view는 원장을 성공적으로 다시 읽은 `Restart` 66전이를 숨긴다. 같은 표시 상태에 머무는 629전이는 tool report에 원래 수가 남는다. 생략한 전이는 TLC 검사에는 포함된다.
summary를 읽은 token-root, 다른 episode/parent child, 근거 없는 restored child와 전달/차단 결과를 별도 표시 상태로 나누어 반례 단계를 숨기지 않는다. 서로 다른 규칙을 깨는 상태를 합치지 않으며 `mayBreak`로 약화하지 않는다. 각 그림의 모든 표시 상태는 시작 상태에서 도달하고, 새 child 판단 상태에는 Stop 화살표가 남는다.

## 규칙과 모델의 실제 사실

- `NoForeignEscape`: token 없는 O-off에서 자기 입력 흡수 증거가 없거나 외부 입력이 섞이면 Escape 0이다. token NativeStop의 foreign 입력도 막는다. 이미 relay가 claim한 순수 외부 opener의 direct pin Escape 1은 기존 권한으로 유지한다.
- `OwnInputStops`: 자기 입력이 실제로 흡수됐거나 running 분기에서 자기 root가 생겼고 foreign 입력이 없으면 Escape 1이다.
- `NoOtherEpisodeEscape` / `NoOtherParentEscape`: 현재 birth 범위가 확인된 token에서 다른 `turn_nonce` / `user_msg_id`의 child가 있으면 Escape 0이다. native pin 자체를 birth 증거로 삼지 않는다.
- `NoUnprovenRestoredChildEscape`: restored token에서 persisted child 근거 없이 ledger A만 남은 입력은 Escape 0이다. 실제로 H1이어도 H2와 durable 상태가 같으면 거절하는 결정2를 명시한다.
- `EnqueueNotAuthority`: token 없는 상태에서 enqueue만으로 Escape 권한을 만들지 않는다.
- `NoDeadlock`: 결과 뒤에는 `Done` 자가 전이가 있다.

`OwnEvidence`는 현재 native root 또는 이미 대응된 own attempt의 증거이며 `foreign`는 현재 범위와 다른 입력 사실이다. 원장 A의 존재만 남은 restored snapshot은 `ledgerChild=TRUE`에 따로 보존하고 current root/paired attempt로 승격하지 않는다. 판독기가 놓친 실제 사실을 지우지 않는다. `ObservedOwn`/`ObservedForeign`, summary 및 restored 허용 판정에서 Before 상수를 사용한다. 따라서 고치기 위해 불변식을 약화하거나 remove-only/summary 상태를 검증 대상에서 빼지 않는다.

`OwnerPaste`는 자기 attempt를 enqueue한다. `AbsorbOnly`는 Command 행 없이 `remove(absorbed_mid_turn)`로 실제 흡수되어 `removed` 상태가 된다. 뒤에 오는 `Absorb`(Command)는 같은 입력을 `absorbed`로 바꿀 뿐 두 번째 입력을 만들지 않는다. Command가 먼저 온 뒤의 `AbsorbAfterCommand`도 같은 입력의 idempotent 사건이다. enqueue만 있으면 실제 OwnEvidence가 없다.
`ForeignAbsorbOnly`는 실제 foreign를 세우고, 구판독(`AbsorptionEvidence=FALSE`)만 그 형식을 놓친다. `CompactSummary`는 같은 SID의 확인된 boolean summary로, 실제 foreign를 세우지 않는다. 일반 user/false/문자열 true/SID 불일치는 이 행동이 아닌 `ForeignInput`에 속한다.

`foreign`는 현재 opener 뒤 전체를 뜻한다. 같은 사용자의 exact frame도 현재 token의 birth episode/parent에 속하지 않으면 해당 native stop에는 foreign다. 새 child 행동은 그 실제 사실을 TRUE로 두므로 기존 `NoForeignEscape`·`OwnInputStops`·`EnqueueNotAuthority` 식을 약화하거나 사실을 지우지 않는다. owner paste/흡수가 이를 초기화하지 않아 opener와 자기 입력 사이·뒤·같은 행의 혼합을 모두 보수적으로 막는다. 새 자기 user root를 여는 `OwnerPasteRunning`만 이전 opener의 foreign를 초기화한다.
`Restart`는 durable attempt/transcript 사실을 남기고 프로세스의 relay token/child를 제거한다. 원장 읽기 성공과 Unread를 각각 검사한다. 일반 running token의 pin 복원 절차 전체는 기존 NativeStop 범위이므로 `Restart`는 running 분기를 제외한다. 새 `OtherEpisodeChild`/`OtherParentChild`는 복구가 child를 거부했지만 같은 source/session/frame ledger에는 여전히 그 attempt가 있는 관측 지점을 따로 접는다. 여기서는 `restarted=TRUE`, `child=FALSE`, `ledgerChild=TRUE`다.

## 새 반례를 코드와 대조

### 자기 remove-only: OwnInputStops 위반, 4단계

0 시작 → 1 ExternalOpen → 2 OwnerPaste → 3 AbsorbOnly → 4 Stop(거부).

| 단계 | 코드 대조 (기준 SHA) |
|---|---|
| 1 | `owned_input_stop.rs:103–144`가 InputFacts의 열린 opener를 찾는다. |
| 2 | `busy_inject.rs:559–589`가 durable frame 등록과 실제 paste/Enter를 수행한다. 등록만으로 흡수를 가정하지 않는다. |
| 3 | 기존 `input_facts/queued.rs:31–32,207–210`은 Command가 없거나 앞/뒤에 오는 `absorbed_mid_turn` 형식을 지원한다. `:147–168`은 같은 입력의 absorption을 대응시킨다. |
| 4 | 기준 `claude_stop_identity.rs:214–230`은 attachment/queued_command가 아닌 행을 Neutral로 둔다. `owned_input_rows():168–175`는 owns=false가 되고, `history.rs:551–590`의 기존 taken_rows도 user/Command+UUID만 읽으므로 remove를 row_owner에 그대로 넘기는 것으로 해결되지 않는다. |

### 외부 remove-only: NoForeignEscape 위반, 4단계

0 시작 → 1 ExternalOpen → 2 OwnerPasteRunning → 3 ForeignAbsorbOnly → 4 Stop(잘못 전달).

1–2는 검증된 자기 root pin/token을 추상화한다. 3은 같은 queue 형식의 외부 B 흡수다. 기준 `pinned_still_current():117–125`도 같은 later_row를 호출해 이를 Neutral로 놓치고 pin을 유지한다. tokenless N2도 정확한 A Command 뒤 B remove-only가 오면 `owned=true, clean=true`가 남는다(`:168–175`). 전달 직전 재검사는 같은 오판을 반복하므로 분류 자체를 수리해야 한다.

### 자동 summary: OwnInputStops 위반, 4단계

0 시작 → 1 ExternalOpen → 2 OwnerPasteRunning → 3 CompactSummary → 4 Stop(거부).

같은 SID의 true boolean summary 두 표기는 기존 `shadow/identity.rs:194–198` helper가 확인한다. 기준 later_row의 `:193–198` SID 검사를 통과한 뒤 `:211–212`가 meta가 아닌 user라는 이유로 Other를 반환한다. token은 `:117–125`에서 pin을 거절하고 N2는 `:168–172`에서 clean=false가 된다. 모델은 이 확인된 summary만 Neutral로 고치며 다른 human 입력 예외를 추가하지 않는다.

## 기존 연결 반례

`InputNativeStopBefore.cfg`는 ExternalOpen→OwnerPaste→Absorb→Stop에서 OwnInputStops를 깬다. 이는 처음 base `216858a711a6dfea5b18243e947199b461ea9b37`의 token 없는 O-off 분기와 claim 틈 N1 누락을 분리한 설정이다. 실제 T1/T3 base RED와 연결 좌표는 레인의 이전 보고서에 보존한다. 기준 f332에서는 N1/N2가 이미 있으므로 현재 결함이라고 다시 주장하지 않는다.
`InputNativeStopRunningBefore.cfg`는 ExternalOpen→OwnerPasteRunning→Stop의 3단계 반례다. #7812의 BusyClaude sink만 있고 busy submit의 receipt/Applied finish가 빠졌던 연결을 끈다. 현재 busy receipt/first_key→begin→typed finish 연결 및 T0 base RED/수리 GREEN은 이전 구현 증거이며 이 모델 실행이 다시 시험한 것은 아니다.

## 다른 birth child 반례와 모델 경계

기준 `bda08f189c775de190d5be53a053f9ba31d94fd0`의 모델은 child를 bool로만 두어, durable frame이 현재 birth에도 속한다는 가정을 숨겼다. 이번에는 `birthChild=episode|parent`라는 별도 사실을 둔다. 이 값은 bound birth 범위가 확인된 token의 복구 child 판정이며 `pinned`에서 추론하지 않는다. `OwnerPasteRunning`이 pin을 얻어도 birthChild는 unbound이고, relay 경로의 pin 없는 추상 상태에서도 명시적 bound-child 관측을 만들 수 있다. 따라서 direct native pin을 birth 증거로 승격하지 않는다.

`OtherEpisodeChild`/`OtherParentChild`는 현재 token에 속하지 않는 child를 복구가 이미 거부한 사실(`child=FALSE`)과, exact durable ledger 조회는 그 attempt를 찾는 사실(`ledgerChild=TRUE`)을 함께 기록한다. `EpisodeChecked=FALSE`일 때만 이 조합을 `ObservedForeign`이 놓친다. 고친 설정은 foreign를 그대로 관측해 Stop을 차단한다. 이는 입력·token 수명 계약을 넓히는 대신 기존 birth 격리를 보존하는 판독 수리다.

새 행동은 clean canonical root 또는 relay NativeStop 상태에서 하나의 다른 birth child를 관측한 뒤 바로 Stop하는 유계 순서만 추가한다. 기존 433상태의 전이는 모두 남기고 이 경계의 8상태/12전이를 더했다. 잘못된 child 뒤 여러 추가 입력·두 mismatch 동시 발생·binding 교체의 순열은 모델 범위 밖이다. 실제 전달 직전 재검사와 여러 child/같은 본문의 정확한 귀속은 구현 시험이 맡는다. 이 경계를 감추려고 property나 invariant 식을 약화하지 않았다.

EpisodeBefore의 각 shortest path는 4단계다.

| 단계 | episode / parent 반례 | 기준 코드 대조 |
|---|---|---|
| 1 | ExternalOpen | 실제 시험 `native_stop_pin/tests.rs:516–521`이 native root와 busy input을 가진 inflight를 저장한다. |
| 2 | OwnerPasteRunning | root의 native pin은 `claude_stop_identity.rs:60–72`로 현재 turn identity가 된다. pin 존재와 birth child 소유는 별개다. |
| 3 | OtherEpisodeChild / OtherParentChild | 시험 `:528–531`은 child의 turn_nonce 또는 user_msg_id만 바꾼다. `NativeBusyChildren::owned_by`(`native_stop_pin.rs:82–87`) 및 복구 필터(`:178–183`)는 child를 token에 복원하지 않는다. 그러나 `claude_stop_identity.rs:358–366`은 token.is_busy_child가 FALSE인 Command를 durable Discord fallback으로 다시 BusyChild라 판정한다. |
| 4 | Stop(sent) | `pinned_still_current`(`claude_stop_identity.rs:94–103`)의 clean 판정이 성공해 native 전달을 허용한다. CI 실제 `begin_user_stop`(`native_stop_pin/tests.rs:603–614`)에서 other-episode의 key count 1이 expected 0에 걸린다. parent도 같은 판독 경계다. |

`NoForeignEscape`와 `NoOtherEpisodeEscape`의 반례는 같은 episode 경로이며 parent는 `NoOtherParentEscape`로 별도 표시한다. fixed는 두 경로 모두 denied다. 이 모델 PASS는 parent 레인의 main 기준선·PR RED·수리 GREEN 및 episode 가드 제거 mutant 실행을 대신하지 않는다.

## 결정2: persisted child 근거 없는 restored token

H1은 preclaim A → claim → restart(children 없음), H2는 claim → A child 저장 실패 → restart(children 없음)다. transcript·frame ledger·현재 inflight에 A가 똑같이 남을 수 있으므로 이를 구분하는 숨은 과거 변수나 새 durable ledger를 만들지 않는다. 결정2는 restored token에서 두 경우 모두 Escape 0이며, live nonrestored claim-gap N1과 tokenless N2는 유지한다.

새 `restoredToken`은 `ReceiptSink.restored`에 대응하는 별도 사실이다. 기존 `restarted`(프로세스 재시작 관측), `pinned`(현재 native pin), direct pin만으로 이 값을 추론하지 않는다. 기준 bda의 `provider/native_stop.rs:62,154–156`은 restored candidate를 별도로 보존하고 `:160–179`는 재검증 뒤 direct accepted pin이 있어도 restored candidate가 있었다는 사실을 지우지 않는다. 모델도 두 사실을 분리한다.

`RestoreUnprovenChild`는 relay가 가진 외부 root의 복구 snapshot에 `restoredToken=TRUE`, `child=FALSE`, `ledgerChild=TRUE`를 세운다. 여기서 `attempt=none`은 A가 실제로 없다는 뜻이 아니다. A의 durable 존재는 ledgerChild에 남지만 현재 restored native root의 child membership 또는 paired own attempt를 증명하지 못한다는 뜻이다. 이는 H1/H2를 구분하지 못한다는 결정의 입력 분류이며, `OwnInputStops` 식을 수정해 PASS를 만드는 것이 아니다. 기존 441상태와 모든 전이를 그대로 두고 이 snapshot의 2상태/3전이만 더했다. 다섯 기존 불변식 식은 s1-7 소스와 문자 단위로 동일함을 확인했다.

새 snapshot 다음에는 바로 Stop만 허용한다. fixed는 `RestoredChecked=TRUE`이므로 거절하고, 새 RestoredBefore에서만 그 가드를 꺼 `NoUnprovenRestoredChildEscape`를 깬다. EpisodeBefore는 known birth mismatch의 별도 판독 축이며 기존 세 위반을 유지한다. 이 역사적 Before 설정들은 각 계약의 누락을 분리하며, 최신 Rust에서 가드 하나만 지운 실제 mutant와 모든 상태가 일대일 대응한다고 주장하지 않는다.

RestoredBefore shortest path는 4단계다.

| 단계 | 모델 사실 | 코드/계약 대조 |
|---|---|---|
| 1 | ExternalOpen | 사람이 연 P의 transcript/source가 있다. |
| 2 | RelayClaim | 현재 mailbox token이 P를 맡는다. 아직 restored membership 증거가 생긴 것은 아니다. |
| 3 | RestoreUnprovenChild | H1/H2가 같은 durable snapshot으로 복원된다. `native_stop_pin.rs:178–183`의 persisted child 복원 근거는 없고 ledger에는 A가 남는다. `ReceiptSink.restored`는 별도 사실이다. |
| 4 | Stop(sent) | 기준 `claude_stop_identity.rs:358–366`의 ledger fallback이 token child 거절을 다시 승인한다. 결정2의 고친 설정은 같은 snapshot에서 denied다. 실제 Rust 가드·mutant·진입점 시험은 parent 레인 증거가 맡는다. |

live claim-gap 경로 `ExternalOpen → OwnerPaste → Absorb → RelayClaim → Stop`은 restoredToken이 FALSE라 기존 N1을 유지한다. tokenless 경로 `ExternalOpen → OwnerPaste → Absorb → Restart(readable) → Stop`도 relay가 없고 restoredToken이 FALSE라 N2를 유지한다. 새 모델은 이 양성 흐름을 삭제하거나 `OwnInputStops`·`NoForeignEscape`·`EnqueueNotAuthority`를 약화하지 않았다. O-on 전달 정책과 실제 source/attempt/parent 비교는 계속 구현 시험의 범위다.

## 모델 밖에서 필요한 구현 시험

- exact frame·attempt·file·offset·native SID와 동일 본문 여러 입력의 귀속은 boolean own보다 풍부하다. 모델은 같은 논리 입력의 Command/absorption을 두 번 세지 않는 계약만 접으며, 다른 attempt 소비 여부를 증명하지 않는다. 실제 Enter→실제 !stop 시험이 필요하다.
- parser strict boolean, session 불일치, malformed/partial, ledger Unread와 source 파일 교체는 실제 판독기의 시험 대상이다. 여기서는 확인된 summary와 foreign 입력을 분리한다.
- P1-3 durable structured-state fallback과 pane active/modal/draft 조건은 모델 범위 밖이며 진입점·전달 시험이 맡는다. 열린 턴이라는 이유만으로 Streaming을 가정하지 않는다.
- Stop은 전달 직전 source/session/opener/open/own/clean 재검사를 한 단계로 접는다. close·foreign 경합과 종료 관측 뒤 반복 !stop은 실제 시험이 필요하다.
- O-on SessionStop, mailbox/intake/bridge/watcher의 수명과 응답 게시 주체는 바꾸거나 증명하지 않는다.

## 재생 명령과 원시 증거

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/input-single-path
for cfg in InputNativeStop.cfg InputNativeStopBefore.cfg InputNativeStopRunningBefore.cfg InputNativeStopAbsorptionBefore.cfg InputNativeStopSummaryBefore.cfg InputNativeStopEpisodeBefore.cfg InputNativeStopRestoredBefore.cfg; do
  ~/.claude/skills/statechart/scripts/statechart InputNativeStop.tla --config "$cfg" \
    --view native-stop.view.mjs --out /tmp/sc-native-stop-new-run-"${cfg%.cfg}"
done
```

레인 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7755/` 아래 `sc-out/s1-6-{fixed,before,running-before,absorption-before,summary-before}/`에 tool chart/report/tlc 원문이 있다. 로그는 `logs/s1-6-statechart-<이름>.log`, 5cfg 수·rc·경로는 `s1-6-statechart-evidence.json`이다. 이 실행은 구현 시험의 GREEN을 대신하지 않는다.

전체 checker는 새 cfg 2개를 포함해 **rc0 / 불일치0 / 85cfg / 201.5초**로 완료했다. 원시 로그는 `logs/s1-6-statecharts-all.log`다. 실행 명령은 `python3 scripts/ci/statecharts_tlc.py --java "$JAVA" --jar "$TLA2TOOLS"`이며 모든 cfg가 등록된 기대 위반 집합과 일치했다. 이후 main 병합이 모델/cfg/expected를 바꾸면 이 증거를 새 입력의 결과로 재사용하지 않는다.

## #7827 병합 뒤 전체 재검사

main이 `InputOwnershipConsumedWaiting`의 7cfg와 기대 행을 추가했으므로 이전 85cfg 결과를 대신 사용하지 않고 전체 checker를 다시 실행했다. 결과는 **rc0 / 불일치0 / 92cfg / 259.6초**다. 원시 로그는 레인 `logs/s1-6-statecharts-after-main.log`, HEAD/main·명령·모델 입력 SHA는 `s1-6-statechart-after-main-evidence.json`에 있다. `InputNativeStop` 5cfg의 기대 위반 집합도 그대로 일치했다. 이는 parent의 최종 Rust 시험·clippy를 대신하지 않는다.

## s1-7 birth-child 확장 검증(이전 모델)

최종 6cfg의 model/view 입력은 레인 `s1-7-statechart-evidence.json`에 SHA256과 함께 저장했다. `sc-out/s1-7-final-{fixed,before,running-before,absorption-before,summary-before,episode-before}/`에 tool chart/report/TLC 원문이 있고, 로그는 `logs/s1-7-statechart-final-<이름>.log`다. 고친 설정은 rc0, Before 다섯 설정은 각각 위 표의 기대 위반으로 rc1이다. 이전 5cfg의 기대 위반 집합은 그대로다. 모든 차트의 표시 상태 도달 가능성과 새 child 상태의 Stop 화살표, mayBreak 없음도 확인했다.

최초 시험 실행 뒤 복구 snapshot의 `restarted=TRUE` 사실을 명시해 최종 6cfg를 새 출력 폴더에서 다시 실행했다. 기존 출력 폴더 재사용 거부(rc2)는 도구의 출력 보호 동작이며 모델 위반/성공으로 세지 않는다. 최종 소스와 일치하는 증거는 final-* 폴더와 로그뿐이다.

전체 checker는 **rc0 / 불일치0 / 93cfg / 220.5초**다. 원시 로그는 `logs/s1-7-statecharts-all.log`, 명령·코드 기준 SHA·전체 TLA/cfg/expected 입력 SHA는 `s1-7-statecharts-all-evidence.json`에 있다. 모델 변경 전의 92cfg 결과를 새 모델의 결과로 재사용하지 않았다. Rust 실행 결과는 parent 레인 보고서에 별도로 기록한다.

## s1-9 restored-child 검증

s1-7의 93cfg 원시 증거는 그대로 보존했다. 이번 7cfg의 입력 SHA·git HEAD·working Rust 전체 SHA는 레인 `s1-9-model-work.json`/`s1-9-statechart-evidence.json`, 각 tool chart/report/TLC 원문은 `sc-out/s1-9-{fixed,before,running-before,absorption-before,summary-before,episode-before,restored-before}/`에 있다. 로그는 `logs/s1-9-statechart-<이름>.log`다. fixed rc0, Before 여섯 cfg는 위 표의 기대 위반으로 rc1이다. 기존 6cfg의 위반 집합은 그대로다. 모든 차트의 표시 상태 도달 가능성과 새 child 상태의 Stop 화살표, mayBreak 없음도 확인했다.

working Rust SHA는 모델 실행 시 함께 관측한 worktree의 코드 스냅샷이며 Rust 컴파일·시험 PASS를 의미하지 않는다. 모델의 기준 코드 좌표는 bda이고 root의 생산 수리와 독립적으로 결정2 계약을 검사했다. 전체 checker는 **rc0 / 불일치0 / 94cfg / 224.0초**다. 원시 로그는 `logs/s1-9-statecharts-all.log`, 명령·전체 TLA/cfg/expected 입력 SHA는 `s1-9-statecharts-all-evidence.json`이다. s1-7의 93cfg 결과를 새 모델 PASS로 재사용하지 않고 새 cfg까지 전수 실행했다. `s1-9-model-work.json`에는 실행 시작/종료의 working Rust SHA와 그 사이 바뀐 파일도 따로 기록했다. 생산 Rust 수리는 root가 병행했지만 모델/TLA 입력은 실행 내내 같은 SHA였다.
