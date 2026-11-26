# owner 입력 busy 분기: 현재 경로 대 바로 주입·합류·키 선택 (#8328 1단계 설계)

기준 코드는 `82702fe252`다. 사람 입력 B 하나와 앞 턴 A 하나를 다루는 독립 모델이다. 같은 모듈의 `Policy` 상수로 현재 경로(`Current`)와 목표 경로(`Target`)를 나누고, `Provider`로 Claude와 Codex를 나눈다. 모델 결과는 Rust 시험을 대신하지 않는다. 설계 문서는 [`docs/design/8328-owner-input-busy-inject.md`](../../design/8328-owner-input-busy-inject.md)다.

## 판 이력

- **설계 준비판**(`124a47a5e3`, cfg 10개): 전제 3은 "TUI 대기열이 입력마다 별도 턴을 지킨다"였다.
- **설계 w1판**(이 판, cfg 20개): 0단계 실측과 사용자 결정 busy-inject-step0-1126 a에 따라 전제 3을 바꿨다.
  - 새 전제는 "유실 0·중복 0, Claude가 진행 중 턴에 흡수하면 합친 답을 1회 게시"다.
  - Codex는 바쁨 판정을 보류 관문이 아니라 키 선택(바쁨 Tab, idle Enter, 불확실 `UncertainKey`)으로 모델링했다.
  - liveness `IdleDelivers`는 그대로 두었다.

## 코드 근거와 값 범위

| 모델 값·행동 | 기준 코드 좌표 | 의미 |
|---|---|---|
| `DriveCurrent`의 token 분기 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:249-296` | `has_active_turn`이 참이면 `hand_busy`, 거짓이면 `decision_drain::hand_idle` |
| Idle 화면의 `Hold`(Current) | `owner_input.rs:407-460`, `…/intake_turn/owned_busy.rs:10,33-96` | `busy_source_unavailable`·`busy_turn_unobserved` NotSent |
| `Retry` | `owner_input.rs:326-345`, `src/services/discord/queue_io/input_owner/held_watch.rs` | NotSent front가 남고 held_watch·retry가 다시 깨운다 |
| `Paste`의 token | `owner_input.rs:943` `start_idle`(토큰 없음 → 자기 턴), `owner_input.rs:463-471`(토큰 있음 → busy child) | 목표: 토큰을 읽은 결과는 장부만 고르고 보류를 만들지 않는다 |
| `AEnds`의 `TerminalOk = FALSE` | `tmux_watcher/turn_identity/soft_terminal_authority.rs:160-187`, `turn_finalizer/reconcile.rs:329-358` | #8432: 답은 게시됐지만 token이 남는다 |
| `TuiAbsorb` | 0단계 실측 Claude S1(O)·S5D(`queue-operation remove absorbed_mid_turn`, `attachment queued_command`); 분류 `src/services/tui_o/writer/input_facts/native.rs:20-60` | Claude가 다음 도구 경계에서 B를 진행 중 턴에 흡수 |
| 합류 귀속 | `src/services/claude_tui/hosting/warm_followup.rs:759-767`(`attempt_row`: 자기 user 행이면 relay, queued command면 진행 중 턴 owner가 게시) | 목표의 echo 귀속 규칙 |
| `mark` | `src/services/discord/turn_view_reconciler/injected.rs:204-231`(➕), `turn_presence/supervisor.rs:884`(✅) | 진행 중 턴에 합류 표시 |
| `KeyCodex` Enter on Busy → `aLost` | 0단계 실측 Codex c2·c3 S1D, S5D, S1LD(steer: A 작업·답 유실) | 바쁜 턴의 Enter 금지 |
| `KeyCodex` Tab on Busy | 0단계 실측 Codex S1T(1회), `src/services/codex_tui/busy_inject/screen.rs:22-27` QUEUE_HEADERS | A 뒤 자기 task |
| `TabAtIdle="submits"`, `UncertainKey="Tab"` | 맥 실측 레그 i8328tab1 `report-tab-1.md`(idle Tab 6/6 즉시 제출, 경계 Tab 8/8, 완료 전 Enter 별도 턴 0/4) | 채택 키 규칙 값 |
| `blocker` | `queue_io/input_owner/ingress.rs:99-115,192-198`, `claude_tui/composer_lock.rs:116`, `claude_tui/busy_inject.rs:88,482`, `claude_tui/input/preparation_hold.rs:112`, `owner_input.rs:1752` | 초안·모달·세션 없음·첨부 준비 전·composer 미준비·복구 kickoff 전(H10) |
| `CapStash` | `readiness-managed-stash.report.md`, #8430 본문 | U1이 채택하지 않은 대조군 |

변수:
- `a`(A의 수명), `screen`(호스트 화면), `token`(mailbox active turn 보유자), `b`(B의 단계)
- `keys`(B의 키 효과 수), `aReplies`/`bReplies`(답 게시 수), `wake`(owner drive 깨움), `blocker`(보류 조건)
- `seen`(Codex 키 선택 때 읽은 화면), `unk`(Unknown 읽기를 이미 썼는지), `mark`(합류 표시)
- `merged`(B 몫이 A 답에 합쳐 게시됨), `aLost`(A 작업 유실)

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoDuplicate` | INVARIANT | B의 키 효과는 최대 1회 |
| `NoSilentLoss` | INVARIANT | B도, A의 작업도 알림 없이 사라지지 않는다 |
| `ReplyExactlyOnce` | INVARIANT | A·B 답은 각각 최대 1회다. 답이 게시된 B는 몫이 정확히 하나다(자기 답 XOR A 답에 합쳐짐) |
| `JoinedMarked` | INVARIANT | A 답에 합쳐진 Claude B에는 합류 표시가 있다 |
| `KeyBeforeLeave` | INVARIANT | front는 키 효과로만 대기열을 떠난다 |
| `IdleDelivers` | PROPERTY | 대기·보류 중인 B가 있고 화면이 Idle이면 B는 결국 주입된다 |
| `ReplyPosted` | PROPERTY | B의 몫(자기 답 또는 합쳐진 몫)은 결국 게시된다 |

