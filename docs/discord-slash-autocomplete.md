# Discord 슬래시 명령 입력 전수표

기준 main: `832254536a342caf0d94bf91abf281a93f43e2ce`. 실제 등록 진입점은
`src/services/discord/runtime_bootstrap/framework_setup.rs:208`의
`run_bot_build_slash_commands`다. 최상위 24개, 하위 명령 1개를 모두 조사했다.
파일 좌표는 이번 조각의 소스 기준이며 아래 경로는 `src/services/discord/` 기준이다. 인자가 없는 명령도 누락 판단을 위해 적었다.

| 명령 | 인자 | 처리 | 코드 좌표 | 소유 레인 |
|---|---|---|---|---|
| `/resume` | `session_id` | 기존 autocomplete: 현재 채널 worktree 계보의 Claude 세션, 표시 제목·시각·worktree / 값 UUID. 현재·live-bound·SDK 세션 제외. 비Claude는 기존 catalog API가 없어 수동 입력 예외(`../session_resume.rs:414`); 새 provider catalog는 이 조각 범위 밖 | `commands/session.rs:14`, `commands/session/autocomplete.rs:10` | #7678-1 |
| `/resume` | `cwd` | autocomplete: 현재 채널에 열린 작업 경로. 임의 경로 직접 입력도 유지; 경로가 없으면 빈 결과. session_id 선택 시 transcript 위치 자동 탐색은 기존 기능 | `commands/session.rs:21` | #7678-1 |
| `/pwd` | 없음 | 대상 인자 없음 | `commands/session.rs:142` | #7678-1 |
| `/status` | 없음 | 대상 인자 없음 | `commands/diagnostics/mod.rs:30` | #7678-1 |
| `/inflight` | 없음 | 대상 인자 없음 | `commands/diagnostics/mod.rs:47` | #7678-1 |
| `/clear` | 없음 | 대상 인자 없음, 소유 레인 착지 뒤 | `commands/control.rs:529` | #6486 |
| `/stop` | 없음 | 대상 인자 없음, 소유 레인 착지 뒤 | `commands/control.rs:387` | #6486 |
| `/cancel-queued` | `message_id` | 선택 문자열. 현재 actor가 가진 취소 가능한 대기·보관 입력을 autocomplete로 표시하며, 생략하면 실행 중 턴과 전달 중 head를 남기고 비운다 | `commands/control.rs:715` | #7678-2 |
| `/skill` | `skill` | 기존 autocomplete 재사용: builtin·runtime cache 이름 / 값 skill 식별자. 권한·25개·이름 100자·값 100자·중복·응답 시간 제한 적용 | `commands/skill.rs:341` | #7678-1 |
| `/skill` | `args` | 임의 프롬프트 문자열, ID·이름 목록이 아님. 선택지 제외 | `commands/skill.rs:346` | #7678-1 |
| `/model` | 없음 | 이미 실행 후 런타임 모델 선택 UI 제공. 문자열 인자 추가 불필요 | `commands/model_picker.rs:194`, `commands/model_ui.rs:220` | #7678-1; 설정 저장 #6486 |
| `/node` | 없음 | 이미 실행 후 노드 이름·instance ID 선택 UI 제공 | `commands/node.rs:292`, `commands/node.rs:169` | #7678-1 |
| `/sidecar` | 없음 | 대상 인자 없음 | `commands/sidecar.rs:10` | #7678-1 |
| `/fast` | 없음 | 토글, 대상 인자 없음 | `commands/fast_mode.rs:40` | #7678-1 |
| `/tool-call` | `action` | 기존 choices: on / off / status | `commands/tool_call.rs:182` | #7678-1 |
| `/goals` | 없음 | 토글, 대상 인자 없음 | `commands/goals.rs:24` | #7678-1 |
| `/effort` | `level` | autocomplete: 채널의 provider·모델이 지금 지원하는 effort 값 중 입력 prefix로 시작하는 값(Codex는 모델별, Claude ultra는 capability 확인 시). 직접 입력도 같은 검증을 지나며 미지원 값은 설정 변화 없이 거절 | `commands/tui_passthrough.rs:285` | #8109 |
| `/compact` | 없음 | 대상 인자 없음 | `commands/tui_passthrough.rs:317` | #7678-1 |
| `/adk` | 하위 명령 | inspect로 분기, 문자열 인자 없음 | `commands/inspect/mod.rs:60` | #7678-1 |
| `/adk inspect` | `view` | 기존 choices: last / session / prompt / context / recovery | `commands/inspect/mod.rs:68` | #7678-1 |
| `/queue` | `all` | Discord 기본 boolean UI, ID·이름 인자 아님 | `commands/diagnostics/mod.rs:66` | #7678-1 |
| `/health` | 없음 | 대상 인자 없음 | `commands/diagnostics/mod.rs:13` | #7678-1 |
| `/allowall` | `action` | 기존 choices: on / off / status, 소유 레인 착지 뒤 | `commands/config.rs:556` | #6486 |
| `/usage` | `period` | 기존 choices: ratelimit / month | `commands/receipt.rs:21` | #7678-1 |
| `/help` | 없음 | 대상 인자 없음 | `commands/help.rs:83` | #7678-1 |
| `/system-prompt` | 없음 | 대상 인자 없음 | `commands/system_prompt.rs:97` | #7678-1 |
| `/restart` | 없음 | 대상 인자 없음 | `commands/restart.rs:353` | #7678-1 |

