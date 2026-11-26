# HumanHoldBound — #8430 사람 입력 보류 상한 (w1)

Code SHA: `a53f299560` (Current). Target = [설계 w1](../../design/8430-human-hold-bound.design.md).
Model: [`HumanHoldBound.tla`](HumanHoldBound.tla), view [`human-hold-bound.view.mjs`](human-hold-bound.view.mjs).

## 무엇을 보나
사람 owner front 하나가 다른 턴의 남은 lease(`cancel_token`)나 inflight 행 뒤에 보류된 모양이다. `HerdrBusyInput`의 `held` 상태(그 모델의 RetryHeld는 Eligible일 때만 돈다)를 이번 모양으로 세분한다. 기존 모델과 cfg는 바꾸지 않았다. 바꾼 cfg는 없고, 아래 19개를 새로 넣었다.

변수의 값 범위는 설계 문서 §2 진입점 전수표에서 가져왔다.
- `token` / `row` ∈ {old, new, none}: C1·C7·C25·C35
- `screen` ∈ {idle, working, draft, modal, unknown, none}: E18–E20, §4
- `transcript` ∈ {open, ended, none}: §4 둘째 근거
- `due` ∈ {none, armed, expired}: E9·E27–E29
- `front` ∈ {queued, held, stalled, sent, dropped, done}: E24·E33·D1–D2
- `ledger` ∈ {pending, finalized}: C13·C14
- `clock` 0..2와 `streak` 0..2: §5·§4(추상 3분, 연속 3회)

## 규칙
- INVARIANT `NoDuplicateDelivery`(중복 전달 0), `NoSilentLoss`(버린 입력은 반드시 특정 안내), `NoBoundOnBusySession`(작업 중·초안·모달·관측 실패 화면에 상한 발동 0), `NoNewTurnClearedByOld`(옛 terminal이 새 턴 lease를 풀지 않음), `TypeOK`
- PROPERTY `IdleEventuallyDelivered`: `<>[](screen = idle) => <>Delivered`. 세션이 Idle로 머물면 보류된 사람 입력은 결국 전달된다.
- PROPERTY `OrphanEventuallyDelivered`(i8372 rowOnly 전용): `<>Delivered`. pane이 없고 생산자가 끝난 행은 답장을 영원히 막지 않는다.

## 결과 (TLC 1.7.4, `scripts/ci/statecharts_tlc.py --base a53f299560 --jobs 4`: PASS 19/19, 20.0초)

| cfg | Policy / Scenario / Mutant | 기대·실측 safety | 기대·실측 liveness |
|---|---|---|---|
| CurrentIncident | current / incident | - | **IdleEventuallyDelivered RED** |
| CurrentClearDrop | current / clearDrop (#8343) | **NoSilentLoss RED** | **IdleEventuallyDelivered RED** |
| CurrentRowOnly | current / rowOnly (#8372) | - | **OrphanEventuallyDelivered RED** |
| TargetIncident | target / incident | - | - (GREEN) |
| TargetRunning | target / running | - | - |
| TargetDraft | target / draft | - | - |
| TargetModal | target / modal | - | - |
| TargetFlicker | target / flicker | - | - |
| TargetLate | target / late | - | - |
| TargetRestart | target / restart | - | - |
| TargetClearDrop | target / clearDrop | - | - |
| TargetRowOnly | target / rowOnly | - | - |
| MutNoIdleRunning | target / running / NoIdle (Idle 조건 제거) | NoBoundOnBusySession RED | - |
| MutNoIdleDraft | target / draft / NoIdle | NoBoundOnBusySession RED | - |
| MutNoTranscript | target / running / NoTranscript (화면만 봄) | NoBoundOnBusySession RED | - |
| MutNoCleanup | target / incident / NoCleanup (정리 제거) | - | IdleEventuallyDelivered RED |
| MutNoDedupe | target / incident / NoDedupe (중복 방지 제거) | NoDuplicateDelivery RED | - |
| MutNoLedgerFence | target / late / NoLedgerFence | NoNewTurnClearedByOld RED | - |
| MutNoRebase | target / clearDrop / NoRebase (R1 제거) | - | IdleEventuallyDelivered RED |

요약: Current RED 3, Target GREEN 9, mutant RED 7. liveness `IdleEventuallyDelivered`는 수정 전 RED(incident, clearDrop), 수정 후 GREEN이다. `OrphanEventuallyDelivered`도 수정 전 RED, 수정 후 GREEN이다.

## 반례 (코드 재생 결과)
- **CurrentIncident**(11:48): owner drive → 보류 → held_watch 2초 깨움 → … → 상한 시각 뒤 재구동 ↔ 보류(Idle 연속 관측됨)를 영원히 반복한다. 코드: OI:249 → OB:53/70 → OB:24-30 Observe → held_watch.rs:65-71 Wake → 다시 OI:249. 로그 `busy_source_unavailable screen=Idle`이 2초 간격으로 반복된 것과 일치한다.
- **CurrentClearDrop**(#8343): 3단계로 `NoSilentLoss`가 깨진다. owner drive(Held, Readiness 기한 무장) → 기한 경과 → `refuse_readiness_front`로 front 버림, 안내는 입력을 특정하지 않는다(readiness_hold.rs:111-149, retained_notice.rs:32-35). liveness 반례(8단계)는 재현 2의 모양이다. 턴이 끝난 뒤 cold start 첫 키가 옛 기한을 물려받아 버려진다(readiness_hold.rs:183-201).
- **CurrentRowOnly**(#8372): 1단계다. owner drive → foreign_row 거절 → NotSent이지만 보류가 아니라 stalled다. 깨움이 없다(OI:1121, drive.rs:113-118).

## 차트
- [current-incident](human-hold-bound.current-incident.chart.mmd), [target-incident](human-hold-bound.target-incident.chart.mmd)
- [current-clear-drop](human-hold-bound.current-clear-drop.chart.mmd), [target-clear-drop](human-hold-bound.target-clear-drop.chart.mmd)
- [current-row-only](human-hold-bound.current-row-only.chart.mmd), [target-row-only](human-hold-bound.target-row-only.chart.mmd)

## 모델 밖(호출자 의무)
- witness 판독 4종(§4)은 추상화했다. 화면·transcript 두 근거의 실제 판독기가 Unknown을 Idle로 뭉개지 않는 것은 시험 T5가 맡는다.
- id 0 lease 거절(§6.2), SessionTransitionLease 경합, routine run 상태 판독은 모델에 없다.
- 재시작 뒤 boot가 lease를 재등록하는 경우만 넣었다(가장 나쁜 쪽). 행만 남는 재시작(C4 :215 건너뜀)은 rowOnly와 같은 모양이다.
