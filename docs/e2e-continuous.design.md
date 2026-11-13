# Discord 테스트 채널 E2E 상시 실행 설계

기준 main: `79bebdd4eaa29b289bf88412825baa7d70a6ac77` (2026-11-13).
정본 입력은 #8120 본문·댓글의 레인 사본 `issue-and-comments.md`, 11-13 13:5x 브리프와
14:3x 코디네이터 결정(controlled ingress·수용 자동화·실측 전 off·알림만)이다.
이 문서는 구현 계약이다. 최초 설계 조사에서는 빌드·provider 실행·Discord 접속·운영 조회를 하지 않았다.
S0는 두 맥의 보존 deploy 로그를 읽기 전용 복사본으로 집계하고 L0는 오프라인 모델을 검증한다.
운영 프로세스·Discord·설정은 변경하지 않으며 cargo/provider 실행도 하지 않는다.

**단일 시험 호스트의 외부 runner가 #8124 인스턴스에서 실행하고, 결과 파일을 감시자가 읽는다.**
배포 후 핵심·순환 조각·일일 전체의 명세는 만들되, 실제 사용량이 없는 묶음의 주기는 켜지 않는다.
기능 실패는 우선 알림이며 새 배포 차단 게이트를 만들지 않는다.

## 1. 확정 전제와 현재 충족 범위

### 1.1 11-13 사용자 결정의 우선순위

`docs/test-infra.design.md`는 인스턴스 격리·receipt·수명 계약의 정본이다.
다만 현재 파일의 세 앱 제안(:110), 운영 봇 권한 변경/HOLD 제안(:185, :505),
사전 실행 게이트 질문(:504)은 **11-13 10:46 사용자 결정 및 이번 브리프로 대체**한다.
이전 문서의 미확정 주기·60분/24턴(:324)도 실측 예산으로 인용하지 않는다.

| 항목 | 이번 설계의 고정값 |
| --- | --- |
| 테스트 앱 | `Test-ClaudeCode`, `Test-Codex`, `Test-Announce`, `Test-Notify` 네 개. 별도 다섯 번째 Driver 앱 없음 |
| 용도 | 앞의 둘은 제품 수신/출력, Announce는 시험 입력 REST, Notify는 관측 Gateway·제어 표식·실패 알림. Announce/Notify 역할별 실제 앱 ID를 receipt로 검증 |
| Discord 영역 | Test category `1570615295284879391`; cc parent `1570615300712566895`, cdx parent `1570615306382999763` |
| 경계 | 시험 인스턴스의 채널 allowlist와 생성한 thread의 parent 확인. 운영 봇 권한은 변경하지 않음. category 권한 변경을 자동 격리 수단으로 삼지 않음 |
| provider | 일반 작업과 같은 Claude·Codex 계정. HOME/세션/출력 격리와 계정 사용량 격리는 별개이며, 사용량은 공유됨 |
| 사전 실행 | 권고 증거 수집. 새 머지/배포 게이트 아님 |
| 실행 장소 | #8124의 지정 시험 호스트 한 곳. macOS이면 launchd 사용. WSL 조사 레인은 실행 주체가 아님 |

시험 인스턴스 receipt의 root·PG·HTTP·tmux socket·Herdr endpoint·binary SHA 경계를 그대로 소비한다
(`docs/test-infra.design.md:38`, :192). 부모 채널을 시험 allowlist에 넣고, 매번 새 thread의
실제 parent가 그 부모인지 확인한 뒤에만 Driver 전송을 허용한다. 이름/category 표시만으로 허용하지 않는다.
Notify는 turn sender로 허용하지 않고, Announce의 일반 입력은 기존 announce 분기를 사용한다
(`src/services/discord/dispatch_policy.rs:233`). 결과 알림을 provider가 받는 채널에 보내 재귀 실행시키지 않는다.

### 1.2 코드에서 확인한 현재 단언

아래 좌표는 모두 기준 main이다. 파일 이름이 아니라 실행 경로로 범위를 구분한다.

| 경로 | 이미 있는 검사 | 없는 증거·이번 책임 |
| --- | --- | --- |
| inventory | `tests/e2e/tui_relay/scenarios/README.md:11`: **active 43 = live 38 + fixture 2 + unsupported 3**, retired 3 별도 | 브리프의 48은 과거 수치. live YAML은 CI 실행 증거가 아님(:3) |
| E-50/E-51 | `E-50-claude-clear-and-turns.yaml:23`, `E-51-codex-clear-and-turns.yaml:24`: T1 응답 뒤 T2, clear ACK 뒤 AFTER_CLEAR; 각각 본문 존재·중복 금지(:39/:40) | 바쁜 턴 중 입력·반응 이력·새 thread·실제 host 검사 없음 |
| deploy 호출 | `scripts/deploy-release.sh:4597`, :4633, :4680, :4707: 두 시나리오를 별도 driver 실행·judge. :3959의 120/150초는 **기존 제한**, 실측 시간이 아님 | 스모크 전체가 300초라는 뜻 아님. gateway 준비(:3952), E-35(:3958)는 별도 |
| turn judge | `scripts/e2e/post_deploy_turn_smoke.py:135`: marker별 미실행/null·0·중복, :159 Herdr detail/CLI, :182 로그 누락, :196 scenario/rc/refusal; :218 run·cell·channel 및 잔여 상태 결합 | reaction·busy 주입 시각·fresh thread 검사는 없음. 기존 검사를 재구현하지 않음 |
| host 판별의 한계 | 같은 파일 :240은 **config**로 host 이름을 결정; :159는 `*-herdr` cell일 때만 실제 Herdr 관측 경로. CLI :674/:680은 classify 뒤 config host를 출력 | `path=herdr` 문자열만으로 Herdr가 검증됐다고 하지 않음. `claude-tui`가 config상 Herdr면 여전히 tmux 로그 judge를 탈 수 있음 |
| continuity 도구 | `scripts/e2e/post_deploy_relay_continuity.py:618`: tmux 전후 identity·watcher reattach·재시작 후 Discord 표식·mailbox; :683 실제 실행/필수 E-9·E-19/timeout/idle/session 보존 | 네 신규 단언 없음. 조건부 deploy E-50/51 호출과 별개 도구 |
| E-63 | `E-63-input-order-across-restart.yaml:17`, :101: A/B의 최종 순서·각 1회·idle | 입력이 pane에 이미 들어갔는지 관측하지 않는다고 명시. 90초 hold(:39)·300초 wait는 주입 SLA가 아님 |
| E-14 | `E-14-claude-tui-thread-stale-reattach.yaml:9`: 기존 thread ID 필요 | fresh 첫 메시지/기본 node의 cold start가 아님 |
| driver | `scripts/e2e/run_tui_relay.py:1073`의 단언 registry에는 네 신규 operation 없음. :5486은 fail 0이면 exit 0 | empty/skip을 상시 PASS로 쓰지 않는 wrapper 필요. 기존 `feature_accounting.py:34` 재사용 |
| host API | `src/db/session_evidence.rs:8`에는 host 없음; :64는 thread 제외. `herdr.py:187`은 channel/provider/bound/pane/nonce 확인 | thread host는 새 exact identity join 필요. 세션 이름/기존 session-evidence API만으로 해결 불가 |

위 YAML들의 경로는 `tests/e2e/tui_relay/scenarios/`, Python child는 `scripts/e2e/tui_relay/`다.
이미 있는 **queued_head의 Herdr gate 포함**(`src/services/discord/router/intake_gate/busy_inject.rs:90`)과
**#8115 제품 수정**은 다시 구현하지 않는다. #8115는 main에 포함된 `135a168e6`(#8129)가
`src/services/cluster/execution_capacity.rs:57`에서 슬롯 미광고 primary만 보존하도록 고쳤다.
대상 PG 시험도 `src/services/cluster/intake_router_hook/agent_execution_node_tests.rs:233`에 있다.
상시 fresh-thread E2E는 아직 없으므로 #8120 전체가 ALREADY_DONE인 것은 아니다.

## 2. 실측과 실행 묶음

### 2.1 확보한 기존 실행 근거와 확보하지 못한 것

원시/부분 보존본을 구별한다. 시간과 provider 입력 수를 provider API 호출 수·token 수로 바꾸지 않는다.

