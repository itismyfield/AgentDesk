# #8339 사람이 대화하는 agy 채널을 `agy acp` 클라이언트로 옮기는 설계

저장소 기준은 `5021d25794`(origin/main)다. 확인한 agy CLI는 1.5.14(`agy --version`)다. 이 문서는 설계만 담고 Rust 코드는 바꾸지 않는다. 상태 차트는 [agy-acp 모델](../../statecharts/agy-acp/AgyAcpSession.tla)·[보고서](../../statecharts/agy-acp/agy-acp.report.md)다. 미확인 가정은 [U]로 표시하고 확인 방법을 적는다.

## 0. 결정 요약

| 쟁점 | 결정 |
|---|---|
| 범위 | 사람이 쓴 입력이 들어오는 **선택된 agy 채널**만 바꾼다. 선택 조건은 `agy_acp.channels` 목록, Discord 사람 작성 또는 인증된 iMessage 입력, dispatch 아님이다. 루틴·파이프라인 리뷰·dispatch·E2E 셀·`provider_exec` 단순 실행·봇 작성 입력은 지금의 `agy --print` 경로를 **그대로 둔다**. |
| 입구 | `stream_json_turn::execute_request`(`src/services/discord/router/message_handler/provider_dispatch/stream_json_turn.rs:181`)가 유일한 분기점이다. ACP 이벤트는 기존 `StreamMessage`와 기존 `AgySpool` 기록으로 바꾼다. 그 아래(turn_bridge·agy writer·스풀·외부 입력·shadow)는 그대로 둔다. |
| 프로세스 수명 | **채널 상주**. 채널마다 `agy acp` 자식 1개를 두고 턴 사이에 유지한다. 재부착은 `session/resume`(기록 재전송 없음)이고, `session/load`는 resume 미지원일 때만 쓰는 대체 경로다. 이때 load 응답 전 갱신은 모두 버린다. AgentDesk 재시작 때 자식은 같이 죽고, 다음 입력에서 새 자식을 띄워 resume한다. |
| 취소 | `!stop`·`/stop`은 `session/cancel`을 보낸 뒤 `stopReason:"cancelled"`를 기다린다. 기한(기본 15초) 안에 응답이 없으면 프로세스 그룹을 종료한다. 자식은 다음 입력에서 다시 띄운다. |
| 실행 중 입력 | 조각 1·2는 지금처럼 메일박스 FIFO에 쌓았다가 앞 프롬프트의 stopReason 뒤에 보낸다. steering(실행 중 `session/prompt`)은 조각 3에서 채널 설정으로 켠다. |
| 턴 종료 | 턴 끝은 `session/prompt` 응답의 stopReason으로 정한다. 프로세스 종료를 기다리지 않는다. #8160은 이것만으로 사라지지 않는다(7절). |
| 권한 | ACP 실행에는 `--dangerously-skip-permissions`를 붙이지 않는다. `session/request_permission`에 AgentDesk가 정책으로 답한다. 기본값은 사용자 결정 D1을 따른다(8절). |
| 모델 | ACP 채널의 모델은 그 세션이 알려 준 `configOptions`(`id:"model"`) 안에서만 고른다. 일반 외부 클라이언트에는 Gemini만 열려 있다. 목록 밖 모델이면 턴을 시작하지 않고 그 이유를 채널에 게시한다. `-p`로 조용히 되돌아가지 않는다. |
| SDK | 공식 crate `agent-client-protocol` 5.5.1을 쓰지 않고, 필요한 v1 부분만 tokio·serde_json으로 직접 구현한다(10절). |
| 조각 | 3개. 추정 합계는 prod 24파일 / +2,050줄이며 조각마다 상한 아래다(13절). |

## 1. 근거

| 근거 | 내용 | 출처 |
|---|---|---|
| agy ACP 문서 | 서버는 "ACP protocol version 1 over stdio"로 동작하고 session load·resume·list·close를 지원한다. "Resuming a session reopens the conversation without replaying its history." 또 "Sending a prompt while a turn runs steers that turn", "Cancelling a turn ends open tool calls and abandons pending permission requests". 규칙에 이미 정해진 행동은 permission 질문이 없다. | https://antigravity.google/docs/ide/extensions/acp (2026-11-23 조회, 날짜 표기 없음) |
| agy 모델 | "Gemini models are available in every client. Third-party models are only offered to JetBrains IDEs …, Zed, and Xcode." | 같은 문서 "Models and thinking level" 절 |
| 권한 규칙 파일 | `~/.gemini/antigravity-cli/settings.json`의 `permissions.allow/deny/ask`에 `action(target)` 항목을 둔다. 우선순위는 Deny > Ask > Allow이고, 명시 규칙이 실행 모드보다 앞선다. | https://antigravity.google/docs/permissions "Antigravity CLI" 절 |
| FAQ | "`agy -p` is intended for direct scripting and CI workflows. Wrapping `agy` inside third-party coding agents or applications … is not supported under consumer plans." | https://antigravity.google/docs/faq |
| 포럼 2026-10-26 | T3 Code에서 개인 구독으로 공식 ACP를 단일 사용자가 대화형으로 쓰는 것은 허용되며, 모델은 Gemini로 제한된다. | https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027 |
| 포럼 2026-11-09 | 개인 단일 사용자 대화형 클라이언트는 허용된다. 조건은 수정하지 않은 `agy acp` 바이너리가 자기 인증을 관리하는 것이다. 다른 에이전트 루프 안의 하위 에이전트나 무인 자동화는 허용되지 않는다. | https://discuss.ai.google.dev/t/personal-app-using-the-official-agy-acp-binary-with-google-ai-pro-permitted/198889 |
| 포럼 2026-11-12 | 서드파티 harness 안에서 `antigravity-acp`를 모델 공급자로 감싸는 것은 consumer `oauth-personal`에서 허용되지 않는다. "driving harness tools with `--dangerously-skip-permissions` are also non-compliant on consumer sign-in". | https://discuss.ai.google.dev/t/policy-question-omp-hosting-unmodified-antigravity-acp-for-one-user/199753 |
| ACP 명세 | `session/load`는 "MUST replay the entire conversation"이다. `session/resume`은 2026-04-22에 안정화됐고 "MUST NOT replay"다. `session/cancel`은 알림이며, 에이전트는 진행 중 prompt에 `cancelled`로 답해야 한다. stopReason은 `end_turn`·`max_tokens`·`max_turn_requests`·`refusal`·`cancelled`다. | https://agentclientprotocol.com/protocol/v1/session-setup, …/v1/prompt-turn, …/announcements/session-resume-stabilized |
| `agy acp --help`(1.5.14) | `--dangerously-skip-permissions`, `--process-per-session`, `--app-data-dir`(기본 `antigravity-cli`), `--gemini-dir`, `--log-file`, `--login`, `--sandbox`가 있다. `--settings`·`--model`은 **없다**. | 로컬 실행(프롬프트 전송 없음) |
| T3 Code | MIT, https://github.com/pingdotgg/t3code @ `c89ca0a2f2c1d996d9b461568860991febaca04f` | 11절 |
| ACP Rust SDK | `agent-client-protocol` 5.5.1(Apache-2.0, 2026-11-21) @ `34aea54f7314b42563b09112fe40b0cd5eb29b8a`, schema `=1.21.2` 고정 | 10절 |

