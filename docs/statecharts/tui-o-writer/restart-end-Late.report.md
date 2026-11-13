# WriterWake

TLC reached 42 states and 94 steps. The chart draws 10 states and 16 arrows.
The chart does not draw 54 steps that stay in one chart state.

## RestartNewPromptNotAbsorbed breaks
The shortest path has 5 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. witness 설치 없이 source 재생: witness 없이 옛 opener 읽기
3. old opener 재생: old cutoff 앞, 새 prompt 읽기 전
4. closer 없이 old Open 유지: 옛 Open이 아직 닫히지 않음
5. cutoff 뒤 새 prompt 처리: 새 prompt가 옛 Open에 흡수됨

## RestartWitnessBeforeSeal breaks
The shortest path has 6 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. witness 설치 없이 source 재생: witness 없이 옛 opener 읽기
3. old opener 재생: old cutoff 앞, 새 prompt 읽기 전
4. closer 없이 old Open 유지: 옛 Open이 아직 닫히지 않음
5. cutoff 뒤 새 prompt 처리: 새 prompt가 옛 Open에 흡수됨
6. old prefix seal, 기존 ledger debt 보존: 새 prompt 흡수 뒤 witness 없이 seal [재시작 가능]

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartOldEndCannotCloseNewOpen, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
