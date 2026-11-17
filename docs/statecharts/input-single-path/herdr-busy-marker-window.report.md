# Herdr busy 첫 시도 거절과 📬 선게시 (#8113 수리 1)

기준 코드 `2f32ed6e23`. 사람 입력 하나가 실행 중인 Herdr Claude 턴에 도착해 큐 커밋 → owner busy 판독 → 보류·재시도 → 주입까지 가는 경로만 모델링한다. 기존 모델·cfg는 바꾸지 않았다. 모델 결과는 Rust 시험의 대체 증거가 아니다.

## 코드 근거와 값

| 모델 값·행동 | 코드 좌표 | 의미 |
|---|---|---|
| `tail="prompt-markers"` | Claude transcript: 제출된 prompt·tool_result 뒤 `file-history-snapshot`·`last-prompt`·`mode`·`permission-mode`·`session-owner`·`atis-latch`·`pr-link`·`frame-link` | 첫 reply 레코드 전 구간. 실기 07:11:17·07:11:46 입력이 이 구간에 도착했다. |
| `tail="reply-markers"` | 끝난 reply(end_turn) 뒤 같은 marker | 턴이 끝나는 중이라 실제로 관측 불가. |
| `Reading` Before | `src/services/tui_turn_state/claude_meta.rs` `observe` | marker 뒤에서 미결 tool 호출만 Streaming, prompt·tool_result는 Unknown. |
| `Reading` Fixed | `claude_meta.rs` `observe_running`, `src/services/claude_tui/host_input/busy/herdr.rs` `Context::read` | busy 쓰기만 marker 뒤 prompt·tool_result를 실행 중으로 읽는다. 끝난 reply·interrupt·local command·meta·compact summary는 Unknown 유지. |
| `Commit`·`MarkerFirst` | `src/services/discord/router/intake_gate/busy_inject.rs` `queue_commit_options` | Before는 Herdr 채널이면 커밋 때 📬를 붙인다(07:11:33 RACE). Fixed는 owner 결과로 미룬다. |
| `Attempt` Unknown → held·📬 | `src/services/discord/router/message_handler/intake_turn/owned_busy.rs` `running`·`hold`, `busy_retry.rs` `note_busy_result` | 키 0 보류와 handback 📬. |
| `Retry` | `src/services/discord/queue_io/input_owner/held_watch.rs` | 같은 판독이 Busy/Idle이 되면 drive를 한 번 깨운다. |
| `Scenario="handoff"`·`Prepare`·`tail="closing"` | `src/services/discord/queue_io/input_owner/claude_handoff.rs` `prepare`(:229 old pane close 뒤 mailbox active면 `hold`) | Herdr 매핑 채널의 옛 tmux 턴이 진행 중일 때 첫 입력. pane은 닫혔지만 옛 턴 finalizer가 mailbox를 놓기 전이라 키 0 보류. |
| `PrepareSettles` (`Policy="Fixed"`) | `provider_dispatch/owner_input.rs owner_drive`: `prepare` 결과와 그 직후 idle 시작을 공통 결과 정산(`note_unsent_front`)으로 합류 | `Fixed1`은 첫 리뷰 head `e9647e3a72`: `prepare(...).await?`가 정산 전에 반환해 📬 0회. |

## 규칙

- `AnswerAtMostOnce`: 주입·➕ 각 1회 이하. `AnswerDelivered`(liveness): 결국 1회 전달.
- `NoMailboxWhenObservable`(새 INVARIANT): 도착 때 턴이 관측 가능하게 실행 중이면 📬 추가 0회. 전환이 필요한 도착(`arrival="handoff"`)은 제외.
- `NoSilentHold`: 키 전 보류된 입력은 📬를 보인다(조용한 유실 0).
- `NoBusyKeyWithoutRunningTurn`: busy 붙여넣기는 reply를 아직 빚진 턴에만.

## 수정 전 RED / 수정 후 GREEN (TLC 1.7.4 실제 실행)

| cfg | Policy·Scenario·Mutant | 깨진 규칙 | 기대 |
|---|---|---|---|
| PromptBefore | Before·prompt-markers | NoMailboxWhenObservable | RED (첫 시도 거절 07:11:17·46) |
| PromptFixed | Fixed·prompt-markers | - | GREEN (liveness 포함) |
| AssistantBefore | Before·assistant | NoMailboxWhenObservable | RED (커밋 📬, 07:11:33 RACE) |
| AssistantFixed | Fixed·assistant | - | GREEN |
| ReplyFixed | Fixed·reply-markers | - | GREEN: 보류+📬, 턴 종료 뒤 전달 |
| AllMarkersBusy | marker 뒤 무엇이든 Busy | NoBusyKeyWithoutRunningTurn | RED |
| HerdrMarkerFirst | Herdr 커밋 📬 복원 | NoMailboxWhenObservable | RED |
| NoHandback | 보류 때 📬 생략 | NoSilentHold | RED |
| HandoffBefore | Before·handoff | - | GREEN (커밋 📬가 이미 보임) |
| HandoffFixed1 | Fixed1·handoff (리뷰 head) | NoSilentHold | RED (조용한 보류, 리뷰 P1-1) |
| HandoffFixed | Fixed·handoff | - | GREEN: 보류 📬 1회, 옛 턴 종료 뒤 전달 |

공식 checker: 11/11 기대 일치, GREEN5 / expected RED6, 25.2초, exit0(기존 8 cfg 결과 불변). 원문은 레인 로그 `logs-fix-2/tlc-after.log`(첫 판 8 cfg는 `logs-fix-1/tlc-after.log`).

## 한계

한 입력·한 턴만 다룬다. source/pin/nonce 검증, 출력 writer 준비, 모달·draft, 복수 입력 합치기는 `HerdrBusyActiveSource`·`HerdrBusyInput`과 Rust 시험의 계약이다. transcript 레코드가 디스크에 쓰이는 시점은 추상화했다.
