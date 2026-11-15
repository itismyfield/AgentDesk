# #7583 최종 설계 — Herdr를 Windows 세션 호스트로, ProcessBackend 삭제, 세션 호스트 공통 경계

<!-- doc-path-refs: proposed src/services/session_host/herdr/transport_pipe.rs src/services/session_host/herdr_core/ src/services/platform/file_identity.rs src/services/session_host/channel_host_plan.rs -->

- 기준: `origin/main` `1974cff8a67bf5d1db041abfdd4d615c518ee104`(2026-11-15). 모든 좌표는 이 SHA에서 다시 확인했다.
- 입력: A(WebGPT pro 초안, 실측 없음), B(Fable xhigh, `refs/review/i7583-design-fable-1` `abad58858d`), C(WSL Codex, `refs/review/i7583-design-1` `b6766912fd`).
- **정본 기반: C.** 채택 이유는 §2. 사실 근거는 C의 전수표([진입점 표](i7583/entrypoints.md)와 TSV)를 우선했고, 전수표 밖 좌표는 합류 레그가 main에서 직접 확인해 보충했다.
- 상태 모델: `docs/statecharts/session-host/`(17 cfg, GREEN 8 + 예상 RED 9). [보고서](../statecharts/session-host/session-host.report.md).
- 이 레그는 설계만 한다. Rust 구현·cargo·운영 설정·Windows 접속은 하지 않았다.

## 0. 결론

1. Windows 관리 Claude·Codex 세션은 Unix와 같은 **Herdr TUI 실행 경로**로 띄운다. 새로 쓰는 것은 named pipe 전송, Windows OS 신원,
   Herdr 서버를 dcserver job 밖에서 띄우는 수명 계층, Windows launch 명령, 파일 신원 헬퍼다. 실행·입력·복구·종료 로직은 지금 Unix 코드다.
2. dcserver 재시작 재채택은 terminal attach·서버 넘기기 없이 이미 돈다(호출마다 새 연결 `src/services/session_host/herdr/transport.rs:1`,
   저장된 pane 재판독 `src/services/discord/recovery_engine/herdr_reader.rs:220`). Windows에서 막는 것은 **dcserver의 kill-on-close job**이다
   (`src/services/platform/windows_job.rs:35`, `:52`, 부팅 `src/cli/dcserver.rs:882`). Herdr 서버가 이 job 안에 있으면 dcserver와 함께 죽는다(모델 `WindowsServerInJob`).
3. 채널 선택은 `runtime.herdr_turn_enabled` 자리를 bool 호환 타입으로 넓힌다. Unix 기본값은 지금과 같다. **선택이 없는 Windows 관리 Claude/Codex만**
   내부 dedicated endpoint로 Herdr를 쓴다(설정에 쓰지 않고 부팅마다 재구성). 운영자가 채널을 옮겨 적는 수동 이사는 없다.
4. ProcessBackend는 Windows Claude·Codex 활성화 뒤 지운다. 삭제 뒤 tmux 없는 Unix 관리 세션은 spawn 전에 명시적 오류로 답하고 원문을 보존한다.
   Windows Qwen은 세션 없는 직접 실행(이미 있는 headless 경로)으로 간다.
5. `InteractiveSessionHost`는 `create`·`submit`·`interrupt`·`terminate(warrant)`·`recover`를 얻는다. 정책(허가·fence·신원·큐 원문·O 게시)은 지금 소유자에 둔다.
   "우회 0"은 **관리 세션의 물리 효과가 `session_host` 어댑터 메서드로만 일어난다**로 정의하고 ratchet으로 지킨다.
6. Herdr Core는 채널 세션 호스트가 아니라 #7438 F4 레인 전용 구현체다.
7. 착지 조각 **32개**(공통 13·Windows 11·삭제 6·Core 2) + 측정 1(W00). 미측정 [U] **20개**(win-desktop 17, 격리 VM 1, 맥 2).

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

### 충돌한 결정과 판정

