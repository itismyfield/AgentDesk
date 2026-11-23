# Herdr 동등성 설계 — clear 종료 기록과 서버 복원

코드 좌표의 기준 main은 `cd9951d3cac6a4f0b81d02789facabbc6ed2a55d`다. 이번 개정은 설계 `86b268dd52`의 리뷰와 11-20 결정 `i8128-w2-1120 = A`를 반영한다. [#8128](https://github.com/itismyfield/AgentDesk/issues/8128)

| 사용자 기능 | tmux 현재 동작 | Herdr 현재 동작 | 같음·다름 | 담당 |
|---|---|---|---|---|
| !clear·/clear 후 이전 세션 종료와 새 대화 | 종료 권한이 확인되면 기록 수집 결과와 무관하게 종료한다. 종료를 확인한 뒤 증거가 있을 때 Claude ClearEnd나 Codex End를 남긴다. | 정확한 nonce 종료와 selector 초기화는 있다. 같은 실행의 종료 기록을 수집하는 두 함수는 Herdr를 건너뛴다. | 다름 | #8128 E0 격리 시험과 E 공통 연결. 기존 exact clear는 #8118, #8208과 공유 |
| 호스트 서버 재시작 뒤 예전 세션 공간 정리 | tmux 서버가 죽으면 이전 서버에 속한 세션이 함께 사라진다. | 자동 에이전트 재개가 꺼진 서버는 예전 공간을 셸로 복원할 수 있다. 기존 재조정은 행·소스·입력 보류를 정리한다. | 다름 | #8128 W1·W2. 사용자는 조건을 증명한 빈 창의 자동 닫기를 승인했다. 서버 능력 확인이 선행한다. |

## 행 2 — 진입점과 인접 경로

git grep으로 clear_channel_session_state, capture_for, commit_confirmed, terminate_explicit_herdr, clear_ends, codex_ends 호출자를 찾고 바인딩·재조정 소비자를 역추적했다. 두 번째 열 이후의 축약 파일 경로는 같은 행의 앞선 디렉터리를 기준으로 한다.

| 기능/경계 | 진입과 생산 | 소비 및 현재 판정 |
|---|---|---|
| 채널·스레드의 text/slash clear | src/services/discord/commands/text_commands.rs:98-110, src/services/discord/commands/control.rs:834-857, slash 등록 src/services/discord/runtime_bootstrap/framework_setup.rs:199-206 → control.rs:526-564 공통 clear와 clear epoch | control.rs:582-620 재조정 특례 또는 큐·PG 경계·메모리·stop, :622-665 selector·종료. 실행 child와 provider별 canonical owner를 유지한다. |
| tmux clear 종료 기록 | src/services/discord/commands/control/clear_teardown.rs:89-122에서 종료 전 capture, 확인 후 commit. Claude는 clear_teardown/clear_end_record.rs:36-59,73-99,133-191의 pane·nonce·source·binding seq·PID/PGID 검사. Codex는 clear_teardown/codex_end_record.rs:8-35 → src/services/codex_tui/execution_end/capture.rs:88, confirm.rs:43-131 | src/services/tui_o/store/clear_end.rs:26-39,78-124, store/codex_end.rs:96-113,283-304 → Claude 출력 전환 writer/rotation/clear_end.rs:24-109,125-164, 입력 알림 writer/input_notices/admission.rs:243, Codex 출력 전환 writer/rotation/codex_end.rs:22-75,136-210. Codex tmux 후속 실행의 ticket은 execution_end/confirm.rs:62-80, 별도 tmux retirement는 clear_teardown.rs:89-103,114-118,123-140. |
| Herdr clear 종료 | src/services/discord/commands/control/clear_teardown.rs:73-77에서 겹친 startup을 기다린 뒤 exact 종료로 들어가 tmux 기록 경로를 돌아간다. :148-191 Known/Pending/Retired/Legacy/Missing/Unknown/Conflict 분기와 :195-221 exact nonce 요청 및 selector postlude | src/services/discord/herdr_terminate.rs:411-430,447-623의 transition, row/dispatch, fence 및 종료 증거 순서. :187-204 close 응답과 독립적으로 pane/old provider 종료를 확인하고 :225-230,598-624 Retired CAS·턴 settlement. turn_orchestrator/retired_exclusion.rs:17-28,79-93에서 callback까지 exact 실행·배제 유지. |
| clear와 호스트 재시작 경합 | control.rs:582-594 → src/services/discord/commands/control/herdr_recovery.rs:21-87에서 RootReplaced/Retired를 먼저 처리 | herdr_recovery.rs:88-123은 같은 row/nonce selector를 처리한다. 기존 종료 복구를 유지한다. 프로세스가 종료 확인과 기록 사이에 중단되면 해당 기록의 유실을 허용한다. |
| 이전 native source | src/db/dispatched_sessions/hosted_execution.rs:33-101 owner/location/launch evidence/source ref. src/services/tui_prompt_dedupe/hosted_source.rs:72-109,204-275의 exact native source와 strict binding log. src/services/discord/tui_prompt_relay/herdr_source.rs:152-249,310-368 launch/reconnect attach | capture는 정산이 끝난 동일 nonce의 기존 binding 증거를 사용한다. control.rs:605-614에서 호출하는 clear_teardown/resume_fence.rs:23-25는 Claude 전용이다. |
| Codex clear 이후 첫 시도와 seal | tmux src/services/codex/command_streaming.rs:50-67에서 분기 전 시도를 시작한다. src/services/codex_tui/execution_end/attempt.rs:168-195,409-439에서 해당 시도가 기존 ticket을 가져가고 epoch를 고정한다. 같은 영역 producer.rs:35-55,64-119가 적격 fresh만 후보화한다. | Herdr의 실제 host dispatch는 src/services/discord/router/message_handler/provider_dispatch.rs:567-578,610-645, Codex 분기와 준비는 src/services/codex/herdr_turn.rs:139-183,235-259이다. replay start가 기존 driver 호출까지 도달한 범위에서 같은 actual host·owner를 확정한다. executor switch·capacity·launch·resume 판정보다 먼저 시도를 묶고 실제 첫 bind에서는 그 시도 ID로만 seal한다. |
| 클러스터 routing owner 종료 | src/services/discord/runtime_bootstrap/intake.rs:10-11 → owner_cleanup.rs:111 → src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:9-123에서 original receipt의 owner/nonce 재검증 및 Herdr terminate | tmux는 같은 파일 :262-344에서 두 기록을 사용한다. Herdr :112-123은 archive/selector postlude를 실행한다. 공통 종료 기록 포트를 이 진입점에서도 소비하고 기존 큐·owner receipt 계약을 유지한다. |
| Stop/Cancel, Terminate 및 HTTP force | text_commands.rs:99, control.rs:668-829의 stop과 clear_teardown.rs:243-252의 clear stop → src/services/discord/turn_bridge/tmux_runtime.rs:509-515. 별도 Terminate는 src/server/routes/herdr_terminate.rs:92-94 → src/services/discord/herdr_terminate.rs:691-716. dispatch/queue force는 src/services/dispatched_sessions/herdr_kill.rs:56, src/services/queue/http_force.rs:88 | 기존 공통 종료 효과에 clear 의도 전달은 clear와 routing owner 종료만 한다. stop/HTTP force로 ExplicitClear를 만들면 그 결과를 잘못된 source 전환에 적용하게 된다. |
| 명령 재시작 및 부팅 | text_commands.rs:129-130, src/services/discord/commands/restart.rs:213-266,268-335,360-390 → restart/herdr.rs:253-359,370-420,429-471 | src/services/discord/herdr_terminate/restart_recovery.rs:31-32의 recover-ended는 별도의 종료 원인이다. src/services/discord/tui_prompt_relay/rehydration.rs:250-270,665-668와 src/services/discord/recovery_engine/herdr_reader.rs:235-370,457-508는 원래 nonce를 다시 확인한다. |

### 처방

1. 종료 권한은 기존 transition, actual endpoint, exact owner/nonce, termination fence와 pane/provider 종료 확인으로 결정한다. 기록 가능 여부는 동일 실행의 native source, strict binding seq 및 source identity로 따로 판단한다. `HostedExecution.source_ref.initial_source`는 힌트이며 가장 최근 exact binding을 대조한다. 종료 전에 읽어야 하는 증거는 메모리에서만 잡는다. source가 없다면 기존 종료·selector·owner cleanup을 계속하고, 기존 native 실행의 증거를 잃은 경우에만 기록 경고를 남긴다.
2. 명시 clear와 기존 routing owner 종료가 공유하는 연결점은 exact 종료 확인 뒤의 `after_retire` postlude다. 이미 핀한 old nonce와 `RetiredExclusion.execution()`을 대조해 해당 실행의 Claude ClearEnd/Codex End를 기존 저장소에 멱등 기록한다. 저장 실패 또는 기록 불명은 advisory 경고다. 종료·selector·archive·queue wake의 기존 결과와 독립적이다. 종료 확인이 있을 때만 기록을 쓴다. 선행 저널과 부팅 drain은 설계에서 덜어냈다. tmux와 같은 best-effort 계약을 지키고 기록 복구를 위한 별도 상태와 I/O를 줄이기 위해서다. close 이후 append 전 프로세스 중단에는 기록이 빠질 수 있다. 과거 Retired 행을 발견한 시각에는 새 종료 기록을 귀속할 근거가 없다.
3. Codex는 durable End를 기록했을 때만 기존 launch-attempt 규칙으로 일회성 자격을 붙인다. 범위는 실제 Herdr endpoint·canonical owner·channel과 clear 시점의 generation이다. 기존 attempt 수명과 claim 규칙을 재사용한다. 최초 exact dispatch가 자격을 회수하고 거절·resume·중첩·전환이면 소진한다. 취소·실패로 binding이 없거나 새 clear/시도가 선점한 경우에도 이후 시도의 seal은 0건이다. 최초 시도가 실제 fresh로 진행하고 동일 attempt의 새 nonce와 native source 첫 binding이 확인된 경우에만 seal 한 건을 쓴다. 아직 binding이 없는 Pending의 후속 dispatch는 별개 시도다.
4. Claude와 Codex O 소비자는 actual host, old nonce, successor nonce의 역할을 구분한다. 기존 논리 이름 필드 `tmux_session`과 물리 pane 주소는 구분한다. V1 tmux 판독과 기존 tmux attempt/marker 검사를 보존하고, Herdr 증거를 해석할 수 있는 reader를 먼저 배치한다. configured endpoint 선별 결함 후보(`clear_teardown.rs:73,91-93,123-127`)는 #7583/#8122 공통 판정 및 #8118 실행 child 귀속에 둔다.

## 행 3 — 진입점과 인접 경로

git grep으로 workspace_marker, WorkspaceCreate, dispose_residue, recover_retired, reconnect_restarted_herdr, granted_server를 찾아 HerdrRequest와 close 호출부를 대조했다. 11월 13일 21시 이슈 관측의 12개 중 11개 shell-only는 워크스페이스 수명 차이의 사례다. 11-20 승인 범위는 복원 계보와 미사용 및 빈 상태를 증명한 창이다. 같은 계기에 발견된 #8146 Retired 넘김 재시도 수리는 main a0cdf7888a에서 확인했다.

| 진입과 생산 | 소비와 권한 한계 |
|---|---|
| 생성·바인딩 src/services/herdr_launch/prepared.rs:23-55,57-82의 정책 witness와 logical_key#nonce, src/services/session_host/herdr/launch_host.rs:575-616의 typed root pane | src/db/dispatched_sessions/hosted_execution.rs:47-54,67-101은 운영 pane과 old process 정보를 기록한다. workspace 생성 ID/증명을 별도로 보존하는 운영 경로를 찾지 못했다. launch_host.rs:563-571,595-598의 workspace ID는 격리 시험 소유권이다. |
| create 응답 유실 해결 src/services/herdr_launch/unplaced.rs:102-114,132-202,349-384, src/services/session_host/herdr/launch_host.rs:771-826 | 같은 서버·launch marker와 구조가 같을 때의 제한된 기존 효과다. src/services/session_host/herdr_gate.rs:1119-1187,1190-1221은 snapshot·root stamp·pane.close·pane/root 부재를 읽고, src/services/session_host/herdr/pane_probe.rs:206,221-244의 LoneShell은 전경만 관측한다. 이 경로가 증명하는 범위는 해당 서버와 당시 pane 상태까지다. |
| 부팅 및 반복 재조정 src/services/discord/tui_prompt_relay/rehydration.rs:250-270,665-668,1524-1525, tui_prompt_relay/source_tick.rs:38,132 → src/services/discord/recovery_engine/herdr_reader.rs:235-370,457-508 | PG의 local Pending/Bound/Retired와 원래 row를 읽어 attach, Missing retire 또는 RootReplaced를 처리한다. src/services/discord/herdr_terminate/root_replaced.rs:422-458,463-566,568-588의 재판정·CAS·겹친 marker/hold 및 턴을 소비한다. |
| 사용자 기본 서버 교체 src/services/discord/recovery_engine/herdr_server_replaced.rs:26-43,96-110,143-210 | startup/교체 증거 이후 Bound recover, unplaced/rebooted Pending으로 나뉜다(:214-240 이후). 기존 호출자는 recovery_engine/herdr_row_end.rs:171, recovery_engine/herdr_row_end/owner.rs:67-70, router/intake_gate/stale_turn.rs:323이다. old row·provider에 적용하는 검증이다. 기존 복구·큐 깨움과 독립해서 정리 후보를 재평가한다. |
| 명령 재시작·clear·terminate·stop | 행 2의 종료/selector/postlude가 provenance의 이전 실행 상태를 바꾼다. src/services/discord/tui_prompt_relay/relay_e2e/herdr_terminate_o_real_tests.rs:120-159에서는 기존 마지막 pane close 한 번으로 workspace가 사라지고 다른 socket이 보존됐다. 사용 중인 Bound/Pending/current successor와 나중에 열린 개인 탭·프로세스를 보호해야 한다. |
| fresh successor src/services/herdr_launch.rs:468-477 | 새 실행은 Retired row를 덮는다. 같은 옛 workspace의 불변 증거를 독립적으로 대조할 수 있는 경우만 계속 적격이다. 행이 유일한 소유 증거였다면 덮는 순간 자격을 잃는다. |
| RPC 및 감사 경계 src/services/session_host/herdr/model.rs:202-258,269-305의 snapshot·pane.process_info와 pane.close(id). src/services/session_host/herdr/contract.rs:48-55의 witness는 socket·PID·start | 현재 client 계약에는 lineage, 복원 이후 활동 이력, 백그라운드 작업 없음과 조건부 close의 원자 조합이 없다. workspace ID·revision 및 두 번의 읽기가 증명하는 범위는 별개 시점의 값까지다. scripts/check_destructive_call_site_ratchet.py:115,248 및 scripts/destructive_call_site_baseline.json:408의 close 제한을 적용한다. |

### 처방과 사용자 결정

- **W1**은 S04 create 포트로 준비한 격리 fixture를 복원 전후 읽어 대상 서버가 제공하는 증거를 조사한다. typed create 결과의 owner/nonce와 복원 lineage, 다른 workspace 또는 재사용한 ID, 복원 직후부터의 활동 변화, 남은 작업 검출을 대조한다. 읽을 수 없는 항목은 불명으로 결과에 남긴다. 관측 결과는 격리 fixture에만 보존한다.
- **W2**는 결정 A에 따라 마지막 pane 닫기 효과를 서버가 다음 세 조건과 **같은 원자 연산**에서 확인할 수 있을 때만 연결한다. ① 변경 불가능한 복원 계보가 기존 AgentDesk 소유의 해당 workspace를 지목한다. ② 복원 시점부터 사용 이력이 한 번도 없었음이 증명된다. 첫 관측 전에 사용했거나 셸이 다시 idle이어도 이력은 남는다. ③ 해당 시점에 남은 foreground·background 작업과 세션이 없고, 그와 동시에 다른 탭/pane이나 새 실행이 없다. 셋 중 거짓 또는 불명 하나라도 있으면 닫지 않는다. 서버가 효과 시점에 증명할 수 없는 항목은 W2의 upstream 선행 의존이다.
- 기존 복구 경로는 지속 적격 대상만 재평가한다. 세대, 계보, activity, 남은 작업의 판정이 일치하면 기존 마지막 pane 정리 효과와 같은 결과를 내고 workspace 부재를 확인한다. 응답 유실 시 동일 identity의 자격을 다시 읽는다. 11월 13일에 관측한 11개의 일괄 처리는 범위 밖이다. 새 운영 명령·스위치 수는 0이다.

## 조각 및 수용 시험

아래는 구현 전 원시 추정이다. 과거 실측 비율과 이번 설계 실측은 전체 조각 보고에서 대조한다.

| 조각 | 예상 prod 파일 / 순 코드 | 의존 | 수정 전 RED → 수정 후 GREEN, 원인 제거 뮤턴트 |
|---|---:|---|---|
| E0 기존 E1 경계의 격리 시험 | 0 / +0 | 지금. main exact 종료 포트 및 기존 routing owner fixture | tmux는 source가 불명이어도 close/selector/wake가 성공한다. Herdr도 기존 종료 권한을 적용하고 기록 추가만 RED인 지점을 분리한다. fixture 초기화 실패나 조기 거절은 RED로 세지 않는다. |
| E1/E2 공통 종료 기록 연결(E) | 9~14 / +420~680 | E0, S07b clear/owner cleanup 및 Codex capture/confirm/producer. Codex Herdr 실행 본체를 바꾸면 S07a, 종료 본체를 바꾸면 S07c, binding context를 바꾸면 S07f와 파일 소유를 조율 | Claude와 Codex 각각 기록 증거가 있는 exact clear/owner 종료: old nonce 기록 0→1, 이전·새 대화 출력 각 1회. source 소실 또는 저장 실패에서도 기존 close·selector·wake 결과와 경고 확인. 미확정 close 기록 0. Codex 첫 exact 시도가 거절/Resume → 다음 fresh의 이전 seal 0, 적격 최초 fresh만 1. 시도 전 자격 회수를 빼거나 최신 후보를 골라 쓰는 뮤턴트는 RED. |
| W1 읽기 전용 격리 검증 | 0 / +0 | 지금. #7583 S04 create(main a549fa311a) | 복원 lineage·미사용 이력·남은 작업을 각각 실제 응답 필드로 대조한다. 동일 표시, ID 재사용, 첫 관측 전 사용 후 idle, background 작업, 근거 없는 옛 row에서 불명/부적격을 확인하고, 조사 관측의 close 0을 단언한다. 현재 서버의 능력 부족은 W2 선행 의존으로 기록한다. |
| W2 복원 후 정확한 셸 정리 | 5~8 / +160~280 | W1, 서버 원자 조건부 close·증거 제공, #7583 S05m 이후 관측 및 종료 경계 조율. upstream 구현·새 운영 소유 레코드가 필요하면 합산 전 재측정 | 지속 적격인 복원 셸이 남던 상태→조건부 close 1회와 workspace 부재 확인. 첫 관측 전 사용, 사용 후 idle, background job, legacy 및 ID 재사용, successor가 기존 증거를 덮은 경우 close 0. 활동 검사를 빼면 재관측 뒤 사용 셸을 닫아 RED. 세 증거 각각 효과 직전 엇갈림 및 lost reply를 격리 시험한다. |

## #7583 접점과 리뷰 관찰

`git ls-remote origin 'refs/review/i7583*'`에서 S04-1 `fcdf8358fc`, S05 `28e9924ddc`, S05m `e2e986849c`, 최신 설계 `e6d6e69af`를 확인했다. S04는 main `a549fa311a`에 있다. 최신 설계 ref의 소비자 목록에 `codex/herdr_turn.rs`는 S07a, clear/owner 및 Codex capture, confirm, producer는 S07b, `herdr_terminate.rs`는 S07c, `binding_context.rs`는 S07f다. S05m과 main 직접 비교에서 `herdr_gate.rs`는 차이가 있고 `termination.rs`는 일치한다. E는 main의 기존 exact 종료 postlude에 provider 연결을 한 번 추가하며 S07b 이후에 착수한다. E0와 W1은 기존 포트의 격리 관측을 앞서 진행한다. 기존 giant에 순증은 없고 연결의 새 책임은 child에 둔다.

- 경계 시각은 확인된 종료와 최초 exact launch attempt다. 이전 실행과 successor를 둘 다 만들어 종료 후 write 순서, old/new nonce 귀속, 시도 승계 금지를 별도로 단언한다.
- 동시성은 같은 nonce의 clear 경합과 Retired row 교체, 새 서버의 효과 직전 활동을 주입해 검증한다.
- 옛 형식은 V1 reader fixture와 새 generation 없는 Herdr fixture를 둔다. label만 있는 후보는 불명으로 기록한다.
- 문서로만 선언한 가드는 W2의 서버 원자 세 조건이다. W1에서 하나라도 지원을 확인할 수 없으면 해당 증명을 선행 의존으로 남긴다.
- 0/PASS 방지는 기존 close 성공, 적격 기록 1건, 기록 실패 경고와 정상 clear 결과, 적격 workspace 부재를 각각 단언한다. 불명·capability 부재·fixture 미실행은 W2 성공 수에서 제외한다.
