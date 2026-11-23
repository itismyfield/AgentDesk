# #8339 antigravity 채널을 `agy acp` 클라이언트로 옮기는 설계

<!-- doc-path-refs: proposed src/services/agy_acp/ src/services/agy_acp/turn.rs src/services/agy_acp/steer.rs src/services/agy_acp/mod.rs src/services/agy_acp/child.rs src/services/discord/router/intake_gate/busy_inject/acp_steer.rs src/services/discord/turn_bridge/receipt_origins.rs src/services/discord/receipt_settle/pending_mark.rs -->
<!-- doc-path-refs: external src/v1/error.rs apps/server/ packages/provider-acp/ AcpSessionRuntime.ts AntigravityAcpSupport.ts AntigravityAdapterV2.ts AntigravityProtocol.ts AntigravitySessionFiles.ts AntigravityTurnOutcome.ts AntigravityEditLocation.ts antigravityAuthSupport.ts adapter.ts acp_agent.rs jsonrpc.rs -->

저장소 기준은 `5021d25794`(origin/main)이고, 확인한 agy CLI는 1.5.14(`agy --version`)다. 이 문서는 설계만 담는다. Rust 코드는 바꾸지 않는다. 상태 차트는 [agy-acp 모델](../../statecharts/agy-acp/AgyAcpSession.tla)과 [보고서](../../statecharts/agy-acp/agy-acp.report.md)다. 미확인 가정은 [U]로 표시하고 확인 방법을 함께 적는다.

## 0. 결정 요약

| 쟁점 | 결정 |
|---|---|
| 범위 | provider가 antigravity로 정해지는 **모든** Discord 채널을 대상으로 한다. 그 채널의 스레드와 antigravity 봇 DM도 포함한다(2절). 채널 이름이나 ID 목록은 두지 않는다. |
| 경로 판정 | 다음을 모두 만족하면 ACP 경로로 간다. ① 전역 스위치 `agy_acp.enabled` 켜짐 ② 턴 provider = Antigravity ③ 사람이 쓴 입력(Discord 사람 작성 또는 인증된 iMessage) ④ dispatch가 아님. 그 밖의 입력은 지금 경로(`agy --print`)로 간다. 스위치는 전체 하나이고 기본은 꺼짐이다. 조각 2 운영 확인 뒤 켠다. |
| 입구 | 분기점은 `stream_json_turn::execute_request`(`src/services/discord/router/message_handler/provider_dispatch/stream_json_turn.rs:181`) 한 곳이다. ACP 이벤트는 기존 `StreamMessage`와 `AgySpool` 기록으로 바꾼다. 그 아래(turn_bridge, agy writer, 스풀, 외부 입력, shadow)는 그대로 둔다. |
| 프로세스 수명 | **채널 상주**(destination당 `agy acp` 자식 하나). 재부착은 `session/resume`으로 하고 기록을 재전송하지 않는다. `session/load`는 대체 경로로만 쓰며, 이때 응답 전 갱신은 버린다. AgentDesk가 재시작하면 Windows는 dcserver job이 자식 트리를 닫고, Unix는 부팅이 옛 자식 그룹을 정리한다(5.2-12). 그 뒤 다음 입력이 새 자식을 띄워 resume한다. |
| 실행 중 입력 | **steering이 ACP 경로의 기본 동작이다**(6절). steer prompt 오류는 종류와 상관없이 그 입력을 큐로 돌리지 않는다(범위 축소 B-noreturn, 코디 결정 i8339-scope-1123). 입력은 턴의 구성원으로 남고, 재전송하지 않으며, 턴의 게시 하나가 안내 한 줄과 함께 덮는다(6.4). 턴의 prompt가 열려 있는 동안 들어온 사람 입력은 그 턴에 `session/prompt`로 보내 턴에 합친다. 턴의 답 게시 하나가 그 입력까지 덮는다. steering을 끄는 방법은 전역 스위치뿐이다. |
| 취소 | `!stop`은 `session/cancel`을 보내고 `stopReason:"cancelled"`를 기다린다. 15초 안에 오지 않으면 그 **자식만** 종료한다. Unix는 자식 전용 process group 신호, Windows는 `taskkill /PID … /T /F` 트리 종료이고, 둘 다 기존 `process.rs` 함수를 그대로 부른다. AgentDesk와 다른 destination은 건드리지 않는다(5.2-13 표). |
| 턴 종료 | 턴은 그 턴에 열린 모든 prompt의 응답(stopReason)이 도착했을 때 끝난다. #8160은 이것만으로 사라지지 않는다(9절). |
| 권한 | **D1 = A(사용자 결정).** `agy acp --dangerously-skip-permissions`로 띄우고, 그래도 오는 `session/request_permission`에는 `allow_once`로 자동 답한다(10절). |
| 모델 | 세션의 `configOptions`(`id:"model"`) 안에서만 고른다(일반 외부 클라이언트는 Gemini만). 목록에 없으면 실패를 한 번 게시하고, `-p`로 몰래 되돌아가지 않는다. |
| SDK | 공식 crate는 쓰지 않는다. v1의 필요한 부분만 tokio·serde_json으로 직접 구현한다(13절). |
| 사용 형태 전제(D0) | **사용자 결정(agy-acp-1123)으로 전제를 확정했다.** 사람이 직접 모는 대화형 클라이언트로 `agy acp`를 띄우는 방식을 허용된 형태로 본다. |
| 조각 | 4개(1, 2a, 2b, 3), 추정 합계 prod 35파일 / +2,730이다(파일 중복 포함). 조각마다 상한 아래다(16절). 1·2a 계약은 동결하고 구현을 먼저 시작한다. 2b의 영수증·원본 계약만 이어서 다듬는다. |

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
| 위임형(비서) 채널 | 턴 안에서 agy가 AgentDesk API로 다른 에이전트에 일을 넘긴다(`src/server/routes/agents.rs:1129` → `src/services/agents/delegation.rs:82-118` `handoff_origin`). 결과 카드는 outbox 봇 게시로 돌아온다(`delegation.rs:243-298`). | ① 위임 호출은 agy의 셸 도구(로컬 API 호출)라서 `session/request_permission` 대상이다. D1 = A라서 별도 allow 규칙 없이 동작한다. 사람이 관리하는 규칙 파일에 deny가 있으면 그 규칙이 먼저다(10절). ② 결과 카드는 봇 게시라 새 agy 입력이 아니다. 지금처럼 ACP 턴을 만들지 않는다. ③ 카드의 [중단]은 작업자 턴을 멈추고, agy 턴과는 무관하다. |
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
| 13 | `src/services/discord/router/intake_gate/busy_inject.rs:283-298`(`offered`), `:311-357`(`attempt`), `:373-400`(`settle`: ➕ 반응, 핸드백 시 큐 앞자리와 checkpoint 전진) | busy inject | **보강**. Antigravity ACP 턴이면 `covers` 앞에서 `acp_steer::offer`를 부른다. 핸드백 경로는 쓰지 않는다. 본체는 새 child `intake_gate/busy_inject/acp_steer.rs`(부모 `busy_inject.rs`의 `mod` 한 줄)에 둔다 |
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

- **새 파일:** `src/services/agy_acp/{mod,rpc,wire,child,turn,store,attach,steer,permission,outcome}.rs`, `src/services/discord/router/intake_gate/busy_inject/acp_steer.rs`, `src/services/discord/turn_bridge/receipt_origins.rs`, `src/services/discord/receipt_settle/pending_mark.rs`
- **바뀌는 파일:** `stream_json_turn.rs`(분기, 플래그), `intake_turn.rs`(인자 하나), `src/config` 한 곳(`agy_acp.enabled`, 기한), `src/services/mod.rs`(mod 한 줄), `intake_gate/busy_inject.rs`(ACP 호출 한 곳), `control.rs`, `retry_state.rs`, `host_recovery_guard.rs`, `model_picker.rs`, `model_catalog.rs`, `recovery_engine/restore_inflight.rs`(호출 한 줄), `turn_bridge/turn_analytics.rs`(영수증 병합 호출), `turn_bridge/mod.rs`(mod 한 줄과 seal 호출 한 줄), `receipt_settle.rs`(`keep_unread`, 대기 표시 기록·재시도, mod 한 줄), `turn_bridge/stream_loop/provider_recovery.rs`(재시도 전 봉인 확인 한 곳), `inflight/clear_store/abandon.rs`(원본을 진 행 보존)
- **가시성만 바꾸는 giant 파일:** `src/services/process.rs`(`from_persisted`의 `cfg(test)` 제거, `unix_process_presence`를 `pub(crate)`로)
- **그대로:** 3절의 유지 행 전부. `intake_gate.rs`(1,414줄)는 손대지 않는다.

## 5. 프로세스 수명

### 5.1 비교와 선택

| | 턴마다 `agy acp` | destination 상주 |
|---|---|---|
| 흐름 | 입력 → spawn → initialize → resume → prompt → stopReason → 종료 | 첫 입력 → spawn → initialize → resume/new → prompt…(턴 사이 유지) → 죽음·clear·유휴 만료·재시작 때만 다시 띄움 |
| 비용 | 턴마다 기동과 resume 왕복이 든다(T3 메모로는 기동마다 약 1GB 압축 해제, [U5]) | destination마다 상주 메모리가 든다. 유휴 만료 30분으로 줄인다 |
| steering | 턴 안에서만 가능하다. 턴 경계와 프로세스 경계가 같아 늦게 온 입력이 종료와 경합한다 | 세션이 살아 있어 자연스럽다 |
| 위험 | 반환 경계는 단순하다. 사용 형태가 `-p` 자동 루프처럼 보인다 | incarnation 간 늦은 응답, clear 뒤 옛 세션, 취소 무응답, steering 경합을 막아야 한다 |
| 모델 | PerTurn GREEN | Design·DesignLoad GREEN. 변이는 각각 `expected.tsv`의 규칙만 깬다(17절) |

**선택: 상주.** 허용된 사용 형태(D0)인 "사람이 모는 대화형 클라이언트"에 맞는다. 기본 동작인 steering과 세션을 유지하는 cancel도 상주 세션에서 의미가 있다.

### 5.2 상주 규칙

