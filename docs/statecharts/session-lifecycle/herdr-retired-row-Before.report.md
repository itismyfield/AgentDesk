# HerdrRetiredRow

TLC reached 11 states and 32 steps. The chart draws 11 states and 16 arrows.
The chart does not draw 16 steps that stay in one chart state.

## LostReaderNeverWrites breaks
The shortest path has 2 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. reader 30초 working: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실 · 상실 뒤 씀

## NoRevival breaks
The shortest path has 3 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실 · 상실 뒤 씀

## EndedTurnNotSelectable breaks
The shortest path has 3 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실 · 상실 뒤 씀

## RowSettles breaks
TLC's counterexample is in tlc-RowSettles.out. It has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실 · 상실 뒤 씀
4. 600초 정체 → unknown: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남 · reader 행 상실 · 상실 뒤 씀
The run stops in "취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남 · reader 행 상실 · 상실 뒤 씀" forever.

## Rules that hold
NewTurnNotRefused, NewTurnRowNotOverwritten, PresenceLeavesBusy
