# owner 입력 busy 분기: 현재 경로 대 바로 주입·합류·채널 lease·Codex 항상 Tab·답 anchor 귀속 (#8328 1단계 설계 w3)

- 기준 코드는 `82702fe252`다.
- 다루는 범위: 사람 입력 B 하나와 앞 턴 A 하나.
- 상수 구분
  - `Policy`: 현재 경로(`Current`)와 목표 경로(`Target`)
  - `Provider`: Claude와 Codex
  - `Scenario`: 앞 턴의 종류
- 모델 결과는 Rust 시험을 대신하지 않는다.
- 설계 문서: [`docs/design/8328-owner-input-busy-inject.md`](../../design/8328-owner-input-busy-inject.md)

## 판 이력

- **설계 준비판**(`124a47a5e3`, cfg 10개)
  - 전제 3: TUI 대기열이 입력마다 별도 턴을 지킨다.
- **설계 w1**(`cafed27b40`, cfg 20개)
  - 전제 3을 바꿨다: 유실 0·중복 0, Claude 합류 시 합친 답 1회.
  - Codex는 바쁨 판정으로 키를 골랐다(바쁨 Tab·idle Enter).
- **설계 w2**(`1113953fa4`, cfg 24개)
  - Codex는 바쁨 판정 없이 항상 Tab이다. 절차는 붙여넣기 → 반영 확인 → Tab → 수락이다.
  - 귀속을 들였다. 답은 모두 입력 anchor 하나에 reply로 건다.
  - 귀속 규칙: 턴을 연 입력의 anchor를 쓴다. 없으면 처음 합류한 Discord 입력의 anchor, 그것도 없으면 none이다.
  - 귀속이 틀린 mutant(게시 순서 귀속, reply reference 없이 게시)를 더했다.
  - liveness는 그대로다.
- **설계 w3**(이 판, cfg 30개). 설계 리뷰 i8328-design-2를 반영했다.
  - 합류 표시(`mark`): 목표는 `TuiAbsorb`(흡수 증거)에서만 세운다. 붙여넣기 때 세우는 것은 현재 경로의 동작이고, 목표에서는 `MarkOnPaste` mutant다. 새 규칙 `MarkOnlyJoined`(`mark ⇒ b = joined ∨ merged`)를 더했다. w2 목표 모델은 붙여넣기 때 표시를 세워서, 리뷰가 찾은 7상태 반례(Arrive→DriveTargetClaude→AEnds→TuiStart→BEnds→PostB)처럼 별도 턴으로 돈 B에도 ➕가 남았다.
  - 채널 실행 lease(`leases`): `node_execution_leases`는 채널마다 행 하나다. 목표에서는 수락된 후속 입력이 실행 중 턴의 lease를 이어 쓰고, 채널에 lease가 없을 때만 키에서 새로 잡는다. 반납은 마지막 후속 턴이 끝난 뒤다. 새 규칙 `OneLease`·`LeaseCovers`·`LeaseReleased`, 새 mutant `AcquirePerTab`(Tab마다 획득)·`NonceOverwrite`(두 번째 획득이 nonce를 덮어씀).
  - transcript: Claude 주입은 transcript 파일을 기다리지 않는다. 새 cfg `TargetIdle`(새 세션)은 GREEN이고, 새 mutant `RequireTranscriptBeforePaste`(지금의 BusyClaude 동작)는 RED다.
  - 수락 뒤 기한: 새 mutant `AcceptedStillExpires`. Tab으로 수락된 B에 30초 기한이 재전송 요청을 내는 경우다.

## 코드 근거와 값 범위

