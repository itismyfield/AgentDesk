# TUI Thinking Relay 정책

> 적용 범위: AgentDesk가 Claude TUI / Codex TUI에서 받는 `Thinking` 엔벨로프(모델 내부 reasoning) 처리.
> 정책 책임자: `src/services/discord/turn_bridge/mod.rs`의 turn-bridge 컴포넌트.
> 최종 갱신: 2026-05-19.

## 배경

모델의 thinking payload는 raw reasoning(prompt 우회 가능, 사용자에게 보여서는 안 되는 내부 추론)을 포함할 수 있다. TUI relay는 thinking을 사용자에게 노출하지 않으면서 진행 상태만 알리고, 턴 종료 판정에도 영향을 주지 않아야 한다.

## 정책 (4 조항)

### 조항 1. Raw thinking은 Discord에 절대 노출 금지
- 사용자가 보는 Discord 메시지(본문, status panel, 임시 placeholder, 옵션, embed의 어느 필드에도) thinking payload의 원문/요약/일부를 **0% 포함**한다.
- summary 필드가 모델로부터 와도 그대로 전달하지 않는다(현재 `redacted_thinking_transcript_event`의 `_summary` 인자가 사용되지 않는 이유).

### 조항 2. 사용자 가시 상태는 **중립 marker 1개**
- thinking 진행 중 사용자에게 보이는 단서는 단일 status line `"💭 Thinking..."` (현재 `thinking_status_line()` 반환값) 하나뿐이다.
- 다국어 변형 금지, 카운트다운/타이머/토큰 수 노출 금지. Discord 메시지 내에 thinking 표식은 최대 1개.
- `StreamMessage::Thinking` 수신 시 `current_tool_line`을 이 marker로 교체하고, 직전 tool 상태는 `prev_tool_status`에 `finalize_in_progress_tool_status`로 정리(현재 `turn_bridge/stream_loop/content_arms.rs`의 `StreamMessage::Thinking` arm).

### 조항 3. Thinking은 turn-completion 산정 제외
- thinking 엔벨로프는 "응답 진행 중" 신호이지 "응답 완료" 신호가 아니다.
- `tui_turn_state.rs`의 envelope classifier가 thinking을 `Idle`로 분류하지 않는다(현재 `claude_envelope_turn_state`는 thinking을 별도 분류하지 않고 fallthrough → `assistant` 또는 turn 진행 상태로 처리됨).
- 정량 척도(turn duration, token-count, latency SLO)에 thinking 시간은 포함되나 "이 턴은 끝났다"의 시그널 자체로는 사용 금지.

### 조항 4. Transcript 이벤트 content는 비움
- 내부 session transcript(`SessionTranscriptEvent`)에 thinking 이벤트를 기록하되, `content`는 **빈 문자열**, `summary`는 **None**, `status`는 `"info"` (현재 `redacted_thinking_transcript_event`의 반환값과 일치).
- transcript 조회/감사/디버그 도구 어디서도 thinking 원문을 보존하지 않는다.
- 외부로 export되는 transcript(예: `/api/sessions/.../events`)에서도 동일 redaction 적용.

## 근거 코드

| 조항 | 파일:라인 | 함수/항목 |
|---|---|---|
| 1, 2 | `src/services/discord/turn_bridge/thinking.rs` | `thinking_status_line()` — 유일한 중립 marker |
| 1, 4 | `src/services/discord/turn_bridge/thinking.rs` | `redacted_thinking_transcript_event()` — summary 무시, content "" |
| 1, 2 | `src/services/discord/turn_bridge/stream_loop/content_arms.rs` 의 `StreamMessage::Thinking` 분기 (검색: `redacted_thinking_transcript_event` call site) | marker 교체, transcript push |
| 3 | `src/services/tui_turn_state.rs` | `claude_envelope_turn_state` — thinking이 별도 Idle 분류로 매핑되지 않음 |

## 위반 사례 정의 (트랙 A U-6 assertion 근거)

다음 중 하나라도 발생하면 정책 위반:
- Discord 메시지(본문 또는 status panel) 텍스트에 thinking payload의 substring 등장
- 한 인플라이트 응답에 `💭 Thinking...` 외 thinking 관련 marker가 2개 이상
- `tui_turn_state.rs`가 thinking 엔벨로프만으로 `Idle` 반환
- `SessionTranscriptEvent` 출력에서 thinking 이벤트의 `content` 또는 `summary` 비어있지 않음

## 변경 절차

이 정책을 수정해야 할 사유가 생기면:
1. 본 문서에 변경안과 사유를 먼저 PR.
2. 관련 코드(`turn_bridge::thinking_status_line`, `redacted_thinking_transcript_event`, 그리고 `StreamMessage::Thinking` 처리 분기)와 트랙 A U-6 테스트를 정책 본문에 정합하게 동기 수정.
3. 위반 사례 정의도 함께 갱신.

정책 본문이 코드보다 우선이며, 코드가 정책에 미달하면 코드 측 결함으로 본다.
