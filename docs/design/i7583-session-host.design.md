# #7583 최종 설계 — Herdr를 Windows 세션 호스트로, ProcessBackend 삭제, 세션 호스트 공통 경계

<!-- doc-path-refs: proposed src/services/session_host/herdr/transport_pipe.rs src/services/session_host/herdr_core/ src/services/platform/file_identity.rs src/services/session_host/channel_host_plan.rs src/services/herdr_launch/placed.rs -->

- 기준: `origin/main` `1974cff8a67bf5d1db041abfdd4d615c518ee104`(2026-11-15). 모든 좌표는 이 SHA에서 다시 확인했다.
- 입력: A(WebGPT pro 초안, 실측 없음), B(Fable xhigh, `refs/review/i7583-design-fable-1` `abad58858d`), C(WSL Codex, `refs/review/i7583-design-1` `b6766912fd`).
- **정본 기반: C.** 채택 이유는 §2. 사실 근거는 C의 전수표([진입점 표](i7583/entrypoints.md)와 TSV)를 우선했고, 전수표 밖 좌표는 합류 레그가 main에서 직접 확인해 보충했다.
- 상태 모델: `docs/statecharts/session-host/`(34 cfg, GREEN 11 + 예상 RED 23). [보고서](../statecharts/session-host/session-host.report.md).
- 개정 2(r2): 독립 리뷰(P0 0·P1 4·P2 2)를 반영했다. 바뀐 결정은 §2.1 표에 모았고, 정본 C 채택과 전체 방향은 그대로다.
- 개정 3(r3): r2 독립 리뷰(P0 0·P1 3·P2 1)를 반영했다(§2.2). S01 계약(§12.4)은 바꾸지 않았다.
- 개정 4(r4, 델타): r3 독립 리뷰(P1 1·P2 2)를 반영했다(§2.3). 33조각 구조와 S01 계약은 그대로다.
- 이 레그는 설계만 한다. Rust 구현·cargo·운영 설정·Windows 접속은 하지 않았다.

## 0. 결론

1. Windows 관리 Claude·Codex 세션은 Unix와 같은 **Herdr TUI 실행 경로**로 띄운다. 새로 쓰는 것은 named pipe 전송, Windows OS 신원,
   Herdr 서버를 dcserver job 밖에서 띄우는 수명 계층, Windows launch 명령, 파일 신원 헬퍼다. 실행·입력·복구·종료 로직은 지금 Unix 코드다.
2. dcserver 재시작 재채택은 terminal attach·서버 넘기기 없이 이미 돈다(호출마다 새 연결 `src/services/session_host/herdr/transport.rs:1`,
   저장된 pane 재판독 `src/services/discord/recovery_engine/herdr_reader.rs:220`). Windows에서 막는 것은 **dcserver의 kill-on-close job**이다
   (`src/services/platform/windows_job.rs:35`, `:52`, 부팅 `src/cli/dcserver.rs:882`). Herdr 서버가 이 job 안에 있으면 dcserver와 함께 죽는다(모델 `WindowsServerInJob`).
3. 채널 선택은 `runtime.herdr_turn_enabled` 자리를 bool 호환 타입으로 넓힌다. Unix 기본값은 지금과 같다. **선택이 없는 Windows 관리 Claude/Codex만**
   내부 dedicated endpoint로 Herdr를 쓴다(설정에 쓰지 않고 부팅마다 재구성). 운영자가 채널을 옮겨 적는 수동 이사는 없다.
   이 PlatformDefault 실행에는 Claude·Codex 실행 bool과 Cancel bool을 적용하지 않는다(실행 허가와 Cancel 허가를 선택과 함께 정한다, §5.1).
   Unix와 endpoint 채널의 세 bool은 지금 뜻 그대로다(Claude·Codex Escape 모두 Cancel bool을 따른다).
4. ProcessBackend는 Windows Claude·Codex 활성화 뒤 지운다. 삭제 뒤 쓸 호스트가 없는 관리 세션(tmux 없는 Unix 등)은 provider 함수의 오류가 아니라
   **intake admission 경계**에서 내구 hold로 분류하고, 모든 provider(Qwen 포함)·모든 출처(live·queued·slash·첨부·headless)의 원문·첨부를 내구 큐나 호출자에 남긴다(§10.2).
   Windows Qwen은 세션 없는 직접 실행(이미 있는 headless 경로)으로 간다.
5. `InteractiveSessionHost`는 `create`·`submit`·`interrupt`·`terminate(warrant)`·`recover`를 얻는다. 정책(허가·fence·신원·큐 원문·O 게시)은 지금 소유자에 둔다.
   "우회 0"은 **관리 세션의 물리 효과가 `session_host` 어댑터 메서드로만 일어난다**로 정의하고 ratchet으로 지킨다.
6. 위치(pane)가 기록됐지만 Bound가 안 된 Herdr Pending은 재시작·주기 복구 패스가 사람 개입 없이 끝낸다: pane이 없으면 retire,
   자기 nonce의 셸만 남았으면 그 pane만 닫고 retire, provider가 살아 있으면 Pending을 지키며 같은 nonce의 기존 첫 입력 경로로 잇는다.
   **provider 증명은 source 증명이 아니다** — Bound는 자기 source가 기록된 뒤 기존 attach만 만든다(§7.3, 조각 S06b).
   dcserver 재시작은 그 실행의 메모리 등록(실행 목록·provider 매핑·Codex placeholder·runtime binding)을 비우므로, resolver는 기존 attach·hook 경로 전에
   같은 nonce의 내구 상태로 그 등록을 DB·pane·hold 쓰기 없이 되살린다(§7.3 재시작 복원).
7. Herdr Core는 채널 세션 호스트가 아니라 #7438 F4 레인 전용 구현체다.
8. 착지 조각 **33개**(공통 14·Windows 11·삭제 6·Core 2) + 측정 1(W00). 미측정 [U] **20개**(win-desktop 17, 격리 VM 1, 맥 2).

## 1. 비교표

| 기준 | A (pro) | B (Fable) | C (WSL Codex) |
|---|---|---|---|
| (1) Windows 재시작 재채택(attach 없이) | API 재연결·source 재채택 순서는 옳다. upstream Herdr의 kill-on-close job 감지·WMI 기동을 지적했으나 AgentDesk job은 보지 않음 | **job이 막는다는 핵심 결함을 main 좌표로 찾음**(`windows_job.rs:35,52`). BREAKAWAY/WMI 대안 제시. 서버 소실 처리(기존 server-replaced) 명시 | 재채택 신원(pipe 서버 PID·FILETIME·boot·nonce·root/provider)과 Pending·unplaced 처리가 가장 정밀. job은 U04 측정으로만 남김(전수표가 `windows_job.rs`를 놓침) |
| (2) ProcessBackend 삭제 안전성 | 옛 `runtime_kind`가 조용히 건너뛰어짐(`restore_inflight.rs:226`)을 지적. Qwen은 Herdr 위 wrapper(새 결합) | tmux 부재 → spawn 전 오류, 가용성 캐시·살아 있는 pane 규칙, 시험 대역 3부류·D0 선행, `runtime_kind_for_recovery` 이중 의미, 클러스터 `process` 광고까지 | 지울 것/남길 것 표가 정확하고 공유 코드 보존 근거가 분명. 대역 치환은 삭제 뒤(D02) |
| (3) 경계 명확성·우회 0 경로 | trait 초안은 있으나 우회 전수표 없음(U-CENSUS로 미룸) | 호출 수 전수(부록 A2·A3) + ratchet. #7272 입력 실행기를 "tmux 어댑터 구현"으로 인정하자는 해석(설계 이탈 후보) | **441행·161파일 감사 전수표**와 판정(BYPASS/ADAPTER/THROUGH_TRAIT/NON_MANAGED), 잔여 101파일 분할표. 입력 실행기는 그대로 두되 물리 효과는 호스트 경유 |
| (4) 분할 현실성 | 조각 이름만, 크기 [U-SIZE] | 27(착지 23). 호출 종류별 기계 이동이라 병렬이 쉬움. P0(Windows 컴파일 census)로 게이트 열기 크기 확정 | 23. 영역별, 각 ≤24파일. S01이 caller 없는 계약 착지라 dead code 위험(C 스스로 cfg(test) 조건을 둠) |
| (5) 운영 마찰 | 새 스위치 없음, PlatformDefault 정규화 | **Windows 채널을 운영자가 `session_hosts.herdr.channels`에 모두 적어야 함**(결정 1). W1 카나리도 설정 작업 | 새 키·명령 없음. 선택 없는 Windows 관리 세션은 내부 endpoint로 자동 |
| (6) 사실 정확도(C 대조) | GitHub 원문 좌표는 대체로 맞음. 실측 없음 | `exec_ops.rs`를 비관리로 분류(틀림, §3). `HerdrHost`를 새 타입으로 적음(이미 있음) | `guarded_save.rs:154`를 Core Enter 지연 근거로 잘못 인용. job 누락 |

## 2. 정본 채택과 보완

**기반은 C다.** 이유: (a) 사용자 결정(새 수동 스위치·런북 없음, 선택은 `runtime.herdr_turn_enabled` 자리 확장)을 그대로 만족하는 유일한 안이다
(B는 Windows 채널 수동 매핑이 필요). (b) DoD 3의 "우회 호출부 전수표가 0"에 대해 실측 전수표와 판정 기준을 가진 유일한 안이다.
(c) 재채택 신원·Pending·close/retire 분리가 가장 정밀하고, 모델도 그 경계를 검사한다.

| 보완 항목 | 출처 | 정본에 넣은 곳 |
|---|---|---|
| dcserver kill-on-close job이 Herdr 서버를 죽인다 → 서버를 job 밖에서 띄움, BREAKAWAY·WMI 대안 | B §0.2·§6.1 | §6, 조각 W04, [U] U04, 모델 `WindowsServerInJob` |
| Windows 컴파일 census를 먼저 재 게이트 열기 크기 확정 | B §8 P0·U8 | W00, [U] U15 |
| 파일 신원(dev·inode)·잠금 공통 헬퍼 | B §4 B8·B9, P2 | W05 |
| 시험 대역 3부류와 대역 치환 선행(D0) | B §7.3 | D00 |
| `runtime_kind_for_recovery`의 이중 의미 0단계 고정 | B §1·§6.3 | D03 |
| 옛 `"process_backend"` 행이 조용히 건너뛰어지는 위험 | A §4 | D03 수용 기준 |
| 클러스터 `process` 광고 제거 | B §1 | D04, [U] U14 |
| tmux 가용성 캐시·살아 있는 pane 규칙을 가용성 판정 안으로 | B §7.1 | D01 |
| Windows Qwen은 headless 직접 실행 | B §10 결정 2 | §10.3, D01 |
| ratchet으로 우회 0 유지 | B §2.3 A7 | S08 |
| 채널 선택을 하나의 `ChannelHostPlan`으로 정규화해 생성·부팅·Cancel·Terminate·clear가 같이 소비 | A §1 | §5, W01 |
| pending 큐를 inflight보다 먼저 복원하는 부팅 순서 유지 | A §5.1 | §7 |
| Codex는 첫 입력과 함께 첫 hook이 오므로 create 완료에 SourceBound를 넣지 않음 | A §2 | S04 수용 기준 |
| never-attached 안내문의 수동 명령을 Windows 정상 탈출로 쓰지 않음 | A §3 | §7, W09 수용 기준 |
| 새 native 세션의 O writer 빈 초기화·checkpoint 0 정식 경로 | A §3 | W09 |
| process 시절 Claude 세션의 resume은 O 커서가 없어 거절된다(`src/services/claude/herdr_turn/handoff.rs:38-43`) → Windows 첫 전환이 입력마다 오류가 되는 경로 | A §3·U-W2(합류 레그가 main에서 확인) | §7.2, W09, [U] U20 |
| Windows PID 신원은 raw 생성 FILETIME | A §5.2 | W03a |
| Core 계약(`run --keep`, `spawn_pending`, 재연결 뒤 snapshot) | A §9 | C01 |
| Core는 `hc --json` CLI 래핑 + 버전·schema 고정 | B §8 C1 + C C01 | C01 |

### 2.1 리뷰 반영(r2)

| 리뷰 항목 | 처리 | 바뀐 곳 |
|---|---|---|
| P1-1 위치가 기록된 Pending의 자동 탈출 | **고침.** main은 placed Pending을 복구 패스에서 건너뛰고(`src/services/discord/recovery_engine/herdr_reader.rs:463-464`), unplaced resolver는 위치 없는 행만 받으며(`src/services/herdr_launch/unplaced.rs:353`), server-replaced의 placed 처리는 user_default·재부팅 뒤에만 있다(`src/services/discord/recovery_engine/herdr_server_replaced.rs:177`, `:238-262`). 세 상태를 나눈 placed Pending resolver를 새로 둔다 | §7.2, §7.3, S06b, W08, W09, U11, 모델 `RecordLocation`/`AdoptPlaced`/`ResidueClose`/`ResolvePlacedGone`, cfg `SkipPlaced`·`UnixHerdrCurrent` RED → 제안 GREEN |
| P1-2 Windows 암묵 기본과 Codex 실행·Cancel bool | **고침.** `ChannelHostPlan`이 호스트와 함께 실행 허가·Cancel 허가를 정한다. PlatformDefault 실행은 `herdr_codex_turn_enabled`(`src/services/turn_host.rs:220-232`)와 `herdr_cancel_enabled`(`src/services/provider/cancel_token_claude_interrupt.rs:544-563`)를 읽지 않는다. endpoint·explicit 채널과 Unix는 지금 뜻 그대로 | §2 충돌표, §5.1, W01, W09, W10, 모델 `GateRun`·`GateCancel` |
| P1-3 D01의 Qwen 대기 원문 | **고침.** 호스트 없음을 provider 함수 `Err`가 아니라 기존 intake admission 거절(`src/services/discord/router/message_handler/intake_turn/host_refusal.rs:6-63`, 큐 복원 `src/services/discord/router/intake_dispatch/queued.rs:353-357`)로 옮긴다. 모든 provider가 같은 경계를 지난다(`src/services/discord/router/message_handler/intake_turn.rs:252`) | §10.2, D01, 모델 `Unavailable`·`ErrConsumes` |
| P1-4 reset-tmux 종료의 NON_MANAGED 오분류 | **고침.** `src/cli/utils.rs:168`(관찰)·`:186`(종료)은 `AgentDesk-*` 관리 세션을 다룬다. 전수표 두 행을 BYPASS로 재분류(BYPASS 320·NON_MANAGED 28), S01에 `:186` 추가(prod 22파일, 관리 종료 28곳), 비관리 종료 허용 4→3, `:168`은 S02 | [audited-bypasses.tsv](i7583/audited-bypasses.tsv), [진입점 표](i7583/entrypoints.md), §4, §12.4 |
| P2-1 S01 ratchet 표기·시험 포함 범위 | **고침.** 기존 `tmux_kill` 통계 범주는 두고, 생산 텍스트(통째 시험 파일·`cfg(test)` 제외)에서 `platform::tmux::`와 `tmux::` 두 표기를 모두 세는 새 범주를 S01에 넣는다. 새 생산 호출 파일은 검사기 상수의 허용 목록으로만 들어간다 | §12.4 |
| P2-2 모델 991=991이 §5 동일성을 보이지 않음 | **고침.** §5 표를 생산 선택 함수의 표 기반 시험 정본으로 삼고 조합 누락을 실패로 만든다(W01). 모델은 endpoint와 스위치를 나눴지만 보장 범위를 "선택된 호스트의 수명 계약"으로 좁혔다 | §5.2, 모델 보고서 |

### 2.2 리뷰 반영(r3)

| 리뷰 항목 | 처리 | 바뀐 곳 |
|---|---|---|
| P1-1 provider 증명만으로 Bound → Codex 첫 입력 경로가 닫힘 | **고침.** Codex는 첫 hook을 첫 prompt와 함께 보내고(`src/services/codex/herdr_turn.rs:266-274`), source attach는 nonce baseline이 없으면 보류된다(`src/services/discord/tui_prompt_relay/herdr_source.rs:294-302`). Bound 행의 다음 입력은 source를 요구하고(`src/services/codex/herdr_turn.rs:515-531`), 첫 입력 함수는 Pending만 받는다(`src/services/codex/herdr_turn/first_input.rs:90-103`). resolver는 provider 증명으로 Pending을 지키고 같은 nonce의 기존 첫 입력 경로를 깨운다. Bound는 main의 세 Bound 작성자(`src/services/discord/tui_prompt_relay/herdr_source.rs:197`, `:244`, `src/services/claude/herdr_turn.rs:873`, 모두 source 필요)만 만든다 | §0-6, §7.2, §7.3, S06b, U11, 모델 `FirstPrompt`·`HookArrives`·`TurnAttach`·`ConfirmReady`, cfg `BindProven` RED → `CodexImplicit`·`CodexHerdr` GREEN |
| P1-2 §5.1 Unix Claude Escape 칸이 Cancel 기본값을 바꿈 | **고침.** 생산 Herdr Escape 전달자는 하나(`src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:801`)이고, Claude·Codex 모두 `herdr_command_eligible`(`:250-262`)·`interrupt_herdr`(`:589-600`)의 bool 검사를 지난다(Claude 토큰 설치 `src/services/claude/herdr_turn.rs:240-246`, 진입 `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:370`, `:401`, `:471`). r2 §3의 "bool은 Codex만"은 틀렸다. Unix·endpoint 칸은 두 provider 모두 settlement ∧ bool, PlatformDefault만 bool 우회 | §3, §5.1, §5.2(Escape 칸·생산 경계 시험), W01, W09, W10, 모델 `UnixCancelAsMain`, cfg `EscapeNoSwitch` RED |
| P1-3 D01이 live Qwen 본문을 보존하지 않음 | **고침.** live submission은 owner·outbox 없이 본문만 갖고(`src/services/discord/router/intake_gate.rs:1325-1352`), host_refusal의 비queued 분기는 uploads·clear만 되돌린다(`src/services/discord/router/message_handler/intake_turn/host_refusal.rs:46-63`, `src/services/discord/admin_host_guard.rs:66-88`). 판정을 `IntakeSubmission` 전체를 가진 router admission(`src/services/discord/router/intake_dispatch.rs:229`)으로 옮겨 기존 deferred 계약(내구 hold 뒤에만 checkpoint, `:690-711`)에 태운다 | §10.2, D01, 모델 `TmuxReturns`·`Requeue` |
| P2 Requeue 도달 불가 | **고침.** tmux 가용성을 변하는 값(`tmuxUp`)으로 바꾸고 가용성 복귀(`TmuxReturns`)를 넣었다. 정상 cfg에서 재제출 도달을 증인 cfg `RequeueReach`(`NeverRequeued` RED)로 확인하고, 복귀 뒤 제출 liveness `HeldInputSubmitted`를 넣었다. `NoAvailWake` 뮤턴트가 그것을 깬다 | 모델, §14 |

### 2.3 리뷰 반영(r4, 델타)

