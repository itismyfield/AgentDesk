# HerdrRetiredRow

TLC reached 20 states and 51 steps. The chart draws 20 states and 37 arrows.
The chart does not draw 14 steps that stay in one chart state.

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
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남
4. 새 nonce 턴 시작: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤
5. 새 턴 IDLE: 취소·row end 끝 · 새 실행 Pending · 행 idle(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤
6. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤
The run stops in "취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤" forever.

## Rules that hold
NewTurnNotRefused, PresenceLeavesBusy
