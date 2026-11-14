# Native custody adoption — receipt/source 보존

기준 `4972a890ef2d6f2681a63131d12d06ffd7cf0f1d`. 운영 enabled=false 유지. 조각1 리뷰의 두 데이터 보존 계약을 보강한다.

`BootRecord`와 `RestoredRecord`는 원래 namespace/settlement/registration에 연결된 optional entry를 보존한다. entry의 immutable identity는 full key의 attempt nonce·frame·source로 만든 entry와 비교한다. 실제 event의 key/UUID/kind는 단조 병합하고 다른 namespace/key가 같은 source event를 차지하면 오류다. 기존 receipt는 지우지 않는다. 새 UUID는 만들지 않는다. payload display의 기존 compaction만 정규화한다.

legacy custody의 entry 필드는 optional이다. canonical에 receipt가 있으면 구형 copy가 그 사실을 지우지 못한다. canonical도 entry도 없으면 관측한 receipt를 합성하지 않으며 원래의 보수적인 recognition 상태를 유지한다. cleanup_complete를 일괄 collision 예외로 삼지 않는다. `collision::rival`의 기존 enqueue/twin 계약은 변경하지 않는다. source 재독·새 observer/replay를 추가하지 않는다.

source는 None→Some만 단조 확정한다. provider/target/incarnation과 existing/pre_source 기대값을 비교하며, current Some을 오래된 None이 지우지 못한다. 다른 Some은 오류다. registration.source와 native ticket.source를 함께 채택하고 outcome·cleanup·notice·extra를 보존한다. cold planned registration이 bound custody를 채택하면 live slot을 새로 열지 않고 passive entry로 보존한다.

## 상태 차트

NativeInputAttribution의 기존 boot 소비 전이에 receipt 보존과 source 확정의 before/fixed를 추가했다. 기존 NativeInputPreparation의 receipt/version·binding 계약은 유지한다.

| cfg | 수정 전 | 수정 후 |
|---|---|---|
| CustodyReceiptBefore/Fixed | A cleanup 뒤 receipt가 없어 동일 frame B head가 진행하지 못함: NativeBootEventuallyDone | 실제 receipt 사실을 보존하여 B 진행 |
| CustodySourceBefore/Fixed | 정상 None/Some 기록을 재채택할 때 충돌을 계속 재현: NativeBootEventuallyDone | 기대값을 만족하는 단조 source 채택 후 진행 |

review.preserved는 현재 native facts 보존, known은 source/registration 채택을 추상화한다. UUID·namespace·full key·entry identity·범용 append의 충돌은 Rust fixture가 검증한다. 원래 결과 없는 PendingFixed에 종료를 요구하지 않으며, 시간으로 원본을 완료하지 않는다. control5종의 권한 commit/bridge/GC 확장은 별도 범위이며 이 표의 GREEN을 그 구현 증거로 쓰지 않는다.
