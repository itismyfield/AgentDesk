# HerdrRetiredRow

TLC reached 7 states and 13 steps. The chart draws 7 states and 8 arrows.
The chart does not draw 5 steps that stay in one chart state.

## NoRevival breaks
The shortest path has 3 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남

## EndedTurnNotSelectable breaks
The shortest path has 3 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남

## RowSettles breaks
TLC's counterexample is in tlc-RowSettles.out. It has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. 600초 정체 → unknown: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown
3. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남
4. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남
The run stops in "취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남" forever.

## Rules that hold
NewTurnNotRefused, PresenceLeavesBusy