포럼 답변 작성자가 Google 직원인지는 페이지에서 확인되지 않는다. 11-12 답변은 AgentDesk 같은 중계 서버를 "harness"로 읽을 여지가 있다. 이 판단은 설계가 아니라 사용자 결정 D0이다(15절).

## 2. agy 경로 진입점 전수표

좌표는 `5021d25794` 기준이다. "ACP 영향" 열에서 **분기**는 바뀌는 좌표, **유지**는 그대로 두는 좌표, **보강**은 ACP 채널을 위해 동작을 더하는 좌표다.

| # | 좌표 | 역할 | ACP 영향 |
|---:|---|---|---|
| 1 | `src/domain/provider/catalog/rows.rs:157-167`, `src/domain/provider/execution.rs:83`, `src/domain/provider/value_methods.rs:106` | `antigravity`/`agy` 별칭, StreamJsonCli(Agy) 어댑터, capability | 유지. ACP는 어댑터 종류를 바꾸지 않고 실행부 안에서 고른다. |
| 2 | `src/services/discord/router/message_handler/intake_turn.rs:1971,2080,2122` | foreground·HTTP 입력의 `AgyForeground` 생성, blocking 실행 | 유지. 사람 작성 여부를 `AgyForeground`에 실어 보낸다(조각 1, giant 파일은 인자 1개만 바꾼다). |
| 3 | `…/provider_dispatch/stream_json_turn.rs:51-79`(`from_claim`), `:81-122`(`from_http_claim`) | Discord·iMessage 원본 자격 | 유지. 입력에 사람 작성 플래그를 더한다. |
| 4 | `…/provider_dispatch/stream_json_turn.rs:181-194`(`execute_request`), `:196-211`(`request`) | 스풀 예약 뒤 `agy::execute_recorded`, 아니면 `execute_streaming` | **분기**. `acp_selected`가 참이면 `agy_acp::run_turn`을 쓴다. 거짓이면 지금 코드 그대로다. |
| 5 | `…/provider_dispatch.rs:305-317,531-533` | agy iMessage 턴의 경로 고정, StreamJsonCli 분기 | 유지. |
| 6 | `src/services/stream_json_cli/dialects/agy.rs:18-38,52-89,90-122` | `-p` argv(`--dangerously-skip-permissions`, `--model`, `--conversation`), 금지 플래그, spawn 준비 | 유지(자동 용도). ACP 경로는 이 argv를 쓰지 않는다. |
| 7 | `src/services/stream_json_cli/runner.rs:59-86,127-131,166-240` | 기록 실행, child pid 등록·취소 watchdog, 시작·무출력 기한 | 유지. ACP는 자기 cancel 감시를 둔다(5절). |
| 8 | `src/services/stream_json_cli/codec.rs:115-360`(`AgyCodec`), `:299-360` `finish` | init·delta·result, 프로세스 종료 뒤 Done | 유지. ACP 턴 끝은 stopReason이다. |
| 9 | `src/services/stream_json_cli/runner/agy_recording.rs:24-68` | `Init`→`spool.init`, `Text`→`spool.delta`, `done`, Drop 시 `fail` | 보강. ACP 어댑터가 같은 `Recording` 규약으로 스풀에 쓴다. |
| 10 | `src/services/tui_o/store/agy_spool.rs:82-94,154-195` | 예약(`reserve`/`reserve_imessage`), `init`/`delta`/`done`/`fail` | 유지. `resume` 값은 attach 결과 세션 ID로 채운다(3.3절). |
| 11 | `src/services/tui_o/store/agy_spool/replay.rs:124,365` | 스풀 재생, conversation 일치 검사 | 유지. |
| 12 | `src/services/tui_o/writer/host/agy_host.rs:45,72,121,159`, `src/services/tui_o/writer/agy_delivery/mod.rs:87,98`, `src/services/tui_o/store/agy_delivery.rs:76,159` | agy shadow custody·전달 journal | 유지. |
| 13 | `src/services/discord/outbound/o_writer_io/agy.rs:20,40,93` | 상관 토큰, POST, effect 적용 | 유지. |
| 14 | `src/services/tui_o/channel_policy/agy.rs:40,44,66`, `src/services/tui_o/cutover.rs:181,210` | agy writer 선택·스레드 기록 | 유지. |
| 15 | `src/services/discord/runtime_bootstrap/queued_recovery/agy_shadow.rs:11`, `src/services/discord/runtime_bootstrap/o_writer_host.rs:549` | 부팅 shadow 관측, writer host 설치 | 유지. |
| 16 | `src/services/imsg_reply/agy_input.rs:52,64,142`, `src/server/routes/agents_turn_deliver.rs:255`, `src/server/routes/agents_turn_deliver/agy_input.rs:28` | iMessage 원본 custody·전달 | 유지. 자격을 갖춘 iMessage는 사람 입력으로 분류한다(6절). |
| 17 | `src/services/discord/health/turn_deliver.rs:322-355`, `src/services/discord/health/turn_deliver/prepared.rs:75` | HTTP 입력 예약, agy 선택 채널의 busy inject 금지 | 유지(조각 1·2). 조각 3에서 steering을 켜면 보강한다. |
| 18 | `src/services/discord/router/intake_dispatch/queued.rs:102,248,407` | iMessage 큐 custody 복원 | 유지. |
| 19 | `src/services/discord/router/message_handler/headless_turn/replay_start.rs:72-87` | HTTP 헤드리스 시작의 agy claim | 유지. |
| 20 | `src/services/discord/recovery_engine/restore_inflight.rs:139`, `…/restore_inflight/replay_hold.rs:5` | iMessage 원본 행 보류 | 유지. |
| 21 | `src/services/discord/commands/control.rs:76-97`(clear/reset 동작 = Noop), `:544`(`clear_channel_session_state_with_session_key`), `:670`(`cmd_stop`), `:831`(`cmd_clear`) | clear·stop | **보강**. ACP 채널의 clear는 자식을 닫고 저장 세션을 지운다(5절). |
| 22 | `src/services/discord/turn_bridge/tmux_runtime/interrupt_policy.rs:62,132` | agy 인터럽트 계획 없음 | 유지. ACP 취소는 CancelToken 감시로 처리한다. |
| 23 | `src/services/turn_lifecycle.rs:98`, `src/server/routes/queue_api.rs:209`, `src/server/routes/kanban.rs:147`, `src/services/discord/turn_bridge/retry_state.rs:184` | API stop/cancel, 자동 재시도 세션 리셋 | 유지(CancelToken 경유). retry 리셋은 ACP 저장 세션도 지운다(조각 2). |
| 24 | `src/services/discord/commands/model_picker.rs:192`, `src/services/discord/model_catalog.rs:592` | 모델 선택·목록("default + custom model id from agy models") | **보강**(조각 2·3). ACP 채널은 세션 `configOptions` 목록을 쓴다. |
| 25 | `src/services/discord/turn_bridge/stream_loop/content_arms.rs:339-357`, `src/services/discord/turn_bridge/response_delivery.rs:116-127`, `src/services/discord/inflight/store.rs:497`, `src/services/discord/turn_bridge/stream_tick/runtime.rs:222` | #8160 Done 오프셋 0 대입·역행 저장 거부·권한 상실 정지 | 유지(7절). |
| 26 | `src/services/routines/agent_executor/launch.rs:6,37-72` | 루틴 헤드리스 시작 | 유지(자동 용도). |
| 27 | `src/services/provider_exec.rs:102-121` | 단순 실행 `execute_streaming` | 유지(자동 용도). |
| 28 | `src/config/test_instance/agy_cell.rs:42,58,124`, `scripts/e2e/agy_cell/E-65.yaml` | E2E 셀 프로필·로그인 홈. Test-Announce 봇 평문 입력 | 유지(자동 용도). 봇 작성 입력이라 ACP 자격이 없다. |
| 29 | `src/services/provider/registry.rs:180`, `src/services/provider_auth_profile.rs:212,308` | 바이너리 경로, 인증 overlay | 유지. ACP 자식도 같은 overlay를 받는다. |

## 3. 입구 변환 경계

### 3.1 매핑

ACP 어댑터(`agy_acp::turn`)는 한 턴 동안 기존 runner와 같은 두 출구로만 내보낸다. 하나는 `Sender<StreamMessage>`(turn_bridge)이고, 다른 하나는 예약된 `AgySpool`(writer)이다. 아래쪽은 이 둘이 `-p`에서 왔는지 ACP에서 왔는지 모른다.

| ACP 입력 | 내부 이벤트 | 스풀 | 비고 |
|---|---|---|---|
| attach 결과(`session/new`·`session/resume` 응답의 sessionId) | `StreamMessage::Init{session_id, raw_session_id}`(3.3절 값) | `init(uuid)` | sessionId가 UUID가 아니면 실패로 닫는다 [U1]. |
| `session/update` `agent_message_chunk`(text) | `Text{content}` | `delta(text)` | 열린 프롬프트가 없으면 버리고 계수만 남긴다(3.2절). |
| `agent_thought_chunk` | `Thinking{summary: None}` | 없음 | 원문은 기록하지 않는다(현 `Thinking` 계약). |
| `tool_call` | `ToolUse{name: title·kind, input: 요약, tool_use_id}` | 없음 | 크기를 제한한다(T3 `AntigravityProtocol.ts:379-409` 참고). |
| `tool_call_update`(completed/failed) | `ToolResult{content: 요약, is_error, tool_use_id}` | 없음 | 상태가 바뀔 때만 보낸다. |
| `plan`, `available_commands_update`, `current_mode_update`, `config_option_update`, `session_info_update` | 없음(진단 로그) | 없음 | 모델 목록은 `config_option_update`로 캐시를 갱신한다. |
| `usage_update` | `StatusUpdate{…}`(알려진 필드만) | 없음 | 필드 대응은 조각 1 fixture로 정한다 [U2]. |
| `session/request_permission` | 없음(정책 응답, 8절) | 없음 | `toolCallId`가 `interaction_`으로 시작하면 질문이다. 조각 1·2는 거절하고 그 사실을 본문 뒤 안내로 남긴다. 조각 3은 Discord로 중계한다. |
| `fs/*`, `terminal/*` 요청 | 없음 | 없음 | capability를 `false`로 광고하고 -32601로 답한다. |
| `session/prompt` 응답 `end_turn` | `Done{result: 누적 text, session_id: 3.3절}` | `done(uuid, body)` | 누적 text가 비면 실패로 닫는다(현 `AgyCodec` 빈 성공 규칙 `codec.rs:429`와 같다). |
| 응답 `cancelled` | `Error{message: "cancelled"}`(현 cancel epilogue가 쓰는 형태) | `fail(Cancelled)` | 기존 runner의 `Failure::Cancelled`(`runner.rs:80-82`)와 같다. |
| 응답 `max_tokens`·`max_turn_requests` | 누적 text가 있으면 `Done`과 잘림 안내, 없으면 `Error` | `done`/`fail` | [U3] agy가 실제로 쓰는지 조각 1 fixture로 확인한다. |
| 응답 `refusal` | `Error{message: "agy quota or refusal"}` | `fail` | T3 `AntigravityTurnOutcome.ts:143-158`은 agy가 쿼터 소진을 refusal로 보낸다고 적는다. |
| JSON-RPC error(-32000 Authentication required 등) | `Error` | `fail(Spawn)` | 인증 안내를 게시한다. 브라우저 로그인 URL은 게시하지 않는다(4.2절 4항). |
| stdout EOF·비정상 종료 | 열린 프롬프트마다 `Error` | `fail` | 모델 `AgyDies`. |
| 무활동·취소 기한 초과 | 열린 프롬프트마다 `Error`(취소 중이면 cancelled) | `fail` | 모델 `WatchdogKill`. |

`end_turn`이지만 본문이 업스트림 오류 문장인 경우가 있다. T3는 대화 DB의 마지막 step(type 17)을 읽어 이를 가린다(`AntigravityTurnOutcome.ts:88-245`). AgentDesk는 조각 3에서 같은 판정을 읽기 전용으로 붙인다. 조각 1·2에서는 그 문장이 답으로 게시된다. 지금 `-p` 경로도 같으므로 퇴행은 아니다.

### 3.2 턴 밖 갱신

agy는 백그라운드 명령이나 `schedule` 도구 때문에 prompt 응답 뒤에도 스스로 답을 흘릴 수 있다(T3 `AntigravityProtocol.ts:49-66`). 이런 갱신을 소유할 입력이 없으므로 다음 입력의 답에 섞지 않는다. 열린 프롬프트가 없을 때 온 `agent_message_chunk`는 버리고 채널별 계수와 로그만 남긴다. 지금 `-p` 경로에서는 프로세스가 끝나 이런 출력 자체가 없으므로 같은 수준이다. 이를 게시하는 일은 후속 후보로 둔다.

### 3.3 세션 ID 저장

ACP 세션 ID는 기존 채널 provider 세션 칸(`inflight_state.session_id`·세션 행)에 **넣지 않는다**. 같은 채널로 `-p` 자동 실행이 들어올 때 그 ID를 `--conversation`으로 쓰거나, stale resume 무효화로 지우면 ACP 세션 연속성이 깨진다. 그래서 ACP 모듈이 `<runtime_root>/agy_acp/<channel>/session.json`(소유자 전용 권한, 원자적 교체, symlink 거부, agy 스풀과 같은 규칙)에 `session_id`·`app_data_dir`·`cwd`·`agy_version`·`created_at`을 둔다. turn_bridge로는 `Init`·`Done`의 `session_id`에 요청이 가져온 기존 값을 그대로 돌려준다. 이렇게 하면 공용 칸이 바이트 단위로 그대로다. 조각 1에서 시험으로 확인한다 [U4].

