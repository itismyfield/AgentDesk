# HerdrRetiredRow

TLC reached 723 states, more than the chart budget of 500, so the chart draws only the bug paths: 16 states and 15 arrows.

## NoticeOnce breaks
The shortest path has 6 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
1. terminate·row end 퇴역: terminate settle 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
2. terminate settle_captured: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
3. dcserver 재시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight output_path 행(이전 세대, 부팅 복구 전)
4. 부팅 복구 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight output_path 행(이전 세대)
5. row end pass 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 1회
6. row end pass 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence busy · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회

## ExeclessRowResolves breaks
TLC's counterexample is in tlc-ExeclessRowResolves.out. It has 11 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
1. terminate·row end 퇴역: terminate settle 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
2. terminate settle_captured: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
3. 600초 정체 → unknown: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · inflight output_path 행(현 세대)
4. dcserver 재시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · 재시작 뒤 · inflight output_path 행(이전 세대, 부팅 복구 전)
5. 300초 경과: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대, 부팅 복구 전)
6. 부팅 복구 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대)
7. owner 입력 도착: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · owner 입력 잠듦
8. row end pass 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 1회 · owner 입력 잠듦
9. row end pass 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦
10. 새 실행 설치: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦
11. stale sweep idle: 취소·row end 끝 · 새 실행 Pending · 행 idle(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦
The run stops in "취소·row end 끝 · 새 실행 Pending · 행 idle(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦" forever.

## HeldInputRuns breaks
TLC's counterexample is in tlc-HeldInputRuns.out. It has 11 steps:
0. 턴 진행 · 실행 Bound · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
1. terminate·row end 퇴역: terminate settle 전 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
2. terminate settle_captured: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence busy · inflight output_path 행(현 세대)
3. 600초 정체 → unknown: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 살아 있음 · presence unknown · inflight output_path 행(현 세대)
4. dcserver 재시작: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · 재시작 뒤 · inflight output_path 행(이전 세대, 부팅 복구 전)
5. 300초 경과: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대, 부팅 복구 전)
6. 부팅 복구 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대)
7. owner 입력 도착: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · owner 입력 잠듦
8. row end pass 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 1회 · owner 입력 잠듦
9. row end pass 1회: 취소·row end 끝 · 실행 Retired · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦
10. 새 실행 설치: 취소·row end 끝 · 새 실행 Pending · 행 turn_active(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦
11. stale sweep idle: 취소·row end 끝 · 새 실행 Pending · 행 idle(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦
The run stops in "취소·row end 끝 · 새 실행 Pending · 행 idle(퇴역 턴) · reader 없음 · presence unknown · heartbeat 오래됨 · 재시작 뒤 · inflight output_path 행(이전 세대) · 공지 2회 · owner 입력 잠듦" forever.

## Rules that hold
RowGoneWithTerminal
