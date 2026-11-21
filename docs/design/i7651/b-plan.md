# #7651 B 설계

기준은 main `92970bd7920f1052133f028684d839760da2f4d5`와 11-21 06:57 판정이다. Claude·Codex TUI의 구경로 게시, watcher 복구, inflight legacy 재생을 제거한다. sweep·정산·wake와 source discovery는 기존 위치 그대로 사용한다. 사용자에게 필요한 추가 동작은 재시작으로 끊긴 턴의 채널 안내 한 번과 대기 원본의 인계다. R1~R7을 착지 조건에 넣지 않는다. **구경로와의 동등성 증명은 DoD가 아니다.**

메인 확인 운영 로그에서 11-20 19:36 Claude 0행, 23:31 Claude 0행·Codex 1행을 읽었다. Codex 행은 `kept a held Herdr turn`으로 남았다. 21:30에는 agy `relay_recovery provider=antigravity`가 관측됐다. 배포는 보통 진행 중인 턴이 끝날 창을 기다린다. WSL에서 운영 로그를 직접 읽을 수 없어 메인 확인값을 인용했다. SSH로 재조회하지 않았다. 행 수는 빈도 근거이며 삭제 안전성은 아래 코드와 시험이 판단한다.

## 전수표

프로덕션 진입점 기준이다. main census의 lexical scan은 send site 275곳·108파일과 cutover gate 66곳·45파일을 확인했다. 이 수치에는 공용 transport·비본문도 들어가며 전수표 행 수와는 기준이 다르다. 같은 행의 여러 좌표는 하나의 호출 경계에 속하는 생산자와 sink다. 파일에 세 분기가 섞이면 각 도달 분기를 따로 센다. 본문 생산자는 `scripts/check_tui_o_writer_census.py:255–374`의 W 분류와 `BodySite`, 복구는 부팅·수동·주기·입력·handoff의 호출부와 re-export를 역추적했다. 표의 `지움`은 전용 코드 또는 Claude/Codex 분기이며, 공용 함수와 agy 호출이 있는 파일은 부분 삭제다. 아직 복구되지 않은 source와 소유권 `Unknown`에 구경로 쓰기 권한을 주지 않는다. 삭제를 적용할 코호트는 기존 O store로 재부팅이 되고 새 세션은 현행 fresh activation으로 시작함을 먼저 확인한다. Warm 최초 인수가 Legacy cursor를 기다리는 조건은 `src/services/tui_o/writer/host/first_init.rs:66–85`에 있다. 현행 TUI 경계로 아직 전환되지 않은 코호트는 먼저 기존 절차로 전환하며 R1 인수 동작으로 보완하지 않는다. 실행 허가의 기준은 routing과 지원되는 TUI 대상이며 provider 문자열만으로 headless를 자르지 않는다.

| 행 | 판정 | main 코드 좌표 | 삭제·유지 범위 |
|---|---|---|---|
| D01 | 지움 | `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:27–37,67–109`, `src/services/discord/tui_prompt_relay/source_tick.rs:51–111` | Claude idle tick의 legacy 본문 후보·본문 릴레이. source phase는 S01. |
| D02 | 지움 | `src/services/discord/tui_prompt_relay/codex_idle_rollout.rs:93–137,324,660–808`, `src/services/discord/tui_prompt_relay/source_tick.rs:115–190` | Codex idle rollout의 legacy 답변 replay/tail 및 본문 후보. 바인딩·회전은 S01. |
| D03 | 지움 | `src/services/discord/tmux_watcher.rs:1956–1979`, `src/services/discord/tmux_watcher/terminal_send.rs:228–252`, `src/services/discord/tmux_watcher/terminal_direct_fallback.rs:180–245,645`, `src/services/discord/tmux_watcher/terminal_direct_fallback_edit.rs:45–83` | W01 terminal short·direct·edit 본문과 새 메시지 fallback. |
| D04 | 지움 | `src/services/discord/tmux_watcher/terminal_long_chunks.rs:296–317,477–511`, `src/services/discord/tmux_watcher/task_response_authority.rs:414–434` | W01 long-chunk 및 task-response 본문 백업. |
| D05 | 지움 | `src/services/discord/tmux_watcher/streaming_status_tick.rs:784–805,850–885`, `src/services/discord/tmux_watcher/provider_output_guard.rs:97–118` | W02/W03 rollover 본문과 provider snapshot 본문. |
| D06 | 지움 | `src/services/discord/footer_view_reconciler/mod.rs:357–366,434–445,479–489` | 기존 답변을 재전송하는 W04 body refresh/fallback만. 카드·상태 유지. |
| D07 | 지움 | `src/services/discord/turn_bridge/stream_tick/output.rs:71–99,299–322,341`, `src/services/discord/turn_bridge/stream_tick/rollover_guard.rs:24–82`, `src/services/discord/turn_bridge/stream_loop/types.rs:85–121` | W14 및 W10e의 TUI preview·rollover·direct body 쓰기. |
| D08 | 지움 | `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:441,649,779–805,909–925`, `src/services/discord/turn_bridge/terminal_controller_cutover.rs:274–290,414–438,558`, `src/services/discord/turn_bridge/terminal_delivery.rs:163–182`, `src/services/discord/tui_prompt_relay/bridge_gateway.rs:3` | W10 short/controller/long/new-chunks의 TUI 본문만. O 결과·queue/dispatch completion 유지. |
| D09 | 지움 | `src/services/discord/turn_bridge/terminal_outcome_delivery/cancel_prompt_replace.rs:238–269,353–378`, `src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs:575–614` | W11/W13 TUI partial·foreign fallback 본문. 취소·목적지 판정과 비본문 유지. |
| D10 | 지움 | `src/services/discord/session_relay_sink.rs:621–712`, `src/services/discord/session_relay_sink/journal.rs:95–119`, `src/services/discord/session_relay_sink/short_controller.rs:84–111`, `src/services/discord/session_relay_sink/task_notification_context.rs:343–370,474–498`, `src/services/discord/task_notification_delivery/response_chunks.rs:132–153,330–351` | W20/W21의 TUI 본문 생성·수정·task fallback만. 범위/영수증 소비, 비TUI delivery 유지. |
| D11 | 지움 | `src/services/discord/runtime_bootstrap/recovery_flush.rs:55–82`, `src/services/discord/tui_prompt_relay/source_tick.rs:137`, `src/services/discord/watchers/lifecycle/restore_retry.rs:60–100,150–173`, `src/services/discord/watchers/lifecycle/restore.rs:21,672–703` | 부팅/rehydration legacy watcher 생성·retry·handled marker의 해당 호출. 세션 바인딩과 공용 cleanup 권한 유지. |
| D12 | 지움 | `src/services/discord/router/message_handler/intake_turn.rs:2020`, `src/services/discord/router/message_handler/watchdog.rs:128–167,204–241,300–407` | ingress·paused watcher 복구 중 본문 릴레이 생성만. 인계/정지/held wake 유지. |
| D13 | 지움 | `src/services/discord/turn_bridge/runtime_handoff_loop.rs:520–553`, `src/services/discord/turn_bridge/runtime_handoff_loop/watcher_handoff.rs:410–450` | 라이브 handoff에서 legacy 출력 watcher 재생성만. standby와 공유 사용자 세션 인계는 S16. |
| D14 | 지움 | `src/services/discord/health/recovery/watchdog_pass.rs:705–708`, `src/services/discord/health/watcher_respawn.rs:233–292,306–397,484–557,710–819`, `src/services/discord/health/watcher_respawn/live_bridge_guard.rs:29–56` | 부재 탐지·force-clean 후 legacy watcher 재부착 분기. O 입력 회수/clear를 동반하는 경로는 S08. |
| D15 | 지움 | `src/services/discord/health/relay_dead_reattach.rs:119–165`, `src/services/discord/relay_recovery/decision.rs:394–489`, `src/services/discord/relay_recovery/apply.rs:183–245`, `src/services/discord/relay_recovery_reattach_apply.rs:9`, `src/services/discord/recovery_engine/manual_rebind/mod.rs:913` | 자동/수동 `ReattachWatcher`의 legacy watcher 생성만. 동일 라우트의 dry-run·pin/queue 수리는 S10/S12. |
| D16 | 지움 | `src/services/discord/recovery_engine/restore_inflight.rs:257–423,790–900,1050–1079,1310–1351,1566–1605` | 부팅 시 wrapper/tail 본문 재생, legacy branch의 watcher 기동·fallback. terminal/custody 분기는 S14. |
| D17 | 지움 | `src/services/discord/recovery_engine/restore_inflight.rs:1774–1825,1959–2089`, `src/services/discord/recovery_paths/restart.rs:598–610,666`, `src/services/discord/recovery_engine/restore_inflight/output_paths.rs:31` | delivered anchor legacy repost 및 live pane의 legacy 재부착. 출력 경로 탐색의 공용 소비자는 그대로 사용한다. |
| D18 | 지움 | `src/services/discord/recovery_engine/restore_inflight.rs:2097–2437`, `src/services/discord/recovery_engine/legacy_restart_replay.rs:77–124,167`, `src/services/discord/recovery_engine/tmux_probe.rs:10` | 구형 reader→`spawn_turn_bridge`→generic auto-retry의 TUI legacy 재생, 이를 위한 분기/호출. #8295 소유 replay 판단/PG 관측은 S15. |
| D19 | 지움 | `src/services/discord/recovery_engine/completion_delivery.rs:301–341,441–530`, `src/services/discord/recovery_engine/terminal_text_idempotency.rs:530–663`, `src/services/discord/recovery_paths/controller_cutover.rs:198–209` | W30/W31/W32 legacy 본문 replacement, dedupe 호출, long fallback. O 증거·비본문 호출은 S13. |
| D20 | 지움 | `src/services/discord/health/recovery.rs:425–453,615–675`, `src/services/discord/recovery_engine/idle_captured_response.rs:36–109,349–385` | W33 stale-leak·idle captured response의 legacy 본문 send만. detection·terminal receipt·row clean은 S10/S13. |
| D21 | 지움 | `src/services/discord/tmux_restart_handoff.rs:277–331` | W34 restart report에 legacy partial body를 붙이는 분기. 명시 handoff 및 비본문 표식은 S16. |
| D22 | 지움 | `src/services/discord/health/relay_auto_heal/producer_resume_pass.rs:34–48,46–82`, `src/services/discord/health/relay_auto_heal.rs:366–480,565–708` | 읽지 못한 legacy 본문을 위해 watcher를 nudge하거나 다시 붙이는 redrive 분기. 호출 loop의 sweep와 정산은 S02/S09. |
| S01 | 남겨 둠(tui_o 공용) | `src/services/discord/tui_prompt_relay.rs:308–321`, `src/services/discord/tui_prompt_relay/source_tick.rs:27–49,57–103,120–184`, `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:8–50` | source discovery, native binding, rotation과 Claude/Codex `row_end_pass`의 cadence. body 루프를 지워도 tick을 지우지 않는다. |
| S02 | 남겨 둠(tui_o 공용) | `src/services/discord/runtime_bootstrap/framework_setup.rs:166`, `src/services/discord/health/recovery/stall_watchdog_task.rs:3–23`, `src/services/discord/health/recovery/watchdog_pass.rs:188–206,707–709`, `src/services/discord/health/relay_auto_heal/producer_resume_pass.rs:4–84` | watchdog 실행과 `sweep_leaked_inflight_rows` 도달, #7676 C2의 현행 주기·순서. |
| S03 | 남겨 둠(tui_o 공용) | `src/services/discord/relay_recovery.rs:72–73`, `src/services/discord/relay_recovery_leaked_row_sweep.rs:7–86`, `src/services/discord/inflight.rs:372–405` | 동일 파일·이름·provider 스캔·row capture 및 토큰 없는 회수 뒤 wake. |
| S04 | 남겨 둠(tui_o 공용) | `src/services/discord/tui_direct_pending_start/foreign_recovery.rs:349–394,396–513`, `src/services/discord/tui_prompt_relay/synthetic_orphan_reclaim.rs:63–117` | FOREIGN 정산의 Complete/Cancel·episode/death/identity fence 및 전담 종료 요청. |
| S05 | 남겨 둠(tui_o 공용) | `src/services/discord/queue_io/input_owner/closed.rs:11–34`, `src/services/discord/queue_io/backstop.rs:692–705`, `src/services/discord/queue_io/input_owner/drive.rs:88–128`, `src/services/discord/turn_finalizer/episode.rs:129–143` | 해당 종료의 단 한 번 wake 책임, 토큰 해제 완료 이벤트, V2 재평가 및 single-flight. |
| S06 | 남겨 둠(tui_o 공용) | `src/services/discord/recovery_engine/herdr_row_end.rs:34–72,112–139,153–175,214–282`, `src/services/discord/recovery_engine/herdr_row_end/owner.rs:28–74,104–140`, `src/services/discord/queue_dispatch.rs:218–243`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:797–806` | 재시작 Herdr row의 전담 정산, 기존 identity/epoch guard, 기존 dequeue gate. 공유 기능의 소스·이름을 옮기지 않는다. |
| S07 | 남겨 둠(tui_o 공용) | `src/services/discord/health/recovery/carried_turn_close.rs:38–58,225–289,307–357` | O 전용 native end+O 전달 증거로 이월 행 정산, 수신 불명 시 기존 hold. |
| S08 | 남겨 둠(tui_o 공용) | `src/services/discord/health/watcher_respawn.rs:428–481,834–872`, `src/services/discord/health/watcher_respawn/live_bridge_guard.rs:29`, `src/services/discord/health/recovery/inflight_repair.rs:73–112,118–149` | 책임이 없어진 bridge/token/relay owner 회수와 입력 진척. watcher 재생성만 D14. |
| S09 | 남겨 둠(tui_o 공용) | `src/services/discord/health/relay_auto_heal.rs:339–364,762–783`, `src/services/discord/relay_recovery/apply.rs:119–177,246–334` | exact orphan-token finish, 큐 drain 및 공용 회수 verdict. |
| S10 | 남겨 둠(tui_o 공용) | `src/services/discord/relay_recovery.rs:249–326,330–356,422`, `src/services/discord/relay_recovery/idle_tmux.rs:1`, `src/services/discord/destructive_cancel_gate.rs:369`, `src/services/discord/router/intake_gate/stale_turn.rs:41–50`, `src/server/routes/health_api/session_repair.rs:97–103`, `src/server/mod.rs:530` | 수리/관측, Axis-B 및 idle/tail 증거를 쓰는 기존 안전 가드. |
| S11 | 남겨 둠(tui_o 공용) | `src/services/discord/inflight.rs:626`, `src/services/discord/inflight/circuit_removal.rs:27–35`, `src/services/discord/relay_recovery_circuit_evidence.rs:1`, `src/services/discord/relay_recovery_auto_heal_apply.rs:68,235` | durable circuit evidence/기존 증거 소비. legacy-only branch가 사라져도 공용 ledger/경계는 소비자가 있는 동안 유지. |
| S12 | 남겨 둠(tui_o 공용) | `src/server/routes/domains/runtime.rs:112–113`, `src/server/routes/health_api.rs:913–964,1081–1111`, `src/services/discord/health/recovery/channel_runtime.rs:369`, `src/services/discord/health.rs:728` | 수동 API의 shared verdict/행 정산·기존 권한. 지운 legacy watcher 액션을 apply로 성공 처리하지 않는다. |
| S13 | 남겨 둠(tui_o 공용) | `src/services/discord/recovery_engine/completion_delivery.rs:112–171,265–298,409–438,593–628`, `src/services/discord/outbound/completed_turn_ledger/o_pending.rs:148`, `src/services/discord/recovery_engine/idle_captured_response.rs:236–347` | exact delivery 증거/receipt/placeholder 비본문, O가 소유한 분기의 completion. 얇은 발송 포트도 공용. |
| S14 | 남겨 둠(tui_o 공용) | `src/services/discord/recovery_engine/restore_inflight.rs:67–117,133–152,210–255`, `src/services/discord/receipt_settle.rs:147–220`, `src/services/discord/recovery_engine/herdr_admitted_restart.rs:137` | 부팅 receipt/보류/전담 종료와 stop pin. 정상 scope와 구형/unknown 행 가드 유지. 기존 복구 결함 보완은 #8295. |
| S15 | 남겨 둠(tui_o 공용) | `src/services/discord/recovery_engine/restore_inflight.rs:155–165`, `src/services/discord/recovery_engine/restore_inflight/replay_hold.rs:149`, `src/services/discord/recovery_engine/restore_inflight/replay_decision.rs:1`, `src/services/discord/health/relay_auto_heal/producer_resume_pass.rs:14–18,63–67`, `src/services/discord/recovery_engine/producer_resume.rs:117,177–191,214–245` | 현행 replay hold/PG 판단, pending receipt의 producer resume 가드. #8295에서 다룰 경계를 여기에 옮기거나 확장하지 않는다. |
| S16 | 남겨 둠(tui_o 공용) | `src/services/discord/turn_bridge/runtime_handoff_loop.rs:469`, `src/services/discord/turn_bridge/runtime_handoff_loop/watcher_handoff.rs:331`, `src/services/discord/standby_relay.rs:109`, `src/services/discord/tmux_restart_handoff.rs:445,485`, `src/services/discord/restart_report.rs:169–189,450–465` | 혼합 standby/handoff 수명·명시적 `/restart` 완료 보고·기존 소유 가드. standby 복구 재설계는 #8295. |
| S17 | 남겨 둠(tui_o 공용) | `src/services/discord/runtime_bootstrap/recovery_flush.rs:40–53,74–79,89–122,135–141`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:26–52,65–71,155–210,218–273` | catch-up, snapshot/dispatch marker/consent, 조건부 merge retry와 대기열 kickoff. |
| S18 | 남겨 둠(tui_o 공용) | `src/services/discord/runtime_bootstrap/shutdown.rs:114–181`, `src/services/discord/runtime_bootstrap/gateway_lease.rs:627–664`, `src/services/discord/runtime_bootstrap.rs:35–42` | 기존 배수·저장 순서와 gateway fence. hot swap, process generation 및 재조정 완료 경계. |
| S19 | 남겨 둠(tui_o 공용) | `src/services/turn_orchestrator/pending_queue_persistence/queued_recovery.rs:91`, `src/services/turn_orchestrator/queued_head/v2/actor.rs:18`, `src/services/discord/queue_io/input_owner/recovery.rs:8–42`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:756–781` | V2 파일 재채택, known-unsent만 requeue, durable receipt 및 held front 보존. |
| S20 | 남겨 둠(tui_o 공용) | `src/services/discord/runtime_bootstrap.rs:340–343,414,457`, `src/services/discord/runtime_bootstrap/o_writer_host.rs:343–353,399–408,817–850`, `src/services/discord/outbound/o_writer_legacy.rs:47–131`, `src/services/tui_o/writer/host.rs:609` | O host·readiness와 현행 `LegacyView` 읽기 관측. 기존 writer의 정합성과 R1의 새 동작은 별개다. |
| S21 | 남겨 둠(tui_o 공용) | `src/services/discord/turn_bridge/headless_delivery.rs:185,319,508,596`, `src/services/discord/turn_bridge/headless_delivery/intake_outbox_argument.rs:3`, `src/services/discord/outbound/manual_delivery/resolved.rs:146–182`, `src/services/message_outbox/body_census.rs:7–34` | 검증된 headless/수동/outbox 전달 및 입력 provenance. 경계가 섞인 호출에서 TUI body만 제거한다. |
| S22 | 남겨 둠(tui_o 공용) | `src/services/discord/outbound/o_writer_io.rs:23–39,110,197`, `src/services/discord/outbound/turn_output_controller.rs:662,692,773`, `src/services/discord/outbound/transport.rs:22–42`, `src/services/discord/gateway.rs:599–625` | Discord transport·shared lease·편집/nonce. 몸통을 없앤 뒤에도 O와 비본문이 쓰는 포트. |
| S23 | 남겨 둠(tui_o 공용) | `src/services/discord/tmux_watcher/no_result_exits.rs:615–622,706–715`, `src/services/discord/tmux_watcher/pre_emit_guard.rs:352`, `src/services/discord/placeholder_controller.rs:205`, `src/services/discord/footer_view_reconciler/mod.rs:89,131,137,479` | no-result, placeholder와 footer의 비본문 상태 갱신. |
| S24 | 남겨 둠(tui_o 공용) | `src/services/discord/turn_bridge/terminal_outcome_delivery/foreign_terminal_handoff.rs:10,119,271–315,396–459`, `src/services/discord/router/message_handler/headless_turn.rs:1010`, `src/server/worker_registry/registry.rs:394` | durable terminal custody, headless watcher 소비자와 session relay supervisor. legacy TUI 본문 효과만 D09/D10/D12에서 차단한다. |
| A01 | 남겨 둠(agy, #8274 뒤 지움) | `src/services/provider/registry.rs:178–184,197`, `src/services/discord/turn_bridge/stream_tick/output.rs:299–322`, `src/services/discord/turn_bridge/terminal_outcome_delivery.rs:649,779–805` | agy에서 도달하는 legacy live/terminal 전용 분기. provider 자체와 headless는 유지. |
| A02 | 남겨 둠(agy, #8274 뒤 지움) | `src/services/discord/tmux_watcher.rs:1956–1979`, `src/services/discord/tmux_watcher/streaming_status_tick.rs:784–885`, `src/services/discord/tmux_watcher/terminal_direct_fallback.rs:180–245,645`, `src/services/discord/session_relay_sink.rs:621–712` | agy watcher·sink 및 그 본문 helper의 마지막 agy 호출. |
| A03 | 남겨 둠(agy, #8274 뒤 지움) | `src/services/discord/runtime_bootstrap/recovery_flush.rs:55–82`, `src/services/discord/watchers/lifecycle/restore_retry.rs:83–105`, `src/services/discord/watchers/lifecycle/restore.rs:672`, `src/services/discord/health/watcher_respawn.rs:484–557` | agy 부팅 watcher restore/retry, force-clean 뒤 재부착. |
| A04 | 남겨 둠(agy, #8274 뒤 지움) | `src/services/discord/health/relay_dead_reattach.rs:119–165`, `src/services/discord/relay_recovery/apply.rs:183–245`, `src/services/discord/relay_recovery_reattach_apply.rs:9`, `src/services/discord/recovery_engine/manual_rebind/mod.rs:913` | agy 전용 `ReattachWatcher` 적용과 legacy watcher spawn. |
| A05 | 남겨 둠(agy, #8274 뒤 지움) | `src/services/discord/runtime_bootstrap/queued_recovery.rs:210`, `src/services/discord/recovery_engine/restore_inflight.rs:305–423,1774–1825,1959–2089,2385–2420` | agy 기존 restart replay/watcher/reader. 공용 restore entry는 유지하며 agy legacy arm만 나중에 삭제한다. |
| A06 | 남겨 둠(agy, #8274 뒤 지움) | `src/services/discord/recovery_engine/completion_delivery.rs:301–341,441–530`, `src/services/discord/recovery_paths/restart.rs:598–610,666`, `src/services/discord/health/recovery.rs:615–675`, `src/services/discord/tmux_restart_handoff.rs:306–330` | agy legacy body·repost·stale-leak·partial 전달의 마지막 호출. |

`#7753 C2`의 실제 sweep 시험, O 소유 재시작 Herdr의 실제 정산/다음 입력 시험 등 앞 레그의 Rust 4건이 이 분류를 지지한다. 회수만 없앤 모델의 영구 정지는 sweep 제거 방지 대조군이다. 후속 정리 후보는 `relay_recovery` 아래 shared sweep의 모듈 이름과 배치 하나이며 이번에는 변경하지 않는다.

