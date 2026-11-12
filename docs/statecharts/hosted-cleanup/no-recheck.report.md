# HostedCleanup

TLC reached 22,588 states, more than the chart budget of 500, so the chart draws only the bug paths: 10 states and 10 arrows.

## ChangedRowRechecked breaks
The shortest path has 5 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: 삭제 판정 완료
3. 새 turn 또는 재spawn: 삭제 판정 완료 · row 변경
4. 조건부 재검증 및 삭제: transaction 조건부 적용 · row 변경
5. transaction commit: 삭제됨 (bulk, 관측 absent, 현재 absent)

## BulkDisconnectedRechecked breaks
The shortest path has 5 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: 삭제 판정 완료
3. 새 turn 또는 재spawn: 삭제 판정 완료 · row 변경
4. 조건부 재검증 및 삭제: transaction 조건부 적용 · row 변경
5. transaction commit: 삭제됨 (bulk, 관측 absent, 현재 absent)

## ExplicitAliasRechecked breaks
The shortest path has 6 steps:
0. 다음 정리 대기
1. DB 후보 snapshot: DB 후보 읽음
2. host marker 관측: lock 종료 후 재관측
3. row와 marker 재관측: 삭제 판정 완료
4. 다른 host marker가 있는 alias 추가: 삭제 판정 완료 · alias 추가
5. 조건부 재검증 및 삭제: transaction 조건부 적용 · alias 추가
6. transaction commit: 삭제됨 (explicit, 관측 absent, 현재 unknown)

## Rules that hold
LegacyObservedMarkerSafe, KnownRetiredSkipsMarker, UnknownAndUnownedKept, ExplicitLockWaitMarkerNotMissed, RollbackKeepsEarlier, NoDeadlock, EligibleEventuallyDeleted
