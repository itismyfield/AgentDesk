# #8328 1단계 설계 (w2): owner 입력 바쁜 중 주입 — 답 anchor 귀속을 핵심으로, Claude는 합류, Codex는 항상 Tab

- 기준 코드: `origin/main` = `82702fe252`. 브랜치는 `wt/i8328-design`이다(w1 `cafed27b40` 위에 이어서).
- 결정 출처:
  - 사용자 결정 busy-inject-step0-1126 a (13:01)
  - 메인 판단·코디 결정 (13:20): Codex는 항상 Tab, 1단계 핵심 범위는 귀속
  - 입력: 설계 준비 보고서, 0단계 실측(`report-e2e-1.md`), Codex Tab 실측(`report-tab-1.md`, `logs/tab-1/`), 코디 요약(`coord-note-tab-for-design.md`), 이슈 #8328
- 운영 코드는 바꾸지 않는다. 바뀌는 것은 문서·모델·cfg·보고서·색인 행뿐이다. cargo는 0회다.
- 약어. 경로는 `src/services/discord/` 기준이다.
  - OI = `router/message_handler/provider_dispatch/owner_input.rs` (1795줄, giant)
  - OB = `router/message_handler/intake_turn/owned_busy.rs` (813줄)
  - IG = `router/intake_gate.rs` (1419줄, giant)
  - IT = `router/message_handler/intake_turn.rs` (2262줄, giant)
  - V2 = `src/services/turn_orchestrator/queued_head/v2*`
  - TW = `src/services/tui_o/writer/`
  - HF = `src/services/tui_input/human_frame/`

## 0. 한 줄 요약

1. **귀속(핵심)**: 게시하는 답은 모두 입력 anchor(사람이 보낸 Discord 메시지) 하나에 reply로 건다.
   - 어느 anchor에 걸지는 transcript에서 그 답이 속한 native 턴과, 그 턴을 연(또는 그 턴에 합류한) 입력의 frame nonce로 정한다.
   - 게시 순서, mailbox 토큰, 주입한 경로로는 정하지 않는다.
   - 합류 입력은 합친 답 한 번으로 친다. 답은 그 턴을 연 입력의 anchor에 걸고, 합류한 입력에는 ➕ '진행 중 턴에 합류' 표시를 단다.
2. **Claude**: 바쁜지 확인하지 않는다. 보류 조건만 보고 붙여넣기+Enter를 한다. 다음 도구 경계에서 흡수되면 합류, 아니면 자기 턴이다.
3. **Codex**: 바쁜지 확인하지 않고 **항상 Tab**이다. 절차는 붙여넣기 → 화면 반영 확인 → Tab → 수락 확인(대기 항목 또는 새 턴 사용자 입력)이다. Tab의 뜻을 확인하지 못한 화면(§2.4 표)에서는 보관한다.
4. echo가 아직 없다는 사실은 재전송 근거가 아니다(w1 그대로).

## 1. 상태 차트 (0단계)

모델은 `docs/statecharts/input-single-path/OwnerInputBusySplit.tla`이고, cfg는 w1의 20개에서 24개로 바뀌었다. 보고서는 `owner-input-busy-split.report.md`다.

w2에서 들인 행동:
- **Codex 항상 Tab**: w1의 `ObserveCodex`·`KeyCodex`(바쁨 읽기 → 키 고르기)를 지웠다. 대신 세 동작을 둔다.
  - `PasteCodex`: 보류 조건 확인 → 붙여넣기 1회(키 효과 1)
  - `Reflect`: 입력창에 frame이 보임
  - `KeyCodex`: Tab. idle 화면은 제출, 바쁜 화면은 대기열 수락이고, 어느 쪽이든 B는 A 뒤 자기 턴으로 돈다.
  - 상수 `TabAtIdle`·`UncertainKey`·`Unknowable`과 변수 `seen`·`unk`를 없앴다.
- **답 anchor 귀속**: 변수 `aTo`·`bTo`(각 답이 reply로 건 anchor)와 시나리오 `external`(AgentDesk가 시작하지 않은 턴 A: anchor 없음, 토큰 없음)을 더했다.
  - 규칙 `AnswerBound`: A 답은 `ATarget`(A에 anchor가 있으면 A, 없으면 합류한 B, 합류한 입력도 없으면 none)에 건다. B의 자기 답은 B에 건다.
- 지킨 규칙: `NoDuplicate`·`NoSilentLoss`·`ReplyExactlyOnce`(합류 입력은 합친 답 한 번)·`JoinedMarked`·`AnswerBound`·`KeyBeforeLeave`. liveness `IdleDelivers`·`ReplyPosted`는 그대로다.

`statecharts_tlc.check_cfg`로 24개 cfg를 직접 돌린 결과(전부 기대 집합 일치):

| cfg | 결과 | 뜻 |
|---|---|---|
| `Current8432` | **RED** `IdleDelivers`·`ReplyPosted` | 현재 경로: 남은 토큰 + Idle 화면 → hand_busy 보류 반복 |
| `CurrentBusy` | GREEN | 현재 경로, A 정상 종료 |
| `Target8432`·`TargetBusy`·`TargetExternal` | **GREEN** | Claude 목표 경로: 남은 토큰, 바쁜 턴, 외부 턴 모두 성립 |
| `TargetHoldCleared`·`TargetComposerNotReady` | GREEN | 보류가 풀리면 전달 |
| `TargetDraftStuck` | RED `IdleDelivers`·`ReplyPosted` (U1로 수용) | 사람 초안이 끝내 남으면 알림만 |
| `TargetDraftStuckCap` | GREEN | U1이 채택하지 않은 대조군 |
| `CodexTargetBusy`·`CodexTarget8432`·`CodexTargetExternal`·`CodexComposerNotReady` | **GREEN** | Codex 항상 Tab |
| `TargetTokenAttribution` | mutant RED `ReplyExactlyOnce`·`ReplyPosted` | 토큰 보유자에게 귀속 |
| `TargetResendBeforeEcho` | mutant RED `NoDuplicate` | echo 전 재전송 |
| `TargetTuiDrop`·`CodexTuiDrop` | mutant RED `NoSilentLoss`·`ReplyPosted` | TUI가 입력을 버림 |
| `TargetJoinUnmarked` | mutant RED `JoinedMarked` | 합류 표시 누락 |
| `TargetJoinDoublePost` | mutant RED `ReplyExactlyOnce` | 합류 몫을 따로 한 번 더 게시 |
| `TargetPostOrder`·`CodexPostOrder` | mutant RED `AnswerBound` | **게시 순서로 귀속**: 외부 턴 A의 답이 키를 누른 B에 걸린다 |
| `TargetNoReference` | mutant RED `AnswerBound` | **reply reference 없이 게시** |
| `CodexAlwaysEnter` | mutant RED `NoSilentLoss` | 항상 Enter: 바쁜 턴 steer로 A 작업 유실 |
| `CodexNoReflectCheck` | mutant RED `ReplyPosted` | 반영 확인 없이 Tab: B가 초안에 남음 |

