# HerdrRetiredRow

TLC reached 49 states and 140 steps. The chart draws 49 states and 118 arrows.
The chart does not draw 22 steps that stay in one chart state.

## LostReaderNeverWrites breaks
The shortest path has 2 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. ToolUse: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실 · 상실 뒤 씀

## NewTurnRowNotOverwritten breaks
The shortest path has 4 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 nonce 턴 시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
4. ToolUse: 취소·row end 끝 · 실행 Retired · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음

## RowSettles breaks
TLC's counterexample is in tlc-RowSettles.out. It has 9 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: force postlude 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · reader 행 상실
2. force postlude disconnect: 취소·row end 끝 · 실행 Retired · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
3. 새 실행 Pending 설치: 취소·row end 끝 · 새 실행 Pending · 행 disconnected(퇴역 턴) · reader 살아 있음 · presence busy · 한때 끝남 · reader 행 상실
4. 새 nonce 턴 시작: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence busy · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
5. 새 턴 IDLE: 취소·row end 끝 · 새 실행 Pending · 행 idle(새 턴) · reader 살아 있음 · presence idle · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
6. 300초 경과: 취소·row end 끝 · 새 실행 Pending · 행 idle(새 턴) · reader 살아 있음 · presence idle · heartbeat 오래됨 · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실
7. ToolUse: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · heartbeat 오래됨 · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음
8. stale sweep idle: 취소·row end 끝 · 새 실행 Pending · 행 idle(새 턴) · reader 살아 있음 · presence idle · heartbeat 오래됨 · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음
9. ToolUse: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · heartbeat 오래됨 · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음
The run loops through "취소·row end 끝 · 새 실행 Pending · 행 turn_active(새 턴) · reader 살아 있음 · presence idle · heartbeat 오래됨 · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음" and "취소·row end 끝 · 새 실행 Pending · 행 idle(새 턴) · reader 살아 있음 · presence idle · heartbeat 오래됨 · 한때 끝남 · 새 턴 시작 뒤 · reader 행 상실 · 상실 뒤 씀 · 새 턴 행 덮음" forever.

## Rules that hold
NoRevival, EndedTurnNotSelectable, NewTurnNotRefused, PresenceLeavesBusy
