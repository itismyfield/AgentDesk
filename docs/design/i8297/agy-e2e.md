# #8297 agy 헤드리스 E2E 셀과 테스트 채널

조사 기준은 `origin/main`의 `d97f7b9d655e4b0c8d83bb89e192968a5ea0f504`이다. 추가 셀은 `agy-headless` 하나다. Discord 텍스트 채널 하나에서 `agy --print --output-format stream-json`을 사용한다. `agy-parity-1121`과 `agy-priority-1121`에 따라 이 셀의 legacy 기준선을 #8274 활성화 전에 확보한다. #8274와 #8296의 증거까지 갖추면 코디네이터가 4채널 전환을 판정한다. E2E 배선 설계이므로 런타임 상태 차트 변경은 해당 없다.

**개정 r2(설계 리뷰 P1 1건 반영).** 이 셀은 릴리스 dcserver가 아니라 #8124 시험 instance 안에서만 돈다. 봇은 Test-Antigravity(앱 ID `1573471163663785994`)이고 토큰은 시험 runtime root의 credential 파일로만 공급한다. 운영 설정·운영 봇 허용 목록·운영 credential·운영 Memento는 건드리지 않는다. 바뀐 절은 「격리 경계」, 「agy 셀」의 접점 문단, 「운영 변경 목록」, 「조각과 크기」다.

## 현재 구조

아래 좌표는 모두 조사 기준 SHA의 저장소 파일이다.

| 대상 | 현재 구조 | 근거 |
| --- | --- | --- |
| E2E 에이전트·채널 예시 | Claude `adk-claude-pipe-e2e`, `adk-claude-tui-e2e`, `adk-claude-tui-2-e2e`. Codex `adk-codex-pipe-e2e`, `adk-codex-tui-e2e`. Claude 오케스트레이터 `adk-e2e-orchestrator`가 팬아웃한다. 각 행은 provider, 채널 ID·이름, 워크스페이스, 프롬프트 파일, runtime을 갖는다. | `agentdesk.example.yaml:168–261` |
| 운영 에이전트 수 | 브리프의 운영 집계는 오케스트레이터 포함 8개이고 provider는 Claude·Codex다. 정적 예시·생성기의 일치하는 명시 행은 위 6개다. 배포 로그 fixture의 `adk-codex-tui-2-e2e` 문자열은 관측 당시 세션명이며 현재 바인딩 근거로 쓰기 어렵다. 운영 집계의 나머지 두 ID 및 매핑은 이 자료로 확정할 수 없다. | `scripts/setup-multi-provider-e2e.sh:113–135`, `docs/e2e/multi-provider-e2e.md:17–26`, `tests/e2e/tui_relay/fixtures/turn_smoke/turns-clean.dcserver.log:14` |
| Discord 생성·격리 | 생성기는 `ADK E2E` 카테고리 아래 5개 워커와 오케스트레이터 채널을 만들고 ID를 반환한다. 운영 절차에서 코디네이터가 ID와 전용 프롬프트를 바인딩한다. 채널 스키마는 provider 키 아래 상세 설정을 허용하고, 실행 시 등록기는 provider를 정규화하고 숫자 채널 ID로 바인딩한다. | `scripts/setup-multi-provider-e2e.sh:107–135`, `docs/e2e/multi-provider-e2e.md:690–750`, `src/config/agent_memory.rs:4–38`, `src/config/agent_channels.rs:14–35`, `src/config.rs:557–602`, `src/services/discord/agentdesk_config.rs:537–574` |
| 하네스 셀과 바인딩 | 하네스는 Claude·Codex pipe, TUI, Herdr 6개를 받는다. `claude-tui`는 tmux 전용 `adk-claude-tui-2-e2e`, `claude-herdr`는 `adk-claude-tui-e2e`를 찾는다. Codex TUI·Herdr는 `adk-codex-tui-e2e`를 찾는다. Herdr은 워크스페이스 표식과 실제 host 증거를 별도로 본다. 수동 matrix의 기본 선택은 pipe/TUI 4개이고 같은 agent/provider 키에서 ID를 찾는다. | `scripts/e2e/run_tui_relay.py:63–70`, `scripts/e2e/run_tui_relay.py:454–492`, `scripts/e2e/run_multi_provider_matrix.py:32–48`, `scripts/e2e/run_multi_provider_matrix.py:118–145` |
| 배포 스모크 | 기본 셀은 `claude-tui`·`codex-tui`다. startup recovery 및 API 준비 후, full 모드에서 E-1 게이트와 채널 조회, E-35, E-50/51 순으로 검사한다. 채널에 Herdr 매핑이 있으면 실제 채널 경로가 판정에 반영된다. E-50/51은 E-1이 해석한 채널을 전제로 하며, Codex ID는 별도로 해석한다. 하네스의 실기 보고서와 해당 실행 구간 로그를 judge가 확인한다. | `scripts/deploy-release.sh:142–150`, `scripts/deploy-release.sh:4472–4558`, `scripts/deploy-release.sh:4657–4835`, `scripts/e2e/post_deploy_turn_smoke.py:111–213`, `scripts/e2e/post_deploy_relay_continuity.py:221–266` |
| E-50·E-51 | 각각 T1 응답 뒤 T2, `!clear` ACK 뒤 AFTER_CLEAR를 보낸다. 각 표식은 provider 응답에 한 번 나타나야 한다. E-50 TUI/Herdr는 clear 전후 세션 변경도 검사한다. E-51 judge는 warm-follow-up timeout·unexpected kill·cold-resume fallback도 본다. 별도의 continuity 명령은 E-9/E-19를 요구한다. | `tests/e2e/tui_relay/scenarios/E-50-claude-clear-and-turns.yaml:29–82`, `tests/e2e/tui_relay/scenarios/E-51-codex-clear-and-turns.yaml:29–68`, `scripts/e2e/post_deploy_turn_smoke.py:185–207`, `scripts/e2e/post_deploy_relay_continuity.py:41–43` |
| 배포 결과 | 건강 확인에서 `DEPLOY_OK`를 기록한다. 이후 기능 스모크는 실패한 시나리오를 `FAIL`로 올리고 경고·증거·로컬 이슈 초안을 남기는 fail-open 단계다. 상시 실행의 배포 요청은 그 뒤 별도 후크로 등록된다. | `scripts/deploy-release.sh:3893–3899`, `scripts/deploy-release.sh:4712–4725`, `scripts/deploy-release.sh:4838–4869`, `scripts/deploy-release.sh:4930–4979` |
| 상시 순환 | `CELLS`는 `claude-tui`, `codex-tui`, `claude-herdr`, `codex-herdr`. R1–R5 × 지원 셀 × 순서 보존한 profile 중 한 step을 실행한다. 배포 inbox가 우선이며, due는 `last_scheduled_at + rotation_interval_s`를 확인한다. 미지원은 `BLOCKED_CAPABILITY`이고, 완료한 scheduled 결과가 cursor를 전진시킨다. `daily_full_at`은 현 runner에서 비워 두는 값이다. | `scripts/e2e/continuous/plan.py:12–79`, `scripts/e2e/continuous/runner.py:38–42`, `scripts/e2e/continuous/runner.py:55–83`, `scripts/e2e/continuous/runner.py:111–131` |
| 릴리스 호스트 주기 | launchd가 300초마다 tick을 깨운다. 릴리스 호스트 프로필 문서의 핵심 P 묶음 주기는 43,200초이며 같은 head·주기 칸의 요청을 합친다. 문서상 R1–R5 순환은 실측 전이라 꺼져 있고 P의 기존 실기 항목은 Claude/Herdr E-50·Codex/Herdr E-51이다. 기기 설정의 현재 적용 여부는 운영 확인 사항이다. | `scripts/launchd/com.agentdesk.e2e-continuous.plist:8–24`, `scripts/e2e/continuous/release_host.py:78–102`, `docs/e2e-continuous/bundles.schema1.json:63–82`, `docs/e2e-continuous/release-host.md:24–57` |
| 상시 실행 제약 | 릴리스 드라이버는 Herdr 매핑에 들지 않은 채널을 tmux로 분류하며 사용량 읽기는 Claude/Codex 전사만 지원한다. agy codec은 성공한 프로세스 종료와 Done 본문을 확인할 수 있고, 현재 E2E 예산 계약에 필요한 턴별 토큰 카운터는 제공하지 않는다. 채널 lock 키는 현재 정확히 두 부모 ID를 받는다. | `scripts/e2e/continuous/release_host.py:40–46`, `scripts/e2e/continuous/smoke_driver.py:126–161`, `scripts/e2e/continuous/usage.py:96–105`, `src/services/stream_json_cli/codec.rs:174–258`, `src/services/stream_json_cli/codec.rs:263–325`, `scripts/e2e/continuous/channel_lock.py:26–33` |

