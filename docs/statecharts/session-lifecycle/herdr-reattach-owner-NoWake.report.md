# HerdrMissingBound

TLC reached 93 states and 165 steps. The chart draws 27 states and 26 arrows.
The chart does not draw 93 steps that stay in one chart state.

## OwnerDelivers breaks
TLC's counterexample is in tlc-OwnerDelivers.out. It has 6 steps:
0. 작업 중 pane / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 첫 입력 대기
2. intake reconcile 완료: 작업 중 pane / 첫 입력 대기
3. 부팅 source 관찰: 작업 중 pane / 첫 입력 대기
4. 원본 수락: 작업 중 pane / 같은 ticket과 원본
5. 현재 증거로 시작 또는 보류: 작업 중 pane / 원본 보류와 관찰 대기
6. 같은 ticket 재수락 시도: 작업 중 pane / 원본 보류와 관찰 대기
The run stops in "작업 중 pane / 원본 보류와 관찰 대기" forever.

## Rules that hold
SameOriginal, OwnerExactlyOnce, BeforeFirstKey
