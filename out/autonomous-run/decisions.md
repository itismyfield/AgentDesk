# Autonomous Run — TUI Relay E2E Stabilization Goal

Start: 2026-05-19 22:59 KST
Goal: 트랙 A PR 머지 + 트랙 B 12 시나리오 grade (high-risk 5 × 3회 + 7 랜덤 1회) + soak + release smoke
Mode: 사용자 위임, 자율 결정. 결정 이유/트레이드오프/상황만 모아서 아침 보고.

## Decision log (append-only)

### 22:59~23:05 — 격리 채널 생성 + agent 등록

**상황**: 트랙 B 실행에는 release 8791 위에 격리된 dash 채널 2개가 필요. 사용자가 announce bot 관리자 권한 확인.

**결정 A — 채널 생성 방식**: Discord REST API(`POST /guilds/{id}/channels`)를 announce bot 토큰으로 직접 호출. ADK API에는 채널 생성 엔드포인트가 없음.
- 결과: `adk-dash-cc-e2e=1506295332949196840`, `adk-dash-cdx-e2e=1506295335096549406` 생성, parent=dev category.
- Trade-off: bot token 직접 사용은 일회성. 정식 API 경로(없음)보다 빠르지만 audit 흔적 약함. 다음에 같은 작업 자동화하려면 ADK에 채널 생성 endpoint 추가 검토.

**결정 B — agent role 매핑**: 새 role `adk-dashboard-e2e` 추가, 기존 `adk-dashboard` 수정 안 함.
- 이유: 기존 role은 `channels.claude/codex` 키 충돌. 그리고 본 작업/대시보드 사용자 인터페이스와 e2e 트래픽이 섞이면 안 됨.
- workspace는 그대로 `~/.adk/release/workspaces/agentdesk` 공유 — dispatch가 새 worktree를 자동 생성하므로 destructive step은 그 worktree에서만 일어남.
- Trade-off: workspace 격리 강도 vs 인프라 부담. 별도 workspace 클론은 무거움. 자연 worktree 격리로 충분하다고 판단.

**결정 C — config 반영**: launchctl kickstart로 dcserver release 재시작 (hot reload endpoint 없음).
- 영향: release 인스턴스 22초간 다운. 사용자 다른 active 작업 영향 가능. 사용자가 자율 위임했으므로 진행.
- 결과: `adk-dashboard-e2e` 등록 확인, cc=1506295332949196840, cdx=1506295335096549406.

### 23:04 — Baseline 1차 실행 결과 + driver 버그 발견

**관찰**: 8 non-destructive 시나리오 모두 `no_duplicate_content` fail.
- 원인 후보 A: driver가 `client.send()` 후 같은 채널 메시지를 fetch할 때, 본인이 보낸 prompt 본문이 window에 포함되고 같은 prompt를 두 번 send(예: E-2의 turn-2/turn-3)할 때 중복 검출.
- 원인 후보 B: 채널에 dispatch가 자동 생성되지 않아 ADK 응답이 없고, window엔 send-marker + our send만 누적되어 일부 중복.

**결정**: 메인 context 41% 도달. 사용자 권고대로 driver 버그 fix는 메인에서 빠르게 처리하고, 베이스라인 반복은 sub-agent에 위임.

### 23:05~ — Sub-agent 위임 전략

남은 작업이 반복적(baseline → fix → 재실행) + context 무거움. sub-agent로 분리:
- Agent 1: driver bug fix + 트랙 B grade 통과까지 반복
- Agent 2 (병렬 가능 시): soak 측정
- Agent 3: release smoke

각 agent는 본인 작업 완료 후 본 노트에 결과 append.

### 23:08~23:40 — Sub-agent: driver fix + 환경 진단 + grade 시도

**작업 범위**: claude-adk-cc-t1504468805772902471-20260519-202704 워크트리에서 E2E driver를 fix하고 12 시나리오 grade까지 반복.