| 현재 시간 상한 | 배포 스모크 | 상시 릴리스 호스트 문서값 |
| --- | --- | --- |
| Claude | E-50 셀 phase 120초 기본값. T1/T2 응답 대기 각각 60초, clear ACK 20초, AFTER_CLEAR 50초. | `claude-herdr.deadline_s=120` |
| Codex | E-51 셀 phase 150초 기본값. T1/T2 각각 75초, clear ACK 20초, AFTER_CLEAR 60초. | `codex-herdr.deadline_s=150` |
| 공통 | startup recovery·gateway readiness 각 120초, E-35 별도 900초 기본값. 셀 phase는 각 step의 합과 별도로 강제하는 상한이다. | `max_run_s=600`, tick/grace 각 300초, usage 기록 대기 기본 20초. Pipe 및 별도 회전 셀의 고정 릴리스 상한은 정의되어 있지 않다. |

시간 근거는 `scripts/deploy-release.sh:4007–4022`, 위 E-50/51 YAML의 대기 단계, `scripts/e2e/run_tui_relay.py:357–362`, `scripts/e2e/run_tui_relay.py:5464–5471`, `docs/e2e-continuous/release-host.md:24–53`이다.


<!-- doc-path-refs: proposed scripts/e2e/agy_cell/driver.py -->

## 격리 경계 (리뷰 P1-1)

전용 워크스페이스와 채널만으로는 격리가 되지 않는다. 봇 토큰은 워크스페이스가 아니라 runtime root를 따라 읽히고(`src/credential.rs:46–57`, `src/runtime_layout/paths.rs:11–21`), 선언이 없으면 root는 `~/.adk/release`다(`src/config/runtime_root_guard.rs:3–34`). 운영 Memento 자동 주입도 시험 instance 선언이 있을 때만 빠진다(`src/config.rs:2342–2357`). 그래서 agy 셀은 `docs/test-infra.design.md:38–50`·`docs/dev-instance.md:13–18`의 시험 instance 경계 안으로 들어간다. 새 격리 장치를 만들지 않고 기존 경계를 agy 단일셀 프로필로 넓힌다.

**프로필.** 새 프로필 이름은 `agy-headless`다. `bots`는 `antigravity`(Test-Antigravity 앱 ID)와 `driver`(Test-Notify 앱 ID) 둘, `channels`는 agy 시험 채널 하나다. Test-ClaudeCode·Test-Codex는 이 프로필에 없다. 두 봇을 함께 띄우면 다른 Mac의 `tui-live` instance와 같은 봇으로 Gateway에 이중 접속하게 되기 때문이다. 시험 instance 임대는 호스트 안의 flock이라 호스트 사이 중복을 막지 못한다(`scripts/dev_instance/lease.py:12–13`).

| 경계 | 현재 코드(기준 SHA) | agy 셀이 들어가는 접점 — S1이 바꾸는 곳 |
| --- | --- | --- |
| 시험 runtime root | root는 `$HOME/.adk/dev/instances/<uuid>` 고정이고 override가 없다(`docs/dev-instance.md:13–18`). 바이너리는 `AGENTDESK_ROOT_DIR`·`AGENTDESK_CONFIG`·manifest 세 env가 모두 있어야 선언을 받는다(`src/config/test_instance.rs:50–73`). 경로에 `.adk/release`가 끼면 거절한다(`src/config/test_instance.rs:100–133`). | 변경 없음. agy 셀의 root·config·API·PG는 이 선언에서만 나온다. 릴리스 root로 떨어지는 경로가 없다는 것은 기존 거절이 담당한다. |
| test_instance 선언 | manifest 키 집합은 고정이고 선택 키는 `claude_login` 하나다(`src/config/test_instance.rs:157–182`). `validate_profile`은 `tui-live`·세 봇·채널 정확히 둘만 받는다(`scripts/dev_instance/profile.py:40–53`). | `profile.py`에 프로필 표를 두고 `agy-headless`의 봇 키·채널 수를 정한다. manifest 선택 키에 `agy_login`을 더한다. witness의 필수 게이트(`scripts/dev_instance/profile.py:98–106`)도 프로필별로 둔다. 옛 `tui-live` manifest는 그대로 읽힌다. |
| credential basename | `resolve_bot_token`은 inline 값이 없으면 `<root>/credential/<bot>_bot_token`을 읽는다(`src/services/discord/agentdesk_config.rs:147–154`). 쓰기 허용 이름은 다섯 개로 고정이다(`scripts/dev_instance/files.py:13–15`, `src/cli/dev_instance.rs:53–60`). 기동 승인은 credential 디렉터리의 파일 이름 집합이 네 개와 정확히 같을 것을 요구한다(`src/config/test_instance/lifecycle.rs:266–290`, `scripts/dev_instance/identity.py:203–208`). | basename은 **`antigravity_bot_token`** 이다. 시험 설정의 봇 키가 provider 이름이라(`scripts/dev_instance/config.py:150–158`) 기존 규칙대로 정해진다. 이 파일은 시험 root에만 있고 Test-Antigravity 전용이다. 허용 이름 두 곳에 한 줄씩 더하고, 필수 이름 집합을 프로필에서 뽑는다: `agy-headless`는 `antigravity_bot_token`·`announce_bot_token`·`notify_bot_token` 셋. control key를 만드는 이름 목록(`src/config/test_instance/lifecycle.rs:516–570`)도 같은 집합을 쓴다. inline token·token env는 만들지 않는다. |
| identity 확인 | `up`은 stdin admission의 앱 ID와 각 credential로 조회한 `GET /users/@me` id가 같아야 진행한다(`scripts/dev_instance/identity.py:30–39`, `:178–200`). 바이너리는 같은 파일의 device·inode·크기·시각을 다시 대조한다(`src/config/test_instance/lifecycle.rs:291–326`). Driver는 운영 봇 ID 넷을 거절한다(`scripts/dev_instance/driver_admission.py:12–13`, `:35–37`). | admission 이름 집합과 역할 대조(`identity.py:180–182`)를 프로필 기준으로 바꾼다. agy 셀은 `profile.bots.antigravity` = admission의 `antigravity_bot_token` = Discord가 돌려준 id = `1573471163663785994` 네 값이 모두 같아야 한다. 운영 Antigravity 봇 ID를 `OPERATING_BOT_IDS`에 더한다(값은 코디네이터가 S1 브리프로 준다, 비밀 아님). |
| env_clear·차단 목록 | supervisor는 상수와 선언만으로 자식 env를 만든다(`scripts/dev_instance/manifest.py:171–200`). 바이너리는 자기 env에 `MEMENTO_*`·`OP_*`·`AGENT_OP_*`·Discord token·`*_BOT_TOKEN`·`AGENTDESK_TOKEN`·provider API key가 있으면 시작을 거절한다(`src/config/test_instance/environment.rs:146–165`, `:198–227`). provider 자식은 `env_clear()` 뒤 선언값만 받는다(`src/config/test_instance/environment.rs:311–355`). | **빈 곳이 하나 있다.** agy 자식은 `Command::new` 뒤 overlay env만 얹어 띄우므로 dcserver env를 그대로 물려받는다(`src/services/stream_json_cli/runner.rs:61–69`, `src/services/stream_json_cli/dialects/agy.rs:65–97`). runner에서 `apply_overlay_to_command`(`src/services/provider_auth_profile.rs:446–458`)와 같은 `configure_child_command`를 거치게 한다. instance 밖에서는 이 함수가 아무것도 바꾸지 않는다(`environment.rs:311–312`). |
| provider home·바이너리 | 시험 instance는 `claude`·`codex` 홈만 안다(`src/config/test_instance/environment.rs:241–249`). org의 auth profile은 정확히 둘이어야 한다(`:96–121`). PATH는 `<worktree>/target/debug:/usr/bin:/bin`이라 `agy`가 잡히지 않는다(`:168–196`, `src/services/platform/binary_resolver.rs:828–832`). | 새 child `src/config/test_instance/agy_cell.rs`가 프로필별 provider 집합, `antigravity` 홈, `agy_login` 검증을 맡는다. `agy` 경로는 선언한 절대경로를 `AGENTDESK_AGY_PATH`로 준다(기존 override 규칙 `src/services/platform/binary_resolver.rs:1282`, Claude 선례 `environment.rs:80–85`). `environment.rs`(850줄)에는 호출만 둔다. |
| Memento 차단 | 설정의 `mcp_servers`가 비어 있고 memory backend가 file이어야 선언을 받는다(`src/config/test_instance/environment.rs:27–31`, `:402–415`, `src/config.rs:2496`). 기본 Memento 주입(`src/config.rs:2343`), Memento backend(`src/services/memory/memento.rs:102`), 기동 probe(`src/services/memory/runtime_state.rs:116`), MCP credential 감시(`src/services/discord/mcp_credential_watcher.rs:294`)는 선언이 있으면 모두 꺼진다. | 서버 쪽은 변경 없음. 남는 경로는 agy CLI가 자기 HOME에서 읽는 MCP 설정이다(`src/services/mcp_config/antigravity.rs:42–43`). 아래 「agy 로그인 홈」으로 막는다. `provider_has_memento_mcp`의 Antigravity 분기(`src/services/mcp_config.rs:89–95`)는 instance 안에서 false여야 하며 시험으로 고정한다. |

