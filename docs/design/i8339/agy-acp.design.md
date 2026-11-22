# #8339 antigravity 채널을 `agy acp` 클라이언트로 옮기는 설계

저장소 기준은 `5021d25794`(origin/main)이고, 확인한 agy CLI는 1.5.14(`agy --version`)다. 이 문서는 설계만 담는다. Rust 코드는 바꾸지 않는다. 상태 차트는 [agy-acp 모델](../../statecharts/agy-acp/AgyAcpSession.tla)과 [보고서](../../statecharts/agy-acp/agy-acp.report.md)다. 미확인 가정은 [U]로 표시하고 확인 방법을 함께 적는다.

## 0. 결정 요약

| 쟁점 | 결정 |
|---|---|
| 범위 | provider가 antigravity로 정해지는 **모든** Discord 채널을 대상으로 한다. 그 채널의 스레드와 antigravity 봇 DM도 포함한다(2절). 채널 이름이나 ID 목록은 두지 않는다. |
| 경로 판정 | 다음을 모두 만족하면 ACP 경로로 간다. ① 전역 스위치 `agy_acp.enabled` 켜짐 ② 턴 provider = Antigravity ③ 사람이 쓴 입력(Discord 사람 작성 또는 인증된 iMessage) ④ dispatch가 아님. 그 밖의 입력은 지금 경로(`agy --print`)로 간다. 스위치는 전체 하나이고 기본은 꺼짐이다. 조각 2 운영 확인 뒤 켠다. |
| 입구 | 분기점은 `stream_json_turn::execute_request`(`src/services/discord/router/message_handler/provider_dispatch/stream_json_turn.rs:181`) 한 곳이다. ACP 이벤트는 기존 `StreamMessage`와 `AgySpool` 기록으로 바꾼다. 그 아래(turn_bridge, agy writer, 스풀, 외부 입력, shadow)는 그대로 둔다. |
| 프로세스 수명 | **채널 상주**(destination당 `agy acp` 자식 하나). 재부착은 `session/resume`으로 하고 기록을 재전송하지 않는다. `session/load`는 대체 경로로만 쓰며, 이때 응답 전 갱신은 버린다. AgentDesk가 재시작하면 자식도 같이 죽고, 다음 입력이 새 자식을 띄워 resume한다. |
| 실행 중 입력 | **steering이 ACP 경로의 기본 동작이다**(6절). 턴의 prompt가 열려 있는 동안 들어온 사람 입력은 그 턴에 `session/prompt`로 보내 턴에 합친다. 턴의 답 게시 하나가 그 입력까지 덮는다. steering을 끄는 방법은 전역 스위치뿐이다. |
| 취소 | `!stop`은 `session/cancel`을 보내고 `stopReason:"cancelled"`를 기다린다. 15초 안에 오지 않으면 process group을 종료한다. |
| 턴 종료 | 턴은 그 턴에 열린 모든 prompt의 응답(stopReason)이 도착했을 때 끝난다. #8160은 이것만으로 사라지지 않는다(9절). |
| 권한 | ACP 실행에는 `--dangerously-skip-permissions`를 붙이지 않는다. `session/request_permission`에는 정책으로 답한다. 정책은 D1이 정해질 때까지 기본 (c)다(10절). |
| 모델 | 세션의 `configOptions`(`id:"model"`) 안에서만 고른다(일반 외부 클라이언트는 Gemini만). 목록에 없으면 실패를 한 번 게시하고, `-p`로 몰래 되돌아가지 않는다. |
| SDK | 공식 crate는 쓰지 않는다. v1의 필요한 부분만 tokio·serde_json으로 직접 구현한다(13절). |
| 사용 형태 전제(D0) | **사용자 결정(agy-acp-1123)으로 전제를 확정했다.** 사람이 직접 모는 대화형 클라이언트로 `agy acp`를 띄우는 방식을 허용된 형태로 본다. |
| 조각 | 3개, 추정 합계 prod 24파일 / +2,300이다. 조각마다 상한 아래다(16절). |

## 1. 근거

| 근거 | 내용 | 출처 |
|---|---|---|
| agy ACP 문서 "Work with the agent" | Steering: "Sending a prompt while a turn runs steers that turn instead of starting a new one." Cancel: "Cancelling a turn ends open tool calls and abandons pending permission requests." Sessions: "Resuming a session reopens the conversation without replaying its history." 이미 규칙이 정한 행동에는 허가 질문을 하지 않는다. steering에 대해 문서가 말하는 것은 이 한 문장뿐이다. 앞 prompt의 응답 형태, 큐잉, 거절은 적혀 있지 않다. | https://antigravity.google/docs/ide/extensions/acp (2026-11-23 조회) |
| agy 모델 | "Gemini models are available in every client. Third-party models are only offered to JetBrains IDEs …, Zed, and Xcode." | 같은 문서 "Models and thinking level" |
| 권한 규칙 | `~/.gemini/antigravity-cli/settings.json`의 `permissions.allow/deny/ask`에 `action(target)` 형식으로 적는다. 우선순위는 Deny > Ask > Allow이고, 명시 규칙이 실행 모드보다 앞선다. | https://antigravity.google/docs/permissions |
| FAQ | "`agy -p` is intended for direct scripting and CI workflows. Wrapping `agy` inside third-party coding agents or applications … is not supported under consumer plans." | https://antigravity.google/docs/faq |
| 포럼 10-26·11-09·11-12 | 10-26·11-09: 개인 단일 사용자가 대화형으로 쓰는 공식 ACP 클라이언트는 허용되며 Gemini 모델만 쓴다. 조건은 수정하지 않은 `agy acp`가 직접 인증하는 것이다. 11-12: harness 안의 모델 공급자로 쓰거나 `--dangerously-skip-permissions`로 모는 것은 consumer 로그인에서 비준수다. | https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027, https://discuss.ai.google.dev/t/personal-app-using-the-official-agy-acp-binary-with-google-ai-pro-permitted/198889, https://discuss.ai.google.dev/t/policy-question-omp-hosting-unmodified-antigravity-acp-for-one-user/199753 |
| ACP 명세 | `session/load`는 "MUST replay the entire conversation", `session/resume`은 "MUST NOT replay"(2026-04-22 안정화)다. `session/cancel`은 알림이며 에이전트는 진행 중 prompt에 `cancelled`로 답한다. stopReason은 `end_turn`·`max_tokens`·`max_turn_requests`·`refusal`·`cancelled`다. v1은 실행 중 두 번째 prompt를 규정하지 않는다. v2 초안은 "Clients MAY send `session/prompt` in any state … What inserting during active work means … is up to the Agent"라고 적는다. | https://agentclientprotocol.com/protocol/v1/session-setup, https://agentclientprotocol.com/protocol/v1/prompt-turn, https://agentclientprotocol.com/protocol/v2/prompt-lifecycle |
| `agy acp --help`(1.5.14) | `--dangerously-skip-permissions`, `--process-per-session`, `--app-data-dir`(기본값 `antigravity-cli`), `--gemini-dir`, `--log-file`, `--login`, `--sandbox`가 있다. `--settings`와 `--model`은 **없다**. | 로컬 실행(프롬프트 전송 없음) |
| T3 Code | MIT, https://github.com/pingdotgg/t3code @ `c89ca0a2f2c1d996d9b461568860991febaca04f`. steering 처리는 `packages/provider-acp/src/server/adapter.ts:566-571,9149,9789-9885`, 버전 게이트는 `apps/server/src/provider/acp/AntigravityAcpSupport.ts:76-84`에 있다. | 14절 |
| ACP Rust SDK | `agent-client-protocol` 5.5.1(Apache-2.0) @ `34aea54f7314b42563b09112fe40b0cd5eb29b8a`, schema `=1.21.2` 고정 | 13절 |

