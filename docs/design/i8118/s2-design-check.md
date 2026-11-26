# #8118 S2a 착수 1 — 설계 ↔ 코드 대조, 겹침, 구현 계획

- 입력은 WebGPT pro 설계 `design-s2-1.out.md`(VERDICT: DESIGN_READY)다. 이 설계의 입력은 준비 묶음 `refs/review/i8118-s2-prep-1`(`b52e052e24`)이었다. 설계 원문은 lane-runs에 있고 저장소에는 없다.
- 대조 기준:
  - 코드는 `b52e052e24e13781569050ec940eb35b9496ed54`(prod는 base `b04ea56650`과 같다).
  - `origin/main` `c28732c52c`는 두 커밋 앞서 있지만, 아래 S2a 대상 파일에는 변경이 없다(`git diff --stat b04ea56650 origin/main -- <S2a 파일들>` 빈 값).
- 경로 표기: `src/services/` 아래는 접두를 생략한다. 그 밖의 경로는 `src/`부터 적는다.
- 이 레그는 prod 코드를 바꾸지 않았다. cargo도 실행하지 않았다.
- 설계상 활성화는 S2c에만 있다. S2a는 "현재 소비자·판정 정합"이고 S2b·S2c는 이번 범위가 아니다.

## 1. 설계 ↔ 코드 대조(S2a 범위)

판정 기호:
- **확인**: 설계 서술과 코드가 같다.
- **확인·주의**: 같지만 구현에서 지킬 조건이 있다.
- **미확인**: 이번에 코드로 닫지 못했다.