**agy 로그인 홈.** 릴리스 서버는 운영자의 실제 HOME 아래 `.gemini/config/mcp_config.json`에 운영 MCP 서버를 동기화한다(`src/bootstrap.rs:56`, `src/services/mcp_config/antigravity.rs:22–43`). agy 자식이 실제 HOME을 쓰면 운영 Memento 설정을 읽게 된다. 그래서 agy 자식의 HOME은 실제 HOME도 instance HOME도 아닌 전용 디렉터리 `$HOME/.adk/dev/agy-login`으로 고정한다. instance root는 head마다 새로 생기므로 로그인 상태를 root 안에 두면 배포마다 브라우저 로그인이 필요해진다. `agy_login` 선언은 `{mode, login_home, cli}` 세 키다. 바이너리는 `login_home`이 `<instances의 부모>/agy-login`과 같고, symlink·`.adk/release` 경로가 아니며, `cli`가 실행 가능한 일반 파일인지 본다. Claude 로그인 선언이 실제 HOME을 허용한 것(`src/config/test_instance/environment.rs:437–496`)보다 좁다. `up`은 admission 때 이 홈에서 MCP 서버가 0개임을 확인한다(`mcp_config.json`이 없거나 서버 항목이 비어 있음). 확인 실패나 판독 불가는 `BLOCKED`이고 입력은 0이다. 이 홈에는 AgentDesk가 아무것도 쓰지 않는다.

- [U] agy 인증이 keychain에 있어 새 HOME에서도 그대로 통하는지, 전용 홈에서 한 번 로그인해야 하는지는 이 조사에서 실행해 보지 않았다. 어느 쪽이든 선언은 같다. O4의 확인 명령이 가른다. 인증이 없으면 `BLOCKED_AUTH`이고 skip이나 PASS가 아니다(`docs/test-infra.design.md:59–60`).
- [U] agy가 `mcp_config.json` 말고 다른 파일로도 MCP 서버를 읽는지는 확인하지 못했다. S1은 `agy mcp list`의 실제 출력으로 admission 판독을 고정하고, 서버가 하나라도 보이면 거절한다.

**실패하면 입력 0.** root 선언·identity·credential 집합·env·로그인 홈 검사 중 하나라도 실패하면 `up`이 Gateway에 붙기 전에 끝난다. E-65·E-66·E-67은 Discord 입력과 HTTP 호출을 한 번도 하지 않고 `UNEVALUABLE(precondition)` 또는 `BLOCKED_*`로 남는다. 시험 토큰이 없을 때 릴리스 root나 운영 토큰으로 넘어가는 경로는 없다. 봇 이름으로 읽는 파일은 선언된 root 하나뿐이고(`src/credential.rs:54–57`), 빠진 파일은 기동 승인의 집합 비교에서 걸린다.

**증명 시험.** 합성 fixture로만 한다. 실제 Discord·운영 파일은 쓰지 않는다.

| 시험 | 기대 |
| --- | --- |
| 올바른 시험 root + Test-Antigravity id + credential 셋 | 선언·admission 통과 |
| 운영 봇 ID, 다른 앱 ID, credential 누락·네 번째 파일 추가, `tui-live` admission 프레임 | 거절, Gateway 접속 0, credential 읽기는 선언 root 안에서만 |
| root가 `.adk/release` 아래, `AGENTDESK_ROOT_DIR` 누락 | 선언 거절, 릴리스 credential 경로 open 0 |
| dcserver env에 `MEMENTO_ACCESS_KEY`·`OP_SERVICE_ACCOUNT_TOKEN`·`ANTIGRAVITY_BOT_TOKEN` | 시작 거절 |
| agy 자식 spawn을 가짜 실행 파일로 관측 | env에 차단 키 0, HOME이 로그인 홈, PATH가 선언값 |
| 로그인 홈이 실제 HOME, symlink, MCP 서버 1개 | admission 거절, agy spawn 0 |
| 뮤턴트: root 검사·id 대조·runner의 `configure_child_command`·로그인 홈 MCP 검사를 하나씩 제거 | 각각 RED |
| 옛 `tui-live` manifest·네 credential | 기존과 같은 결과(프로필 표 도입 전후 동일) |

## agy 셀

