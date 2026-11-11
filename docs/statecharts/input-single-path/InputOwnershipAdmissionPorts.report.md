# S18 목적별 port 설계: admission / grant / receipt / stop

Code SHA: `92ef5b00b205679965c24c5c017f7237e8b5d57f` (S17 포함 main). 프로덕션 변경 0. 이 문서는 구현 전 설계 검토이며 모델 통과·Rust 시험 통과를 주장하지 않는다.

`SC_RUN=mac-needed`: `/home/adk/.claude/skills/statechart/scripts/statechart`와 상위 skill 디렉터리가 이 WSL에 없다. 모델링은 2026-11-11 02:05 UTC에 시작해 02:32:10 UTC에 모델 편집을 마쳤다(30분 이내). TLC 실행 수 0, 반례 실행 수 0. 아래 expected는 실행 결과가 아니라 Mac 실행 때 확인할 예측이다.

PortSpec cfg는 AdmissionSource=start, AdmissionPane=idle인 headless owner start의 두 receipt-eligible original을 추상화한다. H의 busy inject/native uncertainty 전체는 기존 InputOwnership/InputNativeStop와 실제 fixture가 계속 담당한다.

기존 `InputOwnershipAdmission.tla`에 `observer` record를 재사용한 독립 `PortSpec`을 추가했다. 기존 AdmissionInit/AdmissionNext, 부모 InputOwnership의 동작·cfg는 바꾸지 않는다. 신규 모델 모듈을 만들지 않았다. 기존 ExecutionAuthority의 prepare/held/refresh와 WriterAdoption의 owner/legacy body 경계는 아래와 같이 그대로 둔다.

| 기존 모델 | 확인한 가정과 코드 SHA상의 좌표 | S18 영향 |
|---|---|---|
| InputOwnership / InputOwnershipAdmission | stable ticket `queue_io/input_owner/admission.rs:6,98`; preflight→commit→wake `input_owner/start.rs:56`; actor grant `drive.rs:174`, `turn_orchestrator/queued_head/v2.rs:398,411,443`; marked row `router/message_handler/foreign_row.rs:45,72` | H/A는 요청·값만 제공. ID 예약과 grant를 구분. 기존 owner drive·claim·effect protocol 유지 |
| ExecutionAuthority | 최초 prepare `runtime_bootstrap.rs:271`; worker writer `:340`, standby `:457`; prepare/refresh `runtime_bootstrap/owner_execution.rs:70,146`; gate 불확실성·retire fence는 기존 모델 책임 | R의 runtime 등록 관측으로 admission/stop 허가 불가. 매 실행의 기존 owner authority 재검사 유지. 최초 read 실패 뒤 refresh liveness 유지 |
| WriterAdoption | `services/tui_o/channel_policy/adoption.rs`와 `services/tui_o/cutover/channel_gate.rs`의 Held/Committed; legacy body ledger·W10/W14, warm takeover | body/receipt 소유를 port로 옮기지 않음. outbox/body-census/turn_bridge 복구 caller는 제외. 기존 모델을 GREEN으로 재선언하지 않음 |

위 표에서 `queue_io`, `router`, `runtime_bootstrap` 접두사는 `src/services/discord/` 기준이다. WriterAdoption의 역사적 report 좌표를 현재 SHA의 좌표인 것처럼 재사용하지 않는다. 현재 소스 대조: `src/services/tui_o/channel_policy/adoption.rs:420,428`, `src/services/tui_o/cutover/channel_gate.rs:460,466`, `src/services/tui_o/writer/host.rs:530`. 설계서 §0에 함께 고정한다.

## 규칙과 판정

| 요구 규칙 | 모델 | 실행 판정·코드 대조 |
|---|---|---|
| admission 없이 grant 0 | `PortNoGrantWithoutAdmission`, `PortSingleActive` | **미실행**. v2.rs:398은 현재 front 동일성, :411은 grant 소비, :443은 claim number를 검사한다. 신규 port가 mailbox handle/FrontClaim/EffectPermit를 내보내지 않아야 함 |
| 한 입력 receipt 정확히 1 | `PortReceiptAtMostOnce`, `PortReceiptSettledExactlyOnce`, `PortReceiptAfterTerminal` + **PROPERTY `PortInputsSettle`** | **미실행**. db/turns.rs:253,268은 turn_id별 upsert·first terminal·origin union. 한 SQL 실행이라는 뜻이 아님. merged original은 같은 row의 consumed_origin_ids에 연결될 수 있음 |
| 권한 없는 caller의 grant·stop 직접 경로 0 | `PortNoUnauthorizedCalls`, `PortStopSameEpoch` | **미실행·구현 전**. lookup-only 객체에 stop/grant 메서드가 없고 concrete adapter 생성자는 조립부만 사용. 기존 crate 내부 `pub(crate)` API 전체를 이번 조각으로 봉인했다는 주장은 불가 |
| 재시작 직후 이전 receipt와 authority (ticket enqueue와 effect 구분) | `PortBootReceiptBeforeEffect` + PROPERTY `PortAdmissionResumes` | **미실행**. existing foreign_row/receipt_settle와 ExecutionAuthority의 별도 검증을 대체하지 않음 |
| 멈춤 없음 | **PROPERTY `PortInputsSettle`, `PortAdmissionResumes`** | safety의 terminal 상태 유무를 liveness PASS로 쓰지 않음. fair prepare/actor/effect/end/receipt, 유한 crash와 유한 read 장애가 전제 |