정리:
- Current8432는 RED, Target은 GREEN이다.
- mutant 11개가 RED다. 그중 귀속 mutant 3개(게시 순서 ×2, reply reference 없음)가 브리프의 요구다.
- 그 밖의 expected RED는 2개(Current8432, DraftStuck)다.

차트(`docs/statecharts/input-single-path/`):
- `owner-input-busy-split.current-8432.chart.mmd` (exit 1)
- `….target-8432`, `….target-busy-join`, `….codex-target-busy`, `….codex-target-external` (각 exit 0)
- `….codex-always-enter`, `….target-post-order` (각 exit 1)

모델의 한계: 입력 하나와 앞 턴 하나다. 두 입력이 한 턴에 합쳐지는 경우(실측 불일치 b·c, §6.4)는 §6의 "한 턴에 anchor 여럿" 규칙과 조각 A1 시험이 맡는다.

## 2. 목표 경로

### 2.1 owner drive (OI:178 handoff 안)

```
restart_gate(OI:190-199) → writer boot(OI:200-223) → claude_handoff::prepare(OI:235-241)   [그대로]
match origin:
  기계(루틴·예약·HTTP·Decision·effort·restart seed): 그대로(START_WAITS / hand_idle)   [U2]
  사람(Discord·iMessage·허용 봇): inject::hand(front)                                 [새 child]
```

`inject::hand`(새 child `router/message_handler/provider_dispatch/owner_input/inject.rs`)의 순서:
1. 전이 잠금 `session_transition_locks.try_lock_owned()`. 키를 다 누를 때까지 쥔다. 실패하면 `session_transition_busy`. 지금은 OI:264-269, 280에 busy 가지 전용으로만 있다.
2. 보류 조건(§2.2). 걸리면 `NotSent(<사유>)`이고 front는 남는다.
3. 장부 고르기. 이것은 관문이 아니다.
   - 토큰이 있으면 **합류 장부**: front claim, 키 전에 `record_native_busy_child`(OI:463-471)
   - 토큰이 없으면 **자기 턴 장부**: 기존 `decision_drain::hand_idle` → `start_idle`(OI:943)
4. 키 효과
   - Claude: 기존 `BusyClaude`(`claude_tui/host_input/busy/herdr.rs:28-140`)
   - Codex: §2.3 Tab 절차
5. 귀속 대기 항목(§5.1)에 nonce·anchor를 기록한다. 답 anchor는 §6이 정한다.

### 2.2 남는 보류 조건

| # | 조건 | 판정 위치 | 해소·wake | #8430 상한 |
|---|---|---|---|---|
| H3 | 세션 없음 | `Phase::Absent` → `Route::Cold`(`queue_io/input_owner/target.rs:81`), OI:1752 | 보류가 아니라 기존 cold launch(자기 턴 장부) | 불필요 |
| H1 | 사람 초안 | Claude `busy/herdr.rs:77`, `effect/claude.rs` `draft_notice`. Codex `screen::draft_may_be_present`(`codex_tui/busy_inject/screen.rs:102`) | 사람이 보내거나 지움 → held_watch | **알림만**(U1). 기존 Claude managed stash(초안 C-s 보관·복원)는 초안을 제출하지 않으므로 현행 유지 |
| H2 | 모달·승인 창·Tab 뜻 미확인 화면 | Claude `busy/herdr.rs:40-51`. Codex `screen::delivery_veto`(`screen.rs:75-99`)와 §2.4 표 | 사람 응답 → held_watch | 불필요 |
| Hc | composer 미준비(`/clear`·재시작 직후) | Claude `claude_tui/input/preparation_hold.rs:112`. Codex `codex_tui/readiness.rs` 준비 증명. 실측 Tab 0.3s부터 24회 성공 | readiness → held_watch | 불필요 |
| Hq | (Codex) 앞 주입 수락 미확인 | 같은 채널 바로 앞 입력이 대기 항목·새 턴 사용자 입력으로 확인 안 됨(§5.1 "수락됨") | 수락 확인 → held_watch | 불필요 |
| H4 | 첨부 준비 전 | `input_payload.rs:143`, `ingress.rs` Hold::Uploads | 내려받기 완료 | 불필요 |
| H5 | 재시작 복구 중 | `ingress::ingress_hold`(`queue_io/input_owner/ingress.rs:192-198`) | R8 wake | 불필요 |
| H6 | 세션 전환·restart gate·effort restart | 전이 잠금, OI:190-199 | `Gate::Drop` | 불필요 |
| H7 | writer boot | OI:200-223 | writer-ready | 불필요 |
| H8 | 화면 경합 | effect `LockContended` | held_watch | 불필요 |
| H9 | root-replaced Herdr preclaim | `owner_input/preclaim_wait.rs:14-48`(합류 장부에도 적용) | backoff | 불필요 |
| H10 | 복구 kickoff 전 | U3로 H5에 포함(조각 A4 시험) | R8 | 불필요 |
| H11 | 중단 정산 전 | `/stop`·Cancel 뒤 인터럽트 정산 전. 근거: Claude S3D, Codex Esc 실측은 abort 기록 뒤만 측정 | stop 정산 wake | 불필요 |

사라지는 보류 사유:
- `busy_source_unavailable`(OI:455, OB:10)
- `busy_turn_unobserved`(OI:490)
- `busy_turn: transcript unavailable`(OI:1719, `codex/herdr_turn/busy.rs:74`, `codex_tui/busy_inject.rs:281-296` `turn()`)
- 토큰만 남았다는 이유의 보류 전부
- Codex의 바쁨 판정 전부(키 선택 포함)

### 2.3 Codex 항상 Tab 절차

1. composer 잠금(`try_with_composer_mutation_lock`) 안에서 capture한다. `delivery_veto`·draft·readiness·Hq를 확인하고, 하나라도 걸리면 키 없이 NotSent다.
2. 붙여넣기 1회. 키 효과는 1이고, 귀속 대기 항목을 만든다.
3. **반영 확인**: 같은 잠금 안에서 capture해 frame의 nonce 꼬리표가 composer에 보이는지와 `delivery_veto`가 여전히 None인지 본다.
   - 실패하면(반영 안 됨, 또는 붙여넣기 뒤 completion popup이 열림) 키를 누르지 않는다. `WriteUncertain`으로 끝내고 소유 초안 알림(`codex_tui::input::notify_owned_draft`)을 보낸다. 재붙여넣기는 없다.
   - 근거: Codex의 "Paste detection — Recognize pasted-text bursts and keep multiline input together"(바이너리 문구)와 실측 busy1(붙여넣은 직후 Tab → 두 건 합쳐짐·셋째 건 초안 잔류, 0/3).
