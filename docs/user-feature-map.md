# 사용자 기능 지도

생성: `python3 scripts/generate_user_feature_map.py --write --base-ref origin/main`

수기 정본: [user-feature-map.rows.yaml](user-feature-map.rows.yaml). 이 지도는 live **명세** 연결이며 실행 PASS·최근성 증거가 아니다.
fixture·known-gap·retired와 미구현 witness는 live coverage로 세지 않는다. `unspecified` output은 실제 출력 경로를 증명하지 않는다.

기존 시험 이름은 소스 정의 참조이며 실행/PASS가 아니다. 부분 시나리오 references와 needs_live는 covered 수를 늘리지 않는다.

적용 23 · covered 4 · 빈 칸 19 · 미지원 단언 칸 19
시나리오 분류: {"active": 44, "fixture": 2, "live": 39, "retired": 3, "unsupported-known-gap": 3}

호스트 동작은 [`cd9951d3ca`](https://github.com/itismyfield/AgentDesk/tree/cd9951d3cac6a4f0b81d02789facabbc6ed2a55d)에서 확인한 코드 기준이다.

| ID / 기능·범위 | 사용자에게 보이는 계약 | 확인 명령 | 적용 / covered / 빈 칸 | 담당 / 차트 | tmux 동작 | Herdr 동작 | 판정 / 담당 | 근거 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| busy-input / busy 중 입력 주입<br>사용자 입력이 진행 턴에 합류 | 현재 턴 종료 전 후속 입력이 실제 host에 정확히 한 번 주입되고 수신→submit 지연이 상한 안이다. Claude Herdr 우선, tmux·Codex 대응군. | 독립 instance receipt 지정 E2E: busy-input; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 4 / 0 / 4 | src/services/discord/router/intake_gate/busy_inject.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 공통 busy owner에서 source를 확인, submit 후 결과 정산 | 같은 경로와 exact pane/source pin으로 현재 턴에 합류. 증거 부족 시 보류 | 같음 · #8114 실제 Herdr Claude 사용 확인으로 종료. 양쪽 provider·native/managed 지연과 전량 확인 #8120 | `src/services/discord/router/intake_gate/busy_inject.rs:82,279,298,307,369`, `src/services/discord/router/message_handler/intake_turn.rs:161`, `src/services/discord/health/turn_deliver/inject/herdr.rs:165`<br>`{"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}`<br>`{"host":"herdr","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}`<br>`{"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}`<br>`{"host":"tmux","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}` |
| busy-input / 사람이 터미널에서 먼저 시작한 턴 |  |  |  |  | native source를 관측한 뒤 busy owner 합류 | #8114 공통 operation의 Herdr source 사용. source 미확인 또는 pane 교체는 보류 | 같음 · exact source를 확인한 턴의 계약. managed 증거를 native GREEN으로 환산하지 않으며 #8120 native witness 필요 | `src/services/discord/turn_presence/activity/unbound.rs:305`, `src/services/discord/turn_presence/activity/unbound/herdr_bound.rs:18,58`, `src/services/discord/router/intake_gate/busy_inject.rs:307` |
| campaigns / 캠페인<br>캠페인 진행·승인·실행 투영 | 캠페인 진행과 결과가 실제 수행한 작업·실패·대기 상태를 구분해 보인다. | 독립 instance receipt 지정 E2E: campaigns; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 공통 ledger·state view, 작업·실패·대기 표시 | 동일 컴포넌트. host worker의 배치와 실제 실행 유무는 별도 | 같음 · 관측 surface 계약. #7438 worker·activation 및 #8120 live | `src/services/campaign_execution/view.rs:75`, `src/server/routes/server_campaigns/http_tests.rs:1083`. 시작·취소와 API 대기의 실제 상태 전이 계약은 #7438 |
| cancel-stop / 취소·중지<br>활성 턴의 `/stop`·`!stop`·HTTP Cancel | 중지 뒤 추가 본문이 오지 않고 인간 입력은 취소 보존 계약대로 남는다. | 독립 instance receipt 지정 E2E: cancel-stop; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services/discord/commands / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | turn/token fence와 취소 보존·중단 | 동일 대상 정산 계약과 Herdr holder의 중지. actual target이나 nonce 확인 실패 시 거절 | 같음 · 활성 채널 턴의 계약. 걸린 턴은 별도 행. 자식 host 판정 #8118, 사람 입력 보존 실기 #8120 | `src/services/discord/commands/stop.rs:84,98`, `src/services/discord/commands/control.rs:673,745`, `src/server/routes/domains/runtime.rs:123`. 부팅 복원 `src/services/discord/runtime_bootstrap/queued_recovery.rs:210` |
| cancel-stop / `/stop` 걸린 턴 해제 |  |  |  |  | 취소된 토큰이 남고 inflight가 없으면 동일 실행의 종료를 검증한 뒤 해당 lease를 정산하고 대기 입력을 시작 | Herdr holder 응답 뒤 해당 후처리를 건너뜀. 일반 활성 턴 중단과 수동 lease 해제는 기존 경로로 가능 | 다름 · #8128 Z. #7583 S07c의 stop 호출부, holder 변경이 필요할 때 S07f와 조율 | `src/services/discord/commands/stop.rs:69–75,178–195`, `src/services/discord/zombie_foreground_release.rs:185–212,355–411,432–442`. 동일 시나리오에서 취소된 exact 토큰과 종료 증거가 있으면 pane·native ID·큐를 보존하면서 다음 입력이 진행한다. 새 토큰·활성 턴·불명확한 증거에서는 lease를 유지한다 |
| cancel-stop / 수동 lease 확인·해제 API 및 CLI |  |  |  |  | `GET /api/turn-lease/{provider}/{channel_id}` 또는 `agentdesk turn-lease inspect`에서 exact lease를 확인. `POST /api/turn-lease/release` 또는 `agentdesk turn-lease release`에 그 identity와 사유를 제출 | 동일 API와 CLI가 고유 런타임 owner를 찾고 exact lease를 정산. 응답의 `provider_terminal_confirmed`는 `false`이며 운영자가 종료를 확인하는 기존 계약을 사용 | 같음 · 기존 수동 정산 유지. Z는 `/stop`의 확인된 종료 증거와 공통 exact 정산을 사용 | `src/server/routes/domains/runtime.rs:17–20`, `src/server/routes/turn_lease.rs:15,33`, `src/cli/turn_lease.rs:4–28,44–55`, `src/services/discord/turn_lease/registry.rs:15–37`, `src/services/discord/turn_lease.rs:148–166,232–241,272–325` |
| cancel-stop / API `POST /api/agents/{id}/turn/stop` |  |  |  |  | 활성 세션을 골라 queue 보존 stop을 실행하고 결과 응답 | `host_unsupported`가 있으면 중단 전 409 `session_host_not_tmux` | 다름 · #8128 H는 현재 Herdr holder 중단 포트를 그대로 소비해 독립 착수 가능. holder 변경은 #7583 S07f | `src/server/routes/domains/runtime.rs:138`, `src/server/routes/agents_stop.rs:4,28,41,68,99,108`, `src/services/queue/herdr_stop.rs:13`, `src/services/discord/commands/stop.rs:98` |
| cancel-stop / UI·API Terminate/force-kill |  |  |  |  | exact row 및 tmux fence에서 세션 종료 | 별도 exact hosted row·pane identity 검증 후 Herdr 종료. 거절·미확인은 별도 결과 | 같음 · 명시 권한·대상이 검증된 경우에 종료한다는 계약. lifecycle trait 수렴은 #7583 | `src/server/routes/domains/runtime.rs:79–84,103`, `src/services/dispatched_sessions/force_kill.rs:43,185`, `src/services/dispatched_sessions/herdr_kill.rs:21`, `src/services/discord/herdr_terminate.rs:412,448,692` |
| cancel-stop / `/cancel-queued` |  |  |  |  | 큐에서 exact 입력 ID의 취소 결과 | 동일 owner 큐 절차 | 같음 · #8120 실기 | `src/services/discord/commands/control.rs:771`, `src/services/discord/commands/command_policy.rs:213`. stop/clear와 별도 queued item transition |
| dm / DM 대화<br>DM 대화·target 격리 | DM 입력의 올바른 provider 응답과 대화 격리를 확인한다. | 독립 instance receipt 지정 E2E: dm; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services/discord / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | private channel/provider agent 권한 및 outbound bot 분리 | 공통 DM 게이트. host와 무관하게 같은 발신/격리 규칙을 사용 | 같음 · 공통 권한 및 출력 정책. 각 host의 DM/Gateway 실제 실행은 #8120 | `src/services/discord/outbound/send_gate.rs:107,220,334`, `src/services/discord/queue_io/transport.rs:15,23`, `src/services/message_outbox/identity_tests.rs:96`, `src/server/routes/domains/runtime.rs:146` |
| effort-model / effort·model 변경<br>Claude `/model` | 선택한 effort/model이 다음 턴의 실제 provider 설정에 적용되고 표시와 일치한다. | 독립 instance receipt 지정 E2E: effort-model; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services/discord/commands / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 저장 후 필요 시 동일 대화로 TUI 재기동 | 저장 경로, active pane 즉시 적용을 위한 relaunch 조건은 제외 | 다름 · #8128 R2b, #8109 적용 경로 | `src/services/discord/commands/config/provider_settings.rs:182,190,196,229,272`, `src/services/discord/commands/control/relaunch.rs:343`. clear/stop 시 pending 전이 확인 |
| effort-model / Claude `/effort`·`/fast` |  |  |  |  | 저장·apply 의도 반영. 없는 live pane은 다음 실행 적용 분기 | live pane 직접 적용. live pane 없음의 거절·저장 차이 확인 대상 | 다름 · #8109. 실제 reset이 필요한 fast 설정의 소비는 #8128 R2b | `src/services/discord/commands/tui_passthrough.rs:80,341`, `src/services/discord/commands/fast_mode.rs:54,107`, `src/services/discord/commands/tui_passthrough/herdr_settings.rs:183,282` |
| effort-model / Codex 메시지 본문 `/model` |  |  |  |  | Codex local 컴포저 안내 | 같은 local 컴포저 안내 | 같음 · #8120 E-37은 안내 뒤 입력 admission의 부분 명세 | `src/services/discord/router/message_handler/pre_admission_control.rs:129,133`, `tests/e2e/tui_relay/scenarios/E-37-codex-model-local-control.yaml:14` |
| effort-model / Codex `/effort`·ultra |  |  |  |  | effort 저장 및 후속 적용 경로 | Herdr `/effort` 진입은 live Claude만 허용하므로 거절 | 다름 · Herdr Codex effort 진입과 적용 #8109 | `src/services/discord/commands/config/effort.rs:122,142,167,219`, `src/services/discord/commands/tui_passthrough.rs:341`. 설정 소비 `src/services/discord/commands/control.rs:225` |
| effort-model / Codex `/goals`·실제 reset pending 설정 |  |  |  |  | 저장→pending 확인→provider session reset 소비 | 저장·pending 생산은 공통, managed reset 거절 시 효과 미적용 | 다름 · #8128 R2b | `src/services/discord/commands/goals.rs:24,68`, `src/services/discord/commands/config/provider_settings.rs:340,378`, `src/services/discord/commands/control.rs:225`, `src/services/discord/commands/control/provider_reset.rs:16` |
| herdr-handoff / Herdr 넘김<br>tmux→Herdr 넘김 | 넘긴 세션의 실제 host가 Herdr이며 후속 응답과 취소가 같은 세션으로 이어진다. | 독립 instance receipt 지정 E2E: herdr-handoff; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services/herdr_launch.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 기존 승인·resume 근거에 따라 목적 Herdr로 넘김 | 정확한 Herdr 재입력·취소 및 owner 승인 상태 정산 | 같음 · 검증된 channel의 세션 연속 계약, #8077 구현. 양쪽 실제 세대 및 Cancel 입증 #8120 | `src/services/herdr_launch.rs:61,405`, `src/services/discord/router/herdr_consent.rs:128,206,388`, `src/services/discord/commands/stop.rs:98`. E-19/E-30의 기존 identity는 Herdr에 그대로 적용할 수 없음 |
| herdr-handoff / 사용자 소유 Herdr 서버 동의/복귀 |  |  |  |  | 외부 Herdr 서버 승인 대상 없음 | `!herdr consent retry/return`과 부팅 recovery로 승인·복귀 | 불가피(사용자 소유 Herdr 서버 접근 동의) · 사용자 확인 미기록 · #8128의 사용자 소유 서버 동의 경계 | `src/services/discord/commands/command_policy.rs:110`, `src/services/discord/router/herdr_consent.rs:31,128,206,388`, `src/services/discord/router/herdr_consent/return_input.rs:8,47,160`. #8118 child 대상 상속 |
| herdr-handoff / 터미널 UI 배치 |  |  |  |  | tmux 세션·pane 노출 | logical child별 Herdr workspace·tab·pane 노출 | 불가피(서로 다른 앱의 native UI 배치) · 사용자 확인 미기록 · #7583의 workspace UX 제품 결정 | `src/services/herdr_launch.rs:374`, `src/services/herdr_launch/prepared.rs:11`, [상속 설계 §5](session-host-parity.design.md#5-사용자-결정-필요남길-차이) |
| message-delivery / 메시지 전달<br>Discord 채널 첫 입력·idle 후속 | 입력별 provider 본문 marker가 순서대로 정확히 한 번 도착한다. E-50/E-51의 공통 전달 명세이며 인간 인증·병합·보존·clear ack·구체 output path는 증명하지 않는다. | 독립 instance receipt 지정 E2E: message-delivery; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 4 / 4 / 0 | src/services/discord / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | owner를 통해 provider 세션 시작·재사용 후 응답 | host admission과 O readiness를 거쳐 Herdr에서 provider 세션 시작·재사용 후 응답 | 같음 · #8120의 실제 양쪽 실행 수용은 계속 필요 | `src/services/discord/router/message_handler/intake_turn.rs:161`, `src/services/turn_host.rs:212`, `src/services/herdr_launch.rs:405`. Cancel·clear는 아래 행. Claude/Codex의 E-50/E-51은 양쪽 4개 전달 명세<br>`{"host":"tmux","provider":"claude","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"}`<br>`{"host":"tmux","provider":"codex","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"}`<br>`{"host":"herdr","provider":"claude","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"}`<br>`{"host":"herdr","provider":"codex","qualifiers":{"input_source":"legacy_automation","output_path":"unspecified","turn_origin":"managed"},"state":"idle","surface":"channel"}` |
| message-delivery / 출력 본문·placeholder·도구 카드·푸터 |  |  |  |  | O 선택 surface의 writer가 기록·전달, legacy는 기존 relay | O writer 수용을 확인하고 launch. 자식 surface의 O 선택·복원은 스레드 행에서 확인 | 같음 · O-ready 활성 직접 채널의 출력 계약. 자식·복원 검증 #8118/#7651/#8120 | `src/services/tui_o/channel_policy.rs:74`, `src/services/tui_o/cutover/channel_gate.rs:433`, `src/services/discord/runtime_bootstrap/owner_execution.rs:70` |
| message-delivery / API `turn/start`·`turn/deliver` 직접 채널 |  |  |  |  | owner 시작·busy inject·queue | 직접 채널의 외부 Start 및 busy Herdr inject는 연결됨. 생성 요청의 증거·input provenance에 따라 준비되지 않은 내부 snapshot은 기존 거절 | 같음 · 검증된 외부 직접 채널 계약 main 충족(T1 `c58617c5ca` 및 mm3 `ef2256904f`). 나머지 origin·스레드는 #8125 | `src/server/routes/domains/runtime.rs:132–138`, `src/server/routes/agents_turn_target.rs:191,222`, `src/services/discord/router/message_handler/headless_turn.rs:35,157`, `src/services/discord/health/turn_deliver/inject/herdr.rs:165` |
| message-delivery / API의 idle transcript 제출·origin 보존 |  |  |  |  | tmux 전용 fast path와 일반 owner/queue 경로 | 일반 owner/queue 경로와 실제 Herdr pane의 evidence가 있는 inject. tmux 제출 fast path는 조건 제외 | 다름 · #8125가 origin별 동일 custody와 전달 시점 검증 | `src/services/discord/health/turn_deliver/inject.rs:310`, `src/services/discord/health/turn_deliver/inject/herdr.rs:165`, `src/server/routes/agents_turn_deliver/origin_admission.rs:9,85`. Cancel 후 중복 전송 금지 |
| message-delivery / iMessage 답장 대상 |  |  |  |  | 기존 caller target과 provider session으로 발신 대화 연결 | 공통 launch record도 읽음. 이름 fallback과 부모 바인딩은 기존 방식이라 live child identity를 잃을 수 있음 | 다름 · #8125 live child 입력, #8128 조회 API 뒤 외부 스킬 연결 | 저장소 밖 imessage-send 스킬의 `imsg_send.py` 201·215·227·239·263행 및 `imsg_fork.py` 67·77·85행. 현 로컬 스킬의 `tmux_alive`는 정의만 확인, 실제 호출 결함으로 집계하지 않음 |
| reaction-display / 입력 반응 표시<br>입력 반응 | tmux/Claude managed busy 입력의 ➕ 반응은 한 번이고 📬 추가·제거는 없다. Herdr·Codex를 대응군으로 유지한다. | 독립 instance receipt 지정 E2E: reaction-display; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 4 / 0 / 4 | src/services/discord/router/intake_queue_transaction.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | ➕ 직접 표시 계약. 실제 첫 시도 거절 시 📬→➕ 보고 | 같은 표시 계약. #8114 실제 Herdr 시험에서 📬→➕ 관측 | 다름 · #8113의 host/source 조합별 잔여, #8120 반응 이력 | `src/services/discord/router/intake_queue_transaction.rs:331,339`, `src/services/discord/router/intake_gate/busy_inject.rs:369`. [#8114 종료 댓글](https://github.com/itismyfield/AgentDesk/issues/8114)과 #8113 참조<br>`{"host":"tmux","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}`<br>`{"host":"tmux","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}`<br>`{"host":"herdr","provider":"claude","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}`<br>`{"host":"herdr","provider":"codex","qualifiers":{"input_source":"human","output_path":"tui_o","turn_origin":"managed"},"state":"busy","surface":"channel"}` |
| reset-clear / reset·clear<br>`!clear`·`/clear` 실행 종료와 새 세션 | clear acknowledgement와 새 세션 identity, 이후 첫 응답을 함께 확인한다. wait step과 marker만으로 ack 전체를 세지 않는다. | 독립 instance receipt 지정 E2E: reset-clear; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services/discord/commands / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 기존 실행 종료 및 selector를 잊고 후속 입력에서 새 세션 | exact hosted 종료와 selector 정리 후 후속 입력에서 새 세션 | 같음 · main 종료 계약. 지난 세션 기록은 아래 별도 행 | `src/services/discord/commands/text_commands.rs:65`, `src/services/discord/commands/control.rs:526,546,834`, `src/services/discord/commands/control/clear_teardown.rs:47,76,148,243`. E-50/E-51은 순서·증거 범위가 다름 |
| reset-clear / clear 이전 세션의 종료 기록 |  |  |  |  | 증거를 읽고 exact 종료를 먼저 수행. 종료가 확인된 경우 기존 Claude summary/usage 및 Codex 기록을 저장 | hosted 종료가 수집 분기를 우회하거나 legacy probe가 거절. E는 기존 nonce·source로 exact 종료 확인 뒤 적격 기록을 저장하고 실패를 경고한다. 다음 입력의 fresh 시작은 그대로 진행한다. Codex seal 자격은 첫 시도에서 소비하며 해당 시도가 거절되거나 resume하면 이후 fresh에 넘겨주지 않는다 | 다름 · #8128 E0 격리 시험은 독립 착수 가능. 공통 기록 연결 E는 #7583 S07b 이후, 종료 본체 변경 시 S07c, binding context 변경 시 S07f와 조율 | `src/services/discord/commands/control/clear_teardown.rs:76,104–122,148`, `src/services/discord/commands/control/clear_teardown/clear_end_record.rs:45`, `src/services/discord/commands/control/clear_teardown/codex_end_record.rs:15`, `src/services/codex_tui/execution_end/attempt.rs:168–195`. Stop·Terminate 경계는 행 2 상세 |
| reset-clear / `!restart`·`/restart` 확인된 실제 직접 채널 |  |  |  |  | 경고·취소·PreserveSession stop·reset 후 기존 대화에 인사말 시도 | exact 종료 후 같은 native 세션 seed를 owner가 이어서 실행, 실패/불확인 표시 | 같음 · main `ef2256904f`→`35693e24cb`에서 연결된 기존 사용자 경로. 자식 실제 locator는 #8118 대조 뒤 필요할 때만 R2a | `src/services/discord/commands/restart.rs:213,268,289,308,336,360,385`, `src/services/discord/commands/restart/herdr.rs:30,253,371,431,546` |
| reset-clear / restart 인사말과 대기 메시지 순서 |  |  |  |  | 기존 대기 메시지 뒤 인사말 | 대기 메시지 앞 인사말 | 불가피(인사말은 새 실행의 첫 입력이자 내구 요청 기록) · 사용자 승인 완료 (11-13 20:15 KST) · #8128에서 정리한 인사말 순서 | [기존 설계 §3.8 U5](herdr-parity-restart.design.md#38-tmux와의-차이-전수와-남은-결정), `src/services/discord/commands/restart/herdr.rs:253,431`. clear가 seed를 취소하는 경계도 보존 |
| reset-clear / provider reset·dispatch recreate·`/goal --fresh` |  |  |  |  | 실제 대상 row에 reset을 적용하고 명령별로 새 실행 또는 동일 대화를 선택 | 공유 reset·recreate 경로는 기존 tmux guard에서 거절. 현재 `/goal --fresh`의 별도 fresh 준비 분기가 있음 | 다름 · #8128 R2a. reset·relaunch는 #7583 S07b, recreate는 S07c | `src/services/discord/admin_host_guard.rs:94,116,186,227`, `src/services/discord/commands/tmux_recreate.rs:9,104`, `src/services/discord/commands/control/provider_reset.rs:16`, `src/services/discord/router/message_handler/goal_lifecycle.rs:34–62,77–85`, `src/services/discord/router/message_handler/intake_turn.rs:949–966`, `src/services/discord/router/message_handler/headless_turn.rs:533–556` |
| reset-clear / Codex `/goal clear` |  |  |  |  | idle이면 selector를 초기화하고 다음 입력이 fresh로 시작. busy 또는 상태 불명이면 거절 | idle 분기의 provider reset을 tmux 전용 guard가 거절. busy 거절은 공통 분기 | 다름 · #8128 R2a. #7583 S07b reset 포트 소비 | `src/services/discord/router/message_handler/goal_lifecycle.rs:34–62,159–194`. 수용 시험은 idle·busy·다음 입력을 각각 확인하고, selector 초기화 제거 뮤턴트는 RED |
| reset-clear / `/resume`·API resume-previous |  |  |  |  | 선택한 이전 대화의 native selector를 다시 연결 | 기존 legacy 가드에서 hosted 경로의 explicit 복원 거절 | 다름 · #8128 R2b. #7583 actual locator와 S07b reset·relaunch, S07c 모델 선택 접점 | `src/services/discord/commands/session.rs:14,80`, `src/services/session_resume.rs:402`, `src/server/routes/domains/runtime.rs:91`. clear·restart 후 옛 selector 재연결 경계 |
| reset-clear / 자동 큐 reset·routine session reset·강제 recovery·doctor |  |  |  |  | slot/row/inflight 재확인 뒤 안전한 대상만 repair | 현행 자동 reset 및 legacy repair의 hosted 보호·거절 | 다름 · #8128 R2c는 R2a 결과와 기존 조건부 SQL 및 exact 실행 fence를 소비. 다음 run으로 재할당된 slot은 보존 | `src/services/auto_queue/runtime.rs:177–193,346,472`, `src/services/discord/admin_host_guard.rs:227–248`, `src/services/routines/session_control.rs:103–117`, `src/services/discord/health/recovery.rs:242`, `src/server/routes/domains/runtime.rs:42,87,112`, `src/server/routes/health_api/session_repair.rs:17,76,111` |
| reset-clear / dcserver 부팅·재시작 뒤 실행 복원 |  |  |  |  | row/marker/inflight 검사로 재연결 | hosted row·exact target으로 복원하거나 교체 세대 회복. child 상속·정확한 출력 복원 별도 | 다름 · #8118, #7583 및 #8120 E-8/E-9/E-19/E-61/E-63의 실제 세대·입력·출력 증거 | `src/services/discord/runtime_bootstrap/owner_execution.rs:70,147`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:189,210`, `src/services/discord/herdr_terminate/root_replaced.rs:69,267,342` |
| reset-clear / Herdr 서버 재시작 뒤 빈 셸 잔류 |  |  |  |  | 관리 workspace 껍데기가 복원되는 동작 없음 | shell-only workspace가 남을 수 있음. W1은 격리 환경에서 읽기 전용으로 증명 가능성을 확인한다. W2는 불변 복원 계보, 복원 이후 활동 없음, 남은 작업 없음이 닫는 시점의 단일 서버 판정에서 확인될 때만 닫는다 | 다름 · #8128 W1/W2. 사용자 결정 A는 증명된 빈 창에만 적용. 원자 서버 검증 능력이 W2 선행 조건 | `src/services/herdr_launch/prepared.rs:11`, `src/services/discord/herdr_terminate/root_replaced.rs:69,342`, `src/services/session_host/herdr/pane_probe.rs:205–244`, `src/services/herdr_launch.rs:462–475`. Retired 행이 successor에 덮여 증거가 사라지면 자격 종료 |
| reset-clear / 5분 주기 정책의 6시간 idle 정리 |  |  |  |  | 적격 메인 채널의 idle 실행을 닫고 selector를 보존. 다음 입력은 같은 대화 재개 | 정책이 부르는 기존 `kill-tmux`의 legacy guard가 Herdr를 건너뜀 | 다름 · #8128 R2c, #7583 S07e 이후. 기존 idle·소유 guard와 selector 보존 계약 적용 | `policies/timeouts.js:23,76`, `policies/timeouts/idle-kill.js:43–64,123–140`, `src/server/routes/domains/runtime.rs:83`, `src/services/dispatched_sessions/tmux_cleanup.rs:107–131,145–206`. active dispatch와 child는 이 정책 대상에서 제외 |
| reset-clear / 배포 후 smoke 초기화 |  |  |  |  | 격리 E2E cell의 lease/turn CAS·!clear ACK | 현 배포 smoke reset의 Herdr preset skip 또는 generic ready. 별도 Herdr 관측·continuous 활성화는 main에 있음 | 다름 · #8120이 양쪽 동일 초기 전제 확인 책임 | `scripts/deploy-release.sh:4558,4621`, `scripts/e2e/post_deploy_turn_smoke.py:411,419,434,446,508,519`, `scripts/e2e/tui_relay/channel_reset.py:4,14`. generic ready로 초기화 성공을 증명할 수 없음 |
| routines / 루틴<br>Headless·채널·스레드·예약·retry | 예약된 루틴이 지정 시각·대상에 한 번 실행되고 결과가 보인다. | 독립 instance receipt 지정 E2E: routines; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 1 / 0 / 1 | src/services / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 해당 execution mode의 owner 시작과 기록. 예약 scope 고정 | common route를 재사용하는 경로와 기존 종속 start의 제한이 함께 있음. child host 및 origin 전파 미확인 | 다름 · #8117 실행 모드 분류는 main 충족. #8118/#8125 스레드·start, #8120 원래 예약 provenance 실기 | `src/services/routines/agent_executor.rs:112,549,1146,1179`, `src/services/routines/agent_executor/launch.rs:5,72,144,198`, `src/services/scheduled_messages.rs:194,287,308,379,390`, `src/services/scheduled_messages/owner_input.rs:223` |
| routines / persistent `POST /api/routines/{id}/session/kill` |  |  |  |  | 요청에 귀속된 실행을 종료. host guard가 보존하면 세션 row도 유지 | 기존 force-kill 호출의 legacy host 판정이 별도 hosted 종료를 열지 않음 | 다름 · #8128 R2c는 R2a 효과와 #7583 actual-owner 종료 포트를 소비 | `src/server/routes/domains/ops.rs:188–189`, `src/server/routes/routines/handlers.rs:539–560`, `src/services/routines/session_control.rs:120–132,676–698`, `src/services/turn_lifecycle.rs:158–166,348–360` |
| routines / fresh 실행 완료 후 정리 |  |  |  |  | run을 claim했을 때의 routine 소유권과 result로 해당 실행만 종료 | 기존 fresh teardown은 동일 legacy force-kill 경계를 사용. 실행과 row 보존을 별도 검증해야 함 | 다름 · #8128 R2c. R2a 효과 및 #7583 actual-owner 종료 포트 | `src/services/routines/agent_executor.rs:865–896`, `src/services/routines/session_control.rs:155–171,676–698`. 기존 소유권과 exact 행을 함께 확인 |
| routines / 부팅 시 중단된 fresh 실행 정리 |  |  |  |  | 복원된 run의 소유 기록과 다른 실행 중인 run을 확인해 고아 실행만 종료 | 이름이 같아도 다른 host 또는 교체 run은 건드리지 않는 기존 조건이 적용. 실제 Herdr 종료는 legacy 효과에서 빠짐 | 다름 · #8128 R2c. R2a 효과 및 #7583 actual-owner 종료 포트 | `src/server/mod.rs:2696–2703`, `src/services/routines/agent_executor.rs:937–966,995–1006`, `src/services/routines/session_control.rs:236–278,676–698`. 소유 증거 부재 및 교체 실행의 종료 0건 검증 |
| threads / 새 스레드·host·출력<br>preferred node의 첫 입력 | preferred node ready·v1 slots 없음에서 새 thread 첫 입력이 시작된다. 실제 host·source binding·tui_o 출력·후속/취소를 확인하고 source not bound를 실패로 본다. | 독립 instance receipt 지정 E2E: threads; 선언 검사: python3 scripts/generate_user_feature_map.py --validate --base-ref origin/main | 4 / 0 / 4 | src/services/cluster/intake_router_hook/placement.rs / 해당 없음: 이 행은 관측 계약이며 M1에 상태 전이 구현 없음 | 부모 scope bootstrap 및 thread placement | 같은 placement. 지정 노드 슬롯이 없어 첫 입력을 잃는 #8115 결함은 수리됨 | 같음 · #8115 placement 계약. child host·출력 선택은 다음 행 | `src/services/discord/session_runtime.rs:484`, `src/services/cluster/intake_router_hook/placement.rs:16`, `src/services/discord/runtime_bootstrap/owner_execution.rs:70`<br>`{"host":"tmux","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"unowned","turn_origin":"managed"},"state":"idle","surface":"thread"}`<br>`{"host":"herdr","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"unowned","turn_origin":"managed"},"state":"idle","surface":"thread"}` |
| threads / 실제 child host·source·O 출력 |  |  |  |  | 대상 child의 기존 host/session과 독립 출력 | H 부모 아래 child가 실제 tmux/legacy일 수 있는 잔여. 부모 설정과 actual child locator를 각각 관측 | 다름 · #8118 전환과 boot adoption, #8120 실기 | `src/services/discord/session_runtime.rs:484,513`, `src/services/tui_o/channel_policy.rs:74,346`, `src/services/discord/runtime_bootstrap/owner_execution.rs:70,147`, `src/services/discord/router/intake_gate/busy_inject.rs:187`. Cancel·clear·부팅을 동일 actual child로 확인<br>`{"host":"tmux","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"bound","turn_origin":"managed"},"state":"busy","surface":"thread"}`<br>`{"host":"herdr","provider":"claude","qualifiers":{"execution_node":"preferred","input_source":"human","output_path":"tui_o","ownership":"bound","turn_origin":"managed"},"state":"busy","surface":"thread"}` |
| threads / turn/start·deliver로 실행 중인 자식 접근 |  |  |  |  | 검증된 live child의 thread target 허용 | 같은 live child resolver 사용 | 같음 · T2a `386f983364`의 live child admission 계약 | `src/server/routes/agents_turn_target.rs:191,222`, `src/server/routes/agents_turn_target/thread.rs:13,89`, `src/server/routes/domains/runtime.rs:133–138`. Stop·stale child·thread GC 경계 확인 |
| threads / 외부 API로 세션 없는 자식의 첫 턴 시작 |  |  |  |  | Discord에서 bootstrap한 live child를 받은 뒤 API 진입 가능. 세션 없는 경우 API 진입이 제한 | 같은 진입 제한. 첫 입력으로 여는 Herdr child의 host/출력 전환은 #8118에 남음 | 같음 · 양쪽의 API no-session 진입과 queued owner는 #8125 공통 잔여. 별도 child host/출력 상속은 #8118 | `src/server/routes/agents_turn_target/thread.rs:13,89`, `src/server/routes/agents_turn_target.rs:191,222`, `src/services/discord/session_runtime.rs:484,513` |
| threads / 장기 idle GC·selector·child store |  |  |  |  | 측정된 idle/source와 exact child row에서 cleanup·retirement | hosted/unknown 보호와 상속·실제 child 판정 필요. live state 보호 유지 | 다름 · #8118 O/H, #7583 host 판정. 임의 child 삭제 권한 추가 없음 | `src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:13,54,63`, `src/db/dispatched_sessions/thread_gc.rs:80`, `src/services/discord/session_idle_cleanup.rs:75`, `src/services/discord/runtime_bootstrap/queued_recovery.rs:210` |
| 추가 관찰 항목 / Claude `/compact` |  |  |  |  | live 권한 확인 후 기존 Claude command 입력 | legacy guard가 넘기기 전에 거절 | 다름 · #8128 C. #7583 S07c 입력 포트, Claude readiness 변경 시 S07a | `src/services/discord/commands/tui_passthrough.rs:171,190,197,243,373`. Cancel/clear와 같은 exact binding 사용 |
| 추가 관찰 항목 / `/status`·`/health`·`/inflight` 및 `/skill` 진단 |  |  |  |  | tmux live 상태와 화면 일부 | 일부 상태 본문은 공통, 이름 probe는 unsupported 또는 legacy label | 다름 · #8128 D. #7583 S07b 보고 및 S07c 세션 상세 | `src/services/discord/commands/diagnostics/reports.rs:37,131,208,291,391`, `src/services/discord/commands/diagnostics/session_details.rs:3`, `src/services/discord/commands/skill.rs:148,158,168`. stop/clear와 경합한 화면은 현재 locator의 결과로만 표시 |
| 추가 관찰 항목 / `/queue`·`/adk inspect` 및 공통 목록 |  |  |  |  | 공통 queue·inflight/leases·metadata 조회 | 공통 queue·이벤트/metadata. 진단의 host label/capture는 위 행 | 같음 · 공통 보고의 데이터 계약. #8120 실기 | `src/services/discord/commands/diagnostics/reports.rs:523,752`, `src/services/discord/commands/inspect/mod.rs:61,67`, `src/server/routes/domains/runtime.rs:106,108,145`. clear/stop/boot 후 목록은 같은 store/state 사용 |
| 추가 관찰 항목 / API `/agents/{id}/turn`·diag·세션 `/output`·`/tmux-output`·`agentdesk diag` |  |  |  |  | 최근 화면과 상태·메타데이터 제공 | tmux probe/화면이 guard로 생략되고 `host_unsupported`·`session_host_not_tmux` 노출 | 다름 · #8128 D, #7583 S07a agent 조회, S07e output 관찰 포트 | `src/server/routes/domains/agents.rs:21,29`, `src/server/routes/domains/runtime.rs:95,99,132,139,144`, `src/server/routes/agents.rs:156,206,641,923,946`, `src/services/agents/turn.rs:256,298,479`, `src/services/dispatched_sessions/output.rs:81,169,210`, `src/cli/client.rs:2070,2077`, `src/cli/run.rs:311`. watch-agent-turn이 이 API를 소비 |
| 추가 관찰 항목 / watch-agent-turn |  |  |  |  | 실제 상태·최근 터미널 출력 관찰 | 기존 tmux 출력 API·직접 probe에 의존해 Herdr 화면 누락 가능 | 다름 · #8128 D와 외부 호출자 확인 | `src/services/dispatched_sessions/output.rs:81,169`, 저장소 밖 watch-agent-turn 스킬 문서 60·66·88·94행 |
| 추가 관찰 항목 / CLI `reset-tmux`·API `kill-tmux` |  |  |  |  | raw tmux 이름 기반 cleanup. 다른 host 소유 marker는 보존 | 이 raw host 계약의 대상에서 제외. 사용자 수준 종료는 Terminate/force-kill 행으로 제공 | 불가피(raw tmux 세션 이름과 정리 계약) · 사용자 확인 미기록 · #8275 퇴역 대상 | `src/cli/utils.rs:150,168,175,186`, `src/server/routes/domains/runtime.rs:83`, `src/services/dispatched_sessions/tmux_cleanup.rs:20`. 별도 idle 정책 호출과 selector 보존은 위 행에서 판정 |
| 추가 관찰 항목 / provider auth login |  |  |  |  | 인증 전용 tmux terminal·attach | 물리 로그인 UI의 Herdr 대응 필요 여부와 기존 권한 확인 | 다름 · #7583 Windows·수명, #8275 전에 별도 제품 검토. 행 1에는 TUI 대화 세션의 reset만 포함 | `src/server/routes/provider_auth_profiles.rs:511,516`, `src/services/provider_cli/session_guard.rs:51,184` |
| 추가 관찰 항목 / `GET /api/sessions?restart_gate=1` |  |  |  |  | API가 로컬 legacy tmux 활성 row를 표시. 재시작 preflight가 실제 tmux 목록으로 stale 증거를 판정 | active hosted row를 보호. 로컬 tmux probe 권한이 없으므로 stale hosted row의 실제 pane 증거 소비는 후속 | 다름 · #8275 raw tmux projection, #7583 actual locator. API 실패·상대 host·불확실 상태에서는 기존 보수적 처리 | `src/server/routes/domains/runtime.rs:145`, `src/server/routes/agents_crud.rs:1025,1037,1049,1107`, `scripts/_defaults.sh:1800,1812,1835,1847`. Agent PATCH는 별도 handler(`src/server/routes/agents_crud.rs:503,545`) |
| 추가 관찰 항목 / Qwen 관리 세션 |  |  |  |  | Unix에서 tmux 사용 가능하면 기존 Qwen TUI 실행 | Herdr turn plan 허용 대상은 Claude·Codex | 다름 · #7583 provider 범위에서 추적. #8123 현행 provider 축은 Claude·Codex | `src/services/qwen.rs:316,339,342,357`, `src/services/turn_host.rs:390,399,406`, `scripts/generate_user_feature_map.py:31` |
| 추가 관찰 항목 / 일반 사용자 명령 |  |  |  |  | `/help`, `/pwd`, `/usage`, `/system-prompt`, `/skill`, `/node`, `/tool-call`, `/sidecar`, `/allowall` 공통 handler·정책 | 같은 handler·정책. 별도 agent dispatch·host 실행 효과는 해당 기능 행에서 판정 | 같음 · 명령 정책, #8120 실기 | `src/services/discord/commands/command_policy.rs:193–216`, `src/services/discord/commands/help.rs:83`, `src/services/discord/commands/session.rs:142`, `src/services/discord/commands/receipt.rs:19`, `src/services/discord/commands/system_prompt.rs:97`, `src/services/discord/commands/skill.rs:307`, `src/services/discord/commands/node.rs:292`, `src/services/discord/commands/tool_call.rs:180`, `src/services/discord/commands/sidecar.rs:10`, `src/services/discord/commands/config.rs:559` |
| 추가 관찰 항목 / 폐지한 30개 `!` 명령 |  |  |  |  | 공통 폐지·slash 대체 안내 | 같은 도움말 | 같음 · main 충족 | `src/services/discord/commands/command_policy.rs:72–103,117`, `src/services/discord/commands/text_commands.rs:65,76` |

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
- 부분 시나리오 `E-54` (needs_live): Claude tmux·Herdr의 late marker 부재와 idle !stop 답글 정확히 1회 UX, Herdr hold 중 후속 입력 전달 여부만 참조한다. legacy_automation/unspecified 입력·출력이므로 human/tui_o 수기 계약과 다르며 인간 입력 보존·자동 busy witness는 증명하지 않는다.

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
- 부분 시나리오 `E-50` (needs_live): Claude TUI·Herdr 칸은 fresh_session_after_clear의 provider_session_changed와 망각 단언을 실제 평가한다. pipe 칸은 기존 전달 검사만 유지한다. !clear 자동 경로이며 실제 Discord slash 상호작용은 별도 Rust action 시험과 배포 뒤 사람의 /clear로 검증한다.
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
