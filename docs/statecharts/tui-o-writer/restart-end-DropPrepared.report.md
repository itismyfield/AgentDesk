# WriterWake

TLC reached 312 states and 1184 steps. The chart draws 14 states and 52 arrows.
The chart does not draw 736 steps that stay in one chart state.

## RestartPreparedNotLost breaks
The shortest path has 2 steps:
0. 기존 Prepared, 결과 미확인
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. writer process가 중단됨 [이전 재시작 없음]: 재시작이 준비된 답을 잃음

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