| 모델 값·행동 | 기준 코드 좌표 | 의미 |
|---|---|---|
| `DriveCurrent`의 token 분기 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:249-296` | `has_active_turn`이면 `hand_busy`, 아니면 `hand_idle` |
| Idle 화면의 `Hold`(Current) | `owner_input.rs:407-460`, `…/intake_turn/owned_busy.rs:10,33-96` | `busy_source_unavailable`·`busy_turn_unobserved` |
| `Paste`의 token | `owner_input.rs:943` `start_idle`(토큰 없음), `owner_input.rs:463-471`(토큰 있음 → busy child) | 토큰은 장부만 고르고 보류를 만들지 않는다 |
| `AEnds`의 `TerminalOk = FALSE` | `tmux_watcher/turn_identity/soft_terminal_authority.rs:160-187`, `turn_finalizer/reconcile.rs:329-358` | #8432 |
| `Scenario = "external"` | `src/services/discord/inflight/model/turn_kinds.rs:30-34`(ExternalInput), #8328 본문 | AgentDesk가 시작하지 않은 턴. anchor도 토큰도 없음 |
| `TuiAbsorb` | 0단계 Claude S1(O)·S5D, `src/services/tui_o/writer/input_facts/native.rs:20-60` | 도구 경계에서 진행 중 턴에 흡수 |
| `PasteCodex`·`Reflect`·`KeyCodex` | i8328tab1 실측(idle Tab 6/6, 바쁨 Tab 15/15, 경계 8/8, 붙여넣은 직후 Tab 0/3), Codex CLI 0.172.0 바이너리 문구 "Paste detection", "Queue the draft while a task is running" | 항상 Tab 절차 |
| `aTo`·`bTo`, `AnswerBound` | 게시 포트 `src/services/tui_o/writer/mod.rs:115-145`, `src/services/discord/outbound/o_writer_io.rs:184-210`(reference 인자 없음), i8328tab1 Discord reference null 94/94 | 답의 reply anchor |
| `mark` | `src/services/discord/turn_view_reconciler/injected.rs:204-231`, `turn_presence/supervisor.rs:884` | 합류 ➕. 목표는 흡수 증거(`src/services/tui_o/writer/input_facts/native.rs:20-60`) 뒤에만 |
| `leases`, `Acquire`, `Overwrites` | `migrations/postgres/0126_execution_capacity.sql:2-8`(PK instance·provider·channel), `src/services/cluster/execution_capacity/store.rs:3-49`(일반 INSERT, nonce로 renew), `src/services/cluster/execution_capacity.rs:123-202`(renew 실패 → provider 취소), `owner_input.rs:1493-1515`(Idle 경로만 획득, busy는 "이미 잡은 턴에 씀") | 채널 실행 lease |
| `RequireTranscriptBeforePaste` | `src/services/claude_tui/busy_inject.rs:571-581`(transcript metadata를 못 읽으면 `TranscriptUnavailable`), `owner_input.rs:114-137,1275-1280`(V2 `pre_source`) | 새 세션에는 transcript가 없다 |
| `AcceptedStillExpires` | `src/services/turn_orchestrator/queued_head/v2/attempt.rs:107,161`(`confirmation_deadline_ms` 30초) | 수락 뒤 기한 |
| `blocker` | `queue_io/input_owner/ingress.rs:99-115,192-198`, `claude_tui/composer_lock.rs:116`, `codex_tui/busy_inject/screen.rs:75-99`, `claude_tui/input/preparation_hold.rs:112`, `owner_input.rs:1752` | 초안·모달(Tab 뜻 미확인 화면 포함)·세션 없음·첨부·composer 미준비·복구 전 |

변수:
- B의 상태: `a`, `screen`, `token`, `b`, `keys`, `aReplies`/`bReplies`, `wake`, `blocker`
- `refl`: Codex 입력창에 붙여넣은 frame이 보임
- `mark`: 합류 표시
- `merged`: B 몫이 A 답에 합쳐짐
- `aLost`: A 작업 유실
- `aTo`/`bTo`: 각 답이 reply로 건 anchor. `unset`은 아직 게시 전이다.
- `leases`: 이 채널의 실행 lease 행 수. AgentDesk가 시작한 A는 1, 외부 턴과 앞 턴 없음은 0에서 시작한다.

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoDuplicate` | INVARIANT | B의 키 효과(Claude 붙여넣기+Enter, Codex 붙여넣기)는 최대 1회 |
| `NoSilentLoss` | INVARIANT | B도 A의 작업도 알림 없이 사라지지 않는다 |
| `ReplyExactlyOnce` | INVARIANT | A·B 답은 각각 최대 1회. 답이 게시된 B의 몫은 정확히 하나(자기 답 XOR A 답에 합쳐짐) |
| `JoinedMarked` | INVARIANT | A 답에 합쳐진 Claude B에는 합류 표시 |
| `AnswerBound` | INVARIANT | A 답은 `ATarget`(A anchor가 있으면 A, 없으면 합류 B, 아니면 none)에, B의 자기 답은 B에 reply로 건다 |
| `KeyBeforeLeave` | INVARIANT | front는 키 효과로만 대기열을 떠난다 |
| `MarkOnlyJoined` | INVARIANT | 합류 표시는 합류했거나 A 답에 합쳐진 B에만 있다 |
| `OneLease` | INVARIANT | 채널의 실행 lease 행은 최대 하나 |
| `LeaseCovers` | INVARIANT | 실행 중인 AgentDesk 턴(A 또는 B)은 채널 lease를 가진다 |
| `IdleDelivers` | PROPERTY | 대기·보류 중 B + Idle 화면이면 B는 결국 주입된다 |
| `ReplyPosted` | PROPERTY | B의 몫은 결국 게시된다 |
| `LeaseReleased` | PROPERTY | 마지막 후속 턴이 끝나면 lease는 반납되고 다시 잡히지 않는다 |

