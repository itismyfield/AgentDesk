# HumanHoldBound — #8430 사람 입력 보류 상한 (w2)

Code SHA: `a53f299560` (Current). Target = [설계 w2](../../design/8430-human-hold-bound.design.md).
Model: [`HumanHoldBound.tla`](HumanHoldBound.tla), view [`human-hold-bound.view.mjs`](human-hold-bound.view.mjs).

## 무엇을 보나
사람 owner front 하나가 다른 턴의 남은 lease(`cancel_token`)나 inflight 행 뒤에 보류된 모양이다. `HerdrBusyInput`의 `held` 상태(그 모델의 RetryHeld는 Eligible일 때만 돈다)를 이번 모양으로 세분한다. 기존 다른 모델·cfg는 바꾸지 않았다.

변수의 값 범위는 설계 문서 §2 진입점 전수표에서 가져왔다.
- `token` / `row` ∈ {old, new, none}, `rowKind` ∈ {managed, external}: C1·C7·C25·C35·C37·E40
- `screen` ∈ {idle, working, draft, modal, imageDraft, unknown, none}: E18–E20, §4(Codex 공통 화면 gate의 `attachments`)
- `transcript` ∈ {open, ended, none}: §4 둘째 근거
- `run` ∈ {none, terminal}, `producer` ∈ {running, ended}: §6.5 R5b
- `due` ∈ {none, armed, expired}: E9·E27–E29, R1·R2
- `front` ∈ {queued, held, stalled, sent, dropped, done}, `unit` 1..2: E24·E33·D1–D2·R3
- `ledger`, `settle` ∈ {none, mute, reacted}: C13·C14·R5c
- `kick`(QueueEligible), `tickOwed`(owner deadline tick): §6.1 7번, R5c
- `clock` 0..2와 `streak` 0..2: §5·§4(추상 3분, 연속 3회)

## 규칙
- INVARIANT `NoDuplicateDelivery`, `NoSilentLoss`(버린 unit은 모든 member를 특정한 안내), `NoBoundOnBusySession`(작업 중·초안·모달·이미지 초안·관측 실패·producer 생존 화면에서 발동 0), `NoNewTurnClearedByOld`, `BoundSettlesMute`(상한 정산은 완료 반응을 내지 않음), `TypeOK`
- INVARIANT `R1NoDueWhileLeased`(Target cfg만): 다른 턴의 lease가 있는 동안 Readiness 기한이 없다. R1을 직접 단언하는 검증 가드다.
- PROPERTY `IdleEventuallyDelivered`: `<>[](screen = idle) => <>Delivered`
- PROPERTY `OrphanEventuallyDelivered`(headless 행 전용): `<>Delivered`

## w1 → w2에서 바뀐 것
- P2-1: Target Drive는 토큰이 있으면 기한을 만들지 않는다(R1). w1의 두 보완 장치(DueDrop의 토큰 예외, OldTerminal의 기한 초기화)를 지웠다. `MutNoRebase`는 `MutNoR1`(기한 생성 금지만 제거)로 바꿨다. w1 모델에 `R1NoDueWhileLeased`를 붙이면 2 states에서 RED다(Init→Drive, token=old·due=armed). 리뷰어 재현과 일치한다.
- P1-1: `screen = imageDraft`와 mutant `NoScreenGate`(화면 gate 삭제, judge_in만 봄)·`NoAttachments`(attachments 조건 삭제)를 넣었다.
- P2-2: headless 행 witness = 토큰 없음 ∧ run terminal ∧ producer 종료. mutant `RunOnly`.
- P2-3: lease 모양의 깨움은 QueueEligible `kick`이다. 행 모양은 `MailboxReleased`가 없으므로 owner deadline tick(`tickOwed`)으로 깨운다. mutant `NoWake`·`GateTimeout`.
- P2-4: `rowKind = external` 행(watcher 재획득)은 Current에서 hand_busy 보류 루프에 빠진다. Target은 R5a로 다룬다. mutant `ManagedOnly`.
- P2-5: `unit = 2`이고, `notice = first`는 위반이다. mutant `FirstOnly`. R2용 `idleDue` 시나리오를 넣었다.

## 결과 (TLC 1.7.4, `scripts/ci/statecharts_tlc.py --base a53f299560 --jobs 4`: PASS 32/32, 30.0초)

