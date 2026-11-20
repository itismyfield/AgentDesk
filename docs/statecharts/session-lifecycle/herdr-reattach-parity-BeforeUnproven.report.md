# HerdrMissingBound

TLC reached 12 states and 12 steps. The chart draws 4 states and 5 arrows.
The chart does not draw 2 steps that stay in one chart state.

## NoLiveDiscard breaks
The shortest path has 4 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 살아 있는 pane의 원본 폐기

## ParityResolves breaks
TLC's counterexample is in tlc-ParityResolves.out. It has 4 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 살아 있는 pane의 원본 폐기
The run stops in "작업 중 pane / 이름 유일·nonce 일치 / 살아 있는 pane의 원본 폐기" forever.

## Rules that hold
NoIdleOnly, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce
