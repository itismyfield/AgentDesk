# Claude `/stop` Esc 전달 판정

코드 기준: `881cc5c5d5933cdffc7db87eeb35075d846064a9`(Before) + #8081 stop 수리. FirstRepair는 첫 수리(리뷰 ref `i8081-stop-1`), Fixed는 그 리뷰 뒤의 수리다. 모델은 stop 한 번이 읽은 세 가지(턴 식별, transcript 꼬리, 화면)와 Esc 전달 여부만 다룬다. 소유·세대·gate·중복 claim 같은 다른 울타리는 [native stop](../input-single-path/native-stop.report.md)과 Herdr 모델이 맡고 여기서는 통과했다고 둔다.

## 변수와 코드 대응

| 모델 | 뜻 | 코드 |
|---|---|---|
| `tail` = `idle`/`user_submitted`/`streaming`/`unknown` | transcript 꼬리의 구조화 상태. `unknown`은 세션 메타데이터가 꼬리에 붙은 경우 | `tui_turn_state::runtime_binding_turn_state` (`src/services/tui_turn_state.rs:339`), `claude_meta::past_marker_envelope` |
| `identity` = `read`/`unread` | 최근 user 행을 꼬리 256 KiB에서 찾았는가 | `ClaudeStopTurnIdentity::capture` (`src/services/discord/turn_bridge/tmux_runtime/claude_stop_identity.rs:146`) |
| `screen` = `idle`/`words`/`spinner`/`footer` | 화면 증거. `words`는 실행 표시는 없고 프롬프트나 응답 본문에 chrome 낱말(`current work`, `esc to interrupt`)만 있는 유휴 화면 | `claude_stop_pane::chrome_rows`, `legacy_evidence`, `interrupt_offered` |
| `Live`/`Offered` | stop 전용 판독: chrome 행의 스피너 프레임·interrupt 표시만 본다 | `claude_stop_pane.rs:61`, `:79`, `:90` |
| `WideLive`/`WideOffered` | 수리 전 판독: 일반 busy 판독과 문자열 포함 검사라 `words`도 active로 읽는다 | `tmux_common::tmux_capture_indicates_claude_tui_actively_streaming` |
| `Sends` | Esc 전달 판정 | `classify_tui_interrupt_phase` (`claude_stop_delivery.rs:179`), Herdr `deliver` (`codex_stop_delivery.rs:719`, `:791`) |

`ToolReturns`는 Code Mode의 `REPL`처럼 도구가 바로 반환하고 실제 작업은 계속 도는 경우다. 꼬리가 다시 user 행(`tool_result`)이 된다. `Metadata`는 Claude가 턴 중이나 턴 뒤에 `permission-mode` 같은 세션 기록을 덧붙이는 경우다. `Outgrow`는 턴이 꼬리 읽기 범위보다 길어져 user 행을 못 찾는 경우다. `FinishLate`는 종료 기록이 화면 갱신보다 먼저 보이는 순간이다.

## 규칙

1. `ActiveScreenStops`: 식별을 읽었고 화면이 실행 중(스피너·`esc to interrupt`)이면 꼬리가 무엇이든 Esc를 보낸다.
2. `NoEscapeOnIdleScreen`: 화면에 실행 표시가 없으면 꼬리가 무엇이든, 화면 본문에 어떤 낱말이 있든 Esc를 보내지 않는다(빈 입력창·제출 직후 스피너 전 포함).
3. `UnreadFooterStops`: 식별을 못 읽어도 화면에 `esc to interrupt` 표시가 있으면 보낸다.
4. `UnreadNeedsFooter`: 식별을 못 읽은 턴은 스피너만으로는 보내지 않는다.

첫 수리에 있던 "꼬리가 종료면 화면이 active여도 보내지 않는다"는 규칙은 뺐다. 종료 기록과 실행 중 화면이 함께 보이면 화면이 정한다. `FinishLate` 순간에는 Esc 하나가 방금 끝난 턴의 입력창에 닿을 수 있고, 이는 판정과 키 전송 사이의 창과 같은 종류다.

## 실제 TLC 결과

명령: `python3 scripts/ci/statecharts_tlc.py --base origin/main` (10 cfg, 300 상태, `TLA2TOOLS` 1.7.4).

