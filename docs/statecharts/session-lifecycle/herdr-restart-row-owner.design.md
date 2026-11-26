# 재시작 뒤 Herdr 행의 종결 owner — 한 술어 설계 (#8435 S3)

코드 기준: `b04ea56650e5397989d33f1be199be21a83c8d0e`(origin/main). 이 문서는 운영 코드를 바꾸지 않는다.
아래 Rust 경로는 따로 적지 않으면 `src/services/discord/` 기준이다. 모델은
[HerdrRetiredRow.tla](HerdrRetiredRow.tla)의 `Owner # "off"` 절, 결과는 [report](herdr-retired-row.report.md#restart-owner-8435-s3)에 있다.

## 1. 지금 코드의 세 집합

재시작 뒤 이전 세대(`born_generation`이 현 프로세스와 다름, `inflight/clear_store/reconcile_gate.rs:29-34`) Herdr 행에는 판정이 셋 있다.

| 이름 | 좌표 | 조건 | 소비자 |
|---|---|---|---|
| H (입력 보류) | `recovery_engine/herdr_row_end/owner.rs:110-123` `restarted_owner` | hosted provider, channel ≠ 0, 이전 세대, `hosted_execution_nonce` 또는 Herdr 표식 | `RowHold::RestartedHerdrRow`: `queue_dispatch.rs:233-241`, `router/intake_gate/stale_turn.rs:321`, `router/message_handler/provider_dispatch/owner_input.rs:778-781`, `health/turn_deliver.rs:133-137`, `turn_presence/supervisor.rs:893-898` |
| D (전담 owner) | `owner.rs:127-141` `dedicated_cleanup_owner` | H ∧ (T ∨ kind ∨ before-start stop ∨ receipt 파일 ∨ HotSwap ∨ rebind_origin ∨ replay 보류) | 일반 정리가 손을 뗀다: `tui_direct_pending_start/foreign_recovery.rs:378-393`(손 떼고 `request_row_end(Sweep)`), `tui_prompt_relay/synthetic_orphan_reclaim.rs:71-79` |
| T (end pass 대상) | `recovery_engine/herdr_row_end.rs:53-72` `target()` | 이전 세대, nonce 있음, kind·stop·receipt·HotSwap·rebind·replay 없음, `output_path` 빔, runtime kind 해석됨, Herdr 표식 | `row_end_pass`(`:112-140`, `:122` 필터), `request_row_end`(`:153-181`, `:161` 필터 — 거르면 **로그 없이** 반환), `held_at_restore`(`:34-50`), `settle_row`(`:238`) |

T ⊂ D ⊂ H다. 칸마다 일어나는 일:

- **T**: 부팅 복구가 건너뛰고(`recovery_engine/restore_inflight.rs:215`), end pass가 5초마다(`tui_prompt_relay/source_tick.rs:34,121`) 집는다. 집은 뒤 막히면 사유를 남긴다(`herdr_row_end/diagnostics.rs:153-194`, 사유가 바뀔 때마다 1줄).
- **D \ T**: 부팅 복구의 1회 owner만 있다. receipt는 `restore_inflight.rs:143,209` → `herdr_before_start_restart.rs:113-190`, kind는 `restore_inflight.rs:224` → `herdr_admitted_restart.rs:82-195`. 둘 다 보존하면 "다음 부팅"까지 그대로다(`herdr_admitted_restart.rs:193`의 `Retained` 주석 "Kept for the next boot", `herdr_before_start_restart.rs:114`). 부팅 뒤 다시 보는 곳이 없다. 입력은 H로 막히고, 일반 정리는 D로 손을 떼며, end pass는 T가 아니라서 집지 않는다. `request_row_end`도 `:161`에서 조용히 반환한다.
- **H \ D**(예: `output_path`가 있고 X 항이 없는 행): 부팅 때 wrapper 복구 경로로 간다. `born_generation`은 행을 만들 때 한 번 찍히고(`inflight/model.rs:1068`) 복구가 다시 찍지 않는다. 그래서 복구된 행도 계속 이전 세대이고 H로 입력을 막는다. 복구 reader가 끝을 못 보면(`health/recovery/carried_turn_close.rs:197-201` native end 없음) 일반 정리로 넘어간다. 그런데 일반 정리는 Herdr를 정리하지 못한다. stale-age는 tmux가 아니면 "살아 있음"으로 보존하고(`inflight/removal.rs:417-432`, `host_liveness.rs:22-39`), #4030 관문은 거부한다(`foreign_recovery.rs:466-494`).

### 사례와의 대조 (코드로 확인한 것만)

- 14:48:35 `foreign_recovery`의 "left a restarted Herdr row to its dedicated owner"는 `foreign_recovery.rs:381-390`에서만 나온다. 그래서 그 행은 D였다.
- 같은 채널 발췌에 `herdr row end held a restarted row`·`herdr row end task still running`·`settled a restarted turn`이 한 줄도 없다. T였다면 `request_row_end`가 end task를 띄우고, `settle_row`는 어떤 결과든 `diagnostics::held/settled`로 최소 1줄을 남긴다(`diagnostics.rs:174-191`, 첫 관측은 항상 기록). 그래서 그 행은 **D \ T였다고 보는 것이 코드와 맞는다**. 다만 운영 행 원본을 읽지 않았으므로 X 중 어느 항(kind·receipt·HotSwap·rebind·replay)이었는지는 확정하지 못했다([U1]).
- 부팅 owner의 보존 로그(`recovery kept an admitted Herdr terminal`·`recovery kept a before-start policy receipt`)는 `channel_id` 필드 없이 남는다. 그래서 채널 발췌에 없다는 사실만으로는 판정할 수 없다.

## 2. 목표 술어

`owner.rs`에 분류 함수 하나를 둔다. 기존 세 판정은 모두 이 함수에서 나온다. (w2: 리뷰 P1 3·P2 3 반영, 2.2절 표)

```text
restarted_row(provider, row, current) -> Option<RestartedRow>
  None      : hosted provider 아님 | channel 0 | 현 세대 | Herdr 표식·execution nonce 모두 없음
  Some(Receipt)   : herdr_before_start_stop | receipt 파일
  Some(Admitted)  : tui_terminal_kind
  Some(Planned)   : restart_mode == HotSwapHandoff
  Some(Replay)    : replay_rerun_blocked (inflight/model/identity.rs:107-109)
  Some(Pathless)  : 위가 아니고 지금 target()의 나머지 조건(output_path 빔·runtime kind 해석됨)
  Some(Restored)  : 그 밖(output_path 있음 등)
```

- `restarted_owner` 보류 = `dedicated_cleanup_owner` = end pass 대상 = `restarted_row(..).is_some()`. 이 하나로 세 판정을 정한다.
- `row_end_pass`·`request_row_end`는 `is_some()`인 모든 행을 집고, 새 child `herdr_row_end/restarted_shapes.rs`의 dispatcher가 모양별 settle로 보낸다. 집은 뒤 막히면 반드시 `diagnostics::held` 사유를 남긴다. 조용히 반환하는 곳(`herdr_row_end.rs:161`)은 없어진다.

### 2.1 모든 모양에 공통인 것

- **실행이 끝났다는 증거(E).** 행의 실행이 `HostedState::Retired`이거나(`herdr_row_end.rs:413`), 끝났음을 읽거나(`execution_end`, `:414-431`), **새 incarnation이 그 nonce를 대체한 경우**다. 마지막 경우가 새로 들어간다. `install_pending_pg`는 Pending을 "legacy 행이나 retired된 이전 nonce" 위에만 설치한다(`db/dispatched_sessions/hosted_execution.rs:417-425`). 그래서 행의 `hosted_execution_nonce = Some(n)`이고, 같은 logical key·record id의 현 record nonce가 n이 아니면, n의 실행은 이미 retired다. 지금 `hosted_record`(`herdr_row_end.rs:520-551`)는 이 경우를 `execution_unpinned`로 영구 보존한다(Pathless 포함). 목표에서는 `Superseded`를 돌려주고, `observe`는 retire 없이 E로 다룬다. 이때 hold 해제와 source 판독은 행의 nonce n으로 한다. `release_hold(n)`은 파일이 없으면 `Released`를 돌려주므로 멱등이다(`services/claude/herdr_turn.rs:525-537`). 행에 `hosted_execution_nonce`가 없으면 지금처럼 현 record를 그대로 받는다(대체 판단 불가, 지금 동작 유지).
- **actor 없음.** `actor_absent`(`herdr_row_end.rs:347-364`, mailbox token과 Herdr index)를 그대로 쓴다.
- **두 nonce의 역할([D1] 정정).** `turn_nonce`는 행의 턴 식별자다. 정리 때 잠금 안에서 신원 대조에 쓴다(`clear_store/herdr_row_end.rs:35-36`). 레거시 행은 `None`일 수 있고, 그때는 신원만으로 정리하는 계약이 있다(`inflight/model.rs:400-404`). `release_hold`의 인자는 **실행 nonce**(`record.execution_nonce`, `herdr_row_end.rs:409`)이고 `turn_nonce`가 아니다. w1의 "nonce 없는 행은 집을 수 없다"는 두 nonce를 섞은 결론이었다. w2에서는 `turn_nonce` 없는 행도 분류에 넣는다. 정리는 신원·`save_generation`·episode pin 대조(`fresh.turn_nonce == self.turn_nonce`, 둘 다 `None`)로 하고, `:35`의 빈 nonce 거절은 Pathless·Restored 정리에서 이 대조로 바꾼다. [D1]의 "보류에서 뺀다"는 철회한다. 빼더라도 비Managed 행이 남아 REST(`health/turn_deliver.rs:131-142`)와 native owner(`owner_input.rs:778-781`)가 따로 막기 때문이다.
- **공통 성공 경로와 깨우기(P1-2).** 모양별 settle이 `Settled`를 돌려주면 dispatcher가 `diagnostics::settled` → 전환 락 해제 → `queue_io::input_owner::closed::closed(shared, provider, channel, "herdr_row_end")`(`closed.rs:22-34`, 즉시 idle kickoff)을 부른다. Pathless의 지금 순서(`herdr_row_end.rs:292-296`)와 같다. 이 깨우기가 필요한 이유는 이렇다. owner 입력은 `Wait::Event`에서 claim 전에 돌아가고(`owner_input.rs:978-980`), 큐에는 타이머가 없다. presence도 owned 큐는 Idle 전이 때만 깨운다(`turn_presence/supervisor.rs:681-700`). 부팅 경로의 Admitted·Receipt settle은 바꾸지 않는다. 부팅 settle은 `reconcile_done` 전에 끝나고, 그 뒤의 첫 kickoff가 큐를 읽는다.

### 2.2 모양별 settle

| 모양 | settle | 정리 함수(잠금 안 대조) | 공지 1회 근거 |
|---|---|---|---|
| Pathless | 지금 `settle_row`(`herdr_row_end.rs:222-301`). `:238` 재확인은 "같은 모양인가"로 바꾼다 | `clear_restarted_herdr_row`(`clear_store/herdr_row_end.rs:9-65`). 모양 인자 Pathless는 지금 거절 조건과 같다 | commit → notice → clear 순서. clear가 거절되면 다음 pass는 `fresh_mismatch`(`:235-237`) |
| Admitted | 부팅과 같은 `AdmittedRestartTerminal::settle_checked`(`herdr_admitted_restart.rs:82-195`) 재호출 | `clear_admitted_restart_terminal`(`clear_store/identity.rs:279`) | `delivered = terminal_delivery_committed`(`:43`)이면 다시 보내지 않는다. TUI-O 소유 Aborted 분기(`:122-130`)는 ack를 남기지 않지만 전송 키 `herdr-stop:{turn_nonce}`로 dedupe한다(`turn_bridge/before_start_stop.rs:272-292`). 프로세스를 넘기는 dedupe는 [U5] |
| Receipt | `herdr_before_start_restart::decode(..).settle(..)`(`:76-110`, `:113-190`) 재호출 | `clear_before_start_receipt`(`:182`) | `notice_pending()` 거짓이면 다시 보내지 않는다(`:149`, ack `:164`) |
| Restored(P1-1) | `settle_row`의 E 경로. 정리 직전 관문: 이 행 episode의 reader가 살아 있으면 `restored_reader_live`로 보존한다. 판정은 `tmux_watchers.tmux_session_live_for_relay(row 세션) == Some(true)` ∧ `watcher_output_path(row 세션) == row.output_path`(`engine_runtime/relay_state.rs:476-492`). 채널 매핑(`channel_binding`, `:544-559`)만 남은 경우는 막지 않는다 | `clear_restarted_herdr_row`에 기대 모양을 받게 한다. 잠금 안에서 `:42-46`의 고정 거절 대신 "잠긴 최신 행의 분류 == 기대 모양"을 보고, Restored면 `fresh.output_path == self.output_path`를 본다. `:26`·`:33-41`(receipt, nonce·세대·episode pin·신원, 현 세대)과 `:50`(silent O)은 유지한다 | Pathless와 같다. 모양이 맞으면 정리가 거절되는 경우는 행이 실제로 바뀐 경우뿐이다 |
| Replay(P1-3) | 아래 2.3 | `archive_inflight_state_if_matches_identity_generation`(`clear_store/mod.rs:223-330`, 신원·`updated_at`·`save_generation` 대조, 파일은 `archive/`로 이동) | `deliver_held_debt_checked`는 `terminal_delivery_completed()`면 다시 보내지 않는다(`replay_hold.rs:132-136`) |
| Planned | `planned_restart_owner`로 보존한다. 겹친 이전 프로세스가 owner일 수 있다(`herdr_admitted_restart.rs:89-96`). 끝은 planned 보존 기한 삭제(`inflight/removal.rs:447-490`)다 | — | — |

Restored의 path-less 거절(`clear_store/herdr_row_end.rs:43`)이 지키던 것은 "wrapper 복구 reader가 owner인 행을 지우지 않는다"였다. w2는 그 대리 조건을 다음 세 가지 증거로 바꾼다. (1) 2.1의 E, (2) actor 없음, (3) 같은 episode의 reader 생존 판정(세션 핸들이 살아 있고 heartbeat가 신선하며 출력 경로가 같음). wrapper 복구는 `reconcile_done` 전에만 reader를 붙인다. 그래서 (3)을 확인한 뒤 정리하기 전에 새 reader가 붙는 경로는 없다.

### 2.3 Replay의 끝 owner (U2 닫음)

Replay 행은 두 가지를 함께 진다. 하나는 이전 요청의 **재실행 금지**이고, 다른 하나는 채널에 대한 **새 입력 보류**다. w2는 둘을 나눈다.

- **재실행 금지는 PG가 진다.** 판정 원천은 intake_outbox 영수증의 disposition이다(`replay_hold.rs:38-89` → `stored_disposition_blocks_rerun`). 복구 재시도(`turn_bridge/recovery_text/replay.rs:35-58`)와 세대 정리(`inflight/removal/scoped_restore.rs:54-66`)는 행이 아니라 PG를 읽는다. TUI 실행에서는 저장 본문도 PG `replay_preserved`에 있다(`replay_hold_tests.rs:146-157`가 `{"body": full_response}`를 단언).
- **새 입력 보류는 실행이 끝나면 내려놓는다.** pass는 다음 순서로 진행한다.
  1. 2.1의 E와 actor 없음을 확인한다.
  2. `hydrate_replay_hold_checked`(`replay_hold.rs:38-89`)가 `Verified`이고, 막는 영수증이 있음을 다시 확인한다. 미확인이면 `replay_unverified`로 보존한다.
  3. 저장 본문이 있고 전달이 끝나지 않았으면 `deliver_held_debt_checked`(`:114-175`)가 `Acknowledged`여야 한다. 아니면 `replay_debt_unacked`로 보존한다. 본문이 비면 E 경로 중립 공지 1회(`settle_row`와 같은 notice)로 끝을 알린다.
  4. 행을 지우지 않고 **archive**한다(위 표).
  5. 2.1 공통 깨우기를 한다.
- 선례: owner 정리(clear teardown)가 이미 같은 일을 한다. withheld replay 행을 세션이 사라졌거나 native 완료가 확인됐을 때 archive한다(`commands/control/clear_teardown/execution_cleanup.rs:396,465-484`).
- 부팅 경로(`restore_inflight.rs:165-168`)와 기존 시험(`replay_hold_tests.rs:136-178`, 부팅 뒤 행이 남음)은 바꾸지 않는다. archive는 부팅 뒤 end pass만 한다.
- 가시성: `restore_inflight.rs:53`의 `mod replay_hold;`를 `pub(super)`로, 두 함수와 결과 enum을 `pub(in crate::services::discord::recovery_engine)`으로 넓힌다. giant 파일(`restore_inflight.rs`)에는 이 한 줄만 바꾼다.

브리프의 두 문장과의 대응:
- "dedicated owner로 분류한 행은 end pass가 반드시 집는다" → 같은 함수의 `is_some()`이다.
- "end pass가 집지 않는 행은 RestartedHerdrRow로 입력을 막지 않는다" → 보류도 같은 함수다. 집지 않는 행(None)은 보류하지 않는다.

## 3. stale-age의 Herdr 보존 — 유지

`inflight/removal.rs:417-509`의 보존은 그대로 둔다(브리프 좌표 417-498은 같은 함수 `tmux_pane_alive_for_stale_check` 417-432와 `stale_removal_reason` 434-509다).

- 막는 사고: 기한은 `INFLIGHT_MAX_AGE_SECS = 300`(`inflight.rs:356`)이다. Herdr에는 pane 생존 탐침이 없다. `host_liveness.rs:28-38`은 tmux가 아니면 판정하지 않는다. 보존을 없애면 5분 넘게 도는 정상 Herdr 턴의 행이 나이만으로 지워진다. 그 턴의 terminal은 행을 잃는다(조용한 유실). 입력 보류도 턴 도중에 풀려 새 시작이 진행 중 턴 위로 들어간다. 2026-05-26 사고(DrainRestart pane이 살아 있는데 행 삭제, `removal.rs:466-474` 주석)와 같은 부류다.
- 모델: `HerdrRetiredRowOwnerStaleDrop`(Target + 나이 삭제)은 1단계에서 `RowGoneWithTerminal`을 깬다. 실행이 Bound인 채로 행이 사라진다.
- 원칙: row end 진단 머리말(`herdr_row_end/diagnostics.rs:1-2`)이 "timing here only observes, it is never evidence that the turn ended"라고 적는다. 목표 술어는 이전 세대 Herdr 행마다 증거 기반 owner(E·I 판독, 부팅 settle 재호출)를 준다. 그래서 나이 기반 삭제가 그 빈자리를 메울 필요가 없다.

## 4. 진입점 전수표

`git grep` 기준(시험 파일 제외). "목표" 칸은 위 술어에서 그 경로가 어떻게 되는지 적는다. inflight 행의 생성·정리 API 호출부 96곳은 계열로 묶었다. 원 목록은 레인 로그 `census-sites.txt`에 있고, 행마다 대표 좌표를 적었다.

### 4.1 Herdr 행 판정·정리 (직접 바뀜)

| 진입점 | 좌표 | 지금 | 목표 |
|---|---|---|---|
| 입력 보류 판정 | `owner.rs:28-41,110-123` | H | `restarted_row().is_some()` |
| 전담 owner 판정 | `owner.rs:127-141` | D | 같은 함수(H와 같아짐) |
| end pass 대상 | `herdr_row_end.rs:53-72` | T | Pathless 모양 판정으로 남는다 |
| boot 건너뛰기 | `herdr_row_end.rs:34-50`, `restore_inflight.rs:215` | T면 wrapper 복구를 건너뜀 | 그대로(Pathless만 건너뜀). 다른 모양은 지금처럼 부팅 owner가 먼저 처리 |
| end pass | `herdr_row_end.rs:112-140` | `live ∨ T`만 | `live ∨ is_some()`, 모양별 settle. live 분기(`herdr_row_end_live.rs`)는 바꾸지 않음 |
| 입력·sweep의 end 요청 | `herdr_row_end.rs:153-181` | T 아니면 조용히 반환 | `is_some()`이면 task, 아니면 반환(보류도 안 했으므로 대기 입력 없음) |
| settle_row 재확인 | `herdr_row_end.rs:235-240` | `target()` 재확인 | 같은 모양 재확인 |
| 정리 | `inflight/clear_store/herdr_row_end.rs:9-65` | Pathless 정리, `:43`이 `output_path` 행 거절 | 기대 모양 인자. 잠긴 최신 행의 분류가 같을 때만 정리한다(2.2). Replay는 archive(`clear_store/mod.rs:223-330`) |
| admitted settle | `herdr_admitted_restart.rs:82-195`, 정리 `clear_store/identity.rs:279` | 부팅 1회 | 부팅 + end pass 재호출 |
| receipt settle | `herdr_before_start_restart.rs:9-24,113-190`, 정리 `clear_store/identity.rs:339` | 부팅 1회 | 부팅 + end pass 재호출 |
| foreign recovery 위임 | `foreign_recovery.rs:378-393` | D면 손 떼고 요청 | 그대로(D가 넓어져 Restored도 위임) |
| synthetic orphan reclaim | `synthetic_orphan_reclaim.rs:71-79` | D면 거절 | 그대로(같은 확장) |

### 4.2 Herdr 실행 상태 (실행 Retired를 만드는 곳)

| 진입점 | 좌표 | 행에 하는 일 | 목표 |
|---|---|---|---|
| HTTP terminate | `herdr_terminate.rs:596-641`, retire `:229-250`, settle `:327-391` | 잡은 actor가 있으면 HostTerminated로 정리. CAS 거부 시 행 남음(#8435 (b)) | 바꾸지 않음. 재시작 뒤에는 actor가 없어 `capture_turn`이 `None`(`:156-165`), `settle_captured(None)`은 행을 건드리지 않고 `Retired`(`:363`). 남은 행은 end pass가 E 경로로 끝낸다 |
| 재시작 recovery terminate | `herdr_terminate/restart_recovery.rs:140-170` | 위와 같음 | 같음 |
| root-replaced 정리 | `herdr_terminate/root_replaced.rs:539` | retire, 행 건드리지 않음 | 같음 |
| live row end | `herdr_row_end_live.rs:31-94`(S1 소유) | 현 세대 행 | 바꾸지 않음(S1 계약 유지) |
| launch·return retire | `services/herdr_launch.rs:711`, `herdr_launch/retire.rs:97`, `services/claude/herdr_return.rs:84` | 실행만 retire | 같음 |
| install·bind | `db/dispatched_sessions/hosted_execution.rs:419,460` | 새 실행 | 같음(새 실행은 새 nonce 행) |

### 4.3 inflight 행 생성·변경 (계열)

| 계열 | 대표 좌표 | Herdr 행과의 관계 | 목표 |
|---|---|---|---|
| 새 턴 행 생성 | `router/message_handler/intake_turn.rs:148`, `provider_isolation.rs:278-280`, `health/turn_deliver.rs:696`, `tmux/monitor_auto_turn_inflight.rs:54`, `headless_turn/replay_start.rs:73`, `foreign_row.rs:47,125,395-405`, `manual_rebind/mod.rs:770,1222` (`save_inflight_state_create_new`) | 새 행은 현 세대(`model.rs:1068`) | H 밖. 보류 대상 아님 |
| 없을 때만 생성 | `tui_direct_pending_start/state.rs:403`, `tmux_watcher/liveness.rs:529`, `placeholder_reclaim.rs:193`, `manual_rebind/*.rs` (`save_inflight_state_if_absent`) | 이전 세대 행이 있으면 생성하지 않음 | 목표가 이전 세대 행을 끝내야 진행됨(지금은 막힘) |
| Herdr 필드 기록 | `hosted_execution_nonce`: `inflight/save_store/identity_gate/host_locator.rs:82`, `synthetic_start/bridge_handoff.rs:246`, `claim/admitted.rs:227`, `turn_finalizer/native_prepare.rs:109`; kind: `runtime_stamp/herdr_terminal_admission.rs:143`; stop receipt: `runtime_stamp/before_start_stop_admission.rs:184,246`; output_path: `persist_recovery_output_path_if_matches_identity_locked`(`identity_gate.rs:646`) | 모양을 정함 | 모양 분류의 입력. 분류 함수 하나만 읽는다 |
| 진행 저장 | `stream_loop_patch.rs:35,61`, `watcher_state.rs:99,371`, `episode_guard.rs:142,173` | 현 프로세스 reader | 같음 |

### 4.4 inflight 행 정리 (계열, Herdr 이전 세대 행에서의 동작)

| 계열 | 대표 좌표 | 지금 | 목표 |
|---|---|---|---|
| finalizer episode 정리 | `turn_finalizer/episode.rs:144`(S1 소유), `receipt_settle.rs:276`, `turn_lease.rs:261,309`, `completion_postlude.rs:970`, `synthetic_start.rs:436`, `thread_guard_release.rs:137`, `o_writer_io/agy.rs:111` (`clear_inflight_state_for_captured_episode`) | actor CAS가 필요. 재시작 뒤 actor 없음 | 같음 |
| 부팅 reconcile 정리 | `restore_inflight.rs:257,658,1284,1561,1743,1814,1916,2189`, `recovery_engine/runtime.rs:251`, `watchers/dispatched_origin_ghost.rs:103`, rebind `restore_inflight.rs:196` | wrapper 경로 결과 | 같음 |
| 무조건 정리 | `clear_inflight_state` 27곳: `health/recovery/runtime_stop.rs:59,176,256`, `health/relay_auto_heal.rs:1981-3253`(6), `turn_finalizer/cleanup.rs:1180,1690`, `watcher_backstop.rs:366`, `recovery_paths/restart.rs:116,814` 등 | tmux·actor 근거 경로. Herdr 이전 세대 행에는 도달하지 않거나 근거 부족으로 건너뜀(inv-2 표) | 같음 |
| 식별자 확인 정리 | `tmux_watcher/{stall_exit.rs:50,turn_identity.rs:24,146,no_result_exits.rs:463,terminal_commit_epilogue.rs:326}`, `health/recovery/{watchdog_pass.rs:119,592,inflight_repair.rs:81}`, `placeholder_sweeper/abandon_guard.rs:279`, `relay_recovery/idle_tmux.rs:57`, `standby_relay.rs:712`, `completion_postlude.rs:1002`, `already_finalized.rs:53`, `manual_rebind/mod.rs:202` | tmux reader·watcher 소유 | 같음 |
| 죽음 증거 정리 | `relay_recovery/apply.rs:474`, `foreign_recovery.rs:198` | #4030 관문 | 같음. D에 든 Herdr 행은 위임으로 먼저 빠짐 |
| intake stale release | `router/intake_gate/stale_turn.rs:251` (`delete_inflight_state_file_checked`) | legacy tmux 확인 요구(`admin_host_guard.rs:33`) | 같음 |
| stale-age | `inflight/removal.rs:417-509` | Herdr 보존 | 유지(3절) |
| carried close (tui_o) | `health/recovery/carried_turn_close.rs:185-201` | native end 있을 때만 | 같음. Restored 행의 다른 owner |
| 고아 token 정리 | `health/relay_auto_heal.rs:742`, `relay_recovery/decision.rs:221` | producer liveness 미측정으로 건너뜀 | 바꾸지 않음(#8435 (c) 쪽) |

## 5. 필수 3항목

### (i) 재시작·부팅·incarnation 경로

1. 세대: 행의 `born_generation`은 생성 때 한 번 찍힌다(`inflight/model.rs:1068`). 현 세대 판정은 `reconcile_gate.rs:29-34`다. 세대 할당에 실패하면 이전 행이 현 세대로 보일 수 있다. 이 과보류는 의도된 것이다(`restore_inflight.rs:128-134` 주석). 목표 술어는 현 세대를 None으로 두므로 이 성질을 바꾸지 않는다.
2. 부팅 순서(`restore_inflight.rs:98-229`): `receipt_settle::settle_boot`(`:104`) → 행마다 agy 보류 → receipt settle(`:143`) → routine → replay hydrate·보류(`:157-168`) → recovery intake skip → rebind 정리(`:190-196`) → receipt decode(`:209`) → `held_at_restore`(`:215`) → admitted(`:224`) → runtime kind skew 정리(`:257`) → wrapper 복구. 목표는 이 순서를 바꾸지 않는다. 부팅 owner가 보존한 행을 end pass가 이어받을 뿐이다.
3. `reconcile_done` 뒤에만 end pass가 돈다(`herdr_row_end.rs:116`). 그 전에 들어온 입력은 보류되고, `request_row_end`는 `reconcile_done`을 보지 않는다(`:153-181`). 목표에서는 부팅 owner가 아직 처리 중인 행을 입력 요청이 먼저 집지 않도록, 모양이 Pathless가 아니면 `request_row_end`도 `reconcile_done`을 확인한다. 부팅 settle과 end pass settle이 같은 행에서 겹치지 않게 하는 관문이다(모델은 `BootRestore` 뒤에만 `EndTick`이 열린다).
4. 실행 incarnation: install(`hosted_execution.rs:419`) → bind(`:460`) → retire(`:471`, 호출부 4.2). end pass E 경로는 `HostedState::Retired`면 retire 없이 settle한다(`herdr_row_end.rs:413`). 그 밖이면 `execution_end`로 끝을 확인한다(`:414-431`). 새 incarnation이 행의 nonce를 대체했으면 w2부터 E로 본다(2.1). 지금은 `execution_unpinned`로 영구 보존한다(`:541-549`).
5. 서버 교체: `herdr_server_replaced::ensure_server/demand`(`owner.rs:67`, `herdr_row_end.rs:171`), root-replaced preclaim(`queue_dispatch.rs:224-231`). 목표에서 바뀌지 않는다.

### (ii) 같은 상태를 소비하는 인접 경로

| 경로 | 좌표 | 재시작 뒤 이전 세대 Herdr 행에서 | 목표 영향 |
|---|---|---|---|
| Cancel (queue·HTTP force) | `services/queue/http_force.rs:53-87`, `herdr_terminate/http_force.rs` | scoped terminate. actor가 없으면 행을 건드리지 않음 | 없음. 남은 행은 end pass가 끝낸다 |
| Terminate (herdr terminate) | `herdr_terminate.rs:414-449,596-641` | 4.2와 같음 | 없음 |
| clear (사용자 /clear) | `commands/control.rs:585` → `commands/control/herdr_recovery.rs:21-` | root-replaced 복구가 "original inflight execution exists"로 보류(`herdr_recovery.rs:224-232`) | 목표가 행을 끝내면 이 보류도 풀린다. 코드 변경 없음 |
| clear teardown(owner 정리) | `commands/control/clear_teardown/execution_cleanup.rs:358-397,465-484` | identity·withheld disposition이 맞을 때만 archive | 없음. Replay archive(2.3)의 선례 |
| stop (`/stop`·`!stop`) | `turn_bridge/tmux_runtime/judged_stop.rs:59-109`, `codex_stop_delivery.rs:355-363` | Idle·pending이면 `host_owned`로 키를 보내지 않음 | 없음(설계상 host 소유) |
| 운영자 turn-lease release | `turn_lease.rs:195-299`, 정리 `:261,309` | actor CAS 필요 | 없음 |

### (iii) 입력 출처 분류

| 출처 | 쓰는 곳 | 소유자 | 지금 RestartedHerdrRow 보류 | 목표 |
|---|---|---|---|---|
| Discord 사람 메시지(intake) | `router/intake_gate/stale_turn.rs:308-330` | intake → mailbox 큐 | 보류(큐에 쌓음) | `is_some()`일 때만 보류 |
| 큐 자동 dequeue | `queue_dispatch.rs:224-242` → `owner.rs:45-75` | mailbox | 보류 + `request_row_end(Input)` | 같음. 요청이 실제 task를 띄움 |
| owner input(TUI-O·Herdr owner drive) | `owner_input.rs:768-782` | native owner | `Wait::Event` | 같은 술어 |
| REST·headless turn deliver | `health/turn_deliver.rs:131-142` | API | 외부 턴 보유로 거절 | 같은 술어 |
| presence supervisor 공지·주입 | `turn_presence/supervisor.rs:893-898` | presence | 보류 사유 보고 | 같은 술어 |
| 사람이 pane에 직접 친 입력 | `tui_direct_pending_start/*` → `foreign_recovery.rs:378-393` | pane(외부) | 보류 대상 아님. D면 위임만 | 같음(D 확장) |
| synthetic start 고아 회수 | `synthetic_orphan_reclaim.rs:71-79` | synthetic worker | 보류 대상 아님. D면 거절 | 같음 |
| 운영자 API(terminate·turn-lease·rebind) | `server/routes/herdr_terminate.rs:92`, `turn_lease.rs:195`, `manual_rebind/*` | 운영자 | 보류 대상 아님 | 같음 |

RestartedHerdrRow가 막는 것은 위 앞의 다섯 출처뿐이다. 앞의 셋(intake·dequeue·owner input)만 큐에 남아 다시 깨워진다. 목표에서는 보류 집합과 end pass 대상이 같아진다. 그래서 보류된 입력에는 항상 그 행을 끝낼 task가 있다.

## 6. 구현 조각안

리뷰 P1-1 지적("A에서 Restored까지 새 분류에 넣는다면 B의 삭제 계약도 함께 착지해야 한다")에 따라 **착지 단위를 하나**로 한다. 그 안을 커밋 3개(A·B·C)로 나눈다. 분류 함수가 Restored·Replay를 집기 시작하는 시점과 그 모양의 정리 계약이 같은 PR에 든다.

| 커밋 | 바꿀 prod 파일 | 추정 | 수용 시험(수정 전 RED → 후 GREEN) |
|---|---|---|---|
| A. 한 술어·공통 성공 경로·실행 대체 증거·Admitted/Receipt 재호출 | `recovery_engine/herdr_row_end/owner.rs`(분류 함수, 세 판정 교체), `recovery_engine/herdr_row_end.rs`(pass·request 필터, 비Pathless `reconcile_done`, `hosted_record` Superseded, `settle_row` 모양 재확인), 새 child `recovery_engine/herdr_row_end/restarted_shapes.rs`(dispatcher, 공통 성공 경로 settled → 락 해제 → `closed`, Admitted·Receipt·Planned) | 3 파일(새 1), 순 +170 | ① 이전 세대 kind 행, 부팅 settle을 actor 존재로 보존, owner 큐 선두가 `Wait::Event`로 잠든 상태 → actor 사라진 뒤 `row_end_pass` 1회 → 행 정리·공지 1회·추가 이벤트 없이 wake 1·dequeue 1. 지금은 pass가 집지 않아 RED. wake를 뺀 뮤턴트는 RED. ② 같은 조건의 receipt 행(①의 fixture 인자). ⑥ 행 실행 nonce n, 현 record가 새 nonce m → pass가 E로 정리. 지금은 `execution_unpinned` 보존이라 RED. ⑧ `turn_nonce` 없는 비Managed 이전 세대 Retired 행 → 정리 뒤 REST deliver(`turn_deliver.rs:131-142`)와 native owner 시작(`owner_input.rs:778-781`)이 진행 |
| B. Restored 정리 | `restarted_shapes.rs`(Restored → `settle_row`, `restored_reader_live` 관문), `inflight/clear_store/herdr_row_end.rs`(기대 모양 인자, 잠금 안 분류 대조, `output_path` 일치) | 2 파일, 순 +45 | ④ 이전 세대 `output_path` 행, 실행 Retired, actor·reader 없음 → 연속 두 pass에서 첫 pass가 공지 1회·정리·입력 진행, 둘째 pass는 아무것도 하지 않음. `:43` 거절을 되살린 뮤턴트는 RED(공지 2·행 남음). ⑤ 같은 episode reader가 살아 있으면 보존(공지 0) → reader 핸들이 멈추고 매핑만 남으면 다음 pass가 정리. 매핑만 보는 뮤턴트는 RED |
| C. Replay 끝 owner | `restarted_shapes.rs`(PG 영수증 재확인, debt ACK 또는 중립 공지, archive), `recovery_engine/restore_inflight/replay_hold.rs`(두 함수·enum 가시성), `recovery_engine/restore_inflight.rs:53`(`mod` 가시성 한 줄, FILES_AUTO 대상) | 3 파일, 순 +60 | ⑦ 부팅부터 재현: (a) 본문 없음, (b) 부팅에서 debt ACK 완료 → 부팅 뒤 pass가 archive, 이전 요청은 재실행되지 않음(복구 재시도가 `recovery_text/replay.rs:35-58`에서 거절), 새 입력은 진행. 지금은 RED(행이 남아 보류). `replay_keep` 뮤턴트(보존만)는 RED |

- 합계 추정은 prod 6파일(새 1 포함), 순 +275 내외다. 상한(30파일/+1500)과 정지선(+1200) 아래다. giant 파일 `restore_inflight.rs`에는 `mod` 가시성 한 줄만 바꾼다. `herdr_row_end.rs`(724줄)에는 필터·호출 교체만 하고 본체는 새 child에 둔다.
- 시험 줄 상한(바꾼 prod 줄 이하)을 지키려면 ①②, ④⑤, ⑦(a)(b)를 각각 fixture 인자로 묶는다. ③(재호출이 공지를 다시 보내지 않음)은 기존 admitted `delivered` 시험이 덮으면 새로 쓰지 않는다. 넘치면 구현 레그가 `TEST_OVER_PROD` 사유를 적는다.
- S1 겹침: `herdr_row_end_live.rs`·`completion_delivery.rs`·`turn_finalizer/episode.rs`의 계약은 바꾸지 않는다. Replay settle은 `deliver_held_debt_checked`를 통해 `completion_delivery.rs:112`의 `relay_captured_recovery_terminal_notice`를 읽기 전용으로 부른다. pass의 live 분기 선택(`live_target`)도 그대로다. `DESIGN_DELTA` 없음.
- 시험 위치: 기존 rig(`herdr_row_end_rig_tests.rs`, `herdr_row_end_settle_tests.rs`, `restore_inflight/replay_hold_tests.rs`)의 진입 경계(`row_end_pass`·`request_row_end`·`restore_inflight_turns`)에서 한다. 판정 함수 내부 단언은 쓰지 않는다.

## 7. 미해결

- [U1] 사례 행의 X 항(kind·receipt·HotSwap·rebind·replay 중 무엇이었는지). 운영 행을 읽지 않았다. 목표 설계는 어느 항이든 덮는다.
- [U2] (w2에서 닫음) Replay 끝 owner는 2.3이다.
- [U3] wrapper 복구가 Herdr `output_path` 행에 reader를 붙이는지, 붙인다면 mailbox token도 세우는지. `actor_absent`와 `restored_reader_live` 중 어느 관문이 그 reader를 잡든 두 관문이 함께 있으므로 정리하지 않는다. 구현 레그에서 rig로 어느 쪽인지 확인한다.
- [U4] Replay archive 뒤, 운영자 replay 결정 도구가 live inflight 행을 custody로 읽는지. 선례(clear teardown archive)와 PG `replay_preserved`로 보아 archive 사본으로 충분하다고 본다. 구현 레그에서 소비자를 grep으로 확인한다.
- [U5] TUI-O Aborted 공지(`herdr-stop:{turn_nonce}` 키)의 dedupe가 프로세스를 넘어 유지되는지. 넘지 않으면 "부팅에서 게시 → 보존 → 다음 프로세스 재호출"에서 2회가 될 수 있다. 구현 레그에서 deduper 저장소를 확인하고, 확인되기 전까지는 1회 보장을 같은 프로세스 안으로 한정한다.
- [D1] (w2에서 철회) 2.1의 두 nonce 정정 참조.
- 범위 밖: (b) HostTerminated CAS 거부 자체, (c) live row end 공지 반복(S1), 현 세대(재시작 전) Retired 행.