| # | 설계가 기대는 코드 사실(설계 위치) | 코드 좌표(b52e052e24) | 판정 | 비고 |
|---|---|---|---|---|
| F1 | `end_session`이 `host_decision_of(..).is_herdr()` 하나로 Herdr 경로를 고른다(Q11①) | `discord/commands/control/clear_teardown.rs:63,72-73` | 확인 | — |
| F2 | `end_herdr`가 미확인에도 false를 돌려 tmux teardown으로 떨어진다(Q11①) | `clear_teardown.rs:148`; 미확인 `:160-164`(풀·키 없음), `:187-191`(행 Unknown/Conflict). 둘 다 `end_unconfirmed = true` 뒤 `return false`. 호출부 `:78`에서 false면 `:90` managed teardown으로 진행 | 확인 | Legacy·Missing(`:179,:182`)도 같은 false다. 설계가 요구한 "처리됨 / Legacy·Missing 확인 / 미확인" 3분이 반환형에 없다 |
| F3 | `resume_fence::set_channel`이 `.is_herdr()` 하나에 기댄다 | `clear_teardown/resume_fence.rs:23-26` | 확인 | — |
| F4 | `recorded_execution`이 gate 거절을 검증 없이 `Some(true)`로 기록한다(Q11②, P2-2) | `turn_host.rs:171-187`; 기록 `:165`; 검증 함수 `is_live_thread_endpoint` `:255-277` | 확인 | `for_turn`은 `resolve` 결과만 받는다. verified_bound는 `resolve`에서 위로 올려야 한다(`:224`에서 계산됨) |
| F5 | `herdr_codex`는 자기 boot(`has_herdr_endpoint`)를 보고, `herdr_now`가 이미 읽힌다(Q11③) | `discord/commands/config/effort.rs:135-142`(`herdr_now`), `:146-158`(`herdr_codex`), `:186`(capability가 `herdr_now` 읽음), `:199`, `:258-262`; `discord/commands/tui_passthrough.rs:94,98` | 확인 | prod 호출부 3곳(`effort.rs:199,262`, `tui_passthrough.rs:94`). 시험 `effort_settings_tests.rs:536-548`이 `herdr_codex(provider, channel)` 시그니처를 직접 부른다 → 시그니처를 바꾸면 fixture 적응이 필요하다(FILES_AUTO 대상) |
| F6 | `HostedObservation` 조회에 selector가 없다. 같은 snapshot에 넣을 수 있다(Q2) | `src/db/dispatched_sessions/hosted_execution.rs:204-209`(구조체), `:248-312`(`load_hosted_execution_pg`: id 해석 뒤 `:278-281` 단일 SELECT) | 확인 | `claude_session_id`를 `:279` SELECT에 더하면 hosted 상태와 같은 행 snapshot이 된다. 구조체를 만드는 곳은 prod `:308,:379`과 시험 helper 4곳이다 |
| F7 | 기존 selector 삭제는 `hosted_execution IS NULL` 행만 대상이다(Q2) | `src/db/dispatched_sessions.rs:2086-2091,2112-2124` | 확인 | — |
| F8 | FreshSubstitution 위치(Q2·C2) | `claude/tui_session_launch/prepared.rs:119-151` | 확인 | — |
| F9 | `input_runs_herdr`가 tmux probe를 생략해 Legacy를 관측 없이 Nothing으로 만든다(Q7) | `session_host/host_decision.rs:151-160`(`probe_tmux=false`), `:280-282`(`return Observed::Nothing`) | 확인·주의 | S2a에서는 Nothing이 여전히 Tmux(`:203`)라서 probe를 되살려도 동의 결과가 같다. 바뀌는 것은 Herdr 부모 아래 user_default 스레드 입력마다 tmux probe 1회(3초 상한)가 늘어나는 비용뿐이다(S1b 설계 이탈 6이 줄인 것). 넣는 시점은 계획 Q-P2 |
| F10 | `host_decision`에 Present/Missing/ProbeFailed 관측이 이미 있다(Q1) | `host_decision.rs:283-289` | 확인 | — |
| F11 | `turn_host`는 행만 읽고 tmux 생존을 보지 않는다(Q1 상태표의 전제) | `turn_host.rs:200-252`(tmux 관련은 `return_plan`의 marker `:133`뿐) | 확인 | 활성화만 하면 살아 있는 tmux에서 selector가 없는 스레드도 Herdr로 간다. 모델 `S2ExecNaive` `LiveTmuxStaysTmux` RED. S2a "관측 공통화"가 이 공백을 메운다 |
| F12 | `record_judged`는 확인된 Child만 기록한다(Q10) | `session_host/thread_host.rs:90-97` | 확인 | — |
| F13 | `next_host`/`next_is_herdr`가 provider 없이 판정한다(E37·E83) | `thread_host.rs:137,153,158`; 소비자 `discord/router/intake_gate/busy_inject.rs:103-109`, `discord/turn_presence/activity/unbound/completed_scheduled.rs:162` | 확인 | — |
| F14 | 관계 기록이 O 선택 부모에서만 일어난다(Q14) | `tui_o/cutover.rs:175-201`(`record_thread_child_by_id`: `channels().contains(parent)`·`kind(parent)` 없으면 `Ok(false)`), `:204-225`(`record_in`: `snapshot.kind(parent)` 필요) | 확인 | `record_in`이 `install_thread_writer_policy`도 부른다(`:216-221`). 관계 기록을 분리할 때 이 writer 정책 설치는 O 쪽에 남아야 한다 |
| F15 | candidate가 없으면 child 읽기도 없다(Q14) | `tui_o/channel_policy/children.rs:79-86`(`let candidate = self.candidate.clone()?`) | 확인 | 그 결과 `cutover::relation`(`cutover.rs:281-290`)이 NotChild로 읽힌다 |
| F16 | `policy_channel`이 child 읽기에 의존한다(Q14) | `children.rs:476-480` | 확인 | — |
| F17 | O kind는 O 선택 기반이다(Q14) | `tui_o/channel_policy.rs:253-280`(`selected_kinds`) | 확인 | — |
| F18 | 정상 fresh dispatch는 `Adoption::Pending`, late는 `reserved_late`다(Q3) | `children.rs:205-212` | 확인 | — |
| F19 | late 정책 조건(Q3, S2c) | `tui_o/cutover/claude_thread.rs:14-22` | 확인 | S2a는 쓰지 않는다 |
| F20 | `herdr_consent::endpoint`는 자기 boot의 user_default만 본다(E34) | `discord/router/herdr_consent.rs:31-43` | 확인·주의 | 호출부가 12곳이다: `herdr_consent.rs:60,133,216,291`, `herdr_consent/transport.rs:100`, `herdr_consent/return_input.rs:31`, `herdr_consent/admission.rs:42,49`, `intake_dispatch.rs:587,617,896`, `commands/control/clear_report.rs:147`, `recovery_engine/herdr_server_replaced.rs:28`. 함수 자체를 부모 해석으로 바꾸면 return_input(`:31`)·동의 카드·명령 경로까지 자식으로 넓어진다. 설계(E34 "endpoint 선택과 입력 identity 분리", return_input own guard 유지)를 지키려면 실행 서버 선택용 함수를 따로 두고, 호출부를 하나씩 고른다. 카드·명령 경로(`:60,:133,:216,:291`, `transport.rs:100`)를 자식에서 어떻게 할지는 설계에 명시가 없다(미확인 U-A) |
| F21 | return_input은 own mapping guard를 쓴다 | `herdr_consent/return_input.rs:14` `session_hosts::herdr_endpoint(channel)` | 확인 | `:31`은 `endpoint()`를 다시 부른다(F20 주의) |
| F22 | intake checkpoint가 own endpoint로 대기 여부를 정한다(E35) | `discord/router/intake_dispatch.rs:585-591,615-617,895-901` | 확인 | — |
| F23 | clear의 동의 정산이 own endpoint로 문을 연다(E47) | `commands/control/clear_report.rs:136-170`(`:147`) | 확인 | — |
| F24 | server-replaced endpoint가 동의 endpoint(own)다(E74) | `recovery_engine/herdr_server_replaced.rs:27-31`; `demand`·`ensure_server` `:33-44` | 확인 | — |
| F25 | preclaim이 부모 정책을 본 뒤 own `local_endpoint`를 쓴다(E56) | `discord/herdr_terminate/root_replaced/preclaim.rs:23-26`(부모), `:49-55`(`replaced::local_endpoint`) | 확인 | F24를 고치면 preclaim은 파일 변경 없이 따라온다 |
| F26 | 생성 gate는 자기 boot만 보는 bool이다(E17~E19) | `herdr_launch.rs:64-66,75-77`; 호출부 `claude/tui_session_launch.rs:93,108`, `claude.rs:2602`, `codex.rs:1435,1536`, `codex/absence_launch.rs:80,297`, `codex/tui_session_launch/first_input.rs:265`, `codex/tui_session_launch/launch_policy.rs:70`, `discord/codex_absence.rs:31`, `busy_inject.rs:90`, `tui_prompt_dedupe/runtime_binding/codex_policy/absent_open.rs:67` | 확인·주의 | 이 launch 함수들은 동기 함수이고 `TurnHost`가 내려가지 않는다(`claude.rs:598-616` 인자에 없음). 이미 내려가는 판정 통로는 `teardown: Option<&TeardownClearance>`(`provider_teardown.rs:7-18`, `provider_dispatch.rs:68-71`)뿐이다. typed 판정을 쓰려면 이 통로를 확장하거나 인자를 하나 더 내려야 한다. 그러면 giant `claude.rs`(4108)·`codex.rs`(3051)에서 인자 전달 줄이 늘어난다 |
| F27 | dispatch가 provider를 바꿀 수 있다(Q9) | `discord/router/message_handler/intake_turn.rs:788-797` | 확인 | — |
| F28 | Codex absence 분류가 생성 gate bool을 쓴다(E20) | `discord/codex_absence.rs:28-48` | 확인 | — |
| F29 | 부모 Bound는 동의 proof를 생략한다(Q15) | `discord/router/herdr_consent/admission.rs:150-159` | 확인 | S2c(redirect) 범위 |
| F30 | 자식 입력 동의는 `relation` Child면 부모 endpoint + `input_runs_herdr`(E33) | `herdr_consent/admission.rs:36-50` | 확인 | — |
| F31 | 슬롯 후단 gate는 확정 legacy 행만 통과시킨다(E79, S2b) | `auto_queue/runtime.rs:553-595` → `discord/host_teardown_gate.rs:223-251` | 확인 | S2b 범위. 모델 근거로만 썼다 |
| F32 | ordinary fresh는 owner WRITER_HELD로 기다린다(Q3) | `discord/router/message_handler/provider_dispatch/owner_input.rs:192-224` | 확인 | 모델 `OrdinaryReady` 근거 |
| F33 | Bind는 source·nonce 확인 뒤 `bind_pg`(C6) | `discord/tui_prompt_relay/herdr_source.rs:185,267` | 확인 | S2b·S2c 범위. 모델 근거 |
| F34 | 재생 불명은 Unconfirmed로 보관된다(Q12) | `turn_orchestrator/queued_head/v2/retained.rs:77` | 확인 | 모델 `Restart2` 근거 |

