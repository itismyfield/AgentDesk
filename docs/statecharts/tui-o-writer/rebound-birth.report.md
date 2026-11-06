Code baseline: `7a0c819e127964befd6bc0d022c5d4194d669c9c` + park working tree; reviewed delta config `WriterReboundBirth.cfg`. `LogCurrent=FALSE`.

구현 소스 참조: `2b07eed6f3a2a4ce9292456b1483d1fd9e5ef170` (main `a2ab3f26df7059ecf13d9ae9f7fcffa418560089` 위 구현). 최종 검증 결과는 구현 레인 보고서를 따른다.


# WriterRebound

TLC reached 207 states and 219 steps. The chart draws 20 states and 23 arrows.
The view hides 54 steps, so the chart does not draw them.

## UnknownNeedsAbsence breaks
The shortest path has 3 steps:
0. A 채택 · link 없음
1. oldless · 미적용 · Pending · 실패 log 시나리오: 전체 binding log와 복원 상태 준비
2. 원본 pathname Missing: 원본 소실 · resume 시작
3. 전체 log에서 current 판정 후 park 여부 결정: 현재성 Unknown · park · producer unknown

## ReadyAfterFollow breaks
The shortest path has 3 steps:
0. A 채택 · link 없음
1. oldless · 미적용 · Pending · 실패 log 시나리오: 전체 binding log와 복원 상태 준비
2. 원본 pathname Missing: 원본 소실 · resume 시작
3. 전체 log에서 current 판정 후 park 여부 결정: 유효 새 bind 미적용인데 ready

## ParkedReboundHalts breaks
The shortest path has 4 steps:
0. A 채택 · link 없음
1. oldless · 미적용 · Pending · 실패 log 시나리오: 전체 binding log와 복원 상태 준비
2. 원본 pathname Missing: 원본 소실 · resume 시작
3. 전체 log에서 current 판정 후 park 여부 결정: ready · 현재 B · producer unknown
4. 판정 tail 뒤 parked 원본으로 다시 bind: park된 A 재연결 또는 refinement 뒤에도 running

## ActiveLossVisible breaks
The shortest path has 5 steps:
0. A 채택 · link 없음
1. A → B 신규 bind: B 연결 · A 다음 B
2. B → A 재연결 · rotation만 먼저 저장: A 재연결 rotation 저장 · checkpoint는 아직 B
3. 해당 bind checkpoint 저장: 전체 binding log와 복원 상태 준비
4. 원본 pathname Missing: 원본 소실 · resume 시작
5. 전체 log에서 current 판정 후 park 여부 결정: 현재 A를 historical로 오판 · park · halted 없음

## CurrentNeverHistorical breaks
The shortest path has 5 steps:
0. A 채택 · link 없음
1. A → B 신규 bind: B 연결 · A 다음 B
2. B → A 재연결 · rotation만 먼저 저장: A 재연결 rotation 저장 · checkpoint는 아직 B
3. 해당 bind checkpoint 저장: 전체 binding log와 복원 상태 준비
4. 원본 pathname Missing: 원본 소실 · resume 시작
5. 전체 log에서 current 판정 후 park 여부 결정: 현재 A를 historical로 오판 · park · halted 없음

## Rules that hold
ParkPreservesEvidence, NoDeadlock
