# InputNativeStop — 외부 Claude 턴에 넣은 자기 입력의 중단

코드 좌표 기준 SHA: `f332bd53918fcd3ac9867034f9323b42bf0d8cda`, 병합한 main `dc5c742e40079a724a557ed29378cba6c7dbd75f` (#7788 S2/#7824).
모델의 fixed는 이 기준 코드에서 지적된 remove-only 흡수와 자동 compact summary 판독을 수리하는 계약이다. 구현 시험의 성공을 뜻하지 않는다.
기존 Before 두 설정은 처음 설계한 receipt/N1/N2 연결을 각각 끄는 역사적 결함 설정이며, 새 두 Before는 기준 SHA에서 확인한 P1-1/P1-2 판독 결함을 분리한다. `InputOwnership.tla`는 이 변경에서 수정하지 않는다.

| 설정 | `Connected` | `RunningReceipt` | `AbsorptionEvidence` | `CompactSummaryNeutral` | exit | 상태 / 전이 | 깨진 규칙 |
|---|---:|---:|---:|---:|---:|---:|---|
| `InputNativeStop.cfg` | TRUE | TRUE | TRUE | TRUE | 0 | 433 / 1231 | 없음 |
| `InputNativeStopBefore.cfg` | FALSE | TRUE | TRUE | TRUE | 1 | 433 / 1231 | `OwnInputStops` |
| `InputNativeStopRunningBefore.cfg` | TRUE | FALSE | TRUE | TRUE | 1 | 433 / 1231 | `OwnInputStops` |
| `InputNativeStopAbsorptionBefore.cfg` | TRUE | TRUE | FALSE | TRUE | 1 | 433 / 1231 | `NoForeignEscape`, `OwnInputStops` |
| `InputNativeStopSummaryBefore.cfg` | TRUE | TRUE | TRUE | FALSE | 1 | 433 / 1231 | `OwnInputStops` |

## 차트와 그림의 범위

- [고친 설정](native-stop.chart.mmd): 13상태/54화살표.
- [N1/N2 연결 누락](native-stop-before.chart.mmd): 14상태/56화살표.
- [running receipt 연결 누락](native-stop-running-before.chart.mmd): 14상태/55화살표.
- [Command 없는 absorption을 놓침](native-stop-absorption-before.chart.mmd): 15상태/57화살표.
- [compact summary를 foreign user로 오판](native-stop-summary-before.chart.mmd): 14상태/56화살표.

TLC는 각 설정에서 같은 433상태/1231전이를 검사한다. view는 원장을 성공적으로 다시 읽은 `Restart` 66전이를 숨긴다. 같은 표시 상태에 머무는 624전이는 tool report에 원래 수가 남는다. 생략한 전이는 TLC 검사에는 포함된다.
summary를 읽은 token-root를 별도 표시 상태로 나누어 반례의 3번째 단계를 숨기지 않는다. 외부 입력이 섞인 상태의 잘못된 전달은 정상 전달과 다른 표시 상태이며 `NoForeignEscape`의 `breaks` 표식을 보존한다. 각 그림의 모든 표시 상태는 시작 상태에서 도달하고, 종료 결과 상태를 제외하면 나가는 화살표가 남는다.

## 규칙과 모델의 실제 사실

- `NoForeignEscape`: token 없는 O-off에서 자기 입력 흡수 증거가 없거나 외부 입력이 섞이면 Escape 0이다. token NativeStop의 foreign 입력도 막는다. 이미 relay가 claim한 순수 외부 opener의 direct pin Escape 1은 기존 권한으로 유지한다.
- `OwnInputStops`: 자기 입력이 실제로 흡수됐거나 running 분기에서 자기 root가 생겼고 foreign 입력이 없으면 Escape 1이다.
- `EnqueueNotAuthority`: token 없는 상태에서 enqueue만으로 Escape 권한을 만들지 않는다.
- `NoDeadlock`: 결과 뒤에는 `Done` 자가 전이가 있다.

`OwnEvidence`와 `foreign`은 실제 입력 사실이며 판독기가 놓친 사실을 지우지 않는다. `ObservedOwn`/`ObservedForeign`과 summary 판정에서만 Before 상수를 사용한다. 따라서 고치기 위해 불변식을 약화하거나 remove-only/summary 상태를 검증 대상에서 빼지 않는다.

`OwnerPaste`는 자기 attempt를 enqueue한다. `AbsorbOnly`는 Command 행 없이 `remove(absorbed_mid_turn)`로 실제 흡수되어 `removed` 상태가 된다. 뒤에 오는 `Absorb`(Command)는 같은 입력을 `absorbed`로 바꿀 뿐 두 번째 입력을 만들지 않는다. Command가 먼저 온 뒤의 `AbsorbAfterCommand`도 같은 입력의 idempotent 사건이다. enqueue만 있으면 실제 OwnEvidence가 없다.
`ForeignAbsorbOnly`는 실제 foreign를 세우고, 구판독(`AbsorptionEvidence=FALSE`)만 그 형식을 놓친다. `CompactSummary`는 같은 SID의 확인된 boolean summary로, 실제 foreign를 세우지 않는다. 일반 user/false/문자열 true/SID 불일치는 이 행동이 아닌 `ForeignInput`에 속한다.

`foreign`는 현재 opener 뒤 전체를 뜻한다. owner paste/흡수가 이를 초기화하지 않아 opener와 자기 입력 사이·뒤·같은 행의 혼합을 모두 보수적으로 막는다. 새 자기 user root를 여는 `OwnerPasteRunning`만 이전 opener의 foreign를 초기화한다.
`Restart`는 durable attempt/transcript 사실을 남기고 프로세스의 relay token/child를 제거한다. 원장 읽기 성공과 Unread를 각각 검사한다. running token의 재시작 pin 복원은 기존 NativeStop 범위이므로 Restart는 running 분기를 제외한다.

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
for cfg in InputNativeStop.cfg InputNativeStopBefore.cfg InputNativeStopRunningBefore.cfg InputNativeStopAbsorptionBefore.cfg InputNativeStopSummaryBefore.cfg; do
  ~/.claude/skills/statechart/scripts/statechart InputNativeStop.tla --config "$cfg" \
    --view native-stop.view.mjs --out /tmp/sc-native-stop-new-run-"${cfg%.cfg}"
done
```

레인 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7755/` 아래 `sc-out/s1-6-{fixed,before,running-before,absorption-before,summary-before}/`에 tool chart/report/tlc 원문이 있다. 로그는 `logs/s1-6-statechart-<이름>.log`, 5cfg 수·rc·경로는 `s1-6-statechart-evidence.json`이다. 이 실행은 구현 시험의 GREEN을 대신하지 않는다.

전체 checker는 새 cfg 2개를 포함해 **rc0 / 불일치0 / 85cfg / 201.5초**로 완료했다. 원시 로그는 `logs/s1-6-statecharts-all.log`다. 실행 명령은 `python3 scripts/ci/statecharts_tlc.py --java "$JAVA" --jar "$TLA2TOOLS"`이며 모든 cfg가 등록된 기대 위반 집합과 일치했다. 이후 main 병합이 모델/cfg/expected를 바꾸면 이 증거를 새 입력의 결과로 재사용하지 않는다.