**계수: 확인 34(그중 확인·주의 4) / 불일치 0 / 미확인 1(U-A: 동의 카드·명령 경로의 자식 처리).**

`DESIGN_DELTA` 판단: S2a 계약을 바꿔야 하는 불일치는 없다. 주의 4건(F9·F20·F26, 그리고 F2 반환형)은 구현 방식의 조건이다. 계획 질문 Q-P1~Q-P3으로 넘긴다.

## 2. 상태 차트 갱신(§7)

`docs/statecharts/thread-host/ThreadHostS1b.tla` `SpecS2`를 설계 §7.2대로 보강했다. 결과는 report S2 절에 있다.
- 실행 사실:
  - 입구 `LiveTmuxNoSel`·`SelUnknown`·`RetiredNative`를 더했다.
  - 상태 `Unread`(행 읽기 실패)·`Placed`(위치·pane 채택, 미Bind)를 더했다.
- fresh 의도와 허가: `late`(late origin / ordinary fresh adoption)로 나눴다. ordinary는 WRITER_HELD로 기다린 뒤 `OrdinaryReady`(`writer/host/first_init.rs:282`)로 진행한다.
- 미전송 successor: `unsent`를 더했다. `MutS2NoFreshSuccessor` → `FirstInputResolves` RED.
- Resolve 추상화: Adopt(`Resolve` → `Placed`)와 `Bind`를 나눴다. `MutS2SeedOnAdopt` → `SeedMatchesBound` RED.
- current seed와 계획: `judged`를 더했다.
  - `MutS2PlanSeeds`, `MutS2IgnoreVerified` → `SeedMatchesBound` RED.
  - 활성화만 한 경우(`S2ExecNaive`)도 late 거절이 Herdr로 기록된다(`turn_host.rs:171-187`).
