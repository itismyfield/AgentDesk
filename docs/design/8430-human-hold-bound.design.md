# #8430 사람 입력 보류 상한 설계 w2 (T0)

기준 코드 `a53f299560`. 이번 레그는 설계만 한다(prod 코드 변경 0, cargo 0).
상태 차트: [`HumanHoldBound.tla`](../statecharts/input-single-path/HumanHoldBound.tla), [보고서](../statecharts/input-single-path/human-hold-bound.report.md).
w1 이어서 범위 추가: i8343(`!clear` 직후 둘째 입력 버림), i8372(루틴 스레드 첫 답장 거절).
w2: pro 리뷰 review-design-1 반영. P1-1 Codex 화면 근거(§4), P2-1 모델 R1 정합(§6.4, 상태 차트), P2-2 headless 행 종료 근거(§6.5 R5b), P2-3 행 전용 정산·깨움 계약(§6.1, §6.5 R5c), P2-4 non-Managed 행·incarnation(§2.2 C37·C38, §6.5 R5a), P2-5 묶음 전체 특정(§6.4 R3).

약어: **OI** `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs`, **OB** `src/services/discord/router/message_handler/intake_turn/owned_busy.rs`, **IO** `src/services/discord/queue_io/input_owner/`, **QH** `src/services/turn_orchestrator/queued_head/`, **CT** `src/services/claude_tui/`, **D** `src/services/discord/`, **TO** `src/services/turn_orchestrator.rs`.

## 1. 11:48 사고를 코드로 다시 짠 결과

채널 `…0183`(Claude, Herdr 호스트). 보류를 붙잡은 근거는 하나다. 메일박스 `cancel_token`, 곧 `has_active_turn()`(TO:1810-1811, `cancel_token.is_some()`)이다. 이 토큰을 회수할 경로는 넷 있었다. 각자 다른 이유로 안 돌았거나, Herdr에서는 아무 일도 하지 않는 경로였다. 결국 02:57:42Z에 운영자가 `herdr terminate`(내부에서 `submit_terminal(HostTerminated)`)를 실행해서야 풀렸다.

| 경로 | file:line | 왜 안 풀렸나 | 로그 근거(UTC) |
|---|---|---|---|
| owned_busy 보류 | OI:249-259 → OI:282 `hand_busy` :407-426 → OB:33-96 `running` | drive는 `has_active_turn()`만 본다. `running`은 `state == Busy`일 때만 주입한다(OB:52-53). Idle을 포함한 나머지는 모두 `decided("hold", SOURCE_HELD)`(OB:70) 뒤 `Err(unsent)`(OB:95)로 간다. `early`는 Herdr Idle을 idle 전달로 넘기지만(OB:139-141) `running`에는 그 분기가 없다. Idle이면 `source_hold`가 Observe다(OB:24-30). 그래서 10초 Readiness 기한도 걸리지 않는다. | `02:48:17.79 herdr busy entry decision … decision="hold" reason="busy_source_unavailable" screen=Idle`, 02:57:39까지 분당 약 56줄 |
| 2초 반복 | src/services/discord/queue_io/input_owner/held_watch.rs:18 `LOOK=2s`, :65-71; src/services/claude_tui/host_input/busy.rs:206-214 | Idle 화면은 probe에서 `idle_verified` → `Screen::Ready`로 읽혀 Wake가 된다. drive가 다시 돌아도 토큰이 그대로라 같은 보류에 다시 걸린다. 무한 livelock이다. | `drive: owner front not delivered … NotSent("busy_source_unavailable")` |
| finalizer #3277 | src/services/discord/turn_finalizer/reconcile.rs:279-322(:310-312에서 기한을 `now+GATE_BACKSTOP`으로 당김, src/services/discord/turn_finalizer.rs:110 = 8초) → watcher_due :329-360 | 기한 재확인은 엄격 모드다(:350, `at_deadline=false`). 이 뒤에 finalize를 막는 조용한 출구가 셋 있고, 셋 다 로그를 남기지 않는다. (i) 엄격 재확인이 false면 :352-358이 기한을 `WATCHER_REGISTER_BACKSTOP`(약 1800초, turn_finalizer.rs:118-119)으로 되돌린다. (ii) `generic_backstop_allowed`(reconcile.rs:16-18, 39-44)가 막히면 entry가 Pending으로 남는다. 기한은 이미 지나 probe 필터(:289)에도 다시 걸리지 않는다. (iii) `claim_episode`가 Err이면(finalize.rs:88-99; episode.rs:82-89) AlreadyFinalized를 돌려준다. 그러면 reconcile.rs:90-95가 토큰을 풀지 않은 채 entry를 Finalized로 바꾼다. | `02:47:50.40 WARN #3277 … pulling the far-backstop deadline in … streak=2` 뒤 10분 동안 finalize 흔적 0 |
| turn-lease release | src/services/discord/turn_lease.rs:148-249 `release_on`, `identity` :71-111; 라우트 `src/server/routes/turn_lease.rs:15-48` | GET과 POST는 같은 `identity()`(메일박스 snapshot의 `cancel_token`)를 읽는다. `generation`은 부트 세대(634)라 이 프로세스의 모든 턴에서 같다. 턴을 가르는 값은 `user_message_id`·`turn_nonce`·`started_at`이다. `already_released`는 :160-161과 :244-245에서만 나고, 둘 다 그 순간 `identity()`가 None이어야 한다. CAS가 성공하면 남는 `operator_turn_lease_released`(:329-333) 로그는 0건이다. 그러니 POST는 drive가 보던 메일박스를 보지 못했다(다른 프로세스·포트로 요청이 갔을 가능성이 가장 높다). release 경로에 로그가 없어 확정은 못 한다. | turn-lease 관련 로그 0건 |
| `!stop` | src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:443-449 → codex_stop_delivery.rs:417-496(:415-416 주석 "never cancels or cleans up") | `running == false`면 `Err(Idle)`이다(:841-852). NotSent는 무엇이든 `settlement="host_owned"`로 매핑된다(:359-364). `terminal_confirmed`는 false로 고정이다(:491). 호스트의 종료 기록은 이미 지나가서 다시 오지 않는다. 결과적으로 아무 일도 하지 않는다. | `02:52:47.98 herdr user stop … delivery="not_sent" reason="idle" settlement="host_owned" terminal_confirmed=false` |
| inflight stale-age | src/services/discord/inflight/removal.rs:491-501, :417-421; host_liveness.rs:22-38; 호출 removal.rs:622,656; src/services/discord/inflight.rs:356(300초) | `tmux_pane_alive_for_stale_check`는 로컬 tmux가 아니면 true를 돌려준다. 그래서 Herdr 행은 늘 "not confirmed dead"다. 설령 행을 지웠더라도 메일박스 토큰은 남았을 것이다. | `02:52:54 ⚠ inflight stale-age (303s > 300s) overridden — … not confirmed dead`, 약 5초마다 |
| (추가) stale-turn release | src/services/discord/router/intake_gate/stale_turn.rs:186-196 | Herdr 호스트라 거부됐다. | `02:48:17.68 stale turn release refused … (Herdr·미확인·충돌)` |
| (추가) #4030 FOREIGN demotion | src/services/discord/destructive_cancel_gate.rs:245-279 | watcher heartbeat가 살아 있어 거부됐다. | `02:50:06 … denied_reason="fresh_watcher_heartbeat"` |

terminal이 빠진 상류 원인도 있다. 02:47:35에 watcher instance 7이 턴이 끝나는 순간 offset 5017644에서 시작해 fresh-idle만 보고 terminal을 제출하지 않았다(`tmux_watcher/turn_stream_collector/idle_read.rs:165`). 이것은 범위 밖이다. 같은 날 재발 (b)는 05:35Z에 일어났다. 자동 terminate가 `indeterminate`·"captured turn still active"로 실패했고(ledger가 이미 Finalized, 위 (iii)), 05:43 dcserver 재시작(boot 635)으로 풀렸다.

## 2. 진입점 전수표

### 2.1 ① 사람 입력을 보류·NotSent·정지로 만드는 판정 (E행)