공정성: 정상 행동에는 모두 weak fairness를 준다. `TuiAbsorb`(흡수할지 A 뒤에 돌릴지의 선택)와 mutant 행동(`ResendBeforeEcho`, `TuiDrop`)에는 공정성이 없다.

## 결과 (`statecharts_tlc.check_cfg`, 20/20 기대 집합 일치)

| cfg | 조건 | 결과 |
|---|---|---|
| `Current8432` | 현재 경로, A terminal 미제출 | **RED** `IdleDelivers`, `ReplyPosted` |
| `CurrentBusy` | 현재 경로, A 정상 종료 | GREEN |
| `Target8432` | 목표(Claude), 같은 조건 | GREEN |
| `TargetBusy` | 목표(Claude), 합류 경로와 A 뒤 자기 턴 경로 | GREEN |
| `TargetHoldCleared` | 모달 보류 뒤 해소 | GREEN |
| `TargetComposerNotReady` | composer 미준비 보류 뒤 해소 | GREEN |
| `TargetDraftStuck` | 초안이 끝내 남음, 알림만(U1) | RED `IdleDelivers`, `ReplyPosted` (수용) |
| `TargetDraftStuckCap` | 같은 조건 + Idle 상한 보관 제출(대조군) | GREEN |
| `TargetTokenAttribution` | mutant: 답을 token 보유자에게 귀속 | RED `ReplyExactlyOnce`, `ReplyPosted` |
| `TargetResendBeforeEcho` | mutant: echo 전 재전송 | RED `NoDuplicate` |
| `TargetTuiDrop` | mutant: TUI가 바쁜 중 받은 입력을 버림 | RED `NoSilentLoss`, `ReplyPosted` |
| `TargetJoinUnmarked` | mutant: 합류 표시 누락 | RED `JoinedMarked` |
| `TargetJoinDoublePost` | mutant: 합류한 B 몫을 따로 한 번 더 게시 | RED `ReplyExactlyOnce` |
| `CodexTargetBusy` | 목표(Codex) 채택 규칙: 바쁨 Tab·idle Enter·불확실 Tab, Unknown 읽기 1회 허용 | GREEN |
| `CodexTarget8432` | 목표(Codex) 채택 규칙, #8432 조건 | GREEN |
| `CodexUncertainHold` | 대안: 불확실할 때 보류 뒤 재관측 | GREEN |
| `CodexAlwaysEnter` | mutant: 바쁨 판정 없이 항상 Enter | RED `NoSilentLoss` |
| `CodexTabIdleStuck` | 가설: idle 화면 Tab이 제출되지 않고 걸림(실측으로 기각) | RED `ReplyPosted` (대조군) |
| `CodexUncertainEnter` | 불확실할 때 Enter | RED `NoSilentLoss` |
| `CodexTuiDrop` | mutant: Codex가 Tab 대기 입력을 버림 | RED `NoSilentLoss`, `ReplyPosted` |

