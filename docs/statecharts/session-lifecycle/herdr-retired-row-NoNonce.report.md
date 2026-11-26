# HerdrRetiredRow

TLC reached 48 states and 166 steps. The chart draws 48 states and 93 arrows.
The chart does not draw 73 steps that stay in one chart state.

## LostReaderNeverWrites breaks
The shortest path has 2 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. reader 30초 working: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실 · 상실 뒤 씀

## NewTurnNotRefused breaks
The shortest path has 3 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 nonce 턴 시작: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 거절됨 · 새 턴 시작 뒤 · reader 행 상실

## NoRevival breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
4. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실 · 상실 뒤 씀

## EndedTurnNotSelectable breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
4. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실 · 상실 뒤 씀

## NewTurnRowNotOverwritten breaks
The shortest path has 5 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
4. 새 nonce 턴 시작: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
5. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음

## RowSettles breaks
TLC's counterexample is in tlc-RowSettles.out. It has 6 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
4. 새 nonce 턴 시작: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
5. 새 턴 IDLE: 취소·row end 끝 · 새 실행 Pending · 행 idle(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
6. reader 30초 working: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음
The run stops in "취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음" forever.

## Rules that hold
PresenceLeavesBusy
