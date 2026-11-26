# HerdrIdleExpire

TLC reached 96 states and 294 steps. The chart draws 96 states and 282 arrows.
The chart does not draw 6 steps that stay in one chart state.

## AlarmNotRepeated breaks
The shortest path has 3 steps:
0. 실행 Bound·pane 살아 있음 · 작업 없음
1. 6시간 경과: 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴
2. idle-kill 틱: 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 1회
3. idle-kill 틱: 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 2회

## IdleEventuallyCloses breaks
TLC's counterexample is in tlc-IdleEventuallyCloses.out. It has 3 steps:
0. 실행 Bound·pane 살아 있음 · 작업 없음
1. 6시간 경과: 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴
2. idle-kill 틱: 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 1회
3. idle-kill 틱: 실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 2회
The run stops in "실행 Bound·pane 살아 있음 · 작업 없음 · 6시간 유휴 · 보류 경보 2회" forever.

## Rules that hold
TypeOK, NeverClosesBusy, NextMessageResumes