제안 시나리오 ID는 E-65(배포), E-66(상시 짧은 답), E-67(HTTP 입력)이다. 세 시나리오는 같은 셀 식별·읽기 전용 preflight·Discord 본문 판정기를 공유한다. 셀 이름은 `agy-headless`, provider 식별은 `antigravity`다. 시험 instance 안의 agent ID는 다른 시험 agent와 같은 규칙으로 provider 이름 `antigravity`다(`scripts/dev_instance/config.py:131–149`). runtime은 `pipe`, `tui_hosting=false`이고 Herdr 채널 매핑에는 넣지 않는다. `src/domain/provider/catalog/rows.rs:156–178`의 alias·실행 어댑터와 `src/services/stream_json_cli/dialects/agy.rs:27–52`의 spawn 옵션을 사용한다.

| 시나리오 | 입력과 관측 |
| --- | --- |
| E-65 / T1 짧은 답 | Test-Announce가 agy 시험 채널에 평문 한 건을 보낸다. `도구 없이 이 문자열만 답해: AGY-{run}-T1`. provider 본문 전체가 이 문자열이며 종료를 확인한다. |
| E-65 / L 긴 답 | `도구·마크다운 없이 START-{run} 한 줄, 다음 줄에 01부터 55까지 NN과 공백과 0123456789를 8번 반복한 문자열을 한 줄씩, 마지막 줄 END-{run}. 설명은 빼.` 검증기는 기대 본문 약 4.6천 ASCII자를 자체 생성한다. 실제 provider 본문을 담은 서로 다른 Discord ID 3개 이상을 요구한다. 앞 2개가 완료 전에 관측되어 고정된 분할 ID인지 확인하고, 최종 스냅샷에서 같은 ID의 본문과 순서를 대조한다. 시작·끝 표식, 55개 줄, 전체 재조립 본문을 모두 비교한다. 앞 조각 유실이나 중복을 끝 표식만으로 통과시키지 않는다. 분할 자격을 관측하지 못하면 실패로 기록하고 별도 fixture에서 rollover 2개 뒤 authoritative Done 경로의 RED/GREEN을 보인다. |
| E-65 / T2 연속 턴 | L의 종료·입력 해제를 확인한 즉시 같은 채널·대화에 `도구 없이 이 문자열만 답해: AGY-{run}-T2`를 보낸다. T2는 새 입력·turn ID, 같은 대화 fingerprint, 정상 종료로 이어져야 한다. T1→L→T2의 provider 호출 3개로 짧은 답, 긴 답, 연속 턴을 모두 본다. |
| E-66 / 주기용 | T1과 동일한 판정을 표식 `AGY-{run}-DAILY`로 1턴만 실행한다. E-65 PASS가 있어도 주기 실행 기록은 독립적으로 남긴다. |
| E-67 / 1b 이후 | 시험 instance의 `self_url`에 `POST /api/agents/antigravity/turn/deliver`를 보낸다. `source=imessage`, 새 `origin_id`, 같은 시험 채널·provider를 명시하고 `AGY-{run}-HTTP` 한 줄 답을 요청한다. 전송 ACK, origin에 귀속된 실행, Discord 답 1회와 종료를 묶어 판정한다. 실제 iMessage나 Discord 입력 게시를 거치지 않는다. 대상 주소가 instance의 선언 포트(18791–18806)가 아니면 호출하지 않는다(`src/config/test_instance/environment.rs:378–385`). 1b의 유효 기능을 확인하기 전에는 `BLOCKED_CAPABILITY`로 남기고 HTTP 호출 횟수는 0이다. ACK만 수신하거나 송신 결과가 불명확하면 조사하고 재송신을 시작하지 않는다. 1b가 요구하는 기능 플래그가 프로필 허용값(`scripts/dev_instance/profile.py:8–14`)에 없으면 S4가 그 값을 더한다. |

**입력 주체.** Discord 입력은 Test-Announce의 평문이다. announce 봇의 메시지는 `DISPATCH:` 표식 없이 턴을 시작하고(`src/services/discord/dispatch_policy.rs:226–254`), instance는 announce 봇 ID를 자기 root의 `announce_bot_token`에서 해석한다(`src/services/discord/router/intake_gate.rs:338–358`). 조회도 Test-Announce로 한다. 시험 Driver의 REST 클라이언트는 지금 notify 전송과 announce 조회만 있으므로(`scripts/dev_instance/discord_rest.py:46–71`) announce 전송 하나를 더한다. 전송 전에 Driver admission(`scripts/dev_instance/driver_admission.py:27–62`)이 통과해야 한다. E-67은 `src/server/routes/docs/inventory/endpoints/part_03.rs:105–127`, `:157–169`의 요청·`input-result` 계약을 바탕으로 하고 조각 1b가 추가하는 수용 증거를 쓴다.

**종료·정확히 1회 판정** — 실행 시작의 최신 Discord ID와 instance의 head·기동 nonce, 시험 채널의 입력 ID 또는 HTTP origin을 고정한다. Test-Antigravity의 봇 ID와 해당 턴의 게시 ID로 본문 조각을 귀속한다. 입력·announce echo·완료 크롬은 본문 집계에서 제외한다. 동일 message ID의 edit는 마지막 본문으로 합치고, 새 ID에 복제된 본문은 중복으로 센다. 짧은 답은 완전한 본문 ID 한 개, 긴 답은 순서 있는 한 벌의 조각과 일치하는 완전한 논리 답 한 개가 기준이다. 성공 Done과 완료 푸터 또는 writer 완료 영수증을 턴과 대조한다. mailbox·대기열 idle 및 instance API의 `GET /api/turn-lease/antigravity/{channel_id}` idle 응답을 확인하고, legacy에서는 같은 채널의 읽기 전용 watcher-state와 대조해 남은 inflight가 없음을 확인한다. 진단 데이터가 없으면 lease·mailbox의 정상 응답까지 교차 확인하고 오류 응답은 idle로 해석하지 않는다. #8274 활성화 뒤에는 writer의 같은 턴 terminal commit과 입력 해제를 함께 본다. 마지막 턴은 종료 뒤 5초 동안 같은 관측 경계에서 늦은 중복을 검사한다. 잘못된 채널·instance 재기동·관측 누락·lease 잔존·부분 본문·timeout은 필요한 PASS 증거를 채우지 못한 결과다. 좌표는 `src/server/routes/turn_lease.rs:15–30`, `src/server/routes/docs/inventory/endpoints/part_07.rs:581–604`, `scripts/e2e/post_deploy_turn_smoke.py:367–401`, `src/services/discord/turn_bridge/stream_loop/content_arms.rs:310–359`, `src/services/discord/inflight/store.rs:473–542`이다.

**시간 상한 제안** — 시나리오 상한은 E-65가 360초(preflight 최대 30초 + 60/180/60초 + 판정 30초), E-66과 E-67이 각각 120초(최대 20초 + 80초 + 20초)다. 여기에 instance 기동·admission 120초와 정지 20초를 더해 `up --wall-timeout`은 E-65 500초, E-66·E-67 260초로 준다. supervisor 상한 600초 안이다(`scripts/dev_instance/lifecycle.py:22–23`). 같은 단조시계 절대 기한의 잔여 시간으로 각 요청·대기·판정을 자르고 기한 직후 응답은 timeout으로 기록한다. instance 바이너리 빌드는 이 상한 밖이며 따로 1,800초로 끊는다. legacy 첫 실기에서 기동, 입력→첫 본문·마지막 본문·종료와 wall time을 남기고 상한을 다시 본다. Google 쿼터는 호출 수를 우선 제어한다. E-65는 배포당 한 시도에 프롬프트 3개, E-66은 주기 칸당 1개, E-67은 1b 수용 실행당 1개를 예약한다. 짧은 전용 시스템 프롬프트와 일반 텍스트만 쓴다. agy는 tool allowlist를 지원하지 않으므로(`src/services/stream_json_cli/dialects/agy.rs:15–36`) 도구 호출 여부는 격리 수단이 아니다. 격리는 위 instance 경계가 맡고, 도구 관측은 「리뷰 P2 판단」을 따른다.