| 리뷰 항목 | 처리 | 바뀐 곳 |
|---|---|---|
| P1 재시작한 Codex Pending은 메모리 등록이 비어 기존 attach·hook 경로가 끝내지 못함 | **고침.** 재시작은 실행 목록(`src/services/tui_prompt_dedupe/runtime_binding/adopt_skip.rs:102`)·provider 매핑·placeholder·runtime binding을 비운다. Codex attach는 live binding이 그 source여야 하고 아니면 NotPublished다(`src/services/discord/tui_prompt_relay/herdr_source.rs:231-238`). hook ingress는 목록에 없는 실행을 이 실행으로 기록하지 않고(`src/services/tui_prompt_dedupe/runtime_binding/codex_hook.rs:333-335`) runtime binding이 없으면 425다(`:388-390`). 재시작 복원은 Bound만 받고(`src/services/discord/recovery_engine/host_reconcile.rs:96-98`), 첫 입력 함수는 hook이 기록됐으면 거절이다(`src/services/codex/herdr_turn.rs:235-239`, 검사 `:268-275`). resolver 표 첫 칸·셋째 칸 앞에 쓰기 없는 복원 단계(첫 입력 준비 `src/services/codex/herdr_turn/first_input.rs:90-106`, `:140-146`와 Bound 재시작 복원 `herdr_source.rs:305-332` 재사용)를 두고, 그 전에 온 hook은 425로 재시도시킨다 | §0-6, §7.3(재시작 복원·전제 표·같은 부류 전수), S06b, 모델 `RestoreReg`·`BoundOwnSource`·`RestartedPendingResolved`, cfg `CodexNoRestore`·`HookConsumed`·`ForeignSource` RED |
| P2-2 headless `HostUnavailable`의 호출자 정산이 정해지지 않음 | **고침.** 예약 메시지는 오류를 문자열로 바꾼 뒤(`src/services/scheduled_messages.rs:442`) 네 문자열(`:272-281`)만 미룸으로 보고 나머지는 유한 재시도를 쓴다(`:235-258`). routine은 600초 창 뒤 실패로 넘긴다(`src/services/routines/agent_executor/start_deferral.rs:9`, `:57-69`, `:115-126`). 새 variant를 문자열로 바꾸지 않고 호출자까지 넘기며, 생산 호출자 8개(호출 자리 10곳)의 정산을 전수표로 정했다 | §10.2 5·전수, D01 |
| P2-3 모델이 원문 폐기와 재제출을 구별하지 못함 | **고침.** 입력 한 건의 수탁(`owed`: 도착 뒤 답 전)을 추적해 `CustodyKept`(답 전에는 실행 대기 또는 보존)를 두고, `HeldInputSubmitted`의 목표를 보존 해제가 아니라 답(실제 시작·실패·거절)으로, 증인 `NeverResubmitted`를 보존됐던 입력의 실제 시작으로 바꿨다. 폐기 뮤턴트 `DropOnWake`가 `CustodyKept`·`HeldInputSubmitted`를 깬다 | 모델, §14, 보고서 |

### 충돌한 결정과 판정

| 쟁점 | 안 | 판정과 근거 |
|---|---|---|
| 선택 없는 Windows 채널 | B: 삭제 뒤 오류, 운영자가 매핑 / C·A: 내부 기본 Herdr | **C.** 수동 설정 이사는 사용자 결정 위반. 내부 endpoint는 설정에 쓰지 않는다 |
| `herdr_turn_enabled`·`herdr_codex_turn_enabled`·`herdr_cancel_enabled`가 Windows 암묵 기본에 걸리는가 | C: 명시 false면 Windows도 거절 | **셋 다 걸리지 않는다(합류 판정, r2에서 Codex·Cancel로 넓힘).** 세 bool의 현재 뜻은 "endpoint가 설정된 채널의 Herdr 실행·Escape 허가"다(`src/config/runtime_settings.rs:87-96`). 공유 설정의 false가 Windows 노드 전체를 끊거나 Cancel을 막지 않게 한다. 대체되는 process 경로는 Cancel(SIGINT)이 기본으로 됐다. Windows에서 명시 채널 `"tmux"`는 명시적 unavailable. 운영자 pane-close `herdr_terminate_enabled`는 바꾸지 않는다 |
| Windows Qwen | A: Herdr 위 wrapper / B: headless 직접 / C: U10 뒤 결정 | **B.** headless 경로가 이미 있고 cfg 게이트가 없다(`src/services/qwen.rs:417`). 지금 Windows Qwen process 세션도 job으로 dcserver와 함께 끝나므로 연속성이 나빠지지 않는다. 새 호스팅 결합을 만들지 않는다 |
| "입력이 trait을 지난다" | B: #7272 실행기를 tmux 어댑터로 인정(사용자 결정 요청) / C: 실행기는 두고 물리 효과만 호스트 경유 | **C.** 실행기 로직은 옮기지 않고 그 안의 tmux 인자 조립 23곳(B 부록 A3)만 `TmuxHost` 메서드로 바꾼다. DoD 문장 그대로라 설계 이탈이 없다 |
| 재채택 메서드 | B `reattach` / C·A `recover` | **`recover`**, 4값(`SameExecution`·`Gone`·`Withheld`·`Unknown`). "attach"는 쓰지 않는 terminal attach와 헷갈린다 |
| tmux 운영자 terminate | B: `terminate_hosted_session`이 tmux도 닫게 | **기각.** main이 하지 않던 kill 권한이다. 운영자 경로의 tmux는 지금처럼 Unsupported(`src/services/termination_audit/host_terminate.rs:52`) |
| `exec_ops.rs` | B: 비관리 / C: BYPASS | **C.** 정책 엔진 `session.sendCommand`·`kill`이 관리 tmux 이름에 바로 효과를 낸다(`src/engine/ops/exec_ops.rs:394-440`) |
| 비관리 tmux(자기 fallback·로그인·CLI) | B: `TmuxHost` 전용 메서드로 이동 / C: NON_MANAGED 제외 | **C.** ratchet 허용 목록에 고정하고 관리 세션 종료 권한과 묶지 않는다 |
| `HostKind`에 Core | B: `HerdrCore` 추가 / C: F4 태그 | **둘 다.** trait의 `kind()`를 위해 `HostKind::HerdrCore`를 두되 `.host_kind` 표식·`sessions.hosted_execution`에는 쓰지 않는다. 채널 행 파서는 거절 |
| 대역 치환 순서 | B: D0 먼저 / C: D01 뒤 D02 | **B.** 대역을 먼저 바꾸면 삭제 조각이 순수 삭제가 된다 |
| 첫 조각 | C: caller 없는 포트 계약 / B: trait+TmuxHost | **합류.** 포트는 첫 호출자와 함께 착지한다. 첫 조각은 기계적 tmux 종료 이전(S01) |
| Windows 활성화 방식 | B: W1 카나리 채널을 운영 설정으로 켜고 W2에서 나머지 채널 설정 이사 | **기각.** 설정 작업이 수동 스위치·런북이 된다. 활성화는 코드 조각(W09 Claude, W10 Codex)이고, 그 전 실측은 격리 sandbox의 U 패킷이다 |
| Process→Herdr 전환 시 옛 Process 실행 처리 | C: 레지스트리가 살아 있는 동안 자연 종료 뒤 넘김 | **불필요(합류 판정).** Windows에서 새 바이너리 기동은 곧 dcserver 재시작이고 job이 옛 Process 자식을 모두 끝낸다. 첫 입력이 저장된 provider 세션 id로 Herdr를 `--resume`으로 띄운다. tmux 없는 Unix의 옛 고아는 정리하지 않는다(main에 없던 kill) |

## 3. 사실 대조(정정 목록)

모든 file:line은 1974cff8a6에서 다시 열었다. 바뀐 것:

| 출처 | 원래 주장 | 확인 결과 |
|---|---|---|
| 이슈 본문(3c598ff) | `claude.rs:829-843`, `codex/command_streaming.rs:187-205`, `qwen.rs:356-367`, `session_backend.rs:333`, `turn_host.rs:168-195`, `session_host.rs:16-18` | 지금은 `src/services/claude.rs:794-821`, `src/services/codex/command_streaming.rs:184-203`, `src/services/qwen.rs:357-368`, `src/services/session_backend.rs:341`, `src/services/turn_host.rs:220-247`(게이트 `:228`, `:243`), `src/services/session_host.rs:18-20` |
| B §8 A2 | `exec_ops.rs`는 비관리 | 관리 tmux에 효과(`src/engine/ops/exec_ops.rs:394-440`) |
| B §5.2 | `HerdrHost`를 새로 만듦 | 이미 있다(`src/services/session_host/herdr_host.rs:19`, trait 구현 `:210`). 확장한다 |
| C §Core | Core 붙여넣기 뒤 Enter 지연의 근거로 `src/services/discord/turn_bridge/runtime_handoff_loop/guarded_save.rs:154` | 그 줄은 AgentDesk의 저장 코드다. Core 값은 upstream 실측 [U] U16으로 옮긴다 |
| C 전수표 | Windows OS 가정 1446행 | `src/services/platform/windows_job.rs`·`src/cli/dcserver.rs:882`가 빠졌다. [진입점 표](i7583/entrypoints.md) 보충 절에 넣었다 |
| A §3 | 자동 기동은 macOS만 | 맞다(`src/services/session_host/herdr/autostart/spawn.rs:48`) |
| A §5.4 | owner 입력 선택에 `cfg!(unix)` | 맞다(`src/services/discord/queue_io/input_owner/ingress.rs:351-377`, `src/services/discord/router/message_handler/provider_dispatch.rs:320-327`) |
| B §3.3 | 스위치 둘 다 `cfg!(unix) &&` | 맞다(`src/services/turn_host.rs:228`, `:243`) |
| B §7.1 | 가용성 캐시 3회 실패 | 맞다(`src/services/platform/tmux/availability.rs:13-14`: TTL 45초, 임계 3) |
| 합류 r1 전수표(C 원본) | `src/cli/utils.rs:168`·`:186` NON_MANAGED | reset-tmux가 `AgentDesk-*` 관리 세션을 나열·종료한다(`src/cli/utils.rs:166-194`). r2에서 BYPASS로 재분류(관찰·종료 각 1행) |
| 합류 r1 §7.2 | unplaced census를 wake에 걸면 Pending이 스스로 풀린다 | unplaced resolver는 위치·expected가 모두 없는 행만 받고(`src/services/herdr_launch/unplaced.rs:353`), 복구 패스는 Pending을 건너뛴다(`src/services/discord/recovery_engine/herdr_reader.rs:463-464`). 위치 CAS 뒤 확인 못 한 실행 명령은 위치를 남기고 Indeterminate다(`src/services/herdr_launch.rs:533-569`). placed resolver를 새로 둔다(§7.3) |
| 합류 r2 §3·§5.1 | Cancel bool은 Codex Escape에만 걸린다 | **틀렸다(r3 정정).** 생산 Herdr Escape는 Claude·Codex 모두 `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs`를 지나고(`judged_stop.rs:389-402`가 provider 구분 없이 보냄), `herdr_command_eligible`(`:250-262`)이 settlement(`src/services/provider/cancel_token_claude_interrupt.rs:535-541`) 다음 bool을 본다. 기존 시험 `herdr_user_stop_with_the_switch_or_settlement_off_records_and_sends_nothing`(`src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery_tests.rs:691`)이 두 provider 모두에서 이를 고정한다(`:291-309`). Claude Herdr pane stop(`claude_stop_delivery.rs:678`)은 `cfg(test)`뿐이다 |

이 문서의 나머지 좌표는 문서 경로 검사기(`scripts/check_doc_path_refs.py`)로 파일·줄 범위를 확인했다.

## 4. 공통 경계 — `InteractiveSessionHost`

`src/services/session_host/traits.rs:9`의 trait은 이미 관찰(`presence`·`liveness`·`capture_screen`·`current_working_dir`·`execution_pid`)과
`send_text`·`send_keys`·`interrupt`를 가진다. 운영 경로가 이것을 지나지 않는 것이 문제다. 다음 효과를 더한다. 각 메서드는 **첫 호출자와 같은 조각**에서 착지해
dead code를 만들지 않는다.

| 메서드 | 입력 → 결과 | 소유 경계 | 착지 조각 |
|---|---|---|---|
| `create` | 준비된 argv·cwd·env, canonical owner·nonce, 이미 받은 launch permit → `NotSent`/`Created(위치)`/`Indeterminate` | provider가 명령을 준비하고 기존 DB CAS 소유자가 Pending을 설치(`src/services/herdr_launch.rs:482`). 호스트는 생성만. 생성 ACK와 Bound를 합치지 않음 | S04 |
| `submit` | `AuthorizedHostTarget` + 출처·typed payload(텍스트·붙여넣기·키·wrapper frame) → `Confirmed`/`Refused`/`Indeterminate` | raw 세션 이름은 쓰기 권한이 아니다. Herdr mutation gate·tmux first-key gate를 어댑터가 소비. 쓰기 시작 뒤 실패는 `Indeterminate`, 재전송 0 | S03 |
| `interrupt` | 기존 native stop/cancel 소유자가 허락한 실행 대상 → 기존 결과 | Escape/C-c. 턴 중단을 세션 종료로 올리지 않음 | S05 |
| `terminate` | `HostTerminateWarrant`(`src/services/termination_audit/host_terminate.rs:27-48`, Match + 허용 stop 판정에서만 발급) → 소멸 확인 `Gone`/`Refused`/`Indeterminate` | close ACK만으로 소멸이라 하지 않는다. tmux 운영자 경로는 Unsupported 유지 | S05 |
| `recover` | 내구 owner·위치·expected·source + 현재 endpoint witness → `SameExecution`/`Gone`/`Withheld`/`Unknown` | 읽기 전용. 내부에서 create·재전송·추정 kill 0. source/claim/O 게시는 기존 소유자 | S06 |

구현체: `TmuxHost`(`src/services/session_host/tmux_host.rs`), `HerdrHost`(기존 타입 확장, `HerdrLaunchHost`·`HerdrTarget`·`terminate_herdr_once`를 감쌈),
`HerdrCoreHost`(C01). `ProcessHost`는 D04에서 지운다.

**trait 밖에 두는 것**: 채널 → 호스트 선택(`src/services/turn_host.rs:151`), Herdr 허가·동의·fence(`src/services/session_host/herdr_gate.rs`, `policy`),
실행 신원·nonce(`src/services/herdr_launch.rs:466`), hosted row CAS, stop 판정(`src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs`),
입력 단일 경로 소유권(`src/services/discord/queue_io/input_owner/`), O writer. trait은 이들의 판정 뒤에 불린다. 새 transition lock·수동 복구 상태·`kill_pid`/`attach_terminal` 권한을 trait에 넣지 않는다.

**우회 0의 정의(DoD 3).** 관리 세션의 물리 효과(생성·입력·관찰·중단·종료·복구 판독)는 `session_host` 어댑터의 메서드로만 일어난다.
호스트 중립 호출자는 trait(`host_for`)을, tmux 이름만 아는 기존 tmux 전용 호출자는 같은 어댑터(`TmuxHost`)의 메서드를 쓴다.
`platform::tmux`의 세션 함수, tmux 명령 인자 조립, Herdr 요청, provider spawn은 어댑터 밖에서 0이다. 예외는 전수표의 `NON_MANAGED` 28행
(`src/cli/dcserver.rs:831`, `src/server/routes/provider_auth_profiles.rs:339` 등, 자기 fallback·로그인·인증 프로필 세션)과 `TEST_OR_HELPER`뿐이고 ratchet 허용 목록에 좌표로 고정한다.
닫힘의 증거는 [audited-bypasses.tsv](i7583/audited-bypasses.tsv) BYPASS 320행 각각의 호출 체인이 어댑터에 닿는 것이다. 단순 삭제로 0을 만들지 않는다.

## 5. 채널 선택과 기본값

`runtime.herdr_turn_enabled: Option<bool>`(`src/config/runtime_settings.rs:90`) 자리에 untagged `Legacy(bool) | Channels { channels: BTreeMap<ChannelId, "tmux" | "herdr"> }`를 넣는다.
목록에 없는 채널은 `Legacy(false)`와 같다. 모르는 값·필드는 검증 오류. 새 top-level 키·명령은 없다. `herdr_codex_turn_enabled`·`herdr_cancel_enabled`는
endpoint·explicit 채널에서 지금 뜻 그대로 Codex 실행·Codex Escape의 별도 게이트다. PlatformDefault에는 적용하지 않는다(§5.1).
endpoint의 위치는 기존 `session_hosts`(`src/config/session_hosts.rs:350`)가 소유한다.

선택은 하나의 `ChannelHostPlan { host, endpoint, source: Explicit | Endpoint | PlatformDefault, permits }`으로 정규화하고, 생성(`src/services/turn_host.rs:151`)·
부팅 복구·Cancel·Terminate·clear·채널 제외 목록이 **같은 값**을 읽는다. desired host일 뿐이고, 살아 있는 실행의 current host는 내구 witness가 정한다.
설정 변경만으로 살아 있는 실행을 덮어쓰지 않는다(이동은 기존 handoff/return 경계).

| 플랫폼 | endpoint | 선택 | main | 최종 설계 | 바뀌는 조각 |
|---|---|---|---|---|---|
| unix | 없음 | 없음/옛 bool | tmux | tmux | — |
| unix | 있음 | Claude, bool true 또는 채널 `"herdr"` | Herdr | Herdr | — |
| unix | 있음 | Claude, false/None | 입력 전 거절(`ExecutorNotWired`) | 같음 | — |
| unix | 있음 | Codex, 게이트 false/None | unsupported | 같음 | — |
| unix | 없음 | 채널 `"herdr"` | (표현 없음) | IO 전 거절, endpoint 없음 사유 | W01 |
| unix | 있음 | 채널 `"tmux"` | (표현 없음) | tmux. 살아 있는 Herdr 실행은 기존 return/handoff 경계(`src/services/turn_host.rs:102`)에서 옮김 | W01 |
| unix, tmux 없음 | 없음 | 없음 | process 폴백 | **router admission의 내구 hold, 모든 provider·출처 원문 보존(§10.2)** | D01 |
| windows | 없음 | 없음, Claude/Codex | process | **Herdr(내부 dedicated endpoint), 실행·Escape 허가 포함(§5.1)** | W09(Claude)·W10(Codex) |
| windows | 있음 | true/`"herdr"` | 거절(`cfg!(unix)`) | Herdr | W09 |
| windows | 무관 | 채널 `"tmux"` | (표현 없음) | 명시적 unavailable | W09 |
| windows | 없음 | Qwen | process | headless 직접 실행 | D01 |
| 아무 플랫폼 | 위 결과 호스트를 지금 쓸 수 없음(Herdr 서버 down·미설치, tmux 일시 부재 3회 초과) | 무관 | (process 폴백 또는 오류) | admission hold + 원문 보존 + 호스트 복귀 wake. tmux로 새지 않음 | D01·W09 |

Unix의 모든 줄은 W01 이후에도 같다. 근거는 §5.2의 표 기반 시험이다(r1의 "모델 상태 수 991=991"은 근거에서 뺐다 — 상태 수가 같다고 선택 결과가 같다는 뜻이 아니다).
바뀌는 두 줄(tmux 없는 Unix, Windows 무선택)은 ProcessBackend 삭제가 강제하는 변화이며 숨기지 않는다. 내부 Windows endpoint는 runtime root와 실행 노드에서 결정하는 키 `windows-managed-default/v1`과
고정 named session으로 부팅마다 재구성하고 설정에 저장하지 않는다. `legacy_dedicated` 모드만 쓴다(`user_default`는 어댑터 없는 플랫폼에서 부팅 거절,
`src/config/session_hosts.rs:193`). Herdr 미설치·접속 불가는 생성 전 명시적 오류와 원문 보존이며 자동 설치·자동 demotion은 없다.

### 5.1 허가 정규화 — 실행·Escape를 선택과 함께 정한다

main의 세 bool은 모두 "endpoint가 설정된 채널"의 Herdr 허가다(`src/config/runtime_settings.rs:87-97`). 읽는 곳은 실행
(`src/services/turn_host.rs:220-247`, Codex 거절 `:288-295`)과 Escape(`src/services/provider/cancel_token_claude_interrupt.rs:544-563`)다.
Escape의 생산 소비자는 하나의 전달자(`src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs:801`)로 가는 두 검사
(`herdr_command_eligible` `:250-262`, `interrupt_herdr` `:589-600`, 그리고 `:641`, `:796`)이고, Claude·Codex 턴 토큰이 모두 이 경로를 쓴다
(Claude 토큰 설치 `src/services/claude/herdr_turn.rs:240-246`, 진입 `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:370`, `:401`, `:471`).
이 함수들이 설정을 직접 읽으면 PlatformDefault 실행도 `unwrap_or(false)`에 걸려, 기본 설정의 Windows Codex는 실행이 거절되고(삭제 뒤에는 쓸 호스트가 없음) Windows Escape는 버려진다.
W01은 `ChannelHostPlan.permits { run, escape }`를 선택과 **같은 함수**에서 계산하고, 위 두 함수는 그 값을 읽는 얇은 접근자로 바뀐다. Unix 값은 바뀌지 않는다.