## 2. 범위: provider=antigravity 채널

### 2.1 대상 정의

대상은 "채널 → provider 결정 경로가 Antigravity를 내는 채널"이다. 실행 시점에는 그 결과가 턴의 provider(`StreamingTurn.provider`)로 이미 정해져 있다. 그래서 ACP 판정은 그 값만 본다. 이름이나 ID를 따로 대조하지 않는다.

| 결정 단계(우선순위) | 좌표 | 내용 |
|---|---|---|
| 1. 에이전트 채널 바인딩 | `src/services/channel_roles/config_projection.rs:51-79`(`find_channel_binding`), `:81-91`(`binding_provider`), `src/services/channel_roles.rs:35` | `agents[].channels`의 provider 키 → 채널 provider → 에이전트 provider |
| 2. 채널 이름 접미사 | `src/services/discord/settings/validation.rs:4-33`(`channel_supports_provider`), `src/config.rs:1783`(`provider_from_channel_suffix`), `src/domain/provider/catalog/rows.rs:161`(`-ag`) | 바인딩이 없을 때 쓴다. `-ag` → antigravity |
| 3. 기본 provider | `src/services/provider/runtime.rs:88-110` | org schema가 없을 때만 쓴다 |
| DM | `validation.rs:10-12` | antigravity 봇 DM이면 provider는 Antigravity다 |
| 스레드 | `src/services/tui_o/cutover.rs:181,210` | 부모가 antigravity인 스레드는 자기 destination과 세션 키를 갖는다. ACP actor도 destination마다 따로 둔다 |

### 2.2 채널별 분기

| 분기 | 코드 | ACP에서 다른 점 |
|---|---|---|
| 일반 대화 채널 | 2.1 그대로 | 이 문서의 기본 흐름 |
| 위임형(비서) 채널 | 턴 안에서 agy가 AgentDesk API로 다른 에이전트에 일을 넘긴다(`src/server/routes/agents.rs:1129` → `src/services/agents/delegation.rs:82-118` `handoff_origin`). 결과 카드는 outbox 봇 게시로 돌아온다(`delegation.rs:243-298`). | ① 위임 호출은 agy의 셸 도구(로컬 API 호출)라서 `session/request_permission` 대상이다. D1 기본 (c)에서는 사람이 관리하는 규칙 파일에 로컬 API 명령 allow 규칙이 있어야 위임이 동작한다. 없으면 거절 안내가 남는다(10절). ② 결과 카드는 봇 게시라 새 agy 입력이 아니다. 지금처럼 ACP 턴을 만들지 않는다. ③ 카드의 [중단]은 작업자 턴을 멈추고, agy 턴과는 무관하다. |
| antigravity 에이전트가 위임 **받는** 쪽 | API handoff로 들어오는 작업 입력 | 시스템 입력이라 ACP 자격이 없다. 지금 경로로 간다. 이 입력은 consumer 로그인으로 도는 자동 agy가 되므로 12절의 후속 F1로 둔다. |

### 2.3 스위치

- 설정 키는 `agy_acp.enabled`(bool, 기본 false) 하나다. 채널별 목록은 두지 않는다.
- 판정은 `execute_request` 첫 줄에서 `config_live_reload::current()`만 읽는다. IO, 잠금, 프로세스 준비는 판정 뒤에만 한다.
- 꺼져 있으면 지금 코드와 동작이 같다(dormant).
- 실행 중에 끄면 이미 ACP로 돈 턴은 ACP에서 끝난다. 그 뒤의 새 입력은 지금 경로로 간다. 자식은 유휴 만료로 정리된다.

## 3. agy 경로 진입점 전수표

`5021d25794` 기준이다. **분기**는 바뀌는 좌표, **보강**은 ACP를 위해 더하는 좌표, **유지**는 그대로 두는 좌표다.

| # | 좌표 | 역할 | ACP 영향 |
|---:|---|---|---|
| 1 | `src/domain/provider/catalog/rows.rs:157-167`, `src/domain/provider/execution.rs:83`, `src/domain/provider/value_methods.rs:106` | 별칭, StreamJsonCli(Agy) 어댑터 | 유지 |
| 2 | 2.1절 표의 결정 경로 | 채널 → provider | 유지(판정 입력) |
| 3 | `src/services/discord/router/message_handler/intake_turn.rs:1971,2080,2122` | `AgyForeground` 생성, blocking 실행 | 유지. 사람 작성 플래그를 인자로 하나 넘긴다 |
| 4 | `…/provider_dispatch/stream_json_turn.rs:51-122` | Discord·iMessage 원본 자격 | 보강(사람 작성 플래그) |
| 5 | `…/provider_dispatch/stream_json_turn.rs:181-194,196-211` | 스풀 예약, `-p` 실행 | **분기**(`acp_selected` → `agy_acp::run_turn`) |
| 6 | `…/provider_dispatch.rs:305-317,531-533` | iMessage 턴 경로 고정, StreamJsonCli 분기 | 유지 |
| 7 | `src/services/stream_json_cli/dialects/agy.rs:18-122`, `runner.rs:59-240`, `codec.rs:115-360` | `-p` argv, spawn, 취소, 코덱 | 유지(지금 경로) |
| 8 | `src/services/stream_json_cli/runner/agy_recording.rs:24-68`, `src/services/tui_o/store/agy_spool.rs:82-195` | 스풀 기록 규약 | 유지. ACP 어댑터도 같은 규약으로 쓴다 |
| 9 | `src/services/tui_o/store/agy_spool/replay.rs:124,365`, `src/services/tui_o/writer/host/agy_host.rs:45-159`, `src/services/tui_o/writer/agy_delivery/mod.rs:87,98`, `src/services/discord/outbound/o_writer_io/agy.rs:20-93`, `src/services/tui_o/channel_policy/agy.rs:40-66` | 스풀 재생, writer, 전달 | 유지 |
| 10 | `src/services/discord/runtime_bootstrap/queued_recovery/agy_shadow.rs:11`, `…/runtime_bootstrap/o_writer_host.rs:549` | 부팅 shadow, writer host | 유지 |
| 11 | `src/services/imsg_reply/agy_input.rs:52-142`, `src/server/routes/agents_turn_deliver.rs:255`, `…/agents_turn_deliver/agy_input.rs:28`, `…/router/intake_dispatch/queued.rs:102,248,407`, `…/headless_turn/replay_start.rs:72-87`, `…/recovery_engine/restore_inflight.rs:139` | iMessage custody, 큐, 복구 보류 | 유지 |
| 12 | `src/services/discord/router/intake_gate.rs:990-1004`(→ `busy_inject::offered`), `:1005-1031`(BusyActiveTurn 큐) | **실행 중 Discord 입력** | 유지(giant, 손대지 않음) |
| 13 | `src/services/discord/router/intake_gate/busy_inject.rs:283-298`(`offered`), `:311-357`(`attempt`), `:373-400`(`settle`: ➕ 반응, 핸드백 시 큐 앞자리와 checkpoint 전진) | busy inject | **보강**. Antigravity ACP 턴이면 `covers` 앞에서 `acp_steer::offer`를 부른다. 본체는 새 child `intake_gate/acp_steer.rs`에 둔다 |
| 14 | `src/services/discord/health/turn_deliver/inject.rs:342-367`(`has_inject_turn`), `:399-410`(`resolve`: Claude/Codex 외에는 `provider_unsupported`) | 주입 대상 판정 | 유지. ACP 분기는 13에서 먼저 처리한다 |
| 15 | `src/services/discord/health/turn_deliver/prepared.rs:75`, `src/services/discord/health/turn_deliver.rs:322-355` | HTTP 입력. agy 선택 채널은 주입하지 않는다 | 유지(iMessage는 큐에 남긴다, 6.5절) |
| 16 | `src/services/discord/commands/control.rs:76-97,544,670,831`, `src/services/turn_lifecycle.rs:98`, `src/server/routes/queue_api.rs:209`, `src/server/routes/kanban.rs:147` | clear·stop·API 취소 | **보강**(clear/reset 때 자식을 닫고 세션을 지운다). 취소는 CancelToken을 통한다 |
| 17 | `src/services/discord/turn_bridge/retry_state.rs:184`, `src/services/discord/inflight/host_recovery_guard.rs:94` | 세션 리셋 | **보강**(ACP 세션 삭제) |
| 18 | `src/services/discord/commands/model_picker.rs:192`, `src/services/discord/model_catalog.rs:592` | 모델 | **보강**(세션 `configOptions`) |
| 19 | `src/services/discord/turn_bridge/stream_loop/content_arms.rs:339-357`, `…/turn_bridge/response_delivery.rs:116-127`, `src/services/discord/inflight/store.rs:497`, `…/turn_bridge/stream_tick/runtime.rs:222` | #8160 | 유지(9절) |
| 20 | `src/db/turns.rs:246-285`(`consumed_origin_ids`) | 한 턴이 여러 원본을 소비한 영수증 | 유지. steering 입력의 원본을 여기 더한다 |
| 21 | `src/services/routines/agent_executor/launch.rs:6-72`, `src/services/provider_exec.rs:102-121`, `src/config/test_instance/agy_cell.rs:42-124`, `scripts/e2e/agy_cell/E-65.yaml` | 루틴, 단순 실행, E2E 셀 | 유지(12절) |
| 22 | `src/services/discord/turn_bridge/tmux_runtime/interrupt_policy.rs:62,132`, `src/services/provider/registry.rs:180`, `src/services/provider_auth_profile.rs:212,308` | 인터럽트 계획, 바이너리, 인증 overlay | 유지. ACP 자식도 같은 overlay를 쓴다 |

## 4. 입구 변환 경계

### 4.1 매핑

ACP 어댑터(`agy_acp::turn`)는 기존 runner와 같은 두 출구로만 내보낸다. 하나는 `Sender<StreamMessage>`(turn_bridge), 다른 하나는 예약된 `AgySpool`(writer)이다. steering된 턴에서도 출구는 이 둘뿐이다. 이어지는 글은 같은 턴의 출구로 계속 흐른다.

| ACP 입력 | 내부 이벤트 | 스풀 | 비고 |
|---|---|---|---|
| attach 결과 sessionId | `Init`(4.3절 값) | `init(uuid)` | UUID가 아니면 실패로 닫는다 [U1] |
| `agent_message_chunk` | `Text` | `delta` | 열린 prompt가 하나도 없을 때 온 것은 버리고 수만 센다(4.2절) |
| `agent_thought_chunk` | `Thinking{summary: None}` | 없음 | 원문은 기록하지 않는다 |
| `tool_call` / `tool_call_update` | `ToolUse` / `ToolResult` | 없음 | 크기를 제한한다(T3 `AntigravityProtocol.ts:379-409`) |
| `plan`·`available_commands_update`·`current_mode_update`·`config_option_update`·`session_info_update` | 없음(진단) | 없음 | 모델 목록 캐시만 갱신한다 |
| `usage_update` | `StatusUpdate`(알려진 필드) | 없음 | [U2] |
| `session/request_permission` | 없음 | 없음 | 정책으로 답한다(10절) |
| `fs/*`·`terminal/*` 요청 | 없음 | 없음 | capability를 false로 알리고, 요청이 오면 -32601로 답한다 |
| 턴의 **마지막** 열린 prompt 응답 `end_turn` | `Done{누적 글}` | `done` | 누적 글이 비면 실패다(`codec.rs:429`와 같은 규칙) |
| steering된 앞 prompt의 `end_turn` | 없음(턴 계속) | 없음 | 6.2절 |
| `cancelled` | `Error{"cancelled"}` | `fail(Cancelled)` | `runner.rs:80-82`와 같다 |
| `max_tokens`·`max_turn_requests` | 글이 있으면 `Done`과 잘림 안내, 없으면 `Error` | `done`/`fail` | [U3] |
| `refusal` | `Error` | `fail` | T3 `AntigravityTurnOutcome.ts:143-158`(쿼터) |
| JSON-RPC 오류(-32000 등) | `Error` | `fail(Spawn)` | 인증 안내만 게시하고 로그인 URL은 중계하지 않는다 |
| EOF, 기한 초과 | `Error`(취소 중이면 cancelled) | `fail` | 턴의 모든 입력에 대해 |

업스트림 오류 문장이 `end_turn`으로 오는 경우가 있다. T3는 대화 DB step type 17로 이를 가린다(`AntigravityTurnOutcome.ts:88-245`). 이 판정은 조각 3에서 붙인다 [U11]. 그 전에는 지금 `-p` 경로와 똑같이 그 문장이 게시된다.

### 4.2 턴 밖 갱신

백그라운드 명령이나 `schedule`이 prompt 응답 뒤에 스스로 출력을 흘리는 경우가 있다(T3 `AntigravityProtocol.ts:49-66`). 열린 prompt가 없을 때 온 출력은 소유할 입력이 없다. 그래서 다음 턴에 섞지 않고 버리며, 수와 로그만 남긴다. 지금 `-p` 경로에서도 이런 출력은 없다. 게시는 후속 후보로 둔다.

### 4.3 세션 ID와 스풀 예약