| 쟁점 | 안 | 판정과 근거 |
|---|---|---|
| 선택 없는 Windows 채널 | B: 삭제 뒤 오류, 운영자가 매핑 / C·A: 내부 기본 Herdr | **C.** 수동 설정 이사는 사용자 결정 위반. 내부 endpoint는 설정에 쓰지 않는다 |
| `herdr_turn_enabled` bool이 Windows 암묵 기본에 걸리는가 | C: 명시 false면 Windows도 거절 | **걸리지 않는다(합류 판정).** bool의 현재 뜻은 "endpoint가 설정된 채널의 Claude 턴 허가"다(`src/config/runtime_settings.rs:87-90`). 공유 설정의 false가 Windows 노드 전체를 끊는 것을 막는다. Windows에서 명시 채널 `"tmux"`는 명시적 unavailable |
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
`platform::tmux`의 세션 함수, tmux 명령 인자 조립, Herdr 요청, provider spawn은 어댑터 밖에서 0이다. 예외는 C 전수표의 `NON_MANAGED` 30행
(`src/cli/dcserver.rs:831`, `src/server/routes/provider_auth_profiles.rs:339`, `src/cli/utils.rs:186` 등)과 `TEST_OR_HELPER`뿐이고 ratchet 허용 목록에 좌표로 고정한다.
닫힘의 증거는 [audited-bypasses.tsv](i7583/audited-bypasses.tsv) BYPASS 318행 각각의 호출 체인이 어댑터에 닿는 것이다. 단순 삭제로 0을 만들지 않는다.

## 5. 채널 선택과 기본값

`runtime.herdr_turn_enabled: Option<bool>`(`src/config/runtime_settings.rs:90`) 자리에 untagged `Legacy(bool) | Channels { channels: BTreeMap<ChannelId, "tmux" | "herdr"> }`를 넣는다.
목록에 없는 채널은 `Legacy(false)`와 같다. 모르는 값·필드는 검증 오류. 새 top-level 키·명령은 없다. `herdr_codex_turn_enabled`는 그대로 Codex의 별도 게이트다.
endpoint의 위치는 기존 `session_hosts`(`src/config/session_hosts.rs:350`)가 소유한다.

선택은 하나의 `ChannelHostPlan { host, endpoint, source: Explicit | Endpoint | PlatformDefault }`으로 정규화하고, 생성(`src/services/turn_host.rs:151`)·
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
| unix, tmux 없음 | 없음 | 없음 | process 폴백 | **spawn 전 오류, 원문 보존** | D01 |
| windows | 없음 | 없음, Claude/Codex | process | **Herdr(내부 dedicated endpoint)** | W09(Claude)·W10(Codex) |
| windows | 있음 | true/`"herdr"` | 거절(`cfg!(unix)`) | Herdr | W09 |
| windows | 무관 | 채널 `"tmux"` | (표현 없음) | 명시적 unavailable | W09 |
| windows | 없음 | Qwen | process | headless 직접 실행 | D01 |

