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
| `/cancel-queued` | `message_id` | 이번에는 코드 변경 없음. 현재 필수 문자열; 현재 채널 큐 autocomplete와 생략 시 전체 비우기는 조각 2, 소유 레인 착지 뒤 | `commands/control.rs:487` | #6486 / #7676 → #7678-2 |
| `/skill` | `skill` | 기존 autocomplete 재사용: builtin·runtime cache 이름 / 값 skill 식별자. 권한·25개·이름 100자·값 100자·중복·응답 시간 제한 적용 | `commands/skill.rs:341` | #7678-1 |
| `/skill` | `args` | 임의 프롬프트 문자열, ID·이름 목록이 아님. 선택지 제외 | `commands/skill.rs:346` | #7678-1 |
| `/model` | 없음 | 이미 실행 후 런타임 모델 선택 UI 제공. 문자열 인자 추가 불필요 | `commands/model_picker.rs:194`, `commands/model_ui.rs:220` | #7678-1; 설정 저장 #6486 |
| `/node` | 없음 | 이미 실행 후 노드 이름·instance ID 선택 UI 제공 | `commands/node.rs:292`, `commands/node.rs:169` | #7678-1 |
| `/sidecar` | 없음 | 대상 인자 없음 | `commands/sidecar.rs:10` | #7678-1 |
| `/fast` | 없음 | 토글, 대상 인자 없음 | `commands/fast_mode.rs:40` | #7678-1 |
| `/tool-call` | `action` | 기존 choices: on / off / status | `commands/tool_call.rs:182` | #7678-1 |
| `/goals` | 없음 | 토글, 대상 인자 없음 | `commands/goals.rs:24` | #7678-1 |
| `/effort` | `level` | 기존 choices: low / medium / high / xhigh / max | `commands/tui_passthrough.rs:333` | #7678-1 |
| `/compact` | 없음 | 대상 인자 없음 | `commands/tui_passthrough.rs:340` | #7678-1 |
| `/adk` | 하위 명령 | inspect로 분기, 문자열 인자 없음 | `commands/inspect/mod.rs:60` | #7678-1 |
| `/adk inspect` | `view` | 기존 choices: last / session / prompt / context / recovery | `commands/inspect/mod.rs:68` | #7678-1 |
| `/queue` | `all` | Discord 기본 boolean UI, ID·이름 인자 아님 | `commands/diagnostics/mod.rs:66` | #7678-1 |
| `/health` | 없음 | 대상 인자 없음 | `commands/diagnostics/mod.rs:13` | #7678-1 |
| `/allowall` | `action` | 기존 choices: on / off / status, 소유 레인 착지 뒤 | `commands/config.rs:556` | #6486 |
| `/usage` | `period` | 기존 choices: ratelimit / month | `commands/receipt.rs:21` | #7678-1 |
| `/help` | 없음 | 대상 인자 없음 | `commands/help.rs:83` | #7678-1 |
| `/system-prompt` | 없음 | 대상 인자 없음 | `commands/system_prompt.rs:97` | #7678-1 |
| `/restart` | 없음 | 대상 인자 없음 | `commands/restart.rs:353` | #7678-1 |

디스패치·카드·에이전트·채널 ID를 직접 받는 등록된 슬래시 명령은 없다.
HTTP API·버튼 custom_id·텍스트 명령을 슬래시 인자로 오인해 새 명령을 만들지 않는다.
기존 고정 choices와 picker는 위 main의 같은 좌표에 이미 충족된 범위이므로 재구현하지 않는다.
세션·skill 자동완성 등록도 main의 `commands/session.rs:17`과 `commands/skill.rs:353`에 이미 있다.
이번 조각은 등록을 유지하면서 공통 한도·권한 검사와 cwd 후보를 보완한다.

## 조각 2 설계 메모

#6486·#7676 착지 뒤 최신 main에서 아래 API를 다시 대조한다. 이 조각은 해당 owner 코드를 쓰지 않는다.

