Code baseline: `2ddece1ebb2de1f66c4263c4bc29239b36250ec3`. Raw output: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/sc-s4-12/after-gaps-handoff`.

# HostedCleanup

TLC reached 53,944 states, more than the chart budget of 500, so the chart draws only the bug paths: 15 states and 14 arrows.

## GCCurrentLegacyMarkerSafe breaks
The shortest path has 8 steps:
0. 후보 조회 전
1. 후보 snapshot 또는 조회 실패: 후보 snapshot
2. host와 Legacy marker 판단: host gate 통과
3. DB 점유 조회: 점유 없음 관측
4. owner 파일 확인: 로컬 owner 관측
5. tmux presence 확인: tmux 없음 관측
6. 관측 뒤 marker 변경: tmux 없음 관측 · marker 변경
7. DELETE 조건 판정: 조건부 DELETE 적용 · marker 변경
8. statement commit: 삭제됨 · marker 변경

## GCCurrentOwnerLocal breaks
The shortest path has 8 steps:
0. 후보 조회 전
1. 후보 snapshot 또는 조회 실패: 후보 snapshot
2. host와 Legacy marker 판단: host gate 통과
3. DB 점유 조회: 점유 없음 관측
4. owner 파일 확인: 로컬 owner 관측
5. tmux presence 확인: tmux 없음 관측
6. 관측 뒤 owner 변경: tmux 없음 관측 · owner 변경
7. DELETE 조건 판정: 조건부 DELETE 적용 · owner 변경
8. statement commit: 삭제됨 · owner 변경

## GCAliasesRechecked breaks
The shortest path has 8 steps:
0. 후보 조회 전
1. 후보 snapshot 또는 조회 실패: 후보 snapshot
2. host와 Legacy marker 판단: host gate 통과
3. DB 점유 조회: 점유 없음 관측
4. owner 파일 확인: 로컬 owner 관측
5. tmux presence 확인: tmux 없음 관측
6. 관측 뒤 alias 추가: tmux 없음 관측 · alias 추가
7. DELETE 조건 판정: 조건부 DELETE 적용 · alias 추가
8. statement commit: 삭제됨 · alias 추가

## Rules that hold
GCNewTurnKept, GCUnknownLookupKept, GCObservedHostBeforeProbe, GCObservedLegacyMarkerSafe, GCObservedMissingAndLocal, GCKillFailureKept, GCConditionalRefusalsKept, NoDeadlock
