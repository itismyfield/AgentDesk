# #8297 agy 헤드리스 E2E 셀과 테스트 채널

조사 기준은 `origin/main`의 `d97f7b9d655e4b0c8d83bb89e192968a5ea0f504`이다. 추가 셀은 `agy-headless` 하나다. Discord 텍스트 채널 하나에서 `agy --print --output-format stream-json`을 사용한다. `agy-parity-1121`과 `agy-priority-1121`에 따라 이 셀의 legacy 기준선을 #8274 활성화 전에 확보한다. #8274와 #8296의 증거까지 갖추면 코디네이터가 4채널 전환을 판정한다. E2E 배선 설계이므로 런타임 상태 차트 변경은 해당 없다.

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

## agy 셀

제안 시나리오 ID는 E-65(배포), E-66(상시 짧은 답), E-67(HTTP 입력)이다. 배포와 상시 러너는 같은 셀 식별·읽기 전용 preflight·Discord 본문 판정기를 공유한다. 셀 이름은 `agy-headless`, provider 식별은 `antigravity`, 입력은 E-65/66의 일반 Discord 채널 경로와 E-67의 HTTP 경로다. `src/domain/provider/catalog/rows.rs:156–178`의 alias·실행 어댑터와 `src/services/stream_json_cli/dialects/agy.rs:27–52`의 spawn 옵션을 사용한다.

| 시나리오 | 입력과 관측 |
| --- | --- |
| E-65 / T1 짧은 답 | announce 입력을 일반 Discord 채널로 한 번 보낸다. `도구 없이 이 문자열만 답해: AGY-{run}-T1`. provider 본문 전체가 이 문자열이며 종료를 확인한다. |
| E-65 / L 긴 답 | `도구·마크다운 없이 START-{run} 한 줄, 다음 줄에 01부터 55까지 NN과 공백과 0123456789를 8번 반복한 문자열을 한 줄씩, 마지막 줄 END-{run}. 설명은 빼.` 검증기는 기대 본문 약 4.6천 ASCII자를 자체 생성한다. 실제 provider 본문을 담은 서로 다른 Discord ID 3개 이상을 요구한다. 앞 2개가 완료 전에 관측되어 고정된 분할 ID인지 확인하고, 최종 스냅샷에서 같은 ID의 본문과 순서를 대조한다. 시작·끝 표식, 55개 줄, 전체 재조립 본문을 모두 비교한다. 앞 조각 유실이나 중복을 끝 표식만으로 통과시키지 않는다. 분할 자격을 관측하지 못하면 실패로 기록하고 별도 fixture에서 rollover 2개 뒤 authoritative Done 경로의 RED/GREEN을 보인다. |
| E-65 / T2 연속 턴 | L의 종료·입력 해제를 확인한 즉시 같은 채널·대화에 S2를 보낸다. `도구 없이 이 문자열만 답해: AGY-{run}-T2`. T2는 새 입력·turn ID, 같은 대화 fingerprint, 정상 종료로 이어져야 한다. 따라서 T1→L→T2의 provider 호출 3개로 짧은 답, 긴 답, 연속 턴을 모두 본다. |
| E-66 / 주기용 | T1과 동일한 판정을 표식 `AGY-{run}-DAILY`로 1턴만 실행한다. E-65 PASS가 있어도 주기 실행 기록은 독립적으로 남긴다. |
| E-67 / 1b 이후 | 권한 있는 시험용 Discord 작성자 ID로 `POST /api/agents/adk-agy-headless-e2e/turn/deliver`에 `source=imessage`, 새 `origin_id`, 같은 테스트 채널·provider를 명시하고 `AGY-{run}-HTTP` 한 줄 답을 요청한다. 전송 ACK, origin에 귀속된 실행, Discord 답 1회와 종료를 묶어 판정한다. 로컬 시험 자격을 사용하고 실제 iMessage나 Discord 입력 게시를 거치지 않는다. 하네스의 `deliver`는 현재 채널·provider를 payload에 넣지 않으므로 명시 바인딩과 응답 채널 검증을 추가한다. 1b의 유효 기능을 확인하기 전에는 `BLOCKED_CAPABILITY`로 남기고 HTTP 호출 횟수는 0이다. ACK만 수신하거나 송신 결과가 불명확하면 조사하고 재송신을 시작하지 않는다. |

