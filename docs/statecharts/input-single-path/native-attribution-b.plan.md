# B 활성화 범위 재산정

기준 A `6acfb9f6d88ff1a4304a0d8bcbb5ff1d3ab5d619`, P2 첫 커밋 `9e0459bbdfdc12f6e1fe5320ec08698785cabe20`.
P2 두 건만 구현·검증했고 운영 ticket/result producer와 native 완료 actor는 아직 활성화하지 않았다.
계약은 d1을 유지한다. 아래는 계약을 낮추는 대안이 아니라 남은 구현·리뷰 경계다.

## 첫 커밋의 실제 변경

| A 리뷰 | 구현 | 실행 증거 |
|---|---|---|
| 손상 v3 fail-open | `history/v2/native.rs::boot_records`는 migration decode 오류를 그대로 반환한다. 완전히 해석된 V1/V2만 예외이며 native ticket을 담은 v2는 거절한다. | 공백/줄바꿈/필드 순서가 다른 손상3종에서 기존 코드 RED. 수정 뒤 incomplete1·row 유지·동일 guard 재시도. fail_open 뮤턴트 RED |
| custody 전체 보존 | `inflight/removal/native_input_tests.rs`가 reap 전에 raw registration으로 예상 namespace/settlement/registration을 만들고 custody 전체와 비교한다. pending/Executed 두 경우에 warned 및 추가 proof도 넣었다. | 정상 copy GREEN. custody outcome 또는 proof 삭제 뮤턴트 각각 RED |

임시 뮤턴트 분기는 커밋하지 않았다. 기존 결과 CAS·cleanup 권한·source 복원 계약을 바꾸지 않는다.

## 코드에서 확인한 추가 표면

1. `writer/input_facts.rs:527`의 aggregate queue는 모든 record를 읽지만 `:528`의 `Ordered::Input`은 `[adk:tok=`가 있는 줄만 내보낸다. 평범한 external enqueue/dequeue/popAll을 `events()`만으로 복원할 수 없다. 기존 token consumer를 유지하면서 같은 capture에서 native queue/user/command/closer의 순서·범위를 내보내야 한다.
2. `writer/input_facts/queued.rs`의 `find`와 분류기(재산정 당시 :69/:198, B1 이동 뒤 `native.rs::classify`)는 body 기반 aggregate이며 `find`가 첫 후보를 고른다. 이를 ticket별 완료 권한으로 사용하지 않는다. FIFO의 외부 입력도 유지하고 exact receipt/원본 key·source를 결합해 twins를 Pending으로 둬야 한다.
3. `human_frame/history.rs::row_owner`는 `owner_in:773`→`taken_rows:797`에서 파일을 다시 읽는다. 매 record마다 호출하면 두 번째 반복 scan이 된다. 기존 pairing 의미를 이미 capture한 순서 있는 자료에 재사용할 포트가 필요하다.
4. `turn_presence/activity.rs:964`의 worker와 `:1014`의 LivePorts에는 SharedData/terminal sink가 없다. effect poll `:638`, worker `:976`, tail reader 경로를 모두 잇고 같은 binding/generation/revision만 제출해야 한다. giant 본문에 새 fold를 넣지 않는다.
5. `provider_dispatch/owner_input.rs:470,539,882,1246`에서 새 lease primary와 `hand_busy:311` follower의 권한을 구분해 첫 key 전까지 운반해야 한다. 특히 into_busy_pane은 paste_busy가 새 CancelToken을 만드는 경로이므로 현재 mailbox 조회만으로 Some을 합성하지 않는다.
6. `queue_io/input_owner/pump.rs:46`→V2 reserve→`queued_head/v2/sources.rs:167,225`→Registry prepare가 첫 key 전 저장 위치다. prepare만 남긴 crash/NotSent는 실행 결과가 아니다. 기존 no-key proof와 재전송 정책을 그대로 소비해야 한다.
7. `turn_finalizer/finalize_context.rs:15`는 네 bool의 Copy 문맥이고 현재 `completion_admission.rs:61`은 delivery 관문이다. 이를 native 성공으로 가장하거나 빈 response/OperatorRelease로 우회하지 않는다. typed native 메시지/생성자와 별도 cleanup admission이 필요하다.
8. `turn_bridge/stream_tick.rs`는 이미 1,062줄이다. NativeSettled는 cancel과 별도이며 loop/postloop에서 provider Escape·취소 후처리를 하지 않아야 한다. 연결/typed 거절만 두고 해당 giant의 prod 순증은 0 이하다.
9. A boot는 metadata를 보존할 뿐 소비/복원은 하지 않는다. custody-only 재구성, 결과 뒤 crash, row clear 뒤 event 유실, incarnation 변경, stale actor/row CAS debt를 실제 owner wake까지 연결해야 한다.

## 크기 재추정

실측(`pr_cap_prod.py --exclude-test-modules`): A+P2 **6파일/+319**, B P2분 **1파일/-2**.
따라서 PLAN 선 +1200까지 여유는 **881**이다. 아래는 테스트·기존 블록 이동을 제외한 새 prod 코드 추정이며 아직 구현 실측은 아니다.