- ACP 세션 ID는 공용 provider 세션 칸에 넣지 않는다. 대신 `<runtime_root>/agy_acp/<destination>/session.json`에 둔다. 이 파일은 소유자 전용이고, 원자적으로 교체하며, symlink를 거부한다.
- turn_bridge에는 요청이 가져온 세션 값을 그대로 돌려준다. 공용 칸이 그대로인지는 [U4]로 둔다.
- 스풀 예약은 attach 뒤, 그리고 턴 첫 `session/prompt`보다 앞에 한다. `resume` 칸에는 실제 세션 ID를 적는다.
- #8274 2절의 "예약 전 spawn 금지"는 "예약 전 prompt 금지"로 읽는다. 이 해석은 [설계 이탈]이며 리뷰 대상이다. steering 입력은 스풀 형식을 바꾸지 않는다. 대신 턴의 inflight `input_origin_ids`와 turns `consumed_origin_ids`에 원본을 더한다(6.2절).

### 4.4 바뀌는 파일과 그대로 두는 파일

- **새 파일:** `src/services/agy_acp/{mod,rpc,wire,child,turn,store,attach,steer,permission,outcome}.rs`, `src/services/discord/router/intake_gate/acp_steer.rs`
- **바뀌는 파일:** `stream_json_turn.rs`(분기, 플래그), `intake_turn.rs`(인자 하나), `src/config` 한 곳(`agy_acp.enabled`, 기한), `src/services/mod.rs`(mod 한 줄), `intake_gate/busy_inject.rs`(ACP 호출 한 곳), `control.rs`, `retry_state.rs`, `host_recovery_guard.rs`, `model_picker.rs`, `model_catalog.rs`
- **그대로:** 3절의 유지 행 전부. `intake_gate.rs`(1,414줄)는 손대지 않는다.

## 5. 프로세스 수명

### 5.1 비교와 선택

| | 턴마다 `agy acp` | destination 상주 |
|---|---|---|
| 흐름 | 입력 → spawn → initialize → resume → prompt → stopReason → 종료 | 첫 입력 → spawn → initialize → resume/new → prompt…(턴 사이 유지) → 죽음·clear·유휴 만료·재시작 때만 다시 띄움 |
| 비용 | 턴마다 기동과 resume 왕복이 든다(T3 메모로는 기동마다 약 1GB 압축 해제, [U5]) | destination마다 상주 메모리가 든다. 유휴 만료 30분으로 줄인다 |
| steering | 턴 안에서만 가능하다. 턴 경계와 프로세스 경계가 같아 늦게 온 입력이 종료와 경합한다 | 세션이 살아 있어 자연스럽다 |
| 위험 | 반환 경계는 단순하다. 사용 형태가 `-p` 자동 루프처럼 보인다 | incarnation 간 늦은 응답, clear 뒤 옛 세션, 취소 무응답, steering 경합을 막아야 한다 |
| 모델 | PerTurn GREEN | Design·DesignLoad GREEN. 변이 8개는 각각 예상한 규칙만 깬다 |

**선택: 상주.** 허용된 사용 형태(D0)인 "사람이 모는 대화형 클라이언트"에 맞는다. 기본 동작인 steering과 세션을 유지하는 cancel도 상주 세션에서 의미가 있다.

### 5.2 상주 규칙

1. destination마다 actor 하나가 자식 하나와 세션 하나를 소유한다. 자식은 AgentDesk의 process group 안에 둔다. `--process-per-session`은 쓰지 않는다.
2. spawn 명령은 `agy acp --app-data-dir <D> [--gemini-dir <P>]`이다. cwd는 채널 작업 폴더, env는 기존 auth overlay에 `AGY_CLI_DISABLE_AUTO_UPDATE=1`을 더한 것이다. 사설 `TMPDIR`을 쓴다. `--dangerously-skip-permissions`는 넣지 않는다.
3. initialize에는 `protocolVersion: 1`, `clientInfo{name:"agentdesk"}`를 보낸다. 다른 클라이언트로 위장하지 않는다(T3 구버전의 `zed` 위장 금지). fs·terminal capability는 false로 둔다. 응답의 capability와 `agentInfo.version`은 기록한다.
4. authenticate는 보내지 않는다. 캐시된 자격을 쓴다. -32000이면 인증이 필요하다는 안내를 게시하고, 로그인 URL은 중계하지 않는다.
5. attach는 다음 순서다. 저장 세션이 없으면 `session/new`다. 있으면 resume capability가 있을 때 `session/resume`, `loadSession`만 있을 때 `session/load`를 쓰되 응답 전 갱신은 버린다. 거절되면 `session/new`로 열고, 그 턴의 답 앞에 맥락 손실 안내 한 줄을 붙인다.
6. prompt는 스풀 예약과 inflight 저장(write-ahead)을 끝낸 뒤에만 쓴다.
7. 대기 맵 키는 `(incarnation, request id)`다. 리더 task는 자기 incarnation이 아닌 줄을 버린다. steering된 앞 요청은 자기 응답이 올 때까지 맵에 남긴다.
8. 무활동 15분 또는 취소 15초가 지나면 group을 종료하고 열린 턴을 실패 또는 취소로 닫는다.
9. 유휴 30분이 지나면 stdin을 닫고, 그 뒤 SIGTERM, SIGKILL 순으로 끝낸다. 저장 세션은 남긴다.
10. 부팅 때 자식을 미리 띄우지 않는다. 내구 기록이 있고 종료 기록이 없는 턴은, steering 입력까지 포함해 실패 안내로 닫는다. 다시 보내지 않는다. 종료 기록이 있는데 게시가 안 된 턴은 writer가 게시한다.
11. incarnation 번호는 actor 메모리에만 둔다. 재시작하면 옛 리더 task가 없으므로 내구 기록이 필요 없다.

### 5.3 시스템 프롬프트와 저장소 분리

- 시스템 프롬프트: `_meta.antigravity.systemPrompt.append`를 지원한다고 알려 오면 new/resume 때 한 번 보낸다(T3 `AntigravityAcpSupport.ts:86-104`). 그렇지 않으면 세션의 첫 prompt와, 해시가 바뀐 뒤 첫 prompt에만 기존 봉투(`dialects/agy.rs:124-127`)를 붙인다. steering prompt에는 붙이지 않는다 [U7].
- app-data-dir: 기본값은 분리(`antigravity-acp`)다. 전환 뒤 첫 턴은 새 세션이며, 맥락 손실 안내를 붙인다. 공유했을 때 기존 `-p` 대화를 resume할 수 있는지는 조각 2 운영 확인으로 정한다 [U6].

## 6. 실행 중 입력과 ACP steering

### 6.1 (a) 지금 agy의 busy 입력 처리

1. Discord 메시지가 실행 중인 agy 채널에 들어온다. `intake_gate.rs:990-1004`에서 `busy_inject::offered`가 먼저 이 메시지를 받는다(`busy_inject.rs:283`).
2. `owner_takes`·`covers`(env `ADK_BUSY_INJECT_DISCORD_CHANNELS` 게이트, `busy_inject.rs:22-92`)를 지나면 `attempt`로 간다. `has_inject_turn`(`inject.rs:342-352`)은 mailbox에 cancel token이 있으면 provider와 상관없이 참이다. 그러나 `resolve`(`inject.rs:399-410`)가 Claude/Codex 외에는 `provider_unsupported`로 `NotSent`를 낸다.
3. `settle`(`busy_inject.rs:373-387`)은 `NotSent`를 받으면 false를 돌려준다. 메시지는 `intake_gate.rs:1005-1031`의 BusyActiveTurn 큐로 들어가고, 앞 턴이 끝난 뒤 새 턴(새 `agy -p` 프로세스)으로 실행된다.
4. HTTP iMessage는 `turn_deliver/prepared.rs:75`에서 agy writer 선택 채널이면 주입하지 않는다. 큐 custody로 간다(`intake_dispatch/queued.rs:102`).
5. 정리하면 지금 agy에는 busy inject가 없다. 실행 중 입력은 항상 다음 턴이 된다.