Discord E2E 입력은 `scripts/e2e/tui_relay/discord.py:109–135`의 전송·수신 경계를 재사용한다. E-67은 `src/server/routes/docs/inventory/endpoints/part_03.rs:105–127`, `src/server/routes/docs/inventory/endpoints/part_03.rs:157–169`의 요청·`input-result` 계약을 바탕으로 한다. 조각 1b가 추가하는 수용 증거를 사용하며 Discord 정상 입력 수용 증거는 E-65에서 계속 수집한다.

**종료·정확히 1회 판정** — 실행 시작의 최신 Discord ID와 현재 boot/head, 시험 채널의 입력 ID 또는 HTTP origin을 고정한다. provider bot ID와 해당 턴의 게시 ID로 본문 조각을 귀속한다. 사용자 입력·announce echo·완료 크롬은 본문 집계에서 제외한다. 동일 message ID의 edit는 마지막 본문으로 합치고, 새 ID에 복제된 본문은 중복으로 센다. 짧은 답은 완전한 본문 ID 한 개, 긴 답은 순서 있는 한 벌의 조각과 일치하는 완전한 논리 답 한 개가 기준이다. 성공 Done과 완료 푸터 또는 writer 완료 영수증을 턴과 대조한다. mailbox·대기열 idle 및 `GET /api/turn-lease/antigravity/{channel_id}`의 idle 응답을 확인하고, legacy에서는 같은 채널의 읽기 전용 watcher-state와 대조해 남은 inflight가 없음을 확인한다. 진단 데이터가 없으면 lease·mailbox의 정상 응답까지 교차 확인하고 오류 응답은 idle로 해석하지 않는다. #8274 활성화 뒤에는 writer의 같은 턴 terminal commit과 입력 해제를 함께 본다. 마지막 턴은 종료 뒤 5초 동안 같은 관측 경계에서 늦은 중복을 검사한다. 잘못된 채널·boot 전환·관측 누락·lease 잔존·부분 본문·timeout은 필요한 PASS 증거를 채우지 못한 결과다. 좌표는 `src/server/routes/turn_lease.rs:15–30`, `src/server/routes/docs/inventory/endpoints/part_07.rs:581–604`, `scripts/e2e/post_deploy_turn_smoke.py:367–401`, `src/services/discord/turn_bridge/stream_loop/content_arms.rs:310–359`, `src/services/discord/inflight/store.rs:473–542`이다.

**시간 상한 제안** — 배포 E-65는 preflight·lock·판정 포함 360초(최대 30초 + 60/180/60초 + 30초). E-66과 E-67은 각각 120초(최대 20초 + 80초 + 20초). 같은 단조시계 절대 기한의 잔여 시간으로 각 요청·대기·판정을 자르고 기한 직후 응답은 timeout으로 기록한다. 공용 deploy 준비 시간은 현재값을 사용한다. agy timeout을 별도로 설정하고 legacy 첫 실기에서 입력→첫 본문·마지막 본문·종료와 wall time을 남긴다. Google 쿼터는 호출 수를 우선 제어한다. E-65는 배포당 한 시도에 프롬프트 3개, E-66은 주기 칸당 1개, E-67은 1b 수용 실행당 1개를 예약한다. 짧은 전용 시스템 프롬프트와 일반 텍스트만 사용하며 도구 호출은 실패로 관측한다. agy는 실제 tool allowlist를 지원하지 않으므로 전용 워크스페이스와 Discord 권한으로 격리한다. 근거는 `src/services/stream_json_cli/dialects/agy.rs:15–36`이다.

**배포 접점** — full 스모크의 서버 준비 확인 뒤 E-65를 별도 셀 호출로 실행한다. agy preflight는 Claude E-1 채널 해석에 의존하지 않고 구성된 agent/채널의 gateway, 실제 실행 노드, headless runtime, idle을 검사한다. 승인된 오너 호스트에서 필요한 셀이 누락되거나, 채널이 바쁘거나, timeout 또는 judge 실패라면 `E-65=FAIL`을 기능 스모크 결과와 증거 로그에 올린다. API 전용·비오너·standby의 호출 제외와 startup 준비 실패는 이유를 기록하고 #8274의 PASS 증거에서 제외한다. 스모크 실패 시 배포 스크립트의 기존 fail-open 계약으로 서빙과 후속 배포 단계가 이어진다. agy 경로는 E-50/51의 lease release·`!clear` reset과 독립된 읽기 전용 preflight를 사용한다. 이미 점유된 테스트 턴은 소유자가 자연 종료하도록 두고, 증거와 실패 기록을 남긴다. 근거는 `scripts/deploy-release.sh:4816–4835`, `scripts/deploy-release.sh:4930–4973`, `scripts/e2e/post_deploy_turn_smoke.py:463–493`이다.