| # | file:line | 함수 | 사유 | 근거 입력 | 상한과의 상호작용 |
|---|---|---|---|---|---|
| E1 | OI:249-262 | owner_drive | `Ok(true)` + 합류하지 않는 출처면 `START_WAITS` | mailbox token | 사고의 원점. 사람 입력은 `hand_busy`로 간다. synthetic은 상한 밖이다(§3) |
| E2 | OI:263-289 | owner_drive | `restart_recovery` / `upload_materialization_failed` / `session_transition_busy` | `ingress_hold`(src/services/discord/queue_io/input_owner/ingress.rs:192-199), transition lock | 시계는 돈다. 발동은 금지한다(정리 대상 아님, 게이트 보유) |
| E3 | OI:190-199 | owner_drive | `WRITER_HELD` writer_boot_held | src/services/discord/queue_io/input_owner/restart_gate.rs:137-142 | 발동 금지(Gate drop이 wake함) |
| E4 | OI:200-223 | owner_drive | boot custody `WRITER_HELD` | src/services/discord/router/intake_dispatch/boot_custody.rs:39-49 | 발동 금지 |
| E5 | OI:413-415 | hand_busy | 행이 없거나 세션 이름이 없음 → `busy_turn_unobserved`(stalled) | inflight row | 토큰만 남은 모양. 상한은 토큰 CAS로 다룬다 |
| E6 | OI:448-460 | hand_busy | `busy_source_unavailable: {err}` / phase≠Busy | Registration::observed | **상한 대상** |
| E7 | OI:461-462 | hand_busy | `front_not_granted` | owner grant | 경합이라 대상 아님 |
| E8 | OB:53-95 | running | **busy_source_unavailable**(OB:10). Idle·Held·prepare 실패 모두 | BusyObservation(binding row·화면·transcript) | **상한의 핵심 지점**(11:48, i8343) |
| E9 | OB:24-29 | source_hold | Held→Readiness(10초 기한), 그 외→Observe(기한 없음) | BusyState | Observe에는 상한이 필요하다. 토큰이 있을 때 Readiness 기한은 i8343 유실 원인이다(§6.4) |
| E10 | OB:137-188 | OwnerTurn::early | Held, 또는 prepare 실패 + active_turn → hold + `defer_before_submit` | observe_busy, has_active_turn | 상한 대상 |
| E11 | OB:161-170, 208-338 | recover_or_discard | active_turn=false일 때 retire. 실패하면 Identity hold, Refused면 discard | hosted record, `retire::observed` | Identity 기한이 있다. 상한은 토큰·행이 없으면 발동하지 않는다 |
| E12 | OB:394 | legacy_diagnostic | `hosted_tui_busy_pre_submit` | Legacy + transcript | tmux legacy. witness가 있으면 상한 대상 |
| E13 | OB:422-470 | late / hold_late | Busy prepare 실패 또는 Held → SOURCE_HELD | observe_busy, `unwound`(OB:511-531) | 상한 대상 |
| E14 | OB:472-509 | prepare | transcript/session_id/target/source 중 하나라도 없으면 SOURCE_HELD | BusyObservation | Idle 관측에는 source가 없어 반드시 실패한다(11:48) |
| E15 | OB:546-579 | hold | hold_start/hold_identity, probe arm, Readiness면 readiness::hold | owner front | 상한 시계를 여는 지점의 하나(키 OB:627 `same_front`) |
| E16 | src/services/discord/router/message_handler/intake_turn/owned_busy/missing_watch.rs:30-90 | guard | Missing pane → Gone/Ready/Unknown | herdr view, active_turn | 읽기 전용 |
| E17 | src/services/claude_tui/host_input/busy.rs:245-270 | admits / held | `NotSent{busy_source_unavailable, waits:true}` | 등록 source와 현재 source 비교 | 키 직전 재검증. 상한 뒤에도 유지한다 |
| E18 | src/services/claude_tui/host_input/busy/herdr.rs:303-343 | Context::read | 실행 사망·출력 미준비·modal·source 불일치 → Held, cleared 또는 idle_ready → Idle | herdr execution, capture, binding pin | Idle 근거의 원천. modal은 발동 금지 |
| E19 | src/services/claude_tui/host_input/busy/herdr.rs:35-58, 77-115, 126-133 | Adapter::submit | PaneUnavailable / Modal / TranscriptUnavailable / PersonDraft(C-s) / Refused | capture, draft_sighting | 초안·모달이면 발동 금지 |
| E20 | src/services/claude_tui/host_input/busy/tmux.rs:25-36, 50-65 | tmux Adapter | Veto(Modal, ScreenUnread, CopyMode, LoadFailed, TranscriptUnavailable) | pane, transcript | 화면 차단이면 발동 금지 |
| E21 | src/services/discord/queue_io/input_owner/claude_handoff.rs:89-101, 165-175, 208-224 | prepare | `claude_handoff_held`, transition lock, close_legacy 실패 | handoff journal, lock | 자기 예산이 있다(MAX_START_FAILURES=5). 발동 금지 |
| E22 | src/services/discord/queue_io/input_owner/claude_handoff.rs:232-234 | prepare | **has_active_turn(실패하면 true)** → hold | mailbox token | 토큰 루프의 둘째 지점. 상한 대상 |
| E23 | src/services/discord/queue_io/input_owner/claude_handoff.rs:569-610 | observe_busy | Tmux→Legacy, Return/Refused/행 없음/비Bound→Idle(context 없음), owner 불일치→Held | TurnHost, hosted row | context 없는 Idle은 probe가 없다. 상한은 held_watch에 기대지 않는다(§6.1) |
| E24 | src/services/discord/queue_io/input_owner/drive.rs:100-119 | run | handoff Err + `take_held`면 held_watch 뒤 탈출. held가 아니면 `stalled` | owner.held | 상한 시계가 여기서 시작·유지된다. stalled는 i8372 모양 |
| E25 | src/services/discord/queue_io/input_owner/drive.rs:184-206 | grant | stalled+outstanding이면 None, retry wait면 None | owner | retry wait 동안은 발동 금지 |
| E26 | src/services/discord/queue_io/input_owner/held_watch.rs:65-71 | verdict | Ready+선두→Wake, Gone/선두 아님/probe 실패→End, 그 외 Keep | probe | 11:48 2초 루프 |
| E27 | src/services/discord/queue_io/input_owner/held_watch/readiness.rs:7-49 | hold / hold_identity | `busy_readiness_unproven` / `busy_identity_unproven`, 10초 기한 | owner.hold_readiness | 토큰이 있을 때 걸리면 i8343 유실 원인이다(§6.4) |
| E28 | src/services/turn_orchestrator/queued_head/v2/readiness_hold.rs:111-158 | refuse_readiness_front / refuse_expired_front | 기한이 지나면 front를 refused_fronts로 버림 | ReadinessHold | **버림 경로 D1** |
| E29 | src/services/turn_orchestrator/queued_head/v2/readiness_hold.rs:183-211 | approve_first_key / leaf_outlived_due | 기한 뒤 첫 키 거부 | ReadinessHold | **버림 경로 D2**(i8343 재현 2) |
| E30 | src/services/turn_orchestrator/queued_head/v2/retry_wait.rs:62-89 | enter_retry_wait | Pending*/OwnLaunchTimeout/RootRecoveryHeld 30→300초 | grant/claim | 발동 금지(원인이 따로 있다) |
| E31 | src/services/discord/router/message_handler/provider_dispatch/owner_input/preclaim_wait.rs:14-47 | before_claim | `herdr_root_recovery_held` | root_replaced::preclaim | 발동 금지 |
| E32 | OI:770-821, 974-981, 1080-1123 | start_waits / start_idle | synthetic 전용 Watch/Event, WRITER_HELD, TURN_MODE_AWAITED | row, 화면, activity | synthetic이라 상한 밖이다 |
| E33 | OI:1121 ← src/services/discord/router/message_handler/foreign_row.rs:75-99 ← live_bridge.rs:271-331 ← intake_turn.rs:1975-1980 | start_idle → admit | **another turn's durable inflight row**: NotSent인데 hold하지 않음 → `stalled` | inflight row(Managed) | **i8372**. Target은 보류로 바꾸고 상한이 다룬다(§6.5) |
| E34 | OI:1171-1178 | hold_start | owner.hold_front | – | 공통 hold 기록 지점 |
| E35 | OI:1472-1512 | take_owned | Refused / host_returning / idle_unobserved / capacity | TurnHost | 일시적이라 발동 금지 |
| E36 | OI:1752-1754, src/services/discord/queue_io/input_owner/target.rs:59-88 | route | Held(remote_placement / synthetic_waits_idle / provider_unsupported) | route 매트릭스 | 대상 아님 |
| E37 | src/services/discord/queue_io/input_owner/effect.rs:42-48, 96-146 | refused / run | Completion::Held/ReadinessHeld → hold_not_sent, 그 외 retain/refuse | leaf NotSentProof | NotSent 기록 지점. 상한 전달도 이 경로를 그대로 탄다 |
| E38 | src/services/discord/queue_io/input_owner/effect.rs:216-262 | idle_claude | Modal→Held, ReadinessHeld, OWNER_READINESS_DEADLINE→Refused | PreparationHold, `observe_draft`, `focus_modal` | Idle 판정 술어를 재사용할 곳 |
| E39 | src/services/discord/queue_io/input_owner/effect/codex.rs:70-72, 176-256; OI:1713-1747 | idle_codex / CodexBusy | Codex Veto 대부분 waits, `busy_turn: transcript unavailable` | composer lock, rollout | #8426 착지 뒤 대부분 사라진다. witness가 있으면 상한 대상 |
| E40 | src/services/discord/health/turn_deliver.rs:128-142, 166-181 | external_turn_holds_channel / hold_for_start | `turn_source != Managed` 행(또는 이전 프로세스 Herdr 행) 하나만으로 채널 점유 → owner_drive가 토큰 없이도 active=true(OI:249) → hand_busy 보류 | inflight row(`turn_source`), activity | **상한 대상(행 모양)**. 토큰 CAS에 들어가지 못하므로 R5a(pane 있는 행, 출처 무관)로 다룬다. Managed 행은 false라 E33으로 간다 |

### 2.2 ② 상한이 정리할 상태를 만들고 바꾸고 지우는 곳 (C행)

