# #8280 Duplicate

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`.

TLC reached 90 states and 151 steps. The chart draws 29 states and 29 arrows.
The chart does not draw 79 steps that stay in one chart state.

## ExactlyOnce breaks
The shortest path has 6 steps:
0. 살아 있는 유휴 pane / dcserver 종료
1. dcserver가 부팅됨: 살아 있는 유휴 pane / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 살아 있는 유휴 pane / 같은 pane에 이어 쓰기
5. 기존 pane에 한 번 제출: 살아 있는 유휴 pane / 첫 메시지 1회 전달
6. 이미 결산한 원본을 다시 제출: 살아 있는 유휴 pane / 첫 메시지 중복 전달

## Rules that hold
NoLiveRestore, EndedResumeShowsRecovery, NoDeadPaneWrite, FirstInputDelivered