**배포 접점** — E-65는 배포 스크립트 안에서 동기로 돌지 않는다. 시험 instance는 `.adk/release` 밖 worktree의 `target/debug/agentdesk`만 띄울 수 있어(`src/config/test_instance.rs:100–133`, `scripts/dev_instance/lifecycle.py:212–218`) 배포 head의 별도 빌드가 필요하고, 그 빌드를 배포 안에 넣으면 배포가 그만큼 늘어난다. 그래서 기존 배포 뒤 후크(`scripts/deploy-release.sh:4978`, `scripts/e2e/continuous/after_deploy.sh`)가 agy 전용 config에 `deploy-<stamp>` 요청(head 포함)을 넣고 끝난다. 20초 안에 못 넣으면 그 단계만 끊는 기존 계약을 그대로 쓴다. agy 전용 tick이 요청을 받아 전용 worktree를 그 head로 맞추고, 빌드하고, instance를 준비해 E-65를 돌린다. 결과는 배포 stamp·head에 묶인 terminal 기록 한 건으로 agy 상태 디렉터리의 summary·outbox에 남고 `E2E_MONITOR`가 읽는다. 셀이 켜진 호스트에서 instance 준비 실패·admission 거절·채널 바쁨·timeout·judge 실패는 모두 `E-65=FAIL`(사유 포함)이다. 요청 뒤 2,900초(빌드 1,800 + 실행 500 + tick 300×2) 안에 terminal 기록이 없으면 다음 tick이 `E-65=FAIL(NO_RESULT)`를 쓴다. 셀을 켜지 않은 Mac은 요청을 넣지 않고 비대상으로 기록한다. 배포 자체는 지금처럼 fail-open이다(`scripts/deploy-release.sh:4930–4973`).

- S2 구현: 후크 분기는 `scripts/e2e/continuous/after_deploy.sh`가 기존 config 검사보다 먼저 `AGENTDESK_E2E_AGY_CONFIG`를 보고 `python3 -E -s -m scripts.e2e.agy_cell enqueue`만 부른다. `deploy-release.sh`는 이미 이 후크를 부르므로 바꾸지 않는다. 요청의 소비자는 S2의 `agy_cell drain`이다(설계 리뷰 P2-c). drain은 가장 새 요청 하나만 실행하고 앞 요청은 `SUPERSEDED(<새 요청>)`로 닫으며, 2,900초를 넘긴 요청은 실행 없이 `FAIL(NO_RESULT)`로 닫는다(P2-d). 주기적으로 drain을 부르는 launchd tick은 S3가 넣는다. E-65 명세는 tui_relay 카탈로그에 넣지 않고 `scripts/e2e/agy_cell/E-65.yaml`에 둔다.

- [U] DoD 2의 "실패면 배포 결과에 FAIL로 남는다"를 배포 스크립트 출력이 아니라 배포 stamp에 묶인 사후 기록으로 채운다. 동기 실행을 원하면 배포마다 debug 빌드 한 번이 배포 시간에 더해진다. 코디네이터가 사용자 확인을 받는다.

**상시 접점** — `plan.py`의 `CELLS`에 `agy-headless`를 추가하고 R1에 E-66을 등록한다. 공식 `scripts/e2e/generate_continuous_catalog.py --write`로 시나리오 카탈로그를 동기화한다. 셀 호스트의 agy 전용 launchd tick과 상태 디렉터리(`~/.adk/e2e-continuous-agy`)에서 기존 runner를 재사용한다. `rotation_interval_s=43200`, `core_interval_s=null`, `daily_full_at=null`, `max_run_s=500`, tick 300초, 대상 필터 `agy-headless`를 적용해 E-66만 도는 R1 step을 선택한다. 기존 릴리스 호스트 config와 inbox에는 이 셀을 넣지 않는다. 현재 P 요청이나 전체 R1–R5 cursor가 agy 순번을 밀지 않는다. 대상 head는 릴리스 서버 `/api/health/detail`이 알려 준 배포 head다(읽기 전용, `scripts/e2e/continuous/release_host.py:78–102`). driver는 그 head의 instance가 없으면 준비부터 한다. 배포 중·임대 경합으로 입력 0인 연기는 같은 due를 유지하고 다음 tick에서 다시 평가한다. 전송을 시도한 작업은 시간과 결과를 기록하고, 그 요청 ID는 다음 12시간 칸까지 재송신을 막는다. 최근 24시간에 해당 head의 live E-66이 0이면 증거의 지연·누락 또는 실행 불가로 노출한다. 일정은 정상 상태에서 12시간마다 하루 2회 기회를 주며, 실기 acceptance는 연속 24시간에 최소 1회 실행과 보존된 결과다. 배포의 E-65가 이 분모를 채우지 않는다.

- 상시 첫 활성화는 agy에만 요청 수·wall time을 계량하는 명시적 예산 프로필을 쓴다. 초기 상시 한도는 rolling 24시간에 2회·240초이며 입력 직전 한 번씩 예약한다. 새 프로세스·자정·새 head에서 예약을 초기화하지 않는다. 배포와 HTTP 수용 시도는 별도로 표시해 전체 호출을 집계한다. 토큰 사용량은 실제 계측이 없으면 `null`로 기록하고 `usage_complete=false`를 유지한다. E-66의 기능 판정과 실기 횟수는 개별 occurrence로 공개한다. driver·runner에서 `request_time` 예산 증거와 측정된 usage를 별개 필드로 검증한다. 기존 요약의 전체 usage PASS와 Claude·Codex의 예산 기준은 그대로다. agy의 0턴·이전 head·불완전 단언으로 기능 PASS를 만들 수 없다. 근거는 `scripts/e2e/continuous/budget.py:26–76`, `scripts/e2e/continuous/summary.py:114–184`이다.
- 상호 배제는 두 겹이다. 시험 instance 임대(`scripts/dev_instance/lease.py:12–13`)가 같은 호스트의 instance 기동을 하나로 묶고, 바쁘면 `DEFERRED_BUSY`로 입력 없이 물러난다(`scripts/dev_instance/__main__.py:84–85`). runner의 채널 lease는 `channel_lock.py`의 별도 명시 scope에서 guild와 부모 ID 한 개, 고정 bot 집합을 받는다. 기존 두 부모 ID의 키는 유지한다(`scripts/e2e/continuous/channel_lock.py:26–33`, `:80–103`).
- 배포 락과의 관계는 실행 주체별로 정한다. 배포가 넣은 E-65 요청은 배포 락이 풀린 뒤의 tick에서만 실행한다. 배포 락을 쥔 프로세스 안에서 도는 agy 실행은 없으므로 자기 배포를 외부 배포로 오인할 일이 없다. 독립 E-66·E-67은 배포 락이 잡혀 있으면 입력 0으로 연기한다(`scripts/e2e/continuous/release_host.py:49–61`). 실행 중에 배포가 시작되면 이미 보낸 턴은 끝까지 판정하고 추가 입력은 하지 않는다. 시험 instance는 릴리스 서버와 별개 프로세스라 배포 재시작에 끊기지 않는다.

## 리뷰 P2 판단

