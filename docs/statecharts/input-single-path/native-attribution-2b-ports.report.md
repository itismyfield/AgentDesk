# Native attribution — B2b-2 연결 경계

기준 45861072146d2fb87044d880db9ca6783615a8ee. 승인된 ①(2a 네 연결 행)까지 구현한다. ② B2b-1 P2 수리 및 ③ boot/인접 종료/e2e는 미구현이다. 기본 producer와 bridge 활성화는 native_prepare::enabled=false로 유지한다. 새 O observer/새 ticket replay가 범위 밖인 상태에서 ticket만 발행하면 완료 증거를 공급할 운영 경로가 없고, boot custody 소비와 인접 종료도 아직 없다.

## 연결

| 경계 | 현재 구현 |
|---|---|
| producer/cold/created | 명시 primary intent와 follower를 기존 input owner/pump로 운반한다. scoped ticket의 finalizer 등록 ack를 leaf 실행 전에 받는다. |
| AnyKey | 기존 scoped Arc의 keyed 상태를 정상 종료와 unwind 전에 읽는다. true만 receipt를 기록한다. false는 준비 custody만 해제하며 원본을 완료/철회하지 않는다. |
| actor-local custody/admission | ledger에 full ticket 준비를 보관한다. primary에 독립 native barrier를 두며 delivery projection/disposition을 완료로 위조하지 않는다. |
| runtime witness | native row 제거는 기존 inflight 파일 잠금 안에서 hosted execution nonce를 비교한다. 일반 clear의 비교 규칙은 바꾸지 않는다. |
| bridge | exact episode + 살아 있는 동일 actor Arc에만 중립 종료/응답 억제 포트를 제공한다. 기본 호출은 비활성 경계로 단락된다. |

CompletionAdmissionPlan 전체 흡수 대신 drive를 유지한다. 해당 plan은 delivery 관문이며 durable 결과/row CAS/custody 저장을 대체하지 않는다. Pending 승계 종결(P2-3)과 PG await(P2-5)는 ②에 남는다. NativeResult actor의 운영 활성화는 하지 않는다.

## 상태 차트

| cfg | 수정 전 RED / 수정 후 GREEN | 코드 경계 |
|---|---|---|
| PortPreflightBefore / Fixed | release 후 row 선확인 실패로 liveness 위반 / 선확인 후 lease 유지·repair로 GREEN | 기존 25d22a24 preflight를 모델에 반영 |
| PortTransferBefore / Fixed | foreign lease/row에서 debt 영구 Pending / 권한 승계 증명 후 debt-only 종료로 GREEN | ② 목표, production 미구현 |
| PortTransferDeletes | 승계 종료에서 foreign row 삭제 시 safety RED | ② 목표 반례 |
| NativeInputPreparation 기존 cfg | no-key/explicit primary/stamp/receipt/FIFO/가입 계약 유지 | 기존 B1·2a 계약 변경0 |

42cfg 전체를 저장소 TLC runner로 실행해 기대 위반 집합과 대조한다. 모델 GREEN만으로 production 구현을 PASS라 하지 않는다. NoWake/FixedBootLegacy/Unavailable과 WarnIdle은 과거 모델이며 사용자 11-14 범위 축소 대상의 구현 증거에서 제외한다. README는 착지 union 정리 지시로 변경하지 않는다.

## 정지선과 잔여

①의 신규 production 실측은 약 +508이고 종전 네 행 추정 +285~420의 중간값 대비 1.441배다. 기준 25d22a24부터의 조각은 약 +653이다. ② 가드·panic·승계·PG 대기·dedupe +70~90, ③ 결과/row 복원 +85~105·custody reader/adoption +70~90·boot admission/wake +40~55·인접 종료 +80~105로 기본 +345~445가 남는다. 관측 비율을 적용하면 잔여 +497~641, 조각 예상 합계 +1150~1294여서 +1100 정지선을 넘는다. 압축하지 않고 ① 경계에서 PLAN_READY로 멈춘다. 최종 공식 실측은 외부 레인 보고서에 남긴다.

② P2 수리와 ③ boot/인접/e2e를 별도 리뷰 조각으로 제안한다. ③ 안에서도 실제 증거 producer를 어떻게 공급할지는 사용자 범위 축소와 맞춰 결정해야 한다. 새 observer/replay를 임의로 복원하지 않는다. native idle WARN은 제외, herdr e2e는 ③ NOT_RUN이다.
