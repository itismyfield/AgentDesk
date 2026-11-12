# 사용자 결과를 확인하는 시험 기반 통합 설계

설계 기준: `4e0db0cccd59169a09f25fe162d8cc1b2755e96e` (2026-11-12).
대상: [#8124](https://github.com/itismyfield/AgentDesk/issues/8124),
[#8120](https://github.com/itismyfield/AgentDesk/issues/8120),
[#8123](https://github.com/itismyfield/AgentDesk/issues/8123).
이 문서는 구현 전 계약이다. 아래의 신규 명령·키·산출물은 아직 없으며, 이 조사에서는 빌드·live 시험·운영 접근을 하지 않았다.

결론은 **독립 Discord 봇 세트와 전용 시험 채널 두 개를 한 번에 한 인스턴스가 소유**하게 하고,
그 인스턴스에서 얻는 E2E 단언을 기능 지도의 조합 칸과 연결하는 것이다.
실제 Discord·별도 봇·기존 길드를 쓰는 것은 #8124의 **11-12 15:43 KST 사용자 확정**이다.
앞선 댓글의 가짜 Discord 우선 검토는 이 결정으로 대체됐다. 운영 봇 공유·별도 길드는 선택지가 아니다.

## 1. 테스트 인스턴스 — #8124

### 1.1 실행 계약과 격리 축

레인에서 자기 worktree의 고정 HEAD를 빌드하고 띄우는 신규 진입 명령은 다음과 같이 설계한다. **지금 실행하는 명령이 아니다.**

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
| Discord | 테스트 Claude 수신 봇·Codex 수신 봇·Driver 세 앱의 고정 ID와 전용 채널 allowlist | 다른 앱 ID, 같은 token의 수신/driver 겸용, 시험 영역 밖 View/Send 권한 |
| tmux | `TMUX`·`TMUX_PANE` 제거, 전용 `TMUX_TMPDIR` 아래 실제 socket 선택; 신규 instance prefix를 세션 이름 생성·탐색에 함께 적용 | 기본 socket·`AgentDesk-*` 기존 세션 탐색/채택/키 송신·전역 kill |
| Herdr | 시험 전용 daemon·endpoint·workspace 및 tab/pane namespace; `legacy_dedicated` 모드의 socket/home/session·local execution node를 명시 | 사용자 endpoint·기본 endpoint로 fallback, 기존 사용자 workspace 채택 |
| HOME·provider | 자식용 HOME/TMPDIR/XDG_CONFIG_HOME, `CLAUDE_CONFIG_DIR`·`CODEX_HOME` 및 provider 출력·rollout 경로를 instance에 귀속 | 실제 HOME의 설정/skills/MCP/rollout을 쓰거나 운영 세션을 발견·재부착 |
| 실행 노드·외부 연결 | 시험 DB의 local node만 seed. 원격 node·peer bootstrap·루틴·캠페인 worker·connector는 기본 off | 운영 cluster/remote host/API·공유 MCP에 쓰기. 원격 배치를 확인하려면 독립 시험 노드 프로필을 별도 승인·구현 |

프로필과 receipt는 비밀을 포함하지 않는다. receipt에는 instance ID, canonical root, worktree/head,
binary hash, API/PG port, PG data dir/role, bots/channel IDs, tmux socket/prefix,
Herdr endpoint/workspace, child PID·시작 identity를 기록한다.
`env -i`에 준하는 allowlist로 자식 환경을 만들고 필요한 PATH·locale·시험 HOME·인증만 주입한다.
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
| bot token | `src/services/discord/agentdesk_config.rs:141`은 inline/credential fallback; `src/cli/run.rs:148`은 단일 `AGENTDESK_TOKEN` 경로 | config 다중 봇용 신규 `token_env`를 구현. test mode는 inline/credential fallback 금지, missing env 즉시 실패. 단일 CLI token은 상속 제거 |
| tmux | `src/domain/provider/naming.rs:6`의 고정 `AgentDesk` prefix; `src/services/platform/tmux.rs:30`은 `-u`만 지정 | 공통 command 생성과 raw tmux 진입점 전수 확인. socket·prefix를 launch/scan/attach/cancel/clear/recovery 모두에 전달 |
| Herdr | `src/services/herdr_launch.rs:56`, `src/config/session_hosts.rs:158`은 host 선택 시 exact channel 조회 | 시험 endpoint 설정과 실제 session host가 일치. 기본 endpoint는 test mode에서 오류 |
| provider 설정 sync | `src/bootstrap.rs:43`, `src/lib.rs:267`의 일반 RunServer initialize는 MCP sync 수행 | 명시 `dcserver` 경로는 이 initialize를 통과하지 않는 현재 동작 유지. 이를 무조건 발생하는 dcserver 부작용으로 오인하지 않음. HOME·provider home 격리는 별도로 필요 |
| HOME·host 탐색 | `src/services/provider_auth_profile.rs:189`, `:485`의 profile home; `src/services/codex/tui_session_launch.rs:149`의 HOME fallback; `src/services/cluster/session_discovery.rs:707`의 tmux 스캔 | test HOME 아래 named profile만 허용. 시험 Herdr home은 `src/services/session_host/herdr/managed_config/path.rs:47`의 개인 경로 보호와 충돌하지 않는 형제 경로로 배치 |
| store·Herdr default | `src/services/tui_o/store/mod.rs:182`의 root/o_store; `src/config/session_hosts/user_default.rs:31`의 기본 socket | root와 HOME을 모두 강제. 시험 프로필에서 user_default 빈 endpoint 금지 |
| 하네스 재시작·CLI | `scripts/e2e/tui_relay/restart_guard.py:10`, `:19`는 미확인 포트를 release로 선택; `scripts/e2e/tui_relay/herdr.py:27`, `:69`, `:247`은 기본 CLI 경로/endpoint 사용 가능 | 신규 instance adapter는 restart target을 receipt PID로만 결정하고 endpoint·lane absolute binary를 전달. 미지정은 시작 전 오류. 기존 release/dev 분기를 시험 인스턴스에 사용하지 않음 |
| 기존 dcserver 중복 정리 | `src/cli/dcserver.rs:291`은 root marker substring 판정, `:454`는 종료 경로; `src/cli/dcserver/replay_recovery.rs:28`은 root별 lock | 신규 test startup은 고정 길이 UUID root와 정확한 instance identity 검증을 먼저 적용. substring을 test cleanup 권한으로 쓰지 않음. 기존 일반 경로의 결함 범위는 넓히지 않고 별도 후속으로 남김 |
| 실행·복원·정리 | 기존 session 이름/host 경로는 여러 소비자가 사용 | up뿐 아니라 서버 재시작·startup recovery·provider respawn·cancel/stop/clear/down에 같은 manifest가 전달되는지 전수표·음성 시험으로 확인 |

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
비밀 참조만 있는 env mapping을 wrapper가 만들고 `agent-op run`으로 주입한다.
수신 env 이름은 `ADK_TEST_DISCORD_CLAUDE_TOKEN`, `ADK_TEST_DISCORD_CODEX_TOKEN`,
Driver는 `ADK_TEST_DISCORD_DRIVER_TOKEN`으로 설계한다. token 값/항목 내용은 문서·manifest·argv·로그에 남기지 않는다.
빈 값·다른 앱 ID는 접속 전/identity 조회에서 오류다. token을 출력하는 debug·shell xtrace는 허용하지 않는다.

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
초기 프로필은 public thread만 포함하며 DM·private thread는 기능 지도에 gap으로 남긴다.

세 앱만으로 기존 하네스를 그대로 쓸 수는 없다. `src/server/routes/discord.rs:189`의 message GET은 announce credential을 읽고,
`scripts/e2e/tui_relay/discord.py:125`의 control 전송은 notify 봇을 지정한다.
I3에 **Driver token으로 실제 Discord REST 입력/조회/thread 생성**을 하는 instance adapter를 넣고,
receipt의 세 앱 ID를 기존 author override(`scripts/e2e/tui_relay/assertions.py:27`)로 전달한다.
사용자 입력 경로의 회귀는 headless deliver API로 대체하지 않는다.
Driver가 allowed_bot_ids에 있으므로 SETUP/TEARDOWN marker를 보내면 새 턴을 깨울 수 있다.
bookkeeping은 local run ID·새 message snowflake·관측 창에만 기록한다. 실패 알림도 provider가 보는 parent/thread로 보내지 않는다.

### 1.4 채널 두 안과 동시 사용

| 안 | 흐름 | 비용 | 위험·평가 |
| --- | --- | --- | --- |
| **A. 전용 parent 2개(권고)** | Claude/Codex parent 아래 임시 public thread를 만든다. 같은 parent의 host를 시험 프로필로 tmux/herdr 번갈아 지정 | 채널 두 개·최초 권한 설정, 전용 봇 세 앱 | 운영 스모크와 물리적으로 분리. 운영 봇 View deny를 처음부터 증명하기 쉽다. host 전환 때 child/session 잔여가 없어야 함 |
| B. 기존 e2e 채널 8개 임대 | 실제 channel ID 기준 중앙 임대를 운영 스모크·수동 E2E·테스트 인스턴스 모두가 획득 | 새 parent 없음. 모든 기존 진입점에 임대 적용, provider/host별 배치 점검 필요 | 운영 봇도 보고 있으므로 token 세트 임대만으로 중복 수신을 막을 수 없다. 임대 동안 **운영 봇 View 권한까지** 배타 전환해야 하며 실패 시 양쪽 수신. 운영 권한 조작이 생겨 현재 목적에 부적합 |

같은 테스트 봇 세트로 여러 dcserver를 띄워 채널 필터만 나누는 것은 금지한다.
권고 동시성은 **전체 호스트를 통틀어 테스트 봇 세트 1개 = 인스턴스 1개**, 호스트당 1개, provider 활성 턴 1개다.
CI·레인·주기 실행이 동일 임대에 들어간다. 추가 동시성은 별도 봇 세트와 전용 채널 세트를 준비한 뒤 확대한다.

중앙 임대 저장소는 운영 runtime 밖의 지정 시험 호스트 `~/.adk/dev/control/` 하나로 둔다.
원자 `mkdir`/exclusive-create로 bot-set + channel-set을 한 번에 획득하고, local host/instance 예약도 같은 순서로 잡는다.
다른 호스트의 레인은 제한된 SSH helper로 **이 시험 디렉터리만** 접근한다.
TTL 만료만으로 임대를 빼앗지 않는다. 보유 host의 정확한 receipt PID·시작 identity가 종료됐음을 확인할 수 없으면 `BUSY/OWNER_UNKNOWN`으로 둔다.
대기 job은 실행 0건을 PASS로 쓰지 않고 `DEFERRED_BUSY`와 다음 예약을 남긴다.

`down`은 receipt에 든 자기 dcserver·provider·PG·Herdr만 순서대로 종료하고, 종료 확인 뒤 임대를 해제한다.
고아 회수는 dry-run 목록→소유 identity 검증→자기 자식의 종료 확인→자기 root 보관/삭제 순서다.
이름 패턴 kill·공유 tmux server 종료·다른 lane 디렉터리 삭제는 추가하지 않는다.
죽은 owner가 입증된 고아만 자동 회수하고, host 불통은 알림과 보류다. 이 새 수명/임대 계약은 §4 조각 0의 모델·독립 리뷰 대상이다.

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
| 매 변경 머지 전 | candidate worktree 테스트 인스턴스. 변경 feature claim 및 네 회귀의 관련 host/provider 조합 | 아래 신규 단언의 실제 실행 1회 이상. 변경 영향과 무관한 전체 matrix 강제 없음 |
| 배포 직후 | 기존 운영 E-50/E-51 유지; 지원되는 경우 기존 E-1/E-35 | 현재 추가 시간 예산 300초(`deploy-release.sh:3959`) 유지. 운영 봇·채널의 설정을 이 작업에서 확대하지 않음 |
| 새 main/배포 SHA 뒤 | 같은 SHA의 테스트 인스턴스, 회귀 핵심 묶음(반응·busy 주입·fresh thread·thread host/output) | 운영 스모크와 별개. stale candidate 결과를 현재 배포의 PASS로 재사용하지 않음 |
| 6시간마다(실측 후 활성) | 테스트 인스턴스의 live 시나리오 회전 조각; E-1/2/3/5/15/16/20/22/23/50/51/52/54/60/62/64 및 신규 회귀 | 각 시나리오의 실제 지원 cell/host만 선택. 한 번에 20분·8 provider 턴 상한 초안 |
| 야간 1개 예약 창 | 테스트 인스턴스에서 E-8/9/12/14/17/18/19/30/31/61/63 같은 restart/cancel/복원 조각, 긴 E-36 별도 조각 | dcserver restart override는 시험 receipt PID만. 운영 restart wrapper 금지. 매일 전 matrix를 돈다는 약속은 하지 않음 |
| 별도 capability/장기 기능 창 | E-4/6/7/10/11/13/21/35/37/53 및 나머지 지원 live 조합 | direct TUI·dual cell·scheduled wake·durable·model 기능을 독립 예산으로 순환. pipe 지원 미준비는 명시 gap |

fixture E-24/25와 gap E-26/27/28은 offline/인벤토리에서 관리하고 live 실적에서 제외한다.
43개 모두를 지원하는 인스턴스라고 선언하지 않는다. phase-1은 TUI 두 provider×tmux/herdr이며 pipe/원격/DM 등은 적용표에 미지원으로 드러낸다.
같은 봇 세트 임대가 바쁘면 주기 실행이 물러난다. 배포·레인 검증이 우선이며 무한 catch-up으로 다음 창을 채우지 않는다.

### 2.2 새 단언과 실제 관측

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

반응 관측은 **독립 Driver의 실제 Discord Gateway observer**를 시험 채널에 한정해 추가한다.
시험 시작 snapshot과 reaction add/remove 이벤트를 message ID·actor ID·Gateway session/sequence로 보존한다.
sequence 연속성은 채널/reaction 필터 **전의 전체 Dispatch stream**에서 검사한다.
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
관측 실패가 감춰진 채 비용이 늘지 않도록 pending/outbox/잔여 예산을 한 summary에서 읽을 수 있게 한다.

## 3. 사용자 관점 기능 지도 — #8123

### 3.1 표와 정본

수기 정본은 신규 `docs/user-feature-map.rows.yaml`, 시나리오 정본은 기존 YAML이다.
신규 generator가 `docs/user-feature-map.md`와 machine-readable coverage JSON을 함께 만든다.
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
      qualifiers: {turn_origin: managed, output_path: tui_o}
      assertion_ids: [busy-before-end, injection-latency, injection-reaction]
```

`assertion_ids`는 runner가 지원하며 실제 report에 개별 결과를 남기는 안정 ID를 참조한다.
수기 row의 조합·qualifier별 `required_assertion_ids`가 필수 목록의 기계 판독 정본이다.
claim은 그 목록을 모두 포함해야 coverage 한 칸으로 센다. 예를 들어 reaction 단언을 뺀 busy-input claim은 검사 실패다.
generator는 feature unknown·중복 ID·잘못된 축·수기 applicable 밖 claim·없는 assertion·필수 단언 누락을 실패시킨다.
`cells`의 무조건 Cartesian product나 wildcard로 thread/busy coverage를 부풀리지 않는다.
coverage 조건은 live class + 지원되는 실행 lane + 해당 claim의 필수 단언 연결이며,
fixture/known-gap/retired는 별도 수치다. 선언 coverage는 실행 PASS와 구분한다.

신규 `scripts/generate_user_feature_map.py --check`는 읽기/생성 결과를 비교해 drift가 있으면 실패한다.
공식 write 모드만 생성물을 갱신한다. CI contracts와 prepush static 배선에서 이 검사를 반드시 선택한다.
수기 행의 계약/적용 조합과 시나리오 claim이 어긋나면 문서 검사 실패다. 필수 feature·축을 빼서 빈 칸 수를 낮추는 수정도 리뷰 대상이다.
생성 JSON은 schema_version·applicable·covered·missing·unsupported·by_feature와 source hash를 담는다.
빈 칸 = 적용 tuple − 지원 live claim이며, 실행 증거의 freshness/head/instance는 별도 summary join이다.
join key는 scenario×cell×claim tuple×qualifiers×assertion ID다. 같은 네 축의 managed/native·output path를 섞지 않는다.
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

이번 문서 변경의 상태 차트는 **해당 없음**(제품 상태 전이를 바꾸지 않는 조사·설계).
다만 제안한 instance lifecycle/lease에는 상태가 생기므로 구현 **조각 0단계**에서 작은 모델을 먼저 둔다.
`Absent → Reserved → Starting → Ready → Stopping → Reaped`와 `OwnerUnknown/Failed`를 모델링하고,
동일 bot-set 소비자 최대 1, 살아 있는 owner 임대 탈취 0, foreign cleanup 0,
죽은 owner의 확인 가능한 자원은 결국 회수됨을 검사한다. partition·PID 재사용·down 중 crash·시작 일부 실패를 넣는다.
모델의 값·진입점은 구현 조사 전수표에서 가져오고, 새 cleanup 권한은 수용 시험이 있는 자기 receipt 범위로만 제한한다.

모든 리뷰 조각은 **prod 30파일 / 순 코드 +1500 이내**, **+1200에서 코드를 더 쓰기 전에 PLAN_READY**다.
아래는 추정치이며 test/fixture/generated/docs 제외 기준은 `pr_cap_prod.py` 정본을 따른다.
실제 측정이 추정을 넘으면 쪼개고, 한 기능 PR을 묶더라도 리뷰 조각 상한을 합산해 우회하지 않는다.

| 조각 | 범위·착지 가능 조건 | 추정 prod | RED → GREEN·수용 증거 |
| --- | --- | --- | --- |
| 0 | lifecycle/lease 모델·소유 진입점 전수표·독립 설계 리뷰 | 0–2파일 / +0–150 | 현재 read→O_TRUNC lease 동시 획득 반례와 잘못된 orphan 회수 모델 RED; 새 규칙 GREEN. 현재 `scripts/e2e/tui_relay/lease.py:32`를 다중 레인 안전장치로 재사용하지 않음 |
| M1 (#8123) | 전체 기능 행·metadata schema·generator·CI/static 검사. instance 없이 먼저 착지 가능 | 3–6 / +450–750 | 잘못된 claim·없는 assertion·문서 drift·fixture live 둔갑이 검사 FAIL; 원복 GREEN. 실행 칸은 여전히 미검증으로 표시 |
| I1 (#8124 최소 선행) | startup test manifest·config/root/DB/env token 검증·독립 PG routing | 8–14 / +700–1050 | inherited DSN·missing config·sibling fallback·외부 path·잘못된 bot identity가 접속 전 FAIL. live 가능으로 노출하지 않는 opt-in 경계부터 |
| I2 (#8124 최소 선행) | tmux/socket·Herdr·provider HOME·recovery/clear/cancel의 격리 전달 | 8–14 / +750–1100 | default socket/endpoint·다른 instance receipt·복원 경로의 fallback FAIL. 기존 운영 default 동작 유지 |
| I3 (#8124 수용) | launcher·중앙 임대·down/reap·자원 상한·전용 봇/채널 준비·Driver REST/author/instance adapter·host self-check | 5–9 / +750–1100 | 두 lane 경합·부분 기동 crash·PID 재사용·owner 불통 RED. **자기 worktree의 실제 Discord 1턴**, 운영 target 접근 0의 경계 증거, 자기 자원 down/reap GREEN |
| E1 (#8120) | Driver reaction observer·주입/host 관측 adapter·네 회귀 YAML·필수 assertion 결과 | 5–9 / +700–1050 | 각 제품 수리 전 코드에서 실제 시험 인스턴스 실행 1회 FAIL, 수리 후 동일 프로필 GREEN. 기존 버그가 먼저 고쳐지면 원인 제거 mutant로 RED와 원복 byte hash 증명 |
| S1 (#8120·#8123) | 단일 supervisor 주기·예산·outbox/알림·summary·지도 소비·brief/review 틀 배선 | 4–8 / +650–1000 | 0건/skip/옛 head/누락 단언/timeout·usage unknown·알림 단절을 PASS 못 하는 RED; 실제 예약 1회+무인 실패 전달 1회 GREEN |

I2나 E1의 진입점 전수조사에서 +1200을 넘길 것이 보이면 host별/observer별로 먼저 분할한다.
휴면 prod 코드를 미리 쌓아 clippy no-growth를 깨지 않는다. 각 opt-in 기능은 실제 호출자와 거부 경로를 함께 착지한다.
각 구현 조각은 대상 시험·원인 제거 mutant(원복 바이트 동일)·필요 clippy·static gates 및 리뷰 ref read-back을 증거로 낸다.
**E2E는 YAML·fixture·dry-run·관측 로그만으로 완료되지 않고 실제 독립 인스턴스 실행 1회가 증거**다.
RED 확보를 위해 main의 기존 버그를 되살린 운영 서버를 띄우지 않는다. 시험 인스턴스에만 수리 전 head/원인 mutant를 쓴다.

#7335의 현재 본문 하단 **11-02 사용자 overkill-trim**은 Codex Herdr 생성·후속·회수·취소 live 각 1회 증거만 남긴다.
I3의 instance 자체 수용(최소1턴)은 먼저 가능하고, 그 인스턴스로 #7335의 남은 capability 증거를 확보하거나 동일 조건의 기존 근거를 재사용한 뒤 Codex E1 claim을 활성화한다.
옛 허위 green/부분 증거 하네스 정교화 전체를 선행으로 요구하지 않는다.
새 wrapper의 필수 assertion gate는 이 묶음의 좁은 책임이다. #7335 live 증거 미확보 Codex 조합만 `BLOCKED_CAPABILITY`이며, instance 기반 전체 착지를 막거나 통과로 치지 않는다.

사용자 순서 #8121 → #8120·#8123 → #8122 → #8124에서 #8121은 이슈 댓글상 완료(coord-tools `79bfde4`, 12/12)다.
**권고 최소 경로:** M1과 E1 명세/RED 계약을 먼저 정리하고 #8122 설계와 병렬 진행 → I1/I2/I3의 작은 독립 인스턴스 선행 → E1 실제 RED/GREEN → S1.
즉 #8124의 검증을 풀어 주는 조각만 앞당기고, #8120 전체를 기다리게 하지 않는다. M1의 착지는 인스턴스 없이 가능하다.
순서를 엄격히 유지한다면 M1 → E1 metadata/observer 명세(완료 아님) → #8122 → I1–I3 → E1 live/S1로 나눌 수 있다.
어느 경로든 #8120을 먼저 완료 처리하거나, #8124 없이 머지 전 live 검증을 했다고 기록하지 않는다.

## 5. 사용자 결정·미확인 준비물

기술 조사/문서 착지를 막는 결정은 없다. 구현 발사 전에 아래를 한 번에 확인한다.

| 선택 | 권고·이유 | 미확정이면 가능한 일 |
| --- | --- | --- |
| 전용 채널 vs 기존 8개 임대 | 전용 public parent 2개. 기존 채널 임대는 운영 봇 Discord 권한 전환까지 필요 | M1·모델·비밀 없는 startup 검증 가능; live 불가 |
| 봇·금고 준비 | 수신 Claude/Codex+Driver 3앱 및 §1.3 세 항목. 실제 앱/채널 ID·유효 권한·알림 DM recipient와 독립 provider 인증 준비 | env token/identity 음성 검사까지 가능. 실제 token/항목 내용 요청·출력 금지 |
| 실행 호스트·계정·한도 | 중앙 시험 임대 호스트 1곳, 글로벌 instance 1·활성 턴1; 6시간 회전/야간 및 하루60분·24턴은 실측 전 초안 | 대표 묶음 각1회 비용 측정 후 한도 확정. 실측 없는 영구 주기 활성화 금지 |
| 회귀 뒤 배포 처분 | 알림 우선, 기존 운영 fail-open 유지. 다음 배포 자동 차단은 별도 선택 | 실패/heartbeat·outbox 무인 알림은 구현 가능 |
| 사용자 우선순서의 최소 선행 예외 | #8124 I1–I3만 E1 live보다 먼저. M1/#8122는 병렬 | 엄격 순서 대안은 §4와 같으며 #8120 완료가 늦어짐을 명시 |

운영 접근 없이 알 수 없는 live 권한·앱 생성 상태·provider 비용은 미확인으로 남긴다.
설계 파일 커밋은 이 결정들의 승인이나 #8120/#8123/#8124의 구현 완료를 뜻하지 않는다.

<!-- doc-path-refs: proposed scripts/dev-instance.sh docs/user-feature-map.rows.yaml docs/user-feature-map.md scripts/generate_user_feature_map.py -->
