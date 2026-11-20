# HerdrRetiredRow

TLC reached 97 states and 350 steps. The chart draws 97 states and 246 arrows.
The chart does not draw 95 steps that stay in one chart state.

## LostReaderNeverWrites breaks
The shortest path has 2 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. reader 30초 working: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 상실 뒤 씀

## NoRevival breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
4. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 상실 뒤 씀

## EndedTurnNotSelectable breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
4. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 상실 뒤 씀

## NewTurnRowNotOverwritten breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 nonce 턴 시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤
4. reader 30초 working: 취소·row end 끝 · 실행 Retired · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · 상실 뒤 씀 · 새 턴 행 덮음

## Rules that hold
NewTurnNotRefused, RowSettles, PresenceLeavesBusy