| 지적 | 판단 | 한 줄 |
| --- | --- | --- |
| P2-1 도구 호출 관측의 출처 없음 | 최소 가드는 이번 개정에 얹음. 계수 접점은 기각 | "도구 호출은 실패로 관측"을 뺐다. 결과에는 `tool_observation=unobserved`만 쓰고 무도구 PASS·`tool_calls=0`을 기록하지 않는다. codec에 도구 step 계수를 넣는 일은 관측 정확도만의 개선이고, 격리는 instance 경계가 맡으므로 하지 않는다. |
| P2-2 자기 배포 락과 외부 배포 락 구분 | 이번 개정에 얹음 | E-65가 배포 프로세스 밖에서 돌게 되어 자기 락 문제가 사라졌다. 주체별 계약 세 줄을 「상시 접점」에 적었다. S3의 시험은 배포 락 보유 fixture에서 독립 E-66·E-67 입력 0, 락 해제 뒤 E-65 실행을 본다. |

## 운영 변경 목록

O1–O5 각 착수 전 메인에 항목·호스트·되돌리기(`enabled=false`, `down`)를 알린다.

모두 릴리스 runtime 밖의 변경이다. `agentdesk.yaml`·운영 봇 `auth`·운영 credential·운영 launchd 작업은 바꾸지 않는다. 직전 설계의 운영 봇 허용 목록 수정 계획은 없앴다. 셀 호스트는 mac-book 한 곳을 제안한다(직전 설계의 배정 유지). 다른 Mac에는 아래 항목을 적용하지 않는다. 그 Mac은 배포 후크에서 config가 없어 아무 일도 하지 않는다(`scripts/e2e/continuous/after_deploy.sh:7–8`).

명령의 `<...>`는 코디네이터가 채우는 비밀 아닌 값이다. 비밀값은 `agent-op read`의 stdout을 다음 명령의 stdin으로만 넘긴다. 변수에 담지 않고, 인자·env·로그에 남기지 않으며, `set -x`를 켜지 않는다. op 참조는 필드 id를 쓴다(라벨 `자격 증명`은 참조에서 오류가 난다).

**O1 Discord 채널 한 개 — 메인, announce 봇.** Test category(`1570615295284879391`, `docs/e2e-continuous.design.md:27`) 아래에 만든다. 릴리스 CLI의 채널 생성은 announce 봇으로 나간다(`src/cli/direct.rs:800–809`, 인자는 `src/cli/args.rs:405–421`). 운영 봇 권한은 바꾸지 않는다.

```sh
~/.adk/release/bin/agentdesk discord channel-create \
  --name adk-agy-headless-e2e \
  --category-id 1570615295284879391 \
  --topic "Antigravity headless stream-json E2E. Test-Antigravity 전용, 시험 instance 전용."
# 출력 JSON의 id가 <AGY_CHANNEL_ID>다.
```

Test-Antigravity가 이 채널에서 View·Read History·Send·Embed·Attach를 갖는지는 O3 뒤의 preflight가 Test-Antigravity 토큰으로 직접 조회해 확인한다. 권한이 모자라면 Test category의 시험 앱용 역할을 Test-Antigravity에 준다. 운영 봇·`@everyone`·운영 역할의 권한은 손대지 않는다.

**O2 프로필 파일 — 코디네이터, 셀 호스트.** 비밀이 없다. `<NOTIFY_APP_ID>`와 `<ANNOUNCE_APP_ID>`는 기존 시험 instance가 쓰는 Test-Notify·Test-Announce 앱 ID다.

```sh
install -d -m 700 ~/.adk/dev/agy-cell
umask 077
cat > ~/.adk/dev/agy-cell/profile.json <<'JSON'
{"schema_version": 1, "name": "agy-headless",
 "bots": {"antigravity": "1573471163663785994", "driver": "<NOTIFY_APP_ID>"},
 "channels": ["<AGY_CHANNEL_ID>"],
 "features": {"env": {"ADK_BUSY_INJECT": "off", "ADK_BUSY_INJECT_DISCORD_CHANNELS": [],
                      "ADK_BUSY_INJECT_STASH_CHANNELS": [], "ADK_QUEUED_HEAD_INJECT": "0",
                      "ADK_TURN_PRESENCE_TAIL": "0", "ADK_INTAKE_ROUTING_MODE": "disabled",
                      "ADK_IMSG_OBLIGATIONS": "0"},
              "tui_writer": false, "tui_turn": false, "capacity_aware": true, "preferred_node": "local"}}
JSON
cat > ~/.adk/dev/agy-cell/admission.json <<'JSON'
{"kind":"admission","schema":1,"identities":{"antigravity_bot_token":"1573471163663785994","announce_bot_token":"<ANNOUNCE_APP_ID>","notify_bot_token":"<NOTIFY_APP_ID>"}}
JSON
```

**O3 instance 준비와 credential 주입 — 코디네이터, 셀 호스트, head마다.** S2가 착지한 뒤에는 `python3 -E -s -m scripts.e2e.agy_cell ensure --head <sha40>`이 같은 일을 하고, 아래는 그 명령이 하는 일을 손으로 푼 것이다. `<WT>`는 `.adk/release` 밖의 전용 worktree(`~/.adk/dev/agy-cell/worktree`)이고 배포 head로 맞춘 깨끗한 상태여야 한다. 빌드는 캠페인 규칙대로 headroom 대기와 cargo-shim을 거친다.

```sh
unset TMUX TMUX_PANE
WT=~/.adk/dev/agy-cell/worktree
git -C "$WT" fetch origin && git -C "$WT" checkout --detach <sha40>
bash <skill>/scripts/seed_target.sh "$WT" && bash <skill>/scripts/wait_headroom.sh
(cd "$WT" && PATH=<skill>/scripts/cargo-shim:$PATH cargo build --bin agentdesk)
export PATH="$WT/target/debug:$PATH"          # credential 쓰기는 PATH 첫 디렉터리의 레인 바이너리를 쓴다
cd "$WT"
UUID=$(scripts/dev-instance.sh prepare "$WT" --profile ~/.adk/dev/agy-cell/profile.json --agy-login \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["instance_id"])')
agent-op read 'op://AgentDesk/Test-Antigravity/credential' \
  | scripts/dev-instance.sh credential --instance "$UUID" --name antigravity_bot_token
agent-op read 'op://AgentDesk/Test-Announce/credential' \
  | scripts/dev-instance.sh credential --instance "$UUID" --name announce_bot_token
agent-op read 'op://AgentDesk/Test-Notify/credential' \
  | scripts/dev-instance.sh credential --instance "$UUID" --name notify_bot_token
scripts/dev-instance.sh validate --instance "$UUID"
```

`validate`는 선언만 확인한다(`--require-credentials`는 옛 driver 3개 모드 전용이라 쓰지 않는다). credential 3개 집합과 Discord identity의 최종 확인은 agy 프로필에서 필수인 `up` admission이 한다. `--agy-login`과 `agy-headless` 프로필은 S1이 넣는다. Test-Announce·Test-Notify의 op 참조는 메모의 "같은 필드 구성"을 따른 것이므로 실행 전에 항목의 필드 id를 확인한다. 이전 head의 instance는 새 instance가 `validate`를 통과한 뒤 `scripts/dev-instance.sh down --instance <이전 uuid>`로 지운다. `down`은 그 root의 credential까지 지운다(`docs/dev-instance.md:149–152`).

**O4 agy 로그인 홈 — 코디네이터, 셀 호스트, 한 번.** 턴을 쓰지 않는 조회로 인증과 MCP 0개를 확인한다. 인증이 없다고 나오면 사용자가 이 HOME으로 한 번 로그인한다(브라우저 인증이라 사용자 손이 필요하다).