| 기존 기록 | 관측치 | 해석 한계 |
| --- | --- | --- |
| `tests/e2e/tui_relay/fixtures/turn_smoke/saved/e50-herdr-pass-20261101T175451Z.json:43` | 보존된 실제 보고서 부분본의 start/end 차이 **61초**, n=1. ACK된 Discord post 4개 중 clear 1, 작업 prompt 3 | 현재 시험 인스턴스/observer 비용이 아님. 실제 provider start/API 호출·token 미기록. 파일명 stamp와 내부 local 시각의 timezone을 섞지 않음 |
| `i8011/smoke-fail-0043.log:36`–:43 (레인에 보존된 11-09 스모크 원문) | E-51 PASS, T1/T2/AFTER_CLEAR 각 1. 단계 합 **64.4초**, n=1; 작업 prompt 3, clear 1 | 단계 합은 전체 wall time이 아님. 같은 실행의 E-50은 첫 응답 timeout, 단계 합 64.1초(:33) |
| 같은 E-51 단계 :41 | T1 send+wait=20.1초, T2=14.7초, AFTER_CLEAR=20.5초; idle 2·clear 3.6·final 3.5초 | provider inference만의 시간도, 지연 상한의 근거도 아님 |
| #8120 본문과 S0 replay | E-50 PASS24/FAIL3, E-51 PASS30/FAIL7이 같은 40배포 시작 창에서 재현됨 | 원 run 목록이 없어 `match_not_identity`. E-51 실패 7건의 원문 확보·원인 불명 7. 현재 KST 전수 분모와 분리(§5.2) |
| S0의 KST 11-06 이후 mac-book report | E-50 wall n=34, min/median/max **58/67/98초**; E-51 wall n=44, **60/66.5/88초** | 각 report의 start/end 차이이며 묶음 wall·provider inference·native turn/token은 아님. 시나리오별 ACK post는 기록됐으나 clear 포함으로 턴 환산 금지 |

재계산 산출물은 레인 `logs/historical-measurements.json`, `logs/scenario-census.json`에 둔다.
최초 설계 원문 로그 사본의 SHA256은 전자에 기록했다. S0 전수 원문·inventory·report SHA는
[replay JSON](e2e-continuous/s0-replay.json), 시각·head·실패 단계 전체 표는
[replay 표](e2e-continuous/s0-replay.md)에 추가한다. n=1 부분본 둘의 61+64.4=125.4초는
**서로 다른 실행에서 가져온 proxy 합**일 뿐, 핵심 묶음 wall time·p95·평균이 아니다.
새로운 busy/thread 묶음과 순환/전체는 기존 실행 표본 **n=0, 실제 턴·시간·token=unknown**이다.
YAML의 timeout 합이나 약 3.3시간이라는 과거 명세 합
(`tests/e2e/tui_relay/scenarios/README.md:61`)을 실측으로 제출하지 않는다.

### 2.2 묶음 명세와 활성화 조건

새 시나리오는 번호 충돌을 피하려고 여기서는 이름으로 표기한다. 실제 E 번호는 구현 때 배정한다.
`R-marker`/`R-inject`는 §3.1의 **human 원증상** 전제가 필요하다. 무인 회귀는 별도
`controlled_ingress` provenance로 실행하고, #8114의 native-busy 원증상과 managed 대조군을 구별한다(§3.3).

| 묶음 | 고정 목록·조합 | 입력/턴/시간 근거와 주기 |
| --- | --- | --- |
| P: 배포 SHA 핵심 | E-50(Claude/Herdr/channel), E-51(Codex/tmux/channel), R-marker(Claude/tmux/channel/busy), R-inject(Claude/Herdr/channel/native-busy/no-managed-or-external-holder), R-fresh(Claude/tmux/thread/primary-no-slots), R-host(Claude/Herdr/thread/primary-no-slots, busy 후속·취소 포함) | 기존 둘은 작업 prompt 6개; 신규는 A+B, A+B, 첫 입력, A+B로 **계획상 7개**, 합 13개. busy 합류가 성공하면 계획상 native 턴 10개지만 **실제 턴 미실측**. 기존 둘의 시간 proxy만 있고 전체는 unknown. 배포 완료 이벤트당 중복 제거한 요청 1개, 자동 실행은 실측 활성화 뒤 |
| R1: 기본 relay | E-1, E-2, E-3, E-5, E-15; 지원하는 두 TUI provider, host를 회차별 tmux/Herdr 교대 | n=0, 실제 턴/시간/token unknown |
| R2: 입력·본문·로컬 제어 | E-16, E-20, E-22, E-23, E-52, E-64; YAML cells와 실제 capability의 교집합 | n=0. E-20은 같은 세션에 입력 동시 도착이며 provider 활성 턴 2개를 요구하는 E-11과 다름 |
| R3: 직접 TUI·압축·모델 | E-4, E-6, E-7, E-10, E-21, E-37; tmux TUI 및 YAML의 provider만 | n=0; Herdr에 tmux-only 키 검사를 N/A로 바꿔 전체 PASS하지 않음 |
| R4: 복원·중단 | E-8, E-9, E-12, E-14, E-17, E-18, E-19, E-30, E-31, E-54, E-63 | n=0; E-17은 matrix 전용으로 adapter가 필요. 재시작/취소는 #8124 소유 instance만, 기존 release restart 경로 사용 금지 |
| R5: 출력·장기 작업 | E-35, E-53, E-60, E-61, E-62; O/Legacy 프로필을 분리 | n=0; O인 E-60/61과 Legacy인 E-62를 한 boot의 결과로 합치지 않음 |
| F: 전체 census | 위 R1–R5 + E-50/51 + 별도 E-11, E-13, E-36 = 기존 live 38개, 여기에 신규 네 시나리오 | n=0. 하루 1회 전체는 **비활성 후보**. full inventory와 full live를 구분. 매일 다 도는 시간/token 예산을 아직 제시할 수 없음 |

순환은 **한 번에 R1–R5 중 한 조각·한 cell**, 다음 회차에 cursor를 전진한다. P는 고정 회귀 조합,
순환은 그 대응 Codex/thread/tmux/Herdr 통제군을 점차 채운다. 조합별 최근성을 별도 표시한다.
F는 모든 YAML을 무필터 호출하는 방식이 아니다. 시작 때 manifest의 expected occurrence를 확정하고
각 셀·프로필별로 순서대로 실행한다. E-36은 전용 호출/3540초 제한이 필요하다
(`tests/e2e/tui_relay/scenarios/README.md:108`).
E-11은 실제 두 provider 턴 동시성이라 #8124의 활성 턴 1 계약에서는 `BLOCKED_CAPABILITY`;
E-13은 claude-pipe 전용이라 초기 TUI 프로필에서 gap이다. E-35/36의 Herdr N/A도 coverage 0이다
(`scripts/e2e/tui_relay/herdr.py:286`). 이 칸들이 남으면 F는 `PARTIAL`, full-live PASS가 아니다.
fixture E-24/25는 offline 별도 결과, unsupported E-26/27/28과 retired E-32/33/34는 live 성공 수에 넣지 않는다.

기계 판독 정본은 [묶음 manifest](e2e-continuous/bundles.schema1.json)다. 이것은 `NOT_RUN` 계획이며
YAML hash·38 live 목록·P의 실제 host·controlled ingress provenance·공백·off/null 비용을 함께 고정한다.
`python3 -m unittest -v tests.test_e2e_continuous_manifest`는 누락/중복/fixture 혼입·거짓 PASS·
미실측 비용 0·human 승격·배포 hold 뮤턴트를 거부한다. manifest 자체는 실행/coverage 증거가 아니다.

### 2.3 비용 측정 후에만 주기 선택

초기 설정은 `enabled=false`, `rotation_interval_s=null`, `daily_full_at=null`,
provider별 일일 turn/token/time 한도도 `null`이다. 미지정 한도는 무제한이 아니라 정기 실행 불가다.
기존 #8124의 6시간/20분/8턴/야간40분/하루24턴은 이 설계의 활성 기본값으로 승계하지 않는다.

