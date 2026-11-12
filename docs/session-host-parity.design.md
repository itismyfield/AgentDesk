# 세션 호스트·표면·진입점 통합 설계 — #8118·#8125·#8128·#8122

조사 기준은 main `4e0db0cccd59169a09f25fe162d8cc1b2755e96e`, 작성일은 2026-11-12 KST다. 코드 좌표는 별도 표기가 없으면 이 SHA의 좌표다. 코드·이슈를 읽은 설계이며 운영 상태 재측정, Rust 변경, cargo/build/test/clippy, TLC 실행은 하지 않았다. 이슈에 적힌 실측은 이슈 작성자의 관측이지 이 레인의 실행 증거가 아니다.

정본 입력은 [#8118](https://github.com/itismyfield/AgentDesk/issues/8118), [#8125](https://github.com/itismyfield/AgentDesk/issues/8125), [#8128](https://github.com/itismyfield/AgentDesk/issues/8128), [#8122](https://github.com/itismyfield/AgentDesk/issues/8122)의 본문·전체 댓글이다. #8118은 스레드 출력 경로까지 포함하며 legacy 전송 296건은 감소 비교의 과거 관측값이다. 새 전송 0의 판정은 배포 후 창에서 한다. #8128의 `/restart` 및 명령 검토 댓글(11-12 16:44·16:48 KST)을 수용했다.

선행 입력 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/design-boundary-delta-1.md`를 읽고, 조사 중 게시된 **`refs/review/i8114-2=d8e1cabbb27b37a73092d41bd46c5c94ce72e785` 코드와 `refs/review/i8119-d2=01053c2be0312bad4d9634f2df46b3141301f279`의 [명명 설계](https://github.com/itismyfield/AgentDesk/blob/01053c2be0312bad4d9634f2df46b3141301f279/docs/session-naming.design.md)를 fetch해 대조했다.** #8114-2는 Slice1의 [BusyOperation L60–119](https://github.com/itismyfield/AgentDesk/blob/d8e1cabbb27b37a73092d41bd46c5c94ce72e785/src/services/claude_tui/host_input/busy.rs#L60-L119)과 [공통 BusyClaude L9–52](https://github.com/itismyfield/AgentDesk/blob/d8e1cabbb27b37a73092d41bd46c5c94ce72e785/src/services/discord/queue_io/input_owner/effect/claude.rs#L9-L52)를 연결하지만, typed source/observer/reobserve·early/late 승인은 Slice2에 남긴다. **이 묶음의 “#8114 배포 뒤”는 Slice1 ref 존재만이 아니라 Slice2를 포함한 사용자 계약의 배포 완료 SHA 확인**이다. #8119 d2의 S6 BindingContext 부모 필드·S8b1/S8b2 순서를 따른다. d1의 effective descriptor CAS 저장기계는 채택하지 않는다. 이 레인은 ref를 읽었으며 운영 배포 여부는 확인하지 않았다.

## §1 진입점 전수표

표의 T/H는 **부모 채널의 설정**이 tmux/herdr임을 뜻한다. 현재 H 스레드의 실제 실행이 tmux일 수 있으므로 ‘herdr 스레드’가 이미 herdr에 있다는 뜻은 아니다. O는 tui_o다. 채널·스레드의 실제 ID, native session, row/key/nonce, source, 원본·receipt·출력 목적지는 별개다. `D=` 접두는 `src/services/discord/`, `S=`는 `src/services/`, `R=`는 `src/server/routes/`다. 행 안의 공통 좌표는 네 표면 모두에 적용한다. 코드만으로 확인한 행을 실물 GREEN으로 표현하지 않는다.

| 진입점 | tmux 채널 현재 | herdr 채널 현재 | tmux 스레드 현재 | herdr 스레드 현재 | 목표 동작 | 담당 조각 | 불가피한 차이·보존할 사유 |
|---|---|---|---|---|---|---|---|
| Discord 새 메시지·새 세션 | intake→provider launch | intake용 host admission 뒤 Herdr 실행 | 부모 path bootstrap 후 tmux | child ID exact lookup으로 tmux 선택 | 같은 intake, 새 child는 부모 host 정책, 실제 child target 사용 | #8118 H1, #8115 | host IO·admission 수단만 다름. `D=session_runtime.rs:505,513`, `S=herdr_launch.rs:56`, `S=turn_host.rs:151` |
| 일하는 중 사람 입력 | owner가 busy paste, source 불명은 Held | 기존에는 일부 defer; #8114가 공통 operation으로 교체 예정 | 새 owner 경로 사용. 이슈 실측 source-not-bound 실패도 있음 | launch는 tmux인데 busy gate는 부모 H로 배제하는 불일치 | #8114 operation을 동일 소비; child의 source/history를 먼저 bind | #8114 뒤 #8118 O/H | tmux server guard, Herdr exact pin/physical writer lease 보존. `D=router/intake_gate/busy_inject.rs:82`, `D=router/message_handler/intake_turn.rs:1110,1754`(전체 경로는 부록) |
| 터미널에서 시작된 턴 합류 | Claude live transcript→owner busy | endpoint 존재 시 제외 | child ID 기준으로 tmux 합류 | 설정과 actual host가 어긋남 | 정책 host가 아닌 actual operation 관측으로 같은 사람 입력 합류 | #8114, #8118 H1 | native unknown은 key0 Held, post-key 불명은 no-resend. `D=router/message_handler/intake_turn.rs:1751` |
| 출력 본문·placeholder·도구 카드·푸터 | O 선택 채널은 O writer | O-ready가 Herdr launch 선행 | boot selection에 child가 없어 legacy로 빠짐 | 동일; parent O여도 child O 0이라는 이슈 관측 | 부모 O selection·kind·site를 상속, child별 writer/store/adoption/usage | **#8118 O 첫 구현**, 이후 #7651 P2/P3 | legacy 증상 개별 수리 금지. `S=tui_o/channel_policy.rs:74,346`, `S=tui_o/cutover/channel_gate.rs:433`, `D=runtime_bootstrap/owner_execution.rs:79` |
| API target 공통 | agent bound 채널 허용 | 동일 target 허용 | exact bound 아니면 403 | 동일 403 | 검증된 agent parent 아래 actual thread도 허용; provider는 parent | #8125 T2 | unrelated·조회 실패 403 유지. `R=agents_turn_target.rs:65,107` |
| API turn/start | headless owner admission 후 시작 | 일반 refusal→422 executor_not_wired | target 단계 403 | 403; 우회하면 child direct endpoint가 host 오판 | idle 시작, 사람 origin busy는 #8114, 기계 start는 자기 턴 순서 유지 | #8125 T1→T2 | API라는 이유로 모든 입력을 사람으로 분류하지 않음. `R=agents.rs:781,892`, `D=router/message_handler/headless_turn.rs:159` |
| API turn/deliver | preflight→inject/start/queue | **busy Herdr inject는 이미 있음**, idle start는 위 거절 | target 403 | target 403; direct endpoint로 Herdr leaf 누락 | owner/execution ID를 분리해 기존 경로 재사용 | #8125 T1→T2 | injected/queued/unconfirmed, origin obligation 유지. `R=agents_turn_deliver.rs:192,236`, `D=health/turn_deliver/inject/herdr.rs:41,79` |
| API no-session thread | 해당 없음 | 해당 없음 | 403 | 403 | Discord 첫 thread 메시지와 같은 bootstrap·placement 후 시작 | #8115 뒤 #8125 T2 | capacity preferred fallback은 #8115 책임, 무제한 overflow 금지 |
| iMessage 답장 | GUID 원본 identity로 owner 입력 | start 거절; busy deliver는 기존 경로 있음 | API 거부로 본 채널 우회(이슈 실측) | 동일 | 발신 세션의 child/native target에 1회 전달 | #8125 T1/T2 뒤 외부 스킬 | 스킬 예외 제거는 저장소 밖. `D=router/turn_start.rs:79`, `D=queue_io/input_owner/start.rs:18`, `R=agents_turn_deliver.rs:236` |
| 루틴 primary/fallback/retry | agent_executor→start_turn | channel headless 거절 | thread 생성/재사용, parent metadata 및 owner-channel starter 이미 있음 | child host를 tmux로 오판 | 동일 정책 resolver·공통 external start 사용 | #8118 H1, #8125 T1/T2, #8117 | process fresh 실행을 TUI로 강제하지 않음. `S=routines/agent_executor.rs:129,268,582,731`, `agent_executor/launch.rs:72,144,198` |
| scheduled agent fire | pinned target 또는 primary | headless 거절 | parent metadata 있음 | host 상속 누락 | 예약의 original/commit fence를 유지하고 child 실행 | #8118 H1, #8125 T1/T2 | `D=scheduled_messages.rs:287,308,379,390`의 snapshot scope 불변 |
| `/restart` | cancel→PreserveSession stop→reset→완료 seed | cancel 전 managed_reset_refusal | target name 기반 경로 | child direct 판정으로 tmux 처리 가능 | cancel→exact pane 종료 확인→동일 native session resume→완료 seed 1회 | #8128 R **첫 관리 조각** | #8119 S8b1 뒤, #8125 T1 seed 시작 경계 선행. `D=commands/restart.rs:150,216,218,293` |
| `/model` Claude | 저장하고 동일 세션 재기동 | 저장만, 현재 pane 미적용 | feature parent와 actual child 구별 필요 | 재기동 skip 가능 | 현재 실행에 즉시 적용. 기본안은 R의 resume 재기동 재사용 | #8128 M | #8119 S8b1 뒤. `D=commands/config/provider_settings.rs:103,214`, `commands/control.rs:245`, `control/relaunch.rs:343` |
| `/compact` | legacy tmux 확인 뒤 전달 | host guard 거절 | actual tmux target 전달 | child endpoint 누락·legacy 확인 결과 의존 | 기존 host input operation에 `/compact` 전달 | #8128 C | #8119 S8b1 뒤. `D=commands/tui_passthrough.rs:157,176` |
| reset 필요한 설정(모델·fast·Codex goals) | pending flag→managed reset | provider 그대로 둠/재실행 skip | 부모 설정과 child 실행 ID 분리 필요 | raw endpoint 판정 불일치 | pending flag를 실제 적용·확인한 뒤 소거, R 재사용 | #8128 M/S, #8109 | Codex goals는 코드상 reset pending 확정. `D=commands/goals.rs:68`, `config/provider_settings.rs:378`, `control.rs:234,334`, `control/provider_reset.rs:16` |
| Claude `/effort`·`/fast` | 기존 설정 적용 | 살아 있는 pane 직접 적용 구현됨 | actual target 증거 필요 | H 상속 소비만 남음 | 직접 적용 경로 보존, 이 묶음에서 재시작으로 교체하지 않음 | #8109, #8118 H2 | live pane 없을 때 H 거절/T 저장 차이는 §5. `D=commands/tui_passthrough/herdr_settings.rs:183,282`, `commands/fast_mode.rs:107` |
| Codex effort·ultra | 현재 공통 미지원, #8109 예정 | 재기동 capability 필요 | 동일 | 동일 | #8109 S5가 R operation 사용 | #8128 R→#8109 S5 | 새 effort 구현을 본 묶음에 섞지 않음 |
| `/status`·`/inflight`·`/queue`·`/health` | reports의 tmux 상태 label | unsupported-host label, 본문은 나옴 | tmux 상태 조회 | raw child 판정/unsupported 차이 | 실제 host 상태 label·cwd/source 읽기 | #8128 D | `reports.rs`는 화면 capture 자체가 아님. `D=commands/diagnostics/reports.rs:37,46`, `diagnostics/session_details.rs:11,40` |
| API turn 상태·세션 진단 capture | actual tmux capture | host_unsupported | name 기반 조회 | guard/actual mismatch 가능 | resolved actual locator의 capture/cwd 사용 | #8128 D | read-only registry는 read에만 재사용. `S=agents/turn.rs:255,309,479`, `R=agents.rs:186`, `D=commands/inspect/render_session.rs:29` literal tmux label도 수정 |
| tmux_recreate | managed reset 뒤 재생성 | refusal | name 기반 | raw child 판정 위험 | 명령의 사용자 결과인 재생성을 host operation으로 수행 | #8128 R2 | **#8119 S8b1 뒤**. `D=commands/tmux_recreate.rs:91` |
| provider_reset·자동 큐 관리 reset·health recovery | reset/clear 재확인 뒤 처리 | configured refusal | actual locator 의존 | direct endpoint 불일치 | R의 exact end/relaunch를 소비, 기존 recheck 보존 | #8128 R2 | #8119 뒤. `D=commands/control/provider_reset.rs:16`, `S=auto_queue/runtime.rs:485`, `D=health/recovery.rs:267` |
| `/resume` | explicit session rebind | 하위 session_resume에서 refusal | name 기반 | raw child host 누락 가능 | actual host 대상 rebind, R 계약 재사용 | #8128 R2 | **#8119 S8b1 뒤**. `D=commands/session.rs:80`, `S=session_resume.rs:402` |
| `/stop`·`!stop`·HTTP cancel | 기존 turn/token/host stop | Herdr 중단키 전달·settlement 경로 있음 | child target이어야 함 | H 정책과 actual T가 다를 수 있음 | actual host·nonce에 대해 기존 stop 의미 유지 | #8118 H2, #8128 검증 | host 상속만으로 pane close 권한 새로 부여하지 않음. `D=commands/stop.rs:39,75,134`, `D=herdr_terminate.rs:186` |
| `!clear`·`/clear` 세션 종료 | stop·기록·selector clear, 다음 입력에서 새 실행 | end_herdr 전용 경로 이미 있음 | child 기록·locator | child가 부모 pane에 매핑되면 안 됨 | 현재 clear 의미 유지, child exact 실행 종료 | #8118 H2, #8128 E | #8119 뒤. `D=commands/control/clear_teardown.rs:70,81,115`, `commands/control/herdr_recovery.rs:28` |
| `!clear` 이전 세션 종료 기록 | Claude/Codex tmux capture/기록 | 일부 기록 수집·Codex capture skip | tmux 기록 | host 판정 불일치 | 이미 보유한 native history/usage로 같은 결과 기록 | #8128 E | tmux 화면을 Herdr로 합성하지 않음. `D=commands/control/clear_teardown/clear_end_record.rs:45`, `codex_end_record.rs:15` |
| 부팅·restore·cleanup·reaper·turn_presence | row/inflight/marker 실제 대상 | configured guard+hosted record | 복원마다 parent lookup 코드 있음 | raw child endpoint는 H 누락 | 정책 상속 복원 후 actual child locator 검증 | #8118 H2/H3, #8119 S8b2 뒤 | `D=recovery_engine/restore_inflight.rs:677,722,949`, `watchers/lifecycle/restore.rs:198`, `session_idle_cleanup.rs:75`, `turn_presence/activity/unbound.rs:305` |
| CLI guard·auth login·agent 수정·dispatched output | marker/row 가드 후 tmux IO | guard 거절 또는 host_unsupported | key/name 기반 | actual child 증거 의존 | 사용자 행위의 host-neutral operation 또는 기존 거절 보존 | #8128 D/R2·#8118 H3 | #8119 해당 조각 뒤. `src/cli/utils.rs:176`, `R=provider_auth_profiles.rs:511`, `R=agents_crud.rs:1037`, `S=dispatched_sessions/output.rs:171` |
| 배포 스모크 초기화 | cell reset 실행 | configured Herdr reset skip | 시나리오 범위에 따라 미실행 | 동일 | 같은 전제의 reset과 시나리오 양 host 실행 | #8120 (#8128 추적) | `scripts/deploy-release.sh:4514` headless E-1 N/A도 #8125 후 제거. 이 레그는 실행 안 함 |
| `!herdr consent retry/return` | 해당 없음 | 사용자 Herdr 서버 동의/복귀 | 정책 부모·실행 child 구별 필요 | 현재 직접 endpoint | 동의 capability는 endpoint 정책에서 읽고 target은 child | #8118 H3, #8128 추적 | 사용자 소유 Herdr 동의는 불가피한 차이 (§5). `D=router/herdr_consent.rs:34`, `herdr_consent/return_input.rs:14` |
| `/pwd`·`/system-prompt`·`/skill`·`/sidecar`·`/tool-call`·`/usage`·`/node`·`/adk`·`/help`·`/allowall`·`/cancel-queued` | 일반 handler | 직접 host 분기 없음 | 일반 handler | 일반 handler | 동일 계약 실물은 #8120에서 확인 | #8128 추적·#8123 | host 분기가 없다는 조사이지 E2E 통과 아님. `/adk` 진단 backend literal은 위 D에 포함 |
| 옛 `!shell`·`!start`·`!sessions`·`!machine-flip`·`!down`·`!meeting`·`!metrics`·`!receipt` | 폐지/교체 안내 | 동일 | 동일 | 동일 | **재구현하지 않음: 이미 main 충족** | #8128 전수 완료 항목 | `D=commands/command_policy.rs:80,88,90,91,92,93,96,99`, `text_commands.rs:76` |

위 공통 명령 행의 실제 handler 좌표는 `commands/session.rs:142`(pwd), `system_prompt.rs:97`, `skill.rs:341`, `sidecar.rs:10`, `tool_call.rs:180`, `receipt.rs:19`(usage), `node.rs:292`, `inspect/mod.rs:61,67`(adk), `help.rs:83`, `config.rs:557`(allowall), `control.rs:715`(cancel-queued)다(모두 `src/services/discord/` 아래). slash 25개의 등록 위험 분류는 `commands/command_policy.rs:193-216`, health/status/inflight/queue handler는 `commands/diagnostics/mod.rs:13,30,47,64`다. `/queue`·`/health` 자체 보고는 공통이며 reports의 host 상태 차이를 실제 소비하는 status/inflight 행과 구분한다.

### 입력 출처와 소유 경계

| 출처 | 기존 분류·소유 | 이번 변경에서 유지할 것 |
|---|---|---|
| Discord 사람 메시지 | owner original/front, 실제 channel/thread ID | 부모 정책은 runtime 선택에만 사용, 원문·reaction·receipt는 child 원본 |
| iMessage source의 start/deliver | `queued_head/v2.rs:59-66`이 routine/scheduled가 아닌 imessage를 사람으로 분류, `input_owner/start.rs:18`·`origin.rs:45`는 GUID identity 복원 | 옛 GUID 없는 사람 분류도 유지, GUID가 있으면 기존 dedup 유지; 새 GUID 의무 추가 금지 |
| 일반 HTTP turn/deliver | source 기본 external·author preflight 뒤 기존 HumanInputRequest (`agents_turn_deliver.rs:150`, `health/turn_deliver.rs:36`) | generic turn/start의 machine 정책과 구별, 기존 주입/queue 의미 유지 |
| 일반 HTTP start·routine·scheduled·restart seed | machine start, `input_owner/target.rs:78,172`의 자기 턴 대기 | endpoint 거절을 열어도 사람 busy paste로 바꾸지 않음 |
| 터미널 사람 입력/turn | native source·binding/history 증거 | #8114 typed observation으로 합류; 부모 transcript를 child source로 쓰지 않음 |
| 부팅/재시작 복원 | stored original/row/inflight·nonce, 입력 새 생성 아님 | old receipt 재admit 금지, 확인된 actual locator 보존 |

## §2 부모 정책 판정 단일화

### 한 함수와 두 종류의 사실

기존 `services/session_host` 안에 `resolve_surface_policy(VerifiedSurface, BootPolicyView) -> Result<SurfacePolicy, PolicyUnknown>`를 둔다(이름은 제안). `VerifiedSurface`는 actual destination ID와 확인된 thread parent ID, provider/runtime kind를 구조 필드로 가진다. 같은 parent 판정을 launch·busy·output·cleanup caller가 다시 만들지 않는다. low-level `config/session_hosts.rs:349`는 boot map 원시 접근자이고 허용 소유자는 config 자체와 이 경계 adapter로 줄인다. live 설정에서 endpoint를 다시 읽지 않는다. `BootPolicyView`는 immutable boot lookup의 얇은 입력 view이고 O actor/store를 session_host에 소유시키는 새 trait나 registry가 아니다. O의 adoption·writer·게시 권한은 기존 tui_o owner에게 남긴다.

`SurfacePolicy`에는 `destination_id`, `policy_channel_id`, `runtime_kind`, `desired_host`(endpoint 포함), `output_selection`(O 여부·site·boot kind), 판정 provenance를 둔다. 일반 채널은 두 ID가 같고 실제 thread는 `policy_channel_id=verified parent`, `destination_id=child`다. 부모의 role/workspace/memory 상속 `thread_inherit`(`agentdesk_config.rs:240`)와 host/output 상속은 다른 정책이다. 이 이슈의 host/output 상속을 live role flag에 묶지 않는다. provider/runtime이 부모 O kind와 맞지 않으면 기존 KindMismatch처럼 보류하며 잘못된 provider로 강제 전환하지 않는다.

**설정은 다음 새 launch를 고르는 정책, actual row/marker/inflight는 현재 I/O 권한**이다. 실제 target은 기존 `session_host/resolve.rs:239-260 resolve_session_target`과 #8114 InputTarget/InputTransport operation으로 구한다. `SurfacePolicy`로 `ResolvedSessionTarget`·HostedExecution nonce·physical writer·native source 증거를 대체하지 않는다. 설정 H로 바뀌어도 살아 있는 옛 tmux child는 계속 그 actual host로 처리하고, 다음 자연 새 실행부터 Herdr다. 자동 migration·rename·kill은 추가하지 않는다. #8077 명시 migration 경로는 그대로다.

Discord 경계에서 실제 Public/Private/NewsThread만 parent로 인정한다. category parent는 상속 근거가 아니다(`session_runtime/channel_routing.rs:212-217`). `checked_thread_parent:296-303`의 Err를 보존하는 조회를 재사용한다. best-effort `resolve_thread_parent:276`의 None을 권한 검증이나 host fallback에 쓰지 않는다. API metadata의 parent, 표시 이름의 `-t`, session-key 문자열은 권한 증거가 아니다. 검증된 parent ID는 같은 요청의 target에 전달해 안쪽에서 재검증을 반복하지 않는다.

### 부팅 목록을 동적 스레드에 적용

1. 부팅 host 목록은 `BootSessionHosts`의 검증된 map(`session_hosts.rs:114,158,333`), O 목록은 `BootChannels`의 selected/kind/site(`tui_o/channel_policy.rs:28,74,231,628`)다. **스레드 ID를 운영 설정에 추가하지 않는다.** `all_tui`도 부모 TUI binding의 선택을 상속한다.
2. thread metadata의 생산은 첫 Discord intake, API target resolver, routine thread 생성/재사용, stored session/inflight/binding 복원 경계다. 경계가 같은 `VerifiedSurface`를 만들고 기존 O process snapshot 안의 **공유 파생 child-entry map**에 붙인다. 현 `BootChannels.channels/kinds/candidates`는 값 clone이고 `cutover::boot_snapshot:24`도 clone이므로, 그 값 map에만 insert하는 안은 금지다. BOOT가 소유한 `Arc<Mutex<ChildEntries>>` 또는 동등한 한 shared view를 모든 snapshot clone이 공유하도록 기존 container를 확장한다. entry는 unresolved/held 또는 검증된 policy+child candidate의 완전 tuple이며, 동일 child 등록은 멱등이고 서로 다른 parent/kind 충돌은 hold한다. gate·boot_ownership·prepare/refresh·writer 등록이 같은 view를 읽고 partial ready를 공개하지 않는다. unresolved child/candidate 부재는 `channel_gate.rs:473-474`의 legacy 반환으로 떨어지지 않고 Held/identity error로 남긴다. 새 별도 DB·영구 registry·환경 switch는 만들지 않는다. sync body gate가 HTTP를 부르지 않도록 검증·등록은 source/turn/body 공개 전에 끝낸다. 불명 thread는 typed held로 남기며 조회 실패를 tmux/legacy/no-parent로 변환하지 않는다.
3. 부모에서 상속할 것은 selection·kind·site·desired host다. **candidate/adoption·store cursor·checkpoint·OwnerPermit·intake custody·원본·native source·writer readiness는 child별로 생성·복원**한다. `channel_policy.rs:421,427`과 `ownership/execution.rs:128`의 실제 identity는 child를 쓴다. 부모의 Candidate/permit Arc를 alias하면 sibling 답·원본이 섞이므로 금지다. O 비선택 부모의 독립 legacy/non-TUI 계약은 확장하지 않는다.
4. O 조각은 정책 함수의 output 부분과 child 등록을 **같이 연결**한다. `cutover/channel_gate.rs:433`의 membership만 true로 만들고 writer가 없는 상태를 성공으로 내면 답을 잃는다. child별 `boot_ownership` 열거·owner_execution prepare/refresh·writer start_managed·source binding·checkpoint/ready wake를 모두 연결한다(`cutover.rs:30`, `runtime_bootstrap/owner_execution.rs:70,233`, `o_writer_host.rs:429,796,868`, `tui_o/writer/host.rs:530,684`). 새 child도 동일 existing activation/adoption/custody 절차를 통과한다. parent ready는 child ready가 아니다.
5. 부팅은 **동기 install의 committed child 처리부터** 바꾼다. 현 `bootstrap.rs:73`→`channel_policy::seeded:278-285`는 `stored_with_committed(adoption.rs:394-409)`의 child를 kept로 찾은 뒤 `listed_kinds(config,"o_store",child)`에서 non-binding 오류로 부팅을 중단할 수 있다. 먼저 동기 validate/install은 explicit 선택 목록의 기존 오류 검사를 유지하고, binding이 없는 committed ID를 삭제하거나 ordinary로 추정하지 않고 shared child map의 unresolved Held entry로 보존한다. 이어 provider runtime/watcher를 열기 전 기존 async bootstrap 준비 경계에서 committed 목록과 session/inflight/binding 목록의 합집합을 checked Discord 조회한다. 검증된 thread는 부모 boot policy와 own stored adoption으로 entry를 완성하고 owner prepare→writer start→ready/wake로 잇는다. 확인된 일반 non-binding kept ID는 기존 `listed_kinds:197-200`의 config 오류를 관련 provider runtime을 열기 전에 반환하는 처분을 유지하며, metadata 조회 실패는 ordinary 오류로 날조하지 않고 해당 ID만 Held/진단으로 남겨 전체 process boot를 막지 않는다. **child store만 남고 session/inflight가 없어도 복원 대상**이다. S6 부모 필드가 없는 old context는 checked REST로 보완하고 숫자 꼬리를 추측하지 않는다. 영구 child store는 단순 parent selection 재계산으로 버리지 않는다. 복원 중 row 불명도 해당 child만 hold한다. 첫 요청·복원·gateway 재개·execution refresh가 모두 같은 파생 목록을 보며, 새 thread 등록 후 writer start와 ready wake가 child owner를 깨운다. 새 임의 polling loop는 만들지 않는다.
6. 살아 있는 tmux thread의 **host는 유지하면서 출력만 O로** 넘길 수 있다. 기존 late adoption fence·진행 중 legacy body claim을 child 단위로 기다린 뒤 O가 소유한다. legacy와 O 동시 body는 0, adoption 불명은 보류다. 본문 경로를 다시 legacy로 열지 않는다. #8118 댓글의 source-not-bound는 O 전환만으로 해결됐다고 가정하지 않는다. Discord/API/terminal 시작 각각 child history binding watermark와 native source가 먼저 등록됐는지 통합 시험으로 증명한다(`tui_input/human_frame/history/v2.rs:395,545`).
7. 종료/clear/재시작은 child actual execution만 정리하고 부모·sibling pane을 건드리지 않는다. 설정 H지만 live T인 행에서도 actual-target operation을 쓴다. 기존 row/nonce/CAS·source fence·pre-mutation recheck를 넓히지 않는다. unknown을 absence로 보는 현재 main의 주변 결함을 이번 권한 확대의 근거로 사용하지 않는다.

### Herdr 배치와 수명

이번 H1은 **기존처럼 logical child 실행마다 별도 managed workspace(그 root tab/pane)**를 선택한다. 같은 endpoint/herdr session 안에서 logical key+nonce marker로 parent와 관련된 child를 식별하고, 표시 이름은 #8119 typed name/metadata를 따른다. label을 파싱해 권한을 얻지 않는다. 근거는 현 `herdr_launch/prepared.rs:35-58`의 `workspace_marker(logical_key,nonce)`와 `session_host/herdr/launch_host.rs:571`의 WorkspaceCreate다. unplaced 복구는 workspace pane/tab 1개를 전제로 한다(`launch_host.rs:765-780`). 부모 workspace에 tab을 추가하는 안은 이 가정과 exact residue cleanup을 바꾸므로 이 묶음에 넣지 않는다. 같은 workspace 배치가 필수라는 사용자 결정은 없으며, 화면 배치의 차이는 §5에서 설명한다.

수명은 AgentDesk의 현재 exact row-end/close/retire 경로가 관리한다. Herdr Core parent/child, parent 종료시 자식 자동 kill, 새 TTL/reaper/GC는 #7583 또는 별도 요구 없이 구현하지 않는다. 끝난 thread는 기존 세션 종료 권한으로 그 실행만 닫는다. ‘pane 수 증가’를 이유로 main에 없는 삭제 권한을 얹지 않는다.

### 판정 소비 좌표와 A 첫 기준선

부록 A는 요구된 네 패턴의 **254개 Rust 텍스트 hit/142파일 전체**다(정의·import·문자열·시험 포함). 전체 tracked 검색은 287 hit/152파일이며 Rust 밖의 smoke N/A 분기도 조사했다. 이것을 254 production 호출이라고 세지 않는다. 단순 파일명 시험 제외도 98 hit/69파일로 inline cfg(test)·import가 섞인다. #8122 A의 scanner가 실제 cfg/test·정의·alias를 판별해 production site 기준선을 생성해야 한다.

| 소유·소비 묶음 | 대표 좌표와 전수 범위 | 공통 함수/operation으로 가는 조각 |
|---|---|---|
| boot 원천·host factory | `config/session_hosts.rs:158,349,350`, `herdr_launch.rs:56,62`, `turn_host.rs:157,186` | 원천은 경계 내부로 한정, policy resolver를 통과. H1 |
| launch·absence | `claude.rs:2607`, `claude/tui_session_launch.rs:38,53`, `codex.rs:1426,1527`, `codex/absence_launch.rs:78,294`, `codex/tui_session_launch/{first_input.rs:216,launch_policy.rs:70}`, `discord/codex_absence.rs:30` | H1; 기존 admission/refusal 유지 |
| intake·queue·actual source | `discord/router/intake_gate/busy_inject.rs:86`, `router/message_handler/intake_turn.rs:1110,1754`, `queue_dispatch.rs:253`, `queue_io/input_owner/claude_handoff.rs:219`, `router/message_handler/tui_followup/nameless_gate.rs:79`, `session_runtime.rs:513` | #8114가 없앨 중복 재구현 금지. H1 |
| synthetic source·O actor·notice | `discord/tui_prompt_relay.rs:757`, `tui_prompt_relay/synthetic_start/admission_fence.rs:168`, `tui_prompt_dedupe/hosted_source.rs:64`, `tui_o/writer/actor.rs:423`, `tui_o/writer/input_notices/admission.rs:104`, `tui_prompt_dedupe/runtime_binding/codex_policy/absent_open.rs:67` | O 조각은 필요한 output adapter만, 나머지 H2. actual source pin 유지 |
| API 주입·presence | `discord/health/turn_deliver/inject.rs:312`, `inject/herdr.rs:48`, `turn_presence/activity/unbound.rs:305,525`, `unbound/completed_scheduled.rs:162`, `unbound/herdr_bound.rs:18` | API T1/T2와 H2 |
| 명령 설정·clear | `discord/commands/config/provider_settings.rs:117,190,326`, `control.rs:466`, `control/clear_teardown.rs:70,81,115`, `clear_end_record.rs:45`, `codex_end_record.rs:15`, `control/herdr_recovery.rs:28`, `fast_mode.rs:108`, `tui_passthrough.rs:128` | H2 판정만, 기능 차이는 #8128 R/M/C/E; #8119 뒤 |
| consent·exact recovery | `discord/router/herdr_consent.rs:34`, `herdr_consent/return_input.rs:14`, `herdr_terminate/root_replaced.rs:249`, `root_replaced/preclaim.rs:22`, `admin_host_guard.rs:115` | H3; stored exact endpoint와 current policy를 혼동하지 않음 |
| admin guard 소비자 | 부록의 `admin_host_guard` 전부: cli·agents API·auth·auto_queue·restart·tmux_recreate·diagnostics·health/recovery·inflight·stale_turn·idle_cleanup·tmux_watcher·dispatched output·provider guard·routines·session_activity/resume | 설정/실행 판정 중복은 H3로, 관리 기능 개방은 R/R2/D. guard 호출 자체가 저수준 endpoint 직접 사용인 것은 아님 |
| legacy 삭제 예정 | `discord/relay_recovery/apply.rs:100`, `tmux_watcher/host_gate.rs:153` 등 부록의 legacy caller | #7651 삭제 쪽에 귀속. 동작 수리 금지; 살아 있는 O 경계가 이 좌표를 우회하는지는 O 시험으로 검증 |

이 표·부록이 migration checklist다. H1/H2/H3 합계가 한 리뷰 조각의 30파일 상한을 넘으므로 한 번에 바꾸지 않는다. A가 baseline을 새로 늘려 숨기는 일 없이 각 조각 후 direct site 감소를 증명한다. high-level 공통 policy/operation 호출이 늘어나는 것은 재사용이며 금지하지 않는다. 완성 시 **허용 owner 밖 direct policy 판정 0**, legacy 삭제 예정 caller는 삭제 ref/담당을 적고 #7651과 닫는다.

## §3 이슈별 구현 조각·계약

### 순서와 크기 규칙

모든 조각은 prod **30파일/순 코드 +1500**, PLAN 정지선 **+1200**을 적용한다. 아래 수치는 실행 전 추정으로 시험·모델 문서는 별도며, 경계 이동으로 파일이 늘면 먼저 다시 나눈다. 실측/추정은 구현 보고서에 함께 적는다. 기준선·audit 예외 수정은 FILES 승인이 필요하고 숫자를 올려 상한을 피하지 않는다. 여기에는 생산 코드를 추가하지 않았다.

착지 순서는 `#8114 배포 → #8118 O 배포 → #7651 P2/P3 허용`이다. O가 이 묶음의 **첫 구현 조각**이다. 그 뒤 `H1 → #8125 T2`, `#8125 T1 → #8128 R → #8109 S5`로 잇는다. T1(herdr 채널 external start)은 H1 thread 기능과 분리할 수 있지만 O보다 먼저 착지하지 않는다. T1은 early refusal만 제거하지 않고 최종 `StreamingTurn`까지 동일 admitted executor·provider switch·O-ready·actual target 가드를 연결한다. feature off/미준비·Unknown 거절은 계속 유지한다. 관리 명령의 actual locator 소비는 **#8119 S8b1 뒤**, 복원/routine 이름 해석은 **#8119 S8b2 뒤**, BindingContext optional parent_channel_id/thread_id의 schema·producer 변경은 **#8119 S6 뒤**로 표시한다. O가 필요한 checked live parent metadata는 기존 Discord 경계를 사용하며 S6 필드를 중복 신설하지 않는다. S6 이후에는 immutable BindingContext parent/thread도 소비하되 HTTP metadata와 권한 provenance를 구별한다. `/resume` raw old-alias 정규화는 #8119 S3+S8b1 뒤다. d2의 primary tail=current actual name/old alias transaction을 그대로 소비하며 과거 routine ownership token을 successor locator로 정규화하지 않는다. 이 설계에서 #8119 코드를 선취하지 않는다. #8115 배포는 no-session thread 수용의 선행이다. 각 독립 조사·시험 설계는 병렬 가능하다.

| 조각 | 구현 범위·주요 파일군 | 추정 prod / 순증 | 선행·정지 조건 | RED 계약 → GREEN, 원인 제거 뮤턴트 |
|---|---|---:|---|---|
| **8118-O** | session_host surface policy의 output 계약, channel_policy 파생 child entry, cutover gate·boot_ownership, owner_execution prepare/refresh·o_writer_host 등록/재개, child binding/source·checkpoint·ready 연결 | 16~24 / +900~1150 | **#8114 배포 뒤 첫 구현**. +1200 예상시 policy+writer를 불완전하게 나누지 말고 DESIGN_DELTA. 기존 모델 delta 0단계 | parent O/child 미선택에서 legacy body RED→child O answer 1회, parent/sibling 영향0. 살아 있는 tmux thread 출력 O 채택·crash/boot child 복원·terminal/Discord source bind·기존 내부 starter의 API-origin fixture·ready wake. committed child store만 남은 부팅과 parent REST 실패시 child만 Held도 검증. 공개 API thread 실증은 T2 뒤. direct-ID membership 복원, parent Candidate/ledger alias, readiness 선발표, unknown→legacy 뮤턴트 각각 RED |
| 8118-H1 | 같은 SurfacePolicy의 desired host 소비: herdr_launch/turn_host, Claude/Codex launch/absence, intake/queue/bootstrap. #8114 공통 operation을 사용 | 17~24 / +450~750 | O·#8114 배포, thread bootstrap은 #8115, BindingContext schema/producer 교차는 #8119 S6 뒤. 새 core host match 발견시 DELTA | H parent 새 child H·T parent child T, live old T 보존·다음 launch만 H. fake host operation 공통 흐름. direct-child endpoint/parent actual locator 치환/unknown fallback 뮤턴트 RED |
| 8118-H2 | source·presence·O notice·clear/설정 predicate를 공통 정책/actual target에 적응. 관리 기능은 아직 기존 의미 | 18~26 / +350~650 | H1, 명령 부분 #8119 S8b1 뒤. guard 개방을 섞지 않음 | H/T × channel/thread의 capture/source/stop/clear 실제 child 대상. old T row+H policy에서 T IO 유지. source pin 제거·정책 host로 actual IO 선택 뮤턴트 RED |
| 8118-H3 | consent/exact recovery·admin guard·주변 cli/agents/provider cleanup 소비자. legacy caller는 #7651 귀속. audit direct-site 잔여 닫기 | 16~24 / +250~500 | H1/H2, #8119 S8b1/S8b2 해당 후. owner 밖 direct 판정 최종 0 | unknown/conflict/Pending/stale nonce에서 mutation0, child clear/cancel이 parent·sibling에0. guard의 parent lookup 누락·stored endpoint 대신 config 재선택 뮤턴트 RED |
| **8125-T1** | Herdr 채널 external start/deliver. headless의 별도 executor_not_wired 거절을 공통 admitted provider executor로 연결. existing busy Herdr inject는 재사용 | 6~10 / +350~650 | O·#8114 배포. #8128 R seed의 선행 | idle H start가 422인 RED→accepted/started, busy imessage는 공통 주입·generic machine은 자기 턴. tmux 호출0, GUID retry1회, unconfirmed no-resend. 일반 refusal 복원·중복 admission·machine human 오분류 뮤턴트 RED |
| 8125-T2 | 양 API 공통 target: checked thread parent 권한, provider/runtime owner와 destination 분리, no-session bootstrap, deliver HumanInputRequest 적응, 문서 계약 갱신 | 7~11 / +400~700 | T1·H1·#8115 배포. #8119 thread descriptor 충돌은 뒤 | 두 endpoint × T/H × public/private/news thread 허용, 실제 child session/turn ID, foreign/missing parent/non-thread category/lookup error403·provider mismatch422. exact-bound only 복원·metadata parent 신뢰·child로 provider 조회·parent dedupe key 뮤턴트 RED |
| **8128-R** | `/restart`의 host-neutral cancel→PreserveSession stop→exact end→native resume relaunch→seed. 종료/launch owner와 session_host operation 경계 재사용 | 8~13 / +550~900 | **#8114·O·#8125 T1 배포 및 #8119 S8b1 뒤**, H1/H2 뒤 thread 수용 검증. 이슈 내 첫 관리 조각 | H restart 현재 refusal RED→busy 취소·old pane close·new nonce pane+동일 native ID·seed1회·다음 메시지 정상. no-session은 close RPC0+fresh seed, sid 없는 active는 exact end 뒤 fresh 안내. disabled/refused preflight는 cancel/close/launch0. close ACK 불명/서버 교체/stale nonce에서 spawn0. close 확인 제거·clear로 native ID 삭제·seed refusal 복원·parent pane close 뮤턴트 RED |
| 8128-R2 | tmux_recreate·provider_reset·auto_queue/health managed reset·resume가 R operation 재사용 | 8~14 / +350~600 | R·#8119 S8b1/복원 S8b2 뒤, resume은 S3도 선행 | 각 행 동일 결과, pending/reset recheck race 기존 보존, foreign actual0. 공통 operation 우회·verified clear 직전 recheck 제거 뮤턴트 RED |
| 8128-M/S | Claude model 즉시 적용 및 reset 필요한 설정·Codex goals. idle 경계에서 동일 native resume 재기동, busy는 기존 defer·현재 턴 취소0, 저장/적용 결과를 분리 | 5~9 / +300~550 | R·#8119 뒤, #8109 설정 계약 재대조 | 저장 성공+현재 execution 설정 적용 확인, 실패시 pending flag 보존. live skip·적용 전 flag 소거·wrong child target 뮤턴트 RED. Claude effort/fast 직접 적용은 제외 |
| 8128-C | compact를 actual InputTarget/MutationGate에 전달 | 3~6 / +150~300 | #8114 operation·H2·#8119 뒤 | H live compact 전달1회/T 회귀0, unknown/changed target key0. legacy-only guard 복원·per-mutation pin 제거 뮤턴트 RED |
| 8128-D | 진단 상태 label·실제 capture/cwd/native source·API host_unsupported·`/adk` literal label | 6~10 / +300~550 | H2·#8119 S8b1 뒤 | actual H capture 표시, read 불명 명시·mutation0, channel/thread 대조. tmux name capture 복원·read 실패를 absent 처리 뮤턴트 RED |
| 8128-E | clear 결과/종료 기록·Codex capture skip의 사용자 차이 확인·적응 | 4~7 / +200~400 | R·H2·#8119 뒤 | clear 전 native ID/usage 기록 보존, clear 후 새 세션 의미 유지, parent/sibling 기록 영향0. herdr 기록 skip 복원·parent history 사용 뮤턴트 RED |
| 8122-A | 소유 모듈 밖 low-level accessor site ratchet·Rust lexical scanner 보완·baseline generator·CI/prepush wiring | 5~8 script / +350~650 | 최초 baseline 승인, H 조각과 병렬 가능; 신규 직접 분기 금지 | 새 site/동수 site 교체/alias/multiline 증가 실패, helper 재사용·owner·test·string/comment 통과. 총수만 비교·owner scope 오해·inline test 미제거 뮤턴트 RED |
| 8122-B | 3곳 반복 조건식 warning·main 군집 TP/FP/불명 실측·채택 보고 | 3~5 script / +300~550 | A scanner 계약 재사용 | 2회0→3회 경고, whitespace/comment/괄호 동치, literal·순서 다른 식 분리. threshold4·&& 재정렬·string 오인 뮤턴트 RED |

### #8118 출력 수용과 #7651 인계

O 조각은 ‘legacy 전송을 막았다’만으로 완료가 아니다. 부모 O 아래 thread에서 **10턴이 실제 O로 답한 증거**와 다음 항목을 같은 배포 SHA·시각창으로 내야 한다: 남은 placeholder0, 답장 형태 본문0, 중복 body0, 도구 카드 정상, native session usage 푸터, legacy body send0, #7651 census의 해당 child 새 전송0. 기존 296건을 삭제·리셋하거나 누적 전체를 0으로 만들지 않는다. 미실행/빈 메시지 집합/비활성 writer에서 0건은 PASS가 아니다. O의 source binding은 Discord 시작·terminal 시작 및 기존 내부 with_owner_channel starter/격리 fixture의 API-origin 각각 busy inject 1회를 포함해 `source not bound` 재현 계약을 확인한다. 공개 turn/start·turn/deliver의 thread 실증은 현재 403이므로 T2 뒤의 별도 수용 항목이며 O를 기다리게 하지 않는다. 본 레그는 이 실증을 수행하지 않았다.

### #8125 권한·입력 보존

공통 `AgentTurnTarget`은 현재 provider/primary/channel만 갖는다(`agents_turn_target.rs:8`). 목표는 verified owner channel과 actual destination을 별도 필드로 전달하는 것이다. bound channel은 기존 동작, thread는 checked Discord kind+parent와 **요청 agent의 bound parent** 일치로 허용한다. live child row가 있어도 권한 검사를 건너뛰지 않는다. metadata.agent_id mismatch403(`agents.rs:742`), provider mismatch422(`agents_turn_target.rs:117`), author preflight(`health/turn_deliver.rs:70`), source GUID/obligation/unknown no-resend는 그대로다.

runtime/settings/binding 선택의 policy는 parent, mailbox/session/turn-ID/output/origin identity는 child다. 기존 `health/headless_turn.rs:100-139` owner-channel starter, `health/runtime_resolve.rs:167-224`의 runtime parent 수용을 재사용한다. no-session은 Discord 첫 메시지의 bootstrap/worktree/placement를 공통 호출하며 새 thread 전용 launch 구현을 복제하지 않는다. #8115의 ready preferred fallback은 그 이슈에서 고친 것을 전제로 통합시험한다. API가 child를 받는다는 이유로 routine process fresh·scheduled snapshot scope를 일반 channel 세션에 합치지 않는다.

### #8128 재시작 경계

R은 기존 lifecycle owner가 operation을 조립하고 host adapter가 exact pane IO를 맡는다. `InteractiveSessionHost` 기본 Herdr registry는 read-only(`session_host/herdr_registry.rs:49-87`)이므로 그 `send_text`/sole_host를 mutation factory로 쓰지 않는다. #8114의 actual target/observation·host input, `!clear`의 exact end, #8077 close·existing nonce/CAS를 재사용하고 공통 명령 파일에 새 `if herdr` 분기를 늘리지 않는다. creation/destruction 권한은 현 owner에게 남긴다(`session_host.rs:1-3`).

순서는 기존 admission/직렬화 확보→same actual execution pin→cancel→PreserveSession stop→exact close/end 확인→old row retire 확인→같은 native session을 resume하는 새 execution launch→O ready/source binding→완료 seed다. clear는 대화 기록·selector를 지우는 다른 operation이므로 통째로 호출하지 않는다. 특히 `clear_teardown.rs:175-186`의 after_retire는 `clear_retired_selector_pg`를 호출하므로 R이 재사용할 대상이 아니다. `herdr_terminate::OperatorTerminate`와 exact end/RetiredExclusion을 재사용하고 `retired_exclusion.rs:99-104 disconnect_session_keeping_provider`처럼 native ID를 보존하는 postlude를 고른다. provider_reset에서는 별도 reset 플래그에 따라 native ID/history clear 의미를 유지한다. close가 Unknown/Pending/stale/replaced이면 spawn·retire 강행0, 명시 실패로 끝낸다. 새 launch의 ACK 불명도 기존 pending/unplaced 복구를 쓰며 재생성 반복을 하지 않는다. `herdr_terminate.rs:118-120,446-447`의 기존 terminate live switch/명시 사용자 권한은 사전 admission에서 확인한다. Disabled/refused이면 cancel·close·row·launch0으로 반환하고 성공을 표시하지 않는다. 값은 이 레인에서 읽지 않았으며 가드 우회/새 스위치/자동 권한 확대는 하지 않는다. 실제 적용 범위가 R과 충돌하면 구현 전에 DESIGN_DELTA다. 사전 통과 뒤 경합으로 종료가 거절되면 기존 cancel 완료 여부를 있는 그대로 보고하고 후속 launch0으로 끝낸다. tmux의 현재 stop/reset 의미·held queue 처리도 바꾸지 않는다. `/restart`만 열고 `restart.rs:150`의 headless seed가 거절되는 미완성은 금지다.

`RestartAction`의 기존 세 분기(`restart.rs:17-25,45-50,68-82`)도 보존한다. actual old execution이 확인된 resumable은 위 순서, sid 없는 active는 exact end 뒤 기존 안내에 따라 fresh, **NoActiveSession은 absence가 검증된 경우 close/retire RPC 없이 fresh seed**다. missing sid를 이유로 history를 추가로 삭제하지 않는다. unknown/Pending/nonce 충돌은 NoActiveSession이 아니며 known-retired 재시도도 현재 successor 부재·권한을 확인한다. M/S 설정 적용은 R의 취소 정책을 복사하지 않는다. 기존 `control/relaunch.rs:288-293`처럼 busy면 pending을 두고 idle/다음 턴 경계에서 적용하며 현재 턴 cancel/close는 0이다. R에서 재사용하는 것은 same-native relaunch 수단이다.

### #8122 A·B 검사 설계

A는 저수준 접근자의 **site multiset**을 감소만 가능한 기준선으로 고정한다. rule별 `accessor token, owned_paths, path, enclosing_fn, normalized token hash, count`를 공식 generator로 저장하고 line은 진단용으로 둔다. 총수만 비교하면 ‘old 1개 삭제+new 1개 추가’가 통과하므로 새 site·site count 증가는 severity=error다. 공통 helper 호출 반복은 검사 대상이 아니다. use alias·qualified/method 호출을 판별하고 주석·문자열·시험 파일·inline `#[cfg(test)]`를 제외한다. 무조건 파일명 필터/regex comment stripping을 쓰면 현재 전례도 오탐한다(`scripts/audit_maintainability/common.py:42,105`). 필요한 최소 lexical 보완은 A 범위다. 의미상 같은 식 전체를 증명하는 AST 프레임워크는 새로 만들지 않는다.

baseline은 rule별 generated file로 나누고 최초 등록은 승인된 corpus로 한 번, 이후 `--reduce`는 삭제·감소만 허용한다. 일반 실행에서 baseline 자동 증가/allow 예외를 만들지 않는다. `--check-baseline`은 재생성 차이를 검증한다. 검사 등록은 `scripts/audit_maintainability.py:37-53`, 실제 실패는 `:429-438`의 **severity=error**다. hard_gate=True만 넣고 CI 연결됐다고 보고하지 않는다. 기존 prepush runner와 `scripts/ci-script-checks.sh:827` 양쪽에 fixture·검사 실패가 전달되는지 원인 제거 뮤턴트로 확인한다.

B는 if/else-if·match guard·bool let/closure의 복합 조건 토큰을 뽑아 whitespace/comment/불필요 괄호/숫자 구분자를 정규화한다. identifier/literal/operator와 short-circuit 순서는 보존하고 변수 alpha rename·`&&`/`||` 정렬은 하지 않는다. 2개 이상 predicate의 같은 조합이 **서로 다른 3개 site**에 있으면 warning이며 site 수·함수 수도 함께 낸다. 단일 helper call·단일 비교는 재사용/관용구라 제외한다. macro/cfg 해석 한계도 보고한다. 초기에는 warning이다. main 전체 군집을 TP/FP/불명으로 분류하고 precision=TP/(TP+FP), 불명 별도 수치와 실제 hit 예시를 남긴 뒤 hard gate 채택 여부를 판단한다. 이번 레그는 scanner가 없으므로 오탐률을 꾸며 적지 않았다. B 구현의 필수 산출이다.

첫 묶음 순서는 ① **host 판정**(§2·부록 A, H1/H2/H3) ② **이름 의미 해석**(#8119 뒤) ③ **반응 표시 정책**(#8113 뒤)다. 이름 접근자 후보는 `domain/provider/naming.rs:7,44`(owner), `domain/session_identity/naming.rs:4,10`(owner), `cli/doctor/orchestrator.rs:2719`, `cli/utils.rs:178,185`, `services/cluster/session_matcher.rs:49,365,519`, `discord/health/o_transition/codex_census.rs:47`, `health/watcher_respawn.rs:167`, `recovery_engine/state_extractors.rs:139`, `relay_recovery/decision.rs:252`(삭제 대상), `tmux_reaper/host_guard.rs:155`, `turn_presence/activity/unbound.rs:463`, `unbound/completed_scheduled.rs:116`, `unbound/hold_leaf.rs:70`, `maintenance/jobs/worktree_orphan_sweep.rs:634`다. 정의/import/inline 시험을 뺀 최종 baseline은 #8119 후 생성한다.

직접 reaction 호출은 main에서 이미 `discord/reaction_lifecycle.rs:145,149`, `turn_view_reconciler/apply.rs:217,222` owner에 모여 있다. 이를 다시 고치지 않는다. #8113 이후에는 표시 정책의 인라인 복사를 모은다. **정책 호출 자체가 빠진 경로는 A/B 모두 잡지 못한다.** 진입점 표·공통 정책·E2E가 담당한다.

브리프·리뷰 틀의 한 줄: `같은 판정이 세 번째 복사되면 소유 공통 함수로 모은다. 새 저수준 접근자 확산은 #8122 rule·owner·generated baseline·전수표를 함께 검토한다. 공통 함수 호출 반복은 복사가 아니다.`

## §4 상태 차트 가정과 조각 0단계

`docs/statecharts/README.md` 색인과 아래 기존 모델을 읽었다. 이번 설계에서는 **TLC를 돌리지 않았다**. 따라서 이 표는 실행 결과가 아니라 모델 delta 명세다. `expected.tsv:215-216`의 기본 session-lifecycle cfg는 known-gap RED이며 현재 main이 GREEN이라는 근거가 아니다. 기존 RED를 지우거나 invariant를 약하게 해 통과시키지 않는다. #8119의 old/new locator delta와 겹치는 모델은 그 조각 뒤 합성한다.

| 기존 모델·좌표 | 현재 가정 | 바꿀 delta·담당 조각 0단계 | 지킬 safety/liveness·RED/뮤턴트 |
|---|---|---|---|
| `session-lifecycle/TmuxSession.tla:2,10,99,111,203` | 한 channel, channel에서 유도한 tmux name·queue·inflight, crash/restore/open | policy parent와 actual child/session/locator 분리. #8119 locator delta 뒤 H1/H2·R | LiveLocatorPreserved, CleanupRequiresSameExecution. live old T+H policy에서 T 보존, child→parent kill 뮤턴트 RED |
| `session-lifecycle/HerdrSession.tla:13,34,110,135,159,184` | 한 channel의 Pending/Bound/Retired·pane·pin | child actual row/key/nonce와 parent policy 분리(H1), authorized restart cancel/close-confirmed/retire/launch/seed 추가(R) | PromptOnlyToAgent·NoDuplicatePane·RowMatchesPane 유지. close 불명→spawn, stale nonce close, parent close 뮤턴트 RED. restart seed eventually 완료/명시 실패는 exact end 관측 가정 아래 검사 |
| `host-migration/HostMigration.tla:18,48` | boot mapping과 actual row/marker/native id가 같은 channel | mapping lookup만 verified parent, actual child는 독립(H1/H3) | direct child가 T/Return 선택하는 Before, UnknownNeverMeansMissing·live native continuity 유지. parent locator 치환 뮤턴트 RED |
| `input-single-path/InputOwnership.tla:2,33,799,801,804` | Claude tmux 한 channel, src=discord/imessage 사람·start 기계 | surface/verified parent와 common adapter 관측 추가(#8114 Slice2 배포 뒤 H1·T1/T2); src 분류는 기존대로 | StartOwnTurn·StartOnce·NativeHeldRuns. iMessage GUID start는 사람, generic start/routine/seed는 기계. raw metadata parent 신뢰·machine→human·parent origin key 뮤턴트 RED |
| `tui-o-writer/WriterAdoption.tla:12,28,185,220` | selection/site/runtime/execution/adoption이 한 channel, restart store seed | selectedPolicyId와 actualDestinationId, verified/unknown parent, child own adoption/store·공유 child entry의 unresolved→complete 공개·동기 kept scan→async 분류(O) | NoLegacyBodyOnTui·AnswerAtMostOnce·HeadlessKept 유지. parent O/child legacy Before RED→O 1회. parent Candidate alias·snapshot clone의 child 미공유·candidate 없음→legacy·unknown→legacy 뮤턴트 RED |
| `tui-o-writer/WriterWake.tla:32,49,220,258` | 모든 writer start/ready→원본 wake; source key=(provider,channel,source) | child 첫 등록·committed store-only 복원·parent REST 실패/회복·boot restore·execution refresh·gateway resume를 모두 열거하고 child ready wake(O) | ThreadBodyEventuallyAnswered/held original eventual retry는 child writer 활성화/ready 공정성을 명시. hold로 영구 멈춤을 가린 PASS 금지. parent key 치환·ready wake 삭제 뮤턴트 RED |

모델 변수 값의 코드 출처는 §1 모든 행과 §2 소비 좌표다. 최소한 `surface={channel,thread}`, `parentResolution={notThread,verified,unknown}`, `policyHost={T,H}`, `actualHost={T,H,unknown}`, `inputSource={discord,imessage,terminal,genericStart}`와 기존 `originClass={human,machine}`, `writerStart={gateway,execution,cleanup/resume,threadAdmission}`, `stage={cold,restoring,ready,held,restarting}`를 각 해당 기존 모델에 필요한 만큼만 넣는다. 단순히 모든 변수를 한 새 거대 모델에 넣지 않는다.

재시작·부팅·incarnation 경로는 `session_runtime.rs:505` thread bootstrap, `runtime_bootstrap/owner_execution.rs:70` boot, `:233` refresh, `o_writer_host.rs:484` gateway spawn·`:796` execution spawn·`:819` resume, `recovery_engine/restore_inflight.rs:677,722,949`, `commands/restart.rs:216`를 전이로 대응한다. 같은 상태 소비자는 stop/cancel/terminate/clear/reset/resume/idle cleanup이며 §1 행을 빠뜨리지 않는다. thread policy가 부모를 가리킨다는 이유로 이 전이의 actual owner가 부모가 되는 모델은 잘못이다.

각 구현 레인은 0단계로 해당 기존 model/cfg/view를 수정하고 **Before RED → After GREEN → 원인 제거 mutant RED**를 기록한다. invariant와 PROPERTY liveness 실행을 구분하고 코드 SHA·상태 수·violated rule·공정성 가정·실제 검사한 cfg를 결과 표에 적는다. liveness cfg에는 실제 SPECIFICATION/fairness를 적용하고 SYMMETRY를 같이 쓰지 않는다(`README.md:35-51`). known-gap cfg는 기대 결과를 보존한다. 반례는 코드 경로/격리 시험으로 단계별 replay하고 증명 안 된 반례를 제품 결함으로 보고하지 않는다.

현재 실행 표: TLC 실행 수 **0**, Rust 시험 실행 수 **0**, 모델 편집 **0**. 모델 변경이 필요한 조각은 O/H/T/R 및 상태를 바꾸는 M/S/E다. read-only D와 scanner A/B는 상태 전이를 바꾸지 않아 source contract/정적 fixture로 검증한다. D에 상태 effect를 추가하려면 다시 모델 delta 대상이다.

모델 밖 항목은 (1) tmux server atomic guard/Herdr physical writer lease의 syscall 세부—각 adapter 계약시험, (2) native source path/dev/ino·receipt 세부—#8114와 source 시험, (3) Discord 게시 수·푸터/카드·API 상태/DB 영수증—진입점 통합시험과 배포 실증이다. 부모 source 채택이나 child ready wake 누락은 범위 밖으로 빼지 않고 이번 금지 반례로 넣는다.

## §5 사용자 결정 필요·남길 차이

이번 목표를 바꾸는 새 제품 결정을 요구하지 않는다. `/restart` 종료·동일 native resume는 이미 사용자 확정이다. 다음 항목은 불가피한 차이/잔여로 명시해 코디네이터가 최종 parity 전수표에서 확인한다. 사용자 확인 없이 이슈 DoD의 ‘남은 차이 확인’을 완료했다고 표시하지 않는다.

| 항목 | 제안·사유 | 확인/담당 |
|---|---|---|
| tmux/Herdr 내부 IO | tmux pinned server guard와 Herdr per-write pin/physical writer lease는 각 host 보장 수단. 사용자 결과는 동일 | 불가피한 내부 차이, §3 계약시험 |
| 사용자 소유 Herdr consent | `!herdr consent retry/return`은 외부 사용자 서버 동의이므로 T에 대응 명령 없음 | 기존 consent 유지, #8128/#8123 표에 확인 |
| 화면 배치 | child별 managed workspace 유지. 부모 workspace 내 tab 배치는 현 launch/unplaced/pristine 가정을 바꾸므로 미채택 | §2 근거. 수명·입력·출력 결과 차이는 허용하지 않음; 같은 workspace UX가 필수면 #7583 별도 결정 |
| Claude effort/fast live pane 없음 | live H 직접 적용은 이미 있어 이 묶음 제외. H no-live 거절/T 저장 차이는 불가피하다고 확정할 근거 없음 | #8109에서 저장 후 다음 launch 적용으로 통일할지 확인. #8128 DoD 잔여이며 ‘같음’ 처리 금지 |
| 미지원 host/provider | Codex effort/ultra는 #8109 S5, process/non-TUI routine은 기존 별도 실행 의미 | 미구현과 불가피를 혼동하지 않음. R 선행 제공 |

기술 입력 잔여는 사용자 제품 결정과 다르다: #8114 Slice2 포함 배포 SHA, #8119 d2 해당 조각 착지, #8115 no-session 배포를 코디네이터가 확인해야 한다. i8114-2·i8119-d2 ref는 위 SHA로 직접 읽어 누락을 해소했다. 이 레인은 운영 설정·DB·store·dcserver·tmux·herdr를 확인하지 않았으므로 배포 여부를 추정하지 않는다.

## 부록 A — 네 패턴 원시 전수 검색

다음은 조사 SHA에서 실행한 `git grep -n -E 'herdr_endpoint\(|herdr_configured_for_tui_launch|admin_host_guard|executor_not_wired' -- '*.rs'` 전체다. 정의·import·inline/file 시험·문자열까지 보존한 누락 점검용으로, A production baseline이 아니다. 원시 로그는 레인 `logs/host-call-census.txt`에 보존한다. 전체 tracked 검색에서는 금지 설정 파일을 제외했고 `scripts/deploy-release.sh:4514`의 headless N/A 및 `tests/test_deploy_smoke_turn_scenarios_6561.sh:82` fixture도 포함됐다.

```text
src/cli/utils.rs:176:        let refusal = crate::services::discord::admin_host_guard::marker_refusal;
src/config/session_hosts.rs:158:    pub(crate) fn herdr_endpoint(&self, channel: u64) -> Option<&ChannelEndpoint> {
src/config/session_hosts.rs:349:pub(crate) fn herdr_endpoint(channel: u64) -> Option<ChannelEndpoint> {
src/config/session_hosts.rs:350:    with_boot(|boot| boot.and_then(|boot| boot.herdr_endpoint(channel)).cloned())
src/config/session_hosts/migration_tests.rs:645:        assert!(selected.herdr_endpoint(CHANNEL).is_some());
src/config/session_hosts_tests.rs:48:    let bound = boot.herdr_endpoint(41).unwrap();
src/config/session_hosts_tests.rs:54:    assert!(boot.herdr_endpoint(43).is_none());
src/server/routes/agents.rs:186:    let diag = crate::services::discord::admin_host_guard::row_unsupported;
src/server/routes/agents_crud.rs:1037:                crate::services::discord::admin_host_guard::row_is_legacy_tmux(
src/server/routes/provider_auth_profiles.rs:511:    let refusal = crate::services::discord::admin_host_guard::auth_login_refusal;
src/services/agents/turn.rs:309:    let refusal = crate::services::discord::admin_host_guard::row_refusal;
src/services/auto_queue/runtime.rs:10:use crate::services::discord::admin_host_guard::{ManagedResetVerdict, managed_reset_verdict};
src/services/auto_queue/runtime.rs:239:        crate::services::discord::admin_host_guard::run_before_verified_clear_recheck_for_tests(
src/services/auto_queue/runtime.rs:485:        let configured = crate::services::discord::admin_host_guard::configured_refusal;
src/services/auto_queue/runtime.rs:582:        Option<crate::services::discord::admin_host_guard::ManagedReset>,
src/services/auto_queue/runtime/slot_reset_host_pg_tests.rs:3:use crate::services::discord::admin_host_guard::ManagedReset;
src/services/auto_queue/runtime/slot_reset_host_pg_tests.rs:40:    use crate::services::discord::admin_host_guard::ManagedReset;
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:4:use crate::services::discord::admin_host_guard::{self, ManagedReset};
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:88:        admin_host_guard::verified_reset_tests::seed_reset_race_session(
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:133:        admin_host_guard::verified_reset_tests::seed_accepted_queue(&shared, self.channel).await;
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:136:                admin_host_guard::verified_reset_tests::pending_dispatch_bytes(
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:158:        admin_host_guard::before_verified_clear_recheck_for_tests(channel, move || {
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:159:            admin_host_guard::verified_reset_tests::pin_verified_execution(&name, channel, &root);
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:178:            admin_host_guard::verified_reset_tests::accepted_queue_len(shared, self.channel).await,
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:183:            admin_host_guard::verified_reset_tests::pending_dispatch_bytes(shared, self.channel),
src/services/auto_queue/runtime/verified_reset_pg_tests.rs:204:        admin_host_guard::verified_reset_tests::assert_reset_race_session(
src/services/claude.rs:2607:    if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) {
src/services/claude/tui_session_launch.rs:36:    use crate::services::herdr_launch::{HERDR_NOT_ADMITTED, herdr_configured_for_tui_launch};
src/services/claude/tui_session_launch.rs:38:    if herdr_configured_for_tui_launch(channel_id) {
src/services/claude/tui_session_launch.rs:53:    if herdr_configured_for_tui_launch(channel_id) {
src/services/codex.rs:1426:    if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) {
src/services/codex.rs:1527:    if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) {
src/services/codex/absence_launch.rs:78:    if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) {
src/services/codex/absence_launch.rs:294:    if crate::services::herdr_launch::herdr_configured_for_tui_launch(report_channel_id) {
src/services/codex/tui_session_launch/first_input.rs:214:    use crate::services::herdr_launch::{HERDR_NOT_ADMITTED, herdr_configured_for_tui_launch};
src/services/codex/tui_session_launch/first_input.rs:216:    if herdr_configured_for_tui_launch(report_channel_id) {
src/services/codex/tui_session_launch/launch_policy.rs:70:    if crate::services::herdr_launch::herdr_configured_for_tui_launch(channel) {
src/services/discord/admin_host_guard.rs:115:    let endpoint = crate::config::session_hosts::herdr_endpoint(channel_id)?;
src/services/discord/admin_host_guard.rs:408:#[path = "admin_host_guard_tests.rs"]
src/services/discord/admin_host_guard.rs:413:#[path = "admin_host_guard_verified_reset_tests.rs"]
src/services/discord/admin_host_guard_tests.rs:13:use crate::services::discord::admin_host_guard::ManagedReset;
src/services/discord/admin_host_guard_tests.rs:306:    let test = "services::discord::admin_host_guard::tests::a_refused_dispatch_reset_stops_before_the_turn_pg";
src/services/discord/admin_host_guard_tests.rs:617:    let test = "services::discord::admin_host_guard::tests::intake_refuses_a_configured_turn_before_its_runtime_recreate_pg";
src/services/discord/admin_host_guard_tests.rs:640:        (ready, Some("executor_not_wired")),
src/services/discord/admin_host_guard_tests.rs:692:    let test = "services::discord::admin_host_guard::tests::a_configured_channel_refuses_a_dispatch_reset_before_any_change_pg";
src/services/discord/admin_host_guard_tests.rs:764:    let test = "services::discord::admin_host_guard::tests::headless_refuses_a_configured_turn_before_its_runtime_recreate_pg";
src/services/discord/admin_host_guard_tests.rs:960:    let test = "services::discord::admin_host_guard::tests::a_dispatch_reusing_a_configured_thread_is_refused_before_its_bootstrap_pg";
src/services/discord/admin_host_guard_tests.rs:1040:    let test = "services::discord::admin_host_guard::tests::an_unmanaged_runtime_clear_of_a_configured_channel_resets_it_without_tmux_pg";
src/services/discord/codex_absence.rs:30:        && !crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(channel))
src/services/discord/commands/config/provider_settings.rs:117:        if let Some(endpoint) = crate::config::session_hosts::herdr_endpoint(channel_id.get()) {
src/services/discord/commands/config/provider_settings.rs:190:        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
src/services/discord/commands/config/provider_settings.rs:326:        && crate::config::session_hosts::herdr_endpoint(runtime_channel_id.get()).is_some()
src/services/discord/commands/config/provider_settings/revision_tests.rs:165:                    Some(crate::services::discord::admin_host_guard::ManagedReset::Applied(None))
src/services/discord/commands/control.rs:9:use super::super::admin_host_guard::ManagedReset;
src/services/discord/commands/control.rs:466:    crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some()
src/services/discord/commands/control/clear_end_tests.rs:241:    assert!(crate::config::session_hosts::herdr_endpoint(channel.get()).is_some());
src/services/discord/commands/control/clear_entry_tests.rs:14:use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/commands/control/clear_teardown.rs:70:    if crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some()
src/services/discord/commands/control/clear_teardown.rs:81:            && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
src/services/discord/commands/control/clear_teardown.rs:115:        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
src/services/discord/commands/control/clear_teardown/clear_end_record.rs:45:    let herdr = crate::config::session_hosts::herdr_endpoint(channel).is_some();
src/services/discord/commands/control/clear_teardown/codex_end_record.rs:15:        || crate::config::session_hosts::herdr_endpoint(channel).is_some()
src/services/discord/commands/control/codex_clear_relaunch_tests.rs:480:        api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/commands/control/codex_clear_relaunch_tests.rs:666:    if !crate::services::discord::admin_host_guard::tests::api_child(qualified) {
src/services/discord/commands/control/codex_clear_relaunch_tests.rs:776:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests.rs:37:        && crate::services::discord::admin_host_guard::tests::api_child(
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests.rs:220:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/auxiliary_tests.rs:145:) -> crate::services::discord::admin_host_guard::tests::Recorder {
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/auxiliary_tests.rs:146:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/auxiliary_tests.rs:174:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/auxiliary_tests.rs:948:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/auxiliary_tests/joint_owner_tests.rs:312:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/commands/control/codex_clear_relaunch_tests/absent_launch_tests/custody_tests.rs:201:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/commands/control/codex_clear_retirement_tests.rs:387:            assert!(crate::config::session_hosts::herdr_endpoint(CANARY_CHANNEL).is_some());
src/services/discord/commands/control/codex_clear_retirement_tests.rs:424:) -> crate::services::discord::admin_host_guard::tests::Recorder {
src/services/discord/commands/control/codex_clear_retirement_tests.rs:425:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/commands/control/codex_clear_retirement_tests.rs:447:        && crate::services::discord::admin_host_guard::tests::api_child(
src/services/discord/commands/control/herdr_recovery.rs:28:    if crate::config::session_hosts::herdr_endpoint(channel.get()).is_none() {
src/services/discord/commands/control/model_catalog_observation_tests.rs:280:        let discord = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/commands/control/provider_reset.rs:16:    let refusal = crate::services::discord::admin_host_guard::managed_reset_refusal;
src/services/discord/commands/control/relaunch_tests.rs:15:use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
src/services/discord/commands/diagnostics/reports.rs:46:    let refusal = super::super::super::admin_host_guard::channel_refusal;
src/services/discord/commands/fast_mode.rs:108:        && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some()
src/services/discord/commands/restart.rs:5:use crate::services::discord::admin_host_guard::ManagedReset;
src/services/discord/commands/restart.rs:218:        let refusal = super::super::admin_host_guard::managed_reset_refusal;
src/services/discord/commands/restart.rs:363:    use crate::services::discord::admin_host_guard::tests::process;
src/services/discord/commands/skill/stop_response_tests.rs:3:use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/commands/text_commands/clear_http_overlap_tests.rs:14:use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/commands/text_commands/clear_refusal_reply_tests.rs:14:use crate::services::discord::admin_host_guard::tests::{Recorder, process};
src/services/discord/commands/tmux_recreate.rs:91:    let refusal = crate::services::discord::admin_host_guard::managed_reset_refusal;
src/services/discord/commands/tui_passthrough.rs:128:    if crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some() {
src/services/discord/commands/tui_passthrough.rs:176:    let refusal = super::super::admin_host_guard::channel_refusal;
src/services/discord/formatting/delivery.rs:398:        use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/health/recovery.rs:7:use crate::services::discord::admin_host_guard::ManagedReset;
src/services/discord/health/recovery.rs:267:    let refusal = discord::admin_host_guard::managed_reset_refusal;
src/services/discord/health/recovery.rs:273:    discord::admin_host_guard::run_before_verified_clear_recheck_for_tests(channel_id.get());
src/services/discord/health/turn_deliver/inject.rs:312:        && crate::config::session_hosts::herdr_endpoint(channel).is_none()
src/services/discord/health/turn_deliver/inject/herdr.rs:48:        || crate::config::session_hosts::herdr_endpoint(channel).is_none()
src/services/discord/herdr_terminate/http_force_writer_tests.rs:129:        let configured = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/herdr_terminate/missing_marker_turn_tests.rs:113:                crate::services::discord::admin_host_guard::managed_reset_refusal(
src/services/discord/herdr_terminate/root_replaced.rs:249:        crate::config::session_hosts::herdr_endpoint(channel).ok_or("endpoint unavailable")?;
src/services/discord/herdr_terminate/root_replaced/preclaim.rs:22:    let Some(endpoint) = crate::config::session_hosts::herdr_endpoint(channel.get()) else {
src/services/discord/inflight/host_recovery_guard.rs:103:    if crate::services::discord::admin_host_guard::configured_refusal(channel_id).is_some() {
src/services/discord/mod.rs:3:pub(crate) mod admin_host_guard;
src/services/discord/monitoring_status_inline_tests.rs:20:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/queue_dispatch.rs:253:            && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
src/services/discord/queue_dispatch/canary_workspace_tests.rs:56:        || !crate::services::discord::admin_host_guard::tests::api_child(
src/services/discord/queue_dispatch/canary_workspace_tests.rs:63:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/queue_io/input_owner/claude_handoff.rs:219:            || crate::config::session_hosts::herdr_endpoint(channel.get()).is_none())
src/services/discord/relay_recovery/apply.rs:100:            let refusal = super::super::admin_host_guard::turn_release_refusal;
src/services/discord/relay_recovery/tests/host_deferred.rs:17:use crate::services::discord::admin_host_guard::tests::{Host, ModeTmux, Tmux};
src/services/discord/router/herdr_consent.rs:34:        let mapped = boot.herdr_endpoint(channel)?;
src/services/discord/router/herdr_consent/return_input.rs:14:    let Some(mapped) = session_hosts::herdr_endpoint(channel.get()) else {
src/services/discord/router/intake_dispatch/queued/transition_busy_tests.rs:1104:    if !discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/intake_dispatch/tests/owner_ingress_tests.rs:527:    assert!(crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(channel.get())));
src/services/discord/router/intake_gate/busy_inject.rs:86:    let herdr = crate::services::herdr_launch::herdr_configured_for_tui_launch;
src/services/discord/router/intake_gate/live_entry_tests.rs:429:    if !crate::services::discord::admin_host_guard::tests::api_child(&qualified) {
src/services/discord/router/intake_gate/stale_turn.rs:191:    let refusal = crate::services::discord::admin_host_guard::turn_release_refusal;
src/services/discord/router/intake_gate/stale_turn_host_tests.rs:9:use crate::services::discord::admin_host_guard::tests::{Host, ModeTmux, Tmux};
src/services/discord/router/message_handler/attachments.rs:261:        use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/router/message_handler/goal_lifecycle.rs:282:    use crate::services::discord::admin_host_guard::tests::{Recorder, api_child, process};
src/services/discord/router/message_handler/headless_turn/replay_start_tests.rs:4:use crate::services::discord::admin_host_guard::tests::{api_child, intake_replay_rt};
src/services/discord/router/message_handler/intake_turn.rs:883:                let restore = super::super::super::admin_host_guard::return_intake_input;
src/services/discord/router/message_handler/intake_turn.rs:1110:            && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none()
src/services/discord/router/message_handler/intake_turn.rs:1754:                    && crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_none())
src/services/discord/router/message_handler/intake_turn/first_key_gate_paths_tests.rs:82:    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/host_refusal.rs:37:        let restore = super::super::super::super::admin_host_guard::return_intake_input;
src/services/discord/router/message_handler/intake_turn/host_refusal.rs:60:    let restore = super::super::super::super::admin_host_guard::return_intake_input;
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:9:use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:522:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:678:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:818:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1398:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1443:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1485:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1540:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1578:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1628:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1666:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:1725:        crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(Box::pin(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:2069:    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:2143:    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:2254:    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/replay_start_tests.rs:2341:    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/route_claim_tests.rs:17:    crate::services::discord::admin_host_guard::tests::api_child(test)
src/services/discord/router/message_handler/intake_turn/route_claim_tests.rs:161:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/router/message_handler/intake_turn/start_placeholder_tests.rs:6:use crate::services::discord::admin_host_guard::tests::api_child;
src/services/discord/router/message_handler/intake_turn/system_prompt_tests.rs:29:    crate::services::discord::admin_host_guard::tests::intake_replay_rt().block_on(async {
src/services/discord/router/message_handler/intake_turn/worker_entry/dispatch_guard_pg_tests.rs:6:use crate::services::discord::admin_host_guard::tests::api_child;
src/services/discord/router/message_handler/intake_turn/worker_entry/goal_fresh_pg_tests.rs:21:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
src/services/discord/router/message_handler/intake_turn/worker_entry/preserve_cancel_tests.rs:5:use crate::services::discord::admin_host_guard::tests::api_child;
src/services/discord/router/message_handler/provider_dispatch.rs:1074:            "herdr turn refused: executor_not_wired"
src/services/discord/router/message_handler/provider_dispatch/owner_input_codex_tests.rs:629:    crate::services::discord::admin_host_guard::tests::api_child(test)
src/services/discord/router/message_handler/provider_dispatch/owner_input_rollback_tests.rs:54:    _api: discord::admin_host_guard::tests::Recorder,
src/services/discord/router/message_handler/provider_dispatch/owner_input_rollback_tests.rs:183:        let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:132:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/provider_dispatch/owner_input_turn_mode_tests.rs:478:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/provider_dispatch/owner_provenance_tests.rs:5:use crate::services::discord::admin_host_guard::tests::{ModeTmux, Tmux, api_child};
src/services/discord/router/message_handler/provider_dispatch/owner_start_failure_tests.rs:84:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/provider_dispatch_herdr_tests.rs:1366:    let refused = Err("herdr turn refused: executor_not_wired".to_string());
src/services/discord/router/message_handler/provider_dispatch_live_end_tests.rs:201:                discord::admin_host_guard::turn_release_refusal(shared, provider, channel);
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:55:    api: &'a crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:223:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:375:    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:386:    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:397:    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:408:    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:558:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/provider_dispatch_submission_tests.rs:816:    crate::services::discord::admin_host_guard::tests::api_child(test)
src/services/discord/router/message_handler/root_replaced_recovery_tests.rs:162:        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests.rs:201:        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests.rs:216:        let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests.rs:270:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests.rs:530:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/clear_tests.rs:21:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/legacy_clear_tests.rs:8:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL);
src/services/discord/router/message_handler/root_replaced_recovery_tests/legacy_clear_tests.rs:61:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/legacy_clear_tests.rs:126:    assert!(crate::config::session_hosts::herdr_endpoint(CHANNEL).is_none());
src/services/discord/router/message_handler/root_replaced_recovery_tests/policy_tests.rs:97:        let configured = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/preclaim_tests.rs:105:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/preclaim_tests.rs:963:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/recheck_tests.rs:154:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/root_replaced_recovery_tests/wake_tests.rs:260:    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
src/services/discord/router/message_handler/s1_entry_tests.rs:206:    if !discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/s1_entry_tests.rs:222:    let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/s1_entry_tests.rs:299:    if !discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/s1_entry_tests.rs:315:    let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/s1_entry_tests.rs:424:    if !discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/s1_entry_tests.rs:449:    let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/s1_entry_tests.rs:740:    if !discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/s1_entry_tests.rs:763:    let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/s1_entry_tests.rs:1060:    if rig::native_server_child() || !discord::admin_host_guard::tests::api_child(TEST) {
src/services/discord/router/message_handler/s1_entry_tests.rs:1498:    if !discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/s1_entry_tests.rs:1514:    let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/tui_followup/nameless_gate.rs:79:    if crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some() {
src/services/discord/router/message_handler/turn_mark_admission_tests.rs:48:    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/router/message_handler/turn_mark_admission_tests.rs:143:    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/router/message_handler/turn_mark_admission_tests.rs:154:    if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_fresh_launch_tests.rs:640:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_repeated_absence_tests.rs:50:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/runtime_bootstrap/o_writer_host/fence_tests/b1b_repeated_absence_tests.rs:156:    api: &crate::services::discord::admin_host_guard::tests::Recorder,
src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/missing_halt_tests.rs:300:        let endpoint = crate::config::session_hosts::herdr_endpoint(channel).unwrap();
src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/missing_halt_tests.rs:662:        let endpoint = crate::config::session_hosts::herdr_endpoint(channel).unwrap();
src/services/discord/runtime_bootstrap/o_writer_host/tests/owner_execution/missing_halt_tests.rs:710:        let endpoint = crate::config::session_hosts::herdr_endpoint(channel).unwrap();
src/services/discord/session_idle_cleanup.rs:75:        let configured = super::admin_host_guard::configured_refusal;
src/services/discord/session_runtime.rs:513:    if crate::config::session_hosts::herdr_endpoint(thread_channel_id.get()).is_some() {
src/services/discord/tmux_watcher/codex_replacement_tests.rs:13:use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/tmux_watcher/host_gate.rs:153:    let configured = crate::services::discord::admin_host_guard::configured_refusal;
src/services/discord/tui_prompt_relay.rs:757:            if crate::config::session_hosts::herdr_endpoint(channel).is_some() =>
src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests.rs:128:        crate::config::session_hosts::herdr_endpoint(super::CHANNEL_ID).is_some(),
src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests.rs:915:    let endpoint = crate::config::session_hosts::herdr_endpoint(super::CHANNEL_ID).unwrap();
src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests.rs:972:    let endpoint = crate::config::session_hosts::herdr_endpoint(super::CHANNEL_ID).unwrap();
src/services/discord/tui_prompt_relay/relay_e2e/thread_guard_host_e2e.rs:11:use crate::services::discord::admin_host_guard::tests::{Host, ModeTmux, Tmux};
src/services/discord/tui_prompt_relay/synthetic_start/admission_fence.rs:168:        let endpoint = crate::config::session_hosts::herdr_endpoint(self.channel.get());
src/services/discord/tui_prompt_relay/synthetic_start/claim/admitted/transition_tests.rs:99:    assert!(crate::config::session_hosts::herdr_endpoint(strict_channel).is_some());
src/services/discord/tui_prompt_relay/synthetic_start/claim/admitted/transition_tests.rs:259:    assert!(crate::config::session_hosts::herdr_endpoint(channel).is_some());
src/services/discord/tui_prompt_relay/synthetic_start/claim/admitted/transition_tests.rs:480:        assert!(crate::config::session_hosts::herdr_endpoint(channel.get()).is_none());
src/services/discord/tui_prompt_relay/tests.rs:6797:    use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/tui_prompt_relay/tests.rs:6885:    use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/tui_prompt_relay/tests/synthetic_bridge_handoff_pg_tests/herdr_synthetic_identity_pg_tests.rs:56:        let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get()).unwrap();
src/services/discord/tui_prompt_relay/tests/synthetic_bridge_handoff_pg_tests/herdr_synthetic_identity_pg_tests.rs:878:    assert!(crate::config::session_hosts::herdr_endpoint(SOURCE_CHANNEL).is_none());
src/services/discord/tui_prompt_relay/writer_notice_owner_tests.rs:44:    use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/turn_bridge/before_start_stop_tests.rs:1308:        crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(answer))
src/services/discord/turn_bridge/runtime_handoff_loop/codex_replacement_tests.rs:2:use crate::services::discord::admin_host_guard::tests::Recorder;
src/services/discord/turn_bridge/terminal_outcome_delivery/delivery_epilogue_tests/recovery_retry_guard_tests/replay_turn_tests.rs:697:    use crate::services::discord::admin_host_guard::tests::{Recorder, api_child};
src/services/discord/turn_bridge/tmux_runtime/stop_host_tests.rs:924:    let alive = crate::services::discord::admin_host_guard::tests::process(&name, pid);
src/services/discord/turn_bridge/tmux_runtime/stop_host_tests.rs:948:    use crate::services::discord::admin_host_guard::ManagedReset;
src/services/discord/turn_bridge/tmux_runtime/stop_host_tests.rs:1058:            let alive = crate::services::discord::admin_host_guard::tests::process(&name, 66_000);
src/services/discord/turn_presence/activity/unbound.rs:305:    if let Some(endpoint) = crate::config::session_hosts::herdr_endpoint(reads.channel) {
src/services/discord/turn_presence/activity/unbound.rs:525:        && crate::config::session_hosts::herdr_endpoint(channel).is_none()
src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:162:                    || crate::config::session_hosts::herdr_endpoint(channel).is_some()
src/services/discord/turn_presence/activity/unbound/herdr_bound.rs:18:    let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get())?;
src/services/discord/turn_presence/activity/unbound/herdr_bound_tests.rs:62:    let endpoint = crate::config::session_hosts::herdr_endpoint(rig.channel.get()).unwrap();
src/services/discord/turn_presence/activity/unbound/herdr_chain_tests.rs:25:        let at = crate::config::session_hosts::herdr_endpoint(id).unwrap();
src/services/discord/turn_presence/activity/unbound/legacy_divergence_entry_tests.rs:168:    let api = discord::admin_host_guard::tests::Recorder::start().await;
src/services/discord/turn_presence/activity/unbound/live_review_tests.rs:52:        let endpoint = boot.herdr_endpoint(self.channel.get()).unwrap();
src/services/discord/turn_presence/activity/unbound/retired_codex_runtime_marker_tests.rs:53:        let endpoint = crate::config::session_hosts::herdr_endpoint(id).unwrap();
src/services/discord/turn_presence/activity/unbound/retired_codex_runtime_marker_tests.rs:145:    let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get()).unwrap();
src/services/dispatched_sessions/output.rs:171:    let refusal = crate::services::discord::admin_host_guard::session_key_refusal;
src/services/herdr_launch.rs:56:pub(crate) fn herdr_configured_for_tui_launch(channel_id: Option<u64>) -> bool {
src/services/herdr_launch.rs:62:    crate::config::session_hosts::herdr_endpoint(channel).is_some()
src/services/provider/cancel_token_cleanup/executor.rs:162:        let marker = crate::services::discord::admin_host_guard::marker_refusal;
src/services/provider_cli/session_guard.rs:199:        let refusal = crate::services::discord::admin_host_guard::marker_refusal;
src/services/routines/session_control.rs:8:use crate::services::discord::admin_host_guard::ManagedReset;
src/services/session_activity.rs:49:            let refusal = crate::services::discord::admin_host_guard::marker_refusal;
src/services/session_resume.rs:404:    let configured_refusal = crate::services::discord::admin_host_guard::configured_refusal;
src/services/tui_o/writer/actor.rs:423:        let herdr = crate::config::session_hosts::herdr_endpoint(channel).is_some();
src/services/tui_o/writer/fresh_scope_negative_tests.rs:138:                crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(
src/services/tui_o/writer/input_notices/admission.rs:104:        match crate::config::session_hosts::herdr_endpoint(store.init().channel) {
src/services/tui_prompt_dedupe/hosted_source.rs:64:        crate::config::session_hosts::herdr_endpoint(channel).is_some()
src/services/tui_prompt_dedupe/runtime_binding/codex_policy/absent_open.rs:67:        && !crate::services::herdr_launch::herdr_configured_for_tui_launch(Some(CANARY_CHANNEL))
src/services/turn_host.rs:78:            Self::ExecutorNotWired => f.write_str("executor_not_wired"),
src/services/turn_host.rs:157:    let Some(endpoint) = session_hosts::herdr_endpoint(channel_id) else {
src/services/turn_host.rs:186:    session_hosts::herdr_endpoint(channel_id)?;
```