## 재시작·부팅

| 시점 | main 코드 좌표 | 삭제 뒤의 담당 경로 |
|---|---|---|
| dcserver 시작 | `src/cli/dcserver.rs:1269,1362`, `src/server/mod.rs:351`, `src/services/discord/runtime_bootstrap.rs:165,340–343,414`, `src/services/discord/runtime_bootstrap/gateway_runtime.rs:63` | worker와 gateway의 기존 부팅 경계를 유지한다. gateway writer는 기존 채널 권한으로 발송하고, worker는 로컬 입력·행 복구를 계속한다. |
| 배수와 재시작 경계 | `src/services/discord/runtime_bootstrap/shutdown.rs:114–181`, `src/services/discord/runtime_bootstrap/gateway_lease.rs:627–664`, `src/services/discord/runtime_store.rs:512–516` | 배수의 전·후 snapshot과 세대 fence를 유지한다. O의 현재 turn identity와 세대를 내구화해 끊긴 턴의 근거를 남긴다. |
| 채널 재부착 | `src/services/discord/runtime_bootstrap/recovery_flush.rs:70–82`, `src/services/discord/tui_prompt_relay/source_tick.rs:27–49,51–99,115–185`, `src/services/tui_o/writer/host.rs:609`, `src/services/tui_o/writer/actor.rs:378–400` | native source discovery·binding/rotation 및 기존 O writer 부팅으로 받는다. legacy watcher 등록은 삭제 대상이다. 소스가 Unknown이면 기존 hold/재관측으로 간다. |
| 진행 중 턴 판정 | `src/services/tui_o/shadow/seal.rs:89–128`, `src/services/tui_o/writer/input_facts.rs:486–492`, `src/services/discord/recovery_engine/restore_inflight.rs:103–117,210–225`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:185–210` | O의 source/turn 증거에서 직전 세대의 Open을 채택한다. 정확한 과거 inflight 표식은 기존 durable 증거 보조로 읽는다. legacy 본문/offset으로 재생을 시작하지 않는다. |
| 대기 원본 복원 | `src/services/discord/runtime_bootstrap/recovery_flush.rs:40–53,95–101,119–141`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:47–52,65–71,155–177,218–273`, `src/services/discord/queue_io/input_owner/recovery.rs:8–42` | catch-up/queue snapshot/dispatch marker의 기존 merge, V2 adopt 및 receipt 복원 순서를 유지한다. 부팅 작업이 실패한 채널은 기존 held retry가 소유한다. |
| 첫 dequeue | `src/services/discord/runtime_bootstrap/o_writer_host.rs:399–408`, `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:770–781`, `src/services/discord/queue_dispatch.rs:218–243`, `src/services/discord/relay_recovery_leaked_row_sweep.rs:61–67` | boot reconcile·writer ready와 공용 정산 wake가 같은 V2 drive를 깨운다. 살아 있는 이전 턴/행은 기존 가드에서 보류하며 sweep이 종료를 처리한다. |