### 바뀐 cfg의 수정 전(w1)/후(w2)
| cfg | w1 safety / liveness | w2 safety / liveness | 바뀐 점 |
|---|---|---|---|
| CurrentIncident | - / IdleEventuallyDelivered | - / IdleEventuallyDelivered | BoundSettlesMute 추가 |
| CurrentClearDrop | NoSilentLoss / IdleEventuallyDelivered | NoSilentLoss / IdleEventuallyDelivered | unit 2, 추가 불변식 |
| CurrentRowOnly | - / OrphanEventuallyDelivered | - / OrphanEventuallyDelivered | 추가 불변식 |
| TargetIncident·Running·Draft·Modal·Flicker·Late·Restart·RowOnly | - / - | - / - | `R1NoDueWhileLeased`·`BoundSettlesMute` 가드 추가, 깨움은 kick/tick |
| TargetClearDrop | - / -(w1 보완 장치 2개로 GREEN, 64 states) | - / -(R1 그대로, 가드 포함) | P2-1 |
| MutNoIdleRunning·MutNoIdleDraft·MutNoTranscript | NoBoundOnBusySession / - | 같음 | – |
| MutNoCleanup | - / IdleEventuallyDelivered | 같음 | – |
| MutNoDedupe | NoDuplicateDelivery / - | 같음 | – |
| MutNoLedgerFence | NoNewTurnClearedByOld / - | 같음 | – |
| MutNoRebase → **MutNoR1** | - / IdleEventuallyDelivered(턴 경계 초기화 제거) | **R1NoDueWhileLeased** / IdleEventuallyDelivered(기한 생성 금지 제거) | 이름·검증 대상 교체 |

### 새 cfg (w2)
| cfg | safety | liveness |
|---|---|---|
| CurrentIdleDue (R2, 초안 + idle 기한) | **NoSilentLoss RED** | - |
| CurrentRowPane (watcher 재획득 행 + pane Idle) | - | **IdleEventuallyDelivered RED** |
| TargetImageDraft·TargetIdleDue·TargetRowPane·TargetRunLive | - | - (GREEN) |
| MutNoScreenGate (draft) | NoBoundOnBusySession RED | - |
| MutNoAttachments (imageDraft) | NoBoundOnBusySession RED | - |
| MutFirstOnly (idleDue) | NoSilentLoss RED | - |
| MutRunOnly (runLive) | NoBoundOnBusySession RED | - |
| MutNoWake (rowOnly) | - | OrphanEventuallyDelivered RED |
| MutGateTimeout (rowOnly) | BoundSettlesMute RED | - |
| MutManagedOnly (rowPane) | - | IdleEventuallyDelivered RED |

요약: **Current RED 5, Target GREEN 13, mutant RED 14**(32 cfg). liveness `IdleEventuallyDelivered`는 수정 전 RED(incident, clearDrop, rowPane), 수정 후 GREEN이다. `OrphanEventuallyDelivered`도 수정 전 RED(rowOnly), 수정 후 GREEN이다.

## 반례 (코드 재생 결과)
- **CurrentIncident**(11:48): owner drive → 보류 → held_watch 2초 깨움 → … → 상한 시각 뒤 재구동 ↔ 보류를 영원히 반복한다(OI:249 → OB:53/70 → OB:24-30 Observe → held_watch.rs:65-71 Wake).
- **CurrentClearDrop**(#8343): 3단계다. owner drive(Readiness 기한 있음) → 기한 경과 → 기한 만료로 front 버림, 안내가 입력을 특정하지 않는다(readiness_hold.rs:111-149, retained_notice.rs:32-35).
- **CurrentRowOnly**(#8372): 1단계다. owner drive → foreign_row → stalled(OI:1121, drive.rs:113-118).
- **CurrentRowPane**: owner drive가 non-Managed 행으로 active=true(turn_deliver.rs:128) → hand_busy 보류가 영원히 반복된다.
- **CurrentIdleDue**: idle 경로 초안 보류 → 10초 기한 → 특정되지 않는 안내로 unit 버림.

## 차트
- [current-incident](human-hold-bound.current-incident.chart.mmd), [target-incident](human-hold-bound.target-incident.chart.mmd)
- [current-clear-drop](human-hold-bound.current-clear-drop.chart.mmd), [target-clear-drop](human-hold-bound.target-clear-drop.chart.mmd)
- [current-row-only](human-hold-bound.current-row-only.chart.mmd), [target-row-only](human-hold-bound.target-row-only.chart.mmd), [target-row-pane](human-hold-bound.target-row-pane.chart.mmd)

## 모델 밖(호출자 의무, 시험이 맡음)
- `BoundFire`는 토큰 제거·행 제거·원장 Finalized를 한 전이로 처리한다. 정리 도중 다른 진입점이 lease를 만드는 경합(T11)과, 이미 Finalized인데 토큰이 남은 초기 상태(재발 (b))는 직접 검사하지 않는다. 이 둘은 실제 CAS(`release_turn_lease_if_matches`, OperatorRelease 재개방)와 시험 T11·T1이 맡는다.
- witness 판독 4종(§4)과 Codex 공통 화면 gate는 추상화했다. 판독기가 Unknown을 Idle로 뭉개지 않는 것은 T2c·T5가 맡는다.
- id 0 lease 거절, SessionTransitionLease 경합, actor incarnation 교체(T10e), routine run 결합 조회는 모델에 없다.