| source | Claude 실행 | Codex 실행 | Claude Escape | Codex Escape | 조각 |
|---|---|---|---|---|---|
| Endpoint / Explicit, unix | `herdr_turn_enabled`(Legacy bool) 또는 채널 `"herdr"` | `herdr_codex_turn_enabled` | settlement ∧ `herdr_cancel_enabled` | settlement ∧ `herdr_cancel_enabled` | W01(값 무변화) |
| Endpoint / Explicit, windows | 같음 | 같음 | settlement(W08 이식) ∧ `herdr_cancel_enabled` | 같음 | W09·W10 |
| PlatformDefault(windows 내부 endpoint) | 허가 | 허가(bool 무시) | settlement(bool 무시) | settlement(bool 무시) | W09·W10 |
| 호스트 없음 / 채널 `"tmux"`인 windows | admission hold(§10.2) | 같음 | — | — | D01·W09 |

- 표는 stop 소유 경로가 아니라 **이 하나의 Herdr Escape 전달 경로**의 허가다. 다른 stop 경로(tmux Claude interrupt, process 취소 등)에는 bool을 새로 걸지 않는다.
- Escape 허가는 **실행의 내구 endpoint 키**로 고른다. 내부 키 `windows-managed-default/v1`로 만든 실행만 PlatformDefault 허가(bool 우회)를 받는다.
  설정이 나중에 바뀌어도 그 실행은 그 허가로 취소된다(desired host와 current host 분리 §5). 운영자 pane-close `herdr_terminate_enabled`는 바꾸지 않는다.
- settlement(`src/services/provider/cancel_token_claude_interrupt.rs:535-541`)는 `cfg!(unix)`다. Windows는 W08이 terminal settlement를 이식한 뒤 참이 되고,
  그 전에는 Windows Herdr 실행 자체가 열리지 않는다(W09가 W08 뒤). 이 값이 거짓인 Windows 실행이 생기지 않으므로 Escape가 조용히 꺼지는 칸은 없다.
- **다른 게이트 점검**(리뷰 집중 2): Windows Claude·Codex 실행·Escape를 막는 지점은 위 두 함수 외에 owner 입력 `cfg!(unix)`(`src/services/discord/queue_io/input_owner/ingress.rs:351-377`,
  `src/services/discord/router/message_handler/provider_dispatch.rs:320-327`), `#[cfg(unix)]` 판정(`src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:364`, `:397`, `:469`, `:480-490`),
  Codex settlement 분기(`src/services/codex/herdr_turn.rs:132`)다. W09가 앞 둘, W08이 settlement 함수의 Windows판(terminal settlement 이식)을, W10이 Codex 실행 허가를 연다.
  PlatformDefault 계획의 host는 언제나 Herdr이고 `Pick`에 tmux 분기가 없다. Herdr를 못 쓰면 tmux가 아니라 admission hold다(모델 `GateRun`·`GateCancel` RED, `WindowsImplicit`·`CodexImplicit` GREEN).
- 기본값 보존 근거: Unix와 endpoint 채널의 세 bool은 None/false/true 모두 지금 결과 그대로다(Escape도 두 provider 모두 bool을 따른다, 모델 `UnixCancelAsMain`, 뮤턴트 `EscapeNoSwitch` RED).
  PlatformDefault가 대신하는 Windows process 경로는 실행·Cancel(SIGINT)이 기본으로 열려 있었다. bool을 PlatformDefault에 걸면 오히려 Windows 기본 동작이 "실행 거절·Cancel 없음"으로 바뀐다. 새 스위치는 없다.

### 5.2 기본값 동일성의 증명 방식

- **정본은 표 기반 시험이다(W01).** 생산 선택 함수(`ChannelHostPlan`을 만드는 단일 함수, 플랫폼을 인자로 받음)에
  (플랫폼 2 × endpoint 유무 × 선택 값 {없음, `Legacy(true)`, `Legacy(false)`, 채널 `"herdr"`, 채널 `"tmux"`, 목록 밖 채널} × provider {Claude, Codex, Qwen} × Codex bool × Cancel bool × tmux 가용)
  전 조합을 넣고, 기대값은 §5·§5.1 표를 옮긴 **리터럴 표**로 둔다(코드로 계산하지 않는다). 표에 없는 조합이 생기면 실패한다(완전성 검사).
  Unix 칸의 기대값은 main의 현재 결과(`src/services/turn_host.rs:151` 결과·거절 사유)와 같은 문자열이다.
- 생산 경계 시험: (1) Unix endpoint + 실행 bool false가 pane IO 전에 거절된다(기존 거절 사유 단언 유지), (2) Windows 기본 Codex 계획이 bool false에서도 Herdr·run·escape 허가다,
  (3) **Escape 표**: 기존 `begin_user_stop` 생산 시험(`src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery_tests.rs:691`, `with_cases` `:291-309`)을 유지하고
  실제 설정 None/false/true를 표로 더한다 — Unix endpoint Claude·Codex: None/false → `SwitchOff`·Escape 0·stop intent 미기록, true → Escape 1. Windows PlatformDefault Claude·Codex: None/false에서도 Escape 1.
- 판별력: 뮤턴트 (a) PlatformDefault에 `unwrap_or(false)`를 다시 적용, (b) endpoint + false를 tmux로 보냄, (c) 표 한 줄 삭제, (d) Unix Claude만 Cancel bool 생략 — 넷 다 표 시험이나 Escape 표가 죽인다(커밋하지 않음).
- 모델은 보조다. 모델이 보이는 것은 "고른 호스트에서 재시작·Pending·종료 계약이 성립하는가", "PlatformDefault 실행이 bool 때문에 거절·Escape 유실되지 않는가"(`GateRun`·`GateCancel`),
  "Unix endpoint의 Escape가 bool off에서 꺼진 채인가"(`UnixCancelAsMain`)뿐이며,
  §5 표 전체의 동일성은 보이지 않는다. 보고서도 이 범위로 좁혔다.

## 6. Windows 전송·서버 수명·신원

| 막힌 곳 | 좌표 | 조각 |
|---|---|---|
| 전송: `UnixStream`, 호출마다 새 연결 | `src/services/session_host/herdr/transport.rs:6` | W02: named pipe 클라이언트. 논리 socket path → `GenericNamespaced` 변환은 upstream 규약을 따르고 실제 이름은 U01에서 왕복 확인. write 전 실패 `NotSent`, write 뒤 끊김·시간 초과 `Indeterminate`, mutation 재시도 0 |
| 서버 신원: peer PID·시작 시각 | `src/services/session_host/herdr/provenance.rs`, `StartIdentity`·`boot_time` `cfg(unix)` `src/services/session_host.rs:75-76` | W03a: `GetNamedPipeServerProcessId` + `GetProcessTimes` raw FILETIME + OS boot 신원. 표시용 시작 문자열을 신원으로 쓰지 않음 |
| pane 점유·E7·loaded image | `src/services/session_host/herdr/pane_probe.rs`, `src/services/session_host/herdr/observe.rs:107`, `src/services/session_host/herdr/policy/loaded_image.rs` | W03b: hook spawn nonce + root/provider tree로 점유 증거(foreground group 미지원, U09). `resume_agents_on_restore=false`의 실제 서버 증명. 증명 못 하면 Withheld |
| 서버 시작·수명 | `src/services/session_host/herdr/autostart/spawn.rs:48`, `:238`; job `src/services/platform/windows_job.rs:35` | W04: 서버를 dcserver job 밖에서 띄운다. 우선순위 (1) herdr CLI의 자체 기동이 job을 벗어나는지(U04-c) (2) dcserver job에 `JOB_OBJECT_LIMIT_BREAKAWAY_OK` + 서버만 `CREATE_BREAKAWAY_FROM_JOB`(wrapper·provider 자식은 job에 남음, 기존 `forced_runtime_exit_terminates_grandchildren` 유지) (3) WMI `Win32_Process.Create`. 셋 다 안 되면 Windows 활성화 중단(DESIGN_DELTA) |
| 관리 설정 파일 | `src/services/session_host/herdr/managed_config.rs` | W04: 권한 비트 대신 디렉터리·소유 검사 |
| launch 명령 | `src/services/herdr_launch.rs:353-375`(bash·`exec`·`unset`) | W06: `.cmd` 런처, HERDR 변수 제거, 공백·한글 cwd, npm shim 인용(U05) |
| 파일 신원·잠금 | `MetadataExt` dev·inode 29파일, `src/services/claude_tui/hook_relay/ordered_queue.rs` flock | W05: 공통 헬퍼(Windows는 volume serial + file index), `std::fs::File::lock` |
| retired hold | `src/services/herdr_launch/retired_hold.rs`, non-unix 오류 `src/services/herdr_launch.rs:444-451` | W03b |
| endpoint 검증 | `src/config/session_hosts.rs:229-238` 절대 경로 요구 | W01: Windows 논리 주소 허용 |

Windows 미지원(terminal attach, live server handoff, FD handoff, foreground group, 서명 바이너리) 중 재채택이 기대는 것은 없다.
Windows에서 Herdr 서버 재시작·업데이트는 모든 pane을 끝낸다(live handoff 없음). 이것은 dcserver 재시작이 아니라 호스트 소실이다(§7 4행).
권한: dcserver와 Herdr는 같은 Windows 계정·같은 elevation을 전제한다. `allow_unelevated_clients`를 켜지 않는다(U06).

## 7. 재시작·부팅·incarnation

### 7.1 incarnation 종류 (합치지 않는다)

| 신원 | 저장 | 바뀌는 때 | 쓰는 판정 |
|---|---|---|---|
| dcserver 부팅 | 메모리·로그 | dcserver 재시작 | 레지스트리·watcher 재구축. 실행 변경을 뜻하지 않음 |
| Herdr 서버 | witness(pipe 서버 PID + 생성 FILETIME + OS boot) | 서버 재시작·업데이트·재부팅 | 다르면 server-replaced 경로 |
| 실행 nonce | hosted row + 작업공간 label `논리키#nonce`(`src/services/herdr_launch.rs:466`, `:733`) | 새 create | 재채택 동일성, 늦은 callback 무효 |
| pane·root/provider | hosted row location·expected(`src/services/herdr_launch.rs:554`) | pane 교체·provider 재실행 | root-replaced 경로 |
| provider 대화 | native session id | `/clear`·새 세션 | 서버 소실 뒤 `--resume` |
| OS boot | boot 신원 | 재부팅 | PID 재사용을 absent로 치환하지 않음 |

### 7.2 경로표