| # | file:line | 함수 | 상태 | 상한과의 상호작용(경합) |
|---|---|---|---|---|
| C1 | TO:1810-1811 | HasActiveTurn | lease = `cancel_token.is_some()` | 상한의 "남은 lease" 판독 |
| C2 | TO:1963; src/services/discord/queue_io/turn_admission.rs:81,165 | TryStartTurn / mailbox_try_start_turn_adopting | lease **생성** | 정리 직후 새 턴이 설 수 있으므로 CAS로만 푼다 |
| C3 | TO:2042-2060, 792-826 | RestoreActiveTurn | lease 복원 | 재시작 뒤 lease가 다시 생길 수 있다(시계는 0부터 다시 센다) |
| C4 | src/services/discord/recovery_engine/restore_inflight.rs:120, 215, 2248; turn_admission.rs:219 | restore_inflight_states / recovery_kickoff | 부팅 때 행으로 lease 재등록. held/restarted Herdr 행은 건너뜀(:215) | 행만 남는 모양이 생긴다 → E33·§6.5 |
| C5 | src/services/discord/recovery_engine/runtime.rs:370,382 | reregister_active_turn_from_inflight | 행으로 lease 재부착 | C4와 같다 |
| C6 | TO:2408-2436 | FinishTurn | 채널 범위 해제(식별자 없음) | 상한은 쓰지 않는다 |
| C7 | TO:2445-2463; turn_orchestrator/episode_identity.rs:117-141 | FinishTurnIfMatches / release_turn_lease_if_matches | message id + nonce + started_before CAS | **상한 해제의 기본 도구** |
| C8 | TO:2465; witnessed_release.rs:53,76 | release_witnessed_episode | 관찰된 episode 해제 | 대안(쓰지 않음) |
| C9 | TO:2484; src/services/discord/mailbox_finish.rs:116-151 | FinishCancelledTurn | 취소된 lease 해제 | Cancel 경로 |
| C10 | TO:2487; clear_channel.rs:25,113; src/services/turn_orchestrator/queued_head/v2/clear.rs:17 | Clear | lease + 큐 + head 전부 비움 | 상한은 쓰지 않는다(큐 손실) |
| C11 | TO:1827-1939; src/services/discord/mailbox_stop.rs:5 | CancelActiveTurn | token cancel 표시(해제 아님) | `!stop` 전제 |
| C12 | turn_orchestrator/herdr_user_stop.rs:14-55 | admit_herdr_user_stop_if_current | Herdr stop은 의도만 기록 | 11:48의 `!stop` 무효 |
| C13 | src/services/discord/turn_finalizer.rs:259-282, 408-445 | TerminalEvent / **submit_terminal** | 정식 finalizer(OperatorRelease, Complete, HostTerminated, Cancel, GateTimeout) | ledger가 정확히 한 번을 보장한다 |
| C14 | src/services/discord/turn_finalizer/terminal_handler.rs:39-46, 106-117 | handler | OperatorRelease는 먼저 `claim`(CAS)하고, 성공하면 Finalized entry를 Pending으로 다시 연다 | **이미 Finalized인 entry도 푸는 유일한 경로**(재발 (b)) |
| C15 | src/services/discord/turn_finalizer/finalize.rs:167-191 | do_finalize_with_probe | 캡처한 episode로 행 정리 + `mailbox_finish_turn_if_matches` | – |
| C16 | src/services/discord/turn_finalizer/cleanup.rs:276-278 | status panel | OperatorRelease/HostTerminated는 완료 반응 생략 | 상한 정리도 mute settlement |
| C17 | src/services/discord/turn_lease.rs:71-111, 113-146 | identity / matching_inflight | lease 식별자. restart_mode·rebind_origin이면 거부 | 상한도 같은 거부를 상속한다 |
| C18 | src/services/discord/turn_lease.rs:148-249, 273-336 | release_on / OperatorRelease::claim | CAS 뒤 `clear_inflight_state_for_captured_episode`, 행이 없으면 late projection sweep. id 0이면 거부 | **상한이 그대로 재사용한다** |
| C19 | src/server/routes/turn_lease.rs:15,33 | inspect / release | HTTP 운영자 경로 | 그대로 둔다 |
| C20 | src/services/discord/herdr_terminate.rs:151-190, 350-393 | capture_turn / settle_captured | 캡처 + HostTerminated | pane을 죽이므로 상한은 쓰지 않는다 |
| C21 | src/services/discord/herdr_terminate.rs:229, 414, 710 | retire / terminate | hosted row Bound→Retired | 쓰지 않는다 |
| C22 | src/services/herdr_launch/retire.rs:36 | retire::observed(Missing) | Missing 실행 retire | 상한(Idle)과 반대 조건 |
| C23 | src/services/discord/zombie_foreground_release.rs:243-275 | release_zombie_foreground_turn | cancelled + 행 없음 + pane 종료 | 독립 근거 결합의 선례 |
| C24 | src/services/discord/inflight/save_store.rs:89, 1105, 1152, 1301 | save_inflight_state(_create_new/_fenced/_if_absent) | 행 생성·갱신 | 늦은 persist는 C18 sweep이 nonce로 막는다 |
| C25 | src/services/discord/inflight/clear_store/mod.rs:64, 125-198; identity.rs:252 | clear_inflight_state(_if_matches_*) / **_for_captured_episode** | 행 삭제 | 상한은 캡처 버전만 쓴다 |
| C26 | src/services/discord/inflight/removal.rs:434-503, 590-656 | stale_removal_reason / classify | 300초 stale 제거(Herdr는 무효) | 상한이 필요한 이유 |
| C27 | src/services/discord/inflight_heartbeat_sweeper.rs:40, 64, 242 | heartbeat sweep | watcher heartbeat가 멈춘 행 | watcher 없는 Herdr 행은 대상 아님 |
| C28 | src/services/discord/recovery_engine/herdr_row_end.rs:112,153; herdr_row_end/owner.rs:28-46,105-108 | row_end_pass / restarted_herdr_hold | 이전 프로세스의 Herdr 행 종료 | 같은 세대 행은 대상이 아니다 |
| C29 | src/services/discord/queue_io/input_owner/recovery.rs:8-45 | recover | 부팅 adopt, tick(만료 refuse), requeue_known_not_sent | owner.held·held_watch(메모리) 초기화 → 상한 시계도 초기화 |
| C30 | src/services/discord/queue_io/input_owner/restart_gate.rs:75-94, 115-134 | Gate::begin / Drop | 재시작 배타 | 게이트를 쥐고 있는 동안 발동 금지 |
| C31 | src/services/discord/turn_presence/supervisor.rs:206-209, 612-713 | apply / wake | presence: activity, woken_head, queue_held. wake는 `!token`일 때만(:695) | 정리가 토큰을 지우면 supervisor wake·presence 리셋이 저절로 돈다(presence를 따로 지우지 않음) |
| C32 | src/services/discord/turn_presence/supervisor.rs:535-551, 893-898 | CheckHeld / row_hold | 120초 held notice | 상한 notice와 문구를 겹치지 않게 한다 |
| C33 | src/services/discord/tui_direct_pending_start/state.rs:147-491 | mark_present / mark_absent | TUI-direct pending-start presence | 사람 입력 상한과 무관 |
| C34 | src/services/tui_prompt_dedupe/runtime_binding.rs:256 | publish_runtime_binding | host binding(output_path, session_id) | 상한은 바꾸지 않는다 |
| C35 | src/services/discord/router/message_handler/headless_turn.rs:1010-1020 → headless_turn/replay_start.rs:64-88 | headless 행 기록 | routine process 턴의 Managed 행 | i8372 행의 출처 |
| C36 | src/services/discord/turn_bridge/terminal_outcome_delivery/contracts.rs:244-268; src/services/discord/router/message_handler/headless_turn/process_mailbox.rs:100-152 | Unresolved → relinquish | 게시 미확정이면 행을 남기고 :125-152 정리 루프를 건너뜀 | i8372 행이 남는 이유 |
| C37 | src/services/discord/tmux_watcher/liveness.rs:462-473(`build_watcher_reacquire_inflight_state`), 호출 `reacquire_watcher_inflight_for_active_stream` | watcher self-heal | 메일박스 토큰 없이 `ExternalInput`·`user_msg_id=0`·Watcher 소유 행을 저장 | non-Managed 행 모양의 생산자. 그 뒤 terminal이 빠지면 E40으로 보류가 생긴다 → R5a. 모든 TUI-direct가 이 상태라는 뜻은 아니다 |
| C38 | src/services/turn_orchestrator/incarnation.rs:1-4(+ `registry_purge.rs`) | 메일박스 actor incarnation 교체 | purge가 registry entry를 새 actor로 바꿀 수 있다. 수락된 요청의 후속은 보낸 handle에 묶고 채널로 다시 찾지 않는다 | 상한 check는 시계를 열 때의 `ChannelMailboxHandle`을 쥐고, 판독·정리 직전에 현재 registry handle과 같은지(Arc 동일성) 본다. 바뀌었으면 정리 0, 시계 삭제, `declined reason=incarnation_replaced` |

합계: ① 40행 + ② 38행 = **78행**(w1 76행 + C37·C38).

### 2.3 보류된 사람 입력이 '버림'으로 끝나는 경로 (D행, i8343 요구)

