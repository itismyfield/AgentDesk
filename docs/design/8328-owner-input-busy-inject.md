# #8328 1단계 설계 (w5): owner 입력 바쁜 중 주입 — S1(Claude 바로 합류 + 답 anchor 귀속) 먼저, S2(Codex 항상 Tab + 채널 lease)는 뒤로

<!-- doc-path-refs: proposed src/services/claude_tui/hosting/deferred_source.rs src/services/cluster/execution_capacity/channel_lease.rs src/services/tui_input/human_frame/history/v2/key_admit.rs src/services/codex_tui/busy_inject/tab_protocol.rs src/services/discord/recovery_engine/awaiting_echo_boot.rs src/services/discord/router/message_handler/intake_turn/owned_busy/late_start.rs src/services/discord/router/message_handler/provider_dispatch/owner_input/inject.rs src/services/tui_o/store/anchor_bindings.rs src/services/tui_o/writer/deliver/prepared.rs src/services/tui_o/writer/reply_anchor.rs src/services/turn_orchestrator/queued_head/v2/awaiting_echo.rs -->

- 기준 코드: `origin/main` = `82702fe252`. 브랜치는 `wt/i8328-design`이다(w1 `cafed27b40`, w2 `1113953fa4`, w3 `9b934cfcb8`, w4 `c603bfb08c` 위에 이어서).
- 결정 출처:
  - 사용자 결정 busy-inject-step0-1126 a (13:01)
  - 메인 판단·코디 결정 (13:20): Codex는 항상 Tab, 1단계 핵심 범위는 귀속
  - 설계 리뷰 i8328-design-2(`1113953fa4`, P1 2·P2 4): w3에서 모두 반영했다. 반영 위치는 아래 표와 같다.
  - 설계 리뷰 i8328-design-3(`14e9998f50`, P1 3·P2 5)과 메인 순서 결정(15:02): 단계를 S1·S2로 나눈다. 사용자 결정 busy-inject-step0-1126 a의 범위 안에서 순서만 나눈 것이다(아래 "단계 분리").
  - 설계 리뷰 i8328-design-4(`c603bfb08c`, P1 3·P2 1)와 사용자 결정 i8328-s1-scope-1126 c(S1 범위 유지): w5에서 S1 P1 세 건을 닫는다(아래 "w5 반영").
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

## 단계 분리 (w4)

| 단계 | 범위 | 이번 판 | 리뷰 i8328-design-3 |
|---|---|---|---|
| **S1** | Claude 바로 합류 + 답 anchor 귀속 + 합류 표시. transcript 없는 정상 입력 보류 0 | **수렴**(§0-§12) | P1-3 해소(§5.5). P2-3(➕) 닫음(§6.2-2). P2 managed stash 확인 지점(§7.1), P2 출력 송신 경계(§4, S1-D 시험) 반영 |
| **S2** | Codex 항상 Tab + 채널 lease·slot 공유 표 | 설계 항목으로만 남김(§13) | P1-1(lease 등록·반납을 모든 실행 경로에), P1-2(`join`을 임시 예약으로, 미전송이면 해제), P2 Codex 대기열 소멸, P2 lease 복구, P2 모델 가드 |

