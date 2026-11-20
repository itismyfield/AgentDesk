# Claude `/stop` Esc 전달 판정

코드 기준: `881cc5c5d5933cdffc7db87eeb35075d846064a9`(Before) + #8081 stop 수리(Fixed). 모델은 stop 한 번이 읽은 세 가지(턴 식별, transcript 꼬리, 화면)와 Esc 전달 여부만 다룬다. 소유·세대·gate·중복 claim 같은 다른 울타리는 [native stop](../input-single-path/native-stop.report.md)과 Herdr 모델이 맡고 여기서는 통과했다고 둔다.

## 변수와 코드 대응

| 모델 | 뜻 | 코드 |
|---|---|---|
| `tail` = `idle`/`user_submitted`/`streaming` | transcript 꼬리의 구조화 상태 | `tui_turn_state::runtime_binding_turn_state` (`src/services/tui_turn_state.rs:339`) |
| `identity` = `read`/`unread` | 최근 user 행을 꼬리 256 KiB에서 찾았는가 | `ClaudeStopTurnIdentity::capture` (`src/services/discord/turn_bridge/tmux_runtime/claude_stop_identity.rs:505`) |
| `screen` = `idle`/`spinner`/`footer` | 화면 증거. `footer`는 `esc to interrupt`가 보이는 준비 아님 상태 | `claude_tui_stop_pane_evidence`, `claude_stop_pane::interrupt_offered` |
| `Sends` | Esc 전달 판정 | `classify_tui_interrupt_phase` (`claude_stop_delivery.rs:179`), Herdr `deliver` (`codex_stop_delivery.rs:720`) |

`ToolReturns`는 Code Mode의 `REPL`처럼 도구가 바로 반환하고 실제 작업은 계속 도는 경우다. 꼬리가 다시 user 행(`tool_result`)이 된다. `Outgrow`는 턴이 꼬리 읽기 범위보다 길어져 user 행을 못 찾는 경우다. `FinishLate`는 종료 기록이 화면 갱신보다 먼저 보이는 순간이다.

## 규칙

1. `ActiveScreenStops`: 식별을 읽었고 꼬리가 열린 턴(`user_submitted`·`streaming`)이며 화면이 active면 Esc를 보낸다.
2. `NoEscapeOnIdleScreen`: 화면이 유휴면 꼬리가 무엇이든 Esc를 보내지 않는다(빈 입력창·제출 직후 스피너 전 포함).
3. `UnreadFooterStops`: 식별을 못 읽어도 화면에 `esc to interrupt`가 보이면 보낸다.
4. `UnreadNeedsFooter`: 식별을 못 읽은 턴은 스피너만으로는 보내지 않는다.
5. `NoEscapeAfterEnd`: 꼬리가 종료를 보이면 화면에 스피너가 남아 있어도 보내지 않는다.

규칙 1은 꼬리가 종료(`idle`)인 경우를 뺀다. 종료 기록이 있는데 화면만 active인 순간은 `FinishLate`이거나 이 턴이 아닌 작업(compact 등)이어서 규칙 5가 우선한다.

## 실제 TLC 결과

명령: `python3 scripts/ci/statecharts_tlc.py --root <모델 사본> --java /opt/homebrew/opt/openjdk/bin/java --jar ~/.local/share/tla2tools/tla2tools-1.7.4.jar` (6 cfg, 100 상태).

| cfg | 정책 | 결과 |
|---|---|---|
| ClaudeStopBefore | 식별 읽음 + 꼬리 `streaming` + 화면 active일 때만 전송 | RED: ActiveScreenStops, UnreadFooterStops |
| ClaudeStopFixed | 수리 뒤 | GREEN: 다섯 규칙 |
| ClaudeStopNoScreenFirst | 수리에서 `user_submitted` + 화면 active 분기 제거 | RED: ActiveScreenStops |
| ClaudeStopNoUnreadFooter | 수리에서 미판독 + footer 분기 제거 | RED: UnreadFooterStops |
| ClaudeStopBlind | `user_submitted`면 화면과 무관하게 전송 | RED: NoEscapeOnIdleScreen |
| ClaudeStopUnreadSpinner | 미판독 턴을 스피너만으로 전송 | RED: UnreadNeedsFooter |

Before 최단 반례 A: 프롬프트 제출 → 작업 시작(꼬리 user 행, 스피너) → `!stop` → Esc 안 보냄. 11-20 실기 A(`structured=Some(UserSubmitted) active=true`, `reason="idle"`)와 같은 경로다. 반례 B: 제출 → 작업 시작(`esc to interrupt`) → 꼬리 읽기보다 길어짐 → `!stop` → Esc 안 보냄(`reason="unobserved"`).

코드 재현: A는 `herdr_claude_stop_follows_a_live_screen_past_a_lagging_or_unread_transcript`의 `returned tool, live` 행과 `claude_tui_stop_escapes_only_a_pane_showing_live_turn_chrome`의 `pre-generation` 행, B는 같은 Herdr 시험의 `unread, footer` 행이다. 실행 결과는 구현 레인 보고서에 있다.

## 모델 밖

- 식별 읽기가 실패하는 원인. 실측한 하나는 꼬리 읽기 시작점이 다바이트 문자 중간에 떨어지는 경우이고 코드에서 따로 고쳤다. 모델은 실패 여부만 본다.
- 화면 판독 오인(본문이 chrome 모양을 흉내 내는 경우), draft·modal 거절, 화면 읽기 자체의 실패(그대로 unobserved).
- 판정과 키 전송 사이의 시간. 그 사이 턴이 끝나면 Esc 하나가 유휴 입력창에 닿을 수 있고 수리 전에도 같았다.

[Before 차트](claude-stop-before.chart.mmd), [Fixed 차트](claude-stop-fixed.chart.mmd).