4. **Tab** 1회.
5. **수락 확인**: B가 `Queued follow-up inputs` 항목(`screen.rs:22-27` QUEUE_HEADERS)으로 옮겨졌거나 rollout에 B의 사용자 표식이 나타나면 수락이다. 귀속 대기 항목을 "수락됨"으로 표시한다. 다음 front는 이를 기다린다(Hq).
6. 고정 지연은 쓰지 않는다. 실측의 0.12s는 관측치일 뿐이다.

구현 손잡이:
- `session_host::PinnedInput`(`session_host/submit.rs:56-59`)에 `Tab`을 더한다.
- `codex_tui::input::TuiInputAction`(`codex_tui/input.rs:467-472`, 3128줄 giant)에 enum 변형만 더한다.
- `codex/herdr_turn/busy.rs:125-128`의 `[PasteBuffer, Enter]` 한 번 실행을 "붙여넣기 → 반영 확인 → Tab"으로 나눈다.
- `codex_tui/busy_inject.rs:436`과 `effect/codex.rs` `idle_codex`(Enter)는 같은 절차로 합친다.
- `target::route`의 Codex 경로 넷(CodexBusy·CodexIdle·CodexHerdrBusy·Codex HerdrIdle, `target.rs:44-57,81-88`)은 host별 하나씩(tmux·herdr)으로 줄인다.

### 2.4 Codex 화면 상태별 Tab의 뜻 (근거와 처리)

근거 기호:
- 실측: i8328tab1
- 바이너리: Codex CLI 0.172.0 실행 파일(`@openai/codex-darwin-arm64/vendor/aarch64-apple-darwin/bin/codex`)의 `strings`. 소스는 로컬에 없다.
- 코드: AgentDesk 화면 판정 코드

| 화면 상태 | Tab의 뜻 | 근거 | 처리 |
|---|---|---|---|
| idle, 붙여넣은 frame만 있음 | 제출 | 실측 6/6 | **Tab** |
| 도구 실행 중 | 대기열(queue follow-up) | 실측 15/15. 바이너리 "Queue the draft while a task is running.". 상태줄 `tab to queue message`(`screen.rs:12`) | **Tab** |
| 도구 없이 생성 중 | 대기열 | 실측 4/4 | **Tab** |
| task_complete 직전·직후 | 앞 답 보존, 별도 턴 | 실측 8/8 | **Tab** |
| Esc 직후(abort 기록 뒤) | 제출, 별도 턴 | 실측 4/4(116-197ms). abort 기록 전은 미측정 | H11 정산 뒤 **Tab** |
| `/clear` 직후 | 제출 | 실측 0.3s부터 24회 | Hc readiness 뒤 **Tab** |
| 빈 composer | 무효(새 턴 없음) | 실측 키별 2/2 | 반영 확인이 실패하므로 키를 누르지 않음 |
| slash 명령 popup(composer가 `/`로 시작) | 명령 자동완성 | 바이너리 "Type `/` to open the command popup; `Tab` autocompletes slash commands.". frame은 `[`로 시작해 열리지 않는다(`HF.rs:102-115` `plain_inject`) | **보관**(`completion_popup` veto, `screen.rs:85-99`) |
| 파일 mention 등 제안 메뉴(`@…`) | 선택 항목 삽입 | 코드 `screen.rs:97` 힌트 "enter/tab insert · esc close". 바이너리 "only suggestion menus use above-composer placement" | 붙여넣기 뒤 열리면 반영 확인에서 거부하고 키 없이 WriteUncertain(Esc는 바쁜 턴을 끊으므로 쓰지 않음) |
| 승인·권한 모달, 번호 선택 메뉴, "press enter to continue" | 모달 조작(Tab 뜻 미확인) | 코드 `codex_tui/input/submission.rs:236` `submission_modal_in_pane`, `input/inline_banner.rs:12` `pane_has_blocking_action_menu`. 바이너리 승인 동작 목록 | **보관**(veto) |
| 닫을 수 있는 action banner(빈 composer에서 숫자·Esc를 받음) | 미확인 | 코드 `inline_banner.rs:5-10` | **보관**. `delivery_veto`가 이 banner를 포함하지 않으므로 A5에서 더한다 |
| request_user_input 질문 화면 | 질문 이동("Move forward through questions…") | 바이너리 | **보관**. 화면 문자열 판정은 A5에서 fixture로 더한다(미확인 → 보관) |
| transcript overlay·review picker | overlay 조작 | 코드 `screen.rs:15-21` MODAL_FOOTERS | **보관**(veto) |
| Vim composer 모드(normal) | 붙여넣기와 키가 명령으로 해석될 수 있음 | 바이너리 "Turn Vim composer mode on or off", "Start in Vim mode" | **보관**(composer 모드 판정을 못 하면 보관). launch 설정에 Vim 시작이 없는지는 운영 설정을 읽지 않는 범위에서 확인 못 함 |
| side conversation·sidebar에 포커스 | 메인 composer가 아닐 수 있음 | 바이너리 "Focus the side conversation…" | **보관**(메인 composer `›` 행을 못 찾으면 `Composer::Unread`) |
| 사람 초안 있음 | 초안과 합쳐 제출·대기 | 코드 draft 판정 | H1 보관 |

**결론: codex_key = 항상 Tab.** 확인한 상태에서는 모두 Tab이 맞고, 나머지는 키 시점 거부로 보관한다. 바쁨 판정은 키 선택에도 필요 없다.

## 3. 지울 분기 (file:line 재대조)

| 위치 | 내용 | 처리 |
|---|---|---|
| OI:249-260 | `has_active_turn` + `external_turn_hold_for_start` | 사람 입력에서는 읽지 않는다. 기계 입력 판정용으로만 남긴다 ◆ |
| OI:263-294 | 사람 입력 busy/idle 가지 | `inject::hand`로 블록 이동 |
| OI:276 | 도달 불가 arm `upload_materialization_failed` | 삭제 |
| OI:407-491 `hand_busy`·`unobserved` | `phase != Busy` 보류 포함 | 삭제(busy child·claim은 inject로) |
| OI:495-545 `paste_busy` | reply_context, follower 합치기 | inject로 이동 |
| OI:625-672 `into_busy_pane` | 외부 턴 pane 주입 | 삭제(자기 턴 장부의 `execute_owned` busy 가지가 대신한다) |
| OI:1704-1740 Codex 경로 넷 | `CodexIdle`·`CodexBusy|CodexHerdrBusy`의 `read_turn` 관문(1714-1720) | host별 Tab 하나로 합치고 `read_turn` 관문은 삭제 |
| `owner_input/busy_turn.rs` | 맨 StreamingTurn | inject.rs로 합침 |
| OB:33-97 `running`, OB:349-398 `legacy_diagnostic`(호출 IT:1698), OB:399-470 `late` 사람 가지(호출 OI:1251) | busy 재판정 | 삭제. 기계 시작 늦은 읽기와 `unwound`(OB:511)는 child `owned_busy/late_start.rs`로 이동 |
| OB:99-200 `early` busy 주입(OB:197) | | 삭제(Missing 복구만 남김) |
| `queue_io/input_owner/target.rs:44-57,81-88` | Codex 경로 넷 | Codex tmux·herdr 하나씩으로 |
| `effect/codex.rs` `idle_codex`(Enter) | | Tab 절차로 교체 |
| IG:1009-1088 | 토큰이 있으면 drain을 띄우지 않음(B39) | owner 채널은 적재 뒤 토큰과 무관하게 owner_wake ◆ |
| `turn_presence/supervisor.rs:695` | `!token &&` wake 조건 | owner 채널 보관 front는 Idle 전이에서 깨운다 ◇ |