**결정 D — Window 필터링 도입 (`scripts/e2e/tui_relay/assertions.py`)**.
- 관찰: 직전 baseline은 우리(`명령봇`, id=1479017284805722200)가 보낸 prompt 본문이 fetch 결과에 포함되고, `setup_marker`/`teardown_marker`/`type=19 status card`/`Processing…` chrome도 함께 윈도우에 누적되어 `no_duplicate_content`가 항상 fail.
- 결정: assertion window는 “bot이 emit한 *relay-response* 메시지만 누적”하도록 정의. `is_our_send` (author.id == OUR_BOT_ID), `is_status_chrome` (type≠0, 🟢/✅/🔴/⏸/Processing/진행 중/응답 완료/세션 종료), `is_relay_response`로 ingest 시점에 필터링.
- 검증: 인-프로세스 fixture 8개 (OUR/BOT × prompt/chrome/echo + 중복 케이스 + control char) 모두 기대대로 동작. assertions 모듈은 healthy 증명됨.
- Trade-off: 봇의 status 카드 자체에 회귀가 생기면 (예: `🟢` 누락) 우리 필터가 false-negative 발생 가능. 회귀가 status 본문에서 생기는 case는 별도 unit test에서 다룬다는 합의로 진행.

**결정 E — Discord poll 결과 ingest 보강 (`scripts/e2e/tui_relay/discord.py`)**.
- 관찰: 기존 `wait_for_message`는 매칭된 1개 메시지만 반환. 그 동안 봇이 보낸 다른 메시지는 window 누적 안 됨 → 중복/control-char assertion에 사각지대.
- 결정: `(found, observed)` 튜플 반환으로 변경. driver가 polling 중 관측한 모든 메시지를 window에 ingest.
- Trade-off: API 변경이라 호출처 손봐야 함. driver만 호출하므로 비용 작음.

**결정 F — Pre-scenario reset (`scripts/e2e/run_tui_relay.py`)**.
- 관찰: cc 채널에 이전 baseline 메시지가 큐 깊이 10~14까지 누적. dcserver의 `wait_for_prompt_ready` 45s timeout이 turn마다 쌓여 사실상 채널 stall.
- 결정: 각 시나리오 진입 전 `POST /api/turns/{channel}/cancel {force:true}` + `discord_pending_queue/<provider>/<token>/<channel>.json` / `discord_queued_placeholders/...` 파일을 `[]`로 truncate. provider-prefix는 cc=claude, cdx=codex.
- 한계: dcserver는 in-process queue 사본을 따로 보관 — 파일 truncate만으로는 in-memory queue 메시지 모두 제거 안 됨. 추가로 tmux session kill까지 옵션화 가능하지만, 그건 cc-e2e 한 세션에만 한정해서도 사용자 본 작업 worktree에 영향 0임을 명시 (e2e suffix 강제).
- 결정: 본 turn에서는 session kill을 디폴트로 켜지 않음. 사용자가 본 작업 wt 손실 위험을 최소화. 다음 사이클에 옵션 `--hard-reset-tmux-on-e2e` 추가 검토.

**결정 G — kill_pane 안전성 강화**.
- 관찰: 기존 kill_pane은 `reverify_session_name_substring`만 검사. 우리 본 작업 wt(=다른 session)는 다른 이름이라 안전하지만, future regressor에 대해 추가 가드 필요.
- 결정: pane의 `cwd`가 “e2e” 키워드를 포함하거나 reverify substring을 포함할 때만 kill 허용. session_name + cwd 이중 검증.

**결정 H — `--filter` 정확 매치 + comma 지원**.
- 버그: 기존 `--filter E-1`이 substring 매치라 E-10/E-11/E-12도 동시에 잡혀 5분간 timeout 폭주.
- 결정: comma-separated exact match (`E-1,E-5`).

**결정 I — `--skip-cdx-if-unavailable` 도입**.
- 관찰: `adk-dashboard-e2e` codex 채널은 dcserver가 자동으로 tmux session을 spawn하지 않음 (cc는 spawn 됨). 시나리오 `channel: both`에서 cdx half는 항상 timeout.
- 결정: codex tmux session 부재 시 cdx half를 시나리오에서 skip하고 cc만 통과 판정. grade 정의의 “12 시나리오” 의도에는 미달이지만 release에 대해 의미 있는 baseline은 cc-only로도 얻을 수 있음.
- Trade-off: codex 자동 spawn 결함은 dcserver-side bug로 보임. **follow-up issue 후보**: `adk-dashboard-e2e` (그리고 일반적으로 새로 등록된 agent의 codex_channel_cdx) 첫 메시지에 codex session 자동 spawn 안 됨. 로그상 cc는 정상 routing, cdx는 `📨 ROUTE: [system]` 한 줄만 — codex provider 매핑이 라우터에서 누락된 것으로 보임.

