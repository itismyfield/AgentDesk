# HerdrRetiredRow

TLC reached 246 states and 635 steps. The chart draws 105 states and 321 arrows.
The chart does not draw 120 steps that stay in one chart state.

## RowGoneWithTerminal breaks
The shortest path has 1 step:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight 경로 없는 행(현 세대)
1. stale-age 삭제: 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight 나이로 삭제

## Rules that hold
NoticeOnce, ExeclessRowResolves