## 4. 단일 경로로 옮길 책임

| 책임 | 지금 | 새 위치 |
|---|---|---|
| 전이 잠금 | OI:264-269, 280 | `inject::hand` 1단계 |
| restart_recovery 거절 | OI:270-278 | `inject::hand` 2단계(U3, 토큰 유무 무관) |
| native busy child | OI:463-471 | 합류 장부, 키 전 |
| iMessage reply_context | OI:503-523 | inject payload 준비 |
| follower 합치기 | OI:526-535, OI:1012-1017 | inject payload 준비(장부별 기존 형식 유지) |
| preclaim retry | OI:961 | claim 직전(두 장부 공통) |
| never-started 복구 | OI:993 | 자기 턴 장부(그대로) |
| Decision journal | `decision_drain.rs:212` | 그대로(U2) |
| ➕ 표시 시점 | 키 직후 `turn_view_reconciler/injected.rs:179,204` | anchor 결합에서 '합류'로 판정된 순간(§6) |
| 답 게시 주체 | Claude 합류 장부는 토큰 턴 relay, 외부 턴은 tui_o, 0단계 S1T는 A 턴 bridge | **tui_o writer 하나**(TW). 게시마다 §6의 reply anchor를 단다. owner 관찰자(`busy_owner_observer` WF:739, Codex `warm_followup::owner_observer`)는 tui_o가 출력을 맡은 채널(`tui_o::writer::host::channel_accepts`)에서 정산만 하고 게시하지 않는다 |

## 5. 새로 필요한 것

### 5.1 귀속 대기 항목 (키 뒤 echo 전)

V2 `SnapshotV2`(`v2_types.rs:27-65`)에 선택 필드 `awaiting_echo: Vec<AwaitingEcho>`를 더한다. `serde(default, skip_serializing_if = "Vec::is_empty")`이고 형식 전환 장치는 없다.

`AwaitingEcho`에 담는 값: anchor message id 목록(follower 포함), frame nonce, provider·session·transcript(모르면 None)·키 시점 offset, 장부 종류, 토큰 turn id, echo deadline(`head_injection.confirmation_deadline_ms` 계산 재사용).

수명:
1. **만듦**: Submitted·WriteUncertain과 같은 commit에서 만든다.
2. **수락됨**(Codex): 대기 항목이나 사용자 표식이 확인되면 표시한다(Hq).
3. **결합**: §6의 anchor 결합 기록이 생기면 지운다.
4. **기한 초과**: 재주입 없이 retained Unconfirmed + 재전송 요청 1회.
5. **세션 종료**: Cancelled/HostTerminated 정산 + 알림.

본체는 새 child `v2/awaiting_echo.rs`다.

### 5.2 답 anchor 결합 기록 (§6의 저장소)

tui_o store에 채널별 durable journal `anchor_bindings`(새 child `src/services/tui_o/store/anchor_bindings.rs`)를 둔다.
- 키는 `(source id, native turn id)`다. source id는 dev·ino를 포함한 transcript 신원(`tui_o/shadow/mod.rs:50-`)이다.
- 값: opener anchor(없으면 None), 합류 anchor 목록, 결합 근거(입력 행 range, nonce), 확정 시각.
- tui_o store는 이미 채널별 ledger·spool을 durable하게 쓴다(`tui_o/store/ledger.rs`, `spool.rs`). 같은 디렉터리·같은 잠금 규칙을 따른다.
- 게시 ledger의 `LedgerEntry::Prepared`(`tui_o/store/ledger.rs:65-76`)에 `reply_to: Option<u64>`를 선택 필드로 더한다. 재시작 뒤 같은 조각을 다시 보낼 때 같은 reply를 쓰고, 확인 스캔(`TW/confirm.rs`)이 같은 조각으로 인정하게 하기 위해서다.

### 5.3 복구 kickoff 전 보류 (H10)

U3로 H5에 포함된다. 조각 A4에서 `restore_inflight.rs:2248` kickoff → `recovery_flush.rs:120-123` `complete_active_reconcile` 순서를 고정하는 시험을 둔다.

## 6. 귀속 모델: 입력 anchor ↔ native 턴 ↔ 게시 답

### 6.1 결합 지점 (file:line)