- 슬롯: Retired의 다음 호스트가 Herdr여도 해제된다. `MutS2SlotFuturePolicy` → `SlotRotates` RED.
- 재생 불명: `receipt`를 더했고 `S2Replay` cfg를 따로 뒀다(`FirstInputSettles` RED = 외부 확인 없이 성공을 주장하지 않음, `FirstInputClassified` GREEN).
- 기존 cfg 29개는 기대가 그대로다(§7.1). 39/39가 `expected.tsv`와 같다.

## 3. 겹침 대조

### 3.1 S2a 파일 목록(확정 후보)

| 조각 | 파일 | S2a가 할 일 |
|---|---|---|
| S2a-1 | `discord/commands/control/clear_teardown.rs` | `end_session`·`end_herdr`: 저장된 Herdr 행 우선, 3분 결과(처리됨 / Legacy·Missing / 미확인), 미확인에서 tmux 대체 금지 |
| S2a-1 | `discord/commands/control/clear_teardown/resume_fence.rs` | `set_channel`: `.is_herdr()` 대신 저장 행 기준 |
| S2a-1 | `turn_host.rs` | `for_turn`·`recorded_execution`·`resolve`: verified_bound 전달 |
| S2a-1 | `discord/commands/config/effort.rs` | `herdr_codex`: 이미 읽은 `herdr_now` 사용(`:146-158,:186-205,:257-263`) |
| S2a-1 | `discord/commands/tui_passthrough.rs` | `effort_save_notice` `:94-98`: 같은 판정 |
| S2a-2 | `src/db/dispatched_sessions/hosted_execution.rs` | `HostedObservation`·`load_hosted_execution_pg`: selector를 같은 SELECT로 |
| S2a-2 | `session_host/thread_host.rs` | `next_host`·`next_host_cached`·`next_is_herdr`: provider 인자 |
| S2a-2 | `session_host/host_decision.rs` | `decide`·`observe_thread`·`input_runs_herdr`: 공통 관측 사용(Nothing → Tmux는 유지) |
| S2a-2 | 새 `session_host/thread_launch.rs` | 공통 관측(행·selector·tmux) 본체 |
| S2a-2 | `session_host.rs` | `mod thread_launch;` 한 줄(FILES_AUTO 성격) |
| S2a-2 | `turn_host.rs` | `resolve`가 공통 관측을 소비. 답은 그대로 |
| S2a-2 | `tui_o/channel_policy.rs`, `tui_o/channel_policy/children.rs`, `tui_o/cutover.rs` | 관계 조회를 O candidate와 분리(`read`, `policy_channel`, `record_thread_child_by_id`, `record_in`, `relation`) |
| S2a-2 | `discord/router/herdr_consent.rs`, `herdr_consent/admission.rs` | 실행 서버 선택용 endpoint 함수, `check`가 그것을 소비 |
| S2a-2 | `discord/router/intake_dispatch.rs`, `commands/control/clear_report.rs`, `recovery_engine/herdr_server_replaced.rs` | 호출 교체(E35·E47·E74). preclaim(E56)은 파일 변경 없음 |
| S2a-2 | `discord/router/intake_gate/busy_inject.rs`, `discord/turn_presence/activity/unbound/completed_scheduled.rs` | provider를 구별하는 next 판정 호출(E37·E83) |
| S2a-3 | `herdr_launch.rs` | `herdr_configured_for_tui_launch` → provider 명시 typed gate |
| S2a-3 | `claude/tui_session_launch.rs`, `claude.rs`(호출만), `codex.rs`(호출만), `codex/absence_launch.rs`, `codex/tui_session_launch/first_input.rs`, `codex/tui_session_launch/launch_policy.rs` | gate 호출 교체 |
| S2a-3 | `provider_teardown.rs` 또는 `discord/router/message_handler/provider_dispatch.rs` | typed 판정을 sync launch까지 전달하는 통로(F26, 하나만 고른다) |
| S2a-3(검토) | `discord/codex_absence.rs` | E20: 분류 predicate를 생성 gate와 분리(남김/S4. 파일 변경이 없을 수 있음) |