| cfg | 정책 | 결과 |
|---|---|---|
| ClaudeStopBefore | 식별 읽음 + 꼬리 `streaming` + 화면 active일 때만 전송 | RED: ActiveScreenStops, UnreadFooterStops |
| ClaudeStopFirstRepair | 첫 수리: 열린 꼬리(`user_submitted`·`streaming`)만 화면 우선, 넓은 판독 | RED: ActiveScreenStops, NoEscapeOnIdleScreen, UnreadFooterStops, UnreadNeedsFooter |
| ClaudeStopFixed | 수리 뒤 | GREEN: 네 규칙 |
| ClaudeStopNoScreenFirst | 수리에서 `user_submitted` + 화면 실행 중 분기 제거 | RED: ActiveScreenStops |
| ClaudeStopOpenTailOnly | 수리에서 `idle`·`unknown` 꼬리의 화면 우선 제거 | RED: ActiveScreenStops |
| ClaudeStopWideReader | 수리에서 chrome 행 판독을 넓은 판독으로 되돌림 | RED: NoEscapeOnIdleScreen, UnreadNeedsFooter |
| ClaudeStopNoUnreadFooter | 수리에서 미판독 + footer 분기 제거 | RED: UnreadFooterStops |
| ClaudeStopUnreadEnded | 미판독 분기가 종료된 꼬리를 다시 거절 | RED: UnreadFooterStops |
| ClaudeStopBlind | `user_submitted`면 화면과 무관하게 전송 | RED: NoEscapeOnIdleScreen |
| ClaudeStopUnreadSpinner | 미판독 턴을 스피너만으로 전송 | RED: UnreadNeedsFooter |

Before 최단 반례 A: 프롬프트 제출 → 작업 시작(꼬리 user 행, 스피너) → `!stop` → Esc 안 보냄. 11-20 실기 A(`structured=Some(UserSubmitted) active=true`, `reason="idle"`)와 같은 경로다. 반례 B: 제출 → 작업 시작(`esc to interrupt`) → 꼬리 읽기보다 길어짐 → `!stop` → Esc 안 보냄(`reason="unobserved"`).

FirstRepair 최단 반례(첫 수리 리뷰의 P1 두 건):

| 규칙 | 경로 | 코드 재현 행 |
|---|---|---|
| ActiveScreenStops | 제출 → 작업 시작(스피너) → 세션 메타데이터 기록(꼬리 `unknown`) → `!stop` → Esc 안 보냄 | `metadata after it, live`, `ended tail, live`, `finished tail` |
| NoEscapeOnIdleScreen | chrome 낱말이 든 프롬프트 제출(스피너 전) → `!stop` → Esc 보냄 | `asked for the words`, `quoted words` |
| UnreadFooterStops | 작업 시작(`esc to interrupt`) → 꼬리 읽기보다 길어짐 → 종료 기록이 화면보다 먼저 → `!stop` → Esc 안 보냄 | `unread, ended` |
| UnreadNeedsFooter | 미판독 턴 종료(응답에 chrome 낱말) → 세션 메타데이터 기록 → `!stop` → Esc 보냄 | `unread, quoted words` |

코드 재현 행은 `herdr_claude_stop_follows_a_live_screen_past_a_lagging_or_unread_transcript`와 `herdr_claude_stop_reads_a_tool_turn_past_trailing_session_metadata`(`finished tail`)에 있다. A는 같은 시험의 `returned tool, live` 행과 `claude_tui_stop_escapes_only_a_pane_showing_live_turn_chrome`의 `pre-generation` 행, B는 `unread, footer` 행이다. 실행 결과는 구현 레인 보고서에 있다.

## 모델 밖

- 식별 읽기가 실패하는 원인. 실측한 하나는 꼬리 읽기 시작점이 다바이트 문자 중간에 떨어지는 경우이고 코드에서 따로 고쳤다. 모델은 실패 여부만 본다.
- chrome 행 판독의 한계: 왼쪽 끝에서 시작하는 행과 입력창 아래 상태 행만 chrome으로 본다. 응답 본문이 스피너 프레임을 통째로 인용해도 들여쓰기된 행이라 세지 않는다(`quoted frame` 행). 실제 chrome이 들여쓰기돼 그려지는 화면은 모델에 없다.
- draft·modal 거절, 화면 읽기 자체의 실패(그대로 unobserved), tmux 세션 stop의 구조화 판독 없음(`None`)과 유실된 owned source 거절.
- 판정과 키 전송 사이의 시간. 그 사이 턴이 끝나면 Esc 하나가 유휴 입력창에 닿을 수 있고 수리 전에도 같았다.

차트의 `턴 종료 · 화면 갱신 전` 상태는 식별 읽음·미판독을 한 상태로 묶어 그린다. 그래서 그 상태에서 `!stop 판정` 화살표가 둘로 갈라질 수 있다(미판독 + 스피너만 거절).

[Before 차트](claude-stop-before.chart.mmd), [FirstRepair 차트](claude-stop-first-repair.chart.mmd), [Fixed 차트](claude-stop-fixed.chart.mmd).