main의 이미 있는 제한/반례 후보도 명시한다. `receipt_settle.rs:58–62`는 pool/provider 부재나 user_msg_id=0이면 실제 receipt 없이 true를 반환한다. 무조건 모든 입력에 receipt=1이라는 명제는 이 경로에 대해 성립하지 않는다. 새 ticket의 durable admission은 이전 marked receipt가 남아도 가능하고 provider effect만 기존 foreign_row guard를 기다린다. 이를 startup admission 전체 hold로 강화하지 않는다. 모델의 receipt 대상은 PG를 가진 origin-bearing original이며, 이 설계가 no-PG 정책을 새로 강화하지 않는다. `InputOwnershipAdmission`의 기존 `OwnerBusyKeepsMarked`는 일반 Discord busy shortcut이 marked row를 유지하는 경로를 별도로 남긴다. S18은 그 경로를 수리하거나 넓히지 않는다.

## cfg와 기대 반례 (실행하지 않음)

| cfg suffix | safety 예측 | liveness 예측 | 반례의 코드 확인 여부 |
|---|---|---|---|
| Ports | 없음 | 없음 | 구현 전 계약. 소스 좌표 대응만 확인, 실행 PASS 아님 |
| PortsGrantLeak | PortNoGrantWithoutAdmission / PortNoUnauthorizedCalls | 없음 | API에 grant를 새로 노출하는 가상 뮤턴트. main 결함이라고 판정하지 않음 |
| PortsStopLeak | PortNoUnauthorizedCalls | 없음 | lookup 객체에 stop를 내보내는 가상 뮤턴트 |
| PortsLooseStop | PortStopSameEpoch | 없음 | capture 후 exact episode 재검사를 없애는 가상 뮤턴트. existing stop_judgement/ChannelStop의 guard를 유지해야 함 |
| PortsDuplicateReceipt | PortReceiptAtMostOnce / PortReceiptSettledExactlyOnce | PortInputsSettle | idempotent row를 중복 logical receipt로 바꾸는 가상 뮤턴트 |
| PortsNoWake | 없음 | PortInputsSettle | admission 뒤 drive wake 누락 가상 뮤턴트. source `input_owner/start.rs:75`에 실제 wake 있음 |
| PortsBootBypass | PortBootReceiptBeforeEffect | 없음 | marked receipt settlement 전에 provider effect를 허용하는 가상 뮤턴트. ticket enqueue 자체는 허용한다. source foreign_row의 기존 headless guard와 대조 |

cfg는 기존 CI `docs/statecharts/expected.tsv`에 등록했다. 기대 목록은 측정값이 아니다. Mac에서 good cfg + 여섯 mutant를 실행하고 실제 safety/liveness 집합을 expected와 대조한 뒤에만 sc=GREEN으로 올릴 수 있다.

## 모델 범위 밖과 이번 변경의 관계

- 영구 DB/권한 장애·무한 crash: liveness 전제가 깨진다. 요청 거절·보존을 성공으로 보고하지 않는다.
- 실제 paste 직후 crash의 불확실성, native receipt/lease seal: PortRestart의 resolved abstraction이다. 기존 InputOwnership/InputNativeStop와 실제 actor·native receipt 시험이 계속 필요하다.
- Legacy body census, turn_bridge, relay_recovery, PendingQueueSnapshot, teardown/force/reset: 기존 소유자에 남기며 N은 nonforced Herdr holder intent만 다룬다.
- no-PG/message-less receipt와 이미 main에 있는 busy marked-row 경로: 위 제한을 보존하며 전체 receipt 보장으로 합산하지 않는다.
- HTTP 인증·iMessage GUID 중복·본문 검증: H 구현에서 기존 author 재검사, caller extractor와 outer deliver_once 순서를 보존한다. 모델은 이미 인증된 committed original에서 시작하므로 Rust fixture로 별도 검증한다.
- 다중 provider/channel/host, nonce/hash 파일 증거, 실제 thread scheduling: 한 channel·두 original·한 crash 모델의 밖이다. R의 관측을 권한으로 쓰지 않고 기존 selection/host guard가 판단한다.
- 기존 crate 내부 공개 raw API: malicious Rust caller를 모델링한 언어 보안 증명이 아니다. S19/S20에서 전환하는 caller에는 raw grant/stop 핸들을 주지 않는다는 DI 수용 기준이다.

## 진입점 전수표

[전수표](InputOwnershipAdmissionPorts.inventory.md)는 git grep 원시 참조 전체에 file:line, 모델 행동, 제안 port, 제외 경로를 붙였다. 원격 review ref에서도 이 파일을 함께 검토한다. 설계서 §0에는 사람이 추적한 실제 경계 진입점과 boot/cancel 인접 경로를 별도 표로 둔다.

## 증거 목록

- code SHA 위와 동일. census `W=521`, G `419 ID / 746 occurrences`; 대상 registry 41행(S19=23, S20=18). 원시 census는 lane-run logs/s18-1에 보관.
- raw git grep 및 inventory: 모든 symbol reference를 보존하며 test/import/comment도 구분. 이것을 production caller 수로 대체하지 않는다.
- cfg 정적 파싱: 신규 7 cfg 각각 invariant 8개/property 2개; 전체 279 cfg의 expected 등록 일치. `cfg-static.log`.
- `python3 -m unittest tests.test_statecharts_tlc`: total 54, 실행 52, skip 2, failure/error 0 (`statecharts-checker-tests.log`). 이 시험은 checker 검증이며 모델 TLC 결과가 아니다.
- diff whitespace 검증은 별도 로그에 기록한다. TLC와 Rust 바이너리 실행 증거는 없다.
- 설계 정본: `/home/adk/adk/lane-runs/i7353/plan-s18-1.md`; 인계 보고서: `/home/adk/adk/lane-runs/i7353/report-s18-1.md`.
