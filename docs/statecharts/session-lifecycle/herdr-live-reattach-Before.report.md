# #8280 Before

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`.

TLC reached 90 states and 151 steps. The chart draws 27 states and 25 arrows.
The chart does not draw 92 steps that stay in one chart state.

## NoLiveRestore breaks
The shortest path has 4 steps:
0. 살아 있는 유휴 pane / dcserver 종료
1. dcserver가 부팅됨: 살아 있는 유휴 pane / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 살아 있는 유휴 pane / 이어 쓰기에 복원 표시·문맥이 붙음

## Rules that hold
EndedResumeShowsRecovery, NoDeadPaneWrite, ExactlyOnce, FirstInputDelivered
