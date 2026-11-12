# WriterWake

TLC reached 12 states and 24 steps. The chart draws 5 states and 6 arrows.
The chart does not draw 12 steps that stay in one chart state.

## RestartExactEndEventuallyCloses breaks
TLC's counterexample is in tlc-RestartExactEndEventuallyCloses.out. It has 4 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. writer process가 중단됨 [이전 재시작 없음]: writer 재시작, durable 기록 유지
3. 같은 source와 ledger를 다시 읽음: 물리 종료 확인, witness 읽기 전
4. historical scope를 검증하지 못해 보류: witness를 확인하지 못해 보류 [재시작 기회 소진]
The run stops in "witness를 확인하지 못해 보류 [재시작 기회 소진]" forever.

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock, RestartPreparedEventuallyPosted
