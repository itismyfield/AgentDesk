# Claude tmux 기계 입력 선두

기준 `9243b528dd50f4ca4ae4e75442afdc8f6bc2227a`는 #8328 w1g에 main을 병합한 커밋이다.

## 상태와 규칙

| 모델 값 | 코드 근거 |
|---|---|
| `evidence = Idle, Busy, Unknown` | 호스트의 `Activity`와 현재 실행 transcript 끝. `src/services/discord/turn_presence/activity.rs:38`, `src/services/tui_turn_state.rs:34`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:885` |
| `phase = Unread, Held, Idle, Busy` | `Unread`는 관측 전, `Held`는 불확실한 증거의 보류, 나머지는 owner 등록 값. `src/services/discord/queue_io/input_owner/target.rs:37`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:104`, `:1234` |
| `Broken` | `BusyState::Legacy`가 tmux 호스트를 표시한다. 레거시 진단은 transcript가 존재하면 그 상태를 읽지 않고 `Busy`를 등록한다. `src/services/discord/queue_io/input_owner/claude_handoff.rs:576`, `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:349`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:642` |
| `wake` | 입력 승인, 복구, 턴 종료 또는 이미 등록한 probe가 drive를 깨운다. `src/services/discord/queue_io/input_owner/start.rs:74`, `src/services/discord/queue_io/input_owner/drive.rs:88`, `src/services/discord/queue_io/input_owner/held_watch.rs:149`, `src/services/discord/queue_dispatch/kickoff.rs:126` |
| `delivered`, `busyWrites` | 같은 선두의 첫 키와 전달 정산. Idle writer는 효과 전에 현재 준비도와 실행을 검사한다. `src/services/discord/queue_io/input_owner/effect.rs:223`, `src/services/claude_tui/readiness/wait.rs:47`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:1476` |

`NoMachineWriteWhileBusy`는 현재 호스트 상태가 Busy일 때 기계 입력 키 전송 수가 0임을 검사한다. `IdleFrontEventuallyDelivered`는 기계 입력이 계속 선두에 있고 호스트가 안정적으로 Idle이면 전달로 진행하는지 검사한다. 기존 wake가 도착하고 일반 준비도 가드가 풀리는 실행을 모델링한다. 세션 부재와 cold launch, 계정 전환, 초안·모달, 전송 불확실성과 영구 오류는 이 투영 밖이다. 화면 probe는 정확성 판단 값에 포함되지 않는다.

## TLC

| CFG | Busy 입력 0 | 안정적으로 Idle인 선두 전달 |
|---|---|---|
| `Before` | GREEN | RED. Idle 증거로 다시 깨어나도 강제 Busy |
| `Fixed` | GREEN | GREEN |
| `NoFirstKeyGuard` | RED. Idle 관측과 첫 키 사이 새 턴이 열림 | GREEN |
| `NoWake` | GREEN | RED. 첫 보류 뒤 재판정 기회가 들어오지 않음 |

TLC 1.7.4에서 4개 cfg, 9회 실행 모두 예상과 일치했다. `NoWake` 반례의 drive 재판정은 #8458 범위다.