1. destination마다 actor 하나가 자식 하나와 세션 하나를 소유한다. 자식은 기존 `configure_child_process_group`(`src/services/process.rs:84-97`, Unix는 `process_group(0)`, Windows는 `CREATE_NEW_PROCESS_GROUP`)으로 띄운다. 지금 stream-json runner가 이미 이 방식이다(`src/services/stream_json_cli/runner.rs:104-106`). 종료는 새로 만들지 않고 기존 플랫폼 분기를 그대로 쓴다. Unix는 자식 PGID에만 신호를 보내고, Windows는 자식 PID 기준 `taskkill /PID <pid> /T /F`로 그 트리만 끝낸다(`process.rs:579-583`). 진입점별 호출은 13의 표다. AgentDesk 자신이나 다른 destination의 자식에는 닿지 않는다. `--process-per-session`은 쓰지 않는다.
   - `agy_acp::child`는 자식을 `std::process::Child`로 띄우고 기존 `StreamChild`(`src/services/process/stream_child.rs:13-149`)에 맡긴다. 전용 blocking 스레드 하나가 `StreamChild::wait`(`:137-148`)를 돈다. stdin·stdout은 runner처럼 reader·writer 스레드와 채널로 actor에 잇는다.
   - `StreamChild`에는 **자식 전용** `CancelToken::new()`(`src/services/provider.rs:269`)를 넘긴다. 턴의 CancelToken은 넘기지 않는다. 그 토큰을 취소하면 `wait`가 `terminate`를 부른다. 상주 자식은 턴보다 오래 살기 때문이다.
   - 자식을 거두기 전까지 Unix PID는 재사용되지 않는다. 그래서 종료 시점의 identity 확인도 기존 함수에 맡긴다.
2. spawn 명령은 `agy acp --dangerously-skip-permissions --app-data-dir <D> [--gemini-dir <P>]`이다(D1 = A, 10절). cwd는 채널 작업 폴더, env는 기존 auth overlay에 `AGY_CLI_DISABLE_AUTO_UPDATE=1`을 더한 것이다. 사설 `TMPDIR`을 쓴다.
3. initialize에는 `protocolVersion: 1`, `clientInfo{name:"agentdesk"}`를 보낸다. 다른 클라이언트로 위장하지 않는다(T3 구버전의 `zed` 위장 금지). fs·terminal capability는 false로 둔다. 응답의 capability와 `agentInfo.version`은 기록한다.
4. authenticate는 보내지 않는다. 캐시된 자격을 쓴다. -32000이면 인증이 필요하다는 안내를 게시하고, 로그인 URL은 중계하지 않는다.
5. attach는 다음 순서다. 저장 세션이 없으면 `session/new`다. 있으면 resume capability가 있을 때 `session/resume`, `loadSession`만 있을 때 `session/load`를 쓰되 응답 전 갱신은 버린다. 거절되면 `session/new`로 열고, 그 턴의 답 앞에 맥락 손실 안내 한 줄을 붙인다.
6. prompt는 스풀 예약과 inflight 저장(write-ahead)을 끝낸 뒤에만 쓴다.
7. 대기 맵 키는 `(incarnation, request id)`다. 리더 task는 자기 incarnation이 아닌 줄을 버린다. steering된 앞 요청은 자기 응답이 올 때까지 맵에 남긴다.
8. 무활동 15분 또는 취소 15초가 지나면 그 자식만 강제 종료한다(13 표의 "강제 종료"). 열린 턴은 실패 또는 취소로 닫는다.
9. 유휴 30분이 지나면 정상 종료한다(13 표의 "정상 종료"). 저장 세션은 남긴다.
10. 부팅 때 자식을 미리 띄우지 않는다. 내구 기록이 있고 종료 기록이 없는 턴은, inflight `input_origin_ids`에 든 입력(steering된 입력 포함)을 모두 덮는 실패 안내로 닫는다. 다시 보내지 않는다. 종료 기록이 있는데 게시가 안 된 턴은 writer가 게시한다. 첫 spawn 전에 12의 옛 자식 정리를 끝낸다.
11. incarnation 번호는 actor 메모리에만 둔다. 재시작하면 옛 리더 task가 없으므로 내구 기록이 필요 없다.
12. **소유 자식 정리(크래시 뒤).** 부모가 죽을 때 자식이 같이 죽는지는 플랫폼마다 다르다.
    - spawn 직후 `<runtime_root>/agy_acp/<destination>/child.json`에 `{pid, pgid, starttime, macos_lstart_hash}`를 원자적으로 쓴다(`crate::domain::atomic_write::prepare`→`publish`, `src/domain/atomic_write.rs:51,85`). identity 값은 기존 `ProcessIdentity::capture`(`process.rs:177`)의 `persisted_starttime`·`persisted_macos_lstart_hash`(`:269,273`)다. 13 표의 종료가 끝나 `StreamChild::wait`가 돌아오면 기록을 지운다.
    - **Windows:** dcserver는 자식을 띄우기 전에 자기 자신을 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` job에 넣는다(`src/services/platform/windows_job.rs:16-56`, 호출 `src/cli/dcserver.rs:881-885`). 크래시를 포함해 dcserver가 끝나면 OS가 자식 트리 전체를 닫는다. 그래서 부팅 때 남은 `child.json`은 신호 없이 지우기만 한다. Windows에는 PID 재사용을 가릴 identity가 없다(`ProcessIdentity::probe`·`process_group_probe`는 `cfg(unix)`). 기록된 PID에 `taskkill`을 보내면 남의 프로세스를 죽일 수 있어서 보내지 않는다.
    - **Unix:** 자식이 다른 그룹에 있으므로 고아로 남을 수 있다. 부팅은 그 destination의 첫 spawn 전에 `agy_acp::child::sweep_recorded`로 아래 표를 적용한다. 리더만 죽고 손자가 남은 그룹도 이 표가 정리한다.

    | 리더 `ProcessIdentity::from_persisted(..).probe(pid)` | pid 존재 `unix_process_presence(pid)` | 그룹 `process_group_probe(pgid)` | 처리 |
    |---|---|---|---|
    | `Same` | — | — | `kill_pid_tree_if_identity_matches(pid, identity)`(`process.rs:519-536`). 그룹이 `Gone`이 되면 기록을 지운다 |
    | `GoneOrReused` | `Gone` | `Live` | 리더만 죽고 하위 프로세스가 남은 경우다. POSIX는 그룹이 살아 있는 동안 그 번호를 새 PID로 주지 않으므로 이 그룹은 옛 자식의 것이다. `kill_pid_tree(pgid)`(`:506-516`)는 리더가 없으면 그룹에 SIGTERM을 보내고, 그룹이 남으면 SIGKILL을 보낸다(`:545-576`). `Gone`을 확인한 뒤 지운다 |
    | `GoneOrReused` | `Live` | 무관 | PID가 재사용됐다면 옛 그룹은 그 전에 이미 사라졌다(같은 POSIX 규칙). 신호 없이 지운다 |
    | `GoneOrReused` | `Gone` | `Gone` | 지운다 |
    | `ProbeError`, 또는 처리 뒤에도 그룹 `Live` | | | **보류.** 기록을 유지하고 그 destination에는 spawn하지 않는다. 이번 입력은 "이전 agy 정리 실패" 실패 안내로 닫는다(조용한 유실 아님). 다음 입력에서 다시 판정한다 |

    - `process.rs`(1,255줄)는 가시성만 바꾼다. `from_persisted`(`:260-261`)의 `#[cfg(test)]`를 떼고, private `unix_process_presence`(`:338-339`)를 `pub(crate)`로 연다. 새 책임은 얹지 않는다.
    - 정리가 끝나기 전에는 같은 destination에 새 자식을 띄우지 않는다. 옛 자식이 같은 세션을 계속 쓰는 상태에서 새 자식이 resume하는 일을 막기 위해서다(모델 `SweepOrphan`).