### 6.2 (b) ACP에서의 순서, 응답, stopReason

T3 기준 의미는 다음과 같다(`adapter.ts:566-571`). agy는 실행 중 두 번째 `session/prompt`를 받으면 앞 prompt에 `end_turn`으로 답하고, 같은 턴을 새 prompt 아래에서 이어 간다. 도구와 하위 에이전트는 그대로 둔다. agy 1.5.0 이상이 이렇게 동작하고, 1.4는 큐잉한다(`AntigravityAcpSupport.ts:76-84`). 공식 문서에는 앞 응답의 형태가 적혀 있지 않다 [U13]. 그래서 설계는 두 동작(steer, 큐잉) 모두에서 같은 결과가 나도록 만든다.

1. **받기:** 사람이 쓴 Discord 입력 B가 `busy_inject::offered`에 들어온다. provider가 Antigravity이고 스위치가 켜져 있으며 그 destination에 ACP 턴 T가 있으면, `covers` 게이트보다 먼저 `acp_steer::offer(B)`를 부른다.
2. **판정(actor 안, 원자적):** T에 열린 prompt가 하나라도 있으면 steer를 받는다. 열린 prompt가 없으면(T가 마지막 응답을 받아 닫히는 중이면) `NotSent("turn_closing")`를 낸다. B가 이미 T에서 steer를 거절당한 입력이면 `NotSent`다.
3. **내구 기록:** 기존 busy inject처럼 원본을 claim한다(`inject_disposition::claim_source`). T의 inflight `input_origin_ids`와 turns `consumed_origin_ids`에 B를 더하고, Discord 메시지 checkpoint를 B 너머로 옮긴다(`busy_inject.rs:393-400`과 같은 의미). 그래서 catch-up이 B를 다시 실행하지 않는다.
4. **전송:** 같은 세션에 `session/prompt{prompt:[B 본문, 첨부]}`를 새 request id로 보낸다. 시스템 봉투는 붙이지 않는다. B 메시지에 ➕ 반응을 단다(`INJECTED_REACTION`과 같다).
5. **응답:**
   - T의 앞 prompt A가 `end_turn`으로 응답해도 B의 prompt가 열려 있으면 T는 계속된다. Done을 내지 않는다.
   - B 이후의 `agent_message_chunk`는 T의 같은 누적 글과 스풀로 이어진다.
   - T의 **마지막** 열린 prompt가 응답하면 그 stopReason으로 T를 닫는다. `end_turn`이면 `Done{A부터 이어진 전체 글}`이다.
   - 큐잉하는 agy(1.4)는 A를 정상 종료한 뒤 B를 새 agy 턴으로 실행하는데, 그 경우도 같은 규칙으로 하나의 T에 담긴다.
6. **취소:** `!stop`이 오면 열린 prompt 전부에 대해 `cancelled`를 기다린다. T는 취소 안내 하나로 닫히고, 그 안내가 A와 B를 함께 덮는다.

### 6.3 (c) 답 게시 정확히 1

- 게시 단위는 턴이다. T의 terminal 게시(답, 실패 안내, 취소 안내 중 하나) 하나가 T의 모든 구성원(A와 steering된 B들)을 덮는다. B만의 답 게시는 따로 없다. B에는 ➕ 반응이 접수 표시로 달리고, 영수증은 `consumed_origin_ids`다.
- 상태 차트 규칙:
  - `AtMostOnePost`: 입력마다 덮는 게시가 1개 이하
  - `EveryInputSettles`: 받은 입력은 결국 게시 하나에 덮인다. steering 입력도 포함한다
  - `AnswerCoversWholeSteer`: 턴 게시는 구성원의 prompt가 모두 응답한 뒤에만 한다
  - `NoAnswerForRefusedSteer`: 거절된 steer는 그 턴의 답으로 덮이지 않는다
  - `NoDuplicateExecution`: 받아들여진 prompt 쓰기는 입력마다 1회
- 이 규칙들을 Design에서 확인했다. 도달성 증인 두 가지도 Design에서 실제로 일어남을 확인했다. 하나는 "steering된 입력이 턴의 답으로 덮임", 다른 하나는 "거절된 steer가 다음 턴에서 답을 받음"이다(17절).

### 6.4 (d) 거절, 무시, 실패의 되돌림

| 경우 | 처리 | 모델 |
|---|---|---|
| 턴이 닫히는 중(열린 prompt 없음) | `NotSent` → 지금 BusyActiveTurn 큐로 가서 다음 턴이 된다. 닫히는 턴에 합치지 않는다 | 변이 SteerAfterClose(합치면 `AnswerCoversWholeSteer` 위반) |
| B의 `session/prompt`가 JSON-RPC 오류로 거절 | B를 T의 구성원과 `consumed_origin_ids`에서 빼고 ➕를 거둔다. 기존 `HandedBack` 경로(큐 앞자리, 대기 반응, checkpoint, `busy_inject.rs:393-400`)로 되돌린다. 같은 T에 다시 steer하지 않는다 | `SteerRefused`, 변이 SteerRefuseDrop |
| agy가 앞 응답만 주고 B를 큐잉(1.4) | 6.2-5와 같다. 결과가 같다 | AgyResponds가 가장 오래된 prompt부터 답함 |
| 앞 응답 `end_turn`을 턴 끝으로 잘못 처리 | 금지한다. 턴 끝은 마지막 열린 prompt의 응답이다 | 변이 SteerEarlyClose |
| B는 받았지만 답이 B를 반영하지 않음 | 프로토콜로는 감지할 수 없다. 게시 하나에 덮였으므로 유실은 아니다. 사람이 다시 말한다 [U13] | — |
| 취소, 기한 초과, EOF, 재시작 | 턴 하나의 취소 또는 실패 안내가 A와 B를 함께 덮는다. 다시 보내지 않는다 | Design(Crash·Boot, WatchdogKill, AgyDies) |
| 전송 전 actor가 죽음(자식 없음) | `NotSent` → 큐 | — |

### 6.5 범위 밖

- HTTP iMessage의 실행 중 입력은 큐에 남긴다. #8274 1b의 원본별 custody 계약(`prepared.rs:75`, `queued.rs:248-287`)을 바꾸지 않기 위해서다. iMessage steering은 후속 F2로 둔다.
- 봇이 쓴 입력, dispatch, 명령(`!stop` 등)은 `offerable`(`busy_inject.rs:236-244`)이 이미 걸러 내므로 steering하지 않는다.

## 7. 인접 경로와 ACP 대응

