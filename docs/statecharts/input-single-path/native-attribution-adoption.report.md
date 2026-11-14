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

review.preserved는 현재 native facts 보존, known은 source/registration 채택을 추상화한다. UUID·namespace·full key·entry identity·범용 append의 충돌은 Rust fixture가 검증한다. 원래 결과 없는 PendingFixed에 종료를 요구하지 않으며, 시간으로 원본을 완료하지 않는다. control5종의 권한 commit은 아직 구현하지 않았다. 아래의 bridge/GC 계약은 별도로 검증하며 그 GREEN을 control 구현 증거로 쓰지 않는다.


## 승계 종결과 GC 보존 경계

`NativeSettled`는 exact TurnKey와 캡처한 actor 주소가 같은 ledger의 native registration에 속하는지 확인한다. native barrier가 종결되고 phase가 Finalized이며 미결 boot debt가 없을 때만 true다. bridge는 기존 completion-cleanup 토큰 또는 이 조회의 true를 Settled로 받는다. successor에게 권한을 넘긴 경우 토큰의 cleanup 플래그를 조작하지 않으며 successor lease·row·global_active·queue wake를 바꾸지 않는다. 불일치·actor 부재·조회 오류는 기존 Pending/Legacy 분류를 유지한다. 외부 턴의 body를 앞 episode 결과에 합치지 않는 기존 거절 경로를 사용한다.

GC는 native barrier가 미결이거나 boot input이 아직 settled가 아니면 일반 Finalized 표시와 TTL만으로 entry를 지우지 않는다. 시간은 GC 기존 조건으로만 남으며 native result나 cleanup을 합성하지 않는다. 단순 admission bool이 true여도 실제 boot debt가 남으면 보존한다. 일반 native debt가 없는 finalized entry의 기존 GC는 유지한다.

| cfg | 수정 전 | 수정 후 |
|---|---|---|
| NeutralTransferBefore/Fixed | 승계 종결 후 토큰 cleanup만 기다리는 bridge: NativePortEventuallyDone 위반 | exact ledger 종결로 중립 종료, successor 보존 |
| BootGcBefore/Fixed | 일반 terminal+GC가 미결 boot debt를 지움: NativeBootNoFabrication·NativeBootPendingPreserved 위반 | debt 보존, 결과 없는 입력의 성공/철회 합성 0 |

GC fixture는 실제 boot prepare→restore→reconcile→BootReady 입구를 실행한다. bridge fixture는 native registration→typed admission→finalizer drive→bridge state/body/neutral_exit를 실행하며 successor row/lease/counter를 그대로 확인한다. Cancel/Terminate/clear/stop/operator release의 실제 경쟁 및 직접 row 삭제 custody 접점은 이번 단계에서 실행하지 않았다. 이 경로에는 승인할 nonblocking native control commit leaf가 더 필요하다. 운영 enabled=false는 유지한다. native idle WARN·observer replay·lost wake는 사용자 범위 축소로 제외한다.

`native-attribution-b2b.chart.mmd`에 승계 후 exact ledger 중립 종료와 미결 boot debt GC 보존을 표시한다. 그림은 receipt event별 병합/직렬화와 source None→Some 입력 순서 조합을 숨기며, 해당 경계는 위 cfg 및 Rust fixture로 검증한다.