1. #8124 실물 수용 뒤 동일 시험 프로필에서 P, 대표 R, 긴 R4/E-36을 각각 명시된 단발 측정 창으로 실행한다.
   최초 1회로 누락 필드를 찾고, 활성 후보 묶음별 최소 3회(차가운 시작·warm 포함)의 실제 표본을 모은다.
   측정 자체도 예약한 provider 예산을 사용하며 실패/재시도 사용량을 포함한다.
2. 입력 post·provider native turn start·busy input 합류·내부 API 요청 수를 별개 계수로 저장한다.
   tool/API 요청이 여러 번인 턴을 1 token 단위처럼 취급하지 않는다. session/source identity 및 offset으로
   Claude usage message ID 중복을 제거하고 Codex 누적 token count는 baseline delta로 계산한다.
   참고 parser는 `src/services/codex_tui/rollout_tail/parser.rs:752`,
   `src/services/claude_tui/transcript_tail.rs:731`의 usage 형식이다. 일반 작업 계정 전체의 usage를 긁지 않는다.
3. 각 묶음에 실제 elapsed·native turn count·input/output/cache usage의 n/min/median/max를 기록한다.
   적은 표본에 p95를 붙이지 않는다. 종료되지 않은 턴은 시간/usage의 하한과 incomplete를 보존한다.
4. 측정 후 **배포 이벤트만 / 6시간 회전 / 12시간 회전 / 일일 전체**의 일일 예상 사용량을
   `배포 예상횟수×P + 회전횟수×조각별 측정 + F + 재시도 예약`으로 비교해 한 번 선택한다.
   회전과 전체가 같은 날 겹치면 중복 occurrence를 제거하고, 예산 부족 시 오래된 칸부터 다음 창으로 미룬다.

표본의 최대값에 작은 여유를 둔 단순 묶음 상한과 provider별 일일 상한을 함께 설정한다.
새 시나리오/모델/프로필로 측정 범위를 벗어나면 해당 조각을 다시 측정한다. 실행 중 한도를 넘으면
새 턴을 시작하지 않는다. 이미 보낸 입력을 성공으로 꾸미거나 새 kill 권한으로 끊지 않는다.
usage를 못 읽으면 `null/unknown`이며 token 0/비용 0이 아니다. 이 경우 token 실측 DoD는 미완료,
그 provider의 자동 주기는 off다. 일반 작업과 공유하는 계정의 잔여 quota를 보장한다고 주장하지 않는다.

## 3. 빠진 네 단언과 수정 전 RED 계약

### 3.1 공통 전제, 특히 사람 입력

각 단언은 instance/head/boot ID·channel/parent·provider/native session·host execution nonce와
메시지 ID/queued generation을 연결한다. 프로필 선언뿐 아니라 gate·실제 busy 턴·routing·host의
runtime witness가 있어야 한다. witness가 없으면 `UNEVALUABLE(precondition)`, 회귀 RED 아님.

조합은 #8124가 만든 **시험 DB/설정**에 아래 항목만 seed한다. 운영 설정을 복사하지 않는다.

| 시험 프로필 항목 | 생성/확인 계약 |
| --- | --- |
| routing | `cluster.enabled=true`, instance ID는 receipt node; `cluster.intake_routing.enabled=true`, mode=enforce. owner-authority scope를 쓰는 경로는 parent 두 ID만 명시(`src/config/runtime_profile.rs:274`) |
| agent/node | paired agent의 두 channel binding과 default_execution_node_id=receipt node. 독립 PG의 그 node만 online/ready로 광고; execution_slots unset으로 무슬롯 primary 조합 생성. runner의 활성 턴1 제한은 슬롯 광고와 별개 |
| busy gates | `ADK_BUSY_INJECT_DISCORD_CHANNELS`는 parent 두 ID, `ADK_BUSY_INJECT=all`, queued-head 경로는 `ADK_QUEUED_HEAD_INJECT=1`. process OnceLock 값이므로 boot 때 적용·관측. 적용 근거는 `busy_inject.rs:21`, `health/turn_deliver/inject.rs:122`, `queue_io/queued_head_inject.rs:35` |
| host/output | tmux 및 Herdr parent-only map을 별도 boot 프로필로 생성(§3.5). O/Legacy도 분리. fresh thread에는 host 예외·미리 만든 session을 넣지 않음 |

위 busy 양성 계약은 우선 Claude 대상이다. queued_head_ready의 Codex 확장은 현재 test cfg에만 있다
(`src/services/discord/health/turn_deliver/inject.rs:123`). Codex 대조군에 Claude의 10초 합류/➕ 기대값을
무조건 적용하지 않고, runtime capability가 없는 칸은 queue 동작 대조 또는 명시 gap으로 둔다.

**무인 Driver의 bot 입력은 사람 입력 원증상을 그대로 재현하지 못한다.**
`src/services/discord/router/intake_gate.rs:491`, :527은 Announce/allowed bot과 human을 구분하며,
`src/services/turn_orchestrator/queued_head/prefix.rs:50`은 `!author_is_bot`와
`preserve_on_cancel`을 요구한다. `DISPATCH:`로 바꿔도 같은 경로가 되지 않는다.
봇을 사람으로 위장하거나 `/turn/deliver` 성공을 Discord reaction 검증으로 치환하지 않는다.

확정한 수용 방식은 **원증상 human 입력 1회 RED/GREEN + 무인 controlled-ingress 회귀 + bot 대조군**이다.
R-marker/R-inject의 실제 human Gateway 수용은 Notify observer 준비 뒤 코디네이터가 agent-desktop으로
Discord 앱을 조작해 run nonce가 있는 B를 보내 수행한다. 사용자에게 입력을 요청하지 않으며 정기 수동 작업은 0이다.
무인 회차는 human 시나리오의 A를 시작하지 않고 해당 칸을 `NOT_RUN_HUMAN`, coverage 0으로 남긴다.
수정 레인의 실제 intake/owner fixture RED도 원인 증거로 연결하되 live human E2E로 승격하지 않는다.

E1b 앞의 E1-ingress는 기존 `intake_gate/live_entry_tests.rs:337`–:346의
`FullEvent::Message → router::handle_event` 진입점을 시험 전용 adapter로 재사용한다.
#8124 receipt의 instance/boot·실제 parent·run nonce를 검증하고, 동일 nonce의 원래 bot 이벤트와 중복 실행을 막는다.
controlled input에서 실제 host submit·REST reaction·Notify 관측까지 연결하되 ingress provenance는
`controlled_ingress`로 보존한다. 이는 human 분기 회귀를 무인 검증하는 별도 칸이며 실제 human Gateway의
전달 계층을 검증하지 않는다. bot 입력·controlled ingress·human Gateway의 실행 수와 coverage를 합산하지 않는다.

### 3.2 메시지별 반응 변경 이력 — R-marker, #8113

