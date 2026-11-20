# HerdrMissingBound

TLC reached 251 states and 487 steps. The chart draws 45 states and 51 arrows.
The chart does not draw 243 steps that stay in one chart state.

## BeforeFirstKey breaks
The shortest path has 7 steps:
0. 유휴 pane / dcserver 종료
1. dcserver 부팅: 유휴 pane / 첫 입력 대기
2. 원본 수락: 유휴 pane / 같은 ticket과 원본
3. intake reconcile 완료: 유휴 pane / 같은 ticket과 원본
4. 현재 증거로 시작 또는 보류: 유휴 pane / Idle grant
5. 동일 front와 followers 예약: 유휴 pane / 같은 front의 효과 예약
6. 첫 키 전 사망·stop·reset: 유휴 pane / 같은 front의 효과 예약 (첫 키 전 death)
7. provider leaf 1회 실행: 유휴 pane / 제출 결산 (첫 키 전 제어 경계 위반)

## Rules that hold
SameOriginal, OwnerExactlyOnce, OwnerDelivers
