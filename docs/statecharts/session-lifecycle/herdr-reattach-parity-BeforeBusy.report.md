# HerdrMissingBound

TLC reached 17 states and 18 steps. The chart draws 7 states and 8 arrows.
The chart does not draw 3 steps that stay in one chart state.

## NoIdleOnly breaks
The shortest path has 6 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택
5. 위치·registry·watcher와 owner 인수: 작업 중 pane / 이름 유일·nonce 일치 / watcher·진행 턴 lease·Busy grant 인수
6. 평소 입력 규칙 적용: 작업 중 pane / 이름 유일·nonce 일치 / 작업 중인데 Idle을 별도 요구

## ParityResolves breaks
TLC's counterexample is in tlc-ParityResolves.out. It has 6 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택
5. 위치·registry·watcher와 owner 인수: 작업 중 pane / 이름 유일·nonce 일치 / watcher·진행 턴 lease·Busy grant 인수
6. 평소 입력 규칙 적용: 작업 중 pane / 이름 유일·nonce 일치 / 작업 중인데 Idle을 별도 요구
The run stops in "작업 중 pane / 이름 유일·nonce 일치 / 작업 중인데 Idle을 별도 요구" forever.

## Rules that hold
NoLiveDiscard, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce
