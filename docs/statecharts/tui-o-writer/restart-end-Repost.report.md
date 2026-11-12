# WriterWake

TLC reached 112 states and 298 steps. The chart draws 12 states and 33 arrows.
The chart does not draw 153 steps that stay in one chart state.

## RestartReplyOnce breaks
The shortest path has 10 steps:
0. 답 준비됨, 옛 TUI 실행 중
1. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
2. 기존 Prepared identity로 게시: 기존 Prepared의 POST 확인 대기
3. writer process가 중단됨 [이전 재시작 없음]: writer 재시작, POST 확인 미완료
4. 같은 source와 ledger를 다시 읽음: 기존 Prepared의 POST 확인 대기
5. historical binding과 prefix 검증 후 witness 설치: 기존 Prepared의 POST 확인 대기
6. old opener 재생: 기존 Prepared의 POST 확인 대기
7. cutoff에서 old opener만 reset: 기존 Prepared의 POST 확인 대기
8. cutoff 뒤 새 prompt 처리: 기존 Prepared의 POST 확인 대기
9. old prefix seal, 기존 ledger debt 보존: 기존 Prepared의 POST 확인 대기
10. 기존 POST를 확인하지 않고 다시 게시: 같은 답이 두 번 게시됨

## Rules that hold
RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
