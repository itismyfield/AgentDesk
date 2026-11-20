# HerdrMissingBound

TLC reached 24 states and 29 steps. The chart draws 13 states and 17 arrows.
The chart does not draw 5 steps that stay in one chart state.

## OwnerExactlyOnce breaks
The shortest path has 9 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택
5. 위치·registry·watcher와 owner 인수: 작업 중 pane / 이름 유일·nonce 일치 / watcher·진행 턴 lease·Busy grant 인수
6. 평소 입력 규칙 적용: 작업 중 pane / 이름 유일·nonce 일치 / 원본의 평소 Busy 주입 예약
7. 원본 한 번 전달: 작업 중 pane / 이름 유일·nonce 일치 / 응답 불명, receipt 대기
8. 불명 receipt에서 다시 예약: 작업 중 pane / 이름 유일·nonce 일치 / 원본의 평소 Busy 주입 예약
9. 원본 한 번 전달: 작업 중 pane / 이름 유일·nonce 일치 / 제출 결산 (중복 제출)

## ParityResolves breaks
TLC's counterexample is in tlc-ParityResolves.out. It has 10 steps:
0. 작업 중 pane / 이름 유일·nonce 일치 / dcserver 종료
1. dcserver 부팅: 작업 중 pane / 이름 유일·nonce 일치 / 첫 원본 대기
2. 같은 원본 수락: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
3. intake reconcile 완료: 작업 중 pane / 이름 유일·nonce 일치 / 같은 원본이 큐에 있음
4. 현재 pane과 이름을 확인: 작업 중 pane / 이름 유일·nonce 일치 / 이름으로 pane 선택
5. 위치·registry·watcher와 owner 인수: 작업 중 pane / 이름 유일·nonce 일치 / watcher·진행 턴 lease·Busy grant 인수
6. 평소 입력 규칙 적용: 작업 중 pane / 이름 유일·nonce 일치 / 원본의 평소 Busy 주입 예약
7. 원본 한 번 전달: 작업 중 pane / 이름 유일·nonce 일치 / 응답 불명, receipt 대기
8. 불명 receipt에서 다시 예약: 작업 중 pane / 이름 유일·nonce 일치 / 원본의 평소 Busy 주입 예약
9. 원본 한 번 전달: 작업 중 pane / 이름 유일·nonce 일치 / 제출 결산 (중복 제출)
10. 채널에 답 게시: 작업 중 pane / 이름 유일·nonce 일치 / 답 채널 게시 (중복 제출)
The run stops in "작업 중 pane / 이름 유일·nonce 일치 / 답 채널 게시 (중복 제출)" forever.

## Rules that hold
NoLiveDiscard, NoIdleOnly, NamedOnlyCurrent, BusyAuthority, ProvenRejection, SameOriginal
