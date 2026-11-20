# HerdrRetiredRow

TLC reached 26 states and 63 steps. The chart draws 26 states and 46 arrows.
The chart does not draw 17 steps that stay in one chart state.

## NewTurnNotRefused breaks
The shortest path has 3 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 nonce 턴 시작: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤

## NoRevival breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
4. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남

## EndedTurnNotSelectable breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
4. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남

## RowSettles breaks
TLC's counterexample is in tlc-RowSettles.out. It has 6 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
3. 새 nonce 턴 시작: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤
4. 600초 정체 → unknown: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤
5. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤
6. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤
The run stops in "취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤" forever.

## Rules that hold
PresenceLeavesBusy