## 공백 채우기

### 끊김 안내 1회

1. **직전 세대의 근거.** O가 이미 관측하는 Open/Closed 경계에 최소 식별 정보를 남긴다. `(provider, Discord channel, source incarnation, native turn id 또는 exact episode nonce, born generation)`을 사용한다. 흡수 입력/재부착/부팅 횟수는 새 episode를 만들지 않는다. shutdown capture와 부팅 read-only census가 이미 종결을 증명한 행을 제외한다. 부팅에서는 `queued_recovery.rs:197`의 구세대 무효화 전에 정확한 이전 행의 키를 확보한다. 자체 증거를 가진 native turn은 O durable spool/ledger를 이어받아 boot replay가 새 세대 기록을 섞기 전에 판정한다. 계획 재시작에서는 배수 경계 전에 닫힌 턴을 제외하며, 사고 재시작에서는 직전 세대의 내구화된 마지막 상태를 기준으로 한다. 이전 O 기록이 없는 구형 행은 `provider/channel·session/source·turn identity·generation`이 일치할 때만 읽기 호환한다. 오래되었거나 `started_at`만 있는 행, forward schema, tokenless synthetic rebind, ownership Unknown 단독으로 끊김을 합성하지 않는다. 경계 뒤 provider가 끝날 수 있으므로 안내는 AgentDesk의 출력이 재시작으로 끊겼다는 사실만 알린다.
2. **발송 책임.** 새 작은 O writer child의 채널별 restart notice 작업 한 곳만 `재시작으로 턴이 끊겼습니다.`를 발송한다. boot, reconnect, watchdog은 필요한 경우 동일 키 intent를 제공한다. `src/services/discord/recovery_engine/herdr_row_end/outcome.rs:53–60`의 이미 O가 처리한 terminal은 그대로 silent다. 기존 공유 경로가 *같은 O episode의 재시작 끊김 안내*를 직접 게시할 경우에만 새 단일 intent/확인 포트를 호출한다. `herdr_row_end`, sweep, foreign 정산, receipt, 행 clear와 wake의 위치·순서는 유지한다. 다른 사유의 종료·취소, `/restart` 명령 완료 보고, agy는 각자의 기존 경계를 사용한다. 동일 episode를 알아보는 기존 report/anchor가 이미 같은 안내를 게시했다는 정확한 채널·작성자·내용 증거는 같은 intent의 완료로 읽는다. 정산만으로 안내가 발송됐다고 기록하지 않는다.
3. **중복 근거.** `src/services/tui_o/store/ledger.rs:655–669`의 동기 append와 `src/services/tui_o/writer/deliver.rs:595–641,831–906`의 채널 송신 lease, `Prepared`/`Posted` 확인을 재사용한다. 동일 lease 아래 `Attempted` 증거를 내구화한 뒤 처음 POST를 poll한다. 그 증거가 없는 `Prepared`와 보낸 뒤 답이 없는 `Attempted`는 재부팅 시 구분한다. 내구 키와 발송 의도를 원문 없이 보존하며 재기동/경합은 같은 intent를 읽는다. 기존 exact placeholder가 살아 있고 그 turn 소유임을 증명하면 같은 Discord message id의 고정 문구 편집을 재시도한다. 다른 카드나 이미 다른 턴이 사용하는 카드는 건드리지 않는다. fallback이 필요한 경우 동일 intent에 단일 작성자·메시지 표식을 먼저 저장하고, 기존 O POST 확인 루프에서 (a) 기존 생성의 ACK 또는 bot/채널/고유 표식이 일치하는 원격 기록은 `Posted`, (b) 발송이 어긋나지 않았다는 확정적인 미수락은 동일 intent의 재시도, (c) timeout, 빈 history, 권한 손실, 늦은 응답·상충은 `Uncertain`으로 보존한다. 재부팅 후 `Prepared`를 새 송신 허가로 쓰지 않는다. `src/services/tui_o/writer/confirm.rs:24–32,116–156`의 `NotFound`는 독자적으로 두 번째 CREATE의 권한이 아니다. TTL·본문 일치·process-local 캐시를 정확히 1회의 근거로 쓰지 않는다. ACK 뒤 crash에도 원격 효과가 남으므로 먼저 같은 메시지를 확인한다. 확정적 원격 증거가 영구히 사라지면 중복을 막기 위해 intent를 미정으로 보존한다. 이 경우 안내의 최종 전달은 보장할 수 없으며, 대기 원본의 재개는 별도 기존 이벤트에 따른다. 실제 transport 시험에서 미수락/늦은 성공을 구분하는 근거를 검증한 뒤 삭제를 허용한다.