**상시 접점** — `plan.py`의 `CELLS`에 `agy-headless`를 추가하고 R1에 E-66을 등록한다. 공식 `scripts/e2e/generate_continuous_catalog.py --write`로 시나리오 카탈로그를 동기화한다. mac-book의 agy 전용 launchd tick과 상태 디렉터리에서 기존 runner를 재사용한다. `rotation_interval_s=43200`, `core_interval_s=null`, `daily_full_at=null`, `max_run_s=120`, tick 300초, 대상 필터 `agy-headless`를 적용해 E-66만 도는 R1 step을 선택한다. 릴리스 호스트 tick은 기존 디렉터리·작업과 구별되는 agy 전용 config로 기동하고 기존 after-deploy inbox에는 이 config를 전달하지 않는다. 현재 P 요청이나 전체 R1–R5 cursor가 agy 순번을 밀지 않는다. 배포 중·lease 경합으로 입력 0인 연기는 같은 due를 유지하고 다음 tick에서 다시 평가한다. 입력 직전에는 head·boot·lease를 다시 확인한다. 전송을 시도한 작업은 시간과 결과를 기록하고, 그 작업의 요청 ID는 다음 12시간 칸까지 재송신을 막는다. 그 사이 서버의 턴이 남아 있으면 다음 실행도 idle 확인에서 막힌다. 최근 24시간에 해당 head의 live E-66이 0이면, 증거의 지연/누락 또는 실행 불가로 노출한다. 일정은 정상 상태에서 12시간마다 하루 2회 기회를 주며, 실기 acceptance는 연속 24시간에 최소 1회 실행과 보존된 결과다. 배포의 E-65가 이 분모를 채우지 않는다.

- 상시 첫 활성화는 agy에만 요청 수·wall time을 계량하는 명시적 예산 프로필을 쓴다. 초기 상시 한도는 rolling 24시간에 2회·240초이며 입력 직전 한 번씩 예약한다. 새 프로세스/자정/새 head에서 예약을 초기화하지 않는다. 배포와 HTTP 수용 시도는 별도로 표시해 전체 호출을 집계한다. 토큰 사용량은 실제 계측이 없으면 `null`로 기록하고 `usage_complete=false`를 유지한다. E-66의 기능 판정과 실기 횟수는 개별 occurrence로 공개한다. driver/runner에서 `request_time` 예산 증거와 측정된 usage를 별개 필드로 검증해 상시 1회 시도가 실제로 접수됐는지 추적한다. 기존 요약의 전체 usage PASS와 Claude/Codex의 예산 기준은 그대로 적용된다. agy의 0턴·이전 head·불완전 단언으로 기능 PASS를 만들 수 없다. 실제 usage가 확보되면 기존 토큰 예산에 연결할 수 있도록 필드를 보존한다. 근거는 `scripts/e2e/continuous/budget.py:26–76`, `scripts/e2e/continuous/summary.py:114–184`이다.
- 배포·상시·HTTP 수용은 같은 mac-book의 agy singleton 채널 lease를 사용한다. `channel_lock.py`의 별도 명시 scope에서 guild와 부모 ID 한 개, 고정 bot 집합을 받아 같은 고정 inode와 flock을 쓴다. 기존 두 부모 ID의 키는 유지한다. 배포 락 점검과 채널 flock은 둘 다 수행한다. 락을 잡은 프로세스가 전송하고 신원을 재검증하며, 상대 락을 기다리며 선점하지 않는다. 배포가 이미 시작했거나 기한·소유권을 잃은 실행은 추가 입력을 중단한다. 근거는 `scripts/e2e/continuous/channel_lock.py:26–33`, `scripts/e2e/continuous/channel_lock.py:80–103`, `scripts/e2e/continuous/release_host.py:49–59`, `scripts/deploy-release.sh:2535–2569`이다.