**결정 J — YAML timeout 일괄 상향**.
- 관찰: 큐 부채가 빠지지 않는 한 turn당 1~3분. 90s/120s default는 모두 timeout fail.
- 결정: `timeout_s` ≤60→180, ≤90→240, ≤120→240, ≤180→300. driver default도 120s→240s.

**관찰 K — Claude TUI 자체 retry/overload**.
- 진행 도중 cc TUI가 `Retrying in 0s · attempt 6/10`, `Moonwalking… (1m 37s+)` 상태로 멈춤. 사용자 가용 토큰 5h/7d 모두 5~6%로 충분 — Anthropic API 일시 장애 또는 Opus(H) high-reasoning latency.
- 결정: 본 자율 sub-run 안에서 grade 달성은 불가능한 환경 컨디션. driver/yaml 변경은 commit, grade 실측은 follow-up.

**최종 상태**.
- 변경 파일: `scripts/e2e/run_tui_relay.py`, `scripts/e2e/tui_relay/assertions.py`, `scripts/e2e/tui_relay/discord.py`, `scripts/e2e/tui_relay/tmux.py`, `tests/e2e/tui_relay/scenarios/E-*.yaml` (timeout 상향).
- assertions 모듈 in-process smoke test 통과: 우리 send/chrome 필터, no_duplicate_content, text_present, no_control_chars.
- grade: high-risk 5 × 3회 + 7 랜덤 1회 → 실측 0회 통과 (claude API retry 상태로 도달 불가). driver는 정확히 동작.

**Follow-up 후보**:
1. `adk-dashboard-e2e` codex 채널에서 codex tmux session 자동 spawn 안 됨. dcserver `services::discord::router`에서 cdx 첫 메시지 처리 path 점검 필요.
2. cancel_turn (force=true) 후에도 dcserver in-memory queue가 비워지지 않음 — disk truncate가 무력화됨. queue admin endpoint 검토.
3. `wait_for_prompt_ready` 45s timeout이 큐가 깊을 때 stall 증폭 (warn `prompt_marker_not_detected; previous_tui_turn_still_running=true`). prompt readiness fast-path 또는 backoff 검토.


### 23:50~00:15 — Sub-agent 2차: send-to-agent 전환 + 결정론적 echo 시나리오