### 대기 입력 유실 0

- 기존 `처음 수락 → V2 원본 fsync → claim/attempt → receipt → settle` 계약을 유지한다. `src/services/turn_orchestrator/queued_head/v2/actor.rs:18`의 실제 파일 재채택을 사용한다. V2의 native/Discord id·원문·순서·reply/첨부 참조를 보존한다. 부팅에서 동일 Discord id를 본 복구 snapshot, dispatch marker, catch-up은 새 원본을 만들지 않는다. 구형 snapshot의 merge 저장이 실패할 때 input과 marker의 custody는 `queued_recovery.rs:155–177,238–265`의 기존 held retry가 유지한다. 복원 단계에서 파싱할 수 없는 레코드를 빈 큐로 쓰지 않는다.
- `src/services/discord/queue_io/input_owner/recovery.rs:8–42`의 known-unsent만 같은 원본으로 다시 대기시킨다. 이미 제출/확인 중인 attempt는 기존 history/receipt로 정산하거나 미정으로 보존한다. ACK가 없다는 이유로 재전송하지 않는다. 공용 sweep과 Herdr row-end의 기존 identity guard가 old row를 닫고, token release 또는 `closed`가 큐를 다시 평가한다. 완료 후 recheck와 writer-ready가 기존 drive로 합류한다. 새 재시도 타이머와 임의의 행 삭제·provider 종료를 만들지 않는다.
- notice 확인이 늦어도 일반 V2 인계가 post 결과를 기다릴 새 barrier를 만들지 않는다. main에서 이미 receipt/terminal 증거를 요구하는 공유 정산 경로의 권한은 그대로 적용한다. 경쟁 중인 successor의 행·token·원본을 건드리지 않는다. 공정한 writer/queue 동작과 기존 host idle/회수 증거가 생긴 뒤 다음 입력이 결국 시작한다. 예를 들어 기존 same-session 미검증 이월 행(#7828)은 `src/services/discord/health/recovery/carried_turn_close.rs:236–240`에서 계속 보류된다. 이 경우를 위해 새 강제 해제 권한을 도입하지 않는다.

## agy

옵션 (a). #8274의 agy → tui_o 착지 및 해당 provider writer readiness 확인을 agy legacy 분기 삭제(B4)의 배포 선행 조건으로 둔다. Claude·Codex 삭제 조각 B0~B3은 이 조건과 독립이다. A01–A06은 그때까지 agy 실행을 그대로 받으며 이후 해당 legacy 분기만 제거한다. D 행 중 agy와 코드를 공유하는 곳은 먼저 범위와 호출자만 검토하고, 착지 전 범용 함수를 삭제하지 않는다. 원격 구현 ref는 `refs/review/i8274-design-1` (`5eb6a8cb65fa365d1408e21fe68363018c85c115`)과 `refs/review/i8274-s1-wip` (`91151eb3b84048e1c4263f9ec13b2fc328c9ea15`)를 확인했다. 범위 가드는 실제 `ProviderKind::Antigravity`/채널 라우팅과 writer 증거를 사용한다. agy가 아닌 비TUI·headless·수동 메시지까지 자르는 provider-wide 가드는 두지 않는다.

## R1

`refs/review/i7651-r1-fix5`의 `93aef932a9c6a76645ec36ced833e791a2ff7d61`는 xh CLEAN이고 앞 레그의 판정은 `r1_inert=no`다. `src/services/discord/runtime_bootstrap/o_writer_host.rs:504–515` → `src/services/tui_o/writer/host.rs:609` → `src/services/tui_o/writer/host/first_init.rs:37–199`은 운영 late adoption을 실행한다. R1은 legacy delivery frontier에 따른 `late_adopt`·`deferred`·`adoption` 시작점을 바꾼다. 기존 부팅 source 발견과 현재 writer를 그대로 쓰는 B에는 이 동작이 필요 없다. **R1은 B에서 제외하고 착지하지 않는다.** writer adoption 보류·거절·wake, 부팅 정산의 재설계, replay hold, standby 복구의 검토는 #8295에서 한다.

## 의존 재분류

| 이슈 | 분류 | 근거 |
|---|---|---|
| #7753 | 무관 | 이슈 본문은 #7676/F, 11-07 결정은 #7896을 착수 선행으로 둔다. 수동 legacy reattach는 삭제 준비 목록으로만 넘긴다. 이미 착지한 C2의 sweep 회수 후 wake는 유지한다. [#7753](https://github.com/itismyfield/AgentDesk/issues/7753) |
| #7323 | 삭제 착지 뒤 바로 풀림 | dispatch 전달 원장 통합의 #7651 쪽 조건은 출력 경로 정리의 착지다. #7676 선행은 별도 조건이다. [사용자 결정](https://github.com/itismyfield/AgentDesk/issues/7323#issuecomment-6626227223) |
| #7819 | 삭제 착지 뒤 바로 풀림 | 사라질 출력 시험에 중복 작업을 피하기 위해 남은 시험 양의 재산정을 미룬 결정이다. 재산정 기준은 실제 삭제 범위다. [사용자 결정](https://github.com/itismyfield/AgentDesk/issues/7819#issuecomment-6759374965) |

## 크기

예상 prod 파일 수와 순 코드 줄 수다. 혼합 파일은 같은 파일의 분기 삭제도 1파일로 센다. 설계 문서 커밋의 prod 실측과 구현 조각의 추정을 구분한다. 조각별 파일 수·순증은 공식 `pr_cap_prod.py`로 실측해 리뷰 전 확인한다. 스크립트의 `FILES, NET = 30, 1500`과 브리프 정지선 +1200을 사용하며 삭제 줄은 순증에서 차감된다.

| 조각 | 전수표 | prod 파일 추정 | 예상 추가 / 삭제 / 순증 |
|---|---|---:|---:|
| B0 안내·입력 진척 증명 | S06, S17–20, S22의 필요한 호출 추가·교체 및 새 writer child | 8 | +220 / −40 / +180 |
| B1 legacy live/body 제거 | D01–D10 | 29 | +90 / −570 / −480 |
| B2 watcher 복구 제거 | D11–D15, D22 | 20 | +75 / −475 / −400 |
| B3 inflight legacy 재생 제거 | D16–D21 | 16 | +75 / −1,255 / −1,180 |
| B4 agy 분기와 마지막 legacy-only 호출 제거 | A01–A06, #8274 착지 뒤 | 27 | +50 / −650 / −600 |

총 예상 순증 **−2,480**. 출발점은 B0의 호환·검증이다. B0~B3은 #8274와 독립이며 B4만 #8274 착지 뒤다. 1,000줄이 넘는 파일은 기존 블록 제거와 호출 교체만 한다. 지우는 함수·import·필드의 마지막 참조는 `rg`와 lint no-growth로 확인한다. 공용 helper를 삭제 조각의 순감에 포함하지 않는다. 구현 diff가 예산을 넘으면 조각을 다시 나눈 뒤 리뷰한다.

## DoD 개정안

1. Claude·Codex에서 전수표 D 행의 legacy 본문 게시, watcher 복구와 inflight legacy 재생이 제거되고, #8274 착지와 readiness 확인 후 A 행의 agy legacy 분기가 제거된다.
2. 공용 sweep, FOREIGN 정산, `closed` wake, Herdr row-end와 주기 호출은 현재 모듈에서 계속 작동한다. 현행 receipt·source binding·headless 전송의 호출 경계도 유지한다.
3. 검증된 재시작 중단 episode마다 정확한 채널의 끊김 안내는 **최대 1회**다(모든 상태). 원격 결과를 확정할 수 있으면 정확히 1회 보이고 `Posted`로 닫힌다. Prepared 뒤 history가 끝내 증명하지 못한 경우(`NotFound`·`Ambiguous`·`Unresolved`)는 미정으로 닫고 두 번째 CREATE를 하지 않으며, 그 안내는 보이지 않을 수 있다. 대기 입력의 인계·진행은 안내 결과를 기다리지 않는다. terminal 직전·직후, 두 번의 boot, 경쟁하는 복구 트리거, Discord 성공 뒤 로컬 ACK 손실, Prepared 뒤 POST 전 중단, history miss 뒤 늦은 성공을 시험한다.
4. 재시작 전에 수락한 대기 원본은 동일 id·내용·순서를 유지한다. 실제 provider 전달 또는 기존 receipt 정산까지 V2 custody가 이어지며, 회수 가능한 이전 행의 정산 뒤 다음 입력이 한번 시작한다.
5. `I7651Restart`에서 삭제만 한 모델은 안내 불변식 RED, 수정 모델은 두 불변식과 진행성 GREEN이다. Rust 구현 리뷰에서는 실제 entrypoint에서 각 원인 제거 뮤턴트의 RED와 원복 바이트 일치를 남긴다.
6. 구경로와의 동등성 증명은 DoD가 아니다. R1~R7과 #8295의 정교한 재시작 복구는 이번 착지 조건에서 제외한다.

## pro 첫 리뷰 기준 문장

> #7651 B를 검토해 주세요. 지우는 분기가 legacy 게시·watcher 복구·inflight legacy 재생에 한정되고 #8274 착지 전 agy와 기존 tui_o 공용 sweep/정산/wake가 보존되는지 확인해 주세요. 재시작 중단 episode가 정확한 채널에서 한 번만 안내되고, ACK 유실·재부팅·successor 경합에도 대기 입력 원본이 유실 없이 인계되어 다음 턴이 결국 시작하는지를 기준으로 판정해 주세요. **구경로와의 동등성 증명은 DoD도 리뷰 판정 기준도 아닙니다.** R1 및 #8295 동작은 이번 착지 기준에서 제외합니다.

## 상태 차트

기존 `docs/statecharts/input-single-path/InputOwnership.tla`의 completion-wake 전이와 첫 레그의 `CBSweepInit`·`CBSweepNext`를 재사용했다. 새 `I7651Restart`는 독립 공용 sweep을 그대로 두고 안내 intent/발송/확인과 durable 대기 원본의 모델을 더한다. 동일 채널에 0/1/2개의 중단 episode, 중복 부팅 1회, 응답 없는 원격 발송과 확정 미수락을 검사한다. B0 반영으로 안내 규칙을 `NoticeAtMostOnce`(모든 상태)와 `ClosedNoticeWasDelivered`(닫힌 안내는 전달됨)로 나눴다. `GiveUp`은 history miss를 종결 미정(`unknown`)으로 두는 B0 처분이며 새 CREATE 권한이 아니다. 완료는 `NoticesEventuallySettled`(전달 또는 미정 종결)로 검사하고, `NoticesEventuallyClosed`(전달로 종결)는 원격 확인이 결정적인 cfg에서만 GREEN이다.

| 경우 | 안내 최대 1회 | 닫힌 안내는 전달됨 | 대기 입력 유실 0 | 다음 입력 처리 | 안내 종결(전달·미정) | 전달로 종결 |
|---|---|---|---|---|---|---|
| 수정 전, legacy만 삭제하고 공유 sweep 유지 (`I7651RestartDeleteOnly`) | GREEN | **RED** | GREEN | GREEN | GREEN | 검사 안 함 |
| 수정 후 (`I7651RestartAfter`) | **GREEN** | **GREEN** | **GREEN** | **GREEN** | **GREEN** | 검사 안 함 |
| 원격 확인 결정적 (`I7651RestartDecisive`) | GREEN | GREEN | GREEN | GREEN | GREEN | **GREEN** |
| 원격 확인 영구 불가 (`I7651RestartUnknownForever`) | GREEN | GREEN | GREEN | GREEN | GREEN | **RED**(계약상 기대) |
| history miss 뒤 재생성 (`I7651RestartNotFoundRetry`) | **RED** | **RED** | GREEN | GREEN | RED(2부 상한의 산물) | 검사 안 함 |
| ACK 손실 뒤 확인 없이 재전송 (`I7651RestartBlindRetry`) | **RED** | **RED** | GREEN | GREEN | GREEN | 검사 안 함 |
| 공유 sweep 삭제 과거 반례 (`I7651RestartNoSweep`) | GREEN | GREEN | GREEN. 원본은 대기에 남는다. | **RED** | GREEN | 검사 안 함 |
| 확인 없이 대기 원본 폐기 (`I7651RestartDropInput`) | GREEN | GREEN | **RED** | **RED** | GREEN | 검사 안 함 |

TLC 1.7.4, 8 cfg의 기대 일치를 `scripts/ci/statecharts_tlc.py`로 확인한다. 종료를 위한 native idle, 공유 회수, 큐 drive와 원격 확인 시도(`Resolve` 또는 `GiveUp`)가 공정하게 수행되는 범위의 검사다. 모델의 `Resolve`는 확정 효과/미수락을 아는 포트다. Discord history가 언제 완전해지는지, 실제 capture가 모든 중단을 증명하는지, PG·파일 오류 후 복구를 TLC가 대신 검증하지 않는다. Rust 구현의 관련 실측과 삭제 조각의 gate는 DoD 3–5에서 확인한다.