스풀 예약의 `resume`(`stream_json_turn.rs:148-154`)에는 attach가 끝난 **실제** 세션 ID를 넣는다. 그래서 스풀 예약은 attach 뒤, `session/prompt` 앞에 한다. #8274 설계 2절의 "durable reservation 전에는 spawn하지 않는다"는 상주 모델에서 "durable reservation 전에는 `session/prompt`를 쓰지 않는다"로 읽는다. 이 바뀐 읽기를 [설계 이탈]로 표시해 리뷰에 올린다. 자식 spawn은 턴과 무관하고, 턴의 부작용은 prompt 쓰기부터 생기기 때문이다.

### 3.4 바뀌는 파일과 그대로 두는 파일

- 새 파일: `src/services/agy_acp/{mod.rs, rpc.rs, wire.rs, child.rs, turn.rs, store.rs, attach.rs}`(본체는 모두 child 모듈에 둔다).
- 바뀌는 파일: `…/provider_dispatch/stream_json_turn.rs`(분기·사람 플래그), `…/intake_turn.rs`(인자 1개), `src/config` 안 runtime 설정 1곳(`agy_acp.channels`·기한), `src/services/mod.rs`(mod 1줄), `src/services/discord/commands/control.rs`(ACP 채널 clear/reset), `…/turn_bridge/retry_state.rs`(리셋 시 ACP 세션 삭제), `src/services/discord/commands/model_picker.rs`·`src/services/discord/model_catalog.rs`(모델 목록), 조각 3의 `…/health/turn_deliver/prepared.rs`·intake busy 경로(steering).
- 그대로 두는 파일: 2절 1·5~20·22·25~29행 전부. 특히 `stream_json_cli/**`, `tui_o/**`, `imsg_reply/**`, `outbound/o_writer_io/**`, `runtime_bootstrap/**`, routines, `provider_exec.rs`, `config/test_instance/**`가 그렇다.

## 4. 프로세스 수명

### 4.1 비교

| | 턴마다 `agy acp` | 채널 상주 `agy acp` |
|---|---|---|
| 흐름 | 입력 → spawn → initialize → (authenticate) → `session/resume` → prompt → stopReason → 종료 | 첫 입력 → spawn → initialize → resume/new → prompt…(턴 사이 유지) → 죽음·clear·유휴 만료·재시작 때 다시 띄운다 |
| 비용 | 매 턴 기동(T3 메모로는 실행마다 약 1GB 런타임 압축 해제[U5])과 initialize/resume 왕복. 턴 지연이 늘어난다. | 채널마다 상주 메모리. 유휴 만료(기본 30분, T3 `ProviderSessionManager.ts:81-82`)로 줄인다. |
| 위험 | `-p`와 같은 "한 턴 한 프로세스"라 반환 경계는 단순하다. steering을 쓸 수 없고, 사용 방식이 `-p` 자동 루프와 같아 보인다. | 자식 incarnation 사이의 늦은 응답(모델 NoFence), clear 뒤 옛 세션(ClearKeeps), 취소 무응답(NoWatchdog), steering 응답 유실(SteerDrop)을 막아야 한다. |
| 모델 결과 | PerTurn GREEN | Design·DesignSteer·DesignLoad GREEN, 변이 6개 RED |

**선택: 채널 상주.** ACP 문서와 포럼이 허용한 형태는 "사람이 모는 대화형 클라이언트"다. 그 클라이언트처럼 세션을 열어 두고 사람의 입력마다 prompt를 보내는 쪽이 사용 형태에 가깝다. steering과 cancel(세션 유지)은 상주 세션에서만 의미가 있다. 매 턴 기동 비용도 사라진다. 상주의 추가 위험 네 가지는 모델 변이로 드러났고, 각각의 처방을 아래 규칙으로 고정한다.

### 4.2 상주 규칙

1. **자식 소유:** 채널당 하나의 actor가 자식 하나와 그 세션 하나를 소유한다. 채널을 넘는 공유와 `--process-per-session`은 쓰지 않는다(HTTP MCP가 필요할 때만 쓰는 옵션이다). 자식은 AgentDesk의 process group 안에 두어 재시작 때 같이 죽게 한다. 다시 붙을 stdio가 없으므로 살아남은 자식은 쓸모가 없다(T3도 같다).
2. **spawn:** `agy acp --app-data-dir <D> [--gemini-dir <P>]`를 cwd = 채널 작업 폴더, env = 기존 auth overlay(`provider_auth_profile`)와 `AGY_CLI_DISABLE_AUTO_UPDATE=1`로 띄운다. `--dangerously-skip-permissions`는 넣지 않는다. 사설 `TMPDIR`을 채널 actor가 소유하고 종료 때 지운다. `<D>`는 4.4절 [U6]에서 정한다.
3. **initialize:** `protocolVersion: 1`을 쓰고 `clientInfo{name:"agentdesk", version}`을 **정직하게** 보낸다. 다른 클라이언트 흉내(T3의 구버전 `zed` 위장, `AntigravityAcpSupport.ts:106-112`)는 금지한다. clientCapabilities는 fs·terminal을 모두 false로 둔다. 응답의 `agentCapabilities.loadSession`·`sessionCapabilities.resume`·`agentInfo.version`을 actor에 기록한다.
4. **authenticate:** 하지 않는다. `agy acp`가 캐시된 자격을 쓰게 두고(포럼 11-09 조건), -32000이 오면 인증 필요 안내로 실패한다. AgentDesk는 로그인 URL을 중계하지 않는다. 비대화형 stdout의 인증 URL 줄(T3 `antigravityAuthSupport.ts:646-710`)은 JSON이 아니므로 버리고, 안내만 게시한다. 로그인은 사람이 터미널에서 `agy` 또는 `agy acp --login`으로 한다.
5. **attach:** 저장 세션이 없으면 `session/new{cwd, mcpServers: []}`를 보낸다. 저장 세션이 있고 `resume` capability가 있으면 `session/resume{sessionId, cwd, mcpServers: []}`를 보낸다. resume이 없고 `loadSession`만 있으면 `session/load`를 보내되, 응답 전 갱신은 모두 버린다(모델 DesignLoad, 변이 LoadLeak). resume·load가 거절되면 `session/new`를 만들고, 그 턴의 답 앞에 "이전 대화 맥락을 이어받지 못했다"는 한 줄을 붙인다(모델 `ReattachRefused`). 입력은 잃지 않는다.
6. **prompt:** 스풀 예약과 inflight 저장(= 모델의 write-ahead sent mark)이 끝난 뒤에만 `session/prompt`를 쓴다. prompt 본문은 4.3절 시스템 프롬프트 규칙을 따른다.
7. **응답 대응:** 대기 맵은 `(incarnation, request id) → 입력`이다. 리더 task는 자기 incarnation 번호를 갖고, 현재 incarnation이 아닌 줄은 버린다(변이 NoFence). steering 중에도 앞 요청을 맵에서 지우지 않고 그 요청의 응답이 와야 정리한다(변이 SteerDrop).
8. **기한:** 무활동 기한(기본 15분)과 취소 기한(기본 15초)이 지나면 process group을 종료하고, 열린 프롬프트를 실패 또는 취소로 닫는다. 다음 입력이 새 자식으로 resume한다(변이 NoWatchdog).
9. **유휴 만료:** 열린 프롬프트가 없이 30분이 지나면 stdin을 닫고, 2초 뒤 SIGTERM, 다시 2초 뒤 SIGKILL을 보낸다. 저장 세션은 남긴다.
10. **재시작·부팅:** 부팅은 자식을 띄우지 않는다(대기 입력이 있을 때 첫 입력이 띄운다). sent mark가 있고 종료 기록이 없는 입력은 실패 안내로 닫는다. 지금의 #8274 §3.2 규칙과 같고, 자동 재전송은 하지 않는다(변이 RerunSent). 종료 기록은 있고 게시가 안 된 입력은 writer가 이어서 게시한다.
11. **incarnation:** 자식마다 단조 증가하는 번호를 actor 메모리에 둔다. 재시작 뒤에는 새 프로세스라 옛 리더가 없으므로 이 번호를 내구화할 필요가 없다.