## 운영 변경 목록

코디네이터·메인이 O1–O5를 수행하고 Discord 생성은 project-agentmanager가 맡는다. 호스트 배정은 제안이다. 추적된 S0 기록은 mac-book에서 E-50/51 실기 실행, mac-mini에서 양쪽 skip을 보여 준다(`docs/e2e-continuous/s0-replay.md:12–21`). 실행 전 실제 gateway·CLI 로그인 상태와 배정 결과는 운영 담당자가 확인한다.

| 항목 | 담당과 변경 |
| --- | --- |
| O1 Discord 채널 한 개 | project-agentmanager가 기존 `ADK E2E` 카테고리에 `#adk-agy-headless-e2e`를 만든다. 기존 E2E 채널의 권한을 따라 운영 채널과 구획을 분리하고, 시험 운영자·announce·Antigravity 봇에 필요한 권한만 부여한다. 전용 guild/채널/봇/작성자 ID만 다음 항목으로 전달한다. |
| O2 에이전트 등록 | 코디네이터가 `adk-agy-headless-e2e` 전용 바인딩·워크스페이스·짧은 프롬프트 파일을 배치한다. E2E ID 하나로 Discord·HTTP를 공유한다. 운영 프롬프트·업무 파일·다른 agent의 채널/DM 라우팅을 참조하지 않는다. 봇 허용 범위를 조정해야 할 때 이 채널과 지정 ingress만 적용하고 기존 항목을 보존한다. |
| O3 두 Mac 배정 | mac-book이 Discord 입구를 통과한 셀의 agy 실행, 배포 스모크 주입, 상시 tick, 1b 수용 실행을 맡는다. mac-mini는 자체 스모크에서 이 셀을 비대상으로 기록하고 주입·상시 실행을 활성화하지 않는다. 클러스터가 같은 에이전트 라우팅 메타데이터를 공유해도 두 노드의 로컬 실행 허용 목록은 위처럼 두며, 실제 owner가 mac-book과 다르거나 failover로 이동했다면 해당 실기의 PASS를 거부한다. 우선 실행 장비의 정책 키는 API `default_node_id`로 mac-book의 검증된 instance ID를 지정한다. mac-mini 대체 배정 가능성이 있는 우선순위 값이므로, 실제 owner와 게시 노드는 별도로 확인한다. 근거는 `docs/operations/agent-execution-node.md:3–37`, `src/server/routes/execution_requirements.rs:29–60`이다. |
| O4 검증 배선·예산 | mac-book의 deploy smoke에서 agy 셀 필수 여부와 360초 한도를 선택하고, 기기 전용 runner config·launchd entry에 같은 ID, bot set, lock 상태 경로, 12시간 예약, per-run/rolling 예산·owner 기대값을 설정한다. mac-mini는 배포 스모크에서 비오너로 설정한다. 운영 담당자가 mac-book의 기존 agy 로그인 및 headless 실행 경로를 확인하고 시험 증거에 head·boot·노드만 남긴다. |
| O5 단계 활성화 | legacy로 E-65 및 상시 E-66을 활성화한다. 1b 이후 같은 채널의 E-67을 별도 수용 실행으로 연다. 조각 4에서는 E-65에서 기대하는 writer를 `tui_o`로 전환하고 양쪽 결과의 head·boot·실행 기록을 남긴다. |

`agentdesk.yaml` 키 계획은 `agents[].id=adk-agy-headless-e2e`, `name=ADK agy headless E2E`, `provider=antigravity`(`agy`의 canonical ID), `peer_directory=false`, `channels.antigravity.{id,name,workspace,prompt_file,runtime,tui_hosting,thread_inherit,peer_agents}`이다. `id`는 Discord가 발급한 문자열 ID, `name=adk-agy-headless-e2e`, `workspace=~/.adk/release/workspaces/adk-agy-headless-e2e`, `prompt_file=~/.adk/release/config/agents/adk-agy-headless-e2e.prompt.md`, `runtime=pipe`, `tui_hosting=false`, `thread_inherit=false`, `peer_agents=false`를 제안한다. 필요한 경우 기존 `discord.bots.<antigravity-bot>.auth.allowed_channel_ids`와 `allowed_bot_ids`에 시험 채널 및 지정 ingress를 반영한다. `session_hosts.herdr.channels`에 이 채널을 등록하지 않는다. 우선 실행 장비는 O3의 API 값이다. 조각 4의 writer 선택은 #8274가 제공하는 실제 활성화 키를 사용하며, 현행 `tui_o.writer.channels`의 Claude/Codex 검증 제약을 고려한다. 좌표는 `src/config.rs:252–312`, `src/config.rs:557–587`, `src/services/tui_o/channel_policy.rs:79–143`이다.

