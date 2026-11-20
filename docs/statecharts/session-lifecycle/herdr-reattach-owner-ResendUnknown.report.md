# HerdrMissingBound

TLC reached 323 states and 685 steps. The chart draws 55 states and 73 arrows.
The chart does not draw 336 steps that stay in one chart state.

## OwnerExactlyOnce breaks
The shortest path has 8 steps:
0. 유휴 pane / dcserver 종료
1. dcserver 부팅: 유휴 pane / 첫 입력 대기
2. 원본 수락: 유휴 pane / 같은 ticket과 원본
3. intake reconcile 완료: 유휴 pane / 같은 ticket과 원본
4. 현재 증거로 시작 또는 보류: 유휴 pane / Idle grant
5. 동일 front와 followers 예약: 유휴 pane / 같은 front의 효과 예약
6. provider leaf 1회 실행: 유휴 pane / Enter 응답 불명, receipt 관찰 중
7. 불명 receipt에서 다시 예약: 유휴 pane / 같은 front의 효과 예약
8. provider leaf 1회 실행: 유휴 pane / 제출 결산 (중복 제출)

## OwnerDelivers breaks
TLC's counterexample is in tlc-OwnerDelivers.out. It has 11 steps:
0. 유휴 pane / dcserver 종료
1. dcserver 부팅: 유휴 pane / 첫 입력 대기
2. 부팅 source 관찰: 유휴 pane / 첫 입력 대기
3. intake reconcile 완료: 유휴 pane / 첫 입력 대기
4. 원본 수락: 유휴 pane / 같은 ticket과 원본
5. 현재 증거로 시작 또는 보류: 유휴 pane / Idle grant
6. 동일 front와 followers 예약: 유휴 pane / 같은 front의 효과 예약
7. 같은 ticket 재수락 시도: 유휴 pane / 같은 front의 효과 예약
8. provider leaf 1회 실행: 유휴 pane / Enter 응답 불명, receipt 관찰 중
9. 불명 receipt에서 다시 예약: 유휴 pane / 같은 front의 효과 예약
10. provider leaf 1회 실행: 유휴 pane / Enter 응답 불명, receipt 관찰 중 (중복 제출)
11. 불명 receipt에서 다시 예약: 유휴 pane / 같은 front의 효과 예약 (중복 제출)
The run stops in "유휴 pane / 같은 front의 효과 예약 (중복 제출)" forever.

## Rules that hold
SameOriginal, BeforeFirstKey
