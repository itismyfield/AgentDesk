# 세션 호스트·표면·진입점 통합 설계 — #8118·#8125·#8128·#8122

조사 기준은 main `4e0db0cccd59169a09f25fe162d8cc1b2755e96e`, 작성·r2 개정일은 2026-11-12 KST다. 코드 좌표는 별도 표기가 없으면 이 SHA의 좌표다. 코드·이슈를 읽은 설계이며 운영 상태 재측정, Rust 변경, cargo/build/test/clippy, TLC 실행은 하지 않았다. 이슈에 적힌 실측은 이슈 작성자의 관측이지 이 레인의 실행 증거가 아니다.

정본 입력은 [#8118](https://github.com/itismyfield/AgentDesk/issues/8118), [#8125](https://github.com/itismyfield/AgentDesk/issues/8125), [#8128](https://github.com/itismyfield/AgentDesk/issues/8128), [#8122](https://github.com/itismyfield/AgentDesk/issues/8122)의 본문·전체 댓글이다. #8118은 스레드 출력 경로까지 포함하며 legacy 전송 296건은 감소 비교의 과거 관측값이다. 새 전송 0의 판정은 배포 후 창에서 한다. #8128의 `/restart` 및 명령 검토 댓글(11-12 16:44·16:48 KST)을 수용했다.

선행 입력 `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8114/design-boundary-delta-1.md`를 읽고, 조사 중 게시된 **`refs/review/i8114-2=d8e1cabbb27b37a73092d41bd46c5c94ce72e785` 코드와 `refs/review/i8119-d2=01053c2be0312bad4d9634f2df46b3141301f279`의 [명명 설계](https://github.com/itismyfield/AgentDesk/blob/01053c2be0312bad4d9634f2df46b3141301f279/docs/session-naming.design.md)를 fetch해 대조했다.** #8114-2는 Slice1의 [BusyOperation L60–119](https://github.com/itismyfield/AgentDesk/blob/d8e1cabbb27b37a73092d41bd46c5c94ce72e785/src/services/claude_tui/host_input/busy.rs#L60-L119)과 [공통 BusyClaude L9–52](https://github.com/itismyfield/AgentDesk/blob/d8e1cabbb27b37a73092d41bd46c5c94ce72e785/src/services/discord/queue_io/input_owner/effect/claude.rs#L9-L52)를 연결하지만, typed source/observer/reobserve·early/late 승인은 Slice2에 남긴다. **이 묶음의 “#8114 배포 뒤”는 Slice1 ref 존재만이 아니라 Slice2를 포함한 사용자 계약의 배포 완료 SHA 확인**이다. #8119 d2의 S6 BindingContext 부모 필드·S8b1/S8b2 순서를 따른다. d1의 effective descriptor CAS 저장기계는 채택하지 않는다. 이 레인은 ref를 읽었으며 운영 배포 여부는 확인하지 않았다.

## 변경 이력

**r3: O-b b3 site 전수 결과 반영(2026-11-14).** 부팅 때 기록 없는 옛 child는 Held가 아니라 late 예약이고, 부모 확정은 다음 입력 placement의 checked 조회가 맡는다(§2-4~6). 옛 refresh 해소 단계는 없앤다. 롤백 가드는 서비스 중단 뒤 swap 전에 한 번 더 판정한다(§첫 child init과 롤백 하한). O-b 활성화는 b3a(무효과 기반)·b3b(활성화)로 나눈다(§2 끝).

**r2: 1라운드 P1-1 롤백 가드·policy_parent 기록·P2 1~9 반영.** 부모 관계는 생산 경계에서 검증해 기존 init/BindingContext에 기록하고 기록을 우선 소비한다. O는 O-a 준비/O-b 완전 활성화로 나눴으며 호환 capability는 O-b에서만 true다. r2가 마지막 2라운드 설계 리뷰 입력이고, 이 문서는 구현·실물 검증 완료를 뜻하지 않는다.

## §1 진입점 전수표

표의 T/H는 **부모 채널의 설정**이 tmux/herdr임을 뜻한다. 현재 H 스레드의 실제 실행이 tmux일 수 있으므로 ‘herdr 스레드’가 이미 herdr에 있다는 뜻은 아니다. O는 tui_o다. 채널·스레드의 실제 ID, native session, row/key/nonce, source, 원본·receipt·출력 목적지는 별개다. `D=` 접두는 `src/services/discord/`, `S=`는 `src/services/`, `R=`는 `src/server/routes/`다. 행 안의 공통 좌표는 네 표면 모두에 적용한다. 코드만으로 확인한 행을 실물 GREEN으로 표현하지 않는다.

| 진입점 | tmux 채널 현재 | herdr 채널 현재 | tmux 스레드 현재 | herdr 스레드 현재 | 목표 동작 | 담당 조각 | 불가피한 차이·보존할 사유 |
|---|---|---|---|---|---|---|---|
| Discord 새 메시지·새 세션 | intake→provider launch | intake용 host admission 뒤 Herdr 실행 | 부모 path bootstrap 후 tmux | child ID exact lookup으로 tmux 선택 | 같은 intake, 새 child는 부모 host 정책, 실제 child target 사용 | #8118 H1, #8115 | host IO·admission 수단만 다름. `D=session_runtime.rs:505,513`, `S=herdr_launch.rs:56`, `S=turn_host.rs:151` |
| 일하는 중 사람 입력 | owner가 busy paste, source 불명은 Held | 기존에는 일부 defer; #8114가 공통 operation으로 교체 예정 | 새 owner 경로 사용. 이슈 실측 source-not-bound 실패도 있음 | launch는 tmux인데 busy gate는 부모 H로 배제하는 불일치 | #8114 operation을 동일 소비; child의 source/history를 먼저 bind | #8114 뒤 #8118 O/H | tmux server guard, Herdr exact pin/physical writer lease 보존. `D=router/intake_gate/busy_inject.rs:82`, `D=router/message_handler/intake_turn.rs:1110,1754`(전체 경로는 부록) |
| 터미널에서 시작된 턴 합류 | Claude live transcript→owner busy | endpoint 존재 시 제외 | child ID 기준으로 tmux 합류 | 설정과 actual host가 어긋남 | 정책 host가 아닌 actual operation 관측으로 같은 사람 입력 합류 | #8114, #8118 H1 | native unknown은 key0 Held, post-key 불명은 no-resend. `D=router/message_handler/intake_turn.rs:1751` |
| 출력 본문·placeholder·도구 카드·푸터 | O 선택 채널은 O writer | O-ready가 Herdr launch 선행 | boot selection에 child가 없어 legacy로 빠짐 | 동일; parent O여도 child O 0이라는 이슈 관측 | 부모 O selection·kind·site를 상속, child별 writer/store/adoption/usage | **#8118 O-a→O-b 첫 출력 묶음**, #7651 자체 관문 별도 | legacy 증상 개별 수리 금지. `S=tui_o/channel_policy.rs:74,346`, `S=tui_o/cutover/channel_gate.rs:433`, `D=runtime_bootstrap/owner_execution.rs:79` |
| API target 공통 | agent bound 채널 허용 | 동일 target 허용 | exact bound 아니면 403 | 동일 403 | 검증된 agent parent 아래 actual thread도 허용; provider는 parent | #8125 T2 | 확정된 무관 대상403, parent 관계 미확정503·retryable. `R=agents_turn_target.rs:65,107` |
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
| selector clear SQL | configured T는 기존 clear | configured H 부모 행은 목록으로 보호 | child ID가 부모 목록에 없어 보호 누락 | H 부모 child의 selector clear 가능 | child의 검증된 policy parent가 configured인지 공통 SQL/bind에서 판정 | #8118 H2 | `src/config/session_hosts.rs:354`, `src/db/dispatched_sessions.rs:2212,2249`; H child clear0, unknown도 no-clear |
| thread_gc DB 행·child store 은퇴 | occupancy+tmux owner/probe+조건부 delete | Bound/unknown은 hosted SQL 보호 | thread 자신의 exact row만 delete | 현재 tmux ProbeFailed/hosted predicate로 보호 | 같은 GC 성공 증거로 child store 은퇴 요청; O 부채 정산·writer 종료 뒤 논리 은퇴, 물리 삭제0 | O-a 계약/O-b lifecycle, H2/H3 host 적응 | `S=dispatched_sessions/hosted_cleanup/thread_gc.rs:13,54,63`, `src/db/dispatched_sessions/thread_gc.rs:80`, `migrations/postgres/0131_sessions_hosted_execution.sql:7` |
| 배포·외부 artifact·자동 롤백 | 기존 writer/ledger/spool 가드 | 동일 | 옛 바이너리는 child init을 등록 오류로 봄 | 동일 전체 boot 중단 위험 | child 첫 init 전 floor, 대상 binary capability가 true일 때만 downgrade/교체 허용 | #8118 O-a 가드/O-b capability·floor 활성 | `src/cli/build_info.rs:7`, `scripts/deploy-release.sh:1016,1050,1067,1240,2771`; floor 뒤 O-a/old target 거절 |
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

Discord의 실제 Public/Private/NewsThread만 parent로 인정한다. category parent는 상속 근거가 아니다(`session_runtime/channel_routing.rs:212-217`). **첫 Discord intake·dispatch·routine TUI thread 생성/재사용·terminal binding·API target의 생산 경계에서 한 번 검증해 기록**하고 이후는 그 기록을 우선한다. 신뢰하는 것은 서버가 기록한 exact child/provider/owner scope의 parent 관계다. raw HTTP metadata·클라이언트 context JSON/경로·표시 이름·손상/상충 기록은 증거가 아니다. 현재 agent가 그 parent에 바인딩되어 있는지는 요청마다 기존 agent binding으로 별도 확인한다. 기록이 없는 옛 child에만 `checked_thread_parent:296-303`의 Err를 보존하는 REST를 쓰며, best-effort `resolve_thread_parent:276`의 None을 권한·host fallback으로 쓰지 않는다. 이미 확인된 parent는 같은 요청의 target에 전달해 안쪽에서 다시 검증하지 않는다.

### 부모 기록·공유 child view·부팅과 refresh

아래 child Held/body 차단·adoption·ready·은퇴의 production 효과는 **O-b 활성 이후 계약**이다. O-a는 schema/reader/shared view·가드 준비와 격리 fixture까지만 연결하고 기존 출력 membership/body 동작을 유지한다. O-a에 없는 child writer를 ready로 표시하지 않는다.

1. 부팅 host map(`session_hosts.rs:114,158,333`)과 O selected/kind/site(`tui_o/channel_policy.rs:28,74,231,628`)는 그대로 정본이다. thread ID를 운영 설정에 추가하지 않고 `all_tui`는 부모 TUI binding 선택을 상속한다. 관계 기록은 부모 ID만 보존하며 현재 host·O selection을 영구 복제하지 않는다. 매 process의 같은 boot snapshot으로 정책을 계산한다.
2. O-a에서 기존 `tui_o/store/mod.rs:60 Initialized`에 **`#[serde(default)] policy_parent: Option<u64>`**를 더한다. None 직렬화 생략으로 기존 일반 채널 init 바이트를 보존한다. 기존 구조체는 deny_unknown_fields가 없고 old JSON은 None으로 읽힌다. child 첫 init의 parent는 검증된 `VerifiedSurface` 생산자만 채울 수 있다. raw u64 인자/HTTP parent를 저장하는 공개 우회는 두지 않는다. child/parent/provider 또는 기록 간 충돌은 Held다. #8119 S6 이후 서버가 만든 immutable BindingContext의 parent/thread가 1차 관계 증거이며 init.policy_parent와 대조한다. S6 필드를 이 레인에서 중복 신설하지 않는다. S6 이전·context 없는 store-only 복원은 init.policy_parent로 해소한다. 기존 init의 다른 필드·source·cursor를 재작성하지 않는다.
3. BOOT 내부의 **공유 파생 child-entry map**을 모든 snapshot clone이 본다. 현 channels/kinds/candidates는 값 clone(`channel_policy.rs:28-45`, `cutover.rs:24`)이므로 값 map에만 insert하는 안은 금지다. 기존 container에 `Arc<Mutex<ChildEntries>>` 또는 동등한 shared view를 두고 unresolved/held·verified policy+own candidate·lifecycle state를 완전 tuple로 공개한다. 동일 child 등록은 멱등, 다른 parent/kind 충돌은 hold다. gate·boot_ownership·prepare/refresh·writer가 같은 view를 읽는다. **candidate/adoption·store/cursor·checkpoint·OwnerPermit·custody·원본·native source·readiness는 child 소유**이며 parent 객체를 alias하지 않는다. O-b에서는 unresolved/candidate 부재가 `channel_gate.rs:473-474`의 legacy로 빠지지 않고 Held/identity error다. O-a는 view/reader만 준비하며 기존 출력 membership을 바꾸지 않는다.
4. **부팅 REST 호출은 0회**다. 동기 install에서 committed∪session/inflight/binding의 알려진 ID를 수집하되 먼저 init.policy_parent와 S6의 trusted BindingContext를 읽는다. 기록 있는 child는 parent boot policy와 own stored adoption으로 해소하고, 기존 permit/source 등 다른 전제가 유효한 fixture에서는 REST 장애 중에도 즉시 ready다. 기록 없는 옛 child는 **보류하지 않고 late 예약으로 둔다**: 그 thread의 열린 턴과 다음 placement 전 출력은 지금처럼 Legacy가 한 번 보낸다. 부모 확정과 기록은 다음 입력 placement의 checked 조회가 맡는다(5). body 경로에서 gate가 본문을 붙잡는 안은 아래 site 전수표 때문에 비채택한다. `bootstrap.rs:73`→`seeded:278-285`→`stored_with_committed(adoption.rs:394-409)`→`listed_kinds:197-200`에서 child를 일반 비등록 채널 오류로 오인하지 않게 한다. 명시 선택 목록·확인된 일반 non-binding kept ID의 기존 config 오류는 유지한다. 기록 없는 committed ID는 REST 없이 기존 config 오류를 유지한다. 부팅은 thread 본문을 지연시키지 않는다. store-only child도 포함하며 손상/불명 기록은 없음으로 변환하지 않는다.
5. 기록 없는 child의 parent 확정은 **다음 입력 placement**(`cutover/intake_route/input.rs` `for_input_placement`)의 checked 조회 한 번이다. 성공하면 init.policy_parent 경로로 기록하고 late 예약을 잇는다. timeout/오류면 그 입력을 placement에서 보류한다(기존 intake Hold). 입력이 세션에 닿기 전이라 붙잡을 본문이 없고, 보류된 입력은 버리지 않고 다음 placement 시도에서 다시 조회한다. 2초 refresh는 해소 단계를 갖지 않으며 기록 있는 child의 refresh는 REST0이다. 새 timer/polling/backoff/운영자 reset은 만들지 않는다. boot·gateway 재개·execution refresh가 같은 파생 목록을 보고 parent permit을 빌리지 않는다.

   **site 전수표 요약(b3 1레그).** body site(`BodySite` 39개)마다 gate 안에서 기다렸다 한 번 보내는 안을 확인했다. 불가 7곳: W13(전역 `outbox_worker` custody drain 루프, 기다리면 전 채널 outbox 정지), W33(전 채널 공유 watchdog 안의 sync raw claim `health/recovery.rs:599`), W30·W30a·W31·W32·W35(부팅 restore 순차 루프). 나머지 async site도 같은 흐름의 sync peek가 claim보다 먼저 `ChildHeld`를 받아 대부분 건너뛰거나(`tmux_watcher.rs:944`의 `unwrap_or(true)` 등) 버린다. 그래서 부팅 보류의 소유자를 gate로 둘 수 없고, 보류 지점을 본문이 생기기 전인 placement로 옮긴다.
6. **late adoption의 시점**을 고정한다. 이미 committed init이면 own Committed/Held 복원이며 새 reservation을 만들지 않는다. init 없는 child를 BOOT 공개 전 **또는 watcher·turn_bridge 개방 전 child 분류 단계**에 발견했고 `late_adopt_capable`이면 `Candidate::reserved_late()`를 쓴다(`adoption.rs:137`, `host/first_init.rs:68`). 이후 처음 보는 child는 생산 경계가 **과거 legacy 실행·부채 없음까지 확인한 runtime fresh**일 때 Pending이며 등록 전 legacy send0이다(조회 실패는 placement 보류). 늦게 발견한 기존/불명 child는 late 분류를 유지한다. 부팅에서 late 예약된 child는 다음 placement가 기록해도 reserved_late로 이어진다. **인계는 진행 중 Legacy 전송과 late 턴이 끝난 뒤**다: placement가 late 턴 도중에 기록하더라도 child init 확정은 그 Legacy 전송의 정산을 기다리고, 다음 턴부터 O가 한 번 보낸다(상태 차트 `WriterThreadActive` `NoWait`). fresh Pending도 init/adoption 결정 전에 gate·placement·body에 노출해 take()가 Released로 바꾸는 통로를 열지 않고 §2-7의 완전 활성 경계와 묶는다. `channel_policy.rs:35` 불변식 범위를 이 두 초기 단계로 명시 확장한다. 일반 Pending의 body claim은 Released로 넘어갈 수 있어(`adoption.rs:286-294`) boot late child에 쓰면 안 된다. 같은 Candidate Arc·LateBoundary/send counter를 writer 재spawn에도 유지한다. 기존 late fence/custody·진행 중 legacy body claim이 끝난 경계에서 O가 인수하며 legacy/O 동시 body0이다. RED는 legacy 이력+재시작을 가로지르는 inflight tmux thread→O body1회, 일반 Pending·Candidate 재생성 뮤턴트 RED다.
7. O-b 활성화는 parent 판정·child membership만 열지 않는다. `cutover.rs:30` boot_ownership, `owner_execution.rs:70,233` prepare/refresh, `o_writer_host.rs:429,796,868`, `writer/host.rs:530,684` start_managed, child source binding·checkpoint·ready wake를 한 계약으로 연결한다. parent ready는 child ready가 아니다. **`scripts/check_tui_o_writer_census.py`의 각 CENSUS site × child ID fixture에서 실제 gate 판정=O**를 확인하며 EXPECTED_GATES와 기존 장부를 재사용한다. O snapshot.channels() 직접 소비 5파일은 아래 표대로 shared view를 읽는다. Discord intake뿐 아니라 dispatch·routine TUI·terminal thread 등록을 RED에 포함한다. source-not-bound는 O 전환만으로 해결됐다고 보지 않고 각 생산자의 child history watermark/native source 선등록을 검증한다(`history/v2.rs:395,545`).
8. 종료/clear/restart는 actual child execution·nonce에만 I/O하며 parent·sibling을 건드리지 않는다. 설정 H지만 live T인 child도 actual-target operation을 쓴다. child store 은퇴는 아래 existing thread_gc 성공 경계에만 연결하며 host cleanup 권한·TTL을 넓히지 않는다.

### O-b 활성화 분할(b3a·b3b)

b1(기록된 child 복원)·b2(child store 은퇴)에 이어 활성화를 두 리뷰 단위로 나누고 넷을 함께 착지한다. **b3a(무효과 기반)**: 부모 정책 상속(dormant reader·Codex turn 선택을 `policy_channel()`로), 롤백 중단 뒤 재확인, 상태 차트 E. capability는 false 그대로이고 child는 생산되지 않는다. **b3b(활성화)**: 생산자(입력 placement·thread session 생성)의 checked 부모 기록과 production `VerifiedSurface` 생성자, 첫 child init(floor→init.policy_parent), 부팅 late 예약 수집, 인계 대기, census site×child fixture, capability true. production `VerifiedSurface`·checked 해소는 placement 생산자 외에 호출자가 없어 b3b에 둔다(휴면 코드 금지).

### 첫 child init과 롤백 하한

옛 reader는 `store/mod.rs:265 channels_with_init`이 최상위 child init을 찾으면 `seeded:278-285`의 비등록 오류로 전체 부팅을 중단한다. O-a는 parent 필드·reader·floor/guard 기계장치를 같은 조각에 준비한다. **`o_store_thread_children`은 child JSON reader만의 표지가 아니라 child restore+O gate+writer/source/ready 전체 capability**다. 따라서 O-a는 false/미광고, O-b의 완전 활성화 빌드부터 `src/cli/build_info.rs:7-24`에 `o_store_thread_children: true`를 발행한다. O-a에서 true를 미리 내면 O-b→O-a rollback이 통과하여 legacy로 회귀하므로 금지다. O-a production의 신규 child init/adoption commit·floor 생성·child writer 시작은0, 일반 init은 기존 경로다. 새 runtime switch·휴면 #[allow]를 만들지 않고 기존 init 경계에 사용되는 분기로 준비한다.

O-b의 child 첫 `init` publish는 **floor 원자 생성·내구 flush → init.policy_parent 포함 publish → 기존 adoption/writer/source/checkpoint 준비 → ready** 순서다. 공통 경계는 `store/mod.rs:314-336 init_channel_as`이며 첫 init을 쓰는 모든 경로가 여길 지나야 한다. boot 설정 밖 channel의 init_channel_as는 policy_parent가 필수이며 없으면 Err·쓰기0이다. floor 조건은 parent 필드 유무가 아니라 설정 밖 channel이다. `$ADK_REL/o_store/thread_children.floor`를 기존 `input_notice_floor.rs:13-38`처럼 create-once·regular/schema 확인·file sync·parent fsync로 먼저 보장한다. 실패면 init·ready0, init 쓰기 실패 뒤 생긴 floor는 보수적으로 유지한다. floor가 생겼다는 사실은 child ready/게시 성공의 증거가 아니다.

`deploy-release.sh`에 `_rollback_would_revert_o_child_store`와 `_binary_would_revert_o_child_store(binary)`를 추가한다. floor 존재(손상/링크 포함 보수 판정)+대상 binary build-info flag가 정확히 true가 아님(missing/false/unknown/schema·실행 오류 포함)이면 **ROLLBACK REFUSED·fail-forward**, stop/swap/restart·artifact staging0이다. `_rollback_release_binary:1240-1261` O 가드 열, 외부/source/peer artifact 공통 선택 후 pre-stage `:2771-2774`에 연결한다. `_binary_build_flag:973-992`로 **실제 backup/target binary**를 읽어 stale manifest를 호환 근거로 삼지 않는다. ledger(:1016)/writer(:1050)/spool(:1067) 기존 가드와 override 불가 의미를 보존한다. no-floor와 capability-true 대조군은 다른 기존 guard를 유지한다. 롤백은 서비스를 멈춘 뒤 swap 직전에 같은 판정을 **한 번 더** 한다. 첫 판정과 중단 사이에 실행 중 binary가 floor를 만들었으면 swap0·새 binary 재기동(fail-forward)이다. 실행 중 binary에 그 결과를 알려 child init을 끄는 신호는 두지 않는다(운영자 스위치 신설0). 수용 문장은 "재확인 실패 시 옛 reader binary 부팅0(swap 없음), 새 binary 재기동"이다. capability 검사 경로를 manifest 방식으로 바꾸면 manifest writer(:647-676)의 boolean 기록까지 같은 조각에 넣어야 하며 단순 복사는 금지다.

RED/뮤턴트는 (a) floor+old/O-a target deploy fixture에서 REFUSED·rollback0, 외부 artifact stage0, (b) floor publish가 init보다 먼저이며 역전 mutant RED, (c) floor failure→init0, init failure 뒤 floor 유지, (d) **old main reader**에 thread init을 준 `seeded` Err 위험 고정 시험과 new reader 복원 GREEN의 구별, (e) guard 제거·O-a capability true 조기 발행 mutant RED다. child store를 parent 아래 threads 디렉터리로 숨기는 대안은 rollback 때 조용한 legacy 회귀·중복 게시 위험 때문에 비채택한다.

### thread_gc와 child store 은퇴

은퇴는 **물리 삭제가 아니라 durable lifecycle 표식으로 활성 boot/refresh 후보에서 제외**하는 것이다. 현재 `store/mod.rs:162-165`의 ‘생긴 channel directory를 production이 지우지 않는다’는 전제를 유지하고 init/ledger/cursor/receipt/body 파일을 지우지 않는다. 은퇴 표식은 proactive writer/refresh 후보만 제외하며 O 본문 소유를 NonMember/Legacy로 되돌리지 않는다. 지연 old body는 OwnedHeld/검증된 O 재개로만 처리한다. 기존 `hosted_cleanup/thread_gc.rs:54-67`의 occupancy→local host probe→조건부 DB delete, DB `thread_gc.rs:80-106`의 exact row/hosted payload/dispatch/children/idle 재확인을 약화하지 않는다. `agentdesk_hosted_execution_deletable`의 Bound/Pending/unknown 보호와 tmux ProbeFailed도 보존한다. H child+tmux missing만으로 delete0이다.

GC의 **실제 row delete 성공** 뒤 같은 child/init provenance·row/nonce/generation에 대한 은퇴 요청을 O lifecycle owner에 전달한다. 요청은 새 successor나 parent/sibling에 권한을 주지 않는다. O-a는 기록/은퇴 요청 규칙을 정의하고 O-b가 targeted writer lifecycle과 실제 은퇴를 연결한다. 현재 production은 `ManagedWriterHandle::into_detached:493`을 써 stop_and_join(:499)는 cfg(test)뿐이다. 따라서 이미 production stop API가 있다고 가정하지 않고, child entry가 해당 generation의 managed handle/완료 증거를 보유하도록 O-b 배선 비용에 포함한다. 기존 stop/settlement primitive를 production child owner 안에서 재사용하며 provider 전체 gate를 닫지 않는다.

새 input/admission을 같은 child transition 아래 배제한 상태에서 fresh owed 관측(`Readiness::undelivered_after:277`, `actor::Undelivered`)의 owed/notice_owed/prepared/unsealed/uncaptured/binding_pending/verified_work/posting을 확인하고 child writer 종료/settlement를 기다린 뒤, expected generation이 여전히 같고 DB successor가 없을 때만 은퇴 표식을 내구 publish한다. `activation::settled`의 Committed는 부채0 증거가 아니다. 부채·writer 상태·row 증거가 불명확하거나 stop 실패·새 input/rebind가 있으면 표식을 확정하지 않고 보존/Held·기존 refresh 재평가다. 단순 actor 부재도 ledger 부채0으로 간주하지 않는다. tombstone 쓰기 실패/DB delete 직후 crash는 다음 refresh에서 보존된 exact delete 근거와 잔여 store를 재대조하며 없음만으로 은퇴하지 않는다. delete 성공 근거가 crash로 소실되어 입증되지 않으면 active/held store를 보존하며 자동 은퇴 성공·진행성을 주장하지 않는다. 다음 same-thread 입력은 은퇴 표식을 명시 supersede하고 새로운 generation으로 기존 init/ledger/cursor/receipt를 open해 정상 재개하며(새 init 강행0), 지연된 옛 은퇴 요청은 CAS 거절한다. 기존 일반 채널 store 보관 정책·새 광범위 GC는 바꾸지 않는다. RED는 delete→new input→늦은 표식0, 미게시 부채 보존, stop/표식 실패·재시작 후 회복, 부모/sibling 파일 변경0이다.

### Herdr 배치와 수명

이번 H1은 **기존처럼 logical child 실행마다 별도 managed workspace(그 root tab/pane)**를 선택한다. 같은 endpoint/herdr session 안에서 logical key+nonce marker로 parent와 관련된 child를 식별하고, 표시 이름은 #8119 typed name/metadata를 따른다. label을 파싱해 권한을 얻지 않는다. 근거는 현 `herdr_launch/prepared.rs:35-58`의 `workspace_marker(logical_key,nonce)`와 `session_host/herdr/launch_host.rs:571`의 WorkspaceCreate다. unplaced 복구는 workspace pane/tab 1개를 전제로 한다(`launch_host.rs:765-780`). 부모 workspace에 tab을 추가하는 안은 이 가정과 exact residue cleanup을 바꾸므로 이 묶음에 넣지 않는다. 같은 workspace 배치가 필수라는 사용자 결정은 없으며, 화면 배치의 차이는 §5에서 설명한다.

수명은 AgentDesk의 현재 exact row-end/close/retire 경로가 관리한다. Herdr Core parent/child, parent 종료시 자식 자동 kill, 새 TTL/reaper/광범위 GC는 #7583 또는 별도 요구 없이 구현하지 않는다. 채택된 child store 논리 은퇴만 위의 기존 thread_gc 경계에 연결한다. 끝난 thread는 기존 세션 종료 권한으로 그 실행만 닫는다. ‘pane 수 증가’를 이유로 main에 없는 삭제 권한을 얹지 않는다.

### 판정 소비 좌표와 A 첫 기준선

부록 A는 요구된 네 패턴의 **254개 Rust 텍스트 hit/142파일 전체**다(정의·import·문자열·시험 포함). 전체 tracked 검색은 287 hit/152파일이며 Rust 밖의 smoke N/A 분기도 조사했다. 이것을 254 production 호출이라고 세지 않는다. 단순 파일명 시험 제외도 98 hit/69파일로 inline cfg(test)·import가 섞인다. #8122 A의 scanner가 실제 cfg/test·정의·alias를 판별해 production site 기준선을 생성해야 한다. r2의 추가 `configured_channel_ids`·`with_boot`·raw boot.channels() 전수와 thread_gc는 부록 A2에 보존한다. O output accessor 검사는 새 A 중복 rule을 만들지 않고 기존 `scripts/check_tui_o_writer_census.py` CENSUS/EXPECTED_GATES로 대신한다.

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
| SQL selector 정책 | `config/session_hosts.rs:354`, `src/db/dispatched_sessions.rs:2212,2249` | **H2**: configured 부모+검증된 child 관계를 공통 SQL/bind로 전달. H child selector clear0·unknown no-clear, child predicate 제거 mutant RED |
| raw boot inventory·consent | `discord/router/herdr_consent.rs:32,395`, `herdr_consent/return_input.rs:171`, `herdr_consent/startup.rs:27`, `src/cli/doctor/orchestrator/managed_herdr_config.rs:19,30`, `src/server/routes/health_api/runtime_profile.rs:20,61` | H3: child 정책은 shared view, 전역 endpoint inventory는 boot owner 안. registry/maintenance/policy diagnostics 내부 원천은 허용 owner(부록 A2) |
| O 직접 membership 5파일 | `tui_o/topology.rs:35`, `discord/runtime_bootstrap/owner_execution.rs:80,157`, `discord/runtime_bootstrap/o_writer_host.rs:447`, `tui_o/cutover/channel_gate.rs:442,449`, `tui_o/cutover.rs:35,59,73,132,136` | O-a shared enumeration 준비/O-b gate 활성. 값 clone의 직접 channels() 대신 shared child view. CENSUS·EXPECTED_GATES site×child fixture |
| thread GC·store lifecycle | `dispatched_sessions/hosted_cleanup/thread_gc.rs:13,54,63`, `src/db/dispatched_sessions/thread_gc.rs:80`, `tui_o/store/mod.rs:162`, `writer/host.rs:493,499` | H2/H3 host 판정·기존 delete 보호, O-a/O-b 은퇴 계약/실행. directory 삭제0·exact child만 tombstone |
| legacy 삭제 예정 | `discord/relay_recovery/apply.rs:100`, `tmux_watcher/host_gate.rs:153` 등 부록의 legacy caller | #7651 삭제 쪽에 귀속. 동작 수리 금지; 살아 있는 O 경계가 이 좌표를 우회하는지는 O 시험으로 검증 |

이 표·부록이 migration checklist다. H1/H2/H3 합계가 한 리뷰 조각의 30파일 상한을 넘으므로 한 번에 바꾸지 않는다. A가 baseline을 새로 늘려 숨기는 일 없이 각 조각 후 direct site 감소를 증명한다. high-level 공통 policy/operation 호출이 늘어나는 것은 재사용이며 금지하지 않는다. 완성 시 **허용 owner 밖 direct policy 판정 0**, legacy 삭제 예정 caller는 삭제 ref/담당을 적고 #7651과 닫는다.

## §3 이슈별 구현 조각·계약

### 순서와 크기 규칙

모든 조각은 prod **30파일/순 코드 +1500**, PLAN 정지선 **+1200**을 적용한다. 아래 수치는 실행 전 추정으로 시험·모델 문서는 별도며, 경계 이동으로 파일이 늘면 먼저 다시 나눈다. 실측/추정은 구현 보고서에 함께 적는다. 기준선 수치/규칙·audit 예외 변경은 FILES 승인이 필요하고 숫자를 올려 상한을 피하지 않는다. 아래 명시 `--relabel`은 동일 multiset의 위치 라벨만 공식 generator로 옮기는 허용 경로이며 수치/규칙 변경 예외가 아니다. 여기에는 생산 코드를 추가하지 않았다.

착지 순서는 `#8114 배포 → #8118 O-a 준비 → O-b 활성 배포·수용 → #7651 자체 관문 충족 뒤 P2/P3 허용`이다. **O-a/O-b가 이 묶음의 첫 출력 경로 묶음**이고 O 완료/배포라는 선행은 O-b를 뜻한다. O 수용은 #7651의 필요조건이며 충분조건이 아니다. O-a만으로 H1/T1/R·legacy 삭제를 열지 않는다. 그 뒤 `H1 → #8125 T2`, `#8125 T1 → #8128 R → #8109 S5`로 잇는다. T1(herdr 채널 external start)은 H1 thread 기능과 분리할 수 있지만 O-b보다 먼저 착지하지 않는다. T1은 early refusal만 제거하지 않고 최종 `StreamingTurn`까지 동일 admitted executor·provider switch·O-ready·actual target 가드를 연결한다. feature off/미준비·Unknown 거절은 계속 유지한다. 관리 명령의 actual locator 소비는 **#8119 S8b1 뒤**, 복원/routine 이름 해석은 **#8119 S8b2 뒤**, BindingContext optional parent_channel_id/thread_id의 schema·producer 변경은 **#8119 S6 뒤**로 표시한다. O가 필요한 checked live parent metadata는 기존 Discord 경계를 사용하며 S6 필드를 중복 신설하지 않는다. S6 이후에는 immutable BindingContext parent/thread도 소비하되 HTTP metadata와 권한 provenance를 구별한다. `/resume` raw old-alias 정규화는 #8119 S3+S8b1 뒤다. d2의 primary tail=current actual name/old alias transaction을 그대로 소비하며 과거 routine ownership token을 successor locator로 정규화하지 않는다. 이 설계에서 #8119 코드를 선취하지 않는다. #8115 배포는 no-session thread 수용의 선행이다. 각 독립 조사·시험 설계는 병렬 가능하다.

| 조각 | 구현 범위·주요 파일군 | 추정 prod / 순증 | 선행·정지 조건 | RED 계약 → GREEN, 원인 제거 뮤턴트 |
|---|---|---:|---|---|
| **8118-O-a** | shared child view·boot/refresh 기록 우선 reader·Initialized.policy_parent·store 논리 은퇴 요청 계약·floor/rollback/artifact guard machinery. child gate NonMember, 신규 init/floor/writer0·capability false/미광고 | 12~18 / +550~800 | **#8114 배포 뒤 첫 준비**, O-b와 같은 경계 계약. PLAN 여유 +400~650. boot/rollback도 P1 차단 유지 | old JSON/None 호환, trusted provenance만 기록, 기록 child REST0·옛 child Held→기존 refresh 해소, old seeded Err 고정, old/O-a artifact refusal fixture. 필드 무시(REST 호출>0)·raw/HTTP parent 저장·capability 조기 true·가드 제거 뮤턴트 RED |
| **8118-O-b** | child membership·late adoption·writer/source/checkpoint/ready 활성, 모든 CENSUS site·producer 적응, first init 전 floor와 capability true, exact generation child writer stop/정산·GC store 논리 은퇴 실행 | 16~24 / +750~1000 | O-a 후 같은 head 기반, **O 출력 완료는 이 조각**. PLAN 여유 +200~450; +1200 전망이면 활성 계약 완전성을 유지해 재분할·PLAN 정지 | O body1회·parent/sibling0, floor→init 순서·late inflight takeover·dispatch/routine/terminal 등록·site×child=O, committed store-only REST0·legacy Held refresh·child permit refresh. 부채/새 generation 은퇴0. 기록 없음→부팅 REST0→refresh 늦은 해소→late 경계 O body1회, boot-discovered 표지 누락으로 Pending이 되는 뮤턴트 RED. REST 실패 fake+비-O 부모 inflight→Held, 회복→legacy1회; 해소 단계를 provider 분기 안으로 옮긴 뮤턴트 RED. direct ID·parent Candidate/permit alias·floor 역전·일반 Pending·ready 선발표·늦은 은퇴 mutant RED |
| 8118-H1 | 같은 SurfacePolicy의 desired host 소비: herdr_launch/turn_host, Claude/Codex launch/absence, intake/queue/bootstrap. #8114 공통 operation을 사용 | 17~24 / +450~750 | O-b·#8114 배포, thread bootstrap은 #8115, BindingContext schema/producer 교차는 #8119 S6 뒤. 새 core host match 발견시 DELTA | H parent 새 child H·T parent child T, live old T 보존·다음 launch만 H. fake host operation 공통 흐름. direct-child endpoint/parent actual locator 치환/unknown fallback 뮤턴트 RED |
| 8118-H2 | source·presence·O notice·clear/설정 predicate, selector clear 공통 SQL/bind·thread_gc host 보호를 정책/actual target에 적응 | 20~28 / +450~750 | H1, 명령 부분 #8119 S8b1 뒤. guard 개방을 섞지 않음 | H/T × channel/thread의 capture/source/stop/clear 실제 child 대상. old T row+H policy에서 T IO 유지. source pin 제거·정책 host로 actual IO 선택·SQL child 보호 제거·H child tmux probe로 delete 뮤턴트 RED |
| 8118-H3 | consent/exact recovery·raw boot inventory·admin guard·주변 cli/agents/provider cleanup 소비자, ThreadGC exact child lifecycle 연결. legacy는 #7651 귀속 | 18~26 / +350~650 | H1/H2, #8119 S8b1/S8b2 해당 후. owner 밖 direct 판정 최종 0 | unknown/conflict/Pending/stale nonce에서 mutation0, child clear/cancel이 parent·sibling에0. guard의 parent lookup 누락·stored endpoint 대신 config 재선택 뮤턴트 RED |
| **8125-T1** | Herdr 채널 external start/deliver. headless의 별도 executor_not_wired 거절을 공통 admitted provider executor로 연결. existing busy Herdr inject는 재사용 | 6~10 / +350~650 | O-b·#8114 배포. #8128 R seed의 선행 | idle H start가 422인 RED→accepted/started, busy imessage는 공통 주입·generic machine은 자기 턴. tmux 호출0, GUID retry1회, unconfirmed no-resend. 일반 refusal 복원·중복 admission·machine human 오분류 뮤턴트 RED |
| 8125-T2 | 양 API 공통 target: checked thread parent 권한, provider/runtime owner와 destination 분리, no-session bootstrap, deliver HumanInputRequest 적응, 문서 계약 갱신 | 7~11 / +400~700 | T1·H1·#8115 배포. #8119 thread descriptor 충돌은 뒤 | 두 endpoint × T/H × public/private/news thread 허용, 실제 child session/turn ID, 확정 foreign/missing parent/non-thread category403·parent 미확정503 retryable·provider mismatch422. 세션 없는 child deliver2+Discord1 동시 도착→native session 시작1·original3개 queue/inject 유실0·중복 세션0. exact-bound only 복원·metadata parent 신뢰·503→403 압축·bootstrap 재확인 제거·child로 provider 조회·parent dedupe key 뮤턴트 RED |
| **8128-R** | `/restart`의 host-neutral cancel→PreserveSession stop→exact end→native resume relaunch→seed. 종료/launch owner와 session_host operation 경계 재사용 | 8~13 / +550~900 | **#8114·O-b·#8125 T1 배포 및 #8119 S8b1 뒤**, H1/H2 뒤 thread 수용 검증. 이슈 내 첫 관리 조각 | H restart 현재 refusal RED→busy 취소·old pane close·new nonce pane+동일 native ID·seed1회·다음 메시지 정상. no-session은 close RPC0+fresh seed, sid 없는 active는 exact end 뒤 fresh 안내. disabled/refused preflight는 cancel/close/launch0. core None+Bound live는 fresh 금지, close 최종 Retired만 진행·ProviderSurvivedClose/ACK/불명/서버 교체/stale nonce는 spawn0. ProviderSurvivedClose→relaunch·메모리 None→absence·close 확인 제거·clear로 native ID 삭제·seed refusal 복원·parent pane close 뮤턴트 RED |
| 8128-R2 | tmux_recreate·provider_reset·auto_queue/health managed reset·resume가 R operation 재사용 | 8~14 / +350~600 | R·#8119 S8b1/복원 S8b2 뒤, resume은 S3도 선행 | 각 행 동일 결과, pending/reset recheck race 기존 보존, foreign actual0. 공통 operation 우회·verified clear 직전 recheck 제거 뮤턴트 RED |
| 8128-M/S | Claude model 즉시 적용 및 reset 필요한 설정·Codex goals. idle 경계에서 동일 native resume 재기동, busy는 기존 defer·현재 턴 취소0, 저장/적용 결과를 분리 | 5~9 / +300~550 | R·#8119 뒤, #8109 설정 계약 재대조 | 저장 성공+현재 execution 설정 적용 확인, 실패시 pending flag 보존. live skip·적용 전 flag 소거·wrong child target 뮤턴트 RED. Claude effort/fast 직접 적용은 제외 |
| 8128-C | compact를 actual InputTarget/MutationGate에 전달 | 3~6 / +150~300 | #8114 operation·H2·#8119 뒤 | H live compact 전달1회/T 회귀0, unknown/changed target key0. legacy-only guard 복원·per-mutation pin 제거 뮤턴트 RED |
| 8128-D | 진단 상태 label·실제 capture/cwd/native source·API host_unsupported·`/adk` literal label | 6~10 / +300~550 | H2·#8119 S8b1 뒤 | actual H capture 표시, read 불명 명시·mutation0, channel/thread 대조. tmux name capture 복원·read 실패를 absent 처리 뮤턴트 RED |
| 8128-E | clear 결과/종료 기록·Codex capture skip의 사용자 차이 확인·적응 | 4~7 / +200~400 | R·H2·#8119 뒤 | clear 전 native ID/usage 기록 보존, clear 후 새 세션 의미 유지, parent/sibling 기록 영향0. herdr 기록 skip 복원·parent history 사용 뮤턴트 RED |
| 8122-A | 소유 모듈 밖 low-level accessor site ratchet·Rust lexical scanner 보완·baseline generator·CI/prepush wiring | 6~9 script / +400~700 | 최초 baseline 승인, H 조각과 병렬 가능; 신규 직접 분기 금지 | 기본검사 새 site/동수 교체/alias/multiline 증가 실패, relabel 동일 hash multiset·수 불증가만 허용, helper 재사용·owner·test·string/comment 통과. relabel count 증가·총수만 비교·owner scope 오해·inline test 미제거 뮤턴트 RED |
| 8122-B | 3곳 반복 조건식 alpha/receiver 정규화 warning·main TP/FP/불명·known seed recall·채택 보고 | 4~6 script / +400~650 | A scanner 계약 재사용 | 2회0→3회 경고, alpha local·등록 accessor 인자/get 차이 군집 검출, literal·순서·필드 다른 식 분리. precision/seed recall 둘 다 보고. threshold4·정규화 제거·&& 재정렬·string 오인 mutant RED |

### #8118 출력 수용과 #7651 인계

**O-b 수용은 #7651 P2/P3의 필요조건이며 충분조건이 아니다.** 부모 O 아래 thread에서 기존 10턴이 실제 O로 답한 증거(placeholder0·답장형 body0·중복0·카드·native usage 푸터)를 내고, 해당 child 하나가 아니라 **O 부모 아래 모든 child의 새 legacy body 전송0**을 parent 범위로 집계한다. 과거 296건을 초기화하지 않고 배포 SHA·관측 창·producer/provider/parent host별 분모를 남긴다.

#7651 자체 관문(업무 채널50턴 이상·재시작1회·무트래픽0 금지)에 **thread 생산자 4종(Discord intake/dispatch/routine TUI/terminal) × provider 2종(Claude/Codex) × 부모 host 2종(T/H)=16칸**을 더한다. 각 칸은 0 아닌 실제 턴 수·실제 child/답 게시·legacy send 수를 기록한다. 미실행/등록 누락/0분모 칸은 PASS가 아니며 삭제 관문을 열지 않는다. cohort 전체 parent 범위 집계에 synthetic API thread producer를 포함할 때는 T2 배포 후로 추가하고 이 16칸을 축소하지 않는다. 표본 thread 하나의 성공으로 미관측 child를0으로 추정하지 않는다.

O RED에는 dispatch·routine TUI의 thread 등록과 모든 `check_tui_o_writer_census.py` CENSUS site×child ID의 실제 gate=O를 포함한다. source-not-bound는 Discord/dispatch/routine/terminal 시작과 기존 내부 owner-channel starter의 API-origin fixture로 native binding/history 선등록·busy inject를 확인한다. 공개 turn/start·deliver thread 실증은 T2 뒤의 별도 수용이며 O를 기다리는 순환을 만들지 않는다. 이 레그는 이 실증을 수행하지 않았다.

### #8125 권한·입력 보존

공통 `AgentTurnTarget`은 현재 provider/primary/channel만 갖는다(`agents_turn_target.rs:8`). 목표는 verified owner channel과 actual destination을 별도 필드로 전달하는 것이다. bound channel은 기존 동작, thread는 checked Discord kind+parent와 **요청 agent의 bound parent** 일치로 허용한다. live child row가 있어도 권한 검사를 건너뛰지 않는다. metadata.agent_id mismatch403(`agents.rs:742`), provider mismatch422(`agents_turn_target.rs:117`), author preflight(`health/turn_deliver.rs:70`), source GUID/obligation/unknown no-resend는 그대로다.

서버가 기록한 S6 BindingContext parent를 1차 증거로, init.policy_parent와 provenance를 대조한다. 기록이 없는 옛 child에만 checked REST를 사용한다. 현재 요청 agent의 bound parent 소유 확인은 기록 유무와 별개다. 확정된 남의 parent·실제 non-thread/category·확정 parent 없음은403, timeout/REST 오류/손상·상충으로 관계 미확정이면 **`503 {"status":"thread_parent_unverified","retryable":true}`**다. 미확정 응답은 admission·GUID 소비·queue·bootstrap·launch0이며 재시도는 같은 origin을 유지한다. 503 표시는 새 자동 전송기를 만들지 않는다. 기존 bound-channel·provider422 계약은 그대로다.

T2 경합 RED는 **세션 없는 같은 child에 고유 origin의 API deliver2건+Discord 첫 메시지1건 동시 도착**이다. 기존 child transition/mailbox claim·owner admission·physical grant 아래 native session/worktree 시작1회, original3개는 각각 기존 queue/inject/자기 턴으로1회·유실0·중복 세션0이어야 한다. native 세션 시작1회는 model turn 전체를1회로 합친다는 뜻이 아니다. 같은 GUID 재시도 변형은 그 original만1회다. bootstrap 직전 current child 재확인, parent/sibling 영향0을 검증한다(`session_runtime.rs:527`, `session_transition.rs:165`, `router/turn_start.rs:703`, `health/turn_deliver/inject.rs:427`).

runtime/settings/binding 선택의 policy는 parent, mailbox/session/turn-ID/output/origin identity는 child다. 기존 `health/headless_turn.rs:100-139` owner-channel starter, `health/runtime_resolve.rs:167-224`의 runtime parent 수용을 재사용한다. no-session은 Discord 첫 메시지의 bootstrap/worktree/placement를 공통 호출하며 새 thread 전용 launch 구현을 복제하지 않는다. #8115의 ready preferred fallback은 그 이슈에서 고친 것을 전제로 통합시험한다. API가 child를 받는다는 이유로 routine process fresh·scheduled snapshot scope를 일반 channel 세션에 합치지 않는다.

### #8128 재시작 경계

R은 기존 lifecycle owner가 operation을 조립하고 host adapter가 exact pane IO를 맡는다. `InteractiveSessionHost` 기본 Herdr registry는 read-only(`session_host/herdr_registry.rs:49-87`)이므로 그 `send_text`/sole_host를 mutation factory로 쓰지 않는다. #8114의 actual target/observation·host input, `!clear`의 exact end, #8077 close·existing nonce/CAS를 재사용하고 공통 명령 파일에 새 `if herdr` 분기를 늘리지 않는다. creation/destruction 권한은 현 owner에게 남긴다(`session_host.rs:1-3`).

순서는 기존 admission/직렬화 확보→same actual execution pin→cancel→PreserveSession stop→exact close/end 확인→old row retire 확인→같은 native session을 resume하는 새 execution launch→O ready/source binding→완료 seed다. clear는 대화 기록·selector를 지우는 다른 operation이므로 통째로 호출하지 않는다. 특히 `clear_teardown.rs:175-186`의 after_retire는 `clear_retired_selector_pg`를 호출하므로 R이 재사용할 대상이 아니다. `herdr_terminate::OperatorTerminate`와 exact end/RetiredExclusion을 재사용하고 `retired_exclusion.rs:99-104 disconnect_session_keeping_provider`처럼 native ID를 보존하는 postlude를 고른다. provider_reset에서는 별도 reset 플래그에 따라 native ID/history clear 의미를 유지한다. close가 Unknown/Pending/stale/replaced이면 spawn·retire 강행0, 명시 실패로 끝낸다. 새 launch의 ACK 불명도 기존 pending/unplaced 복구를 쓰며 재생성 반복을 하지 않는다. `herdr_terminate.rs:118-120,446-447`의 기존 terminate live switch/명시 사용자 권한은 사전 admission에서 확인한다. Disabled/refused이면 cancel·close·row·launch0으로 반환하고 성공을 표시하지 않는다. 값은 이 레인에서 읽지 않았으며 가드 우회/새 스위치/자동 권한 확대는 하지 않는다. 실제 적용 범위가 R과 충돌하면 구현 전에 DESIGN_DELTA다. 사전 통과 뒤 경합으로 종료가 거절되면 기존 cancel 완료 여부를 있는 그대로 보고하고 후속 launch0으로 끝낸다. tmux의 현재 stop/reset 의미·held queue 처리도 바꾸지 않는다. `/restart`만 열고 `restart.rs:150`의 headless seed가 거절되는 미완성은 금지다.

`RestartAction`의 기존 세 분기(`restart.rs:17-25,45-50,68-82`)도 보존한다. actual old execution이 확인된 resumable은 위 순서, sid 없는 active는 exact end 뒤 기존 안내에 따라 fresh, **NoActiveSession은 아래 resolver의 hosted row·marker와 기존 cold-launch admission으로 absence가 검증된 경우에만 close/retire RPC 없이 fresh seed**다. missing sid를 이유로 history를 추가로 삭제하지 않는다. unknown/Pending/nonce 충돌은 NoActiveSession이 아니며 known-retired 재시도도 현재 successor 부재·권한을 확인한다. M/S 설정 적용은 R의 취소 정책을 복사하지 않는다. 기존 `control/relaunch.rs:288-293`처럼 busy면 pending을 두고 idle/다음 턴 경계에서 적용하며 현재 턴 cancel/close는 0이다. R에서 재사용하는 것은 same-native relaunch 수단이다.

absence는 `resolve_session_target`(`session_host/resolve.rs:239`) 경계가 읽는 hosted row·inflight/marker의 authoritative 증거와 기존 cold-launch admission으로만 확인한다. **`core.sessions=None`은 absence가 아니다.** 이 resolver의 `TargetHost::Unknown/Conflict`, `UnknownHost::NoSessionRow/NoHostEvidence`는 VerifiedAbsent가 아니므로 fresh 권한으로 승격하지 않는다(`resolve.rs:184-210,304-305`). core None+Bound live fixture는 exact old end 완료 뒤1회 또는 명시 거절/spawn0이어야 한다. 단순 sid 부재도 provider 부재가 아니다.

종료 결과의 전수 매핑(`herdr_terminate.rs:74-112`)은 다음과 같다. low-level ACK/Indeterminate가 서비스 내부 `read_end:186-203`→retire/settle을 거쳐 최종 Retired가 될 수 있는 기존 복구는 보존하며, ACK만 받은 시점에는 진행하지 않는다.

| 최종 TerminationResult/Close 세부 | R의 relaunch·seed |
|---|---|
| `Retired` | keep-provider postlude 성공·동일 exact row/generation 재확인 뒤만 진행 |
| `Disabled` / `Close(Refused(Disabled))` | spawn0·자동 switch 변경0 |
| `Refused(reason)` / `Close(Refused(Gate(...)))` | spawn0·거절 이유를 사실대로 보고 |
| `Close(NotSent(reason))` / `Close(ConfirmationRequired)` | spawn0·서비스가 confirmation 대신 승인하지 않음 |
| `Close(Acknowledged)` | spawn0·ACK는 provider 종료 증거 아님 |
| `Indeterminate(reason)` / `Close(Indeterminate(reason))` | spawn0·이미 일어난 부분 effect는 사실대로 보고 |
| `ProviderSurvivedClose` | spawn0·동일 native 이중 writer 금지; 이를 relaunch로 바꾸는 mutant RED |

**코디네이터가 R 발사 전에 `runtime.herdr_terminate_enabled`의 값·관측 시각을 읽기 전용으로 브리프에 기록한다.** 본 레인은 설정을 읽지 않는다. 코드의 Some(true) gate(`:118-120,446-447`)와 physical close 재확인은 유지하며 꺼져 있으면 R 수용을 추정하지 않고 기존 preflight 거절·0spawn을 확인한다. 권한/스위치 우회나 새 설정 기능은 만들지 않는다.

### #8122 A·B 검사 설계

A는 저수준 접근자의 **site multiset**을 감소만 가능한 기준선으로 고정한다. rule별 `accessor token, owned_paths, path, enclosing_fn, normalized token hash, count`를 공식 generator로 저장하고 line은 진단용으로 둔다. 총수만 비교하면 ‘old 1개 삭제+new 1개 추가’가 통과하므로 새 site·site count 증가는 severity=error다. A rule은 기존 herdr_endpoint·명명/반응 접근자 외에 **configured_channel_ids·with_boot**를 포함한다. 경계 내부 config/registry/maintenance/진단 inventory owner는 허용 범위로 명시하고 외부 child 판정 복사는 잡는다. output 쪽 A는 기존 census 스크립트로 대신한다. 공통 helper 호출 반복은 검사 대상이 아니다. use alias·qualified/method 호출을 판별하고 주석·문자열·시험 파일·inline `#[cfg(test)]`를 제외한다. 무조건 파일명 필터/regex comment stripping을 쓰면 현재 전례도 오탐한다(`scripts/audit_maintainability/common.py:42,105`). 필요한 최소 lexical 보완은 A 범위다. 의미상 같은 식 전체를 증명하는 AST 프레임워크는 새로 만들지 않는다.

baseline은 rule별 generated file로 나누고 최초 등록은 승인된 corpus로 한 번, 이후 `--reduce`는 삭제·감소만 허용한다. 일반 실행에서 baseline 자동 증가/allow 예외를 만들지 않는다. `--check-baseline`은 재생성 차이를 검증한다. 검사 등록은 `scripts/audit_maintainability.py:37-53`, 실제 실패는 `:429-438`의 **severity=error**다. hard_gate=True만 넣고 CI 연결됐다고 보고하지 않는다. 기존 prepush runner와 `scripts/ci-script-checks.sh:827` 양쪽에 fixture·검사 실패가 전달되는지 원인 제거 뮤턴트로 확인한다.

A의 공식 generator에는 명시 **`--relabel`**을 둔다. 순수 rename/move의 old→new site mapping과 diff를 남기고, 전후 **`(rule, normalized call hash)` multiset이 같고 각 count가 증가하지 않을 때만** path/enclosing_fn 라벨 이동을 허용한다. 기본 검사의 동수 site 교체 실패와 --reduce의 삭제/감소 의미는 유지한다. 총수만 같거나 hash별 multiplicity가 달라지면 실패다. rule/accessor·owner 범위·threshold·수치 증가를 relabel로 허용하지 않는다. 동일 의미의 위치 라벨만 공식 생성하는 이 경로는 수치 인상 승인이 아니며 새로운 baseline 예외를 만들지 않는다. RED는 rename/move 허용·hash 변경/복사 count 증가 거절, count 증가를 허용하는 relabel mutant RED다.

B는 if/else-if·match guard·bool let/closure의 조건을 lexical token으로 뽑아 whitespace/comment/불필요 괄호/숫자 구분자를 정규화하고 **bound local 식별자를 첫 등장 순서로 alpha-rename**한다. 같은 local 재참조 관계는 보존하며 함수/모듈 path·method·field·상수·literal·operator·short-circuit 순서는 지우거나 정렬하지 않는다. 등록된 low-level accessor의 channel 인자에 한해 bare local과 확인된 ChannelId receiver `.get()`을 같은 인자로 취급하는 **receiver-argument normalization 옵션**을 시험한다. 예시 `herdr_endpoint(channel_id.get()).is_none()`과 `herdr_endpoint(channel).is_none()`은 해당 옵션에서 같은 host 판정 패턴으로 잡혀야 한다. 모든 .get()/effectful 인자/타 의미까지 일반화하는 범용 타입 추론 엔진은 만들지 않는다. 판별 불명은 ambiguous 후보로 따로 낸다.

같은 복합 판정의 서로 다른3 site가 warning이다. 단일 **고수준 공통 helper** 호출은 재사용이라 제외하지만 등록된 저수준 accessor의 단일 predicate는 known-accessor 후보로 별도 집계한다. 단순 predicate라고 제외해 원인 seed를 숨기지 않는다. exact-token 결과와 alpha/receiver 후보 결과를 함께 비교하고 site·함수 수, macro/cfg/분석 한계도 보고한다. 경고가 같은 bool 값이라는 증명은 아니며 공통 정책 복사를 찾는 후보일 뿐이다.

부록 A/A2에서 확인한 host 중복(예: intake_turn:1110,1754·queue_dispatch:253·inject:312)과 이름/반응 정책의 positive·negative 군집을 고정 seed 정답표로 둔다. main hit 군집 TP/FP/불명으로 **precision=TP/(TP+FP)**와 **seed recall=검출 positive/전체 seed positive**를 mode별로 함께 보고한다. 단일 accessor·macro 등 놓친 seed는 FN/미지원으로 표시하고 분모를 줄이지 않는다. 그 결과로 B 기본 정규화 범위와 hard gate 채택 여부를 판단한다. r2는 scanner 미실행이라 수치는 없음이다. threshold4·alpha/receiver 옵션 제거·&& 순서 정렬·seed 누락 뮤턴트가 해당 fixture/recall 계약에서 RED여야 한다.

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
| `tui-o-writer/WriterAdoption.tla:12,28,185,220` | selection/site/runtime/execution/adoption이 한 channel, restart store seed | selectedPolicyId와 actualDestinationId, verified/unknown parent, child own adoption/store·공유 child entry의 unresolved→complete 공개·기록 우선 boot REST0·legacy refresh·reserved_late/Committed/Fresh 경계(O-a/O-b) | NoLegacyBodyOnTui·AnswerAtMostOnce·HeadlessKept 유지. parent O/child legacy Before RED→O 1회. parent Candidate alias·일반 Pending으로 boot late 대체·재spawn Candidate 재생성·snapshot child 미공유·candidate 없음/unknown→legacy mutant RED |
| `tui-o-writer/WriterWake.tla:32,49,220,258` | 모든 writer start/ready→원본 wake; source key=(provider,channel,source) | child 등록·기록 child/store-only REST0 복원·legacy Held의 기존2초 refresh 회복·gateway resume·논리 은퇴/next input O 재활성화와 child ready wake(O-a/O-b) | ThreadBodyEventuallyAnswered/held original eventual retry는 child writer 활성화/ready 공정성을 명시. hold로 영구 멈춤을 가린 PASS 금지. parent key 치환·ready wake 삭제 뮤턴트 RED |
| `execution-authority/ExecutionAuthority.tla:2,22,54,138,193,263` | 한 O channel의 view/permit/host와 기존2초 refresh, gateway/worker/standby·retirement wait | **O-a/O-b**: parent policy와 child/sibling execution 분리, init 기록/legacy parent 해소·child view/permit·refresh target 열거. 코드 `owner_execution.rs:70,146,157`, `channel_policy.rs:427,496`, `ownership/execution.rs:128`, `owner_cleanup.rs:18,86` | **Before 기능 RED**: child가 enumeration에서 빠져 ChildAdmissionResumes 실패 → **After GREEN**: 기존 tick의 parent 해소 후 child permit/ready·일시 PG 실패 후 회복 → **mutant RED**: child refresh 누락·parent permit alias·old writer retirement 생략. 영구 REST/PG 장애에 진행성을 주장하지 않음 |
| `hosted-cleanup/HostedCleanup.tla:153,178,242,275` (`ThreadGC*.cfg`) | GC 후보·occupancy·local owner/Missing probe·exact row SQL delete. 기존 Bound/Unknown 보호 | **H2/H3·O-b 은퇴**: child Herdr 행+tmux probe delete0, parent 정책/actual child 분리, GC success→fresh debt 관측→same writer stopped→durable dormant 및 next input 재개. 코드 `hosted_cleanup/thread_gc.rs:13,54,63`, `db/dispatched_sessions/thread_gc.rs:80`, `writer/host.rs:277,499` | 기존 GC safety Before GREEN 보존. **새 retirement 기능 Before RED**: 삭제 뒤 active child candidate 누적 → **After GREEN**: exact settled child만 dormant·ledger 보존/O 재개 → **mutant RED**: H child+tmux Missing 삭제·delete 실패인데 은퇴·debt/default0·writer 미종료·late tombstone이 새 generation 숨김·dormant→Legacy |
| `turn-presence/TurnPresence.tla:11,60,190,206,403` | session/owner/nonce/turn/source/core identity와 progress 분리, retired marker·local vacancy | **H2**: endpoint policy는 parent지만 presence proof/row/marker/source는 child. 코드 `turn_presence/activity/unbound.rs:305,509,525`, `unbound/herdr_bound.rs:18` | **Before 기능 RED**: H parent Bound child가 direct endpoint(None)로 Busy 게시 누락 → **After GREEN**: ChildBoundPublishesBusy·exact retired child만 다음 입력 허용 → **mutant RED**: parent proof alias·child endpoint 누락 복원·H policy만으로 live old T를 absence 처리. 기존 publication liveness 유지 |
| `intake-consent/IntakeConsent.tla:2,10,49,221` | Herdr 동의 UI 자체가 아닌 before-effects worker authority/rollback, original·claim/generation·private scope proof | **H3**: parent endpoint/user-default consent 선택만 상속, request/IntentRef/row/source/claim은 child. 코드 `cluster/intake_worker/o_input.rs:80`, `intake_turn/worker_entry.rs:58`, `herdr_consent/admission.rs:440`, `worker_handoff.rs:17` | 기존 proof safety Before GREEN 보존. **새 child parity Before RED**: direct child endpoint로 consent 분기 누락 → **After GREEN**: actual child original만 custody 이관/release1 → **mutant RED**: parent로 proof/request 재key·source/claim 생략·private key 공개·effect 뒤 rollback 변환 |

모델 변수 값의 코드 출처는 §1 모든 행과 §2 소비 좌표다. r2 표의 Before RED→After GREEN→mutant RED는 새 기능 수용 projection의 실행 계획이다. main이 이미 지키는 GC/consent safety의 Before GREEN을 RED로 날조하지 않고 기존 known-gap cfg도 보존한다. O의 floor/init/capability/rollback 순서는 WriterAdoption의 lifecycle projection 또는 같은 조각의 작은 cfg에 추가하고 floor 없이 child init·O-a로 rollback을 금지하는 invariant를 검증한다. 최소한 `surface={channel,thread}`, `parentResolution={notThread,recorded,legacyUnresolved,verified,unknown}`, `policyHost={T,H}`, `actualHost={T,H,unknown}`, `inputSource={discord,imessage,terminal,genericStart}`와 기존 `originClass={human,machine}`, `writerStart={gateway,execution,cleanup/resume,threadAdmission}`, `stage={cold,restoring,ready,held,restarting,retirementPending,dormant}`를 각 해당 기존 모델에 필요한 만큼만 넣는다. 단순히 모든 변수를 한 새 거대 모델에 넣지 않는다.

재시작·부팅·incarnation 경로는 `session_runtime.rs:505` thread bootstrap, `runtime_bootstrap/owner_execution.rs:70` boot, `:233` refresh, `o_writer_host.rs:484` gateway spawn·`:796` execution spawn·`:819` resume, `recovery_engine/restore_inflight.rs:677,722,949`, `commands/restart.rs:216`를 전이로 대응한다. 같은 상태 소비자는 stop/cancel/terminate/clear/reset/resume/idle cleanup이며 §1 행을 빠뜨리지 않는다. thread policy가 부모를 가리킨다는 이유로 이 전이의 actual owner가 부모가 되는 모델은 잘못이다.

ExecutionAuthority의 같은 tick에 ResolveLegacyChild를 연결하고 일시 실패 후 eventual 성공 공정성을 명시한다. HostedCleanup의 은퇴 projection은 delete→new input/rebind→late marker, I/O 실패·crash/boot·next input 재활성화를 넣고 `Dormant => exactDelete && sameIdentity && writerStopped && debt0`, HistoryBytesPreserved·DormantNeverLegacy를 확인한다. 은퇴 진행성은 exact delete 근거 보존·새 input 없음·부채 eventual 정산·정상 stop·durable write eventual 성공 조건 아래에서만 주장한다.

각 구현 레인은 0단계로 해당 기존 model/cfg/view를 수정하고 **Before RED → After GREEN → 원인 제거 mutant RED**를 기록한다. invariant와 PROPERTY liveness 실행을 구분하고 코드 SHA·상태 수·violated rule·공정성 가정·실제 검사한 cfg를 결과 표에 적는다. liveness cfg에는 실제 SPECIFICATION/fairness를 적용하고 SYMMETRY를 같이 쓰지 않는다(`README.md:35-51`). known-gap cfg는 기대 결과를 보존한다. 반례는 코드 경로/격리 시험으로 단계별 replay하고 증명 안 된 반례를 제품 결함으로 보고하지 않는다.

현재 실행 표: TLC 실행 수 **0**, Rust 시험 실행 수 **0**, 모델 편집 **0**. 모델 변경이 필요한 조각은 O-a/O-b/H/T/R 및 상태를 바꾸는 M/S/E다. read-only D와 scanner A/B는 상태 전이를 바꾸지 않아 source contract/정적 fixture로 검증한다. D에 상태 effect를 추가하려면 다시 모델 delta 대상이다.

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

r2 처분은 코디네이터 지시로 확정되어 추가 제품 결정은 없다. P1-1·새 방향·P2-1~9를 모두 반영했다. O-a에서 capability true를 조기 발행하는 안은 rollback의 의미를 깨므로 P2-9 허용 범위 안에서 true 발행만 O-b 활성화 경계로 옮겼다. store 은퇴는 물리 삭제가 아닌 exact child의 논리 은퇴로 한정했다. O-a가 출력 외부 효과를 켜지 않아도 부팅/rollback 변경의 P1은 계속 차단하며 리뷰 기준을 낮추지 않는다.

기술 입력 잔여는 사용자 제품 결정과 다르다: #8114 Slice2 포함 배포 SHA, #8119 d2 해당 조각 착지, #8115 no-session 배포·R 발사 전 terminate switch 값/시각을 코디네이터가 확인해야 한다. i8114-2·i8119-d2 ref는 위 SHA로 직접 읽어 누락을 해소했다. 이 레인은 운영 설정·DB·store·dcserver·tmux·herdr를 확인하지 않았으므로 배포 여부를 추정하지 않는다.

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


### 부록 A2 — r2 추가 접근자·출력 소비·thread GC

r1의 네 패턴 254줄은 바이트 동일하게 보존한다. 아래 첫 블록은 같은 소스 SHA의 `git grep -n -E 'configured_channel_ids\(|with_boot\(|boot\.channels\(' -- '*.rs'` 추가 원시 검색이다. 정의·시험도 포함하며 production baseline 수치가 아니다. boot endpoint inventory를 읽는 내부 owner와 child 정책 소비자를 §2에서 구별했다. with_boot 전체를 ‘5파일’로 잘라 세지 않는다. **5파일은 두 번째 블록의 O snapshot.channels() 직접 소비자**이며 모두 shared child view 이관 대상이다. 출력 판정 전수는 기존 census CENSUS/EXPECTED_GATES를 정본으로 사용한다. 마지막 블록은 별도 thread_gc 경계다.

추가 host/boot 접근자 (`logs/r2-extra-host-accessors.txt`):

```text
src/cli/doctor/orchestrator/managed_herdr_config.rs:19:    with_boot(|boot| boot.is_some())
src/cli/doctor/orchestrator/managed_herdr_config.rs:30:    if let Some(boot) = config::session_hosts::with_boot(|boot| boot.cloned()) {
src/config/session_hosts.rs:350:    with_boot(|boot| boot.and_then(|boot| boot.herdr_endpoint(channel)).cloned())
src/config/session_hosts.rs:354:pub(crate) fn configured_channel_ids() -> Vec<String> {
src/config/session_hosts.rs:355:    let ids = |boot: &BootSessionHosts| boot.channels().keys().map(u64::to_string).collect();
src/config/session_hosts.rs:356:    with_boot(|boot| boot.map_or_else(Vec::new, ids))
src/config/session_hosts.rs:360:    with_boot(|boot| {
src/config/session_hosts/boot_pg_tests.rs:187:            session_hosts::with_boot(|boot| {
src/config/session_hosts/boot_pg_tests.rs:222:        session_hosts::with_boot(|boot| {
src/config/session_hosts/boot_pg_tests.rs:225:                boot.channels(),
src/config/session_hosts/migration/consent_holds_tests.rs:61:    session_hosts::with_boot(|boot| boot.unwrap().clone())
src/config/session_hosts/migration/consent_returns_tests.rs:39:    let mut hosts = crate::config::session_hosts::with_boot(|boot| boot.unwrap().clone());
src/config/session_hosts_tests.rs:45:        boot.channels().keys().copied().collect::<Vec<_>>(),
src/config/session_hosts_tests.rs:94:    assert!(boot.channels().is_empty());
src/config/writer_channels_tests.rs:172:        boot.channels().iter().copied().collect::<Vec<_>>(),
src/config/writer_channels_tests.rs:319:    let channels: Vec<u64> = boot.channels().iter().copied().collect();
src/db/dispatched_sessions.rs:2212:    .bind(crate::config::session_hosts::configured_channel_ids())
src/db/dispatched_sessions.rs:2249:        .bind(crate::config::session_hosts::configured_channel_ids())
src/server/routes/health_api/runtime_profile.rs:20:            let channels: Vec<String> = boot.channels().keys().map(u64::to_string).collect();
src/server/routes/health_api/runtime_profile.rs:61:        with_boot(|boot| match boot {
src/server/routes/health_api/runtime_profile.rs:143:        live.session_hosts = with_boot(|b| b.unwrap().config().clone());
src/server/routes/health_api/runtime_profile.rs:173:        booted.session_hosts = with_boot(|boot| boot.unwrap().config().clone());
src/server/routes/health_api/runtime_profile/managed_herdr_tests.rs:94:    live.session_hosts = with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/commands/control/codex_clear_relaunch_tests.rs:182:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/commands/control/codex_clear_retirement_tests.rs:372:                crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/router/herdr_consent.rs:32:    session_hosts::with_boot(|boot| {
src/services/discord/router/herdr_consent.rs:395:    let enabled = session_hosts::with_boot(|boot| {
src/services/discord/router/herdr_consent/admission_tests.rs:1851:    let boot = session_hosts::with_boot(|boot| boot.cloned()).unwrap();
src/services/discord/router/herdr_consent/return_input.rs:171:        let approved = || session_hosts::with_boot(|boot| binding_digest(boot?.config(), channel));
src/services/discord/router/herdr_consent/return_input_tests.rs:272:    let hosts = session_hosts::with_boot(|boot| boot.unwrap().clone());
src/services/discord/router/herdr_consent/startup.rs:27:    session_hosts::with_boot(|boot| {
src/services/discord/router/message_handler/codex_resume_companion_queue_tests.rs:65:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/router/message_handler/provider_dispatch_codex_t14_actual_tests.rs:107:    assert!(crate::config::session_hosts::with_boot(
src/services/discord/router/message_handler/provider_dispatch_codex_t14_composed_tests.rs:96:        crate::config::session_hosts::with_boot(|boot| boot == Some(&expected)),
src/services/discord/router/message_handler/root_replaced_recovery_tests.rs:57:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/tui_prompt_relay/relay_e2e/prompt_identity_e2e_tests/herdr_direct_roundtrip_tests.rs:461:        crate::config::session_hosts::with_boot(|boot| boot == Some(&disk_boot)),
src/services/discord/turn_presence/activity/unbound/clear_owner_race_tests.rs:694:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/turn_presence/activity/unbound/collection_publication_tests.rs:37:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/turn_presence/activity/unbound/herdr_bound_tests.rs:117:        crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/turn_presence/activity/unbound/stale_binding_tests.rs:54:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/turn_presence/activity/unbound/stale_binding_tests.rs:655:                    crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/discord/watchers/lifecycle/resume_companion_restore_tests.rs:719:            crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/session_host/herdr/managed_config/maintenance.rs:279:    if let Some(boot) = with_boot(|boot| boot.cloned()) {
src/services/session_host/herdr/managed_config/maintenance.rs:286:    let Some(boot) = with_boot(|boot| boot.cloned()) else {
src/services/session_host/herdr/managed_config_real_tests.rs:157:    crate::config::session_hosts::with_boot(|boot| {
src/services/session_host/herdr/policy/diagnostics.rs:213:    with_boot(|boot| checker_evidence(boot.map_or(&config.session_hosts, |boot| boot.config())))
src/services/session_host/herdr/policy/diagnostics_tests.rs:110:    config.session_hosts = session_hosts::with_boot(|boot| boot.unwrap().config().clone());
src/services/session_host/herdr_gate/missing_marker_rig_tests.rs:107:                crate::config::session_hosts::with_boot(|boot| HerdrRegistry::build(boot.unwrap()));
src/services/session_host/herdr_gate_tests.rs:763:        crate::config::session_hosts::with_boot(|boot| HerdrRegistry::build(boot.unwrap()))
src/services/session_host/herdr_registry.rs:436:    crate::config::session_hosts::with_boot(|boot| match boot {
```

O 직접 channels() 5파일 (`logs/r2-o-direct-channels.txt`):

```text
src/services/discord/runtime_bootstrap/o_writer_host.rs:447:                        .channels()
src/services/discord/runtime_bootstrap/owner_execution.rs:80:        .channels()
src/services/discord/runtime_bootstrap/owner_execution.rs:157:    for &channel in snapshot.channels() {
src/services/tui_o/cutover.rs:35:        let channels = snapshot.channels();
src/services/tui_o/cutover.rs:59:        snapshot.channels().iter().filter_map(owned).collect()
src/services/tui_o/cutover.rs:73:    let hosted = channel_policy::owns_output(snapshot.channels(), channel, kind);
src/services/tui_o/cutover.rs:132:        let Some(snapshot) = snapshot.filter(|s| !s.channels().is_empty()) else {
src/services/tui_o/cutover.rs:136:        if channel.is_some_and(|channel| !snapshot.channels().contains(&channel)) {
src/services/tui_o/cutover/channel_gate.rs:442:    if snapshot.channels().is_empty() {
src/services/tui_o/cutover/channel_gate.rs:449:    if !snapshot.channels().contains(&channel_id) {
src/services/tui_o/topology.rs:35:        .channels()
```

thread_gc 서비스 진입·probe·delete (`logs/r2-thread-gc-sites.txt`):

```text
src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:5:    delete_stale_thread_cleanup_row_pg, load_stale_thread_cleanup_rows_pg,
src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:10:    gc_stale_thread_sessions_with_probe_pg(pool, probe_locally_owned_tmux).await
src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:13:async fn probe_locally_owned_tmux(key: String) -> SessionPresence {
src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:54:        if !crate::services::tmux_turn_liveness::cleanup_row_is_unoccupied_pg(pool, &key, &row)
src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:59:        if probe(key.clone()).await != SessionPresence::Missing {
src/services/dispatched_sessions/hosted_cleanup/thread_gc.rs:63:        if delete_stale_thread_cleanup_row_pg(pool, &row)
```
