# HerdrMissingBound

TLC reached 14 states and 14 steps. The chart draws 5 states and 6 arrows.
The chart does not draw 2 steps that stay in one chart state.

## NoIdentityLoss breaks
The shortest path has 5 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / pane identity 재관찰 대기
5. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 살아 있는 pane의 원본 폐기

## ParityResolves breaks
TLC's counterexample is in tlc-ParityResolves.out. It has 5 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
2. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / pane identity 재관찰 대기
5. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 살아 있는 pane의 원본 폐기
The run stops in "pane identity 미확인 / 이름 유일·nonce 일치 / 살아 있는 pane의 원본 폐기" forever.

## Rules that hold
NoLiveDiscard, NoIdleOnly, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce, NoCompetingClaim
