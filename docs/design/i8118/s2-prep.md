# #8118 S2 설계 준비 — Herdr 부모 채널 아래 스레드 세션의 Herdr 배치

- 기준: `origin/main` = `b04ea56650e5397989d33f1be199be21a83c8d0e`. 이 문서와 상태 차트 말고는 코드 변경이 없다.
- 이 문서는 설계 본문이 아니다. 해법은 고르지 않고 코드 좌표·상태 모델·분류표·질문만 적는다. 설계 본문은 WebGPT pro가 쓴다.
- 경로 표기: `src/services/` 아래는 접두를 생략한다(`discord/…`는 `src/services/discord/…`). 그 밖의 경로는 `src/`부터 적는다.
- 앞 설계: `design-hh-3`(개정 3), `design-hh-4`(델타 4: split-1121 = B, S3 기각, 활성화를 S2로 옮김), `design-hh-5`(S1b 델타 5). 셋 다 lane-runs 사본이고 저장소에는 없다.
- 드리프트: 앞 설계의 기준 `d6f5aa7b7a`에서 지금까지 S2 관련 파일 219개가 바뀌었다(+11764/−2376). `first_input.rs`, `handoff.rs`, `herdr_launch.rs`, `launch_host.rs`, `unplaced.rs`, `claude_thread.rs`만 해도 +311/−281이다. 그러니 개정 3·델타 4의 줄 번호는 그대로 믿지 말고 다시 확인해야 한다. Herdr 클라이언트에는 아직 `tab.create`가 없다(`session_host/herdr/model.rs:238` `WorkspaceCreate`만 있음).

## 1. 범위 표

### 1.1 S1a·S1b가 main에 착지한 범위