Discord REST의 Get Reactions는 현재 반응한 사용자 목록이지 변경 이력이 아니다.
`after`는 사용자 pagination이며 반응 시각 cursor가 아니다. polling으로 📬 add/remove가 사이에 지나가면
놓친다. 이력에는 입력 전 연결된 별도 Notify observer가 받는 Gateway reaction add/remove,
remove-all/remove-emoji를 사용한다. 단건 add/remove는 `user_id` actor로 귀속한다.
bulk remove-all/remove-emoji에는 actor가 없으므로 actor 필터로 버리지 않고, 해당 메시지 전체 또는 해당
이모지의 관측 집합 전체에 제거를 적용한다. 이벤트의 message/channel과 Gateway session/shard/sequence 순서,
**수신 monotonic 시각**을 보존하며 반응 자체 timestamp는 가정하지 않는다.
actor 없는 두 bulk 이벤트 replay와 이를 버리는 뮤턴트는 제거 이력 누락이나 불완전 이력의 PASS를 RED로 검출해야 한다.
근거: [bulk reaction 제거 payload](https://docs.discord.com/developers/events/gateway-events#message-reaction-remove-all),
[Get Reactions](https://docs.discord.com/developers/resources/message#get-reactions),
[reaction events / payload sequence](https://docs.discord.com/developers/events/gateway-events#message-reaction-add)
(2026-11-13 문서 확인, 앱 접속 없음).

관측 구현은 #8124 설계(:275)의 **serenity observer example + Python NDJSON 소비**를 따른다.
Notify observer에만 GUILDS/GUILD_MESSAGES/GUILD_MESSAGE_REACTIONS의 필요한 intent를 부여한다.
제품 Gateway intent는 그대로 둔다(`src/services/discord/runtime_bootstrap.rs:103`, :681).
Cargo.lock:2928의 serenity 0.12.5 `RawEventHandler`는 Event만 넘기며 callback을 별도 task로 dispatch한다
(SDK `src/client/event_handler.rs:507`, `src/client/dispatch.rs:54`). callback counter/시각을
원시 Gateway sequence/수신 순서로 부르면 안 된다. SDK `src/gateway/ws.rs:119`의 recv_json도 crate-private다.
E1a에서 **필터 전 Dispatch의 원시 sequence와 순서를 확보하는 seam**을 먼저 입증한다.
공개 RawEventHandler만으로 충분하다는 설계는 채택하지 않는다. seam 미확보 시 strict-history는
`BLOCKED_CAPABILITY(observer-sequence)`이며 snapshot PASS로 낮추지 않는다. dependency/vendor 변경이
필요하면 그 크기와 유지 비용을 별도 PLAN으로 제시한다. 이번 문서가 그 변경을 승인하지 않는다.

observer READY → 새 message 전송 → provider actor별 모든 reaction operation 수집 → 종료 fence와
snapshot 대조 순서다. sequence는 전체 Dispatch에서 검사한 뒤 시험 채널/메시지만 저장한다.
재연결·sequence gap/역순·bounded queue overflow·window 조기 종료는 `UNEVALUABLE(observer-gap)`다.
제품의 해당 메시지 reaction 작업 완료 receipt와 pending 작업0 확인 뒤 Notify fence를 보내고,
그 fence의 Gateway sequence까지 처리했는지 검증한다. 완료/pending 관측이 없으면 window는 불완전하다.
같은 메시지의 늦은 operation은 최종 summary 전에 반영하며, 결과 이모지 📬/➕/❓와 별도 status cleanup을 구분한다.
REST snapshot의 `me`는 Notify 자신이지 provider 봇이 아니다. provider actor ID는 이벤트 및 필요한 경우
reaction-user 조회로 대조한다. 시작 후 새 message라 초기 provider reaction 집합은 비어 있어야 한다.

정상 managed busy 주입 대상 B의 결과는 ➕ add 1, 📬 add/remove 0이다. handback은 📬 1,
unconfirmed는 ❓ 1의 별도 기대값이며, 채널 전체 다른 actor/메시지 반응과 섞지 않는다.
수정 전 `intake_gate.rs:1004`의 BusyActiveTurn default와
`intake_queue_transaction.rs:99`, :375의 QueueState는 📬를 먼저 만들므로
마지막 ➕만 보는 시험은 GREEN이어도 이력 단언은 RED가 된다. 원인 뮤턴트는 새 보류 조건 제거,
반대 대조군은 gate off/Herdr/Codex/allowed bot의 기존 📬 기대값이다.

REST는 시작·끝 snapshot과 필요 actor 조회만 한다. route bucket/Remaining/Reset-After 및 429의
Retry-After/retry_after를 따르고, 남은 시험 deadline보다 길면 환경 실패로 끝낸다.
reaction endpoint의 초당 고정 quota를 가정하지 않는다. 현재 문서의 bot global 50 req/s도 목표 처리율이 아니다.
근거: [Discord rate limits](https://docs.discord.com/developers/topics/rate-limits).
메시지 조회에는 기존 `discord.py:25`, :38의 제한 재시도/최대 delay 규칙을 재사용한다.
입력 mutation timeout에는 자동 POST 재시도를 붙이지 않는다.

### 3.3 진행 중 입력의 주입 시점 — R-inject, #8114

새 단언은 **현재 A가 끝나기 전 B의 실제 host submit 확인** 및
`host_submit_observed − inbound_received ≤ 10초`를 함께 요구한다. 10초는 측정된 성능값이 아니라
4분 보류를 잡기 위한 **초기 기능 계약 제안**이다. 측정 뒤 유지/조정하되 턴 종료 전 조건은 완화하지 않는다.
Discord 생성→수신 지연은 별도 수치이며 server/observer의 서로 다른 clock 원점을 빼지 않는다.

**원증상 native-busy 시험과 managed 대조군은 별도 occurrence다.** 원증상의 A는 AgentDesk가 시작하지 않은
동일 native session의 실제 busy source이며, foreground hold tool 진입과 A terminal record를 남긴다.
B 수신 직전 managed active claim/inflight와 durable external holder가 모두 없음을 동일 session/nonce에
연결해 확인해야 한다. `native` 라벨만 붙이거나 managed 턴 A를 시작한 것만으로 이 조건을 충족하지 않는다.
`intake_gate.rs:989`–:1015·:1073의 managed active 큐잉/return과
`router/message_handler/provider_dispatch/owner_input.rs:200`–:241의 external-holder/`hand_busy`는
`router/message_handler/intake_turn.rs:1753`–:1768과 다른 경로다.
따라서 B가 실제 pre-submit busy 분기에 도달한 witness를 필수로 저장한다. 선행 상태나 분기 witness가
없으면 `UNEVALUABLE(precondition)`이며 원증상 RED/GREEN으로 세지 않는다.

managed 대조군은 managed active claim/inflight가 있는 A에 B를 보내 queue/`hand_busy` 경로의 정상 동작을
검증한다. 그 GREEN은 #8114 원증상 분기 coverage가 아니며 원증상과 결과를 합치지 않는다.
각 시험은 busy를 확인한 뒤 B를 보내고, 서버 수신·queue 수락·host write/submit receipt·provider가 B를 읽은
native source offset·A end를 잇는다. fixture hold는 최대 45초 후 자연 종료하므로 broken 경로도 끝나지만,
B를 A 종료 후 보내는 순차 시험은 불가능하다. 구체적으로 다음을 저장한다.

| 단계 | 증거와 판정 |
| --- | --- |
| 원증상 선행/분기 | native busy source identity·offset, B 수신 직전 managed claim/inflight 및 durable external holder 부재, 실제 pre-submit busy 분기 도달 receipt. 하나라도 없으면 UNEVALUABLE |
| 수신/수락 | B의 Discord message ID 또는 controlled ingress ID·generation, provenance, A turn nonce, gate의 실제 적용값. server의 한 clock ID로 수신 시각 기록 |
| host submit | 해당 host의 실제 입력 제출 완료 receipt. adapter가 받은 시각을 같은 서버 clock으로 기록해 상한을 보수적으로 측정. 함수 진입/queue 성공/붙여넣기 호출 의도는 아님 |
| provider 관측 | 같은 native session의 B user/input source record와 offset, A의 terminal record보다 앞선 순서. fixture가 요청한 고유 marker를 provider 출력으로도 확인 |
| 종료/유일성 | submit은 A end보다 **엄격히 이전**; B 처리 1회. 새 턴의 응답이 B marker를 포함한 것만으로 통과 불가 |

trace는 인스턴스의 bounded observation NDJSON에 metadata만 기록하고, body/token/운영 session은 싣지 않는다.
receipt 없는 host나 provider format은 `BLOCKED_CAPABILITY`다. 10초 경계·같은 시각·A end 직전/직후·
늦게 도착한 source record를 deterministic replay로 검증한다. native source 순서와 수집 도착 순서를 구분한다.
수리 전 고정 SHA에서 **위와 같은 선행/분기 witness** 및 :1768의 `hosted_tui_busy_pre_submit`을 함께 관측한다.
이 조건의 `intake_turn.rs:1753` Herdr 제외→:1768 `defer_before_submit`은 A 종료 뒤 B를 처리해
`submit < A_end` 또는 10초 단언에 RED여야 한다. 수리 후 GREEN과 Herdr 제외를 복구한 뮤턴트 RED도
동일 witness 조건으로 제출한다. 이미 Herdr를 포함하는 `queued_head_covers`나 managed 대조군의 GREEN만으로
원증상을 통과시키지 않는다. 결과 순서만 유지돼도 시간 단언이 RED여야 한다.

### 3.4 새 thread 첫 입력 — R-fresh, #8115

비밀 없는 시험 프로필에서 paired agent의 `default_execution_node_id`를 시험 local node로 지정하고,
그 node는 ready·heartbeat 유효·provider/auth profile 일치, **execution_capacity v1/slots 미광고**로 만든다.
`cluster.execution_slots`는 unset이다(`src/config/runtime_profile.rs:193`). preference 자체가
capacity-aware를 연다(`placement.rs:24`, :96). 기존 owner/session/binding이 없는 **새 public thread**를
parent 아래 만들고 첫 입력 하나만 보낸다. 재시도 입력이나 직접 session pre-create로 복구하지 않는다.

첫 입력 뒤 초기 제안 deadline 90초 내 native session start, expected node의 binding, provider 응답 1회를
모두 확인한다. 첫 메시지 전에 active owner/binding이 있으면 오염으로 평가 불가다.
main의 새 `rank(..., primary)`는 이미 수정됐으므로 제품 코드는 건드리지 않는다.
수정 전 비교는 `135a168e6^`의 `execution_capacity.rs:56`–:64(모든 후보에 version1·slots 필수)를
사용한다. preferred node가 탈락하여 첫 세션이 생기지 않으면 RED. 원인 뮤턴트도 primary 보존 분기만 제거한다.
ready primary 무슬롯/ready primary 슬롯 있음/primary offline의 세 통제군으로 실패가 단순 auth/준비 부족이 아님을 고정한다.

### 3.5 thread·channel 실제 host — R-host, #8118

시험 프로필의 `session_hosts.herdr.channels`에는 **parent 두 개만** 넣고, 방금 만든 thread ID를
수동으로 추가하지 않는다. 그 우회는 원증상을 숨긴다. endpoint는 receipt가 만든 local
`legacy_dedicated` Herdr의 execution_node/socket/home/session이고 기본 endpoint fallback은 금지한다
(`src/config/session_hosts.rs:75`, :91). tmux 통제 프로필은 같은 parent를 Herdr map에서 제외한다.
프로필 전환은 인스턴스 수명 계약을 거쳐 boot ID를 새로 받으며, 실행 중 config hot switch로 하지 않는다.

| 기대 host | 실제 확인 |
| --- | --- |
| Herdr | health detail의 실제 admission + `agentdesk herdr status`의 channel/provider/nonce/bound/pane witness(`herdr.py:145`, :187) + receipt endpoint/workspace/pane의 provider child identity |
| tmux | receipt의 전용 socket에서 exact session/pane ID·PID 시작 identity·native session/turn binding. `AgentDesk-*` 이름 패턴이나 host marker 파일 하나로 추정하지 않음 |
| thread 출력 | actual thread→parent binding과 O source/output sink, 첫 응답·busy 후속·취소 결과가 그 thread에만 있음. parent 오송신·legacy fallback 0; `source not bound`는 FAIL |

기존 health/status는 channel→endpoint join이 부족하다고 명시한다(`herdr.py:155`). 새 관측 adapter는
expected와 observed host를 별도 필드로 유지하고 exact nonce로 join한다. config에 Herdr라고 적힌 사실을
actual host로 복사하면 안 된다. binding DB만 있고 provider child가 없거나 모호하면 평가 불가다.
수정 전 `herdr_launch.rs:56`→`session_hosts.rs:158`은 exact channel 조회, `tui_o/channel_policy.rs:74`도
parent 상속이 없어 새 thread가 tmux/legacy로 가면 **응답 본문이 정상이어도 host/output 단언은 RED**다.
뮤턴트는 thread의 parent 상속을 각각 host·output 쪽에서 제거한다. tmux 대조군은 계속 GREEN이어야 한다.
부모 필드의 정본/이름 변경은 #8119 소유다(`docs/session-naming.design.md:81`). 새 이름 문자열에 의존하지 않는다.

### 3.6 RED 증거의 등급

위 네 항목의 **원인 분기와 예상 RED 조건**을 고정했으나, 이 설계 레인에서 새 시나리오를 실행한 것은 아니다.
구현 수용은 수리 전 고정 SHA·후 SHA·동일 프로필/단언·바이너리 SHA·raw trace를 한 쌍으로 제출한다.
#8113의 기존 레인 보고서는 실제 intake/owner fixture에서 📬 add/remove→➕ RED를 보여 준다
(`i8113/report-impl-1.impl3.md`의 single_marker 표). 이를 새 Discord E2E의 실행 증거로 대신하지 않는다.
#8115도 기존 PG 시험이 있다고 새 thread live GREEN을 주장하지 않는다.
수리 전 binary를 시험 인스턴스 격리 adapter와 함께 실행할 수 없는 경우 test transport/observation만
같게 이식한 harness commit을 별도 기록하고 제품 수정 바이트가 들어가지 않았음을 diff로 증명한다.
원인 제거 뮤턴트는 각 단언에 RED, 복원 바이트 SHA256 동일과 복원 후 GREEN을 요구한다.

#8123 기능 지도와 실행 명세를 연결할 표는 다음과 같다. operation 이름은 아직 등록되지 않은 계약이다.

| 기능 claim | scenario / 필수 operation | 원증상 qualifier | 현재 live 증거 |
| --- | --- | --- | --- |
| busy 입력 결과 표시 | R-marker / reaction_history, host_identity | #8113, human/managed/Claude/tmux/channel/busy | gap; 기존 fixture RED는 별도 |
| busy 입력 즉시 합류 | R-inject / inject_before_turn_end, injection_latency, exactly_once, pre_submit_busy_witness | #8114, human 또는 controlled_ingress(별도)/native-busy/Claude/Herdr/channel; B 직전 managed claim/inflight·durable external holder 없음 | gap; witness 없으면 UNEVALUABLE |
| managed busy 대조군 | R-inject-managed / inject_before_turn_end, injection_latency, exactly_once | managed active claim/inflight/Claude/Herdr/channel/busy; 원증상 분기와 별도 | gap; #8114 원증상 coverage에 합산 안 함 |
| 새 대화 첫 입력 | R-fresh / fresh_session_started, execution_node | #8115, thread/primary-no-slots/idle-unowned | 제품 fix·PG 시험 존재, live gap |
| thread host·출력 상속 | R-host / session_host, output_sink, busy_followup, cancel | #8118, Herdr/thread/idle→busy | gap |

## 4. 실행 주체와 읽기 형식

실행 주체는 **dcserver 밖의 작은 Python runner + #8124 supervisor**, macOS launchd는 이를 깨우는 역할이다.
dcserver 내부 scheduler는 재시작 시험과 같이 죽고 스스로의 실패를 감추므로 사용하지 않는다.
campaign monitor는 실행·Discord token 소유자가 아니라 결과 소비자다. 새 queue 서비스/분산 스케줄러는 만들지 않는다.

활성화 후 launchd는 짧은 tick에서 enabled·due_at·일일 잔여 예산·채널 lease만 검사한다.
배포 완료 요청은 deploy SHA와 request ID가 담긴 비밀 없는 inbox 파일 1개다. 별도 시험 호스트의
전달은 coordinator의 기존 ref 전달 경로를 사용하고, runner가 운영 API나 다른 호스트의 운영 root를 찾지 않는다.
같은 request ID는 중복 실행하지 않고, 오래된 배포 요청은 superseded로 기록해 catch-up 폭주를 막는다.
주기 tick은 provider 실행 주기가 아니다. off여도 disabled/heartbeat를 발행하여 감시자가 고장과 off를 구분한다.

결과는 #8124의 `outputs/e2e/<run-id>/` 아래 원시 report/observer/host/usage를 남긴다.
기존 `report.<cell>.json` schema2는 유지하고, 별도의 **continuous schema1 summary**를 tmp→atomic rename으로 발행한다.
summary와 별도로 단일 supervisor의 `status.json`에 heartbeat·enabled·running/due·last completed run·outbox pending을 둔다.
아래는 **미구현 형식 예시**다.

```json
{
  "schema_version": 1,
  "run_id": "uuid",
  "request_id": "deploy-sha-or-scheduled-occurrence",
  "suite": "P",
  "instance_id": "uuid",
  "boot_id": "uuid",
  "head": "full-sha",
  "binary_sha256": "sha256",
  "profile_hash": "sha256",
  "lease": {"resource": "test-parent-pair", "owner_run_id": "uuid"},
  "started_at": "UTC",
  "completed_at": "UTC",
  "expected": ["R-marker/claude/tmux/channel/busy/human"],
  "occurrences": [{
    "key": "R-marker/claude/tmux/channel/busy/human",
    "status": "UNEVALUABLE",
    "reason": "NOT_RUN_HUMAN",
    "assertions_expected": ["reaction_history", "host_identity"],
    "assertions": [],
    "real_provider_contacted": false,
    "evidence_paths": []
  }],
  "result": "PARTIAL",
  "attempts": [],
  "usage": {"claude": {"native_turns": null, "input_tokens": null, "output_tokens": null, "cache_read_tokens": null, "cache_write_tokens": null}},
  "elapsed_s": 0.0,
  "usage_complete": false,
  "notification_pending": 0
}
```

occurrence key는 scenario×provider×host×surface×idle/busy에 ingress provenance(human_gateway/controlled_ingress/bot)·managed/native·
preferred-node 조건·O/Legacy qualifier와 assertion operation/occurrence를 더한다.
#8123 기능 지도에는 같은 key로 join한다. 선언 coverage와 실제 실행 최근성은 별도 열이다.
같은 cell의 다른 host나 bot/controlled_ingress 실행을 human Gateway/Herdr 칸에 합산하지 않는다.

PASS는 expected=executed live occurrence set, 실행 수>0, 필수 assertion 전부 complete/pass,
실제 provider contact, instance/head/binary/profile 일치, timeout/dirty residue 없음일 때뿐이다.
fixture/dry-run/skip/known-gap/옛 summary/미지원 schema/누락 파일은 PASS가 아니다.
FAIL은 관측된 기능 위반, UNEVALUABLE은 증거/환경 부족, PARTIAL은 필요한 칸 미실행;
DEFERRED_LEASE/BUDGET, DISABLED도 별도 상태로 남긴다. 0과 null의 차이를 보존한다.
재시도 PASS는 첫 실패를 덮지 않고 `RECOVERED_ENV` 또는 `FLAKY`로 요약한다.

`briefing_checks.py`는 저장소 밖 issue-pipeline 도구다. 현재 :161–:213은 DAG/host/disk만 읽고
E2E 소비자는 없다. **S3의 별도 배선 산출물**로 `BC_E2E_STATUS_FILE` 또는 비밀 없는 시험 호스트
summary mirror를 읽는 순수 소비자를 추가한다. 전체 briefing 명령은 :189의 issue sync 쓰기를 포함하므로
이 설계 조사에서 실행하지 않았다. coord monitor 구현 원본도 이 레인에 제공되지 않았으므로 배선 완료를 주장하지 않는다.
같은 순수 status 판정 함수를 소비하도록 다음 한 줄 형식을 고정한다.

```text
E2E_CHECK suite=P head=<sha> result=PARTIAL live=4/6 assertions=12/16 age_s=120 stale=0 reason=NOT_RUN_HUMAN
E2E_MONITOR enabled=0 heartbeat_age_s=30 last_run=<id> pending_alerts=0 next_due=none
```

활성 주기가 정해진 뒤 freshness 한도는 `interval + 최대 실행시간 + tick 여유`로 계산한다.
heartbeat는 tick 두 번 누락 시 경고; future timestamp·옛 head·schema 오류는 UNKNOWN이다.
첫 실행 전은 NO_DATA, 의도적으로 off는 DISABLED다. coordinator가 보는 배포 SHA와 summary SHA가
다르면 STALE_HEAD이며 최근 완료 시각만으로 green이 되지 않는다.
실패/기한 초과/runner 사망은 supervisor가 부분 summary를 완성하고 #8124의 notification outbox에 넣는다.
Notify로 등록 operator에게 제한 재시도하는 전송은 구현 수용에서 한 번 증명한다. 전송 실패는 pending을
보존해 감시자가 표시한다. GitHub 이슈/PR/DAG 자동 수정·실패마다 이슈 생성은 하지 않는다.
동일 head·qualifier·실패 사유의 알림은 최초1회와 상태 변경 때만 보내고, 알려진 human/gap 칸을 tick마다 재알림하지 않는다.
끄기는 enabled=false 한 곳으로 새 턴 예약을 멈추고 진행 중 run은 소유 수명 계약에 따라 끝낸다.
launchd unload나 test job pause가 운영 dcserver 중단·배포 중단을 뜻하지 않는다.

## 5. 실패 처분과 E-51 조사

### 5.1 처분·재시도

기존 배포의 healthy 후 fail-open 불변식은 `scripts/deploy-release.sh:4870`–:4876에 있다.
이번 작업으로 rollback/peer 전파 차단/다음 배포 차단을 붙이지 않는다. PASS→FAIL 비교는
같은 assertion version·qualifier·provider model/profile의 마지막 complete PASS를 기준으로 한다.
첫 실패는 NEW_FAIL, 비교 가능한 과거 PASS가 없는데 regression이라고 단정하지 않는다.

| 실패 영역 | 즉시 처분 | 다음 자동 배포 제안 |
| --- | --- | --- |
| 손실·중복·순서·새 세션 불가·host/output 오배치 | 기능 FAIL, 증거와 이전 PASS를 붙여 우선 알림. 의미 위반을 환경 부하로 면제하지 않음 | 초기 **알림만**. 향후 차단이 필요하면 이 영역의 confirmed regression에 한정한 opt-in hold를 별도 결정 |
| 시험 경계/receipt/allowlist 위반 | 해당 **시험 실행**을 중단하고 test runner pause. foreign cleanup 금지 | 운영 권한/프로세스/배포를 자동 변경하지 않음 |
| 반응 깜빡임 | 완전한 이력이 있으면 기능 FAIL | 알림만 |
| busy 주입이 A 종료 뒤에 일어남 | 전제 충족·완전한 receipt가 있으면 기능 FAIL | 우선 알림만. 단순 느린 응답과 구분 |
| 시간 상한만 위반·host 부하 동반 | `ENV_SUSPECT`와 원 FAIL 보존 | 차단 안 함, 조용한 창에서 같은 binary/profile 1회만 재시도 |
| provider auth/quota/429, Discord 장애, observer/log 유실 | ENV/UNEVALUABLE | 차단 안 함. 시험 새 턴은 보류하고 알림 |
| lease 경합/예산 부족 | DEFERRED, 실행/coverage 0 | 차단 안 함. catch-up 무한 재시도 없음 |

부하 분류용으로 시험 호스트의 가용 메모리·load/코어·scheduler/observer lag를 run 전/중 기록한다.
초기 단순 경보 후보는 메모리 4GiB 미만 또는 load/cores>1.5가 60초 지속이다(**미교정 제안**).
부하 수치 없이 timeout을 HOST_OVERLOAD로 확정하지 않는다. 부하가 높아도 중복/host 오배치가 있으면
기능 FAIL을 유지한다. 부하 해소·Gateway/observer 정상·남은 예산·정확한 idle 확인 뒤 1회 재시도하며,
두 번째 결과와 원본을 모두 남긴다. 그 뒤 실패가 같으면 반복 회귀 의심, 성공해도 처음 실패는 지우지 않는다.
오염된 세션을 자동 kill/reset해서 통과시키지 않고 #8124 소유 정리만 사용한다.
입력 ACK 불명은 Discord nonce/message 조회로 중복 여부를 확인하기 전 재송신하지 않는다.

### 5.2 E-51 FAIL 7/37과 S0 전수 replay

S0는 mac-book deploy 로그 **398개/34,757,622 bytes**, mac-mini **352개/27,428,192 bytes**를
읽기 전용으로 복사했다. 복사 snapshot의 inventory·파일별 SHA256·report SHA를 남긴다.
브리프의 약 400개는 snapshot에서 398개였으며, 보존본 밖 로그의 존재나 결과를 추정하지 않는다.
전수 표의 시각은 smoke **묶음 시작 stamp**이며 시나리오별 start/end와 구별한다.
정식 경계는 **2026-11-06 00:00 KST**다. 현재 snapshot까지 94개 묶음·188개 시나리오 판정이 있다.

| 정식 KST 경계 이후 | PASS | FAIL | SKIPPED | 실제 scenario 실행 |
| --- | ---: | ---: | ---: | ---: |
| mac-book E-50 | 26 | 8 | 14 | 34 |
| mac-book E-51 | 37 | 8 | 3 | 44 (reset 실패 1건은 입력 전) |
| mac-mini E-50 | 0 | 0 | 46 | 0 |
| mac-mini E-51 | 0 | 0 | 46 | 0 |

mini의 no-relay-channel skip은 실행 0이며 PASS가 아니다. 로그의 `path=herdr`는 보고/config 경로이고
actual runtime host 증거는 아니므로 실제 host는 unknown으로 남긴다. 배포/시나리오 결과도 분리한다.
원문 수치 E-50 **24 PASS/3 FAIL**, E-51 **30 PASS/7 FAIL**은
**11-06 09:00 KST~11-12 12:58:35 KST 묶음 시작 창**의 같은 40배포에서 동시에 재현됐다.
원 run-list가 없으므로 이 창은 `match_not_identity`이며 원래 37건 identity를 확정한 것은 아니다.
정식 KST 경계와 현재 snapshot에는 이 추정 창 앞뒤의 실행이 추가된다. 기간 밖 실패로 분자를 채우지 않는다.

| 추정 E-51 7 FAIL의 원문 witness | 건수 | 분류와 한계 |
| --- | ---: | --- |
| composer readiness 로그 카운터를 deploy judge가 거부 | 5 | **불명**. scenario report PASS와 deploy FAIL이 함께 남음. 과부하나 제품 회귀를 확정할 host 지표 없음 |
| 첫 Discord 응답 대기 timeout | 1 | **불명**. 시간 위반만으로 과부하라고 하지 않음 |
| cell-reset HTTP 409 / provider runtime unavailable | 1 | **불명**. scenario 입력 전 중단; native 턴 실행으로 세지 않음 |
| 원문 확보 / 원인 확정 | **7 / 0** | 과부하 0·외부 503 0·Herdr 전환 0·진짜 회귀 0·불명 7 |

정식 전수의 FAIL 16개 중 외부 HTTP 503을 직접 기록한 실행은 **1개**, 나머지는 **불명 15개**다.
이 외부 503 실행을 위 E-51 7건에 넣지 않는다. 시간 상한·readiness·로그 발췌 누락을 원인과 혼동하지 않는다.
운영 세션/설정/토큰으로 보강하거나 새 provider 턴을 실행하지 않았다.

시각·호스트·head·결과·첫 실패 단계·분류·원문 파일/행/SHA·marker·report·duration·ACK post는
[기계 판독 replay](e2e-continuous/s0-replay.json)와 [전체 표](e2e-continuous/s0-replay.md)에 있다.
`python3 scripts/e2e/smoke_replay.py`는 보존 snapshot만 읽어 재생성하며 run/host/scenario 중복이나
report identity 불일치는 오류로 중단한다. native turn·usage·scenario 중 host 부하와 actual runtime host는
자료가 없어 null이다. 이번 역사적 운영 스모크 표본을 신규 P/R/F 시험 인스턴스 비용 n으로 합산하지 않는다.
새 묶음 비용은 여전히 n=0이므로 주기·예산 활성화와 자동 차단율은 결정하지 않는다.

## 6. 채널 lease와 작은 모델

현재 `scripts/e2e/tui_relay/lease.py:15`는 실제 채널이 아닌 cell 문자열로 키를 만들고,
:34의 read 후 :41의 O_TRUNC open 사이 경합이 있다. TTL 1시간 뒤 살아 있는 run을 탈취할 수 있고,
:50의 unconditional unlink도 후계 소유자를 지울 수 있다. `run_tui_relay.py:5416`과
`post_deploy_turn_smoke.py:428`이 이를 사용한다. **현재 파일을 안전한 channel lease로 재사용하지 않는다.**
이 일반/운영 경로의 기존 결함은 main 좌표로 고정한 후속이다. #8120이 TTL 강제 회수·패턴 kill·
운영 root 삭제를 추가하며 범위를 넓히지 않는다.

시험 host에서는 parent 쌍을 **단일 resource**로 보고 고정 inode의 nonblocking flock을 사용한다.
키는 guild+정렬한 실제 parent IDs+bot-set이며 cell 별칭/프로필/자식 thread와 무관하다.
#8124의 bot-set lifetime flock과 같은 원칙(close-only, unlink/rename/TTL 탈취 없음)을 쓰되,
**인스턴스 lifetime lock과 run의 channel lock은 구분**한다. server가 살아 있다고 여러 driver가 동시에
입력해도 된다는 뜻이 아니다. global 활성 provider 턴 1이 초기 계약이다.

배포 companion P·정기 조각·수동 instance E2E가 모두 같은 채널 lock을 획득한다.
보유 범위는 preflight→observer ready→첫 입력→마지막 단언/idle·부분 summary까지다.
고정 파일을 O_CREAT로 열어 flock 성공 뒤 owner JSON을 갱신하며 owner JSON은 진단용이다.
supervisor→해당 driver에만 FD를 전달하고 fstat device/inode·instance witness를 검증한다.
observer/provider/임의 helper에는 FD를 무제한 전파하지 않는다. 드라이버가 살아 있으면 parent 사망 후에도
임대는 남고, 둘 다 사망하면 커널이 회수한다. 다음 run은 host의 실제 idle을 다시 입증해야 한다.
이전 provider가 남아 있거나 idle 판독 불가면 `UNEVALUABLE_DIRTY`로 시험을 보류한다.
새 kill/reset/reap 권한을 이 lease에 넣지 않는다.

HTTP timeout 직후의 idle만으로는 아직 도착하지 않은 POST를 배제할 수 없다. 전송 **직전**에
run/instance/boot/nonce가 있는 mutation-pending 기록을 남기고, ACK와 해당 입력 settlement가 확인돼야 닫는다.
ACK 불명·중간 crash면 다음 보유자는 lock을 얻어도 이 기록 때문에 새 입력을 보내지 않고 시험을 pause한다.
owner JSON의 생존 추정/TTL과 이 미완결 전송 기록을 혼동하지 않는다. 정확한 메시지/settlement를
나중에 확인해 해제하는 좁은 복구만 허용하며, 자동 재POST·시간 경과 삭제·idle snapshot 하나로 해제하지 않는다.

우선순위는 배포 요청→명시 수동 검증→정기 실행이다. 보유자를 선점하지 않고, 바쁜 정기 tick은
DEFERRED를 남긴다. 두 parent를 단일 resource로 잡으므로 multi-lock deadlock이나 thread별 우회가 없다.
다른 호스트는 ref만 전달하고 lock 파일을 공유/NFS/SSH TTL lease로 만들지 않는다.
기존 release E-50/51은 **다른 운영 채널**에 남아 있어 이 시험 lock에 연결됐다고 주장하지 않는다.
배포 후 같은 SHA의 **시험 인스턴스 P 요청**부터 직렬화하며, 이후 스모크를 시험 채널로 옮길 때도
동일 wrapper를 사용한다. 운영 스모크를 옮기기 전에는 기존 권한·설정·fail-open 경로를 바꾸지 않는다.

제품 상태 차트 변경은 없다. 다만 run lease의 새 수명에는 작은 모델 검토가 **필요**하다.
#8124 조각0의 bot-set/FD 모델을 재사용하여 Pending→Held→Running→Draining→Released와
parent/driver crash·double acquire·FD 유실·옛 owner receipt·release 전 늦은 send·dirty 재진입을 넣는다.
mutation-pending 작성 전후 crash와 ACK 불명→늦은 서버 수락도 모델에 포함한다. lock 해제 뒤
미완결 전송이 있으면 Pending 대신 PausedUncertain으로 가며, 다음 writer가 시작하지 않아야 한다.
불변식은 resource당 live 입력 writer≤1, 살아 있는 holder 탈취0, foreign cleanup0,
Released 후 이전 writer의 신규 입력0이다. 생존성은 모든 holder 종료 및 실제 idle 확인 뒤 다음 요청 진행이다.
L0의 실제 TLC 결과·짧은 반례·실제 Python flock 재생은
[lease 수명 모델 보고서](statecharts/e2e-run-lease/lease.report.md)에 있다. 정상 GREEN은 제안한 수명 계약의
유계 검증이며 현재 제품 lease를 고쳤다는 뜻이 아니다. 기존 read/O_TRUNC·TTL·unlink 반례는 별도로 재생한다.
분산 lease·복잡한 자동 복구·새 운영 정리 권한은 모델과 구현 범위에 넣지 않는다.

## 7. 구현 조각과 수용 순서

정본 상한은 `pr_cap_prod.py:15`의 **prod FILES=30, NET=1500**, +1200 전에 PLAN_READY다.
아래는 코드 조사에 따른 추정이며 실제 파일/순증 측정으로 다시 확인한다. 문서/시험 제외 규칙을 이용해
SDK vendoring이나 외부 소비자 코드를 숨기지 않는다. giant driver에는 registry/호출만 넣고 child로 분리한다.

| 조각 | 범위·예상 prod 파일/순 코드 | 선행·수용 |
| --- | --- | --- |
| S0 | 보존 로그/usage 자료 수집·replay 표·묶음 manifest, 0–2 / +0–180 | 94 묶음/188 판정과 원문 수치 일치 창을 분리해 확보. 원인·usage unknown 유지. 근거 없는 0/PASS와 기간 밖 표본 혼합 RED |
| L0 | run lease와 #8124 FD 수명 작은 모델, 0–1 / +0–100 | #8124 조각0와 통합 검토. TTL 탈취/동시 writer/부모 crash/FD 누락 뮤턴트 RED |
| E1a | serenity sequence seam spike·Notify observer/example·NDJSON, 3–6 / +400–800 | #8124 I1/I2/I3a의 경계, I3b의 실제 앱 identity. seam 미입증은 다음 조각 진행해도 strict-history claim off. gap/역순/overflow/actor 혼동·actor 없는 bulk 제거 유실 RED |
| E1-ingress | test-only FullEvent→router adapter·receipt/boot/parent/nonce 검증·원래 bot 이벤트 중복 방지·provenance, 2–4 / +180–350(미실측) | E1b 앞, #8124 I2/I3a 경계 및 I3b 실제 host·E1a 관측 재사용. 경계/옛 boot/nonce 재사용·이중 실행·controlled_ingress를 human Gateway로 합산하는 뮤턴트 RED |
| E1b | busy trace·reaction/주입 adapter·R-marker/R-inject·human 수용, 4–8 / +600–1100 | I3b + E1a + E1-ingress. 수리 전 #8113/#8114 RED, 수리 후 GREEN 및 원인 뮤턴트. #8114 native-busy 선행/분기 witness 필수·managed 대조군 분리. human Gateway 1회는 코디네이터 agent-desktop; 정기 수동 0 |
| E1c | fresh/thread host/output adapter·R-fresh/R-host, 3–7 / +450–900 | I3b, #8115 fix 재사용, #8118/#8119 parent descriptor 계약. 수리 전/후 및 host/output 개별 상속 제거 뮤턴트 |
| S1 | 단일 runner·channel flock·budget·manifest/cursor·배포 inbox, 4–8 / +600–1050 | L0 + #8124 I3a/I3b, capability별 E1. 동시 acquire·crash·old owner·due 경계·off·예산 reservation RED |
| S2 | summary/status/outbox·공유 순수 소비자, 3–6 / +350–700 | S1. 0건/skip/옛 head/schema/누락 단언/timeout/usage unknown/알림 실패 RED. scheduled 실제1회 + 실패 알림1회 증거 |
| S3 | briefing_checks·coord monitor 배선 및 deploy companion 요청, 2–5 / +150–400 | S2. 외부 도구 소유 coordinator와 파일 경계 확정 후 별도 리뷰. fixture status 소비·STALE/NO_DATA/disabled/host 불가 검증. deploy는 fail-open 유지 |

#8124 최소 순서는 기존 설계의 **조각0 → I1(root/PG/credential/플래그) → I2(host/HOME/API 경계)
→ I3a(supervisor/FD 수명) → I3b(Driver 실제1턴)**을 따른다. E1 명세·offline replay는 앞서 만들 수 있지만
실제 live 완료는 I3b 뒤다. 이 네 앱/allowlist 사용자 결정과 충돌하는 옛 provisioning 문구는 #8124 구현 때 정정한다.
#7335는 하네스 전체 재설계의 선행으로 확대하지 않는다. 현재 범위는
`docs/test-infra.design.md:482`의 Codex Herdr 생성·후속·회수·취소 실물 각1회다.
같은 조건의 기존 증거를 검증해 재사용하거나 I3b 이후 시험 인스턴스에서 확보한 뒤 **그 Codex Herdr 칸만** 연다.
없으면 BLOCKED_CAPABILITY이며 다른 Claude/tmux 조각까지 가짜 PASS 또는 전면 대기로 바꾸지 않는다.
#8123는 metadata key/기능 지도 join을 먼저 착지할 수 있으나 신규 operation을 실행 증거 없이 covered로 표시하지 않는다.

각 구현 조각은 대상 시험·RED/GREEN·뮤턴트 복원·자기 파일 clippy 경고0·필수 CI 선택을 확인하고,
리뷰 전에 source-scan readers와 정적 prepush를 수행한다. 이번 문서는 설계 단계이므로 cargo/실물 시험을
실행하지 않으며 구현 게이트의 통과를 주장하지 않는다.

## 8. 결정·준비물과 남는 한계

설계 문서 커밋을 막는 결정은 없다. 운영 권한·앱 수·채널·공유 계정·사전 실행 비차단은 다시 묻지 않는다.

| 구현/활성화 항목 | 확정 결정과 준비물 |
| --- | --- |
| **원증상 human 경로의 무인화 범위** | **확정:** E1b 앞 E1-ingress를 추가해 무인 회귀는 controlled_ingress로 실행. human Gateway와 동등 표시 금지. human 입력 1회 RED/GREEN은 코디네이터가 agent-desktop으로 Discord 앱을 조작해 수용하며 사용자 입력 요청·정기 수동 작업은 0 |
| 주기·일일 예산 | **확정:** S0/단발 실측 전 off. 이후 배포 이벤트·6/12시간 회전·일일 전체 중 선택. 기존 24턴/60분은 승인값이 아님 |
| strict reaction seam | **권고:** E1a에서 기존 serenity로 검증, 불가능하면 비용/크기를 제시하고 별도 결정. raw seq 없는 callback을 완전 이력으로 승인하지 않음 |
| host·자료·알림 준비 | #8124 지정 시험 호스트/receipt·앱 ID·operator recipient, S0로 스모크 보존본과 추정 7 FAIL 원문은 확보. scenario 중 host 지표/native turn/usage는 여전히 없음. 운영 token·설정값을 레인으로 전달하는 방식은 사용하지 않음 |
| 향후 다음 배포 hold | **확정:** 알림만. 향후 hold가 필요하면 confirmed 데이터 손실·세션/host 회귀에 한정해 별도 결정. 이번 설계의 선행 조건 아님 |

리뷰 집중 항목: **경계 시각**은 submit/A-end 엄격 순서·clock ID·native offset과 deadline 경계 replay;
**동시성**은 실제 parent 쌍 flock·FD 수명·늦은 send 모델;
**옛 형식 복원**은 schema2 원본 유지·schema1 명시·unknown schema/누락을 PASS 금지;
**문서뿐인 guard**는 allowlist/receipt/실제 precondition witness 음성 시험;
**잘못된 0/PASS**는 expected occurrence 집합·필수 단언·실제 contact·null usage·human/gap 칸을 독립 확인한다.
남은 반례는 controlled ingress가 검증하지 않는 실제 human Gateway 전달 계층, strict sequence seam 미입증, E-51 7건의 원인 불명, 새 묶음 비용 n=0이며
각각 §3.1/§3.2/§5.2/§2의 준비·평가 불가 상태로 남긴다.

<!-- doc-path-refs: external i8011/ i8113/ src/client/ src/gateway/ briefing_checks.py pr_cap_prod.py -->
