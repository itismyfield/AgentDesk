# WriterWake

TLC reached 240 states and 636 steps. The chart draws 8 states and 12 arrows.
The chart does not draw 508 steps that stay in one chart state.

## RestartExactEndOnly breaks
The shortest path has 4 steps:
0. 옛 Open, native closer 없음
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. historical binding과 prefix 검증 후 witness 설치: witness 설치 후 옛 opener 읽기
3. old opener 재생: old cutoff 앞, 새 prompt 읽기 전
4. cutoff에서 old opener만 reset: 다른 nonce 또는 cutoff를 닫음

## Rules that hold
RestartReplyOnce, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
