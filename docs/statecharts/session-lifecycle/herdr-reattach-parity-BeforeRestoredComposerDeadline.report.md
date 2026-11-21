# HerdrMissingBound

TLC reached 108 states and 124 steps. The chart draws 13 states and 17 arrows.
The chart does not draw 32 steps that stay in one chart state.

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
TLC's counterexample is in tlc-ParityResolves.out. It has 7 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / 기존 composer가 입력을 막음
1. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 기존 composer가 입력을 막음
2. composer 기한과 입력을 저장한 뒤 재시작: pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
3. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
4. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
5. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / identity 보류에 옛 기한이 남음
6. identity 보류 중 같은 큐 재인수: pane identity 미확인 / 이름 유일·nonce 일치 / identity 보류에 옛 기한이 남음
7. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 옛 기한이 원본과 followers를 폐기
The run stops in "pane identity 미확인 / 이름 유일·nonce 일치 / 옛 기한이 원본과 followers를 폐기" forever.

## NewComposerEventuallyTimed breaks
TLC's counterexample is in tlc-NewComposerEventuallyTimed.out. It has 10 steps:
0. pane identity 미확인 / 이름 유일·nonce 일치 / 기존 composer가 입력을 막음
1. composer 기한과 입력을 저장한 뒤 재시작: pane identity 미확인 / 이름 유일·nonce 일치 / dcserver 종료
2. dcserver 부팅: pane identity 미확인 / 이름 유일·nonce 일치 / 첫 원본 대기
3. 같은 원본 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. intake reconcile 완료: pane identity 미확인 / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
5. 현재 pane과 이름을 확인: pane identity 미확인 / 이름 유일·nonce 일치 / identity 보류가 저장된 기한을 중지
6. 보류 기준 시각 경과: pane identity 미확인 / 이름 유일·nonce 일치 / 기한이 지나도 같은 원본 보존
7. 별도 복구가 identity를 확인: pane identity 미확인 / 이름 유일·nonce 일치 / 별도 복구가 같은 실행을 재부착
8. 별도 직접 입력 B 수락: pane identity 미확인 / 이름 유일·nonce 일치 / 직접 CLI 입력 B가 active turn을 시작
9. 같은 실행의 일반 Busy 경로로 재진입: pane identity 미확인 / 이름 유일·nonce 일치 / A의 watcher가 일반 Busy 진입을 깨움
10. 평소 입력 규칙 적용: pane identity 미확인 / 이름 유일·nonce 일치 / modal에 막힌 A에 새 기한이 없음
The run stops in "pane identity 미확인 / 이름 유일·nonce 일치 / modal에 막힌 A에 새 기한이 없음" forever.

## Rules that hold
NoLiveDiscard, NoIdleOnly, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce, NoCompetingClaim