```sh
install -d -m 700 ~/.adk/dev/agy-login
env -i HOME="$HOME/.adk/dev/agy-login" PATH=/opt/homebrew/bin:/usr/bin:/bin LANG=C.UTF-8 agy models
env -i HOME="$HOME/.adk/dev/agy-login" PATH=/opt/homebrew/bin:/usr/bin:/bin LANG=C.UTF-8 agy mcp list
# 인증 없음일 때만, 사용자가 직접:
# env -i HOME="$HOME/.adk/dev/agy-login" PATH=/opt/homebrew/bin:/usr/bin:/bin TERM="$TERM" LANG=C.UTF-8 agy
```

**O5 identity 확인과 단계 활성화 — 코디네이터, 셀 호스트.** 먼저 기동 한 번으로 Test-Antigravity identity를 확인한다. `up`은 admission의 앱 ID와 토큰으로 조회한 id가 다르면 Gateway에 붙기 전에 거절한다.

```sh
HEAD=$(git -C "$WT" rev-parse HEAD); SHA=$(shasum -a 256 "$WT/target/debug/agentdesk" | cut -d' ' -f1)
( cat ~/.adk/dev/agy-cell/admission.json; cat ) | scripts/dev-instance.sh up --instance "$UUID" --head "$HEAD" \
  --binary-sha256 "$SHA" --wall-timeout 260 --stop-grace 20 --resources ~/.adk/dev/agy-cell/resources.json
# 다른 셸: python3 -E -s -m scripts.e2e.agy_cell preflight --instance "$UUID"   (S2 제공, 입력 0)
```

`resources.json`은 `docs/dev-instance.md:123–126`의 형식 그대로다. 그 뒤 단계는 다음 순서다.

1. legacy: S2 착지 뒤 `python3 -E -s -m scripts.e2e.agy_cell run --scenario E-65 --instance "$UUID"`로 첫 실기를 돌려 시간을 잰다. 통과하면 `~/.adk/e2e-continuous-agy/config.json`(0600, `enabled=true`, 채널·봇 ID·경로만)과 launchd 작업 `com.agentdesk.e2e-continuous-agy`를 올린다. 올리기는 S3가 넣는 `after_deploy.sh`의 agy 분기가 다음 배포에서 하고, 그 전에 켜려면 같은 렌더링을 손으로 한다. 끄기는 config의 `enabled=false` 한 곳이다.
2. 1b 뒤: 같은 instance에서 `--scenario E-67`을 별도 수용 실행으로 연다.
3. #8274 조각 4: 프로필의 `tui_writer`를 `true`로 바꾼 새 프로필로 그 head의 instance를 준비한다(O3 반복). E-65·E-66이 같은 채널·같은 시나리오로 다시 돈다.

| 항목 | 릴리스 runtime에 닿는 것 |
| --- | --- |
| O1 | 릴리스 CLI로 Discord 채널 1개 생성(announce 봇). 설정 파일 변경 없음 |
| O2·O3·O4 | 없음. 모두 `~/.adk/dev` 아래 |
| O5 | 셀 호스트에 launchd 작업 1개와 `~/.adk/e2e-continuous-agy` 추가. 릴리스 서버에는 `/api/health/detail` 읽기뿐 |

## 조각과 크기

| 조각 | 범위 | 추정 prod 파일 / 순증 |
| --- | --- | --- |
| S1 시험 instance의 agy 단일셀 경계 | `scripts/dev_instance`의 `profile.py`·`config.py`·`manifest.py`·`files.py`·`identity.py`·`driver_admission.py`·`__main__.py`에 프로필 표와 `agy_login`을 넣는다. Rust는 새 child `src/config/test_instance/agy_cell.rs`가 본체이고 `test_instance.rs`·`environment.rs`·`lifecycle.rs`·`src/cli/dev_instance.rs`·`src/services/stream_json_cli/runner.rs`는 호출·이름 목록 교체만 한다. 위 「증명 시험」 표가 수용 시험이다. | 13 / +390 |
| S2 agy 셀 실행·판정·배포 접점 | 새 패키지 `scripts/e2e/agy_cell`(`__main__`·`instance`·`scenario`·`judge`)이 instance 준비, E-65 송수신, 본문 재조립·1회·종료 판정을 맡는다. `scripts/dev_instance/discord_rest.py`에 announce 전송, `after_deploy.sh`에 agy config 분기, `deploy-release.sh`에 그 호출 한 줄. E-65 YAML, rollover·권한·기한 fixture와 시험. | 7 / +440 |
| S3 상시 회전·주기·예산 | `plan.py`·`runner.py`의 셀 선택, `release_host.py`의 agy tick, `budget.py`·`summary.py`·`publication.py`의 agy 예약·증거, `channel_lock.py`의 단일 부모 scope, 새 `scripts/e2e/agy_cell/driver.py`, 전용 launchd 템플릿. E-66 YAML, R1 배선·카탈로그는 공식 생성 절차를 쓴다. | 9 / +330 |
| S4 1b HTTP 시나리오 | `agy_cell`의 `scenario.py`·`judge.py`에 instance API 호출·origin 영수증 검사와 1b gate를 연결한다. E-67 YAML 및 HTTP 응답 fixture. | 2 / +110 |
| O 설정 조각 | O1–O5. 저장소에 실 ID나 비밀값을 기록하지 않는다. | 0 / +0 |

조각별 단순합은 **31파일 / +1270줄**이고 S4의 두 파일이 S2와 겹쳐 유니크 추정은 **29파일 / +1270줄**이다. 조각마다 따로 PR을 낸다. 가장 큰 S2도 PLAN_READY 정지선 +1200과 상한(30파일 / +1500, `pr_cap_prod.py`의 `FILES, NET`)에 한참 못 미친다. 전체를 한 PR로 묶으면 정지선을 넘으므로 묶지 않는다. 순서는 S1 → S2 → S3이고 S4는 1b 뒤다. 직전 설계(14파일 / +1200)보다 늘어난 몫은 S1이다. 실측은 구현 후 각 조각의 base와 HEAD에 `pr_cap_prod.py`를 적용한다. 문서·시험 경로는 측정기 제외 규칙을 따른다. `deploy-release.sh`(5,008줄)에는 후크 호출 한 줄만 두고, `environment.rs`(850줄)에는 호출만 둔다. agy 본체는 새 child에 둔다.

| 리뷰 집중 항목 | 구현 때 확보할 증거 |
| --- | --- |
| 경계 시각 | 완료가 상한과 같을 때와 직후일 때, 12시간 due·rolling 24시간 경계, instance 재기동 직전·직후, 배포 요청의 2,900초 기한 직전·직후를 가짜 시계로 검사한다. |
| 동시성 경합 | 같은 호스트에서 배포 요청·상시·HTTP 두 실행을 경합시켜 instance 임대와 입력 권한이 하나임을 확인한다. 임대 보유 중 프로세스 종료 뒤 다음 실행의 fail-closed idle 확인도 시험한다. |
| 옛 형식 복원 | `agy_login` 없는 `tui-live` manifest·receipt, agy 없이 저장된 cursor·요약·예산 파일, 기존 2부모 lock 키를 읽어 호환성을 검증한다. flat·wrapped agy terminal record를 같은 입력·턴 규칙으로 읽으며 포맷 혼용을 거절한다(`src/services/stream_json_cli/codec.rs:148–160`). |
| 문서로만 선언한 가드 | 「격리 경계」의 거절 fixture 전부에서 Gateway 접속 0·전송 0을 호출 카운터로 확인한다. agent·provider·채널 ID·셀 호스트·1b capability 불일치도 전송 0이다. 읽기 전용 preflight가 실패할 때 reset·release·kill 호출 횟수는 0이다. |
| 잘못된 0/PASS | 0턴, 끝 표식만 게시, 동일 본문 새 ID 복제, 성공 문자열과 lease 잔존, HTTP ACK만 존재, legacy fallback, 사용량 unknown, 셀을 켜지 않은 호스트의 결과, 도구 관측 없음에서 무도구 PASS를 각각 검증한다. #8160 보존 fixture는 두 분할 freeze 뒤 Done에서 종료 누락을 RED로, 정상 게시·lease 해제를 GREEN으로, 종료 검사 제거 뮤턴트를 RED로 잡는다. |