13. **종료 진입점과 부르는 함수.** 모든 진입점은 아래 두 동작 중 하나로 모인다. 직접 신호를 보내는 새 함수는 만들지 않는다.
    - **강제 종료:** 자식 전용 토큰 취소 → `StreamChild::wait`가 `StreamChild::terminate`(`stream_child.rs:125-135`) 호출 → `process::kill_child_tree`(`process.rs:588-595`) → `kill_pid_tree`(`:506-516`).
    - **정상 종료:** stdin writer를 닫고 `StreamChild::wait`가 2초 안에 돌아오는지 본다. 돌아오지 않으면 강제 종료로 넘어간다. 2초는 [U14]로 정한다.

    | 진입점 | 동작 | Unix에서 실제로 일어나는 일 | Windows에서 실제로 일어나는 일 |
    |---|---|---|---|
    | Cancel(`!stop`·`/stop`·API cancel) | `session/cancel` 알림만 보낸다. 15초 안에 `cancelled`가 없으면 아래 "기한 초과" | 프로세스 동작 없음 | 같음 |
    | Terminate(hard stop) | 강제 종료 | `kill(-pgid, SIGTERM)` → 200ms → 리더 identity가 맞거나 리더가 없고 그룹이 남으면 `kill(-pgid, SIGKILL)`(`process.rs:545-576`) → `child.kill`·`wait` | `taskkill /PID <pid> /T /F`(`:579-583`) → `child.kill`·`wait` |
    | `/clear`·`!clear` | 열린 prompt가 있으면 강제 종료. 없으면 `session/close`(지원 시) 뒤 정상 종료 | 위 둘 중 하나 | 위 둘 중 하나 |
    | stop(서비스 정지, 스위치 끄기, 유휴 만료) | 정상 종료 | stdin EOF로 끝나면 `observe_exit`가 남은 그룹을 SIGTERM→SIGKILL한다(`stream_child.rs:51-111`, WNOWAIT로 PID를 잡은 채). 2초를 넘기면 강제 종료 | stdin EOF로 끝나면 거두기만 한다. 2초를 넘기면 강제 종료 |
    | 기한 초과(취소 15초, 무활동 15분) | 강제 종료 | Terminate와 같다 | Terminate와 같다 |
    | 자식 스스로 종료(EOF) | 없음(`wait`가 돌아옴) | `observe_exit`가 남은 그룹을 정리한다 | 거두기만 한다. 손자가 남을 수 있고 dcserver가 끝날 때 job이 닫는다. 지금 `-p` 경로(`StreamChild`의 Windows 분기)와 같다. 넓히지 않고 후속 F3로 둔다 |
    | AgentDesk 크래시 | 다음 부팅의 12 | 12의 표 | job이 이미 닫았다. 기록만 지운다 |

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
2. **판정(actor 안, 원자적):** T에 열린 prompt가 하나라도 있고, T가 봉인되지 않았고(3a), 취소가 요청되지 않았으면 steer를 받는다(수락). 그 밖에는 `NotSent`를 낸다. 열린 prompt가 없으면(T가 마지막 응답을 받아 닫히는 중이면) `turn_closing`, 봉인됐으면 `turn_sealed`, 취소 중이면 `turn_stopping`이다. B는 아직 저장도 전송도 하지 않았으므로 지금 BusyActiveTurn 큐로 가서 다음 턴이 된다. actor는 입력을 하나씩 처리한다. 수락한 입력의 저장과 전송(또는 버림)이 끝나기 전에는 다음 입력도, T의 응답도 처리하지 않는다.
3. **원본 키와 내구 기록(write-ahead):**
   - **원본 키:** Discord 메시지의 원본 키는 `dmsg:<logical channel id>:<message id>`다. 기존 원본(위임 `dlg:`, HTTP `human_input/origin_id`)과 같은 inflight 칸 `input_origin_ids`(`src/services/discord/inflight/model.rs:151`)에 넣는다. 새 칸도 새 원장도 없다.
   - **A(턴의 첫 입력):** 일반 Discord intake는 `source_message_ids`만 채우고 `input_origin_ids`는 빈 채로 시작한다(`inflight/model.rs:1094-1095`, `src/services/discord/router/message_handler/intake_turn/intake_dispatch.rs:4-15`). 그래서 ACP 턴은 첫 `session/prompt` 전에 A의 원본 키를 같은 칸에 더해 저장한다. 키는 `source_message_ids`마다 하나이고, 그 목록이 비면 `user_msg_id` 하나다. 자리는 `agy_acp::turn`의 스풀 예약 뒤, 첫 prompt 직전이다. HTTP 입력처럼 원본이 이미 있는 행(`router/message_handler/provider_dispatch/stream_json_turn.rs:41-48` `stamp_http`)에는 더하지 않는다.
   - **B(steer):** `inject_disposition::claim_source`는 메모리 표와 Weak lease라서(`src/services/discord/inject_disposition.rs:239-258`) 부팅 복구의 근거가 될 수 없다. 내구 소유자는 T의 inflight 행이다. 수락 뒤 prompt를 쓰기 전에 B의 원본 키를 T의 같은 칸에 더한다.
   - 저장은 A·B 모두 기존 identity-gated 저장 `save_inflight_state_if_identity_unchanged`(`src/services/discord/inflight/save_store/identity_gate.rs:44-53`)를 쓴다. 새 저장 함수는 없다. `inflight/model.rs`(1,441줄)는 giant라 손대지 않는다.
   - 저장이 `Saved`가 아니면(T의 행이 바뀌었거나 IO 오류) 그 prompt를 쓰지 않는다. B는 `NotSent` → 지금 큐로 간다. A면 첫 prompt 없이 턴을 시작 실패와 같은 `Error`로 닫는다. 어느 쪽도 실행하지 않았으므로 중복은 없다. A가 실행되지 않은 이 실패 턴의 영수증에는 A 원본이 없을 수 있다(실행 0이라 재실행 판단에 쓰이지 않는다).
   - 저장한 뒤 actor는 T가 취소 중이거나 봉인됐는지 다시 본다. 그렇다면 prompt를 쓰지 않는다. B는 T의 구성원으로 남고, T의 게시가 실행 전 거절 안내(6.4.1 ①)와 함께 덮는다. A면 7절 `!stop` 행대로 취소 안내가 덮는다.
   - 저장한 뒤에만 checkpoint를 B 너머로 옮기고 prompt를 쓴다. 그래서 catch-up은 B를 다시 실행하지 않는다. 재시작이 저장과 전송 사이에 오면 그 입력은 쓰이지 않고 행에 남아 부팅 실패 안내가 덮는다. 저장 전(수락만 한 때)에 오면 행에도 checkpoint에도 흔적이 없으므로 catch-up이 다시 가져온다.
   - 부팅·표시된 행·강제 정지의 영수증(`src/services/discord/receipt_settle.rs`)은 디스크 행의 이 칸만 읽는다(`receipt_settle.rs:52-90`의 `record`). `src/db/turns.rs:239-289`가 합집합으로 누적한다. 다만 bridge의 완료 영수증은 메모리 사본을 쓴다. 그래서 봉인(3a) 뒤 완료 직전에 디스크 원본을 병합한다(3b). 그러면 A와 B는 T의 답, 취소, 실패 중 어느 쪽이든 T의 게시 하나와 영수증 하나에 덮인다.
   - 칸에서 원본을 빼는 일은 없다. steer 오류가 와도 B는 T의 구성원으로 남는다(6.4). 그래서 제거와 저장 사이의 경계가 없다.
   - 그래서 steer가 실패한 B도 T의 `consumed_origin_ids`에 남는다. 이것은 의도다. catch-up과 부팅이 B를 다시 실행하지 않게 하고, 다시 보낼지는 안내를 본 사람이 정한다. 다시 보낸 글은 새 message id다.
   - 어느 steer가 오류로 끝났는지와 그 종류는 actor 메모리에만 둔다. 게시의 안내 문구를 고르는 데만 쓴다. 재시작으로 잃어도 부팅 실패 안내가 B를 덮으므로 유실은 아니다.
3a. **steer 수락 봉인:**
   - **문제:** bridge는 `!stop`에서 actor의 저장 완료를 기다리지 않고 루프를 나온다(`turn_bridge/stream_loop.rs:220-225`). B를 수락한 뒤 저장 직전에 `!stop`이 오면, bridge가 A만 읽고 영수증 기록을 기다리는 동안 B가 같은 턴에 저장되고 checkpoint가 전진할 수 있다. 뒤이은 삭제는 같은 identity·nonce의 행을 지우므로(`inflight/clear_store/identity.rs:175-207`) B가 영수증과 inflight 양쪽에서 사라진다(1f 리뷰 P1).
   - **계약:** bridge는 스트림 루프가 끝난 직후, 종단 게시(`turn_bridge/mod.rs:778` `run_terminal_outcome_delivery`)와 영수증 조회보다 먼저 `receipt_origins::seal(provider, channel_id, &inflight_state)`를 부른다. 자리는 `turn_bridge/mod.rs:677`(루프 결과 match 앞)의 한 줄이다. seal은 그 destination의 actor에 `Seal{turn_nonce}`를 보내고 답을 기다린다.
     - actor는 그 턴의 수락을 닫는다. 이미 수락한 입력의 저장, 그리고 3의 전송이나 버림이 끝난 뒤에 답한다. agy의 취소 응답은 기다리지 않는다.
     - 답은 `Sealed`, `NoTurn`(actor가 없거나 그 nonce의 턴이 없다. 받을 주체가 없으므로 봉인과 같다), `TimedOut`(5초) 중 하나다.
     - seal은 같은 nonce에 대해 멱등이다. 영수증 조회 직전(`latest_origins`)에 한 번 더 부른다. 그때도 `TimedOut`이면 읽기 실패와 같이 보류한다(3b 표). 늦게 저장된 입력도 행에 남아 다음 정산이 기록한다.
     - 봉인 뒤 들어온 입력은 `NotSent("turn_sealed")` → 지금 큐 경로다. 그 입력은 T의 구성원이 아니다. T의 행, 영수증, 게시에 들어가지 않고 다음 턴이 된다. B-noreturn과 같은 방향이다(한 번 정한 소속은 바꾸지 않는다).
     - provider가 Antigravity가 아니거나 스위치가 꺼져 있으면 seal은 잠금과 IO 없이 바로 `NoTurn`을 돌려준다.
     - 정상 완료에서는 actor가 마지막 응답 뒤 T를 닫으므로 이미 봉인된 상태다. seal은 확인만 한다.
3b. **완료 영수증 직전 디스크 원본 병합:**
   - **문제:** bridge는 시작할 때 inflight를 메모리로 복제한다(`src/services/discord/turn_bridge/mod.rs:427`, `turn_bridge/stream_loop.rs:121-133`). 실행(`intake_turn.rs:2108`의 `spawn_blocking`)이 bridge 생성(`:2177`)보다 먼저 시작하고, A·B의 write-ahead는 디스크 행에만 들어간다. bridge 사본은 다음 stream tick 병합에서야 그 원본을 안다. 그 전에 취소가 오면 bridge는 tick 없이 끝난다(`stream_loop.rs:220-225`). 종료 저장 병합은 pending message 후보가 있을 때만 돈다(`turn_bridge/stream_loop/exit_reconcile.rs:179-207`). 그래서 완료 영수증(`turn_bridge/completion_postlude.rs:904-919` → `turn_bridge/turn_analytics.rs:26-67`, `consumed_origin_ids: &inflight_state.input_origin_ids`는 `:49`)에 A·B가 빠질 수 있다.
   - **계약:** `record_terminal_receipt`(`turn_analytics.rs:26-67`)가 `TurnReceipt`를 만들기 직전에 새 child `turn_bridge/receipt_origins.rs`의 `latest_origins(provider, channel_id, &inflight_state) -> ReceiptOrigins`를 부른다. 이 함수는 seal을 다시 확인한 뒤 그 턴의 최신 디스크 행을 읽어 메모리 원본과 병합한다.
     - 읽기는 기존 `inflight::load_inflight_state_read_only_result`(`src/services/discord/inflight.rs:909-925`)다. 없음(`Ok(None)`)과 읽기 실패(`Err`)를 구분한다.
     - 소유 판정은 bridge 완료 경로가 이미 쓰는 두 검사다. `InflightTurnIdentity::from_state(&inflight_state).matches_state(&row)`(`inflight/model/identity.rs:57-63`)와 `row.owned_by_turn_nonce(inflight_state.turn_nonce.as_deref())`(`inflight/save_store/identity_gate/captured_nonce.rs:12-15`)다.
     - 병합한 목록은 기록과 보류 판단에 함께 쓴다. 메모리 사본의 빈 목록으로 `owes_origin_receipt`(`inflight/terminal_receipt_stamp.rs:13-17`)를 판단하면 원본을 진 행이 "원본 없음"으로 지워지기 때문이다.
     - 새 원장도 새 inflight 필드도 없다. 거대 파일 `completion_postlude.rs`(1,078줄)는 손대지 않는다.

   | 디스크 읽기 결과 | 처리 | 이유 |
   |---|---|---|
   | 같은 identity·nonce의 행 | 메모리 원본 뒤에 디스크에만 있는 원본을 순서대로 더한다(중복 제거). 그 목록으로 기록한다 | A·B를 포함한다 |
   | 다른 턴의 행(identity 또는 nonce 불일치) | 합치지 않는다. 메모리 원본만 기록한다 | 남의 원본을 이 턴의 소비로 적지 않는다 |
   | 행 없음(`Ok(None)`) | 메모리 원본만 기록한다 | 행을 지우는 다른 주인은 모두 지우기 전에 디스크 행으로 영수증을 기록하거나 행을 표시해 남긴다(`receipt_settle.rs` 모듈 계약 1-2행, 아래 표). 영수증은 합집합으로 누적되므로 잃는 원본이 없다 |
   | 읽기 실패(`Err`), 또는 seal `TimedOut` | **기록하지 않는다.** 빈 목록 성공으로 보지 않는다. 새 `receipt_settle::keep_unread`가 메모리 원본과 상관없이 행을 보존하고 false를 돌려준다. 그러면 `completion_postlude.rs:904-919`의 `receipt_kept`가 참이 되어 행을 지우지 않는다. 같은 identity·nonce로 `terminal_receipt` 표시를 시도하고, 표시하지 못하면 대기 표시로 남긴다(3c) | 소유권을 확인하지 못한 채 행을 지우면 A·B의 증거가 사라진다 |

