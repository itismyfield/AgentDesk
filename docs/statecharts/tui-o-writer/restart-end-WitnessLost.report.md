# WriterWake

TLC reached 122 states and 320 steps. The chart draws 13 states and 22 arrows.
The chart does not draw 257 steps that stay in one chart state.

## RestartWitnessSurvivesBoot breaks
The shortest path has 2 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. writer process가 중단됨 [이전 재시작 없음]: durable witness가 crash에서 유실됨

## RestartWitnessBeforeSeal breaks
The shortest path has 8 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. writer process가 중단됨 [이전 재시작 없음]: durable witness가 crash에서 유실됨
3. 같은 source와 ledger를 다시 읽음: durable witness가 crash에서 유실됨
4. witness 설치 없이 source 재생: durable witness가 crash에서 유실됨
5. old opener 재생: durable witness가 crash에서 유실됨
6. closer 없이 old Open 유지: durable witness가 crash에서 유실됨
7. cutoff 뒤 새 prompt 처리: durable witness가 crash에서 유실됨
8. old prefix seal, 기존 ledger debt 보존: durable witness 유실 뒤 witness 없는 seal [재시작 기회 소진]

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartPreparedNotLost, RestartNoNativeForgery, NoDeadlock
