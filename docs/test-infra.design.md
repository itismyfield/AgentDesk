# 사용자 결과를 확인하는 시험 기반 통합 설계

설계 기준: `4e0db0cccd59169a09f25fe162d8cc1b2755e96e` (2026-11-12).
개정: **r2: 리뷰 CLEAN P2 11 이관·새 방향 단일 호스트 flock 임대·root credential**.
설계 리뷰는 CLEAN으로 종료됐다. 아래 P2 계약은 각 구현 조각의 RED·수용 시험으로 이관하며 설계 재리뷰 라운드를 열지 않는다.
대상: [#8124](https://github.com/itismyfield/AgentDesk/issues/8124),
[#8120](https://github.com/itismyfield/AgentDesk/issues/8120),
[#8123](https://github.com/itismyfield/AgentDesk/issues/8123).
이 문서는 구현 전 계약이다. 아래의 신규 명령·키·산출물은 아직 없으며, 이 조사에서는 빌드·live 시험·운영 접근을 하지 않았다.

결론은 **독립 Discord 봇 세트와 전용 시험 채널 두 개를 한 번에 한 인스턴스가 소유**하게 하고,
그 인스턴스에서 얻는 E2E 단언을 기능 지도의 조합 칸과 연결하는 것이다.
실제 Discord·별도 봇·기존 길드를 쓰는 것은 #8124의 **11-12 15:43 KST 사용자 확정**이다.
앞선 댓글의 가짜 Discord 우선 검토는 이 결정으로 대체됐다. 운영 봇 공유·별도 길드는 선택지가 아니다.
현재 release 경로의 근거는 `scripts/deploy-release.sh:153`의 release label 기본값과
`scripts/refresh_release_launchd_plist.py:15`의 release plist 생성이다. dev 인스턴스 부재는 #8124의 현황 기록을 입력으로 받았으며,
이 조사에서 launchd·운영 디렉터리를 다시 열어 확인하지 않았다.

## 1. 테스트 인스턴스 — #8124

### 1.1 실행 계약과 격리 축

시험 인스턴스는 **지정 시험 호스트 한 곳**에서만 실행한다(기본값 제안 mac-mini, §5 확인).
다른 호스트 레인은 고정 HEAD를 `refs/review/*`로 push하고, 시험 호스트가 해당 SHA를 자기 worktree로 checkout해 빌드·실행한다.
미커밋 변경·다른 호스트의 기존 바이너리는 시험하지 않는다. 시험 호스트에서 쓰는 신규 진입 명령은 다음과 같다. **지금 실행하는 명령이 아니다.**

```sh
scripts/dev-instance.sh up "$PWD" --lane g8120 --build --profile tui-live
```

`up`은 worktree·HEAD·바이너리 SHA256을 고정하고, 첫 빌드 시 기존 `seed_target.sh`와
`wait_headroom.sh`를 거친 뒤 PATH의 cargo-shim으로만 빌드한다. seed/headroom의 정본은 issue-pipeline 스킬이다.
그 다음 비밀 없는 시험 프로필로 독립 설정을 생성하고, preflight를 통과한 **그 바이너리의 명시적 `dcserver`**만 실행한다.
일반 서버 실행·배포 스크립트·운영 launchd 경로를 재사용하지 않는다.
후속 인터페이스는 `e2e --instance <id> --suite <name>`, `down --instance <id>`,
`reap --instance <id> --dry-run`이다. 모든 하위 명령은 같은 receipt를 요구한다.

| 축 | 시험 값의 출처·격리 계약 | 금지·실패 조건 |
| --- | --- | --- |
| HTTP·self API | 프로필에서 loopback 전용 시험 포트 풀(초안 18791–18806)을 배정, `server.port`에 명시; self API도 같은 값 | 기본 release 포트·운영 주소·이미 점유한 포트, 실제 health의 instance/head 불일치 |
| runtime·파일 | `~/.adk/dev/instances/<instance-id>/` 안의 config/runtime/data/logs/outputs/policies/workspaces; `AGENTDESK_ROOT_DIR`와 명시 config를 함께 지정 | release subtree, 외부로 나가는 symlink·절대경로, 누락 config의 fallback |
| PostgreSQL | 인스턴스 전용 local cluster·소켓 디렉터리·DB·role; 초안 loopback 25432–25447. 운영 cluster를 공유하지 않음 | 15432·5432·운영 DSN, 상속 `DATABASE_URL`·`PG*`, 소유 receipt 없는 cluster. DB 미기동을 SQLite/메모리 성공으로 대체 금지 |
| Discord | 테스트 Claude 수신 봇·Codex 수신 봇·Driver 세 앱의 고정 ID와 전용 채널 allowlist; root credential 파일 공급 | 다른 앱 ID, 같은 token의 수신/driver 겸용, 시험 영역 밖 View/Send 권한, token env 상속 |
| tmux | `TMUX`·`TMUX_PANE` 제거, 전용 `TMUX_TMPDIR` 아래 실제 socket 선택; 신규 instance prefix를 세션 이름 생성·탐색에 함께 적용 | 기본 socket·`AgentDesk-*` 기존 세션 탐색/채택/키 송신·전역 kill |
| Herdr | wrapper가 직접 띄워 receipt에 기록한 daemon·endpoint·workspace; `legacy_dedicated` 모드의 socket/home/session·local execution node를 명시 | user_default/autostart·기본 endpoint fallback, 기존 사용자 workspace 채택, receipt 없는 daemon |
| HOME·provider | 자식용 HOME/TMPDIR/XDG_CONFIG_HOME, `CLAUDE_CONFIG_DIR`·`CODEX_HOME` 및 provider 출력·rollout 경로를 instance에 귀속 | 실제 HOME의 설정/skills/MCP/rollout을 쓰거나 운영 세션을 발견·재부착 |
| 기능 프로필(P2-1) | 비밀이 아닌 기능 플래그를 **키 이름 단위 allowlist**로 시험 프로필에 명시; 채널 ID는 시험 ID로 치환. receipt에 실제 적용값 기록 | 운영 env 파일 복사·임의 ADK_* wildcard 상속, 플래그 누락/게이트 off를 제품 결함 RED로 집계 |
| launchd·CLI API | 미등록 `com.agentdesk.test.<id>` label·고유 release-tmux 이름, `AGENTDESK_API_URL`/`ADK_API_URL` 모두 시험 API, PATH 첫 항목은 레인 바이너리 | release label/default API, doctor/restart의 launchd·tmux fallback |
| 공유 인증·자원 | Discord token·agent-op 자격·`MEMENTO_ACCESS_KEY`는 자식 env에서 제거. 독립 provider 인증 및 keychain 경계 확인; build/provider는 nice 또는 taskpolicy -b와 headroom 적용 | 공유 Memento 자동 연결, 확인되지 않은 keychain 로그인/로그아웃, 자원 예산 없는 실행 |
| 실행 노드·외부 연결 | 시험 DB의 local node만 seed. 원격 node·peer bootstrap·루틴·캠페인 worker·connector는 기본 off | 운영 cluster/remote host/API·공유 MCP에 쓰기. 원격 배치를 확인하려면 독립 시험 노드 프로필을 별도 승인·구현 |

프로필과 receipt는 비밀을 포함하지 않는다. receipt에는 instance ID, canonical root, worktree/head,
binary hash, API/PG port, PG data dir/role, bots/channel IDs, tmux socket/prefix,
Herdr endpoint/workspace, child PID·시작 identity를 기록한다.
`env -i`에 준하는 allowlist로 PATH·locale·시험 HOME·API/label·독립 provider 인증·**명시 기능 프로필**을 만든다.
`ADK_BUSY_INJECT_DISCORD_CHANNELS`는 시험 채널 allowlist로 선언한다. preferred node/capacity_aware와 tui_o 관련 기능도 시나리오가 요구하는 상태를 명시한다.
운영 파일은 읽지 않고 코드의 키·기본값과 비밀 없는 시험 선언을 대조한다. 기타 운영 ADK_* 키 보존은 자동 복사가 아니라 프로필 항목 추가로 수행한다.
적용 플래그·host·실행 노드의 precondition witness가 없으면 `UNEVALUABLE(precondition)`이며 회귀 RED가 아니다.
provider 인증은 독립 config home에 사전 준비하며 운영 credential 파일을 복사하지 않는다.
계정이 같으면 사용량은 공유되므로 §2 예산으로 제어한다. 인증 준비 안 됨은 `BLOCKED_AUTH`이지 skip/PASS가 아니다.

### 1.2 현재 코드가 보장하는 것과 새로 막아야 할 것

아래 좌표는 모두 위 기준 SHA다. runtime root만 바꿔도 안전하다는 전제를 두지 않는다.

| 설정·진입점 | 현재 코드 근거 | 구현 조각의 필수 검사·증거 |
| --- | --- | --- |
| root | `src/config/runtime_root_guard.rs:3`은 `AGENTDESK_ROOT_DIR` 우선, `:10`은 test temp, `:34`는 release 기본 | 신규 test-instance startup은 canonical dev root를 필수로 받고 기본값을 거부 |
| config 선택 | `src/config/config_path.rs:22`, `src/config/config_path/selection.rs:22`는 runtime/cwd/release 탐색; `:41`은 explicit sibling 허용 | test mode에서 정확한 manifest config 파일만 허용. 파일 없음·sibling 대체도 오류, config load 전에 검사 |
| 파일 경로 | `src/config.rs:2378`의 절대경로는 root 재귀속 없이 그대로 사용 | data·prompt·workspace·policy·routine·o_store·provider output 경로 전수표를 만들고 생성/열기 경계에 동일 root 검사 |
| API | `src/config.rs:164`의 `server.port`; `src/cli/dcserver.rs:1147`, `:1183`의 HTTP/self API | endpoint를 receipt에서만 공급하고 health에서 실제 bound port·instance·head를 read-back |
| DB | `src/config.rs:707`의 database 설정; `src/db/postgres.rs:190`에서 `DATABASE_URL`이 config보다 우선 | DB 접속 전에 test manifest endpoint/DB/user와 실제 connect options 일치. 독립 PG 권한으로 운영 DB 연결 자체가 불가능해야 함 |
| bot token | `src/services/discord/agentdesk_config.rs:141`은 inline/credential fallback; `src/credential.rs:46`은 runtime root 기준; `src/runtime_layout/paths.rs:11`은 `<bot>_bot_token` 경로 | 신규 token_env를 만들지 않음. root credential만 허용, inline/단일 AGENTDESK_TOKEN 상속 금지. canonical ancestor·파일/권한은 별도 검증 |
| tmux | `src/domain/provider/naming.rs:6`의 고정 `AgentDesk` prefix; `src/services/platform/tmux.rs:30`은 `-u`만 지정 | 공통 command 생성과 raw tmux 진입점 전수 확인. socket·prefix를 launch/scan/attach/cancel/clear/recovery 모두에 전달 |
| Herdr | `src/services/herdr_launch.rs:56`, `src/config/session_hosts.rs:158`은 host 선택 시 exact channel 조회 | 시험 endpoint 설정과 실제 session host가 일치. 기본 endpoint는 test mode에서 오류 |
| provider 설정 sync | `src/bootstrap.rs:43`, `src/lib.rs:267`의 일반 RunServer initialize는 MCP sync 수행 | 명시 `dcserver` 경로는 이 initialize를 통과하지 않는 현재 동작 유지. 이를 무조건 발생하는 dcserver 부작용으로 오인하지 않음. HOME·provider home 격리는 별도로 필요 |
| HOME·host 탐색 | `src/services/provider_auth_profile.rs:189`, `:485`의 profile home; `src/services/codex/tui_session_launch.rs:149`의 HOME fallback; `src/services/cluster/session_discovery.rs:707`의 tmux 스캔 | test HOME 아래 named profile만 허용. 시험 Herdr home은 `src/services/session_host/herdr/managed_config/path.rs:47`의 개인 경로 보호와 충돌하지 않는 형제 경로로 배치 |
| store·Herdr default | `src/services/tui_o/store/mod.rs:182`의 root/o_store; `src/config/session_hosts/user_default.rs:31`의 기본 socket | root와 HOME을 모두 강제. 시험 프로필에서 user_default 빈 endpoint 금지 |
| launchd·doctor | `src/cli/dcserver.rs:15`, `:255`, `:681`, `:734`의 release label/restart fallback; `src/cli/doctor/orchestrator.rs:1712`의 fix restart | 미등록 test label과 고유 `AGENTDESK_RELEASE_TMUX_SESSION` 필수. test startup에서 release/누락 label 거부, 직접 doctor/restart fallback 거부. 후계 dcserver도 supervisor의 lease FD를 받아야 함 |
| CLI 기본 API | `src/cli/monitoring.rs:3`, `:32`는 기본8791; `src/cli/client.rs:16`은 env/root 사용 | dcserver·provider 자식 모두 두 API env를 시험 주소로 명시; monitoring 등록도 시험 endpoint만 사용 |
| Memento | `src/config.rs:2323`, `:2354`는 env key만으로 공유 MCP 자동 삽입 가능 | provisioner 외 자식에서 MEMENTO_ACCESS_KEY 제거; test config의 mcp_servers.memento는 startup 거부 |
| Herdr daemon | `src/services/session_host/herdr/launch_host.rs:641`은 UserDefault에서 autostart; `src/services/session_host/herdr/autostart/spawn.rs:244`는 env_clear/setsid | legacy_dedicated 유지, test mode autostart 금지. wrapper receipt daemon만 사용·정리. env_clear/setsid를 FD close 증거로 쓰지 않음 |
| keychain·자원 | provider HOME 격리만으로 macOS keychain 항목 분리는 코드에서 입증되지 않음 | 준비 단계에서 값 없이 항목 이름/소유 범위가 독립인지 확인 전 BLOCKED_AUTH. 운영 항목을 이번 조사에서 열지 않음; 동일 호스트 자원 경합은 nice/taskpolicy·headroom·§2 한도로 제한 |
| 하네스 재시작·CLI | `scripts/e2e/tui_relay/restart_guard.py:10`, `:19`는 미확인 포트를 release로 선택; `scripts/e2e/tui_relay/herdr.py:27`, `:69`, `:247`은 기본 CLI 경로/endpoint 사용 가능 | 신규 instance adapter는 restart target을 receipt PID로만 결정하고 endpoint·lane absolute binary를 전달. 미지정은 시작 전 오류. 기존 release/dev 분기를 시험 인스턴스에 사용하지 않음 |
| 기존 dcserver 중복 정리 | `src/cli/dcserver.rs:291`은 root marker substring 판정, `:454`는 종료 경로; `src/cli/dcserver/replay_recovery.rs:28`은 root별 lock | 신규 test startup은 고정 길이 UUID root와 정확한 instance identity 검증을 먼저 적용. substring을 test cleanup 권한으로 쓰지 않음. 기존 일반 경로의 결함 범위는 넓히지 않고 별도 후속으로 남김 |
| 실행·복원·정리 | 기존 session 이름/host 경로는 여러 소비자가 사용 | up뿐 아니라 서버 재시작·startup recovery·provider respawn·cancel/stop/clear/down에 같은 manifest가 전달되는지 전수표·음성 시험으로 확인 |

호스트 전체 프로세스 스캔도 I2 격리 전수표에 포함한다(P2-4). `ps -ax*`·`pgrep` 호출 전체를 조사하고 **root·socket·receipt 밖 PID를 읽거나 종료하지 않는 test 경계**를 적용한다.

| 진입점 | 현재 선택 방식·남은 반례 | test mode 계약 |
| --- | --- | --- |
| `src/cli/dcserver.rs:305`, `:272`, `:291`, `:454` | pgrep 후보·ps eww·root substring으로 중복 종료 | 전역 duplicate cleanup을 비활성화하고 supervisor/lease 경로만 사용 |
| `src/services/discord/tmux_reaper.rs:700`, `:752`, `:776`, `:728` | 전체 ps-axo·ppid1·wrapper argv로 kill, root 검사 없음 | test orphan reaper off. release output-file의 고아 줄을 선택하지 않는 음성 시험; receipt reap만 허용 |
| `src/services/session_backend.rs:445` | 전체 PID/PPID에서 wrapper 자식 선택 | receipt wrapper baseline 선확인 후 범위가 제한된 조회 |
| `src/services/discord/turn_bridge/tmux_runtime/process_table.rs:224` | 전체 command 조회 후 pane descendant 선택 | 시험 socket pane identity 선확인, receipt 자식 PID만 조회·선택 |
| `src/services/process.rs:611` → `src/services/provider_cli/session_guard.rs:190` | 전체 ps aux에서 존재 확인 | receipt PID 대상 조회로 대체. 개별 PID의 args/etime/pgid 조회도 같은 소유 경계 적용 |

main의 전역 스캔/정리 동작은 이 조각에서 확대하지 않는다. test 진입점의 차단·범위 제한만 수용하며, 일반 경로의 기존 결함은 후속으로 남긴다.

**운영에 닿지 않는 증명은 아직 얻지 않았다.** 구현 수용 때에는 (1) wrapper가 만든 resolved manifest,
(2) child가 검증한 effective targets, (3) 독립 PG role/cluster·socket·Herdr 소유권,
(4) Discord 권한 read-back, (5) 접속/파일열기/프로세스 생성의 시험 경계 감사 결과를 묶는다.
운영 파일/DB를 읽어 전후 diff 하는 방식은 쓰지 않는다. 운영 경로를 가리킨 음성 프로필이
첫 파일/DB/Gateway 접속 **전** 거부되는 시험, 누락·상속·symlink·재시작 경로 시험을 필수로 둔다.
임의의 시험 빌드를 OS sandbox처럼 격리한다는 주장은 하지 않는다. 이 계약의 범위는 정상 제품 코드와 시험 진입점이며,
누락된 우회 진입점이 발견되면 그 인스턴스를 live 가능으로 표시하지 않는다.

### 1.3 봇 수·비밀값·권한

권고는 **세 앱**이다: `Test Discord Claude`, `Test Discord Codex`, `Test Discord Driver`.
앞의 둘은 제품 수신/출력, Driver는 입력·관측·시험 실패 알림을 담당한다. utility/notify 봇은 추가하지 않는다.
최소 단일 provider만 보면 수신 1 + Driver 1로 두 앱이지만, 두 provider 정규 경로를 고정 구성으로 덮으려면 세 앱이 필요하다.
전체 matrix도 수신 봇 provider를 바꾸며 매번 재기동하면 두 앱으로 순차 실행할 수 있다.
따라서 세 앱은 이론적 최소가 아니라 고정 provider 구성·작성자 검증·반복 실행을 단순하게 하는 권고다.

근거: `src/config.rs:241`의 bot provider는 단일 값이고,
`src/services/discord/settings/read.rs:339`는 token으로 launch configs를 중복 제거한다.
`src/services/discord/runtime_bootstrap.rs:215`가 settings provider를 고정하며,
`src/services/discord/router/message_handler/intake_turn.rs:208`의 일반 턴은 이 provider를 사용한다.
역할의 cross-provider dispatch만으로 모든 정규 경로를 대체할 수 없고,
`src/services/discord/router/message_handler/headless_turn.rs:114`는 shared provider mismatch를 거부한다.
Driver를 수신 봇과 합치면 `src/services/discord/router/intake_gate.rs:314`의 자기 메시지 무시 경로에 걸린다.
Driver ID만 수신 봇의 `allowed_bot_ids`에 넣는다(`intake_gate.rs:352`). 다른 봇의 메시지는 허용하지 않는다.

사용자에게 한 번에 준비를 요청할 것은 세 앱 생성·기존 길드 초대·아래 권한 적용·1Password 항목 추가다.
금고는 `AgentDesk`, **항목 이름은 위 세 이름**, 필드는 `token`으로 제안한다(현재 존재 여부 미확인).
`agent-op` provisioner가 token을 stdin/pipe로 받아 fresh instance root의 `credential/`에만 쓴다.
설정 bot 키 `claude`, `codex`의 정확한 파일명은 `claude_bot_token`, `codex_bot_token`; Driver는 별도 `driver_bot_token`을 직접 읽고 daemon bot 목록에 넣지 않는다.
기존 `write_secret_file`(`src/utils/secret_file.rs:41`, `:96`)로 디렉터리0700·파일0600을 만든다.
root guard는 env PathBuf를 반환할 뿐 ancestor canonical 검증은 하지 않는다. wrapper가 root/ancestor/leaf symlink·외부 경로를 검사하고 unknown legacy credential subtree는 거부한다.
기존 reader의 leaf 검사(`src/utils/secret_file.rs:124`)·legacy migration(`src/runtime_layout/mod.rs:357`)만으로 이 검증을 대신하지 않는다.
dcserver·provider·tmux/Herdr·Driver에는 Discord token env·agent-op 자격을 전달하지 않는다. token 값/항목 내용은 문서·manifest·argv·로그에 남기지 않는다.
빈 파일·다른 앱 ID는 접속 전/identity 조회에서 오류다. `down`은 제품/Driver 종료 뒤 credential 파일을 제거한다.
시험 자식 env와 ps env/argv에 sentinel token이 없는지 검사하며, env 정리 제거 뮤턴트는 RED여야 한다.
root credential은 같은 UID의 임의 파일읽기를 막는 sandbox가 아니라 **환경변수 전파 방지**다. debug·shell xtrace는 허용하지 않는다.

기존 `@everyone`·운영 role의 길드 권한을 변경하는 방식은 쓰지 않는다.
테스트 앱 전용 role/member overwrite로 non-test category/channel의 effective View/Send를 막고 테스트 category에서만 허용한다.
기존 운영 봇에는 신규 테스트 category의 View Channel을 주지 않는다. Administrator나 다른 role/child overwrite로 이 경계가 무력화되지 않는지 effective permission으로 확인한다.
경계를 만족하지 못하면 live 기동을 막고 권한 준비를 요청한다. 채널 필터는 추가 방어이며 Discord 권한을 대신하지 않는다.

| 대상 | 필요한 채널 권한·Gateway 설정 | 부여하지 않는 권한 |
| --- | --- | --- |
| 수신 Claude/Codex | View Channel, Read Message History, Send Messages, Send Messages in Threads, Create Public Threads, Add Reactions, Embed Links, Attach Files; 해당 제품의 guild message·Message Content intents | Administrator, Manage Guild/Roles/Channels/Webhooks, Mention Everyone. private-thread 시험 전에는 Manage Threads도 제외 |
| Driver | 위 중 관측·입력·첨부·public thread 생성에 필요한 권한, Guild Message Reactions intent 및 Message Content; 자기 시험 메시지/반응만 정리 | 운영 채널 접근, 다른 사용자의 메시지를 삭제하는 Manage Messages. 종료는 자신이 만든 thread만 대상 |
| 세 앱 OAuth | `bot` 및 제품 slash 시험 대상 수신 앱의 `applications.commands` | 운영 앱 token·기존 운영 command 등록 경로 공유 |

첫 기동에서 앱 ID와 slash 등록 대상 application ID를 기록한다.
현재 `src/services/discord/runtime_bootstrap/framework_setup.rs:35`는 guild 등록을 호출한다.
별도 앱의 command namespace인지 SDK 호출 경로와 실제 시험 앱의 command read-back으로 확인한다.
운영 command 목록을 변경하거나 운영 API로 확인하지 않는다.
앱의 guild 명령은 운영 채널에도 목록으로 보일 수 있다. 준비 때 Discord Integrations에서 **시험 앱 명령을 시험 category로 제한**한다.
`src/services/discord/runtime_bootstrap/gateway_runtime/command_guard.rs:29`의 담당 채널 외 ephemeral 거절도 유지하고, 비허용 채널을 모사한 음성 시험에서 거절·시험 PG 변화0을 확인한다(운영 채널에 시험 명령을 보내지 않음).
초기 프로필은 public thread만 포함하며 DM·private thread는 기능 지도에 gap으로 남긴다.

세 앱만으로 기존 하네스를 그대로 쓸 수는 없다. `src/server/routes/discord.rs:189`의 message GET은 announce credential을 읽고,
`scripts/e2e/tui_relay/discord.py:125`의 control 전송은 notify 봇을 지정한다.
I3b에 **Driver credential로 실제 Discord REST 입력/조회/thread 생성**을 하는 instance adapter를 넣고,
receipt의 세 앱 ID를 기존 author override(`scripts/e2e/tui_relay/assertions.py:27`)로 전달한다.
세 ID는 필수다. 누락·운영 기본 bot ID·receipt 불일치·중복이면 **첫 전송 전 `UNEVALUABLE(identity)`·전송0건**으로 닫는다(P2-6).
사용자 입력 경로의 회귀는 headless deliver API로 대체하지 않는다.
다만 Driver 입력은 실제 Gateway를 지나는 **allowed-bot 공통 경로** 증거다.
claim에 `input_source: allowed_bot`을 붙이고 아래 분기를 명시한다(P2-7).

| 인간과 다른 지점 | 코드 근거 | 시험·coverage 계약 |
| --- | --- | --- |
| bot identity·slash 분류 | `src/services/discord/router/intake_gate.rs:314`, `:352`, `:368`; `src/services/discord/router/intake_gate/gate.rs:84` | self 무시·허용 bot ID 검사, 등록 slash 텍스트 제외는 인간만. Driver의 command 결과를 인간 slash UX로 승격 금지 |
| 텍스트 provenance admission | `src/services/discord/dispatch_policy.rs:126`, `:255`; `src/services/discord/router/intake_gate.rs:496` | 평문 Driver 입력은 통과한다고 가정하지 않음. 현재 허용되는 DISPATCH envelope로 보내고 실제 admission witness 확인 |
| auth·취소 보존 | `src/services/discord/router/intake_gate.rs:514`, `:526` | auth bypass·preserve_on_cancel=false. 인간 인증/입력 보존 coverage0 |
| 봇 전용 dedup | `src/services/discord/router/intake_gate.rs:767` | 30초 dedup이 있는 입력임을 기록. 인간 dedup과 동일하다고 주장하지 않음 |
| 연속 입력 병합 | `src/services/discord/router/intake_gate.rs:832`; `src/services/discord/router/intake_gate/gate.rs:161` | bot은 병합하지 않음. 단일 입력 또는 명시한 병합 창 밖 간격으로 공통 경로 시험; 인간 병합 coverage0 |
| queued ack 카드 | `src/services/discord/router/intake_gate.rs:1036` | 인간만 카드 생성. Driver reaction 증거로 인간 ack 카드 칸을 채우지 않음 |
| dispatch thread guard | `src/services/discord/router/intake_gate.rs:839` | allowed bot의 parent 입력만 별도 큐 경로. 인간 parent/thread routing coverage로 승격 금지 |
| source generation·노드 적격성 | `src/services/discord/router/intake_queue_transaction.rs:124`; `src/services/cluster/intake_router_hook/placement.rs:79`; `src/services/cluster/intake_worker_capabilities.rs:220` | 인간 user_instruction과 bot/automation source가 다르고 preserve_on_cancel이 노드 적격성까지 전달됨. 인간 source/배치 coverage0 |

생성기는 input_source와 필수 단언의 source applicability를 대조한다. 인간 전용 병합/ack/auth/보존 및 인간 thread-routing 계약은 allowed_bot claim으로 채울 수 없다.
Driver가 allowed_bot_ids에 있으므로 SETUP/TEARDOWN marker를 보내면 새 턴을 깨울 수 있다.
bookkeeping은 local run ID·새 message snowflake·관측 창에만 기록한다. 실패 알림도 provider가 보는 parent/thread로 보내지 않는다.

### 1.4 채널 두 안과 동시 사용

| 안 | 흐름 | 비용 | 위험·평가 |
| --- | --- | --- | --- |
| **A. 전용 parent 2개(채택 기본값)** | Claude/Codex parent 아래 임시 public thread를 만든다. 같은 parent의 host를 시험 프로필로 tmux/herdr 번갈아 지정 | 채널 두 개·최초 권한 설정, 전용 봇 세 앱 | 운영 스모크와 물리적으로 분리. 운영 봇 View deny를 처음부터 증명하기 쉽다. host 전환 때 child/session 잔여가 없어야 함 |
| B. 기존 e2e 채널 8개 임대 | 실제 channel ID 기준 중앙 임대를 운영 스모크·수동 E2E·테스트 인스턴스 모두가 획득 | 새 parent 없음. 모든 기존 진입점에 임대 적용, provider/host별 배치 점검 필요 | 운영 봇도 보고 있으므로 token 세트 임대만으로 중복 수신을 막을 수 없다. 임대 동안 **운영 봇 View 권한까지** 배타 전환해야 하며 실패 시 양쪽 수신. 운영 권한 조작이 생겨 현재 목적에 부적합 |

같은 테스트 봇 세트로 여러 dcserver를 띄워 채널 필터만 나누는 것은 금지한다.
동시성은 **지정 시험 호스트의 봇 세트1 = 인스턴스1**, provider 활성 턴1이다. CI·레인·주기 실행은 시험 호스트의 같은 큐/lock을 사용한다.
다른 호스트는 ref 제출만 하며 시험 control 디렉터리에 접근하는 SSH lease helper는 만들지 않는다.
전체 동시성 확대는 별도 봇/채널 세트와 자원 예산이 준비된 뒤의 별도 선택이다.

임대는 `~/.adk/dev/control/<bot-set>.lease` **고정 파일 inode의 커널 flock**이다.
supervisor는 Python 표준 `fcntl.flock(LOCK_EX|LOCK_NB)`을 쓰고, dcserver는 기존 libc 사용 선례
(`Cargo.toml:109`, `src/cli/dcserver/replay_recovery.rs:28`)를 따라 상속 FD를 검증·보유한다. shell flock 설치·새 분산 잠금은 필요 없다.
supervisor가 FD를 보유한 채 child **exec로 dcserver에 동일 open-file-description FD를 상속**한다.
wrapper/owner 기록 전후 crash에도 살아 있는 dcserver가 lock을 유지하므로 새 인스턴스는 `DEFERRED_BUSY`다.
owner JSON은 진단 receipt이며 배타권 정본은 커널 lock이다. 파일이 남고 holder가 없으면 재획득할 수 있다.
r1의 mkdir owner 게시·owner 이전·원격 생존 확인·TTL·OWNER_UNKNOWN 교착 설계는 이 모델로 대체한다(P2-5).

FD 상속은 supervisor→dcserver exec에만 허용한다. dcserver는 startup에서 root/instance·FD의 device/inode·lease witness를 확인하고,
**provider/daemon을 띄우기 전에 FD_CLOEXEC를 다시 설정**한다. 자기 FD는 계속 보유하며 token처럼 provider env에 넣지 않는다.
`env_clear`/`setsid` 또는 daemon 동작만으로 FD가 닫힌다고 가정하지 않는다.
상속 lock의 해제는 **close-only**다. 다른 보유자가 살아 있을 때 명시적 LOCK_UN을 호출하지 않고 lease 파일을 unlink/rename-replace하지 않는다.
root별 기존 dcserver singleton은 별도로 유지한다. 후계 dcserver도 supervisor 경유로 같은 lease FD를 받아야 Gateway에 붙을 수 있다.
supervisor 자체도 동시에 한 dcserver만 spawn한다. 후계는 이전 PID·Gateway 종료를 확인한 뒤 시작하며 같은 FD를 두 live dcserver에 복제 전달하지 않는다.
test mode의 직접 launchd/tmux restart fallback은 금지한다.

`reap`는 고정 lease 파일의 device/inode를 지정한 `lsof`로 **정확한 FD 보유 후보**를 식별한다.
lsof 결과만으로 kill하거나 잠금 해제를 판정하지 않는다. 배타권은 비차단 flock 재획득, 정리 권한은 receipt의 PID·시작 baseline·root·socket/endpoint 일치로 판정한다.
`src/services/process.rs:184`의 baseline 없는 matches=true 경로를 쓰지 않고 baseline 필수 검사/tri-state probe를 사용한다.
probe 오류·receipt 불일치는 HOLD이며 임의 PID 종료 권한을 추가하지 않는다.
`down`/`reap`는 자기 dcserver·provider·PG·tmux socket·Herdr receipt 자원만 종료하고 credential을 제거한다.
daemon이 lease FD를 보유한다고 기대하지 않으므로 lock 재획득 뒤 남은 receipt 자원을 정리하기 전에는 새 Gateway를 시작하지 않는다.
이름 패턴 kill·공유 tmux server 종료·다른 lane 디렉터리 삭제는 없다. 수명/상속 crash 가정과 RED는 §4 조각0/I3a로 이관한다.

## 2. Discord E2E 상시화 — #8120

### 2.1 기준선과 실행 배치

브리프/이슈의 48개는 현재 코드와 다르다. active top-level YAML은 **43개 = live 38 + fixture 2 + unsupported-known-gap 3**,
retired 3개는 제외한다(`tests/e2e/tui_relay/scenarios/README.md:11`,
`src/services/discord/tui_prompt_relay/tests/scenario_census_e2e.rs:30`).
현재 census는 인벤토리와 fixture 시험이며, live 전체 실행의 증거가 아니다.
운영 스모크의 실효 기본 TUI subset은 E-50/E-51이나, 코드에는 조건부 E-1/E-35도 있다.
Herdr에서 E-1/E-35는 not-applicable이다(`scripts/deploy-release.sh:4514`, `:4546`, `:4668`).

| 시점·주기 초안 | 인스턴스·대상 | 제한·판정 |
| --- | --- | --- |
| 머지 전 권고 증거 수집 | 시험 호스트가 candidate ref의 worktree를 빌드. 변경 feature claim 및 네 회귀의 관련 host/provider 조합 | 관련 실물1회와 증거 첨부를 권고. **새 머지 차단 게이트는 기본 off**이며 도입은 §5 사용자 결정. E1 자체 수용의 live 증거 요구와 구분 |
| 배포 직후 | 기존 운영 E-50/E-51 유지; 지원되는 경우 기존 E-1/E-35 | 현재 추가 시간 예산 300초(`deploy-release.sh:3959`) 유지. 운영 봇·채널의 설정을 이 작업에서 확대하지 않음 |
| 새 main/배포 SHA 뒤 | 같은 SHA의 테스트 인스턴스, 회귀 핵심 묶음(반응·busy 주입·fresh thread·thread host/output) | 운영 스모크와 별개. stale candidate 결과를 현재 배포의 PASS로 재사용하지 않음 |
| 6시간마다(실측 후 활성) | 테스트 인스턴스의 live 시나리오 회전 조각; E-1/2/3/5/15/16/20/22/23/50/51/52/54/60/62/64 및 신규 회귀 | 각 시나리오의 실제 지원 cell/host만 선택. 한 번에 20분·8 provider 턴 상한 초안 |
| 야간 1개 예약 창 | 테스트 인스턴스에서 E-8/9/12/14/17/18/19/30/31/61/63 같은 restart/cancel/복원 조각, 긴 E-36 별도 조각 | dcserver restart override는 시험 receipt PID만. 운영 restart wrapper 금지. 매일 전 matrix를 돈다는 약속은 하지 않음 |
| 별도 capability/장기 기능 창 | E-4/6/7/10/13/21/35/37/53 및 나머지 지원 live 조합 | direct TUI·scheduled wake·durable·model 기능을 독립 예산으로 순환. pipe 지원 미준비는 명시 gap |

fixture E-24/25와 gap E-26/27/28은 offline/인벤토리에서 관리하고 live 실적에서 제외한다.
43개 모두를 지원하는 인스턴스라고 선언하지 않는다. phase-1은 TUI 두 provider×tmux/herdr이며 pipe/원격/DM 등은 적용표에 미지원으로 드러낸다.
같은 봇 세트 임대가 바쁘면 주기 실행이 물러난다. 배포·레인 검증이 우선이며 무한 catch-up으로 다음 창을 채우지 않는다.
E-60/61은 실제 O receipt가 필요한 프로필, E-62는 실제 Legacy receipt가 필요한 별도 프로필이다.
output 프로필을 명시하고 재기동·잔여 세션 정리 후 실행하며, 하나의 기본 프로필로 세 시나리오를 모두 덮었다고 하지 않는다.
E-11은 두 provider 턴 동시 실행을 요구하므로 phase-1 활성 턴1 계약에서는 gap이다.
동일 인스턴스·두 채널을 유지한 채 활성 턴2인 별도 예약 예외는 자원 실측과 사용자 선택 뒤에만 연다.

### 2.2 새 단언과 실제 관측

모든 제품 단언 앞에 `environment_preconditions` witness를 둔다(P2-1).
선언값과 실제 적용값을 분리하고 instance/head/boot incarnation·관측 시각·channel/provider/host를 붙인다.
busy 주입은 `ADK_BUSY_INJECT_DISCORD_CHANNELS`의 해당 시험 채널 게이트 열림·실제 busy turn·host를 확인한다
(`src/services/discord/router/intake_gate/busy_inject.rs:24`, `:41`, `:59`).
fresh thread는 preferred node 지정·ready·v1 slot 광고 없음 및 capacity_aware 경로 진입을 함께 확인한다
(`src/services/cluster/intake_router_hook/placement.rs:24`, `:96`, `src/services/cluster/execution_capacity.rs:48`, `:56`).
thread→parent binding과 O/Legacy의 evaluated output receipt도 필수다. `AGENTDESK_E2E_FEATURES`의 운영자 선언은 runtime witness가 아니다.
전제 없음/미관측은 `UNEVALUABLE(precondition)`·coverage0이며 제품 회귀 RED로 집계하지 않는다.
플래그를 뺀 음성 실행과 witness 검사를 제거한 원인 뮤턴트가 이 분류를 검증해야 한다.

신규 ID는 구현 착수 때 충돌 없는 번호를 정한다. 기존 E-63은 최종 순서/1회만 본다
(`tests/e2e/tui_relay/scenarios/E-63-input-order-across-restart.yaml:17`, `:101`).
E-14는 기존 thread ID의 stale reattach 시험이지 fresh thread 시험이 아니다
(`tests/e2e/tui_relay/scenarios/E-14-claude-tui-thread-stale-reattach.yaml:9`).
`scripts/e2e/run_tui_relay.py:1073`의 현재 지원 목록에는 아래 reaction/주입시간/host 단언이 없다.

| 계약·첫 대상 | 필수 관측과 실패 조건 |
| --- | --- |
| busy 반응(#8113) | 실제 busy hold 중 Driver가 쓴 message ID별 provider 봇의 reaction 이벤트·집합을 기록. managed busy 주입 성공은 ➕ 1개, 📬 추가/제거 0. 최종 snapshot 하나·내부 API 성공 로그만으로 통과 금지 |
| busy 주입 지연(#8114) | Discord 생성/수신·queue admission·실제 host submit receipt·provider가 현재 턴 중 입력을 관측한 witness·A 종료를 같은 message/turn identity로 연결. `submit − inbound_received ≤ 10초` 초안 및 `submit < A_end`, 정확 1회. paste 호출 성공이나 최종 답 순서만으로 통과 금지 |
| fresh thread(#8115) | 기본 execution node가 ready이고 v1 slot 광고가 없는 프로필에서 public thread 생성→첫 입력→실제 session/binding→응답. node/provider/session identity를 기대값과 비교. 기존 세션 재사용으로 cold-start를 통과시키지 않음 |
| thread host/output(#8118) | herdr parent 아래 새 thread의 실제 endpoint/workspace/pane identity·provider child를 관측. tmux parent 통제군은 실제 socket/session identity. parent와 thread의 tui_o source binding·출력 sink가 일치하며 legacy fallback 전송 0, busy 후속·취소·응답 전달까지 확인 |

네 축은 host(tmux/herdr)×provider(claude/codex)×surface(channel/thread)×state(idle/busy)다.
기본 실행 노드·managed/native origin·output path는 qualifier로 명시한다. 설정 label이나 세션 이름 추정만으로 host/output을 단언하지 않는다.
Claude에서 확인한 결함을 Codex에도 존재한다고 단정하지 않되, 양 provider의 대응 조합은 통제군으로 실행한다.
주입 지연은 기존 첫 prompt→Discord relay body latency(`docs/e2e/multi-provider-e2e.md:482`)와 다른 지표다.
같은 host monotonic clock의 receipt 차이를 주입 상한에 쓰고, Discord created_at→수신은 별도 외부 전달 지연으로 남긴다.

반응 관측 구현은 **기존 serenity를 쓰는 Rust 관측 전용 example 바이너리 + Python NDJSON 소비**로 확정한다(P2-9).
신규 경로는 `tests/e2e/tui_relay/discord_observer.rs`, Cargo target은 `e2e-discord-observer`인 명시적 `[[example]]`(test=false/bench=false)다.
기존 `Cargo.toml:47`의 serenity/client/gateway 및 tokio/serde_json을 재사용하며 Python websocket 구현·새 websocket 의존은 추가하지 않는다.
시험 호스트 wrapper가 `cargo build --example e2e-discord-observer`로 만든 실행물의 경로·SHA를 receipt에 고정한다. E1 PLAN에서 실제 prod 분류/크기를 측정한다.
Driver credential 파일로 인증하고, 시험 채널/앱 identity를 검증한 뒤 관측한다. 기본 실행/인자 누락은 live 연결하지 않는다.

시험 시작 snapshot과 reaction add/remove 이벤트를 message ID·actor ID·Gateway session/sequence로 보존한다.
sequence 연속성은 채널/reaction 필터 **전의 전체 Dispatch stream**에서 검사한다.
**SDK seam 수용 조건:** 조사한 serenity0.12.5 공개 RawEventHandler는 Event만 주고 원시 `s`는 노출하지 않으며 callback은 별도 task로 dispatch된다.
따라서 callback local counter를 Gateway sequence/수신 순서로 이름 붙이지 않는다.
E1의 첫 계약은 SDK 수신 경계에서 원시 sequence·순서를 확보하는 seam을 입증하는 것이다.
기존 공개 callback만으로 strict 이력을 구현할 수 없으면 해당 claim은 `BLOCKED_CAPABILITY`/`UNEVALUABLE(observer-sequence)`로 남기고,
단순 snapshot/재연결 감지로 전체 이력 PASS를 대체하지 않는다. 필요한 SDK seam 수정·크기는 E1 PLAN에 명시하며 이 문서에서 무검증 API를 약속하지 않는다.
녹화한 Dispatch stream의 gap/역순·재연결 주입은 반드시 UNEVALUABLE, 정상 stream은 actor별 이력으로 나오는 RED·뮤턴트 계약이다.
REST snapshot의 `me`는 Driver 자신이므로 provider 증거로 쓰지 않는다. 새 메시지부터 provider user_id 이벤트를 잇거나 reaction-user 조회로 초기 actor를 확인한다.
observer ready 이전 입력, 재연결 누락/sequence gap, REST 재동기화로 복구할 수 없는 중간 이력은 `UNEVALUABLE`이다.
현재 제품 Gateway에는 reaction intent가 없다(`src/services/discord/runtime_bootstrap.rs:100`, `:685`).
제품 전체 intent를 늘리는 대신 Driver에 필요한 intent를 부여한다.
주입/host receipt는 시험 인스턴스의 읽기 전용 observation 경로로 노출하되 비밀·본문은 기록하지 않고, 실제 provider witness와 함께 대조한다.
Herdr의 기존 N/A 관측을 tmux 증거로 대체하지 않는다(`scripts/e2e/tui_relay/herdr.py:286`, `:676`).

### 2.3 성공·실패·알림·예산

결과는 instance의 `outputs/e2e/<run-id>/`에 원시 Discord 관측·host receipt·driver report·usage를 두고,
마지막에 atomic rename으로 summary를 발행한다. summary에는 schema/run ID/head/binary hash/instance/bot-set/lease,
expected 및 executed scenario×cell, 필수 assertion별 결과, provider contacted, 실패 분류,
벽시계/실제 usage/예산 소진 여부를 담는다. 실패·timeout도 부분 관측을 보존한다.

**runner exit 0만으로 PASS하지 않는다.** 현재 driver는 fail 0이면 성공 exit가 가능하다
(`scripts/e2e/run_tui_relay.py:5486`). 새 상시 wrapper는 기존
`scripts/e2e/tui_relay/feature_accounting.py:34`의 assertion coverage/실행 상태를 재사용하여,
expected set = 실제 실행 live set, 필수 단언 complete/pass, 실제 provider contact,
head/instance/binary 일치, 실행 수 > 0을 모두 요구한다.
dry-run/fixture/unsupported/NA/skip/누락은 coverage 0이며, 필요한 칸에 있으면 `UNEVALUABLE`로 실패한다.
이 규칙은 새 runner 경계에 둔다. #7335에서 걷어낸 하네스 전체 재설계를 되살리지 않는다.

알림은 수동 조작 없이 **runner → 영속 notification outbox → 독립 시험 supervisor → Driver의 등록 운영자 DM**으로 보낸다.
이는 제품 DM 입력 기능과 별개인 알림 전송이다. recipient ID는 비밀 없는 설정으로 한 번 준비하고, provider 봇이 보는 시험 채널에는 알림을 게시하지 않는다.
job가 죽거나 timeout이면 supervisor가 summary 부재를 실패로 만든다. 네트워크/Discord 장애는 outbox를 보존하고 제한 재시도하며,
중앙 감시 소비자가 heartbeat 만료·미전달 outbox를 읽는다. 이 소비자 배선까지가 상시화 완료 조건이다.
캠페인/브리핑 점검은 summary와 coverage JSON을 읽고 실패/최근성/빈 칸을 표시한다. runner가 DAG·이슈를 자동 수정하지 않는다.
기존 운영 스모크는 `scripts/deploy-release.sh:4778`의 실패 보고와 `:506`의 알림을 유지한다.
이 경로는 release API를 쓰므로 독립 시험 runner의 전송 경로로 복사하지 않는다.

기능 단언 FAIL은 즉시 회귀 알림; provider auth/rate-limit·host 과부하·Discord 장애·임대 경합은 환경/평가 불가로 분류한다.
환경 실패만 여유 예산 안에서 1회 재시도하고 원 실패를 보존한다. 재시도로 회귀 FAIL을 지우거나 PASS로 덮지 않는다.
운영 배포의 현재 healthy 후 fail-open(`deploy-release.sh:4870`)은 유지한다.
**다음 배포 자동 차단은 별도 사용자 결정 전까지 도입하지 않고 알림부터** 시작한다.

비용 수치는 아직 실측하지 않았다. 초안은 global 활성 턴 1, 주기 job 20분/8턴,
야간 40분/16턴, 정기 합계 하루 **60분/24턴**, 예산 초과 시 새 턴 시작 금지다.
6시간 job 네 번+야간을 모두 최대치로 실행할 수는 없고, 일일 잔여 예산으로 회전한다.
E-36의 기존 phase deadline은 3540초(59분, `tests/e2e/tui_relay/scenarios/README.md:104`)이므로 일반 야간 40분 창에 넣지 않는다.
E-36 전용 예약일에는 일일60분/24턴 잔여에서 최대59분과 시나리오 최대 턴을 먼저 예약하고 다른 정기 조각을 생략한다.
각 묶음 시작 전 최대 시간·턴 예산을 예약하며 부족하면 시작하지 않고 `DEFERRED_BUDGET`으로 남긴다.
머지 전·배포 후 검증도 같은 provider 사용량 원장에 기록하되, 일일 총한도 초과 실행은 명시 예약 예산을 요구한다.
최초 대표 핵심/회전/긴 묶음을 각 1회 실제 실행해 wall time, input/output/cache usage, provider별 실측을 문서화한 뒤 주기와 token ceiling을 확정한다.
usage를 얻을 수 없는 provider는 `unknown`으로 두고 턴·시간 상한을 적용한다. token 0·비용 0으로 쓰지 않는다.
이 경우 #8120의 토큰 실측 DoD는 미완료이며, 대체 한도 채택 여부를 사용자 결정으로 남긴다.
관측 실패가 감춰진 채 비용이 늘지 않도록 pending/outbox/잔여 예산을 한 summary에서 읽을 수 있게 한다.

## 3. 사용자 관점 기능 지도 — #8123

### 3.1 표와 정본

수기 정본은 신규 `docs/user-feature-map.rows.yaml`, 시나리오 정본은 기존 YAML이다.
신규 generator가 `docs/user-feature-map.md`와 `docs/generated/user-feature-coverage.json`을 함께 만든다.
수기 문서의 '덮는 칸'을 따로 편집하지 않는다. source-of-truth 및 runbook에는 이 지도 링크만 추가한다.

| 열 | 관리 주체·의미 |
| --- | --- |
| feature_id / 사용자 기능 | 수기 안정 ID와 사용자 표현 |
| 사용자에게 보이는 것 | 정상/실패 표시·순서·정확 1회·시간 등 관측 계약 |
| 확인 명령 / scenario IDs | 시험 receipt를 요구하는 API·CLI·E2E 명령. 운영 URL 기본값 금지 |
| 적용 조합·필수 단언 | 수기 명시 tuple(host/provider/surface/state)·qualifier 및 조합별 `required_assertion_ids`. 불가능한 조합은 제외 이유 |
| 덮는 E2E 칸 / 빈 칸 수 | **시나리오 메타데이터에서 생성**. live 명세 coverage와 실행 증거 최근성을 별도 표시 |
| 담당 모듈 / 상태 차트 | repo 경로와 기존 모델 링크 또는 해당 없음 사유 |

초기 모든 기능 행은 메시지 전달, busy 주입, 반응 표시, 취소·중지, reset·clear, 스레드,
DM, 루틴, Herdr 넘김, effort·model 변경, 캠페인을 포함한다.
정본/명령표/기존 시나리오와 대조해 빠진 사용자 기능을 추가하고, 지원하지 않는 행도 삭제하지 않고 gap으로 남긴다.
현재 메타데이터는 agent_mode/coverage_class/cells/steps/assertions 수준이다
(`docs/e2e/multi-provider-e2e.md:34`). 신규 feature claim이 필요하다.

### 3.2 시나리오 메타데이터와 생성 검사

기존 YAML에 아래 형식을 추가한다. 이름은 예시이며 **선언만으로 단언 구현·실행이 생기지 않는다.**

```yaml
feature_coverage:
  schema: 1
  claims:
    - feature_id: busy-input
      host: herdr
      provider: claude
      surface: thread
      state: busy
      qualifiers: {turn_origin: managed, output_path: tui_o, input_source: allowed_bot}
      assertion_ids: [busy_injected_before_turn_end, injection_latency_within, reaction_history_matches, surface_thread_observed, state_busy_observed]
```

위 예시 operation은 E1의 **미구현 계약 예시**이므로 M1에서 claim으로 등록하지 않는다.
M1의 assertion ID는 기존 runner의 **operation명**이다. `ASSERTION_OPTIONS`를 신규 순수 모듈
`scripts/e2e/tui_relay/assertion_registry.py`로 옮겨 driver와 generator가 함께 import한다
(`scripts/e2e/run_tui_relay.py:1073`, `:1093`). full driver import·AST scrape·표 복제는 하지 않는다.
현재 assertion sibling은 requires_feature만 허용하므로 임의 invocation id를 YAML에 넣지 않는다(`run_tui_relay.py:1106`).
개별 실행 occurrence ID/report join은 E1에서 구현한다. M1은 operation 참조 무결성과 선언 coverage만 제공한다.
수기 row의 조합·qualifier별 `required_assertion_ids`가 필수 목록의 기계 판독 정본이다.
아직 없는 필수 operation은 row의 `pending_assertions`에 issue/미구현 사유·input_source 범위를 함께 명시하고 gap으로 둔다. registry에 가짜 지원 operation을 올리지 않는다.
claim은 그 목록을 모두 포함해야 coverage 한 칸으로 센다. 예를 들어 reaction 단언을 뺀 busy-input claim은 검사 실패다.
generator는 feature unknown·중복 ID·잘못된 축·수기 applicable 밖 claim·없는 assertion·필수 단언 누락을 실패시킨다.
`cells`의 무조건 Cartesian product나 wildcard로 thread/busy coverage를 부풀리지 않는다.
P2-8의 네 검사는 다음과 같이 고정한다.

1. claim host×provider가 **그 시나리오의 cells**에서 가능한 명시 조합이어야 한다. M1은 검증된 네 TUI cell 매핑만 사용하며 pipe/빈 orchestration cells는 임의 추론하지 않는다.
2. `claim.assertion_ids ⊆ (해당 YAML assertions의 실제 action keys ∩ exported runner registry)`여야 한다. registry에만 있거나 YAML에만 있는 ID 모두 실패한다.
3. thread/busy claim에는 `surface_thread_observed`/`state_busy_observed` witness를 자동 필수로 포함한다. 미구현 witness는 gap이며 metadata 선언만으로 통과시키지 않는다.
4. `--base-ref` 대비 feature/applicable/required assertion 감축은 제거 대상 안정 key와 맞는 새 `removal_reason`이 없으면 `--check` 실패다. 옛 일반 사유를 다음 감축에 재사용하지 못하며 base ref 조회 실패도 실패다.

human-only 필수 계약과 allowed_bot claim을 연결하면 coverage0이며, 잘못된 claim은 진단한다(P2-7).
coverage 조건은 live class + 지원되는 실행 lane + 해당 claim의 필수 단언 연결이며,
fixture/known-gap/retired는 별도 수치다. 선언 coverage는 실행 PASS와 구분한다.

신규 `scripts/generate_user_feature_map.py --check`는 읽기/생성 결과를 비교해 drift가 있으면 실패한다.
공식 `--write` 모드만 생성물을 갱신한다. CI contracts와 prepush static 배선에서 이 검사를 반드시 선택한다.
수기 행의 계약/적용 조합과 시나리오 claim이 어긋나면 문서 검사 실패다. feature·축·필수 단언 감축은 위 base-diff 사유 검사와 리뷰를 함께 거친다.
CI 진입점은 `scripts/ci-script-checks.sh` contracts와 `scripts/ci/script_check_inputs.py:23`의 입력 선택이다.
외부 prepush는 guards의 한 줄 Python audit/unittest만 추출하므로 contracts 등록만으로 static에 걸렸다고 하지 않는다.
guards에 한 줄 `"$PYTHON" -m unittest tests.test_user_feature_map_static`를 등록하고, 이 시험이 실제 저장소 generator `--check --base-ref <base>`를 subprocess로 실행한다.
contracts에는 본 검사와 RED 세트를 둔다. 입력 선택은 rows/생성물/scenario/registry/generator/tests를 포함하고 docs-only·선택 정보 부재에도 빠지지 않아야 한다.
base는 CI의 `TEST_LANE_BASELINE_REF`/`GFP_BASE_SHA`로 명시 전달하고, 로컬 static은 검증 가능한 origin/main base를 사용한다. base 부재·조회 실패를 초기 지도0/PASS로 축약하지 않는다.
base commit이 유효하나 rows가 아직 없는 **최초 도입**은 명시 bootstrap으로 처리한다. 이때도 §3.1 전체 기능 행이 존재하고 비어 있지 않아야 하며, commit 조회 오류와 구별한다.
감축 사유는 제거된 feature/tuple/qualifier/assertion의 안정 key에 대응시켜 base의 옛 사유로 새 감축을 허용하지 않는다.
생성 JSON은 schema_version·applicable·covered·missing·unsupported·by_feature와 source hash를 담는다.
빈 칸 = 적용 tuple − 지원 live claim이며, 실행 증거의 freshness/head/instance는 별도 summary join이다.
E1 실행 join key는 scenario×cell×claim tuple×qualifiers×assertion operation/occurrence다. 같은 네 축의 managed/native·output path·input_source를 섞지 않는다.
브리핑/캠페인은 이 JSON을 읽고 수기 숫자 사본을 만들지 않는다.

### 3.3 먼저 채울 빈 칸과 main에 이미 있는 것

첫 수용은 11-12의 네 결함이 드러낸 조합부터 채운다. 아래는 시험 우선순위이며 이 설계가 제품 결함을 함께 고친다는 뜻이 아니다.

| 기능·결함 | 첫 조합·프로필 | main 근거·남은 확인 |
| --- | --- | --- |
| busy 반응 #8113 | tmux/Claude/channel/busy, managed 턴; 이후 herdr·Codex 통제군 | `src/services/discord/router/intake_gate.rs:1004`는 기본 옵션, `src/services/discord/router/intake_queue_transaction.rs:96`은 QueueState 기본값. 새 이력 단언 필요 |
| busy 주입 #8114 | herdr/Claude/channel/busy; tmux 및 Codex 대응군 | `src/services/discord/router/message_handler/intake_turn.rs:1109`, `:1754`에 Herdr 제외. **이미 있는** `src/services/discord/router/intake_gate/busy_inject.rs:90` queued_head_covers의 Herdr 포함은 재구현하지 않음 |
| 새 스레드 #8115 | tmux/herdr × Claude × thread × idle/unowned, preferred node ready·v1 slots 없음 | `src/services/cluster/intake_router_hook/placement.rs:24`, `:96` → `src/services/cluster/execution_capacity.rs:56`의 필터. fresh start의 실제 node/session 확인 필요 |
| thread host·출력 #8118 | herdr/Claude/thread/idle와 busy; tmux 통제군, Codex 확대 | `src/services/herdr_launch.rs:56` → `src/config/session_hosts.rs:158` exact lookup; `src/services/tui_o/channel_policy.rs:74`에 parent 상속 없음. host뿐 아니라 source binding·tui_o·후속/취소까지 |

#8118의 댓글에 있는 간헐 `source not bound`도 busy thread 계약의 반례로 넣는다.
E-14의 thread 선언이나 E-50/E-51의 herdr cell만으로 위 칸이 채워졌다고 하지 않는다.

브리프·리뷰 프롬프트 틀에 함께 넣을 한 줄:

> 기능 지도 행: `<feature_id>` / 영향 조합: `<host×provider×surface×state>` / 보이는 계약·검증: `<계약, E-ID>` / 남는 빈 칸: `<수 또는 해당 없음 이유>`

틀의 정본은 #8121 도구가 관리하는 외부 coord-tools다. 이 저장소 설계에서는 문구만 제안하며 외부 파일을 고치지 않는다.

## 4. 조각·순서·RED 계약

### 4.1 조각0 — flock 상속 모델의 가정 표

상태 차트는 **해당**이다. r2에서는 기존 중앙 임대 모델을 아래 가정 표로 대체하며 TLC는 구현 조각0에서 실행한다.
설계 CLEAN 판정은 끝났으므로 별도 설계 리뷰를 추가하지 않는다. 모델·반례·뮤턴트는 구현 조각의 수용 증거다.
모델 경로는 `docs/statecharts/test-instance-lease/`로 제안한다.

| r2 가정/상태 | 구현·모델 행동 | 깨뜨릴 RED/뮤턴트 |
| --- | --- | --- |
| 지정 단일 호스트, 고정 lease inode | 다른 호스트는 ref 제출만; supervisor의 nonblocking flock 획득이 유일한 진입 | inode unlink/교체로 서로 다른 lock에 동시 진입하면 NoDualGateway 실패 |
| KernelHeld → ExecInherited → Ready → Stopping → Released | supervisor→dcserver의 동일 FD 상속; 양쪽 close 뒤 마지막 holder가 없어야 release | exec에서 CLOEXEC 유지로 child가 lock을 잃으면 wrapper crash 뒤 두 서버 반례 |
| owner receipt는 진단만 | owner 기록 전/중 crash, receipt 누락에도 kernel lock은 그대로. holder 모두 사망 시 재획득 | receipt 없는 파일을 영구 OWNER_UNKNOWN으로 간주하거나 죽은 wrapper 기록만 보고 살아 있는 child lock 탈취 |
| daemon/provider는 holder 아님 | dcserver가 자식 spawn 전에 CLOEXEC 복구. FD holder와 auxiliary receipt 자원을 별도 상태로 모델링 | FD 전파 방치로 lease가 daemon에 묶이거나, daemon이 항상 fd를 닫는다고 가정해 잘못된 해제 |
| close-only·동시 Gateway child1 | LOCK_UN 없이 FD close; 이전 dcserver 종료 후 후계 exec | 명시 LOCK_UN/동일 FD의 두 live dcserver 복제 전달로 NoDualGateway 실패 |
| reap 소유권·생존성 | lsof의 inode/FD 후보 + receipt baseline/root/socket 대조, 배타권은 flock 재획득. ProbeError=HOLD | foreign PID 선택·baseline 없는 matches 허용·보유자 남은 채 credential/root 삭제 |

불변식은 bot-set당 live Gateway dcserver≤1, 살아 있는 상속 holder의 임대 탈취0, foreign cleanup0이다.
생존성은 holder가 모두 끝나고 소유 자원 조회가 가능하면 다음 run/자기 고아 정리가 결국 진행됨이다.
wrapper SIGKILL·dcserver 단독 생존·owner 기록 전후 crash·부분 기동·down 중 crash·후계 전환을 포함한다.
r1의 SSH helper·원격 owner·TTL 상태/분산 복구 모델은 제거한다. flock만으로 auxiliary 정리나 임의 PID kill 안전까지 증명하지 않는다.

### 4.2 구현 조각과 P2 수용 계약

모든 리뷰 조각은 **prod 30파일 / 순 코드 +1500 이내**, **+1200에서 코드를 더 쓰기 전에 PLAN_READY**다.
아래는 추정치이며 test/fixture/generated/docs 제외 기준은 `pr_cap_prod.py` 정본을 따른다.
실제 측정이 추정을 넘으면 쪼개고, 한 기능 PR을 묶더라도 리뷰 조각 상한을 합산해 우회하지 않는다.

| 조각 | 범위·착지 가능 조건 | 추정 prod | RED → GREEN·수용 증거 |
| --- | --- | --- | --- |
| 0 | §4.1 flock/FD 수명 모델·현재 lock/후계/정리 진입점 전수표. TLC는 구현 때 | 0–1파일 / +0–100 | wrapper crash/상속 누락/LOCK_UN/동시 후계/외부 cleanup 반례와 원인 뮤턴트 RED → 고정 모델 GREEN |
| M1 (#8123) | 전체 기능 행·metadata schema·순수 registry export·generator·CI/static 검사. instance 없이 착지 | 5파일 / +550–850 | P2-7/8의 cells 교집합·witness·source·base 감축·생성 drift·선택 배선 RED/뮤턴트. 새 실행 단언 미등록, 네 결함 칸은 gap |
| I1 (#8124 최소 선행) | startup test manifest·config/root/독립 PG·root credential·명시 기능 프로필 | 8–12 / +600–950 | P2-1/2: 플래그 누락은 UNEVALUABLE; 외부 path/DSN·inline token·token env 상속·unknown legacy credential 거부. sentinel env 노출 제거 뮤턴트 RED |
| I2 (#8124 최소 선행) | tmux/socket·Herdr 수동 기동·provider HOME·label/API/Memento·전체 scan 경계·복원/clear/cancel | 10–16 / +800–1100 | P2-3/4: 누락 label/default API·Memento·autostart·release orphan 줄·foreign receipt가 경계를 넘지 않음. monitoring은 시험 API, down 뒤 자기 Herdr0. keychain 미확인은 BLOCKED_AUTH |
| I3a (#8124 수명) | **시험 호스트 launcher/supervisor·상속 flock·down/reap·자원 상한·health read-back**. Driver REST 미포함 | 4–7 / +550–850 | P2-5/10: up→ready→down/reap, identity 조회까지만. 동시 acquire1000회 성공1, wrapper SIGKILL 후 dcserver 생존 시 새 acquire 거부, orphan 보유자 종료 뒤 재획득·자기 자원 정리 |
| I3b (#8124 실물 수용) | **Driver REST·author/instance adapter·admission envelope·실제1턴**. lifecycle은 I3a 재사용 | 3–5 / +400–700 | P2-6/7: import/첫 전송 전 ID 누락·운영 기본값·receipt mismatch는 전송0/UNEVALUABLE. 자기 ref의 시험 호스트 worktree 실제 Discord1턴과 root credential·host/output identity 증거 |
| E1 (#8120) | Rust serenity observer/example·필수 SDK 관측 seam·주입/host/precondition adapter·네 회귀 YAML | 5–10 / +800–1150(관측 seam 실측 전 추정) | P2-1/9: precondition 누락·녹화 Dispatch gap/역순·observer epoch 오류는 UNEVALUABLE. 실제 수리 전 FAIL/수리 후 GREEN. SDK seam 미입증은 strict-history gap이며 PLAN에서 크기 재평가 |
| S1 (#8120·#8123) | 단일 supervisor 주기·예산·outbox/알림·summary·지도 소비·brief/review 틀 배선 | 4–8 / +650–1000 | 0건/skip/옛 head/누락 단언/timeout·usage unknown·알림 단절을 PASS 못 하는 RED; 실제 예약 1회+무인 실패 전달 1회 GREEN |

I2나 E1의 진입점 전수조사에서 +1200을 넘길 것이 보이면 host별/observer별로 먼저 분할한다.
I3a의 acquire1000회 시험은 attempt별로 lease 파일을 독립 open하고, winner가 barrier 동안 lock을 유지한 상태의 경합을 뜻한다.
I3b는 assertions 모듈 import 전에 세 양의 u64 ID·상호 구별·credential identity·receipt·운영 기본값 불일치를 검증한다. gate FAIL의 summary는 UNEVALUABLE(identity)이며 전송0건이다.
P2-8은 cells 밖 host/provider, registry/YAML 교집합 밖 ID, thread/busy witness 제거, 감축 사유 제거의 네 fixture와 검사 제거 뮤턴트를 각각 RED로 고정한다.
P2-3/4의 운영 상태 음성 시험은 합성 release 경로/프로세스 표/비허용 채널 fixture로 수행하며 운영 파일·프로세스·Discord 채널을 실제 조작하지 않는다.
휴면 prod 코드를 미리 쌓아 clippy no-growth를 깨지 않는다. 각 opt-in 기능은 실제 호출자와 거부 경로를 함께 착지한다.
각 구현 조각은 대상 시험·원인 제거 mutant(원복 바이트 동일)·필요 clippy·static gates 및 리뷰 ref read-back을 증거로 낸다.
**E2E는 YAML·fixture·dry-run·관측 로그만으로 완료되지 않고 실제 독립 인스턴스 실행 1회가 증거**다.
RED 확보를 위해 main의 기존 버그를 되살린 운영 서버를 띄우지 않는다. 시험 인스턴스에만 수리 전 head/원인 mutant를 쓴다.

#7335의 현재 본문 하단 **11-02 사용자 overkill-trim**은 Codex Herdr 생성·후속·회수·취소 live 각 1회 증거만 남긴다.
I3a의 수명 수용과 I3b의 instance 자체 수용(최소1턴)은 먼저 가능하고, 그 인스턴스로 #7335의 남은 capability 증거를 확보하거나 동일 조건의 기존 근거를 재사용한 뒤 Codex E1 claim을 활성화한다.
옛 허위 green/부분 증거 하네스 정교화 전체를 선행으로 요구하지 않는다.
새 wrapper의 필수 assertion gate는 이 묶음의 좁은 책임이다. #7335 live 증거 미확보 Codex 조합만 `BLOCKED_CAPABILITY`이며, instance 기반 전체 착지를 막거나 통과로 치지 않는다.

사용자 순서 #8121 → #8120·#8123 → #8122 → #8124에서 #8121은 이슈 댓글상 완료(coord-tools `79bfde4`, 12/12)다.
**기본 최소 경로:** M1과 E1 명세/RED 계약을 먼저 정리하고 #8122 설계와 병렬 진행 → 조각0·I1/I2/I3a/I3b의 작은 독립 인스턴스 선행 → E1 실제 RED/GREEN → S1.
즉 #8124의 검증을 풀어 주는 조각만 앞당기고, #8120 전체를 기다리게 하지 않는다. M1의 착지는 인스턴스 없이 가능하다.
순서 예외는 통보하고 이의가 있으면 M1 → E1 metadata/observer 명세(완료 아님) → #8122 → I1–I3b → E1 live/S1의 엄격 순서로 진행한다.
어느 경로든 #8120을 먼저 완료 처리하거나, #8124 없이 머지 전 live 검증을 했다고 기록하지 않는다.

## 5. 사용자 결정·미확인 준비물

M1 착지와 r2 문서 제출을 막는 결정은 없다. 선택은 아래 두 부류로 나누고, 필수 준비물은 실제 live 단계에서 한 번에 요청한다(P2-11).

| 분류 | 항목·진행 기본값 | 확인/질문 시점·미확정 처분 |
| --- | --- | --- |
| **기본값으로 진행·확인 요청** | 전용 public parent2개(A안). 기존8개 임대는 채택하지 않음 | 기존 사용자 결정에 맞춰 통보한다. 채널 방식 자체를 다시 질문하지 않음 |
| **기본값 제안·확인 요청** | 지정 시험 호스트 **mac-mini(집 상주·24GB)** 한 곳. 모든 다른 레인은 review ref 제출, 해당 호스트 worktree에서 빌드/실행 | 사용자에게 호스트를 확인하되 M1/모델/격리 구현은 계속 진행. headroom·nice/taskpolicy 및 global instance1/활성턴1로 운영 경합 제한 |
| **기본값으로 진행·확인 요청** | 실패 알림 우선·기존 운영 fail-open 유지, 다음 배포 자동 차단 off | 자동 배포 차단으로 바꾸려면 별도 결정. 알림/summary 구현은 진행 |
| **기본값으로 진행·확인 요청** | 6시간 회전/야간·하루60분/24턴은 실측 전 초안, 영구 주기 off | 대표 묶음 실측 후 확정 요청. unknown token 사용량은0으로 쓰지 않고 토큰 실측 DoD 미완료 |
| **기본값으로 진행·확인 요청** | M1/#8122 병렬·I1–I3b 최소 선행 | 통보 후 이의가 있으면 §4 엄격 순서. 별도 승인 대기로 구현 준비를 멈추지 않음 |
| **반드시 물어야 함** | 머지 전 실제 실행을 **모든 관련 PR의 새 차단 게이트**로 강제할지 | 기본은 권고/증거 첨부·차단 아님. 사용자 결정 전 게이트를 켜지 않음. E1 자체 수용의 실물 증거는 별개 |
| **반드시 물어야 함** | 운영 봇 Administrator/다른 권한 때문에 시험 category 경계를 만족하지 못할 때 운영 권한 변경 | 운영 권한을 자동 변경하지 않음. 해당 live 기동만 HOLD하고 사용자에게 선택 요청 |
| **반드시 준비를 요청** | 세 시험 앱·§1.3 금고 항목·독립 provider 인증/키체인·시험 채널·명령 Integrations 제한·알림 recipient | I3a identity 조회/I3b live 전 한 번에 요청. token 값/금고 내용은 요청·출력하지 않고 항목 생성/권한만 안내. 준비 안 됨은 BLOCKED_AUTH/권한 HOLD |
| **반드시 물어야 함(확대 시)** | 활성턴2(E-11)·추가 bot-set/호스트·원격/DM/private-thread 확대 | 초기 gap 유지. 자원/권한/coverage 실측 없이 자동 확대하지 않음 |

운영과 다른 비밀 없는 기능 플래그는 키/시험 적용값 목록을 제시해 한 번 확인한다. 운영 env 파일을 읽어 복사하지 않는다.

운영 접근 없이 알 수 없는 live 권한·앱 생성 상태·provider 비용은 미확인으로 남긴다.
설계 파일 커밋은 이 결정들의 승인이나 #8120/#8123/#8124의 구현 완료를 뜻하지 않는다.

<!-- doc-path-refs: proposed scripts/dev-instance.sh docs/user-feature-map.rows.yaml docs/user-feature-map.md docs/generated/user-feature-coverage.json scripts/generate_user_feature_map.py scripts/e2e/tui_relay/assertion_registry.py tests/test_generate_user_feature_map.py tests/test_user_feature_map_static.py tests/e2e/tui_relay/discord_observer.rs docs/statecharts/test-instance-lease/ -->
