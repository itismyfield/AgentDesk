Code baseline: `2ddece1ebb2de1f66c4263c4bc29239b36250ec3`. Raw output: `/Users/itismyfield/.adk/release/lane-runs/hl-20260925/i7353/sc-s4-12/no-recheck-handoff`.

# HostedCleanup

TLC reached 52,776 states, more than the chart budget of 500, so the chart draws only the bug paths: 9 states and 8 arrows.

## GCNewTurnKept breaks
The shortest path has 8 steps:
0. 후보 조회 전
1. 후보 snapshot 또는 조회 실패: 후보 snapshot
2. host와 Legacy marker 판단: host gate 통과
3. DB 점유 조회: 점유 없음 관측
4. owner 파일 확인: 로컬 owner 관측
5. tmux presence 확인: tmux 없음 관측
6. 새 turn이 DB 조건 변경: tmux 없음 관측 · 새 turn
7. DELETE 조건 판정: 조건부 DELETE 적용 · 새 turn
8. statement commit: 삭제됨 · 새 turn

## GCConditionalRefusalsKept breaks
The shortest path has 8 steps:
0. 후보 조회 전
1. 후보 snapshot 또는 조회 실패: 후보 snapshot
2. host와 Legacy marker 판단: host gate 통과
3. DB 점유 조회: 점유 없음 관측
4. owner 파일 확인: 로컬 owner 관측
5. tmux presence 확인: tmux 없음 관측
6. 새 turn이 DB 조건 변경: tmux 없음 관측 · 새 turn
7. DELETE 조건 판정: 조건부 DELETE 적용 · 새 turn
8. statement commit: 삭제됨 · 새 turn

## Rules that hold
GCUnknownLookupKept, GCObservedHostBeforeProbe, GCObservedLegacyMarkerSafe, GCObservedMissingAndLocal, GCKillFailureKept, NoDeadlock