| 경로 | 좌표 | ACP |
|---|---|---|
| `!stop`·API stop/cancel | `control.rs:670`, `turn_lifecycle.rs:98`, `queue_api.rs:209`, `kanban.rs:147` → CancelToken | 열린 prompt가 있으면 `session/cancel`을 보내고 `cancelled`를 기다린다. 15초를 넘기면 group을 종료한다. prompt를 쓰기 전이면 쓰기를 취소하고 입력을 큐로 돌린다. 대기 중인 permission 요청에는 `cancelled`로 답한다. 자식은 남긴다 |
| Terminate(hard stop) | CancelToken | 기한 없이 곧바로 group을 종료하고 취소 안내를 낸다 |
| `/clear`·`!clear` | `control.rs:544,831`(큐 폐기, 턴 중단, provider 세션 삭제, agy는 Noop `:90`) | 같은 순서에 다음을 더한다. 턴을 stop하고, 자식을 닫는다(`session/close`가 있으면 그것을 쓰고, 없으면 group 종료). `session.json`을 지운다. 다음 입력은 `session/new`다 |
| 자동 재시도 세션 리셋, 호스트 복구 정리 | `retry_state.rs:184`, `host_recovery_guard.rs:94` | `session.json`을 지우고 자식을 detach한다 |
| 모델 변경 | `model_picker.rs:192` | 다음 턴 첫 prompt 전, 유휴 세션에 `session/set_config_option{configId:"model"}`을 보낸다. 거절되면 그 입력은 실패로 한 번 게시한다 |
| 효과 수준 | — | `thought_level` 옵션이 있을 때만 쓴다(조각 3) |
| 재시작·부팅 | `restore_inflight.rs:139`, agy shadow | 5.2-10 |
| 자식 자체 종료 | — | EOF가 열린 턴을 실패로 닫는다. 다음 입력이 respawn과 resume을 한다 |

## 8. 입력 출처 분류

| 출처 | 작성자 | 소유자 | ACP 자격 | ACP 전송 |
|---|---|---|---|---|
| Discord 채널·스레드·DM 메시지 | 사람(`author_is_bot=false`, `intake_gate.rs:906`) | intake 메일박스 → `intake_turn` | 있음 | 턴이 없으면 새 턴 `session/prompt`, 있으면 steering(6절) |
| Discord 봇 메시지(E2E Test-Announce, 다른 에이전트, 위임 결과 카드) | 봇 | intake | 없음 | 지금 경로(카드는 입력이 아니다) |
| dispatch·파이프라인 | 시스템 | dispatch | 없음 | 지금 경로 |
| 위임 handoff로 들어오는 작업 | 시스템(다른 에이전트) | API handoff | 없음 | 지금 경로(F1) |
| 인증된 iMessage(`/turn/deliver`) | 사람 | `imsg_reply::agy_input` | 있음 | 턴이 없으면 `session/prompt`, 실행 중이면 큐(6.5절) |
| 루틴, `provider_exec` | 시스템 | 각 호출자 | 없음 | 지금 경로(12절) |
| 복구·catch-up 재전달 | 원 입력의 작성자 | recovery engine | 원 입력을 따름 | 내구 기록이 있는 입력은 실패 안내로 닫고 다시 보내지 않는다 |
| 큐 재구성 입력(`queued.rs:496,583`이 `author_is_bot:false`로 채움) | 미상 | intake | 내구 사람 증거가 있을 때만 [U8] | 증거가 없으면 지금 경로 |

## 9. 턴 종료와 #8160

**stopReason만으로는 #8160이 사라지지 않는다.**

- #8160은 turn_bridge의 authoritative Done 처리에서 생긴다(`content_arms.rs:339-357`, 조건 `response_delivery.rs:116-127`). 이 처리가 rollover로 얼린 메시지가 있는데도 오프셋을 0으로 되돌린다. 그러면 단조 저장 거부(`inflight/store.rs:497`)와 권한 상실 정지(`stream_tick/runtime.rs:222`)로 이어진다.
- ACP 어댑터도 `Text` 델타와 `Done{누적 글}`을 내므로 같은 조건을 만든다. steering된 턴은 글이 더 길어지므로 조건을 더 자주 만족한다.
- ACP가 없애는 부분은 프로세스 종료를 기다리던 턴 끝 판정뿐이다. 이제 마지막 응답 하나가 끝을 확정한다.
- 남는 수리는 11-14 결정대로 #8274 writer가 흡수한다. ACP 조각은 같은 긴 답 fixture를 넣었을 때 `-p` 코덱과 ACP 어댑터가 turn_bridge에 같은 메시지열을 준다는 것만 시험한다(넓히지 않음).

## 10. 권한과 #8299

- 포럼 11-12에 따라 ACP 경로에는 `--dangerously-skip-permissions`를 쓰지 않는다. `agy acp`에는 `--settings`가 없다. 그래서 규칙은 사람이 관리하는 프로필 `settings.json`뿐이고, AgentDesk는 그 파일을 쓰지 않는다.
- 규칙 밖 요청(`session/request_permission`)에 대한 응답 정책은 D1이다. D1은 코디네이터가 사용자 결정 포럼에 올렸다.
  - (a) `allow_once` 자동
  - (b) Discord 승인 버튼과 기한 뒤 `reject_once`(권장)
  - (c) 규칙 밖은 `reject_once`로 답하고 본문 뒤에 거절 안내를 붙임
- 결정 전 기본은 **(c)**다. 위임형 채널의 로컬 API 호출(2.2절)은 (c)에서 규칙 파일에 allow가 있어야 동작한다.
- `interaction_*` 질문(T3 `AntigravityProtocol.ts:68-73`)은 조각 1·2에서 `cancelled`로 답하고 안내를 남긴다. 조각 3에서는 (b)와 같은 경로로 Discord에 보인다.
- #8299의 `-p` 변경은 지금 경로(남은 시스템 입력)에 그대로 적용한다. ACP 채널은 #8299를 대체하지 않고 비켜 간다.

## 11. 모델

- 일반 외부 클라이언트에는 Gemini 모델만 열린다. `clientInfo` 위장으로 다른 모델을 얻지 않는다.
- 채널 모델 설정은 "다음 턴 첫 prompt 전 `set_config_option`"으로 동작한다. 목록에 없는 값이면 사용 가능한 목록과 함께 실패를 한 번 게시한다. `-p`로 되돌아가지 않는다.
- `/model` 목록은 ACP 채널이면 actor가 마지막으로 본 `configOptions`를 쓴다. 세션이 없으면 "첫 대화 뒤 갱신"이라고 표시한다. `model_catalog.rs:592` 문구는 스위치가 꺼졌을 때만 쓴다.

## 12. 자동 용도 현황(그대로 둠)