3c. **표시 재시도:**
   - **문제:** 최초 조회와 `keep_marked`의 표시 함수(`inflight/terminal_receipt_stamp.rs:21-51`)는 같은 파일을 읽는다. 오류가 두 호출에 걸치면 `keep_marked`(`receipt_settle.rs:111-144`)는 행을 보존하지만 표시를 쓰지 못한다. 다음 admission은 표시된 행만 정산하므로(`receipt_settle.rs:186-195` `settle_marked_row`, `router/message_handler/foreign_row.rs:82-98`) 파일이 다시 읽혀도 새 입력이 부팅까지 계속 밀린다(1f 리뷰 P1).
   - **계약:** 행 보존과 표시 성공을 나눈다. `keep_marked`와 `keep_unread`는 표시가 `Saved`가 아니면 새 child `receipt_settle/pending_mark.rs`에 대기 표시 `(provider, channel) → (identity, nonce, stamp)`를 남긴다.
     - `settle_marked_row`는 행에 표시가 없으면 대기 표시를 찾는다. identity·nonce가 그 행과 같을 때만 `stamp_terminal_receipt_if_matches_identity`를 다시 부른다.
     - 재표시가 `Saved`면 기존 `settle_marked`로 디스크 행 그대로(A·B) 기록하고 행을 지운다. 그 뒤 admission은 `foreign_row.rs:82-98`의 기존 재시도로 새 행을 만들고 진행한다.
     - 아직 읽지 못하면 false를 돌려준다. 지금처럼 미루고 다음 admission에서 다시 한다.
     - identity·nonce가 다른 행(다른 턴)에는 표시하지 않고 대기 표시를 버린다.
     - 대기 표시는 프로세스 메모리라 재시작으로 잃는다. 부팅은 표시 없는 행도 복구와 `BootReceipts::settle_cleared`로 디스크 행에서 기록한다(`receipt_settle.rs:204-266`). 그래서 새 원장이 아니다.
     - 같은 변경은 기존 DB 기록 실패 경로(`record_or_stamp`, `turn_analytics.rs:59-66`)에도 적용된다. 그 경로에도 표시 실패 때 같은 막힘이 있었다.

   - **같은 경계를 쓰는 경로 전수(ACP 턴):**

   | 경로 | 영수증을 쓰는 곳 | 읽는 원본 | 처리 |
   |---|---|---|---|
   | 정상 완료 | `completion_postlude.rs:904-919` → `turn_analytics.rs:26-67` | bridge 메모리 + 디스크 병합 | 봉인 → 병합 |
   | `!stop`·`/stop`·API cancel | 토큰 취소 → `stream_loop.rs:220-225`(cancel 분기, tick 없음) → 같은 postlude | bridge 메모리 + 디스크 병합 | 봉인(이미 수락한 B의 저장을 기다림) → 병합 |
   | `/clear`·`!clear` | `commands/control/clear_teardown.rs:243-255` `stop_released_turn`(`PreserveSession`: 행을 지우지 않음) → 토큰 취소 → 같은 postlude | bridge 메모리 + 디스크 병합 | 봉인 → 병합 |
   | Terminate·기한 초과·자식 EOF | actor가 자식 종료(5.2-13) → 턴 `Error` → bridge → 같은 postlude | bridge 메모리 + 디스크 병합 | 봉인 → 병합 |
   | DB 기록 실패 뒤 | `keep_marked` → `settle_marked_row`(`router/message_handler/foreign_row.rs:45,84`) 또는 `settle_boot` | 디스크 행 | 표시 실패면 대기 표시 재시도(3c) |
   | 크래시 재시작 | `recovery_engine/restore_inflight.rs:104` `settle_boot`(`receipt_settle.rs:204-243`), `BootReceipts::settle_cleared`(`:245-266`), 2a의 ACP 부팅 실패(5.2-10) | 디스크 행 | 병합 필요 없음(디스크가 정본) |
   | 계획 재시작(`restart_mode`) | bridge는 영수증을 쓰지 않고 행을 남긴다(`completion_postlude.rs:896-902`의 `bridge_epilogue_clears_inflight`) → 부팅 | 디스크 행 | 병합 필요 없음 |
   | health 강제 정지 | `health/recovery/runtime_stop.rs:124,163,244` → `receipt_settle::stopped`(`receipt_settle.rs:347-364`) | 디스크 행 | 병합 필요 없음 |
   | bridge 진입 거절 | `turn_bridge/bridge_entry_persist.rs:510` → `entry_aborted`(`receipt_settle.rs:403-420`) | 디스크 행 | 병합 필요 없음 |
   | fallback·quota 자동 재시도(profile retry, agent takeover) | `turn_bridge/stream_loop/content_arms.rs:423-446` → `turn_bridge/stream_loop/provider_recovery.rs` `on_error` | — | **`on_error` 첫머리에서 ACP 턴을 봉인하고 디스크 행을 읽는다. 원본이 둘 이상이면(steer가 들어감) 또는 읽지 못하면 재시도하지 않는다.** 재시도는 A만 다시 실행하기 때문이다. 턴은 실패 안내 하나로 닫히고 postlude가 병합 영수증을 쓴다. 원본이 A 하나면 같은 행·같은 원본으로 A를 다시 실행한다(지금 HTTP 원본 행과 같다) |
   | bridge task 비정상 `Drop`(panic, 조기 return) | `turn_bridge/guards.rs:308-323` → `inflight/clear_store/abandon.rs:134-240`(영수증 없이 행 삭제) | 디스크 행 | **원본을 진 행(`owes_origin_receipt`)은 지우지 않고 기존 `keep_owed_row_marked`(`inflight/terminal_receipt_stamp.rs:57-75`)로 실패 표시해 남긴다.** placeholder 기록은 지금대로 남긴다. 다음 admission이나 부팅이 디스크 행으로 기록한다. HTTP 원본 행에도 같이 적용된다(지금은 영수증 없이 지운다) |
   | agy writer `Effect::ClearInflight` | `outbound/o_writer_io/agy.rs:99-119` | — | **범위 밖.** 운영 부팅은 agy writer를 시작하지 않는다(`runtime_bootstrap/o_writer_host.rs:548-556`: boot 선택 채널의 스레드 조회만 설치한다). 이 writer를 운영에 연결하는 변경은 `ClearInflight` 전에 `receipt_settle::record_or_stamp`(`receipt_settle.rs:94-107`)를 거쳐야 한다(후속 F6) |
   | tmux·watcher·herdr·native 경로(`tmux_watcher/*`, `turn_finalizer/native_*`, `recovery_engine/herdr_admitted_restart.rs:173`, `health/recovery/carried_turn_close.rs:454`) | ACP 턴은 tmux·watcher·herdr·native를 쓰지 않는다 | — | 해당 없음 |

3d. **봉인과 A 원본 저장의 자체 공격:**

   | 공격 | 결과 | 근거 |
   |---|---|---|
   | B를 수락한 직후 `!stop`(봉인 직전 수락) | seal이 B의 저장 결과를 기다린 뒤 답한다. B는 행에 있어 영수증에 들어간다. 취소 중이라 B의 prompt는 쓰지 않고, T의 취소 게시가 ① 안내와 함께 덮는다 | 변이 SealOff가 이 순서로 `NoMissingOrigin`을 깬다(14걸음). Design은 통과하고 증인 `NoUnsentSteer`가 실제로 일어난다 |
   | 봉인 뒤 입력이 큐로 가서 다음 턴에 실행됨(중복 게시) | 봉인 뒤 입력은 T의 행과 게시에 없다. 자기 턴의 게시 하나만 받는다 | `AtMostOnePost`, `NoDuplicateExecution` 통과 |
   | actor가 seal에 답하지 않음 | 5초 뒤 `TimedOut` → 영수증을 쓰지 않고 행을 보존한다(3b 표). 늦게 저장된 입력도 행에 남아 다음 정산이 기록한다 | 3b·3c |
   | seal이 다른 턴을 닫음 | seal은 nonce를 받는다. actor의 현재 턴 nonce가 다르면 `NoTurn`을 돌려주고 아무것도 닫지 않는다 | 3a |
   | A 원본과 기존 Discord 기록의 이중 기록 | `source_message_ids`는 intake custody(`src/db/replay_disposition.rs`의 `sources`)와 catch-up(`recovery_known_ids.rs:60-70`)이 읽는다. turns 영수증은 `input_origin_ids`만 쓴다(`receipt_settle.rs:52-90`, `turn_analytics.rs:42-51`). `dmsg:` 키는 turns `consumed_origin_ids`에만 들어간다. 두 기록은 다른 표와 다른 키라 서로를 세지 않는다 | 소비자 grep 대조(보고서) |
   | `dmsg:` 키가 다른 원본 소비자와 섞임 | `consumed_origin_ids`의 소비자는 위임 카드(`src/services/agents/delegation.rs:252`, `dlg:%`만)와 `find_origin_receipt`(`src/db/turns.rs:304`; HTTP 원본으로만 조회: `agents/input_result.rs:90`, `queue_io/decision_drain.rs:96`)다. `dmsg:`로 조회하는 소비자는 없다. HTTP `origin_id`는 형식 검사가 없어 인증된 호출자가 `dmsg:`를 보낼 수 있다(기존 자유도). 후속 F5 | — |
   | A 원본이 HTTP 원본과 겹침 | HTTP 원본이 이미 있는 행에는 더하지 않는다 | 3 |
   | 대기 표시가 다른 턴의 행에 찍힘 | identity·nonce가 같을 때만 다시 표시한다 | 3c |
   | 재시작이 저장과 전송 사이에 옴 | 입력은 쓰이지 않고 행에 남아 부팅 실패 안내가 덮는다. 재실행은 없다 | 증인 `NoCrashAfterSave`, `NoDuplicateExecution` 통과 |
   | 표시 재시도가 없으면 채널이 막힘 | 재시도는 admission마다 돈다. 읽기가 돌아오면 정산된다. 재시작이면 부팅이 정산한다 | 변이 MarkNoRetry가 `EveryInputSettles`를 깬다 |

   - **모델:** `SendPrompt`를 `Accept`(수락)·`Save`(write-ahead)·`Wire`(prompt 쓰기, 취소·봉인이면 버림) 세 전이로 나눴다. 첫 입력의 원본도 저장한다(`Durable`). `EarlyLookup`은 `!stop` 때 bridge가 봉인하고 영수증을 조회하는 단계이고, 저장되지 않은 수락 입력이 있으면 기다린다. `Settle`은 그 밖의 경우 봉인과 조회를 같은 단계에서 한다. 조회 실패는 행을 남기고(`held`), 표시 여부는 정해지지 않는다(`marked`). `AdmitSettle`이 표시를 다시 하고 기록한다. 변이 SealOff, LeaderOriginOff, MarkNoRetry, ReceiptFromMemory가 각 경계를 깬다(17절).
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
  - `EveryInputSettles`: 받은 입력은 결국 게시 하나에 덮인다. steering 입력과 steer가 실패한 입력도 포함한다
  - `AnswerCoversWholeSteer`: 턴 게시는 구성원의 prompt가 모두 응답한 뒤에만 한다
  - `NoAnswerForRefusedSteer`: steer가 실패한 입력은 안내 없는 답으로 덮이지 않는다. 그 입력을 덮는 게시는 실패 안내 한 줄을 갖는다
  - `NoDuplicateExecution`: 받아들여진 prompt 쓰기는 입력마다 1회
  - `DurableOwner`: 저장(write-ahead)된 입력은 언제나 그 원본을 inflight에 가진 턴의 구성원이다
  - `PostOnlyWithDisposition`: 게시 뒤에는 그 턴의 내구 종료 기록이 있다
  - `NoReviveAfterClear`: clear 때 이미 보낸 입력은 다시 큐에 들어가지 않는다(clear 뒤 늦게 온 steer 오류 포함)
  - `NoMissingOrigin`: 게시가 덮은 입력은 첫 입력과 steering 입력을 포함해 그 턴의 영수증 `consumed_origin_ids`에 있다. 아니면 그 턴의 행이 영수증을 위해 아직 남아 있다(6.2-3a~3c)