### 4.3 시스템 프롬프트

`-p`는 매 턴 `SYSTEM_LEN=…` 봉투에 시스템 프롬프트를 싣는다(`dialects/agy.rs:124-127`). 상주 세션에서 매 턴 보내면 기록이 쌓인다. initialize가 `_meta.antigravity.systemPrompt.append`를 광고하면 `session/new`·`resume` 때 한 번 보낸다(T3 `AntigravityAcpSupport.ts:86-104`). 광고하지 않으면 세션의 첫 prompt와, 시스템 프롬프트 해시가 바뀐 뒤 첫 prompt에만 봉투를 붙인다 [U7].

### 4.4 상태 저장소 분리

`agy acp`의 기본 app-data-dir는 `antigravity-cli`(help)로 `-p`와 같다. 분리해서 `antigravity-acp`를 쓰면 자동 용도의 `-p` 대화와 ACP 세션이 섞이지 않는다. 대신 채널의 기존 `-p` 대화 맥락은 넘어오지 않는다(전환 첫 턴은 새 세션이고 안내 한 줄을 붙인다). 공유하면 기존 conversation ID를 resume할 수 있을지 모른다. 결정은 조각 2 운영 확인으로 미룬다 [U6]. 기본값은 분리다.

## 5. 인접 경로 전수와 ACP 대응

| 경로 | 좌표 | 지금 agy | ACP 채널 |
|---|---|---|---|
| `!stop`·`/stop` | `commands/control.rs:670` → CancelToken | `runner.rs:127-131`이 child를 종료한다 | CancelToken 감시 → `session/cancel{sessionId}`(알림). prompt를 쓰기 전이면 쓰기를 취소하고 큐로 되돌린다. 쓴 뒤면 응답을 기다린다. `cancelled`면 취소 안내, `end_turn`이 경합해 오면 답 게시, 기한 초과면 group 종료 후 취소 안내. 대기 중인 permission 요청에는 `{outcome:"cancelled"}`로 답한다(ACP 명세). 자식은 남는다. |
| API stop/cancel | `turn_lifecycle.rs:98`, `queue_api.rs:209`, `kanban.rs:147` | CancelToken | 위와 같다(CancelToken 하나로 모인다). |
| Terminate(턴 강제 종료·hard stop) | `interrupt_policy.rs:132`(agy 없음), CancelToken | child 종료 | 취소 기한 없이 곧바로 group 종료와 취소 안내. 저장 세션은 남긴다. |
| `/clear`·`!clear` | `control.rs:544,831`, `turn_orchestrator/clear_channel.rs` | 큐 폐기·턴 중단·provider 세션 삭제, agy 프로세스 리셋은 Noop(`control.rs:90`) | 같은 순서에 ACP 단계를 더한다. 턴이 있으면 위 stop을 하고, 자식을 닫고(`session/close`가 있으면 그것, 아니면 group 종료), `session.json`을 지운다(변이 ClearKeeps). 다음 입력은 `session/new`다. |
| 세션 리셋(자동 재시도) | `turn_bridge/retry_state.rs:184` | provider 세션 삭제 | `session.json` 삭제와 자식 detach. |
| 호스트 복구 세션 정리 | `inflight/host_recovery_guard.rs:94` | provider 세션 정리 | 같은 정리 함수를 부른다. |
| 모델 변경 | `commands/model_picker.rs:192` | 다음 턴 `--model` | 다음 prompt 전 유휴 세션에서 `session/set_config_option{configId:"model", value}`를 보낸다. 거절되면 턴을 시작하지 않고 안내한다. 조각 2. |
| 효과 수준(effort) | `request.rs:21`(agy는 None) | 쓰지 않음 | `category:"thought_level"` 옵션이 있을 때만 쓴다. 조각 3. |
| AgentDesk 재시작·부팅 | `recovery_engine/restore_inflight.rs:139`, agy shadow 부팅 | 진행 중 child는 같이 죽고, 스풀 미완은 실패 | 같다. 4.2절 10항. |
| 자식 자체 종료 | — | 해당 없음(턴마다 종료) | EOF가 열린 프롬프트를 실패로 닫는다. 다음 입력이 respawn·resume한다. |

## 6. 입력 출처 분류

| 출처 | 작성자 | 소유자 | ACP 자격 | ACP 전송 |
|---|---|---|---|---|
| Discord 채널 메시지 | 사람(`author_is_bot=false`, `intake_gate.rs:906`) | intake 메일박스 → `intake_turn` | 있다(채널이 목록에 있을 때) | 큐 선두가 되면 `session/prompt`(텍스트와 첨부 실체화 텍스트) |
| Discord 봇 메시지(E2E Test-Announce, 다른 에이전트) | 봇 | 같은 intake | 없다 | 지금 `-p` |
| dispatch·파이프라인 리뷰 턴 | 시스템 | dispatch(`registration_origin`) | 없다 | 지금 `-p` |
| 인증된 iMessage(`/turn/deliver`) | 사람(인증된 원본 GUID) | `imsg_reply::agy_input` custody | 있다 | 같은 세션 `session/prompt` |
| 실행 중 들어온 사람 입력 | 사람 | 메일박스 큐(조각 1·2) | 있다 | 앞 stopReason 뒤 `session/prompt`. 조각 3에서 채널 설정이 켜져 있으면 실행 중 `session/prompt`(steer). 앞 요청은 `end_turn`으로 닫혀 그때까지 받은 글로 게시된다. 그 글이 비면 "다음 답으로 이어 받았다"는 안내 한 줄이다. 뒤 입력이 이어지는 답을 소유한다. |
| 실행 중 들어온 iMessage | 사람 | `turn_deliver/prepared.rs:75`(agy 선택 채널은 inject 금지) | 있다 | 조각 1·2는 큐. 조각 3도 HTTP는 큐로 둔다(원본 custody 계약을 바꾸지 않는다). |
| 루틴(`agent` action) | 시스템 | `routines/agent_executor` | 없다 | 지금 `-p` |
| `provider_exec` 단순 실행 | 시스템 | 호출자 | 없다 | 지금 `-p` |
| 복구·catch-up 재전달 | 원 입력의 작성자 | recovery engine | 원 입력과 같다 | 같은 입력 키로 큐에 넣는다. 이미 prompt를 쓴 입력은 실패 안내다(4.2절 10항). |
| 큐 재구성 입력(`queued.rs:496`은 `author_is_bot:false`로 채운다) | 미상 | intake | 원본 작성자 증거가 내구로 있을 때만 있다 [U8] | 증거가 없으면 `-p` |