- 파이프라인 리뷰는 더 이상 agy를 쓰지 않는다. 프로필 질문 루틴은 Gemini API 키 모드로 옮겼다. consumer 로그인으로 도는 자동 agy는 없다(코디 델타 11-23).
- 코드 경로(루틴 `agent_executor/launch.rs`, `provider_exec.rs:102`, dispatch, 봇 입력, 위임 작업 입력)는 바꾸지 않는다. ACP 자격이 없으므로 지금 경로를 지나간다.
- 남은 시스템 입력이 antigravity 채널로 들어오면 그 채널의 인증으로 `-p`가 돈다(위임 handoff, dispatch, 봇 입력). 이것을 consumer 로그인에서 막을지는 후속 **F1**로 둔다. F1 후보는 두 가지다. 하나는 antigravity 대상 시스템 입력을 거절하고 안내하는 것, 다른 하나는 API 키 프로필로 돌리는 것이다.
- #8297의 헤드리스 셀(O3~O5)은 보류 중이다. agy E2E는 ACP 경로를 기준으로 다시 정한다. ① 시험 instance에서 가짜 ACP 에이전트(스크립트 stdio)로 짧은 답, 긴 답, steering, `!stop`, 재시작 뒤 이어 말하기를 시험한다. ② 실제 agy 확인은 사람이 시험 채널에서 한다. 봇 입력으로 실제 agy를 모는 셀은 만들지 않는다.

## 13. 공식 SDK와 직접 구현

| | crate(`agent-client-protocol` 5.5.1) | schema crate만(`=1.21.2`) | 직접 구현(선택) |
|---|---|---|---|
| 의존 | 61~75 crate. `process` 기능은 두 번째 reactor를 쓴다 | 32 crate(proc-macro 위주) | 없음 |
| 맞음 | 콜백이 dispatch를 막는다(`jsonrpc.rs:1342-1346`). cwd를 지정할 수 없다(`acp_agent.rs:80-86`) | 타입이 정확하다 | 필요한 v1 부분만 |
| 변동 | 6주에 메이저 2회. schema가 정확히 고정되어 있다 | schema minor를 따라가야 한다 | 모르는 변형은 `Value`로 받아 관용 |
| 크기 | — | 전송부 +400~600 | 약 +900 |

직접 구현 범위는 다음과 같다.

- 줄 단위 JSON-RPC 2.0(16 MiB 상한, JSON이 아닌 줄은 버림)
- `(incarnation, id)` 대기 맵
- 보내는 메서드: `initialize`, `session/new`, `session/resume`, `session/load`, `session/prompt`(steering 포함), `session/cancel`, `session/set_config_option`, `session/close`
- 받는 메시지: `session/update`, `session/request_permission`
- `fs/*`·`terminal/*`은 거절

v2는 범위 밖이다 [U9].

## 14. T3 Code에서 가져올 것

T3 Code(MIT, `c89ca0a2f2c1d996d9b461568860991febaca04f`)는 읽기만 했다. 동작을 Rust로 옮길 때 옮긴 파일 머리에 MIT 고지를 유지한다. Zed는 참고하지 않았다.

| agy 특유 동작 | T3 위치 | Rust 이식 |
|---|---|---|
| 스레드당 자식 하나, 재사용 | `adapter.ts:7771-7795` | 그대로(destination당) |
| 인증 URL이 JSON이 아닌 stdout 줄로 나옴 | `antigravityAuthSupport.ts:646-710` | 버린다. 중계하지 않는다 |
| eager `authenticate` | `AntigravityAcpSupport.ts:125` | 보내지 않는다 |
| resume 우선, load replay 차단 | `AcpSessionRuntime.ts:2446-2470,1838-1863` | resume 우선. load는 대체 경로로만 쓰고 응답 전 갱신을 모두 버린다 |
| resume 실패 시 조용히 new | `adapter.ts:8027-8048` | new로 열되 맥락 손실 안내를 붙인다 |
| 실행 중 prompt = steer, 앞 prompt는 `end_turn`, 한 턴이 여러 prompt에 걸침 | `adapter.ts:566-571,9149,9789-9885`, `AntigravityAcpSupport.ts:76-84` | 의미는 그대로다(턴 하나, 마지막 응답이 끝). 닫히는 턴에는 steer하지 않는다. T3는 "settle된 prompt 너머로도" steer하지만 AgentDesk는 큐로 보낸다 |
| cancel 15초 뒤 종료 | `AcpSessionRuntime.ts:2608-2660` | 그대로 |
| 업스트림 오류가 `end_turn`으로 옴 | `AntigravityTurnOutcome.ts:88-245` | 조각 3, 읽기 전용 |
| `refusal`이 쿼터를 뜻함 | `AntigravityTurnOutcome.ts:143-158` | 그대로 |
| prompt 끝이 턴 끝과 다름(백그라운드) | `AntigravityProtocol.ts:49-66` | 턴은 stopReason으로 닫는다. 턴 밖 출력은 버리고 센다 |
| 권한과 `interaction_*` | `AntigravityAcpSupport.ts:156-220`, `AntigravityProtocol.ts:68-73` | D1 |
| fs read true, write false | `AntigravityAcpSupport.ts:131-139` | 둘 다 false |
| 모델은 `configOptions`와 `set_config_option` | `AntigravityAcpSupport.ts:238-327` | 그대로 |
| 편집 위치, 일회용 세션 정리 | `AntigravityEditLocation.ts`, `AntigravitySessionFiles.ts:117-168` | 버림 |
| 15분 무활동 감시 | `AntigravityAdapterV2.ts:300-301` | 그대로. steering 때 시계를 다시 맞춘다(`adapter.ts:9836-9846`) |
| 환경 정리 | `antigravityAuthSupport.ts:202-245` | 자동 업데이트 끔과 TMPDIR은 그대로. 가짜 `SSH_CONNECTION`은 쓰지 않는다 |

## 15. 기존 이슈 방향

- **#8274 계속.** ACP 어댑터는 스풀 생산자를 하나 더할 뿐이다. 3절 9~11행은 바뀌지 않는다. 예약 시점 해석(4.3절)만 리뷰 대상이다. steering 입력은 스풀 형식을 바꾸지 않고 턴 영수증의 원본 목록에 들어간다.
- **#8297 재정의.** 헤드리스 셀(O3~O5)은 보류를 유지한다. agy E2E는 가짜 ACP 에이전트 셀과 사람 확인으로 바꾼다(12절).
- **#8160 흡수 유지.** #8274 writer로 흡수한다(9절).
- **#8299 지금 경로에만.** ACP 채널은 D1 정책을 따른다.

이슈 댓글 초안은 레인 보고서에 있다.

## 16. 조각 계획