| # | file:line | 함수 | 계기 | 안내가 입력을 특정하나 | 자동 재전송 | 이번 규칙 |
|---|---|---|---|---|---|---|
| D1 | src/services/turn_orchestrator/queued_head/v2/readiness_hold.rs:111-149(via v2/retained.rs:65-66, src/services/discord/queue_io/input_owner/drive.rs:139-171) | refuse_readiness_front | Readiness/Identity 기한 10초 만료(i8343 재현 1) | 아니오 | 아니오 | 토큰이 있는 동안에는 기한을 걸지 않는다(R1, §6.4). 남는 버림은 입력을 특정한다(R3) |
| D2 | src/services/turn_orchestrator/queued_head/v2/refused.rs:12-50 ← src/services/discord/queue_io/input_owner/effect.rs:125-129 | refuse_not_sent | 기한 뒤 첫 키 거부·`leaf_outlived_due`(i8343 재현 2) | 아니오 | 아니오 | R1로 토큰 있는 동안 건 기한이 없어 cold start가 물려받지 않는다. 특정 안내(R3) |
| D3 | src/services/turn_orchestrator/queued_head/v2/retained.rs:177-179, 211-213 | routine_delivery_closed | 루틴 전달 창 닫힘 | 아니오 | 아니오 | 기계 입력이라 범위 밖 |
| D4 | src/services/turn_orchestrator/queued_head/v2/readiness_hold.rs:51-78 | hold_identity | pane identity 재판독, D1·D2로 끝남 | 아니오 | 아니오 | D1과 같다 |
| D5 | src/services/turn_orchestrator/queued_head/v2/retained.rs:206-247 ← src/services/discord/queue_io/input_owner/effect.rs:130 | retain_not_sent | NotSent가 증명됐으나 깨끗이 정리할 수 없음 | 아니오 | 재시작 때만(:341-353) | 보관이지 유실은 아니다. 특정 안내만 추가 |
| D6 | src/services/turn_orchestrator/queued_head/v2/retained.rs:69-84 | tick head retain | 확인 기한이 지난 head(unconfirmed) | 아니오 | 아니오(중복 위험) | 특정 안내만 추가 |
| D7 | src/services/turn_orchestrator/queued_head/v2/start_failure.rs:60-161 ← provider_dispatch/start_failure.rs:258-261, OI:1063-1075 | start_failed | `owner_start_failed` | 아니오 | 아니오 | 특정 안내 |
| D8 | src/services/turn_orchestrator/queued_head/v2/claude_handoff.rs:196-239 | handoff_held / retain_held | handoff 예산 소진 | 아니오 | 아니오 | 특정 안내 |
| D9 | foreign_row.rs:75-99 → OI:1121 → src/services/discord/queue_io/input_owner/drive.rs:113-118 | admit | another turn's durable inflight row(i8372) | **안내 없음**, stalled | 아니오 | 보류로 바꾸고 상한이 다룬다(§6.5) |
| D10 | src/services/turn_orchestrator/queued_head/v2/clear.rs:154-190, v2/control.rs:367-395 | clear_all / clear_waiting | 사용자 `/clear`가 대기 front를 취소 | 개별 안내 없음 | 아니오 | 사용자 명시 동작이라 범위 밖(현행 유지) |
| D11 | src/services/discord/queue_io/input_owner/recovery.rs:15-44 | recover → tick | 프로세스가 내려가 있던 사이 기한 만료 → D1 | 아니오 | D5만 재큐 | D1과 같다 |
| D12 | src/services/turn_orchestrator/queued_head/v2/control.rs:257-265 | take_cancelled | 명시 취소 | 호출자 응답 | 아니오 | 범위 밖 |
| D13 | src/services/discord/queue_io/input_owner/notice.rs:16 → decision_drain::settle_channel | Decision 원본 제거 | 닫힌 Decision | – | – | 범위 밖 |

안내 본문은 모두 `src/services/discord/queue_io/retained_notice.rs:30-45` 한 함수에서 나온다. `OutboxMessage`에는 reply 참조 필드가 없다. 그래서 지금은 어느 안내도 입력을 특정하지 않는다.

## 3. 입력 출처 분류

분류 원천은 `InputOrigin::from_context`(src/services/discord/queue_io/input_owner/origin.rs:42-65)다. 사람 판정은 `ChannelMailboxHandle::person_input`(src/services/turn_orchestrator/queued_head/v2.rs:77-85)에 있다.

