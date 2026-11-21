# #8280 TrustRow

기준 소스 `8470a8b911849b2570e00103fb9bee33c5d9c7eb`.

TLC reached 67 states and 112 steps. The chart draws 22 states and 21 arrows.
The chart does not draw 66 steps that stay in one chart state.

## NoDeadPaneWrite breaks
The shortest path has 5 steps:
0. 실행 행만 확인됨 / dcserver 종료
1. dcserver가 부팅됨: 실행 행만 확인됨 / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 실행 행만 확인됨 / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 실행 행만 확인됨 / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 실행 행만 확인됨 / 행만으로 제출 후보 선택
5. 종료되거나 미확인인 pane에 제출: 실행 행만 확인됨 / 첫 메시지 유실

## ExactlyOnce breaks
The shortest path has 5 steps:
0. 실행 행만 확인됨 / dcserver 종료
1. dcserver가 부팅됨: 실행 행만 확인됨 / 첫 메시지 대기
2. 첫 메시지를 같은 큐에 등록: 실행 행만 확인됨 / 같은 원본이 큐에 있음
3. 입력 reconcile 완료: 실행 행만 확인됨 / 같은 원본이 큐에 있음
4. 현재 실행 증거로 경로를 고름: 실행 행만 확인됨 / 행만으로 제출 후보 선택
5. 종료되거나 미확인인 pane에 제출: 실행 행만 확인됨 / 첫 메시지 유실

## FirstInputDelivered breaks
TLC's counterexample is in tlc-FirstInputDelivered.out. It has 6 steps:
0. 실행 행만 확인됨 / dcserver 종료
1. dcserver가 부팅됨: 실행 행만 확인됨 / 첫 메시지 대기
2. 독립 reader가 source를 대조: 실행 행만 확인됨 / 첫 메시지 대기
3. 첫 메시지를 같은 큐에 등록: 실행 행만 확인됨 / 같은 원본이 큐에 있음
4. 입력 reconcile 완료: 실행 행만 확인됨 / 같은 원본이 큐에 있음
5. 현재 실행 증거로 경로를 고름: 실행 행만 확인됨 / 행만으로 제출 후보 선택
6. 종료되거나 미확인인 pane에 제출: 실행 행만 확인됨 / 첫 메시지 유실
The run stops in "실행 행만 확인됨 / 첫 메시지 유실" forever.

## Rules that hold
NoLiveRestore, EndedResumeShowsRecovery
