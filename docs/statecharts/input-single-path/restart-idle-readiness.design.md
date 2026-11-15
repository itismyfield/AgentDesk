# 재시작 뒤 쉬는 herdr Claude 세션의 첫 입력 (#8161 3차)

설계 기준: `land/batch-1115c` 9b42e53297(#8184 포함). 코드 좌표는 이 커밋 기준이고, 구현 레그에서 더한 새 함수·init 좌표(`observe_restored`, `idle_reading`, `launched_before_start`, `restored_screen_idle`, `dcserver.rs:969`, `evidence.rs:167-181`)만 구현 커밋 기준이다.
범위: herdr 경로(`claude/herdr_turn`)만 고친다. tmux·legacy 경로와 공유 `observe()`의 의미는 바꾸지 않는다.

## 1. 관측 (맥 release 5726d52ca8, 2026-11-15 KST, 읽기 전용)

| 대상 | 관측 | 시각(KST) |
|---|---|---|
| dcserver | 재시작 15:25:03(로그 `pending queue recovery quarantined`), 15:56:47. 현재 PID 35499 기동 15:56:06 | 16:31 확인 |
| 게임-cc Claude | PID 94111, 12:59:16 기동(`--resume a4862da2…`), herdr pane `w2T:p1`, nonce `ff64a667…` | 16:31 |
| 게임-cc transcript | 거절 두 건 시점의 끝: 15:03:25 `stop_hook_summary` → `turn_duration`(offset 3,296,131). 그 뒤 기록은 16:23:35 사용자가 TUI에 직접 친 턴이 처음이다. 즉 15:25 재시작 뒤 새 활동이 0 | 16:31 |
| 게임-cc 거절 | 15:34:56, 16:22:47 `NotSent("unproven: PreparationHold { reason: "herdr_composer_not_ready", detail: "…Refused(Unproven)", unproven: true }")` | 로그 |
| 게임-cc 큐 | `items=[]`, `head_injection=null`, `retained_inputs` 3건. 그중 2건이 이번 건(`known_not_sent: vetoed: … Refused(Unproven)`, 15:34:40·16:22:33 수신) | 16:33 |
| 게임-cc 내구 기록 | `runtime/herdr_input_holds/`에 이 nonce 없음, `discord_inflight`에 이 채널 없음 | 16:33 |
| 게임-cc pane | 빈 입력창 `❯`, 실행 표시 없음(`Worked for 43s · done 4:24 PM`), 하단 `1 shell still running`(백그라운드 셸) | 16:33:25 |
| adk-cc | 13:31:55 `Refused(Unknown)`. 직전 dcserver 재시작 12:58:34 뒤 13:31:20에 턴이 끝나 Stop 훅이 floor를 풀었다. transcript 끝은 `turn_duration` 뒤 `last-prompt`·`ai-title`·`mode`·`permission-mode`·`session-owner`·`atis-latch`·`pr-link`·`frame-link` | 로그·transcript |
| adk-cc pane | (`w2P:p1`, agentdesk cwd) 지금은 실행 중(`Running 1 shell command…`) | 16:33:25 |

같은 사유(`Refused(Unproven)`)가 11-14 22:29 캠핑-cc·최고의셰프-cc에서도 났다. 재시작 뒤 쉬는 채널에서 공통으로 일어난다.

## 2. 원인 확정

1. `readiness/mod.rs:68` `LEDGER`는 프로세스 메모리에만 있다. dcserver를 재시작하면 원장이 비고 `STARTED`(`:64`)가 새 시각이 된다.
2. 재시작 뒤 첫 `observe`(`:86`)는 원장 항목이 없어서 `recovery = (현재 transcript 길이, STARTED)`로 항목을 만든다(`:136-142`).
   훅 경로도 같은 floor를 만든다(`:517-520`).
3. `settled`(`:192-221`)의 `recovered`(`:197`)는 `tail.activity > floor`를 요구한다. 쉬는 pane은 floor 뒤에 아무것도 쓰지 않으므로 계속 거짓이다.
   `resumed`(`:223`)는 이번 실행이 받은 `SessionStart(resume)`이 있어야 하는데, Claude는 재시작되지 않았으므로 그 훅이 없다.
   floor를 푸는 경로는 이번 실행에 발행된(`published >= STARTED`) 비-continuing Stop(`:587-603`)과 새 활동뿐이다.
4. 그래서 `composer.rs:122` 대기가 10초 예산을 다 쓰고, `Watched::unproven`(`readiness/mod.rs:265`)이 참이 되며(바이트가 그대로이고, Idle이고, 예약·continuation·compact가 없음) `composer.rs:164`가 `Refused(Unproven)`을 낸다.
   `first_input.rs:172`가 이것을 retained로 넘긴다(815dbe7e82의 1차 수리).
5. 사람이 TUI에 직접 입력하면 새 활동과 Stop이 생겨 floor가 풀린다. 16:23:35에 풀린 것과 맞는다.

가설이 맞다. 원 설계 `transcript-readiness.design.md:195`는 "daemon만 재시작하고 provider는 살아 있으면 새 Start를 기다리지 않고 검증된 current source의 완료 턴에서 R을 재구성한다"고 적었다.
하지만 구현은 floor 뒤의 새 활동을 요구하고, `:234-237`은 이 조합을 "진행성 미증명 경계"로 남겼다. 이번 결함은 그 남은 경계가 운영에서 드러난 것이다.

**adk-cc 판정: 다른 원인.** 그 시점에는 재시작 floor가 이미 풀려 있었다. 막은 것은 transcript 끝의 메타 레코드다. `evidence.rs:126-129`와 `:179-181`이 모르는 레코드를 `End::None`으로 두고, `permission-mode`(`:165-169`)는 terminal을 0으로 돌린다.
그래서 hard terminal이 없고 `settled`가 거짓이다. 재시작과 무관하게 쉬는 세션에서 난다. 구현 레그에서 함께 고쳤다(§5.4).

## 3. recovery floor가 막는 위험

| 위험 | 재시작 뒤 원장이 모르는 것 | 근거 |
|---|---|---|
| R1 재시작 직전에 보낸 입력이 아직 transcript에 없음 | 우리 `pending` 예약과 `submitted` | `mod.rs:338`, `:395`; 설계 `:70` "restart도 pre-key 경계" |
| R2 continuation latch 유실(Stop이 feedback으로 막힌 직후) | `continuations`, `final_stops`(`:57-58`) | 설계 `:226-237` UnknownAfterRestore |
| R3 실행 중인데 기록이 늦는 턴(사람의 Enter, UserPromptSubmit 훅 대기, provider continuation) | 기록 전 활동 | 설계 `:418` "unreported native key 창" |
| R4 사람이 쓰던 초안 | 없음(화면으로만 보인다) | `composer.rs:129-141` D guard |
| R5 새 incarnation(herdr server 재시작, 넘김, 호스트 재부팅)의 시작 전 화면 | 이번 실행의 Start | 설계 `:176-180` |

floor는 이 위험을 모두 "새 활동이 증명할 때까지 기다림"으로 덮는다. 위험은 막지만 쉬는 세션은 영원히 증거를 못 낸다.

## 4. 수리안 비교

| | (A) 재시작 floor를 결합 증거로 충족 | (B) 원장 일부 영속화 |
|---|---|---|
| 흐름 | 재시작 floor가 서 있고 새 활동이 없을 때, 아래 §5의 transcript·incarnation 조건이 맞으면 herdr 전용 `observe_restored`가 restored proof를 낸다. 호출자는 같은 대기 루프 안에서 화면 조건을 확인한 뒤 기존 `reserve`·`sent`·`not_sent`로 진행한다 | `pending`·`continuations`·`recovery`·마지막 증명 terminal을 source별 파일로 쓰고, 부팅 때 복원한다 |
| 비용 | prod 3파일, 순증 약 +70. 새 저장소 없음 | 새 디스크 형식, 쓰기 지점 7곳 이상(reserve·not_sent·sent·note_hook 세 갈래·compact), 부팅 복원, 손상·버전 처리. prod +300 이상 |
| 위험 | 화면 판정에 기댄다(R3). 화면 표시가 늦는 창은 기존 Idle 경로와 같은 잔여다 | 설계 `:162` "proof를 serialize하거나 재시작에 ready=true로 복원하지 않는다"와 정면으로 충돌한다. 영속 기록과 실제 pane이 어긋나면(사람의 Enter, crash 중간) 거짓 ready를 복원한다 |
| 판정 | **채택** | 기각(새 저장소이고 원 설계 위반) |

## 5. 안 A 상세

### 5.1 restored 증거 (모두 성립할 때만)

transcript와 원장(`readiness/mod.rs` 안, 새 함수 `observe_restored`):

- T1 `entry.recovery = Some((floor, at))`이고 `at == *STARTED`: 재시작 floor다. 파일 교체 floor(`:147`, `Utc::now()`)는 제외한다.
- T2 `tail.end == End::Hard`이고 `tail.activity <= tail.terminal <= floor`, `tail.barrier <= floor`: floor 앞의 마지막 의미 레코드가 hard terminal이고 그 뒤에 활동도 재시작 뒤 `permission-mode`도 없다. 무시 레코드(§5.4)는 `End`를 바꾸지 않는다.
- T3 공유 observer Idle(`native_idle`, 구현 `readiness/mod.rs:103` `idle_reading`: 공유 observer가 Idle이거나, Unknown이면서 그 이유가 hard terminal 뒤의 `permission-mode`뿐일 때), `entry.pending`·`compact_ticket`·`compact_done` 없음, `entry.start` 거짓, continuation(`blocked`)·unread continuation 없음. 기존 조기 반환(`:104-119`)이 그대로 걸러 준다.
- T4 incarnation이 재시작보다 오래됐다: `Source.nonce`의 `execution_context.created_at < *STARTED`(구현 `mod.rs:112` `launched_before_start`, 원장 잠금 전에 읽는다). nonce가 없으면 거부한다.
  `STARTED`는 `dcserver.rs:969` `readiness::initialize()`가 런타임 생성·모든 launch·복구 worker보다 먼저 고정한다. 늦게 고정하면 재시작 직후 복구가 띄운 새 incarnation이 `created_at < STARTED`로 보여 T4를 통과한다.
  herdr server 재시작·넘김·호스트 재부팅의 새 incarnation은 이 조건으로 빠지고, 기존 Start(resume) 경로를 기다린다(R5).
- T5 `Stamp` 재확인(기존 `:176`). `Source::current`와 clear 재확인(기존 `:241-243`).

화면(호출자, `herdr_turn/composer.rs`):

- S1 `composer.rs:97` `restored_screen_idle(pane)`. ANSI를 지우고 `normalize_prompt_readiness_panel_in_capture`로 정규화한 화면에서 넷이 모두 참일 때만 참이다.
  1. `composer_owner(pane) == ComposerOwner::Empty`(초안 파서, D guard와 같은 것)
  2. `!tmux_capture_indicates_claude_tui_interactive_modal`(modal 파서)
  3. `tmux_capture_indicates_claude_tui_ready_for_input`(prompt marker 파서)
  4. `!tmux_capture_indicates_claude_tui_busy`(실행 표시 파서: spinner `✽ … (… esc to interrupt)`·`running UserPromptSubmit hooks…` 등)

  기존 `observation()`은 쓰지 않는다. 그 함수는 화면에 남은 MCP 인증 배너(`needs authentication`)를 거부 사유로 읽는데, 그 배너는 쉬는 pane에 계속 남아 이 사고 화면에서도 거부된다. S1은 위 네 파서만 다시 쓰고 새 화면 파서는 만들지 않는다.
- S2 기존 D guard(`:129-141`)는 그대로 먼저 돈다. 사람 초안이면 `Break(None)`이다.

내구 기록(기존 gate가 readiness보다 먼저 막는다. 새 읽기는 없다):

- D1 herdr input hold: `herdr_turn.rs:264,268` `not_held`가 첫 키 전 hold가 남은 nonce의 턴을 거부한다. hold는 `hold()`(`:572`)로 fsync되고 `composer_settled`(`:587`) 뒤에만 지워진다.
- D2 V2 `head_injection`: owner는 single-flight다. 부팅 복구(`queued_recovery.rs:21` load, quarantine)가 미정산 attempt를 정산하기 전에는 새 front를 열지 않는다.
- D3 이전 프로세스의 inflight 행: owner는 front를 열기 전에 `provider_dispatch/owner_input.rs:207` `external_turn_hold_for_start()`로 내구 보유자를 읽고(`start_waits` `:723`도 같은 판정), herdr 행이면 `recovery_engine/herdr_row_end/owner.rs:105` `holds_rest`·`:111` `restarted_owner`가 `RestartedHerdrRow`로 잡는다. 그 동안 첫 입력 경로로 가지 않는다.
  행을 푸는 것은 row-end 정산이다. `herdr_row_end.rs:557` `idle_read`가 같은 execution의 화면을 `host_input::classify`로 `ReadySameExecution`(실행 표시·초안·modal 없음)으로 읽고, transcript 마지막 턴이 닫혔으면(`terminal_replay.rs:56` `closed_turn_read`) 입력 자신의 턴을 찾지 못해도 NoTerminal로 정산하고 행을 지운다.
  즉 재시작 직전에 보낸 입력의 턴이 아직 transcript에 없을 때, 화면이 그 턴을 보여 주는 동안은 row-end가 행을 풀지 않는다. 화면도 transcript도 그 턴을 보여 주지 않는 창이 있어야 D3이 뚫리는데, Q5 실측에서 그런 창은 없었다(§11). 모델은 이 정산을 `RowEnd`로 넣었다(§10).

### 5.2 흐름 변경

```
composer::when_idle 루프 (기존)
  proof = observe_restored(...)             // 기존 증거, 없으면 §5.1 T1-T5 (proof.restart)
  pane  = capture()
  D guard (기존)                             // R4
  modal (기존)
  if proof.restart() && !restored_screen_idle(pane) { Continue }   // 새: S1 (R3, R5 보조)
  reserve(proof)                            // restart proof면 restored 평가로 재검증
```

`observe_restored`는 공유 평가 함수 `evidence(session, path, restart_ok=true)`다. 기존 증거(settled·resumed)가 있으면 `restart=false` proof를 그대로 내므로 기존 경로와 결과가 같다. `observe()`는 `restart_ok=false`로 같은 함수를 불러 의미가 바뀌지 않는다.

- `reserve`(`:314`)는 `proof.restart`에 따라 같은 평가 함수를 다시 부르고, `restart` 값까지 비교한다. 예약 뒤에는 `pending = Some(floor)`가 서므로 같은 증거로 두 번 예약할 수 없다.
- `sent`(`:392`)·`not_sent`(`:374`)는 바뀌지 않는다. `sent` 뒤의 새 턴은 floor 뒤의 활동이라 기존 `recovered`가 받는다.
- `first_input.rs:153-171` 보류 감시 closure도 `observe_restored`로 읽고, proof가 restart면 같은 capture에 S1을 더한다. 사람 초안으로 보류된 front가 초안이 지워진 뒤 깨어나야 하기 때문이다. awaiting_clear일 때는 기존 `clear_wakeable`만 쓴다.
- `Watched::unproven`(구현 커밋 `readiness/mod.rs:322`)과 대기 예산은 바꾸지 않는다. restored 증거가 있으면 대기가 먼저 끝나고, 화면이 실행 중이면 곧 transcript가 움직여 unproven이 거짓이 된다. unproven 판정은 바이트·공유 Idle·예약·continuation·compact만 읽고 화면·restored proof는 읽지 않는다. 그래서 transcript만 늦는(`lag=transcript`) 동안 화면이 실행 중이어도 예산이 끝나면 unproven으로 retained될 수 있다. 이것은 기존 동작이고 이번에 넓히지 않는다(모델 `Unproven`, §10).
- `settled`에 한 줄을 더했다(`mod.rs:253`): resume으로 시작한 실행(`entry.resumed = Some((floor, _))`)은 floor 뒤의 활동이 있어야 그 terminal을 자기 것으로 본다. (B)로 메타 레코드가 `End`를 지우지 않게 되면서, 옛 history의 마지막 terminal이 resume 직후 실행을 settled로 잘못 읽는 것을 막는다(기존 시험 `compact_resumed_start_yields…`가 잡았다).

### 5.3 진입점 전수표 (원장 상태를 만들거나 바꾸는 모든 경로)

| 경로 | 위치 | 원장 효과 | 안 A의 영향 |
|---|---|---|---|
| observe(첫 관측) | `readiness/mod.rs:136-142` | 재시작 floor `(len, STARTED)` 생성 | T1의 근거. 변경 없음 |
| observe(파일 교체·축소) | `:143-154` | floor `(len, now)`, start·compact_done 초기화 | T1에서 제외(at ≠ STARTED) |
| observe(pending 소비) | `:230-232` | pending 해제 | 변경 없음 |
| observe_restored | 새 함수 `mod.rs:98`(공유 `evidence(.., restart_ok)`) | 원장 쓰기 없음(proof만) | 새 |
| 부팅 시각 고정 | `cli/dcserver.rs:969` `readiness::initialize()` | `STARTED` 고정 | 런타임·worker 전으로 옮김(T4) |
| transcript 꼬리 판독 | `readiness/evidence.rs:167-181` | (판독) | 메타 레코드 무시·permission-mode barrier(§5.4) |
| reserve | `:314-353` | pending·revision | restart proof 재검증 분기 |
| Reservation::not_sent | `:374-391` | pending 해제, clear release | 변경 없음 |
| Reservation::sent | `:392-397` | submitted | 변경 없음 |
| note_hook SessionStart(startup/resume/clear) | `:458-506` | start, resumed, recovery=None | floor가 없어져 restored 대상이 아님 |
| note_hook Stop/UPS 항목 생성 | `:508-521` | 재시작 floor 생성(STARTED) | T1과 같은 floor |
| note_hook UserPromptSubmit | `:552-564` | pending=Some(len) | T3에서 거부 |
| note_hook continuing Stop | `:565-577` | continuations, start=false | T3에서 거부(R2) |
| note_hook 비-continuing Stop | `:578-605` | final_stops, recovery=None(이번 실행 발행분만) | floor가 풀리면 기존 경로 |
| unread continuation | `:529-543` | unread_continuations | 조기 반환으로 거부 |
| clear 인증 복원 | `readiness/source/clear.rs:217-245`(`:238-240`) | start=true, recovery=None | 대상 아님(기존 경로) |
| clear adopt/wakeable | `source/clear.rs:330`, `:355`; `first_input.rs:100-122`, `:151-156` | clear 소비 | awaiting_clear면 restored를 쓰지 않음 |
| compact 예약·완료 | `readiness/compact.rs:181`; `compact/completion.rs:114-115`, `:150-152` | compact_ticket, compact_done | T3에서 거부. compact는 restored proof를 쓰지 않음 |
| herdr 첫 입력 | `claude/herdr_turn/composer.rs:113-167` | (호출자) | observe_restored + S1 |
| herdr 보류 감시 | `claude/herdr_turn/first_input.rs:148-171` | (호출자) | observe_restored + S1 |
| herdr Busy 재관측 | `claude_tui/host_input/busy/herdr.rs:271-289` | 예약 후 즉시 not_sent | 변경 없음. 화면이 실행 중일 때의 Busy 분류라 대상이 아님 |
| tmux 대기·초안 정리 | `claude_tui/input.rs:375-435`, `readiness/wait.rs:7` | (호출자) | 변경 없음(범위 밖). 특히 `claude_tui/input.rs:408` 초안 정리는 observe()에 묶여 있어 넓히지 않는다 |
| tmux submission | `claude_tui/input/submission.rs:154-180` | (호출자) | 변경 없음 |
| 활동 표시 | `discord/turn_presence/activity/claude_activity.rs:82` | 읽기 | 변경 없음 |
| watch_draft/current/modal | `readiness/mod.rs:625-676` | 감시 등록 | 변경 없음(tmux) |

### 5.4 (B) 턴 뒤 세션 메타 레코드 (adk-cc 원인)

`evidence.rs`가 모르는 레코드를 `End::None`으로 두고 `permission-mode`가 terminal을 0으로 돌려서, 턴이 끝난 뒤 Claude가 쓰는 세션 상태 블록이 hard terminal을 지웠다. 쉬는 세션은 그 뒤 아무것도 쓰지 않으므로 `settled`가 영원히 거짓이다.

조사(읽기 전용, 이 맥의 최근 transcript 300개, `~/.claude/projects/*/*.jsonl`): `turn_duration`/`result` 바로 뒤에 이어진 메타 레코드 블록을 셌다.

| 레코드 | 턴 끝 뒤 출현 | 무시 근거 |
|---|---|---|
| `queue-operation` | 11,456 | 이미 무시하던 것. 큐 조작 기록이고 턴을 진행하지 않는다 |
| `file-history-snapshot` | 514 | 이미 무시하던 것 |
| `last-prompt` | 543 | 이미 무시하던 것. 값은 직전 프롬프트였고(19,044건) 다음 프롬프트와만 맞는 경우는 0이다 |
| `ai-title` | 460 | 세션 제목 |
| `mode` | 454 | 세션 모드 상태 |
| `atis-latch` | 453 | 세션 상태 플래그 |
| `pr-link` | 357 | 세션에 연결된 PR |
| `session-owner` | 261 | 세션 소유자 표기 |
| `cost-state` | 130 | 누적 비용 |
| `frame-link` | 56 | 세션 링크 |
| `tools-used` | 33 | 누적 도구 목록 |
| `permission-mode` | 411 | **무시하지 않는다.** `End`·terminal은 그대로 두고 `barrier`만 옮긴다 |

- 위 11종은 `End`·terminal·activity를 바꾸지 않는다(`evidence.rs:179-181`). 블록 다음 레코드는 `user`(2,377)·`api-request*`(3,281)·예약 실행(510)·compact(24)·EOF(68)였고, 블록 안에 턴 진행 레코드가 섞인 경우는 없었다.
- `permission-mode`는 블록 안에 411번 나왔다. 하지만 resume 경로는 이것을 턴 시작 표지로 읽는다(`readiness_tests.rs` resume 사례). 그래서 hard terminal은 지우지 않되 `barrier = offset`을 남긴다(`evidence.rs:167-170`). 공유 observer(`tui_turn_state.rs:610`)가 `permission-mode`를 Unknown으로 읽는 것은 바꾸지 않고, readiness 쪽 `idle_reading`(`mod.rs:103`)이 "Unknown이고 hard terminal 뒤의 barrier뿐"이면 Idle로 본다.
- `attachment`, `api-request*`, 모르는 새 type은 지금처럼 `End::None`이다. 새 레코드 종류가 생기면 보수적으로 막는다.
- 반례 검토: 블록이 다음 턴의 시작에 붙는다면 무시가 실행 중 턴을 숨긴다. `last-prompt` 값이 다음 프롬프트와 맞는 경우가 0이고, 블록 뒤의 다음 턴은 항상 자기 `user`/`api-request` 레코드로 시작했다. 이 레코드들은 지금처럼 `End::None`을 만든다(시험 `turn_records_after_the_metadata_still_run`).
- hard terminal의 의미는 바꾸지 않으므로 상태 차트는 고치지 않는다(모델의 `tail="hard"`는 "마지막 의미 레코드가 terminal"이고, 메타 레코드는 의미 레코드가 아니다).

## 6. 재시작·부팅·incarnation 경로

| 경우 | 원장 | pane·incarnation | 결과 |
|---|---|---|---|
| dcserver만 재시작(이번 사고) | 비움 → 재시작 floor | 같은 nonce, created_at < STARTED | T1-T5 + S1이면 입력을 받는다 |
| 호스트 재부팅 | 비움 | dcserver가 새 incarnation을 resume으로 띄운다(created_at ≥ STARTED) | T4로 제외. SessionStart(resume)가 오면 기존 `resumed_quiet`. Start를 잃으면 지금처럼 retained |
| herdr server 재시작(dcserver 생존) | 옛 nonce 항목이 남지만 Source가 바뀐다 | 새 nonce, created_at ≥ STARTED | T4로 제외. 새 Start 경로 |
| 넘김 직후(tmux→herdr, herdr 넘김) | 넘김은 새 launch와 SessionStart를 만든다(`claude/herdr_turn/handoff.rs`) | 새 nonce | 넘김 뒤 dcserver를 재시작하지 않았다면 Start 경로. 넘김 뒤 재시작이면(게임-cc: 12:59 넘김, 15:25 재시작) 그 incarnation은 재시작보다 오래됐으므로 restored 대상 |
| 넘김 도중 재시작 | `unfinished_handoff` 저널(V2) | 넘김 소유자가 정산 | D2와 같은 single-flight로 첫 입력 전 정산 |
| 부팅 순서 | `STARTED`는 `dcserver.rs:969` `readiness::initialize()`에서 고정. 그 뒤에 `Runtime::new()`·모든 spawn·launch·복구 worker | 재시작 직후 복구가 띄운 incarnation은 created_at ≥ STARTED | T4로 제외(시험 `the_boot_clock_starts_before_any_new_incarnation`이 새 프로세스에서 init→launch는 거부, launch→init은 통과로 순서를 가른다) |

## 7. 인접 경로(같은 상태를 소비)

| 경로 | 영향 |
|---|---|
| Cancel(`InputRun::Cancelled{0}`) | `not_sent`가 pending을 풀고, restored 증거가 다시 성립하면 다음 front가 진행 |
| Terminate·pane 소멸 | `Source::current`·capture 실패로 restored 불가. 기존 lifecycle 소유 |
| /clear | SessionStart(clear) 또는 clear 인증 복원이 floor를 없앤다. awaiting_clear 첫 입력은 restored를 쓰지 않음 |
| /stop(Esc) | interrupt 레코드가 `End::Hard`(`evidence.rs:148-157`). 재시작 뒤라도 T2를 만족한다 |
| compact | T3(compact_ticket·compact_done)으로 거부. compact 트리거는 restored proof를 쓰지 않는다(재시작 직후 compact는 지금처럼 새 활동을 기다림) |

## 8. 입력 출처 분류

| 출처 | 경로 | 이중 투입·덮어쓰기 방지 |
|---|---|---|
| Discord 입력 | owner front → `first_input::write` → `composer::when_idle` | restored 증거 + S1, 예약 single-flight |
| 재시작 전 보낸 미정산 입력 | V2 `head_injection`, herdr hold, inflight | D1-D3이 readiness보다 먼저 막는다. 같은 원본은 부팅 복구가 정산하고 다시 보내지 않는다 |
| 사람의 TUI 초안 | 화면만 | D guard(R4)가 S1보다 먼저 `Break(None)`, 보류 감시가 초안이 지워지면 restored 증거 + S1로 깨움 |
| 사람의 Enter, UPS 훅 끝나기 전 | 화면: `✽ … (running UserPromptSubmit hooks… 1/2 · 2s)`. transcript: 아직 없음 | S1의 busy 파서가 막는다(Q5, `HOOK_RUNNING` fixture). UPS 훅 timeout은 5초다 |
| 사람의 Enter, UPS 훅 뒤 | transcript에 `user` 레코드, 화면에 spinner | T2(terminal 뒤 활동)가 막고 S1도 막는다 |
| 예약 프롬프트(routine·scheduled) | 같은 owner front | Discord 입력과 같다. #8184 예약 반환은 reserve 의미를 바꾸지 않는다 |

## 9. 남는 경계와 후속

- **화면 표시 지연(Q5에서 해소)**: Claude Code 2.1.325는 UserPromptSubmit 훅을 기다리는 동안 `running UserPromptSubmit hooks…` spinner를 내고, transcript는 훅 뒤에 쓴다. 그래서 화면과 transcript가 둘 다 턴을 숨기는 창은 실측에서 없었다. 이 표시가 바뀌는 Claude 판에서는 S1이 이 창을 못 볼 수 있다(모델 `LateBlind` RED가 그 경우다).
- **check-to-key 창**: S1과 첫 키 사이에 사람이 Enter를 치는 창은 기존 Idle 경로와 같다.
- **continuation과 turn_duration 순서 [U]**: Stop이 막힌 직후 continuation 전에 `turn_duration`이 먼저 쓰이면 T2가 잠깐 참일 수 있다. 그때 provider는 실행 중이라 S1이 막는다(모델 `RunningIgnoreScreen`).
- **adk-cc 원인(메타 레코드 꼬리)**: 이번에 같이 고쳤다(§5.4). `permission-mode`는 barrier로 남긴다.
- **백그라운드 셸 표시(Q6에서 해소)**: 게임-cc pane의 `1 shell still running`은 busy 파서가 실행 중으로 보지 않는다. S1을 통과한다.
- **unproven 예산과 transcript 지연**: §5.2. 화면이 실행 중이고 transcript만 늦는 동안 예산이 끝나면 retained될 수 있다. 기존 정책이고 넓히지 않는다.
- **Q4 통합 시험**: 복구 엔진 전체(inflight 행 복원 → row-end 정산 → owner 깨움 → restored 첫 입력)를 한 시험으로 돌리지는 않았다. 근거는 각 경계의 시험(S1의 hook-running 거부, restored 예약 single-flight)과 모델 `RowEnd`/`LateBlind`다. 후속 후보.

## 10. 상태 차트

`ClaudeReadiness.tla`에 #8161 restart 모드를 넣었다(`InputPath.tla` `ReadinessSpec`, N=2). 결과는 `restart-idle-readiness.report.md`에 있다.

- 행동: `Restart`(원장 초기화, 재시작 floor, handoff면 새 incarnation), `PreRestartSend`(재시작 전 송신, 내구 기록이 남고 transcript가 늦음; `LateBlind` 변이에서만 화면도 늦음), `RowEnd`(D3 정산: 화면이 같은 execution에서 idle이고 마지막 턴이 닫혔으면 inflight 해제), `PersonLateSubmit`(사람의 Enter, 기록만 지연), `LateRecord`, `LostStart`, 사람 초안(`HumanClears`: 초안만 지우고 보류 front는 깨우지 않는다), `WatchHeld`(보류 감시: restored 증거 + 화면으로 보류 front를 깨운다. `DraftNoWatch` 변이는 이 감시를 뺀다).
- `Unproven`은 코드 `Watched::unproven`이 읽는 것만 쓴다(Booting 아님, 공유 Idle, 현재 source, continuation·compact fence 없음, 현재 증거 없음). 화면·restored proof·provider phase는 읽지 않는다. 그래서 `lag="transcript"`인 동안의 만료도 모델에 들어 있다.
- 규칙: `ReadyNoIdleRunningInjection`, `ReadyNoDuplicate`(실행 중·미정산 위 투입 0), `ReadyNoHumanDraftExecution`(초안 덮어쓰기 0), `ReadyNoEarlyInput`, 새 liveness `ReadyRestartIdleEventuallyReceives`.
- 값 범위는 §5.3 전수표의 상태만 쓴다: `restored ∈ {no, floor, passed}`(floor 생성 `:140`·`:518`, 해제 `:502`·`:601`, 새 활동), `lag`(기록·화면 지연), `inflight`(D1-D3), `oldIncarnation`(T4).

## 11. 시험 계획(구현 레그)

| ID | 시험 | 위치 | 기대 |
|---|---|---|---|
| Q1 | 재시작 floor + transcript 끝 `turn_duration` + 오래된 incarnation + 빈 화면 | `readiness_tests.rs` (`daemon_restart` 도우미), `composer_tests.rs` | 키 1회, pending 생성, 두 번째 restored 예약 거부 |
| Q2 | Q1에서 created_at ≥ STARTED(새 incarnation) | 같음 | 키 0, 지금처럼 Unproven retained |
| Q3 | Q1에서 화면에 사람 초안 / modal / spinner | `composer_tests.rs` | 초안·modal은 키 0 + 보류 감시. 초안이 지워지면 감시가 깨워 키 1 |
| Q4 | 재시작 시 inflight 기록이 있는 채널 | `first_input/unproven_tests.rs` 확장 또는 recovery 시험 | 복구된 턴이 끝날 때까지 첫 입력 경로로 가지 않음 |
| Q5 | [U] UPS 훅 대기 중 pane 표시 | 격리 실측(release 아님) | 실행 표시가 있으면 S1이 막음. 없으면 §9 잔여로 기록 |
| Q6 | 백그라운드 셸 화면(관측 fixture: `✻ Worked for 43s · done 4:24 PM · 1 shell / still running`) | `composer_tests.rs` | S1 통과(포그라운드 실행 아님) |
| Q7 | 파일 교체 floor(`at ≠ STARTED`) | `readiness_tests.rs` | restored 미적용 |
| Q8 | continuation fence·unread continuation·compact_done이 남은 경우 | `readiness_tests.rs` | restored 미적용 |
| Q9 | 기존 `a_timed_out_hold_tells_an_unmoved_idle_source_from_a_running_turn`, `an_idle_source_that_proves_nothing_ends_retained_and_a_running_one_waits` | 기존 | Restarted 경우는 created_at을 STARTED 앞으로 두면 이제 전달된다. 새 incarnation 경우를 Retained 사례로 추가 |
| Q10 | tmux 경로 회귀 | 기존 `input.rs` 시험 | 변경 없음 |

### 11.1 구현 레그 결과

| ID | 결과 | 시험 |
|---|---|---|
| Q1 | 키 1회, 두 번째 예약 거부, `not_sent` 뒤 다시 성립, 우리 턴 terminal이 floor를 넘김 | `restart_tests::a_launch_older_than_the_restart_is_ready_on_its_last_hard_terminal`, `composer_tests::a_pane_that_outlived_the_restart_takes_one_input_only_on_an_idle_screen`(idle·MCP 배너·백그라운드 셸 = 전달) |
| Q2 | 새 incarnation·교체된 파일·다른 source는 restored 아님 | `restart_tests::a_new_incarnation_or_a_moved_source_never_rests_on_the_restart_floor`, composer "new incarnation" = `Refused(Unproven)` |
| Q3 | 초안·modal = `Refused(Unknown)`(키 0), hook-running·turn-starting spinner = `Refused(Unproven)`(키 0). 초안 보류 front는 초안이 지워지면 감시가 깨워 키 1 | composer 같은 시험, `unproven_tests::a_restored_front_held_by_a_draft_wakes_once_the_draft_clears` |
| Q4 | 코드 근거 §5.1 D3. row-end는 화면이 같은 execution에서 `ReadySameExecution`일 때만 inflight를 푼다. Q5로 화면이 그 턴을 보여 주므로 미기록 송신 위에 넣지 않는다. 모델 `LateAfter` GREEN, `LateBlind` RED. 복구 엔진 전체 통합 시험은 쓰지 않았다(§9) | composer hook-running 거부, 모델 |
| Q5 | Claude Code 2.1.325, 격리 pane(운영 아님): 훅 대기 중 `✽ Marinating… (running UserPromptSubmit hooks… 1/2 · 2s)`, 훅 뒤 `✽ Fiddle-faddling…`. busy 파서가 둘 다 실행 중으로 읽는다. DESIGN_DELTA 없음 | composer `HOOK_RUNNING`, `TURN_STARTING` fixture |
| Q6 | `1 shell still running`은 busy 아님, 전달 | composer `BACKGROUND_SHELL` fixture |
| Q7 | 파일 교체 floor 미적용 | Q2 시험 |
| Q8 | assistant·prompt·summary만·재시작 뒤 permission-mode·continuing Stop이면 restored 아님 | `restart_tests::anything_after_the_terminal_or_an_open_fence_keeps_the_floor` |
| Q9 | 기존 Restarted 사례를 오래된 incarnation(전달, `Case::RestartedOld`)과 새 incarnation(retained)으로 나눴다 | `unproven_tests::an_idle_source_that_proves_nothing_ends_retained_and_a_running_one_waits` |
| Q10 | 공유 `observe()`·tmux 경로 무변화 | 기존 `claude_tui::readiness` 시험 |
| (B) | adk-cc 꼬리 = hard terminal 유지, 꼬리 뒤 턴 레코드 = 실행 중 | `restart_tests::a_turns_trailing_session_metadata_keeps_its_hard_terminal`, `restart_tests::turn_records_after_the_metadata_still_run` |

## 12. 크기

- prod: `claude_tui/readiness/mod.rs`, `claude_tui/readiness/evidence.rs`, `claude/herdr_turn/composer.rs`, `claude/herdr_turn/first_input.rs`, `cli/dcserver.rs`(init 1줄; 순서 시험 포함). 5파일. 측정치는 구현 보고서의 `pr_cap_prod.py` 값을 따른다.
- `pr_cap_prod.py` 상한(30파일 / +1500)과 +1200선 아래이므로 분할하지 않는다.
