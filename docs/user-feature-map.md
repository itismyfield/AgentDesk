# 사용자 기능 지도

생성: `python3 scripts/generate_user_feature_map.py --write --base-ref origin/main`

수기 정본: [user-feature-map.rows.yaml](user-feature-map.rows.yaml). 이 지도는 live **명세** 연결이며 실행 PASS·최근성 증거가 아니다.
fixture·known-gap·retired와 미구현 witness는 live coverage로 세지 않는다. `unspecified` output은 실제 출력 경로를 증명하지 않는다.

기존 시험 이름은 소스 정의 참조이며 실행/PASS가 아니다. 부분 시나리오 references와 needs_live는 covered 수를 늘리지 않는다.

적용 23 · covered 4 · 빈 칸 19 · 미지원 단언 칸 19
시나리오 분류: {"active": 43, "fixture": 2, "live": 38, "retired": 3, "unsupported-known-gap": 3}

| ID / 기능 | 사용자에게 보이는 계약 | 확인 명령 | 적용 / covered / 빈 칸 | 담당 / 차트 |
| --- | --- | --- | --- | --- |
| busy-input / busy 중 입력 주입 | 현재 턴 종료 전 후속 입력이 실제 host에 정확히 한 번 주입되고 수신→submit 지연이 상한 안이다. Claude Herdr 우선, tmux·Codex 대응군. | 독립 instance receipt 지정 E2E: busy-input; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 4 / 0 / 4 | src/services/discord/router/intake_gate/busy_inject.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| campaigns / 캠페인 | 캠페인 진행과 결과가 실제 수행한 작업·실패·대기 상태를 구분해 보인다. | 독립 instance receipt 지정 E2E: campaigns; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| cancel-stop / 취소·중지 | 중지 뒤 추가 본문이 오지 않고 인간 입력은 취소 보존 계약대로 남는다. | 독립 instance receipt 지정 E2E: cancel-stop; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services/discord/commands / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| dm / DM 대화 | DM 입력의 올바른 provider 응답과 대화 격리를 확인한다. | 독립 instance receipt 지정 E2E: dm; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services/discord / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| effort-model / effort·model 변경 | 선택한 effort/model이 다음 턴의 실제 provider 설정에 적용되고 표시와 일치한다. | 독립 instance receipt 지정 E2E: effort-model; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services/discord/commands / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| herdr-handoff / Herdr 넘김 | 넘긴 세션의 실제 host가 Herdr이며 후속 응답과 취소가 같은 세션으로 이어진다. | 독립 instance receipt 지정 E2E: herdr-handoff; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services/herdr_launch.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| message-delivery / 메시지 전달 | 입력별 provider 본문 marker가 순서대로 정확히 한 번 도착한다. E-50/E-51의 공통 전달 명세이며 인간 인증·병합·보존·clear ack·구체 output path는 증명하지 않는다. | 독립 instance receipt 지정 E2E: message-delivery; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 4 / 4 / 0 | src/services/discord / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| reaction-display / 입력 반응 표시 | tmux/Claude managed busy 입력의 ➕ 반응은 한 번이고 📬 추가·제거는 없다. Herdr·Codex를 대응군으로 유지한다. | 독립 instance receipt 지정 E2E: reaction-display; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 4 / 0 / 4 | src/services/discord/router/intake_queue_transaction.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| reset-clear / reset·clear | clear acknowledgement와 새 세션 identity, 이후 첫 응답을 함께 확인한다. wait step과 marker만으로 ack 전체를 세지 않는다. | 독립 instance receipt 지정 E2E: reset-clear; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services/discord/commands / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| routines / 루틴 | 예약된 루틴이 지정 시각·대상에 한 번 실행되고 결과가 보인다. | 독립 instance receipt 지정 E2E: routines; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 1 / 0 / 1 | src/services / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |
| threads / 새 스레드·host·출력 | preferred node ready·v1 slots 없음에서 새 thread 첫 입력이 시작된다. 실제 host·source binding·tui_o 출력·후속/취소를 확인하고 source not bound를 실패로 본다. | 독립 instance receipt 지정 E2E: threads; 선언 검사: python3 scripts/generate_user_feature_map.py --check --base-ref origin/main | 4 / 0 / 4 | src/services/cluster/intake_router_hook/placement.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 |

## busy-input

needs_live: `true`


| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | busy_injected_before_turn_end, injection_latency_within, reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"herdr","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | busy_injected_before_turn_end, injection_latency_within, reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | busy_injected_before_turn_end, injection_latency_within, reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"tmux","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | busy_injected_before_turn_end, injection_latency_within, reaction_history_matches, state_busy_observed | — | unsupported |

- `busy_injected_before_turn_end` (#8114 / #8120; human): 실제 busy·host submit·provider 관측·반응 이력 단언이 미구현이다. 내부 API 성공은 대체 증거가 아니다.
- `injection_latency_within` (#8114 / #8120; human): 실제 busy·host submit·provider 관측·반응 이력 단언이 미구현이다. 내부 API 성공은 대체 증거가 아니다.
- `reaction_history_matches` (#8114 / #8120; human): 실제 busy·host submit·provider 관측·반응 이력 단언이 미구현이다. 내부 API 성공은 대체 증거가 아니다.
- `state_busy_observed` (#8114 / #8120; human): 실제 busy·host submit·provider 관측·반응 이력 단언이 미구현이다. 내부 API 성공은 대체 증거가 아니다.

## campaigns

needs_live: `true`

- 기존 시험 정의: `src/db/campaigns/tests.rs::postgres_live_status_reads_the_issue_card_without_touching_the_ledger_pg`
- 기존 시험 정의: `src/server/routes/server_campaigns/http_tests.rs::public_execution_is_unavailable_without_legacy_queue_or_card_effects_pg`
- 기존 시험 정의: `src/services/campaign_execution/view_tests.rs::planned_or_claimed_work_is_not_displayed_as_actual_execution`

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | campaign_progress_observed | — | unsupported |

- `campaign_progress_observed` (#8120; human): legacy live-status와 상태 projection 시험은 main에 있다. server execution은 Unavailable이며 API/view 시험은 tmux/Claude/human/tui_o 실행을 증명하지 않으므로 실제 campaign 진행 관측은 pending이다.

## cancel-stop

needs_live: `true`

- 기존 시험 정의: `src/services/discord/router/intake_queue_transaction.rs::only_human_intake_sets_cancel_preservation_marker`
- 기존 시험 정의: `src/services/discord/turn_bridge/tmux_runtime/herdr_http_cancel_tests.rs::herdr_http_cancel_stops_a_live_turn_and_completes_nothing_pg`
- 기존 시험 정의: `src/services/discord/turn_bridge/tmux_runtime/native_stop_contract_tests.rs::preserve_stop_failed_command_notice_once_and_reader_continues`
- 부분 시나리오 `E-18` (needs_live): Claude/Codex의 cancel 뒤 late marker 부재와 hold/health 명세를 참조한다. 입력은 legacy_automation이며 인간 입력 취소보존과 구체 tui_o output receipt를 증명하지 않는다. Herdr의 durable hold·legacy idle 검사는 N/A이고 자동 busy witness operation도 없다.
- 부분 시나리오 `E-54` (needs_live): Claude tmux의 late marker 부재와 idle !stop 답글 정확히 1회 UX만 참조한다. legacy_automation/unspecified 입력·출력이므로 human/tui_o 수기 계약과 다르며 인간 입력 보존·자동 busy witness는 증명하지 않는다.

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | cancel_effect_observed, cancel_preservation_observed | — | unsupported |

- `cancel_effect_observed` (#8120; human): 중지·인간 보존 정책과 기존 단위시험은 main에 있다. E-18/E-54의 부분 명세는 인간 Gateway 입력 보존·실제 host/output witness까지 증명하지 않으므로 해당 실행 단언은 pending이다.
- `cancel_preservation_observed` (#8120; human): 중지·인간 보존 정책과 기존 단위시험은 main에 있다. E-18/E-54의 부분 명세는 인간 Gateway 입력 보존·실제 host/output witness까지 증명하지 않으므로 해당 실행 단언은 pending이다.

## dm

needs_live: `true`

- 기존 시험 정의: `src/services/discord/outbound/send_gate.rs::dm_default_agent_allows_headless_private_channel_when_provider_bound`
- 기존 시험 정의: `src/services/discord/queue_io/transport/tests.rs::rest_queue_routing_preserves_thread_parent_and_dm_policy`
- 기존 시험 정의: `src/services/message_outbox/identity_tests.rs::dm_session_routes_outbox_delivery_through_provider_bot`

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"dm"} | dm_surface_observed | — | unsupported |

- `dm_surface_observed` (#8120; human): 인간 DM routing·provider bot 분리 단위시험은 main에 있다. 기존 TUI runner에는 DM 전용 시나리오가 없고 인간 Gateway→실제 tmux/tui_o 응답 관측은 pending이다.

## effort-model

needs_live: `true`

- 기존 시험 정의: `scripts/e2e/tui_relay/test_driver_health.py::E37CodexModelLocalControl.test_passes_when_model_stays_local_and_prompt_with_empty_sources_is_admitted`
- 기존 시험 정의: `src/services/claude_tui/session/effort_launch_tests.rs::ultra_persists_and_normal_effort_turns_ultracode_off`
- 기존 시험 정의: `src/services/discord/commands/config/effort_settings_tests.rs::codex_effort_persists_and_restores_on_its_channel`
- 부분 시나리오 `E-37` (needs_live): Codex /model의 local-control 안내 뒤 일반 입력 admission·응답 정확히 1회 명세만 참조한다. legacy_automation 입력이며 현재 Claude/human 수기 칸과 provider/source가 다르다. 구체 output profile과 실제 model/effort 선택·적용 효과는 관측하지 않는다.

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | effort_model_effect_observed | — | unsupported |

- `effort_model_effect_observed` (#8120; human): effort/model 저장·다음 턴 전달 시험은 main에 있다. E-37은 Codex local-control 이후 입력 처리만 확인하므로 현재 Claude/human/tui_o 칸의 실제 설정 적용 witness는 pending이다.

## herdr-handoff

needs_live: `true`

- 기존 시험 정의: `scripts/e2e/tui_relay/test_herdr_observe.py::HerdrObserve.test_cold_and_post_turn_row_contract`
- 기존 시험 정의: `src/services/herdr_launch_tests.rs::herdr_launch_commits_pending_then_marker_then_one_create_and_fills_that_nonce_pg`
- 기존 시험 정의: `src/services/herdr_launch_tests.rs::herdr_launch_gate_needs_an_owned_ready_channel_with_a_checkpoint_and_changes_no_store`
- 부분 시나리오 `E-19` (needs_live): Claude/Codex의 재시작 전후 문맥·응답 연속성을 참조한다. 입력은 legacy_automation, output_path는 unspecified다. Herdr에서 tmux identity capture/preservation step은 N/A이며 실제 Herdr host·동일 세션 취소까지 증명하지 않는다.
- 부분 시나리오 `E-30` (needs_live): Codex의 연속 후속 응답·중복 부재 명세만 참조한다. 입력은 legacy_automation, output_path는 unspecified다. 현재 수기 칸은 Claude이므로 provider가 다르고, Herdr에서는 tmux identity step이 N/A여서 실제 host·동일 세션·취소를 증명하지 않는다.

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | host_handoff_observed | — | unsupported |

- `host_handoff_observed` (#8120; human): Herdr launch 및 row/nonce readiness 단위시험은 main에 있다. E-19/E-30 identity 단계는 Herdr N/A이므로 실제 endpoint·session 넘김 witness는 pending이다.

## message-delivery

needs_live: `true`

- 기존 시험 정의: `scripts/e2e/tui_relay/test_assertions.py::ProviderAuthorScenarios.test_every_turn_wait_and_e36_request_is_provider_scoped`

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-50 | covered |
| {"host":"herdr","provider":"codex","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-51 | covered |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-50 | covered |
| {"host":"tmux","provider":"codex","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-51 | covered |

## reaction-display

needs_live: `true`


| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"herdr","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"tmux","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |

- `reaction_history_matches` (#8113 / #8120; human): message/actor별 전체 반응 이벤트와 실제 busy witness를 아직 관측하지 않는다.
- `state_busy_observed` (#8113 / #8120; human): message/actor별 전체 반응 이벤트와 실제 busy witness를 아직 관측하지 않는다.

## reset-clear

needs_live: `true`

- 기존 시험 정의: `scripts/e2e/tui_relay/test_herdr_observe.py::HerdrTextClear.test_clear_completion_shares_the_refusal_boundary`
- 기존 시험 정의: `src/services/discord/commands/control/clear_end_tests.rs::a_clear_ends_a_running_tmux_session_and_forgets_its_provider_session_pg`
- 기존 시험 정의: `src/services/discord/commands/control/herdr_recovery_tests.rs::live_clear_forgets_the_retired_selector_pg`
- 부분 시나리오 `E-50` (needs_live): Claude의 clear 뒤 첫 provider 응답·중복 부재 명세를 참조한다. ack는 wait step이며 clear_ack/fresh-session assertion은 없다. legacy_automation/unspecified이므로 현재 human/tui_o 수기 계약과 source/output이 다르고 세션 identity를 증명하지 않는다.
- 부분 시나리오 `E-51` (needs_live): Codex의 clear 뒤 첫 provider 응답·중복 부재 명세를 참조한다. ack는 wait step이며 clear_ack/fresh-session assertion은 없다. 현재 Claude/human/tui_o 수기 칸과 provider/source/output이 다르며 실제 새 세션 identity를 증명하지 않는다.

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | clear_ack_observed, fresh_session_observed | — | unsupported |

- `clear_ack_observed` (#8120; human): clear 처리와 selector 정리 시험은 main에 있다. E-50/E-51의 ack wait는 assertions 교집합이 아니며 새 세션 identity witness도 없으므로 두 실행 단언은 pending이다.
- `fresh_session_observed` (#8120; human): clear 처리와 selector 정리 시험은 main에 있다. E-50/E-51의 ack wait는 assertions 교집합이 아니며 새 세션 identity witness도 없으므로 두 실행 단언은 pending이다.

## routines

needs_live: `true`

- 기존 시험 정의: `src/services/routines/discord_log.rs::render_run_log_sections_keeps_stable_order_and_sanitizes_code_fences`
- 기존 시험 정의: `src/services/routines/reliability_pg_tests.rs::logged_unicode_object_completion_and_next_due_run_persist_pg`
- 기존 시험 정의: `src/services/routines/reliability_pg_tests.rs::slot_missed_during_startup_grace_runs_exactly_once_after_it_pg`

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | routine_fired_observed | — | unsupported |

- `routine_fired_observed` (#8120; human): AgentDesk due claim·중복 방지·결과 로그 시험은 main에 있다. 실행 provenance는 routine이며 인간 입력 source와 다르다. E-13 ScheduleWakeup은 별개라 실제 routine/run/host/output 관측은 pending이다.

## threads

needs_live: `true`


| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"bound","turn_origin":"managed"},"state":"busy","surface":"thread"} | actual_host_observed, fresh_thread_node_observed, output_binding_observed, state_busy_observed, surface_thread_observed | — | unsupported |
| {"host":"herdr","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"unowned","turn_origin":"managed"},"state":"idle","surface":"thread"} | actual_host_observed, fresh_thread_node_observed, output_binding_observed, surface_thread_observed | — | unsupported |
| {"host":"tmux","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"bound","turn_origin":"managed"},"state":"busy","surface":"thread"} | actual_host_observed, fresh_thread_node_observed, output_binding_observed, state_busy_observed, surface_thread_observed | — | unsupported |
| {"host":"tmux","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"unowned","turn_origin":"managed"},"state":"idle","surface":"thread"} | actual_host_observed, fresh_thread_node_observed, output_binding_observed, surface_thread_observed | — | unsupported |

- `actual_host_observed` (#8115 / #8118 / #8120; human): fresh node/session·실제 endpoint/pane·출력 binding witness가 미구현이다. E-14의 기존 thread 재부착은 대체 불가.
- `fresh_thread_node_observed` (#8115 / #8118 / #8120; human): fresh node/session·실제 endpoint/pane·출력 binding witness가 미구현이다. E-14의 기존 thread 재부착은 대체 불가.
- `output_binding_observed` (#8115 / #8118 / #8120; human): fresh node/session·실제 endpoint/pane·출력 binding witness가 미구현이다. E-14의 기존 thread 재부착은 대체 불가.
- `state_busy_observed` (#8115 / #8118 / #8120; human): fresh node/session·실제 endpoint/pane·출력 binding witness가 미구현이다. E-14의 기존 thread 재부착은 대체 불가.
- `surface_thread_observed` (#8115 / #8118 / #8120; human): fresh node/session·실제 endpoint/pane·출력 binding witness가 미구현이다. E-14의 기존 thread 재부착은 대체 불가.

source_hash: `5c20cfe92466aec85d982e99c0d0b10628da2fe983a4f0b07f4d066a92218a19`
