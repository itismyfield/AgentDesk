# HostedCleanup

TLC reached 27,514 states, more than the chart budget of 500, so the chart draws only the bug paths: 13 states and 15 arrows.

## LegacyCurrentMarkerSafe breaks
The shortest path has 5 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: 삭제 판정 완료
3. 다른 host marker가 있는 alias 추가: 삭제 판정 완료 · alias 추가
4. 조건부 재검증 및 삭제: transaction 조건부 적용 · alias 추가
5. transaction commit: 삭제됨 (bulk, 관측 absent, 현재 unknown)

## AllAliasesRechecked breaks
The shortest path has 5 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: 삭제 판정 완료
3. 다른 host marker가 있는 alias 추가: 삭제 판정 완료 · alias 추가
4. 조건부 재검증 및 삭제: transaction 조건부 적용 · alias 추가
5. transaction commit: 삭제됨 (bulk, 관측 absent, 현재 unknown)

## BulkLockWaitMarkerNotMissed breaks
The shortest path has 6 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: 다른 row lock 대기
3. lock 보유자가 marker 기록: 다른 row lock 대기
4. 먼저 잡은 row lock 해제: 삭제 판정 완료
5. 조건부 재검증 및 삭제: transaction 조건부 적용
6. transaction commit: 삭제됨 (bulk, 관측 absent, 현재 unknown)

## ExplicitActivePreserved breaks
The shortest path has 6 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: lock 종료 후 재관측
3. 새 turn 또는 재spawn: lock 종료 후 재관측 · row 변경
4. row와 marker 재관측: 삭제 판정 완료 · row 변경
5. 조건부 재검증 및 삭제: transaction 조건부 적용 · row 변경
6. transaction commit: 삭제됨 (explicit, 관측 absent, 현재 absent)

## Rules that hold
LegacyObservedMarkerSafe, KnownRetiredSkipsMarker, UnknownAndUnownedKept, ExplicitLockWaitMarkerNotMissed, ChangedRowRechecked, ExplicitAliasRechecked, BulkDisconnectedRechecked, RollbackKeepsEarlier, NoDeadlock, EligibleEventuallyDeleted
