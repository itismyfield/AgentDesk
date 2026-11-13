# 사용자 기능 지도

생성: `python3 scripts/generate_user_feature_map.py --write --base-ref origin/main`

수기 정본: [user-feature-map.rows.yaml](user-feature-map.rows.yaml). 이 지도는 live **명세** 연결이며 실행 PASS·최근성 증거가 아니다.
fixture·known-gap·retired와 미구현 witness는 live coverage로 세지 않는다. `unspecified` output은 실제 출력 경로를 증명하지 않는다.

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

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | campaign_progress_observed | — | unsupported |

- `campaign_progress_observed` (#8120; human): 캠페인 실행 증거와 사용자 표시를 연결하는 단언 미구현이다.

## cancel-stop

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | cancel_effect_observed, cancel_preservation_observed | — | unsupported |

- `cancel_effect_observed` (#8120; human): 중지 효과와 인간 입력 보존의 실행 관측 단언이 없다.
- `cancel_preservation_observed` (#8120; human): 중지 효과와 인간 입력 보존의 실행 관측 단언이 없다.

## dm

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"dm"} | dm_surface_observed | — | unsupported |

- `dm_surface_observed` (#8120; human): 초기 phase-1은 channel/thread만 지원하므로 DM 실제 실행과 단언이 미지원이다.

## effort-model

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | effort_model_effect_observed | — | unsupported |

- `effort_model_effect_observed` (#8120; human): UI 표시와 실제 다음 턴 설정을 잇는 단언 미구현이다.

## herdr-handoff

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | host_handoff_observed | — | unsupported |

- `host_handoff_observed` (#8120; human): label이나 cell 선언만으로 실제 endpoint·session의 넘김을 관측하지 않는다.

## message-delivery

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-50 | covered |
| {"host":"herdr","provider":"codex","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-51 | covered |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-50 | covered |
| {"host":"tmux","provider":"codex","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"} | no_duplicate_content, no_duplicate_marker, provider_text_present | E-51 | covered |

## reaction-display

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"herdr","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |
| {"host":"tmux","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"} | reaction_history_matches, state_busy_observed | — | unsupported |

- `reaction_history_matches` (#8113 / #8120; human): message/actor별 전체 반응 이벤트와 실제 busy witness를 아직 관측하지 않는다.
- `state_busy_observed` (#8113 / #8120; human): message/actor별 전체 반응 이벤트와 실제 busy witness를 아직 관측하지 않는다.

## reset-clear

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | clear_ack_observed, fresh_session_observed | — | unsupported |

- `clear_ack_observed` (#8120; human): E-50/E-51의 ack wait는 assertions 교집합이 아니며 새 세션 witness도 미구현이다.
- `fresh_session_observed` (#8120; human): E-50/E-51의 ack wait는 assertions 교집합이 아니며 새 세션 witness도 미구현이다.

## routines

| 적용 조합·qualifier/source | 필수 단언 | live 명세 | 상태 |
| --- | --- | --- | --- |
| {"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"idle","surface":"channel"} | routine_fired_observed | — | unsupported |

- `routine_fired_observed` (#8120; human): 시각·실행 identity·전달 결과를 연결하는 명세 단언 미구현이다.

## threads

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

source_hash: `18edd40f135f23b65d5b62167b2762bd5dcc2686a547526fc4cd05e4923f2dfc`