- 이 규칙들을 Design에서 확인했다. 도달성 증인 일곱 가지도 Design에서 실제로 일어남을 확인했다(17절).

### 6.4 (d) 거절, 무시, 실패의 처리

| 경우 | 처리 | 모델 |
|---|---|---|
| 턴이 닫히는 중(열린 prompt 없음) | `NotSent` → 지금 BusyActiveTurn 큐로 가서 다음 턴이 된다. 닫히는 턴에 합치지 않는다 | 변이 SteerAfterClose(합치면 `AnswerCoversWholeSteer` 위반) |
| 턴이 봉인됨(3a) 또는 취소 중 | `NotSent("turn_sealed"/"turn_stopping")` → 지금 큐. T의 구성원이 아니다 | 변이 SealOff(봉인 없이 조회하면 `NoMissingOrigin` 위반) |
| 수락·저장 뒤 prompt 전에 취소나 봉인이 옴 | prompt를 쓰지 않는다. B는 T의 구성원으로 남고, T의 게시가 ① 안내와 함께 덮는다 | `Wire`의 버림 분기, 증인 `NoUnsentSteer` |
| write-ahead 저장 실패 | prompt를 쓰지 않는다. B는 `NotSent` → 큐, A는 시작 실패 안내 | — (2b 시험) |
| B의 `session/prompt`가 오류로 끝남(종류 무관) | B를 T의 구성원으로 둔다. 큐로 돌리지 않고 재전송하지 않는다. ➕는 그대로 둔다(접수 표시). T의 terminal 게시 하나가 B를 덮고 안내 한 줄을 붙인다. 실행 전 거절(6.4.1 ①)이면 "B는 전달되지 않았다. 다시 보내 달라", 결과 불명(②)이면 "B는 처리 결과를 확인하지 못해 다시 실행하지 않았다"다 | `SteerErrored`, 규칙 `NoAnswerForRefusedSteer`, 변이 SteerErrorSilent(안내 없이 덮음)·SteerErrorRequeue(큐로 돌림) |
| `/clear` 뒤에 B의 steer 오류가 늦게 옴 | 위와 같다. B는 T의 구성원이라 clear가 멈춘 T의 취소·실패 게시가 B를 덮는다. 되살아날 경로가 없다 | `Clear`, 규칙 `NoReviveAfterClear`. 변이 SteerErrorRequeue는 이 경로에서 B를 큐로 되살려 그 규칙을 깬다 |
| agy가 앞 응답만 주고 B를 큐잉(1.4) | 6.2-5와 같다. 결과가 같다 | AgyResponds가 가장 오래된 prompt부터 답함 |
| 앞 응답 `end_turn`을 턴 끝으로 잘못 처리 | 금지한다. 턴 끝은 마지막 열린 prompt의 응답이다 | 변이 SteerEarlyClose |
| B는 받았지만 답이 B를 반영하지 않음 | 프로토콜로는 감지할 수 없다. 게시 하나에 덮였으므로 유실은 아니다. 사람이 다시 말한다 [U13] | — |
| 취소, 기한 초과, EOF, 재시작 | 턴 하나의 취소 또는 실패 안내가 A와 B를 함께 덮는다. 다시 보내지 않는다 | Design(Crash·Boot, WatchdogKill, AgyDies) |
| 전송 전 actor가 죽음(자식 없음) | `NotSent` → 큐 | — |

#### 6.4.1 steer prompt 오류 분류(안내 문구 선택)

분류는 안내 문구를 고르는 데만 쓴다. 어느 쪽이든 큐로 돌리지 않고 재전송하지 않는다. 그래서 분류가 틀려도 중복이나 유실은 생기지 않는다. 문구만 틀린다. 모르는 오류는 ②다.

| 분류 | 응답 | 근거 |
|---|---|---|
| ① 실행 전 거절 확실("전달되지 않았다, 다시 보내 달라") | `-32700` Parse error, `-32600` Invalid request, `-32601` Method not found, `-32602` Invalid params(요청 해석·검증 단계), `-32000` Authentication required("before this operation can be performed"). 로컬에서 요청 줄을 쓰기 **시작하기 전에** 파이프가 닫혀 있었던 경우 | ACP schema `ErrorCode`(`agent-client-protocol-schema` 1.21.2 `src/v1/error.rs:145-200`). JSON-RPC 표준 코드의 의미 |
| ② 결과 불명("결과를 확인하지 못해 다시 실행하지 않았다") | `-32603` Internal error, `-32800` Request cancelled(정의가 "Execution of the method was aborted"라 일부 실행을 배제하지 않는다), `-32002` Resource not found(일반 코드라 실행 전 세션 조회 실패라는 증명이 안 된다), 그 밖의 모든 코드(`Other`), 응답 기한 초과, 요청 줄을 쓰기 시작한 뒤의 EOF·파이프 오류 | 같은 schema. T3도 일반 오류를 턴 실패로 처리한다(`packages/provider-acp/src/server/adapter.ts:9337-9386`). 오류로 답한 에이전트도 이미 시작한 도구는 끝낼 수 있다고 전제한다(`:9359-9365`). T3가 재전송하는 "Agent is already processing"(`:158-185`)은 다른 에이전트(pi-agent-core)의 전용 문구라 agy 근거로 쓰지 않는다 |

앞(첫) prompt의 오류는 분류와 상관없이 지금처럼 턴 실패 안내로 닫고 재전송하지 않는다(4.1절).

#### 6.4.2 걷어 낸 반환 설계(후속 후보 F4)

1c·1d는 실행 전 거절이 확실한 B를 큐 앞자리로 되돌리는 설계를 두었다. 그 구성은 내구 Returning 기록, steer 원장, 티켓 없는 `requeue_front` 반환, 부팅 복구, `/clear`의 반환 종료였다. 1d 델타 리뷰가 `/clear` 직후 늦은 거절로 입력이 되살아나는 경합을 찾았고, 코디 결정 i8339-scope-1123으로 이 경계 전체를 걷어 냈다.

이 설계가 맡던 비용은 거절된 드문 입력을 사람이 다시 보내는 것으로 바꾼다. 2b 운영 확인([U13])에서 steer 거절이 잦다고 나오면 후속 F4로 되살린다. 되살릴 때는 1d 델타 리뷰 3절의 최소 계약을 함께 넣는다. clear 시점의 Member 입력에 대한 내구 clear epoch로 늦은 Member→Returning 전이를 막아야 한다. 1d 설계는 `refs/review/i8339-design-1d`(`fe8797c4e7`)의 6.4.2·6.4.3에 남아 있다.

### 6.5 범위 밖

- HTTP iMessage의 실행 중 입력은 큐에 남긴다. #8274 1b의 원본별 custody 계약(`prepared.rs:75`, `queued.rs:248-287`)을 바꾸지 않기 위해서다. iMessage steering은 후속 F2로 둔다.
- 명령(`!stop` 등)과 `DISPATCH:` 접두는 `offerable`(`busy_inject.rs:236-244`)이 거른다. 그러나 `offerable`은 봇 여부를 보지 않는다. 그래서 `acp_steer::offer`가 사람 작성 여부(`author_is_bot == false`이고 허용된 자동화 작성자가 아님)를 직접 확인한다. 봇 입력은 steering하지 않고 지금 큐로 보낸다.

## 7. 인접 경로와 ACP 대응