> project-agentmanager님, #8297을 위해 기존 `ADK E2E` 카테고리에 일반 텍스트 채널 `adk-agy-headless-e2e` 한 개를 만들어 주세요. 기존 `adk-*-e2e`와 동일한 접근 범위를 적용하고, 시험 운영자·announce 봇·Antigravity 봇이 입력 게시와 결과 조회에 필요한 권한을 갖도록 확인해 주세요. 토픽은 “Antigravity headless stream-json E2E, mac-book 소유, 배포·상시·HTTP 수용 시험 전용”으로 부탁드립니다. 생성 후 guild·채널 ID와 권한 확인 결과를 코디네이터에 전달해 주세요.

## 조각과 크기

| 조각 | 범위 | 추정 prod 파일 / 순증 |
| --- | --- | --- |
| S1 배포와 실기 판정 | `scripts/deploy-release.sh`는 기존 turn-smoke 호출부를 보조 스크립트 호출로 바꾸고 결과를 합친다. 새 Python child가 셀 해석·headless preflight·E-65 송수신·판정을 맡는다. 기존 Discord 클라이언트, `channel_lock.py`, `post_deploy_turn_smoke.py`는 재사용할 접점만 조정한다. E-65 YAML, rollover/권한/기한 fixture와 시험을 추가한다. | 5 / +460 |
| S2 상시 회전·주기·예산 | `plan.py`·`runner.py`의 셀 선택, `release_host.py`·`smoke_driver.py`의 headless 분기, `budget.py`·`summary.py`·`publication.py`의 agy 예약·증거, 전용 launchd 템플릿과 새 agy 예산 child를 추가한다. E-66 YAML, R1 배선·카탈로그는 공식 생성 절차를 사용한다. | 9 / +630 |
| S3 1b HTTP 시나리오 | 공용 headless child와 `scripts/e2e/tui_relay/discord.py`에 명시 바인딩·origin 영수증 검사와 1b gate를 연결한다. E-67 YAML 및 HTTP 응답 fixture를 추가한다. | 2 / +110 |
| O 설정 조각 | O1–O5. 실제 채널 생성과 환경별 바인딩은 운영 담당자 작업이다. 저장소에 실 ID나 비밀값을 기록할 필요가 없다. | 0 / +0 |

조각별 단순합은 **16파일 / +1200줄**이고 S3와 S1의 두 파일이 겹치므로 유니크 추정은 **14파일 / +1200줄**이다. 파일별 diff 실측은 구현 후 각 실제 base와 HEAD에 `pr_cap_prod.py`를 적용한다. 파일 30 / 순 코드 +1500 상한은 WSL의 `/home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py:15`에서 확인했고, +1200은 브리프의 PLAN_READY 정지선이다. 문서·시험 경로는 측정기 제외 규칙(`:18–22`)을 적용한다. `deploy-release.sh` 5,008줄 및 `run_tui_relay.py` 5,551줄에 agy의 독립 판정 로직을 넣을 필요가 없다. 배포 쉘에는 블록 이동·호출 교체만 두며 agy 하네스는 독립 child에 둔다.