| # | 출처 | 타입 file:line | 상한 적용 | 이유 |
|---|---|---|---|---|
| S1 | Discord 사람 | `InputOrigin::Discord` origin.rs:6(context None이면 Discord :49-51), `is_person` :30-38 | **적용** | 사고 모양 |
| S2 | Discord 허용 봇(announce·Grok Bot·E2E 시험 봇) | 같은 `InputOrigin::Discord`. 봇 표식은 ingress.rs:418-425, 529에만 있다 | **적용** | 코드상 사람과 구분되지 않는다. #8328 U4도 "허용 봇: 사람 취급 유지"다. i8343 재현 입력도 봇 저자였다 |
| S3 | 허용 목록 밖 announce·notice | intake gate가 버린다(authorization.rs:8-16) | 해당 없음 | 큐에 들어오지 않는다. owner 자신의 notice는 `Hold::is_notice`(ingress.rs:142)로 거른다 |
| S4 | iMessage | `InputOrigin::Imessage` origin.rs:7 | **적용** | person이고 running turn에 합류한다 |
| S5 | Decision | `InputOrigin::Decision` origin.rs:9, `decision_input` v2.rs:89-96 | **적용** | `is_person` true인데 `joins_running_turn` false라 E1 `START_WAITS`에서 무한 대기할 수 있다. 같은 증상 부류다 |
| S6 | HttpStart(wake-watch 재개, `agents/turn/start`) | origin.rs:10, context `origin:"headless"`(start.rs:95) | 미적용 | synthetic이다. 죽은 Bound 쪽은 #8422가 맡는다 |
| S7 | Routine·예약 | origin.rs:14, `synthetic_original`(routine.rs:286-300) | 미적용 | 자기 턴에서만 완료한다(target.rs:76-78). 전달 창은 D3 |
| S8 | EffortContinue | origin.rs:11, effort_continue.rs:5-7 | 미적용 | 기계 입력(OI:235-236) |
| S9 | RestartSeed | origin.rs:13, restart_seed.rs:22-25 | 미적용 | greeting permit이 판정한다 |
| S10 | 재시도·재전송 | `same_input`(ingress.rs:348-354), `Owner::retry_original`(v2.rs:101-125). retry_wait front(src/services/discord/queue_io/input_owner/drive.rs:193-197). 사용자가 다시 보낸 입력은 새 Discord 메시지 | 원래 출처를 따름. **unattempted front에만 적용** | `head_injection`·retained·attempted 입력은 재전송하지 않는다(#8328 L63 "echo가 없다는 사실은 재전송 근거가 아니다") |
| S11 | hook에서 온 입력(TUI 직접 입력 UserPromptSubmit, Monitor 자동 턴) | 별도 enum 없음. owner 큐 밖 ExternalInput(#8328 I13), Monitor는 토큰만 등록(tmux.rs:614) | 해당 없음(입력 자체에는) | 다만 Monitor 자동 턴의 토큰이 사람 front를 붙잡으면 그 토큰은 정리 대상이 될 수 있다. id 0 lease면 정리를 거절한다(§6.2) |

합계 **11행**.

**#8422와 겹치는 곳**(`refs/review/i8422-impl-w2` c0285b2891, 미착지):
- #8422는 죽은 Bound 위 synthetic start의 영구 보류를 고친다(`start_wait::observe` → 공통 `dead_bound::recover`).
- 같은 파일을 만진다: OB(`recover_or_discard`를 [owned_busy의 dead_bound](https://github.com/itismyfield/AgentDesk/blob/c0285b2891f315812fb3fcf64c369e1f2254f31b/src/services/discord/router/message_handler/intake_turn/owned_busy/dead_bound.rs)로 이동, `early`가 guard 인자를 받음), OI(`start_waits` Watch, `start_idle` `Wait::Reject`), held_watch probe 의미.
- 계약 L: 무턴 판독부터 CAS까지 `SessionTransitionLease`를 쥔다. 이번 상한의 정리도 같은 lease를 `try_acquire`로 쥐고 한다. 못 쥐면 발동하지 않는다.
- 계약 T: #8422는 inflight가 정확히 `Ok(None)`일 때만 retire한다. 이번 상한은 남은 행을 **캡처한 episode 식별자로만** 지운다(pane retire 없음). 둘은 대상(synthetic 대 사람)과 동작(retire 대 lease CAS)이 다르다.
- #8422 설계 1절 7항의 "hold에는 시간 기한이 없다(drive.rs:110-114)"를 이번 상한이 사람 입력에 한해 다룬다.

## 4. Idle 판정

상한은 **양성 근거만 센다**. 잎 판독은 Unknown을 구분하지만, 종합 단계(`BusyState::Held`, `BusyObservation::probe`)는 Unknown을 뭉갠다. 그래서 상한은 잎 판독을 직접 부르는 새 witness(`hold_bound/witness.rs`)를 쓴다. witness는 `Idle` / `NotIdle(reason)` / `Unknown(reason)` 셋 중 하나를 돌려준다.

| 조건 | herdr Claude | herdr Codex | tmux Claude | tmux Codex |
|---|---|---|---|---|
| prompt_ready·빈 composer·모달 없음 (화면 근거) | `composer::restored_screen_idle`(src/services/claude/herdr_turn/composer.rs:106-117: 빈 composer, 모달 없음, `ready_for_input`, `!busy`) | **Codex 공통 화면 gate**(아래). capture는 `HerdrTarget::capture_ansi(-80)`(src/services/codex/herdr_turn/busy.rs:47과 같은 판독) | `prompt_readiness_snapshot`·`final_prompt_ready`(src/services/claude_tui/input.rs:106-112, 188, 478-482), `prompt_draft_detected` false | **Codex 공통 화면 gate**(아래). capture는 tmux `capture-pane -p -e`(src/services/codex_tui/busy_inject.rs:189-195 `composer_proof`와 같은 판독) |
| foreground 도구 없음 | 화면 busy footer 없음(tmux_common.rs:480, `HeldRead::Busy` screen.rs:213-215) + transcript에 열린 tool 요청 없음(claude_meta.rs:285 `requests_tool` → :213-232). herdr `ForegroundProcesses::Listed`에 도구가 있으면 거부, `Unreported`는 거부 근거로 쓰지 않음(contract.rs:350-373) | 같은 herdr process_info + rollout 열린 턴 없음 | `Descendants::NoneObserved`(src/services/claude_tui/activity/processes.rs:13-24). `Unknown`이면 Unknown | 같은 Descendants(readiness.rs:352) |
| 턴 끝 (transcript 근거) | `jsonl_completion_scan_idle`(completion_scan.rs:29-37; result, `system{turn_duration\|stop_hook_summary}`, interrupt marker) 또는 `readiness::observe` Proof(readiness/mod.rs:128-140) | `read_native_end`(rollout.rs:105) == `Ended`, 또는 `readiness::Verdict::Idle(Proof{Record\|StopHook})`(readiness.rs:74-80). `jsonl_completion_scan_idle`은 쓰지 않는다(Codex는 `turn.completed`만 인정하고 TUI rollout의 `task_complete`는 종료 표식이 아님, tui_turn_state.rs:2293-2304) | herdr Claude와 같다 | herdr Codex와 같다. `judge_in`(readiness.rs:224-276) `Verdict::Idle`은 **여기(transcript·실행 근거)에만** 쓴다. 화면을 읽지 않으므로(readiness.rs:224, 318, 387) 화면 근거로 세지 않는다 |
| 관측 실패 | capture 실패, `HeldRead::Unread`, `TuiTurnState::Unknown`, binding 없음 → **Unknown** | capture 실패·빈 capture, `Composer::Unread`, `Verdict::Unknown`, `NativeEnd::Unknown` → **Unknown** | `Screen::Unknown`, `PaneLiveness::ProbeError` → **Unknown** | capture 실패·빈 capture, `Composer::Unread`, `Verdict::Unknown` → **Unknown** |

**Codex 공통 화면 gate**(P1-1, herdr Codex·tmux Codex 공통). 새 UI 파서를 만들지 않고 기존 `codex_tui::busy_inject::screen::read`(screen.rs:50-72)와 `delivery_veto`(screen.rs:75-83)를 재사용한다. 다음을 모두 만족해야 화면 근거가 양성이다.

```
capture 성공 ∧ capture.trim() 비어 있지 않음
∧ delivery_veto(capture) == None      (제출 모달·action menu·completion popup·modal footer)
∧ !screen.modal
∧ !screen.queued
∧ !screen.attachments                 (이미지만 첨부한 초안도 사람 초안이다, screen.rs:256-272)
∧ screen.composer == Composer::Empty  (Text·Unread는 아님. Unread는 Unknown)
```

이 조건은 legacy steering이 이미 쓰는 조건(src/services/codex/herdr_turn/busy.rs:57-62, `composer != Empty || attachments`면 거부)과 같다. tmux Codex의 `judge_in`과 두 호스트의 `read_native_end`는 transcript·실행 근거로만 쓴다. 따라서 Codex 한 회는 "공통 화면 gate 양성 ∧ transcript·실행 근거 양성"이다.

- **연속 Idle**: 상한 시각(머리가 처음 보류된 뒤 180초)이 지난 뒤, 2초 간격 판독 3회가 모두 `Idle`이어야 한다(창 4초 이상). 화면 근거와 transcript 근거가 **둘 다** 양성이어야 한 회로 센다. `NotIdle` 또는 `Unknown`이 한 번이라도 나오면 횟수를 0으로 되돌린다.
- **관측 실패는 상한 미적용**(안전 방향). 상한은 30초 뒤 다시 시도한다(§5). 상한 시각이 지나도 계속 Idle이 아니면 한 번만 "보류 중" 안내를 낸다(§6.6). 정리는 하지 않는다.
- **i8372 행 전용 모양**(pane 없는 headless 행): 화면이 원래 없는 종류이므로 관측 실패로 치지 않는다. 대신 생산자 종료 근거를 쓴다(§6.5).

## 5. 보류 시계

- **시작점**: owner drive가 front를 처음 보류(hold)·정지(stalled)로 남긴 시각이다. 키는 OB:627 `same_front`와 같은 `(channel, ticket, message_id, queued_generation)`이다. `Instant`를 새 child `src/services/discord/queue_io/input_owner/hold_bound.rs`의 프로세스 메모리 map에 둔다. 2초마다 같은 front를 다시 보류해도 첫 값을 유지한다. `ReadinessHold`의 "첫 값 유지" 의미(readiness_hold.rs:34-39)와 같다.
- **초기화**: front가 바뀜(전달·버림·취소·`prefer_person`으로 다른 front가 앞에 옴), front가 attempted가 됨(`head_injection`), 상한이 발동함. 이 경우 map entry를 지운다.
- **재시작 뒤**: 시계는 메모리라 0부터 다시 센다(C29 `held:None`과 같다). 영속 필드를 새로 만들지 않는다. 재시작은 그 자체로 lease를 다시 판정하는 사건이고(C3·C4·C28), 다시 세면 전달이 늦어질 뿐 더 일찍 정리하지는 않는다(안전 방향). 모델 `HumanHoldBoundTargetRestart`가 재시작이 한 번 끼어도 liveness가 GREEN임을 보인다.
- **판정 시점**: held_watch probe에 기대지 않는다. context 없는 Idle(E23)은 probe가 없기 때문이다. `src/services/discord/queue_io/input_owner/drive.rs:139-171` `settle_at_deadline`이 쓰는 `settle_due`에 `hold_bound::due(channel)`(첫 보류 + 180초, 재시도는 +30초)을 최솟값으로 더한다. 기한이 되면 `hold_bound::check`가 2초 간격 판독 3회를 직접 돌린다.

## 6. 정리와 전달

### 6.1 흐름(순서 고정: 옛 턴 정리 → 전달)
0. 시계를 열 때 쥔 `ChannelMailboxHandle`이 현재 registry handle과 같은지 본다(C38). 다르면 정리 0, 시계 삭제.
1. `hold_bound::check(channel)`이 SessionTransitionLease를 `try_acquire`한다(#8422 계약 L). 못 쥐면 30초 뒤 다시 본다.
2. 게이트를 확인한다. restart_gate 활성(C30), writer boot custody(E3·E4), root recovery(E31), retry wait(E30), readiness/identity hold가 **idle 경로**에서 걸린 경우(E27이 토큰 없이 걸린 것, R2)면 발동하지 않는다.
3. front가 아직 unattempted이고(`Owner::front()`, `head_injection` 없음), 출처가 §3 적용 행인지 확인한다.
4. 막는 근거를 고정한다.
   - (a) 토큰 모양(11:48·i8343 잔여): `turn_lease::identity()`(C17)로 `LeaseIdentity`를 읽는다. `restart_mode`, `rebind_origin`, `user_message_id == 0`이면 거절한다(`input_hold_bound_declined`).
   - (b) 행만 남은 모양(i8372): §6.5.
5. witness 3회(§4)가 모두 Idle이면 정리한다. 하나라도 아니면 횟수를 0으로 하고 30초 뒤 다시 본다.
6. **정리는 정식 finalizer로 한다.** 새 crate 내부 함수 `turn_lease::release_for_hold_bound(shared, provider, channel, expected)`를 둔다. 이 함수는 `release_on`(C18)을 `reason = "input_hold_bound"`로 부르는 얇은 래퍼다. 흐름은 `TerminalEvent::OperatorRelease` → `submit_terminal`(C13) → actor `claim`(C14: identity 재비교 → `release_turn_lease_if_matches` CAS(C7) → `clear_inflight_state_for_captured_episode`(C25) → Finalized entry 재개방)다. OperatorRelease는 `is_mute_settlement`가 true인 두 이벤트 가운데 하나라 완료 반응을 내지 않는다(turn_finalizer.rs:288-295). `herdr_terminate::settle_captured`(C20)는 쓰지 않는다. pane을 죽이고, 재발 (b)처럼 ledger가 이미 Finalized이면 실패하기 때문이다.
7. **전달은 상한이 직접 하지 않는다.** 토큰 모양에서는 CAS가 `removed_token`을 돌려주므로 `publish_release`가 `MailboxReleased`를 낸다(episode.rs:32-43). 다만 idle queue listener는 `queue_is_eligible()`가 아닌 이벤트를 무시한다(queue_io/backstop.rs:692-700). 실제 깨움은 completion admission이 `mailbox_released ∧ operator_released`로 `QueueEligible`을 발행할 때 생긴다(turn_finalizer/completion_admission.rs:85). 이 drain이 owner drive를 깨운다. 행 모양의 깨움은 §6.5 R5c다. drive는 `has_active_turn()=false`를 보고 기존 `hand_idle` 경로로 그 front를 한 번 전달한다. 대기열 메시지는 그대로 있다(front는 큐에서 빠지지 않았다). 그래서 중복 방지는 owner grant·`approve_first_key`·`head_injection`이라는 기존 장치가 그대로 맡는다.
8. 결과 로그와 안내(§6.6). 시계 entry를 지운다.

### 6.2 거절과 남은 위험
- id 0 lease: CAS 키가 없어 정리할 수 없다(turn_lease.rs:189-194와 같은 이유). `declined reason=unbound_message_identity`를 남기고 입력을 특정한 "보류 중" 안내를 한 번 낸다. 리뷰(w1) 확인 결과, 실제로 추적한 Monitor·recovery·TUI-direct 경로는 0이 아닌 active identity를 쓴다. `row.user_msg_id=0`(예: watcher 재획득 행 C37)과 `LeaseIdentity.user_message_id=0`은 같은 뜻이 아니다. 행 쪽 0은 R5a가 행 식별자(`turn_nonce`)로 다루고, lease 쪽 0만 드문 예외로 남는다.
- `release_for_hold_bound`가 `Err("lease changed …")`를 돌려주면 새 턴이 섰다는 뜻이다. 상한은 아무것도 하지 않고 시계를 지운다(다음 hold가 다시 시작한다).

### 6.3 옛 턴의 늦은 terminal 제출·답 재게시와의 중복 0
- 같은 key, TTL(10분, turn_finalizer.rs:125) 안: entry가 Finalized라 `AlreadyFinalized`, 아무 일도 하지 않는다(terminal_handler.rs:112-114). 다만 모든 늦은 terminal이 같은 ledger key로 온다고 일반화하지는 않는다(Monitor 등은 key가 다를 수 있다). 그런 경우에도 아래 nonce 울타리가 후속 턴을 보호한다.
- GC 뒤: 후속 턴 토큰이 다른 nonce로 살아 있으면 `episode.rs:82-89`에서 Err, 아무 일도 하지 않는다. 채널이 비어 있으면 `episode_row_miss`(:90-92)가 후속 행을 보호한다.
- id 0 키의 무방비 `mailbox_finish_turn`(finalize.rs:190)은 남은 위험이다. 상한은 id 0 lease를 정리하지 않으므로 이 위험을 늘리지 않는다.
- 답 재게시: finalizer는 본문을 게시하지 않는다. 11:48에는 terminate 직후 wrapper 세대가 바뀌어 relay watermark가 리셋됐다(02:58:04). 상한은 pane을 건드리지 않으므로 wrapper 세대가 바뀌지 않고, 이 리셋 경로를 타지 않는다. 시험 T6이 이를 고정한다.
- 모델: `HumanHoldBoundTargetLate` GREEN, `MutNoLedgerFence`는 `NoNewTurnClearedByOld` RED.

### 6.4 i8343 규칙: 토큰이 있는 동안 건 Readiness 기한은 버림 근거가 아니다
- 원인: `!clear` 직후 새 세션 source가 아직 pin되지 않았다. 그래서 `Context::read`가 Held를 냈고(src/services/claude_tui/host_input/busy/herdr.rs:303-343, `current_source` :399-413), `source_hold`가 Readiness 10초 기한을 걸었다(OB:24-30 → readiness_hold.rs:25-46). 그 기한이 턴 A가 끝나기 전에, 또는 턴이 끝난 직후 cold start의 첫 키에서 만료됐다(재현 1 D1, 재현 2 D2). 안내는 어느 입력인지 밝히지 않았다.
- 규칙 R1: **메일박스 토큰이 있는 동안(`has_active_turn()`=true) 건 보류는 Readiness 기한을 걸지 않는다.** `source_hold(state)`(OB:24-30)를 `source_hold(state, active_turn)`로 바꾼다. `active_turn`이면 Held도 Observe를 돌려준다. 이 함수를 부르는 OB `running`(늘 토큰 있음)·`early`(E10)·`late`(E13)가 모두 같은 규칙을 따른다. liveness는 턴 끝 drain wake(기존)와 이번 상한이 맡는다.
- 규칙 R2: **토큰 없이 건 기한은 지금과 같다.** 토큰이 없을 때 Held로 걸린 기한(예: idle 경로의 초안·모달·composer 미준비)은 그대로 10초 뒤 버림으로 끝난다. 이것을 턴 경계에서 지우면 그 보류는 기한도 상한도 없이 남는다(상한은 토큰·행이 있어야 발동한다). 그래서 지우지 않는다. 결과적으로 모델의 "턴 경계에서 기한을 다시 잡음"은 R1(토큰이 있는 동안에는 기한 자체가 없음)으로 구현된다. 저장된 `ReadinessHold` 형식은 바뀌지 않는다.
- 규칙 R3: **그래도 버리거나 보관할 수밖에 없으면, 그 unit의 모든 member를 특정한다**(P2-5). 실제 버림 단위는 원문 하나가 아니다. `refuse_readiness_front`는 front + `matching_followers`를 함께 지우고 `RefusedFront` 하나를 만든다(readiness_hold.rs:115-141). `refuse_not_sent`도 attempt의 모든 member를 지우고 `RefusedInput` 하나를 만든다(refused.rs:12-50). notice drain은 entry당 안내 하나를 보낸다(input_owner/notice.rs:82-115). 그래서 원문을 지우기 **전에** 각 member의 Discord 메시지 ID와 짧은 발췌(최대 40자, 한 줄)를 모아 entry의 notice identity JSON(`input.members[]`)에 싣는다. `retained_notice::enqueue` 한 곳에서 목록을 본문에 넣는다(member가 많으면 앞 5개 + "외 n건"). entry당 outbox 1건과 기존 persistent dedupe 키(identity·attempt nonce·kind, retained_notice.rs:15-26)는 그대로 둔다. 대상은 D1·D2·D4·D5·D6·D7·D8이다.
- 모델: `HumanHoldBoundCurrentClearDrop`에서 **INVARIANT `NoSilentLoss`가 RED**다(3단계 반례: owner drive → Readiness 기한 경과 → 기한 만료로 front 버림, 안내가 입력을 특정하지 않음). `IdleEventuallyDelivered`도 RED다. `HumanHoldBoundTargetClearDrop`은 GREEN이다. w2 모델은 R1을 그대로 옮겼다. Target Drive는 토큰이 있으면 기한을 만들지 않고, w1의 두 보완 장치(DueDrop의 토큰 예외, OldTerminal의 기한 초기화)는 지웠다. R1을 직접 단언하는 `R1NoDueWhileLeased`를 Target cfg의 검증 가드로 넣었다. w1 모델에 이 가드를 붙이면 Init→Drive 1전이에서 RED다(리뷰어 재현과 일치). `MutNoR1`(기한 생성 금지만 제거)은 `R1NoDueWhileLeased`와 `IdleEventuallyDelivered`가 RED다. R2(idle 경로 기한 유지)는 `CurrentIdleDue`(`NoSilentLoss` RED)와 `TargetIdleDue`(GREEN)로 따로 보고, `MutFirstOnly`(첫 member만 특정)는 `NoSilentLoss` RED다.
- **#8328 S1과의 분담**: S1(미착지)은 E8 `busy_source_unavailable` 보류 자체와 G2 `Context::read`→Held를 지운다. 토큰이 있으면 즉시 붙여넣고 source는 나중에 pin한다(설계 §5.5.3). 그래서 S1이 착지하면 i8343의 두 재현 모두 전달된다. 이번 상한이 맡는 몫은 S1 뒤에도 남는 것이다. (a) H1 초안·H2 모달·Hc composer 미준비·H8 잠금 경합의 Readiness 기한이 버림으로 끝나는 경로의 특정 안내(R3). (b) 토큰이 있는 동안 건 기한이 턴 경계를 넘어가는 문제(R1, S1 뒤에도 `early`·`late`에 남는 Held). (c) 토큰이 끝내 안 풀리는 Observe 보류(상한 본체). (d) 정지 상태 front(i8372, §6.5).

### 6.5 i8372 판정: 같은 상태가 아니다. 이번 상한이 보류로 바꿔 함께 푼다
- 코드 판정: **상한이 다루는 보류와 같은 상태가 아니다.**
  - 보류된 머리가 없다. E33이 NotSent를 돌려주지만 `hold_start`를 부르지 않는다. 그래서 drive는 `stalled`이고(src/services/discord/queue_io/input_owner/drive.rs:113-118), `grant`는 None이다(:189-191). 기한도, 재시도도, 안내도 없다.
  - 메일박스 토큰이 없다(10:19:00.995에 턴이 끝났다).
  - pane이 없다(headless process, `routine-process-…` 라벨).
  - 막는 것은 Unresolved로 끝난 턴이 남긴 **Managed inflight 행**이다(C35·C36).
- 그래도 결과는 같은 부류(조용한 정지, liveness 없음)다. 그래서 이번 상한이 다음 두 가지를 맡는다.
  - 규칙 R4: E33의 foreign-row 거절을 사람 front에서는 **보류로 기록한다**(`hold_start`, 사유 `foreign_row`). 상한 시계가 돌고, 30초 Observe 재시도로 다시 판정한다. owner_input.rs(giant)에는 호출 한 줄만 바꾸고, 본체는 `src/services/discord/queue_io/input_owner/hold_bound/row_only.rs` (제안)에 둔다.
  - 규칙 R5: **행만 남은 모양(토큰 없음 + 행 있음)의 정리.** 먼저 pane 유무와 종료 근거 종류로 나눈다(P2-4). 어느 쪽이든 삭제는 캡처한 episode 식별자로만 한다(`clear_inflight_state_for_captured_episode`, C25). 행의 `restart_mode`·`rebind_origin`이 있으면 거절한다. 판독할 때마다 메일박스 `cancel_token`이 None이어야 한다(또는 그 nonce가 행의 `turn_nonce`가 아니어야 한다).
    - **R5a pane 있는 행**(출처 무관: Managed·`ExternalInput`·watcher 재획득 C37): §4 witness 두 근거를 3회 연속 적용한다. `turn_source == Managed` 제한은 두지 않는다.
    - **R5b headless routine 행**(pane 없음, `routine-process` 라벨, P2-2): 다음을 모두 만족해야 한 회 양성이다. 하나라도 조회 실패·행 없음·식별자 불일치면 **Unknown**이다.
      1. **결합**: 캡처한 row의 finalizer turn id(`discord:<channel>:<n>`)로 `routine_runs`를 정확히 찾는다(`turn_id` 열, routines/store/recovery.rs:16-23). 라벨에는 run ID가 아니라 turn ID의 마지막 구성요소만 있으므로(routines/agent_executor/execution.rs:58-70, `turn_id.rsplit(':')`) 라벨만으로 결합하지 않는다. 그 run의 attempt와 행의 `turn_nonce`가 같아야 한다.
      2. **명시 terminal 상태**: `!= running`이 아니라 store가 정의한 종료 값(`succeeded`, `failed`, `interrupted`)인지 본다.
      3. **producer 종료 receipt**: run 종료와 producer 종료는 같은 사실이 아니다. 루틴은 `complete_agent_run` 뒤에 세션 teardown을 하고(agent_executor.rs:445-459), ProcessMailbox는 final frame이 CLI 종료보다 먼저 올 수 있다고 명시한다(process_mailbox.rs:105-106). Unresolved 분기는 producer를 join하지 않고 반환한다(:115-118). 그래서 ProcessMailbox가 retained로 끝나는 분기에서도 producer를 join하고, 그 결과를 행에 `producer_exited` receipt로 남기게 한다(행의 `turn_nonce`로 fence한 기존 저장). 새 장기 감시 장치는 두지 않는다. receipt가 없으면 Unknown이다(이전 프로세스의 행이 여기에 해당한다. 남은 위험으로 적는다).
    - **R5c 성공 계약**(P2-3): 캡처한 행 제거 → **해당 원장 entry만** mute 정산 → transition lease 해제 → owner wake. GateTimeout은 쓰지 않는다. `is_mute_settlement`는 OperatorRelease·HostTerminated만 true이고(turn_finalizer.rs:288-295), GateTimeout의 의미를 바꾸면 기존 호출자에 영향이 크다. 그래서 좁은 경로 하나를 둔다. `TerminalEvent`에 행 전용 변형 하나와 `is_mute_settlement` 한 줄을 더하고(giant turn_finalizer.rs에는 이 두 줄만), 처리 본체는 새 child `src/services/discord/turn_finalizer/row_end.rs` (제안)에 둔다. 원장 entry가 없으면 정산을 건너뛴다. 토큰이 없으므로 `publish_release`는 이벤트 없이 돌아온다(episode.rs:32-35). 그래서 깨움은 `MailboxReleased`에 기대지 않는다. 상한이 transition lease를 놓은 뒤 기존 `drive::wake(drive)`(drive.rs:159-161의 deadline tick과 같은 함수)를 직접 부른다. drive가 E33·E40을 다시 지나 전달한다. 정산은 완료 반응이나 성공 전달 증거를 새로 만들지 않는다.
- 결과 미게시 원인(tui_o "selected channel runtime kind is unknown")은 별도 조사 레그가 맡는다. 겹치는 파일만 적는다: `src/services/tui_o/cutover/channel_gate.rs`(:18-19, :475-485, :512-522, :636-665), `tui_o/cutover/claude_thread.rs`, `tui_prompt_dedupe` `runtime_binding_for_tmux_session`, `tmux_common` `resolve_tmux_runtime_kind_marker`, `tui_o/alarm.rs`, `src/services/discord/router/message_handler/headless_turn.rs:440`, `src/services/discord/router/message_handler/headless_turn/replay_start.rs:90-98`, `src/services/discord/turn_bridge/terminal_outcome_delivery/contracts.rs:244-268`, `src/services/discord/router/message_handler/headless_turn/process_mailbox.rs:100-152`. 이번 상한은 이 파일들 가운데 아무것도 바꾸지 않는다.
- 모델: `CurrentRowOnly`는 `OrphanEventuallyDelivered` RED(1단계: owner drive → 남은 행에 막혀 깨움 없이 멈춤)이고 `TargetRowOnly`는 GREEN이다. `CurrentRowPane`(watcher 재획득 행 + pane Idle)은 `IdleEventuallyDelivered` RED이고 `TargetRowPane`은 GREEN이다. `TargetRunLive`(run 종료, producer 생존)는 producer가 끝날 때까지 정리 0이다. mutant `MutRunOnly`(producer 조건 제거)는 `NoBoundOnBusySession` RED, `MutNoWake`(R5c 깨움 제거)는 `OrphanEventuallyDelivered` RED, `MutGateTimeout`(GateTimeout 정산)은 `BoundSettlesMute` RED, `MutManagedOnly`(Managed 제한 복원)는 `IdleEventuallyDelivered` RED다.

### 6.6 이벤트 이름(고정)과 채널 notice
| 이벤트 | 수준 | 필드 | 언제 |
|---|---|---|---|
| `input_hold_bound_released` | warn | channel_id, message_id, shape(lease\|row_only), turn_nonce, held_ms, witness(host/provider), settlement | 정리 성공 직후 |
| `input_hold_bound_declined` | info | channel_id, message_id, reason(not_idle\|observation_unknown\|gate\|unbound_message_identity\|lease_changed\|attempted) | 상한 시각이 지났지만 발동하지 않음(사유가 같으면 front당 1회) |
| `input_hold_bound_waiting` | info | channel_id, message_id, reason | 상한 시각이 지나도 Idle이 아니라서 "보류 중" 안내를 낸 때(front당 1회) |

채널 notice는 기존 outbox(`src/services/discord/queue_io/retained_notice.rs`, `enqueue_outbox_pg_returning_id_with_persistent_dedupe`)에 새 함수 하나로 낸다. dedupe 키는 `hold_bound:{channel}:{message_id}:{kind}`이고, 한 줄이다.
- released: "⏱️ 이 입력(「<발췌>」)은 앞 턴 기록이 정리되지 않아 3분 넘게 보류됐습니다. 세션이 입력 대기 상태임을 확인하고 남은 턴 기록을 정리한 뒤 전달합니다."
- waiting: "⏱️ 이 입력(「<발췌>」)은 3분 넘게 보류 중입니다(사유: <화면 초안·모달·작업 중·관측 실패>). 상태가 풀리면 전달합니다."

새 운영자 명령이나 수동 스위치는 만들지 않는다.

## 7. 인접 작업과의 계약

| 작업 | ref | 겹치는 파일·함수 | 계약 | 착지 순서 |
|---|---|---|---|---|
| #8328 S1(S1-A 구현 진행 중) | `refs/review/i8328-design-5` 8893cb82e9, [#8328 설계](https://github.com/itismyfield/AgentDesk/blob/8893cb82e9a86c1a2b80f311444d43f5a5441047/docs/design/8328-owner-input-busy-inject.md) | S1-A는 `src/services/tui_o/writer/reply_anchor.rs` (제안), `src/services/tui_o/store/anchor_bindings.rs` (제안), `src/services/tui_o/writer/deliver/prepared.rs` (제안), `pieces.rs`, `writer/mod.rs`, `deliver.rs`, `store/ledger.rs`, `o_writer_io.rs`, `turn_view_reconciler/injected.rs`, `human_frame/history/v2.rs`, `rotation/owe.rs`만 바꾼다. **이번 상한과 파일 겹침 0.** S1-B(`busy.rs` `admits`, `busy/herdr.rs`), S1-C(`v2/retained.rs`, `turn_lease.rs`, `supervisor.rs`), S1-D(OB:33-97 `running`·OI:407-491 `hand_busy` 삭제)는 겹친다 | S1 설계 L479 A9 "`#8430 상한` \| 안전망 유지. 초안·모달·작업 중 화면에는 미적용(U1) \| 별도", L488 "#8430은 안전망으로 남는다", L160·L163 §2.2 표(H1만 "알림만", H2–H11 "불필요"). **계약 충돌 없음.** 다만 넷을 지킨다. (1) 상한 본체를 S1-D가 지울 `running`/`hand_busy`에 두지 않는다(→ `src/services/discord/queue_io/input_owner/hold_bound.rs`). (2) S1-C 뒤에는 `awaiting_echo`가 비어 있을 때만 발동한다(L235·L478 귀속 대기는 자기 기한). 이번 조각에서 `hold_bound::check`의 게이트 함수 한 곳에 S1-C 결합점을 남긴다. (3) unattempted front만 전달한다(L63). (4) R1의 `source_hold` 변경은 S1-D가 그 경로를 지울 때 함께 사라진다 | **T0 묶음(이번 S1~S3) 먼저**, S1-A는 파일 겹침이 없어 순서 무관. w2의 S3는 `turn_finalizer.rs`(enum 변형·mute 한 줄)과 새 `src/services/discord/turn_finalizer/row_end.rs` (제안)를 더한다. #8432 겹침 목록(`turn_finalizer/reconcile.rs:223-358`)과는 다른 함수다. S1-B~D가 나중에 맞춘다(OB·`turn_lease.rs`·`v2/retained.rs` 병합 지점) |
| #8437 tui_o 입력 공지 | `refs/review/i8437-fix-w1-wip` 069f818b50 | `tui_prompt_dedupe/extract.rs`, `tui_o/writer/input_notices*`, `store/ledger/input_notices.rs`, `actor/notice_*` | 파일 겹침 0. 상한 notice는 owner outbox 경로라 tui_o 입력 공지와 다르다. 다만 상한이 전달한 front가 실제로는 돌던 턴에 흡수되면 #8437의 `command_row_uuid` receipt 경로를 탄다. Idle 2근거로 막는 오판 시나리오와 같다 | 무관 |
| #8422 synthetic dead Bound | `refs/review/i8422-impl-w2` c0285b2891 | OB(`early`, [owned_busy의 dead_bound](https://github.com/itismyfield/AgentDesk/blob/c0285b2891f315812fb3fcf64c369e1f2254f31b/src/services/discord/router/message_handler/intake_turn/owned_busy/dead_bound.rs)), OI(`start_waits`, `start_idle`), `missing_watch.rs`, `v2/control.rs`, `turn_start.rs` | 계약 L(SessionTransitionLease)을 따른다. 계약 T와는 대상이 달라 충돌하지 않는다(§3) | #8422가 먼저 착지하면 R1의 OB 변경을 그 위에 맞춘다(3줄) |
| #8426 herdr Codex busy | `refs/review/i8426-s1-w1` 0dec337a74 | OI `hand_busy` → [busy_source::live_source](https://github.com/itismyfield/AgentDesk/blob/0dec337a74770be9e3013cded7c0ddfb65726d0b/src/services/discord/router/message_handler/provider_dispatch/owner_input/busy_source.rs) | 파일 겹침은 OI 한 곳(이번에는 OI:1121 호출 한 줄만 바꿈). #8426 뒤 Codex의 남은 토큰 + Idle 모양은 대부분 주입되므로 상한 발동이 줄어든다 | 무관 |

**S1 계약을 바꿀 필요가 없으므로 DESIGN_DELTA는 없다.**

## 8. 접근 비교

| 안 | 흐름 | 비용 | 위험 |
|---|---|---|---|
| **A. owner drive 상한(채택)** | front 보류 시계 → 3분 + Idle 2근거 3회 → 기존 OperatorRelease CAS(정식 finalizer)로 옛 episode 정리 → 기존 drain이 1회 전달 | 새 child 5개 + 호출 교체. 약 19파일/+950 | 정리가 돌고 있는 턴을 지울 위험 → 2근거·연속 3회·unattempted·CAS로 막음. id 0 lease는 남은 위험 |
| B. 막힌 정리 경로 각각 수리 | finalizer 조용한 출구 3곳 로그·재시도, stale-age의 Herdr 생사를 herdr pane 상태로 판정, `running`의 Idle을 idle 전달로 넘김, release API 진단 | giant 4개(turn_finalizer.rs 4803, inflight.rs 6345, removal.rs 1694, OI 1795)를 고침. 약 10파일/+400 | 원인마다 따로 막는다. 다음 새 원인에는 liveness가 보장되지 않는다(이슈가 말한 근본 구조 그대로). Idle 전달은 남은 lease 위에 새 턴을 얹어 귀속이 섞인다 |

B는 원인 하나하나에는 싸지만 "결국 전달"을 보장하지 못한다. A는 보류 사유와 무관하게 상한을 건다. 정리도 기존 운영자 해제와 같은 CAS를 쓴다. **A를 고른다.** B의 진단 로그(finalizer 출구 (i)(ii), release 경로)는 별도 일감 후보로 보고서에 적는다.

## 9. 구현 조각과 시험

| 조각 | 내용 | prod 파일(추정) | 순 +줄(추정) |
|---|---|---|---|
| S1 상한 본체(T0) | `src/services/discord/queue_io/input_owner/hold_bound.rs`(시계 map, 시계를 연 actor handle, due, check, 게이트, 이벤트), `src/services/discord/queue_io/input_owner/hold_bound/witness.rs`(§4 판독 4종 + Codex 공통 화면 gate), `src/services/discord/queue_io/input_owner/mod.rs` 등록, `src/services/discord/queue_io/input_owner/drive.rs` settle_due 결합·시계 초기화 훅, `src/services/discord/turn_lease.rs` `release_for_hold_bound`, `src/services/discord/queue_io/retained_notice.rs` released/waiting 안내 | 6 | +470 |
| S2 i8343 무유실 | `OB` `source_hold(state, active_turn)`와 호출 3곳(R1), `retained_notice.rs` member 목록 본문(R3), `src/services/turn_orchestrator/queued_head/v2/readiness_hold.rs`·`src/services/turn_orchestrator/queued_head/v2/refused.rs`·`src/services/turn_orchestrator/queued_head/v2/retained.rs`·`src/services/turn_orchestrator/queued_head/v2/start_failure.rs`·`src/services/turn_orchestrator/queued_head/v2/claude_handoff.rs` 제거 전 member ID·발췌 수집 | 7(retained_notice는 S1과 중복) | +150 |
| S3 행만 남은 모양 | `src/services/discord/queue_io/input_owner/hold_bound/row_only.rs` (제안) (R4 보류화, R5a·R5b 근거, R5c 순서·wake), OI:1121 호출 한 줄 교체, routine run 결합 판독(`routines/store` 쿼리 1개), `headless_turn/process_mailbox.rs` retained 분기 producer join·receipt, `inflight/model.rs` receipt 필드 1개, `turn_finalizer.rs` 행 전용 변형·mute 한 줄, 새 `src/services/discord/turn_finalizer/row_end.rs` (제안) | 7 | +330 |
| 합계 | | **19파일**(중복 제외) | **+950** |

모든 조각이 상한(30파일/+1500)과 PLAN_READY 정지선(+1200) 안에 든다. 합계도 +1200 안이다. giant에는 새 책임을 얹지 않는다. OI(1795)는 호출 한 줄, turn_finalizer.rs(4803)는 enum 변형 하나와 mute 판정 한 줄, inflight/model.rs(1446)는 필드 하나만 바꾸고, 본체는 새 child(`hold_bound*`, `src/services/discord/turn_finalizer/row_end.rs` (제안))에 둔다. OB(813)·drive.rs(227)·turn_lease.rs(340)·retained_notice.rs(173)·process_mailbox.rs(294)는 1000줄 미만이다.

시험(→ 상태 차트 cfg):
- T1(이슈 1) terminal 미제출 + inflight·토큰 잔존 + Herdr Idle → 180초 + 3회 Idle 뒤 `input_hold_bound_released`, front 1회 전달, notice 1건 (`TargetIncident`)
- T2(이슈 2) 같은 모양 + 사람 초안 / 모달 / 작업 중(transcript 열림, 도구 사이 Idle처럼 보임) → 발동 0, waiting 안내 1건 (`TargetDraft`·`TargetModal`·`TargetRunning`, `MutNoIdle*`·`MutNoTranscript` RED)
- **T2c(P1-1)** herdr Codex·tmux Codex 두 호스트 × {텍스트 초안, 모달, 이미지 전용 초안(`attachments` ∧ `Composer::Empty`)}에서 종료 rollout·잔여 lease·같은 front를 고정하고 각각 3회 관측 → 정리 0, lease·행 보존, 전달 0. 차단 요소를 지우면 정리 1·전달 1. tmux Codex에서 `judge_in`=Idle이어도 화면 gate가 음성이면 정리 0 (`TargetImageDraft`, `MutNoScreenGate`·`MutNoAttachments` RED)
- T3(이슈 3) 상한 정리 직후 QueueEligible drain과 held_watch wake가 겹쳐도 전달 1회, `head_injection` 있는 front에는 발동 0 (`MutNoDedupe` RED)
- T4 재시작 뒤: 시계 0부터, lease 재등록 뒤 다시 180초 후 발동 (`TargetRestart`)
- T5 관측 실패: capture 실패·`TuiTurnState::Unknown`·`Composer::Unread`가 끼면 횟수 0, 발동 0, 회복 뒤 3회 연속이면 발동 (`TargetFlicker`)
- T6 늦은 terminal 제출: 정리 뒤 옛 `Complete`가 오면 `AlreadyFinalized`, 새 턴 lease 유지, 답 재게시 0(relay watermark 리셋 없음) (`TargetLate`, `MutNoLedgerFence` RED)
- T7 id 0 lease: 발동 거절 `declined reason=unbound_message_identity` + waiting 안내 1건
- T8(i8343) `!clear` 직후 source 미pin + busy 보류 → 기한이 생기지 않고(R1), 턴 A 끝 뒤 cold start 첫 키가 거부되지 않고 1회 전달. 10초가 지나도 버림 0 (`TargetClearDrop` + 가드 `R1NoDueWhileLeased`, `CurrentClearDrop` RED, `MutNoR1` RED)
- T9(i8343·R2) idle 경로 Readiness 기한 버림(초안이 남은 채 10초)은 그대로 일어나고 안내에 member 목록이 들어간다 (`TargetIdleDue`, `CurrentIdleDue` RED)
- **T9b(P2-5)** 서로 다른 A·B를 한 unit으로 묶어 Readiness 만료와 `refuse_not_sent`를 각각 실행 → 안내 1건에 A·B 둘 다. drain 재실행·재시작 뒤에도 안내 1건 (`MutFirstOnly` RED)
- T10(i8372) 토큰 없음 + Managed 행 + run `succeeded` + producer receipt → foreign_row 거절이 보류로 기록되고, 상한 뒤 행 정리 + 전달 1회 (`TargetRowOnly`, `CurrentRowOnly` RED)
- **T10b(P2-2)** run은 종료 상태지만 producer 생존(receipt 없음) → 정리 0. receipt가 생기면 정리 1·전달 1. 다른 attempt·조회 실패·행 없음 → 정리 0 (`TargetRunLive`, `MutRunOnly` RED)
- **T10c(P2-3)** 토큰 없이 Pending 원장 + Managed 행 fixture, `MailboxReleased`가 한 번도 나오지 않아도 정리 뒤 front 1회 전달. 정산이 완료 반응·성공 전달 증거를 만들지 않음 (`MutNoWake`·`MutGateTimeout` RED)
- **T10d(P2-4)** 토큰 없는 Managed·`ExternalInput`(watcher 재획득) pane 행: Idle이면 정리·전달 1회, Busy·Unknown이면 정리 0 (`TargetRowPane`, `CurrentRowPane` RED, `MutManagedOnly` RED)
- **T10e(P2-4)** actor incarnation 교체: 옛 actor handle로 시작한 check가 새 actor의 lease·행을 정리하지 않음(`declined reason=incarnation_replaced`)
- T11 상한 정리 중 새 턴이 lease를 잡음 → `lease_changed`, 정리 0, 새 턴 행 보존