| 경로 | 좌표 | ACP |
|---|---|---|
| `!stop`·API stop/cancel | `control.rs:670`, `turn_lifecycle.rs:98`, `queue_api.rs:209`, `kanban.rs:147` → CancelToken | 열린 prompt가 있으면 `session/cancel`을 보내고 `cancelled`를 기다린다. 15초를 넘기면 그 자식만 강제 종료한다(5.2-13). prompt를 쓰기 전이면 쓰기를 취소하고 입력을 큐로 돌린다. 단 2b에서 첫 입력 원본을 이미 저장했으면 큐로 돌리지 않고 취소 안내가 덮는다(6.2-3). 대기 중인 permission 요청에는 `cancelled`로 답한다. 자식은 남긴다 |
| Terminate(hard stop) | CancelToken | 기한 없이 곧바로 **그 destination 자식만** 강제 종료하고 취소 안내를 낸다(5.2-13: Unix 그룹 신호, Windows `taskkill /T /F`). AgentDesk와 다른 destination은 계속 돈다 |
| `/clear`·`!clear` | `control.rs:544,831`(큐 폐기, 턴 중단, provider 세션 삭제, agy는 Noop `:90`) | 기존 순서에 다음을 더한다. steering된 입력은 T의 구성원이라 T의 취소·실패 게시가 덮는다. clear 뒤 늦게 온 steer 오류도 같다(6.4). 열린 prompt가 있으면 자식을 강제 종료하고, 없으면 `session/close`(지원 시) 뒤 정상 종료한다(5.2-13). `session.json`을 지운다. 다음 입력은 `session/new`다 |
| 자동 재시도 세션 리셋, 호스트 복구 정리 | `retry_state.rs:184`, `host_recovery_guard.rs:94` | `session.json`을 지우고 자식을 정상 종료한다 |
| 모델 변경 | `model_picker.rs:192` | 다음 턴 첫 prompt 전, 유휴 세션에 `session/set_config_option{configId:"model"}`을 보낸다. 거절되면 그 입력은 실패로 한 번 게시한다 |
| 효과 수준 | — | `thought_level` 옵션이 있을 때만 쓴다(조각 3) |
| 재시작·부팅 | `restore_inflight.rs:139`, agy shadow | 5.2-10(steering된 입력도 inflight 원본으로 덮음), 5.2-12(Unix 옛 자식 그룹 정리, Windows는 기록 삭제) |
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
| 큐에서 복원된 입력 | 큐 기록의 작성자(생산 큐 저장·복원은 bot 플래그를 보존한다. `queued.rs:496,583`의 `author_is_bot:false`는 `#[cfg(test)]` fixture다) | intake | 큐 기록의 사람 증거를 `queued_submission`에서 ACP 판정까지 그대로 넘길 때만 [U8]. 출처 필드가 없는 옛 큐는 자격 없음 | 증거가 없으면 지금 경로 |

## 9. 턴 종료와 #8160

**stopReason만으로는 #8160이 사라지지 않는다.**

- #8160은 turn_bridge의 authoritative Done 처리에서 생긴다(`content_arms.rs:339-357`, 조건 `response_delivery.rs:116-127`). 이 처리가 rollover로 얼린 메시지가 있는데도 오프셋을 0으로 되돌린다. 그러면 단조 저장 거부(`inflight/store.rs:497`)와 권한 상실 정지(`stream_tick/runtime.rs:222`)로 이어진다.
- ACP 어댑터도 `Text` 델타와 `Done{누적 글}`을 내므로 같은 조건을 만든다. steering된 턴은 글이 더 길어지므로 조건을 더 자주 만족한다.
- ACP가 없애는 부분은 프로세스 종료를 기다리던 턴 끝 판정뿐이다. 이제 마지막 응답 하나가 끝을 확정한다.
- 남는 수리는 11-14 결정대로 #8274 writer가 흡수한다. ACP 조각은 같은 긴 답 fixture를 넣었을 때 `-p` 코덱과 ACP 어댑터가 turn_bridge에 같은 메시지열을 준다는 것만 시험한다(넓히지 않음).

## 10. 권한과 #8299

