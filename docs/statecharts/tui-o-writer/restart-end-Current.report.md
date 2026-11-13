# WriterWake

TLC reached 28 states and 64 steps. The chart draws 9 states and 14 arrows.
The chart does not draw 28 steps that stay in one chart state.

## RestartExactEndEventuallyCloses breaks
TLC's counterexample is in tlc-RestartExactEndEventuallyCloses.out. It has 8 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. writer process가 중단됨 [이전 재시작 없음]: writer 재시작, durable 기록 유지
3. 같은 source와 ledger를 다시 읽음: 물리 종료 확인, witness 읽기 전
4. witness 설치 없이 source 재생: witness 없이 옛 opener 읽기
5. old opener 재생: old cutoff 앞, 새 prompt 읽기 전
6. closer 없이 old Open 유지: 옛 Open이 아직 닫히지 않음
7. cutoff 뒤 새 prompt 처리: old prefix seal 전
8. old prefix seal, 기존 ledger debt 보존: 옛 Open이 Unknown에 남음 [재시작 기회 소진]
The run stops in "옛 Open이 Unknown에 남음 [재시작 기회 소진]" forever.

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock, RestartPreparedEventuallyPosted