| 경로 | 좌표 | 최종 동작 |
|---|---|---|
| 새 실행 | `src/services/herdr_launch.rs:378`, Pending CAS `:482`, 생성 `:515`, 위치 CAS `:554` | 선택·기존 witness 읽기 → nonce → Pending → `host.create` → 위치·expected → native source Bound. 생성 응답 유실은 Pending으로 남아 unplaced census(`src/services/herdr_launch/unplaced.rs`)가 정리. 두 번째 create 0(모델 `ReplayPending`) |
| dcserver만 재시작 | Windows job `src/services/platform/windows_job.rs:35` | Herdr 서버·pane·ConPTY는 job 밖이라 산다(W04). 내구 행·nonce·O cursor가 남는다 |
| 부팅 순서 | `src/services/discord/runtime_bootstrap/queued_recovery.rs:26` | pending 큐를 inflight보다 먼저 복원(재시작 틈 입력을 알려진 메시지로 오판하지 않음) → owner·generation → 저장 호스트 판독·재채택 또는 hold → writer·source·cursor → 입력 owner 정산 → wake |
| Herdr 재연결 | `src/services/discord/tui_prompt_relay/rehydration.rs:270`, `:668` → `src/services/discord/recovery_engine/herdr_reader.rs:220` | 새 pipe 연결로 snapshot·process info, witness 비교 → `host.recover` → `attach_restarted_herdr_source`(`src/services/discord/tui_prompt_relay/herdr_source.rs:274`). provider spawn 0, terminal attach 0, payload 재전송 0. W08 수용 시험이 Windows 생산 부팅 진입점에서 이 호출 도달을 단언(모델 `NoWake`) |
| intake 열기 | `src/services/discord/runtime_bootstrap.rs:35`, standby `src/services/discord/runtime_bootstrap.rs:472`; 로그 `src/services/discord/runtime_bootstrap/recovery_flush.rs:118-131` | active reconcile 뒤 wake. standby는 주입 0 |
| Unknown 판독 | `src/services/session_host/model.rs:52`, `src/services/discord/recovery_engine/host_reconcile.rs:81` | 행 보존, health·rehydration 주기 재판독. 죽었다고 단정하지 않음 |
| Pending 탈출(위치 없음) | `src/services/herdr_launch/unplaced.rs:353`, 복구 패스 `src/services/discord/recovery_engine/herdr_reader.rs:377-392` | 기존 unplaced resolver(nonce census·in-flight·quiet). 무변화 |
| Pending 탈출(위치 있음) | 건너뜀 `src/services/discord/recovery_engine/herdr_reader.rs:463-464`, 위치 기록 `src/services/herdr_launch.rs:533-569`, 안내문 `src/services/claude/herdr_turn/never_attached.rs:57-76` | **§7.3 placed resolver(S06b)**. boot·health 복구 패스가 다음 사용자 입력 없이 끝낸다(provider가 살아 있으면 Pending을 지키고 기존 첫 입력 경로로 잇는다). 안내문의 `agentdesk herdr terminate`/`retire`는 정상 탈출 경로가 아니다. resolver가 결론을 못 낸 동안(Unknown)만 거절 안내가 나가고, 그 문구는 수동 명령 대신 "자동 정리 대기"로 바꾼다(S06b) |
| Herdr 서버 소실·재부팅 | `src/services/discord/recovery_engine/herdr_server_replaced.rs:246`, `src/services/discord/recovery_engine/herdr_row_end.rs` | witness 불일치·부재 → 서버 재기동(W04) → 옛 실행 Retired + 알림 1회 → 대기 큐 wake → 다음 입력이 `--resume <id>`로 새 nonce 실행. 돌던 턴만 잃고 대화는 이어진다. Herdr 자체의 agent 자동 resume은 E7 off로 막아 이중 실행 0 |
| 옛 inflight 행 | `src/services/discord/recovery_engine/restore_inflight.rs:226`, `:985`; `src/services/discord/inflight/model.rs:1367` | D03: `"process_backend"` 문자열은 읽기 전용 legacy 값으로 남겨 "세션 없음"으로 닫고 알림 1회. 모르는 값으로 떨어져 조용히 건너뛰지 않게 한다. tmux로 추정하지 않는다 |
| 첫 Windows 전환 | O writer `src/services/tui_o/writer/renumbered.rs`, `src/services/tui_o/writer/rotation.rs`; resume 판정 `src/services/claude/herdr_turn/handoff.rs:38-43`, `src/services/claude/session_handoff.rs:155` | 유효한 Fresh owner → writer 빈 초기화 → 실제 checkpoint 0 저장 → input-ready → create와 첫 입력. O 파일에 0을 직접 쓰지 않는다. process 시절 Claude 세션은 O가 그 transcript의 커서를 갖지 않아 main 규칙대로 resume이 `Err`가 되고, 그대로 두면 채널의 **모든 입력이 시작 실패**한다. W09는 이 경우를 (1) 기존 writer adoption(#7651 단계 1, 파일 끝에서 commit)이 그 transcript를 채널 자기 bind로 묶어 `o_reads`가 참이 되면 resume, (2) 아니면 기존 `src/services/claude/fresh_notice.rs` 알림 1회와 fresh 실행으로 처리한다. 어느 쪽인지는 U20. 재게시·오류 반복·수동 `/clear` 요구 0. Codex Herdr 실행은 main에서 항상 fresh(`src/services/codex/herdr_turn.rs:194`)라 Windows Codex도 전환 때 새 대화로 시작한다(기존 Herdr Codex 제약, 넓히지 않음) |
| 채널 호스트 이동 | `src/services/turn_host.rs:102-121`, `src/services/claude/herdr_return.rs`, `src/services/claude/session_handoff.rs` | Unix 무변화. Windows는 tmux가 없어 Return이 생기지 않음 |
| `/restart`·tmux 재생성 | `src/services/discord/commands/restart.rs`, `src/services/discord/commands/tmux_recreate.rs:57` | 무변화(tmux 전용) |

### 7.3 위치가 기록된 Pending — 자동 탈출 계약

**왜 남는가.** create가 pane을 돌려줬지만 실행 명령 확인이 끊기면(`CreatedUnconfirmed`) 위치 CAS 뒤 Indeterminate로 끝난다(`src/services/herdr_launch.rs:533-569`).
행은 위치가 있는 Pending이다. main에서 이 행을 끝내는 길은 셋뿐이다: 다음 입력이 never-attached 거절로 수동 명령을 안내(`src/services/claude/herdr_turn/never_attached.rs:57-76`),
user_default endpoint에서 재부팅 뒤의 server-replaced(`src/services/discord/recovery_engine/herdr_server_replaced.rs:26-31`, `:238-262`, Darwin boot만),
운영자 명령. Windows는 `legacy_dedicated`·FILETIME boot라 둘째도 닿지 않는다. 사람 없이 끝나지 않으면 그 채널 입력이 영영 행 뒤에 묶인다(모델 `UnixHerdrCurrent`·`SkipPlaced` RED).

**세 상태를 나눈다.**

| 상태 | 식별 | 처리 |
|---|---|---|
| 위치 없음 | `location = None ∧ expected = None` | 기존 unplaced resolver(무변화) |
| 위치 있음, provider 미확인 | `location = Some ∧ expected = None` | 아래 placed resolver |
| expected 있음, source 미연결 | `expected = Some`, 행 Pending | 같은 placed resolver |

**계약: provider 증명 ≠ source 증명.** main에서 행을 Bound로 쓰는 곳은 셋뿐이고 모두 자기 source가 있어야 한다:
`attach_launched_herdr_source`(nonce baseline 필요, `src/services/discord/tui_prompt_relay/herdr_source.rs:154-197`),
`attach_launched_codex_herdr_source`(`src/services/discord/tui_prompt_relay/herdr_source.rs:210-244`), Claude `bind_once_logged`(`src/services/claude/herdr_turn.rs:860-873`).
resolver는 새 Bound 작성자를 만들지 않는다. provider가 살아 있어도 자기 source가 없으면 Pending을 지키고, 같은 nonce의 **기존 첫 입력 경로**
(Claude `pending_source` `src/services/claude/herdr_turn.rs:395-428`, Codex `pending_launch` → `first_prompt` `src/services/codex/herdr_turn.rs:156-166`, `:228-261`)가 잇는다.
Codex는 첫 hook을 첫 prompt와 함께만 보내므로(`src/services/codex/herdr_turn.rs:266-274`) 첫 입력 전 Codex에 source가 생기지 않는다.
source 없는 Bound는 다음 입력이 모두 거절된다(`src/services/codex/herdr_turn.rs:515-531`)—r2 처방의 결함이다.

**placed resolver** (새 `src/services/herdr_launch/placed.rs`(제안), 호출은 `src/services/discord/recovery_engine/herdr_reader.rs`의 boot·health 패스가 Pending을 건너뛰던 자리):

1. 그 행의 transition lease를 owner·nonce·endpoint·서버 witness·pane 신원으로 잡는다. 같은 채널에 in-flight 턴·진행 중 create·close가 있으면 이번 패스는 넘긴다.
2. 서버 witness가 행을 만든 서버와 다르면 server-replaced 경로로 넘긴다. Windows는 live handoff가 없어 옛 pane이 남지 않으므로
   W08이 `legacy_dedicated`에도 그 경로를 열고, 재부팅 조건 없이 witness 불일치를 소실로 본다(Unix user_default의 재부팅 조건은 그대로).
3. 같은 서버에서 `host.recover`로 판독하고 아래 표의 한 칸으로 끝낸다. "첫 입력 전"의 증거는 receipt 부재가 아니라
   **input hold 파일 부재**(첫 write 전에 기록·fsync된다, `src/services/claude/herdr_turn.rs:566-580`, 검사 `:545-564`)와 Codex의 nonce hook 부재(`hook_logged`)다.

| 판독 | 결론 |
|---|---|
| provider 증명(hook spawn nonce 또는 root/provider tree가 행 nonce와 일치), 자기 source 기록됨, 첫 입력이 이미 닿음(Codex nonce hook 기록 — 첫 prompt 뒤 attach 전에 끊긴 경우) | 먼저 아래 **재시작 복원**(source 있음 갈래)으로 이 nonce의 메모리 등록과 runtime binding을 되살리고, 기존 Codex attach(`attach_launched_codex_herdr_source`)로 Bound. 복원이 거절되면 Bound 0·hold 유지로 이번 패스를 넘긴다. create 0, 재전송 0. 남은 hold는 nonce hook이 제출 증거이므로 기존 `release_hold`(`src/services/claude/herdr_turn.rs:498-510`)로 끝낸다(hook 없이는 끝내지 않음) |
| provider 증명, 첫 입력 전(hold 없음, Codex nonce hook 없음; Claude는 자기 SessionStart 기록) | expected만 기록(턴이 하던 같은 probe: Claude `never_attached::probe_again`, Codex `launch_herdr_session`의 생성 금지 probe `src/services/codex/herdr_turn.rs:254-261`)하고 **Pending 유지**. 대기 입력이 있으면 큐를 깨워 기존 첫 입력 경로가 같은 pane·nonce로 첫 paste 1회 → hook·source attach → Bound. create 0 |
| provider 증명, hold 있음, source 없음(첫 write 뒤 hook 전) | paste 0, close 0, 행·hold 유지. 재시작 뒤면 먼저 아래 **재시작 복원**(hold만 갈래)으로 실행 목록·provider 매핑·placeholder를 되살려 hook이 이 실행으로 기록될 수 있게 한다. hook이 기록되면 다음 패스가 첫 칸으로 끝낸다. hook이 끝내 오지 않으면 main의 hold 계약 그대로(자동 해제 없음 `src/services/claude/herdr_turn.rs:557-564`, 해제는 행 끝 `src/services/discord/recovery_engine/herdr_row_end.rs:495`) — 넓히지 않음 |
| Claude provider 증명, 자기 SessionStart 없음, quiet 경과, hold 없음 | 미시작 잔여: residue warrant로 close → retire + wake. 같은 대기 입력의 자동 재기동은 1회이고, 다시 미시작이면 기존 start failure 경로(알림 1회, owner 원문 복원)로 넘긴다. Codex에는 이 칸이 없다(첫 입력 전 hook 없음이 정상) |
| pane 없음, 그리고 nonce 작업공간 census에 `논리키#nonce` 없음 | retire + 큐 wake. kill 0. hold가 있으면 행 끝이 해제한다 |
| pane에 다른 nonce(pane id 재사용) | 자기 nonce 작업공간이 census에 없으면 위와 같이 retire, 남의 pane에는 효과 0. 다른 pane에서 찾으면 같은 nonce의 위치 CAS 뒤 한 번 더 판정 |
| 자기 nonce 작업공간에 셸만(provider 자식 없음), quiet 경과 | residue warrant로 `host.terminate` → pane 부재 재확인 → retire + wake |
| Unknown(probe 오류·시간 초과) | 행·원문 유지, 다음 패스(기존 `UNPLACED_RETRY` 30초)에서 재판독. 죽었다고 단정하지 않는다 |

4. 결론마다 한 번만 기록·알림한다. retire 뒤 대기 큐가 깨어나 새 nonce로 create한다.

**재시작 복원(r4).** dcserver 재시작은 메모리에만 있는 네 가지를 비운다: 실행 목록(`HERDR_EXECUTIONS`, `src/services/tui_prompt_dedupe/runtime_binding/adopt_skip.rs:102`),
provider 매핑(`register_provider_session`, `src/services/tui_prompt_dedupe/runtime_binding/binding_access.rs:97`), Codex placeholder·채널(`register_codex_herdr_placeholder`,
`src/services/tui_prompt_dedupe/runtime_binding/codex_hook.rs:31-54`), runtime binding. 내구로 남는 것은 binding 로그(nonce baseline)·hold 파일·행이다.
첫 prompt를 받은 Codex Pending은 재시작 뒤 이 등록 없이는 아래 전제 표의 어느 경로로도 Bound에 닿지 못한다(모델 `CodexNoRestore` RED).
첫 입력 함수는 hook이 기록됐으면 거절하므로(`src/services/codex/herdr_turn.rs:235-239`) 다시 불러 메모리를 채울 수도 없다.

| 기존 경로 | 메모리 전제 | 재시작 뒤 등록이 비었을 때 |
|---|---|---|
| Codex attach `attach_launched_codex_herdr_source`(`src/services/discord/tui_prompt_relay/herdr_source.rs:210-244`) | live runtime binding이 nonce baseline과 같은 source(`:231-238`) | NotPublished + withhold, 행은 영영 Pending |
| 재시작 복원 `attach_restarted_herdr_source`(`herdr_source.rs:274-337`) | `admits_reconnect()` = Bound Match뿐(`src/services/discord/recovery_engine/host_reconcile.rs:96-98`) | Pending은 Refused. 이 판정은 넓히지 않는다 |
| Codex hook ingress `observe_codex_herdr_hook`(`codex_hook.rs:324-342`) | 실행 목록에 같은 nonce(`:333-335`) | None → 이 실행으로 기록하지 않고 다른 경로로 넘어간다(`src/services/claude_tui/hook_server/observation_ingress.rs:187-211`). 소비될 수 있다 |
| 그 안의 기록 `observe_codex_hook_under_source_authority`(`codex_hook.rs:345`) | runtime binding(placeholder 포함)과 채널 일치(`:372-399`) | binding 없음 → `Unavailable(RestoreNotReady)`(`:388-390`), 425 재시도(`observation_ingress.rs:291-320`) |
| verified Codex ingress(`src/services/tui_prompt_dedupe/runtime_binding/codex_verified.rs:631-644`) | 문맥 검증과 채널 등록(`:637-644`) | `RestoreNotReady`·`ChannelNotRestored`, 425 재시도 |
| Codex 첫 입력 `pending_launch`(`src/services/codex/herdr_turn.rs:228-261`) | — | hook 기록 뒤 거절(`:235-239`). 첫 칸의 복구 수단이 아니다 |

복원 단계는 resolver(`placed.rs`)의 작은 내부 helper 하나다. 순서: 행 transition lease(위 1) → 판독(위 3) → **복원** → 기존 attach 또는 hook 대기.

1. 검증은 첫 입력 준비와 같다(`src/services/codex/herdr_turn/first_input.rs:90-106`): listing order 잠금과 source authority 아래에서 행이 Pending, owner·nonce 일치,
   pane의 spawn nonce marker = 행 nonce. 하나라도 어긋나면 복원 0, 결론 Unknown(다음 패스).
2. **hold만 갈래(source 없음, 셋째 칸).** `restore_herdr_execution_under_source_authority`(목록), `register_provider_session("codex", logical, logical)`,
   `register_codex_herdr_placeholder(logical, channel)` — 첫 입력이 `first_input.rs:140-146`에서 부르는 같은 세 호출이다. 이 갈래는 메모리만 바꾼다(쓰기 0).
   그 뒤 같은 실행의 hook이 기존 ingress에서 source를 기록하고, 다음 패스가 첫 칸으로 attach한다.
3. **source 있음 갈래(첫 칸).** nonce baseline이 이 nonce의 source를 가리킬 때만, Bound 재시작 복원의 등록부(`herdr_source.rs:305-332`)를 그대로 쓴다:
   live binding이 없으면 `codex_restored_binding`(`:256`)으로 만들고 `register_restored_codex_pane_under_source_authority`
   (`src/services/tui_prompt_dedupe/runtime_binding/pane_registration.rs:249-258`, 그 경로가 아직 같은 파일일 때만, 새로 기록하는 사실 없음)로 등록,
   live binding이 다른 source면 아무것도 바꾸지 않고 거절(`herdr_source.rs:310`의 `Some(_) => return None`), 이어 `restore_execution_fence`. 목록·provider 매핑·placeholder는 2와 같이 되살린다.
   그 다음에만 기존 `attach_launched_codex_herdr_source`가 Pending CAS로 Bound를 쓰고, Bound 뒤에만 기존 `release_hold`가 돈다.
4. `admits_reconnect()`는 넓히지 않는다. helper는 새 Bound 작성자가 아니고 행·hold·pane에 쓰지 않는다. Bound 작성자는 여전히 main의 셋이다.

**쓰기 없음의 범위.** DB 행·hold 파일·pane·큐에 쓰지 않고 binding 로그에 새 사실을 남기지 않는다(`pane_registration.rs:247-248`).
source 있음 갈래의 등록은 Bound 재시작 복원과 같이 로컬 Codex 커서 checkpoint에 복원한 같은 값을 다시 저장한다(`src/services/tui_prompt_dedupe/runtime_binding/codex_cursor.rs:303-307`).
값이 같아 멱등이며 새 판정 입력이 아니다. DB 쓰기는 그 뒤 기존 attach의 `bind_pg` 하나뿐이다(main과 같음).

**남의 source를 받지 않는다.** 목록은 같은 pane에 다른 nonce가 있으면 바꾸지 않고 거절한다(`adopt_skip.rs:213-226`, Occupied는 같은 nonce일 때만 참).
source는 이 nonce의 baseline에서만 오고, live binding이 다른 source면 거절, attach는 baseline = source ∧ live = source를 다시 본다(`herdr_source.rs:231-238`).
행 nonce·spawn marker 검증이 1에 있으므로 다른 실행·다른 세션의 transcript를 이 행에 묶는 경로가 없다(모델 `ForeignSource` RED `BoundOwnSource`).

**이중 등록 없음.** 같은 채널의 in-flight 턴이 있으면 resolver는 그 패스를 넘기므로(위 1) 첫 입력과 복원은 겹치지 않는다. 겹쳐도 세 호출은 멱등이다:
목록 Occupied 같은 nonce → 참, provider 매핑은 같은 값 덮어쓰기, placeholder는 `entry().or_insert`(`codex_hook.rs:49-52`)라 live binding을 덮지 않고, 채널은 같은 값이다.
source 있음 갈래는 live binding이 같은 source면 그 binding을 그대로 쓴다(`herdr_source.rs:309`, unread cursor 보존). placeholder·채널이 두 벌 생기는 자료 구조가 없다(맵 키 = 논리 키).

**복원 전에 온 hook.** 재시작 뒤 resolver가 그 행을 처음 판정하기 전에도 hook은 올 수 있다. 지금은 목록이 비어 이 실행으로 기록되지 않고 넘어간다.
`observe_codex_herdr_hook`은 이번 부팅의 placed 판정이 끝나지 않은 논리 키(부팅 첫 복구 패스 전이면 모두)에서, 자기 runtime root의 Herdr 실행 문맥인데 목록에 없으면
`Unavailable(RestoreNotReady)`를 돌린다(425, 같은 request id 재시도). 판정 대기 집합은 메모리 static이고 hook 경로는 IO 없이 읽는다.
resolver가 그 키를 결론 내면(복원·retire·withhold) 집합에서 뺀다. 그 뒤의 미등록 hook은 main 그대로다. 모델 `HookConsumed` RED가 이 재시도를 뺀 반례다.
"첫 복구 패스 전"은 결론과 무관하게 부팅 복구 패스가 한 번 끝나기 전이다. 그 사이에는 Bound 실행의 미등록 hook도 425로 재시도된다(main은 넘김).
첫 패스의 `attach_restarted_herdr_source`가 Bound를 목록에 올린 뒤 같은 hook이 기록되므로 바뀌는 것은 지연뿐이다(의도). Unknown으로 남은 키는 결론까지 판정 대기에 남는다.

**같은 부류 전수(r4).** 재시작이 메모리 등록을 비워 기존 경로가 NotPublished·거절·무기록으로 끝나는 곳을 실행 목록·runtime binding·채널 reader에서 모두 찾았다.

| 경로 | 재시작 뒤 결과 | 처리 |
|---|---|---|
| Codex placed Pending attach(위 표 첫 행) | NotPublished | 복원 단계(이번 P1) |
| Codex hook ingress(위 표 셋째·넷째 행) | 무기록 통과 또는 425 | 판정 대기 동안 425, 복원 뒤 기록 |
| Claude resume 실행의 SessionStart `resume_start::record`(`src/services/claude_tui/hook_server/observation_ingress/resume_start.rs:27-34`) | `herdr_resume_launch`가 목록을 요구해(`src/services/tui_prompt_dedupe/binding_events/herdr_resume.rs:7-15`) 목록이 비면 `own` 거짓 → 기록 없이 `Ok`, hook은 `Proceed`로 끝난다(`observation_ingress.rs:203-210`) | **같은 부류, 이번에 막음.** helper가 Claude placed Pending의 목록만 같은 검증으로 복원하고(provider 매핑·placeholder 없음), 판정 대기 동안 자기 root의 Claude resume 문맥이 목록에 없으면 `NotDurable`(재시도) |
| health 패스의 "이 프로세스가 목록에 올림 → 건너뜀"(`src/services/discord/recovery_engine/herdr_reader.rs:339-340`) | 복원이 목록을 채우면 다음 패스가 그 행을 건너뛸 수 있다 | resolver는 행 상태 Pending으로 먼저 갈라 이 skip 앞에서 판정한다. 수용 시험의 "hook 뒤 다음 패스가 attach"가 지킨다 |
| Claude placed Pending attach `attach_launched_herdr_source`(`herdr_source.rs:154-197`) | binding을 스스로 등록(`:185-187`) | 영향 없음 |
| Bound 재시작 `attach_restarted_herdr_source` | 스스로 복원(`:305-332`) | 영향 없음 |
| watcher claim `herdr_claim_admission`(`adopt_skip.rs:176-201`) | 복원만 된 목록은 admit 전이라 보류 | 의도. attach가 admit한다(`herdr_source.rs:240`). 무변화 |
| 목록을 Herdr 표지로 읽는 곳(`src/services/tui_prompt_dedupe/hosted_source.rs:65`, `src/services/discord/watchers/lifecycle/watch_host.rs:44`, `src/services/discord/recovery_engine/producer_resume.rs:182`) | 복원 전에는 marker·endpoint·행이 같은 판정을 한다 | 복원은 재시작 전 값으로 되돌릴 뿐. 무변화 |
| Herdr 반환 판정 `returned`(`src/services/turn_host.rs:126-130`) | tmux marker가 있는 반환에만 쓴다 | 해당 없음 |

**residue warrant.** `src/services/termination_audit/host_terminate.rs:27-48`의 warrant에 새 발급 근거 하나를 더한다: 같은 서버 witness, pane label nonce = 행 nonce,
(셸만이거나 Claude 미시작이면서) hold 없음, 행이 그 위치의 Pending. 소비 직전 같은 조건을 재확인하고 하나라도 어긋나면 Refused. 운영자 경로·tmux에는 발급하지 않는다.
main에서 운영자가 안내문을 보고 하던 같은 close를, 자기 create가 남긴 증명된 잔여 하나로 좁혀 자동화한 것이다. 더 넓은 정리·kill 권한은 얹지 않는다.

**경합.** 확인과 close 사이에 provider가 늦게 떠도 잃는 것이 없다: 첫 입력은 hold 기록 뒤에만 쓰이고 hold가 있으면 close하지 않는다.
원문은 그때까지 내구 큐가 소유한다. 늦게 뜬 provider가 닫혀도 새 nonce create가 원문을 한 번 보낸다(이중 0, 유실 0).

**Unix 동작 변화(의도).** 같은 resolver가 Unix Herdr에도 돈다. main의 never-attached 고정 시험
`a_never_attached_pending_with_only_its_shell_is_judged_nothing_pg`(`src/services/discord/router/message_handler/provider_dispatch_herdr_tests.rs:980`, 픽스처 `:766-781`)는
"판정 없음·수동 안내"에서 "복구 패스가 residue close 뒤 retire"로 기대가 바뀐다. 단언 변경이라 S06b PR에 사유를 적는다.
provider가 살아 있는 Pending은 main처럼 다음 입력의 첫 입력 경로가 잇고, 바뀌는 것은 사용자 입력 없이도 대기 큐가 깨어난다는 점이다.

**수용 시험(S06b, owner 경계 1곳: 생산 복구 패스 진입점).**
- 첫 입력 전 Codex: live Codex, placed Pending, source 없음, hold 없음, nonce hook 없음, 대기 입력 1. 진입 조건 두 가지(실행 명령 ACK 유실, 첫 입력 전 재시작) 각각에서
  복구 패스 뒤 같은 pane·nonce, create 0, 첫 paste 1, hook·source attach 뒤 Bound. 대기 입력이 없으면 Pending 유지·paste 0·Bound 0.
- 첫 write 뒤 hook 전(hold 있음): 복구 패스 중 paste 0, Bound 0, close 0. 이어 hook이 기록되면 attach로 Bound, paste 0.
- pane 없음 → Retired + wake 1. 셸만 → close 1 + Retired + wake 1. Unknown → 행 유지 + close 0. Claude 미시작 → close 1 + Retired, 재기동 1회 상한.
- 재시작 뒤 같은 결과. 뮤턴트: provider 증명만으로 Bound(r2 처방) → 첫 시험 실패, 셸만 칸에서 hold 검사 제거, 패스가 Pending을 다시 건너뜀.
- **재시작한 Codex Pending(r4).** 첫 prompt를 받은 placed Pending, 메모리 등록을 비운 새 프로세스(실제 재시작 경로의 static 초기 상태)에서 두 시점 각각:
  (a) hook 전 재시작 — 복원 전에 온 hook은 425, 같은 hook을 한 번 다시 보내면 기록, (b) hook 뒤 attach 전 재시작 — 재주입 0.
  둘 다 생산 복구 패스 뒤 같은 pane·nonce·source, create 0, paste 0, close 0, Bound, hold 해제, 그 뒤 대기 입력 1건이 정확히 1회 제출.
  baseline이 다른 nonce·다른 파일을 가리키거나 live binding이 다른 source면 Bound 0·hold 유지·목록 변경 0. Claude resume 실행의 같은 재시작은 SessionStart가 425 뒤 기록된다.
  뮤턴트: 복원 단계 제거 → (a)(b) Bound 단언 실패, 판정 대기 425 제거 → (a) 실패(커밋하지 않음).
  r2의 "provider 증명 → 무조건 Bound" 단언은 쓰지 않는다.

**재시작 유실 경로 점검**: dcserver 재시작만으로는 유실이 없다 — 조건은 W04(job 밖 서버)·W03a(witness)·W08(부팅 도달)이고 각각 U04·U03·U11로 확인한다.
남는 유실은 Herdr 서버 자체가 끝나는 경우(업데이트·재부팅·로그오프 U07)이며 조용하지 않다(행 닫기 + 알림 + resume).

## 8. 인접 경로 — Cancel·Terminate·clear·stop

| 경로 | 좌표 | 최종 동작 |
|---|---|---|
| Cancel/stop(턴) | `src/services/discord/commands/stop.rs:67`, `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs:411`, `src/services/discord/turn_bridge/tmux_runtime/claude_stop_delivery.rs:579`, `src/services/discord/turn_bridge/tmux_runtime/codex_stop_delivery.rs`, `src/services/queue/herdr_stop.rs:13` | 현재 턴 소유자만 `host.interrupt`. 세션 생존, 행 유지. Escape ACK는 턴 완료·O 게시 완료가 아님 |
| process 취소 | `src/services/discord/turn_bridge/tmux_runtime/process_backend_cancel.rs` | D03에서 삭제(사라지는 권한: SIGINT·tree kill). 새 권한 0 |
| 운영자 terminate | `src/services/termination_audit/host_terminate.rs:52`, `:116`; `src/services/discord/herdr_terminate.rs:192`, `:576`; `src/server/routes/herdr_terminate.rs` | warrant → `host.terminate` → 정확한 pane/root/provider 부재 재확인 → retire. tmux는 Unsupported 유지. `herdr_terminate_enabled` 정책 확장 없음 |
| force-kill | `src/services/turn_lifecycle.rs:489`, `src/services/dispatched_sessions/force_kill.rs`, `src/services/dispatched_sessions/herdr_kill.rs` | 기존 조건 그대로 어댑터 경유 |
| clear | `src/services/discord/commands/control/clear_teardown.rs:99`, `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:160`, `:380` | 같은 incarnation 재관측. turn·원문·receipt·O debt의 기존 끝 순서. process 레지스트리 부정 조건은 D02에서 삭제 |
| managed reset | `src/services/discord/commands/control/managed_reset.rs:104`, `:142` | tmux kill은 S01, process handle 제거는 D02 |
| reaper·idle | `src/services/discord/tmux_reaper.rs:329`, `:553`; `src/services/discord/session_idle_cleanup.rs`; `src/services/dispatched_sessions/tmux_cleanup.rs:338` | 관찰만 공통화. Unix 이름 나열을 Herdr·Windows 프로세스 전체 열거·kill로 넓히지 않음 |
| provider teardown | `src/services/provider_teardown.rs:84` | 어댑터 경유, clearance(`src/services/provider_teardown.rs:22`) 유지 |

close 응답이 불명확하면 행과 원문을 남겨 재관측한다. close 앞·뒤 어디서 crash가 와도 행을 먼저 지우지 않는다(모델 `EarlyRetire`가 반례).

## 9. 입력 출처 분류표

| 출처 | 원문·권위 소유자 | 진입 좌표 | 물리 효과 | Windows |
|---|---|---|---|---|
| Discord 사용자 메시지 | #7272 queue/input_owner | `src/services/discord/queue_io/input_owner/ingress.rs:351`, `src/services/discord/router/message_handler/provider_dispatch.rs:320` | owner → provider plan → `host.submit`. 전달 뒤 불확실을 새 원문으로 enqueue 0 | W09에서 owner 경로의 `cfg!(unix)`를 연다 |
| catch-up·queued follower·Held | 같은 내구 큐 | `src/services/discord/runtime_bootstrap/queued_recovery.rs:26` | 재시작이 새 출처가 되지 않음. source ready + wake 뒤 한 번 | 같음 |
| 턴 첫 입력 | provider first input | `src/services/claude/herdr_turn/first_input.rs`, `src/services/codex/herdr_turn/first_input.rs`, `src/services/claude/first_input.rs` | Claude: SessionStart 뒤 paste. Codex: 첫 prompt와 첫 hook이 함께 | Herdr |
| busy 추가 입력 | provider busy inject | `src/services/claude_tui/busy_inject.rs:357`, `src/services/codex_tui/busy_inject.rs:216`, `src/services/codex/herdr_turn/busy.rs:129` | draft stash 이후 오류는 `WriteUncertain` 유지 | Herdr |
| routine·goal·campaign 지시 | run/attempt owner | `src/services/routines/agent_executor/reliability.rs:259` | 목적·결과 식별자 보존. 호스트가 사용자 턴을 만들지 않음 | Herdr / Core는 run argv |
| control: `/clear`·`/exit`·model/effort·`/compact` | control transition·settings·handoff owner | `src/services/claude/session_handoff.rs:110`, `src/services/discord/commands/tui_passthrough/herdr_settings.rs:122`, `src/services/discord/tmux_watcher/terminal_token_update.rs:113` | user 원문과 분리. 같은 gated submit | Herdr(자동 `/compact`는 tmux watcher 전용, Windows 없음) |
| Cancel/Escape/C-c | cancel token·native stop owner | §8 | 세션 종료 권한과 다름 | Herdr 키 |
| Terminate/HTTP force/cleanup | warrant·termination owner | §8 | 허용한 소유 대상만 | Herdr close |
| 사람이 pane에 직접 입력 | 사람·native provider 기록 | `src/services/tui_input/human_frame.rs`, `src/services/discord/tui_prompt_relay/herdr_source.rs` | 관찰만. receipt로 소유하지 않음. busy·draft 관측은 허가가 아님 | Herdr 창에서 가능(U08) |
| hook·OSC·JSONL 관측 | source binding·O reader | `src/services/claude_tui/hook_relay/ordered_queue.rs` | 관측 자료, 재송신 지시 아님 | 같음 |
| legacy wrapper follow-up(FIFO) | wrapper owner | `src/services/claude.rs:2795`, `src/services/codex/followup_reader.rs`, `src/services/qwen/followup_reader.rs` | S03에서 `TmuxHost` typed submit 경유. FIFO 형식 유지 | 없음 |
| process follow-up | — | `src/services/session_backend.rs:548`, `src/services/claude.rs:2881` | D01에서 삭제 | — |
| 정책 엔진 tmux 연산 | policy engine | `src/engine/ops/exec_ops.rs:394-440` | `TmuxHost` 경유(S07) | 없음 |
| headless 직접 실행(세션 없음) | 해당 턴 | `src/services/claude.rs:824`, `src/services/codex/command_streaming.rs:206`, `src/services/qwen.rs:371` | stdin 1회. 무변화 | Windows Qwen이 여기로 |
| Core child→parent 결과 알림 | F4 attempt/parent owner | `src/services/campaign_execution/control.rs:514` | 결과 도착 신호. attempt id·산출물로 확인 후 수용 | C02 |
| retry/recovery 합성 입력 | 기존 원문 소유자 | — | write 뒤 불확실에는 자동 재붙여넣기 0 | 같음 |

## 10. ProcessBackend 삭제

### 10.1 지울 것 / 남길 것

| 구분 | 좌표 | 처리 |
|---|---|---|
| 생성·레지스트리 | `src/services/session_backend.rs:56`(`SessionConfig`), `:93`, `:166`(`ProcessBackend`), `:341`(`PROCESS_SESSIONS`), `:548` | D04 삭제 |
| 실행 진입 | `src/services/claude/process_session_launch.rs`, `src/services/codex/process_session_launch.rs`, `src/services/qwen/session_lifecycle.rs:310-480` | D01 |
| 라우팅 | `src/services/claude.rs:794-821`, `src/services/codex/command_streaming.rs:184-203`, `src/services/qwen.rs:357-368` | D01: 오류 반환(Windows Qwen은 headless) |
| demotion guard | `src/services/claude/backend_routing.rs:48-122` | D01 삭제. `prepare_tmux_backend_after_refused_process_demotion`(`:92`)의 "살아 있는 pane이면 tmux" 규칙은 가용성 판정 안으로 옮김 |
| 호스트 모델 | `src/services/session_host/process_host.rs:12`, `src/domain/host_observation.rs:6`, `src/services/session_host/resolve.rs` | D04. `.host_kind`의 옛 `process` 값은 Unrecognized로 읽혀 파괴 경로를 열지 않음 |
| 런타임 종류 | `src/services/agent_protocol.rs:72`, `src/services/discord/turn_bridge/runtime_handoff_loop.rs:831`, `:871`, `src/services/discord/recovery_engine.rs:408` | D03(디스크 문자열은 읽기 전용) |
| 레지스트리 조회 소비자 | `src/services/discord/commands/control/clear_teardown/execution_cleanup.rs:161`, `src/services/discord/admin_host_guard.rs:381`, `src/services/discord/turn_presence/activity/unbound/live.rs:62`, `src/services/herdr_launch/runtime_marker.rs:90`, `src/services/codex_tui/execution_end/confirm.rs:28` 등 15곳 | D02(조건 삭제 = 항상 "process 없음") |
| wrapper pipe 모드 | `src/cli/args/command_args.rs:222`, `src/cli/run.rs:421`, `src/services/tmux_wrapper.rs:375`, `src/services/codex_tmux_wrapper/input.rs:67`, `src/services/qwen_tmux_wrapper.rs:128` | D05. FIFO 모드와 wrapper 자식 stdin은 유지 |
| auth profile·출력 캡처 | `src/services/session_backend/auth_profiles.rs`, `src/services/session_backend/output.rs` | D04(호출부가 process launch뿐인지 D04 착수 때 재확인) |
| 클러스터 광고 | `src/services/cluster/readiness.rs:339` | D04: `process` 제거, 로컬 Herdr endpoint가 있으면 `herdr`. 정책이 `process`를 요구하는지 U14 |
| **남김** | `src/services/session_backend.rs:26`(`ReadOutputFailure`), `:855`(stream_line), `:868`(terminal_usage), `:887` 이후 JSONL tail/harvest; `src/services/session_backend/stream_line.rs`; `src/services/process/`(`kill_pid_tree`, `ProcessIdentity`); headless 직접 실행 | tmux wrapper·headless가 쓴다 |
| 먼저 옮김 | `src/services/claude/process_session_launch.rs:183` `without_resume_arg`(호출 `src/services/claude.rs:2430`) | D01 전에 provider argv 준비로 이동 |

### 10.2 호스트 없음 — intake admission 경계에서 내구 hold (tmux 없는 Unix 등)

**r1의 결함.** r1은 세 provider 함수(`src/services/claude.rs:794-821`, `src/services/codex/command_streaming.rs:184-203`, `src/services/qwen.rs:357-368`)가 `Err`를 돌려
start failure로 알린다고 했다. 그러나 이 함수들은 큐에서 원문을 꺼내 턴을 시작한 **뒤**에 불린다. owner가 없는 턴(Qwen 전부, owner 경로가 아닌 Claude/Codex)은
`src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:284-286`에서 곧바로 `execute_inner`로 가고, 그 `Err`는 원문을 큐에 되돌리지 않는다.
대기 중이던 Qwen 원문이 사라지는 경로다.

**r2의 결함(r3 정정).** r2는 판정을 intake_turn의 `host_refusal`로 옮겼다. 그런데 일반 live 메시지는 `owner = None`, `intake_outbox_id = None`인 본문만 가진
submission이고(`src/services/discord/router/intake_gate.rs:1325-1352`, Qwen은 TUI owner 대상도 아님 `src/services/discord/queue_io/input_owner/ingress.rs:349-383`),
`host_refusal`의 비queued 분기는 알림 뒤 uploads·clear 플래그만 되돌리며(`src/services/discord/router/message_handler/intake_turn/host_refusal.rs:46-63`,
`src/services/discord/admin_host_guard.rs:66-88`) 호출자는 `Executed`로 끝낸다(`src/services/discord/router/message_handler/intake_turn.rs:252-255`).
그 앞에서 live checkpoint가 이미 전진할 수 있다(`src/services/discord/router/intake_gate.rs:1271-1283`). 본문이 내구 큐에 남지 않는다.

**최종.** "쓸 관리 호스트가 없다"는 판정을 `IntakeSubmission` 전체(본문·첨부·source id)를 가진 **router admission**으로 옮기고, 이미 있는 deferred 계약에 태운다.

1. 판정: `src/services/turn_host.rs:177`의 `refusal_before_turn`은 지금 endpoint 없는 채널을 IO 없이 통과시킨다(`:186`). D01은 `ChannelHostPlan`(§5)에서
   "관리 세션 이름이 있고 계획 호스트가 없거나 지금 못 쓴다"(tmux 미설치·가용성 캐시 연속 실패 3회 초과·Herdr 서버 down)를 계산하는 함수 하나를 두고,
   아래 세 자리가 같은 함수를 읽는다. 판정은 캐시된 가용성만 읽고 IO를 하지 않는다. endpoint 있는 채널의 정책 거절 판정과 순서는 그대로다.
2. **router admission(정본).** `admit_text_intake`(`src/services/discord/router/intake_dispatch.rs:229`)가 이 판정이 참이면 새 `IntakeAdmission::DeferredHostUnavailable`
   (기존 `DeferredWriterBoot`·`DeferredTurnMode`와 같은 모양, `:121-147`)을 돌린다. 이 종류를 deferred 목록(`:690-696`)에 넣어
   `finish_deferred`(`:699-711`)가 **큐가 원문 전체를 내구로 받은 뒤에만** checkpoint를 전진시킨다(수탁은 기존 `hold_in_custody`·`defer_live_submission`, `:861-956`).
   수탁이 실패하면 `Err`이고 checkpoint는 그대로다. `checkpoint_waits_for_admission`(`:583-589`)도 같은 판정에서 참을 돌려,
   live gate의 선행 checkpoint(`src/services/discord/router/intake_gate.rs:1281-1283`)가 이 판단보다 먼저 전진하지 않게 한다.
   첨부는 기존 deferred 종류처럼 portable uploads로 만든 결과(`Result<PendingUploads, String>`)를 싣는다. 만들 수 없으면 `hold_in_custody`가 본문만 남기지 않고 `Err`를 돌려(`:868-872`)
   checkpoint가 읽히지 않은 채 남고, 기존 catch-up이 그 메시지를 다시 읽는다(유실·부분 보존 0).
   admission은 live·queued drain·slash skill·raw attachment 네 출처(`IntakeOrigin`, `:55-61`)가 모두 지난다(live `:609`, `:641`, 일반 `:578`, queued `src/services/discord/router/intake_dispatch/queued.rs:100`, `:253`).
3. **queued drain**은 기존 원문 소유자의 복원 계약을 유지한다: admission이 deferred를 돌리면 큐 항목은 큐에 그대로 남는다. admission 뒤 경합으로 turn 단계에서
   알게 된 경우는 r2 그대로 `host_refusal`의 `QueuedTransitionBusyHandoff`(`src/services/discord/router/intake_dispatch/queued.rs:353-357`)가 통째 복원한다.
4. **intake_turn 둘째 방어선.** admission 뒤 호스트가 사라진 경합과 ADK thread의 재사용 thread(대상 채널이 inbound와 다름,
   `src/services/discord/router/message_handler/intake_turn/adk_thread.rs:166-176`)는 intake_turn에서 알게 된다. 비queued `HostUnavailable`은 `Executed`가 아니라
   새 `IntakeExecution`의 host-held 결과로 dispatcher에 돌려, submission을 가진 dispatcher가 같은 `defer_live_submission`으로 수탁한 뒤에만 checkpoint를 전진시킨다.
   정책 거절(endpoint 채널의 기존 Herdr 거절)은 main 그대로다(알림·uploads 복원).
5. **headless 턴**(routine·예약 실행, `src/services/discord/router/message_handler/headless_turn/entrypoints.rs:160`)은 routine 관리 세션 이름을 쓰고
   (`src/services/discord/router/message_handler/headless_turn.rs:391-408`) 같은 `provider_dispatch::execute`로 간다(`:1073`). D01은 예약 전에 같은 판정을 내려
   `HeadlessTurnStartError`(`src/services/discord/router/turn_start.rs:121-125`)의 새 variant `HostUnavailable`로 돌려준다. 프롬프트는 호출자가 그대로 갖고,
   시작 안 된 시도로 남는다. **문자열로 바꾸기 전에 타입으로 분류하고, 어느 호출자도 이것을 재시도 소비·종단 실패로 정산하지 않는다**(아래 정산표).
   exhaustive match인 호출자(`agents.rs`·`start_deferral.rs`·`turn_deliver.rs`·`agent_handoff.rs`·`restart.rs`)는 새 variant 없이 컴파일되지 않는다.
6. 알림은 채널당 사유가 바뀔 때 한 번(queued 재시도는 이미 조용함, `src/services/discord/router/message_handler/intake_turn/host_refusal.rs:36`). 문구 예: `managed session host unavailable: tmux is not installed on this node and the channel has no Herdr endpoint`.
7. 수탁된 원문은 큐 backlog로 남고, 호스트 가용성이 돌아오는 신호(tmux 가용성 캐시 회복, Herdr 서버 기동·health 성공)가 drain을 깨운다. 사용자가 다시 말하지 않아도 한 번 제출된다.
   큐는 내구라 dcserver 재시작을 지나도 남는다(부팅 복원 `src/services/discord/runtime_bootstrap/queued_recovery.rs:26`). drain은 admission을 다시 지나므로 아직 못 쓰면 조용히 남는다.
8. provider 함수 안의 "호스트 없음" 분기는 방어용 `Err`로만 남는다. D01 수용 시험이 경합 아닌 경로에서는 이 분기에 닿지 않음을 단언한다.

**headless `HostUnavailable` 정산 전수(r4).** `HeadlessTurnStartError`를 받는 생산 호출자 전부(시험·`cfg(all(test, unix))` `relay_e2e` 제외).

| 호출자 | main의 `Internal` 정산 | `HostUnavailable` 정산(D01) |
|---|---|---|
| 예약 메시지 `src/services/scheduled_messages.rs:429-442` → `:235-258` | `anyhow!` 문자열로 바꾼 뒤(`:442`) 네 문자열(`:272-281`)에 안 맞으면 `interrupt_for_retry`(`:856`, 유한 재시도 소비) | 문자열로 바꾸기 전에 분류(오류 체인에 타입을 남겨 `downcast`)해 `defer_delivery_without_retry_pg`(`src/db/scheduled_messages/agent.rs:212`)로 재시도 횟수를 쓰지 않고 미룬다. 호스트가 돌아올 때까지 반복, 종단 실패 0 |
| routine `src/services/routines/agent_executor/launch.rs:60-68`(DM 예약), `:212` → `start_deferral.rs:57-69`, `:92-97` | runtime 미준비 문자열이면 `ProviderNotReady`로 600초 창(`:9`, `:115-126`) 안에서만 재시도, 창 뒤·다른 `Internal`은 실패 | 새 `TransientStartKind::HostUnavailable`: 같은 60초 cadence(`:7`)로 다시 시도하되 600초 창을 열거나 소비하지 않는다(창 밖에서도 실패로 넘기지 않음). run은 pending 유지 |
| 사람 입력 전달 `src/services/discord/health/turn_deliver.rs:422-453` | `From`이 `Internal`을 `StartAttempt::Unavailable`로(`:116`) → `HumanInputError::RuntimeUnavailable`(`:244`), ack 0 | 같은 `Unavailable`(ack 0, 원 출처가 원문을 갖고 재시도) |
| HTTP turn·DM `src/server/routes/agents.rs:838`, `:850` → `:884-902` | 503 | 503 + `status: "host_unavailable"`(호출자가 원문 소유) |
| agent handoff `src/services/discord/agent_handoff.rs:564` → `:514-518` | `turn_unavailable` | 같은 `turn_unavailable`(handoff 원문은 호출자 소유) |
| live agent 복구 `src/services/discord/health/recovery/live_agent_recovery.rs:255-275` | 경고 + pending intent 유지 | 같음(intent 유지, 다음 복구 주기) |
| exact replay `src/services/discord/router/message_handler/headless_turn/replay_start.rs:517-535` | 경고, 시작 전이면 `ExactHandoff::Refused`(`src/services/discord/replay_policy.rs:177-180`) | 같음(시작 전 거절 → 원 소유자 유지) |
| `/restart` seed `src/services/discord/commands/restart.rs:151`, `:176-196` | 첫 시도 `Failed` 알림 | 같은 `Failed` 알림. seed는 명령이 만든 문구라 사용자 원문 유실이 아니다(넓히지 않음) |

**같은 부류 진입점 전수(r3).** provider 턴을 여는 진입은 router admission의 네 출처, intake_turn의 두 호출(일반 `intake_turn.rs:252`, ADK thread `adk_thread.rs:171`),
headless 진입(`headless_turn/entrypoints.rs:160`·`:187`과 owner 진입 `start_reserved_headless_turn_with_owner`; 호출자는 위 정산표 8개(호출 자리 10곳)다. r3은 `turn_deliver.rs:438`의 직접 호출을 빠뜨렸다)이고,
`provider_dispatch::execute`의 생산 호출은 `intake_turn.rs:2130`과 `headless_turn.rs:1073` 둘뿐이다. 각 진입의 처리: 네 출처 → 2, 두 intake_turn 호출 → 4, headless → 5.
busy 추가 입력·catch-up은 이미 살아 있는 세션이나 큐 항목이라 새 진입이 아니다.

일시 부재는 지금처럼 가용성 캐시(45초·3회, `src/services/platform/tmux/availability.rs:13-14`)가 흡수하고, 기록된 pane이 살아 있으면 사용 가능으로 본다
(`src/services/claude/backend_routing.rs:92`의 규칙을 가용성 판정 안으로). 부팅을 막지 않는다(Herdr 채널·headless는 돈다).
업그레이드 순간 옛 바이너리가 남긴 process wrapper(자기 process group, `src/services/session_backend.rs:240`)는 정리하지 않는다(main에 없던 kill). 운영 노드 해당 여부는 U14.

**수용 시험(D01, 실제 진입점).**
- live intake: `origin = LiveMessage`, `owner = None`, `intake_outbox_id = None`인 Qwen 본문·첨부 메시지 → 호스트 없음 → 자식 0, 알림 1, 큐 항목의 본문·첨부·source id가 입력과 같음,
  dcserver 재시작 뒤 남음, 가용 신호 뒤 제출 정확히 1회. 내구 수탁 실패를 주입하면 checkpoint 미전진.
- queued drain: 대기 중인 Qwen 원문(첨부 포함)으로 drain → 같은 단언(큐 항목 유지). Claude·Codex의 owner 없는 턴도 live·queued 각각 한 번.
- headless: `HostUnavailable` 반환·자식 0·예약 0. 호출자 정산(정산표 각 행, 실제 호출 경계): 예약 메시지 fire → 배달 행이 미룸 상태·재시도 횟수 불변·종단 실패 0,
  routine run → 600초 창을 넘긴 뒤에도 pending·실패 0, `turn_deliver` → `RuntimeUnavailable`·ack 0, HTTP → 503 `host_unavailable`.
  뮤턴트 (d) 예약 메시지를 문자열 분류로 되돌림 → 재시도 횟수 단언 실패, (e) routine을 `ProviderNotReady`(600초 창)로 분류 → 창 뒤 실패 단언 실패(커밋하지 않음).
- 뮤턴트: (a) 지금처럼 uploads·clear만 복원하고 `Executed` 반환 → live 본문 단언 실패, (b) admission 판정을 빼고 provider 함수의 일반 `Err`만 남김 → 원문 단언 실패,
  (c) 가용성 wake 제거 → 복귀 뒤 제출 단언 실패(커밋하지 않음).
모델 `UnixNoTmux` GREEN, `ErrConsumes`(호스트 없음 오류가 원문을 소비) RED `NoInputLoss`·`CustodyKept`, `NoAvailWake`(가용성 wake 없음) RED `HeldInputSubmitted`,
`DropOnWake`(wake가 보존을 풀고 실행 대기에 넣지 않음) RED `CustodyKept`·`HeldInputSubmitted`, `RequeueReach`(정상 설계에서 보존된 입력이 tmux 복귀 뒤 실제로 시작되는 도달 증인), `SilentFallback`(폴백을 남김) RED.

### 10.3 Windows Qwen

`tmux_session_name`이 있어도 Windows Qwen은 세션 없는 직접 실행(`src/services/qwen.rs:371` 이후, `execute_qwen_streaming_attempt` `:417`, cfg 게이트 없음)으로 보낸다.
판정은 provider·플랫폼 규칙이지 런타임 폴백이 아니다. 대화 연속은 저장된 session id의 resume 전략이 맡는다. 지금도 Windows Qwen process 세션은 job으로 dcserver와 함께 끝나므로 재시작 연속성은 같다. U13에서 실측.

### 10.4 시험 대역

process 레지스트리를 "살아 있는 세션" 대역으로 쓰는 시험(B 부록 A4: 55파일 175참조, C `process-uses.tsv`의 test-named 행)을 세 부류로 나눈다.
(1) process 경로 자체의 시험은 해당 코드와 같은 PR에서 지우고 `scripts/ci/test-removal-justifications.txt`에 사유. (2) 호스트 중립 동작에 대역으로만 쓰는 시험은
`src/services/session_host/test_support.rs`의 주입(`InjectedLivenessGuard`·`injected_presence`)과 기록형 가짜 호스트로 바꾼다(D00, 단언 삭제 0).
(3) `RuntimeHandoffKind::ProcessBackend`를 "TUI 아님" 표본으로 쓰는 표는 `LegacyTmuxWrapper`나 `None`으로 바꾼다. 기록형 가짜 호스트(약 60줄)는 새 시험 인프라라 코디네이터 승인이 필요하다.
wrapper 종료 뒤 늦게 append하는 후손 시험(`src/services/discord/replay_policy/live/observer_tests.rs`)은 bool 대역으로 바꾸면 뜻이 사라지므로 실제 자식 프로세스를 유지한다.

## 11. Herdr Core와 F4

- 위치: `HerdrCoreHost`는 같은 trait의 구현체이고 F4 lane factory만 만든다. 채널 기본 호스트로 선택되지 않는다. 결과 승인·다음 작업 결정은 F4 서버 권한이다
  (`src/domain/campaign/model.rs`, `src/services/campaign_execution/control.rs`, `src/services/campaign_execution/host_gate.rs`).
- 구현: `hc --json` CLI 래핑(run/send/keys/interrupt/read/get/kill). preview(0.x)라 프로토콜 직접 구현은 버전마다 깨진다. 실측 버전·schema 해시를 고정하고 필요한 필드가 없으면 typed unsupported(U16).
- 계약: 첫 지시는 `run` argv로(부팅 중 send는 drop 가능, blind resend 금지). OSC 7501 idle/working/done/blocked/error는 진행 상태일 뿐 결과가 아니다.
  `run --keep`은 잃은 명령을 다시 실행하지 않고 shell을 만든다 — 같은 agent가 살아난 것이 아니다. `spawn_pending`은 이미 실행됐을 수 있어 중복 spawn 금지.
  Core 프로세스마다 event sequence가 다시 시작하므로 재연결 뒤 snapshot. 자식 자동 알림은 끄고 서버가 결과를 모은다.
  `--outlive-parent`를 무조건 켜지 않고, dcserver를 parent terminal로 잘못 연결하지 않는다(U17).
- Claude/Codex 세션 이어 열기 API가 없으므로 capability로 Unsupported. 재부팅·holder 소실의 옛 attempt는 Lost로 정산, 새 lane은 새 attempt(U19).

## 12. 분할 계획

상한은 `pr_cap_prod.py`의 prod 30파일 / 순증 +1500, 설계 재분할 선 +1200. 숫자는 주석·빈 줄·시험 제외 추정이다(측정값 아님).
"win"은 Windows CI job·win-desktop 실측 필요 여부다. unix 전용 API를 새로 쓰거나 부르는 모든 조각은 `cargo check --lib --tests --target x86_64-pc-windows-gnu`가 필수다.
포트는 첫 호출자와 같은 조각에서 착지한다(dead code 0). 공유 파일은 아래 순서의 단일 작성자가 이어 수정한다.

### 12.1 공통 포트 트랙 (Unix 동작 보존)

| 조각 | 목적 | 소유 파일 | prod 파일 / 순증 | 수용 시험 | win | 의존 |
|---|---|---|---|---|---|---|
| **S01** | tmux 종료 효과를 `TmuxHost`로 이전 + 어댑터 밖 관리 효과 ratchet 도입(현재 수를 기준선으로) | §12.4 목록 21파일 + `src/services/session_host/tmux_host.rs` | 22 / +95 | 바뀐 모듈과 호출 형제 모듈의 기존 시험, ratchet 새 범주: 생산 텍스트의 두 표기 종료 호출이 허용 3곳뿐, 다른 범주는 현재 수 이하(§12.4) | CI 컴파일 | — |
| S02 | 관찰 포트: presence/liveness/has_session 경로 | `src/services/session_host/{tmux_host,legacy_collapse,resolve}.rs`, `src/services/tmux_diagnostics.rs`, `src/services/provider/session_probe.rs`, `src/services/session_activity.rs` 외 C S02 | 12 / +350 | 3값·기존 bool collapse 같은 장소·같은 뜻, ProbeError를 Missing으로 합치지 않음 | 컴파일 | S01 |
| S03 | `submit` 포트 + 입력 물리 효과(실행기 23곳의 tmux 인자 조립, FIFO, Herdr gated adapter) | `src/services/claude_tui/host_input.rs`, `src/services/codex_tui/host_input.rs`, `src/services/tui_input/actor/pane.rs`, `src/services/tui_input/submission_tmux.rs`, `src/services/claude_tui/busy_inject.rs` 외 C S03 | 18 / +850 | 원문 owner·first-key cancel·draft stash·partial write·receipt. spy가 어댑터 밖 tmux 호출 0 확인 | 컴파일 | S01 |
| S04 | `create` 포트 + 생성·핸드오프 | `src/services/claude/tui_session_launch.rs`, `src/services/claude/herdr_turn/handoff.rs`, `src/services/claude/herdr_turn/never_attached.rs`, `src/services/codex/tui_session_launch/first_input.rs`, `src/services/herdr_launch.rs` 외 C S04 | 18 / +850 | Pending crash·ACK 유실·동시 create 2개·same-session Return·argv. Codex create 완료에 SourceBound 요구 0 | 컴파일 | S01 |
| S05 | `interrupt`·`terminate(warrant)` 포트 + Herdr 종료 | `src/services/termination_audit/host_terminate.rs`, `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs`, `src/services/discord/turn_bridge/tmux_runtime/native_stop.rs`, `src/services/dispatched_sessions/herdr_kill/effects.rs` 외 C S05 | 16 / +600 | Cancel 뒤 세션 생존, close 전/후 crash, mismatch·Unknown warrant 무효, tmux 운영자 terminate Unsupported 유지 | 컴파일 | S01 |
| S06 | `recover` 포트 + 복구 소비자 | `src/services/discord/recovery_engine/host_reconcile.rs`, `src/services/discord/recovery_engine/herdr_reader.rs`, `src/services/discord/recovery_engine/herdr_server_replaced.rs`, `src/services/discord/recovery_engine/restore_inflight.rs`, `src/services/discord/tui_prompt_relay/herdr_source.rs`, `src/services/discord/tui_prompt_relay/rehydration.rs` 외 C S06 | 18 / +850 | source cursor·옛 형식·standby·boot+health 재채택·reconcile wake | 컴파일 | S02 |
| **S06b** | 위치가 기록된 Pending 자동 탈출(§7.3): placed resolver(provider 증명 ≠ source 증명, 첫 입력 전이면 Pending 유지 + 큐 wake), 재시작한 Pending의 쓰기 없는 메모리 등록 복원과 판정 대기 동안의 hook 425, residue warrant, never-attached 문구 | `src/services/discord/recovery_engine/herdr_reader.rs`, `src/services/herdr_launch.rs`, 새 `src/services/herdr_launch/placed.rs`(제안), `src/services/herdr_launch/unplaced.rs`, `src/db/dispatched_sessions/hosted_execution.rs`, `src/services/termination_audit/host_terminate.rs`, `src/services/claude/herdr_turn/never_attached.rs`, `src/services/codex/herdr_turn.rs`, `src/services/codex/herdr_turn/first_input.rs`(등록 준비 세 호출을 helper와 공유), `src/services/discord/tui_prompt_relay/herdr_source.rs`(기존 Codex attach·재시작 등록부 재사용 노출), `src/services/tui_prompt_dedupe/runtime_binding/adopt_skip.rs`(판정 대기 집합), `src/services/tui_prompt_dedupe/runtime_binding/codex_hook.rs`·`src/services/claude_tui/hook_server/observation_ingress/resume_start.rs`(판정 대기 동안 재시도), 큐 wake 호출 | 14 / +1,050 | §7.3 수용 시험(생산 복구 패스 진입점: 첫 입력 전 Codex 2진입·첫 write 뒤 hook 전·pane 없음·셸만·Unknown·Claude 미시작, 재시작 뒤 같음, 재시작한 Codex Pending 두 시점·남의 source 거절·Claude resume SessionStart). 뮤턴트: provider 증명만으로 Bound, 복원 단계 제거, 판정 대기 425 제거. 기존 never-attached 시험 기대 변경(사유 기록) | 컴파일 | S05, S06 |
| S07a~f | 잔여 소비자 6묶음 | [consumer-slices.tsv](i7583/consumer-slices.tsv)의 파일 목록(앞 조각과 겹치면 남은 효과만) | 각 ≤17 / +250 | leaf spy + source scan, 기존 guard·bool·결과 동등 | 각 컴파일 | S02~S06 |
| S08 | ratchet 마감: 모든 범주 어댑터 밖 관리 효과 0(W 트랙 코드 포함 재검색) | `scripts/check_destructive_call_site_ratchet.py`와 기준선 | 0 / 스크립트 | 스크립트 시험, 전수표 BYPASS 320행 재검색 0 | — | S07f |

### 12.2 Windows 트랙

| 조각 | 목적 | 소유 파일 | prod 파일 / 순증 | 수용 시험 | win | 의존 |
|---|---|---|---|---|---|---|
| W00 | 측정만: 게이트를 임시로 뒤집어 Windows 컴파일 오류·파일 수 census(착지 없음) | — | 0 | U15 기록 | 맥 | — |
| W01 | 선택 표현 + `ChannelHostPlan` 정규화와 허가(§5.1) 계산(Windows는 W09 전까지 기존 결과). [state-entrypoints.tsv](i7583/state-entrypoints.tsv)의 `herdr_turn_enabled` reader·시험 setter 전부를 새 타입으로, 실행·Escape 허가 접근자를 계획 읽기로 | `src/config/runtime_settings.rs`, `src/config/session_hosts.rs`, `src/services/turn_host.rs`, `src/services/provider/cancel_token_claude_interrupt.rs`, reload/validation, 새 `src/services/session_host/channel_host_plan.rs`(제안) | 16 / +850 | §5.2 표 기반 시험(전 조합·완전성), 생산 경계 3건(Escape 표: 기존 `begin_user_stop` 시험에 None/false/true × Claude·Codex, Unix 결과 무변화), stale current host, 생산 진입점에서 off일 때 IO 0. 뮤턴트 §5.2 (a)~(d) | 컴파일 | — |
| W02 | named pipe 전송 | `src/services/session_host.rs`, `src/services/session_host/herdr/{transport,model,wire,contract}.rs`, 새 `src/services/session_host/herdr/transport_pipe.rs`(제안), registry/factory | 10 / +950 | 실제 임시 pipe·framing·timeout·write 0바이트 전/후·동시 mutation 직렬. Unix rig 무변화 | **Windows CI** | W00 |
| W03a | 서버 witness(pipe 서버 PID·FILETIME·boot) | `src/services/session_host/herdr/provenance.rs`, `src/services/session_host/herdr/transport.rs` 소비부, Windows OS leaf | 6 / +500 | PID 재사용·새 서버·boot 변경·권한 오류를 absent로 바꾸지 않음 | **CI** | W02 |
| W03b | 점유·E7·loaded image·retired hold | `src/services/session_host/herdr/{pane_probe,observe}.rs`, `src/services/session_host/herdr/policy/`, `src/services/herdr_launch/retired_hold.rs` | 10 / +650 | 살아 있는/죽은 pid, nonce 점유, E7 Off 미증명 Withheld, hold 왕복, user_default 부팅 거절 유지 | **CI** | W03a |
| W04 | Herdr 서버 job 밖 기동 + 관리 설정 Windows판 | `src/services/platform/windows_job.rs`, `src/services/session_host/herdr/autostart.rs`와 하위, `src/services/session_host/herdr/managed_config.rs`, `src/bootstrap.rs` | 8 / +500 | Windows CI: job 안 부모가 죽어도 탈출시킨 서버는 산다, 탈출 안 한 자식은 죽는다. 기존 서버 발견 시 재시작·업그레이드 0 | **CI + win** | W02, U04 |
| W05 | 파일 신원·잠금 공통 헬퍼 | 새 `src/services/platform/file_identity.rs`(제안) + dev·inode 사용 파일 | 22 / +150 | Unix: 같은·다른·교체된 파일 판정이 이전과 같음. Windows CI: 같은 3경우 | **CI** | W00 |
| W06 | Windows launch 명령 + native hook/source | `src/services/herdr_launch.rs`, `src/services/claude/tui_session_launch.rs`, `src/services/codex/tui_session_launch/first_input.rs`, `src/services/discord/tmux_session_files.rs`, hook relay 경로 | 14 / +800 | `.cmd` 런처를 `cmd.exe`로 실행해 env·인자·cwd 전달, HERDR 변수 없음, 공백·한글·npm shim, SessionStart·rollout 경로 | **CI + win** | W03b, W04, W05 |
| W07 | 게이트 열기 1: 실행 경로 컴파일(스위치는 아직 `cfg!(unix)`) | `src/services/claude.rs`, `src/services/codex.rs`, `src/services/claude_tui/mod.rs`, `src/services/discord/router/message_handler/provider_dispatch.rs`와 하위 | ~20 / +300(W00로 확정) | Windows CI: 컴파일, 생산 진입점에서 Windows Herdr 턴 거절·IO 0 유지 | **CI** | W01, W06 |
| W08 | 게이트 열기 2: 복구·종료·관찰 컴파일 + 부팅 도달 + terminal settlement Windows판 + server-replaced를 `legacy_dedicated`·Pending까지 | `src/services/discord/mod.rs`, `src/services/discord/recovery_engine.rs`, `src/services/discord/recovery_engine/herdr_server_replaced.rs`, `src/services/provider/cancel_token_claude_interrupt.rs`, `src/services/discord/tui_prompt_relay.rs`와 하위 | ~27 / +550(W00로 확정, 30파일을 넘으면 settlement·server-replaced를 W08b로 분리) | Windows CI: 생산 부팅 진입점이 재시작 패스(`rehydration.rs:270`, `:668`)에 도달, 같은 서버 pane 재채택, 서버 교체면 Bound·placed Pending 모두 Retired + 알림 1회(재부팅 조건 없음), settlement 참 | **CI** | W07, S06b |
| W09 | 활성화 — Windows Claude: PlatformDefault Herdr와 허가, 스위치 `cfg!(unix)` 해제, owner 입력 경로, `#[cfg(unix)]` Escape 판정, process 시절 세션의 resume 처리 | `src/services/turn_host.rs`, `src/services/discord/queue_io/input_owner/ingress.rs`, `src/services/discord/router/message_handler/provider_dispatch.rs`, `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs`, `src/services/provider_hosting.rs`, `src/services/claude/herdr_turn/handoff.rs`, CI 선택 감사 | 11 / +320 | 부팅·턴 중간·dcserver crash 행렬에서 같은 session/pane/nonce, 이중 0, O cursor, cancel/terminate, placed Pending 자동 탈출(S06b 픽스처의 Windows판), O writer 빈 초기화. 생산 진입점: 공유 설정 `herdr_turn_enabled=false`·`herdr_cancel_enabled` 미설정/false에서도 Windows 기본 Claude가 Herdr로 시작하고 Escape 1회 전달. Windows endpoint 채널은 bool을 따름. O 커서 없는 저장 세션이 있는 채널의 첫 입력이 시작 실패가 아니라 resume 또는 fresh + 알림 1회로 시작. Herdr down이면 tmux 0·admission 거절 | **CI + win(U01~U12 전부)** | W08, U 패킷 |
| W10 | 활성화 — Windows Codex: PlatformDefault 실행 허가(Escape 허가는 W01 계산·W09 경로 공유) | `src/services/turn_host.rs`, Codex hosting 경로 | 6 / +150 | 위 행렬의 Codex판, 첫 입력 hook 순서, placed Pending의 첫 입력 전 Codex가 같은 nonce로 첫 prompt를 받음. 생산 진입점: `herdr_codex_turn_enabled`·`herdr_cancel_enabled` 미설정/false에서 Windows 기본 Codex가 시작되고 Escape가 `SwitchOff`로 버려지지 않음. endpoint 채널은 지금처럼 bool을 따름 | **CI + win(U10)** | W09 |

### 12.3 삭제 트랙과 Core

| 조각 | 목적 | 소유 파일 | prod 파일 / 순증 | 수용 시험 | win | 의존 |
|---|---|---|---|---|---|---|
| D00 | 시험 대역 치환(시험만) | §10.4 둘째·셋째 부류 약 30 시험 파일, `src/services/session_host/test_support.rs` | 0(+가짜 호스트 60줄) | 바꾼 시험 통과, 단언 삭제 0 | CI | — |
| D01 | 라우팅·process launch 삭제, 호스트 없음 판정과 router admission의 내구 hold(§10.2: live·queued·slash·첨부 출처, intake_turn 둘째 방어선, headless), 가용성 wake, Windows Qwen headless, 가용성 규칙 | `src/services/claude.rs`, `src/services/claude/backend_routing.rs`, `src/services/claude/process_session_launch.rs`, `src/services/codex/command_streaming.rs`, `src/services/codex/process_session_launch.rs`, `src/services/qwen.rs`, `src/services/qwen/session_lifecycle.rs`, `src/services/claude_command.rs`, `src/services/platform/tmux/availability.rs`, `src/services/turn_host.rs`, `src/services/discord/router/intake_dispatch.rs`, `src/services/discord/router/intake_dispatch/replay_admission.rs`, `src/services/discord/router/intake_gate.rs`, `src/services/discord/router/message_handler/intake_turn.rs`, `src/services/discord/router/message_handler/intake_turn/host_refusal.rs`, `src/services/discord/router/intake_dispatch/queued.rs`, `src/services/discord/router/message_handler/headless_turn/entrypoints.rs`, `src/services/discord/router/turn_start.rs`, headless 정산 호출자(`src/services/scheduled_messages.rs`, `src/services/routines/agent_executor/start_deferral.rs`, `src/services/discord/health/turn_deliver.rs`, `src/server/routes/agents.rs`, `src/services/discord/agent_handoff.rs`, `src/services/discord/commands/restart.rs`), 가용성 wake 등록 | ~26 / −430 | §10.2 수용 시험: live intake(`owner=None`·`intake_outbox_id=None` Qwen 본문·첨부: 자식 0, 알림 1, 내구 원문 동일, 재시작 생존, 복귀 뒤 1회, 수탁 실패 시 checkpoint 유지), queued drain 같은 표, Claude·Codex owner 없는 턴, headless `HostUnavailable`과 호출자 정산(예약 메시지 재시도 횟수 불변, routine 600초 창 뒤에도 pending, `turn_deliver` ack 0, HTTP 503). 뮤턴트 5. 살아 있는 pane이면 tmux | **CI** | W10, D00 |
| D02 | 레지스트리 조회 소비자 정리 | §10.1 소비자 15곳의 13파일 + `src/services/provider/session_probe.rs` | 13 / −120 | clear·reset·admin guard 기존 시험 | CI | D01 |
| D03 | `ProcessBackend` 종류·`ProcessReady`·process 취소 삭제 | `src/services/agent_protocol.rs`, `src/services/discord/turn_bridge/runtime_handoff_loop.rs`, `src/services/discord/turn_bridge/stream_loop.rs`, `src/services/discord/recovery_engine.rs`, `src/services/discord/turn_bridge/tmux_runtime.rs`, `src/services/discord/turn_bridge/tmux_runtime/process_backend_cancel.rs`, `src/services/discord/inflight/model.rs`, `src/services/discord/health/routine_process.rs` 등 | 24 / −600 | 0단계: 종류 없는 헤드리스 행의 현재 복구 분기를 시험으로 먼저 고정. 디스크 `"process_backend"` 행은 "세션 없음"으로 닫히고 알림 1회, silent-skip 0 | CI | D02 |
| D04 | 레지스트리·`ProcessHost`·`HostKind::Process` 삭제, 클러스터 광고 | `src/services/session_backend.rs`, `src/services/session_backend/auth_profiles.rs`, `src/services/session_backend/output.rs`, `src/services/session_host/process_host.rs`, `src/services/session_host/model.rs`, `src/services/session_host/resolve.rs`, `src/domain/host_observation.rs`, `src/services/cluster/readiness.rs` 등 | 14 / −900 | 실행 소스의 ProcessBackend/ProcessHost/Process kind 0, JSONL 파서·3종 runtime 유지, 옛 `process` 표식 Unrecognized | CI | D03, U14 |
| D05 | wrapper pipe 모드·CLI 인자 삭제, CLI 문서 재생성 | §10.1 wrapper 행 9파일 | 9 / −250 | FIFO 모드 시험, `--input-mode pipe` 거절 | CI | D04 |
| C01 | `HerdrCoreHost`(hc CLI) | 새 `src/services/session_host/herdr_core/`(제안), `src/domain/host_observation.rs` | 6 / +700 | hc 출력 JSON 파싱 계약, 고정 버전 왕복(실제 hc 있을 때) | win(U16) | S04·S05·S06 |
| C02 | F4 레인 하나 실측 기록 + 하니스 | #7438 소유 소비자 경로, 기존 command/store API를 부르는 하니스 | ≤10 / +800 | 첫 지시·OSC·parent wait·결과·재부팅 loss, 다른 terminal 정리 0 | **win(U17~U19)** | C01, #7438 F4 intake |

조각 수: S01~S06 6 + S06b 1 + S07a~f 6 + S08 1 + W01~W10(W03a·b 포함) 11 + D00~D05 6 + C01·C02 2 = **착지 33**, 측정 W00 1.
최대 추정은 prod 27파일(W08), 순증 +950(W02). +1200을 넘는 조각은 없다. W08은 W00 측정이 30파일을 넘기면 W08b로 나눈다(그때 34). r3에서 바뀐 조각: S06b(10 / +800), W01(Escape 표 시험, 크기 같음), W09·W10(수용 시험), D01(~19 / −520). r4에서 바뀐 조각: S06b(14 / +1,050, +1200 아래), D01(~26 / −430).
순서: S 트랙과 W 트랙은 병렬. W09 전에는 Windows 기본 전환이 없고, W10 전에는 ProcessBackend 삭제가 없다. DoD 3 최종 검사는 S08과 D05 뒤 각각 한다.

### 12.4 첫 조각 S01 — 구현 레그 인계

- 목적: 관리 세션의 tmux 종료 효과(`kill_session`·`kill_session_output`·`kill_session_output_timeout`)를 `TmuxHost` 메서드 뒤로 옮긴다. 동작·조건·반환·감사 기록 동일.
- `TmuxHost`에 `kill(name, reason) -> bool`, `kill_output(name, reason)`, `kill_output_timeout(name, reason, timeout)`를 더하고 `platform::tmux`에 위임한다(새 정책 없음).
- 바꿀 호출부: 생산 텍스트(`RUST_LEXER._production_text`, 통째 시험 파일·`cfg(test)` 제외)에서 두 표기(`platform::tmux::kill_session*`와 `tmux::kill_session*`)를 모두 찾은
  31곳 중 관리 세션 28곳, 21파일. 전수표 terminate·BYPASS의 `tmux::kill_session*` 행 28개와 좌표가 같다(r2 재분류 반영, 전송 trait 메서드 `src/services/codex_tui/host_input.rs:681`은 경로 표기가 아니라 S03).
  `src/cli/utils.rs:186`; `src/engine/ops/exec_ops.rs:437`, `:438`; `src/services/claude_tui/host_input.rs:216`; `src/services/codex.rs:1243`, `:1395`; `src/services/codex_tui/host_input.rs:188`;
  `src/services/discord/commands/control/managed_reset.rs:142`; `src/services/discord/commands/tmux_recreate.rs:57`; `src/services/discord/router/message_handler/provider_isolation.rs:546`;
  `src/services/discord/tmux_reaper.rs:329`, `:553`, `:674`, `:1542`; `src/services/discord/tmux_watcher/post_stream_exit.rs:273`; `src/services/discord/tmux_watcher/pre_emit_guard.rs:330`;
  `src/services/discord/tmux_watcher/terminal_abort_exits.rs:148`; `src/services/discord/turn_bridge/retry_state.rs:376`; `src/services/discord/watchers/lifecycle/restore.rs:890`;
  `src/services/dispatched_sessions/tmux_cleanup.rs:338`; `src/services/provider/cancel_token_cleanup/executor.rs:371`; `src/services/provider_teardown.rs:84`;
  `src/services/qwen/session_lifecycle.rs:57`, `:83`, `:105`; `src/services/tmux_turn_liveness.rs:48`; `src/services/tui_prompt_dedupe/binding_context.rs:466`; `src/services/turn_lifecycle.rs:489`.
- `src/cli/utils.rs:186`은 reset-tmux(`src/cli/utils.rs:166-194`)의 `AgentDesk-*` 관리 세션 종료다. 기존 marker 거절·이름 필터와 시험
  `reset_tmux_leaves_another_hosts_session_running`(`src/cli/utils.rs:224`)을 그대로 지나게 옮긴다. 같은 함수의 나열(`:168`)은 관찰이라 S02.
- 건드리지 않음: `src/services/codex_tui/host_input.rs:681`(전송 trait 메서드, S03), `src/services/claude/herdr_return.rs:80`, `:83`(`/exit` 경로, S03),
  비관리 종료 3곳(`src/cli/dcserver.rs:831`, `src/server/routes/provider_auth_profiles.rs:341`, `:490`), process 행(D 트랙).
- ratchet(`scripts/check_destructive_call_site_ratchet.py`): 지금 `tmux_kill` 패턴(`:199-203`)은 `platform::tmux::` 표기만 잡고, 통째 시험 파일 제외(`:671`) **전에** 센다(`:664-669`).
  `use crate::services::platform::tmux;` 뒤 `tmux::kill_session(...)` 표기는 빠지고, 시험 파일의 호출은 들어간다. 그래서 기존 범주는 통계용으로 그대로 두고
  새 범주 `managed_tmux_kill_outside_adapter`를 더한다:
  (1) 두 표기를 모두 잡는 패턴(`(?:platform\s*::\s*)?tmux\s*::\s*kill_session(?:_output_timeout|_output|_checked)?\s*\(`, `TmuxHost` 메서드 이름과 겹치지 않게 경로 접두 필수),
  (2) `_is_whole_test_file` 제외 뒤 `RUST_LEXER._production_text`로 센 생산 텍스트만,
  (3) 어댑터 파일(`src/services/session_host/tmux_host.rs`, `src/services/platform/tmux*`)은 제외, (4) 허용은 위 비관리 3곳의 **파일 단위 수**를 검사기 상수로 고정
  (`src/cli/dcserver.rs` 1, `src/server/routes/provider_auth_profiles.rs` 2). 그 밖의 파일에 하나라도 생기면 실패, 허용 파일의 수가 늘어도 실패.
  스크립트 시험에 두 표기·시험 파일·`cfg(test)` 블록·어댑터 파일 픽스처를 넣어 각 규칙을 고정한다.
  같은 조각에서 생성·입력·관찰 tmux 세션 함수와 tmux 인자 조립에 "어댑터 밖" 범주를 현재 수 기준선으로 더해, W 트랙과 이후 조각이 새 우회를 늘리지 못하게 한다(줄기만 함).
  기준선 수치·허용 항목은 코디네이터 소유라 FILES로 승인받는다.
- 시험: 새 Rust 시험 없음(동작 보존 이동). 바뀐 모듈과 호출 형제 모듈 시험(`turn_lifecycle`, `tmux_reaper`, `provider_teardown`, `cancel_token_cleanup`, `exec_ops`, `managed_reset`, `cli::utils`)과 source-scan reader,
  ratchet 스크립트 시험. 뮤턴트: 어댑터 메서드가 reason을 버리는 변형을 기존 감사 시험이 잡는지, ratchet이 `tmux::` 표기 새 호출을 잡는지 확인(커밋하지 않음).
- 크기: prod 22파일, 순증 약 +95(스크립트 제외). Windows: unix 전용 API를 새로 쓰지 않음(CI가 컴파일 확인).

## 13. [U] 통합

합친 원본: A의 U-W1~W4·U-F4·U-CENSUS·U-SIZE·U-MODEL, B의 U1~U12, C의 U01~U14. 뺀 것: Herdr 설치 경로·버전(C가 0.9.5 확인), hc 미설치(C가 확인),
U-CENSUS(C 전수표로 해소), U-MODEL(이번 모델 실행으로 해소), U-SIZE(조각마다 `pr_cap_prod.py`로 확정, Windows 측정 아님), A U-W3(Qwen wrapper 경로를 택하지 않아 불필요). A U-W2는 wrapper 대신 "process 시절 transcript의 O 커서" 질문으로 바꿔 U20에 넣었다.
모든 실측은 격리 sandbox(새 임시 경로, `HERDR_SESSION=i7583-<nonce>`, 직접 띄운 child만 대상)에서 한다. 운영 dcserver·AgentDesk-* 세션·설정에는 적용하지 않는다.

| ID | 무엇 | win-desktop에서 잴 방법 | 관측값 | 기대 / 어긋나면 | 막는 조각 | 출처 |
|---|---|---|---|---|---|---|
| U01 | pipe 이름·schema·framing | `herdr api schema --json` 저장; 임시 서버 `herdr --session i7583-<nonce> status server`; PowerShell `[IO.Directory]::GetFiles('\\.\pipe\') -match 'herdr'`; `NamedPipeClientStream`으로 `{"id":"1","method":"ping","params":{}}` + 같은 연결 두 번째 요청 | pipe 실제 이름, version/protocol, 연결당 요청 수 | protocol 22(`src/services/session_host/herdr/model.rs:13`)와 일치, 연결당 1요청 / 다르면 W02 규약 조정 | W02 | C U01, B U1·U2 |
| U02 | 송신 경계 | 임시 pipe proxy로 0바이트 전 실패·첫 write 뒤 timeout·ACK 유실·malformed id 주입 | 결과 분류 | NotSent/Indeterminate 분리, 재송신 0 | W02 | C U02 |
| U03 | 서버 witness·E7·loaded image | `GetNamedPipeServerProcessId`·`GetProcessTimes`·`QueryFullProcessImageNameW` 기록; 서버 PID 교체·설정 교체·바이너리 교체 각각 | PID·FILETIME·이미지 경로 | 같은 handle 서버만 승인, 미증명 Off는 Withheld / 안 되면 해당 launch 모드 비활성 | W03a·W03b | C U03, A U-W4 |
| U04 | Herdr 서버의 job 탈출 | 코드 기준 dcserver launch를 재현한 임시 부모가 kill-on-close job 안에서 (a) 일반 자식 서버 (b) `BREAKAWAY_OK` + `CREATE_BREAKAWAY_FROM_JOB` (c) herdr CLI 자체 기동 (d) WMI `Win32_Process.Create`로 서버를 띄운 뒤 부모 `taskkill /F`·job close; 작업 스케줄러·서비스 형태의 바깥 job도 | 서버·pane·provider PID+FILETIME 생존, API 재연결 | (a) 죽음 (b)~(d) 중 하나 이상 생존 / 모두 실패면 Windows 활성화 중단, DESIGN_DELTA | W04 | B U3, C U04, A U-W1 |
| U05 | launch·root/provider/nonce·셸 인용 | 임시 `workspace.create`; `herdr pane run <pane> "cmd /d /c <tmp>\t.cmd"`; 공백·한글 cwd, `.exe`/`.cmd`/npm shim, marker와 process tree | pane 기본 셸, 전달된 env·cwd, `HERDR_*` 변수 | nonce·root·provider 생성 시각 일치, 다른 incarnation 거절 | W06 | C U05, B U4 |
| U06 | 실행 계정·elevation·발견 | 코드와 같은 계정 경계의 임시 harness에서 candidate 발견·pipe 접속; `whoami /user`, 소유 프로세스 token SID·elevation | SID·elevation 일치 | 같은 계정 접속 / 불일치는 생성 전 오류 | W04 | C U06 |
| U07 | pane 생존 범위 | Herdr 창 닫기, RDP 로그오프, 임시 dcserver 재시작, (격리 VM) 재부팅 뒤 `herdr api snapshot` | pane 유지 여부 | 창 닫기·dcserver 재시작 유지, 재부팅 소멸 / 로그오프에 죽으면 서비스 계정 실행 검토(사용자 결정) | W09 | B U9 |
| U08 | Claude TUI in ConPTY | pane에서 `claude` 실행 뒤 bracketed paste, `enter`·`esc`·`ctrl+u`·`ctrl+c`, `pane.read`; SessionStart·Stop hook의 HTTP POST 도달; `%USERPROFILE%\.claude\projects\` 경로; `claude --resume <id>` | 붙여넣기 한 덩어리 여부, 키 동작, hook 도달, transcript 경로 | Unix와 같음 / 키 표 Windows 분기 | W06·W09 | B U5·U7, C U07 |
| U09 | `pane.process_info` | claude 실행 중·종료 뒤 호출 | foreground pid·이름 | 셸 pid만 나오면 점유 증거를 hook nonce + 자식 목록으로 | W03b | B U6 |
| U10 | Codex TUI | rollout 경로, `codex resume`, 키·붙여넣기, 첫 prompt와 첫 hook 순서 | U08과 같은 항목 | Unix와 같음 / 다르면 W10만 늦춤 | W10 | B U10, C U07 |
| U11 | 재시작 crash 행렬 | 임시 dcserver를 create 앞·ACK 유실·위치 CAS 앞·**위치 CAS 뒤 실행 명령 확인 전·provider 기동 중·expected 기록 뒤 Bound 전·Codex 첫 prompt 전·첫 write 뒤 hook 전**·Bound idle·턴 중간·완료 뒤·O 게시 앞에 종료 후 같은 sandbox로 재기동. 같은 서버에서 닫힌 pane의 id가 재사용되는지도 기록 | server/pane/root/provider/session/nonce, 실행 수, 원문, 출력, placed Pending 결론, pane id 재사용 여부 | 같은 실행, 실행 수 1, 누락 0, 중복 0, terminal attach 0. 위치 있는 Pending은 사용자 입력 없이 §7.3의 한 결론으로 끝남, 첫 입력 전 Codex는 같은 nonce로 첫 prompt 1회 / Unknown이 반복되면 W09 보류 | W09 | C U08, r2 P1-1 |
| U12 | Cancel·Terminate·clear·stop | 임시 왕복 중 Cancel/Stop/HTTP force/clear/reset, close 앞·뒤 crash, foreign nonce | pane 생존·소유 child | cancel 뒤 생존, 종료 뒤 소유 child 0, 오류면 행·원문 보존, foreign 변경 0 | W09·D03 | C U09 |
| U13 | Qwen headless·기본값 표 | Windows에서 Qwen headless 첫·후속 입력과 resume; Unix harness PATH에서 tmux 제거 후 §5 표 전 칸 | 응답·오류 문구·원문 보존 | Qwen 동작, tmux 없음 거절 1·silent loss 0, 표 일치 | D01 | C U10, B 결정 2 |
| U14 | 운영 사실(읽기 전용) | 코디네이터가 운영 설정을 읽어 Windows 노드의 Qwen 채널 수, tmux 없는 Unix 노드 유무, 클러스터 정책의 `process` backend 요구, Windows 노드의 기존 `herdr_turn_enabled` 값 | 개수·값 | 모두 0 또는 영향 없음 / 정책 항목 먼저 정리 | D01·D04 | B U12 |
| U15 | Windows 컴파일 census(맥) | 게이트를 뒤집은 임시 브랜치에서 `cargo check --lib --tests --target x86_64-pc-windows-gnu` | 오류 수·파일 수 | W07·W08 각 30파일 이하 / 넘으면 더 나눔 | W07·W08 | B U8·P0 |
| U16 | Core 제공·버전·API | 격리 Windows Core에서 `hc --version`, `hc schema`, `hc --skill`; run/read/interrupt/kill/rm(소유 terminal만); 입력 quiet·Enter 지연 | 버전·schema 해시, 기본 quiet, 붙여넣기 뒤 Enter 지연 | 고정 버전 기록(0.1.14와 0.1.21 혼용 금지) | C01 | C U11, B U11 |
| U17 | Core holder·parent 수명 | `HC_SESSION=i7583-<nonce>`; `hc run --bg --name …` parent/child; core만 재시작; parent wait/message/timeout·parent 종료 | program 생존, 자식 취소 | core 재시작에도 같은 program, parent 종속 정확 | C02 | C U12 |
| U18 | F4 결과·OSC | `hc run --bg --name lane-<nonce> --cwd <fixture> <provider> <first-task>`; `hc wait <id>`/`--children`; `hc read <id>`; attempt 산출물 | 첫 지시 수, OSC, 결과 ID | 부팅 중 send 0, 첫 지시 1, message/timeout/not_started를 완료로 보지 않음 | C02 | C U13, A U-F4 |
| U19 | Core 재부팅·loss | 코디네이터가 준비한 **격리 VM**에서 owned terminal → VM 재부팅 → `hc ls`/read, F4 attempt | 옛 program 소멸, keep shell | 옛 attempt Lost, 자동 재송신 0, 고아 0. win-desktop 운영 호스트 재부팅 금지 | C02 | C U14 |
| U20 | process 시절 transcript의 O 커서 채택(맥, 코드·시험) | `src/services/tui_o/writer/late_adopt_binding_tests.rs` 계열과 `src/services/tui_o/channel_policy/adoption/`를 읽고, 임시 runtime root에서 Claude transcript를 adoption에 통과시킨 뒤 `src/services/claude/session_handoff.rs:155` `o_reads` 결과를 보는 대상 시험 | `o_reads` 참/거짓, 게시된 줄 수 | 참이고 게시 0이면 W09가 resume / 아니면 fresh + 알림. 어느 쪽도 입력 오류 반복 0 | W09 | A U-W2(재구성) |

U01~U12를 일부만 발사하지 않는다(W09의 일괄 조건). 실측 불가를 PASS로 보고하지 않는다. U04·U03·U05 중 핵심 계약이 불가능하면 W09 전에 DESIGN_DELTA로 돌아온다.

## 14. 상태 모델

34 cfg, TLC 1.7.4. 성립 11, 예상 RED 23. 상세·반례 대조는 [보고서](../statecharts/session-host/session-host.report.md).

| cfg | 깨지는 규칙 | 상태 수 |
|---|---|---|
| UnixTmuxCurrent, UnixTmux | 없음 | 3,481 |
| UnixHerdr | 없음(Claude, Cancel bool off → Escape 꺼진 채) | 6,034 |
| CodexHerdr | 없음(Codex, Unix endpoint, 재시작 복원 포함) | 8,340 |
| WindowsHerdr, WindowsImplicit | 없음 | 12,068 |
| CodexImplicit | 없음(Windows 기본 Codex, 첫 입력 경로) | 16,680 |
| UnixEndpointOffCurrent, UnixEndpointOff, WindowsSelectedCurrent | 없음 | 40 / 40 / 80 |
| UnixNoTmux | 없음(admission hold, tmux 복귀 뒤 재제출) | 6,886 |
| UnixHerdrCurrent | PendingEventuallyResolved, RestartEventuallyReconnect(main의 placed Pending) | 4,204 |
| CodexHerdrCurrent | PendingEventuallyResolved, RestartEventuallyReconnect, RestartedPendingResolved(main Codex의 placed Pending) | 5,915 |
| CodexNoRestore | PendingEventuallyResolved, RestartEventuallyReconnect, RestartedPendingResolved(r3 설계: 재시작 복원 없음 — 이번 P1) | 8,112 |
| HookConsumed | PendingEventuallyResolved, RestartEventuallyReconnect, RestartedPendingResolved(복원 전 hook이 무기록으로 소비) | 8,570 |
| ForeignSource | BoundOwnSource(다른 실행의 source로 Bound) | 8,394 |
| SkipPlaced | PendingEventuallyResolved, RestartEventuallyReconnect | 8,408 |
| BindProven | BoundOwnSource; CodexFirstInputDelivered(r2 처방: provider 증명만으로 Bound) | 35,462 |
| EscapeNoSwitch | UnixCancelAsMain(r2 표: Unix Claude Escape = settlement만) | 6,034 |
| GateRun | NoDefaultRefusal | 80 |
| GateCancel | NoDefaultCancelDrop | 12,068 |
| ErrConsumes | NoInputLoss, CustodyKept | 4,600 |
| NoAvailWake | HeldInputSubmitted | 3,513 |
| DropOnWake | CustodyKept; HeldInputSubmitted(wake가 보존을 풀고 실행 대기에 넣지 않음) | 3,687 |
| RequeueReach | NeverResubmitted(도달 증인: 보존된 입력이 tmux 복귀 뒤 실제로 시작됨) | 6,886 |
| WindowsCurrent | NoSessionLoss | 1,384 |
| UnixNoTmuxCurrent | NoSessionLoss, NoDoubleSession, NoOrphanAfterTerminate; RestartEventuallyReconnect, InputAnswered, TerminateEventuallyGone | 5,000 |
| SilentFallback | 위 6개 + NoProcessHost | 5,000 |
| WindowsServerInJob | NoSessionLoss | 13,170 |
| WindowsNoAutostart | HostRecovers | 5,926 |
| ReplayPending | NoSessionLoss, NoDoubleSession, NoOrphanAfterTerminate | 9,232 |
| EarlyRetire | NoSessionLoss, NoOrphanAfterTerminate | 12,200 |
| NoWake | RestartEventuallyReconnect, InputAnswered, PendingEventuallyResolved | 2,522 |
| ForgetIdentity | NoSessionLoss; RestartEventuallyReconnect, InputAnswered, PendingEventuallyResolved | 1,804 |

리뷰 항목의 수정 전/후: r3 P1-1 `BindProven` RED → `CodexImplicit`·`CodexHerdr` GREEN. r3 P1-2 `EscapeNoSwitch` RED → `UnixHerdr` GREEN(`UnixCancelAsMain`).
r3 P1-3·P2 `NoAvailWake`·`ErrConsumes` RED → `UnixNoTmux` GREEN, 재제출 도달은 `RequeueReach`가 증인. r2 항목(`SkipPlaced`, `GateRun`, `GateCancel`)은 그대로 RED → GREEN.
r4 P1 `CodexNoRestore`·`HookConsumed` RED → `CodexHerdr` GREEN, 남의 source는 `ForeignSource` RED(`BoundOwnSource`). r4 P2-3 `DropOnWake` RED → `UnixNoTmux` GREEN(`CustodyKept`).
r4 모델은 내구 source 기록(`srcOf`, 어느 실행의 source인지)과 재시작이 비우는 이 프로세스의 등록(`reg`)을 나눴다. hook은 등록된 실행에서만 source를 남기고(아니면 재시도),
attach와 placed resolver의 Bound는 등록 ∧ 자기 source를 요구한다. `RestoreReg`가 재시작 뒤 그 등록을 되살린다. 입력 한 건의 수탁(`owed`)은 도착부터 답(시작·실패·거절)까지다.
`PendingEventuallyResolved`는 "Bound·Retired, 또는 provider가 살아 있고 첫 입력 전이며 대기 입력이 없는 Pending"에 닿는 것이다(첫 입력 전 Codex는 source가 없어 Bound가 될 수 없다).
대기 입력이 있으면 `CodexFirstInputDelivered`가 첫 prompt 전달을 요구한다. 모델은 §5 표 전체의 동일성을 보이지 않는다(§5.2). hold가 남은 뒤 hook이 끝내 오지 않는 경우(§7.3 셋째 칸)는 모델에 넣지 않았다(main hold 계약).

## 15. DoD 대조

| DoD 문장 | 설계 절 | 조각 | 수용 시험 |
|---|---|---|---|
| Windows에서 Claude·Codex 관리 세션이 Herdr로 뜬다 | §5, §6 | W01~W10 | W09·W10 Windows CI + U08·U10 |
| 입력·출력·취소가 된다 | §8, §9 | S03, S05, W09 | U08·U12 |
| dcserver 재시작 뒤 재연결(Windows CI·win 실측) | §7 | W04, W08, W09 | W08 생산 부팅 도달 시험, U04·U11 |
| ProcessBackend·ProcessHost·`HostKind::Process`가 없고 Windows CI 녹색 | §10 | D00~D05 | 심볼 삭제 컴파일, D01 admission 시험(호스트 없음 → 자식 0·원문 보존·복귀 뒤 1회, 모든 provider, headless 호출자 정산) |
| 생성·입력·관찰·중단·종료가 trait을 지나고 우회 전수표 0 | §4 | S01~S08 | ratchet 0(두 표기·생산 텍스트), BYPASS 320행 재검색 |
| 채널별 tmux·Herdr 선택, 기본값 같음 | §5 | W01 | §5.2 표 기반 시험(전 조합·완전성)과 생산 경계 3건(Escape 표 포함). 모델은 보조(`GateRun`·`GateCancel`·`UnixCancelAsMain`) |
| (재시작 뒤 사람 개입 없음) 위치가 기록된 Pending도 자동으로 끝난다 | §7.3 | S06b, W08, W09, W10 | S06b 복구 패스 시험(첫 입력 전 Codex, 재시작한 Codex Pending 두 시점 포함), U11 |
| HerdrCoreHost가 F4 레인 하나를 띄우고 결과를 받는 실측 기록 | §11 | C01, C02 | U16~U19 기록 |

## 16. 리뷰 집중 항목

| 항목 | 막은 방법 / 남은 반례 |
|---|---|
| 고르지 않은 안의 결정이 충돌한 채 남았는가 | §2 충돌표 13건에 하나씩 판정. B의 수동 매핑·tmux 운영자 terminate·`exec_ops` 비관리, C의 Windows bool 적용·옛 Process 실행 이관, A의 Qwen wrapper를 기각하고 이유를 적었다. 남은 것: 없음 |
| Windows 재채택의 세션 유실 경로 | job 밖 서버(W04, U04)·witness(W03a, U03)·부팅 도달(W08, `NoWake`)이 모두 서야 유실 0. 모델 `WindowsServerInJob`·`NoWake`·`ForgetIdentity`가 각 조건을 뺀 반례. 합류 자체 공격에서 찾은 인접 경로: 첫 전환 때 process 시절 Claude 세션의 resume 거절이 입력마다 시작 실패를 내는 경로 → W09 수용 시험과 U20으로 막음. 남는 것: Herdr 서버 자체 소실(업데이트·재부팅·로그오프 U07)은 행 닫기 + 알림 + resume로 조용하지 않게 처리, Codex는 main 규칙대로 새 대화 |
| ProcessBackend 삭제가 tmux 부재 폴백을 조용한 유실로 바꾸는가 | router admission의 내구 hold + 알림 1회 + 호스트 복귀 wake(§10.2, `UnixNoTmux` GREEN, `SilentFallback`·`ErrConsumes`·`NoAvailWake` RED). 옛 `"process_backend"` 행 silent-skip은 D03이 막는다. 남는 것: 업그레이드 순간 옛 wrapper 고아는 정리하지 않음(main에 없던 kill) |
| 우회 전수표 대비 0 경로 | BYPASS 320행 → S01~S07이 파일 단위로 덮고, S01이 들인 ratchet이 W 트랙 포함 이후 조각의 새 우회를 막으며 S08이 0으로 닫는다. NON_MANAGED 28행은 좌표 허용 목록. 남는 것: 함수 포인터·재수출 호출은 grep으로 못 잡음 — 삭제는 컴파일러, 이동은 ratchet과 source scan |
| 채널 기본값이 지금과 같은가 | Unix 전 칸 동일(Escape 포함: Unix endpoint의 Claude·Codex Escape는 지금처럼 Cancel bool을 따른다) — 근거는 §5.2 표 기반 시험(리터럴 기대값, 완전성 검사)과 기존 `begin_user_stop` 생산 시험에 더한 None/false/true 표이고 모델 상태 수가 아니다. 세 bool은 Windows 암묵 기본에만 걸지 않아 공유 설정 false가 Windows 실행·Escape를 끊지 않는다. 바뀌는 두 칸(tmux 없는 Unix, Windows 무선택)은 삭제가 강제하는 변화로 표에 명시 |
| (r3) 위치가 기록된 Pending이 재시작 뒤 사람 개입 없이 끝나지 않는 경로가 남는가 | 막은 것: boot·health 패스의 placed resolver가 pane 없음·다른 nonce → retire, 셸만·Claude 미시작(hold 없음) → residue close 후 retire, provider가 살아 있으면 Pending을 지키고 대기 입력을 깨워 같은 nonce의 기존 첫 입력 경로(Codex 첫 prompt → hook → 기존 attach → Bound)로 잇고, 첫 prompt 뒤 attach 전에 끊긴 Codex는 hook이 기록되면 기존 attach로 Bound한다. provider 증명만으로 Bound하지 않는다(Bound 작성자는 main의 source 필요 3곳뿐). Windows 서버 교체는 W08이 재부팅 조건 없이 retire. **남은 반례**: 첫 write 뒤 hold가 남았는데 hook이 끝내 오지 않는 실행(입력이 composer에 남았을 수 있음)은 main hold 계약대로 자동 해제·재송신하지 않는다(`src/services/claude/herdr_turn.rs:557-564`, Bound 행도 같은 계약). probe가 계속 Unknown이면 행·원문을 지키며 30초마다 재판독. 모델 `BindProven`·`SkipPlaced`·`UnixHerdrCurrent`·`CodexHerdrCurrent` RED → `CodexImplicit`·`CodexHerdr`·`UnixHerdr`·`WindowsImplicit` GREEN |
| (r3) Windows 기본 전환 뒤 Codex 실행·Cancel이 다른 게이트에 막혀 거절되거나 tmux로 새는가 | 막은 것: `ChannelHostPlan`이 실행·Escape 허가를 선택과 함께 정하고 PlatformDefault(내부 endpoint 키로 만든 실행)만 세 bool을 우회한다. Escape 전달자는 하나(`codex_stop_delivery.rs:801`)이고 그 두 검사(`:250-262`, `:589-600`)가 계획 허가를 읽는다. 나머지 게이트(owner 입력 `cfg!(unix)`, `#[cfg(unix)]` 판정 `judged_stop.rs:364`·`:397`·`:469`·`:480`, settlement `cfg!(unix)`, Codex 실행 거절 `turn_host.rs:288-295`)를 W08·W09·W10이 열고 생산 진입점 시험이 bool 미설정/false에서 시작·Escape 1회를 단언. PlatformDefault의 host는 Herdr뿐이라 tmux 분기가 없고, Herdr를 못 쓰면 admission hold다. Unix·endpoint 채널은 bool을 그대로 따른다. 남은 반례: 없음(설계상). 모델 `GateRun`·`GateCancel`·`EscapeNoSwitch` RED → `WindowsImplicit`·`CodexImplicit`·`UnixHerdr` GREEN |
| (r3) tmux·Herdr 둘 다 없을 때 어떤 provider든 원문이 사라지는 경로가 남는가 | 막은 것: 판정을 `IntakeSubmission` 전체를 가진 router admission으로 옮겨 live·queued·slash·첨부 네 출처가 기존 deferred 계약(내구 수탁 뒤에만 checkpoint)을 지난다. intake_turn 단계(경합·ADK thread 재사용 thread)는 `Executed` 대신 host-held로 dispatcher에 돌려 같은 수탁을 거치고, headless는 예약 전 `HostUnavailable`로 프롬프트를 호출자에 남긴다. `provider_dispatch::execute` 생산 호출 2곳의 상위 진입을 모두 열거했다(§10.2 전수). 복귀 신호가 drain을 깨워 1회 제출. 남은 반례: admission과 spawn 사이에 호스트가 사라지는 좁은 경합에서 provider 함수의 방어 `Err`에 닿으면 기존 start failure 규칙을 따른다. 모델 `ErrConsumes`·`NoAvailWake` RED → `UnixNoTmux` GREEN, `RequeueReach` 증인 |
| (r4) 복원 helper가 다른 실행·세션의 source를 등록해 이중 세션·transcript 오귀속을 만드는가 | 막은 것: 검증이 첫 입력 준비와 같다(행 Pending·owner·nonce·spawn marker, `first_input.rs:90-106`). 목록은 같은 pane의 다른 nonce를 덮지 않고(`adopt_skip.rs:213-226`), source는 이 nonce의 baseline에서만 오며 live binding이 다른 source면 거절(`herdr_source.rs:310`), attach가 baseline = source ∧ live = source를 다시 본다(`:231-238`). 모델 `ForeignSource` RED → `CodexHerdr` GREEN(`BoundOwnSource`). 남은 반례: 없음(설계상) |
| (r4) "쓰기 없음"이 사실인가 | 막은 것: hold만 갈래는 메모리 세 호출뿐이다. source 있음 갈래는 DB·hold·pane·큐 쓰기 0, binding 로그 새 사실 0(`pane_registration.rs:247-248`). 남는 것: Bound 재시작 복원과 같이 로컬 Codex 커서 checkpoint에 복원한 같은 값을 다시 저장한다(`codex_cursor.rs:303-307`). 값이 같아 멱등이고 새 판정 입력이 아니다. DB 쓰기는 기존 attach의 `bind_pg` 하나(main과 같음) |
| (r4) 첫 입력 경로와 재시작 복원이 두 번 등록해 placeholder·채널이 겹치는가 | 막은 것: 같은 채널의 in-flight 턴이 있으면 resolver가 패스를 넘긴다. 겹쳐도 목록(같은 nonce면 참)·provider 매핑(같은 값)·placeholder(`or_insert`, live binding을 덮지 않음)·채널(같은 값)은 논리 키 맵이라 두 벌이 생기지 않는다. source 있음 갈래는 같은 source의 live binding을 그대로 쓴다(unread cursor 보존). 남은 반례: 없음 |
| (r4) 같은 부류(재시작이 메모리 등록을 비워 기존 경로가 NotPublished·거절·무기록)가 남는가 | 막은 것: Codex attach·Codex hook ingress·Claude resume SessionStart(`resume_start.rs:27-34`, 목록 요구)를 이번 개정에서 막았고, 복원이 채운 목록 때문에 health 패스가 resolver의 Pending을 건너뛰는 자리(`herdr_reader.rs:339-340`)는 resolver가 그 앞에서 판정한다. 나머지 reader는 §7.3 전수표에서 영향 없음·의도로 판정. 남은 반례: 판정 대기 집합이 비워진 뒤 늦게 온 미등록 hook은 main 그대로 넘어간다(그 실행은 이미 Bound·Retired·withhold 결론) |
| (r4) headless `HostUnavailable`이 호출자에서 재시도 소비·종단 실패가 되는가 | 막은 것: 문자열로 바꾸기 전에 타입으로 분류하고 생산 호출자 8개(호출 자리 10곳)의 정산을 §10.2 표로 고정했다. 예약 메시지는 재시도를 쓰지 않는 미룸, routine은 600초 창 밖 재시도, `turn_deliver`는 ack 0, HTTP는 503. 남은 반례: `/restart` seed는 main처럼 `Failed` 알림(사용자 원문이 아님) |
| (r3) S01 호출부 목록과 ratchet 기준선이 전수표와 일치하는가 | 일치(r2 리뷰도 재대조로 확인). 생산 텍스트 두 표기 스캔 31곳 = 관리 28(S01 목록 21파일) + 비관리 3(허용 상수 `src/cli/dcserver.rs` 1, `src/server/routes/provider_auth_profiles.rs` 2). 관리 세션 종료가 NON_MANAGED로 빠진 곳은 없다(`src/cli/utils.rs:186` r2 재분류). S01 계약은 r3에서 바꾸지 않았다 |
