# HerdrIdleExpire

TLC reached 192 states and 588 steps. The chart draws 192 states and 564 arrows.
The chart does not draw 12 steps that stay in one chart state.

## AlarmNotRepeated breaks
The shortest path has 3 steps:
0. Claude 실행 Bound·pane 살아 있음 · 작업 없음
1. 6시간 경과: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴
2. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 1회
3. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 2회

## IdleEventuallyCloses breaks
TLC's counterexample is in tlc-IdleEventuallyCloses.out. It has 3 steps:
0. Claude 실행 Bound·pane 살아 있음 · 작업 없음
1. 6시간 경과: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴
2. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 1회
3. idle-kill 틱: Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 2회
The run stops in "Claude 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 2회" forever.

## Rules that hold
TypeOK, NeverClosesBusy, NeverClosesUsed, NextMessageResumes
