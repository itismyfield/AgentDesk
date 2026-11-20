# #8280 NoRecoveryEffects

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`.

TLC reached 77 states and 120 steps. The chart draws 25 states and 25 arrows.
The chart does not draw 61 steps that stay in one chart state.

## EndedResumeShowsRecovery breaks
The shortest path has 5 steps:
0. 종료가 확인된 pane / dcserver 종료
1. dcserver가 부팅됨: 종료가 확인된 pane / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 종료가 확인된 pane / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 종료가 확인된 pane / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 종료가 확인된 pane / 종료된 실행의 후계 준비
5. 기존 실행을 정리하고 새 프로세스로 resume: 종료가 확인된 pane / 새 프로세스 resume 뒤 복원 표시 누락

## Rules that hold
NoLiveRestore, NoDeadPaneWrite, ExactlyOnce, FirstInputDelivered
