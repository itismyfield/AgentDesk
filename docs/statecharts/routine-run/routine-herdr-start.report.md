# 루틴·예약 시작 턴의 호스트 판정 모델 (#8125 T2 계획)

코드 기준 `d4e32f2420c8cec4613f94f4d303ad2a24c955d7`, 2026-11-17 KST. 한 루틴 run 또는 예약 메시지의 시작 턴이 **자기 턴 채널에서 어느 실행기로, 몇 번 시작되고 몇 번 종결되는가**만 모델링했다. run 의 claim·재시도·복구·알림은 `RoutineRun`, headless 격리는 `RoutineHeadlessIsolation` 이 계속 맡고 두 모델의 source 는 바뀌지 않았다. 이 모델은 계획 단계의 것이다: 생산 코드는 바뀌지 않았고, `Routed*` cfg 는 후보 정책의 비교다.

## 모델 경계

- `Host`: 턴 채널에 설정된 세션 호스트(`tmux` | `herdr`). 코드에서는 `session_hosts::herdr_endpoint(channel)` 한 곳이 정한다. 스레드 ID 는 설정 목록에 없으므로 `tmux` 다.
- `Backend`: run 에 고정된 실행 backend(`tui` | `process`). `agent_executor/execution.rs` 의 `AgentExecutionMode`.
- `Identity`: 시작이 채널 자신의 세션 키로 도는지(`channel`), 자기 키·pane 을 갖는지(`own`). fresh 루틴과 예약 snapshot 은 `own`, snapshot 이 아닌 예약 메시지는 `channel` 이다(`headless_turn/routine_metadata.rs` 의 `headless_session_basis`).
- `Admit`→`OwnerWake`: TUI 시작은 owner 가 받아 자기 wake 에서 실행한다. process 시작은 owner 를 거치지 않는다(`owner_input_execution`).
- `Judge*`: 시작 직전의 호스트 판정과 실행기 선택. `Before` 는 지금 코드, `gate_only` 는 판정만 intake 와 같게 푼 경우, 나머지 `Policy` 는 backend·identity 로 실행기를 고르는 경우이며 own 키 TUI 시작을 어떻게 다루는지로 갈린다.
- `Restart`: boot 복구가 미전송 입력을 철회하고 run 을 interrupted 로 닫는다(상세는 `RoutineRun`). `GiveUp`: 시작 유예·기한 소진.
- 공정성: Admit·OwnerWake·Judge·HumanFinish·Finish 는 약한 공정성. Restart·GiveUp 은 선택 사건이다. 사람 턴은 시작 시점에 진행 중일 수 있고 언젠가 끝난다.

## 규칙

| 규칙 | 뜻 |
|---|---|
| `NotRefusedAsUnwired` | 시작이 `executor_not_wired` 로 거절되지 않는다 |
| `StartedAtMostOnce` / `ClosedAtMostOnce` | 한 run 의 시작 턴은 많아야 한 번 시작되고, run 은 많아야 한 번 종결된다 |
| `TmuxUnchanged` | tmux 채널에서는 호스트 판독 0, 실행기는 기존 tmux pane 또는 process 다 |
| `OneExecutionPerChannel` | 한 채널의 Herdr 실행은 하나다 |
| `OwnSessionIsolated` | 자기 키를 가진 시작(fresh·snapshot·process)은 채널의 실행에 입력을 쓰지 않는다 |
| `ProcessStaysProcess` | process run 은 TUI 실행기로 가지 않는다 |
| `BackendPinned` | TUI run 이 process 로 바뀌지 않는다 |
| `NoBlindStart` | 채널의 Herdr 실행이 바쁜 동안, 그 실행을 읽지 않은 시작이 실행기에 닿지 않는다(닿은 뒤 실행기의 late 재검사는 모델 밖) |
| `SuccessUnreachable` | 도달성 표식. **깨지면** herdr 채널의 run 이 succeeded 로 끝날 수 있다는 뜻이다 |
| `EveryRunCloses` (liveness) | 모든 run 은 종결한다 |

## 15cfg 결과

`scripts/ci/statecharts_tlc.py`(tla2tools 1.7.4)로 15cfg 를 실제 검사했고 expected.tsv 와 전부 일치한다. `EveryRunCloses` 는 15cfg 모두 유지된다.

| cfg | Host / Backend / Identity / Policy | 깨지는 규칙 |
|---|---|---|
| `BeforeChannel` | herdr / tui / channel / before | `NotRefusedAsUnwired` |
| `BeforeOwn` | herdr / tui / own / before | `NotRefusedAsUnwired` |
| `BeforeProcess` | herdr / process / own / before | `NotRefusedAsUnwired` |
| `TmuxTui` · `TmuxProcess` · `TmuxRouted` | tmux / … | 없음 |
| `GateOnlyChannel` | herdr / tui / channel / gate_only | `NoBlindStart`, `SuccessUnreachable` |
| `GateOnlyOwn` | herdr / tui / own / gate_only | `OneExecutionPerChannel`, `SuccessUnreachable` |
| `GateOnlyProcess` | herdr / process / own / gate_only | `OneExecutionPerChannel`, `ProcessStaysProcess`, `SuccessUnreachable` |
| `RoutedChannel` | herdr / tui / channel / routed | `SuccessUnreachable` 뿐 |
| `RoutedProcess` | herdr / process / own / routed | `SuccessUnreachable` 뿐 |
| `RoutedOwnRefused` | herdr / tui / own / own_refused | 없음(성공에 도달하지 못한다) |
| `RoutedOwnSecondExecution` | herdr / tui / own / own_second_execution | `OneExecutionPerChannel`, `SuccessUnreachable` |
| `RoutedOwnIntoChannel` | herdr / tui / own / own_into_channel | `OwnSessionIsolated`, `SuccessUnreachable` |
| `RoutedOwnAsProcess` | herdr / tui / own / own_as_process | `BackendPinned`, `SuccessUnreachable` |

읽는 법:

- 수정 전(`Before*`)은 세 조합 모두 `executor_not_wired` 로 닫히고 성공에 도달하지 못한다.
- 판정만 푸는 수리(`GateOnly*`)는 거절은 없애지만 다른 규칙을 깬다: 채널 키 시작은 기존 대기가 tmux pane 만 읽어(`owner_input_start_wait.rs` 의 Legacy 분기) 바쁜 Herdr 실행을 보지 못한 채 실행기에 닿고, 자기 키 시작과 process 시작은 채널에 두 번째 Herdr 실행을 만든다.
- backend·identity 로 고르면 채널 키 TUI 시작과 process 시작은 안전 규칙을 모두 지키며 성공에 도달한다.
- 자기 키 TUI 시작(fresh 루틴의 tmux backend, 예약 snapshot)은 어느 후보도 규칙 하나를 내준다. 거절을 정확한 사유로 남기면 안전하지만 성공하지 못하고, 나머지 셋은 각각 채널당 실행 하나·격리·backend 고정 중 하나를 깬다. 이 선택이 설계 판정 대상이다.

## 모델 밖

- process 턴이 Herdr 설정 채널에서 돌 때 그 채널의 inflight 행·출력 게시·row-end 판정이 어떻게 반응하는지는 넣지 않았다. `RoutedProcess` 의 GREEN 은 실행기 선택만의 결과다.
- 한 채널에 Herdr 실행을 둘 두는 경우의 O writer·hook 라우팅·canonical 행 조회는 넣지 않았다.
- 같은 채널의 cancel·stop·clear 는 기존 모델(`RoutineRun`, `input-single-path`)의 범위다.