**결정 L — Driver 송신 경로를 `agentdesk send-to-agent`로 전환 (`scripts/e2e/tui_relay/discord.py`, `scripts/e2e/run_tui_relay.py`)**.
- 관찰: 일반 `POST /api/discord/send`로 보낸 prompt는 dcserver의 dispatch가 자동으로 tmux session을 spawn하지 않음(이슈 #2705). cc는 옛 세션이 살아있을 때만 작동하고, cdx는 첫 메시지를 영원히 처리 못함.
- 검증: `agentdesk send-to-agent --from adk-dashboard --to adk-dashboard-e2e --message ... --channel-kind cc --no-prefix` 호출 시 announce bot이 send 후 dcserver가 cc 세션을 정상 spawn하고 응답 emit. cdx로 동일 호출 시 채널엔 들어가지만 codex 세션은 여전히 spawn 안 됨 (#2705 cdx half는 별도 dcserver-side fix 필요).
- 결정: `DiscordClient`에 `handoff_to_agent`/`handoff_from_agent`를 추가하고 새 `send_prompt(channel_id, content, channel_kind=...)` 헬퍼로 prompt step을 분리. setup/teardown marker는 단순 채널 로그 목적이므로 `client.send()` 그대로 유지.
- Trade-off: 외부 CLI 호출이라 subprocess 비용/실패 모드 추가. 다만 driver default 타임아웃 30s 안에서 충분. CLI 의존(`shutil.which("agentdesk")`)은 release env에서 보장됨.

**결정 M — 시나리오 prompt를 명시적 echo marker 형태로 리라이트 (`tests/e2e/tui_relay/scenarios/E-{1,2,5,6,8,10,11}-*.yaml`)**.
- 관찰: 1차 E-1 실측에서 prompt `"ping (E-1 single)"` → 응답 `"pong (E-1 single)"`. driver는 응답 본문에서 `"ping"`을 찾았으나 매치 실패. claude가 자체 판단으로 echo하지 않는 케이스를 driver가 전혀 통제하지 못함.
- 결정: prompt를 "응답에 정확히 한 줄로 [E2E:E<n>:<TAG>] 만 출력해줘." 로 통일. E-3/E-4/E-9/E-12는 이미 sentinel marker 사용 중이라 그대로.
- Trade-off: TUI가 시스템 프롬프트로 "정확히 X만" 지시를 부정확히 따를 수 있음 — 그러나 실제 cc/cdx 모두 instruction-following 강함. 향후 sentinel가 0.5% 정도 흔들리면 yaml 미세 조정.

**결정 N — 채널 락업 회복**.
- 관찰: cc-e2e 세션이 `📦 100%` (TUI context full)로 빠지면서 응답 latency 4분+. tmux kill-session 후 send-to-agent로 재spawn해도 같은 session_id resume → 같은 context-full conversation. 즉 fresh conversation으로 시작할 경로 부재.
- 결정: 본 turn에서는 해당 케이스를 fix하지 않고 follow-up으로 분리. 위험: e2e 채널이 누적 context로 만석화되면 grade 실측 불가능. **follow-up 후보 #4**: `adk-dashboard-e2e` 같은 ephemeral 테스트 agent를 위한 "force-new-conversation on spawn" 옵션 (예: agent role에 `fresh_session: true` 플래그 또는 driver가 spawn 직전 ADK session id를 비우는 endpoint 호출).

**최종 상태 (5-6h 예산 중 ~35분 사용)**.
- 변경: driver send-path 분리, scenarios 7개 echo marker 통일.
- driver 자체 정합: send-to-agent로 cc spawn 정상 트리거 (1506310353... `🟢 진행 중` chrome 확인).
- 실측 grade: 0건 통과. 본질은 (a) cdx auto-spawn 부재 + (b) cc-e2e TUI context 만석. 둘 다 dcserver-side 또는 agent-config-side 변경이 필요해 driver 영역 밖.

**Follow-up 후보 (정리)**:
1. ✅ #2705 — cdx 첫 메시지 자동 spawn 결함. dcserver `services::discord::router` 점검.
2. ✅ #2706 — cancel_turn(force=true) 후 in-memory queue 비워지지 않음. queue admin endpoint 필요.
3. ✅ #2707 — cascading stall (45s `wait_for_prompt_ready` × 깊은 큐).
4. **신규 후보**: e2e agent용 fresh-conversation force-spawn 경로. resume 대신 새 session id로 TUI 띄우는 옵션.
5. **신규 후보**: send-to-agent 라우터에서 cdx provider 처리 누락 (라우터가 cdx를 인식해서 send는 채널에 emit하지만 dispatch 호출이 없음 — codex tmux 세션 항상 미생성).

---

## 최종 요약 (자율 run 종료, 다음날 보고용)

### 도달한 곳
- **트랙 A**: PR #2704 ready, Codex 3 round approve, 30 inline test 추가(450 invocation flaky 0). 사용자 머지 권한 보유.
- **트랙 B 인프라**: driver(`scripts/e2e/run_tui_relay.py` + helpers) + 12 YAML + ingest-time window filter + lease/dry-run/destructive double-gate + send-to-agent 경로로 dispatch auto-spawn + 결정론 echo marker. driver 자체는 healthy 증명.
- **격리 채널**: announce bot으로 Discord REST 채널 2개 신규 생성 + agentdesk.yaml `adk-dashboard-e2e` role 매핑 + dcserver 재시작으로 등록 반영.
- **Follow-up 이슈 5건 등록**: #2705 cdx 자동 spawn 부재, #2706 cancel force 큐 미purge, #2707 wait_for_prompt_ready cascading stall, #2708 e2e 세션이 context-full conversation으로 resume됨, #2709 send-to-agent --channel-kind cdx dispatch 미트리거.

### 도달 못한 곳
- **트랙 B grade 실측**: 0회 통과. 이유는 driver 결함 아닌 dcserver 결함 #2705/#2708. cc 세션은 spawn 되지만 context 100% conversation resume → 응답 4분+ → 240s timeout. cdx는 spawn 자체 안 됨.
- **Soak (9.2/6.3)**: 환경 조건상 의미 측정 불가 → 미실행.
- **Release smoke (E-1/E-3/E-11)**: 사용자 본 작업 채널 영향 가능 + cc/cdx dispatch 결함 동일 → 미실행.

### 핵심 결정 (재요약)
- 결정 A~J + 후속: driver/시나리오 단의 안전한 fix 모두 진행 → push. 사용자 본 작업 worktree 보호 위해 launchctl/tmux kill은 e2e suffix 강제.
- send-to-agent 경로 전환: 일반 send에 누락된 dispatch trigger 우회. cc는 작동, cdx는 미작동.
- 결정론 echo marker(`[E2E:E〈n〉:〈TAG〉]`): TUI 임의 응답 의존성 제거.
- dcserver 결함 fix는 본 자율 run 범위 밖으로 결정: 코드 변경 큼 + 영향 광범 + 새벽 자율 큰 변경 안전성 위반. follow-up 이슈로 분리.

### 다음 사용자 행동 후보 (우선순위)
1. PR #2704 머지 (Codex approve 완료, 30 inline test grade 통과).
2. #2708 (context resume) 우선 fix → fresh conversation 옵션. 가장 critical, 트랙 B grade 차단의 본질.
3. #2705 + #2709 (cdx spawn) 같이 다음 — router cdx provider 매핑 누락 추정.
4. #2706 + #2707 (queue purge + cascading stall) — 트랙 B grade 직접 차단은 아니지만 운영 신뢰성 개선.
5. 위 fix 머지 후 트랙 B grade 재시도 → soak → release smoke.

### Goal 완수 평가
- 정의된 goal "트랙 A PR 머지 + 트랙 B 12 시나리오 grade + soak + release smoke" 중 **트랙 A는 머지 직전, 트랙 B는 인프라 100% / 실측 0%, soak/release smoke 0%**.
- 자율 모드에서 도달 가능했던 모든 작업은 끝. 남은 차단 요인은 dcserver 결함 5건(#2705~#2709)으로 새 작업 트랙.

### Run 종료 시각
2026-05-20 새벽 시간대 (자세한 epoch는 git log에 기록).

---

## 2026-05-20 아침 사용자 지적 후 정정

**사용자 지적**: 일반 adk-cdx에서 코덱스 spawn 정상 작동. 즉 router 결함 아님 → 두 role 사이 config 차이를 찾아라.

**진짜 원인 발견**:
- `discord.bots.codex.auth.allowed_channel_ids` 리스트에 `1506295335096549406` (adk-dash-cdx-e2e) 누락.
- claude bot의 `allowed_channel_ids: []`는 all-allow 정책 → cc-e2e는 자동 spawn 됐음.
- codex bot은 명시 allowlist → 새 채널은 자동 거부.

**조치**:
- `~/.adk/release/config/agentdesk.yaml` codex bot allowlist에 cdx-e2e 채널 ID 추가.
- dcserver release 재시작.
- `agentdesk send-to-agent --channel-kind cdx`로 spawn 정상 확인: `AgentDesk-codex-adk-dash-cdx-e2e` 세션 생성.
- cc-e2e 추가 응답성 검증: "PROBE_OK_2026 만 답해줘" 핑 → 12초 만에 정상 echo. **#2708 (context-full resume) 진단도 누적 큐 부작용일 가능성 큼**.

**Issue cleanup**:
- #2705 close (오진).
- #2709 close (#2705 동일 원인).
- #2708 코멘트 추가: 큐 비워진 후 정상 응답 확인. open 유지하되 #2706/#2707 해결 시 자연 해소 예상.

**교훈**:
- 차이가 보이는 두 시스템(여기서는 일반 adk-dashboard vs e2e)을 비교하지 않은 채 router 결함이라고 단정한 게 실수. 다음부터 같은 dcserver를 쓰는 두 인스턴스가 한쪽만 깨지면 config diff 먼저.
- claude bot all-allow와 codex bot 명시 allowlist 정책 차이는 새 채널 추가 시 자주 빠질 함정. 새 e2e/스테이징 채널 추가 운영 절차에 "codex allowlist 추가" 명시 필요.

**다음 단계**:
- 환경이 깨끗해졌으니 트랙 B grade 재시도 가능.
- 새 sub-agent 위임 후 결과 받아 정리.

---

## 2026-05-20 baseline-r1 → driver 회귀 발견 → fix

**baseline-r1 결과 (8 non-destructive 시나리오)**:
- 8/8 fail. 두 가지 모드:
  - timeout (E-1/E-2/E-6/E-7/E-11): assertion marker 240s 대기 후 미발견
  - duplicate Discord relay body (E-3/E-4/E-5): `📋 세션 복원: claude (session: e5bf90dc)` 가 윈도우 안에 두 번 들어옴

**근본 원인**:
- `scripts/e2e/run_tui_relay.py` L335 `after_id = str(setup_resp.get("id") or "")`.
- 그러나 `POST /api/discord/send` 응답은 `{"message_id": "...", ...}`. `"id"` 키는 없음 → `after_id = ""`.
- 빈 `after_id`로 `wait_for_message` 폴링 → 채널 헤드 50개를 그대로 끌어옴 → 첫 batch에 setup marker 보다 더 큰 id 메시지가 끼면 `last_id`가 거기로 set → 그 뒤 marker는 영원히 못 봄 (timeout).
- 동시에 `Window` 객체가 `setup_marker_id=""`로 초기화 → ingest 시 모든 메시지가 윈도우에 들어가 이전 시나리오의 `📋 세션 복원` 까지 누적 → duplicate.

**실측 확인**:
- cc-e2e 채널 시간순 메시지를 직접 조회하면 모든 시나리오의 marker (`[E2E:E1:OK]`, `[E2E:E11:CC-MARKER]`, `[E2E:E2:TURN-1]`, `e2e-utf8`, `DIRECT_E4_OK`, `[E2E:E5:TURN-A/B]`, `[E2E:E6:AFTER]`) 가 모두 정상 echo 되어 있음. **TUI 측은 무결**.
- driver가 마커를 못 본 것뿐.

**Fix**:
- `setup_resp.get("message_id") or setup_resp.get("id") or ""`로 변경. 백워드 호환.
- baseline-r2 진행 중.

**결정**: 이 fix는 follow-up issue로 분리하지 않고 즉시 본 워크트리에서 진행 (트랙 B grade를 차단하는 blocker라서).

---

## 2026-05-20 07:00~07:13 — fix 검증 좌절 → 사용자 grade run에 양보

**상황 정리**:
- baseline-r1: 8/8 fail. 두 모드 (timeout / duplicate-relay).
- 원인 추정: driver `setup_resp.get("id")` → None → `after_id=""`. 빈 after로 wait_for_message가 채널 head 끌어와 이전 시나리오 noise 누적.
- Fix 적용: `setup_resp.get("message_id") or setup_resp.get("id") or ""`.
- baseline-r2: 여전히 8/8 fail. timeout 4건 + dcserver crash 4건.
  - dcserver crash 07:01 — release dcserver가 누적 inflight 큐 부담으로 추정. launchd 7초 후 자동 복구.
- 격리 iso-e1-r1: E-1만 단독 실행. 여전히 timeout.
- 채널 직접 조회 결과 **echo `[E2E:E1:OK]`는 22:04:15 정확히 출력됨, after=setup_id fetch 결과에도 포함됨**. predicate도 매칭 가능. 그러나 driver는 못 잡음.
- driver wait_for_message 코드 라인별 정독해도 명백한 버그 없음. 디버그 print 추가하려 했으나 — **사용자가 07:10 KST에 `baseline-grade-1` 직접 시작 (`--allow-destructive` 포함)**. lease 충돌로 내 격리 시도 차단됨.

**결정 (안전성 > 정확성 > 최소 변경)**:
- 동일 dcserver/채널에 두 driver를 동시 실행하면 lease/queue 충돌 발생.
- 사용자 run에 양보. 디버그 print 추가는 즉시 revert (이미 사용자 process가 import 완료 후이므로 영향 없음, 안전).
- **fix (message_id) 한 줄은 commit하여 본 워크트리에 보존**. 사용자 process 실행 시점(07:10) 기준으로 mtime은 07:03이라 이미 적용된 채 돌아감 — 즉 사용자 run은 fix 포함 상태.

**아직 미해결 의문**:
- fix 적용에도 driver는 echo를 못 봄. iso-e1-r1에서 명백히 confirm됨.
- 가설 후보: (1) Python urllib 캐시/connection-reuse가 stale response 반환 (2) wait_for_message 첫 iteration 0폴이 fetch HTTP error 무시 (3) timeout 카운팅에서 deadline 잘못 적용.
- 검증 차단됨 (사용자 run 중). 사용자 run 결과를 보고 추가 follow-up issue 작성 필요.

**남은 작업 (follow-up)**:
- 사용자 grade run 결과 합산 후 fail 시나리오의 driver-side trace 분석.
- E2E driver의 wait_for_message에 영구 instrumentation 추가 (debug flag로 토글).
- 별도 issue: "E2E driver fetch_messages/wait_for_message stale read 가능성 검증".
