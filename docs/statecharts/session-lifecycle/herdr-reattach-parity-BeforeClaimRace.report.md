# HerdrMissingBound

TLC reached 30 states and 38 steps. The chart draws 17 states and 23 arrows.
The chart does not draw 4 steps that stay in one chart state.

## NoCompetingClaim breaks
The shortest path has 5 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택
5. 같은 채널의 새 headless claim: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택 (새 headless claim이 기존 턴을 막음)

## ParityResolves breaks
TLC's counterexample is in tlc-ParityResolves.out. It has 5 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택
5. 같은 채널의 새 headless claim: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택 (새 headless claim이 기존 턴을 막음)
The run stops in "작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택 (새 headless claim이 기존 턴을 막음)" forever.

## Rules that hold
NoLiveDiscard, NoIdleOnly, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce
