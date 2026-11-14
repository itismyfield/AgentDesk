# Native input boot custody — 비활성 조각 1

기준 `f8c80664eae4955f477e51ffbf86d554eb3980d5`. 운영 `native_prepare::enabled()`는 false다. 이 조각은 저장 결과와 custody를 소비하는 boot 포트만 연결한다. O 공급·인접 control 종료·활성화·herdr e2e는 후속 승인 조각이다. 사용자 11-14 범위 축소: native idle WARN·새 ticket replay·lost-wake 감시는 제외한다.

## 소비 순서와 권한

`BootInputs::prepare`는 canonical registry와 기존 custody의 모든 native copy에서 전체 namespace/registration/settlement를 읽는다. complete manifest, safe copy path, digest, episode key, full ticket, source, AnyKey와 결과 proof를 검증한 뒤 기존 registry에 멱등 채택한다. namespace/key가 같은 다른 registration/결과는 transaction 오류이며 쓰지 않는다. 기존 Passive display compaction만 정규화하고 opaque extra는 보존한다. 미존재 registration은 passive retained로 채택하여 live reservation을 다시 열지 않는다.

이후 기존 `prepare_before_reap`가 원본을 custody에 보존한 다음 기존 reaper가 옛 row를 정리한다. `BootInputs::restore`는 먼저 actor에 미결 custody와 admission barrier를 등록하고, actor 밖에서 withdrawal 안내의 PG enqueue/dedupe를 처리한 다음 Settle을 보낸다. PG 실패에도 actor가 남아 intake를 막는다. 저장 결과가 없으면 결과/안내/cleanup을 합성하지 않는다.

old-generation constructor는 원래 generation/message/nonce를 유지한다. 현재 generation과 같거나 미래이면 거절한다. CancelToken이나 과거 monotonic Instant를 재생성하지 않으며 mailbox/global_active를 새로 잡거나 감소시키지 않는다. 살아 있는 같은 actor의 lease나 권한이 바뀐 row는 빌리지 않는다. 현재 row가 exact 원본이면 captured started/offset과 재확인 후 기존 snapshot CAS를 쓴다. 읽을 수 있는 후속 row/lease가 이겼으면 원본 cleanup debt만 닫고 successor row 삭제/wake를 하지 않는다.

cleanup ack 뒤에는 최신 full record를 기존 custody publication으로 다시 보존한다. 이것이 실패하면 actor admission은 계속 Pending이다. canonical registry가 나중에 없어져도 옛 미결 copy가 완료 결과를 되돌리거나 재주입하지 못한다. 이미 cleanup된 copy도 보존 성공을 확인한 뒤 admission을 연다. 결과 완료와 row 삭제 사이, row 삭제와 wake 사이 crash는 저장 결과의 재소비 및 기존 owner recover로 이어지며 새 event replay task를 만들지 않는다.

## 상태 차트 전후

| cfg 계열 | 수정 전 반례 | 수정 후 검사 |
|---|---|---|
| PortTransferUnreadable / PortTransferAbsentUnreadable | 기존 Transfer에는 unreadable 상태가 없어 가드 삭제가 생존 | unreadable→repair→종결, parentLease 2/0; `NativeAckReadable` 안전성, `NativePortEventuallyDone` 진행성 |
| BootSavedBefore / Fixed | durable 결과가 있는데 boot consumer 부재로 진행성 실패 | 결과 재생성 없이 cleanup 후 head 진행 |
| BootCustodyBefore / Fixed | canonical 부재에서 registration 미채택으로 진행성 실패 | full custody adoption 후 head 진행 |
| BootCorruptBefore / Fixed | 손상 상태에서 reaper가 row를 지워 안전성·진행성 실패 | 실제 자료 repair 후에만 reap/consume; 영구 손상에 무조건 진행성 주장 없음 |
| BootOldGenerationBefore / Fixed | 옛 generation의 결과 소비 경로가 없어 진행성 실패 | 원래 key 유지, lease remint 0, head 진행 |
| BootQueueBefore / Fixed | cleanup됐어도 기존 boot owner drive가 없으면 head 정체 | 기존 recover→wake→Drive의 공정 실행 하에 head 진행 |
| BootPendingFixed | 결과 없는 원본을 종결하면 안 됨 | 결과/cleanup/head 합성 0, lease remint 0; 미결 입력의 무조건 종결은 요구하지 않음 |

NativeInputAttribution·NativeInputPreparation 합계 57 cfg에서 기대 위반 집합과 실제 집합이 일치한다. `statecharts_tlc.py`는 의도한 Before 반례도 확인하여 전체 rc=0이다. unreadable 두 cfg의 `PortTransferred`에서 `~review.failed`를 지우면 `NativeAckReadable` 위반이 발생한다. 모델 변수는 기존 값만 재사용한다. boot의 generation=2와 parentLease=0은 새 실행이 옛 lease를 재생성하지 않는 코드 계약이다.

## 실행 경계

운영 bootstrap는 enabled일 때만 prepare→기존 reaper→restore를 실행하며 준비·보존 오류면 intake를 열기 전에 돌아간다. owner recover와 start_waits도 같은 actor-local boot admission을 조회한다. enabled=false에서는 기존 reaper와 recover 경로를 그대로 실행한다. readonly admission query는 actor를 기다릴 뿐 PG를 기다리지 않는다.

이 조각의 fixture는 실제 registry/custody/reaper/finalizer와 기존 V2 owner Drive를 통과한다. 실제 Discord 송신·herdr·운영 transcript 조회는 없다. 새 source 증거 공급과 Cancel/Terminate/clear/stop/operator-release의 native 결과 채택은 아직 구현하지 않았으므로 활성화 근거로 삼지 않는다.

부팅 채택은 source가 있는 registration에서 기존 Frame::bind로 passive 인식 entry만 복원한다. live slot/lease/UUID receipt는 만들지 않는다. 기존 entry가 있으면 receipts를 보존하며, 없는 entry는 기존 receipt 진입점이 실제 row를 확인할 수 있게 한다. entry=None인 채 same-source bind가 조기 반환하는 반례는 실제 Registry::receipt RED→GREEN 및 entry 생성/보수 제거 mutant로 검증한다. source 없는 cold 입력은 기존 bind가 나중에 entry를 만들도록 유지한다. 모델에서는 기존 NativePreparationReceiptBefore/Fixed의 receipt/version 행동이며 새 source 읽기·전이/observer를 추가하지 않는다.