공정성: 정상 행동에는 weak fairness를 준다. `TuiAbsorb`와 mutant 행동에는 공정성이 없다.

## 결과 (`statecharts_tlc.check_cfg`, 30/30 기대 집합 일치)

w2 → w3 전후표. 모든 cfg에 새 규칙 4개(`MarkOnlyJoined`·`OneLease`·`LeaseCovers`·`LeaseReleased`)가 더해졌다.

| cfg | 조건 | w2 결과 | w3 결과 |
|---|---|---|---|
| `Current8432` | 현재 경로, A terminal 미제출 | RED `IdleDelivers`, `ReplyPosted` | **RED** `IdleDelivers`, `ReplyPosted`, `MarkOnlyJoined`, `LeaseCovers` |
| `CurrentBusy` | 현재 경로, A 정상 종료 | GREEN | **RED** `MarkOnlyJoined`(바쁜 화면 붙여넣기에 바로 ➕), `LeaseCovers`(A가 끝나면 lease가 풀려 B 자기 턴이 lease 없이 돈다) |
| `Target8432` | Claude 목표, 남은 토큰 | GREEN | GREEN |
| `TargetBusy` | Claude 목표, 합류·A 뒤 자기 턴 | GREEN | GREEN(자기 턴 B에 ➕ 없음, lease는 B 끝까지 유지) |
| `TargetExternal` | Claude 목표, 외부 턴 A | GREEN | GREEN(B 키가 lease를 새로 잡음) |
| `TargetIdle` | Claude 목표, 새 세션(transcript 없음) | (없음) | GREEN |
| `TargetHoldCleared`·`TargetComposerNotReady` | 보류 뒤 해소 | GREEN | GREEN |
| `TargetDraftStuck` | 초안 영구, 알림만(U1) | RED `IdleDelivers`, `ReplyPosted`(수용) | RED `IdleDelivers`, `ReplyPosted`(수용) |
| `TargetDraftStuckCap` | 상한 보관 제출(대조군) | GREEN | GREEN |
| `CodexTargetBusy`·`CodexTarget8432`·`CodexTargetExternal`·`CodexComposerNotReady` | Codex 항상 Tab | GREEN | GREEN |
| `TargetMarkOnPaste` | mutant: 목표에서 붙여넣기 때 ➕ | (없음) | RED `MarkOnlyJoined` |
| `CodexAcquirePerTab` | mutant: Tab마다 capacity 획득 | (없음) | RED `OneLease` |
| `CodexNonceOverwrite` | mutant: 두 번째 획득이 nonce 덮어씀 → A renew 실패·취소 | (없음) | RED `NoSilentLoss` |
| `TargetRequireTranscript` | mutant: 붙여넣기 전 transcript 요구 | (없음) | RED `IdleDelivers`, `ReplyPosted` |
| `CodexAcceptedStillExpires` | mutant: Tab 수락 뒤에도 30초 기한이 재전송 요청 | (없음) | RED `NoDuplicate` |
| `TargetTokenAttribution` | mutant: 토큰 보유자에게 귀속 | RED `ReplyExactlyOnce`, `ReplyPosted` | 같음 |
| `TargetResendBeforeEcho` | mutant: echo 전 재전송 | RED `NoDuplicate` | 같음 |
| `TargetTuiDrop`·`CodexTuiDrop` | mutant: TUI가 입력을 버림 | RED `NoSilentLoss`, `ReplyPosted` | 같음 |
| `TargetJoinUnmarked` | mutant: 합류 표시 누락(흡수 때) | RED `JoinedMarked` | 같음 |
| `TargetJoinDoublePost` | mutant: 합류 몫 이중 게시 | RED `ReplyExactlyOnce` | 같음 |
| `TargetPostOrder`·`CodexPostOrder` | mutant: 게시 순서로 귀속 | RED `AnswerBound` | 같음 |
| `TargetNoReference` | mutant: reply reference 없이 게시 | RED `AnswerBound` | 같음 |
| `CodexAlwaysEnter` | mutant: 항상 Enter(바쁜 턴 steer) | RED `NoSilentLoss` | 같음 |
| `CodexNoReflectCheck` | mutant: 반영 확인 없이 Tab | RED `ReplyPosted` | 같음 |

