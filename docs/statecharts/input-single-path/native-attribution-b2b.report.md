# Native input B2b-1 — 비활성 결과/정리 기반

기준 `8698df14b76e589d29e3b6145d9cdc8724aad4b9`(B2a). B1 및 B2a의 귀속/admission 계약은 변경하지 않는다. 새 NativePrepared/NativeCleanup 및 finalizer Native 메시지는 `cfg(all(test, unix))`로 비활성이다. 운영 producer, native actor 활성화, bridge 중립 종료, boot 소비 및 WARN은 연결하지 않았다. 기존 Start 처리만 동등한 actor_state leaf로 옮겼다.

## +1100 정지선과 분할

공식 측정기의 staged tree 실측 prod4파일/+436(공백·주석·시험 제외)에서 생산 코드 추가를 멈췄다. 정지 후 변경은 pre-key current-generation 거절 1줄 보강·회귀·검증 수리와 문서다. 아래 잔여는 B2a 보고서의 각 행을 그대로 쓰며, core에 포함되지 않은 기존 actor ledger/admission 연결을 별도로 남긴다.

| 잔여 | 순 코드 추정 | 코드에 남은 연결 |
|---|---:|---|
| 명시 primary/follower 운영 producer | +95~140 | owner_input::{submit,created,run}, pump::submit_native_front, first_input의 첫-key callback에서 prepare/AnyKey를 운반 |
| actor ledger·기존 cleanup admission | +35~55 | Native 메시지를 production으로 켜고 exact ledger phase/release 및 CompletionAdmissionPlan과 연결. 현재 cfg(test) 경로의 직접 QueueEligible는 활성화 계약이 아님 |
| bridge 중립 종료·자기 응답 범위 | +95~140 | stream_loop/stream_tick의 NativeSettled, post_loop_finalize 거절, legacy terminal 거절과 own membership 이후 response cutoff |
| boot/custody-only·incarnation | +130~185 | boot_custody preserve 뒤 native record 소비, old generation 별도 admission, owner 재구성과 restore/wake |
| Cancel/Terminate/clear/stop/release 경쟁 | +100~160 | d1 전수표의 기존 종료 진입점을 full ticket outcome CAS와 연결 |
| native idle WARN1 | +65~90 | warned의 영속 once 및 clock 경계, 증거 실패 시 Pending 유지 |
| 관측→actor·유실 wake | +40~65 | activity의 새 ticket full replay, current stamp를 재확인한 비동기 제출, 복구 wake |
| **잔여** | **+560~835, 중간값 +697.5** | e2e 및 시험 코드는 제외 |

**436 + 697.5 = 1133.5 > 1100.** B2b-1은 현재 비활성 기반의 검증 경계, B2b-2는 위 모든 활성화/복구와 isolated herdr e2e를 함께 닫는 경계로 제안한다. 이 레그에서 상한·착지 정책을 바꾸지 않는다. A+B1+B2a+B2b 전체 함께 착지 조건을 유지한다.

## 상태 차트와 코드 대조

기존 NativeInputAttribution20cfg 및 NativeInputPreparation12cfg는 그대로 검사한다. NativeInputAttribution에 bounded DebtSpec refinement 세 cfg를 추가했다. 기존 모델의 Settle→ProjectionClear→Kick을 코드에서 결과 저장→exact actor/nonce/cutoff release→snapshot row CAS→cleanup ack→wake에 대응시킨다. row의 `Missing` 중 `not_found`만 통과시키고 읽기/파싱 오류는 debt로 남긴다.

| 반례 | 수정 전/뮤턴트 RED | 수정 뒤 GREEN의 범위 |
|---|---|---|
| 손상 row를 없음으로 취급해 wake | DebtBefore: NativeRowBarrier 및 NativeDebtEventuallyDrained 위반 | DebtFixed: row 유지→복구→CAS→wake, 위반0 |
| 결과 뒤 release는 됐지만 row 재시도 없음 | DebtNoRetry: NativeDebtEventuallyDrained 위반. 기존 claim_normal_episode는 released lease의 strict actor를 다시 claim하지 못함 | 새 Native 메시지에 동일 작업 재제출→결과 idempotent, counter 두 번 차감0, row 정리 후 wake1 |
| no-key 원본을 귀속 후보로 종결 | 기존 NativeInputPreparation의 Before/Fixed 및 새 Rust pre-key/AnyKey 검사 | marker가 없으면 admission 거절, 결과0·row 불변 |