자격 판정은 다음과 같다. `acp_selected = agy_acp.channels ∋ destination ∧ provider = Antigravity ∧ AgyForeground 있음 ∧ 사람 작성 증거 ∧ dispatch 아님`. 이 판정은 `execute_request` 첫 줄에서 IO 없이 한다. 목록이 비어 있으면(기본값) 어떤 준비도 하지 않고 지금 코드로 간다(dormant).

## 7. 턴 종료와 #8160

**판정: stopReason만으로는 #8160이 사라지지 않는다.** #8160의 원인은 turn_bridge가 authoritative Done을 받을 때 rollover가 얼린 메시지가 있는데도 오프셋을 0으로 되돌리는 것이다(`content_arms.rs:339-357`). 조건은 `response_delivery.rs:116-127`이다(오프셋 > 0, 스트리밍한 글 있음, 여러 메시지, `full_response == result`). 그 저장을 inflight 단조 규칙이 거부하고(`inflight/store.rs:497`), stream tick이 권한 상실로 멈춘다(`stream_tick/runtime.rs:222`). ACP 어댑터도 `Text` 델타를 흘리고 `Done{result: 누적 글}`을 보내므로 같은 조건을 그대로 만든다. 턴 끝 신호가 프로세스 종료에서 stopReason으로 바뀌어도 이 경로는 같다.

ACP가 없애는 것은 "agy 프로세스는 끝났는데…"의 종료 판정 부분뿐이다. 끝은 응답 한 줄로 확정되고 EOF 경합이 사라진다. 남는 수리는 그대로 남는다. 11-14 사용자 결정대로 #8274 writer 경로(Done 뒤 첫 게시, 영수증 동결)가 #8160 fixture를 수용 시험으로 흡수한다. ACP 채널은 같은 스풀을 쓰므로 writer가 활성화되면 함께 해소된다. ACP 조각은 #8160을 넓히지 않는다는 것만 시험한다. 같은 긴 답 fixture를 `-p`와 ACP 어댑터에 넣었을 때 turn_bridge가 받는 메시지열이 같아야 한다.

## 8. 권한과 #8299

- `agy acp`에도 `--dangerously-skip-permissions`가 있으므로 기술적으로 #8299와 같은 수준을 낼 수 있다. 그러나 포럼 11-12는 consumer 로그인에서 이 플래그로 harness 도구를 모는 것을 비준수로 적는다. 그래서 ACP 경로에서는 플래그를 쓰지 않는다.
- `agy acp`에는 `--settings`가 없다. 규칙 파일은 프로필의 `settings.json`(`~/.gemini/antigravity-cli/settings.json`, 또는 `--gemini-dir`로 분리한 프로필)뿐이다. AgentDesk가 그 파일을 쓰려면 사용자 설정 파일을 건드리게 되므로, 기본 설계에서는 쓰지 않는다. 사람이 관리하는 규칙은 그대로 적용된다("rules already allow or deny don't prompt").
- 규칙이 다루지 않는 행동은 `session/request_permission`으로 온다. AgentDesk의 응답 정책은 사용자 결정 D1의 세 가지 중 하나다.
  - (a) `allow_once` 자동 응답. #8299 A안(11-21)과 같은 수준이다. 포럼 11-12의 취지와는 충돌할 수 있다.
  - (b) Discord 승인. 사람에게 버튼을 보이고, 기한 안에 답이 없으면 `reject_once`로 답한다. 조각 3에서 구현한다.
  - (c) 사람이 관리하는 규칙 파일의 allow 목록 밖은 모두 `reject_once`로 답하고, 본문 뒤에 거절 사실을 남긴다.
- 권장은 (b)이고 (c)를 그 대체로 둔다. D1이 정해질 때까지 조각 1·2의 기본은 (c)다(조용한 허용을 만들지 않는다). 사용자가 (a)를 고르면 설정값 하나로 바꾼다.
- `interaction_*` 질문은 권한이 아니라 선택지 질문이다(T3 `AntigravityProtocol.ts:68-73`). 조각 1·2는 `cancelled`로 답하고 안내를 남긴다. 조각 3에서는 (b)와 같은 경로로 Discord에 보인다.
- #8299의 `-p` 경로(자동 용도)는 그대로 둔다. ACP 채널은 #8299를 대체하는 것이 아니라 비켜 간다.

## 9. 모델

- 일반 외부 클라이언트에는 Gemini 모델만 열린다(1절). `clientInfo`를 바꿔 third-party 모델을 얻는 방식은 쓰지 않는다(4.2절 3항).
- 채널 모델 설정의 의미는 "다음 prompt 전 `session/set_config_option`"이다. 세션의 `configOptions`(`id:"model"`)에 없는 값이면 턴을 시작하지 않고 "이 채널은 외부 클라이언트용 모델만 쓸 수 있다"는 안내와 사용 가능한 목록을 게시한다. 입력은 실패로 닫혀 한 번 게시된다. `-p`로 되돌아가지 않는다.
- `/model` 목록은 ACP 채널이면 actor가 마지막으로 본 `configOptions`를 쓴다(조각 2). 아직 세션이 없으면 "첫 대화 뒤 갱신"을 표시한다. 지금 문구 "default + custom model id from agy models"(`model_catalog.rs:592`)는 `-p` 채널에만 남는다.
- 설정 칸의 모델이 비어 있으면 agy 기본값을 쓴다.

## 10. 공식 SDK와 직접 구현

| | crate 의존(`agent-client-protocol` 5.5.1) | schema crate만(`=1.21.2`, default-features off) | 직접 구현(선택) |
|---|---|---|---|
| 의존 | 61~75 crate. `process` 기능은 async-process·async-io 등 두 번째 reactor를 가져온다. | 32 crate(주로 proc-macro) | 없음(tokio·serde·serde_json 기존) |
| 맞음 | JSON-RPC 상관·세션 라우팅 제공. 단 콜백이 dispatch를 막는다(`jsonrpc.rs:1342-1346`). `AcpAgentConfig`에 cwd가 없다(`acp_agent.rs:80-86`). | wire 타입이 정확하다 | 필요한 v1 부분만 |
| 변동 | 6주에 메이저 2회(3.2→5.5.1), schema 정확 고정, `unstable_*`는 minor에서 바뀐다(`lib.rs:135-150`) | schema minor를 따라가야 한다 | 모르는 변형은 `Value`로 받아 관용 |
| 크기 | — | 전송 +400~600 | 약 +900(SDK 조사 추정 1.0~1.5k에서 fs·terminal·auth·v2를 뺐다) |