| # | 결합 | 지금 코드 | 새로 할 일 |
|---|---|---|---|
| J1 | **입력 anchor ↔ frame nonce** | 키 전 등록. Codex `history::append::register((session,id,author), transcript, offset, frame, target_turn, message_id, …)`(`HF/history/append.rs:128-165`, 호출 `codex/herdr_turn/busy.rs:82-90`). Owner 모드는 `HF/history/v2.rs` `scoped_register`(native evidence `Ticket{key, channel_id, source}`, `src/domain/human_input/native_source.rs:26-33`). Claude `history::register(who, transcript, offset, &frame)`(`claude_tui/host_input/busy/herdr.rs:67`, message id 없음) | 두 provider 모두 V2 admission의 anchor message id(follower 포함)를 nonce와 함께 `awaiting_echo`에 쓴다(§5.1). Claude history 등록에도 message id를 싣는다 |
| J2 | **frame nonce ↔ native 턴** | transcript 입력 행 판정: `HF/history.rs:838` `attempt_row`(자기 user 행 = true, queued command = false). Claude 분류 `TW/input_facts/native.rs:20-60`(`QueueRecord`: Prompt·Absorbed·Human). 턴 경계 `TW/input_facts.rs:40-57`(`Ordered::Opened{range, native_turn_id}`·`Closed{…, aborted}`·`Input{range, record}`). Codex native turn id는 rollout `task_started`의 turn id(`codex_tui/busy_inject/rollout.rs` `read_turn`) | 입력 행에서 frame 헤더의 nonce 꼬리표(`HF.rs:102-119` `plain_inject`/`untagged`)를 읽고, 그 행을 감싼 native 턴에 결합한다. 자기 user 행이 턴을 열면 그 턴의 opener, 진행 중 턴 안의 흡수·queued command면 합류 |
| J3 | **native 턴 ↔ 게시 답 조각** | tui_o writer가 record마다 조각을 만든다: `TW/pieces.rs:115-202` `UnitDeriver::derive*` → `OwedWork{derived, source, range}`(`TW/pieces.rs:45-52`). 조각에는 이미 `output_origin` 운반 칸이 있다(`TW/pieces.rs:22-29,81-86,131-132`) | 새 child `TW/reply_anchor.rs`가 같은 record 흐름에서 현재 native 턴을 따라가며 조각에 `reply_to`를 붙인다(`output_origin`과 같은 운반 방식). 조각의 range가 속한 턴의 결합 기록에서 정한다 |
| J4 | **게시 ↔ Discord reply reference** | `TW/deliver.rs:532-563` `send_piece`가 `LedgerEntry::Prepared` 뒤 `port.post_unit(channel, payload, origin, kind)`(`TW/deliver.rs:590-595`)를 부른다. 포트 `TW/mod.rs:115-145`, Discord 구현 `discord/outbound/o_writer_io.rs:184-210`. **reference 인자가 없다** | `post_unit`에 `reply_to`를 더하고, Discord 구현은 기존 `discord/outbound/serenity_reference.rs`로 message reference를 붙인다. anchor가 지워졌으면(삭제된 메시지) reference 없이 게시하고 결합 기록에 사유를 남긴다 |
| J5 | **결합 관측 기록** | `relay_delivery` 이벤트는 session_relay_sink 경로에만 있다(`session_relay_sink/task_notification_context.rs:576-605`). tui_o writer는 내지 않는다(실측 이벤트 0, bridge 확인 0/94의 원인) | 게시마다 `tui_o reply anchored` 한 줄(channel, native turn id, anchor, msg id, 근거 range)과 `emit_relay_delivery(…, prompt_anchor, "tui_o_writer", …)`를 남긴다 |

### 6.2 규칙

1. **답 조각의 anchor는 그 조각이 속한 native 턴의 결합 기록이 정한다.**
   - 턴을 연 입력에 Discord anchor가 있으면 그 anchor에 건다.
   - 없으면(TUI 직접 입력, 백그라운드 알림이 연 턴) 그 턴에 처음 합류한 Discord 입력의 anchor에 건다.
   - 그것도 없으면 reference 없이 게시한다(지금과 같음).
   - 모델의 `ATarget`이 이 규칙이고, `AnswerBound`가 검사한다.
2. **합류 입력은 합친 답 한 번으로 친다.**
   - 합류 입력에는 ➕ "진행 중 턴에 합류"를 단다. 지금 반응 `turn_view_reconciler/injected.rs:204-231`이 있고, Idle 관측에서 ✅로 바뀐다(`turn_presence/supervisor.rs:884`).
   - 따로 답하지 않는다. 합친 답은 규칙 1의 anchor 하나에 건다.
   - 한 Discord 메시지에는 reference를 하나만 걸 수 있으므로 여러 anchor에 나누지 않는다.
3. **게시 순서로 귀속하지 않는다**(`PostOrder` RED). "가장 오래된 미답 입력"에 붙이는 규칙은 외부 턴 A의 답을 Tab으로 대기한 B에 붙인다. 0단계 S1T의 "B 답이 A bridge 수명 안에서 게시"가 바로 이 모양이다.
4. **reply reference 없이 게시하지 않는다**(`NoReference` RED). anchor가 있는 답은 반드시 reference를 단다.
5. **결합이 확정되기 전에는 그 턴의 조각을 보내지 않는다.** 턴 opener 행이 이미 spool에 들어왔고 nonce 판정은 같은 record에서 끝나므로 지연은 0에 가깝다. 단 nonce가 있는데 `awaiting_echo`·history에 짝이 없으면 결합 근거가 없는 것이다. 이때는 reference 없이 게시하고 사유를 남긴다. 조각을 막지 않는다(유실 0 우선).
6. **echo가 없다는 사실은 재전송 근거가 아니다**(w1 규칙 유지, `ResendBeforeEcho` RED).

### 6.3 Claude와 Codex를 한 모델로

