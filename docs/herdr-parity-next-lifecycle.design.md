# Herdr 동등성 설계 — clear 종료 기록과 서버 복원

기준 main은 cd9951d3cac6a4f0b81d02789facabbc6ed2a55d다. 아래 좌표는 모두 이 기준으로 고정했다. [#8128](https://github.com/itismyfield/AgentDesk/issues/8128)

| 사용자 기능 | tmux 현재 동작 | Herdr 현재 동작 | 같음·다름 | 담당 |
|---|---|---|---|---|
| !clear·/clear 후 이전 세션 종료와 새 대화 | 이전 실행을 먼저 읽고 종료가 확인되면 Claude ClearEnd나 Codex End를 남긴다. O가 증거로 이전 출력 경계를 정리한다. | 정확한 nonce 종료와 selector 초기화는 있다. 같은 실행의 종료 기록을 수집하는 두 함수는 Herdr를 건너뛴다. | 다름 | #8128 E1·E2. 기존 exact clear는 #8118, #8208과 공유 |
| 호스트 서버 재시작 뒤 예전 세션 공간 정리 | tmux 서버가 죽으면 이전 서버에 속한 세션이 함께 사라진다. | 자동 에이전트 재개가 꺼진 서버는 예전 공간을 셸로 복원할 수 있다. 기존 재조정은 행·소스·입력 보류를 정리한다. | 다름 | #8128 W1·W2. 자동 종료 효과는 사용자 결정 후보 |

## 행 2 — 진입점과 인접 경로

git grep으로 clear_channel_session_state, capture_for, commit_confirmed, terminate_explicit_herdr, clear_ends, codex_ends 호출자를 찾고 바인딩·재조정 소비자를 역추적했다. 두 번째 열 이후의 축약 파일 경로는 같은 행의 앞선 디렉터리를 기준으로 한다.

| 기능/경계 | 진입과 생산 | 소비 및 현재 판정 |
|---|---|---|
| 채널·스레드의 text/slash clear | src/services/discord/commands/text_commands.rs:98-110, src/services/discord/commands/control.rs:834-857, slash 등록 src/services/discord/runtime_bootstrap/framework_setup.rs:199-206 → control.rs:526-564 공통 clear와 clear epoch | control.rs:582-620 재조정 특례 또는 큐·PG 경계·메모리·stop, :622-665 selector·종료. 실행 child와 provider별 canonical owner를 유지한다. |
| tmux clear 종료 기록 | src/services/discord/commands/control/clear_teardown.rs:89-122에서 종료 전 capture, 확인 후 commit. Claude는 clear_teardown/clear_end_record.rs:36-59,73-99,133-191의 pane·nonce·source·binding seq·PID/PGID 검사. Codex는 clear_teardown/codex_end_record.rs:8-35 → src/services/codex_tui/execution_end/capture.rs:88, confirm.rs:43-131 | src/services/tui_o/store/clear_end.rs:26-39,78-124, store/codex_end.rs:96-113,283-304 → Claude 출력 전환 writer/rotation/clear_end.rs:24-109,125-164, 입력 알림 writer/input_notices/admission.rs:243, Codex 출력 전환 writer/rotation/codex_end.rs:22-75,136-210. Codex tmux 후속 실행의 ticket은 execution_end/confirm.rs:62-80, 별도 tmux retirement는 clear_teardown.rs:89-103,114-118,123-140. |
| Herdr clear 종료 | src/services/discord/commands/control/clear_teardown.rs:73-77에서 겹친 startup을 기다린 뒤 exact 종료로 들어가 tmux 기록 경로를 돌아간다. :148-191 Known/Pending/Retired/Legacy/Missing/Unknown/Conflict 분기와 :195-221 exact nonce 요청 및 selector postlude | src/services/discord/herdr_terminate.rs:412-431,448-624의 transition, row/dispatch, fence 및 종료 증거 순서. :187-204 close 응답과 독립적으로 pane/old provider 종료를 확인하고 :225-230,598-624 Retired CAS·턴 settlement. herdr_terminate/retired_exclusion.rs:15-26,77-91에서 callback까지 exact 실행·배제 유지. |
| clear와 호스트 재시작 경합 | control.rs:582-594 → src/services/discord/commands/control/herdr_recovery.rs:21-87에서 RootReplaced/Retired를 먼저 처리 | herdr_recovery.rs:88-123은 같은 row/nonce selector를 처리한다. 이미 접수한 clear의 재개는 같은 의도·nonce의 durable 증거를 소비해야 한다. |
| 기존 소스 및 후속 fresh 증거 | src/db/dispatched_sessions/hosted_execution.rs:33-101 owner/location/기존 launch evidence/source ref. src/services/tui_prompt_dedupe/hosted_source.rs:72-109,204-275의 exact native source와 strict binding log. src/services/discord/tui_prompt_relay/herdr_source.rs:152-249,310-368 launch/reconnect attach | control.rs:605-614가 commands/control/clear_teardown/resume_fence.rs의 fence를 건다. #8208의 clear 뒤 fresh 선택은 후속 실행 증거로 재사용한다. 후속은 동일 채널·logical key의 새 nonce 첫 verified bind로 증명한다. |
| 클러스터 routing owner 종료 | src/services/discord/runtime_bootstrap/intake.rs:10-11 → owner_cleanup.rs:111 → src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:9-123에서 original receipt의 owner/nonce 재검증 및 Herdr terminate | tmux는 같은 파일 :262-344에서 두 기록을 사용한다. Herdr :112-123은 archive/selector postlude를 실행한다. 공통 종료 기록 포트를 이 진입점에서도 소비하고 기존 큐·owner receipt 계약을 유지한다. |
| Stop/Cancel, Terminate 및 HTTP force | text_commands.rs:99, control.rs:668-829의 stop과 clear_teardown.rs:243-252의 clear stop → src/services/discord/turn_bridge/tmux_runtime.rs:535-541. 별도 Terminate는 src/server/routes/herdr_terminate.rs:92-94 → src/services/discord/herdr_terminate.rs:692-717. dispatch/queue force는 src/services/dispatched_sessions/herdr_kill.rs:61, src/services/queue/http_force.rs:88 | 기존 공통 종료 효과에 clear 의도 전달은 clear와 routing owner 종료만 한다. stop/HTTP force로 ExplicitClear를 만들면 그 결과를 잘못된 source 전환에 적용하게 된다. |
| 명령 재시작 및 부팅 | text_commands.rs:129-130, src/services/discord/commands/restart.rs:213-266,268-335,360-390 → restart/herdr.rs:253-359,370-420,429-471 | src/services/discord/herdr_terminate/restart_recovery.rs:31-32의 recover-ended는 별도의 종료 원인이다. src/services/discord/tui_prompt_relay/rehydration.rs:250-270,665-668와 src/services/discord/recovery_engine/herdr_reader.rs:235-370,456-507는 원래 nonce를 다시 확인한다. |

### 처방

1. 명시 clear와 기존 routing owner 종료에 종료 전 증거 수집, 종료 확인, 확정 기록을 연결한다. 기존 Herdr canonical owner, actual endpoint, execution nonce와 native source 증거를 transition과 termination fence 안에서 잡는다. HostedExecution.source_ref.initial_source는 초기값이다. 최신 source는 동일 실행의 strict binding과 inode/native session을 대조한다. Pending이 이 증거를 갖추기 전 끝난 경우 아직 이전 native 세션 기록 대상이 없다. 이전 native 실행은 있었으나 증거를 읽을 수 없으면 기록 보류를 보고한다.
2. 기존 pane/provider 부재 증거와 Retired CAS로 종료를 확인한다. 종료 전 clear 목적과 원래 실행 증거를 내구성 있게 남기고, close/Retired CAS 직후 dcserver 중단 시 재조정이 이어받는다. append와 완료는 같은 원래 identity로 멱등 처리한다. 원래 capture/확정 시각을 경계로 유지한다. 기록 실패는 clear 응답과 관측에 남기고 선행 큐/대화 초기화를 되돌리지 않는다. 내구성 있는 증거가 없는 종료는 미확정으로 표시한다.
3. Claude와 Codex O 소비자는 실제 host/nonce까지 검사한다. 바인딩 논리 이름을 보관하는 tmux_session 필드와 물리 pane 비교를 분리한다. Herdr Codex End는 같은 채널의 clear epoch 뒤 첫 verified fresh nonce·native source bind에서 seal한다. tmux attempt ticket과 물리 pane 캡처는 기존 tmux가 계속 사용한다. V1 tmux 레코드의 독립 판독을 유지한다. 새 레코드 저장 전 새 reader를 착지시켜 구버전이 Herdr 레코드로 tmux 전환을 증명하거나 V1 파일 읽기를 잃는 일을 피한다.
4. 기존 configured endpoint 선별 결함 후보(clear_teardown.rs:73,91-93,123-127)는 #7583/#8122 공통 판정과 #8118 실행 child 귀속에 둔다. 이 조각은 exact row에서 확정된 clear 증거만 처리한다. 이미 의도가 없이 Retired된 행에 현재 clear 시각으로 종료 기록을 소급 생성하면 clear 순서가 왜곡된다.

## 행 3 — 진입점과 인접 경로

git grep으로 workspace_marker, WorkspaceCreate, dispose_residue, recover_retired, reconnect_restarted_herdr, granted_server를 찾아 HerdrRequest와 close 호출부를 대조했다. 11월 13일 21시 이슈 관측의 12개 중 11개 shell-only를 워크스페이스 수명 차이의 사례로 본다. 댓글은 닫기 승인 대기를 명시한다. 같은 계기에 발견된 #8146 Retired 넘김 재시도 수리는 main a0cdf7888a에서 확인했다.

| 진입과 생산 | 소비와 권한 한계 |
|---|---|
| 생성·바인딩 src/services/herdr_launch/prepared.rs:23-55,57-82의 정책 witness와 logical_key#nonce, src/services/session_host/herdr/launch_host.rs:575-616의 typed root pane | src/db/dispatched_sessions/hosted_execution.rs:47-54,67-101은 운영 pane과 old process 정보를 기록한다. workspace 생성 ID/증명을 별도로 보존하는 운영 경로를 찾지 못했다. launch_host.rs:563-571,595-598의 workspace ID는 격리 시험 소유권이다. |
| create 응답 유실 해결 src/services/herdr_launch/unplaced.rs:101-113,131-201,345-380, src/services/session_host/herdr/launch_host.rs:771-826 | 같은 허가 서버의 같은 launch marker와 untouched 1-tab/1-pane LoneShell만 dispose한다. src/services/session_host/herdr_gate.rs:1119-1187,1190-1221의 snapshot·root stamp·pane.close·pane/root 부재 확인 구조를 재사용할 수 있다. bare/중복 marker, 달라진 root는 후일 청소 권한의 근거가 되지 않는다. |
| 부팅 및 반복 재조정 src/services/discord/tui_prompt_relay/rehydration.rs:250-270,665-668,1524-1525, tui_prompt_relay/source_tick.rs:38,132 → src/services/discord/recovery_engine/herdr_reader.rs:235-370,456-507 | PG의 local Pending/Bound/Retired와 원래 row를 읽어 attach, Missing retire 또는 RootReplaced를 처리한다. src/services/discord/herdr_terminate/root_replaced.rs:422-458,463-566,568-588의 재판정·CAS·겹친 marker/hold 및 턴을 소비한다. |
| 사용자 기본 서버 교체 src/services/discord/recovery_engine/herdr_server_replaced.rs:26-43,96-110,143-210 | startup/교체 증거 이후 Bound recover, unplaced/rebooted Pending으로 나뉜다(:214-240 이후). 기존 호출자는 recovery_engine/herdr_row_end.rs:171, recovery_engine/herdr_row_end/owner.rs:67-70, router/intake_gate/stale_turn.rs:323이다. old row·provider에 적용하는 검증이다. 기존 복구·큐 깨움과 독립해서 정리 후보를 재평가한다. |
| 명령 재시작·clear·terminate·stop | 행 2의 종료/selector/postlude가 provenance의 이전 실행 상태를 바꾼다. src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_real_tests.rs:120-159에서는 기존 마지막 pane close 한 번으로 workspace가 사라지고 다른 socket이 보존됐다. 사용 중인 Bound/Pending/current successor와 나중에 열린 개인 탭·프로세스를 보호해야 한다. |
| RPC 및 감사 경계 src/services/session_host/herdr/model.rs:202-258,269-305의 snapshot·pane.process_info와 pane.close(id). src/services/session_host/herdr/contract.rs:48-55의 witness는 socket·PID·start | scripts/check_destructive_call_site_ratchet.py:115,248 및 scripts/destructive_call_site_baseline.json:408의 넓은 close 제한을 적용한다. 관측과 pane.close 사이에 사용자 활동이 시작되면 서버 witness가 같아도 상태가 변한다. |

### 처방과 사용자 결정 후보

- **W1**은 typed create 결과의 원래 owner/nonce, endpoint·서버 birth, workspace generation/ID, root·pane identity를 보존하고 복원 후보를 읽는다. Retired 이후에도 같은 원래 identity를 재확인한다. logical_key#nonce는 후보 탐색에 사용한다. label만 일치하거나 중복 후보·ID 재사용·구형 행만 있으면 보류한다. ID나 소유 표식이 재시작을 넘어 보존되고 복제와 구별되는지 격리 fixture에서 확인한다.
- **W2**는 11월 13일에 명시한 닫기 승인과 서버 능력 실측을 전제로 한다. 같은 로컬 endpoint의 서버 교체, old provider/실행 종료, 새 실행·사용자 활동 없음, exact workspace와 마지막 pane, untouched LoneShell을 재검증한다. **서버가 이 조건을 확인하는 동작과 마지막 pane 닫기를 원자적으로 처리**할 때만 보낸다. 새 서버에서 workspace와 shell 부재를 읽어 효과를 확인한다. 응답 유실 뒤 ID만 보고 재전송하면 새로 붙은 사용자를 위험에 놓는다. 현재 AgentDesk 계약에서 조건부 close를 찾지 못했다. 격리 시험으로 동등한 보장이 확인되지 않으면 capability 마련을 W2의 선행 작업으로 둔다. 새 사용자 명령·스위치 수는 0이다.
- 사용자 결정 후보는 **행 3 하나**다. 승인 범위는 AgentDesk가 조성했고 손대지 않은 복원 workspace의 자동 정리다. 11월 13일의 과거 11개를 소유 근거 없이 일괄 닫는 작업은 대상 밖이다.
- eventual 대상은 AgentDesk 소유 입증, 서버 교체 확인, old 실행 종료, 원자 조건부 close를 모두 충족한 한 pane·한 tab 셸이다. 승인 뒤 조건이 계속 맞으면 기존 recovery 재평가가 재시도한다. 사용자 tab·프로세스나 새 세션이 생기면 정리 자격이 사라진다.

## 조각 및 수용 시험

아래는 구현 전 원시 추정이다. 과거 실측 비율과 이번 설계 실측은 전체 조각 보고에서 대조한다.

| 조각 | 예상 prod 파일 / 순 코드 | 의존 | 수정 전 RED → 수정 후 GREEN, 원인 제거 뮤턴트 |
|---|---:|---|---|
| E1 clear·owner 종료 증거와 Claude | 10~15 / +520~720 | main #7583 terminate 포트, #8118 actual child 귀속, #8208 fresh 경계. S05 파일 접점 조율 | 격리 Herdr Claude clear와 routing owner 종료에서 old nonce ClearEnd 0건·O 후속 경계 미증명 → 정확히 1건·old tail과 새 대화 출력 각각 한 번. capture 제거나 close 전 append 허용 시 RED. close↔CAS, CAS↔append 중단, 재부팅·lost reply, 불명 증거, 새 nonce·다른 child를 시험한다. |
| E2 Codex End 및 후속 fresh seal | 7~11 / +310~480 | E1 capture/finalize/재개 포트 | Herdr Codex clear의 confirmed End·첫 fresh successor seal 0건 → 1건씩·old/new O 출력 한 번. 첫 verified nonce 검사 제거 시 옛 rollout/session이나 다른 child에 엉뚱한 End를 연결해 RED. V1 tmux 판독과 unsynced/write failure를 대조한다. |
| W1 생성 소유 기록·읽기 전용 재조사 | 7~11 / +260~390 | #7583 S04 create(main a549fa311a) | 같은 표시의 두 workspace 중 old 실행 증거로 하나를 특정할 수 없음 → typed receipt와 복원 식별자가 같은 항목만 후보. generation/owner 대조 제거 시 duplicate/legacy/복제 fixture가 후보에 들어 RED. 부팅·부분 write·복원에서 close RPC 0 확인. |
| W2 복원 후 정확한 셸 정리 | 7~11 / +270~410 | W1, 사용자 승인, S05 이후 포트, 원자 서버 capability | 복원된 owned 마지막 셸이 pass 뒤 계속 남음 → 조건부 동작 한 번으로 workspace·shell 사라짐을 읽어 확인하고 새 실행/개인 공간 보존. close 직전 입력·provider·tab·교체 witness를 barrier로 주입하고 서버 조건 제거 시 RED. lost reply·겹친 pass 뒤에도 같은 대상만 사라짐. |

## #7583 접점과 리뷰 관찰

git ls-remote origin 'refs/review/i7583*'에서 S04-1 fcdf8358fc, S05 28e9924ddc, S05m e2e986849c를 읽었다. S04는 main a549fa311a에 착지했다. S05m 대비 git diff --name-only HEAD...e2e986849c의 접점은 src/services/session_host/herdr_gate.rs, session_host/traits.rs, session_host/termination.rs, termination_audit/host_terminate.rs 및 하위 계약, tui_prompt_dedupe/binding_context.rs, discord/commands/control/managed_reset.rs와 종료 시험이다. main의 session_host/traits.rs:33-39와 session_host/termination.rs:27-48에는 operator terminate 포트가 있다. E1은 main의 exact 종료를 사용하고 binding_context 및 종료 시험 접점을 S05와 조율한다. S05m과 main의 직접 비교에서 herdr_gate.rs는 변경이 있고 termination.rs는 일치한다. W2는 S05 착지 뒤 herdr_gate.rs의 효과 경계를 확정한다. End journal과 재시작 후 workspace 소유 증거는 S05와 책임이 다르다. 기존 giant herdr_gate.rs(1,226줄)에는 이동·호출 교체만 두고 새 책임은 child에 둔다. commands/control.rs(996줄)도 진입점 호출 교체만 하고 증거 수집·재개 책임은 child에 둔다.

- 경계 시각은 원래 epoch/nonce와 old process stamp에서 확인한다. restart와 retry 요청 시각을 종료 시각으로 대체하는 뮤턴트는 RED다.
- local actor와 서버의 변경을 각각 시험한다. 앱의 transition lock으로 외부 사용자 작업을 막을 수 없다.
- 옛 형식은 V1 reader fixture와 새 generation 없는 Herdr fixture로 보존한다. label은 기존 생성 권한을 복원하는 증거가 아니다.
- exact nonce와 서버 조건을 제거한 격리 뮤턴트가 RED인지 보고 원복 바이트를 확인한다.
- 0/PASS를 막기 위해 fixture는 성공한 close 및 durable append, 또는 실제 workspace 부재를 각각 단언한다. 미확정·capability 부재·fixture 미실행은 통과 수에서 제외한다.
