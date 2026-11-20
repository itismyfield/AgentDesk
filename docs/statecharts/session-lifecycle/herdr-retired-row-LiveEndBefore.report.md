# HerdrRetiredRow

TLC reached 31 states and 69 steps. The chart draws 31 states and 66 arrows.
The chart does not draw 3 steps that stay in one chart state.

## EndedTurnNotSelectable breaks
The shortest path has 2 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: row end 정리 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. live row end 정리: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 종료 중 · presence busy

## NoRevival breaks
The shortest path has 5 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
1. terminate·row end 퇴역: row end 정리 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy
2. live row end 정리: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 종료 중 · presence busy
3. 300초 경과: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 종료 중 · presence busy · heartbeat 오래됨
4. stale sweep idle: 취소·row end 끝 · 실행 Retired · 행 idle(퇴역 턴) · reader 종료 중 · presence busy · heartbeat 오래됨 · 한때 끝남
5. 루프 끝 TURN_ACTIVE: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 마무리 · presence busy · 한때 끝남

## Rules that hold
NewTurnNotRefused, RowSettles, PresenceLeavesBusy
