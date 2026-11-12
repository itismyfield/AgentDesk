# WriterWake

TLC reached 104 states and 278 steps. The chart draws 11 states and 37 arrows.
The chart does not draw 152 steps that stay in one chart state.

## RestartPreparedNotLost breaks
The shortest path has 2 steps:
0. 답 준비됨, 옛 TUI 실행 중
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. writer process가 중단됨 [이전 재시작 없음]: 재시작이 준비된 답을 잃음

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
