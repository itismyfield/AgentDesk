# HerdrMissingBound

TLC reached 24 states and 26 steps. The chart draws 6 states and 6 arrows.
The chart does not draw 7 steps that stay in one chart state.

## NoIdentityLoss breaks
The shortest path has 6 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / 기존 composer가 입력을 막음
1. composer 기한과 입력을 저장한 뒤 재시작: pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
2. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
5. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / identity 보류에 옛 기한이 남음
6. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 옛 기한이 원본과 followers를 폐기

## RestoredComposerUnit breaks
The shortest path has 6 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / 기존 composer가 입력을 막음
1. composer 기한과 입력을 저장한 뒤 재시작: pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
2. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
5. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / identity 보류에 옛 기한이 남음
6. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 옛 기한이 원본과 followers를 폐기

## ParityResolves breaks
TLC's counterexample is in tlc-ParityResolves.out. It has 6 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / 기존 composer가 입력을 막음
1. composer 기한과 입력을 저장한 뒤 재시작: pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
2. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
5. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / identity 보류에 옛 기한이 남음
6. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 옛 기한이 원본과 followers를 폐기
The run stops in "pane identity 미확인 / 이름 유일·nonce 일치 / 옛 기한이 원본과 followers를 폐기" forever.

## Rules that hold
NoLiveDiscard, NoIdleOnly, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce, NoCompetingClaim