## 자체 점검

**대상 밖 경로 [F].** Rust에 넣는 분기는 모두 시험 instance 선언이 있을 때만 켜진다. 선언이 없으면 `configure_child_command`는 명령을 그대로 둔다(`src/config/test_instance/environment.rs:311–312`, `:353–354`). 릴리스 dcserver의 agy 실행, Claude·Codex 경로, 다른 노드의 동작은 main과 같다. `tui-live` instance는 프로필 표에서 지금의 봇 셋·채널 둘·credential 넷을 그대로 얻는다. 상시 runner의 기존 config·cursor·inbox에는 agy가 들어가지 않는다.

**같은 부류의 변형.**

| 변형 | 막는 곳 |
| --- | --- |
| instance 재기동 뒤 다른 토큰 파일로 바뀜 | 기동마다 identity를 다시 조회하고 파일 device·inode·시각을 대조한다(`scripts/dev_instance/identity.py:203–213`, `src/config/test_instance/lifecycle.rs:291–326`). |
| 두 Mac이 모두 셀을 켬 | Test-Antigravity가 두 곳에서 Gateway에 붙어 답이 두 번 나갈 수 있다. config는 한 호스트에만 둔다. 실수로 둘 다 켜지면 E-65의 새 ID 복제 검사가 FAIL로 드러낸다. 자동 방지는 없다. |
| agy의 다른 spawn(버전 probe·모델 목록) | instance PATH와 dcserver env로 돈다(`src/services/platform/binary_resolver.rs:828–832`). HOME은 instance HOME이라 운영 설정이 없다. |
| 로그인 홈에 누가 MCP 서버를 추가 | `up`마다 admission이 다시 본다. |
| E-67이 릴리스 API로 나감 | 호출 주소는 instance의 `self_url`만 받고 포트 풀 밖이면 보내지 않는다. |
| 시험 토큰 누락 시 다른 봇으로 대체 | 봇 키 `antigravity`가 읽는 파일은 선언 root의 한 곳뿐이고, 빠지면 credential 집합 비교에서 기동이 거절된다. |

**기준 문장 대조.**

| 기준 문장 | 설계 절 | 시험 |
| --- | --- | --- |
| 테스트 봇은 Test-Antigravity다 | 격리 경계 / identity 확인, O2·O5 | `agy_cell_admission_rejects_other_application_id` |
| 채널은 announce 봇으로 만든다. 운영 봇 권한은 바꾸지 않는다 | O1 | 운영 절차(코드 시험 없음). 설정·auth 변경 목록이 비어 있음을 O 표에 적음 |
| 토큰은 시험 runtime root credential 파일로 공급한다 | 격리 경계 / credential basename, O3 | `agy_cell_credential_set_is_exact`, `agy_cell_never_reads_release_credential` |
| 운영 채널·운영 credential·운영 Memento에 닿을 경로가 없는가 | 격리 경계 표, agy 로그인 홈 | `agy_child_env_has_no_forbidden_keys`, `agy_login_home_with_mcp_server_is_rejected` |
| 검증 실패면 E-65/66/67 모두 입력 0 | 격리 경계 / 실패하면 입력 0 | `agy_cell_precondition_failure_sends_nothing` |
| DoD 1 설정에 있고 운영 채널과 격리 | 격리 경계, O1–O3 | 위 시험 묶음 |
| DoD 2 짧은 답·긴 답·연속 턴, 실패면 FAIL | agy 셀 표, 배포 접점 | E-65 judge 시험, `deploy_request_without_result_becomes_fail` |
| DoD 3 하루 1회 이상 | 상시 접점 | 12시간 due·24시간 0회 노출 시험 |
| DoD 4 활성화 판 PASS 기록 | #8274와의 관계 | 조각 4 head의 E-65 기록 |

## #8274와의 관계

| 시점 | 통과해야 할 항목 | 보존 증거 |
| --- | --- | --- |
| 활성화 전 legacy | S1·S2 및 O1–O5 뒤, E-65에서 짧은 답·2개 이상 rollover를 거친 긴 답·후속 턴이 각각 종료한다. legacy turn bridge가 본문을 게시하고 순서·완료·lease 해제를 보여야 한다. #8160이 실제 남아 있으면 셀은 FAIL을 기록하며 #8160 수리 판에서 기준선 PASS를 확보한다. S3 뒤 E-66 주기 실행도 검증한다. | 배포 stamp·head, instance ID·기동 nonce·바이너리 SHA256, 셀 호스트, 출력 경로 `legacy`, run·입력·turn·Discord message ID 목록, 응답 재조립 비교, Done·종료·idle·lease 증거, 시간·실기 횟수. |
| 1b 활성화 뒤 | E-67을 별도로 열고 HTTP origin당 소비·완료와 답 1회를 증명한다. 1b의 유효 capability를 증거로 남긴다. Discord 기반 E-65의 입력 경로는 그대로 검증한다. | 1b head, instance ID·nonce, HTTP ACK, origin과 execution ID, Discord 본문 ID, 종료 및 해제. |
| 조각 4 `tui_o` 활성화 배포 | 같은 채널 ID·E-65 시나리오를 `tui_writer=true` 프로필의 새 instance에서 독립 run으로 반복한다. writer membership·스풀의 turn·session 식별과 terminal commit을 확인한다. 각 답이 끝까지 한 벌만 게시되고 종료되며, 해당 턴의 legacy 본문 전송 0을 실제 게시 증거와 함께 확인한다. 이 PASS를 #8274의 활성화 판 배포 기록에 포함한다. E-66도 같은 기대 출력 경로로 다시 실행한다. | 배포 stamp·head, 새 instance ID·nonce, 동일 채널 ID, turn별 스풀·출력 receipt·완료·lease 해제 및 Discord message ID, instance census의 이 턴 범위와 probe coverage. 빈 census나 stale head는 0건 증거로 해석하지 않는다. |
| 조각 4 이후 관측 | #8274 자체 DoD의 24시간 antigravity LegacyBody 0건은 릴리스 서버의 관측이다. 이 셀은 같은 head의 바이너리를 시험 instance에서 돌린 증거이고, 릴리스 서버의 실제 활성화 설정과 운영 채널 동작은 #8274가 따로 수집한다. | `src/server/routes/health_api/response.rs:267–270`의 census 상세와 #8274의 별도 수용 기록. |

현행 main에는 #8274 전의 headless dispatch 기반(실기 테스트가 호출할 코드와 설정 provider 등록)이 이미 있다(`src/services/discord/router/message_handler/provider_dispatch.rs:406–513`, `src/services/discord/router/message_handler/provider_dispatch.rs:942–976`). #8297의 네 DoD는 이 SHA에서 미충족이다. 첫째 agy 시험 프로필·채널이 없고 시험 instance는 Claude·Codex만 받는다. 둘째 배포 뒤 후크는 agy 요청을 넣지 않는다. 셋째 상시 `CELLS`는 네 TUI·Herdr 조합뿐이다. 넷째 활성화 판의 PASS는 위의 실기 증거가 있어야 충족된다.
