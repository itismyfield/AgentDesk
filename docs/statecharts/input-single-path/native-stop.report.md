# InputNativeStop — 외부 Claude 턴에 넣은 자기 입력의 중단

코드 기준 SHA: `216858a711a6dfea5b18243e947199b461ea9b37` (#7812 착지 직후).
`s1-2`에서는 busy submit의 빠진 native receipt 연결과 N1·N2 구현 계약을 모델링한다. 구현 시험 결과를 대신하지 않는다.
`InputOwnership.tla`의 전이와 저장 수명은 바꾸지 않는다.

| 설정 | `Connected` (N1/N2) | `RunningReceipt` | exit | 상태 / 전이 | 깨진 규칙 |
|---|---:|---:|---:|---:|---|
| `InputNativeStopBefore.cfg` | FALSE | TRUE | 1 | 109 / 222 | `OwnInputStops` |
| `InputNativeStopRunningBefore.cfg` | TRUE | FALSE | 1 | 109 / 222 | `OwnInputStops` |
| `InputNativeStop.cfg` | TRUE | TRUE | 0 | 109 / 222 | 없음 |

첫 Before는 running receipt 수리가 있는 상태에서 N1·N2 누락을 분리한다. RunningBefore는 N1·N2가 있는 상태에서 receipt 누락을 분리한다. 기준 SHA의 실제 busy 경로는 두 연결이 모두 없지만 각 누락의 최단 코드 반례는 다른 연결의 유무에 영향을 받지 않는다.

N1·N2 수정 전 차트: [native-stop-before.chart.mmd](native-stop-before.chart.mmd) (11상태·29화살표).
running receipt 수정 전 차트: [native-stop-running-before.chart.mmd](native-stop-running-before.chart.mmd) (11상태·28화살표).
두 연결을 적용한 차트: [native-stop.chart.mmd](native-stop.chart.mmd) (10상태·28화살표).
원장을 성공적으로 다시 읽은 `Restart` 16전이는 그림에서 생략하며 TLC는 모두 검사한다. 같은 표시 상태에 머무는 89전이도 그림에 표시하지 않는다. 생략 후에도 모든 표시 상태는 초기 상태에서 도달하고, 모델에 후속 전이가 있는 표시 상태에는 나가는 화살표가 있다.

## 규칙과 범위

- `NoForeignEscape`: O-off에서 token 없는 외부 턴에 흡수된 자기 입력의 증거가 없거나, 현재 root 뒤에 외부 입력이 섞이면 Escape 0이다. relay token과 running token의 NativeStop에도 외부 human 입력 섞임은 Escape 0이다. 이미 relay가 claim한 순수 외부 opener의 direct pin Escape 1은 기존 권한이므로 제외한다.
- `OwnInputStops`: opener 뒤에 흡수된 자기 attempt의 durable 증거가 있거나, running 분기에서 자기 user root가 기록됐으며 외부 입력 섞임이 없으면, 중단 명령은 Escape 1이다. tokenless N2·claim 틈 N1·running receipt 연결을 함께 검사한다.
- `EnqueueNotAuthority`: tokenless 상태에서 enqueue만 된 자기 입력은 중단 권한을 만들지 않는다. relay가 가진 기존 direct pin 권한을 enqueue가 만든 권한으로 취급하지 않는다.
- `NoDeadlock`: 최종 결과 뒤에는 `Done` 자가 전이가 있다.

`foreign`은 현재 opener 뒤의 구간 전체를 뜻한다. `OwnerPaste`나 `Absorb`는 이 값을 초기화하지 않으므로 opener와 A 사이·A 뒤·A와 같은 행의 외부 입력을 모두 섞임으로 추상화한다. opener 자체는 외부 입력이지만 섞임에는 포함하지 않는다.
`attempt = absorbed`는 ledger에 등록된 동일 session의 Discord owner busy attempt와 transcript queued 행의 대응이다. enqueue만 있으면 `enqueued`이며 권한이 생기지 않는다.
`OwnerPaste` 때 relay가 이미 있으면 기존 `hand_busy`처럼 `child`를 사전 등록한다. paste 뒤의 `RelayClaim`은 이 child를 만들지 않으므로 N1이 필요한 창과 기존 Escape 1 경로를 구별한다.
`Restart`는 attempt와 transcript 상태를 보존하고 프로세스의 relay token과 child를 제거한다. 원장 읽기 성공과 Unread를 각각 선택하므로 성공 시 같은 판정, 실패 시 Escape 0을 모두 검사한다.

`OwnerPasteRunning`은 intake가 만들었던 token을 retained 상태로 두고, 실제 붙여넣은 frame과 일치하는 완전한 자기 user 행이 새 native root가 되는 성공 전이다. composer lock 안의 receipt 준비→Applied finish→native pin 관찰을 한 전이로 접는다. `RunningReceipt = FALSE`이면 user 행과 token은 있지만 pin이 없다. 새 user root가 생겼으므로 이전 root 앞 입력은 새 root 뒤의 섞임이 아니며 `foreign`을 초기화한다. 새 root 이후의 `ForeignInput`은 그대로 중단을 막는다.
이 전이는 enqueue-only나 부분 전송을 성공으로 올리지 않는다. native user root pin은 queued child의 ledger 조회와 별개인 기존 검증 경로이며, 모델의 `readable`은 queued-row ledger 조회만 뜻한다.

## N1·N2 반례를 코드와 대조

`OwnInputStops`: “자기 입력이 흡수됐고 섞임이 없으면 내 중단 명령이 전달된다.” 최단 반례는 4단계다.

| 단계 | 모델 / 관측 | 기준 SHA의 코드 근거 |
|---|---|---|
| 0 | 시작 전 | token 없는 O-off 외부 턴 경로를 선택한다. 초기 상태의 추상화다. |
| 1 | `ExternalOpen`: 외부 Claude 턴 P 시작 | intake가 busy 진단을 읽고 owner 경로로 들어간다: `src/services/discord/router/message_handler/intake_turn.rs:1721`. |
| 2 | `OwnerPaste`: Discord owner A를 enqueue | `defer_owned_front`가 token을 해제한다: `busy_retry.rs:49`–`:60`. 그 뒤 `intake_turn.rs:1767`의 `owner.into_busy_pane`로 A를 전달한다. token 수명 연장은 하지 않는다. |
| 3 | `Absorb`: A의 queued 행이 P에 흡수 | `human_frame/history.rs:530`의 `RowOwner::Command`, `:597`의 `row_owner`, `:636`–`:638`의 행/attempt 대응이 durable 증거다. ledger 등록만으로 흡수를 가정하지 않는다. |
| 4 | `Stop`: 자기 입력 중단 실패 | `judged_stop.rs:425`의 SessionStop 분기는 O-on만 선택하고, token 없는 O-off는 `:449`–`:450`에서 `NoActiveTurn`이 된다. Escape가 전달되지 않는다. |

이는 코드로 확인한 T1 재현 후보이며 실제 Discord admission→drive→intake→fake Enter→`!stop` 시험으로 넘긴다. 모델 자체를 구현 시험 PASS로 세지 않는다.

relay가 3과 4 사이에 claim하면 기존 `claude_stop_identity.rs:162`–`:167`은 token에 사전 등록된 child만 허용한다. A는 token이 없을 때 paste됐으므로 child 등록이 없고 `Other`가 된다. N1은 durable ledger의 같은 session Discord busy attempt를 인정하여 이 T3 경로를 해소한다.
기존 relay-claimed 순수 P의 direct pin은 owner A 없이도 Escape 1이다. 모델의 relay authority가 이 동작을 보존한다. `hand_busy`의 child 사전 등록은 `provider_dispatch/owner_input.rs:280`–`:291`에 있다.
N2의 판정→전달 재검사는 `session_stop.rs:75`–`:99`와 같은 패턴이며, 실제 전달 경계는 `claude_stop_delivery.rs:442`, `:507`, `:549`이다. 모델의 `Stop`은 이 재검사를 포함한 한 단계로 추상화한다.

## Running receipt 반례를 코드와 대조

`OwnInputStops`: “token이 남은 running 분기에 자기 user 행이 기록됐으면 내 중단 명령이 그 root를 멈춘다.” 최단 반례는 3단계다.

| 단계 | 모델 / 관측 | 기준 SHA의 코드 근거 |
|---|---|---|
| 0 | 시작 전 | intake는 자신의 token을 보유하고 뒤늦게 busy pane을 만나는 경로를 선택한다. |
| 1 | `ExternalOpen`: 외부 Claude 턴 P가 실행 중 | `provider_dispatch/owner_input.rs`의 `execute_owned` running 분기다. 외부 provider 동작을 한 환경 단계로 접는다. |
| 2 | `OwnerPasteRunning`: 자기 user 행은 기록되지만 pin 없음 | `queue_io/input_owner/effect.rs:204`–`:212`는 sink만 설치한다. `claude_tui/busy_inject.rs:569`–`:594`는 ledger 등록·stash submit만 하고 native receipt·typed finish를 부르지 않는다. 실제 T0 base RED가 Enter user 행은 존재하나 native pin이 없음을 확인했다(`report-s1-1.md`, `logs/base-t0.log`). |
| 3 | `Stop`: 자기 입력 중단 실패 | `provider/native_stop.rs:79`–`:103`은 receipt와 Applied finish를 모두 받아야 하고, `:116`–`:137`은 applied 상태에서만 `try_pin_native_user`를 호출한다. 둘 중 빠진 연결을 기다리는 것으로 pin이 생기지 않는다. |

정상 참조 경로는 `claude_tui/input/submission.rs:149`의 composer lock 내 receipt, `:165`의 first-key, `:168`의 begin, `:175`–`:181`의 typed outcome, `:99`의 finish다. busy 경로의 first-key는 이미 `busy_inject/stash.rs:51`에 있으므로 모델이 별도 키 전이를 추가하지 않는다.
수정 후 `RunningReceipt = TRUE`는 같은 함수·순서·frame을 busy submit에 연결한 계약이며 실제 T0 GREEN과 receipt 삭제 뮤턴트 RED로 확인한다. 이 문서는 실행 전인 시험의 성공을 주장하지 않는다.

## 생략한 범위와 실제 시험의 책임

- O-on tokenless SessionStop 정책은 범위 밖이며 기존 회귀 시험으로 고정한다.
- running 전이는 완전한 자기 user 행이 기록되고 submit이 Applied가 된 경우만 다룬다. `NotSent`·부분 전송·실패의 typed finish 대응과 token 없는 TLS no-op은 코드 시험으로 확인한다.
- running token의 재시작 pin 복원은 기존 NativeStop 수명 문제이므로 이 모델의 `Restart`에서는 제외한다. tokenless·relay claim 틈의 durable 재판정은 계속 검사한다.
- 세부 JSON 파싱, 부분 행, source 파일 교체, session/opener 동일성, P 종료 뒤 Q 시작은 boolean보다 풍부한 실제 상태이므로 코드 시험에서 검증한다.
- 판정과 Escape 사이의 경쟁, 반복 `!stop`과 transcript 종료 기록의 시점은 모델의 원자적 `Stop`이 증명하지 않는다. 실제 전달 직전 재검사 및 경쟁 시험이 필요하다.
- 외부 입력의 위치를 하나의 `foreign` 값으로 접기 때문에, 같은 행 안의 혼합을 실제 parser가 검출하는지는 별도 시험으로 확인해야 한다.
- mailbox·intake·bridge·watcher의 수명이나 응답 게시 소유권은 변경하거나 증명하지 않는다.

## 재생 명령과 원시 증거

```sh
export TLA2TOOLS=~/.local/share/tla2tools/tla2tools-1.7.4.jar
export JAVA=/opt/homebrew/opt/openjdk/bin/java
cd docs/statecharts/input-single-path
~/.claude/skills/statechart/scripts/statechart InputNativeStop.tla \
  --config InputNativeStopBefore.cfg --view native-stop.view.mjs --out /tmp/sc-native-before-s1-2-new
~/.claude/skills/statechart/scripts/statechart InputNativeStop.tla \
  --config InputNativeStopRunningBefore.cfg --view native-stop.view.mjs --out /tmp/sc-running-before-s1-2-new
~/.claude/skills/statechart/scripts/statechart InputNativeStop.tla \
  --config InputNativeStop.cfg --view native-stop.view.mjs --out /tmp/sc-native-fixed-s1-2-new
```

레인 원시 출력: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7755/sc-out/s1-2-{native-before,running-before,native-fixed}/` (`report.md`, `chart.mmd`, `tlc.out` 포함).
요약 로그: 같은 레인의 `logs/s1-2-statechart-native-before.log`, `logs/s1-2-statechart-running-before.log`, `logs/s1-2-statechart-native-fixed.log`.
세 cfg 모두 `docs/statecharts/expected.tsv`에 4컬럼으로 등록한다. 전체 checker는 모델 완료 후 한 번 실행하여 rc=0, 불일치 0, 68cfg, 183.1초를 기록했다. 원시 로그는 같은 레인의 `logs/s1-2-statecharts-all-final.log`다. 실행 명령은 `python3 scripts/ci/statecharts_tlc.py --java "$JAVA" --jar "$TLA2TOOLS"`이며 구현 레인의 최종 검증 증거에도 합친다.