### 3.2 교집합(`s2a-overlap-files.txt`)

| 상대 ref(head, merge-base) | 겹친 파일 | 상대가 바꾼 내용의 성격(`git diff <mb>..<ref> -- <파일>`) | S2a가 그 파일에서 할 일 |
|---|---|---|---|
| `refs/review/i7353-mkrx-w7` (`1ad1db99ce`, mb `4adca870ec`) | `turn_host.rs` (+17/−17) | 가시성만: `pub(crate)` → `pub`(`TurnHost`, `HerdrTurnPlan`, `HerdrRefusal`, `for_turn`, `for_observation`, `refusal_before_turn` …). 로직 0 | S2a-1: `recorded_execution`·`for_turn`·`resolve`(verified_bound). 시그니처 줄이 겹친다 |
| 〃 | `session_host/thread_host.rs` (+9/−9) | 가시성만 | S2a-2: `next_host*`·`next_is_herdr` 시그니처 |
| 〃 | `session_host/host_decision.rs` (+9/−9) | 가시성만 | S2a-2: `decide`·`observe_thread`·`input_runs_herdr` |
| 〃 | `session_host.rs` (+56/−58) | 가시성 + `use` 목록 재배치(2줄) | S2a-2: `mod thread_launch;` 한 줄 |
| 〃 | `src/db/dispatched_sessions/hosted_execution.rs` (+44/−49) | 가시성, rustfmt 재배치(`load_hosted_execution_pg` 시그니처 한 줄화 등), 시험 지원 `#[path = "hosted_execution_support.rs"]` 분리 | S2a-2: `HostedObservation` 필드, `load_hosted_execution_pg` SELECT |
| 〃 | `tui_o/cutover.rs` (+38/−56) | 가시성, 함수 시그니처 한 줄화(rustfmt), `#[cfg(test)]` → `#[cfg(any(test, feature = "test-support"))]` | S2a-2: `record_thread_child_by_id`, `record_in`, `relation`, `policy_channel` |
| 〃 | `tui_o/channel_policy.rs` (+21/−21) | 가시성만 | S2a-2: 관계 accessor·kind 출처 |
| 〃 | `tui_o/channel_policy/children.rs` (+11/−11) | 가시성만 | S2a-2: `ChildEntry::read`, `policy_channel` |
| 〃 | `recovery_engine/herdr_server_replaced.rs` (+3/−2) | 경로: `discord::UnplacedLease` → `discord::session_transition::UnplacedLease` | S2a-2: `local_endpoint` |
| 〃 | `herdr_launch.rs` (+26/−26) | 가시성만 | S2a-3: `herdr_configured_for_tui_launch` |
| 〃 | `claude/tui_session_launch.rs` (+3/−3) | 가시성만 | S2a-3: `:91-108` gate 호출 |
| 〃 | `claude.rs` (+22/−22) | 가시성만 | S2a-3: `:2602` 호출(+ 판정 전달 시 인자) |
| 〃 | `codex.rs` (+28/−28) | 가시성만 | S2a-3: `:1435,:1536` 호출 |
| 〃 | `codex/tui_session_launch/launch_policy.rs` (+10/−10) | 가시성만 | S2a-3: `:70` 호출 |
| 〃 | `discord/codex_absence.rs` (+5/−5) | 가시성만 | S2a-3(검토): `local_tui` |
| 〃 | `provider_teardown.rs` (+5/−2) | 가시성만 | S2a-3(후보): `TeardownClearance` 통로 |
| 〃 | `discord/router/message_handler/provider_dispatch.rs` (+2/−2) | 경로: `UnplacedLease` 경로, `shared.as_ref()` | S2a-3(후보): `StreamingTurn` 판정 전달 |
| `refs/review/i7353-rxb-d-w8` (`259ec832e2`, mb `8bdeaace4c`) | `discord/router/message_handler/provider_dispatch.rs` (+5/−5) | import 경로: `HerdrEndpointId` 등을 `recovery_engine::host_reconcile`에서 `session_host`로 | 위와 같음 |
| `refs/review/i8430-s1-w1`, `refs/review/i8399-s3b2-w4`, `refs/review/i8432-fix-w1` | 없음 | — | — |

