# HerdrRetiredRow

TLC reached 136 states and 290 steps. The chart draws 88 states and 176 arrows.
The chart does not draw 56 steps that stay in one chart state.

## ExeclessRowResolves breaks
TLC's counterexample is in tlc-ExeclessRowResolves.out. It has 7 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
1. terminate·row end 퇴역: terminate settle 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
2. 600초 정체 → unknown: terminate settle 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · inflight output_path 행(현 세대)
3. terminate settle_captured: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · inflight output_path 행(현 세대)
4. dcserver 재시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · 재시작 뒤 · inflight output_path 행(이전 세대, 부팅 복구 전)
5. 부팅 복구 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · 재시작 뒤 · inflight output_path 행(이전 세대)
6. 300초 경과: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대)
7. stale sweep idle: 취소·row end 끝 · 실행 Retired · 행 idle(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 한때 끝남 · 재시작 뒤 · inflight output_path 행(이전 세대)
The run stops in "취소·row end 끝 · 실행 Retired · 행 idle(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 한때 끝남 · 재시작 뒤 · inflight output_path 행(이전 세대)" forever.

## Rules that hold
RowGoneWithTerminal, NoticeOnce