직접 구현 범위는 다음과 같다. 줄 단위 JSON-RPC 2.0(16 MiB 줄 상한, JSON이 아닌 줄은 버림), 요청 ID→oneshot 대기 맵(incarnation 포함), 그리고 메서드 `initialize`, `session/new`, `session/resume`, `session/load`(대체), `session/prompt`, `session/cancel`(알림), `session/set_config_option`, `session/close`(광고 시), 들어오는 `session/update`, `session/request_permission`, `fs/*`·`terminal/*` 거절이다. v2(prompt 수락과 완료 분리)는 agy가 v1을 말하므로 범위 밖이다. agy가 v2로 옮기면 그때 다시 설계한다 [U9].

## 11. T3 Code에서 가져올 것

T3 Code(MIT, `c89ca0a2f2c1d996d9b461568860991febaca04f`)는 읽기만 했다. 구현이 이 동작을 Rust로 옮길 때 옮긴 파일 머리에 MIT 저작권·허가 고지를 그대로 둔다. Zed(GPL 혼합)는 참고하지 않았다.

| agy 특유 동작 | T3 위치 | Rust 이식 |
|---|---|---|
| 스레드당 자식 하나, 턴 사이 재사용 | `packages/provider-acp/src/server/adapter.ts:7771-7795` | 그대로(채널당 하나) |
| 인증 URL이 JSON 아닌 stdout 줄로 나온다 | `apps/server/src/provider/antigravityAuthSupport.ts:646-710` | 그대로 버린다. 중계하지 않는다(바꿈). |
| eager `authenticate(methodId)` | `AntigravityAcpSupport.ts:125` | 바꿈: 보내지 않는다(캐시 자격, 포럼 조건) |
| 재부착은 resume, 실행 중엔 load와 replay 차단 | `AcpSessionRuntime.ts:2446-2470,1838-1863` | resume 우선은 그대로. load는 대체만, 응답 전 전부 버림 |
| resume·load 실패 시 조용히 new | `adapter.ts:8027-8048` | 바꿈: new는 하되 맥락 손실 안내를 붙인다 |
| 실행 중 두 번째 prompt = steer(agy ≥1.5.0), 앞 prompt는 `end_turn` | `adapter.ts:9789-9870`, `AntigravityAcpSupport.ts:76-84` | 조각 3에서 그대로. 버전 게이트는 [U10] |
| cancel은 prompt가 쓰인 뒤, 15초 뒤 종료 | `AcpSessionRuntime.ts:2608-2660` | 그대로 |
| 업스트림 오류가 `end_turn`과 채팅 글로 온다 | `AntigravityTurnOutcome.ts:88-245`(대화 DB step type 17) | 조각 3에서 읽기 전용으로 이식 [U11] |
| `refusal` = 쿼터 소진 | `AntigravityTurnOutcome.ts:143-158` | 그대로(안내 문구) |
| prompt 끝 ≠ 턴 끝(백그라운드 명령, schedule) | `AntigravityProtocol.ts:49-66`, `AntigravityAdapterV2.ts:241-253` | 바꿈: 턴은 stopReason에서 닫는다. 턴 밖 갱신은 버리고 계수한다(3.2절). |
| 권한: mode `default`, 클라이언트가 답한다, `interaction_*`는 질문 | `AntigravityAcpSupport.ts:156-220`, `AntigravityProtocol.ts:68-73` | 응답 정책은 D1(8절) |
| fs read true, write false(write를 광고하면 전체 파일 편집으로 바뀐다) | `AntigravityAcpSupport.ts:131-139` | 바꿈: 둘 다 false |
| 모델은 `configOptions` `id:"model"`과 `set_config_option` | `AntigravityAcpSupport.ts:238-327` | 그대로 |
| 편집 위치 재계산(diff 표시용) | `AntigravityEditLocation.ts` | 버림(Discord에 diff UI가 없다) |
| 세션 파일 정리(일회용 세션) | `AntigravitySessionFiles.ts:117-168` | 버림(일회용 probe 세션을 만들지 않는다) |
| 15분 무활동 감시 | `AntigravityAdapterV2.ts:300-301` | 그대로 |
| 환경 정리(`AGY_CLI_DISABLE_AUTO_UPDATE`, 사설 TMPDIR, 가짜 `SSH_CONNECTION`) | `antigravityAuthSupport.ts:202-245` | 자동 업데이트 끔·TMPDIR은 그대로. 가짜 `SSH_CONNECTION`(토큰 저장 위치 우회)은 쓰지 않는다. |

## 12. 기존 이슈 방향

- **#8274(tui_o 스풀): 계속.** ACP 어댑터는 #8274 스풀의 생산자를 하나 더 만들 뿐이다. writer·shadow·iMessage 경계는 바뀌지 않는다. 스풀 예약 시점의 읽기(3.3절)만 리뷰에 올린다.
- **#8297(agy E2E 셀): 축소·유지.** E-65 셀은 `-p` 자동 경로의 회귀 시험으로 계속 쓴다. ACP 경로는 봇 입력으로 실제 agy를 모는 E2E를 만들지 않는다. 그것이 곧 지원 밖 사용이다. 가짜 ACP 에이전트(스크립트 stdio)로 시험 instance 안에서만 확인한다. 실제 agy 확인은 사람이 시험 채널에서 한다.
- **#8160: #8274 writer로 흡수(현 결정 유지).** stopReason은 원인을 없애지 않는다(7절).
- **#8299: 자동 용도에는 유지, ACP 채널에는 적용하지 않는다.** D1에 따른다.

댓글 초안은 레인 보고서에 둔다. 이슈에 쓰는 것은 코디네이터의 일이다.

## 13. 조각 계획