- **`s1_independent = yes`**: S1은 채널 lease·slot 새 표 없이 간다. Claude 합류 경로는 실행 중 턴 A가 이미 잡은 lease를 그대로 쓰고, 새로 잡지도 slot을 더 쓰지도 않는다. 코드 근거는 §1.1이다.
- 사용자 체감 문제(Claude 채널 무한 보류: #8432 모양, source 결합 전 보류)를 S1이 먼저 없앤다.
- S2 모델 `OwnerInputBusySplit.tla`(cfg 30)는 w3 상태 그대로 둔다. S1 모델은 새 `OwnerInputBusyS1.tla`(cfg 24)다.

### w5 반영 (리뷰 i8328-design-4)

| 지적 | 결정 | 반영 위치 |
|---|---|---|
| P1-1 꼬리표 없는 Idle·Cold opener를 J2가 결합하지 못함 | J2는 기존 native membership receipt(ticket·row·range·frame hash)로 결합한다. 헤더 nonce는 보조 근거다. 프롬프트 바이트는 바꾸지 않는다. 짝이 하나가 아니면 결합하지 않는다(오귀속 0) | §6.1 J2, §6.2-1·5, 조각 S1-A, 모델 `TaggedHeaderOnly` RED |
| P1-2 E1–E8 밖 source 관문(idle observer → `idle_unobserved`, Herdr `Context::read` → Held) | 관문 전수표 G1–G10을 만들었다. 키 허가 관찰(pane·writer·세대·composer)과 source 관찰을 나눈다. tmux 자기 턴 observer는 source를 키 뒤에 기다린다 | §5.5.4, 조각 S1-B, 모델 `RequireIdleObserverBeforeKey`·`RequireBusyReadStateWithoutSource` RED, liveness `NoCircularWait` |
| P1-3 지연 pin이 `select_source` 동등성 검사에 걸림 | `BindingIntent.selected`의 None → Some을 단일 확정점으로 쓴다. 지연 분기에서는 미정 session·path 비교를 하지 않고, 확정 뒤 변경은 계속 거절한다 | §5.5.3, 조각 S1-B, 모델 `DeferredEquality` RED |
| P2 GREEN이 늦은 결합을 보장하지 못함 | transcript 생성(`trans`)과 `Bound` 기록(`src`)·확정(`sel`)·결합(`aBound`·`bBound`)을 나눴다. 게시와 ➕는 결합을 소비한다 | §1.0, 모델 `NoLateBind`·`RequireTranscriptFresh` RED |

w3 반영 표(리뷰 i8328-design-2). "S2"라고 적힌 행은 §13으로 옮겼다.

| 리뷰 지적 | 결정 | 반영 위치 |
|---|---|---|
| P1-1 Codex Tab마다 capacity 획득 → 같은 채널 lease 충돌 | (S2) 채널 실행 lease는 하나다. 수락된 후속 입력은 실행 중 턴의 lease를 이어 쓰고, 마지막 후속 턴이 끝난 뒤 반납한다. 리뷰 i8328-design-3 P1-1·P1-2가 이 결정의 연결 누락을 지적했다 | §13.2-§13.3, S2 모델 `AcquirePerTab`·`NonceOverwrite` RED |
| P1-2 BusyClaude가 transcript 없이 `TranscriptUnavailable` | 키 전에 anchor·nonce·frame hash·target incarnation을 V2 `pre_source`로 등록하고, source는 나중에 결합한다. w4에서 어댑터 앞의 두 검사(`admits`, `pin_source`)까지 넓혔다 | §5.5, 조각 S1-B, S1 모델 `RequireTranscript`·`AdmitsPathMatch`·`PinSourceFirst` RED |
| P2-1 수락된 Codex 입력에 30초 미확인 | `AwaitingAcceptance`와 `AcceptedAwaitingNative`로 나눈다. 수락이 짧은 기한을 끝낸다. 대기열에서 사라진 것만으로는 미실행으로 판정하지 않는다(w4) | §5.1, 조각 S1-C(Claude), §13(Codex) |
| P2-2 기존 stash + 현재 사람 초안을 제출하는 경로 | 사람 소유이거나 소유를 모르는 composer는 같은 잠금 안에서 키 전에 보류한다. managed stash 예외는 안전 보관과 빈 composer가 증명될 때만이다 | §2.2 H1, §7.1, 조각 S1-D |
| P2-3 실제 합류 전 ➕ | 목표에서 ➕는 흡수 증거 뒤에만 단다. 별도 턴이면 ➕를 달지 않고, 이미 달린 것은 거둔다 | §6.2-2, §4, 조각 S1-A, S1·S2 모델 `MarkOnPaste` RED·`MarkOnlyJoined` |
| P2-4 `channel_accepts`를 출력 소유로 오해 | 관찰자 게시는 durable output ownership(`tui_o/cutover`)으로 정하고, `channel_accepts`는 readiness로만 쓴다 | §4, §12-1, 조각 S1-D |

## 0. 한 줄 요약 (S1)

1. **귀속(핵심)**: 게시하는 답은 모두 입력 anchor(사람이 보낸 Discord 메시지) 하나에 reply로 건다.
   - 어느 anchor에 걸지는 transcript에서 그 답이 속한 native 턴과, 그 턴을 연(또는 그 턴에 합류한) 입력의 frame nonce로 정한다.
   - 게시 순서, mailbox 토큰, 주입한 경로로는 정하지 않는다.
   - 합류 입력은 합친 답 한 번으로 친다. 답은 그 턴을 연 입력의 anchor에 건다. 합류한 입력에는 ➕ '진행 중 턴에 합류' 표시를 단다. 다는 시점은 흡수 증거(J2)가 나온 뒤이고, 별도 턴이면 달지 않거나 거둔다.
   - J2의 결합 근거는 native membership receipt(등록 ticket ↔ 검증된 user 행의 row·range·frame hash)다(w5). 꼬리표 없는 정상 Idle·Cold 입력도 이것으로 결합한다.
2. **Claude 바로 주입**: 바쁜지 확인하지 않는다. 보류 조건(§2.2)만 보고 붙여넣기+Enter를 한다. 다음 도구 경계에서 흡수되면 합류, 아니면 자기 턴이다.
3. **transcript 없는 키**: 키 전에 anchor·nonce·frame hash·target incarnation을 durable하게 등록한다. source(transcript 경로·offset)는 나중에 확정·결합한다. source가 결합되지 않았다는 이유로는 보류하지 않는다. 키 전 source 관문은 E1–E8(§5.5.1)과 G1–G10(§5.5.4) 전부다. pane·writer·실행 세대 검사는 그대로 둔다. 지연 pin의 source는 `BindingIntent.selected`에서 한 번만 확정한다(§5.5.3).
4. echo가 아직 없다는 사실은 재전송 근거가 아니다(w1 그대로).
5. **실행 용량**: S1은 새로 잡지 않는다. 합류 입력은 A의 기존 lease로 돈다(§1.1). 채널 lease 공유 표는 S2다.
6. Codex는 S1에서 지금 동작 그대로 둔다. 항상 Tab은 S2(§13.1)다.

## 1. 상태 차트 (0단계)

### 1.0 S1 모델

모델은 `docs/statecharts/input-single-path/OwnerInputBusyS1.tla`(Claude만)이고 cfg는 33개다(w4 24개). 보고서는 `owner-input-busy-s1.report.md`이고 w4 → w5 전후표가 거기에 있다.

변수:
- `trans`: transcript 파일 있음. B의 턴이 만든다(`TuiStart`)
- `src`: writer의 `Bound` 기록(`BindSource`, 파일이 있어야 한다)
- `reg`: 키 전 durable 등록
- `pin`: recorded·deferred
- `sel`: `BindingIntent.selected` 확정(`Select`, 한 번)
- `aBound`·`bBound`: J2 결합(`BindA`: 꼬리표 없는 opener를 receipt로, `BindB`: B)
- `why`: 보류 사유

A 답은 A 결합 뒤(합류 B가 있으면 B 결합도 뒤)에, B 답은 B 결합 뒤에 나간다. ➕는 `BindB`(흡수 증거)에서 단다.

시나리오: `busy`, `i8432`, `external`, `fresh`(파일도 `Bound`도 없음), `rebind`(파일은 있고 `Bound`가 늦음)

규칙:
- INVARIANT: `NoDuplicate`·`NoSilentLoss`·`ReplyExactlyOnce`(합류 시 합친 답 1회)·`JoinedMarked`·`MarkOnlyJoined`·`AnswerBound`·`KeyBeforeLeave`·`KeyAfterRegister`·`NoSourceHold`(transcript 없는 정상 입력 보류 0)
- PROPERTY: `IdleDelivers`·`ReplyPosted`·**`NoCircularWait`**(w5: transcript 없고 보류 조건 없는 대기 입력은 결국 키를 받는다)

`statecharts_tlc.check_cfg`로 33개를 모두 직접 돌렸고, 결과가 전부 기대 집합과 일치했다.

| 묶음 | cfg | 결과 |
|---|---|---|
| Current RED 4 | `Current8432` | IdleDelivers·ReplyPosted·MarkOnlyJoined |
| | `CurrentBusy` | MarkOnlyJoined |
| | `CurrentRebind` | NoSourceHold·MarkOnlyJoined |
| | **`CurrentFresh`**(w5 새) | **NoCircularWait**·IdleDelivers·ReplyPosted·NoSourceHold: 첫 입력이 transcript를 만들어야 하는 순환 대기 |
| Target GREEN 7 | Busy, 8432, External, Fresh, Rebind, HoldCleared, ComposerNotReady | 모든 규칙 성립(NoCircularWait 포함) |
| 수용 RED 1 | `TargetDraftStuck` | IdleDelivers·ReplyPosted(U1) |
| mutant RED 21: source 관문 7 | `AdmitsPathMatch`, `PinSourceFirstRebind`, `RequireTranscript`, **`RequireBusyReadStateWithoutSource`**(G2) | NoSourceHold |
| | `PinSourceFirst`, **`RequireTranscriptFresh`**, **`RequireIdleObserverBeforeKey`**(G1) | NoSourceHold·NoCircularWait·IdleDelivers·ReplyPosted |
| ③ 지연 pin | **`DeferredEquality`**, **`DeferredEqualityRebind`** | ReplyPosted(확정 0 → 결합 0 → 답 0) |
| ① receipt | **`TaggedHeaderOnly`** | AnswerBound(A 답이 합류 B에 걸림) |
| P2 늦은 결합 | **`NoLateBind`**, **`NoLateBindFresh`** | ReplyPosted(w4 모델에서는 GREEN이던 변형) |
| 그 밖 | RegisterAfterKey(KeyAfterRegister), MarkOnPaste(MarkOnlyJoined), JoinUnmarked(JoinedMarked), JoinDoublePost·TokenAttribution(ReplyExactlyOnce), PostOrder·NoReference(AnswerBound), ResendBeforeEcho(NoDuplicate), TuiDrop(NoSilentLoss) | |

- liveness 전후: `NoCircularWait`는 Current fresh에서 RED, Target 7개 모두 GREEN이다. ①②③을 되돌리는 mutant(`TaggedHeaderOnly`, G1·G2 관문, `DeferredEquality`)는 모두 RED다.
- w4 결과가 바뀐 cfg는 `TargetPinSourceFirst`(+NoCircularWait)와 `TargetRegisterAfterKey`(AnswerBound가 빠짐: 결합이 등록을 기다리므로 귀속은 지켜지고 KeyAfterRegister만 깨진다) 둘이다.
- 차트(`owner-input-busy-s1.*.chart.mmd`)
  - RED: current-fresh, current-rebind, current-8432, admits-path-match, pin-source-first, idle-observer-gate, deferred-equality, tagged-header-only, mark-on-paste
  - GREEN: target-rebind, target-fresh, target-busy-join, target-8432

S2 모델: `OwnerInputBusySplit.tla` cfg 30은 w3 그대로다(§13.5에 가드 숙제).

### 1.1 S1 독립성 판정: `s1_independent = yes`

질문: S1의 Claude 합류 경로가 실행 중 턴의 기존 lease를 그대로 쓰는가(새 획득·새 slot 없음)?

| 실행 경로 | lease 획득 | guard 수명 | S1에서 |
|---|---|---|---|
| owner 자기 턴 장부(ClaudeIdle·CodexIdle) | OI:1501-1515 `execution_capacity::acquire`. match는 `ClaudeIdle \| CodexIdle`만이고 나머지는 `None`이다 | OI:1568 `drop(capacity)`, until_done 뒤 | 그대로(토큰 없는 B의 자기 턴) |
| owner 합류 장부(ClaudeBusy, tmux·Herdr) | **없음**. OI:1493-1494 주석: "a busy pane writes into the turn that already holds it" | – | 그대로. 합류 B는 A의 guard로 돈다 |
| tmux 스트리밍 턴(cold 시작 포함) | `provider_dispatch.rs:406-411` `_execution_guard` | 그 함수가 반환할 때 | 그대로(H3 세션 없음 → 기존 cold launch) |
| Herdr 턴 실행 | `provider_dispatch.rs:623-628` `_execution_guard` | `provider_dispatch.rs:653`(Codex)·`:670`(Claude) 반환 때 drop | 그대로 |

결론:
- S1은 `execution_capacity`에 획득·보관·이전 호출을 하나도 더하지 않는다.
- 합류 장부 키(Claude busy adapter)는 지금처럼 lease를 잡지 않는다. 흡수된 B는 A의 lease 수명 안에서 돈다.
- 그래서 리뷰 i8328-design-3 P1-1(경로별 등록·반납 누락)과 P1-2(키 못 보낸 입력의 slot 점유)는 S1에서 생기지 않는다. 둘 다 S2의 공유 표가 만드는 문제다.

남는 것(S1이 바꾸지 않는 기존 성질, S2 항목):
- 흡수되지 않은 합류 B는 A 뒤 자기 턴이 된다. 이 턴은 lease 없이 돈다(S2 모델 `CurrentBusy` RED `LeaseCovers`). 지금 hand_busy 붙여넣기와 같은 창이다.
- #8432 모양(토큰만 남고 Idle)에서 S1은 B를 토큰 A의 합류 장부로 붙여넣는다. 이 B도 lease 없이 돈다. 지금은 이 입력이 무한 보류되므로, S1이 "전달됨·용량 미계상"으로 바꾸는 셈이다. 노드 용량을 채널당 최대 1턴 과소 계상할 수 있다. 막으려면 채널 lease 표(S2 P1-1)가 필요하다. 메인 확인 항목으로 남긴다(§12-5).

## 2. 목표 경로

### 2.1 owner drive (OI:178 handoff 안)

```
restart_gate(OI:190-199) → writer boot(OI:200-223) → claude_handoff::prepare(OI:235-241)   [그대로]
match origin:
  기계(루틴·예약·HTTP·Decision·effort·restart seed): 그대로(START_WAITS / hand_idle)   [U2]
  사람(Discord·iMessage·허용 봇), Claude: inject::hand(front)                         [S1 새 child]
  사람, Codex: 지금 경로 그대로(S1). 항상 Tab은 S2(§13.1)
```

`inject::hand`(새 child `src/services/discord/router/message_handler/provider_dispatch/owner_input/inject.rs`)의 순서:
1. 전이 잠금 `session_transition_locks.try_lock_owned()`. 키를 다 누를 때까지 쥔다. 실패하면 `session_transition_busy`. 지금은 OI:264-269, 280에 busy 가지 전용으로만 있다.
2. 보류 조건(§2.2). 걸리면 `NotSent(<사유>)`이고 front는 남는다. composer 소유 판정(H1, §7.1)은 키를 누를 composer 잠금 안에서 한다.
3. 장부 고르기. 이것은 관문이 아니다.
   - 토큰이 있으면 **합류 장부**: front claim, 키 전에 `record_native_busy_child`(OI:463-471). 실행 용량은 잡지 않는다(A의 lease, §1.1).
   - 토큰이 없으면 **자기 턴 장부**: 기존 `decision_drain::hand_idle` → `start_idle`(OI:943). 실행 용량은 기존대로 OI:1501-1515에서 잡는다.
4. **키 전 등록**(§5.5): anchor 목록·nonce·frame hash·target incarnation을 V2 attempt로 durable하게 등록한다. source(transcript 경로·offset)는 비어 있어도 된다(`pre_source`, OI:114-137). 등록이 실패하면 키 0이다.
5. **키 전 검사**(§5.5 진입점 전수표): pane·writer·실행 세대만 본다. source 결합·경로 일치·transcript 파일은 보지 않는다.
6. 키 효과: 붙여넣기+Enter.
7. 귀속 대기 항목(§5.1)은 4단계 등록과 같은 commit에서 만든다. 답 anchor는 §6이 정한다.

### 2.2 남는 보류 조건

| # | 조건 | 판정 위치 | 해소·wake | #8430 상한 |
|---|---|---|---|---|
| H3 | 세션 없음 | `Phase::Absent` → `Route::Cold`(`queue_io/input_owner/target.rs:81`), OI:1752 | 보류가 아니라 기존 cold launch(자기 턴 장부) | 불필요 |
| H1 | 사람 소유이거나 소유를 모르는 composer(§7.1) | 키를 누를 composer 잠금 안에서 판정한다. Claude `busy_inject::draft_sighting`(`claude_tui/busy_inject/ownership.rs:26-40`)이 `Settled`가 아니면 보류. 기존 stash + 현재 글자(`Unsettled`), 읽지 못한 composer(`Composer::Unknown`), 모달 footer도 여기에 든다. Codex `screen::draft_may_be_present`(`codex_tui/busy_inject/screen.rs:102`) | 사람이 보내거나 지움 → held_watch | **알림만**(U1). 지금 코드에는 사람 초안을 제출하는 경로가 있다: stash A가 있고 composer에 사람 초안 B가 있으면 `Unsettled`인데도 `["paste","Enter"]`로 B+frame을 제출한다(`claude_tui/busy_inject/stash_tests.rs:369-377`). 그 뒤 `draft_notice`(`effect/claude.rs:86-90`)는 제출 뒤 알림일 뿐이다. 목표에서는 이 경로를 키 전 보류로 바꾼다. managed stash(C-s) 예외는 화면에 stash가 없고(`Stash::AbsentInRecognizedLayout`), C-s 뒤 composer가 비었음이 증명될 때만 남긴다 |
| Hk | (S2) 실행 용량 거절 | §13.4. S1에는 없다 | 다른 턴이 끝나면 held_watch | 불필요 |
| H2 | 모달·승인 창 | Claude `busy/herdr.rs:40-51`. (S2 Codex: `screen::delivery_veto`(`screen.rs:75-99`)와 §13.1 표) | 사람 응답 → held_watch | 불필요 |
| Hc | composer 미준비(`/clear`·재시작 직후) | Claude `claude_tui/input/preparation_hold.rs:112`. Codex `codex_tui/readiness.rs` 준비 증명. 실측 Tab 0.3s부터 24회 성공 | readiness → held_watch | 불필요 |
| Hq | (S2, Codex) 앞 주입 수락 미확인 | 같은 채널 바로 앞 입력이 대기 항목·새 턴 사용자 입력으로 확인 안 됨(§5.1 "수락됨") | 수락 확인 → held_watch | 불필요 |
| H4 | 첨부 준비 전 | `input_payload.rs:143`, `ingress.rs` Hold::Uploads | 내려받기 완료 | 불필요 |
| H5 | 재시작 복구 중 | `ingress::ingress_hold`(`queue_io/input_owner/ingress.rs:192-198`) | R8 wake | 불필요 |
| H6 | 세션 전환·restart gate·effort restart | 전이 잠금, OI:190-199 | `Gate::Drop` | 불필요 |
| H7 | writer boot | OI:200-223 | writer-ready | 불필요 |
| H8 | 화면 경합 | effect `LockContended` | held_watch | 불필요 |
| H9 | root-replaced Herdr preclaim | `owner_input/preclaim_wait.rs:14-47`(합류 장부에도 적용) | backoff | 불필요 |
| H10 | 복구 kickoff 전 | U3로 H5에 포함(조각 S1-E 시험) | R8 | 불필요 |
| H11 | 중단 정산 전 | `/stop`·Cancel 뒤 인터럽트 정산 전. 근거: Claude S3D, Codex Esc 실측은 abort 기록 뒤만 측정 | stop 정산 wake | 불필요 |

사라지는 보류 사유:
- `busy_source_unavailable`(OI:455, OB:10)
- `busy_turn_unobserved`(OI:490)
- `busy_turn: transcript unavailable`(OI:1719, `codex/herdr_turn/busy.rs:74`, `codex_tui/busy_inject.rs:281-296` `turn()`)
- 토큰만 남았다는 이유의 보류 전부
- (S2) Codex의 바쁨 판정 전부(키 선택 포함)
- source 결합 전 보류 전부: Herdr `admits` 경로 불일치(`busy_source_unavailable`), `pin_source`의 "cold source binding not recorded", 어댑터의 `TranscriptUnavailable`(§5.5)

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
| OB:33-97 `running`, OB:349-398 `legacy_diagnostic`(호출 IT:1698), OB:399-470 `late` 사람 가지(호출 OI:1251) | busy 재판정 | 삭제. 기계 시작 늦은 읽기와 `unwound`(OB:511)는 child `src/services/discord/router/message_handler/intake_turn/owned_busy/late_start.rs`로 이동 |
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
| ➕ 표시 시점 | 키 직후 `turn_view_reconciler/injected.rs:179,204` | J2가 흡수(진행 중 턴 안의 `queued_command`·`absorbed_mid_turn`, `src/services/tui_o/writer/input_facts/native.rs:20-60`)를 확인한 뒤에만 단다. 별도 턴의 opener로 결합되면 달지 않는다. 키 직후 달린 ➕가 있으면(배포 직후) 거둔다(§6.2-2) |
| 실행 용량 | 자기 턴 장부만 키마다 `acquire`(OI:1493-1515), tmux 턴 `provider_dispatch.rs:406-411`, Herdr 실행 `provider_dispatch.rs:623-628`. guard는 그 턴과 함께 drop된다 | **S1: 그대로**(§1.1). S2: 채널 lease 하나(§13.2-§13.4) |
| 답 게시 주체 | Claude 합류 장부는 토큰 턴 relay, 외부 턴은 tui_o, 0단계 S1T는 A 턴 bridge | **tui_o writer 하나**(TW). 게시마다 §6의 reply anchor를 단다. 관찰자 게시 여부는 **durable output ownership**으로 정한다. 근거는 boot policy와 이 프로세스의 adoption이다(`tui_o/cutover.rs:1-4,76-92`, 판정 `tui_o::cutover::peek_o_owns_tui_output_for_channel`, 본문 송신은 `claim_then_send`·`o_keeps_body`(`tui_o/cutover/channel_gate.rs:232-263`) cutover fence).<br>O가 소유한 채널에서는 owner 관찰자(`busy_owner_observer` WF:739, Codex `warm_followup::owner_observer`)가 정산만 하고 본문을 게시하지 않는다. legacy 소유 채널의 관찰자 송신은 조회 결과만 보고 하지 않는다. 실제 송신을 `claim_then_send` 안에 두어, 조회 뒤 송신 전에 adoption이 `Pending → Committed`로 바뀌어도 본문 합계가 1회가 되게 한다(리뷰 i8328-design-3 P2). `tui_o::writer::host::channel_accepts`(`tui_o/writer/host.rs:269-278`: readiness·gateway 소유·stopping·resumed)는 readiness일 뿐이다. 이것이 거짓이어도 legacy 게시로 바꾸지 않는다. O 본문은 writer가 회복한 뒤 1회 나간다 |

## 5. 새로 필요한 것

### 5.1 귀속 대기 항목 (키 뒤 echo 전)

V2 `SnapshotV2`(`v2_types.rs:27-65`)에 선택 필드 `awaiting_echo: Vec<AwaitingEcho>`를 더한다. `serde(default, skip_serializing_if = "Vec::is_empty")`이고 형식 전환 장치는 없다.

`AwaitingEcho`에 담는 값:
- anchor message id 목록(follower 포함)
- frame nonce, frame hash
- target incarnation: provider·session·Herdr pane 또는 tmux target. V2 attempt의 `target`(OI:109-113)과 같다.
- source: 키 시점에 알면 transcript 경로·file id·offset, 모르면 `pre_source`(OI:114-123)
- 장부 종류, 토큰 turn id, 상태, 상태별 기한

상태:
1. **`AwaitingAcceptance`**: 키 전 등록(§2.1 4단계)과 같은 commit에서 만든다. 키 전에 만들기 때문에 키 뒤 재시작에도 남는다. 짧은 확인 기한(`confirmation_deadline_ms` 30초, `v2/attempt.rs:107,161`)은 이 상태에만 걸린다.
2. **`AcceptedAwaitingNative`**: TUI가 입력을 받았다는 증거가 있는 상태다. 증거는 Claude 입력창이 비고 queue 기록(`enqueue`)이 생긴 것, (S2) Codex 대기열 항목 또는 사용자 표식(§13.1)이다. 이 전이가 짧은 기한을 끝낸다. 항목은 native 결합까지 남고 기한이 없다. 재전송 요청은 그 nonce에 연결된 명시적 철회·미실행 증거가 있을 때만 한다. 증거는 Esc 회수(`Withdraw`), 세션 종료, clear다. **대기열에서 항목이 사라진 것만으로는 미실행으로 보지 않는다.** B가 정상으로 시작했는데 J2가 아직 그 행을 읽지 않았을 수 있기 때문이다(리뷰 i8328-design-3 P2).
3. **결합**: J2가 현재 incarnation의 source와 native user 행에서 같은 nonce·frame hash를 찾으면 원래 anchor에 결합하고 항목을 지운다. source가 늦게 생겨도 재주입하지 않는다.
4. **`AwaitingAcceptance` 기한 초과**: 재주입 없이 retained Unconfirmed + 재전송 요청 1회. 기한을 늘리는 것으로 고치지 않는다.
5. **세션 종료·clear·Terminate**: Cancelled/HostTerminated 정산 + 알림. (S2) 후속 입력이면 채널 lease에서도 뺀다(§13.3-§13.4).

본체는 새 child `src/services/turn_orchestrator/queued_head/v2/awaiting_echo.rs`다.

### 5.2 답 anchor 결합 기록 (§6의 저장소)

tui_o store에 채널별 durable journal `anchor_bindings`(새 child `src/services/tui_o/store/anchor_bindings.rs`)를 둔다.
- 키는 `(source id, native turn id)`다. source id는 dev·ino를 포함한 transcript 신원(`tui_o/shadow/mod.rs:50-`)이다.
- 값: opener anchor(없으면 None), 합류 anchor 목록, 결합 근거(입력 행 range, nonce), 확정 시각.
- tui_o store는 이미 채널별 ledger·spool을 durable하게 쓴다(`tui_o/store/ledger.rs`, `spool.rs`). 같은 디렉터리·같은 잠금 규칙을 따른다.
- (S1-A 구현) `Prepared` 형식은 그대로 두고, `OutputOrigin`과 같은 방식의 새 항목 `LedgerEntry::ReplyTo{serial, anchor}`를 `Prepared` 바로 앞에 쓴다(`src/services/tui_o/store/ledger/reply_to.rs`). 결합 기록이 durable해진 뒤에만 쓴다. 로컬 재시도·operator resume은 이 값을 `ReplyRef::Fixed`로 다시 실어 같은 reply를 쓴다. `Prepared`에 선택 필드를 더하지 않은 이유: 시험 fixture의 `Prepared` 리터럴 약 75곳이 모두 바뀌기 때문이다(동작은 같다).

### 5.3 복구 kickoff 전 보류 (H10)

U3로 H5에 포함된다. 조각 S1-E에서 `restore_inflight.rs:2248` kickoff → `recovery_flush.rs:120-123` `complete_active_reconcile` 순서를 고정하는 시험을 둔다.

### 5.5 Claude 키와 transcript 분리 (P1-2 → P1-3, S1)

수용 기준: transcript 없는 정상 입력 보류 0(S1 모델 `NoSourceHold`).
원칙: owner `pre_source`에서는 pane·writer·실행 세대 검사를 지키고, source 검사만 후행 J2로 옮긴다. J2 결합은 기존 V2 snapshot 갱신과 custody ACK까지 이어진다.

#### 5.5.1 키 전 source 검사 진입점 전수표

| # | 진입점 | 지금 검사 | source가 없을 때 지금 | S1 목표 |
|---|---|---|---|---|
| E1 | V2 attempt source 만들기 OI:114-123 | live `(path, Some(session))`면 `std::fs::metadata(path)?` | live 값은 있는데 파일이 아직 없으면 `?`로 Err → 키 0 | metadata 실패면 `pre_source`로 만든다. 등록 지점 OI:1275-1280 `(host, session, None)`은 그대로 |
| E2 | O writer owner 경로 `Producer::wrap` `src/services/discord/router/message_handler/provider_dispatch/owner_input/native_input.rs:71-109` → `native_prepare::pin_source` → `src/services/discord/turn_finalizer/native_binding.rs:18-106` | `ticket.source`가 없으면 binding log에서 같은 owner의 `Bound` 기록을 찾는다(`native_binding.rs:31-40`). 이어서 execution nonce(41-43), launch path·provider root(59-68), pre_source 기대값(69-79), 파일 경계(80-88, `boundary` 108-130)를 본다 | `Bound`가 없으면 "cold source binding not recorded" → `effect::unsent` → 키 0 | `Bound`가 없으면 **지연 pin**을 만든다. 세대 검사는 지킨다: `binding_context::execution_context("claude", target.execution_nonce)`가 같은 tmux session·channel이고 `observe_spawn_nonce_marker`가 Known(nonce)이어야 한다. 아니면 키 0(pane 교체 대조군) |
| E3 | V2 slot `Owner::pin_source` `src/services/turn_orchestrator/queued_head/v2/cold_source.rs:15-56` | `pin.binding_seq == 0`·`session_id` 빈 값이면 "source pin lineage changed"(40-44) | 지연 pin을 거절 | `PreKeyPin`(`src/services/turn_orchestrator/queued_head/v2_types/cold_source.rs:7-15`)에 선택 필드 `deferred_after_seq: Option<u64>`(serde default)를 더한다. Some이면 binding_seq·session·transcript 검사를 건너뛰고, ticket·head·episode·provider·`PreSource` 검사는 그대로 둔다. 옛 기록에는 이 필드가 없어 None으로 읽히므로 전환 장치가 필요 없다 |
| E4 | Herdr `BusyObservation::admits` `src/services/claude_tui/host_input/busy.rs:245-261`(호출 `busy/herdr.rs:80-86`) | 관찰 source 경로 == 요청 transcript, `context.logical == request.session`, 읽기 상태 Busy·Idle, 다시 읽은 source id == 관찰 source id | 관찰 source가 None이면 false → `busy_source_unavailable` 보류 | 요청이 `pre_source`(transcript None)면 source 비교를 건너뛴다. 남기는 것: `context.logical`, pane target·writer(`Context` `target`·`writer`·`attempt_target`, `busy/herdr.rs:280-299`), 읽기 가능 상태. source가 결합된 요청은 지금처럼 비교한다 |
| E5 | Herdr adapter `claude_tui/host_input/busy/herdr.rs:53-68` | `std::fs::metadata(request.transcript)` → `history::register(who, transcript, offset, frame)` | `TranscriptUnavailable` | `Request.transcript: Option<&Path>`. owner 범위면 `key_admit`(§5.5.2) → 키 |
| E6 | tmux adapter `claude_tui/busy_inject.rs:565-581` | `TurnGate::BusyOnly`면 `observe_claude_jsonl_turn_state(transcript).is_busy()`(565-570), 이어서 metadata(571-573) → register(574-581) | `NotBusy` 또는 `TranscriptUnavailable` | owner 범위에서는 BusyOnly 관문을 쓰지 않는다(S1은 바쁨을 보지 않는다). metadata는 E5와 같이 처리 |
| E7 | history 등록 `src/services/tui_input/human_frame/history.rs:222-240` → `history/v2.rs:743-765` `scoped_register` → `registered`(`v2.rs:610-627`) | 결합된 source(`registry.source`, `v2.rs:602-609`)와 file id가 있어야 한다 | "source not bound" → 등록 실패 → 키 0 | owner 범위에서는 `key_admit`로 바꾼다. 결합 전이면 경로·offset 비교를 건너뛰고, 결합 뒤면 지금처럼 비교한다 |
| E8 | 현재 hand_busy 보류 OI:455, OB:10(`busy_source_unavailable`), OI:490(`busy_turn_unobserved`) | active turn·phase·source | 보류 | 사람 Claude 입력에서는 삭제한다(§3, S1-D) |

키 전에 남는 검사(대조군, 모두 키 0): pane 교체(E2 실행 세대, E4 pane target·writer), writer 소유(`owner_input::bind_writer`, `busy/herdr.rs:32-34`), 모달(H2), 초안(H1), composer 미준비(Hc), 전이 잠금.

#### 5.5.2 키 허가 `key_admit`

- 새 child `src/services/tui_input/human_frame/history/v2/key_admit.rs`.
- 준비된 V2 attempt(`v2.rs:370` `prepare`, `with_attempt` `v2.rs:717-742`)와 nonce·provider·target owner·frame hash가 같은지 확인한다.
- source가 결합 전(`pre_source`)이면 경로·offset 비교를 건너뛴다.
- 이것은 형식 전환 장치가 아니다. owner 범위 밖 legacy 호출자는 지금의 `register_inner`(`history.rs:242`)를 그대로 쓴다.

#### 5.5.3 지연 pin의 단일 확정점과 늦은 결합 (P1-3)

지금 코드: `Owner::select_source`(`src/services/turn_orchestrator/queued_head/v2/cold_source.rs:89-116`)는 새 source가 pin의 session·transcript·offset·file과 같아야 한다(96-103, "source binding differs from pre-key pin"). 지연 pin에는 비교할 값이 없으므로 이 검사에서 반드시 거절된다. 호출 순서는 `select_source` 성공 → `history.bind_native_source` → `ack_source`(`native_binding.rs:240-261`)이므로 결합·ACK에 닿지 못한다.

목표: **`BindingIntent.selected`의 None → Some이 유일한 확정점**이다.
- 지연 pin(`deferred_after_seq: Some(w)`)에 불변으로 보관하는 것: ticket(target·provider·owner·execution nonce 포함), watermark `w`, owner root.
- `bound_source`(지연 분기)는 다음을 모두 만족하는 첫 기록을 고른다: `seq > w`, 같은 channel·provider·owner·execution nonce, `Bound(Source)` 또는 `Bound(Pending)` + 짝 `Resolved`. 옛 실행의 `Bound`(다른 execution nonce)와 watermark 이전 `Bound`는 고르지 않는다. 이어 `verify_source`(파일 신원)를 거친다.
- `select_source`의 지연 분기
  - `intent.selected`가 None이면 session·transcript·offset·file 선행 동등성을 요구하지 않는다. `Source::validate(&ticket.key)`(`history/v2.rs:21-58`: provider·target·execution nonce·파일 신원, `pre_source` 기대값은 null이면 통과)는 그대로 거친다. 통과하면 `selected = Some(source)`를 commit하고 flush한다.
  - `intent.selected`가 Some이면 같은 source만 받는다. 다른 source는 계속 거절한다.
- 지연 pin의 offset은 0이다. 키 시점에 파일이 없었으므로(fresh) 처음부터가 B의 범위다. rebind처럼 파일이 이미 있던 경우에도 0부터 읽는다. 대신 J2 receipt의 유일성 검사(같은 frame hash가 offset 이후 정확히 한 번, `receipt.rs:131-149`)가 오귀속을 막는다. 둘 이상이면 결합하지 않고 reference 없이 게시한다.
- 이후 `bind_native_source`와 `ack_source`는 기존의 같은-source·재시도 가능 계약 그대로다. `retry_source_bindings`는 `selected`가 Some이면 해석을 건너뛴다(`native_binding.rs:223-224`).

확정 전·후에 들어오는 입력:
- 확정 전에 다른 입력 C가 오면 C도 자기 ticket·자기 지연 pin을 가진다(`Bound`가 아직 없으므로). 각 BindingIntent는 독립으로 확정된다.
- 확정 뒤 같은 실행에 `Bound`가 기록되면 새 입력의 `pin_source`는 기존 경로(recorded pin)를 탄다.
- 확정은 입력마다 한 번이다. 입력 B의 확정이 C의 pin을 바꾸지 않는다.
- 부채 상한 `CAPACITY`(4096, `history.rs:27`, 검사 `cold_source.rs:46`)는 지연 pin도 센다. 상한에 닿으면 "source binding debt full"로 키 0이다(G5).

재시작 뒤 복원:
- `selected`가 durable commit된 뒤라면 부팅 뒤 `retry_source_bindings`가 같은 source로 `bind_native_source` → `ack_source`를 이어 간다.
- 확정 전이라면 지연 분기 해석을 처음부터 다시 한다. `Bound` 기록은 append-only라 같은 첫 기록을 고른다.
- 키를 다시 누르지 않는다(재주입 0).
- 실행 세대가 바뀌었으면 확정하지 않는다. `awaiting_echo` retained 정산 + 알림.

#### 5.5.4 E1–E8 밖 source 의존 관문 전수 (P1-2)

| # | 관문 file:line | 타는 입력 | 지금 | S1 처리 |
|---|---|---|---|---|
| G1 | tmux 자기 턴 observer: OI:1275-1276 `observer()` → OI:1367-1383 → `claude_tui/hosting/warm_followup.rs:589-611` `owner_observer`(`resolve_live_claude_tui_session(...).ok()?` 609-610, `std::fs::metadata(transcript).ok()?` 611) → None → OI:1495-1498 `NotSent("idle_unobserved")` | I1·I2·I8·I11·I12·I14 Claude, 토큰 없음, 살아 있는 tmux pane(ClaudeIdle) | 새 세션·재결합 창에서 키 0. 첫 입력이 transcript를 만드는 경우 순환 대기 | **키 허가 관찰과 source 관찰을 나눈다.** 키 전에는 pane·overlay·auth profile(602-605)만 요구하고 observer를 만든다. source 해석과 시작 offset은 키 뒤로 옮긴다. 새 child `src/services/claude_tui/hosting/deferred_source.rs`가 이미 구독 중인 hook 이벤트(`subscribe_hook_events`, 612)의 transcript 경로나 기존 `rebind` 해석을 기다린다. cancel·custody 정산과 함께 끝나며 sleep 폴링은 없다. 해석되면 offset 0부터 읽는다(지금 `rebound_source`의 "rebound session read from this submit's time"과 같은 규칙). `until_done` 검사(OI:1495-1498)는 남는다. pane 신원 실패만 막고, capacity 유지·취소·완료 정산을 지킨다 |
| G2 | Herdr `Context::read` `claude_tui/host_input/busy/herdr.rs:303-343`: clear·idle readiness 특례(307) 밖에서 `current_source(authority)?`(320, 399-) → 없으면 `Reading::held()` → `admits`의 Busy·Idle 요구(`busy.rs:259-260`) false → `busy_source_unavailable`(herdr.rs:80-86) | Claude 토큰 있음(합류 장부), Herdr, source 재결합 창 | 키 0 | `Context::key_ready()`를 더한다(같은 파일, 443줄이라 giant 아님). 검사 항목: `execution_alive`(304), spawn nonce marker == `self.nonce`(idle_ready 352-355와 같은 검사), `output_ready`(310), capture 가능·모달 아님(311). `pre_source` 요청의 `admits`는 `read().state` 대신 `key_ready()`를 쓴다. 초안(H1)·composer(Hc)는 adapter가 같은 잠금에서 본다. `read()`는 source가 결합된 요청과 관찰용으로 남는다 |
| G3 | `BusyObservation` 생성 `busy/herdr.rs:280-299` `observe()`(생성 때 `read()`의 state·source를 담음), 호출 `queue_io/input_owner/claude_handoff.rs:574` | G2와 같음 | 생성 시점 source None을 들고 간다 | G2로 덮는다(`admits`가 생성 시점 state를 쓰지 않는다) |
| G4 | `native_binding.rs:108-130` `boundary`: 파일 끝이 개행이 아니면 "incomplete pre-key record" | recorded pin(파일 있음) | 쓰는 중이면 키 0, held_watch 재시도 | **유지**. 파일이 있을 때의 일시 상태이고 transcript 없음과 무관하다. 지연 pin은 파일을 읽지 않으므로 해당 없음 |
| G5 | `cold_source.rs:46` source binding 부채 `CAPACITY`(4096) | 모든 pre_source | 상한이면 키 0 | **유지**. 명시된 상한 보류이고, 지연 pin 결합이 부채를 줄인다. 상한 도달은 경보 대상 |
| G6 | `native_binding.rs:215-221` 종결된 cold ticket은 source를 얻지 못함 | 키 뒤 Cancel·cleanup된 입력 | 키 뒤 결합 거절 | **유지**(키 전 관문 아님). Cancel·Terminate 정산이 `awaiting_echo`를 끝낸다(§5.6) |
| G7 | `discord/health/turn_deliver/inject/herdr.rs:96` `TranscriptUnavailable` | I6 HTTP deliver(기계, 보관) | 키 0 | S1 범위 밖. 기계 시작은 START_WAITS 유지(U2) |
| G8 | OI:1714-1720 Codex `read_turn` 관문 | Codex | 키 0 | S2(§13.1) |
| G9 | OB:33-97 `running`, OB:399-470 `late`(transcript 재판정) | 지금 hand_busy 늦은 판정 | 보류 | S1-D에서 사람 입력 가지 삭제(§3) |
| G10 | `claude/herdr_turn/first_input.rs:126-133` Cold·HerdrIdle 첫 입력 source | Cold·HerdrIdle | metadata 실패면 이미 `pre_source` | 관문 아님(확인만). 이후 `Producer::wrap` E2를 탄다 |

순환 대기 해소: 첫 입력이 transcript를 만드는 경우 G1·E2·E5–E7 모두 키 전에 source를 묻지 않는다. 모델 `TargetFresh` GREEN, `CurrentFresh`·`RequireIdleObserverBeforeKey`·`RequireTranscriptFresh`·`PinSourceFirst` RED(`NoCircularWait`).

**P2-1 (리뷰 i8328-design-5, S1-B가 닫는다).** 진입 관문을 지난 뒤에도 source를 다시 요구하는 사슬이 둘 있다.
- Idle 전송: `src/services/discord/queue_io/input_owner/effect.rs:216-229` `idle_claude` → `src/services/claude_tui/input.rs:360-370` `send_warm_followup_owned` → `src/services/claude_tui/input/submission.rs:35-37`(`wait_for_prompt_ready`)·`:171-192`(`readiness::observe`·`reserve`). warm follow-up 준비 증명이 source 관찰에 기대면 G1을 고쳐도 여기서 키가 0이 된다.
- Herdr 재시도: `src/services/claude_tui/host_input/busy/herdr.rs:41-44`(모달 보류의 probe) → `src/services/claude_tui/host_input/busy.rs:206-212` `probe`(`read.state == Busy || idle_verified`일 때만 `Screen::Ready`) → `src/services/discord/queue_io/input_owner/held_watch.rs:65-69` `verdict`·`:140-149`(Ready여야 Wake). `read()`가 source를 요구하므로(G2) source가 없으면 Ready가 되지 않고 보류가 풀리지 않는다.

계약: **허용한 `pre_source` 입력의 전송·보류 해제에 source 생성이 다시 필수 조건으로 들어오지 않는다.** S1-B는 두 사슬이 G2의 `key_ready()`와 같은 검사(실행 세대·spawn nonce·출력 준비·모달 아님)로 판정하게 하고, 시험은 source 없는 Herdr·tmux 입력에서 전송 1·보류 해제 1을 본다.

### 5.6 같은 상태를 소비하는 진입점 (BindingIntent·source pin·receipt·anchor 결합)

| 진입점 | 소비하는 상태 | S1 계약 |
|---|---|---|
| 키 전 `Producer::wrap` → `pin_source`(E2·E3) | BindingIntent 생성 | recorded 또는 지연 pin. 실패면 키 0, `release_unkeyed_source`(`native_binding.rs:134-160`)가 no-key proof로 정리 |
| 늦은 결합 `retry_source_bindings`(`native_supply.rs:602`) | BindingIntent·selected | §5.5.3 단일 확정 → history binding → ACK |
| J2 receipt `observe` → `port.inner.receipt`(`tui_o/writer/rotation/owe.rs:594-617`) | MembershipReceipt | 등록 ticket과 user 행을 잇는다. receipt를 다 기록한 뒤 같은 poll에서 `settle_anchors`가 evidence 축약의 ticket→run 관계(`input_facts/native/projection/membership.rs`)로 run별 opener·합류를 다시 계산한다. anchor는 ticket `key.identity.primary`다 |
| `anchor_bindings`·`LedgerEntry::ReplyTo`(§5.2) | 결합 기록 | 게시·재게시·로컬 재시도가 같은 anchor를 쓴다 |
| Idle 전송 `idle_claude` → `send_warm_followup_owned` → `submission::send`, Herdr 재시도 `probe` → `held_watch::verdict`(§5.5.4 P2-1) | 키 허가·보류 해제 | 허용한 `pre_source` 입력의 전송·보류 해제에 source 생성이 다시 필수 조건으로 들어오지 않는다(S1-B) |
| 부팅 `retry_source_bindings`, writer boot | BindingIntent·anchor_bindings | §8 |
| HTTP Cancel·force Cancel(§9 A1a·A1b) | 키 뒤 BindingIntent | 키 뒤 Cancel이면 G6 규칙대로 결합하지 않는다. `awaiting_echo` HostTerminated/Cancelled 정산. 이미 결합된 답은 원래 anchor에 그대로 둔다 |
| Herdr Terminate(A2) | BindingIntent·awaiting_echo | 실행 세대가 끝나 확정하지 않는다. retained 알림 |
| `/clear`(A3) | BindingIntent·awaiting_echo | clear 뒤 새 실행의 `Bound`는 execution nonce가 달라 옛 지연 pin에 고르지 않는다. 옛 항목은 정산 + 알림 |
| `/stop`·`!stop`(A4a) | awaiting_echo | 흡수 전 Esc 회수는 미전송 + 재전송 요청. 키 뒤 미확정 pin은 G6 |
| 세션 교체·incarnation | ticket target(execution nonce) | 다른 세대의 `Bound`·receipt는 결합하지 않는다 |

## 6. 귀속 모델: 입력 anchor ↔ native 턴 ↔ 게시 답

### 6.1 결합 지점 (file:line)

| # | 결합 | 지금 코드 | 새로 할 일 |
|---|---|---|---|
| J1 | **입력 anchor ↔ frame nonce** | 키 전 등록. Codex `history::append::register((session,id,author), transcript, offset, frame, target_turn, message_id, …)`(`src/services/tui_input/human_frame/history/append.rs:128-165`, 호출 `codex/herdr_turn/busy.rs:82-90`). Owner 모드는 `src/services/tui_input/human_frame/history/v2.rs` `scoped_register`(native evidence `Ticket{key, channel_id, source}`, `src/domain/human_input/native_source.rs:26-33`). Claude `history::register(who, transcript, offset, &frame)`(`claude_tui/host_input/busy/herdr.rs:67`, message id 없음) | 두 provider 모두 V2 admission의 anchor message id(follower 포함)를 nonce와 함께 `awaiting_echo`에 쓴다(§5.1). Claude history 등록에도 message id를 싣는다 |
| J2 | **입력 등록 ↔ native 턴**(w5: receipt 우선, nonce 보조) | transcript 입력 행 판정: `src/services/tui_input/human_frame/history.rs:838` `attempt_row`(자기 user 행 = true, queued command = false). Claude 분류 `src/services/tui_o/writer/input_facts/native.rs:20-60`(`QueueRecord`: Prompt·Absorbed·Human). 턴 경계 `src/services/tui_o/writer/input_facts.rs:40-57`(`Ordered::Opened{range, native_turn_id}`·`Closed{…, aborted}`·`Input{range, record}`). Codex native turn id는 rollout `task_started`의 turn id(`codex_tui/busy_inject/rollout.rs` `read_turn`) | **결합 근거는 native membership receipt**다(`src/services/tui_o/writer/input_facts/native/receipt.rs:6-22` `MembershipReceipt{namespace, ticket, row, kind, range, frame_hash}`, 생성 `receipt.rs:82-167` `candidates`, 소비 `tui_o/writer/rotation/owe.rs:594-617`). receipt는 등록 ticket의 exact frame hash와 user·queued command 행 본문의 hash(붙여넣기 내부 hash 포함, `receipt.rs:66-69`)를 맞춘다. 정상 Idle·Cold 입력은 `human_frame::idle()`(`human_frame.rs:54`)이 nonce 꼬리표 없이 만들고(OI:1607-1621, 1695-1702), Busy만 꼬리표가 있다(OI:1652-1653). 그래서 꼬리표는 보조 근거로만 쓴다. 프롬프트 바이트는 바꾸지 않는다. receipt의 ticket이 V2 attempt를 가리키고, 그 attempt의 `sources`(OI:124-127)가 anchor다. receipt 행을 감싼 native 턴에서 자기 user 행이 턴을 열면 opener, 진행 중 턴 안의 흡수·queued command면 합류다.<br>**실패 처리(오귀속 0)**: 짝 ticket이 하나가 아니거나(`receipt.rs:110-128`의 `let [owner]`), 같은 hash 행이 offset 이후 둘 이상이거나(131-149), 중복 row(93-100)면 receipt가 없다. 그러면 결합하지 않고 reference 없이 게시하며 사유를 남긴다. 추측으로 다른 anchor에 걸지 않는다 |
| J3 | **native 턴 ↔ 게시 답 조각** | tui_o writer가 record마다 조각을 만든다: `src/services/tui_o/writer/pieces.rs:115-202` `UnitDeriver::derive*` → `OwedWork{derived, source, range}`(`src/services/tui_o/writer/pieces.rs:45-52`). 조각에는 이미 `output_origin` 운반 칸이 있다(`src/services/tui_o/writer/pieces.rs:22-29,81-86,131-132`) | 새 child `src/services/tui_o/writer/reply_anchor.rs`가 같은 record 흐름에서 현재 native 턴을 따라가며 조각에 `reply_to`를 붙인다(`output_origin`과 같은 운반 방식). 조각의 range가 속한 턴의 결합 기록에서 정한다 |
| J4 | **게시 ↔ Discord reply reference** | `src/services/tui_o/writer/deliver.rs:532-563` `send_piece`가 `LedgerEntry::Prepared` 뒤 `port.post_unit(channel, payload, origin, kind)`(`src/services/tui_o/writer/deliver.rs:590-595`)를 부른다. 포트 `src/services/tui_o/writer/mod.rs:115-145`, Discord 구현 `discord/outbound/o_writer_io.rs:184-210`. **reference 인자가 없다** | `post_unit`에 `reply_to`를 더하고, Discord 구현은 기존 `discord/outbound/serenity_reference.rs`로 message reference를 붙인다. anchor가 지워졌으면(삭제된 메시지) reference 없이 게시하고 결합 기록에 사유를 남긴다 |
| J5 | **결합 관측 기록** | `relay_delivery` 이벤트는 session_relay_sink 경로에만 있다(`session_relay_sink/task_notification_context.rs:576-605`). tui_o writer는 내지 않는다(실측 이벤트 0, bridge 확인 0/94의 원인) | 게시마다 `tui_o reply anchored` 한 줄(channel, native turn id, anchor, msg id, 근거 range)과 `emit_relay_delivery(…, prompt_anchor, "tui_o_writer", …)`를 남긴다 |

### 6.2 규칙

1. **답 조각의 anchor는 그 조각이 속한 native 턴의 결합 기록이 정한다.** 결합 기록은 receipt(J2)로 만든다. 꼬리표 없는 opener도 마찬가지다(모델 `TaggedHeaderOnly` RED).
   - 턴을 연 입력에 Discord anchor가 있으면 그 anchor에 건다.
   - 없으면(TUI 직접 입력, 백그라운드 알림이 연 턴) 그 턴에 처음 합류한 Discord 입력의 anchor에 건다.
   - 그것도 없으면 reference 없이 게시한다(지금과 같음).
   - 모델의 `ATarget`이 이 규칙이고, `AnswerBound`가 검사한다.
2. **합류 입력은 합친 답 한 번으로 친다.**
   - 합류 입력에는 ➕ "진행 중 턴에 합류"를 단다. 지금 반응 `turn_view_reconciler/injected.rs:204-231`이 있고, Idle 관측에서 ✅로 바뀐다(`turn_presence/supervisor.rs:884`).
   - ➕는 J2가 흡수를 확인한 뒤에만 단다(w3, 모델 `MarkOnlyJoined`, `MarkOnPaste` RED). 키 직후에는 달지 않는다.
   - J2가 그 입력을 별도 턴의 opener로 결합하면 ➕를 달지 않는다. 이미 달린 ➕(배포 경계)가 있으면 거둔다.
   - 따로 답하지 않는다. 합친 답은 규칙 1의 anchor 하나에 건다.
   - 한 Discord 메시지에는 reference를 하나만 걸 수 있으므로 여러 anchor에 나누지 않는다.
3. **게시 순서로 귀속하지 않는다**(`PostOrder` RED). "가장 오래된 미답 입력"에 붙이는 규칙은 외부 턴 A의 답을 Tab으로 대기한 B에 붙인다. 0단계 S1T의 "B 답이 A bridge 수명 안에서 게시"가 바로 이 모양이다.
4. **reply reference 없이 게시하지 않는다**(`NoReference` RED). anchor가 있는 답은 반드시 reference를 단다.
5. **결합이 확정되기 전에는 그 턴의 조각을 보내지 않는다.** 조각은 결합된 source에서만 나오고(tui_o writer는 결합된 source를 읽는다), receipt 판정은 같은 record에서 끝나므로 지연은 0에 가깝다. 결합이 **확정적으로 불가능**하면 reference 없이 게시하고 사유를 남긴다. 불가능한 경우는 receipt 유일성 실패, 실행 세대 변경, Cancel·Terminate 정산이다. 조각을 막지 않는다(유실 0 우선). 늦게 오는 것(`Bound` 대기)은 불가능이 아니다.
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
| (b) 붙여넣은 직후 Tab → 두 건 합쳐짐 | 2 | C3 BUSY1-1, BUSY1-2 | 둘 다 input UUID `01a21608-53ea…`. 1의 답 없음, 2만 게시. 3은 초안 잔류(수동 Enter 뒤 정상, 87에 포함) | 붙여넣기 묶음 인식 중 키 | **반영 확인·수락 확인**(§13.1.1, S2)으로 제거. 생기면 nonce 둘 규칙 |
| (c) 앞 초안 잔류 + 다음 시작이 합쳐짐 | 2 | C2 BUSY1-A, BUSY2-A | 둘 다 `01a21609-9c9b…`. BUSY1-A 답 없음. BUSY2-A는 A 표식 2개가 한 사용자 메시지에 | 첫 Enter가 초안에 남음(composer 미준비·반영 전 키) | **반영 확인 + Hc + 수락 확인(Hq)**으로 제거. 생기면 nonce 둘 규칙 |

**bridge 귀속 확인 0/94**는 오귀속이 아니다. 결합 기록이 없는 관측 공백이다.
- 실측 ACK 94개의 Discord `reference`가 모두 null이었다(J4: tui_o 포트에 reference 인자가 없음).
- stdout에 답 ID와 turn id를 잇는 줄이 0개였다.
- `relay_delivery` 이벤트도 0개였다(J5: tui_o writer가 내지 않음).
- J4·J5가 이 공백을 메운다.

## 7. 입력 출처 분류표

"바로 주입"은 §2.2 보류만 보고 넣는다는 뜻이다. Codex는 S1에서 지금 경로 그대로이고, 항상 Tab 절차(§13.1.1)는 S2다. "답 anchor"는 §6 규칙에 따른 reply 대상이다.

| # | 출처 | 진입 | 처리 | 답 anchor |
|---|---|---|---|---|
| I1 | 사람 owner front(Discord) | IG:120 → `intake_dispatch.rs:815` → `ingress.rs:407` | Claude: **바로 주입**(S1). 토큰 없는 tmux 자기 턴은 G1 observer를 source 없이 만든다. Codex: 지금 경로(S1), 항상 Tab은 S2 | 자기 메시지(합류면 턴 opener). 결합은 receipt(꼬리표 유무 무관) |
| I2 | 허용 봇·announce·wake | `ingress.rs:418-425`(`bot`은 표식, origin=Discord) | **I1과 같다**(U4) | I1과 같다 |
| I3 | busy follow-up retry | owner 채널은 V2 `control.rs:108-115`가 RequeueFront 거절 | 따로 없음. 보관 front가 다음 wake에 I1을 탄다 | I1 |
| I4 | 루틴 | `routines/agent_executor/owner_input.rs:180,219` | **보관**(START_WAITS) | 루틴 자기 카드(기존). 이번 범위에서 바꾸지 않음 |
| I5 | 예약 메시지 | `scheduled_messages/owner_input.rs:198` | **보관** | 기존 |
| I6 | HTTP start·deliver | `agents_turn_deliver.rs:137` → OI:713 | **보관**(409 유지) | 기존 |
| I7 | Decision | `decision_target.rs:52` | **보관**(턴 경계, PG journal) | 기존 |
| I8 | iMessage | `origin_admission.rs:24` | **바로 주입**(reply_context 이동) | iMessage 답 경로(reply_context) + Discord anchor가 있으면 그것 |
| I9 | effort 이어가기 | `effort_restart/tmux.rs:359` | **보관**(H6) | 기존 |
| I10 | 재시작 seed | `restart_seed.rs:95` | **보관**(greeting_permit) | 기존 |
| I11 | 첨부 대기 | `ingress.rs` Hold::Uploads | **보관**(H4) 뒤 바로 주입 | I1 |
| I12 | 스레드 입력 | IG:228, 스레드마다 V2 owner | 스레드 채널마다 I1. THREAD-GUARD(IG:870-926)는 명시 보류로 유지 | 스레드 안 자기 메시지 |
| I13 | TUI 직접 입력 | owner 밖(ExternalInput) | owner 입력이 아님. 사람 초안(H1)의 근원 | 그 입력이 연 턴은 anchor 없음 → 처음 합류한 Discord 입력 또는 none |
| I14 | Claude 입력, source 결합 전(새 세션·재바인딩·Herdr observation `source=None`) | I1과 같다 | **바로 주입**(S1). 키 전 등록 + 지연 pin. G1 observer는 키 뒤 source를 기다리고, G2 Herdr는 `key_ready()`를 쓴다. 확정은 `selected` 한 번(§5.5.3). 보류 0 | I1과 같다(확정·receipt 결합 뒤) |

### 7.1 composer 소유 분류 (H1, P2-2)

판정은 키를 누를 composer 잠금 안의 capture 하나로 한다. 판정 함수는 Claude `draft_sighting`(`claude_tui/busy_inject/ownership.rs:26-40`)·`draft_unproven`(같은 파일 46-52), Codex `screen::draft_may_be_present`다.

| composer 상태 | 지금 | 목표 |
|---|---|---|
| 빈 composer, stash 없음(`Settled`) | 주입 | **주입** |
| 사람 글자, stash 없음(`PersonDraft`) | Claude C-s로 보관 → 주입 → 복원(`stash_tests.rs:333-345`). tmux `stash::submit_with_start`(`claude_tui/busy_inject/stash.rs:22-92`)는 "No pane is read after a key"(1행)라 C-s 성공 응답 뒤 capture 없이 paste로 간다. Herdr Ctrl-S(`busy/herdr.rs:88-98`)도 `Confirmed`만 보고 간다 | **S1: 보류**(H1). owner 사람 입력 경로에서는 C-s를 쓰지 않는다. managed stash 예외를 다시 들이려면 두 확인 지점이 필요하다: C-s 뒤 같은 잠금에서 capture해 빈 composer와 해당 초안의 stash를 확인해야 한다(tmux `stash.rs:72-92`, Herdr `busy/herdr.rs:88-98`). C-s가 성공을 반환해도 초안이 남거나 stash를 식별할 수 없으면 paste·Enter 0, 기존 stash와 초안이 함께 있으면 C-s도 0이다. 이 예외는 메인 결정(§12-4) 뒤에만 들인다 |
| 기존 stash A + 현재 사람 글자 B(`Unsettled`) | **paste+Enter로 B+frame 제출**(`stash_tests.rs:369-377`). 그 뒤 `draft_notice`(`effect/claude.rs:86-90`) | **보류**(H1). 키 0, 두 초안 모두 보존, 채널 알림(U1) |
| 읽지 못한 composer(`Composer::Unknown`), 모달 footer | `Unsettled` | **보류**(H1/H2) |
| AgentDesk가 쳤다가 남은 frame(`ComposerOwner::AgentDesk`) | 기존 처리 | 같은 nonce의 `awaiting_echo`가 있으면 그 입력의 몫이다. 재주입은 없다(§5.1) |
| (S2) Codex 초안 | `draft_may_be_present` | **보류**(H1) |

"제출 뒤 알림"(`draft_unproven` → `draft_notice`)은 목표에서 "키 전 거절"이 된다. 제출 뒤 알림으로 남겨 두는 변형은 mutant로 RED를 확인한다(조각 S1-D).

U 결정(코드 근거, w1 그대로):
- **U2 기계 시작: START_WAITS를 남긴다.** 근거: `target.rs:76-78`, OI:1219-1243, `routine_delivery`·Decision journal의 턴 경계 정산.
- **U3 `reconcile_done=false`: 사람 입력은 토큰 유무와 무관하게 보류.** 근거: `recovery_kickoff.rs:39`(빈 slot만 claim), 복구 relay 범위.
- **U4 허용 봇: 사람 취급 유지.** 근거: `ingress.rs:418-425`.
- **U1 사람 초안: 알림만.** 사람 소유이거나 소유를 모르는 composer 전부에 적용한다(§7.1, w3).

## 8. 재시작·부팅·incarnation 경로

재시작을 넘는 것: V2 큐 파일(`pending_queue_persistence.rs:95-106`, 여기에 `awaiting_echo`를 더함), tui_o store(ledger·spool, 여기에 `anchor_bindings`·`Prepared.reply_to`를 더함), HF history 저장소.

| 대상 | 재시작 전 | 부팅에서 | 결과 |
|---|---|---|---|
| 보관 입력(키 0) | V2 `items` 머리 | R6 `owner_input/native_input.rs:188-214` → U3로 H5 보류 → R8 wake → `inject::hand` | 정확히 1회 주입 |
| 키 뒤 echo 전 | `awaiting_echo` | R2 native 정산 → R5 `settle_boot`·토큰 재건 → tui_o writer가 source를 다시 읽으며 J2 결합 시도(입력 행이 이미 있으면 바로 결합) | 결합되면 §6 규칙. 기한 초과면 retained 재전송 요청. **재주입 0** |
| 결합 확정 뒤 답 게시 중 | `anchor_bindings` 행, ledger `Prepared{reply_to}`·`Posted` | tui_o boot(`src/services/tui_o/writer/boot.rs`)가 ledger를 이어받는다. 남은 조각은 `anchor_bindings`에서 같은 anchor를 다시 읽는다. `Prepared` 뒤 결과가 모르는 POST는 기존 확인 스캔(`src/services/tui_o/writer/confirm.rs`)이 같은 payload로 맞춘다 | 같은 anchor에 1회. 중복 0 |
| 결합 기록이 resume 지점보다 앞인 턴 | `anchor_bindings`에만 있음 | source 재독은 resume 지점부터라 opener 행을 다시 못 본다. `anchor_bindings`에서 읽는다 | anchor 유지 |
| `anchor_bindings`가 없는 옛 턴(배포 전 시작) | 없음 | 결합 근거 없음 → reference 없이 게시(규칙 6.2-5) | 전환 장치 없이 운영 절차(세션 종료 → 배포)로 끝 |
| 키 중간 | `head_injection` phase | R6 `recovery.rs:8-44` 현행 | 현행(`NoDuplicate`) |
| 세션 교체(`!clear`, Herdr pane 교체, 새 세션) | 옛 source id에 묶인 항목 | source id(dev·ino)가 다르므로 새 세션 조각에 옛 결합이 붙지 않는다. 옛 `awaiting_echo`는 clear(A3)·terminate(A2)·R13이 정산하고 알림 | 옛 입력은 알림으로 끝 |
| R13 carried episode | 이전 generation 토큰 | 사람 입력은 합류 장부로 주입된다. 답 anchor는 J2 결합이 정하고 토큰과 무관 | 옛 episode로 잘못 귀속되지 않음 |
| R9-R12 | 각자 보류 | 그대로. preclaim은 합류 장부에도 적용 | |
| 키 전 등록 뒤, source 미정(`pre_source`) | V2 attempt(`pre_source`)와 `awaiting_echo` `AwaitingAcceptance` | 키를 눌렀는지 모르는 상태는 현행 `head_injection` 복구(R6)가 정한다. 키가 들어갔다면 J2가 새 source에서 같은 nonce·frame hash를 찾아 V2 `bind`로 결합한다 | 원래 anchor에 결합. 재주입 0 |
| 수락 뒤(`AcceptedAwaitingNative`, Claude는 queue 기록) | `awaiting_echo` | 짧은 기한 없이 남는다. TUI 대기열에 있던 입력이 재시작 뒤 native 턴이 되면 J2가 결합한다 | 게시 1회, 재전송 요청 0 |
| 지연 pin(`deferred_after_seq`, §5.5.3) | V2 `source_bindings`(BindingIntent, `selected`) | `selected`가 commit된 뒤면 같은 source로 history binding → ACK를 이어 간다. 확정 전이면 지연 분기 해석을 다시 한다(append-only `Bound`라 같은 첫 기록). 확정 직후·history binding 직후 ACK 전 재시작 모두 같은 source로 수렴한다. 실행 세대가 바뀌었으면 확정하지 않고 retained 정산 + 알림 | 확정 1회, 결합 1회, 재주입 0 |
| receipt 결합(J2) | tui_o native custody(receipt event), `anchor_bindings` | writer boot가 custody를 이어받고, resume 지점 앞 결합은 `anchor_bindings`에서 읽는다 | 같은 anchor |
| G1 observer(키 뒤 source 대기) | 메모리(observer closure) | 재시작하면 observer는 사라진다. 키가 나간 입력은 `awaiting_echo`·BindingIntent로 이어지고, 완료는 writer·native custody가 정산한다(지금 재시작 뒤 owner 턴 정산과 같은 경로) | 재주입 0 |
| 실행 용량 | S1은 바꾸지 않는다(§1.1) | 기존 guard는 프로세스와 함께 사라지고 DB 행은 TTL로 만료된다(지금과 같음). 채널 lease 복구는 S2(§13.4) | 지금과 같음 |
| 출력 소유 | boot policy·adoption | 부팅 때 `tui_o/cutover` 판정을 다시 한다. writer 준비 전(`channel_accepts` 거짓)에도 소유는 그대로다 | O 소유 채널에서 legacy 본문 0, writer 회복 뒤 O 본문 1 |

## 9. 인접 경로 전수

◆는 #8432와 직접 겹친다(표시만). ◇는 간접이다.

| # | 경로 | 진입 | 목표 계약 | 조각 |
|---|---|---|---|---|
| A1a | HTTP Cancel(보존) | `queue_api.rs:209` → `judged_stop.rs:59-69` | 합류 장부 입력은 토큰의 busy child라 함께 멈춘다. 토큰 없는 `Ok(None)` + `awaiting_echo`면 Claude `owned_input_stop`(`judged_stop.rs:458`). (S2 Codex: Esc 1회. 대기열이 있을 때 Esc가 "중단 후 즉시 전송"이라 Tab 대기 B가 곧바로 시작하므로 알림에 남긴다.) drain은 H11 뒤 | S1-C |
| A1b | Cancel(force) | `queue/http_force.rs:53` | `awaiting_echo` HostTerminated 정산. (S2: 후속 입력 lease 정리) | S1-C |
| A1c | `/cancel-queued` | `commands/control.rs:772` | 유지 | – |
| A2 | Herdr Terminate | `herdr_terminate.rs:151,514`, `retired_wake.rs:47` | 전이 잠금을 키까지 쥐므로 `herdr_terminate.rs:514` 재확인이 본다. `awaiting_echo` 정산. 이미 게시된 답의 anchor는 그대로 | S1-C |
| A3 | clear | `commands/control.rs:548,835`, V2 `clear.rs:17,154`, `session_idle_cleanup.rs:87,180` | `protected`(`v2/clear.rs:39-43`)에 귀속 대기 id 포함. clear 뒤 새 source id에는 옛 결합이 이어지지 않는다(§8). composer 미준비는 Hc | S1-C |
| A4a | `/stop`·`!stop` | `commands/control.rs:674`, `judged_stop.rs:352,449-458` | busy child를 inject로 옮겨 native stop 확인 유지. 흡수 전 Esc 회수(`Withdraw`)는 미전송 종료 + 재전송 요청 | S1-D |
| A4b | agent/Herdr stop | `agents_stop.rs:45`, `codex_stop_delivery.rs:251,417,772` | Codex Tab 입력이 연 턴의 stop을 `anchor_bindings`·`awaiting_echo` 근거로 허용(지금은 later native turn이라 409) | S2 |
| A5a | stale release | `stale_turn.rs:219,286` | 고아 행 해제 유지, 입력 결정권은 뺌 ◆ | S1-E |
| A5b | lease release | `turn_lease.rs:71,148,273` | inspect에 `awaiting_echo` 노출 ◆ | S1-C |
| A6 | #4030 demotion | `foreign_recovery.rs:74,365`, `destructive_cancel_gate.rs:234` | `awaiting_echo`가 있으면 pane을 살아 있음으로 봄 ◆ | S1-C |
| A7 | presence wake | `supervisor.rs:657,695,884` | wake 조건에서 토큰 제외(owner 보관 front), unknown 알림 조건에 `awaiting_echo` 추가, ➕는 합류 결합 뒤에만 ◇ | S1-A(➕), S1-C(wake·알림) |
| A8 | #3277 deadline | `turn_finalizer/reconcile.rs:223,270,331` | 토큰 턴 유지, 귀속 대기는 자기 deadline ◆ | S1-C |
| A9 | #8430 상한 | | 안전망 유지. 초안·모달·작업 중 화면에는 미적용(U1) | 별도 |
| A11 | (S2) 실행 용량 | `execution_capacity::acquire`(OI:1501-1515, `provider_dispatch.rs:623-628`), renew 실패 취소(`execution_capacity.rs:178-184`) | S1: 그대로(§1.1). S2: 채널 lease 하나(§13.2-§13.4) | S2 |
| A12 | 출력 cutover fence | `tui_o/cutover.rs:76-92` `owned_in`, `claim_then_send`·`o_keeps_body`(`tui_o/cutover/channel_gate.rs:232-263`) | 관찰자 본문 게시는 소유 판정으로만 정한다. readiness(`channel_accepts`)로 legacy에 넘기지 않는다 | S1-D |
| A10 | tui_o 게시 확인·재게시 | `src/services/tui_o/writer/confirm.rs`, `src/services/tui_o/writer/local_retry.rs`, `tui_o/repost/` | 재게시·확인 스캔은 `Prepared.reply_to`를 그대로 쓴다. reference가 달라 같은 payload를 다른 조각으로 보는 일이 없게 한다 | S1-A |

## 10. #8432·#8430 경계

- #8432와 겹치는 함수(표시만): `health/turn_deliver.rs:131,166`, `stale_turn.rs:219-302`, `turn_lease.rs:71-334`, `turn_finalizer/reconcile.rs:223-358`, `foreign_recovery.rs:74,365`. 구현은 #8432가 착지한 main 위에서 새 브랜치로 시작한다.
- 목표 경로는 입력 liveness를 토큰 해제에 기대지 않는다(`Target8432` GREEN). 그래도 Cancel·stop·lease 손잡이 때문에 #8432는 필요하다.
- #8430은 안전망으로 남는다.

## 11. S1 구현 조각 분할 (귀속을 맨 앞에)

크기는 추정이다(prod 코드 순증, 시험 제외). 측정은 착지 레그에서 `pr_cap_prod.py <base> HEAD`로만 한다.

giant 파일에는 블록 이동·호출 교체·필드나 enum 변형 추가만 한다:
- OI·IG·IT
- `tui_o/store/ledger.rs`(1174줄)
- `restore_inflight.rs`(2796줄)
- `claude_tui/hosting/warm_followup.rs`(1057줄)

`src/services/tui_o/writer/deliver.rs`는 998줄이다. 조각에서 `reply_to` 전달 한 줄만 더하면 1000줄을 넘으므로, 같은 조각에서 `send_piece`의 Prepared 생성 블록(약 15줄)을 child로 옮긴다.

S1은 `execution_capacity`를 건드리지 않는다(§1.1). Codex 파일도 건드리지 않는다.

순서: **S1-A → S1-B → S1-C → S1-D → S1-E**. S1-B(transcript 없는 키)를 S1-D(단일 경로)보다 앞에 둔다. 그래야 S1-D가 보류를 지울 때 source 보류가 다시 생기지 않는다.

### S1-A — 답 anchor 귀속과 합류 표시 (w3 A1 + w5 receipt 결합)
- 파일(prod 11개):
  - 새 child: `src/services/tui_o/writer/reply_anchor.rs`(+270: native 턴 추적, receipt 결합(꼬리표 없는 opener 포함), nonce 보조 판독, 결합·합류 규칙, 실패면 결합 0), `src/services/tui_o/store/anchor_bindings.rs`(+130: durable journal), `src/services/tui_o/writer/deliver/prepared.rs`(블록 이동 +15)
  - 수정:
    - `src/services/tui_o/writer/pieces.rs`(+15, `reply_to` 운반)
    - `src/services/tui_o/writer/mod.rs`(+8, 포트 인자)
    - `src/services/tui_o/writer/deliver.rs`(-15 +3)
    - `tui_o/store/ledger.rs`(+3, 선택 필드)
    - `discord/outbound/o_writer_io.rs`(+20, message reference)
    - `turn_view_reconciler/injected.rs`(+30, 흡수 결합 뒤 ➕와 별도 턴이면 거두기)
    - `src/services/tui_input/human_frame/history/v2.rs`(+25, ticket → attempt anchor 조회)
    - `src/services/tui_o/writer/rotation/owe.rs`(+8, 기록한 receipt를 reply_anchor에 넘김)
- 예상: prod 11파일 / 순 +515.
- RED 계획(현행에서 실패 확인 뒤 구현):
  1. tui_o 조각 게시에 reference가 없다(현행 `post_unit`에 인자 없음). 목표는 opener anchor reply다.
  2. **꼬리표 없는 opener(w5 P1-1)**: 실제 owner Idle 경로(`human_frame::idle`)가 만든 태그 없는 A → A 답 reference = A. 시험용 A에 nonce를 붙이지 않는다.
  3. 같은 A에 태그 있는 B가 흡수되면: 합친 답은 A reference, B에 흡수 결합 뒤 ➕ 1회, B 별도 답 0. 헤더 nonce만 읽는 변형(`TaggedHeaderOnly`)은 RED다(A 답이 B에 걸림).
  4. Cold·HerdrIdle opener와 재시작 뒤에도 같은 identity로 결합한다.
  5. receipt 유일성 실패(같은 본문 두 번, 짝 ticket 둘) → 결합 0, reference 없이 게시, 오귀속 0.
  6. 외부 턴 A + 붙여넣은 B: A의 조각은 reference 없음(합류가 아니면), B의 조각은 B reply(게시 순서 귀속 금지).
  7. ➕ 시점(P2-3): 바쁜 화면에 붙여넣었지만 A 뒤 자기 턴으로 돈 B는 ➕ 0, B 답 1회. `MarkOnPaste` 변형 RED. 키 직후 달린 ➕는 별도 턴 결합 뒤 거둬진다.
  8. 한 user 행에 nonce 둘(두 nonce fixture): 첫 anchor reply, 둘째 ➕와 알림.
  9. 재시작: `Prepared{reply_to}` 뒤 결과 모르는 POST 확인 스캔이 같은 조각으로 인정, `anchor_bindings`에서 anchor 복구.
  10. J5 관측 줄과 `relay_delivery` 1회.

### S1-B — Claude 키 전 등록, source 관문 전수, 지연 pin 확정 (P1-2·P1-3)
- 파일(prod 12개):
  - 새 child: `src/services/tui_input/human_frame/history/v2/key_admit.rs`(+70), `src/services/claude_tui/hosting/deferred_source.rs`(+80, G1 키 뒤 source 대기)
  - 수정:
    - OI(E1, metadata 실패면 `pre_source`, +4/-2)
    - `owner_input/native_input.rs`(E2 호출부, +4)
    - `turn_finalizer/native_binding.rs`(E2 지연 pin 만들기 +30, `bound_source`·`resolve_pending` 지연 분기 +30)
    - `turn_orchestrator/queued_head/v2_types/cold_source.rs`(E3 선택 필드 +3)
    - `turn_orchestrator/queued_head/v2/cold_source.rs`(E3 검사 분기 +10, `select_source` 지연 분기 단일 확정 +15)
    - `claude_tui/host_input/busy.rs`(E4 `admits`, G2 `key_ready` 사용, +11/-4)
    - `claude_tui/host_input/busy/herdr.rs`(E5 +12/-10, G2 `Context::key_ready` +20)
    - `claude_tui/busy_inject.rs`(E6, +12/-10)
    - `src/services/tui_input/human_frame/history.rs`(E7 owner 범위 분기, +8)
    - `claude_tui/hosting/warm_followup.rs`(1057줄 giant: G1 source 해석을 `deferred_source` 호출로 교체, +6/-4)
- 예상: prod 12파일 / 순 +285.
- RED 계획(실제 owner 진입점에서):
  1. **G1**: `owner_drive → start_idle → execute_owned → take_owned`에 토큰 없음·live tmux pane·O ready·transcript 파일 없음 → paste 1, Enter 1. 이후 `Bound` 공급 시 확정·결합·ACK, 재주입 0. 현행은 `NotSent("idle_unobserved")`이고, observer를 키 전에 요구하는 변형(`RequireIdleObserverBeforeKey`)은 RED다.
  2. **G2**: 실제 `Context::read()`를 돌린다. `cleared=false`, `idle_ready=false`, `source=None`, pane·composer 정상 → paste 1, Enter 1. 상태를 손으로 Busy로 만들지 않는다. `RequireBusyReadStateWithoutSource` 변형은 RED다.
  3. **E2**: O writer 준비 + `Bound` 없음 → paste 1, Enter 1(`PinSourceFirst` RED).
  4. **단일 확정(P1-3)**: 지연 pin 뒤 `Bound(Source)`, `Bound(Pending)`→`Resolved` 두 경로 모두 실제 V2 `Owner`에서 `selected` 확정 → `bind_native_source` → `ack_source` 완료. 지연 pin에도 session·path 동등성을 적용하는 변형(`DeferredEquality`)은 RED다.
  5. 재시작: `selected` 저장 직후, history binding 직후 ACK 전에서 각각 재시작해도 같은 source로 수렴하고 재주입 0.
  6. 선택 제외: 다른 execution nonce·owner·provider, watermark 이전 `Bound`, 같은 owner의 옛 실행 `Bound`가 남은 fixture에서 선택 0.
  7. 새 세션(transcript 파일 없음), tmux·Herdr 각각 → paste 1, Enter 1(`RequireTranscript`, 무토큰 포함).
  8. 대조군(키 0): pane·실행 nonce 교체, writer 변경, 모달, 사람 초안.
  9. 등록 실패는 키 0(`KeyAfterRegister`).

### S1-C — 귀속 대기 손잡이와 인접 경로 (w3 A2의 Claude 부분)
- 파일(prod 11개):
  - 새 child: `src/services/turn_orchestrator/queued_head/v2/awaiting_echo.rs`(+180: `AwaitingAcceptance`·`AcceptedAwaitingNative`, 철회 증거), `src/services/discord/recovery_engine/awaiting_echo_boot.rs`(+45)
  - 수정: `v2_types.rs`(+12), `v2.rs`(+20), `v2/clear.rs`(+8), `v2/attempt.rs`(+10, 수락 전이가 `confirmation_deadline_ms`를 끝냄), `herdr_terminate.rs`(+25), `judged_stop.rs`(+25), `turn_lease.rs`(+15), `turn_presence/supervisor.rs`(+15), `v2/retained.rs`(+30)
- 예상: prod 11파일 / 순 +385.
- RED 계획:
  1. clear protected, terminate 정산, boot 재결합.
  2. `AwaitingAcceptance` 기한 초과면 retained 재전송 요청 1회, 재주입 0.
  3. Claude 수락(queue 기록) 뒤 30초가 지나도 Unconfirmed 0, 재전송 요청 0.
  4. 대기열에서 사라졌지만 아직 결합 전(J2 읽기 지연)이면 재전송 요청 0, 결합·게시 1회(리뷰 i8328-design-3 P2).
  5. 명시적 철회(Esc `Withdraw`, 세션 종료)일 때만 재전송 요청이 나간다.

### S1-D — Claude 사람 입력 단일 경로 (w3 A3)
- 파일(prod 12개):
  - 새 child: `src/services/discord/router/message_handler/provider_dispatch/owner_input/inject.rs`(+290), `src/services/discord/router/message_handler/intake_turn/owned_busy/late_start.rs`(이동 +60)
  - 수정·삭제: OI(-210), `busy_turn.rs`(삭제 -41), OB(-250), IT(-20), WF(관찰자 본문 송신을 `claim_then_send` 안으로 +10), `busy_retry.rs`(+5), `claude_tui/busy_inject/ownership.rs`(+15, 키 전 보류 판정), `queue_io/input_owner/effect/claude.rs`(-10, 제출 뒤 알림 → 키 전 거절), `claude_tui/host_input/busy/herdr.rs`(owner 경로에서 C-s 안 씀 -8), `claude_tui/busy_inject.rs`(같음 -5)
- 예상: prod 12파일 / 순 약 -165.
- RED 계획:
  1. #8432 모양(남은 토큰 + Idle) → Submitted 1회. 현행은 NotSent 반복이다.
  2. 토큰 없는 가지의 전이 잠금
  3. U3 보류
  4. iMessage reply_context 보존
  5. busy child가 키 전에 고정됨
  6. 사람 초안 보류, tmux·Herdr 각각: (a) stash A + 초안 B → NotSent, C-s·paste·Enter 0, 두 초안 보존. (b) 사람 글자만 있음 → C-s 0, paste·Enter 0, 알림 1. `Unsettled`를 통과시키는 변형과, 키 전 거절을 제출 뒤 알림으로 바꾸는 변형은 RED다.
  7. 출력 소유: O 소유 채널에서 `resumed=false`·`stopping=true` → legacy 본문 0, writer 회복 뒤 O 본문 1. legacy 소유 채널에서 anchor reply 1회와 `Pending → Committed` 전환이 송신 직전에 겹쳐도 본문 합계 1회(대조군).
  8. 관문 회귀(리뷰 i8328-design-4): 사람 입력을 `inject::hand`로 옮긴 뒤에도 G1·G2·E1–E8의 source 관문이 다른 이름으로 남지 않는지, 실제 `owner_drive` 진입점에서 새 세션·재결합 창 입력이 paste 1·Enter 1인지 확인한다.

### S1-E — intake 적재·wake와 재시작 정리 (w3 A4)
- 파일(prod 5개): IG(-30/+25), `intake_gate/queue_effects.rs`(+10), `mailbox_probe.rs`(+5), `ingress.rs`(+5), `stale_turn.rs`(-10).
- 예상: prod 5파일 / 순 +5.
- RED 계획: blocking 토큰이 있을 때 적재하면 owner wake 1회(현행 없음). kickoff → reconcile 순서를 고정한다.

### S1 합계
- 서로 다른 prod 파일 약 47개, 순 약 +1025(+515, +285, +385, -165, +5).
- w4 추정(44 / +855, 최대 S1-A 10 / +465)보다 +3파일·+170줄이다. 사유:
  - S1-A +1파일·+50: receipt 결합 경로(`reply_anchor` +40, `owe.rs` receipt 전달 +8)
  - S1-B +2파일·+120: G1 `deferred_source.rs` +80, `warm_followup.rs` 호출 교체, G2 `key_ready` +23, `select_source` 지연 분기 +15
- 조각 최대 증가는 S1-A(11파일·+515)다. 조각마다 30파일·+1500 상한과 PLAN_READY 선 +1200 아래다.
- 추정이다. 이 설계 레그의 실측(`pr_cap_prod.py`)은 prod 0이다(문서·모델만).

## 12. 남은 위험·미확인

1. **출력 소유와 readiness(P2-4로 정정)**: w2는 `tui_o::writer::host::channel_accepts`가 거짓인 채널을 legacy relay 채널로 보았다. 이것은 틀렸다. `channel_accepts`(`tui_o/writer/host.rs:269-278`)는 readiness·gateway 소유·stopping·resumed를 함께 본다. 그래서 O 소유 채널에서도 재개 전이나 정지 중에는 거짓이다. 소유는 boot policy와 adoption 계약이다(`tui_o/cutover.rs:1-4,76-92`). 관찰자 게시는 소유 판정과 `claim_then_send`·`o_keeps_body` fence로만 정한다. legacy 소유 채널(소유 판정이 거짓)에서는 J3·J4가 닿지 않는다. 그런 채널에서는 기존 관찰자가 게시하고, anchor는 J1·J2 결합 기록을 공유해 같은 규칙으로 단다. S1-D 시험 7이 이 경계를 고정한다.
2. **J2의 nonce 판독**: 사용자가 frame 헤더를 흉내 낸 텍스트를 TUI에 직접 치면 가짜 nonce가 생긴다. 결합은 `awaiting_echo`·history에 같은 nonce와 같은 frame hash(`src/services/tui_input/human_frame/history/v2/native.rs:40` `frame_hashes`)가 있을 때만 인정한다.
3. **Codex Vim 모드·side conversation**: 운영 launch 설정을 읽지 않는 범위라 Vim 시작 여부를 확인하지 못했다. 화면 판정으로 보관한다.
4. **Claude managed stash(P2-2로 정정)**: w2는 "초안을 제출하지 않으므로 U1과 충돌하지 않는다"고 보았다. 이것은 틀렸다. 기존 stash + 현재 사람 초안(`Unsettled`)에서 지금 코드는 B+frame을 제출한다(`stash_tests.rs:369-377`). 목표에서는 §7.1처럼 키 전에 보류한다. **S1은 owner 사람 입력 경로에서 managed stash(C-s)를 쓰지 않는다.** 사람 글자는 모두 보류하고 알림만 한다(U1). 예외를 다시 들일지는 메인 결정으로 남긴다. 들인다면 tmux `stash.rs:72-92`, Herdr `busy/herdr.rs:88-98`에 C-s 뒤 확인 capture가 필요하다.
5. **S1의 실행 용량 미계상(§1.1)**: 흡수되지 않은 합류 B와 #8432 모양의 B는 lease 없이 자기 턴으로 돈다. 앞의 것은 지금 hand_busy와 같은 성질이고, 뒤의 것은 지금은 무한 보류라서 S1이 "전달됨·미계상"으로 바꾼다. 노드 용량을 채널당 최대 1턴 과소 계상할 수 있다. S2의 채널 lease 표가 고친다. 메인이 S1 착지 전에 받아들일지 확인할 항목이다.
6. 앞 턴 relay가 뒤 입력의 자기 턴 출력까지 앞 턴 몫으로 보내지 않는지(w1 위험 1)는 tui_o writer 하나가 게시하고 조각마다 턴 결합으로 anchor를 정하는 구조에서는 문제가 되지 않는다. 다만 legacy 소유 채널(위 1)에서는 남는다.
7. **원시 증거 재집계**: 리뷰는 12건 불일치 분류의 원시 증거가 head에 없어 다시 셀 수 없었다고 적었다. 원시 증거(`logs/tab-1/attribution-*`)는 비밀값 검사 전이라 저장소에 넣지 않았다. 대신 S1-A 시험 5와 S2 Codex 두 입력 fixture(§13.5)가 같은 모양의 fixture를 저장소에 들인다.

8. **P2(리뷰 i8328-design-4)**: S1 모델에서 transcript 생성과 source 결합·확정·anchor 결합을 나눴고, `NoLateBind`·`RequireTranscriptFresh`를 RED로 확인했다(§1.0). custody ACK의 재시작 수렴은 S1-B Rust 시험 5가 맡는다.

## 13. S2 설계 항목 (이번에 풀지 않음)

S2 = Codex 항상 Tab + 채널 lease·slot 공유 표. S1 착지 뒤 별도 설계 회차에서 수렴한다. S2 모델은 `OwnerInputBusySplit.tla`(cfg 30, w3 그대로)다. 아래는 w3 설계 본문과 리뷰 i8328-design-3이 남긴 숙제다.

### 13.1 Codex 항상 Tab (w3 §2.3·§2.4를 옮김)

#### 13.1.1 Codex 항상 Tab 절차

1. composer 잠금(`try_with_composer_mutation_lock`) 안에서 capture한다. `delivery_veto`·draft·readiness·Hq를 확인하고, 하나라도 걸리면 키 없이 NotSent다.
2. 붙여넣기 1회. 키 효과는 1이고, 귀속 대기 항목을 만든다.
3. **반영 확인**: 같은 잠금 안에서 capture해 frame의 nonce 꼬리표가 composer에 보이는지와 `delivery_veto`가 여전히 None인지 본다.
   - 실패하면(반영 안 됨, 또는 붙여넣기 뒤 completion popup이 열림) 키를 누르지 않는다. `WriteUncertain`으로 끝내고 소유 초안 알림(`codex_tui::input::notify_owned_draft`)을 보낸다. 재붙여넣기는 없다.
   - 근거: Codex의 "Paste detection — Recognize pasted-text bursts and keep multiline input together"(바이너리 문구)와 실측 busy1(붙여넣은 직후 Tab → 두 건 합쳐짐·셋째 건 초안 잔류, 0/3).
4. **Tab** 1회.
5. **수락 확인**: B가 `Queued follow-up inputs` 항목(`screen.rs:22-27` QUEUE_HEADERS)으로 옮겨졌거나 rollout에 B의 사용자 표식이 나타나면 수락이다.
   - 귀속 대기 항목을 `AwaitingAcceptance`에서 `AcceptedAwaitingNative`로 옮긴다(§5.1). 이 전이가 30초 확인 기한(`v2/attempt.rs:107,161`의 `confirmation_deadline_ms`)을 끝낸다.
   - 수락된 B는 A가 몇 분을 돌든 native 턴이 올 때까지 기다린다. 재전송 요청은 없다.
   - 다음 front는 이 수락을 기다린다(Hq).
   - 수락된 B는 채널 lease의 후속 입력으로 남는다(§13.4).
6. 고정 지연은 쓰지 않는다. 실측의 0.12s는 관측치일 뿐이다.

구현 손잡이:
- `session_host::PinnedInput`(`session_host/submit.rs:56-59`)에 `Tab`을 더한다.
- `codex_tui::input::TuiInputAction`(`codex_tui/input.rs:467-472`, 3128줄 giant)에 enum 변형만 더한다.
- `codex/herdr_turn/busy.rs:125-128`의 `[PasteBuffer, Enter]` 한 번 실행을 "붙여넣기 → 반영 확인 → Tab"으로 나눈다.
- `codex_tui/busy_inject.rs:436`과 `effect/codex.rs` `idle_codex`(Enter)는 같은 절차로 합친다.
- `target::route`의 Codex 경로 넷(CodexBusy·CodexIdle·CodexHerdrBusy·Codex HerdrIdle, `target.rs:44-57,81-88`)은 host별 하나씩(tmux·herdr)으로 줄인다.

#### 13.1.2 Codex 화면 상태별 Tab의 뜻 (근거와 처리)

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
| slash 명령 popup(composer가 `/`로 시작) | 명령 자동완성 | 바이너리 "Type `/` to open the command popup; `Tab` autocompletes slash commands.". frame은 `[`로 시작해 열리지 않는다(`src/services/tui_input/human_frame.rs:102-115` `plain_inject`) | **보관**(`completion_popup` veto, `screen.rs:85-99`) |
| 파일 mention 등 제안 메뉴(`@…`) | 선택 항목 삽입 | 코드 `screen.rs:97` 힌트 "enter/tab insert · esc close". 바이너리 "only suggestion menus use above-composer placement" | 붙여넣기 뒤 열리면 반영 확인에서 거부하고 키 없이 WriteUncertain(Esc는 바쁜 턴을 끊으므로 쓰지 않음) |
| 승인·권한 모달, 번호 선택 메뉴, "press enter to continue" | 모달 조작(Tab 뜻 미확인) | 코드 `codex_tui/input/submission.rs:236` `submission_modal_in_pane`, `input/inline_banner.rs:12` `pane_has_blocking_action_menu`. 바이너리 승인 동작 목록 | **보관**(veto) |
| 닫을 수 있는 action banner(빈 composer에서 숫자·Esc를 받음) | 미확인 | 코드 `inline_banner.rs:5-10` | **보관**. `delivery_veto`가 이 banner를 포함하지 않으므로 A5에서 더한다 |
| request_user_input 질문 화면 | 질문 이동("Move forward through questions…") | 바이너리 | **보관**. 화면 문자열 판정은 A5에서 fixture로 더한다(미확인 → 보관) |
| transcript overlay·review picker | overlay 조작 | 코드 `screen.rs:15-21` MODAL_FOOTERS | **보관**(veto) |
| Vim composer 모드(normal) | 붙여넣기와 키가 명령으로 해석될 수 있음 | 바이너리 "Turn Vim composer mode on or off", "Start in Vim mode" | **보관**(composer 모드 판정을 못 하면 보관). launch 설정에 Vim 시작이 없는지는 운영 설정을 읽지 않는 범위에서 확인 못 함 |
| side conversation·sidebar에 포커스 | 메인 composer가 아닐 수 있음 | 바이너리 "Focus the side conversation…" | **보관**(메인 composer `›` 행을 못 찾으면 `Composer::Unread`) |
| 사람 초안 있음 | 초안과 합쳐 제출·대기 | 코드 draft 판정 | H1 보관 |

**결론: codex_key = 항상 Tab.** 확인한 상태에서는 모두 Tab이 맞고, 나머지는 키 시점 거부로 보관한다. 바쁨 판정은 키 선택에도 필요 없다.


### 13.2 P1-1: lease 등록·반납을 모든 실행 경로에 (리뷰 i8328-design-3)

- w3 §5.4와 A6는 `owner_input.rs:1501,1568`과 Herdr의 획득만 지정했다.
- tmux 스트리밍 턴(cold 시작 포함)은 `provider_dispatch.rs:406-411`에서 따로 잡는다. 이 A가 실행 중이면 B의 `join`은 표에서 lease를 못 찾아 두 번째 획득을 시도하고, slot 부족이나 PK 충돌로 보류된다.
- Herdr는 guard를 표에 넘긴 뒤 `provider_dispatch.rs:653,670`에서 반환하므로, 명시한 반납 지점을 지나지 않는다.
- 숙제: 세 실행 경로(OI 자기 턴, tmux 406, Herdr 623)를 같은 세대별 수명 관리에 연결해 모든 반환에서 종료를 정산한다.
- 시험: slot 1·2에서 tmux A 실행 중 B 수락과 DB nonce 유지. Herdr 성공·오류·취소에서 마지막 턴 뒤 행 0, 다른 채널의 slot 획득.

### 13.3 P1-2: 키를 못 보낸 입력의 slot 점유 (리뷰 i8328-design-3)

- w3 `join`은 V2 등록 전에 후속 nonce를 더하고, `followup_done`에는 등록 실패와 확정 `NotSent`가 없다.
- 숙제: `join`을 임시 예약으로 만들고, 확정 미전송이면 반환과 함께 해제한다. 키 효과가 불명확하면 추적 상태로 넘기고 `Unconfirmed` 이후의 반납 조건도 정한다.
- 시험: 등록 실패, 화면 확인과 키 사이 대상 변경, 확정 NotSent에서 A 종료 뒤 행 0, slot 1에서 다른 채널 실행 가능.

### 13.4 w3 채널 lease 설계 (출발점)

지금 코드:
- `node_execution_leases`의 PK는 `(instance_id, provider, channel_id)`다(`migrations/postgres/0126_execution_capacity.sql:2-8`). 채널마다 행은 하나뿐이다.
- `store::acquire`는 만료 행을 지우고 일반 INSERT를 한다(`src/services/cluster/execution_capacity/store.rs:3-26`). 같은 채널에 살아 있는 행이 있으면 PK 충돌이다.
- renew는 자기 nonce로만 한다(`store.rs:28-38`). renew가 실패하면 provider를 취소한다(`src/services/cluster/execution_capacity.rs:178-184`, `cancel_with_tmux_cleanup`).
- 그래서 Tab마다 acquire하면 같은 채널의 실행 중 lease와 충돌한다(`AcquirePerTab`). `ON CONFLICT DO UPDATE`로 nonce를 덮으면 실행 중 턴의 renew가 실패해 그 턴이 취소된다(`NonceOverwrite`). 둘 다 쓰지 않는다.
- 자기 턴 장부만 키마다 잡고(OI:1493-1515), busy 가지는 "이미 잡은 턴에 쓴다"며 잡지 않는다. guard는 그 턴과 함께 drop된다(OI:1568). 이 때문에 바쁠 때 붙여넣은 입력이 A 뒤 자기 턴이 되면 lease 없이 돈다(모델 `CurrentBusy` RED `LeaseCovers`).

목표: 새 child `src/services/cluster/execution_capacity/channel_lease.rs`(프로세스 안 채널별 표)
- `hold(channel, guard)`: 턴을 시작한 쪽이 받은 `ExecutionGuard`를 채널 lease로 맡긴다. 맡기는 곳은 OI take_owned의 `capacity`(OI:1501-1515)와 Herdr 실행의 `_execution_guard`(`provider_dispatch.rs:623-628`)다. giant 두 곳에는 호출 교체만 한다.
- `join(channel, nonce)`: §2.1 3단계, 키 전에 부른다.
  - lease가 있으면 후속 입력 집합에 nonce를 더한다. DB 호출은 없다.
  - 없으면 기존 `acquire`로 하나 잡아 `hold`한다. 거절되면 `NotSent(capacity)`(Hk)이고 키는 0이다.
- `turn_ended(channel)`: OI:1568의 `drop(capacity)` 자리에서 부른다. 후속 입력이 남아 있으면 guard를 지킨다. renew는 같은 nonce로 계속된다.
- `followup_done(channel, nonce)`: 후속 입력이 끝났을 때 부른다. 끝남은 그 입력이 연 native 턴의 `Closed`(J2·J3, reply_anchor가 결합 기록으로 앎), 합류한 턴의 종료, `awaiting_echo`의 철회·세션 종료·clear·Terminate 정산이다. 후속 입력도 실행 중 턴도 없으면 guard를 drop한다. 반납은 nonce로 DELETE한다(`store.rs:39-49`).
- 다른 채널은 영향을 받지 않는다. node slot(`semaphore`, `execution_capacity.rs:123-153`)은 채널 lease 하나에 permit 하나다. 한 채널의 후속 입력은 slot을 더 쓰지 않는다.
- 재시작: 프로세스 안 표와 renew loop는 사라진다. DB 행은 30초(`LEASE_SECONDS`) 뒤 만료된다. `awaiting_echo_boot`는 `AcceptedAwaitingNative`나 결합된 실행 중 후속 입력이 있는 채널에서 lease를 하나 다시 잡는다. 옛 행이 아직 살아 있어 PK 충돌이 나면 덮어쓰지 않고 만료까지 다시 시도한다(§12-5).


### 13.5 S2 P2 숙제와 모델 가드 (리뷰 i8328-design-3)

- lease 복구: Tab 수락 직후 상태 저장 전에 재시작하면 B가 `AwaitingAcceptance`에 남아 복원 대상에서 빠진다. 신규 실행 승인 전에 키 효과가 불명확한 항목을 TUI 증거와 대조하고, 복원 대상의 slot을 먼저 예약한다. 시험은 slot 1에서 Tab 직후와 수락 저장 직후를 각각 중단하고 신규 채널을 동시에 넣는 것이다.
- Codex 대기열 소멸은 미실행 증거가 아니다(§5.1에 이미 반영. S2 Codex 시험: rollout 읽기 지연 중 요청 0, 결합·게시 1).
- S2 모델 가드
  - `PasteCodex` 전에 용량 가드를 둔다(지금 외부 턴 GREEN은 lease 없이 붙여넣고 Tab에서 처음 잡는다).
  - `AcquirePerTab`은 행 1을 유지하고 A 종료 전 B 수락 실패를 내게 바꾼다(실제 운영 보류 재현).
  - `AcceptedStillExpires`는 타이머와 알림을 따로 모델링한다.
  - Codex 두 입력 fixture(불일치 b·c, w3 A5 시험 8).

### 13.6 w3 조각 원문 (S2 출발점)

#### w3 A2 (Codex 부분만 S2: Hq 수락 표시, `destructive_cancel_gate.rs`, Codex 수락 시험)
- 파일(prod 12개):
  - 새 child: `src/services/turn_orchestrator/queued_head/v2/awaiting_echo.rs`(+200: `AwaitingAcceptance`·`AcceptedAwaitingNative`, 철회 증거), `src/services/discord/recovery_engine/awaiting_echo_boot.rs`(+50)
  - 수정: `v2_types.rs`(+12), `v2.rs`(+20), `v2/clear.rs`(+8), `v2/attempt.rs`(+10, 수락 전이가 `confirmation_deadline_ms`를 끝냄), `herdr_terminate.rs`(+25), `judged_stop.rs`(+35), `turn_lease.rs`(+15), `turn_presence/supervisor.rs`(+15), `v2/retained.rs`(+30), `destructive_cancel_gate.rs`(+10)
- 예상: prod 12파일 / 순 +430.
- RED 계획:
  1. clear protected, terminate 정산, Hq 수락 표시, boot 재결합.
  2. `AwaitingAcceptance` 기한 초과면 retained 재전송 요청 1회, 재주입 0.
  3. **수락 뒤 기한(P2-1)**: 가상 시계에서 A가 60초 돌고 B가 Tab으로 수락됐다. 30초가 지나도 Unconfirmed 0, 재전송 요청 0이다. 재시작을 넘어 B가 결합되고 1회 게시된다. 수락 뒤에도 기한이 걸리는 변형(`AcceptedStillExpires`)은 RED다.
  4. 철회 증거(Esc `Withdraw`, 세션 종료)가 있을 때만 재전송 요청이 나간다.

#### w3 A6 — 채널 실행 lease 하나
- 파일(prod 7개):
  - 새 child: `src/services/cluster/execution_capacity/channel_lease.rs`(+160: `hold`·`join`·`turn_ended`·`followup_done`)
  - 수정: `src/services/cluster/execution_capacity.rs`(+3, mod), OI(take_owned `capacity` → `hold`, OI:1568 `drop` → `turn_ended`, +4/-2), `provider_dispatch.rs`(Herdr `_execution_guard` → `hold`, +3/-1), `awaiting_echo_boot.rs`(+20, 재시작 뒤 다시 잡기), `reply_anchor.rs`(+8, 후속 턴 Closed → `followup_done`), `v2/awaiting_echo.rs`(+8, 철회·종료 → `followup_done`)
- 예상: prod 7파일 / 순 +205.
- RED 계획(로컬 PG 시험 하네스, `execution_capacity/tests.rs` 방식):
  1. slot 1, A 실행 중 + B Tab 수락 → 수락 성공, DB lease 행 1(현행 방식으로 B가 acquire하면 PK 충돌 또는 slot 거절).
  2. slot 여유가 있어도 행은 1이다.
  3. A가 끝나도 B가 남아 있으면 lease를 유지하고, B가 끝난 뒤 반납한다(행 0).
  4. 다른 채널은 여전히 slot 상한에 걸린다.
  5. Claude: 바쁜 화면 붙여넣기 뒤 A 뒤 자기 턴 B가 lease를 가진다(현행 `CurrentBusy` 모양, lease 0).
  6. 변형 RED: Tab마다 acquire(`AcquirePerTab`), `ON CONFLICT DO UPDATE` nonce 덮어쓰기(`NonceOverwrite`). 덮어쓰기에서는 A의 renew가 실패해 A가 취소된다.
  7. 재시작: 옛 행이 살아 있는 동안 boot 재획득은 덮어쓰지 않는다. 만료 뒤 행 1이다.

#### w3 A5 — Codex 항상 Tab
- 파일(prod 10개):
  - 새 child: `src/services/codex_tui/busy_inject/tab_protocol.rs`(+130: 반영 확인·수락 확인·veto 보강, 키 전 `channel_lease::join`)
  - 수정: `session_host/submit.rs`(+8)·`tmux_host.rs`·`herdr_gate.rs`(+20), `codex_tui/input.rs`(enum +6), `codex_tui/busy_inject.rs`(+10/-20), `codex/herdr_turn/busy.rs`(+15/-15), `effect/codex.rs`(`idle_codex` 교체 +20/-70), `queue_io/input_owner/target.rs`(-20), OI(Codex 경로 합침 -35), `codex_tui/busy_inject/screen.rs`(action banner·질문 화면 veto +20), `codex_stop_delivery.rs`(+20)
- 예상: prod 10파일 / 순 약 +70.
- RED 계획:
  1. Codex 바쁜 화면: 현행은 `NotSent("busy_turn: transcript unavailable")` 또는 Enter다. 목표는 Tab 1회다.
  2. idle 화면도 Tab(현행 `idle_codex`는 Enter).
  3. 반영 안 됨이면 키 0 + WriteUncertain(재붙여넣기 0).
  4. 붙여넣은 뒤 completion popup이면 키 0.
  5. 수락 미확인이면 다음 front Hq.
  6. action banner·질문 화면 fixture면 보관.
  7. `/clear` 직후 readiness 전이면 Hc.
  8. **A5 두 입력 fixture(불일치 b·c 모양)**: 붙여넣기 직후 Tab으로 두 입력이 한 사용자 메시지에 합쳐진 rollout, 앞 초안 잔류 + 다음 시작이 합쳐진 rollout. 각 입력의 답·➕·알림이 §6.3 "nonce 둘" 규칙대로 나오는지 본다. 목표 절차에서는 반영 확인·Hq 때문에 이 모양이 생기지 않는다(키 순서 시험).
  9. Tab 수락이 채널 lease를 하나만 쓴다(A6 API 호출 1회, DB 호출 0).
- 화면 fixture는 i8328tab1 `logs/tab-1/` 스냅샷에서 옮긴다(비밀값 없음 확인 뒤).

