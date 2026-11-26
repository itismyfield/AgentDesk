# owner 입력 busy 분기: 현재 경로 대 바로 주입 (#8328 1단계 설계 준비)

기준 코드 `82702fe252`. 사람 입력 B 하나와 앞 턴 A 하나를 다루는 독립 모델이다. 같은 모듈의 `Policy` 상수로 현재 경로(`Current`)와 목표 경로(`Target`)를 나눈다. 모델 결과는 Rust 시험을 대신하지 않는다. TUI가 바쁜 중 받은 입력을 자기 대기열에 안전하게 쌓는다는 가정은 0단계 실측이 확인할 몫이며, 그 가정이 깨진 경우는 `TuiDrop` mutant로 따로 둔다.

## 코드 근거와 값 범위

| 모델 값·행동 | 기준 코드 좌표 | 의미 |
|---|---|---|
| `DriveCurrent`의 token 분기 | `src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:249-296` | `has_active_turn`(사람 입력은 `external_turn_hold_for_start`까지)이 참이면 `hand_busy`, 거짓이면 `decision_drain::hand_idle`. |
| Idle 화면의 `Hold` | `owner_input.rs:407-460`, `src/services/discord/router/message_handler/intake_turn/owned_busy.rs:10,33-96` | `hand_busy`는 `owned_busy::running`·`Registration::observed`가 Busy를 못 보면 `busy_source_unavailable`·`busy_turn_unobserved` NotSent로 끝난다. |
| `Retry` | `owner_input.rs:326-345`(`busy_retry::note_unsent_front`), `src/services/discord/queue_io/input_owner/held_watch.rs` | NotSent front는 남고 held_watch·retry가 owner를 다시 깨운다. #8432 운영 로그의 약 2초 간격 재보류. |
| `hand_idle`의 token 획득 | `src/services/discord/queue_io/decision_drain.rs:212` → `owner_input.rs` `start_idle` | idle 시작은 자기 턴의 mailbox token을 잡는다. |
| `AEnds`의 `TerminalOk = FALSE` | `tmux_watcher/turn_identity/soft_terminal_authority.rs:160-187`, `turn_finalizer/reconcile.rs:329-358`(#8432 보고서) | 답은 게시됐지만 watcher가 soft terminal을 거부하고 당긴 deadline이 연장돼 token이 남는다. 이 경로의 수리는 #8432 레인 몫이다. |
| `TuiStart` echo | `src/services/claude_tui/host_input/busy/herdr.rs:303-333`(transcript turn-state 판독), `src/services/codex_tui/busy_inject.rs:288`(rollout `read_turn`) | transcript에 B의 user record가 나타나는 순간. 목표 경로의 답 귀속 근거. |
| `blocker` 값 | `src/services/discord/queue_io/input_owner/ingress.rs:99-115,148-186`, `src/services/claude_tui/composer_lock.rs:116`, `src/services/claude_tui/busy_inject.rs:88,482`, `owner_input.rs:1752` | 초안(PersonDraft)·모달/승인창(Modal, Dialog)·세션 없음(Phase::Absent → Cold)·첨부 준비 전(Uploads). 목표 경로에 남는 보류 조건 전부. |
| `CapStash` | `docs/statecharts/input-single-path/readiness-managed-stash.report.md`, #8430 본문 | 초안이 있는 Idle 화면에서 C-s 왕복으로 초안을 보존하고 B를 제출하는 기존 수단을 상한 안전망으로 쓴 경우. |

변수: `a`(A의 수명), `screen`(호스트 화면), `token`(mailbox active turn 보유자), `b`(B의 단계), `keys`(B의 키 효과 수), `aReplies`/`bReplies`(답 게시 수), `wake`(owner drive 깨움), `blocker`(보류 조건).

## 규칙

| 규칙 | 종류 | 뜻 |
|---|---|---|
| `NoDuplicate` | INVARIANT | B의 키 효과는 최대 1회. |
| `NoSilentLoss` | INVARIANT | B가 키 효과 뒤 아무 알림 없이 사라지지 않는다. |
| `ReplyExactlyOnce` | INVARIANT | A·B 답은 각각 최대 1회이고, B가 답 게시 상태면 B 몫의 답이 정확히 1회다(다른 입력으로 잘못 귀속하면 깨진다). |
| `KeyBeforeLeave` | INVARIANT | front는 키 효과로만 대기열을 떠난다. |
| `IdleDelivers` | PROPERTY | 대기·보류 중인 B가 있고 호스트 화면이 Idle이면 B는 결국 주입된다. |
| `ReplyPosted` | PROPERTY | B의 답은 결국 1회 게시된다. |

공정성: 모든 정상 행동에 weak fairness. mutant 행동(`ResendBeforeEcho`, `TuiDrop`)에는 공정성이 없다.

## 결과 (`scripts/ci/statecharts_tlc.py --base origin/main`, 10/10 기대 집합 일치)

| cfg | 조건 | 결과 |
|---|---|---|
| `Current8432` | 현재 경로, A terminal 미제출 | **RED** `IdleDelivers`, `ReplyPosted` |
| `Target8432` | 목표 경로, 같은 조건 | GREEN |
| `CurrentBusy` | 현재 경로, A 정상 종료 | GREEN |
| `TargetBusy` | 목표 경로, A 정상 종료 | GREEN |
| `TargetHoldCleared` | 목표 경로, 모달 보류 뒤 해소 | GREEN |
| `TargetDraftStuck` | 목표 경로, 초안이 끝내 남고 상한 없음 | RED `IdleDelivers`, `ReplyPosted` |
| `TargetDraftStuckCap` | 같은 조건 + Idle 상한(초안 보관 뒤 제출) | GREEN |
| `TargetTokenAttribution` | mutant: 답을 token 보유자에게 귀속 | RED `ReplyExactlyOnce`, `ReplyPosted` |
| `TargetResendBeforeEcho` | mutant: echo 전 재전송 | RED `NoDuplicate` |
| `TargetTuiDrop` | mutant: TUI가 바쁜 중 받은 입력을 버림 | RED `NoSilentLoss`, `ReplyPosted` |

- [현재 경로 #8432 반례](owner-input-busy-split.current-8432.chart.mmd): A 실행 → B 도착 → A 종료·답 게시(token 남음, 화면 Idle) → owner drive가 active turn을 보고 `hand_busy` → Idle이라 보류 → retry → 같은 보류를 영원히 반복.
- [목표 경로](owner-input-busy-split.target-8432.chart.mmd): 같은 조건에서 보류 조건이 없으므로 바로 주입하고, B의 echo로 B 몫의 답을 1회 게시한다. A의 남은 token은 입력을 막지 않는다.

## 읽는 법과 한계

- `TokenAttribution` RED는 목표 경로가 "답 귀속을 token에 두지 않는다"를 전제로만 GREEN임을 보인다. token이 A에 남은 채로 B의 답을 token으로 귀속하면 A 답이 두 번 나간다.
- `ResendBeforeEcho` RED는 TUI 대기열에 들어간 입력의 echo가 A가 끝난 뒤에야 나타나므로, "echo가 아직 없다"를 재전송 근거로 쓰면 중복이 생긴다는 뜻이다.
- `TuiDrop` RED는 목표 경로가 0단계 실측(TUI 자체 대기열 안전성)에 기대고 있음을 보인다.
- `DraftStuck`과 `DraftStuckCap`은 사람 초안이 끝내 남는 경우에만 Idle 상한이 liveness에 필요함을 보인다. 모달·세션 없음·첨부 준비 전은 해소(`ClearBlocker`)가 사람 응답이나 기존 시작·업로드 경로에 달려 있고, 이 모델은 그 해소를 공정성 가정으로 둔다.
- 입력 하나, 앞 턴 하나다. 여러 입력의 순서, 시스템 주입(START_WAITS), 스레드, 재시작·부팅은 설계 준비 보고서의 전수표가 다룬다. A의 남은 token을 지우는 일(#8432)은 이 모델에서 일어나지 않는다. 목표 경로에서도 Cancel·stop·release 계약 때문에 그 수리는 따로 필요하다.

원시 로그: `/Users/itismyfield/.adk/lane-runs/i8328-prep/logs/`(`sc-check.out`, `sc-check.json`, `tlc/*.out`, `chart-*`).