| 경우 | 입력 행 | 결합 | 답 anchor | 표시 |
|---|---|---|---|---|
| Claude, 진행 중 턴에 흡수 | `remove absorbed_mid_turn`·`queued_command`(A 턴 안) | B = A 턴의 합류 | A의 anchor(A가 외부 턴이면 B) | B에 ➕ |
| Claude, A 뒤 자기 턴 | `dequeue` 뒤 user 행 | B = 새 턴 opener | B | 없음 |
| Claude, 남은 토큰 + Idle(#8432) | user 행 | B = 새 턴 opener | B (토큰 A가 아님) | 없음 |
| Codex Tab, 바쁨 | A의 task_complete 뒤 새 task_started + 사용자 표식 | B = 새 턴 opener | B | 없음 |
| Codex Tab, idle | task_started + 사용자 표식 | B = opener | B | 없음 |
| Codex 한 user 행에 nonce 둘(불일치 b·c 모양) | 한 사용자 메시지에 frame 둘 | 첫 nonce = opener, 둘째 = 합류 | 첫 anchor | 둘째에 ➕와 "다른 입력과 합쳐 처리됨" 알림 |
| 같은 native 턴에 사용자 표식 둘(steer, 불일치 a 모양) | 한 턴 안에 사용자 메시지 둘 | 첫째 = opener, 둘째 = 합류 | 첫 anchor | 둘째에 ➕. 첫째의 작업이 끊겼을 수 있으므로 알림 |

### 6.4 실측 불일치 12건 분류 (원시 증거 `logs/tab-1/attribution-summary.txt`, `attribution-joined.jsonl`)

i8328tab1은 답 대상 99건 중 본문·native 턴 일치 87건을 보고했다. 나머지 12건은 다음과 같다. 그중 Discord 답이 없는 것이 5건(A 답 유실 또는 합쳐짐), 답은 있지만 본문·턴이 맞지 않는 것이 7건이다.

| 분류 | 건수 | 표식 | 원시 증거 모양 | 원인 | 이 설계에서 |
|---|---|---|---|---|---|
| (a) 완료 전 Enter → steer | 8 | C2·C3 EDGE-PRE-ENTER1/2 A·B | A·B가 같은 rollout UUID(예: C2 PRE-ENTER1 `01a2160c-6c7d…`), task_complete 1개. A 답 없음 3건(C2 PRE-ENTER1-A, C3 PRE-ENTER1-A, C3 PRE-ENTER2-A). C2 PRE-ENTER2는 A·B 모두 게시됐지만 UUID가 같음 | 바쁜 턴의 Enter(Codex steer) | **발생 원인 제거**(항상 Tab). 그래도 생기면 §6.3 마지막 행(합류 + 알림) |
| (b) 붙여넣은 직후 Tab → 두 건 합쳐짐 | 2 | C3 BUSY1-1, BUSY1-2 | 둘 다 input UUID `01a21608-53ea…`. 1의 답 없음, 2만 게시. 3은 초안 잔류(수동 Enter 뒤 정상, 87에 포함) | 붙여넣기 묶음 인식 중 키 | **반영 확인·수락 확인**(§2.3)으로 제거. 생기면 nonce 둘 규칙 |
| (c) 앞 초안 잔류 + 다음 시작이 합쳐짐 | 2 | C2 BUSY1-A, BUSY2-A | 둘 다 `01a21609-9c9b…`. BUSY1-A 답 없음. BUSY2-A는 A 표식 2개가 한 사용자 메시지에 | 첫 Enter가 초안에 남음(composer 미준비·반영 전 키) | **반영 확인 + Hc + 수락 확인(Hq)**으로 제거. 생기면 nonce 둘 규칙 |

**bridge 귀속 확인 0/94**는 오귀속이 아니다. 결합 기록이 없는 관측 공백이다.
- 실측 ACK 94개의 Discord `reference`가 모두 null이었다(J4: tui_o 포트에 reference 인자가 없음).
- stdout에 답 ID와 turn id를 잇는 줄이 0개였다.
- `relay_delivery` 이벤트도 0개였다(J5: tui_o writer가 내지 않음).
- J4·J5가 이 공백을 메운다.

## 7. 입력 출처 분류표

"바로 주입"은 §2.2 보류만 보고 넣는다는 뜻이다. Codex는 언제나 Tab 절차(§2.3)다. "답 anchor"는 §6 규칙에 따른 reply 대상이다.

| # | 출처 | 진입 | 처리 | 답 anchor |
|---|---|---|---|---|
| S1 | 사람 owner front(Discord) | IG:120 → `intake_dispatch.rs:815` → `ingress.rs:407` | **바로 주입**(Claude Enter, Codex Tab) | 자기 메시지(합류면 턴 opener) |
| S2 | 허용 봇·announce·wake | `ingress.rs:418-425`(`bot`은 표식, origin=Discord) | **S1과 같다**(U4) | S1과 같다 |
| S3 | busy follow-up retry | owner 채널은 V2 `control.rs:108-115`가 RequeueFront 거절 | 따로 없음. 보관 front가 다음 wake에 S1을 탄다 | S1 |
| S4 | 루틴 | `routines/agent_executor/owner_input.rs:180,219` | **보관**(START_WAITS) | 루틴 자기 카드(기존). 이번 범위에서 바꾸지 않음 |
| S5 | 예약 메시지 | `scheduled_messages/owner_input.rs:198` | **보관** | 기존 |
| S6 | HTTP start·deliver | `agents_turn_deliver.rs:137` → OI:713 | **보관**(409 유지) | 기존 |
| S7 | Decision | `decision_target.rs:52` | **보관**(턴 경계, PG journal) | 기존 |
| S8 | iMessage | `origin_admission.rs:24` | **바로 주입**(reply_context 이동) | iMessage 답 경로(reply_context) + Discord anchor가 있으면 그것 |
| S9 | effort 이어가기 | `effort_restart/tmux.rs:359` | **보관**(H6) | 기존 |
| S10 | 재시작 seed | `restart_seed.rs:95` | **보관**(greeting_permit) | 기존 |
| S11 | 첨부 대기 | `ingress.rs` Hold::Uploads | **보관**(H4) 뒤 바로 주입 | S1 |
| S12 | 스레드 입력 | IG:228, 스레드마다 V2 owner | 스레드 채널마다 S1. THREAD-GUARD(IG:870-926)는 명시 보류로 유지 | 스레드 안 자기 메시지 |
| S13 | TUI 직접 입력 | owner 밖(ExternalInput) | owner 입력이 아님. 사람 초안(H1)의 근원 | 그 입력이 연 턴은 anchor 없음 → 처음 합류한 Discord 입력 또는 none |

U 결정(코드 근거, w1 그대로):
- **U2 기계 시작: START_WAITS를 남긴다.** 근거: `target.rs:76-78`, OI:1219-1243, `routine_delivery`·Decision journal의 턴 경계 정산.
- **U3 `reconcile_done=false`: 사람 입력은 토큰 유무와 무관하게 보류.** 근거: `recovery_kickoff.rs:39`(빈 slot만 claim), 복구 relay 범위.
- **U4 허용 봇: 사람 취급 유지.** 근거: `ingress.rs:418-425`.
- **U1 사람 초안: 알림만.**

## 8. 재시작·부팅·incarnation 경로

재시작을 넘는 것: V2 큐 파일(`pending_queue_persistence.rs:95-106`, 여기에 `awaiting_echo`를 더함), tui_o store(ledger·spool, 여기에 `anchor_bindings`·`Prepared.reply_to`를 더함), HF history 저장소.

| 대상 | 재시작 전 | 부팅에서 | 결과 |
|---|---|---|---|
| 보관 입력(키 0) | V2 `items` 머리 | R6 `owner_input/native_input.rs:188-214` → U3로 H5 보류 → R8 wake → `inject::hand` | 정확히 1회 주입 |
| 키 뒤 echo 전 | `awaiting_echo` | R2 native 정산 → R5 `settle_boot`·토큰 재건 → tui_o writer가 source를 다시 읽으며 J2 결합 시도(입력 행이 이미 있으면 바로 결합) | 결합되면 §6 규칙. 기한 초과면 retained 재전송 요청. **재주입 0** |
| 결합 확정 뒤 답 게시 중 | `anchor_bindings` 행, ledger `Prepared{reply_to}`·`Posted` | tui_o boot(`TW/boot.rs`)가 ledger를 이어받는다. 남은 조각은 `anchor_bindings`에서 같은 anchor를 다시 읽는다. `Prepared` 뒤 결과가 모르는 POST는 기존 확인 스캔(`TW/confirm.rs`)이 같은 payload로 맞춘다 | 같은 anchor에 1회. 중복 0 |
| 결합 기록이 resume 지점보다 앞인 턴 | `anchor_bindings`에만 있음 | source 재독은 resume 지점부터라 opener 행을 다시 못 본다. `anchor_bindings`에서 읽는다 | anchor 유지 |
| `anchor_bindings`가 없는 옛 턴(배포 전 시작) | 없음 | 결합 근거 없음 → reference 없이 게시(규칙 6.2-5) | 전환 장치 없이 운영 절차(세션 종료 → 배포)로 끝 |
| 키 중간 | `head_injection` phase | R6 `recovery.rs:8-44` 현행 | 현행(`NoDuplicate`) |
| 세션 교체(`!clear`, Herdr pane 교체, 새 세션) | 옛 source id에 묶인 항목 | source id(dev·ino)가 다르므로 새 세션 조각에 옛 결합이 붙지 않는다. 옛 `awaiting_echo`는 clear(A3)·terminate(A2)·R13이 정산하고 알림 | 옛 입력은 알림으로 끝 |
| R13 carried episode | 이전 generation 토큰 | 사람 입력은 합류 장부로 주입된다. 답 anchor는 J2 결합이 정하고 토큰과 무관 | 옛 episode로 잘못 귀속되지 않음 |
| R9-R12 | 각자 보류 | 그대로. preclaim은 합류 장부에도 적용 | |

## 9. 인접 경로 전수

◆는 #8432와 직접 겹친다(표시만). ◇는 간접이다.

| # | 경로 | 진입 | 목표 계약 | 조각 |
|---|---|---|---|---|
| A1a | HTTP Cancel(보존) | `queue_api.rs:209` → `judged_stop.rs:59-69` | 합류 장부 입력은 토큰의 busy child라 함께 멈춘다. 토큰 없는 `Ok(None)` + `awaiting_echo`면 Claude `owned_input_stop`(`judged_stop.rs:458`), Codex Esc 1회. Codex는 대기열이 있을 때 Esc가 "중단 후 즉시 전송"이라 Tab 대기 B가 곧바로 시작한다. 알림에 남긴다. drain은 H11 뒤 | A2 |
| A1b | Cancel(force) | `queue/http_force.rs:53` | `awaiting_echo` HostTerminated 정산 | A2 |
| A1c | `/cancel-queued` | `commands/control.rs:772` | 유지 | – |
| A2 | Herdr Terminate | `herdr_terminate.rs:151,514`, `retired_wake.rs:47` | 전이 잠금을 키까지 쥐므로 `:514`가 본다. `awaiting_echo` 정산. 이미 게시된 답의 anchor는 그대로 | A2 |
| A3 | clear | `commands/control.rs:548,835`, V2 `clear.rs:17,154`, `session_idle_cleanup.rs:87,180` | `protected`(`v2/clear.rs:39-43`)에 귀속 대기 id 포함. clear 뒤 새 source id에는 옛 결합이 이어지지 않는다(§8). composer 미준비는 Hc | A2 |
| A4a | `/stop`·`!stop` | `commands/control.rs:674`, `judged_stop.rs:352,449-458` | busy child를 inject로 옮겨 native stop 확인 유지. 흡수 전 Esc 회수(`Withdraw`)는 미전송 종료 + 재전송 요청 | A3 |
| A4b | agent/Herdr stop | `agents_stop.rs:45`, `codex_stop_delivery.rs:251,417,772` | Codex Tab 입력이 연 턴의 stop을 `anchor_bindings`·`awaiting_echo` 근거로 허용(지금은 later native turn이라 409) | A5 |
| A5a | stale release | `stale_turn.rs:219,286` | 고아 행 해제 유지, 입력 결정권은 뺌 ◆ | A4 |
| A5b | lease release | `turn_lease.rs:71,148,273` | inspect에 `awaiting_echo` 노출 ◆ | A2 |
| A6 | #4030 demotion | `foreign_recovery.rs:74,365`, `destructive_cancel_gate.rs:234` | `awaiting_echo`가 있으면 pane을 살아 있음으로 봄 ◆ | A2 |
| A7 | presence wake | `supervisor.rs:657,695,884` | wake 조건에서 토큰 제외(owner 보관 front), unknown 알림 조건에 `awaiting_echo` 추가, ➕는 합류 결합 뒤에만 ◇ | A1(➕), A2(wake·알림) |
| A8 | #3277 deadline | `turn_finalizer/reconcile.rs:223,270,331` | 토큰 턴 유지, 귀속 대기는 자기 deadline ◆ | A2 |
| A9 | #8430 상한 | | 안전망 유지. 초안·모달·작업 중 화면에는 미적용(U1) | 별도 |
| A10 | tui_o 게시 확인·재게시 | `TW/confirm.rs`, `TW/local_retry.rs`, `tui_o/repost/` | 재게시·확인 스캔은 `Prepared.reply_to`를 그대로 쓴다. reference가 달라 같은 payload를 다른 조각으로 보는 일이 없게 한다 | A1 |

## 10. #8432·#8430 경계

- #8432와 겹치는 함수(표시만): `health/turn_deliver.rs:131,166`, `stale_turn.rs:219-302`, `turn_lease.rs:71-334`, `turn_finalizer/reconcile.rs:223-358`, `foreign_recovery.rs:74,365`. 구현은 #8432가 착지한 main 위에서 새 브랜치로 시작한다.
- 목표 경로는 입력 liveness를 토큰 해제에 기대지 않는다(`Target8432` GREEN). 그래도 Cancel·stop·lease 손잡이 때문에 #8432는 필요하다.
- #8430은 안전망으로 남는다.

## 11. 구현 조각 분할 (귀속을 맨 앞에)

크기는 추정이다(prod 코드 순증, 시험 제외). 측정은 착지 레그에서 `pr_cap_prod.py <base> HEAD`로만 한다.

giant 파일에는 블록 이동·호출 교체·필드나 enum 변형 추가만 한다:
- OI·IG·IT
- `tui_o/store/ledger.rs`(1174줄)
- `codex_tui/input.rs`(3128줄)
- `restore_inflight.rs`(2796줄)
- `claude_tui/hosting/warm_followup.rs`(1057줄)

`TW/deliver.rs`는 998줄이다. 조각에서 `reply_to` 전달 한 줄만 더하면 1000줄을 넘으므로, 같은 조각에서 `send_piece`의 Prepared 생성 블록(약 15줄)을 child로 옮긴다.

순서: **A1 → A2 → A3 → A5 → A4**

### A1 — 답 anchor 귀속 (핵심)
- 파일(prod 10개):
  - 새 child: `TW/reply_anchor.rs`(+230: native 턴 추적, nonce 판독, 결합·합류 규칙), `tui_o/store/anchor_bindings.rs`(+130: durable journal), `TW/deliver/prepared.rs`(블록 이동 +15)
  - 수정: `TW/pieces.rs`(+15, `reply_to` 운반), `TW/mod.rs`(+8, 포트 인자), `TW/deliver.rs`(-15 +3), `tui_o/store/ledger.rs`(+3, 선택 필드), `discord/outbound/o_writer_io.rs`(+20, message reference), `turn_view_reconciler/injected.rs`(+15, 합류 ➕), `HF/history.rs` 또는 `HF/history/v2.rs`(+25, nonce → anchor 조회)
- 예상: prod 10파일 / 순 +450.
- RED 계획(현행에서 실패 확인 뒤 구현):
  1. tui_o 조각 게시에 reference가 없다(현행 `post_unit`에 인자 없음). 목표는 opener anchor reply다.
  2. 외부 턴 A + Codex Tab 대기 B: A의 조각은 reference 없음, B의 조각은 B reply(게시 순서 귀속 금지).
  3. Claude 흡수(`absorbed_mid_turn` fixture): 합친 답은 A anchor, B에 ➕ 1회, B 별도 게시 0.
  4. 한 user 행에 nonce 둘(실측 busy1 fixture): 첫 anchor reply, 둘째 ➕와 알림.
  5. 재시작: `Prepared{reply_to}` 뒤 결과 모르는 POST 확인 스캔이 같은 조각으로 인정, `anchor_bindings`에서 anchor 복구.
  6. 결합 근거 없는 nonce: reference 없이 게시하고 조각을 막지 않는다.
  7. J5 관측 줄과 `relay_delivery` 1회.
- 검증: tui_o writer actor 시험군(`actor_tests.rs` 포트 하네스), ledger 시험, o_writer_io 시험.

### A2 — 귀속 대기 손잡이와 인접 경로
- 파일(prod 11개):
  - 새 child: `v2/awaiting_echo.rs`(+170), `recovery_engine/awaiting_echo_boot.rs`(+50)
  - 수정: `v2_types.rs`(+12), `v2.rs`(+20), `v2/clear.rs`(+8), `herdr_terminate.rs`(+25), `judged_stop.rs`(+35), `turn_lease.rs`(+15), `turn_presence/supervisor.rs`(+15), `v2/retained.rs`(+30), `destructive_cancel_gate.rs`(+10)
- 예상: prod 11파일 / 순 +390.
- RED 계획: clear protected, terminate 정산, echo 기한 retained(재주입 0), Hq 수락 표시, boot 재결합.

### A3 — 사람 입력 단일 경로(Claude 바로 주입·합류)
- 파일(prod 9개):
  - 새 child: `owner_input/inject.rs`(+290), `owned_busy/late_start.rs`(이동 +60)
  - 수정·삭제: OI(-210), `busy_turn.rs`(삭제 -41), OB(-250), IT(-20), WF(관찰자 게시 끔 +8), `busy_retry.rs`(+5), `claude_tui/host_input/busy/herdr.rs`(history 등록에 message id +5)
- 예상: prod 9파일 / 순 약 -150.
- RED 계획:
  1. #8432 모양(남은 토큰 + Idle) → Submitted 1회. 현행은 NotSent 반복이다.
  2. 토큰 없는 가지의 전이 잠금
  3. U3 보류
  4. iMessage reply_context 보존
  5. busy child가 키 전에 고정됨

### A5 — Codex 항상 Tab
- 파일(prod 10개):
  - 새 child: `codex_tui/busy_inject/tab_protocol.rs`(+120: 반영 확인·수락 확인·veto 보강)
  - 수정: `session_host/submit.rs`(+8)·`tmux_host.rs`·`herdr_gate.rs`(+20), `codex_tui/input.rs`(enum +6), `codex_tui/busy_inject.rs`(+10/-20), `codex/herdr_turn/busy.rs`(+15/-15), `effect/codex.rs`(`idle_codex` 교체 +20/-70), `queue_io/input_owner/target.rs`(-20), OI(Codex 경로 합침 -35), `codex_tui/busy_inject/screen.rs`(action banner·질문 화면 veto +20), `codex_stop_delivery.rs`(+20)
- 예상: prod 10파일 / 순 약 +60.
- RED 계획:
  1. Codex 바쁜 화면: 현행은 `NotSent("busy_turn: transcript unavailable")` 또는 Enter다. 목표는 Tab 1회다.
  2. idle 화면도 Tab(현행 `idle_codex`는 Enter).
  3. 반영 안 됨이면 키 0 + WriteUncertain(재붙여넣기 0).
  4. 붙여넣은 뒤 completion popup이면 키 0.
  5. 수락 미확인이면 다음 front Hq.
  6. action banner·질문 화면 fixture면 보관.
  7. `/clear` 직후 readiness 전이면 Hc.
- 화면 fixture는 i8328tab1 `logs/tab-1/` 스냅샷에서 옮긴다(비밀값 없음 확인 뒤).

### A4 — intake 적재·wake와 재시작 정리
- 파일(prod 5개): IG(-30/+25), `intake_gate/queue_effects.rs`(+10), `mailbox_probe.rs`(+5), `ingress.rs`(+5), `stale_turn.rs`(-10).
- 예상: prod 5파일 / 순 +5.
- RED 계획: blocking 토큰이 있을 때 적재하면 owner wake 1회(현행 없음). kickoff → reconcile 순서를 고정한다.

### 합계
- 서로 다른 prod 파일 약 40개, 순 약 +755(+450, +390, -150, +60, +5).
- 조각 최대는 A1의 10파일·+450이다. 모두 30파일·+1500 상한과 PLAN_READY 선 +1200 아래다.

## 12. 남은 위험·미확인

1. **owner 채널의 출력 주체가 언제나 tui_o writer인지**: `tui_o::writer::host::channel_accepts`가 거짓인 채널(legacy relay)은 J3·J4가 닿지 않는다. 그런 채널에서는 기존 관찰자가 게시한다(w1 방식). anchor는 J1·J2 결합 기록을 공유해 같은 규칙으로 단다. A1 첫 시험에서 owner 채널 목록과 `channel_accepts`의 관계를 고정한다.
2. **J2의 nonce 판독**: 사용자가 frame 헤더를 흉내 낸 텍스트를 TUI에 직접 치면 가짜 nonce가 생긴다. 결합은 `awaiting_echo`·history에 같은 nonce와 같은 frame hash(`HF/history/v2/native.rs:40` `frame_hashes`)가 있을 때만 인정한다.
3. **Codex Vim 모드·side conversation**: 운영 launch 설정을 읽지 않는 범위라 Vim 시작 여부를 확인하지 못했다. 화면 판정으로 보관한다.
4. **Claude managed stash**(`busy/herdr.rs:77-98`): 초안을 제출하지 않으므로 U1과 충돌하지 않는다고 판단했다. **메인 확인 권장.**
5. **Codex 실행 용량(execution capacity)**: 지금은 idle 경로만 `execution_capacity::acquire`를 잡는다(OI:1495-1515). 항상 Tab이면 키를 누를 때 B가 곧 시작할지 A 뒤에 시작할지 모른다. 그러나 B는 어느 쪽이든 자기 native 턴이 된다. 그래서 A5는 Codex Tab마다 용량을 잡고 B 턴이 끝날 때 놓는 것으로 정한다. A가 도는 동안 B가 용량을 미리 쥐는 시간이 생기고, 노드 용량이 빠듯하면 B가 `NotSent(capacity)` 보관이 된다. A5 RED 시험에 넣는다.
6. 앞 턴 relay가 뒤 입력의 자기 턴 출력까지 앞 턴 몫으로 보내지 않는지(w1 위험 1)는 tui_o writer 하나가 게시하고 조각마다 턴 결합으로 anchor를 정하는 구조에서는 문제가 되지 않는다. 다만 legacy relay 채널(위 1)에서는 남는다.
