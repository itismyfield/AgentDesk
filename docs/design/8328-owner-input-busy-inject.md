# #8328 1단계 설계: owner 입력 바쁜 중 주입 — Claude는 진행 중 턴에 합류, Codex는 키 선택

- 기준 코드: `origin/main` = `82702fe252` (설계 준비 레그 head `124a47a5e3`, 이 문서 브랜치 `wt/i8328-design`).
- 사용자 결정: busy-inject-step0-1126 a (2026-11-26 13:01). 입력 자료:
  - 설계 준비 보고서(출처 13·busy 지점 42·인접 12·재시작 14·보관 조건 10)
  - 0단계 실측 보고서(입력 53건, 시험 셀 3개)
  - 이슈 #8328 본문과 댓글
- 이 문서는 운영 코드를 바꾸지 않는다. 바뀌는 것은 모델·cfg·보고서·색인 행뿐이다. cargo는 0회 돌렸다.
- 약어(설계 준비 보고서와 같음). 경로는 `src/services/discord/` 기준이다.
  - OI = `router/message_handler/provider_dispatch/owner_input.rs` (1795줄, giant)
  - OB = `router/message_handler/intake_turn/owned_busy.rs` (813줄)
  - IG = `router/intake_gate.rs` (1419줄, giant)
  - IT = `router/message_handler/intake_turn.rs` (2262줄, giant)
  - V2 = `src/services/turn_orchestrator/queued_head/v2*`
  - WF = `src/services/claude_tui/hosting/warm_followup.rs` (1057줄, giant)

## 0. 한 줄 요약

사람 입력(Discord·iMessage·허용 봇)은 바쁜지 확인해서 보관하지 않는다. 보류 조건만 확인한 뒤 바로 넣는다.
- Claude: 붙여넣기+Enter. TUI가 다음 도구 경계에서 입력을 진행 중 턴에 흡수하면, 그 입력의 몫은 진행 중 턴의 답에 한 번만 합쳐 게시한다. 입력 메시지에는 '진행 중 턴에 합류'(➕) 표시를 단다. 흡수되지 않고 자기 턴을 열면 자기 답을 받는다.
- Codex: 바쁜지 읽은 값은 어떤 키를 누를지 고르는 데에만 쓴다. 바쁘면 Tab, idle이면 Enter, 불확실하면 실측 결과를 따른다(§12, 지금은 자리만 둔다).

답을 어느 입력에 귀속할지는 mailbox 토큰이 아니라 transcript에 남은 입력 echo의 위치로 정한다. echo가 아직 없다는 사실은 재전송 근거가 되지 못한다.

## 1. 상태 차트 (0단계, 모델 갱신)

모델 `docs/statecharts/input-single-path/OwnerInputBusySplit.tla`를 고쳤다. cfg는 10개에서 20개로 늘었다. 보고서는 `owner-input-busy-split.report.md`에 있다.

전제 3을 바꿨다. 설계 준비판은 "TUI 대기열이 입력마다 별도 턴을 지킨다"를 전제로 했다. 이번 판은 "유실 0·중복 0, 합류하면 합친 답을 1회 게시"를 전제로 한다.
- `TuiAbsorb`(Claude가 도구 경계에서 B를 흡수)를 공정성 없이 넣었다. 흡수가 일어나는 경로와 A가 끝난 뒤 B가 자기 턴을 여는 경로를 둘 다 탐색한다.
- A가 끝나면 합류한 B의 몫이 A의 답 하나에 실려 나간다(`merged`).
- `ReplyExactlyOnce`는 이렇게 바꿨다. 답이 게시된 B의 몫은 정확히 하나여야 한다. 자기 답이든 A 답에 합쳐진 몫이든 둘 중 하나만 있어야 한다.
- `JoinedMarked`(새 규칙): Claude B가 A 답에 합쳐졌으면 합류 표시가 있어야 한다.
- `NoSilentLoss`에 A 작업 유실(`aLost`, Codex steer)을 더했다.
- liveness `IdleDelivers`("Idle이면 보관 입력은 결국 전달")는 그대로 두었다. `ReplyPosted`는 "B 몫이 결국 1회 게시"로 바꿨다.
- Codex는 `ObserveCodex`(보류 조건 확인 → 화면 한 번 읽기, 1회는 Unknown일 수 있음)와 `KeyCodex`(지금 화면에 키가 들어감) 두 단계로 나눴다. 읽기와 키 사이에 A가 끝나는 경합도 포함한다.

`statecharts_tlc.check_cfg`로 20개 cfg를 직접 돌린 결과는 아래 표와 같다. 전부 기대 집합과 일치했다.