35cfg 전체 rc0, expected violation 집합 일치(104.6초). 원시 `b2b-tlc.txt`, 바이트 목록 `b2b-tlc-source-sha.json`. DebtSpec은 파일 IO 실패가 회복된다는 fairness만 가정한다. host/Discord 도착이나 운영 boot/wake를 증명하지 않는다.

기존 event 유실 보충은 `runtime_bootstrap.rs::complete_active_reconcile`→`v2_channels`→`session_transition::wake_installed_owner`다. 이미 설치된 V2 owner만 깨운다. `input_owner/recovery::recover`의 tick/requeue/history projection/notice drain은 custody-only ticket을 owner로 재구성하지 않는다. `boot_reaper::prepare_before_reap`→`boot_custody::preserve_native_before_boot_reap`는 준비/보존까지만 하므로 모델 BootRestore GREEN을 운영 boot 소비 PASS로 세지 않는다.

## 구현 경계

- Registry는 full registration/ticket 일치로 AnyKey receipt와 cleanup ack를 저장한다. cleanup ack는 동일 outcome과 철회 안내 선행을 요구한다. V3의 기존 flatten 확장 공간을 사용하며 V1/V2 예외를 넓히지 않는다.
- NativePrepared는 key 전에 명시 episode·현재 generation·actor Arc·monotonic cutoff를 capture한다. follower는 actor를 빌릴 수 없다. source None→검증된 binding 외의 ticket 변경은 거절한다. AnyKey marker가 없으면 native 후보가 있어도 완료로 바꾸지 않는다.
- NativeCleanup은 admission의 같은 capture 증거를 먼저 저장하고 exact lease CAS를 호출한다. lease가 이미 없는 정리 재제출은 row debt만 처리한다. 후속 actor가 있으면 거절한다. row는 provider/channel/episode/generation/session/path/execution nonce와 captured start/offset을 확인하고 기존 lock-held snapshot CAS로 지운다.
- 철회 안내는 full namespace/ticket의 persistent outbox dedupe→ack 순서다. follower의 lease/row/global_active/queue wake 권한은 없다. Native release는 generic cancelled를 켜지 않는다.
- Native 메시지는 기존 finalizer actor에서 직렬 실행하고 panic을 Err로 돌려 durable debt를 보존한다. 현재는 비활성 검증 경계이며 기존 ledger/admission 및 production observer와의 연결은 B2b-2다.

## DoD·운영 활성화 잔여

큐 fixture 회귀와 pre-key/no-key, partial cleanup, follower 안내는 실제 lib actor/store/outbox 진입점으로 검증한다. 운영 producer→O→actor→후속 head의 DoD는 아직 미완료다. withdraw 전체 진행, WARN1, herdr relay e2e는 NOT_RUN이며 PASS로 세지 않는다. e2e는 실행 전 외부 레인 보고서에 host/socket/root/channel/cleanup을 고정했으나 이번 정지 경계에서 서버를 띄우지 않았다.

Cancel·stop(mailbox_finish/owned_input_stop), Terminate(HostTerminated/row_retirement), clear(clear_channel/checked_purge/execution_cleanup), operator release(turn_lease::OperatorRelease::claim), boot/circuit_removal/rebind_reap는 운영 변경0이다. 전체 목록은 [d1 전수표](native-attribution.entrypoints.md)에 유지한다. 새 수동 명령·kill·전역 guard·시간 자동 종결은 없다.

[정리 상태도](native-attribution-b2b.chart.mmd)는 Prepared→Result→Released→RowGone→Admitted→Wake를 보여 준다. source/receipt/FIFO 상세 분기, boot 및 인접 종료는 기존 모델/전수표로 남기고 이 작은 도식에서 숨겼다. native idle WARN이나 운영 liveness를 이 도식의 종료 상태로 추론하지 않는다.
