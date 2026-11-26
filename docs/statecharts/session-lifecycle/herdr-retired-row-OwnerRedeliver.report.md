# HerdrRetiredRow

TLC reached 154 states and 341 steps. The chart draws 94 states and 213 arrows.
The chart does not draw 56 steps that stay in one chart state.

## NoticeOnce breaks
The shortest path has 5 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight terminal kind 행(현 세대)
1. terminate·row end 퇴역: terminate settle 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight terminal kind 행(현 세대)
2. terminate settle_captured: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight terminal kind 행(현 세대)
3. dcserver 재시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight terminal kind 행(이전 세대, 부팅 복구 전)
4. 부팅 복구 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight terminal kind 행(이전 세대, 공지 ack) · 공지 1회
5. row end pass 정리: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight 정리됨 · 공지 2회

## Rules that hold
RowGoneWithTerminal, ExeclessRowResolves