| cfg | 결과 | 뜻 |
|---|---|---|
| `Current8432` | **RED** `IdleDelivers`·`ReplyPosted` | 현재 경로: 남은 토큰 + Idle 화면 → hand_busy 보류 반복 (#8432 모양) |
| `CurrentBusy` | GREEN | 현재 경로, A 정상 종료 |
| `Target8432` | **GREEN** | 목표 경로: 남은 토큰이 있어도 바로 주입, echo로 B 답 귀속 |
| `TargetBusy` | GREEN | 합류 경로와 A 뒤 자기 턴 경로 모두 성립 |
| `TargetHoldCleared`·`TargetComposerNotReady` | GREEN | 모달·composer 미준비 보류가 풀리면 전달 |
| `TargetDraftStuck` | RED `IdleDelivers`·`ReplyPosted` (U1로 수용) | 사람 초안이 끝내 남으면 알림만 하고 전달하지 않는다 |
| `TargetDraftStuckCap` | GREEN | (U1이 채택하지 않은) 상한 보관 제출 대조군 |
| `TargetTokenAttribution` | mutant RED `ReplyExactlyOnce`·`ReplyPosted` | 토큰 귀속 금지 |
| `TargetResendBeforeEcho` | mutant RED `NoDuplicate` | echo 없음을 근거로 재전송하는 것 금지 |
| `TargetTuiDrop`·`CodexTuiDrop` | mutant RED `NoSilentLoss`·`ReplyPosted` | TUI가 입력을 버리면 깨진다(실측에서는 0건) |
| `TargetJoinUnmarked` | mutant RED `JoinedMarked` | 합류 표시 누락 |
| `TargetJoinDoublePost` | mutant RED `ReplyExactlyOnce` | 합류한 B 몫을 따로 한 번 더 게시하는 것(A 꼬리를 B 답으로 다시 relay) |
| `CodexTargetBusy`·`CodexTarget8432` | GREEN | 바쁨 Tab, idle Enter, 불확실은 보류 |
| `CodexAlwaysEnter` | mutant RED `NoSilentLoss` | 바쁨 판정을 없애고 항상 Enter를 누르면 steer로 A 작업을 잃는다(실측 3회) |
| `CodexUncertainEnter` | RED `NoSilentLoss` | 불확실할 때 Enter는 금지 |
| `CodexUncertainTab` | GREEN (`TabAtIdle=submits` 가정) | 불확실할 때 Tab은 idle Tab이 제출되는 경우에만 안전하다 |
| `CodexTabIdleStuck` | RED `ReplyPosted` (실측 대기 가설) | idle 화면의 Tab이 제출되지 않고 걸리면, 읽기와 키 사이 경합에서 B가 멈춘다 |

정리: Current8432는 RED, 바꾼 Target은 GREEN이다. mutant 7개(토큰 귀속·echo 전 재전송·TUI drop ×2·합류 표시 누락·합류 이중 게시·항상 Enter)는 모두 RED다. Codex 두 가설 cfg(`CodexTabIdleStuck`·`CodexUncertainTab`)는 실측 레그 i8328tab1이 어느 쪽인지 정한다(§12).

차트(모두 `docs/statecharts/input-single-path/`):
- `owner-input-busy-split.current-8432.chart.mmd` (exit 1)
- `….target-8432.chart.mmd`, `….target-busy-join.chart.mmd`, `….codex-target-busy.chart.mmd` (exit 0)
- `….codex-always-enter.chart.mmd` (exit 1)

모델 변수 값 범위는 모두 설계 준비 전수표에서 가져왔다.
- `blocker` 값: 보관 조건 H1·H2·H3·H4, composer 미준비, H10
- `Scenario`: 재시작 R·#8432 경로
- `TuiAbsorb`: 0단계 실측 S1(O)·S5D의 `absorbed_mid_turn`
- `CodexAlwaysEnter`: 실측 c2·c3 S1D·S5D

## 2. 목표 경로

### 2.1 owner drive (OI:178 handoff 안)

```
restart_gate(OI:190-199) → writer boot(OI:200-223) → claude_handoff::prepare(OI:235-241)   [그대로]
match origin:
  기계(루틴·예약·HTTP·Decision·effort·restart seed; !joins_running_turn):
      그대로 — has_active_turn이면 START_WAITS, 아니면 hand_idle                    [U2]
  사람(Discord·iMessage·허용 봇; joins_running_turn):
      inject::hand(front)                                                           [새 child]
```

`inject::hand`는 새 child 모듈 `router/message_handler/provider_dispatch/owner_input/inject.rs`에 둔다. 하는 일은 다음 순서다.

1. **전이 잠금**: `session_transition_locks.lock(ch).try_lock_owned()`. 잠금은 키를 다 누를 때까지 쥔다. 실패하면 `NotSent("session_transition_busy")`. 지금은 busy 가지에만 있다(OI:264-269, 280).
2. **보류 조건**(§2.2): 하나라도 걸리면 `NotSent(<사유>)`. front는 남고, 실제 wake(사유별 해소)를 기다린다.
3. **장부 고르기**. 이것은 보류 관문이 아니다. 토큰을 읽은 결과가 무엇이든 보류로 이어지지 않는다.
   - mailbox 토큰이 있으면(살아 있든 #8432로 남았든) **합류 장부**를 쓴다. 새 토큰을 만들지 않고 front를 claim한다. 토큰 행의 nonce가 맞고 native stop이 끝나지 않았으면 `record_native_busy_child`(지금 OI:463-471)로 키 입력 전에 고정한다. 그다음 §5의 귀속 대기 항목을 쓴다.
   - 토큰이 없으면 **자기 턴 장부**를 쓴다. 기존 `decision_drain::hand_idle` → `start_idle`(OI:943)이 토큰을 잡고 턴을 연다. pane이 외부 턴으로 바쁘면 `execute_owned`가 이미 `running_transcript`(OI:1316) → `Registration::observed` + `busy_observer`(OI:1326)로 바쁜 pane 주입을 한다. 이 가지는 손대지 않는다.
4. **phase는 키를 누를 때 관측한 값**(B13)으로 정한다. `Registration::observed`·`read_as`(OI:70-110)의 phase는 경로(`target::route`, `queue_io/input_owner/target.rs:63-91`)를 고르는 입력일 뿐이다. `phase != Busy`를 보류로 바꾸던 검사(OI:458, OI:646)는 지운다. 합류 장부에서 키 시점 화면이 Idle이면(#8432: A 답은 끝나고 토큰만 남음) 경로는 그대로 `ClaudeBusy`(토큰 턴 아래 붙여넣기)다. B는 TUI에서 자기 턴을 열고, 그 답은 §6의 echo 관찰자가 게시한다.
5. **키 효과**는 경로별 기존 effect를 쓴다(`queue_io/input_owner/effect/{claude.rs,codex.rs}`).
   - Claude: `BusyClaude`(Herdr는 `claude_tui/host_input/busy/herdr.rs:28-140`).
   - Codex: §2.3의 키 선택을 거친 `BusyCodexHerdr`/`BusyCodex`/`idle_codex`.
6. **관찰자 설치**: 합류 장부에서도 echo 기반 관찰자를 설치한다. Claude는 `busy_owner_observer`(WF:739)이고, Codex 대응은 §6.3에서 다룬다. 지금 hand_busy는 관찰자 없이 토큰 턴의 relay에 맡긴다(=토큰 귀속). 이것이 `TokenAttribution` mutant와 같은 모양이다.

### 2.2 남는 보류 조건 (목표 경로 전부)

| # | 조건 | 판정 위치 | 해소·wake | #8430 상한 |
|---|---|---|---|---|
| H3 | 세션 없음 | `Phase::Absent` → `Route::Cold`(target.rs:81), OI:1752 | 보류가 아니라 기존 cold launch(자기 턴 장부). 실패 시 `start_failure.rs` retry wait | 불필요 |
| H1 | 사람 초안 | 키 시점 effect: `busy/herdr.rs:77`(PersonDraft), `effect/claude.rs` `draft_notice`, Codex `screen::delivery_veto` | 사람이 보내거나 지움 → held_watch 재관측 | **알림만**(U1). 상한이 와도 자동 제출하지 않는다. 기존 managed stash(초안을 C-s로 보관했다가 복원하는 왕복)는 사람 초안을 제출하지 않으므로 이 결정 밖이고 현행대로 둔다 |
| H2 | 모달·승인 창 | `busy/herdr.rs:40-51`, `codex_tui/busy_inject/screen.rs` MODAL_FOOTERS, `ingress.rs` Hold::Modal/Dialog | 사람 응답 → held_watch | 불필요 |
| Hc | composer 미준비(`/clear`·재시작 직후) | Claude: `claude_tui/input/preparation_hold.rs:112`. Codex: 세션 전환 뒤 `codex_tui/readiness.rs`의 준비 증명이 날 때까지(실측 0.3s 실패, 2s 성공) | readiness 증명 → held_watch | 불필요 |
| H4 | 첨부 준비 전 | `input_payload::busy_body`(`input_payload.rs:143`), `ingress.rs` Hold::Uploads | 내려받기 완료 wake. 실패는 알림 | 불필요 |
| H5 | 재시작 복구 중 | `ingress::ingress_hold`(`ingress.rs:192-198`) | R8 `complete_active_reconcile` wake | 불필요 |
| H6 | 세션 전환·restart gate·effort restart | 전이 잠금(위 1), OI:190-199, V2 `v2.rs` effort_restart 차단 | `Gate::Drop`·전이 해제 wake | 불필요 |
| H7 | writer boot | OI:200-223 | writer-ready·held_watch | 불필요 |
| H8 | 화면 경합 | effect의 `LockContended` | held_watch | 불필요 |
| H9 | root-replaced Herdr preclaim | `owner_input/preclaim_wait.rs:14-48`. 지금은 idle 가지 전용인데 합류 장부에도 적용한다 | 자체 backoff | 불필요 |
| H10 | 복구 kickoff 전 | U3(아래)로 H5에 포함된다. S4 시험이 순서를 고정한다 | R8 wake | 불필요 |
| H11 | 중단 정산 전(새로 명시) | `/stop`·Cancel 뒤 인터럽트가 아직 정산되지 않음(`herdr_interrupt_state`, native stop receipt 미완) | stop 정산 wake | 불필요 |

H11은 실측에 근거해 추가했다. Claude S3D에서 Esc 0.3s 뒤 주입하면 turn 경계 없이 중단된 턴에 흡수됐다(`absorbed_mid_turn`). 중단된 턴의 relay는 이미 끝났으므로, 이 상태로 두면 합류한 B의 몫을 게시할 주체가 없다. 그래서 인터럽트가 정산될 때까지 보류하고, 끝나면 정상 경로로 넣는다. A1a(Cancel 뒤 drain)도 같은 이유로 정산 뒤로 미룬다.

지금은 있지만 목표 경로에서 사라지는 보류 사유:
- `busy_source_unavailable`(OI:455, OB:10)
- `busy_turn_unobserved`(OI:490)
- `busy_turn: transcript unavailable`(OI:1719, `codex/herdr_turn/busy.rs:74`, `codex_tui/busy_inject.rs:281-296`의 `turn()`)
- 토큰이 남아 있다는 이유만으로 생기는 보류 전부

`busy/herdr.rs:53-57`의 `TranscriptUnavailable`은 키 시점에 transcript 파일이 없을 때의 진단으로만 남긴다. 귀속 대기 항목이 echo를 늦게 묶는다(§5). transcript를 확정할 때까지 보류하는 일은 없다.

### 2.3 Codex 키 선택 (바쁨 판정 = 키 선택)

- 판정 입력: 키를 누를 composer 잠금 안에서 찍은 같은 capture.
  - 화면 상태줄에 작업 중 표시(`screen.rs:9-14` STATUS_MARKERS의 `tab to queue message` 같은 작업 중 표식)가 있으면 **Busy**.
  - composer가 읽히고 작업 중 표식이 없으면 **Idle**.
  - 둘 다 아니면 **Unknown**.
  - rollout `read_turn`(`codex_tui/busy_inject.rs:288`)은 진단 보조로만 쓰고 관문으로 쓰지 않는다.
- 규칙: Busy → **Tab**(Codex `Queued follow-up inputs`, A 뒤 자기 task), Idle → **Enter**, Unknown → **[i8328tab1 결과로 채움]**. 모델 기준 안전한 기본값은 보류(Hold, held_watch 재관측)다. Enter는 금지다(`CodexUncertainEnter` RED).
- 키 경합: 읽기 직후 A가 끝나면 Tab이 idle 화면에 들어간다. 그때의 피해가 실측 항목 4·5다. 결과에 따라 둘 중 하나가 된다.
  - (a) idle Tab이 제출되면 그대로 둔다(`CodexUncertainTab` GREEN).
  - (b) 걸리면 S3에서 "Tab 직후 같은 잠금 안에서 화면을 다시 읽어, B가 composer에 남았으면 Enter 1회"를 넣는다. 이것은 같은 키 효과를 마무리하는 동작이고 재붙여넣기가 아니다. keys는 계속 1이다.
- 구현 손잡이:
  - `session_host::PinnedInput`(`session_host/submit.rs:56-59`)에 `Tab`을 더한다.
  - `codex_tui::input::TuiInputAction`(`codex_tui/input.rs:467-472`)에 `Tab`을 더한다. 3128줄 giant지만 enum 변형과 match 가지만 추가한다.
  - `codex_tui/busy_inject.rs:436`(tmux)과 `codex/herdr_turn/busy.rs:125-128`(Herdr)의 `[Paste, Enter]` 계획을 `[Paste, key]`로 바꾼다.

## 3. 지울 분기 (설계 준비 §7 기준, file:line 재대조)

| 위치 | 내용 | 처리 |
|---|---|---|
| OI:249-260 | `has_active_turn` + `external_turn_hold_for_start(None,…)`(B1·B2) | 사람 입력에서는 읽지 않는다. 기계 입력의 START_WAITS 판정용으로만 남긴다 ◆B2 |
| OI:263-290 | 사람 입력 busy 가지(전이 잠금·ingress_hold·`hand_busy` 호출) | 블록을 `inject::hand`로 옮기고 호출만 교체한다 |
| OI:291-294 | 사람 입력 `Ok(false)` → `hand_idle` | `inject::hand` 안 자기 턴 장부로 옮긴다 |
| OI:276 | `_ => "upload_materialization_failed"`(도달 불가 arm) | 삭제 |
| OI:407-487 `hand_busy` | 행 → `owned_busy::running` → observed → `phase != Busy` 보류 → claim → busy child → `paste_busy` | 함수 삭제. busy child·claim은 inject로 옮긴다(§4) |
| OI:489-491 `unobserved` | `busy_turn_unobserved` | 삭제 |
| OI:495-545 `paste_busy` | iMessage reply_context(503-523), follower `busy_body` 합치기(526-535), `busy_turn::build`, `execute_front` | inject로 옮긴다(블록 이동) |
| OI:625-672 `into_busy_pane` | AgentDesk가 시작하지 않은 턴의 pane에 주입 + phase 보류 | 삭제. 호출자 OB `legacy_diagnostic`도 함께 삭제. 같은 일은 자기 턴 장부의 `execute_owned` busy 가지가 한다 |
| `owner_input/busy_turn.rs:3` | 맨 `StreamingTurn` | inject.rs로 합친다(파일 삭제) |
| OI:1714-1720 | `read_turn` None → `busy_turn: transcript unavailable` | 진단 로그로 바꾸고 `target_turn`은 빈 값으로 진행한다(키 선택은 §2.3) |
| OB:33-97 `running` | Herdr active-source 관측과 front 보류 | 삭제 |
| OB:349-398 `legacy_diagnostic` (호출 IT:1698) | `into_busy_pane` 호출 | 삭제, 호출 제거 |
| OB:399-470 `late` + `hold_late` (호출 OI:1251) | 늦은 busy 재판정 | 사람 입력 가지는 삭제한다. 기계 시작의 늦은 읽기(`synthetic` 가지, OI:1219-1243)와 `unwound`(OB:511)는 남기고, 같은 파일 안 `late_start.rs` child로 옮긴다 |
| OB:99-200 `early` (호출 IT:985) | 이른 busy 주입(OB:197 `paste_busy`) + Herdr Missing 복구 | busy 주입은 지우고, Missing 복구(`recover_or_discard`)만 남긴다 |
| IG:1009-1088 | 토큰이 살아 있으면 `busy_active_turn`으로 큐 적재, blocking 토큰이 있으면 drain을 띄우지 않음(B38·B39) | owner 채널(`owner_takes`)에서는 적재 뒤 **토큰과 상관없이 owner_wake**를 한다. 적재 자체는 순서를 지키려고 남긴다. legacy 채널은 그대로 ◆stale_turn |
| `turn_presence/supervisor.rs:695` | `if !token && (turned_idle \|\| due)` | owner 채널의 보관 front는 토큰과 무관하게 Idle 전이에서 깨운다(대비책. 주입은 Idle을 기다리지 않는다) ◇ |

## 4. 단일 경로로 옮길 책임 (hand_busy·idle 가지 → inject)

| 책임 | 지금 위치 | 새 위치 | 이유·근거 |
|---|---|---|---|
| 전이 잠금 | OI:264-269, 280 (busy 가지 전용) | `inject::hand` 1단계, 키 입력까지 보유 | clear·terminate 배제(`herdr_terminate.rs:514` 재확인, V2 `clear.rs`)의 실제 장치. idle 가지에는 없어 지금도 구멍이다 |
| restart_recovery 거절 | OI:270-278 (busy 가지 전용) | `inject::hand` 2단계, 토큰 유무와 무관(U3) | R1 drain 중 주입 금지 |
| native busy child 고정 | OI:463-471 | 합류 장부, 키 입력 전 | 빠지면 Claude native stop이 UNCONFIRMED가 된다(A4a, `claude_stop_identity.rs:76`) |
| iMessage reply_context | OI:503-523 | inject payload 준비 | `with_started_contract`를 잃으면 iMessage 답 경로가 끊긴다 |
| follower 합치기 | OI:526-535(`busy_body`), start_idle OI:1012-1017(text 이어 붙이기) | inject payload 준비 하나 | 두 가지가 합치는 방식이 다르다. 합류 장부는 `busy_body` 형식, 자기 턴 장부는 기존 start 형식을 유지한다(동작 보존) |
| preclaim retry wait | `start_idle` OI:961 `preclaim_wait::before_claim` | `inject::hand`의 claim 직전(두 장부 공통) | 지금은 idle 가지 전용(R11) |
| never-started 복구 | OI:993 `pending_recovery::recover` | 자기 턴 장부(그대로, start_idle 안) | 합류 장부는 새 턴을 열지 않으므로 해당 없음 |
| Decision journal | `decision_drain.rs:212` `hand_idle` | 그대로 | Decision은 기계 쪽 정책(U2)이다. person이지만 `joins_running_turn=false`(origin.rs:33-35) |
| handoff_held 알림·📬 | OI:318-345 | 그대로(보류 사유만 줄어든다) | |
| ➕ 표시 | 키 효과 직후 `busy_retry::note_busy_result` → `turn_view_reconciler/injected.rs:179,204` | echo가 합류로 분류된 순간(§6) | 키 시점에는 합류할지 자기 턴을 열지 모른다(실측 S1LD) |

## 5. 새로 필요한 것

### 5.1 귀속 대기 손잡이 (토큰 없는 주입 시도의 손잡이)

V2 `SnapshotV2`(`v2_types.rs:27-65`)에 `awaiting_echo: Vec<AwaitingEcho>`를 더한다. 새 선택 필드이고 `serde(default, skip_serializing_if = "Vec::is_empty")`다. 옛 파일은 빈 목록으로 읽힌다. 형식 전환 장치나 fallback은 넣지 않는다.

`AwaitingEcho`에 담는 값:
- 원본 message id 목록(follower 포함), attempt nonce
- provider, session, transcript 경로(모르면 None, 나중에 묶음), 키 시점 transcript offset
- 장부 종류(합류/자기 턴), 묶인 토큰 turn id(있으면)
- echo deadline: `head_injection.confirmation_deadline_ms`(`v2_types.rs:118`)와 같은 계산

수명:
1. **만듦**: 키 효과가 Submitted 또는 WriteUncertain이면 front 제거와 같은 commit에서 만든다. 키 전 NotSent에서는 만들지 않는다.
2. **해소**: echo가 분류되면(§6) 지운다. 합류로 분류됐으면 ➕를 달고, 자기 턴이면 관찰자가 relay를 시작한다. 해소는 기존 native receipt(`tui_o/writer/input_facts/native/receipt.rs` MembershipReceipt, `history::attempt_row` `tui_input/human_frame/history.rs:838`)를 그대로 쓴다.
3. **기한 초과**: 자동 재주입은 없다. `retained_inputs`의 Unconfirmed(V2 `retained.rs`)로 옮겨 재전송 요청 알림을 1회 보낸다(`NoDuplicate`, `ResendBeforeEcho` RED).
4. **세션 종료**(terminate·clear·pane 교체): Cancelled/HostTerminated로 정산하고 알림을 보낸다.

소비자(§9의 조정 열과 1:1):
- A3 clear: `protected` 계산(`v2/clear.rs:39-43`)에 귀속 대기 id를 더한다.
- 합류 장부는 토큰의 `active_absorbed_source_ids`에도 넣는다. 쓰는 곳은 `dispatch_reservation.rs:108`과 같은 mailbox op를 쓴다. 해제는 `lease_release.rs:50`이 이미 비운다.
- A2 terminate, A1a/A4 stop(토큰 없는 가지), A5b lease inspect, A7 presence, A8 deadline, R5·R6 재시작.

새 책임이므로 본체는 새 child `turn_orchestrator/queued_head/v2/awaiting_echo.rs`에 둔다. v2.rs(681줄)에는 호출만 더한다.

### 5.2 복구 kickoff 전 보류 (H10)

U3(재시작 직후 `reconcile_done=false`면 사람 입력 보류)를 정하면 H10은 H5에 포함된다. 순서는 다음과 같다.
- `restore_inflight`의 `mailbox_recovery_kickoff`(R5, `restore_inflight.rs:2248`)가 복구 턴의 토큰을 다시 세운다.
- 그 뒤 `complete_active_reconcile`(`recovery_flush.rs:120-123`)이 `reconcile_done`을 세우고, `transition_wake.rs:30-40`이 owner를 깨운다.

S4에 이 순서를 고정하는 시험을 둔다. 순서가 보장되지 않는 경로가 나오면 그 경로에 한해 "kickoff 완료" 표지를 더한다. 새 플래그는 그때만 만든다.

## 6. '진행 중 턴에 합류' 표시와 합친 답 1회 게시: 귀속 규칙

### 6.1 규칙

1. **입력의 몫은 그 입력의 echo가 들어간 턴에 귀속한다.** mailbox 토큰, 키를 누를 때의 화면, 주입 경로 어느 것도 귀속을 정하지 않는다.
2. **echo 분류**(Claude, 기존 `tui_o/writer/input_facts/native.rs:20-60` `QueueRecord`, `history::attempt_row`):
   - `user` 행(`Prompt`)이 B의 frame hash로 새 턴을 열면 → **자기 턴**. B의 관찰자가 그 offset부터 그 턴의 끝(turn_duration/stop)까지 relay해 B의 답으로 한 번 게시한다.
   - `queue-operation remove absorbed_mid_turn`(`Absorbed`)이나 사람 `queued_command` 첨부(`Human`)가 앞 턴 안에 있으면 → **합류**. B의 관찰자는 아무것도 게시하지 않는다(WF:765-766의 현행 동작: "A queued command joins the running turn, whose owner posts its answer"). 그 턴의 relay가 B의 몫을 포함한 답을 한 번 게시한다. B 메시지에는 ➕ "진행 중 턴에 합류"를 단다.
   - `Withdraw`(합류 전 Esc 등으로 회수)·`Delivered`(다른 agent로 전달)는 미전송 종료다. 재전송 요청 알림만 보낸다.
3. **echo가 없다는 사실은 재전송 근거가 아니다.** 도구 없이 생성하는 동안 TUI 대기열에 든 입력의 echo는 앞 턴이 끝난 뒤에야 나온다(실측 S1LD). 기한이 지나도 §5.1-3대로 재전송 요청만 한다.
4. **앞 턴의 relay는 자기 턴 끝에서 멈춘다.** A의 relay가 B의 자기 턴 출력까지 A 몫으로 게시하면 B가 두 번 게시된다(`JoinDoublePost`와 같은 결과). 반대로 B의 관찰자가 합류한 B 이후의 A 꼬리를 B 답으로 relay해도 이중 게시가 된다.
5. **합류 표시는 Claude 합류에만 단다.** Codex Tab은 A 뒤 자기 task로 돈다. 별도 답이 나오므로 ➕를 달지 않는다. Codex Enter가 바쁜 턴에 들어가는 경우(steer)는 규칙상 일어나지 않는다(§2.3).

### 6.2 표시

- ➕ 반응은 이미 있다(`turn_view_reconciler/injected.rs:204-231` `note_busy_injected`, Idle 관측에서 ✅로 바뀜 `:234`, `turn_presence/supervisor.rs:884`).
- 바뀌는 것은 다는 시점이다. 키 효과 시점이 아니라 echo가 합류로 분류된 순간에 단다.
- 반응의 뜻("진행 중 턴에 합류: 이 입력의 답은 진행 중 턴의 답에 합쳐 게시됨")은 기존 큐 상태 안내(`queue_reactions/queue_status_presentation`)의 범례에 한 줄 더한다.
- 자기 턴을 연 입력은 기존 답·footer 흐름을 그대로 탄다.

### 6.3 Codex

- Tab으로 대기한 B는 A의 task_complete 뒤 자기 task를 연다(실측 S1T, 1회).
- 0단계 §3-3에서 S1T의 B 답은 A의 turn bridge 범위 안에서 게시됐다. 결과는 맞았지만, 이 장부상으로는 B가 A 턴에 귀속된다(토큰 귀속).
- 목표: Codex에도 echo 기준 관찰자를 둔다. rollout에서 B의 user marker가 연 task를 B의 답으로 relay하고, A의 bridge는 A의 task_complete에서 멈춘다.
- 본체는 새 child `codex_tui/tab_followup.rs`에 둔다(`codex_tui/warm_followup.rs`는 1328줄 giant).
- 여러 건 Tab의 순서와 귀속은 i8328tab1 실측 결과로 확정한다(§12).

## 7. 입력 출처 분류표

설계 준비 표 13행에 목표 처리를 붙였다. "바로 주입"은 §2.2 보류 조건만 확인한 뒤 넣는다는 뜻이다. "키 선택"은 Codex에서 §2.3을 거친다는 뜻이다.

| # | 출처 | 진입(준비 보고서 대조) | 목표 처리 | 근거 |
|---|---|---|---|---|
| S1 | 사람 owner front(본 채널 Discord) | IG:120 → `intake_dispatch.rs:815` → `ingress.rs:407` | **바로 주입**(Claude), **키 선택**(Codex) | 사용자 결정 |
| S2 | 허용 봇·announce·wake 메시지 | `ingress.rs:418-425`(`bot`은 표식일 뿐, origin=Discord) | **S1과 같다**(U4: 사람 취급 유지) | 아래 U4 |
| S3 | busy follow-up retry | 옛 저장소 `busy_followup_retry_store.rs:241`. owner 채널은 V2 `control.rs:108-115`가 RequeueFront를 거절 | **따로 없음**. 보관 front가 머리에 남아 다음 wake에 S1을 다시 탄다. 보관 사유가 §2.2만 남는다 | busy 사유 소멸 |
| S4 | 루틴 | `routines/agent_executor/owner_input.rs:180,219` | **보관**(START_WAITS, 기존) | U2 |
| S5 | 예약 메시지 | `scheduled_messages/owner_input.rs:198` | **보관**(S4와 같다) | U2 |
| S6 | HTTP start·deliver(wake-watch 포함) | `agents_turn_deliver.rs:137` → OI:713 `admit_start` | **보관**(S4와 같다, `external_turn_conflict` 409 유지) | U2 |
| S7 | Decision | `health/turn_deliver/decision_target.rs:52` | **보관**(턴 경계에서 원본 하나로, PG journal) | origin.rs:33-35 계약 |
| S8 | iMessage | `agents_turn_deliver/origin_admission.rs:24` | **바로 주입/키 선택**(S1과 같다, reply_context를 inject로 옮긴다) | §4 |
| S9 | effort 이어가기 | `effort_restart/tmux.rs:359`, OI:355-359 | **보관**(세션 수명 입력, H6) | 기존 |
| S10 | 재시작 seed | `restart/herdr.rs:292`, `restart_seed.rs:95` | **보관**(greeting_permit) | 기존 |
| S11 | 첨부 대기 입력 | `ingress.rs` Hold::Uploads, `input_payload.rs:143` | **보관**(H4) → 준비 뒤 바로 주입/키 선택 | §2.2 |
| S12 | 스레드 입력 | IG:228, 스레드 채널마다 자기 V2 owner | **스레드 채널마다 S1 정책**. 부모 채널의 THREAD-GUARD(IG:870-926, 디스패치 스레드 실행 중 부모 봇 메시지 큐잉)는 명시 보류로 남긴다 | 주입 결정이 아니라 순서 |
| S13 | TUI 직접 입력(사람이 pane에 직접 침) | owner를 거치지 않음(ExternalInput 관측) | **owner 입력이 아니다.** 사람 초안(H1)의 근원이다. 외부 턴은 더 이상 사람 owner 입력을 막지 않는다(B2 삭제) | §3 |

### 사용자 미정 항목 결정 (코드 기준)

- **U2 기계 시작 경로: 남긴다.**
  - `target::route`(target.rs:76-78)가 `!joins_running_turn && Busy`를 `synthetic_waits_idle`로 보낸다.
  - `execute_owned`가 기계 시작에 한해 pane을 한 번 읽는다(OI:1219-1243).
  - 루틴의 `routine_delivery`(`v2_types.rs:124`)와 Decision PG journal(`decision_drain.rs:212`)은 "자기 턴에서 시작해 자기 턴에서 끝난다"를 전제로 receipt를 정산한다. 바로 주입으로 바꾸면 이 정산 계약이 깨진다.
  - 사용자 결정의 범위도 owner 사람 입력이다.
- **U3 재시작 직후 `reconcile_done=false`: 사람 입력은 토큰 유무와 무관하게 보류(H5).**
  - 지금은 busy 가지만 보류하고(OI:270-278), idle 가지는 곧바로 시작한다.
  - 곧바로 시작하면 B가 빈 slot을 먼저 잡는다. 그러면 복구 kickoff가 거절되거나(`recovery_kickoff.rs:39` `kickoff_refusal`, 빈 slot만 claim), B의 echo가 복구 watcher의 relay 범위(`last_offset`부터)에 들어가 복구 턴에 귀속된다(R5).
  - 보류는 부팅 동안으로 한정되고 R8 wake로 풀린다.
- **U4 허용 봇 출처: 사람 취급을 유지한다.**
  - `ingress.rs:418-425`의 `bot`은 작성자 표식이고 origin은 Discord다.
  - #8328 댓글 3의 관측(사람 2·announce 봇 1이 같은 결과)과 0단계 실측의 (O) 경로(명령봇 `bot=announce`)가 모두 이 정책 위에서 나왔다.
  - 봇 메시지를 기계 시작으로 바꾸면 START_WAITS에 묶여 바쁜 턴 중에는 넣지 못한다. 그러면 announce 기반 시험·wake-watch 회신이 지금보다 늦어진다.
  - 부모 채널 봇 메시지에 대한 THREAD-GUARD는 그대로 둔다.
- **U1 사람 초안: 알림만**(메인 판단). `TargetDraftStuck` RED를 수용한 결과로 기록한다.

## 8. 재시작·부팅·incarnation 경로

재시작을 넘는 것은 V2 큐 파일뿐이다(`pending_queue_persistence.rs:95-106`). 이 설계는 여기에 `awaiting_echo`를 더한다. 메모리에만 있는 것(`Owner.held`, restart_gate ACTIVE, held_watch WATCHES, drive LANES)은 사라지고 R6 복구가 다시 세운다.

| 대상 | 재시작 전 상태 | 부팅 순서에서 일어나는 일 | 결과 |
|---|---|---|---|
| 보관 입력(H1-H11로 NotSent, 키 0) | V2 `items` 머리 | R6 `owner_input/native_input.rs:188-214` `recover` → `boot_ready`면 wake → U3로 `reconcile_done`까지 H5 보류 → R8 wake → `inject::hand` | 정확히 1회 주입(키 0이었으므로 중복 없음) |
| 키를 누른 뒤 echo 전(귀속 대기, 합류·자기 턴 장부) | `awaiting_echo` 영속. 합류 장부면 토큰 행의 `native_stop_busy_children`에도 있음(`native_stop_pin.rs:183-210`) | R2 native 정산 → R5 `settle_boot`·토큰 재건·busy child 재등록 → **새 단계**: 미해소 `awaiting_echo`마다 키 시점 offset부터 echo 관찰자를 다시 단다(새 child `recovery_engine/awaiting_echo_boot.rs`, `restore_inflight.rs`에는 호출 한 줄) | echo가 오면 §6 규칙으로 귀속한다. 기한을 넘기면 retained 재전송 요청. **재주입은 없다** |
| 합류한 입력(echo 분류 끝, A 턴 안) | 항목 해소됨. B 몫은 A 턴 relay 범위 안 | R5가 A의 복구 watcher를 `last_offset`부터 다시 단다 | B 몫은 A 답에 한 번만 실린다. ➕ 표시는 이미 달려 있다 |
| 자기 턴을 연 입력(관찰자 relay 중) | 관찰자 스레드는 프로세스와 함께 사라짐. 토큰이 B면 B 행이 R5로 복구됨 | 토큰 B: R5 복구 watcher가 이어 relay. 합류 장부(토큰 A 아래 자기 턴): 위 새 단계가 echo offset부터 다시 단다 | B 답 1회 |
| 재시작 중 진행하던 키 입력(키 중간) | `head_injection` phase(V2) | R6 `recovery.rs:8-44`: 확인 기한이 지났거나 Unknown이면 Unconfirmed retained, 증명된 미전송만 재적재(`retained.rs:444`) | 현행 유지(`NoDuplicate`) |
| 세션 교체(incarnation: `!clear`, Herdr pane 교체 w82→w83, 재시작으로 새 세션) | 옛 session/transcript에 묶인 `awaiting_echo` | clear(A3)·terminate(A2)·R13 carried close가 옛 incarnation 항목을 Cancelled/HostTerminated로 정산하고 알림. 새 세션 transcript에서 옛 nonce를 찾지 않는다 | 옛 입력은 재주입 없이 알림으로 끝 |
| R13 carried episode | 이전 generation 토큰이 mailbox를 쥔 채 남음 | 사람 입력은 합류 장부로 carried 토큰 아래 주입된다. echo 관찰자가 귀속을 정하고, carried watcher는 자기 턴 끝에서 멈춘다(§6.1-4) | 옛 episode로 잘못 귀속되지 않는다. S2 시험으로 고정 |
| R9 writer boot, R10 restart gate, R11 preclaim, R12 Herdr 행 보류 | 각자 보류 | 그대로. preclaim은 합류 장부에도 적용(§4) | |

"재시작 직후 무한 보류"가 다시 생기지 않는 근거:
- 목표 경로에는 토큰 해제를 기다리는 보류가 없다.
- H5는 R8로 끝난다. 부팅 실패는 별도 결함 부류다.

## 9. 인접 경로 전수

◆는 #8432와 직접 겹치는 함수다(표시만 하고 #8432가 착지한 결과를 따른다). ◇는 토큰 해제 시점에 간접으로 기대는 곳이다.

| # | 경로 | 진입 | 목표 계약 | 필요한 조정(조각) |
|---|---|---|---|---|
| A1a | HTTP Cancel(보존) | `queue_api.rs:209` → `queue.rs:354` → `judged_stop.rs:59-69` `judge` | 합류 장부 입력은 토큰 A의 busy child라 A의 stop이 함께 멈춘다. 토큰이 없을 때(`Ok(None)`) `awaiting_echo`가 있으면 Claude는 `owned_input_stop`(`judged_stop.rs:458`)을 쓰고, Codex는 Esc 1회(대기열이 있으면 Codex가 Esc를 "중단 후 즉시 전송"으로 처리한다는 점을 알림에 남긴다)를 보낸다. cancel 뒤 drain은 인터럽트 정산 뒤로 미룬다(H11) | S2 |
| A1b | HTTP Cancel(force) | `queue/http_force.rs:53` | 세션을 죽인다. 진행 중 `awaiting_echo`를 HostTerminated로 정산한다 | S2 |
| A1c | `/cancel-queued` | `commands/control.rs:772` → V2 `control.rs:367` | 유지(키를 누른 입력은 이미 큐 밖) | 없음 |
| A2 | Herdr Terminate | `herdr_terminate.rs:151` `capture_turn`, `:514` 전이 중 재확인, `herdr_terminate/retired_wake.rs:47` | 주입이 전이 잠금을 키까지 쥐므로 `:514` 재확인이 본다(§4). 잡은 토큰과 그 `awaiting_echo`를 HostTerminated로 정산한다. never-started 거절 규칙은 유지 | S2 |
| A3 | clear(`/clear`·`!clear`·idle 자동) | `commands/control.rs:548,835`, V2 `clear.rs:17,154`, `session_idle_cleanup.rs:87,180` | `protected`에 귀속 대기 id를 넣는다(§5.1). 보류 front는 지금처럼 버린다("대기 메시지 n건을 비웠어요"). `clear_epoch`·전이 잠금 유지. clear 직후 composer 미준비는 Hc 보류 | S2(protected), S3(Codex Hc) |
| A4a | `/stop`·`!stop` | `commands/control.rs:674`, `judged_stop.rs:352,449-458`, `claude_stop_identity.rs:76` | busy child 고정을 inject로 옮겨 native stop 확인을 유지한다. 합류 전 Esc로 회수된 입력(`Withdraw`)은 미전송 종료로 보고 재전송 요청을 보낸다. stop 직후 입력은 H11 | S1(busy child), S2(Withdraw 정산) |
| A4b | agent Stop·Herdr stop | `agents_stop.rs:45`, `codex_stop_delivery.rs:251,417,772` | Codex Tab 입력은 A 뒤 자기 task로 돈다. 이것은 "토큰이 시작한 턴"이 아니라 later native turn이라 지금은 409다. `awaiting_echo`(장부: 합류, Codex)를 근거로 B task의 stop을 허용한다 | S3 |
| A5a | stale release(intake) | `stale_turn.rs:219,286`(IG:946, IG:1009) | 고아 행 해제는 유지한다. 입력 결정권은 뺀다(§3 IG 행) ◆ | S4 |
| A5b | 운영자 lease release | `turn_lease.rs:71,148,273` | 토큰 에피소드는 유지한다. inspect에 `awaiting_echo`를 노출한다. #8432의 already_released 응답 수리와 겹친다 ◆ | S2 |
| A6 | #4030 stale-FOREIGN demotion | `tui_direct_pending_start/foreign_recovery.rs:74,365`, `destructive_cancel_gate.rs:234` | `destructive_cancel_gate`가 `awaiting_echo`가 있는 채널의 pane을 "살아 있음"으로 본다. 주입됐지만 TUI 대기열에 있어 pane이 ready·무진행일 때 옛 행이 demote되는 것을 막는다 ◆ | S2 |
| A7 | turn presence supervisor wake | `turn_presence/supervisor.rs:657`(unknown 알림 조건 `token \|\| head`), `:695`(wake), `:884`(➕→✅) | wake: owner 채널 보관 front는 토큰과 무관하게 Idle 전이에서 깨운다. unknown 알림 조건에 `awaiting_echo`를 더한다(사람 경고 문구 제거는 #8328 DoD 1의 후속 범위). ✅ 전환이 echo 전에 표시를 지우지 않게, ➕는 합류 분류 뒤에만 단다(§6.2) ◇ | S2 |
| A8 | #3277 deadline | `turn_finalizer/reconcile.rs:223,270,331`, `watcher_backstop.rs:51` | 토큰 턴은 유지한다. 귀속 대기는 자기 echo deadline(§5.1)으로 끝난다. finalize 뒤 `publish_claimed_queue_eligible`은 보관 front용으로 남는다. 당긴 deadline 연장 결함은 #8432 몫 ◆ | S2 |
| A9 | #8430 보류 상한 | (#8430 본문) | 작은 안전망으로 유지한다. 대상은 Idle이 끝내 판정되지 않아 생기는 보류(H8 화면 경합 지속 등)다. 사람 초안·모달·작업 중 화면에는 적용하지 않는다(U1). 이 설계가 busy 보류를 없애므로 상한이 실제로 걸릴 일은 줄어든다 | 별도 이슈 |

## 10. #8432·#8430과의 경계

- #8432(끝난 턴 terminal 거부·release 응답)는 별개로 착지한다.
  - 겹치는 함수(표시만): `health/turn_deliver.rs:131,166`(B2·B36), `router/intake_gate/stale_turn.rs:219-302`(A5a), `turn_lease.rs:71-334`(A5b), `turn_finalizer/reconcile.rs:223-358`(A8), `tui_direct_pending_start/foreign_recovery.rs:74,365`(A6).
  - 이 설계의 조각은 #8432가 착지한 main 위에서 rebase 없이 새 브랜치로 시작한다.
- 목표 경로는 입력 liveness를 토큰 해제에 기대지 않는다(`Target8432` GREEN).
  - 그래도 Cancel·stop·terminate·lease가 토큰을 손잡이로 쓰므로 #8432 수리는 여전히 필요하다.
  - 모델에서도 토큰 A는 남는다.
- #8430은 안전망으로 남는다. 이 설계는 #8430의 동작을 흡수하지 않는다.

## 11. 구현 조각 분할

크기는 추정이다(prod 코드 순증, 시험 제외). 측정은 착지 레그에서 `pr_cap_prod.py <base> HEAD`로만 한다. 이 레그의 실측(문서·모델만)은 보고서에 적는다. 모든 조각은 30파일·+1500 상한과 +1200 PLAN_READY 선 아래로 잡았다. giant 파일(OI·IG·IT·WF·`codex_tui/input.rs`·`codex_tui/warm_followup.rs`·`restore_inflight.rs`·`queue_effects.rs`)에는 블록 이동·호출 교체·enum 변형 추가만 한다.

순서: **S2 → S1 → S4 → S3**.
- 손잡이(S2)를 먼저 넣는다. 그래야 S1이 동작을 바꿀 때 clear·terminate·stop이 귀속 대기 입력을 볼 수 있다.
- S3은 i8328tab1 결과를 기다린다.

### S2 — 귀속 대기 손잡이 (동작 변화: 기존 hand_busy·자기 턴 주입도 등록만 한다)
- 파일(prod 약 12개):
  - 새 child: `v2/awaiting_echo.rs`(+170), `recovery_engine/awaiting_echo_boot.rs`(+70)
  - 수정: `v2_types.rs`(+12), `v2.rs`(+20), `v2/clear.rs`(+8), `herdr_terminate.rs`(+25), `turn_bridge/tmux_runtime/judged_stop.rs`(+35), `turn_lease.rs`(+15), `turn_presence/supervisor.rs`(+15), `turn_finalizer/reconcile.rs` 또는 `v2/retained.rs`(+30), `destructive_cancel_gate.rs`(+10), `restore_inflight.rs`(호출 +3)
- 예상: prod 12파일 / 순 +430.
- RED 계획(시험을 먼저 쓰고 현행에서 실패를 확인):
  1. 합류 입력이 있는 채널에서 clear하면, 현행은 B id가 `protected`에 없어 머리가 버려진다. 목표는 보존·정산이다.
  2. terminate가 `awaiting_echo`를 HostTerminated로 정산한다(현행에는 항목 없음).
  3. echo 기한을 넘기면 retained 재전송 요청 1회, 키 1회 유지(재주입 0).
  4. 부팅 시 미해소 항목에 관찰자를 다시 단다.
- 검증: V2 단위 시험, `queued_head` owner_tests, clear·terminate 시험군.

### S1 — 사람 입력 단일 경로(Claude 바로 주입·합류)
- 파일(prod 약 9개):
  - 새 child: `owner_input/inject.rs`(+300), `intake_turn/owned_busy/late_start.rs`(블록 이동 +60)
  - 수정·삭제: OI(-200: hand_busy·paste_busy·into_busy_pane·unobserved 삭제, owner_drive 호출 교체), `owner_input/busy_turn.rs`(삭제 -41), OB(-250: running·legacy_diagnostic·late 사람 가지), IT(호출 2곳 -20), WF(합류 분류 콜백 +12), `turn_view_reconciler/injected.rs`(+15), `busy_retry.rs`(+10)
- 예상: prod 9파일 / 순 약 -110(삭제가 더 많다).
- RED 계획:
  1. **#8432 모양**: 토큰 A가 남고 화면이 Idle일 때 사람 입력. 현행은 `NotSent("busy_source_unavailable"/"busy_turn_unobserved")`를 반복한다(drive_tests). 목표는 Submitted 1회와 echo 관찰자 설치다.
  2. 토큰 없는 가지의 전이 잠금: clear가 잠금을 쥐면 `session_transition_busy`(현행 idle 가지는 잠금이 없어 주입한다).
  3. U3: `reconcile_done=false` + 토큰 없음 → 보류(현행은 시작한다).
  4. 합류 echo(`absorbed_mid_turn`) → B 별도 게시 0회 + ➕ 1회. 자기 턴 echo → B 답 1회.
  5. iMessage reply_context 보존.
  6. native busy child가 키 전에 고정된다(현행 시험 이전).
- 검증: owner_input drive_tests, effect herdr_tests, native_stop_pin 시험, intake_turn 시험군.

### S4 — intake 적재·wake와 재시작 정리
- 파일(prod 약 5개): IG(owner 채널 적재 뒤 wake, -30/+25), `intake_gate/queue_effects.rs`(+10), `mailbox_probe.rs`(+5), `queue_io/input_owner/ingress.rs`(+5), `stale_turn.rs`(결정권 제거 -10).
- 예상: prod 5파일 / 순 +5.
- RED 계획:
  1. owner 채널에서 blocking 토큰이 있을 때 적재하면, 현행은 idle drain·wake가 없다(B39). 목표는 owner wake 1회다.
  2. `restore_inflight` kickoff → `complete_active_reconcile` 순서 고정(H10 ⊂ H5).
- 검증: intake_gate 시험군, recovery_flush 시험.

### S3 — Codex 키 선택 (i8328tab1 결과 반영 뒤)
- 파일(prod 약 9개):
  - 새 child: `codex_tui/tab_followup.rs`(+130), `codex_tui/busy_inject/key_choice.rs`(+60)
  - 수정: `session_host/submit.rs`(+8)와 host 매핑 `tmux_host.rs`·`herdr_gate.rs`(+20), `codex_tui/input.rs`(enum 변형 +6), `codex_tui/busy_inject.rs`(+15/-10), `codex/herdr_turn/busy.rs`(+15/-5), OI(1714-1720 진단화 -5/+5), `codex_stop_delivery.rs`(+20)
- 예상: prod 9파일 / 순 +280.
- RED 계획:
  1. Codex(Herdr) 바쁜 화면 + 사람 입력. 현행은 `NotSent("busy_turn: transcript unavailable")`(rollout 미확정) 또는 Enter steer다. 목표는 Tab 키 1회다.
  2. idle 화면이면 Enter.
  3. Unknown이면 실측 규칙.
  4. 세션 전환 직후 readiness 증명 전이면 Hc 보류.
  5. Tab 입력 B의 답이 B의 task로 귀속되고 A bridge는 task_complete에서 멈춘다.
  6. 키 경합(읽기 뒤 A 종료) 고정 시험. 실측이 "걸림"이면 Tab 뒤 Enter 1회 마무리를 넣는다.
- 검증: codex busy_inject 시험군, codex_herdr_tests, effect 시험.

### 합계
- 추정 prod는 서로 다른 파일 약 33개(조각별 9-12개), 순 코드 약 +605(+430, -110, +5, +280)다.
- 조각별 최대치는 S2의 12파일·+430이다. 모두 PLAN_READY 선(+1200) 아래다.

## 12. Codex 키 규칙 (자리 — i8328tab1 결과 대기)

맥 실측 레그 i8328tab1(`/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i8328/report-tab-1.md`)이 끝나면 코디가 알린다. 아래 칸을 채우고 모델 cfg `CodexTabIdleStuck`·`CodexUncertainTab` 중 어느 쪽이 현실인지 정한다. S3은 그 뒤에 시작한다.

| 칸 | 값 | 근거 |
|---|---|---|
| 바쁨 | Tab (잠정, 0단계 S1T 1회) | 대기 |
| idle | Enter (0단계 S4D2·S3) | 대기 |
| 불확실 | 대기 (모델상 안전한 기본값은 보류. Enter는 금지) | 대기 |
| idle 화면 Tab의 피해 | 대기 (`TabAtIdle` = submits / stuck) | 대기 |
| 바쁜 중 Tab 여러 건 순서·귀속 | 대기 | 대기 |
| `/clear` 뒤 composer 준비 | Enter 2s 성공 / 0.3s 실패(0단계), Tab 대기 | 대기 |
| 바쁨 판정 화면 표식 | 대기 (후보: 상태줄 `tab to queue message`, 작업 중 표시) | 대기 |

## 13. 남은 위험·미확인

1. **A relay가 자기 턴 끝에서 멈추는지**(§6.1-4). Claude 토큰 턴 bridge와 tui_o relay가 B의 자기 턴 출력을 A 몫으로 게시하지 않는지 코드로 끝까지 확인하지 못했다. S1 RED 4번이 고정한다. 깨지면 S1 범위 안에서 A relay 종료 조건을 turn 종료 기록으로 묶는다.
2. **Claude Herdr busy submit의 managed stash(`busy/herdr.rs:77-98`)**: 사람 초안이 있으면 C-s로 보관한 뒤 B를 제출한다. 초안 자체는 제출하지 않으므로 U1과 충돌하지 않는다고 판단했다. 메인이 "초안이 보이면 무조건 보류"를 원하면 S1에서 이 가지를 보류로 바꾼다(+10줄).
3. **THREAD-GUARD 큐를 비우는 wake**(S12)는 미확인이다. 이 설계는 THREAD-GUARD를 바꾸지 않는다.
4. **0단계 §3-5**의 `response_sent_offset` invariant ERROR는 기존 결함이고 이 설계 범위 밖이다.
5. **Codex Esc + 대기열 동작**: Codex는 대기열이 있을 때 Esc를 "중단 후 즉시 전송"으로 처리한다(0단계 S1D 화면 문구). Cancel 뒤 Tab 대기 입력이 바로 시작된다. 큐 보존 Cancel 계약상으로는 허용되지만 알림 문구에 남긴다(A1a).