겹친 파일은 18개다(mkrx_w7 17, rxbd_w8 1; `provider_dispatch.rs`는 둘 다와 겹친다). S2a-1 첫 커밋 5파일 중 겹침은 `turn_host.rs` 하나(mkrx_w7, 가시성)다. 상대 변경에 로직은 없지만 가시성·시그니처 줄이 S2a 수정 함수와 겹친다. 그래서 둘 중 늦게 착지하는 쪽이 텍스트 충돌을 푼다.

### 3.3 승인 요청 목록(구간 이름)

- S2a-1:
  - `turn_host.rs` — `for_turn`, `recorded_execution`, `resolve`(verified_bound 반환), `is_live_thread_endpoint`(호출만).
- S2a-2:
  - `src/db/dispatched_sessions/hosted_execution.rs` — `HostedObservation`(필드·접근자), `load_hosted_execution_pg`(SELECT·decode).
  - `session_host/thread_host.rs` — `next_host`, `next_host_cached`, `next_is_herdr`.
  - `session_host/host_decision.rs` — `decide`, `observe_thread`, `input_runs_herdr`.
  - `session_host.rs` — `mod thread_launch;`.
  - `tui_o/cutover.rs` — `record_thread_child_by_id`, `record_in`, `relation`, `policy_channel`.
  - `tui_o/channel_policy.rs` — 관계 accessor 설치(`install` 인근), kind 출처.
  - `tui_o/channel_policy/children.rs` — `ChildEntry::read`, `policy_channel`.
  - `recovery_engine/herdr_server_replaced.rs` — `local_endpoint`.
