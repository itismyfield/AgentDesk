# WriterWake

TLC reached 92 states and 210 steps. The chart draws 12 states and 24 arrows.
The chart does not draw 112 steps that stay in one chart state.

## RestartOldEndCannotCloseNewOpen breaks
The shortest path has 7 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. historical binding과 prefix 검증 후 witness 설치: witness 설치 후 옛 opener 읽기
3. old opener 재생: old cutoff 앞, 새 prompt 읽기 전
4. cutoff에서 old opener만 reset: 옛 Open만 Idle, 다음 record 읽기
5. cutoff 뒤 새 prompt 처리: 새 Open 유지, old prefix seal 전
6. old prefix seal, 기존 ledger debt 보존: durable Prepared, 게시 대기
7. 새 Open 뒤 old End 재관측: 옛 End가 새 Open을 닫음

## Rules that hold
RestartReplyOnce, RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartWitnessBeforeSeal, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