차트:
- [현재 경로 #8432 반례](owner-input-busy-split.current-8432.chart.mmd): 남은 토큰 + Idle 화면에서 `hand_busy` 보류를 영원히 반복한다.
- [목표 #8432](owner-input-busy-split.target-8432.chart.mmd): 보류 조건이 없으므로 바로 주입하고, B의 echo로 B 몫의 답을 1회 게시한다.
- [목표 Claude 합류](owner-input-busy-split.target-busy-join.chart.mmd): 진행 중 턴에 주입된 B가 흡수되면 A 답에 합쳐 1회 게시되고 합류 표시가 붙는다. 흡수되지 않으면 A 뒤 자기 턴으로 답한다.
- [목표 Codex](owner-input-busy-split.codex-target-busy.chart.mmd): 보류 조건 → 화면 읽기 → 바쁨 Tab, idle Enter, Unknown Tab(읽기 뒤 A가 끝나도 idle Tab이 제출됨).
- [Codex 항상 Enter 반례](owner-input-busy-split.codex-always-enter.chart.mmd): 바쁜 턴의 Enter가 steer로 A 작업을 잃는다.

## 읽는 법과 한계

- `TokenAttribution`·`JoinDoublePost` RED는 목표가 "귀속은 echo 위치로, 합류한 몫은 진행 중 턴 답에 한 번만"을 전제로만 GREEN임을 보인다.
- `ResendBeforeEcho` RED는 TUI 대기열에 든 입력의 echo가 A가 끝난 뒤에야 나타날 수 있으므로 "echo가 아직 없다"를 재전송 근거로 쓰면 중복이 생긴다는 뜻이다.
- `CodexTabIdleStuck`과 `CodexTargetBusy`는 같은 규칙(불확실 Tab)이 실측값 `TabAtIdle`에 따라 갈린다는 것을 보인다. i8328tab1이 idle Tab 6/6 제출을 확인했으므로 채택 규칙은 GREEN 쪽이다. composer나 앞 입력 수락이 불확실할 때의 보류(`CodexUncertainHold`)도 GREEN이다.
- 모델은 입력 하나만 다룬다. 여러 건을 곧바로 붙여넣고 Tab을 누르면 TUI가 두 건을 합치는 현상(i8328tab1 busy1)은 설계 문서 §2.3의 키 전·후 확인 규칙과 S3 시험이 맡는다.
- 입력 하나, 앞 턴 하나다. 여러 입력의 순서, 기계 시작(START_WAITS), 스레드, 재시작·부팅, Cancel·stop은 설계 문서 §7-§9의 전수표가 다룬다. A의 남은 token을 지우는 일(#8432)은 이 모델에서 일어나지 않는다.

원시 로그: `/tmp/i8328d1/`(이 레그의 `direct.out`, `out2-*` 차트 출력). 저장소에는 넣지 않았다.