- S2a-3:
  - `herdr_launch.rs` — `herdr_configured_for_tui_launch`(typed gate로 교체).
  - `claude/tui_session_launch.rs` — `prepare_and_create_claude_tui_session` gate 두 곳.
  - `claude.rs` — `execute_streaming_local_tmux` gate 한 곳(+ 판정 인자 전달).
  - `codex.rs` — gate 두 곳.
  - `codex/tui_session_launch/launch_policy.rs` — gate 한 곳.
  - `discord/codex_absence.rs` — `local_tui`(분류 분리, 변경이 없을 수 있음).
  - `provider_teardown.rs`(`TeardownClearance`) 또는 `provider_dispatch.rs`(`StreamingTurn`·`drive` 전달) 중 하나.

## 4. S2a 구현 계획

### 4.1 조각 나눔

설계 §5의 S2a 추정(첫 커밋 포함 22~26파일, +350~600)은 prod 30파일 상한에 가깝다. 그래서 세 조각으로 나누는 안을 낸다. 조각마다 별도 리뷰 ref를 둔다.

| 조각 | prod 파일 추정 | 순증 추정 | 선행 | 겹침 |
|---|---:|---:|---|---|
| **S2a-1** 첫 커밋(clear·verified_bound·effort) | 5 | +70~120 | 없음 | `turn_host.rs`(mkrx_w7 가시성) |
| **S2a-2** 관측·관계·동의 endpoint 정합 | 15~16(새 1 포함) | +250~420 | S2a-1 | 9파일(mkrx_w7) |
| **S2a-3** provider 명시 생성 gate | 8~9 | +100~200 | S2a-2(공통 관측 사용) | 8~9파일(mkrx_w7·rxbd_w8) |

- 누적 고유 prod 파일은 약 27~29개다. `turn_host.rs`가 S2a-1·2에 겹친다. 누적 순증은 약 +420~740이다.
- 실측: 이 레그는 prod 변경이 0이라 `pr_cap_prod.py b04ea56650 HEAD` 결과가 prod 0이다. 조각별 실측은 구현 레인이 `pr_cap_prod.py <base> HEAD`로 한다. +1200 정지선은 어느 조각도 추정상 넘지 않는다.
- giant 파일에는 호출 교체·인자 전달만 둔다: `claude.rs` 4108, `codex.rs` 3051, `provider_dispatch.rs`, `intake_dispatch.rs`, `intake_turn.rs`(S2a는 손대지 않음). 본체는 새 `session_host/thread_launch.rs`에 둔다.

### 4.2 조각별 수용 시험·뮤턴트·RED 방법

