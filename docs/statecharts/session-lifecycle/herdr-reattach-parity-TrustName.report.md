# HerdrMissingBound

TLC reached 20 states and 23 steps. The chart draws 10 states and 12 arrows.
The chart does not draw 4 steps that stay in one chart state.

## NamedOnlyCurrent breaks
The shortest path has 5 steps:
0. 작업 중 pane / 같은 logical key의 이름 복수 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 같은 logical key의 이름 복수 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 같은 logical key의 이름 복수 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 같은 logical key의 이름 복수 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 같은 logical key의 이름 복수 / 이름으로 pane 선택
5. 위치·registry·watcher와 owner 인수: 작업 중 pane / 같은 logical key의 이름 복수 / watcher·진행 턴 lease·Busy grant 인수

## Rules that hold
NoLiveDiscard, NoIdleOnly, BusyAuthority, ProvenRejection, SameOriginal, OwnerExactlyOnce, ParityResolves
