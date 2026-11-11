# outbound-port: intent → reservation → nonce → 게시 → receipt

코드 SHA `92ef5b00b205679965c24c5c017f7237e8b5d57f`. 모델 `OutboundPort.tla`.
Discord outbound 값 계약(dedup key·결과·receipt·port)을 Discord 모듈 밖으로 옮길 때,
dedup·nonce 권한과 receipt 기록 순서가 그대로인지 확인한다. `Conv`는 dedup key를 어느 쪽 ID로 만드는지만 바꾼다.

## 진입점과 값 범위

| 모델 값 | 범위 | 코드 |
|---|---|---|
| 예약 결과 | Reserved / Duplicate / InFlight | `src/services/discord/outbound/transport.rs:188,192,195,204` |
| 대기 결과 | Delivered / Released / TimedOut(→Skip) | `transport.rs:308,309,317`; `delivery.rs:98,114` |
| receipt 기록 | 게시 성공 뒤에만 `record` | `delivery.rs:277,329,475,641`; `transport.rs:168,271` |
| 예약 해제 | 실패·취소·Drop | `transport.rs:219,285`; `delivery.rs:659` |
| nonce | 없음 / 강제(`enforce_nonce=true`) 단일 메시지만, split이면 거절 | `delivery.rs:201,204,520,542`; 생산자 `gateway.rs:320`, `manual_delivery.rs:547,612`, `serenity_reference.rs:50` |
| 전송 실패 분류 | transient(응답 없음·5xx·429·408) / permanent | `dispatches/discord_delivery/transport.rs:77`; `delivery.rs:570` |
| 재시작 | 프로세스 메모리 deduper 소실 | `outbound/mod.rs` `shared_outbound_deduper` |

## 규칙

- `NoDuplicatePost`: 같은 intent의 Discord 게시는 1번 이하.
- `NoSilentLoss`: 요청이 모두 끝났는데 게시가 없으면 적어도 한 요청이 transient/permanent 실패를 들고 있다.
- `NoSuccessWithoutReceipt`: sent/duplicate는 자기 intent의 실제 게시를 가리킨다.
- `NoPostWithoutReservation`: 모든 게시 시도는 그 key의 예약을 쥔 요청만 한다(nonce가 중복을 가려도 잡는다).
- `EveryRequestSettles`(PROPERTY): 모든 요청이 끝난다.

## 결과 (plain TLC, `scripts/ci/statecharts_tlc.py`)

| cfg | Conv / Nonce / 응답 없는 생성 / 변형 / 재시작 | 깨진 규칙 |
|---|---|---|
| Before | serenity / enforced / 있음 / 없음 / 1 | - |
| After | domain / enforced / 있음 / 없음 / 1 | - |
| AfterInProcess | domain / none / 없음 / 없음 / 0 | - |
| AfterNoNonceAmbiguous | domain / none / 있음 / 없음 / 0 | NoDuplicatePost |
| AfterNoNonceRestart | domain / none / 없음 / 없음 / 1 | NoDuplicatePost |
| LossyKey | lossy / enforced / 있음 / 없음 / 1 | NoSilentLoss, NoSuccessWithoutReceipt |
| RecordBeforePost | domain / enforced / 없음 / 게시 전 기록 / 0 | NoSilentLoss, NoSuccessWithoutReceipt |
| NoReserve | domain / none / 없음 / 예약 생략 / 0 | NoDuplicatePost, NoPostWithoutReservation |
| NoReserveNonce | domain / enforced / 없음 / 예약 생략 / 0 | NoPostWithoutReservation |

`EveryRequestSettles`는 모든 cfg에서 성립한다.
NoNonce 두 cfg는 현재 코드의 알려진 한계다. 메모리 deduper는 재시작·재시도를 넘지 못하고, 그 경로의 중복 방지는 durable outbox와 nonce가 맡는다.
LossyKey는 Serenity ID → 값 ID 변환이 thread parent를 버리면 다른 intent가 남의 receipt를 받는다는 것을 보인다. 변환은 단사여야 한다.

## 모델 밖

- idempotency window 0(`without_idempotency`) 생산자: dedup을 호출자가 맡으며 이번 이동과 무관.
- Discord nonce 재생 창의 시간 한계: 외부 계약이며 이번 이동이 바꾸지 않는다.
- split/compact/파일 fallback·thread parent 재시도: 같은 예약 안의 전송 변형이며 receipt 기록 위치는 같다.
- durable `message_outbox`·delivery_record·receipt_index: 별도 소유자이며 이번 이동이 건드리지 않는다.
- 취소(`CancelToken`): 호출자 의사에 따른 Skip이며 손실 규칙 대상이 아니다.

## caller 흐름 (`OutboundCallerFlow.tla`)

코드 SHA `ae9e46fdfe114591d20bce6abec816c0467f15d4`. routine 로그·이슈 공지·dispatch 완료 요약 caller가 결과를 어떻게 다루는지(재시도·fallback·관측 순서)와,
send가 어느 deduper를 쓰는지를 본다. `Entry`: caller가 공유 deduper를 넘김(이동 전) / adapter port가 묶음(이동 후) / caller가 새 deduper를 만듦(변형).

| 모델 값 | 범위 | 코드 |
|---|---|---|
| routine edit 결과 | ok / transient(→Err) / permanent·missing(→replacement send) / skip(→Err) | `src/services/routines/discord_log.rs:574-622` |
| routine send | window 0(예약 없음), 결과 → Ok/Err | `discord_log.rs:572,626-650` |
| 이슈 공지 send | review_notification(예약), Skip → Err | `src/services/issue_announcements.rs:393-420` |
| dispatch 요약 send | review_notification(예약), Duplicate·Skip → Ok(()) | `src/services/dispatches/outbox_route.rs:491-515` |
| 관측 | 매 raw HTTP 시도 begin/result 기록 뒤 반환 | `src/services/discord/outbound/raw_output.rs:136,153,190-200` |

규칙: `ReplacementOnlyAfterAuthoritativeEditFailure`, `ObservedBeforeOutcome`, `NoDuplicatePost`, `OkHasReceipt`, `SettledOutcome`, PROPERTY `EveryRequestSettles`.

| cfg | 깨진 규칙 |
|---|---|
| RoutineBefore / RoutineAfter | - |
| IssueBefore / IssueAfter | - |
| OutboxBefore / OutboxAfter | - |
| RoutineFallbackOnTransient | ReplacementOnlyAfterAuthoritativeEditFailure |
| RoutineObserveAfter | ObservedBeforeOutcome |
| OutboxFreshDeduper | NoDuplicatePost |

dispatch 요약의 Skip→Ok(())는 기존 동작이다. 같은 key의 예약을 쥔 다른 요청이 실패를 들고 재시도 권한을 가진다(`OutboundPort` NoSilentLoss). 모델은 이를 `ok_skip`으로 따로 둔다.
모델 밖: 이슈 공지의 edit는 deduper를 거치지 않는 직접 edit이고 실패를 HTTP 상태와 함께 돌려준다(fallback 없음). routine 로그의 thread 생성·HealthRegistry 경로, durable 재시도 주기는 이번 이동과 무관하다.