| 조각 | 시험(설계 §6) | 수정 전 RED 방법 | 원인 제거 뮤턴트 |
|---|---|---|---|
| S2a-1 | T-S2-10: (1) Pending clear → exact terminate 진입, 추정 tmux 종료 0. (2) Retired + selector clear → 그 selector만 삭제, successor 보존. (3) Hold/Unknown clear → `end_unconfirmed`, 두 호스트 destructive 0. (4) verified_bound = false gate 거절 → JUDGED Herdr 0. (5) 자기 boot 없는 Bound Codex 자식 → capability·저장·restart가 같은 Herdr 판정 | 같은 시험을 base(`b04ea56650`)에 적용해 FAIL을 남긴다. (1)(3)은 Herdr 부모 + 자식 Pending/Unknown 행에서 `end_herdr` 미진입 또는 tmux teardown 진입, (4)는 `recorded_execution`이 `Some(true)`, (5)는 `effort.rs:202` 거절 | `.is_herdr()` gate 복원, verified_bound 무시, own-boot `herdr_codex` 복원 |
| S2a-2 | T-S2-3 대조군(selector 있는 tmux는 tmux, FreshSubstitution 뒤 재판정 0). selector를 같은 snapshot에서 읽고 조회 실패를 None으로 줄이지 않음. 살아 있는 tmux + selector 없음은 Tmux(`LiveTmuxStaysTmux`). T-S2-15 일부(O 미선택 자식도 관계 기록, 조회 실패 ≠ NotChild). 자식 동의 endpoint는 부모 user_default, return_input은 own 유지(C12). E37·E83 provider 구별 | 대조군은 base PASS로 기록한다(설계: 올바른 기존 동작을 FAIL로 꾸미지 않음). 관계·endpoint 시험은 base에서 NotChild·None으로 FAIL | probe 생략 복원(`MutS2ProbeSkipped`와 같은 뜻), O-only recorder 복원, endpoint를 own만 보게 복원, selector 조회 실패를 None으로 |
| S2a-3 | T-S2-18 일부(provider 명시 gate: Codex 기존 동작 회귀 0, Hold에서 tmux create 0). T-S2-4 대조군(tmux 부모 그대로) | 대조군 base PASS. Hold 경로는 S2a에서 turn_host가 스레드에 Hold를 내지 않으면 생산 경로상 도달하지 않는다 → Q-P1 | gate가 own-boot bool만 보게 복원 |

- 실행 호스트: 미니는 cargo 금지다. RED/GREEN·뮤턴트·PG 시험(`pg_throwaway.sh`)은 WSL(S1b가 쓴 Opus 레인)에서 하고, WSL을 못 쓰면 GCP에서 한다. Windows 대상 컴파일은 S2a-3(unix 경계 함수 호출 교체)에서 필요하다.
- 상태 차트: 모델 쪽은 이번 레그로 끝났다. 조각 구현 뒤에는 report의 "코드 대응" 열만 갱신한다.

### 4.3 계획 질문(설계 리뷰·코디)

- **Q-P1.** S2a-3 typed gate는 활성화(S2c) 전에는 관측되는 변화가 없다. turn_host가 스레드에 Hold를 내지 않기 때문이다. 그러면 RED 시험이 없는 배관이 된다. S2c로 옮길지(S2c는 이미 25~29파일), S2a에 회귀 대조군만 두고 남길지 정해야 한다.
- **Q-P2.** `input_runs_herdr`의 probe 되살리기(F9)는 S2a에서 결과가 같고 입력당 probe 비용만 는다. Nothing이 Herdr가 되는 S2c와 같이 넣는 편이 시험 의미가 맞는다. 설계 E11은 S2a다. 어느 쪽인지 정해야 한다.
- **Q-P3.** 동의 endpoint 분리(F20, U-A): 카드·명령 경로(`herdr_consent.rs:60,133,216,291`, `transport.rs:100`)를 자식 입력에서 부모 endpoint로 볼지, own으로 둘지 설계 확인이 필요하다.
- **Q-P4.** 겹침: S2a-1과 mkrx_w7은 `turn_host.rs` 가시성 줄만 겹친다. 착지 순서(mkrx_w7 먼저 / S2a-1 먼저)는 코디가 정한다.
