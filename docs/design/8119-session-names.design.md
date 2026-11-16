# #8119 세션 이름 단순화 — 설계 r3(재계획, main `406da315a7` 기준)

작성 2026-11-16 KST. 조사 기준 `406da315a7344c1d0116a5bb1d1bbe9f75c2f20f`(1116b: #7583 1단계·#8177·#8185 착지 뒤). 문서·조사만 했다. 코드·시험·운영 설정·세션은 바꾸지 않았고 cargo도 돌리지 않았다.

이 문서는 기존 설계 [`docs/session-naming.design.md`](../session-naming.design.md)(r2, 리뷰 CLEAN, S1 착지 때 함께 들어옴)를 **대체하지 않는다**. r2의 이름 규칙·권고안 A(자연 재생성 때 primary 승격)·파일 경로 공식은 그대로 쓴다. r3는 세 가지를 새로 정한다.

1. 지금 main에서 이름을 읽는 지점의 전수표를 다시 낸다(부록 A).
2. 실물 겹침 검사와 이중 판독 기간 모델 검사를 다시 돌렸다(§3, 부록 B).
3. S1 착지와 S2 보류(main 미착지, 충돌 3건) 상태에서 남은 조각의 순서와, 진행 중 레인과 겹치는 파일을 다시 짠다(§6).

## 0. 결론 요약

| 항목 | 결과 |
|---|---|
| 이름 규칙 | 사용자 결정대로 채널 세션은 `<채널 이름>`, 스레드는 `<채널 이름>-<스레드ID>`다. 채널이 아닌 힌트(DM·scheduled·routine·recovery)만 provider 구분자를 남긴다(r2 §이름 규칙). |
| 판독 지점(Rust, 시험 제외) | 접두어 판정 18행, provider·채널 파싱 59행(42파일), `-t` 꼬리 35행(이 중 tmux 이름·세션 키에서 읽는 18행만 영향), 기대 이름 재구성 56행(41파일). 세션 키에서 tmux 꼬리를 꺼내는 95행은 r2 권고안 A의 불변식("primary 키 꼬리 = 실제 이름")으로 자동 해결된다. |
| 판독 지점(Rust 밖) | 저장소 7행(policies 5·dashboard 1·e2e 1)과 migration 0101 함수 1개. |
| 외부 도구·문서 | 34파일: 저장소 비-Rust 9, 운영 문서 5, 스킬 정본 19, adk-config 1. `AgentDesk-*` 보호 문구는 13곳이다. 이 중 **r2에 없던 것**: `imessage-decision-alert/scripts/notify.py`(바인딩 문맥 폴백이 없어 완전히 깨짐), 이슈 파이프라인 `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/launch-headless-leg.sh:119` cwd 보호, 배포 스크립트의 `AgentDesk-dcserver-release-manual`(이름을 바꾸면 보호 glob에 걸리는 세션이 이것 하나만 남음). |
| 겹침(실측) | 설정된 48채널은 provider를 떼도 겹침 0, 44바이트 초과 0이다. 지금 이 맥의 live 세션 2개도 떼고 나서 겹침 0이다. 발견한 겹침 부류는 4가지다(§3). |
| 이중 판독 모델 | 정상 모델은 불변식 4종(중복 세션 0·남의 세션 채택 0·남의 세션 조작 0·살아 있는 세션 끊김 0)이 모두 GREEN이다. 뮤턴트 7종은 각자 노린 불변식 하나에서만 RED다(부록 B, TLC 1.7.4, 32회 실행). |
| 남은 조각 | 13개(S1 착지 완료). S2'(S2 재착지)부터 S10(옛 판독 제거)까지. 스킬·운영 규칙 갱신은 코디네이터 작업으로 S8c(작성 전환) 전에 한다. |

## 1. 지금 상태(406da315a7)

- **S1 착지**(#8139, `d2d1b81cf5`). tmux 세션은 모두 `new-session -e AGENTDESK_IDENTITY=<json>`으로 표식을 달고 생긴다(`src/services/platform/tmux.rs:172`, 생성 경로 9곳). 마지막 관문 `require_managed`(`src/services/platform/tmux/session_identity.rs:7`)는 이름이 `AgentDesk-`로 시작하면 바로 통과시키고(`:17`), 아니면 표식의 `owner_scope`가 현재 런타임과 같을 때만 send·paste·kill을 허용한다.
- **표식 내용의 빈칸**: `binding_identity`(`src/services/session_identity_metadata.rs:40`)는 provider·channel_id·spawn_nonce·name_kind만 채운다. `parent_channel_id`·`thread_id`·`token_namespace`·`logical_key`는 비어 있다. 스레드를 판독하는 쪽을 옮기려면 S6에서 이 칸을 채워야 한다.
- **S2 보류**. `refs/review/i8119-s2-3`·`-s2-fix-1`·`-s2-win-1`은 1113c 묶음에서 빠졌고(PR #8149 본문) main에 없다. `rv/s2-win-1`은 base `33d1f4b5a9` 대비 prod 30파일/순증 +882다(`scripts/pr_cap_prod.py`, 파일 상한까지 남은 여유 0). 지금 main과 병합하면 충돌이 3건 난다: `src/services/cluster/session_matcher.rs`, `src/services/tui_prompt_dedupe/binding_context.rs`, `docs/statecharts/README.md`(`git merge-tree` 실측).
- **S2 이후 main에 새로 생긴 이름 의존 지점**: `src/services/discord/tmux/relay_decisions.rs:31`·`:35`(provider 파싱), `src/services/tmux_turn_liveness/claude_idle.rs:10`, `src/services/discord/commands/tui_passthrough/herdr_settings.rs:37`(빌더로 만든 이름을 Herdr 기록의 logical_key와 `:58`에서 비교), `src/services/discord/turn_presence/activity/unbound/hold_leaf.rs:72`.
- **이미 쓸 수 있는 저장 정보**:
  - 세션 행(`sessions.provider`·`channel_id`·`thread_channel_id`·`identity_kind`)과 locator 별칭 `session_key_aliases`(migration 0101).
  - 채널로 행을 찾는 함수 `derive_channel_session_name`(`src/services/discord/host_key_derivation.rs:105`).
  - Herdr 기록 `hosted_execution.owner.logical_key`.
  - 실행 문맥 `AGENTDESK_BINDING_CONTEXT`.

## 2. 대체 근거와 판독 규칙

이름에서 의미를 읽던 지점은 아래 근거를 위에서부터 차례로 쓴다. 근거가 없으면 Unknown으로 남긴다. Unknown을 "관리 세션 아님"이나 "일반 채널"로 읽지 않는다.

| 순위 | 근거 | 주는 것 |
|---|---|---|
| E1 | `sessions` 행(primary 또는 alias로 찾음) | provider·channel_id·thread_channel_id·identity_kind·현재 물리 이름(primary 꼬리) |
| E2 | Herdr `hosted_execution` 기록 | owner(provider·token·channel·logical_key)·nonce·pane |
| E3 | tmux 세션 환경 `AGENTDESK_IDENTITY` | owner_scope(관리 여부)·provider·channel_id·(S6 뒤) parent/thread·name_kind·display_name |
| E4 | 실행 문맥 `AGENTDESK_BINDING_CONTEXT` | 세션 안 프로세스(스킬)의 provider·channel_id·(S6 뒤) parent/thread |
| E5 | 메모리 세션 맵 `sessions[channel]` | channel_name(스레드는 합성 이름 `<부모>-t<ID>`), 현재 tmux 이름 |
| L | 옛 이름 해독기(`parse_provider_and_channel_from_tmux_name` 등) | `AgentDesk-`로 시작하는 이름만, E1~E4가 없을 때만 |

판독 규칙은 다섯 가지다.

- **관리 여부는 E3만 정한다.** 옛 접두어(L)는 이중 판독 기간에 "보호(죽이지 않음)" 판정에만 쓴다. 채택·입력·정리 권한의 근거로 새로 쓰지 않는다. 접두어만 보고 통과시키는 `require_managed`의 지름길(`:17`)은 main 범위를 유지하는 호환 예외이고 S10에서 지운다.
- **L을 쓸 때마다 계수한다.** 판독 지점별 카운터 `session_name.legacy_decode{site}`를 S3에서 한 번에 넣는다. §4의 제거 기준이 이 값을 쓴다.
- **스레드의 합성 채널 이름은 바꾸지 않는다.** 메모리 맵·훅 본문의 `channel_name`은 `<부모>-t<ID>`(`src/services/discord/session_runtime/channel_routing.rs:3`)로 남긴다. 이것은 tmux 이름이 아니다. 이 합성 이름을 읽는 `-t` 판독 17행은 영향이 없다(부록 A-THR의 "유지"). tmux 이름만 `<부모>-<ID>`가 된다. 빌더는 끝 숫자를 거꾸로 해석하지 않고, 명시된 `channel_id`가 스레드라는 확인(`is_synthetic_thread_channel_name(name, channel_id)` 또는 BindingContext `thread_id`)이 있을 때만 스레드 형식을 만든다. r2의 S6 범위가 여기서 줄어든다.
- **기대 이름 재구성(56행)은 S2 `session_locator`로 바꾼다.** 채널 이름에서 이름을 다시 만들어 비교하던 곳은 "이 채널의 저장된 현재 이름"을 묻는다(E1 → E2 → 이중 이름 생존 확인).
- **Herdr는 기록이 정본이다.** 새 launch는 새 이름을 logical_key로 기록한다. workspace label은 r2 결정 (a)대로 `logical#nonce`를 유지한다. 비교하는 쪽(`herdr_settings.rs:58` 등)은 빌더 결과가 아니라 저장된 logical_key나 locator 결과와 비교한다.

### 2.1 생성 관문의 이중 이름 확인 (r3 추가)

채널 종류의 tmux·Herdr 생성은 모두 S2 locator의 admission 한 곳을 지나게 하고, 그 자리에서 아래 순서로 확인한다.

1. 이 `(provider, token, channel)`의 E1 행 primary와 E2 기록.
2. 옛 이름 `=AgentDesk-<provider>-<옛 label>`과 새 이름 `=<새 label>`의 tmux 생존(정확 대상 `=name`), 그리고 각 세션의 E3 표식. 옛 이름 쪽은 표식이 없으면 접두어와 E1을 함께 확인한다.
3. 결과에 따라 갈린다.
   - **자기 세션이 하나라도 살아 있으면** 생성하지 않고 그 세션을 쓴다(옛 이름 우선).
   - **조회가 실패하면** 생성을 보류한다(retryable).
   - **새 이름을 남(표식 없음, 다른 owner_scope)이 쓰고 있으면** r2의 대체 후보 순서로 간다.

이렇게 하면 아직 locator로 옮기지 않은 재구성 호출부가 남아 있어도 같은 채널에 옛·새 세션이 둘 다 생기지 않는다. 부록 B의 `mixed_builder`·`generator_first`·`probe_absent` 뮤턴트가 이 확인을 하나씩 빼면 RED가 되는 경우다.

## 3. 겹침 검사(실물)

원시 기록은 `~/.adk/release/lane-runs/hl-20260925/i8119/logs/d1r3-*.log`에 있다. 기본 tmux 소켓은 `list-sessions`로 읽기만 했다. 생성 실험은 격리 소켓(`tmux -L`, `-f /dev/null`)과 `mktemp -d` 임시 디렉터리에서 했고 끝나고 지웠다.

| 검사 | 결과 | 처분 |
|---|---|---|
| 설정된 채널 48개(`agentdesk.yaml` agents.channels)에서 provider를 뗀 이름 | 겹침 0. 44바이트 초과 0. sanitize로 바뀌는 이름 0. 한글 채널 20개(`원창-cc`, `스마트홈-cdx` 등). provider 단어로 시작하는 채널·숫자로 끝나는 채널·`AgentDesk`로 시작하는 채널 0 | 규칙 그대로 |
| 이 맥 live tmux(14:21 KST, tmux 3.8) | `AgentDesk-claude-adk-cc-t1570308839910412348`, `AgentDesk-claude-스마트홈-cc` 2개. 떼고 나서 겹침 0. 새 이름과 같은 사용자 세션 0. 모두 NFC | mac-mini 측 실측은 S8c 활성화 시험에서 |
| **겹침 부류 1: DM** | `dm-<사용자ID>` 힌트(`src/services/discord/health/headless_turn.rs:176`, `src/services/routines/agent_executor/launch.rs:157`)는 Claude·Codex 봇이 같은 사용자 DM을 열면 provider를 뗀 이름이 같아진다 | `dm-<provider>-<DM채널ID>`(r2) |
| **겹침 부류 2: scheduled·routine** | `scheduled:<정의>`와 `routine <이름> - <agent>`(`src/services/routines/agent_executor.rs:1198`)는 provider 없이 같은 정의·agent를 provider별로 돌리면 겹친다 | provider 구분자 유지(r2) |
| **겹침 부류 3: 한글 NFC/NFD** | 격리 tmux: NFC `스마트홈-cc`(15B)와 NFD(30B)가 **서로 다른 세션 두 개로 공존한다**(`has-session -t =<NFD>` 실패, 둘 다 생성됨). APFS: NFC 이름으로 만든 파일을 NFD 이름으로 열면 **같은 파일이다**(EEXIST). 그래서 옛 경로 공식(`agentdesk-<hash>-<host>-<이름>`, `src/services/tmux_common.rs:1113`)에 이름이 그대로 들어가면 두 세션이 한 파일을 나눠 쓸 수 있다 | 빌더는 NFC로 정규화한다. 새 이름의 경로는 r2 S7a 공식(원 바이트 SHA256 basename)으로 만든다. 옛 이름은 소급하지 않는다(Discord가 보내는 이름은 NFC다) |
| **겹침 부류 4: 사용자 세션과 같은 이름** | 격리 tmux: 표식 없는 사용자 세션 `adk-cc`는 `show-environment … AGENTDESK_IDENTITY`가 실패한다(rc 1). 같은 이름으로 다시 만들면 `duplicate session`으로 거부된다 | 관리 판정은 E3로 한다. 생성이 거부되면 대체 후보로 간다(r2). 남의 세션은 건드리지 않는다 |
| 44바이트 자르기 | 옛 규칙은 한글 14자에서 잘린다(`가`×14+`-a`/`-b`는 각각 44B로 구별되지만, `가`×20+`-cc`/`-cdx`는 같은 이름으로 잘린다). 지금은 provider가 달라서 가려졌다 | 새 이름은 자르지 않는다(r2 규칙 2, 최대 521B; 격리 tmux 생성 성공은 r2 부록 D) |
| 같은 이름 채널이 다른 길드·카테고리에 있음 | channel_id가 다르면 다른 identity. 이름 선점은 부류 4와 같은 경로 | `<agent>-<channel_id>` 대체(r2) |
| 접두 일치 대상 지정 | tmux `-t adk-cc`는 `adk-cc`가 없으면 `adk-cc-1570…`이나 사용자의 `adk-cc-notes`에 접두 일치한다(옛 이름도 같은 성질). Rust 쪽은 `=name`·`exact_target`을 쓴다 | 스킬 `watch-agent-turn`의 `capture-pane -t "AgentDesk-…"`·`kill-session -t`를 `=` 정확 대상으로 바꾼다(§5) |
| Herdr pane·workspace 이름 | label = `logical_key` 또는 `logical_key#<32hex>`(`src/services/herdr_launch/unplaced.rs:101`). 한글은 tmux→Herdr 넘김의 문자 검사(`src/services/claude/session_handoff/pinned_tmux.rs:251`, `is_alphanumeric`)를 통과한다 | 기록된 logical_key로 판정한다. 새 label이 옛 label을 접두로 포함하는 경우는 없다(`#` 구분) |
| 셸 인용 | Rust는 argv로 전달해 셸을 거치지 않는다. 스크립트는 `"$s"` 인용과 `=` 대상을 계약으로 둔다. 새 이름의 문자 집합(L/N·`-`·`_`)에는 셸 메타문자가 없다 | 문제 없음 |

겹침 발견 수는 **4부류**다. 부류 1·2는 이름 규칙(provider 유지)으로, 부류 3은 NFC 정규화와 해시 경로로, 부류 4는 E3 판정과 대체 후보로 막는다.

## 4. 이전 방식과 이중 판독

### 4.1 원칙(r2 유지)

- 살아 있는 옛 이름 세션의 이름·키·경로·inflight·nonce는 그대로 둔다. 일괄 rename·DB backfill·live kill은 하지 않는다.
- 옛 세션이 끝나 자연 재생성될 때만 새 이름을 쓴다. 이때 등록 transaction에서 primary를 새 키로 올리고 옛 키는 별칭으로 남긴다(권고안 A, S2 구현).
- 작성 전환(S8c) 전에 모든 노드(mac-mini·mac-book·Windows)에 판독 조각이 깔려 있어야 한다. 클러스터 탐색(`src/services/cluster/session_discovery.rs:391`)이 다른 노드의 이름을 파싱하므로, 판독 조각이 없는 노드는 새 이름 세션을 "주인 없음"으로 본다.
- 롤백 하한은 S8c 착지 기록에 남기는 "판독 조각을 모두 포함한 SHA"다. 그보다 옛 바이너리로 내리면 새 이름 세션을 보지 못하고 옛 이름으로 다시 만들어 중복이 생긴다.

### 4.2 같은 세션을 쓰는 인접 경로

| 경로 | 이름 의존 지점 | 처분 |
|---|---|---|
| 재시작·부팅 복원 | `src/services/discord/watchers/lifecycle/restore.rs:45`·`:79`·`:128`, `src/services/discord/recovery_engine/restore_inflight.rs:679`·`:722`·`:949`·`:1833`, `src/services/discord/session_runtime/restore_cwd.rs:162` | S2'(primary 승격·locator)와 S4. 복원은 E1 primary로 하므로 옛 세션은 옛 이름으로 다시 잡힌다(모델 Restart 전이) |
| Herdr 재부팅 재기동(#8185) | `src/services/session_host/herdr/autostart/candidate.rs`는 이름을 읽지 않는다 | 영향 없음 |
| incarnation·nonce 표식 | spawn nonce 표식 경로가 `session_temp_path(이름)`를 쓴다(`src/services/tmux_common.rs:1134`) | 옛 이름은 같은 바이트 공식을 쓴다. 새 이름은 S7a 해시 공식. 두 판독자(표식·source authority)는 같은 함수를 쓴다(r2) |
| Cancel | `src/services/queue/cancel_effects.rs`(키→tmux 꼬리), `src/services/provider/cancel_token_cleanup/authority.rs:100`(provider 파싱) | 꼬리는 A 불변식으로 해결. provider 파싱은 S3 |
| Terminate·force kill | `src/services/dispatched_sessions/force_kill.rs:87`, `src/services/dispatched_sessions/tmux_cleanup.rs:40`·`:526`, `src/services/discord/tmux_kill_policy.rs:132` | S3. 옛 별칭 키는 별칭 resolver로 현재 primary를 먼저 찾는다 |
| clear·reset | `src/services/discord/commands/control/managed_reset.rs:27`, `src/services/discord/commands/control/provider_reset.rs:34`, `src/services/discord/commands/control.rs:205`·`:537` | S8b1(locator 조회) |
| stop | 대시보드·정책 중지는 세션 키로 하고(`policies/timeouts/idle-kill.js`), Rust 정지는 표식 관문을 지난다 | S9(정책 판독). 시험은 "중지 명령 전달"이 아니라 "새 이름 물리 세션이 실제로 사라짐"을 본다 |
| tmux 재시작 handoff | `src/services/discord/tmux_restart_handoff.rs:206`(재구성)·`:497`(파싱) | S4·S8b2 |
| 복구 handoff | `src/services/agent_recovery/handoff.rs:39`(`build_tmux_session_name(&binding.channel_id)`) | S8b2 |
| Herdr 넘김(#8177)·소유자 handoff | `pinned_tmux.rs`는 `=logical` 정확 대상과 기록의 nonce를 쓴다. `src/services/discord/queue_io/input_owner/claude_handoff.rs:441`은 기록 owner를 그대로 쓴다 | 영향 없음. `herdr_settings.rs:37`/`:58` 비교만 S8b1 |
| 입력 출처 분류 | `src/domain/human_input.rs`, `queue_io`, `owner_input`, `tui_o`, `claude_tui/host_input`, `claude/herdr_turn`, `claude_tui/readiness`, `idle_detector.rs`에서 이름 파싱 0건(grep 실측). 유일한 예외는 `src/services/claude_tui/tui_relay.rs:619`(provider 파싱, S3) | 이 변경과 무관 |

### 4.3 이중 판독을 지우는 시점(관측 기준)

S10(옛 판독 제거)은 아래 다섯 가지가 **모든 노드에서 7일 연속** 성립할 때 착수한다. 코디네이터는 S8c 착지 기록의 판정 명령으로 확인한다.

1. tmux 기본 소켓에 `AgentDesk-<provider>-` 채널·스레드 세션이 0개다(도구 세션 `AgentDesk-dcserver-release-manual`은 제외하거나 먼저 이름을 바꾼다).
2. `sessions` 행 중 primary 키 꼬리가 `AgentDesk-`로 시작하고 최근 7일 안에 heartbeat가 있는 행이 0개다. 옛 이름을 가리키는 inflight 파일이 0개다. `hosted_execution.owner.logical_key`가 옛 형식이고 상태가 retired가 아닌 기록이 0개다.
3. `session_name.legacy_decode` 카운터 합계가 0이다.
4. 표식 없는 live tmux 세션(`require_managed` 지름길로만 통과하는 세션)이 0개다.
5. §5의 외부 도구 preflight 스캔에서 `AgentDesk-` 의존이 0건이다(정본 스킬·adk-config 포함).

S10은 옛 해독기를 옛 행 판독용 한 곳으로만 남기거나 지우고, `require_managed`의 접두어 지름길과 migration 0101의 scheduled 접두어 예외 의존을 걷어낸다. 시험은 지우지 않고 "옛 이름 = 보호만, 권한 없음" 쪽으로 바꾼다.

### 4.4 상태 차트 판단

0단계 판단은 "해당 있음"이다. 관리 세션 식별이 이름에서 표식으로 옮겨 가면 세 곳의 전이가 달라진다: 정리 후보 선택(`src/services/discord/tmux_reaper.rs:210`·`:393`), 복원의 재획득 대상(`restore.rs:45`·`:128`), 생성 관문의 채택·생성. S1 모델 `SessionIdentityGate`는 마지막 관문만 다루고, r2 `SessionNaming`은 S2 refs에만 있고 main에 없다. 그래서 이중 판독 기간의 값(옛 이름 live, 사용자 동명 세션, 사용자의 옛 접두어 세션, 조회 실패, 재시작)을 넣은 경계 모델을 따로 돌렸다(부록 B). 결과는 `tlc=fixed GREEN 4/4, mutant RED 7/7(각 1개 불변식), unexpected 0`이다.

모델은 문서 한 파일 규칙 때문에 저장소에 커밋하지 않았다. 본문은 부록 B에 있고, 파일은 lane 로그 폴더에 있다. S2' 착지 때 이 모델을 `docs/statecharts/session-lifecycle/`에 r2 `SessionNaming`과 함께 넣고 `expected.tsv` 행을 추가한다.

## 5. 외부 도구·문서와 고칠 순서

정본(스킬·adk-config)은 목록만 낸다. 고치는 일은 코디네이터의 별도 승인 작업이다.

### 5.1 새 보호 판정식 (모든 외부 도구가 같은 식을 쓴다)

> 관리 세션 = 세션 환경에 `AGENTDESK_IDENTITY`가 있음 ∨ 이름이 `AgentDesk-`로 시작함 ∨ (자기 자신 판정일 때) 프로세스 환경에 `AGENTDESK_BINDING_CONTEXT`가 있음. 조회 실패 = 관리 세션으로 본다(보호).

판정 한 줄의 예시(`=` 정확 대상, 인용 필수):

```sh
tmux list-sessions -F '#{session_name}' | while IFS= read -r s; do
  case "$s" in AgentDesk-*) echo "$s"; continue;; esac
  tmux show-environment -t "=$s" AGENTDESK_IDENTITY >/dev/null 2>&1 && echo "$s"
done
```

이 식은 "죽이지 말라" 쪽의 보수적 판정이다. 입력·종료 권한은 Rust 관문(E3 owner_scope)만 준다.

### 5.2 목록(34파일)

| 순서 | 위치 | 파일:행 | 지금 의존 | 바꿀 것 |
|---|---|---|---|---|
| 1 (S9) | 저장소 | `policies/lib/timeouts-helpers.js:54`, `:57`, `:67`, `:73` | 접두어·provider 제거, `-t\d{15,}` 부모·스레드 추출 | 행의 `thread_channel_id`·`channel_id`·`agent_id` 우선, 옛 키만 해독 |
| 1 (S9) | 저장소 | `policies/timeouts/idle-kill.js:54`, `:84` | SQL `session_key !~ '-t[0-9]{15,}'`와 helper로 스레드 제외 | 새 스레드 이름은 정규식에 안 걸린다. `thread_channel_id IS NULL` 가드만 남으므로 **S6에서 새 스레드 행의 `thread_channel_id` 기록을 보장한 뒤** 판독을 바꾼다. Unknown은 kill 0 |
| 1 (S9) | 저장소 | `dashboard/src/components/agent-manager/discord-routing.ts:112` | 접두어·provider 제거로 채널 표시 | API의 channel_id·provider·display 필드 우선 |
| 1 (S8c) | 저장소 | `scripts/e2e/run_tui_relay.py:469`, `scripts/e2e/tui_relay/midturn_kill.py:168`, `scripts/e2e/post_deploy_turn_smoke.py:68` | 이름 생성·kill 거부·`-t` 꼬리 | BindingContext(`midturn_kill.py:202`가 이미 읽음)와 E3 기준 |
| 1 (S5) | 저장소 | `skills/agentdesk-restart/scripts/restart_agentdesk.sh:41` | 현재 세션이 `AgentDesk-*`면 재시작 거부 | 5.1 식(자기 세션의 `AGENTDESK_BINDING_CONTEXT`·표식) |
| 1 (S6) | 저장소 | `migrations/postgres/0101_canonical_discord_session_identity.sql:182` 함수(`canonical_identity.rs:609`·`:646`에서 사용) | scheduled를 `AgentDesk-<p>-scheduled-` 접두어로만 제외 | **새 scheduled 이름(`scheduled-<p>-…`)은 이 함수에서 "일반 채널"로 분류된다.** Rust 후보 SQL에 identity_kind·name_kind 제외 가드를 넣는다(r2 P2-7). migration은 고치지 않는다 |
| 1 (검토) | 저장소 | `scripts/deploy-release.sh:396` | `AgentDesk-dcserver-release-manual` 세션 이름 | 이름 변경은 선택이다. 바꾸지 않으면 §4.3 기준 1의 예외로 적는다 |
| 2 (S9) | 운영 문서 | `docs/session-binding.md`(이름 형식 정의 `:15`), `docs/agent-maintenance/multinode-two-node-smoke.md:38`(보호 문구), `docs/agent-maintenance/discord-outbound-migration.md:150`, `docs/relay-state-contract.md:1295`, `docs/generated/cli-reference.md`(`src/cli/utils.rs` 문구에서 생성) | 옛 형식 설명·보호 | 5.1 식과 새 형식 |
| 3 (코디네이터, S8c 전) | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/imessage-decision-alert/scripts/notify.py:115` | **접두어가 없으면 바로 `None` 반환, 바인딩 문맥 폴백 없음** | `AGENTDESK_BINDING_CONTEXT` 우선(imsg_send와 같은 방식) |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/imessage-send/scripts/imsg_send.py:237`-`240` | 폴백 이름 해석(`-t\d+` 제거) | S6 parent/thread 필드로 부모 binding. 폴백은 옛 이름만 |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-restart/scripts/restart_agentdesk.sh:40` + `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-restart/SKILL.md:19`·`:63` | 재시작 거부·예외 채널 예시 | 5.1 식 |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/launch-headless-leg.sh:119` | live TUI cwd 보호를 `^AgentDesk-` awk로 판정 | 5.1 식. 빠지면 #6332(레인 transcript 채택)가 다시 난다 |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/stop_orphan_test_procs.py:168`, `~/ObsidianVault/RemoteVault/99_Skills/SKILL.md:52`, `~/ObsidianVault/RemoteVault/99_Skills/references/execution.md:196` | 보호 문구·제외 | 5.1 식 |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/watch-agent-turn/SKILL.md:26`·`:52`·`:94`·`:113`·`:127` | 이름 조립·grep·`-t` 접두 대상 | locator 결과(세션 키 API) 또는 5.1 목록과 `=` 정확 대상 |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-runtime-ops/SKILL.md:12`·`:31`, `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-runtime-ops/references/commands.md:52`, `~/ObsidianVault/RemoteVault/99_Skills/topology.md:18`, `~/ObsidianVault/RemoteVault/99_Skills/recovery-playbook.md:9`(같은 references 폴더) | 보호·계수 | 5.1 식 |
| 3 | 스킬 정본 | `~/ObsidianVault/RemoteVault/99_Skills/adk-release/SKILL.md:42`·`:48`, `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-relay-e2e/SKILL.md:15`, `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-relay-audit/SKILL.md:51`, `~/ObsidianVault/RemoteVault/99_Skills/imessage-send/SKILL.md:72`, `~/ObsidianVault/RemoteVault/99_Skills/imessage-decision-alert/SKILL.md:37`, `~/ObsidianVault/RemoteVault/99_Skills/relay-gap-diagnose/SKILL.md:23`(이미 `ADK-*`로 잘못 적힘) | 보호·설명 | 5.1 식·새 형식 |
| 4 (코디네이터) | adk-config | `~/ObsidianVault/RemoteVault/adk-config/agents/project-agentdesk.prompt.md:15` | "`AgentDesk-*` 작업 세션은 사용자가 원할 때만 종료" | 5.1 식 |

괄호 안 조각은 그 파일을 고칠 조각이다. 정본 스킬은 S8c 전에 고친다. `.pre-*` 백업 파일은 대상이 아니다.

## 6. 조각 계획

상한은 `scripts/pr_cap_prod.py` 상수 `FILES, NET = 30, 1500`이고, 계획 단계에서 +1200에서 멈춘다. 추정값은 부록 A의 행 수와 r2 추정을 바탕으로 했고, 실제 값은 `pr_cap_prod.py <base> <head>`로 다시 잰다.

| 조각 | 선행 | prod 파일/순증 추정 | 내용 | RED / 뮤턴트 |
|---|---|---:|---|---|
| ~~S1~~ | — | 착지 | 표식·마지막 관문 | — |
| **S2'** | S1 | 30/+900 (지금 30/+882) | S2를 406da315a7 위로 재병합한다(충돌 3건 해소). 30파일을 넘으면 **S2a**(DB `key_promotion`·`canonical_identity`)와 **S2b**(locator·호출부·§2.1 이중 이름 생성 관문)로 나눈다. 부록 B 모델을 statecharts에 넣는다 | primary=new/옛 별칭=같은 행/outcome=new. 옛 live 중 생성 0(`generator_first`·`mixed_builder`) |
| S3 | S2' | 25/+450 | provider·raw 별칭 판독(부록 A-PRS의 S3 29행)과 `legacy_decode` 카운터 | queue 취소·policy kill 뒤 새 세션 없음 / 옛 별칭 직접 파싱·provider 기본값 |
| S4 | S2',S3 | 16/+500 | watcher·복원(부록 A의 S4·S2'→S4 24행), `state_extractors.rs:139` 저장 이름 우선 | 옛 live의 경로·nonce·offset 불변 |
| S5 | S2'~S4 | 16/+600 | 정리·inventory(부록 A-PFX의 S5 14행과 reaper·unbound의 PRS 행). wrapper 경로 역추정(`tmux_reaper.rs:830`)은 명시 인자로 바꾼다. 저장소 `restart_agentdesk.sh` | 남의 세션·가짜 옛 이름·Unknown에서 추가 kill 0(`name_reap`) |
| S6 | S2'~S5 | 22/+550 | 표식·BindingContext의 parent/thread 채움, tmux·키에서 읽는 `-t` 판독 18행, `session_evidence.rs:66`, 후보 SQL 가드, 훅의 `thread_channel_id` 기록 보장 | identity 없는 scheduled 승격 0 / parent 읽기 제거 |
| S7a | S1,S2' | 4/+200 | 순수 경로 공식(옛 이름 = 같은 바이트, 새 이름 = 원 바이트 해시) | NFC/NFD 분리·NAME_MAX |
| S7b | S7a | 7/+350 | 새 할당 예약 | 동명 경합 시 덮어쓰기 0 |
| S8a | S1~S6 | 15/+400 | 이름 입력 타입과 생산자(DM·scheduled·routine·recovery, 부록 A-GEN의 S8a 8행) | DM provider별 분리 |
| S8b1 | S2',S8a | 20/+350 | 제어 쪽 재구성 호출부 28행 → locator(`herdr_settings.rs` 비교 포함) | 옛 live 재사용·새 primary 정지의 실제 효과 |
| S8b2 | S2',S4,S8a | 20/+350 | 복원·정리 쪽 재구성 20행 → locator | 옛·새 자연 재생성 |
| S9 | S2',S6 | 10/+250 | policies·dashboard·운영 문서·생성 문서 | 중지 뒤 새 물리 세션 없음 |
| S8c | 위 전부 + §5 외부 3·4단계 | 20/+550 | 작성 전환(새 생성만 새 이름), e2e 작성자, 활성화 시험(r2 §실측), 롤백 하한 SHA 기록 | 44B 자르기 복원·`AgentDesk-` label 대체 제거 |
| S10 | §4.3 관측 기준 | 15/−(순감) | 옛 판독 제거, `require_managed` 접두어 지름길 제거 | 옛 접두어 사용자 세션 채택 0(`prefix_trust`) |

남은 조각은 **13개**다(S2'를 둘로 나누면 14개). 순서는 판독(S2'~S7b) → 생산자·호출부(S8a~S9) → 외부 도구(코디네이터) → 작성 전환(S8c) → 관측 → 제거(S10)다.

### 6.1 진행 중 레인과 겹치는 파일

| 레인 | 그 레인의 범위 | 겹치는 조각과 파일 | 처분 |
|---|---|---|---|
| #7651 | `tui_o/writer/**` | 없음(`src/services/tui_o`에 이름 판독 0건) | 독립 |
| #8114 | `claude_tui/host_input/busy*` | 없음(S1에서 `busy_inject`를 이미 바꿈, host_input에 이름 판독 0건) | 독립 |
| #8125 | `owner_input*`, turn/start 스레드 | **S2'**: `provider_dispatch/owner_input.rs`와 owner_input 시험 5개. **S6**: 스레드 parent 필드(#8125는 스레드 ID를 받는 API) | S2'를 먼저 착지하거나, 늦게 착지하는 쪽이 재병합한다. 스레드 parent의 정본은 S6(r2 #8118 경계와 같음) |
| #8161 | `claude/herdr_turn`, `claude_tui/readiness` | 직접 겹침 없음. S8c가 Herdr launch(`src/services/session_host/herdr/launch_host.rs`)와 `src/services/claude/tui_session_launch.rs`를 건드린다 | S8c는 #8161 뒤 |
| #8196 | `idle_detector`, heartbeat 출처 | **S2'**: `watchers/lifecycle/activity.rs`. **S6**: `turn_bridge/activity_heartbeat.rs`(합성 이름 판독이라 S6는 손대지 않는다. 표만 갱신) | activity.rs는 늦게 착지하는 쪽이 재병합 |
| #8192 | 메신저 중립 이동(SharedData·`discord/mod.rs`·ChannelId) | **S2'**: `src/services/discord/mod.rs`. S3~S8b2 대부분이 `src/services/discord/**`에 있어 파일 이동과 충돌한다 | #8192 P0의 파일 이동 전에 S2'를 착지하는 것을 권한다. 이동 뒤라면 조각마다 경로를 다시 매긴다 |
| #7583 | 세션 호스트 공통화(2단계 이후) | S5(`src/cli/utils.rs`의 `TmuxHost.kill`), S8c(`session_host/**`), `src/services/session_host/resolve.rs` | #7583 2단계는 관리 판정을 이름이 아니라 `ManagedIdentity`로 받는다. 둘 중 늦게 착지하는 쪽이 판정식을 맞춘다 |

## 7. 리뷰 집중 항목

- **`AgentDesk-*` 보호에 기대는 규칙·도구가 새 이름 세션을 보호하지 못하는 길.** §5의 X 13곳은 새 이름 세션에 대해 조용히 "보호 대상 아님"이 된다. 가장 위험한 것은 `restart_agentdesk.sh`(작업 세션 안에서 dcserver를 재시작해 자기를 끊음)와 `~/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/scripts/launch-headless-leg.sh:119`(live TUI cwd를 레인이 차지함)다. S8c 선행 조건으로 5.1 판정식 이행과 preflight 스캔 0건을 둔다. 이름을 바꾼 뒤 glob에 걸리는 세션은 `AgentDesk-dcserver-release-manual` 하나뿐이라 "보호가 동작하는 것처럼 보이는" 착시가 생긴다.
- **사용자 tmux 세션을 관리 세션으로 오인하는 길.** 새 이름은 E3 owner_scope로만 관리 판정을 한다. 격리 실측에서 표식 없는 `adk-cc`는 조회 rc 1 → 거부였다. 남은 길은 두 가지다. (1) 사용자가 `AgentDesk-…` 이름으로 만든 세션은 `require_managed` 지름길(`:17`)로 통과한다(main부터 있던 구멍, 모델 `prefix_trust` RED). 이중 판독 기간에는 접두어를 보호에만 쓰고 채택에는 E1 일치를 요구한다. 지름길은 S10에서 지운다. (2) 정책·스크립트의 접두 일치 `-t name`. `=` 정확 대상을 계약으로 둔다.
- **이중 판독 기간에 같은 채널에 옛·새 세션이 둘 다 생기는 길.** 세 가지가 있다.
  - 재구성 호출부 56행이 새 이름을 기대해 "없음"으로 보고 생성하는 길(`generator_first`).
  - 옮기지 않은 호출부가 옛 이름을 다시 만드는 길(`mixed_builder`).
  - 조회 실패를 "없음"으로 읽는 길(`probe_absent`).

  §2.1 생성 관문의 이중 이름 확인이 셋을 한 곳에서 막는다. 나머지 둘은 운영 쪽 길이다: 판독 조각이 없는 노드나 롤백 바이너리(§4.1의 전 노드 선배포·롤백 하한 SHA), 그리고 옛 판독을 일찍 지우는 길(`early_legacy_removal`, §4.3 관측 기준).

## 8. 증거 목록

| 파일 | 내용 |
|---|---|
| `~/.adk/release/lane-runs/hl-20260925/i8119/logs/d1r3-live-tmux.log` | 이 맥 live tmux 이름(읽기 전용) |
| `~/.adk/release/lane-runs/hl-20260925/i8119/logs/d1r3-collision-sim.log` | 설정 채널 48개의 옛·새 이름 겹침 계산 |
| `~/.adk/release/lane-runs/hl-20260925/i8119/logs/d1r3-nfc-nfd-probe.log` | 격리 tmux·APFS NFC/NFD·사용자 동명 세션 실측 |
| `~/.adk/release/lane-runs/hl-20260925/i8119/logs/d1r3-tla/` | 부록 B 모델·cfg·`tlc-summary.txt` |

<!-- doc-path-refs: proposed src/db/dispatched_sessions/key_promotion.rs src/services/discord/session_locator.rs -->

# 부록 A. 이름 판독 전수표(406da315a7, 시험 제외)

조사 방법: `#[cfg(test)]` 블록과 시험 파일(`*_tests.rs`, `tests/`, `tests_pg.rs`, `relay_e2e/`, fixture 모듈)을 빼고 Rust 소스를 검색했다. 접두어 판정, `parse_provider_and_channel_from_tmux_name`·`provider_and_channel()`, `-t` 꼬리 해독기, `build_tmux_session_name` 호출이 대상이다. API 문서 예시(`src/server/routes/docs/inventory/`)와 단순 문구는 뺐다. 세션 키에서 tmux 꼬리를 꺼내는 `SessionIdentity::parse`·`tmux_name_from_session_key` 95행은 권고안 A의 불변식으로 해결되므로 표에 넣지 않았다. 다만 옛 별칭 키를 요청으로 받는 `force_kill`·`tmux_cleanup`·`session_resume`은 S3에서 별칭 resolver를 먼저 거친다(r2 부록 B 26경계).

"유지(합성 채널 이름)"는 tmux 이름이 아니라 메모리·훅의 합성 채널 이름 `<부모>-t<ID>`를 읽는 행이다. §2 규칙대로 그대로 둔다.

### A-PFX. 이름 접두어로 관리/분류 — 18행 / 16파일

대체 근거: E3 표식(`AGENTDESK_IDENTITY` owner_scope) → 없으면 E1 행; 옛 이름은 L(보호 쪽으로만).

| file:line | 조각 | 발췌 |
|---|---|---|
| `src/cli/doctor/orchestrator.rs:2719` | S5 | `if session.server_pid == 0 \|\| !session.session_name.starts_with("AgentDesk-") {` |
| `src/cli/utils.rs:178` | S5 | `.starts_with("AgentDesk-")` |
| `src/cli/utils.rs:185` | S5 | `if name.starts_with("AgentDesk-") {` |
| `src/db/session_evidence.rs:66` | S6 | `AND s.session_key NOT LIKE '%:AgentDesk-' \|\| s.provider \|\| '-scheduled-%'` |
| `src/services/cluster/session_matcher.rs:365` | S2' | `let prefix = format!("{}-", tmux_session_prefix());` |
| `src/services/cluster/session_matcher.rs:519` | S2' | `let prefix = format!("{}-", tmux_session_prefix());` |
| `src/services/discord/health/o_transition/codex_census.rs:47` | S5 | `if name.starts_with("AgentDesk-") {` |
| `src/services/discord/health/watcher_respawn.rs:167` | S5 | `tmux_session.is_some_and(\|session\| session.starts_with("AgentDesk-"))` |
| `src/services/discord/recovery_engine/state_extractors.rs:139` | S4 | `crate::domain::provider::naming::tmux_session_prefix()` |
| `src/services/discord/relay_recovery/decision.rs:252` | S5 | `tmux_session.is_some_and(\|session\| session.starts_with("AgentDesk-"))` |
| `src/services/discord/tmux_reaper.rs:830` | S5 | `let pos = stem.find("AgentDesk-")?;` |
| `src/services/discord/tmux_reaper/host_guard.rs:157` | S5 | `crate::domain::provider::naming::tmux_session_prefix()` |
| `src/services/discord/turn_presence/activity/unbound.rs:465` | S5 | `crate::domain::provider::naming::tmux_session_prefix()` |
| `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:116` | S5 | `crate::domain::provider::naming::tmux_session_prefix(),` |
| `src/services/discord/turn_presence/activity/unbound/hold_leaf.rs:72` | S5 | `crate::domain::provider::naming::tmux_session_prefix()` |
| `src/services/maintenance/jobs/worktree_orphan_sweep.rs:634` | S5 | `if !session.starts_with("AgentDesk-") {` |
| `src/services/message_outbox/session_notice.rs:62` | S3 | `.starts_with(&format!("AgentDesk-{}-", provider.as_str()))` |
| `src/services/platform/tmux/session_identity.rs:17` | S10(접두어 지름길 제거) | `if session.starts_with("AgentDesk-") {` |

### A-PRS. 이름에서 provider·채널 — 59행 / 42파일

대체 근거: E1 행(provider·channel_id) / E2 Herdr 기록 / E3 표식; 실패는 Unknown 보존.

| file:line | 조각 | 발췌 |
|---|---|---|
| `src/db/session_agent_resolution.rs:19` | S3 | `let (_, channel_name) = parse_provider_and_channel_from_tmux_name(&identity.tmux_name)?;` |
| `src/domain/outbound/outbox/identity.rs:87` | S3 | `crate::domain::session_identity::naming::parse_provider_and_channel_from_tmux_name(` |
| `src/services/auto_queue/runtime.rs:125` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(` |
| `src/services/claude_tui/tui_relay.rs:619` | S3 | `parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/cluster/session_discovery.rs:391` | S2'→S4 | `let (provider, tmux_segment) = parse_provider_and_channel_from_tmux_name(&tmux_name)?;` |
| `src/services/cluster/session_matcher.rs:231` | S2'→S4 | `let (provider, channel_segment) = parse_provider_and_channel_from_tmux_name(session_name)?…` |
| `src/services/cluster/session_matcher.rs:327` | S2'→S4 | `let _ = parse_provider_and_channel_from_tmux_name(session_name)?;` |
| `src/services/cluster/session_matcher.rs:363` | S2'→S4 | `parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/health/recovery/stall_alert.rs:26` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&identity.tmux_name)` |
| `src/services/discord/health/recovery/stall_alert.rs:36` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)` |
| `src/services/discord/health/watcher_respawn/idle_relay_absence.rs:84` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session)` |
| `src/services/discord/host_defer_gate.rs:255` | S3 | `let parsed = crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_nam…` |
| `src/services/discord/host_teardown_gate.rs:199` | S3 | `None => crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)` |
| `src/services/discord/inflight_heartbeat_sweeper.rs:97` | S3 | `parse_provider_and_channel_from_tmux_name(&tmux_session_name)` |
| `src/services/discord/org_schema.rs:317` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/org_schema.rs:337` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/recovery_engine/herdr_row_end.rs:361` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux.as_deref()?)?;` |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:250` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)` |
| `src/services/discord/recovery_engine/restore_inflight.rs:679` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(name)` |
| `src/services/discord/recovery_engine/restore_inflight.rs:722` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(name)` |
| `src/services/discord/recovery_engine/restore_inflight.rs:949` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(name)` |
| `src/services/discord/recovery_engine/restore_inflight.rs:1833` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(` |
| `src/services/discord/relay_recovery.rs:372` | S3 | `let Some((identity_provider, channel)) = identity.provider_and_channel() else {` |
| `src/services/discord/tmux/monitor_auto_turn_inflight.rs:26` | S3 | `let channel_name = parse_provider_and_channel_from_tmux_name(tmux_session_name)` |
| `src/services/discord/tmux/relay_decisions.rs:31` | S3 | `let claude = !matches!(crate::services::provider::parse_provider_and_channel_from_tmux_nam…` |
| `src/services/discord/tmux/relay_decisions.rs:35` | S3 | `if matches!(crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_ses…` |
| `src/services/discord/tmux_kill_policy.rs:132` | S5 | `let (provider, _) = parse_provider_and_channel_from_tmux_name(tmux_session_name)?;` |
| `src/services/discord/tmux_lifecycle.rs:72` | S2'→S4 | `let parsed_channel_name = parse_provider_and_channel_from_tmux_name(tmux_session_name)` |
| `src/services/discord/tmux_reaper.rs:210` | S5 | `parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/tmux_reaper.rs:232` | S5 | `let parsed_channel_name = parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/tmux_reaper.rs:393` | S5 | `let Some((session_provider, _)) = parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/tmux_reaper/host_guard.rs:162` | S5 | `let (provider, _) = parse_provider_and_channel_from_tmux_name(name)?;` |
| `src/services/discord/tmux_restart_handoff.rs:497` | S4 | `let Some((provider_kind, _)) = parse_provider_and_channel_from_tmux_name(tmux_session_name…` |
| `src/services/discord/tmux_watcher.rs:234` | S4 | `parse_provider_and_channel_from_tmux_name(&tmux_session_name).unwrap_or((` |
| `src/services/discord/tmux_watcher/discrete_trigger_marker.rs:33` | S4 | `let label = crate::services::provider::parse_provider_and_channel_from_tmux_name(` |
| `src/services/discord/tmux_watcher/entry.rs:69` | S4 | `parse_provider_and_channel_from_tmux_name(tmux_session_name).and_then(\|(pk, _)\| {` |
| `src/services/discord/tmux_watcher/post_stream_exit.rs:220` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)` |
| `src/services/discord/tmux_watcher/provider_session_persistence.rs:91` | S4 | `&crate::services::provider::parse_provider_and_channel_from_tmux_name(` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:468` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:551` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)?;` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:556` | S4 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&base_session_name)?;` |
| `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:113` | S5 | `parse_provider_and_channel_from_tmux_name(&receipt.name).is_some_and(\|(provider, _)\| {` |
| `src/services/discord/turn_presence/activity/unbound/routine_owner.rs:72` | S5 | `let Some((provider, _)) = parse_provider_and_channel_from_tmux_name(name) else {` |
| `src/services/discord/turn_presence/activity/unbound/routine_owner.rs:124` | S5 | `let (provider, _) = parse_provider_and_channel_from_tmux_name(name)?;` |
| `src/services/discord/watchers/lifecycle/restore.rs:45` | S2'→S4 | `parse_provider_and_channel_from_tmux_name(l)` |
| `src/services/discord/watchers/lifecycle/restore.rs:79` | S2'→S4 | `if let Some((_, channel_name)) = parse_provider_and_channel_from_tmux_name(session_name)` |
| `src/services/discord/watchers/lifecycle/restore.rs:128` | S2'→S4 | `if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {` |
| `src/services/dispatched_sessions.rs:568` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)` |
| `src/services/dispatched_sessions/force_kill.rs:87` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);` |
| `src/services/dispatched_sessions/tmux_cleanup.rs:40` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);` |
| `src/services/dispatched_sessions/tmux_cleanup.rs:526` | S3 | `let channel = crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_na…` |
| `src/services/message_outbox/session_notice.rs:57` | S3 | `let (provider, channel) = parse_provider_and_channel_from_tmux_name(&identity.tmux_name)?;` |
| `src/services/provider/cancel_token_cleanup/authority.rs:100` | S3 | `let parsed_provider = parse_provider_and_channel_from_tmux_name(name).map(\|(kind, _)\| ki…` |
| `src/services/session_activity.rs:69` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)` |
| `src/services/session_resume.rs:236` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(&tmux_name);` |
| `src/services/stale_turn_reconciler.rs:311` | S3 | `let Some((tmux_provider, _)) = identity.provider_and_channel() else {` |
| `src/services/stale_turn_reconciler.rs:731` | S3 | `let Some((tmux_provider, _)) = identity.provider_and_channel() else {` |
| `src/services/tmux_turn_liveness.rs:68` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_session_name)` |
| `src/services/tmux_turn_liveness/claude_idle.rs:10` | S3 | `crate::services::provider::parse_provider_and_channel_from_tmux_name(session),` |

### A-THR. `-t<숫자>` 꼬리 — 35행 / 24파일

대체 근거: E1 `thread_channel_id`·E4 BindingContext `thread_id`(S6 추가); 합성 채널 이름 판독은 그대로.

| file:line | 조각 | 발췌 |
|---|---|---|
| `src/db/dispatched_sessions/thread_gc.rs:28` | S6 | `let Some(thread_channel_id) = parse_thread_channel_id_from_session_key(&session_key) else …` |
| `src/db/session_agent_resolution.rs:8` | S6 | `let pos = channel_name.rfind("-t")?;` |
| `src/db/session_agent_resolution.rs:25` | S6 | `parse_thread_channel_name(&channel_name).map(\|(_, thread_id)\| thread_id.to_string())` |
| `src/db/session_agent_resolution.rs:215` | S6 | `session_key.and_then(parse_thread_channel_id_from_session_key);` |
| `src/db/session_agent_resolution.rs:239` | S6 | `let channel_name = parse_thread_channel_name(channel_name)` |
| `src/services/cluster/session_matcher.rs:213` | S6 | `let pos = channel_segment.rfind("-t")?;` |
| `src/services/cluster/session_matcher.rs:232` | S6 | `let (parent_segment, thread_id) = thread_suffix_channel_id(&channel_segment)?;` |
| `src/services/discord/adk_session.rs:51` | 유지(합성 채널 이름) | `let pos = channel_name.rfind("-t")?;` |
| `src/services/discord/recovery_engine/restore_inflight.rs:2274` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);` |
| `src/services/discord/router/message_handler/intake_turn/adk_thread.rs:10` | 유지(합성 채널 이름) | `.and_then(adk_session::parse_thread_channel_id_from_name)` |
| `src/services/discord/session_runtime/channel_routing.rs:21` | 유지(합성 채널 이름) | `&& is_synthetic_thread_channel_name(existing_name, channel_id)` |
| `src/services/discord/tmux_lifecycle.rs:77` | 유지(합성 채널 이름) | `.and_then(super::adk_session::parse_thread_channel_id_from_name)` |
| `src/services/discord/tmux_lifecycle.rs:81` | S6 | `.and_then(super::adk_session::parse_thread_channel_id_from_name)` |
| `src/services/discord/tmux_reaper.rs:502` | 유지(합성 채널 이름) | `let pos = n.rfind("-t")?;` |
| `src/services/discord/tmux_reaper.rs:530` | 유지(합성 채널 이름) | `.and_then(super::adk_session::parse_thread_channel_id_from_name),` |
| `src/services/discord/tmux_reaper/host_guard.rs:154` | S6 | `let full_suffix = format!("-t{thread_channel_id}{env_suffix}");` |
| `src/services/discord/tmux_watcher.rs:254` | S6 | `crate::services::discord::adk_session::parse_thread_channel_id_from_name(` |
| `src/services/discord/tmux_watcher/post_stream_exit.rs:309` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);` |
| `src/services/discord/tmux_watcher/provider_session_persistence.rs:90` | S6 | `crate::services::discord::adk_session::parse_thread_channel_id_from_name(` |
| `src/services/discord/tmux_watcher/terminal_token_update.rs:43` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:557` | S6 | `let thread_suffix = session_segment` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:560` | S6 | `if thread_suffix.is_empty() \|\| !thread_suffix.chars().all(\|c\| c.is_ascii_digit()) {` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:563` | S6 | `thread_suffix.parse::<u64>().ok()` |
| `src/services/discord/turn_bridge/activity_heartbeat.rs:10` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)` |
| `src/services/discord/turn_bridge/activity_heartbeat.rs:15` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)` |
| `src/services/discord/turn_bridge/completion_postlude.rs:237` | 유지(합성 채널 이름) | `crate::services::discord::adk_session::parse_thread_channel_id_from_name,` |
| `src/services/discord/turn_bridge/post_loop_finalize.rs:468` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name),` |
| `src/services/discord/turn_bridge/stream_tick.rs:676` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name),` |
| `src/services/discord/turn_bridge/turn_analytics.rs:374` | 유지(합성 채널 이름) | `.and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name)` |
| `src/services/discord/watchers/lifecycle/restore.rs:129` | S6 | `if let Some(pos) = ch_name.rfind("-t") {` |
| `src/services/discord/watchers/lifecycle/restore.rs:787` | 유지(합성 채널 이름) | `super::super::super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);` |
| `src/services/dispatched_sessions/hook.rs:13` | 유지(합성 채널 이름) | `.and_then(parse_thread_channel_name)` |
| `src/services/dispatched_sessions/hook.rs:16` | S6 | `.or_else(\|\| parse_thread_channel_id_from_session_key(&body.session_key));` |
| `src/services/dispatched_sessions/listing.rs:95` | S6 | `.and_then(parse_thread_channel_id_from_session_key)` |
| `src/services/routines/session_control.rs:564` | S6 | `let thread_suffix_regex = format!("-t{}(-dev)?$", thread_channel_id);` |

### A-GEN. 채널 이름→기대 이름 재구성 — 56행 / 41파일

대체 근거: S2 `session_locator`(저장 primary·Herdr 기록·이중 이름 생존 확인)로 대체.

| file:line | 조각 | 발췌 |
|---|---|---|
| `src/services/agent_recovery/handoff.rs:39` | S8b2 | `tmux_name: fallback_provider.build_tmux_session_name(&binding.channel_id),` |
| `src/services/cluster/session_matcher.rs:233` | S8b1 | `let parent_session_name = provider.build_tmux_session_name(parent_segment);` |
| `src/services/cluster/session_matcher.rs:426` | S8b1 | `provider.build_tmux_session_name(channel_id)` |
| `src/services/discord/adk_session.rs:84` | S8a | `let tmux_name = provider.build_tmux_session_name(&basis);` |
| `src/services/discord/admin_host_guard.rs:165` | S8b1 | `let tmux_name = provider.build_tmux_session_name(&name);` |
| `src/services/discord/admin_host_guard.rs:205` | S8b1 | `let tmux_name = named.map(\|name\| provider.build_tmux_session_name(name));` |
| `src/services/discord/admin_host_guard.rs:320` | S8b1 | `channel_name.map(\|name\| provider.build_tmux_session_name(name))` |
| `src/services/discord/commands/control.rs:205` | S8b1 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/commands/control.rs:537` | S8b1 | `.map(\|ch_name\| provider.build_tmux_session_name(ch_name))` |
| `src/services/discord/commands/control/managed_reset.rs:27` | S8b1 | `.map(\|name\| provider.build_tmux_session_name(name))` |
| `src/services/discord/commands/control/provider_reset.rs:34` | S8b1 | `.map(\|channel_name\| provider.build_tmux_session_name(channel_name))` |
| `src/services/discord/commands/control/relaunch.rs:71` | S8b1 | `let tmux = provider.build_tmux_session_name(&channel_name);` |
| `src/services/discord/commands/diagnostics/reports.rs:209` | S8b1 | `session_channel_name.map(\|name\| provider.build_tmux_session_name(&name));` |
| `src/services/discord/commands/diagnostics/reports.rs:314` | S8b1 | `session_channel_name.map(\|name\| provider.build_tmux_session_name(&name));` |
| `src/services/discord/commands/tui_passthrough.rs:225` | S8b1 | `.map(\|channel_name\| effective_provider.build_tmux_session_name(channel_name));` |
| `src/services/discord/commands/tui_passthrough/herdr_settings.rs:37` | S8b1 | `let logical = ProviderKind::Claude.build_tmux_session_name(&name);` |
| `src/services/discord/health/recovery.rs:264` | S8b1 | `.map(\|channel_name\| provider.build_tmux_session_name(channel_name))` |
| `src/services/discord/health/recovery/channel_runtime.rs:284` | S8b1 | `.map(\|channel_name\| provider.build_tmux_session_name(channel_name))` |
| `src/services/discord/health/recovery/runtime_lookup.rs:130` | S8b1 | `let expected_tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/health/turn_deliver/inject.rs:283` | S8b1 | `named.map(\|name\| provider.build_tmux_session_name(&name)),` |
| `src/services/discord/host_defer_gate.rs:71` | S8b1 | `let tmux_name = provider.build_tmux_session_name(&name);` |
| `src/services/discord/host_teardown_gate.rs:105` | S8b1 | `names.extend(channel_name.map(\|name\| provider.build_tmux_session_name(name)));` |
| `src/services/discord/host_teardown_gate.rs:302` | S8b1 | `let name = channel_name.map(\|name\| provider.build_tmux_session_name(name));` |
| `src/services/discord/recovery_engine/manual_rebind/mod.rs:265` | S8b2 | `let tmux = provider.build_tmux_session_name(&ch_name);` |
| `src/services/discord/recovery_engine/manual_rebind/watcher_claim.rs:74` | S8b2 | `provider.build_tmux_session_name(&channel_name)` |
| `src/services/discord/recovery_engine/restore_inflight.rs:314` | S8b2 | `.map(\|name\| tmux_runtime_paths(&provider.build_tmux_session_name(name)).0)` |
| `src/services/discord/recovery_engine/restore_inflight.rs:945` | S8b2 | `.map(\|name\| provider.build_tmux_session_name(name))` |
| `src/services/discord/recovery_engine/state_extractors.rs:143` | S8b2 | `provider.build_tmux_session_name(name)` |
| `src/services/discord/router/message_handler.rs:248` | S8b1 | `.map(\|name\| provider.build_tmux_session_name(name))` |
| `src/services/discord/router/message_handler/headless_turn/entrypoints.rs:124` | S8b1 | `let own = named.is_some_and(\|name\| provider.build_tmux_session_name(name) == pane);` |
| `src/services/discord/router/message_handler/headless_turn/routine_metadata.rs:83` | S8b1 | `.map(\|name\| provider.build_tmux_session_name(name))` |
| `src/services/discord/router/message_handler/tui_followup/nameless_gate.rs:93` | S8b1 | `.map(\|name\| provider.build_tmux_session_name(name))` |
| `src/services/discord/session_idle_cleanup.rs:88` | S8b2 | `let tmux_name = \|name: &String\| provider.build_tmux_session_name(name);` |
| `src/services/discord/session_runtime.rs:359` | S8b2 | `let tmux_name = provider.build_tmux_session_name(ch);` |
| `src/services/discord/session_runtime/restore_cwd.rs:162` | S8b2 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/session_runtime/restore_cwd.rs:199` | S8b2 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/session_runtime/restore_cwd.rs:260` | S8b2 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/tmux_reaper.rs:227` | S8b2 | `.map(\|ch_name\| provider.build_tmux_session_name(ch_name) == session_name)` |
| `src/services/discord/tmux_reaper.rs:418` | S8b2 | `.map(\|ch\| provider.build_tmux_session_name(ch) == session_name)` |
| `src/services/discord/tmux_reaper.rs:491` | S8b2 | `provider.build_tmux_session_name(channel_name.as_deref().unwrap_or("unknown"));` |
| `src/services/discord/tmux_restart_handoff.rs:206` | S8b1 | `let tmux_name = provider_kind.build_tmux_session_name(channel_name);` |
| `src/services/discord/tui_prompt_relay/claude_idle_runtime.rs:545` | S8b1 | `let base_session_name = provider.build_tmux_session_name(segment);` |
| `src/services/discord/turn_presence/activity/unbound.rs:91` | S8a | `name: provider.build_tmux_session_name(basis),` |
| `src/services/discord/turn_presence/activity/unbound.rs:104` | S8a | `name: provider.build_tmux_session_name(basis),` |
| `src/services/discord/turn_presence/activity/unbound/completed_scheduled.rs:163` | S8a | `\|\| provider.build_tmux_session_name(&format!("scheduled:{}", row.definition))` |
| `src/services/discord/turn_presence/supervisor.rs:919` | S8b1 | `named.map(\|name\| self.provider.build_tmux_session_name(&name)),` |
| `src/services/discord/watchers/lifecycle/restore.rs:64` | S8b2 | `let tmux_name = provider.build_tmux_session_name(ch_name);` |
| `src/services/discord/watchers/lifecycle/restore.rs:111` | S8b2 | `let tmux_name = provider.build_tmux_session_name(&channel.name);` |
| `src/services/discord/watchers/lifecycle/restore.rs:818` | S8b2 | `let tmux_name = provider.build_tmux_session_name(&dc.channel_name);` |
| `src/services/discord/watchers/lifecycle/restore_core.rs:26` | S8b2 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/watchers/lifecycle/restore_mapping.rs:36` | S8b2 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/discord/watchers/lifecycle/restore_support.rs:199` | S8b2 | `let tmux_name = provider.build_tmux_session_name(channel_name);` |
| `src/services/routines/agent_executor/reliability.rs:245` | S8a | `provider.build_tmux_session_name(&routine_agent_session_name(&run.name, agent_id));` |
| `src/services/routines/fresh_session_reaper.rs:69` | S8a | `provider.build_tmux_session_name(&routine_agent_session_name(&routine.name, agent_id))` |
| `src/services/routines/session_control.rs:433` | S8a | `.unwrap_or_else(\|\| provider.build_tmux_session_name(&fallback_channel_name));` |
| `src/services/routines/session_control.rs:904` | S8a | `provider.build_tmux_session_name(&routine_agent_session_name(&routine.name, agent_id))` |

# 부록 B. 이중 판독 기간 경계 모델 `SessionNameDualRead`

채널 하나에 대한 경계 투영이다. 변수는 옛 이름 관리 세션(`oldLive`), 새 이름 관리 세션(`newLive`), 대체 후보 세션(`fbLive`), 새 이름을 먼저 쓴 사용자 세션(`userNew`), 사용자가 만든 `AgentDesk-…` 이름 세션(`userOld`), 행 primary(`row`), 채택 대상(`adopted`)이다. 조회 성공·실패는 `Turn` 안에서 비결정으로 고르고, `Restart`는 메모리 채택을 잃는다. 전체 수명(턴·입력·DB 경합)은 증명 범위가 아니다. 그 부분은 S2 PG 시험과 S8c 활성화 시험이 맡는다. 조회 실패 뒤 진행(liveness)은 검사하지 않았다.

실행: TLC 1.7.4(jar sha256 앞 16자 `936a262061c91469`), OpenJDK 27.0.1, `-workers 1`, `CHECK_DEADLOCK FALSE`, 모드×불변식마다 따로 1회씩 총 32회(2026-11-16 14:23 KST).

| 모드 | 무엇을 뺐나 | NoDuplicate | NoForeignAdoption | NoForeignTouch | NoLiveCut |
|---|---|---|---|---|---|
| `fixed` | 정상(§2.1 관문, E1 우선, 조회 실패 보류, 표식 판정) | GREEN | GREEN | GREEN | GREEN |
| `generator_first` | 저장 행·옛 이름을 보지 않고 새 이름만 확인 | RED | GREEN | GREEN | GREEN |
| `name_adopt` | 새 이름이 있으면 표식 없이 채택 | GREEN | RED | GREEN | GREEN |
| `probe_absent` | 조회 실패를 없음으로 읽음 | RED | GREEN | GREEN | GREEN |
| `prefix_trust` | 옛 접두어만으로 채택 | GREEN | RED | GREEN | GREEN |
| `mixed_builder` | 옮기지 않은 호출부가 옛 이름 재생성 | RED | GREEN | GREEN | GREEN |
| `early_legacy_removal` | 관측 전에 옛 이름 세션을 잔재로 정리 | GREEN | GREEN | GREEN | RED |
| `name_reap` | 새 이름 공간을 이름으로 정리 | GREEN | GREEN | RED | GREEN |

결과: 정상 모델은 4개 불변식 모두 GREEN이다. 뮤턴트 7개는 각자 노린 불변식 하나에서만 RED이고, 예상하지 않은 결과는 0이다. 상태 수는 모드마다 12~44 distinct다(`tlc-summary.txt`).

```tla
------------------------- MODULE SessionNameDualRead -------------------------
\* #8119 dual-read window, one channel. Bounded projection: names, marker, row
\* primary locator, probe outcome, restart. Not the full lifecycle.
EXTENDS Naturals, FiniteSets
CONSTANT Mode
Modes == {"fixed", "generator_first", "name_adopt", "probe_absent",
          "prefix_trust", "mixed_builder", "early_legacy_removal", "name_reap"}
ASSUME Mode \in Modes
VARIABLES oldLive, newLive, fbLive, userNew, userOld, row, adopted, cut, foreign
vars == <<oldLive, newLive, fbLive, userNew, userOld, row, adopted, cut, foreign>>
Managed == {"old", "new", "fb"}
Live(s) == CASE s = "old" -> oldLive [] s = "new" -> newLive [] s = "fb" -> fbLive
Init == /\ oldLive \in BOOLEAN            \* old-name managed session may be live
        /\ newLive = FALSE /\ fbLive = FALSE
        /\ userNew \in BOOLEAN            \* a user tmux session already named adk-cc
        /\ userOld \in BOOLEAN /\ (userOld => ~oldLive)
        /\ row \in IF oldLive THEN {"old"} ELSE {"none", "old"}
        /\ adopted = IF oldLive THEN "old" ELSE "none"
        /\ cut = FALSE /\ foreign = FALSE
OldEnds == /\ oldLive /\ oldLive' = FALSE
           /\ adopted' = IF adopted = "old" THEN "none" ELSE adopted
           /\ UNCHANGED <<newLive, fbLive, userNew, userOld, row, cut, foreign>>
UserTakesNew == /\ ~userNew /\ ~newLive /\ userNew' = TRUE
                /\ UNCHANGED <<oldLive, newLive, fbLive, userOld, row, adopted, cut, foreign>>
UserTakesOld == /\ ~userOld /\ ~oldLive /\ userOld' = TRUE
                /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, row, adopted, cut, foreign>>
Restart == /\ adopted' = "none"
           /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
\* Spawn under the new name; a name held by anyone else falls to the fallback candidate.
SpawnNew == IF userNew \/ newLive
            THEN /\ fbLive' = TRUE /\ adopted' = "fb" /\ row' = "fb"
                 /\ UNCHANGED <<oldLive, newLive>>
            ELSE /\ newLive' = TRUE /\ adopted' = "new" /\ row' = "new"
                 /\ UNCHANGED <<oldLive, fbLive>>
\* Resolver: stored row first, probe failure defers, marker decides ownership.
Resolve(probeOk) ==
  CASE ~probeOk /\ Mode # "probe_absent" -> UNCHANGED vars
    [] Mode = "prefix_trust" /\ row = "old" /\ ~oldLive /\ userOld ->
         /\ adopted' = "userOld"
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] Mode = "name_adopt" /\ row # "new" /\ ~(row = "old" /\ oldLive) /\ userNew /\ ~newLive ->
         /\ adopted' = "userNew"
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] Mode \notin {"generator_first"} /\ probeOk /\ row = "old" /\ oldLive ->
         /\ adopted' = "old"
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] row \in {"new", "fb"} /\ Live(row) ->
         /\ adopted' = row
         /\ UNCHANGED <<oldLive, newLive, fbLive, userNew, userOld, row, cut, foreign>>
    [] OTHER -> SpawnNew /\ UNCHANGED <<userNew, userOld, cut, foreign>>
Turn == /\ adopted = "none"
        /\ \E probeOk \in BOOLEAN : Resolve(probeOk)
\* A caller still on the old builder (not migrated) re-derives the old name.
LegacyCaller == /\ Mode = "mixed_builder"
                /\ row \in {"new", "fb"} /\ Live(row) /\ ~oldLive /\ ~userOld
                /\ oldLive' = TRUE
                /\ UNCHANGED <<newLive, fbLive, userNew, userOld, row, adopted, cut, foreign>>
\* Cleanup sweep. Fixed: kills only marker-owned sessions neither row nor adopted names.
Reap ==
  \/ /\ Mode = "early_legacy_removal" /\ oldLive
     /\ oldLive' = FALSE /\ cut' = (cut \/ adopted = "old" \/ row = "old")
     /\ adopted' = IF adopted = "old" THEN "none" ELSE adopted
     /\ UNCHANGED <<newLive, fbLive, userNew, userOld, row, foreign>>
  \/ /\ Mode = "name_reap" /\ userNew /\ adopted # "userNew"
     /\ userNew' = FALSE /\ foreign' = TRUE
     /\ UNCHANGED <<oldLive, newLive, fbLive, userOld, row, adopted, cut>>
  \/ /\ \E s \in Managed : Live(s) /\ s # row /\ s # adopted
     /\ UNCHANGED vars
Next == OldEnds \/ UserTakesNew \/ UserTakesOld \/ Restart \/ Turn \/ LegacyCaller \/ Reap
Spec == Init /\ [][Next]_vars
NoDuplicate == Cardinality({s \in Managed : Live(s)}) <= 1
NoForeignAdoption == adopted \notin {"userNew", "userOld"}
NoForeignTouch == ~foreign
NoLiveCut == ~cut
==============================================================================
```

cfg는 모드마다 `CONSTANT Mode = "<모드>"`, `INIT Init`, `NEXT Next`, 불변식 하나, `CHECK_DEADLOCK FALSE`다.