합계: GREEN 11, expected RED 19. RED는 Current 2개, 수용한 초안 영구 1개, mutant 16개다.

차트:
- [현재 경로 #8432 반례](owner-input-busy-split.current-8432.chart.mmd)
- [목표 #8432](owner-input-busy-split.target-8432.chart.mmd)
- [목표 Claude 합류](owner-input-busy-split.target-busy-join.chart.mmd)
- [목표 Codex 항상 Tab](owner-input-busy-split.codex-target-busy.chart.mmd): 보류 조건 → 붙여넣기 → 반영 → Tab
- [목표 Codex 외부 턴](owner-input-busy-split.codex-target-external.chart.mmd): A 답은 anchor 없음, B 답은 B에 reply
- [게시 순서 귀속 반례](owner-input-busy-split.target-post-order.chart.mmd): 외부 턴 A가 끝날 때 A 답이 붙여넣어진 B에 걸린다
- [Codex 항상 Enter 반례](owner-input-busy-split.codex-always-enter.chart.mmd)
- [현재 경로 정상 종료 반례](owner-input-busy-split.current-busy.chart.mmd): 붙여넣기 때 ➕, A 종료 뒤 lease 없이 B 자기 턴
- [붙여넣기 때 ➕ 반례](owner-input-busy-split.target-mark-on-paste.chart.mmd)
- [Tab마다 lease 반례](owner-input-busy-split.codex-acquire-per-tab.chart.mmd): 실행 중 A의 lease 위에 두 번째 행
- [nonce 덮어쓰기 반례](owner-input-busy-split.codex-nonce-overwrite.chart.mmd): A renew 실패로 A 작업 유실

## 읽는 법과 한계

- `PostOrder`·`NoReference` RED는 귀속을 "가장 오래된 미답 입력"이나 "reference 없는 게시"에 맡기면 깨진다는 것을 보인다. 목표의 귀속 근거는 echo가 들어간 native 턴이다.
- `CodexNoReflectCheck` RED: 붙여넣기가 화면에 반영되기 전 Tab은 B를 초안에 남긴다. i8328tab1 busy1의 셋째 건과 같은 모양이다.
- `CodexAlwaysEnter` RED: 바쁜 턴의 Enter는 steer다. i8328tab1 완료 전 Enter는 0/4 분리였고 3/4에서 A 답이 유실됐다.
- `leases`는 한 채널만 본다. 다른 채널이 node slot 상한에 걸리는지는 조각 A5의 Rust 시험이 맡는다.
- `RequireTranscriptBeforePaste`는 host와 무관하게 하나로 모델링했다. Herdr와 tmux 각각의 RED는 조각 A3 시험이 맡는다.
- 입력 하나만 다루므로 두 입력이 한 턴에 합쳐지는 경우(실측 불일치 b·c)는 다루지 않는다. 설계 문서 §6.3의 "한 턴에 anchor 여럿" 규칙과 조각 A1의 두 nonce fixture, A5의 두 입력 fixture가 맡는다. 기계 시작, 스레드, 재시작, Cancel·stop은 설계 문서 §7-§9가 다룬다.

원시 로그: `/tmp/i8328d1/`(`direct-w3.out`, `w3b-*` 차트 출력). 저장소에는 넣지 않았다.
