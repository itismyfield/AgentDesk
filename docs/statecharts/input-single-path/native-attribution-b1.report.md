# B1 — native 입력 증거와 pre-key intent

기준은 A 리뷰 head `6acfb9f6d88ff1a4304a0d8bcbb5ff1d3ab5d619`, 재개 head는 `245283f242b56ba75ea664142c3e7391a6596923`이다. P2 수리를 포함하여 측정한다. A+B1+B2 함께 착지하며 B1은 native 완료를 활성화하지 않는다.

## 실제 경계

| 포트 | B1 동작 | B2 경계 |
|---|---|---|
| `InputFacts::observe_native/poll/native_evidence` | 기존 O capture가 이미 해석한 완전한 줄과 그 줄의 Open/strict Close를 같은 순서로 읽는다. 첫 record 전에 명시적으로 켜며 prefix 없는 resume/실패한 capture는 거절한다. 기본 reader에는 버퍼를 만들지 않는다. | 기존 observation worker·tail factory에 연결. 원본 transcript를 별도 scanner로 다시 읽지 않는다. |
| `input_facts/native.rs::fold` | 모든 외부 enqueue도 FIFO에 포함. dequeue→native user, human command/absorption 선후·생략, remove/popAll/transfer, 늦은 dequeue를 구분한다. source·attempt·유일한 receipt/등록을 결합한다. twins/충돌/불명확한 순서는 Pending이다. | 이 read-only 후보를 persisted outcome으로 승격할 typed actor. |
| `Registry::native_evidence_registrations` | namespace/전체 ticket/class hash/기존 native row receipt를 한 번 읽는다. 기존 history wrapper 규칙을 재사용한다. positive no-key로 abandoned 된 등록, 이미 결과가 있는 등록, cleanup 완료 등록은 새 귀속 후보가 아니다. | 저장된 outcome/cleanup debt 소비는 별도 경로. boot custody는 기존 A처럼 전체 기록을 보존한다. |
| `pump::submit_native_front` → `Owner::native_intent` | explicit Primary(원본 id/nonce/generation) 또는 Follower를 기존 payload attribution에 실어 durable reserve 뒤 첫-key leaf 전에 검사한다. 잘못된 primary는 NotSent이며 unarmed boot restore가 원본을 돌려놓는다. | 성공한 lease claim의 캡처를 넘길 실제 owner_input caller. 현재 mailbox 조회로 Primary를 합성하지 않는다. |
| RuntimeReady·unknown restore | 기존 `prepare_history/bind_source`와 queue 복원이 intent를 보존한다. merged frame에서도 episode id는 head 원본과 같아야 한다. | 첫 native ticket/v3 저장 및 운영 writer 활성화. B1 테스트의 일반 frame 등록은 envelope2다. |

모든 새 native 증거/intent API는 `cfg(test)` 경계에 있다. 운영 경로에는 동일한 attribution을 전달하는 pump 분리만 적용한다. native 결과 기록·episode/actor CAS·row 삭제·wake·boot 소비·WARN·실제 herdr e2e는 B2다. `turn_bridge` 본문과 다른 레인 소유 파일은 수정하지 않는다.

증거는 namespace, 전체 ticket, 원래 run id/시작 위치, membership row/범위, strict closer 범위와 disposition을 운반한다. 응답 범위는 membership 이후이며 앞 외부 턴 본문을 포함하지 않는다. end_turn만으로 완료하지 않고 연속 closer는 한 종료다. native disposition은 배송 receipt가 아니다.

이 단계의 opt-in prefix 자료는 비활성 재생 기반이다. B2는 이를 pending ticket의 관측 수명과 기존 worker에 연결하고, 장기 관측의 자료 보존을 새 무제한 journal이나 최근-N exactly-once 캐시로 바꾸지 않아야 한다. opener/membership을 인증할 수 없는 prefix는 Pending이며 시간으로 종결하지 않는다. 이 경계를 운영 진행성 PASS로 선언하지 않는다.

## 상태 차트

`NativeInputAttribution.tla` 기존 20cfg는 B2의 결과·정리까지 설명하는 추상 모델로 유지한다. `NativeInputPreparation.tla` 4cfg가 B1의 Prepare/AnyKey/NotSent, explicit primary pin, RuntimeReady와 restart를 정밀화한다. B2 전이는 이번 코드에서 켜지지 않는다.

| 변형 | 수정 전/원인 제거 RED | 고정 GREEN |
|---|---|---|
| 준비 전 key 송신 | `KeyBeforePrepare`: NoKeyBeforePrepare | `Fixed`: 준비 뒤에만 AnyKey |
| busy follower 권한 차용 | `BorrowBusy`: FollowerNoAuthority | Follower pin=None, Primary pin 보존 |
| no-key 원본 삭제 | `DropNotSent`: NoSilentLoss | NotSent/restore 뒤 원본 보존 |
| 진행성 | 상기 제어에서 안전성을 별도로 검사 | PreparationEventuallyLeavesIntent, SourceEventuallyBound |

repository `scripts/ci/statecharts_tlc.py`로 같은 바이트의 24cfg를 전수 검사했다: 109 TLC 실행, 100.4초, 전체 rc0·기대 위반 집합 일치. 고정 preparation 모델은 36개 상태다. source hash와 원시 로그, Rust 실제 실행 수/뮤턴트/바이너리 SHA는 코디네이터 보고서 `report-impl-b1-1.md`에 남긴다. 모델은 저장소/파일/Actor side effect를 실행하는 시험이 아니다.

짧은 반례는 Init→AnyKey(준비 전 송신), Follower Init→Prepare(남의 lease 차용), Init→NotSent(원본 삭제)다. [차트](native-attribution-b1.chart.mmd)는 이 준비 경계와 native read-only 후보를 보여준다.

## 실제 진입점 시험

- `native_8148_b1_*`: O poll의 FIFO/strict closer, command/absorption 순서·생략, 철회/transfer/popAll/late dequeue, source/twins/receipt 충돌, wrapper/meta 구분, 분할 poll/replay/파일 교체를 검사한다.
- `native_8148_b1_prekey_*`, `native_8148_b1_invalid_primary_*`: 실제 mailbox reserve→pump leaf와 RuntimeReady·unknown/unarmed restore를 통과한다. None follower를 현재 mailbox의 primary로 바꾸지 않는다.
- A·P2 107개 회귀와 기존 InputFacts/pump 회귀를 다시 선택한다. lib 전체 실행으로 분모를 흐리지 않는다.

compiler/fixture 준비 실패는 기능 RED로 세지 않는다. 원인 RED는 정상 fixture에서 해당 규칙만 제거한 실행 단언 실패로 남기며 mutation hook은 커밋하지 않는다. PG 시험은 실제 throwaway PG, `--test-threads=1`, 선택=실행=통과·ignored0을 확인한다.