| 조각 | 범위 | 소유 파일(prod) | 추정 | 수용 시험 | 운영 확인 |
|---|---|---|---|---|---|
| 1. 자격 게이트와 ACP 클라이언트 핵심(dormant) | `agy_acp.channels` 설정(기본 빈 목록), `acp_selected` 판정, JSON-RPC 전송·wire 타입, 채널 actor(spawn·initialize·new·prompt·cancel·EOF·기한·incarnation fence), 3.1절 매핑과 스풀 기록, 사람 작성 플래그 | `agy_acp/{mod,rpc,wire,child,turn}.rs`, `stream_json_turn.rs`, `intake_turn.rs`(인자), `src/config` 1곳, `services/mod.rs` | 9파일 / +950 | ① **자동 경로 불변**: 목록이 비면 어떤 agy 턴도 ACP 준비를 부르지 않는다. 목록에 있어도 루틴·`provider_exec`·dispatch·봇 작성(E2E Test-Announce)·HTTP 헤드리스(원본 claim 없음) 턴은 `execute_streaming`/`execute_recorded`로 간다(생산 진입점 `execute_request`에서 단언). ② 가짜 에이전트 stdio fixture로 짧은 답·긴 답·빈 답·cancelled·EOF·기한·늦은 옛 incarnation 응답 → `StreamMessage`열과 스풀 레코드. ③ 긴 답 fixture에서 `-p` 코덱과 ACP 어댑터가 turn_bridge에 같은 메시지열을 준다(#8160 불확대). ④ 공용 세션 칸이 그대로다. | 없음(dormant) |
| 2. 세션 수명 | `session.json` 저장, resume 우선, load 대체(replay 차단), 거절 시 new와 안내, 재시작·부팅 orphan 실패, clear·리셋·호스트 복구 연결, 모델 `set_config_option`과 목록, 유휴 만료 | `agy_acp/{store,attach}.rs`, `control.rs`, `retry_state.rs`, `host_recovery_guard.rs`, `model_picker.rs`, `model_catalog.rs`, `agy_acp/child.rs`·`turn.rs` | 8파일 / +650 | 상태 차트 변이와 1:1 대응: load 재생 무게시(LoadLeak), 부팅 orphan 실패·재전송 0(RerunSent), clear 뒤 새 세션(ClearKeeps), 거절 resume → new와 안내 1회, 모델 목록 밖 → 실패 게시 1회. PG를 쓰는 시험은 `pg_throwaway.sh`로 돌린다. | 시험 채널 1개를 목록에 넣고, 사람이 짧은 답·긴 답·`!stop`·`/clear`·dcserver 재시작 뒤 이어 말하기를 1회씩 확인한다. 이때 [U1][U4][U6][U7][U12]를 확정한다. |
| 3. 실행 중 입력과 상호작용 | 채널 설정 `steer`(기본 off)와 intake busy 경로 연결, permission·interaction Discord 중계(D1=b) 또는 정책(a/c), 업스트림 오류 판정, `thought_level`, 운영 진단(채널별 자식·incarnation·턴 밖 갱신 계수) | `agy_acp/{permission,outcome}.rs`, intake busy 경로 1~2곳, `turn_deliver/prepared.rs`, `agy_acp/turn.rs`, 진단 route 1곳 | 7파일 / +450 | steering: 두 입력이 각각 한 번 게시, 앞 응답 유실 0(SteerDrop). permission 기한 거절·취소 시 `cancelled` 응답. 오류 step 판정 fixture. | 시험 채널에서 사람이 실행 중 입력과 권한 승인을 1회씩 확인한 뒤 실제 사람 채널을 하나씩 목록에 넣는다. |
| 합계 | | 파일 중복 포함 | 24파일 / +2,050 | | |

각 조각은 prod 30파일 / +1,500 상한과 +1,200 정지선 아래로 추정했다. 1,000줄이 넘는 기존 파일(`intake_turn.rs`, `provider_dispatch.rs`, `control.rs`)에는 호출 교체나 인자 전달만 하고, 본체는 `agy_acp/` 아래에 둔다. 추정은 #8274 조각 실측(1/+340, 2/+465, 3/+560)과 SDK 조사의 LOC 추정을 근거로 했다. 구현 뒤에는 `pr_cap_prod.py`로 실측한다.

## 14. 상태 차트 결과 요약

규칙은 다음과 같다. 입력마다 터미널 게시 정확히 1(`AtMostOnePost`와 `EveryInputSettles`), prompt 실행 1회(`NoDuplicateExecution`), 남의 응답으로 닫지 않음(`NoForeignResponse`), clear 뒤 옛 세션 금지(`NoPromptToClearedSession`), 게시에는 내구 종료 기록(`PostOnlyWithDisposition`)이다. 설계 4개 구성(Design·DesignSteer·DesignLoad·PerTurn)은 6개 규칙을 모두 통과했다. 변이 6개와 합본 1개는 각각 예상한 규칙만 깨졌다. 전체 11개 구성이 `scripts/ci/statecharts_tlc.py`에서 rc=0이었다. 반례 경로는 [보고서](../../statecharts/agy-acp/agy-acp.report.md#counterexamples)에 있다.

## 15. 사용자 결정과 미확인 가정

- **D0(필수, 착수 전):** AgentDesk가 Discord 입력을 `agy acp`로 중계하는 것이 포럼 11-09의 "개인 단일 사용자 대화형 클라이언트"인지, 11-12의 "서드파티 harness 안의 모델 공급자"인지. 후자로 본다면 이 설계 대신 `gemini-api-key` 같은 다른 인증이 필요하다. 이 설계는 D0 = 전자를 전제로 한다.
- **D1:** 권한 응답 정책(a)/(b)/(c)(8절). 권장은 (b)다.
- [U1] agy ACP sessionId가 UUID인가. 확인 방법: 조각 2 운영 확인에서 사람이 연 세션의 `session.json`을 본다. UUID가 아니면 스풀 `init` 입력 형식을 넓히는 별도 설계가 필요하다.
- [U2] `usage_update` 필드와 `StatusUpdate` 대응. 확인 방법: 조각 2 운영 확인의 프로토콜 로그.
- [U3] agy가 `max_tokens`·`max_turn_requests`를 실제로 쓰는가. 확인 방법: 같은 로그. 안 쓰면 해당 분기는 방어 코드로 남는다.
- [U4] turn_bridge가 `Init`·`Done`의 session_id를 그대로 돌려받을 때 공용 세션 칸이 바이트 단위로 그대로인가. 확인 방법: 조각 1 시험.
- [U5] 매 spawn 비용(T3 메모의 약 1GB 압축 해제). 확인 방법: 조각 2 운영 확인에서 spawn과 initialize 사이 시간을 잰다.
- [U6] app-data-dir 분리와 공유 중 무엇을 쓰는가, 공유 시 `-p` conversation ID를 resume할 수 있는가. 확인 방법: 조각 2 운영 확인(사람 조작).
- [U7] `_meta.antigravity.systemPrompt.append` 광고 여부. 확인 방법: initialize 응답 로그.
- [U8] 큐 재구성 입력의 작성자 증거. 확인 방법: 조각 1에서 `queued.rs:496,583`의 출처를 따라가, 사람 증거가 내구로 남지 않으면 ACP 자격을 주지 않는다.
- [U9] agy가 ACP v2로 옮길 시점. 확인 방법: initialize의 `protocolVersion`을 기록하고, 1이 아니면 거절하고 경보를 낸다.
- [U10] `agentInfo.version` 형식(T3 fixture는 `agy_acp_server_1.1.1`). 확인 방법: initialize 로그. 버전 문자열로 steering을 게이트하지 않고 채널 설정으로만 켠다.
- [U11] 대화 DB(`conversations/<sid>.db`) 위치와 스키마의 안정성. 확인 방법: 조각 3 전에 읽기 전용으로 열어 본다. 위치가 문서화돼 있지 않으므로 없으면 판정을 끈다.
- [U12] `session/resume`이 다른 프로세스가 만든 세션(재시작 전 자식)에 대해 성공하는가. 확인 방법: 조각 2 운영 확인에서 dcserver 재시작 뒤 이어 말하기.
