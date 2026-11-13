# NativeInputAttribution

Code SHA: `33d1f4b5a9dce7023e2d0014e45a05ed6c530bba`. Design incident baseline, production unchanged.
Generated with `NativeAttributionCurrentQueue.cfg`; raw TLC logs are under `/Users/itismyfield/.adk/lane-runs/i8148/logs/d1-chart-current-v3/`.

TLC reached 6,416 states, more than the chart budget of 500, so the chart draws only the bug paths: 11 states and 13 arrows.

## NoForeignResponse breaks
The shortest path has 5 steps:
0. 원본 두 개 대기
1. 외부 예약·알림 입력 enqueue: 외부 입력 enqueue
2. 예약·알림·TUI 직접 입력 시작: 외부 턴 실행
3. AgentDesk 입력 주입: AgentDesk 입력 주입
4. Claude 내부 큐 enqueue: Claude 내부 큐 수신
5. 외부 assistant end_turn: 외부 assistant end: native 종료 전 · 남의 본문 축적

## IdleEventuallyReleased breaks
TLC's counterexample is in tlc-IdleEventuallyReleased.out. It has 13 steps:
0. 원본 두 개 대기
1. 외부 예약·알림 입력 enqueue: 외부 입력 enqueue
2. 예약·알림·TUI 직접 입력 시작: 외부 턴 실행
3. AgentDesk 입력 주입: AgentDesk 입력 주입
4. Claude 내부 큐 enqueue: Claude 내부 큐 수신
5. 프로세스 재시작: 재시작: 원본과 결과 보존
6. boot: 원본과 cleanup debt 복원: Claude 내부 큐 수신
7. 외부 assistant end_turn: 외부 assistant end: native 종료 전 · 남의 본문 축적
8. 외부 strict closer: 외부 native 종료: 내 큐 입력은 미실행 · 남의 본문 축적
9. native idle 미종결 경보 1회: 외부 native 종료: 내 큐 입력은 미실행 · 남의 본문 축적
10. Claude 내부 큐 dequeue: dequeue: 다음 user 대기 · 남의 본문 축적
11. 자기 user로 실행 시작: 자기 입력의 native 실행 · 남의 본문 축적
12. 자기 assistant end_turn: 자기 입력의 native 실행 · 남의 본문 축적
13. 자기 strict closer: 자기 native 종료: 결과 확정 대기 · 남의 본문 축적
The run stops in "자기 native 종료: 결과 확정 대기 · 남의 본문 축적" forever.

## HeadEventuallyInjected breaks
TLC's counterexample is in tlc-HeadEventuallyInjected.out. It has 13 steps:
0. 원본 두 개 대기
1. 외부 예약·알림 입력 enqueue: 외부 입력 enqueue
2. 예약·알림·TUI 직접 입력 시작: 외부 턴 실행
3. AgentDesk 입력 주입: AgentDesk 입력 주입
4. Claude 내부 큐 enqueue: Claude 내부 큐 수신
5. 프로세스 재시작: 재시작: 원본과 결과 보존
6. boot: 원본과 cleanup debt 복원: Claude 내부 큐 수신
7. 외부 assistant end_turn: 외부 assistant end: native 종료 전 · 남의 본문 축적
8. 외부 strict closer: 외부 native 종료: 내 큐 입력은 미실행 · 남의 본문 축적
9. native idle 미종결 경보 1회: 외부 native 종료: 내 큐 입력은 미실행 · 남의 본문 축적
10. Claude 내부 큐 dequeue: dequeue: 다음 user 대기 · 남의 본문 축적
11. 자기 user로 실행 시작: 자기 입력의 native 실행 · 남의 본문 축적
12. 자기 assistant end_turn: 자기 입력의 native 실행 · 남의 본문 축적
13. 자기 strict closer: 자기 native 종료: 결과 확정 대기 · 남의 본문 축적
The run stops in "자기 native 종료: 결과 확정 대기 · 남의 본문 축적" forever.

## EveryInputEventuallySettled breaks
TLC's counterexample is in tlc-EveryInputEventuallySettled.out. It has 13 steps:
0. 원본 두 개 대기
1. 외부 예약·알림 입력 enqueue: 외부 입력 enqueue
2. 예약·알림·TUI 직접 입력 시작: 외부 턴 실행
3. AgentDesk 입력 주입: AgentDesk 입력 주입
4. Claude 내부 큐 enqueue: Claude 내부 큐 수신
5. 프로세스 재시작: 재시작: 원본과 결과 보존
6. boot: 원본과 cleanup debt 복원: Claude 내부 큐 수신
7. 외부 assistant end_turn: 외부 assistant end: native 종료 전 · 남의 본문 축적
8. 외부 strict closer: 외부 native 종료: 내 큐 입력은 미실행 · 남의 본문 축적
9. native idle 미종결 경보 1회: 외부 native 종료: 내 큐 입력은 미실행 · 남의 본문 축적
10. Claude 내부 큐 dequeue: dequeue: 다음 user 대기 · 남의 본문 축적
11. 자기 user로 실행 시작: 자기 입력의 native 실행 · 남의 본문 축적
12. 자기 assistant end_turn: 자기 입력의 native 실행 · 남의 본문 축적
13. 자기 strict closer: 자기 native 종료: 결과 확정 대기 · 남의 본문 축적
The run stops in "자기 native 종료: 결과 확정 대기 · 남의 본문 축적" forever.

## Rules that hold
CompletionExactlyOnce, NoForeignEnd, NoSilentLoss, NoSuccessorRelease, NoSourceBorrow