| 리뷰 집중 항목 | 구현 때 확보할 증거 |
| --- | --- |
| 경계 시각 | 완료가 상한과 같을 때와 직후일 때, 12시간 due·rolling 24시간 경계, boot/head 교체 직전/직후를 가짜 시계로 검사한다. |
| 동시성 경합 | 동일 셀에서 배포·상시·HTTP 두 실행을 경합시켜 입력 권한 한 개를 확인한다. lock 보유 중 프로세스 종료 및 다음 실행의 fail-closed idle 확인도 시험한다. |
| 옛 형식 복원 | agy 없이 저장된 cursor/요약·예산 파일과 기존 2부모 lock 키를 읽어 호환성을 검증한다. flat/wrapped agy terminal record를 같은 입력·턴 규칙으로 읽으며 포맷 혼용을 거절한다(`src/services/stream_json_cli/codec.rs:148–160`). |
| 문서로만 선언한 가드 | agent·provider·채널 ID·오너 노드·1b capability 불일치 fixture에서 전송 0을 검증한다. 읽기 전용 preflight가 실패할 때 reset·release·kill 호출 횟수도 0이다. |
| 잘못된 0/PASS | 0턴, 끝 표식만 게시, 동일 본문 새 ID 복제, 성공 문자열과 lease 잔존, HTTP ACK만 존재, legacy fallback, 사용량 unknown, API 전용·비오너 결과를 각각 검증한다. #8160 보존 fixture는 두 분할 freeze 뒤 Done에서 종료 누락을 RED로, 정상 게시·lease 해제를 GREEN으로, 종료 검사 제거 뮤턴트를 RED로 잡는다. |

## #8274와의 관계

| 시점 | 통과해야 할 항목 | 보존 증거 |
| --- | --- | --- |
| 활성화 전 legacy | S1 및 O1–O4 뒤, E-65에서 짧은 답·2개 이상 rollover를 거친 긴 답·후속 턴이 각각 종료한다. legacy turn bridge가 본문을 게시하고 순서·완료·lease 해제를 보여야 한다. #8160이 실제 남아 있으면 셀은 FAIL을 기록하며 #8160 수리 판에서 기준선 PASS를 확보한다. S2 및 O5 뒤 E-66 주기 실행도 검증한다. | 배포 head·boot·mac-book instance·출력 경로 `legacy`, run/입력/turn/Discord message ID 목록, 응답 재조립 비교, Done/종료·idle/lease 증거, 시간·실기 횟수. |
| 1b 활성화 뒤 | E-67을 별도로 열고 HTTP origin당 소비/완료와 답 1회를 증명한다. 1b 활성화의 유효 capability와 실제 host를 증거로 남긴다. Discord 기반 E-65의 입력 경로는 그대로 검증한다. | 1b head·boot, HTTP ACK, origin과 execution ID, Discord 본문 ID, 종료 및 해제. |
| 조각 4 `tui_o` 활성화 배포 | 같은 agent ID·채널 ID·E-65 시나리오를 독립 run으로 반복한다. 셀을 대상으로 writer membership/owner·스풀의 turn/session 식별과 terminal commit을 확인한다. 각 답이 끝까지 한 벌만 게시되고 종료되며, 해당 턴의 legacy 본문 전송 0을 실제 게시 증거와 함께 확인한다. 이 PASS를 #8274의 활성화 판 배포 결과에 포함한다. E-66도 같은 기대 출력 경로로 다시 실행한다. | 배포 head·새 boot·동일 채널 ID, turn별 스풀/출력 receipt·완료·lease 해제 및 Discord message ID, census의 이 턴 범위와 probe coverage. 빈 census나 stale head는 0건 증거로 해석하지 않는다. |
| 조각 4 이후 관측 | #8274 자체 DoD의 24시간 antigravity LegacyBody 0건은 관측 범위·노드·boot·collector coverage를 확인한다. 실제 루틴 시험 및 1회 실행과 상태 차트 증거는 #8274에서 수집한다. | `src/server/routes/health_api/response.rs:267–270`의 census 상세와 #8274의 별도 수용 기록. |

현행 main에는 #8274 전의 headless dispatch 기반(실기 테스트가 호출할 코드와 설정 provider 등록)이 이미 있다(`src/services/discord/router/message_handler/provider_dispatch.rs:406–513`, `src/services/discord/router/message_handler/provider_dispatch.rs:934–976`). #8297의 네 DoD는 이 SHA에서 미충족이다. 첫째 전용 에이전트·채널이 정적 정의에 없고, 둘째 배포는 E-50/51만 호출하며, 셋째 상시 `CELLS`는 네 TUI/Herdr 조합뿐이다. 넷째 활성화 판의 PASS는 위의 실기 증거가 있어야 충족된다.