Unix의 모든 줄은 W01 이후에도 같다(모델 `UnixTmux*`·`UnixHerdr*`: main과 최종 991 상태 동일). 바뀌는 두 줄(tmux 없는 Unix, Windows 무선택)은
ProcessBackend 삭제가 강제하는 변화이며 숨기지 않는다. 내부 Windows endpoint는 runtime root와 실행 노드에서 결정하는 키 `windows-managed-default/v1`과
고정 named session으로 부팅마다 재구성하고 설정에 저장하지 않는다. `legacy_dedicated` 모드만 쓴다(`user_default`는 어댑터 없는 플랫폼에서 부팅 거절,
`src/config/session_hosts.rs:193`). Herdr 미설치·접속 불가는 생성 전 명시적 오류와 원문 보존이며 자동 설치·자동 demotion은 없다.

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
| Pending 탈출 | `src/services/claude/herdr_turn/never_attached.rs:57-76` | 기존 unplaced census·in-flight·quiet 검사를 recovery wake에 연결해 다음 사용자 입력 없이도 정리. 안내문의 `agentdesk herdr terminate`/`retire` 명령은 기존 Unix 문구이며 Windows의 정상 탈출 경로로 쓰지 않는다(넓히지 않음, 문구 정리는 후속 후보) |
| Herdr 서버 소실·재부팅 | `src/services/discord/recovery_engine/herdr_server_replaced.rs:246`, `src/services/discord/recovery_engine/herdr_row_end.rs` | witness 불일치·부재 → 서버 재기동(W04) → 옛 실행 Retired + 알림 1회 → 대기 큐 wake → 다음 입력이 `--resume <id>`로 새 nonce 실행. 돌던 턴만 잃고 대화는 이어진다. Herdr 자체의 agent 자동 resume은 E7 off로 막아 이중 실행 0 |
| 옛 inflight 행 | `src/services/discord/recovery_engine/restore_inflight.rs:226`, `:985`; `src/services/discord/inflight/model.rs:1367` | D03: `"process_backend"` 문자열은 읽기 전용 legacy 값으로 남겨 "세션 없음"으로 닫고 알림 1회. 모르는 값으로 떨어져 조용히 건너뛰지 않게 한다. tmux로 추정하지 않는다 |
| 첫 Windows 전환 | O writer `src/services/tui_o/writer/renumbered.rs`, `src/services/tui_o/writer/rotation.rs`; resume 판정 `src/services/claude/herdr_turn/handoff.rs:38-43`, `src/services/claude/session_handoff.rs:155` | 유효한 Fresh owner → writer 빈 초기화 → 실제 checkpoint 0 저장 → input-ready → create와 첫 입력. O 파일에 0을 직접 쓰지 않는다. process 시절 Claude 세션은 O가 그 transcript의 커서를 갖지 않아 main 규칙대로 resume이 `Err`가 되고, 그대로 두면 채널의 **모든 입력이 시작 실패**한다. W09는 이 경우를 (1) 기존 writer adoption(#7651 단계 1, 파일 끝에서 commit)이 그 transcript를 채널 자기 bind로 묶어 `o_reads`가 참이 되면 resume, (2) 아니면 기존 `src/services/claude/fresh_notice.rs` 알림 1회와 fresh 실행으로 처리한다. 어느 쪽인지는 U20. 재게시·오류 반복·수동 `/clear` 요구 0. Codex Herdr 실행은 main에서 항상 fresh(`src/services/codex/herdr_turn.rs:194`)라 Windows Codex도 전환 때 새 대화로 시작한다(기존 Herdr Codex 제약, 넓히지 않음) |
| 채널 호스트 이동 | `src/services/turn_host.rs:102-121`, `src/services/claude/herdr_return.rs`, `src/services/claude/session_handoff.rs` | Unix 무변화. Windows는 tmux가 없어 Return이 생기지 않음 |
| `/restart`·tmux 재생성 | `src/services/discord/commands/restart.rs`, `src/services/discord/commands/tmux_recreate.rs:57` | 무변화(tmux 전용) |

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

### 10.2 tmux 없는 Unix

세 진입점에서 관리 세션 이름이 있고 tmux를 쓸 수 없고 Herdr 선택도 없으면 **아무것도 띄우지 않고** `Err`를 돌려 기존 start failure 경로
(`src/services/discord/router/message_handler/provider_dispatch/start_failure.rs`)로 사용자에게 한 번 알린다. 큐 원문은 소비하지 않는다.
문구 예: `managed session host unavailable: tmux is not installed on this node and the channel has no Herdr endpoint`.
일시 부재는 지금처럼 가용성 캐시(45초·3회)가 흡수하고, 기록된 pane이 살아 있으면 사용 가능으로 본다. 부팅을 막지 않는다(Herdr 채널·headless는 돈다).
업그레이드 순간 옛 바이너리가 남긴 process wrapper(자기 process group, `src/services/session_backend.rs:240`)는 정리하지 않는다(main에 없던 kill). 운영 노드 해당 여부는 U14.
모델 `UnixNoTmux` GREEN, `SilentFallback`(폴백을 남김) RED.

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
| **S01** | tmux 종료 효과를 `TmuxHost`로 이전 + 어댑터 밖 관리 효과 ratchet 도입(현재 수를 기준선으로) | §12.4 목록 20파일 + `src/services/session_host/tmux_host.rs` | 21 / +90 | 바뀐 모듈과 호출 형제 모듈의 기존 시험, ratchet: 종료 범주는 NON_MANAGED만, 다른 범주는 현재 수 이하 | CI 컴파일 | — |
| S02 | 관찰 포트: presence/liveness/has_session 경로 | `src/services/session_host/{tmux_host,legacy_collapse,resolve}.rs`, `src/services/tmux_diagnostics.rs`, `src/services/provider/session_probe.rs`, `src/services/session_activity.rs` 외 C S02 | 12 / +350 | 3값·기존 bool collapse 같은 장소·같은 뜻, ProbeError를 Missing으로 합치지 않음 | 컴파일 | S01 |
| S03 | `submit` 포트 + 입력 물리 효과(실행기 23곳의 tmux 인자 조립, FIFO, Herdr gated adapter) | `src/services/claude_tui/host_input.rs`, `src/services/codex_tui/host_input.rs`, `src/services/tui_input/actor/pane.rs`, `src/services/tui_input/submission_tmux.rs`, `src/services/claude_tui/busy_inject.rs` 외 C S03 | 18 / +850 | 원문 owner·first-key cancel·draft stash·partial write·receipt. spy가 어댑터 밖 tmux 호출 0 확인 | 컴파일 | S01 |
| S04 | `create` 포트 + 생성·핸드오프 | `src/services/claude/tui_session_launch.rs`, `src/services/claude/herdr_turn/handoff.rs`, `src/services/claude/herdr_turn/never_attached.rs`, `src/services/codex/tui_session_launch/first_input.rs`, `src/services/herdr_launch.rs` 외 C S04 | 18 / +850 | Pending crash·ACK 유실·동시 create 2개·same-session Return·argv. Codex create 완료에 SourceBound 요구 0 | 컴파일 | S01 |
| S05 | `interrupt`·`terminate(warrant)` 포트 + Herdr 종료 | `src/services/termination_audit/host_terminate.rs`, `src/services/discord/turn_bridge/tmux_runtime/judged_stop.rs`, `src/services/discord/turn_bridge/tmux_runtime/native_stop.rs`, `src/services/dispatched_sessions/herdr_kill/effects.rs` 외 C S05 | 16 / +600 | Cancel 뒤 세션 생존, close 전/후 crash, mismatch·Unknown warrant 무효, tmux 운영자 terminate Unsupported 유지 | 컴파일 | S01 |
| S06 | `recover` 포트 + 복구 소비자 | `src/services/discord/recovery_engine/host_reconcile.rs`, `src/services/discord/recovery_engine/herdr_reader.rs`, `src/services/discord/recovery_engine/herdr_server_replaced.rs`, `src/services/discord/recovery_engine/restore_inflight.rs`, `src/services/discord/tui_prompt_relay/herdr_source.rs`, `src/services/discord/tui_prompt_relay/rehydration.rs` 외 C S06 | 18 / +850 | source cursor·옛 형식·standby·boot+health 재채택·reconcile wake | 컴파일 | S02 |
| S07a~f | 잔여 소비자 6묶음 | [consumer-slices.tsv](i7583/consumer-slices.tsv)의 파일 목록(앞 조각과 겹치면 남은 효과만) | 각 ≤17 / +250 | leaf spy + source scan, 기존 guard·bool·결과 동등 | 각 컴파일 | S02~S06 |
| S08 | ratchet 마감: 모든 범주 어댑터 밖 관리 효과 0(W 트랙 코드 포함 재검색) | `scripts/check_destructive_call_site_ratchet.py`와 기준선 | 0 / 스크립트 | 스크립트 시험, 전수표 BYPASS 318행 재검색 0 | — | S07f |

### 12.2 Windows 트랙

| 조각 | 목적 | 소유 파일 | prod 파일 / 순증 | 수용 시험 | win | 의존 |
|---|---|---|---|---|---|---|
| W00 | 측정만: 게이트를 임시로 뒤집어 Windows 컴파일 오류·파일 수 census(착지 없음) | — | 0 | U15 기록 | 맥 | — |
| W01 | 선택 표현 + `ChannelHostPlan` 정규화(Windows는 W09 전까지 기존 결과). [state-entrypoints.tsv](i7583/state-entrypoints.tsv)의 `herdr_turn_enabled` reader·시험 setter 전부를 새 타입으로 | `src/config/runtime_settings.rs`, `src/config/session_hosts.rs`, `src/services/turn_host.rs`, reload/validation, 새 `src/services/session_host/channel_host_plan.rs`(제안) | 14 / +700 | §5 표 전 칸(bool/None/Channels, Codex 게이트, endpoint 없는 `"herdr"` 거절), stale current host, 생산 진입점에서 off일 때 IO 0 | 컴파일 | — |
| W02 | named pipe 전송 | `src/services/session_host.rs`, `src/services/session_host/herdr/{transport,model,wire,contract}.rs`, 새 `src/services/session_host/herdr/transport_pipe.rs`(제안), registry/factory | 10 / +950 | 실제 임시 pipe·framing·timeout·write 0바이트 전/후·동시 mutation 직렬. Unix rig 무변화 | **Windows CI** | W00 |
| W03a | 서버 witness(pipe 서버 PID·FILETIME·boot) | `src/services/session_host/herdr/provenance.rs`, `src/services/session_host/herdr/transport.rs` 소비부, Windows OS leaf | 6 / +500 | PID 재사용·새 서버·boot 변경·권한 오류를 absent로 바꾸지 않음 | **CI** | W02 |
| W03b | 점유·E7·loaded image·retired hold | `src/services/session_host/herdr/{pane_probe,observe}.rs`, `src/services/session_host/herdr/policy/`, `src/services/herdr_launch/retired_hold.rs` | 10 / +650 | 살아 있는/죽은 pid, nonce 점유, E7 Off 미증명 Withheld, hold 왕복, user_default 부팅 거절 유지 | **CI** | W03a |
| W04 | Herdr 서버 job 밖 기동 + 관리 설정 Windows판 | `src/services/platform/windows_job.rs`, `src/services/session_host/herdr/autostart.rs`와 하위, `src/services/session_host/herdr/managed_config.rs`, `src/bootstrap.rs` | 8 / +500 | Windows CI: job 안 부모가 죽어도 탈출시킨 서버는 산다, 탈출 안 한 자식은 죽는다. 기존 서버 발견 시 재시작·업그레이드 0 | **CI + win** | W02, U04 |
| W05 | 파일 신원·잠금 공통 헬퍼 | 새 `src/services/platform/file_identity.rs`(제안) + dev·inode 사용 파일 | 22 / +150 | Unix: 같은·다른·교체된 파일 판정이 이전과 같음. Windows CI: 같은 3경우 | **CI** | W00 |
| W06 | Windows launch 명령 + native hook/source | `src/services/herdr_launch.rs`, `src/services/claude/tui_session_launch.rs`, `src/services/codex/tui_session_launch/first_input.rs`, `src/services/discord/tmux_session_files.rs`, hook relay 경로 | 14 / +800 | `.cmd` 런처를 `cmd.exe`로 실행해 env·인자·cwd 전달, HERDR 변수 없음, 공백·한글·npm shim, SessionStart·rollout 경로 | **CI + win** | W03b, W04, W05 |
| W07 | 게이트 열기 1: 실행 경로 컴파일(스위치는 아직 `cfg!(unix)`) | `src/services/claude.rs`, `src/services/codex.rs`, `src/services/claude_tui/mod.rs`, `src/services/discord/router/message_handler/provider_dispatch.rs`와 하위 | ~20 / +300(W00로 확정) | Windows CI: 컴파일, 생산 진입점에서 Windows Herdr 턴 거절·IO 0 유지 | **CI** | W01, W06 |
| W08 | 게이트 열기 2: 복구·종료·관찰 컴파일 + 부팅 도달 | `src/services/discord/mod.rs`, `src/services/discord/recovery_engine.rs`, `src/services/discord/tui_prompt_relay.rs`와 하위 | ~25 / +400(W00로 확정) | Windows CI: 생산 부팅 진입점이 재시작 패스(`rehydration.rs:270`, `:668`)에 도달, 같은 서버 pane 재채택, 서버 교체면 Retired + 알림 1회 | **CI** | W07, S06 |
| W09 | 활성화 — Windows Claude: PlatformDefault Herdr, 스위치 `cfg!(unix)` 해제, owner 입력 경로, process 시절 세션의 resume 처리 | `src/services/turn_host.rs`, `src/services/discord/queue_io/input_owner/ingress.rs`, `src/services/provider_hosting.rs`, `src/services/claude/herdr_turn/handoff.rs`, CI 선택 감사 | 9 / +260 | 부팅·턴 중간·dcserver crash 행렬에서 같은 session/pane/nonce, 이중 0, O cursor, cancel/terminate, never-attached 자동 탈출, O writer 빈 초기화. 생산 진입점: O 커서 없는 저장 세션이 있는 채널의 첫 입력이 시작 실패가 아니라 resume 또는 fresh + 알림 1회로 시작 | **CI + win(U01~U12 전부)** | W08, U 패킷 |
| W10 | 활성화 — Windows Codex | `src/services/turn_host.rs`, Codex hosting 경로 | 6 / +150 | 위 행렬의 Codex판, 첫 입력 hook 순서 | **CI + win(U10)** | W09 |

### 12.3 삭제 트랙과 Core

| 조각 | 목적 | 소유 파일 | prod 파일 / 순증 | 수용 시험 | win | 의존 |
|---|---|---|---|---|---|---|
| D00 | 시험 대역 치환(시험만) | §10.4 둘째·셋째 부류 약 30 시험 파일, `src/services/session_host/test_support.rs` | 0(+가짜 호스트 60줄) | 바꾼 시험 통과, 단언 삭제 0 | CI | — |
| D01 | 라우팅·process launch 삭제, spawn 전 오류, Windows Qwen headless, 가용성 규칙 | `src/services/claude.rs`, `src/services/claude/backend_routing.rs`, `src/services/claude/process_session_launch.rs`, `src/services/codex/command_streaming.rs`, `src/services/codex/process_session_launch.rs`, `src/services/qwen.rs`, `src/services/qwen/session_lifecycle.rs`, `src/services/claude_command.rs`, `src/services/platform/tmux/availability.rs` | 10 / −900 | 생산 진입점: tmux·Herdr 둘 다 없으면 오류로 끝나고 자식 프로세스 0, 원문 보존. 살아 있는 pane이면 tmux | **CI** | W10, D00 |
| D02 | 레지스트리 조회 소비자 정리 | §10.1 소비자 15곳의 13파일 + `src/services/provider/session_probe.rs` | 13 / −120 | clear·reset·admin guard 기존 시험 | CI | D01 |
| D03 | `ProcessBackend` 종류·`ProcessReady`·process 취소 삭제 | `src/services/agent_protocol.rs`, `src/services/discord/turn_bridge/runtime_handoff_loop.rs`, `src/services/discord/turn_bridge/stream_loop.rs`, `src/services/discord/recovery_engine.rs`, `src/services/discord/turn_bridge/tmux_runtime.rs`, `src/services/discord/turn_bridge/tmux_runtime/process_backend_cancel.rs`, `src/services/discord/inflight/model.rs`, `src/services/discord/health/routine_process.rs` 등 | 24 / −600 | 0단계: 종류 없는 헤드리스 행의 현재 복구 분기를 시험으로 먼저 고정. 디스크 `"process_backend"` 행은 "세션 없음"으로 닫히고 알림 1회, silent-skip 0 | CI | D02 |
| D04 | 레지스트리·`ProcessHost`·`HostKind::Process` 삭제, 클러스터 광고 | `src/services/session_backend.rs`, `src/services/session_backend/auth_profiles.rs`, `src/services/session_backend/output.rs`, `src/services/session_host/process_host.rs`, `src/services/session_host/model.rs`, `src/services/session_host/resolve.rs`, `src/domain/host_observation.rs`, `src/services/cluster/readiness.rs` 등 | 14 / −900 | 실행 소스의 ProcessBackend/ProcessHost/Process kind 0, JSONL 파서·3종 runtime 유지, 옛 `process` 표식 Unrecognized | CI | D03, U14 |
| D05 | wrapper pipe 모드·CLI 인자 삭제, CLI 문서 재생성 | §10.1 wrapper 행 9파일 | 9 / −250 | FIFO 모드 시험, `--input-mode pipe` 거절 | CI | D04 |
| C01 | `HerdrCoreHost`(hc CLI) | 새 `src/services/session_host/herdr_core/`(제안), `src/domain/host_observation.rs` | 6 / +700 | hc 출력 JSON 파싱 계약, 고정 버전 왕복(실제 hc 있을 때) | win(U16) | S04·S05·S06 |
| C02 | F4 레인 하나 실측 기록 + 하니스 | #7438 소유 소비자 경로, 기존 command/store API를 부르는 하니스 | ≤10 / +800 | 첫 지시·OSC·parent wait·결과·재부팅 loss, 다른 terminal 정리 0 | **win(U17~U19)** | C01, #7438 F4 intake |

조각 수: S01~S06 6 + S07a~f 6 + S08 1 + W01~W10(W03a·b 포함) 11 + D00~D05 6 + C01·C02 2 = **착지 32**, 측정 W00 1.
순서: S 트랙과 W 트랙은 병렬. W09 전에는 Windows 기본 전환이 없고, W10 전에는 ProcessBackend 삭제가 없다. DoD 3 최종 검사는 S08과 D05 뒤 각각 한다.

### 12.4 첫 조각 S01 — 구현 레그 인계

- 목적: 관리 세션의 tmux 종료 효과(`kill_session`·`kill_session_output`·`kill_session_output_timeout`)를 `TmuxHost` 메서드 뒤로 옮긴다. 동작·조건·반환·감사 기록 동일.
- `TmuxHost`에 `kill(name, reason) -> bool`, `kill_output(name, reason)`, `kill_output_timeout(name, reason, timeout)`를 더하고 `platform::tmux`에 위임한다(새 정책 없음).
- 바꿀 호출부(C 전수표 terminate·BYPASS 중 tmux, process 행 제외):
  `src/engine/ops/exec_ops.rs:437`, `:438`; `src/services/claude_tui/host_input.rs:216`; `src/services/codex.rs:1243`, `:1395`; `src/services/codex_tui/host_input.rs:188`;
  `src/services/discord/commands/control/managed_reset.rs:142`; `src/services/discord/commands/tmux_recreate.rs:57`; `src/services/discord/router/message_handler/provider_isolation.rs:546`;
  `src/services/discord/tmux_reaper.rs:329`, `:553`, `:674`, `:1542`; `src/services/discord/tmux_watcher/post_stream_exit.rs:273`; `src/services/discord/tmux_watcher/pre_emit_guard.rs:330`;
  `src/services/discord/tmux_watcher/terminal_abort_exits.rs:148`; `src/services/discord/turn_bridge/retry_state.rs:376`; `src/services/discord/watchers/lifecycle/restore.rs:890`;
  `src/services/dispatched_sessions/tmux_cleanup.rs:338`; `src/services/provider/cancel_token_cleanup/executor.rs:371`; `src/services/provider_teardown.rs:84`;
  `src/services/qwen/session_lifecycle.rs:57`, `:83`, `:105`; `src/services/tmux_turn_liveness.rs:48`; `src/services/tui_prompt_dedupe/binding_context.rs:466`; `src/services/turn_lifecycle.rs:489`.
- 건드리지 않음: `src/services/codex_tui/host_input.rs:681`(전송 trait 메서드, S03), `src/services/claude/herdr_return.rs:80`, `:83`(`/exit` 경로, S03), NON_MANAGED(`src/cli/dcserver.rs:831`, `src/cli/utils.rs:186`, `src/server/routes/provider_auth_profiles.rs:341`, `:490`), process 행(D 트랙).
- ratchet: `scripts/check_destructive_call_site_ratchet.py`의 `kill_session` 범주(`:202`)에 어댑터 파일 소유 규칙을 더하고 기준선을 NON_MANAGED 4곳으로 낮춘다. 같은 조각에서 생성·입력·관찰 tmux 세션 함수와 tmux 인자 조립에 "어댑터 밖" 범주를 현재 수 기준선으로 더해, W 트랙과 이후 조각이 새 우회를 늘리지 못하게 한다(줄기만 함). 기준선 수치·허용 항목은 코디네이터 소유라 FILES로 승인받는다.
- 시험: 새 시험 없음(동작 보존 이동). 바뀐 모듈과 호출 형제 모듈 시험(`turn_lifecycle`, `tmux_reaper`, `provider_teardown`, `cancel_token_cleanup`, `exec_ops`, `managed_reset`)과 source-scan reader. 뮤턴트: 어댑터 메서드가 reason을 버리는 변형을 기존 감사 시험이 잡는지 확인(커밋하지 않음).
- 크기: prod 21파일, 순증 약 +90. Windows: unix 전용 API를 새로 쓰지 않음(CI가 컴파일 확인).

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
| U11 | 재시작 crash 행렬 | 임시 dcserver를 create 앞·ACK 유실·위치 CAS 앞·Bound idle·턴 중간·완료 뒤·O 게시 앞에 종료 후 같은 sandbox로 재기동 | server/pane/root/provider/session/nonce, 실행 수, 원문, 출력 | 같은 실행, 실행 수 1, 누락 0, 중복 0, terminal attach 0 | W09 | C U08 |
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

| cfg | 깨지는 규칙 | 상태 수 |
|---|---|---|
| UnixTmuxCurrent, UnixTmux, UnixHerdrCurrent, UnixHerdr | 없음 | 991 |
| UnixNoTmux | 없음 | 40 |
| WindowsHerdr, WindowsImplicit | 없음 | 991 |
| WindowsSelectedCurrent | 없음 | 40 |
| WindowsCurrent | NoSessionLoss | 238 |
| UnixNoTmuxCurrent | NoSessionLoss, NoDoubleSession, NoOrphanAfterTerminate; RestartEventuallyReconnect, InputAnswered, TerminateEventuallyGone | 231 |
| SilentFallback | 위 6개 + NoProcessHost | 231 |
| WindowsServerInJob | NoSessionLoss | 1,113 |
| WindowsNoAutostart | HostRecovers | 528 |
| ReplayPending | NoDoubleSession | 634 |
| EarlyRetire | NoSessionLoss, NoOrphanAfterTerminate | 1,004 |
| NoWake | RestartEventuallyReconnect, InputAnswered, PendingEventuallyResolved | 364 |
| ForgetIdentity | NoSessionLoss; RestartEventuallyReconnect, InputAnswered, PendingEventuallyResolved | 238 |

## 15. DoD 대조

| DoD 문장 | 설계 절 | 조각 | 수용 시험 |
|---|---|---|---|
| Windows에서 Claude·Codex 관리 세션이 Herdr로 뜬다 | §5, §6 | W01~W10 | W09·W10 Windows CI + U08·U10 |
| 입력·출력·취소가 된다 | §8, §9 | S03, S05, W09 | U08·U12 |
| dcserver 재시작 뒤 재연결(Windows CI·win 실측) | §7 | W04, W08, W09 | W08 생산 부팅 도달 시험, U04·U11 |
| ProcessBackend·ProcessHost·`HostKind::Process`가 없고 Windows CI 녹색 | §10 | D00~D05 | 심볼 삭제 컴파일, `tmux_missing_turn_errors_without_spawning`(생산 진입점) |
| 생성·입력·관찰·중단·종료가 trait을 지나고 우회 전수표 0 | §4 | S01~S08 | ratchet 0, BYPASS 318행 재검색 |
| 채널별 tmux·Herdr 선택, 기본값 같음 | §5 | W01 | §5 표 전 칸 시험, 모델 Unix 쌍 동일 상태 수 |
| HerdrCoreHost가 F4 레인 하나를 띄우고 결과를 받는 실측 기록 | §11 | C01, C02 | U16~U19 기록 |

## 16. 리뷰 집중 항목

| 항목 | 막은 방법 / 남은 반례 |
|---|---|
| 고르지 않은 안의 결정이 충돌한 채 남았는가 | §2 충돌표 13건에 하나씩 판정. B의 수동 매핑·tmux 운영자 terminate·`exec_ops` 비관리, C의 Windows bool 적용·옛 Process 실행 이관, A의 Qwen wrapper를 기각하고 이유를 적었다. 남은 것: 없음 |
| Windows 재채택의 세션 유실 경로 | job 밖 서버(W04, U04)·witness(W03a, U03)·부팅 도달(W08, `NoWake`)이 모두 서야 유실 0. 모델 `WindowsServerInJob`·`NoWake`·`ForgetIdentity`가 각 조건을 뺀 반례. 합류 자체 공격에서 찾은 인접 경로: 첫 전환 때 process 시절 Claude 세션의 resume 거절이 입력마다 시작 실패를 내는 경로 → W09 수용 시험과 U20으로 막음. 남는 것: Herdr 서버 자체 소실(업데이트·재부팅·로그오프 U07)은 행 닫기 + 알림 + resume로 조용하지 않게 처리, Codex는 main 규칙대로 새 대화 |
| ProcessBackend 삭제가 tmux 부재 폴백을 조용한 유실로 바꾸는가 | spawn 전 오류 + 원문 보존 + start failure 알림(`UnixNoTmux` GREEN, `SilentFallback` RED). 옛 `"process_backend"` 행 silent-skip은 D03이 막는다. 남는 것: 업그레이드 순간 옛 wrapper 고아는 정리하지 않음(main에 없던 kill) |
| 우회 전수표 대비 0 경로 | BYPASS 318행 → S01~S07이 파일 단위로 덮고, S01이 들인 ratchet이 W 트랙 포함 이후 조각의 새 우회를 막으며 S08이 0으로 닫는다. NON_MANAGED 30행은 좌표 허용 목록. 남는 것: 함수 포인터·재수출 호출은 grep으로 못 잡음 — 삭제는 컴파일러, 이동은 ratchet과 source scan |
| 채널 기본값이 지금과 같은가 | Unix 전 칸 동일(모델 991=991). bool은 Windows 암묵 기본에 걸지 않아 공유 설정 false가 Windows를 끊지 않는다. 바뀌는 두 칸(tmux 없는 Unix, Windows 무선택)은 삭제가 강제하는 변화로 표에 명시 |