등록 계약 시험은 `run_bot_build_slash_commands`가 만든 Discord 등록 JSON의 모든 명령·하위 명령·인자를 이 표와 대조한다.
문자열 인자는 autocomplete나 choices가 필요하며, 명시적 예외는 임의 프롬프트인 `skill.args`다.

자동완성 가드·권한·PG·후보 조회는 interaction 생성 시각 기준 2.3초의 단일 deadline을 공유한다.
Discord 3초 한도까지 응답 전송 여유를 남기고, catalog는 같은 deadline보다 200ms 먼저 부분 결과를 반환한다.
후보는 값 접두어, 값 포함, 표시 포함 순서로 안정 정렬한 뒤 25개로 제한한다.
채널 정보는 interaction payload, 같은 guild cache, REST 순서로 한 번 조회해 안내 후보에도 재사용한다.
채널 확인 오류나 durable recovery의 불확정 `Skip`은 확인된 fallback이 없는 한 다른 봇 선택 대신 재시도를 안내한다.

디스패치·카드·에이전트·채널 ID를 직접 받는 등록된 슬래시 명령은 없다.
HTTP API·버튼 custom_id·텍스트 명령을 슬래시 인자로 오인해 새 명령을 만들지 않는다.
기존 고정 choices와 picker는 위 main의 같은 좌표에 이미 충족된 범위이므로 재구현하지 않는다.
세션·skill 자동완성 등록도 main에 이미 있다:
[session.rs L17](https://github.com/itismyfield/AgentDesk/blob/832254536a342caf0d94bf91abf281a93f43e2ce/src/services/discord/commands/session.rs#L17),
[skill.rs L353](https://github.com/itismyfield/AgentDesk/blob/832254536a342caf0d94bf91abf281a93f43e2ce/src/services/discord/commands/skill.rs#L353).
이번 조각은 등록을 유지하면서 공통 한도·권한 검사와 cwd 후보를 보완한다.

## 대기 입력 취소

`/cancel-queued [message_id]`의 자동완성은 현재 봇·채널에 이미 있는 actor를
`mailboxes.peek`으로 찾는다. V2는 `Owner::cancelable`, V1은 `try_snapshot`의 Soft 입력을 읽는다.
actor가 없거나 조회가 실패하면 빈 선택지를 반환한다. 조회 중 actor 생성, 파일 읽기·정리,
adopt, commit은 하지 않는다. 값은 기존 단일 취소가 받는 primary 메시지 ID다.

표시는 `작성자 · MM-DD HH:mm · 본문 앞부분`이고 시각은 KST다. 작성자는 로컬 cache를
우선하며 없으면 `user:<id>` 또는 합성 입력 표시를 쓴다. 보관 입력에는 `(보관)`을 붙인다.
공통 검색·25개·100자 제한을 적용하고 이름 조회용 REST 요청은 하지 않는다.

ID를 생략하면 V2 `clear_waiting` 또는 V1 `discard_waiting_or_refused`로 대기 입력을 비운다.
응답에 비운 개수·앞부분·유지한 입력 수를 표시한다. 전달 중 head와 실행 중 턴은 계속 진행하며,
그 결과는 평소처럼 안내될 수 있다. 보관 입력은 같은 commit에서 `enqueued=true`,
`kind=cleared`로 표시하여 재시작·recover 뒤에도 다시 대기시키거나 새 보관 안내를 만들지 않는다.
전송 여부가 불확정인 보관의 원문·attempt·늦은 전달 확인은 보존한다.
저장 실패나 개수 불확정은 성공 0개로 표시하지 않는다.

실제 dispatcher 시험은 `runtime_bootstrap/gateway_runtime/slash_command_tests/cancel_queued_tests.rs`에 있다.
등록, 읽기 전용 후보, 채널·provider·권한, 단일 취소, 저장 오류, 무인자 비우기,
재시작·recover·안내, 잘못된 ID, follower claim, 실행 중 턴과 held watch를 검사한다.