| 필요한 경계 | 현재 코드 좌표 | 후속 설계 |
|---|---|---|
| slash 진입·권한 | `commands/control.rs:487` | `message_id: Option<String>`으로 바꾸고 기존 check_auth·명령 정책 유지. autocomplete는 읽기 전용, 기존 공통 응답 제한 재사용 |
| 단일 취소 직렬화 | `queue_io.rs:98`, `../turn_orchestrator/queued_head/v2/control.rs:96` | 현재 채널·provider·token namespace로 기존 actor 요청 사용. autocomplete 뒤 dispatch된 항목은 stale 응답; 실행 중 턴은 취소하지 않음 |
| V1 읽기 | `../turn_orchestrator/checked_purge.rs:79`, `../turn_orchestrator/episode_identity.rs:441` | `try_snapshot()` 오류를 빈 큐 성공으로 바꾸지 않는다. 기본 snapshot의 intervention_queue만으로 V2가 비었다고 판단하면 안 됨 |
| V2 owner 요청 | `../turn_orchestrator/queued_head/v2/actor.rs:56`, `../turn_orchestrator/queued_head/v2.rs:382` | actor의 기존 owner에서 `Owner::json`으로 읽을 수 있다. autocomplete용 queued·retained DTO 조회 API가 필요하며, 묶인 입력은 `InputAttempt::members`(`../turn_orchestrator/queued_head/v2.rs:389`)의 original+followers를 펼친다. 조회 중 v2_ensure/adopt 금지: adopt가 commit을 할 수 있음 |
| 부팅 전 read-only 파일 조회 | `../turn_orchestrator/pending_queue_persistence/queued_recovery.rs:122`, `../turn_orchestrator/pending_queue_persistence/snapshot_io.rs:77` | read_catalog는 provider/token 범위 전체를 읽으므로 autocomplete에는 현재 채널 전용 checked reader 필요. 기존 snapshot_io reader는 V2를 거절하므로 `../turn_orchestrator/queued_head/compat.rs:10`의 read-only inspect를 사용하는 전용 projection이 필요하다. 디스크 조회는 spawn_blocking과 deadline 적용. namespace 일치 확인. `load_pending_queues_checked`는 임시 파일 정리 부작용이 있어 호출 금지 |
| V2 기존 clear 제한 | `../turn_orchestrator/queued_head/v2/control.rs:230`, `../turn_orchestrator/queued_head/v2/projection.rs:154` | 기존 clear/purge는 untried만 제거하고 attempted head·retained·terminal을 보존. 전체 비우기에 그대로 사용하지 않음 |
| owner 상태·commit | `../turn_orchestrator/queued_head/v2.rs:24`, `../turn_orchestrator/queued_head/v2/commit.rs:17` | 큐와 retained를 한 owner commit에서 비우는 전용 요청 필요. front grant/claim, admission receipt/bind, retained notice와 retry drive의 오래된 권한을 함께 무효화. active 턴 종료 권한은 추가하지 않음 |
| 실행 drive | `queue_io/input_owner/drive.rs:37`, `queue_io/input_owner/drive.rs:163`, `queue_io/input_owner/recovery.rs:37` | 비우기와 claim/reserve/settle 경쟁을 actor가 직렬화. 이미 전송한 head의 Unknown 증거를 삭제해 재전송시키지 않도록 계약을 먼저 확정 |

자동완성 값은 실제 Discord message ID, 표시는 `작성자 · 시각 · 본문 앞부분`이다.
현재 채널의 취소 가능한 queued·retained 입력만 읽고 25개·이름 100자 제한을 적용한다.
retained의 합성 ID나 여러 source를 가진 입력을 어떤 ID로 선택할지는 #7676의 새 owner 계약을 따른다.
Discord snowflake 시각 또는 저장된 생성 시각을 쓰며 작성자 이름은 로컬 캐시를 우선한다.

인자 생략 시 전체 비우기는 큐 항목과 retained 입력을 모두 세어 개수·본문 앞부분을 응답한다.
owner commit 실패·CAS 불확정은 성공 0개로 표시하지 않고 오류/보류로 응답한다.
V2 파일 직접 쓰기·unlink·legacy /clear 우회는 금지한다.
main의 현재 V2 clear는 attempted head와 retained를 보존한다는 점을 먼저 합의하고,
"전체"가 실행 중/전송 불확정 head를 지울 권한까지 뜻한다고 임의 확장하지 않는다.

후속 검증: V1/V2 각 prefix·25개·빈 결과·다른 채널/사용자 비노출, 선택 후 단일 취소,
인자 생략 시 queued+retained 제거, 재시작 후 비어 있음, claim·reserve·notice와의 경쟁,
commit 실패/Unknown 보존, 빈 큐 0개, 원인 제거 뮤턴트 RED, 운영 autocomplete 1회 확인.