| 조각 | main 커밋 | 착지 묶음 | 바꾼 것 |
|---|---|---|---|
| S1a | `6f077a6f7d`, `2668bdeefd` | `ea778458ff` (#8315 batch-1121c) | 새 `session_host/thread_host.rs`(`parent_policy`, `left_herdr`, `current_execution_cached`, `JUDGED`, `next_host`, `runs_herdr`). 입력·출력 동기 소비자 16곳이 이 함수들로 넘어갔다. `turn_host.rs` `for_turn`이 `record_judged`를 부른다 |
| S1b | `27c4026025`, `64dbeade52`, `59a87ed6e4`, `07bb7e7636`, `9e28c137ae`, `436601ab62`, `408eb510ed` | `4f30b65cce` (#8334 batch-1121g) | 새 `session_host/host_decision.rs`(행 + tmux 관측으로 Herdr/Tmux/Hold 판정). A1~A9(재시작·reset·locator·복구 정리·idle·watcher kill·슬롯 리셋), R1~R4 relaunch, `/fast`·`/effort`·설정 owner, W2 입력 동의가 이 판정을 쓴다. `recorded_execution`은 거절된 턴도 검증된 Bound면 기억한다. 상태 차트 `ThreadHostS1b` |

S1a·S1b는 소비자만 바꿨다. **Herdr 부모 아래 스레드의 새 실행을 Herdr로 판정하는 경로는 main에 아직 없다.** `turn_host.rs:239-242`가 Bound 행이 없는 스레드를 모두 Tmux로 보낸다(§5).

### 1.2 DoD별 대조

| DoD | 내용 | S1a·S1b 뒤 상태 | S2 몫 | S2 밖 |
|---|---|---|---|---|
| 1 | Herdr 채널 아래 새 스레드 세션이 Herdr로 뜨고, 입력·출력·취소가 된다(배포 뒤 실제 스레드로 확인) | 미충족. 11-26 18:12 재현(§5) | **전부.** `next_host` 본문, `turn_host` 스레드 분기, 첫 launch 허가, 부모 workspace 배치(`tab.create`), Pending 복구, tmux 생성 가드, 첫 입력 1회 | 배포 뒤 실기 확인(배포 수용 레그) |
| 2 | tmux 채널 아래 스레드는 지금처럼 tmux로 뜬다 | 충족(`parent_policy` Tmux → `turn_host.rs:248-250`) | 회귀 0만 지킨다(모델 `TmuxParentStaysTmux`). 기존 tmux 스레드(selector 있음)도 tmux로 둔다(`ExistingTmuxStaysTmux`) | — |
| 3 | tui_o 채널 아래 스레드에서 턴 10회: 남은 자리표시 0, 답장 형태 0, 중복 0, 푸터 사용량, 예전 경로 본문 0 | 출력 조각 O-a·O-b·reentry(#8239 lookup) 착지. Herdr 자식이 없어서 Herdr 쪽 출력은 검증되지 않았다 | 첫 Herdr 보호 턴의 출력 보호(`source_policy` 고정, 첫 키 전 fence)와 자리표시 정산(개정 3 §2.2-2·5, §2.3 [U3]). 푸터 사용량은 자식 native session 기록에서 나온다 | 턴 10회 실측은 배포 수용 레그. 루틴 process 스레드 kind는 S1r |
| 4 | #7651 census에서 스레드 채널의 예전 경로 전송이 더 늘지 않는다 | 측정 안 함 | 새 Herdr 자식이 예전 경로로 본문을 보내지 않는다(`ChildDeliveryHeld`, 보호 source) | census 측정은 배포 수용 레그 |
| 5 | 호스트 판정을 우회하는 호출부 0(전수표) | S1b가 A1~A9·R·W2를 옮겼다. boot 원시 읽기 31곳은 접근자 기준선에 남았다 | §3 표의 [C](own boot만 읽음)·[F](tmux 생성 가드) 행이 S2 판정과 같은 답을 내게 하거나, 남기는 근거를 적는다 | Codex 자식은 S4 |

### 1.3 S2 정의(델타 4 조각표 + 노드 evidence)

- S2 = 부모 workspace tab 배치 + Pending 복구 + 새 스레드 활성화. 활성화는 S2 한 조각에서만 한다. 델타 4 추정은 prod 약 21~25파일 / +830~1130줄이다. +1200을 넘으면 S2a(배치)·S2b(활성화)로 나누는데, 그러면 S2a 배치 코드에는 생산 호출자가 없게 되므로 코디네이터가 정해야 한다.
- **첫 커밋 계약**(노드 evidence 1126):
  - S1b pro P2-1: `/clear`·`!clear`가 Herdr 자식의 Pending·Retired·Hold에서 `end_herdr`·selector 삭제·경고를 건너뛴다(`discord/commands/control/clear_teardown.rs:72-73,148`, `resume_fence.rs:23-24`).
  - S1b pro P2-2: `recorded_execution`이 검증한 Bound 증거를 받지 않는다(`turn_host.rs:171-187`). 그래서 첫 턴이 거절된 새 스레드가 `Some(true)`로 기록된다.
  - 1121i P2-1: `/effort`의 `herdr_codex`(`discord/commands/config/effort.rs:146-155`, `has_herdr_endpoint` = 자기 boot)를 `herdr_now`(`:135-142`, `host_decision`) 기준으로 바꾼다. 바꾸지 않으면 Codex Herdr 자식이 `:202`에서 거절된다.

## 2. 상태 차트(0단계)

- 기존 모델 `docs/statecharts/thread-host/ThreadHostS1b.tla`에 **S2 배치 절**(`SpecS2`)을 더했다. S1 부분의 동작은 바꾸지 않았다. 기존 19개 cfg는 결과가 그대로이고, 새 cfg 10개와 합쳐 29/29가 `expected.tsv`와 같다.
- 모드:
  - `Main`: b04ea56650.
  - `S2Naive`: 활성화만 한 경우다. `turn_host`가 selector 없는 스레드를 `configured_turn`으로 보내고, 허가·boot 복구·idle 정리는 main 그대로다.
  - `S2`: 설계가 지킬 계약이다.
  - `MutS2*`: S2에서 규칙 하나를 지운 것이다.
- 입구 4종(§3 표의 행에서 왔다):
  - `NewThread`: 행 없음, selector 없음.
  - `FreshAfterKill`: Legacy 행, tmux 죽음, selector 지움. 18:12 증상이다.
  - `ResumeTmux`: Legacy 행, tmux 죽음, selector 있음.
  - `TmuxParentThread`: tmux 부모 아래 스레드.
- `exec` 값은 §3의 행 상태에서 왔다: 없음, LegacyDead, Tmux, Pending(`herdr_launch.rs:550` → `src/db/dispatched_sessions/hosted_execution.rs:419`), Bound, Retired, Gone(thread_gc 삭제).

| 규칙 | 뜻 |
|---|---|
| `NewThreadFollowsParent` | Herdr 부모 아래 새 실행은 tmux 세션을 0개 만든다. 재시작·thread_gc·슬롯 리셋 뒤 다시 뜰 때도 같다 |
| `ExistingTmuxStaysTmux` / `TmuxParentStaysTmux` | selector가 있는 기존 tmux 스레드와 tmux 부모 아래 스레드는 Herdr pane을 0개 만든다 |
| `NoInputRefused` | 첫 입력이 거절되지 않는다(유실 0) |
| `SingleSubmit` | 첫 입력을 한 번만 제출한다(중복 0) |
| `NoDuplicatePane` | 스레드당 pane은 1개 이하다 |
| `FirstInputResolves` (liveness) | 첫 입력은 결국 배치되어 제출된다 |
| `EventuallyReleased` (liveness) | idle 실행은 결국 해제된다 |
| `SlotRotates` (liveness) | idle 슬롯 스레드는 결국 리셋된다 |

| cfg | 기대 | 결과 |
|---|---|---|
| `S2MainFresh` | RED(증상 재현) | `NewThreadFollowsParent`. 1단계: Legacy 행 → tmux launch(§5 경로) |
| `S2MainAll` | RED | `NewThreadFollowsParent`. 나머지 GREEN |
| `S2NaiveAll` | RED | `NoInputRefused`(허가 없이 `OWriterNotReady`), `FirstInputResolves`, `EventuallyReleased`·`SlotRotates`(idle Herdr 자식이 해제되지 않고 슬롯 리셋도 거절됨) |
| `S2All` | GREEN | GREEN(9개 규칙) |
| `MutS2NoPermit` | RED | `NoInputRefused`, `FirstInputResolves` |
| `MutS2NoBootResolve` | RED | `NoInputRefused`, `FirstInputResolves`. 3단계: Pending → 재시작 → 재생 입력이 `turn_host.rs:239-241`에 막힌다 |
| `MutS2NoRelease` | RED | `EventuallyReleased`, `SlotRotates` |
| `MutS2TabNoMarker` | RED | `NoDuplicatePane`. `FirstInputResolves`는 pane 2개 상한에서 멈춰서 생긴 것이다(유한화 결과) |
| `MutS2SelectorIgnored` | RED | `ExistingTmuxStaysTmux` |
| `MutS2ReplayResubmit` | RED | `SingleSubmit`. 이건 계약 뮤턴트이고, 재생 경로가 실제로 다시 제출하는지는 [U]다(Q12) |

자세한 결과는 `docs/statecharts/thread-host/thread-host-s1b.report.md`의 S2 절에 있다.

## 3. 진입점 전수표

- 범위: 스레드 세션의 호스트를 정하거나 바꾸거나 소비하는 경로 전부.
- `git grep` 결과를 정리했고, 핵심 좌표는 `sed`로 직접 대조했다.
- "S2 뒤 기대" 기호:
  - **A**: 활성화로 답이 바뀐다. 새 스레드(selector 없음)는 Herdr다.
  - **B**: 행·턴 사실로 판정하므로 그대로다.
  - **C**: 자기 boot만 읽는다. 그래서 Herdr 자식에게 틀린 답을 낸다. S2가 바꾸거나, 남기는 근거를 적어야 한다.
  - **D**: `JUDGED`(프로세스 메모리)를 읽는다. 재시작 뒤 seed가 필요하다(S1b `RestoredBeforeRead`).
  - **E**: 판정이 없다. 받은 endpoint를 쓴다.
  - **F**: tmux 생성 가드다. `next_host` = Herdr이면 거절해야 한다(델타 4 #46-48).
- "모델" 열은 `ThreadHostS1b`의 행동이다(S1 = 기존 S1b 부분, S2 = 새 절).

| # | 범주 | file:line | 함수 | 현재 판정 | Herdr 부모 아래 스레드, 지금 답 | S2 뒤 기대 | 모델 |
|---|---|---|---|---|---|---|---|
| E1 | 핵심 | `session_host/thread_host.rs:24` | `parent_policy` | 자기 boot, 없으면 `relation` → 부모 boot | Child면 `Herdr{via_parent}`, 기록 안 된 스레드는 NotChild → Tmux | B. E43 기록 의존은 Q14 | S2 입구 구분 |
| E2 | 핵심 | `thread_host.rs:48` | `left_herdr` | `parent_policy`(Unknown → false) | false | B | S1(journal) |
| E3 | 핵심 | `thread_host.rs:87,90,103` | `JUDGED`/`record_judged`/`current_execution_cached` | 판정 기록(쓰는 곳은 `turn_host.rs:165` 하나) | 미판정·재시작 뒤 Unknown | D: boot seed(Q10) | S1 `Restart`/`BootRestore` |
| E4 | 핵심 | `thread_host.rs:137,153,158` | `next_host`/`next_host_cached`/`next_is_herdr` | 부모 Herdr ∧ 판정 Herdr | 판정된 Herdr일 때만 Herdr | **A: 본문을 바꾼다**(selector·실행 부재 입력, 델타 4 ①②) | S2 `LaunchHost` |
| E5 | 핵심 | `thread_host.rs:163` | `runs_herdr` | `current_execution_cached` | 판정된 Herdr일 때만 | D | S1 `SyncRead` |
| E6 | 핵심 | `turn_host.rs:157,165,171-187` | `for_turn`/`recorded_execution` | `resolve(execution=true)` → 기록 | Bound 일치면 Herdr/거절, 그 밖 Tmux | A + P2-2(검증 증거) | S2 `Launch`, S1 `Turn` |
| E7 | 핵심 | `turn_host.rs:191` | `for_observation` | `resolve(false)`, 기록 안 함 | E6과 같음 | A(같은 판정) | — |
| E8 | 핵심 | `turn_host.rs:200-252` | `resolve` | 자기 boot → configured. 스레드: `is_live_thread_endpoint`(`:255`) Bound만 configured. 행 오류 → 거절. Herdr 흔적 → `HostedHerdrUnconfigured`(`:239-241`). 그 밖 **Tmux(`:242`)** | Tmux | **A: 판정 지점**(§5) | S2 `Launch`, `Rejudge` |
| E9 | 핵심 | `turn_host.rs:282,306` | `refusal_before_turn`/`intake_refusal_before_turn` | 부모 정책 Tmux면 통과, 아니면 `for_turn` | Bound 없음 → 통과 | A. 사전 판정은 허가를 소비하지 않는다(개정 3 I6) | S2 `Launch` |
| E10 | 핵심 | `turn_host.rs:412-466` | `configured_plan` | provider 스위치(`:423`), 로컬 노드(`:431`), admission(`:445`), `o_writer_ready`(`:446`) | 도달 안 함 | A: late 자식 첫 launch 허가(개정 3 §2.1) | S2 `PermitMissing` |
| E11 | 핵심 | `session_host/host_decision.rs:75,86,105,151,183,200-202` | `host_decision*`/`decide` | 행 + Bound 일치 + tmux probe. **실행 없음 → Tmux(`:200-202`)** | Bound 일치 → Herdr. Pending·불일치 → Hold. 없음 → Tmux | A: `Observed::Nothing`을 `next_host`에 연결(S1b 인계 3) | S1 `Decision`, S2 `SlotReset` |
| E12 | 핵심 | `discord/host_defer_gate.rs:109,298,309` | `returning_session_key`/`host_decision_of`/`configured_refusal` | E6/E11 | E11 | B | S1 |
| E13 | 첫 턴 | `discord/router/message_handler/intake_turn/host_refusal.rs:19` | `admitted_owner_uploads` | `intake_refusal_before_turn` | 통과 | A | S2 `Launch` |
| E14 | 첫 턴 | `intake_turn.rs:1940` | `handle_text_message` | `for_turn` | **Tmux** | A | S2 `Launch` |
| E15 | 첫 턴 | `intake_turn.rs:2097` → `discord/turn_teardown_clearance.rs:11` → `discord/inflight/host_recovery_guard.rs:94-110` | 정리 허가 | `host_decision_for_key` → keyed gate | Tmux | B | S1 `CmdTmuxOp` |
| E16 | 첫 턴 | `discord/router/message_handler/provider_dispatch.rs:360-371` | `drive` | 받은 `TurnHost` 그대로 | tmux driver | B(Herdr면 `herdr_turn` `:574`) | S2 `Launch` |
| E17 | tmux 가드 | `claude/tui_session_launch.rs:91-94,106-108` | `prepare_and_create_claude_tui_session` | `herdr_configured_for_tui_launch`(`herdr_launch.rs:64,75` 자기 boot) | false → tmux 생성 | **F** | S2 `Launch`(Tmux 분기) |
| E18 | tmux 가드 | `claude.rs:2602` | `execute_streaming_local_tmux` | 같음 | false | F | 〃 |
| E19 | tmux 가드 | `codex.rs:1435,1536`; `codex/absence_launch.rs:80,297`; `codex/tui_session_launch/first_input.rs:265`; `codex/tui_session_launch/launch_policy.rs:70` | Codex launch | 같음 | false | F(Codex: Q9) | 모델 밖 |
| E20 | tmux 가드 | `discord/codex_absence.rs:31` | `local_tui` | 같음 | 로컬 TUI | C(Codex) | 모델 밖 |
| E21 | headless | `discord/router/message_handler/headless_turn/admission.rs:48,51`; `headless_turn.rs:993` | 시작 판정 | `(intake_)refusal_before_turn`, 그다음 `for_turn` | Tmux | A | S2 `Launch` |
| E22 | owner 입력 | `discord/queue_io/input_owner/claude_handoff.rs:261-286` | `judge` | 스위치 ∧ `next_is_herdr` → `for_turn` | 미판정 → handoff 없음 | D | S1 |
| E23 | owner 입력 | `claude_handoff.rs:64,290,530` | `prepare`/`recover` | `left_herdr` | false(journal 유지) | B | S1 |
| E24 | owner busy | `intake_turn/owned_busy.rs:119,285,357` | busy 판정 | `for_turn` | Bound 일치면 Herdr | A(같은 판정) | — |
| E25 | owner 입력 | `provider_dispatch/owner_input.rs:417,639,1159` | `hand_busy`/`into_busy_pane`/`bootstrap_probe` | `for_turn`/`held_look` | 〃 | A | — |
| E26 | owner 시작 | `provider_dispatch/owner_input_start_wait.rs:50` | `observe` | `for_observation` | 〃 | A | — |
| E27 | followup | `discord/router/message_handler/tui_followup/nameless_gate.rs:79,180` | promote 판정 | `runs_herdr`; `for_turn == Return` | 판정된 Herdr일 때만 probe 생략 | D | S1 `SyncRead` |
| E28 | 재개 | `discord/router/message_handler/unverified_thread_resume.rs:43-46` | `replaces` | `for_observation == Tmux` ∧ pane 죽음 | Tmux | A | — |
| E29 | HTTP | `discord/health/thread_turn.rs:148` | `check_live` | `for_turn` + 행 endpoint 대조 | Bound면 Herdr | B | — |
| E30 | HTTP | `discord/health/turn_deliver/preflight.rs:20,29` | `host` | `intake_refusal_before_turn` → `runs_herdr` | E9 → JUDGED | A/D | — |
| E31 | HTTP | `turn_deliver/inject.rs:312,440`; `inject/herdr.rs:48` | `turn_mode_tmux`/`resolve`/`bound_pane` | `runs_herdr` + 행 | JUDGED | D | S1 `SyncRead` |
| E32 | HTTP | `src/server/routes/agents_turn_deliver/origin_admission.rs:19` | admission | thread 인자 또는 `has_herdr_endpoint`(자기) | — | C | 모델 밖 |
| E33 | 입력 동의 | `discord/router/herdr_consent/admission.rs:31-49` | `check` | Child → `endpoint(parent)` + `input_runs_herdr` | Bound일 때만 동의 대상 | A: 새 자식 입력의 부모 proof 대기(델타 5 W2-3·W2-5) | S1 `Input` |
| E34 | 입력 동의 | `discord/router/herdr_consent.rs:31`(호출 `:133,:216`, `return_input.rs:14,31`) | `endpoint` | 자기 boot(user_default) | None | C | 모델 밖 |
| E35 | intake | `discord/router/intake_dispatch.rs:587,617,896` | checkpoint 대기 | `herdr_consent::endpoint`(자기) | false | C | 모델 밖 |
| E36 | busy 주입 | `discord/router/intake_gate/busy_inject.rs:86-92` | `covers` | 자기 또는 Discord 부모가 boot Herdr이면 false | **실제가 tmux여도 false** | C(현재 실행 기준으로 바꿀지: Q8) | 모델 밖 |
| E37 | busy 주입 | `busy_inject.rs:103-109`(호출 `intake_turn.rs:1123`) | `defer_owned_busy_view` | `!next_is_herdr` | 미판정 → tmux 쪽 | D | S1 `SyncRead` |
| E38 | busy 재시도 | `discord/router/message_handler/busy_retry.rs:251,269` → `busy_inject.rs:95` | `queued_head_covers` | 판정 없음 | — | E | — |
| E39 | 세션 생성 | `discord/session_runtime.rs:415-418` | `bootstrap_thread_session` | 스레드 자신의 boot | None → tmux 쪽 세션·worktree 준비 | A? 새 Herdr 자식의 worktree·행 준비(Q14) | S2 입구 |
| E40 | 관계 기록 | `discord/router/intake_gate.rs:230,239-246`; `session_runtime.rs:426`; `intake_turn/adk_thread.rs:246`; `src/server/routes/discord.rs:125`; `discord/recovery_engine/manual_rebind/mod.rs:298`; `tui_o/cutover/claude_thread.rs:307` → `tui_o/cutover.rs:166,175` | `record_thread_child(_by_id)` | 부모가 O 선택 TUI 채널이어야 Child | E1·E8·E11의 Child 판정 원천 | B/Q14 | S2 입구 |
| E41 | 정책 채널 | `tui_o/channel_policy/children.rs:476`; `tui_o/cutover.rs:276`; `session_host/surface_policy.rs:12,41` | `policy_channel`/`relation` | child store | 부모 | B | — |
| E42 | driver | `provider_hosting.rs:247` | driver 선택 | `policy_channel` → 부모 hosting | 부모 설정 | B | — |
| E43 | /clear | `discord/commands/control.rs:585` → `control/herdr_recovery.rs:30` | `herdr_recovery::clear` | `parent_policy` Herdr → 스레드 정규 행 | 진입 | B | — |
| E44 | /clear | `control.rs:607` → `control/clear_teardown/resume_fence.rs:23-24` | `set_channel` | Claude ∧ `host_decision_of.is_herdr` | Bound일 때만 fence | **P2-1** | S1 `CmdTmuxOp` |
| E45 | /clear | `control.rs:654` → `control/clear_teardown.rs:72-73`(`end_herdr` `:148`) | `end_session` | `is_herdr` → `end_herdr`, 아니면 tmux | Pending·Retired·Hold에서 건너뜀 | **P2-1** | 〃 |
| E46 | /clear | `clear_teardown.rs:126` | canary 분기 | 자기 boot + canary | 해당 없음 | B | — |
| E47 | /clear | `discord/commands/control/clear_report.rs:147` | `settle_consents` | `herdr_consent::endpoint`(자기) | 정산 안 함 | C | 모델 밖 |
| E48 | /clear 경로 | `control/clear_teardown/execution_cleanup.rs:272` | `normal_end` | `host_decision_of` | E11 | B | S1 |
| E49 | relaunch | `control.rs:245-246` → `discord/commands/config/provider_settings.rs:211` | `pending_relaunch` | `host_decision_of` | E11 | B | S1 `CmdRelaunch` |
| E50 | relaunch | `control.rs:396` | `relaunch_on_fast_drift` | `host_decision_of == Tmux` | E11 | B | S1 `CmdRelaunch` |
| E51 | /restart | `discord/commands/restart.rs:291`(tmux `:228`) | `restart_channel` | `host_decision`: Hold 거절, Herdr → `restart/herdr.rs` run, Tmux → reset | 실행 없음 → Tmux | A: 실행 없는 스레드의 /restart(E11 연결) | S1 `CmdRestart` |
| E52 | reset 가드 | `discord/admin_host_guard.rs:96,110,121,140`(호출: `commands/control/provider_reset.rs:16`, `tmux_recreate.rs:104`, `restart.rs:228`, `health/recovery.rs:269`, `health/managed_reset_port.rs:17`) | `managed_reset_refusal`/`reset_refusal` | `configured_refusal` → keyed gate | Herdr/Hold 거절 | B | S1 `CmdTmuxOp` |
| E53 | 가드 | `admin_host_guard.rs:18,29,289,322-325` | `channel_refusal`/`turn_release_refusal` | hosted 행 + marker + inflight | 행 기준 | B | — |
| E54 | /stop·취소 | `discord/commands/stop.rs:84-91,98,116` → `discord/turn_bridge/tmux_runtime/judged_stop.rs:365,405,436-449,528` | `begin_stop`/`herdr_turn` | 턴 CancelToken의 Herdr 상태(spawn 때 정해짐) | 턴 사실 | B(Herdr 자식 취소 실기는 DoD 1) | 모델 밖 |
| E55 | terminate | `discord/herdr_terminate/root_replaced.rs:249-262` | `local` | `parent_policy` endpoint ↔ 행 위치 | 부모 기준 | B | — |
| E56 | terminate | `discord/herdr_terminate/root_replaced/preclaim.rs:24,50` | `before_idle` | `:24` `parent_policy`, `:50` `herdr_server_replaced::local_endpoint`(자기) | `:50` None → server_replaced=false | C | 모델 밖 |
| E57 | terminate | `discord/herdr_terminate.rs:193,535,595,679`; `herdr_terminate/restart_recovery.rs:71,121` | probe/terminate | 행 위치 | 행 기준 | B | — |
| E58 | /effort | `discord/commands/config/effort.rs:135-142` | `herdr_now` | `host_decision_of.is_herdr` | E11 | B | — |
| E59 | /effort | `effort.rs:146-155,184-205` | `herdr_codex`/`read_entry_capability` | `has_herdr_endpoint`(자기) | Codex Herdr 자식 → `:202` 거절 | **첫 커밋 계약(1121i P2-1)** | 모델 밖 |
| E60 | /effort | `effort.rs:257-262`; `discord/commands/tui_passthrough.rs:94,98` | `update_channel_effort`/`effort_save_notice` | `local_tui(herdr_now)` ∨ `herdr_codex` | Codex 자식 → not_local | 첫 커밋 계약 | 모델 밖 |
| E61 | /effort·/fast pane | `discord/commands/tui_passthrough/herdr_settings.rs:20-80` | `target` | 행 Bound + 행 위치 | 행 기준 | B | — |
| E62 | /model | `provider_settings.rs:194,237-238,263` | `update_channel_model_override` | `host_decision_of` → Tmux일 때 relaunch | E11 | B | S1 `CmdRelaunch` |
| E63 | 설정 owner | `provider_settings.rs:119-131` | `check_settings_owner` | `host_decision_of`(Herdr → 실행 노드, Hold 거절) | E11 | B | — |
| E64 | /fast | `discord/commands/fast_mode.rs:107-117,150-158`; `provider_settings.rs:321-335` | `fast_save_notice`/`herdr_store_channel`/`update_channel_fast_mode` | `is_herdr`. 저장 채널은 Child → 부모 | E11 | B | — |
| E65 | idle 정리 | `discord/session_idle_cleanup.rs:75-81,87` | 만료 루프 | `configured_refusal`. Herdr/Hold → **건너뜀** | — | A?: idle Herdr 자식 해제 경로(Q6) | S2 `ReleaseIdle` |
| E66 | locator | `discord/session_locator.rs:185` | `channel_key` | `configured_refusal` → `observe_old` | E11 | B | S1 |
| E67 | watcher kill | `discord/tmux_watcher/host_gate.rs:153` | `admits_automatic_kill` | `configured_refusal` + teardown | E11 | B | S1 |
| E68 | watcher host | `discord/watchers/lifecycle/watch_host.rs:30-77` | `watch_host_of` | Herdr marker/목록/행 | 행 기준 | B | — |
| E69 | 부팅 복원 | `discord/watchers/lifecycle/restore.rs:829` | `clean_dead_startup_session` | keyed teardown | 행 기준 | B | — |
| E70 | reaper | `discord/tmux_reaper/host_guard.rs:75,92` | 가드 | keyed teardown | 행 기준 | B | — |
| E71 | resume | `session_resume.rs:406-413` | rebind | `host_decision_for_key` → 행 | E11 | B | S1 `CmdTmuxOp` |
| E72 | 재시작 재연결 | `discord/recovery_engine/herdr_reader.rs:184-215` | `reconnect_pass` | 로컬 Herdr 행 | 행 기준 | B + JUDGED seed 후보(Q10) | S1 `BootRestore` |
| E73 | Pending 복구 | `herdr_reader.rs:363-366` | `resolve_unplaced_rows` | `herdr_endpoint(owner.channel_id)`(자기) | **스레드 Pending은 건너뜀** | **A/C: 부모 endpoint로 복구**(Q5) | S2 `Resolve`(boot), `Rejudge` |
| E74 | 서버 교체 | `discord/recovery_engine/herdr_server_replaced.rs:27-44`(호출 `:35,:42,:177`; `recovery_engine/herdr_row_end.rs:171`; `herdr_row_end/owner.rs:67,88,96`; `intake_gate/stale_turn.rs:323`) | `local_endpoint`/`demand`/`ensure_server`/`recover` | `herdr_consent::endpoint`(자기) | demand no-op, hold 없음 | C(Q8) | 모델 밖 |
| E75 | 재시작 probe | `recovery_engine/tmux_probe.rs:127`; `herdr_row_end.rs:78`; `herdr_reader.rs:189` | `hosted_provider` | provider 스위치만 | 채널 무관 | B | — |
| E76 | thread_gc | `dispatched_sessions/hosted_cleanup/thread_gc.rs:50-96`; `src/db/dispatched_sessions/thread_gc.rs:55-75,92-118`; `migrations/postgres/0131_sessions_hosted_execution.sql:7` | `gc_stale_thread_sessions_retiring_pg` | tmux Missing ∧ deletable(NULL 또는 retired) | Bound/Pending 행은 지우지 않음 | B(Retired만 지움) | S2 `Gc` |
| E77 | selector 지움 | `src/db/dispatched_sessions.rs:2086,2123` | `clear_stale_session_id_pg`/`clear_session_id_by_key_pg` | 설정 채널 목록 vs 행 채널 | 행 모양에 따라 다름 | Q2(selector 출처) | 모델 밖 |
| E78 | 슬롯 리셋 | `auto_queue/runtime.rs:389-392,498-507`(호출 `:248`; `dispatches/discord_delivery/transport.rs:648`; `orchestration.rs:758,775`; `auto_queue/control_routes.rs:65`) | `filter_safe_slot_thread_reset_targets` | `host_decision_for_rows` | Bound → Herdr → **건너뜀**, Pending → Hold → 건너뜀 | A?: Bound 자식 슬롯 회전(Q6) | S2 `SlotReset` |
| E79 | 슬롯 리셋 | `auto_queue/runtime.rs:555-590` → `host_teardown_gate.rs:223` | `slot_thread_host_refusal` | keyed row gate | 행 기준 | B | S2 `SlotReset` |
| E80 | 출력 | `discord/tui_prompt_relay.rs:754` | `owner_channel_for_tmux_session` | `runs_herdr` | JUDGED | D | S1 `SyncRead` |
| E81 | 출력 | `discord/tui_prompt_relay/synthetic_start/admission_fence.rs:168` | `read_current` | `current_execution_cached` | 미판정 → None | D | 〃 |
| E82 | 출력 | `tui_prompt_dedupe/hosted_source.rs:63` | `herdr_evidence` | `runs_herdr` ∨ 목록 ∨ marker ∨ 행 | 보완됨 | B | — |
| E83 | presence | `discord/turn_presence/activity/unbound/herdr_bound.rs:19`; `unbound/completed_scheduled.rs:162` | `admitted`/`collect` | `current_execution_cached`/`next_is_herdr` | JUDGED | D | 〃 |
| E84 | presence | `discord/turn_presence/activity/unbound.rs:305,534` | `host_scope_result`/`local_registration` | `herdr_endpoint`(자기) | tmux/inventory | C(R2 유지 결정. 자식 ID는 `:267-288`에서 먼저 걸러짐) | 모델 밖 |
| E85 | writer | `tui_o/writer/actor.rs:462,469-470` | `run_publishing` | `runs_herdr` | 미판정 → unsettled 미전송 | D | 〃 |
| E86 | writer | `tui_o/writer/input_notices/admission.rs:131` | `admit_notice` | `current_execution_cached` | 미판정 → fence 없음 | D | 〃 |
| E87 | writer | `tui_o/writer/rotation/readers.rs:806` | `aged_replacements` | `has_herdr_endpoint`(자기) | false | C | 모델 밖 |
| E88 | queue | `discord/queue_dispatch.rs:253` | 자동 개입 | 자기 boot + canary | 해당 없음 | B | — |
| E89 | Herdr launch | `herdr_launch.rs:550` → `src/db/dispatched_sessions/hosted_execution.rs:419` | `install_pending_pg` | 판정 없음(plan endpoint) | 도달 안 함 | E: 자식 owner·부모 배치 근거 | S2 `Launch`(Pending) |
| E90 | Herdr 배치 | `session_host/herdr/launch_host.rs:619`(`WorkspaceCreate`); `session_host/herdr/model.rs:238` | create | 채널별 workspace(tab 생성 요청 없음) | 도달 안 함 | A: 부모 workspace tab(Q4) | S2 `Create` |
| E91 | Pending 복구 | `herdr_launch/unplaced.rs:229,316`(호출 `claude/herdr_turn/pending.rs:108`, `codex/herdr_turn.rs:312`) | `resolve_unplaced` | 받은 endpoint + workspace marker | 도달 안 함 | A: tab marker(Q4) | S2 `Resolve`(같은 프로세스), `MutS2TabNoMarker` |
| E92 | Herdr gate | `session_host/herdr_gate.rs:225` | `try_notice_fence` | 받은 endpoint + nonce | — | E | — |
| E93 | 첫 입력 | `claude/herdr_turn/first_input.rs`(`submit_owned`) | 첫 입력 제출 | 판정 없음 | 도달 안 함 | A: 첫 키 전 fence(개정 3 §2.2-5) | S2 `SubmitHerdr` |

행 수: 93(E1~E93).

## 4. 입력 출처 분류표

호스트 판정 공통 경로는 `turn_host::for_turn`(`turn_host.rs:157`) → `resolve`의 스레드 분기(`:220-243`)다. Bound 행이 없으면 출처와 관계없이 Tmux다(`:242`).

| # | 입력 출처 | 작성자 | 진입 file:line | 소유자·경로 | 호스트 판정 지점 | 첫 입력(세션 생성) 가능? | S2 뒤 기대 |
|---|---|---|---|---|---|---|---|
| I1 | 스레드에 친 Discord 메시지 | 사람 | `discord/router/intake_gate.rs:122`(부모 조회 `:230`, 관계 기록 `:239-246`) | `intake_dispatch.rs:230`(동의 `:236`) / `:605` / `:637` → `:787`(동의 재확인 `:804`) → `queue_io/input_owner/ingress.rs:407` → `provider_dispatch/owner_input.rs:165` → `intake_turn.rs:161` | `intake_turn/host_refusal.rs:19`, `herdr_consent/admission.rs:41-48`, `intake_turn.rs:1940` | 예(지금 tmux) | Herdr fresh + 첫 launch 허가 |
| I2 | 스레드에 올라온 봇 메시지(dispatch 카드, 리뷰 후속, 알림) | announce·허용 봇 | 게시 `dispatches/discord_delivery/orchestration.rs:608`(스레드 생성 `:904`, 게시 `:926`, 재사용 `:816`). 수신 `intake_gate.rs:122`(봇 허용 `:374-386`, `:522`) | I1과 같음. 턴 중이면 큐에 넣음(`intake_gate.rs:950-991`) | I1과 같음 | 예. 슬롯 스레드 생성의 주 경로 | Herdr(fresh dispatch 스레드는 일반 `o_writer_ready` 경로. 개정 3 §2.1-2) |
| I3 | 부모 채널에 온 dispatch를 스레드로 돌림 | announce 봇 | `intake_turn.rs:624` → `intake_turn/adk_thread.rs:82`(재사용 `:171`, 생성 `:234`) → `session_runtime.rs:407` | 부모에서 I1 뒤 `channel_id = route.target`(`intake_turn.rs:645`) | 동의·사전 판정은 **부모 ID**. 재사용은 `host_refusal.rs:67`. 최종 `intake_turn.rs:1940`(스레드) | 예 | Herdr. 부모 ID 판정 문제는 Q15 |
| I4 | 바쁜 pane에 붙이는 legacy live 입력 | 사람·봇 | `intake_gate.rs:1012` → `intake_gate/busy_inject.rs:283,311` → `health/turn_deliver/inject.rs:492` | owner가 가지면 건너뜀(`busy_inject.rs:302`). `covers` false(부모 Herdr) | `inject.rs:440` → `host_defer_gate.rs:27,109` | 아니오 | Herdr 자식이면 owner 경로(#8114) |
| I5 | 실행 중 턴에 owner busy 주입 | 사람 | `provider_dispatch/owner_input.rs:282,407,625` | owned_busy → `claude_tui` busy | `owner_input.rs:417,639` | 아니오 | Herdr 즉시 주입(#8114) |
| I6 | 큐 drain·idle kickoff | 큐에 넣은 원 작성자 | `discord/queue_dispatch/kickoff.rs:371`(deferred `queue_io/backstop.rs:214`) → `gateway.rs:775` → `router/ports.rs:61` → `intake_dispatch/queued.rs:92,419` | 동의 `:236` → `:787,804` | `intake_turn.rs:1940`(+ `nameless_gate.rs:180`, `host_defer_gate.rs:109`) | 예 | I1과 같음 |
| I7 | 재시작 재생: 보류 큐 복원 | 원 작성자 | `discord/runtime_bootstrap/queued_recovery.rs:31,57,76` → `owner_input.rs:752` | mailbox 복원 → I6 또는 owner | I6과 같음 | 예 | Pending 자식이면 복구 전 보류(`Rejudge` 금지, Q5·Q12) |
| I8 | 재시작 재생: 놓친 Discord 메시지 따라잡기 | 사람·봇 | `discord/catch_up.rs:445`(후보 `:218`) | mailbox → I6 | I6과 같음 | 예 | 〃 |
| I9 | Herdr 동의 해제·부팅 동의 복구 | 원 작성자(트리거는 owner 명령 또는 부팅) | `discord/router/herdr_consent.rs:206,388`; `herdr_consent/startup.rs:91`; `herdr_consent/custody.rs:258` | I6 | I6과 같음 | 원칙상 예 | 자식 입력의 부모 proof(W2-3) |
| I10 | 클러스터 intake worker·보호 요청 정확 재시도 | 원 작성자 / 시스템 재생 | `intake_turn/worker_entry.rs:7,44,62,76,138` ← `gateway.rs:767` ← `turn_bridge/retry_state.rs:57` | owner 또는 `handle_text_message` | `intake_turn.rs:1940` | 예 | I1과 같음(중복 0: Q12) |
| I11 | 실패한 resume의 이력 재시도 | 시스템(사용자 입력 재생) | `discord/turn_bridge/recovery_text.rs:765` ← `gateway.rs:724`, `retry_state.rs:44` | 내부 개입 → I6 | I6과 같음 | 예 | 〃 |
| I12 | HTTP deliver `POST /api/agents/{id}/turn/deliver` | API를 쓴 사람 | `src/server/routes/agents_turn_deliver.rs:137` → `health/turn_deliver/prepared.rs:13,27` | `health/thread_turn.rs:79` `check_live` 필수 → `turn_deliver.rs:198` | `thread_turn.rs:148`(비스레드 `preflight.rs:20`) | 아니오(`thread_cold`) | Bound Herdr 자식에 주입 |
| I13 | HTTP start(스레드 지정) | API 호출자 | `src/server/routes/agents.rs:858` → `agents_turn_target.rs:223,252,261` → `health/headless_turn.rs:503,525` | `router/message_handler/headless_turn.rs:35,222` → `owner_input.rs:713` | `thread_turn.rs:148` → `headless_turn/admission.rs:46-52` → `headless_turn.rs:993` | 아니오(`check_live`) | B |
| I14 | 루틴(스레드) | 루틴 스케줄러 | `routines/agent_executor/launch.rs:79,213,227`; `agent_executor.rs:1052`; `routines/agent_executor/owner_input.rs:165`; `health/consumer_ports.rs:130` → `health/headless_turn.rs:295,351` | owner(`InputOrigin::Routine`) 또는 legacy headless | `headless_turn.rs:157-159`, `admission.rs:51`, `headless_turn.rs:993` | 예(라벨 tmux 세션) | Q13(S1r과의 경계) |
| I15 | 대상이 스레드인 예약 메시지 | 스케줄러 | `scheduled_messages.rs:361,443` → `consumer_ports.rs:130` | I14와 같음(`check_live` 없음) | `admission.rs:46-52`, `headless_turn.rs:993` | 예 | Q13 |
| I16 | 스레드에서 친 `/restart`·`!restart` 시드 | 사람 명령 → 시스템 시드 | `discord/commands/restart.rs:370,395` → `:143` → entrypoints `:200` | `host_decision` `restart.rs:291` → headless/owner 시작 | `restart.rs:291`, `headless_turn.rs:993` | 예 | 실행 없는 스레드: E51 |
| I17 | Claude tmux→Herdr handoff journal 시작 | 내부(owner) | `claude_handoff.rs:44,331` | owner front | `claude_handoff.rs:261,280` | 아니오 | B(D) |
| I18 | effort 재시작 이어 가기 | 내부(`/effort`) | `discord/commands/effort_restart/tmux.rs:369` → `owner_input.rs:713`; `queue_io/input_owner/effort_continue.rs:9` | owner start | `headless_turn.rs:993` | 아니오 | Codex: 첫 커밋 계약 |
| I19 | TUI에 직접 친 입력·ScheduleWakeup | 터미널의 사람 / provider 내부 | `discord/tui_direct_pending_start.rs:192` | 다시 제출하지 않음. inflight만 만듦 | 없음 | 아니오 | B |
| I20 | Decision 승인 전달 | 승인한 사람 | `health/turn_deliver/decision_target.rs:265` | turn_deliver | `preflight.rs:20` | 불명(스레드 해석 미확인) | [U] |
| I21 | 에이전트 handoff·복구 | 다른 에이전트 / 복구 | `agent_handoff.rs:566`; `health/recovery/live_agent_recovery.rs:255` → `health/headless_turn.rs:16` | headless 시작 | `admission.rs:46-52` → `headless_turn.rs:993` | handoff는 바인딩 채널만. 복구는 [U] | [U] |

출처 수: 21(I1~I21).

입력이 아닌 것:
- `relay_auto_heal` redrive(`health/relay_auto_heal.rs:366`)는 출력 재전달이다.
- boot custody notice(`inflight/removal/custody_notice.rs:33`)는 외부로 나가는 안내다.
- `replay_start`(`intake_turn.rs:1928`)는 재생 가드다.

## 5. 증상 경로 — 18:12 launch 줄에서 거꾸로

로그 줄: `[session-strategy] launch provider=claude tmux=AgentDesk-claude-adk-cc-t1570308839910412348 … resumed=false`. 부모 adk-cc는 `session_hosts.herdr.channels`에 있다(검증 사본 근거. 운영 설정은 읽지 않았다).

1. `src/services/claude_tui/session.rs:498-502`(`write_launch_script` `:475`)가 이 줄을 남긴다. `resumed`는 argv에 `--resume <id>`가 있는지로 정한다.
2. 호출 경로: `claude_tui/session.rs:398` `prepare_claude_tui_launch_with_continuation_owner` ← `claude/tui_session_launch/prepared.rs:87-88`(`prepare`, `:18`) ← `claude/tui_session_launch.rs:136` ← `prepare_and_create_claude_tui_session`(`:67`) ← `claude.rs:1799`(`execute_streaming_local_tui_tmux`, `:1695`).
3. tmux 생성 가드는 둘 다 통과한다: `tui_session_launch.rs:91-94`와 `:106-108`. 둘 다 `herdr_configured_for_tui_launch(channel_id)`(`herdr_launch.rs:64`)이고, 이 함수는 `configured`(`:75`) = `session_hosts::herdr_endpoint(channel)`, 곧 **스레드 자신의 boot 항목**만 본다. 그래서 false다.
4. `resumed=false`인 이유: selector가 지워져 있었다. 이때 `prepared::resolve`(`claude/tui_session_launch/prepared.rs:119-129`)가 `requested_session_id = None`을 받아 새 UUID를 만든다. selector가 있어도 transcript가 없으면 FreshSubstitution(`:140-149`)으로 같은 줄이 나온다(Q2).
5. 이 driver는 `provider_dispatch.rs:360-362`의 `drive`가 `TurnHost::Tmux`일 때만 고른다.
6. `TurnHost::Tmux`는 `intake_turn.rs:1940` `for_turn` → `turn_host.rs:200` `resolve`에서 나왔다.
   - `:207-208`: 스레드 자신의 boot 항목 없음.
   - `:219`: `read_row`가 Legacy 행을 읽음. `restart-coord-fresh.sh`가 tmux만 죽이고 selector만 지웠으므로 행은 남는다.
   - `:220-221`: `policy_channel`이 부모를 돌려준다(관계가 기록된 경우). 부모 endpoint가 있다.
   - `:224`: `is_live_thread_endpoint`가 false다. Legacy 행은 `HostedRecord::Known`이 아니다(`:262-268`).
   - `:235`: 행 읽기 성공.
   - `:239`: `herdr_trace`(`:94-104`)가 Legacy에 `None`을 낸다.
   - **`:242`: `TurnHost::Tmux`.**
   - 관계가 기록되지 않았으면 `:221`의 `policy == channel` 쪽으로 가고, `:245` `return_plan`이 None, `:248-250`에서 역시 Tmux다.
7. `:165` `record_judged(thread, Some(false))`이 기록되고, 이후 동기 소비자도 tmux로 읽는다.

**판정 지점(root):** `src/services/turn_host.rs:239-242`. 이유: 스레드 분기는 "부모 endpoint에 Bound된 자기 Herdr 실행"만 Herdr로 보내고, 실행이 없는 스레드(행 없음·Legacy·selector 없음)의 다음 실행을 부모 정책으로 보내는 규칙이 없다. `thread_host::next_host`(`thread_host.rs:137-150`)도 같은 규칙이다(판정된 Herdr일 때만 Herdr). 보조 지점은 tmux 생성 가드 `tui_session_launch.rs:91-94,106-108`과 `claude.rs:2602`다. 이 가드들은 자기 boot만 보므로, 판정이 Tmux로 새도 막지 못한다.

상태 차트 반례 대조(`S2MainFresh`, 1단계):

| 모델 | 코드 |
|---|---|
| 초기 `exec = LegacyDead`, `inq = Queued` | Legacy 행 유지, tmux 죽음, selector 지움, 메시지 도착(I1) |
| `Launch` → `LaunchHost = "Tmux"`(`Mode = Main`) | `turn_host.rs:239-242` |
| `exec' = Tmux`, `tmuxs' = 1` | `provider_dispatch.rs:362` → `claude.rs:1799` → `tui_session_launch.rs:93,108` 통과 → `TmuxHost.create_session`(`:162`) → `session.rs:499` |

모든 단계를 코드로 확인했다(운영 스크립트 `restart-coord-fresh.sh`의 동작은 검증 사본의 서술을 따랐다).

## 6. 설계가 답할 질문

- **Q1. "새 스레드"의 범위.** `FreshAfterKill`(기존 스레드, Legacy 행, tmux 죽음, selector 지움)도 Herdr로 뜨는가?
  - 델타 4 D2는 "Missing ∧ selector 없음 → Herdr"이고, 검증 sweep은 18:12 증상을 FAIL로 판정했다.
  - 다른 안은 "Legacy 행이 있으면 tmux"(D2의 좁힌 안)다.
  - 어느 쪽이냐에 따라 RED 시험의 입구가 달라진다.
- **Q2. selector의 출처.** 판정이 읽는 selector는 intake 단계의 DB `claude_session_id`(E77)인가, launch의 `requested_session_id`인가? selector는 있는데 transcript가 없어 FreshSubstitution이 되는 경우(`prepared.rs:140-149`)는 Tmux인가 Herdr인가?
- **Q3. 첫 launch 허가.** 개정 3 §2.1의 `ChildLaunchPermit`(late 자식, fresh Pending 제외)이 b04ea56650에서도 성립하는가? `first_input.rs`·`handoff.rs`가 크게 바뀌었다(드리프트). 허가 없이 활성화하면 `OWriterNotReady`로 첫 턴이 거절된다(`S2NaiveAll`·`MutS2NoPermit`).
- **Q4. 배치.**
  - 부모 workspace에 `tab.create`(클라이언트 미구현, protocol 22)로 둘지, 지금처럼 채널별 workspace로 둘지 정해야 한다.
  - tab을 쓰면 응답 유실 복구가 tab marker로 같은 pane을 찾아야 한다(`MutS2TabNoMarker` → `NoDuplicatePane`).
  - 부모 pane이 없을 때 거절할지(D1)도 정해야 한다.
- **Q5. 재시작 뒤 Pending 복구.** `herdr_reader.rs:363`이 자기 boot만 읽어 스레드 Pending을 건너뛴다. 그 사이 재생 입력은 `turn_host.rs:239-241`에서 `HostedHerdrUnconfigured`로 거절된다(`MutS2NoBootResolve`). 부모 endpoint로 복구하는 위치와, 복구 전 입력을 보류하는 계약이 필요하다.
- **Q6. 해제(liveness).**
  - idle 정리는 Herdr를 건너뛴다(`session_idle_cleanup.rs:75-81`).
  - thread_gc는 Legacy·Retired 행만 지운다(0131).
  - 슬롯 리셋은 Bound 자식을 건너뛴다(`auto_queue/runtime.rs:498-507`).
  - 이대로 활성화하면 idle Herdr 자식 pane이 남고 슬롯 회전이 영구히 멈춘다(`S2NaiveAll` `EventuallyReleased`·`SlotRotates`).
  - idle 해제를 S2에서 할지, #7583 후속으로 넘기고 S2는 슬롯 리셋 경로만 열지 정해야 한다.
- **Q7.** `host_decision.rs:200-202`의 `Observed::Nothing → Tmux`를 `next_host`에 연결할 때, 실행 없는 Herdr 부모 스레드에서 /restart·reset·locator(E51·E52·E66)가 어떻게 답하는가?
- **Q8. 자기 boot만 읽는 [C] 행 13곳**(E20·E32·E34·E35·E36·E47·E56·E59·E60·E74·E84·E87, E73은 Q5). 이 중 Claude Herdr 자식에서 S2가 바꿀 것과 근거를 적고 남길 것을 나눠야 한다. 특히 다음 둘이다.
  - E36 `covers`: 부모가 Herdr이면 실제가 tmux여도 false다.
  - E74 server_replaced: 자식에게는 demand·hold가 없다.
- **Q9. Codex 자식.** S2는 Claude만 다루는가? Codex tmux 가드(E19)와 `configured_plan`의 Codex 스위치, 첫 커밋 계약 E59와의 관계를 정해야 한다.
- **Q10. JUDGED boot seed.** S1b가 알려진 공백으로 남긴 `RestoredBeforeRead`를 어디서 seed할지(E72 `reconnect_pass` 후보), 동기 소비자 D행 13곳이 seed를 기다리는 방식을 정해야 한다.
- **Q11. 첫 커밋 계약.** P2-1(`/clear`의 Pending·Retired·Hold), P2-2(`recorded_execution`의 verified_bound), 1121i P2-1(`herdr_codex` → `herdr_now`)을 S2 첫 커밋으로 확정하고, 각각의 시험을 정해야 한다.
- **Q12. 입력 중복.** 첫 입력을 제출한 뒤 재시작했을 때 I7·I8·I10·I11 재생이 같은 입력을 다시 제출하지 않는다는 근거가 필요하다. owner front 소비 시점과 `hold(nonce)` 좌표를 확인해야 한다. `MutS2ReplayResubmit`은 계약 뮤턴트다.
- **Q13. 루틴·예약 입력**(I14·I15). Herdr 부모 아래 루틴 스레드의 첫 실행은 S2 대상인가, S1r 대상인가?
- **Q14. 관계 기록 의존.** 스레드가 tui_o child로 기록되지 않으면(부모가 O 미선택이거나 기록 전) `parent_policy`가 Tmux가 된다. 호스트 판정이 O 선택과 독립된 관계 기록을 써야 하는가? `bootstrap_thread_session`(E39)의 worktree·행 준비는 Herdr 자식에서 어떻게 되는가?
- **Q15.** dispatch redirect(I3)에서 동의·사전 판정을 부모 ID로 하는 것을 S2에서 스레드 ID로 바꾸는가?
- **Q16. 크기.** 델타 4 추정 +830~1130이 정지선 +1200에 가깝다. 분할 경계(S2a 배치 / S2b 활성화)와, 그때 생기는 생산 호출자 없는 배치 코드를 어떻게 처리할지 정해야 한다.

질문 수: 16.

## 7. 수용 기준·시험 후보(설계가 확정)

| 후보 | 지키는 계약 | 모델 규칙 |
|---|---|---|
| T-S2-1 | Herdr 부모 아래 selector 없는 스레드의 첫 입력 → Herdr Pending → Bound → 제출 1회. tmux create 0 | `NewThreadFollowsParent`, `SingleSubmit` |
| T-S2-2 | 18:12 재현: Legacy 행 + tmux 죽음 + selector 지움 → Herdr(Q1 결정에 따름) | `S2MainFresh` RED → GREEN |
| T-S2-3 | selector 있는 기존 tmux 스레드는 tmux 재생성, Herdr create 0 | `ExistingTmuxStaysTmux` |
| T-S2-4 | tmux 부모 아래 스레드는 그대로 | `TmuxParentStaysTmux` |
| T-S2-5 | late 자식의 첫 턴이 `OWriterNotReady`로 거절되지 않는다 | `NoInputRefused`, `MutS2NoPermit` |
| T-S2-6 | Pending 중 재시작 → 부팅 복구가 부모 endpoint로 채택 또는 은퇴. 재생 입력은 거절 대신 보류 | `MutS2NoBootResolve` |
| T-S2-7 | create 응답 유실 → 같은 pane 채택, 두 번째 pane 0 | `NoDuplicatePane`, `MutS2TabNoMarker` |
| T-S2-8 | idle Herdr 자식 해제와 슬롯 회전(Q6 결정 범위) | `EventuallyReleased`, `SlotRotates` |
| T-S2-9 | 재시작 뒤 동기 소비자가 Bound 자식을 tmux로 읽지 않는다 | S1 `RestoredBeforeRead`(`S2SeedAll`) |
| T-S2-10 | 첫 커밋: P2-1·P2-2·effort 계약(Q11) | S1 `CmdTmuxOp`·`Turn` |