- **D1 = A(사용자 결정).** ACP 자식을 `agy acp --dangerously-skip-permissions`로 띄운다(`agy acp --help` 1.5.14에 있는 플래그). 그래도 오는 `session/request_permission`에는 `allow_once`를 고른다. 없으면 `allow_always`를 고른다(T3 `AntigravityAcpSupport.ts:156-220`, `adapter.ts:1626-1634`와 같은 순서). 사람이 관리하는 프로필 `settings.json`의 명시 규칙(특히 deny)은 agy가 계속 먼저 적용한다(1절 권한 규칙 행: 명시 규칙이 실행 모드보다 앞선다). AgentDesk는 그 파일을 쓰지 않는다.
- Claude·Codex 채널과 같은 권한 수준이다(#8299 A안과 같은 판단). 포럼 11-12 답변은 consumer 로그인에서 이 플래그를 harness 도구에 쓰는 것을 비준수로 적었다. 사용자는 D0·D1 결정에서 이를 감수했다. 이 근거는 1절에 남긴다.
- 위임형 채널의 로컬 API 호출(2.2절)은 별도 규칙 파일 없이 동작한다.
- `interaction_*` 질문은 권한이 아니라 선택지 질문이다(T3 `AntigravityProtocol.ts:68-73`). 조각 1·2에서는 `cancelled`로 답하고 안내를 남긴다. 조각 3에서는 Discord에 선택지로 보인다.
- 취소할 때 대기 중인 permission 요청에는 `{outcome:"cancelled"}`로 답한다(ACP 명세).
- #8299의 `-p` 변경은 지금 경로(남은 시스템 입력)에 그대로 적용한다. ACP 경로도 같은 플래그를 쓰므로 두 경로의 권한 수준이 같다.

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
| 권한과 `interaction_*` | `AntigravityAcpSupport.ts:156-220`, `AntigravityProtocol.ts:68-73` | D1 = A: 플래그와 `allow_once` 자동. `interaction_*`는 조각 3에서 Discord 선택지로 보인다 |
| prompt 오류 처리 | `adapter.ts:158-185`(다른 에이전트의 busy 재전송), `:9337-9386`(일반 오류는 턴 실패), `:9343-9349`(-32800을 삽입 전 취소로 보는 것은 자체 close 직후에만) | 일반 오류는 재전송하지 않는다. 6.4.1의 ① 목록만 반환한다 |
| fs read true, write false | `AntigravityAcpSupport.ts:131-139` | 둘 다 false |
| 모델은 `configOptions`와 `set_config_option` | `AntigravityAcpSupport.ts:238-327` | 그대로 |
| 편집 위치, 일회용 세션 정리 | `AntigravityEditLocation.ts`, `AntigravitySessionFiles.ts:117-168` | 버림 |
| 15분 무활동 감시 | `AntigravityAdapterV2.ts:300-301` | 그대로. steering 때 시계를 다시 맞춘다(`adapter.ts:9836-9846`) |
| 환경 정리 | `antigravityAuthSupport.ts:202-245` | 자동 업데이트 끔과 TMPDIR은 그대로. 가짜 `SSH_CONNECTION`은 쓰지 않는다 |

## 15. 기존 이슈 방향

- **#8274 계속.** ACP 어댑터는 스풀 생산자를 하나 더할 뿐이다. 3절 9~11행은 바뀌지 않는다. 예약 시점 해석(4.3절)만 리뷰 대상이다. steering 입력은 스풀 형식을 바꾸지 않고 턴 영수증의 원본 목록에 들어간다.
- **#8297 재정의.** 헤드리스 셀(O3~O5)은 보류를 유지한다. agy E2E는 가짜 ACP 에이전트 셀과 사람 확인으로 바꾼다(12절).
- **#8160 흡수 유지.** #8274 writer로 흡수한다(9절).
- **#8299 두 경로 모두 같은 권한 수준.** D1 = A로 ACP 경로도 `--dangerously-skip-permissions`와 `allow_once` 자동 응답을 쓴다.

이슈 댓글 초안은 레인 보고서에 있다.

## 16. 조각 계획

| 조각 | 범위 | 소유 파일(prod) | 추정 | 수용 시험 | 운영 확인 |
|---|---|---|---|---|---|
| 1. 판정과 ACP 핵심(dormant) | `agy_acp.enabled`(기본 꺼짐), `acp_selected`, 사람 출처 판정(8절, [U8]), JSON-RPC 전송, wire 타입, actor(spawn, initialize, new, prompt, cancel, EOF, 기한, incarnation fence), **`StreamChild` 기반 자식 수명과 `child.json`, 플랫폼별 종료, Unix 부팅 전 옛 그룹 정리**(5.2-1, 5.2-12, 5.2-13), 4.1절 매핑, 스풀 기록 | `agy_acp/{mod,rpc,wire,child,turn}.rs`, `stream_json_turn.rs`, `intake_turn.rs`(인자), `src/config` 한 곳, `services/mod.rs`, `process.rs`(가시성 2줄) | 10파일 / +1,050 | ① **자동 경로 불변:** 스위치가 꺼져 있으면 어떤 agy 턴도 ACP 준비를 부르지 않는다. 켜져 있어도 루틴, `provider_exec`, dispatch, 봇 작성(E2E Test-Announce), 원본 claim이 없는 HTTP 헤드리스, 위임 작업 입력은 `execute_streaming`/`execute_recorded`로 간다. 이것을 생산 진입점 `execute_request`에서 단언한다. ② 가짜 에이전트 fixture로 짧은 답, 긴 답, 빈 답, cancelled, EOF, 기한, 옛 incarnation 응답을 넣어 메시지열과 스풀을 확인한다. ③ 긴 답에서 `-p`와 ACP가 같은 메시지열을 낸다. ④ 공용 세션 칸이 그대로다(최초 입력·clear 직후의 None 포함, [U4]). ⑤ **그룹 격리:** 두 destination에 가짜 에이전트(손자 프로세스 하나씩)를 띄우고 한쪽에 hard stop을 하면, 그 자식과 손자만 죽고 시험 부모 프로세스와 다른 destination은 산다. 자식 전용 그룹을 끈 변이에서는 이 시험이 실패한다. 같은 시험의 `cfg(windows)` 판은 `taskkill /T /F` 경로로 손자까지 죽는지 본다(ci-nightly `full_windows`, windows-latest). ⑥ **Unix 부팅 정리:** `child.json`이 맞는 살아 있는 그룹만 정리하고, pid만 같고 시작 시각이 다르면 건드리지 않는다. 리더는 죽고 손자만 남은 그룹(리더 PID 없음, 그룹 `Live`)은 정리한 뒤 spawn한다. probe 오류는 보류와 실패 안내 1회로 끝나고 spawn하지 않는다. Windows 부팅은 기록만 지우고 신호를 보내지 않는다. | 없음 |
| 2a. 세션 수명 | `session.json`, resume/load/거절, 부팅 orphan 실패, clear·리셋·호스트 복구, 모델, 유휴 만료 | `agy_acp/{store,attach}.rs`, `control.rs`, `retry_state.rs`, `host_recovery_guard.rs`, `model_picker.rs`, `model_catalog.rs`, `recovery_engine/restore_inflight.rs` | 8파일 / +700 | LoadLeak, RerunSent, ClearKeeps를 각각 RED 재현에서 GREEN으로 만든다. 거절 resume은 new와 안내 1회, 목록 밖 모델은 실패 1회다. PG 시험은 `pg_throwaway.sh`로 돌린다. | 시험 채널에서 짧은 답, 긴 답, `!stop`, `/clear`, dcserver 재시작 뒤 이어 말하기를 1회씩 확인한다. [U1][U12]를 닫는다. |
| 2b. steering | **busy 입력 → steering**(`acp_steer::offer`, 사람 작성 확인), 첫 입력과 steer 입력 원본의 inflight write-ahead(6.2-3), steer 수락 봉인(6.2-3a), 완료 영수증 병합(6.2-3b), 표시 재시도(6.2-3c), 우회 경로 정리(6.2-3c 표: 자동 재시도, 비정상 `Drop`), steer 오류의 안내 문구(6.4, 6.4.1) | `agy_acp/steer.rs`, `agy_acp/mod.rs`(mod 한 줄), `agy_acp/turn.rs`(A 원본 저장, 안내 줄), `intake_gate/busy_inject/acp_steer.rs`, `busy_inject.rs`(mod 한 줄과 호출 한 곳), `turn_bridge/receipt_origins.rs`, `turn_bridge/turn_analytics.rs`(호출 몇 줄), `turn_bridge/mod.rs`(mod 한 줄과 seal 호출 한 줄), `receipt_settle.rs`, `receipt_settle/pending_mark.rs`, `turn_bridge/stream_loop/provider_recovery.rs`(호출 한 곳), `inflight/clear_store/abandon.rs` | 12파일 / +630 | SteerEarlyClose, SteerAfterClose, SteerErrorSilent, SteerErrorRequeue, ReceiptFromMemory, SealOff, LeaderOriginOff, MarkNoRetry를 각각 RED에서 GREEN으로 만든다. 가짜 ACP 에이전트로 다음을 본다. **① 역순(봉인):** B를 수락한 직후 저장 완료를 붙잡고 `!stop`을 넣어 bridge가 영수증 조회 직전까지 오게 한 뒤 저장을 푼다. 디스크 영수증 `consumed_origin_ids`에 A·B가 있고, 봉인 뒤 보낸 C는 T의 행에 없이 다음 턴으로 한 번 실행된다. seal 호출을 지운 뮤턴트는 B 누락으로 RED다. **② 실제 Discord intake:** fixture에 A 원본을 심지 않고 실제 Discord intake로 A를 시작하고 B를 steer한다. `!stop` 영수증과 재시작(부팅) 영수증 모두 A·B다. A 저장만 지운 뮤턴트는 두 시험 모두 A 누락으로 RED다. **③ 조회·표시 오류:** 영수증 조회와 표시 양쪽에 같은 읽기 오류를 넣으면 영수증 미기록, 행 보존(표시 없음)이다. admission 직전에 오류를 풀면 그 admission이 표시를 다시 하고 A·B를 기록한 뒤 다음 턴이 시작한다. 재표시를 지운 뮤턴트는 admission이 계속 밀려 RED다. **④ 경계:** B 저장 직후 stream tick 전에 `/clear`, 재시작에서도 A·B. 디스크 행이 다른 턴이면 합치지 않음. 병합 호출을 지운 뮤턴트는 `!stop`·`/clear` 시험이 RED. A 저장 직후 `!stop`이면 첫 prompt 0, 재큐 0, 취소 안내 1. **⑤ 우회 경로:** steer가 들어간 턴에 quota 오류를 넣으면 자동 재시도 0, 실패 안내 1, 영수증 A·B. B 저장 뒤 bridge task를 비정상 종료시키면 행이 표시된 채 남고 다음 admission이 A·B를 기록하며 재실행은 0. **⑥ 기존 항목:** steering 입력은 게시 0개 추가, ➕와 `consumed_origin_ids`, 재시작 뒤 catch-up 재실행 0. steer 오류(두 종류 각각)는 재전송 0, 큐 반환 0, 종류에 맞는 안내 1줄. steering 중 `/clear`를 하고 steer 오류를 늦게 넣으면 큐 반환 0, B를 덮는 게시는 T의 취소 게시 정확히 1. 같은 상황에서 clear 직후 재시작해도 재전송·재큐잉 0. write-ahead 저장이 실패하면 prompt 0, `NotSent` → 큐 1. 봇 작성 입력은 steering하지 않는다. | 시험 채널에서 **실행 중 입력(steer)**과 steer 오류를 프로토콜 로그로 확인한다. [U13]의 wire 순서와 거절 빈도를 기록한다. [U6][U7]을 닫은 뒤 `agy_acp.enabled`를 켠다. 오류 코드 분류 확정은 스위치 전 필수가 아니다. |
| 3. 상호작용과 진단 | `interaction_*` 중계, 업스트림 오류 판정, `thought_level`, 진단(destination별 자식, incarnation, 턴 밖 출력 수, steer 수, 거절 수, 결과 불명 수) | `agy_acp/{permission,outcome}.rs`, `agy_acp/turn.rs`, 진단 route 한 곳, `model_picker.rs` | 5파일 / +350 | `interaction_*` 기한 뒤 거절, 취소 때 `cancelled` 응답, 오류 step fixture | 시험 채널에서 선택지 질문 1회 |
| 합계 | | 파일 중복 포함 | 35파일 / +2,730 | | |

- 조각 1은 +1,050, 2a는 +700, 2b는 +630으로 각각 +1,200 정지선 아래다(추정). 1·2a 계약은 동결하고 구현을 먼저 시작한다.
- 2b +630 = 아래 steering 본체 +400, 영수증 병합 약 +50(1f), 이번 수리 약 +180의 합이다.
  - 영수증 병합 +50: 새 child `receipt_origins.rs`의 읽기·판정·병합·보류 분기 약 40줄, `turn_analytics.rs` 호출과 보류 연결 약 8줄, mod 한 줄.
  - 이번 수리 +180: A 원본 저장과 실패 분기(`turn.rs`) 약 25줄, actor의 봉인 처리와 저장 뒤 취소·봉인 재확인(`steer.rs`) 약 40줄, `receipt_origins::seal`과 재시도 전 확인 약 35줄, `turn_bridge/mod.rs` 호출 한 줄, 대기 표시 child `pending_mark.rs` 약 45줄, `receipt_settle.rs`의 `keep_unread`와 재시도 연결 약 20줄, `provider_recovery.rs` 호출 2줄, `abandon.rs` 보존 분기 약 8줄. 합계 약 176줄을 +180으로 적었다.
- steering 본체 +400의 근거: 가장 가까운 기존 코드인 `busy_inject.rs:221-453`(Discord 쪽 offer·attempt·settle과 핸드백)이 빈 줄과 주석을 뺀 코드 209줄이다(1e에서 측정). `acp_steer.rs`는 예약과 핸드백이 없어 그보다 작게 약 150줄로 잡았다. actor 쪽 `agy_acp/steer.rs`(열린 prompt 판정, write-ahead 저장, 전송, 오류 종류 기록)는 약 180줄, `turn.rs` 안내 줄은 약 30줄, mod·호출 줄은 약 10줄이다. 합계 약 +370이고 여유를 두어 +400으로 적었다. 1d의 +750에서 원장, 반환 결과 표, 부팅·clear 호출, 가시성 변경이 빠졌다. 1이 +1,150을 넘을 것 같으면 그룹 격리와 옛 그룹 정리(`child.rs`의 해당 부분)를 1b로 나눈다. 1b가 끝나기 전에는 스위치를 켜지 않는다.
- 스위치는 2b 운영 확인 뒤 켠다. 2a만 있을 때 실행 중 입력은 지금처럼 큐로 간다(`acp_steer::offer`가 없으므로 기존 `NotSent` 경로).
- 1,000줄이 넘는 파일(`intake_turn.rs`, `intake_gate.rs`, `provider_dispatch.rs`)에는 인자 전달이나 호출 한 줄만 더하거나 아예 손대지 않는다. 본체는 `agy_acp/`와 `intake_gate/busy_inject/acp_steer.rs`에 둔다. `acp_steer`는 `intake_gate.rs`(1,414줄)가 아니라 `busy_inject.rs`(547줄)의 자식으로 둔다. 그래야 giant에 `mod` 줄도 더하지 않는다.
- 추정 근거는 #8274 조각 실측(1/+340, 2/+465, 3/+560)과 SDK 조사의 LOC 추정이다. 구현 뒤 `pr_cap_prod.py`로 실측한다.

## 17. 상태 차트 결과 요약

- **규칙(안전 10개와 PROPERTY 1개):** 입력마다 게시 1개 이하(`AtMostOnePost`), 받은 입력은 결국 덮임(`EveryInputSettles`, PROPERTY), prompt 실행 1회(`NoDuplicateExecution`), 남의 응답으로 닫지 않음(`NoForeignResponse`), clear 뒤 옛 세션 금지(`NoPromptToClearedSession`), 게시에는 내구 종료 기록이 있음(`PostOnlyWithDisposition`), steering 입력 조건 2개(`AnswerCoversWholeSteer`, `NoAnswerForRefusedSteer`), 저장된 입력의 내구 소유자(`DurableOwner`), clear 뒤 되살아남 0(`NoReviveAfterClear`), 첫 입력과 steering 입력의 영수증 누락 0(`NoMissingOrigin`).
- **결과:** 설계 3구성(Design: 상주·resume·steering·steer 오류는 턴에 남기고 안내·첫 입력과 steer의 write-ahead·조회 전 봉인·완료 영수증 직전 원본 병합·조회 실패 뒤 표시 재시도·옛 자식 정리, DesignLoad, PerTurn)은 모든 규칙을 통과한다. 변이 13개와 합본 1개는 각각 `expected.tsv`의 규칙만 깬다. 17개 구성 모두 `scripts/ci/statecharts_tlc.py`에서 일치한다(보고서).
- **1e(B-noreturn)에서 바뀐 것:**
  - `SteerRejected`, `ReturnToQueue`, Returning 변수를 걷어 냈다. steer 오류는 두 종류 모두 `SteerErrored` 하나로 모았다. 입력은 턴에 남고, `Settle`이 실패 안내를 붙인다(`noticed`).
  - 반환 변이 3개(SteerRefuseDrop, SteerReturnVolatile, ClearReturnsOld)를 지웠다.
  - `NoAnswerForRefusedSteer`를 "실패한 steer를 덮는 게시는 실패 안내를 갖는다"로 개정했다. 새 변이 SteerErrorSilent(안내 없이 덮음)가 이를 깬다.
  - `NoReviveAfterClear`는 "clear 때 이미 보낸 입력은 다시 큐에 들어가지 않는다"로 넓혔다. 1d 델타 리뷰의 반례는 "clear 때 Member였던 입력이 늦게 거절되어 되살아남"이었다. 이것을 변이 SteerErrorRequeue(오류 난 steer를 큐로 돌림)가 8걸음으로 재현하고 이 규칙을 깬다. 두 prompt 열림 → `/clear` → steer 실패 → 큐로 돌아감이다. Design에서는 막힌다. 경계가 같은 이유로 RerunSent와 합본 Mutants도 이 규칙을 깬다.
  - SteerEarlyClose·SteerAfterClose는 이제 `AnswerCoversWholeSteer`와 `NoAnswerForRefusedSteer`를 깬다. 일찍 게시된 steer가 그 뒤 실패하면 안내가 빠진 게시가 된다.
- **1f에서 더한 것:** bridge 메모리와 디스크 write-ahead의 경계를 넣었다(`settlerView`, `StreamTick`, `receiptHas`). 새 규칙 `NoMissingOrigin`과 변이 ReceiptFromMemory를 더했다.
  - 변이의 최단 경로는 8걸음이다: 두 prompt 열림 → agy 종료(EOF) → 게시와 영수증(B 없음).
  - 취소 경로도 따로 확인했다: 두 prompt 열림 → `!stop` → `session/cancel` → 취소 기한 → 게시와 영수증(B 없음). 같은 모델에 검사용 불변식을 더해 rc=12로 확인했고, 이 불변식은 커밋하지 않았다.
  - Design에서는 증인 `NoMergeOnCancel`이 "취소된 턴의 영수증에 B가 병합으로만 들어감"이 실제로 일어남을 보인다. 재시작은 부팅이 디스크 행으로 정산하므로 병합 없이도 B가 들어간다.
- **1g에서 더한 것(2b 영수증·원본 계약):**
  - `SendPrompt`를 `Accept`·`Save`·`Wire`로 나눴다. 저장과 전송 사이의 재시작에 실제로 도달하고(증인 `NoCrashAfterSave`), 그 입력은 다시 쓰이지 않는다. 저장 전 재시작이면 입력은 큐로 돌아온다.
  - 첫 입력의 원본도 저장한다(`Durable`). 변이 LeaderOriginOff(첫 입력 원본을 저장하지 않음)는 8걸음으로 `NoMissingOrigin`을 깬다: 한 입력 → prompt → 답 → 영수증에 A 없음.
  - 봉인: `!stop` 때 bridge가 먼저 조회하는 `EarlyLookup`을 두었다. 변이 SealOff는 리뷰의 순서 그대로 14걸음으로 `NoMissingOrigin`을 깬다: A prompt 열림 → B 수락 → `!stop` → 조회 → B 저장 → 게시(B 없음). Design에서는 봉인이 B의 저장을 기다리고, 봉인 뒤 입력은 다음 턴이 된다. 수락 뒤 취소로 쓰이지 않은 steer가 ① 안내와 함께 덮이는 경로가 실제로 일어난다(증인 `NoUnsentSteer`).
  - 조회 실패: 행을 남기고(`held`) 표시 여부를 정하지 않는다. 다음 admission이 표시를 다시 하고 기록한다(`AdmitSettle`). 변이 MarkNoRetry는 표시 없는 행이 채널을 막아 `EveryInputSettles`를 깬다. 표시 없는 행을 admission이 기다리는 상태가 Design에서 실제로 일어난다(증인 `NoRetrySettle`).
  - ReceiptFromMemory의 최단 경로는 이제 첫 입력 하나로 8걸음이다. 첫 입력 원본도 디스크에만 있어 bridge 메모리 사본이 모르기 때문이다.
  - 상태 수를 줄이려고 stream tick은 memory 변이에서만 탐색한다(병합하면 tick이 영수증을 바꾸지 못한다). 조회 실패는 설계 3구성과 MarkNoRetry에서만 켠다(`ReadFails`).
- **모델 밖:** 플랫폼별 종료 호출(Unix 그룹 신호, Windows `taskkill /T`와 dcserver job). Windows는 job이 크래시 때 트리를 닫아 orphan이 생기지 않는다. 그래서 모델의 orphan 경로가 두 플랫폼을 함께 덮는다. 신호가 남의 프로세스에 닿지 않는지는 조각 1 프로세스 시험 ⑤⑥이 맡는다.
- **도달성:** Design에서 steering 응답, 실패한 steer의 턴 게시, clear 뒤 실패한 steer의 턴 게시, 취소된 턴의 영수증 병합, 저장과 전송 사이의 재시작, 쓰이지 않은 steer의 게시, 표시 없는 행을 기다리는 admission이 실제로 일어남을 증인 불변식 위반(rc=12)으로 확인했다.
- 상세는 [보고서](../../statecharts/agy-acp/agy-acp.report.md)에 있다.

## 18. 사용자 결정과 미확인 가정

- **D0: 닫힘.** 사용자 결정(agy-acp-1123)으로 "사람이 직접 모는 대화형 클라이언트"를 전제로 확정했다. 착수 전 필수 목록에서 뺐다.
- **D1: 닫힘, A.** 사용자 결정으로 `--dangerously-skip-permissions`와 `allow_once` 자동 응답을 쓴다(10절).
- **F1(후속 후보):** antigravity 채널로 들어오는 시스템 입력(위임 작업, dispatch, 봇)의 consumer 로그인 `-p` 실행 처분.
- **F2(후속 후보):** 실행 중 iMessage의 steering.
- **F3(후속 후보):** Windows에서 자식이 스스로 끝난 뒤 남은 손자 정리(자식별 job object). 지금 `-p` 경로와 같은 동작이라 이번에 넓히지 않는다. dcserver가 끝날 때는 런타임 job이 정리한다(5.2-13).
- **F5(후속 후보):** HTTP `human_input/origin_id`에 `dmsg:`·`dlg:` 같은 내부 접두를 거절하는 형식 검사(`src/services/agents/input_result.rs:15-35`). 지금도 인증된 호출자의 자유도이고, `dmsg:`로 조회하는 소비자는 없다(6.2-3d).
- **F6(후속 후보):** agy writer를 운영 부팅에 연결할 때 `Effect::ClearInflight`(`outbound/o_writer_io/agy.rs:99-119`) 앞에 `receipt_settle::record_or_stamp`를 둔다(6.2-3c 표).
- **F4(후속 후보):** 실행 전 거절된 steer의 자동 큐 반환. 2b 운영 확인에서 steer 거절이 잦으면 되살린다. 1d 설계(`fe8797c4e7`의 6.4.2·6.4.3)에 1d 델타 리뷰 3절의 clear epoch fence를 더해야 한다(6.4.2).
- **[U1]** ACP sessionId가 UUID인가. 일반 ACP는 형식을 정하지 않는다. 조각 1의 wire·스풀 경계를 고정하기 전에 실제 `session/new` 결과로 확인한다. 확인 전에는 UUID를 전제하지 않는 문자열로 다룬다.
- **[U2]** `usage_update`와 `StatusUpdate`의 필드 대응. 프로토콜 로그로 본다. U2·U3·U5·U10은 착수 차단 사유가 아니다. 정해진 fallback과 fixture로 진행하고 실측으로 보완한다.
- **[U3]** agy가 `max_tokens`·`max_turn_requests`를 실제로 쓰는가. 프로토콜 로그로 본다.
- **[U4]** 공용 세션 칸이 바이트 단위로 그대로인가. 기존 ID뿐 아니라 최초 입력과 clear 직후의 None도 보존해야 한다. 기존 Init 처리는 `Some(sid)`를 쓰므로 "요청 값을 그대로 돌려줌"의 표현을 조각 1에서 정하고 시험으로 닫는다.
- **[U5]** spawn 비용. 조각 2에서 측정한다.
- **[U6]** app-data-dir를 분리할지 공유할지. 기본값인 분리로 진행한다. 공유로 바꾸려면 조각 2a의 세션 store·attach 계약을 고정하기 전에 검증한다.
- **[U7]** `_meta.antigravity.systemPrompt.append` 지원 여부. 조각 1 fixture에 지원·미지원 분기를 넣고, 실제 capability는 스위치를 켜기 전에 initialize 로그로 본다.
- **[U8]** 큐에서 복원된 입력의 사람 증거. 조각 1의 자격 판정을 구현하기 전에 내구 증거와 전달 경로(큐 기록 → `queued_submission` → ACP 판정)를 정한다. 1b의 근거로 든 `queued.rs:496,583`은 `#[cfg(test)]` fixture라 근거가 아니다. 출처를 모르면 ACP 미자격이다.
- **[U9]** agy의 ACP v2 이행. initialize의 `protocolVersion`이 1이 아니면 거절하고 경보를 낸다. 조각 1 fixture에 비호환 분기를 넣는다.
- **[U10]** `agentInfo.version` 형식. steering을 버전으로 게이트하지 않으므로 기록만 한다.
- **[U11]** 대화 DB의 위치와 스키마. 조각 3 전에 읽기 전용으로 확인한다.
- **[U12]** 재시작 전 세션이 새 자식에서 resume되는가. 조각 2a의 resume 운영 확인에서 닫는다. 스위치를 켜기 전에 끝내야 한다.
- **[U13]** 실제 steering 응답 형태와 오류 형태. 공식 문서는 한 문장뿐이고, 형태는 T3 코드 주석에 근거한다. 조각 2b 운영 확인 때 프로토콜 로그로 앞 prompt 응답, 새 prompt 응답, 거절, 취소의 실제 wire 순서와 steer 거절 빈도를 기록한다. 이 기록은 스위치를 켜기 전에 한다. 거절이 잦으면 F4 판단에 쓴다. 오류 코드 분류(6.4.1)는 안내 문구 선택에만 쓰므로 스위치 전 필수가 아니다. 분류가 틀려도 문구만 틀리고 중복·유실은 없다. `-32002`는 ②로 두고, 실행 전 세션 조회에서만 나온다는 것이 로그로 보이면 ①로 올린다. 어느 응답 형태든 6.2-5 규칙으로 같은 결과가 나도록 설계했다. 답이 steer를 의미상 무시했는지 자동으로 감지하는 보장은 없다.
- **[U14]** `agy acp`가 stdin EOF에서 스스로 끝나는가. 끝나지 않아도 5.2-12의 그룹 정리가 처리하지만, 정상 종료 시간(2초 기다림)을 정하는 데 쓴다. 조각 1 운영 전 로컬 확인(prompt 없이 initialize 뒤 stdin 닫기)으로 본다.
