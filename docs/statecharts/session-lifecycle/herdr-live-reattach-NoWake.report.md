# #8280 NoWake

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`.

TLC reached 59 states and 88 steps. The chart draws 21 states and 19 arrows.
The chart does not draw 45 steps that stay in one chart state.

## FirstInputDelivered breaks
TLC's counterexample is in tlc-FirstInputDelivered.out. It has 6 steps:
0. 살아 있는 작업 중 pane / dcserver 종료
1. dcserver가 부팅됨: 살아 있는 작업 중 pane / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 살아 있는 작업 중 pane / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 살아 있는 작업 중 pane / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 살아 있는 작업 중 pane / 진행 중인 턴의 입력 경계 대기
5. 독립 reader가 source를 대조: 살아 있는 작업 중 pane / 진행 중인 턴의 입력 경계 대기
6. pane 또는 입력 준비 상태가 확인됨: 살아 있는 작업 중 pane / 증거가 바뀌었으나 입력 보류
The run stops in "살아 있는 작업 중 pane / 증거가 바뀌었으나 입력 보류" forever.

## Rules that hold
NoLiveRestore, EndedResumeShowsRecovery, NoDeadPaneWrite, ExactlyOnce