| 남은 표면 | 예상 순 코드 | 사용할 접점 |
|---|---:|---|
| O ordered native facts·FIFO/membership/withdraw fold·receipt 유일성 | +220~280 | input_facts/queued, history receipt pairing |
| primary/follower intent·pre-key 준비·RuntimeReady/NotSent 연계 | +130~170 | owner_input, pump, V2 sources |
| registry 결과/cleanup/warn/index/복원 API | +95~125 | history/v2/native leaf |
| typed native actor·exact CAS·독립 admission | +180~230 | terminal_handler/episode/completion_admission |
| native response 범위·bridge 중립 종료 | +60~85 | stream tick/loop/postloop leaf |
| boot/custody-only replay·incarnation·wake debt | +95~130 | boot prepare, restore/recover, owner drive |
| Cancel/Terminate/clear/stop/release 접점 | +90~125 | d1 인접 경로 표 |
| persisted idle WARN1(120초, 종결0) | +45~65 | native lifecycle leaf/clock |
| 기존 worker·tail·boot/owner wake 연결 | +35~55 | presence/owner leaf 연결 |
| **B 잔여** | **+950~1265** | 중복 파일 합산 제외 약18~23 prod 파일 |
| **A+P2+B 예상** | **+1269~1584** | 총 약22~28 prod 파일 |

원래 B +400~530은 위 O 입력별 fold·권한 운반·복원 포트가 A에서 이미 준비된 것으로 읽힐 수 있으나 A의 실제 범위는 reader-first였다. d1 전체 +900~1180 추정도 위 접점을 따라 다시 보정해야 한다. 현재 실측이 +1200을 넘었다고 주장하지 않는다. 잔여 추정이 여유881보다 커서 활성화 전에 PLAN_READY로 멈춘다.

## 권고 분할과 착지 조건

- B1: 같은 O capture에서 typed native evidence와 FIFO/membership/withdraw/source/twin/replay를 검증하고 explicit primary intent의 pre-key 운반을 고정한다. Native 결과/lease 정리는 아직 활성화하지 않는다. 새 휴면 코드의 경고는 기존 cfg(test) 경계로 막고 allow/baseline을 늘리지 않는다.
- B2: B1 계약 위에서 typed native actor·결과→actor CAS→row CAS→cleanup admission→wake를 연결한다. boot/custody-only·lost wake·adjacent 경합·WARN·isolated herdr e2e를 같은 활성화 조각에서 닫는다. 연결만 먼저 켜지 않는다.
- **A+B 함께 착지 지시는 유지한다.** 우선 B1/B2를 같은 묶음의 별도 review ref로 검토하고 B1 실측으로 B2 추정을 보정하는 안을 권한다. 합계가 PLAN 선을 넘는 진행은 코디네이터 판단 후 재개하며, +1500 공통 상한을 넘을 전망이면 착지 경계 재결정이 필요하다. 이 레인에서 예외 상한이나 착지 정책을 변경하지 않는다.
- giant는 기존 enum/연결 책임을 leaf로 분리할 필요가 있을 때 의미가 같은 이동만 하고, 숫자를 맞추려 압축하거나 무관한 코드를 지우지 않는다. 각 giant의 prod 순증≤0을 별도로 측정한다.

## 검증·모델·DoD 잔여

P2는 기존 모델의 PrepareFailure→row 보존→retry 전이에 대한 입력·직렬화 정밀화다. 새 운영 전이를 켜지 않았으며 A 20cfg를 그대로 전수 실행한다. custody의 필드 전체 보존은 모델의 preserved 추상화 바깥이므로 Rust 전체 BootRecord 단언으로 검증한다.

B1/B2가 실제 전이를 켜기 전에는 모델에 pre-key Prepare/AnyKey/NotSent와 explicit primary pin, native typed admission, event 유실 후 boot wake의 실제 동작을 대조한다. no-key 원본을 완료/철회한 것으로 만들지 않고 원래 queued/retained 상태를 유지한다. 새 전이는 수정 전 RED·후 GREEN 및 원인 제거 RED를 갖춰야 한다.

| DoD | 이번 P2/회귀 | B1/B2가 실제 진입점에서 증명할 내용 |
|---|---|---|
| 큐 fixture | A 회귀 및 metadata 보존 | external 예약/알림/TUI enqueue를 포함한 FIFO·dequeue·own closer, mid-turn 선후/중복/생략, own response 범위 |
| withdraw 대기0 | None 결과/안내·parent 불변 회귀 | remove/popAll/delivered/absorbed/twin·late dequeue의 typed proof→결과·안내·정리 |
| native idle WARN1 | warned 필드 보존 회귀 | N 직전/경계/직후·새 open·source 교체·재시작·IO 실패, 시간 종결0 |
| herdr relay e2e | 미실행 | 격리 host/시험 channel에서 queue/join/withdraw 및 둘째 head, 재시작·pane 재사용·late old event·body/footer 중복0 |

실제 Claude/운영 herdr·운영 Discord 조작 금지는 그대로다. native 실기 미실행은 PASS로 세지 않는다. B1/B2의 e2e는 허용된 격리 fixture/host 범위를 먼저 고정한다.
