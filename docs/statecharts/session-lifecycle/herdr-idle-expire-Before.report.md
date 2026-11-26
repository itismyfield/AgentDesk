# HerdrIdleExpire

TLC reached 5,820 states, more than the chart budget of 500, so the chart draws only the bug paths: 8 states and 7 arrows.

## AlarmNotRepeated breaks
The shortest path has 3 steps:
0. Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 부팅 유예 중
1. 6시간 경과: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 부팅 유예 중 · 6시간 유휴
2. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 부팅 유예 중 · 6시간 유휴 · 보류 경보 1회
3. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 부팅 유예 중 · 6시간 유휴 · 보류 경보 2회

## IdleEventuallyCloses breaks
TLC's counterexample is in tlc-IdleEventuallyCloses.out. It has 5 steps:
0. Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 부팅 유예 중
1. 6시간 경과: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 부팅 유예 중 · 6시간 유휴
2. 부팅 유예 지남: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴
3. /effort 의도 저장: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · owner 슬롯 /effort 의도 · /effort 종료 대기 · 6시간 유휴
4. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · owner 슬롯 /effort 의도 · /effort 종료 대기 · 6시간 유휴 · 보류 경보 1회
5. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · owner 슬롯 /effort 의도 · /effort 종료 대기 · 6시간 유휴 · 보류 경보 2회
The run stops in "Claude 실행 Bound·pane 살아 있음 · 작업 없음 · owner 슬롯 /effort 의도 · /effort 종료 대기 · 6시간 유휴 · 보류 경보 2회" forever.

## Rules that hold
TypeOK, NeverClosesBusy, NeverClosesUsed, NextMessageResumes, ClearNeverResumes