| 조각 | 범위 | 소유 파일(prod) | 추정 | 수용 시험 | 운영 확인 |
|---|---|---|---|---|---|
| 1. 판정과 ACP 핵심(dormant) | `agy_acp.enabled`(기본 꺼짐), `acp_selected`, JSON-RPC 전송, wire 타입, actor(spawn, initialize, new, prompt, cancel, EOF, 기한, incarnation fence), 4.1절 매핑, 스풀 기록, 사람 작성 플래그 | `agy_acp/{mod,rpc,wire,child,turn}.rs`, `stream_json_turn.rs`, `intake_turn.rs`(인자), `src/config` 한 곳, `services/mod.rs` | 9파일 / +950 | ① **자동 경로 불변:** 스위치가 꺼져 있으면 어떤 agy 턴도 ACP 준비를 부르지 않는다. 켜져 있어도 루틴, `provider_exec`, dispatch, 봇 작성(E2E Test-Announce), 원본 claim이 없는 HTTP 헤드리스, 위임 작업 입력은 `execute_streaming`/`execute_recorded`로 간다. 이것을 생산 진입점 `execute_request`에서 단언한다. ② 가짜 에이전트 fixture로 짧은 답, 긴 답, 빈 답, cancelled, EOF, 기한, 옛 incarnation 응답을 넣어 메시지열과 스풀을 확인한다. ③ 긴 답에서 `-p`와 ACP가 같은 메시지열을 낸다. ④ 공용 세션 칸이 그대로다. | 없음 |
| 2. 세션 수명과 steering | `session.json`, resume/load/거절, 부팅 orphan 실패, clear·리셋·호스트 복구, 모델, 유휴 만료, **busy 입력 → steering**(`intake_gate/acp_steer.rs`, `busy_inject.rs` 호출, 원본 기록, ➕, 핸드백) | `agy_acp/{store,attach,steer}.rs`, `intake_gate/acp_steer.rs`, `busy_inject.rs`, `control.rs`, `retry_state.rs`, `host_recovery_guard.rs`, `model_picker.rs`, `model_catalog.rs` | 10파일 / +1,000 | 상태 차트 변이와 1:1로 맞춘다. LoadLeak, RerunSent, ClearKeeps, SteerEarlyClose, SteerAfterClose, SteerRefuseDrop을 각각 RED 재현에서 GREEN으로 만든다. steering 입력은 게시 0개 추가, ➕와 `consumed_origin_ids`, 재시작 뒤 catch-up 재실행 0을 확인한다. 거절 resume은 new와 안내 1회, 목록 밖 모델은 실패 1회다. PG 시험은 `pg_throwaway.sh`로 돌린다. | 시험 채널에서 사람이 짧은 답, 긴 답, **실행 중 입력(steer)**, `!stop`, `/clear`, dcserver 재시작 뒤 이어 말하기를 1회씩 확인한다. [U1][U4][U6][U7][U12][U13]을 확정한 뒤 `agy_acp.enabled`를 켠다. |
| 3. 상호작용과 진단 | permission과 `interaction_*` 중계(D1), 업스트림 오류 판정, `thought_level`, 진단(destination별 자식, incarnation, 턴 밖 출력 수, steer 수와 거절 수) | `agy_acp/{permission,outcome}.rs`, `agy_acp/turn.rs`, 진단 route 한 곳, `model_picker.rs` | 5파일 / +350 | permission 기한 뒤 거절, 취소 때 `cancelled` 응답, 오류 step fixture | 시험 채널에서 권한 승인 1회 |
| 합계 | | 파일 중복 포함 | 24파일 / +2,300 | | |

- 조각 2의 +1,000은 +1,200 정지선 아래다. 그러나 여유가 적다. 구현 중 +1,100을 넘을 것 같으면 steering(`steer.rs`, `acp_steer.rs`, `busy_inject.rs`)을 조각 2b로 나눈다. 스위치는 2b 운영 확인 뒤 켠다.
- 1,000줄이 넘는 파일(`intake_turn.rs`, `intake_gate.rs`, `provider_dispatch.rs`)에는 인자 전달이나 호출 한 줄만 더하거나 아예 손대지 않는다. 본체는 `agy_acp/`와 `intake_gate/acp_steer.rs`에 둔다.
- 추정 근거는 #8274 조각 실측(1/+340, 2/+465, 3/+560)과 SDK 조사의 LOC 추정이다. 구현 뒤 `pr_cap_prod.py`로 실측한다.

## 17. 상태 차트 결과 요약

- **규칙(7개):** 입력마다 게시 1개 이하(`AtMostOnePost`), 받은 입력은 결국 덮임(`EveryInputSettles`), prompt 실행 1회(`NoDuplicateExecution`), 남의 응답으로 닫지 않음(`NoForeignResponse`), clear 뒤 옛 세션 금지(`NoPromptToClearedSession`), 게시에는 내구 종료 기록이 있음(`PostOnlyWithDisposition`), steering 입력 조건 2개(`AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer`).
- **결과:** 설계 3구성(Design: 상주·resume·steering, DesignLoad, PerTurn)은 7개 규칙을 모두 통과한다. 변이 8개와 합본 1개는 각각 예상한 규칙만 깬다. 12개 구성 모두 `scripts/ci/statecharts_tlc.py`에서 rc=0이다.
- **도달성:** Design에서 steering 응답과 거절 뒤 재실행이 실제로 일어남을 증인 불변식 위반(rc=12)으로 확인했다.
- 상세는 [보고서](../../statecharts/agy-acp/agy-acp.report.md)에 있다.

## 18. 사용자 결정과 미확인 가정

- **D0: 닫힘.** 사용자 결정(agy-acp-1123)으로 "사람이 직접 모는 대화형 클라이언트"를 전제로 확정했다. 착수 전 필수 목록에서 뺐다.
- **D1:** 권한 응답 정책. 사용자 결정 포럼에 올라가 있고, 그 전 기본은 (c)다.
- **F1(후속 후보):** antigravity 채널로 들어오는 시스템 입력(위임 작업, dispatch, 봇)의 consumer 로그인 `-p` 실행 처분.
- **F2(후속 후보):** 실행 중 iMessage의 steering.
- **[U1]** ACP sessionId가 UUID인가. 조각 2 운영 확인 때 본다.
- **[U2]** `usage_update`와 `StatusUpdate`의 필드 대응. 프로토콜 로그로 본다.
- **[U3]** agy가 `max_tokens`·`max_turn_requests`를 실제로 쓰는가. 프로토콜 로그로 본다.
- **[U4]** 공용 세션 칸이 바이트 단위로 그대로인가. 조각 1 시험으로 본다.
- **[U5]** spawn 비용. 조각 2에서 측정한다.
- **[U6]** app-data-dir를 분리할지 공유할지. 조각 2 사람 확인으로 정한다.
- **[U7]** `_meta.antigravity.systemPrompt.append` 지원 여부. initialize 로그로 본다.
- **[U8]** 큐 재구성 입력의 사람 증거. 조각 1에서 본다.
- **[U9]** agy의 ACP v2 이행. initialize의 `protocolVersion`이 1이 아니면 거절하고 경보를 낸다.
- **[U10]** `agentInfo.version` 형식. steering을 버전으로 게이트하지 않으므로 기록만 한다.
- **[U11]** 대화 DB의 위치와 스키마. 조각 3 전에 읽기 전용으로 확인한다.
- **[U12]** 재시작 전 세션이 새 자식에서 resume되는가. 조각 2 사람 확인으로 본다.
- **[U13]** 실제 steering 응답 형태(앞 prompt `end_turn`, 새 prompt가 턴을 이어 감). 공식 문서는 한 문장뿐이고, 형태는 T3 코드 주석에 근거한다. 조각 2 운영 확인 때 프로토콜 로그로 확인한다. 어느 형태든 6.2-5 규칙으로 같은 결과가 나도록 설계했다.
