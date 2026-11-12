# WriterWake

TLC reached 356 states and 1398 steps. The chart draws 15 states and 49 arrows.
The chart does not draw 888 steps that stay in one chart state.

## RestartReplyOnce breaks
The shortest path has 13 steps:
0. 기존 Prepared, 결과 미확인
1. 읽기 가능한 history로 부재 확인: 기존 Prepared, 결과 미확인
2. old incarnation의 실제 종료 기록: 물리 종료 확인, witness 읽기 전
3. 확인된 부재를 ledger에 기록: 부재 확인 receipt, 새 Prepared 전
4. 같은 unit의 새 Prepared receipt 기록: durable Prepared receipt, POST 허가
5. 새 Prepared receipt의 허가로 POST 시작: 기존 Prepared의 POST 확인 대기
6. writer process가 중단됨 [이전 재시작 없음]: writer 재시작, POST 확인 미완료
7. 같은 source와 ledger를 다시 읽음: 기존 Prepared의 POST 확인 대기
8. historical binding과 prefix 검증 후 witness 설치: 기존 Prepared의 POST 확인 대기
9. old opener 재생: 기존 Prepared의 POST 확인 대기
10. cutoff에서 old opener만 reset: 기존 Prepared의 POST 확인 대기
11. cutoff 뒤 새 prompt 처리: 기존 Prepared의 POST 확인 대기
12. old prefix seal, 기존 ledger debt 보존: 기존 Prepared의 POST 확인 대기
13. confirm 허가 없이 기존 Prepared 재게시: 같은 답이 두 번 게시됨

## Rules that hold
RestartExactEndOnly, RestartNewPromptNotAbsorbed, RestartOldEndCannotCloseNewOpen, RestartWitnessBeforeSeal, RestartPreparedNotLost, RestartWitnessSurvivesBoot, RestartNoNativeForgery, NoDeadlock
