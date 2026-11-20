# #8280 NoRecheck

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`.

TLC reached 15 states and 24 steps. The chart draws 6 states and 7 arrows.
The chart does not draw 10 steps that stay in one chart state.

## NoDeadPaneWrite breaks
The shortest path has 6 steps:
0. 살아 있는 유휴 pane / dcserver 종료
1. dcserver가 부팅됨: 살아 있는 유휴 pane / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 살아 있는 유휴 pane / 같은 pane에 이어 쓰기
5. 첫 키 전에 pane이 종료됨: 살아 있는 유휴 pane / 제출 후보 pane이 사라짐
6. 종료되거나 미확인인 pane에 제출: 살아 있는 유휴 pane / 첫 메시지 유실

## ExactlyOnce breaks
The shortest path has 6 steps:
0. 살아 있는 유휴 pane / dcserver 종료
1. dcserver가 부팅됨: 살아 있는 유휴 pane / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 살아 있는 유휴 pane / 같은 pane에 이어 쓰기
5. 첫 키 전에 pane이 종료됨: 살아 있는 유휴 pane / 제출 후보 pane이 사라짐
6. 종료되거나 미확인인 pane에 제출: 살아 있는 유휴 pane / 첫 메시지 유실

## FirstInputDelivered breaks
TLC's counterexample is in tlc-FirstInputDelivered.out. It has 7 steps:
0. 살아 있는 유휴 pane / dcserver 종료
1. dcserver가 부팅됨: 살아 있는 유휴 pane / 첫 메시지 대기
2. 독립 reader가 source를 대조: 살아 있는 유휴 pane / 첫 메시지 대기
3. 첫 메시지를 같은 큐에 등록: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
4. 입력 reconcile 완료: 살아 있는 유휴 pane / 같은 원본이 큐에 있음
5. 현재 실행 증거로 경로를 고름: 살아 있는 유휴 pane / 같은 pane에 이어 쓰기
6. 첫 키 전에 pane이 종료됨: 살아 있는 유휴 pane / 제출 후보 pane이 사라짐
7. 종료되거나 미확인인 pane에 제출: 살아 있는 유휴 pane / 첫 메시지 유실
The run stops in "살아 있는 유휴 pane / 첫 메시지 유실" forever.

## Rules that hold
NoLiveRestore, EndedResumeShowsRecovery
