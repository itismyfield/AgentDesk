# #7353 S4-07 설계 r2 — scope 전용 발급 권한을 보존하는 consent 분리

- 레그: 구현 전 PLAN, production 변경 0. 기준 production SHA `08e2336e47986d70d0cbe95e56215858377aed5c`; d1 문서 head `4bf484ed8363bdbdc7734e21013bd157ffc43bd6`.
- 첫 커밋 `e8f13ac6a744b27bf85da308c8be5ea40766904c`: 기존 모델 :159의 IF 우선순위만 `(IF …)`와 `drift'=TRUE` 별도 conjunct로 수정. amend 없이 먼저 커밋했다.
- 정본 r2: `/home/adk/adk/lane-runs/i7353/plan-s4-07-2.md`. 현재 문서는 r2 설계 내용을 보존하고, 구현 후 줄 수가 바뀐 DB handoff의 과거 좌표는 r2 기준 SHA 링크로 구분한다. d1은 이력으로 남기며 r2가 이를 대체한다.
- 선택: **adapter 소유 opaque proof + 기존 o_input만 만들 수 있는 private scope key + 봉인된 service issuer child**. CAS는 기존 bool 그대로, **SpawnPermit/변환 API/저장 가능한 CAS receipt 없음**.
- 추정 **16 production 파일 / +580 순 코드**(소계+450, 보수 여유+130), 범위 +400~750. d1 14/+520 대비 +2파일/+60. hard cap은 공식 `pr_cap_prod.py:15`의 30/+1500, PLAN 정지선 +1200.
- WSL 모델 실행기 없음: `SC_RUN=mac-needed`. 새 모델/TLC/state count의 GREEN/RED 관측은 없다. cfg의 결과 표는 예측이다. 기존 모델링과 새 모델링 결과를 섞지 않는다.

## 0. 리뷰 지적과 상태 차트

| 지적 | 인정한 문제 | r2 처리 / 검증 |
|---|---|---|
| P1 공개 변환 | CAS 성공을 발급 권한으로 정의한 d1은 기존 권한을 넓혔다. sibling이 permit을 보관하고 unscoped executor effects 뒤 proof로 바꿔 rollback 가능 | CAS API를 바꾸지 않음. public conversion 삭제. private-field WorkerScopeKey 생성은 기존 o_input 안에서만; opaque proof의 factory는 key가 필요한 service scope 내부에만. complete API composition의 same-crate non-test compile probe, PG missing-authority 경계, key visibility widening mutant |
| P2 동일 전이계 | d1 Boundary는 ASSUME에만 있어 Current/Proposed가 같았고 공개 조합·effects 후 rollback을 놓쳤다 | Boundary가 3개 실제 발급 프로토콜을 선택. receipt 보관, scope 밖 변환, proofless effects, late rollback, repeated claim, process epoch을 넣음. NoEffectsThenRollback 추가 |
| P2 FS 간극 해석 | observed=TRUE 뒤 FS fact가 FALSE여도 CAS/commit 가능 | 현행과 같은 기존 간극으로 명시. GREEN을 최신 consent file 보장으로 사용하지 않음. 새 FS/PG lock·재관측 없음 |
| :159 모델 오류 | drift'가 ELSE 안에 묶여 일부 successor가 미정의 | 요청한 두 줄 수정 먼저 독립 커밋. 새 record EXCEPT의 IF/CASE value는 괄호로 묶음; 정적 cfg 파싱은 SANY/TLC를 대신하지 않음 |

기존 execution-authority 및 input-single-path의 permit/native-stop/boot 모델을 d1에서 읽고 consent 행동 부재를 확인했다. production 코드가 바뀌지 않아 그 조사와 source table 좌표를 재사용한다. intake-consent 영역을 개정하고 root index/expected manifest를 같이 갱신한다.

### 0.1 규칙

기존 여섯 INVARIANT(`TypeOK`, `NoUnauthorizedIssue`, `ProofConsumedOnce`, `NoStaleProofWrite`, `RollbackExactAndBookkeeping`, `ReleaseOnce`)와 PROPERTY 두 개(`ProofEventuallySettles`, `TransactionEventuallyEnds`)를 유지한다. 무권한 발급은 CAS 성공만으로 허용하지 않으며 private scope 또는 private-keyed scope 출처가 필요하다.

새 INVARIANT `NoEffectsThenRollback`은 같은 원본 입력에 effects가 난 뒤 rollback commit으로 pending이 되는 경로를 금지하고, 그 원본 입력의 반복 claim으로 effects가 두 번 나는 것도 금지한다. DB에 새 effects ledger를 만든다는 뜻은 아니다. 지금 DB exact-row 검사는 이미 난 effects를 알 수 없으므로 **늦은 authority 취득을 허용하지 않는 것**이 가드다.

MaxClaims=2, process epoch 0/1로 같은 durable row를 반복 claim한다. 권한 인스턴스는 `(epoch, serial)`로 구분하되 serial을 DB CAS에 가상의 더 강한 fence로 넣지 않는다. 실제처럼 같은 process의 claim_owner token은 반복될 수 있다. restart는 in-memory receipt/key/proof를 폐기하고 새로운 인스턴스만 허용하며, claimed-only pre-accept reclaim은 기존 경로대로 모델링한다. spawned row는 restart로 자동 pending이 되지 않는다.

### 0.2 cfg별 기대 결과와 서로 다른 전이계

| cfg / Boundary | 실제로 선택되는 발급·공개 조합 행동 | 예상 결과 / 깨질 규칙 | 새 상태 수 |
|---|---|---|---|
| Current / current | CurrentPrivateIssue; sibling은 RejectOutsideConversion | GREEN | 미실행, Mac 필요 |
| D1Public / d1 | spawned CAS가 permit 보유 → D1RetainReceipt → D1ConvertOutsideScope; 정상 worker는 D1EnterScope | RED: NoUnauthorizedIssue, NoEffectsThenRollback | 미실행, Mac 필요 |
| Proposed / r2 | R2PrivateScopeKey → R2EnterKeyedScope; sibling key/receipt 없음 → RejectOutsideConversion | GREEN | 미실행, Mac 필요 |
| Forge / r2 | CAS/발급 권한 없는 proof 생성 | RED: NoUnauthorizedIssue | 미실행 |
| Reuse / r2 | spent proof 두 번째 소비 | RED: ProofConsumedOnce | 미실행 |
| Stale / r2 | exact row 비교 제거 | RED: NoStaleProofWrite | 미실행 |
| Bookkeeping / r2 | rollback retry_count 증가 | RED: RollbackExactAndBookkeeping | 미실행 |
| OwnerOnly / r2 | production retire→owner transfer 결합 제거 | RED: NoStaleProofWrite | 미실행 |
| ScopeOpen / r2 | private key ctor 가시성 확대 → OpenKeyOutsideScope → EnterLateScope | RED: NoUnauthorizedIssue, NoEffectsThenRollback | 미실행 |

r2의 Boundary는 선언/ASSUME 외 각 Current/D1/R2 행동의 guard와 CAS 반환값·unscoped entry 선택에 쓰인다. worker at spawned에서 Current는 즉시 private issue, r2는 private keyReady 상태를 거친다. sibling at spawned에서 d1만 permit을 갖고 retain/convert가 활성화된다. 따라서 같은 전이계를 cfg 이름만 바꾼 모델이 아니다. 이 enabled-action 차이는 구조 증거이고, 실제 상태 수·정제 동등성 증명은 아니다. 코디네이터가 알려준 예전 4,372 상태 일치는 수정 전 동일 모델의 결과로만 기록하며 새 모델 증거로 재사용하지 않는다.

P1 예상 trace: sibling Claim(1) → accepted CAS → spawned CAS → retain receipt → executor first-entry(outside scope, proof 없음) → effects → public convert → RollbackMove → owner lock → row lock → exact CAS → commit pending. 원본 payload/claim/owner/marker는 그대로라 d1 소비 1회 검사도 통과한다. 다음 claim에서 같은 입력이 다시 실행된다. Current/r2는 effects 뒤 취득할 authority가 없어 이 조합이 막힌다. 실제 trace는 Mac에서 새 규칙만 검사한 run으로 확인해야 하며 구문 예외·timeout은 RED가 아니다.

### 0.3 FS와 owner 경계

`ObserveAndMove(TRUE) → ChangeConsent(FALSE) → PrepareCAS → Commit`는 Current/r2에 모두 허용된다. 현재 순서가 `load/검증 → begin → owner advisory lock → row FOR UPDATE → CAS → commit`이므로 service 이동이 새 간극을 만들지 않으며, 파일 최신성을 commit까지 보장하지도 않는다. 강한 최신성 요구는 별도 cross-store lock/발급·소비 시점 설계가 필요하다.

C-owner 차단은 main `1fe3a57d266e1e7fe384796d22a925076f1d99fc`에서 이미 충족한다. `owner_record/custody.rs:190–192`의 open-row retire 후 :201–203 owner transfer가 같은 transaction에서 실행되고, :323–324 및 :383–392(또는 `db/replay_disposition/write.rs:217–228`)가 old spawned row를 unknown으로 바꿔 hold :308의 status CAS를 막는다. replay_only=true는 schema `0136:15–25`와 writer `write/start.rs:151–158`에 의해 unknown이므로 정상 spawned row의 제외 구멍이 아니다. 새 active-owner hold query나 cleanup 권한을 추가하지 않는다.

## 1. P1 재설계와 권한 범위

### 1.1 두 안을 재비교

| 안 | 흐름·비용 | 권한 경계 / 판단 |
|---|---|---|
| A domain proof + service 발급 | 순수 projection/proof를 domain에 두고 service issuer만 생성 허용 | Rust pub(in)은 조상에만 제한 가능하므로 service 형제에 friend constructor를 줄 수 없다. naked pub(crate) ctor나 공개 CAS receipt로 풀면 d1 P1 반복. domain 아래 runtime issuer를 넣어 순수성을 깨는 안도 기각 |
| **B r2 adapter capability + private-keyed service issuer** | proof actual definition은 adapter capsule, FS/runtime은 service, pure wire/projection/predicate는 domain. 기존 o_input에만 private key ctor를 둠 | no receipt/no conversion. 원래 scope 발급 권한을 새 private key로 식별하여 기존 code 위치에서만 capsule 진입. Rust privacy 때문에 issuer를 proof module의 child로 선언하되 source/책임은 service에 둔다. 16/+580으로 채택 |

### 1.2 정확한 모듈·가시성·시그니처

아래는 설계 시그니처다. 이 레그에서 Rust 스파이크/production 변경은 없다.

| 소유·파일 | 타입/API | 가시성·봉인·소비 |
|---|---|---|
| A `src/intake_consent_authority.rs`(신규) | `struct BeforeEffects(IntakeOutboxRow)`; `row(&self)->&IntakeOutboxRow` | 타입/빌림 getter pub(crate), tuple field **private**. 실제 정의/impl은 여기이며 service 타입 alias가 아니다. Clone/Copy/Default/serde/From<Row>/Deref/owned row getter/into_before_effects 없음. cfg(test) for_test만 현행처럼 허용 |
| 같은 capsule의 child 선언 | `#[path="services/cluster/intake_worker/consent_scope.rs"] pub(crate) mod issuer;` | issuer child는 private field에 접근 가능. parent에는 runtime/FS/callback 실행 본체나 runtime fn reexport를 두지 않음. domain 모듈이 아니며 DB가 issuer 함수를 호출하지 않음 |
| S `o_input.rs`(현 :73–118 자리) | `pub(crate) struct WorkerScopeKey(())` | **tuple field private**. 생성자/필드/Default/Clone/serde/From<bool>/From<Row>/factory/re-export한 생성 함수 없음. o_input과 기존 test descendants만 생성 가능. worker 부모·DB·sibling은 타입을 알아도 만들 수 없음 |
| S `o_input::scope_before_effects<T>(row, execution)->T` | 기존 private wrapper 이름·입력 유지; 내부에서 `issuer::scope(WorkerScopeKey(()),row,execution).await` | private 유지. key를 만드는 시점은 기존 scope 호출 자리. 공개 scope helper나 proof 반환값 신설 0. CAS 성공 다음 old wrapper가 유일 production 진입 |
| S `issuer::scope<T>(key:WorkerScopeKey,row:&IntakeOutboxRow,execution:impl Future<Output=T>)->T` | pub(crate) fn이나 **private key 필수** | key by value 소비. row clone/status Spawned → private proof → private task-local slot → 기존 future scope. 별도 key/proof/receipt 반환 채널은 없음. generic T는 실제 production에서 기존 IntakeExecution 결과이며, T 자체를 capability가 될 수 없다고 가정하는 봉인은 아님. key를 가진 기존 trusted issuer가 임의 future를 구성하는 권한은 현행 private scope와 같음. naked `scope(row,future)` overload 없음 |
| S `issuer::take(key:WorkerScopeKey)->Option<BeforeEffects>` | pub(crate), private key 필수; task-local slot private | by-value key 소비 후 destructive slot.take. raw take를 key 없이 호출할 API 없음 |
| S `o_input::take_before_effects(request)`(현 :96–104) | 기존 pub(crate) wrapper | 내부에서 별도 일회용 WorkerScopeKey를 만들어 issuer::take 호출 후 기존 `intake_request_from_row(...)=request` filter. filter/decoder는 기존 service에 남아 decoder 가시성 확대 없음. wrong request도 slot 복원0 |
| S `intake_worker.rs:35–38` | `WorkerScopeKey` **타입만** pub(crate) reexport; 기존 BeforeEffects/take 호환경로 | 타입 reexport가 private tuple 생성자를 공개하지 않음. raw key/proof factory reexport 없음. execute_before_effects는 현 pub(super), scope는 private 그대로 |
| A `worker_status.rs:330–336` | `mark_spawned(...)->Result<bool,...>` | **변경 없음**. bool 외 값/receipt/key/proof를 돌려주지 않음. d1 mark_spawned_before_effects/SpawnPermit/into_before_effects는 계획에서 완전히 제거 |
| D `domain/intake_consent.rs`, `decision.rs` | IntentRef actual wire, row/payload projections, digest, unresolved predicate, checked HoldInput | 값/순수 판정만, DB/service/Directory/task-local/key/proof 발급 없음. payload 26 fields+마지막 Null=27 슬롯 유지 |
| S `herdr_consent/worker_handoff.rs` | 기존 :481 자리에 by-value `hold(pool, proof, store, expected, intent)` | private admission child helper가 load/full equality/unresolved/source 검사 후 D binding → A. bool/unchecked DTO가 proof를 대체하지 않음 |
| A `consent_handoff.rs` | `hold_before_effects(pool, proof:BeforeEffects,input:HoldInput)` / `return_before_record(pool, proof)` | capsule의 실제 proof 타입 직접 소비. DB는 service/key/issuer 참조0. FS/callback 주입0, exact row/marker CAS 및 기존 transaction만 |

**Rust 봉인과 층 구분:** 같은 crate에서 임의 friend-module 가시성은 불가능하므로, 실제 opaque 타입은 중립 adapter capsule에 정의하고 **발급 본체만 그 자식인 service source file**에 놓는다. 이 lexical 관계는 private field 접근을 위한 의도적 bridge이며 숨기지 않는다. parent는 DB row 기반 capability 값만 소유하고, issuer child는 기존 service의 task-local/future scope를 소유한다. domain에 runtime을 옮기거나 DB에 filesystem/서비스 callback을 넣는 설계가 아니다. DB가 보는 것은 parent에 실제 정의된 proof이며 service type의 reexport facade가 아니다. module inventory/layer audit가 이 child 배치를 어떻게 분류하는지도 구현 전 리뷰 항목이다. 실제 ownership mapping은 `intake_consent_authority=adapter`, `intake_consent_authority::issuer=application`, `domain::intake_consent=foundation`으로 layers.toml에 근거 좌표와 함께 명시한다. root default assembly를 상속해 DB→assembly debt를 만들거나 issuer를 adapter라고 잘못 분류하지 않는다. module declaration 외 parent→issuer runtime 호출/reexport는 없다. accepted baseline 증가는 금지하고 공식 생성으로 실제 debt 감소와 no-growth를 확인한다. 이 배치가 층 계약상 허용되지 않으면 raw factory/receipt로 타협하지 않고 별도 설계로 멈춘다.

key는 “CAS가 발급한 새 권한”이 아니라 **기존 o_input 모듈의 constructor 권한을 좁은 호출 인수로 표현한 것**이다. 밖으로 저장·반환하는 API가 없다. pub(crate) issuer 함수가 보여도 key가 없으므로 호출 조합이 닫힌다. key ctor를 pub(crate)로 넓히는 mutant는 공격자가 임의 synthetic scope를 만들 수 있게 하여 P1을 복원한다. key 자체를 안전한 Rust에서 임의 생성하지 못함을 실제 same-crate probe로 확인한다(unsafe 메모리 fabrication은 현행 proof도 동일하게 범위 밖).

### 1.3 누가 proof를 얻을 수 있는가

| caller / 가진 값 | Current | d1(기각) | r2 |
|---|---|---|---|
| 기존 o_input private scope 구현 | tuple ctor로 scope slot 발급 | scope 또는 public conversion | private WorkerScopeKey → issuer scope만, 직접 반환 factory 없음 |
| 기존 executor first entry, 정확한 request, active scope | take 후 proof 1개 | 동일 | 동일 public wrapper, private key를 밖으로 노출하지 않고 take/filter |
| 같은 crate sibling, DB pool/row + successful spawned CAS | bool만, scope 밖 take=None | **permit→public convert로 proof 획득 가능(P1)** | bool만. private key 생성 불가, scope 밖 take=None |
| sibling, effects 완료/row spawned/빈 marker | proof 없음, rollback 인수 구성 불가 | 보관 permit을 proof로 바꿔 rollback 가능 | 변환 API 없음, issuer::scope/take key 구성 불가 |
| 기존 worker 부모/자식의 pub(super) executor wrapper caller | old wrapper로 실제 first-entry 경로만 호출, proof 반환 없음 | d1은 public receipt 추가 경로까지 허용 | old wrapper visibility/실제 단일 production 호출 위치 그대로, key/proof 반환 없음 |
| DB adapter | 이미 전달받은 proof를 by value 소비 | CAS로 caller에게 permit 제공 | 소비만, key/proof ctor 접근 불가, mark_spawned bool 유지 |
| boot/restart/consent inventory | row/IntentRef/nonce로 발급 불가 | serializer는 없어도 살아 있는 permit의 지연 변환 위험 | process-local key/proof 소멸, row/receipt로 복원 불가 |
| cfg(test) 기존 fixture | for_test raw row 허용 | for_test 허용 | 동일 test-only, non-test probe에선 없음 |

worker 내부의 기존 trusted 발급 경로를 임의로 악성 변경하는 것까지 막는 새 temporal type system을 주장하지 않는다. 모델의 worker actor는 실제 `intake_worker.rs:458–480` 호출 순서를 따르고 sibling actor는 그 scope 권한이 없는 caller다. r2는 Current의 safe API 발급 범위를 넓히지 않는다. CAS caller의 범위를 발급 caller로 재정의하지 않는다. 정확한 payload를 가진 DTO나 IntentRef도 권한이 되지 않는다. DB가 effects 여부를 나중에 재확인할 것이라고 선언하지 않는다.

### 1.4 호출 순서 보존

worker accepted CAS → 기존 spawned bool CAS → 기존 execute_before_effects pub(super) wrapper → old private scope에서 key 생성/소비 → 중간 외부 await 없이 동일 위치 BeforeEffects slot 발급 → executor 첫 take/filter → consent 없으면 proof drop 후 effects. r2 keyReady는 이 함수 인수 생성의 모델상 microstep이며 앞선 CAS에서 발급된 저장 가능한 권한이 아니다.

consent는 recording=true → 기존 record attempt → by-value service handoff helper → load/full Consent equality/unresolved/exact intent → DB begin → owner advisory lock → row FOR UPDATE → exact payload/claim/owner/marker/status CAS → commit → card effect. before-record rollback과 release/wake/ack의 원래 transaction 경계/시각 샘플은 그대로다. 이미 released인 release no-op을 owner query 뒤로 옮기지 않는다. 재시도 횟수/created_at/TTL/completed_at을 초기화하지 않는다.

cfg(unix)는 기존 proof와 동일하게 유지한다. non-unix의 mark_spawned bool/direct executor future 경로는 그대로다. 새 capsule/issuer의 registration·imports도 Unix gate를 적용해 Windows dead code를 만들지 않는다. src/lib.rs는 현재 301줄이고 새 등록만, giant intake_outbox.rs 1372줄에는 mapper 등록만 넣는다.

## 2. 갱신한 진입점 전수표

좌표는 production 기준 SHA의 src/ 상대 경로다. proposed-only 파일/시그니처에는 기존 line을 꾸며 넣지 않는다. S=service, D=domain, A=adapter, OOS=범위 밖. d1의 현행 코드 표를 재사용하되 proof 관련 제안과 가시성을 갱신했다.

| ID | file:line / 진입점 | 모델 값·행동 | r2 경계·입력 출처 | 가시성 |
|---|---|---|---|---|
| E01 | `services/cluster/intake_worker.rs:434,458,472` | AdvanceStatus → CurrentPrivateIssue / R2PrivateScopeKey → R2EnterKeyedScope | A의 spawned bool → S의 기존 private scope/key. claim owner는 이 worker restart token | 현재 CAS pub(crate), executor wrapper pub(super) |
| E02 | `services/cluster/intake_worker/o_input.rs:75,79,84,107–118` | CurrentPrivateIssue / keyed scope; proof=slot | adapter capsule 실제 proof + key-gated service issuer child. cfg(test) ctor는 production 부재 유지 | 현재 type/getter pub(crate), tuple/scope private; for_test cfg(test) |
| E03 | 같은 파일 `:96–104`, `intake_worker.rs:37` | Take(TRUE/FALSE), proof spent | 기존 pub(crate) take wrapper가 private key로 slot을 꺼낸 뒤 전체 request 동등성 유지. wrong source도 slot 복원 0 | 현재 take pub(crate), slot private |
| E04 | `services/discord/router/message_handler/intake_turn/worker_entry.rs:58–74` | Take → entry / DropForNormalExecution | S 최초 entry, 일반 executor 요청은 proof 없음. consent 없을 때 먼저 drop | 현재 core pub(crate), proof 반환 없음 |
| E05 | `services/discord/router/herdr_consent/admission.rs:438–491` | RecordAttempt → ObserveAndMove | S만 check/record/load, A에 proof 소유값 + D checked binding 전달 | 현재 worker pub(in router), helper는 private child |
| E06 | 같은 파일 `:499–509` | RollbackMove | recording 시작 전만 같은 proof를 A rollback에 move, 다른 row id fallback은 authority 아님 | rollback pub(crate), proof by value |
| E07 | 같은 파일 `:215–242`; `consent.rs:304–334,479–499` | attempted / FS record | S Store.question/append, source별 참조만 저장. payload/upload 복사 없음 | Hold.record private; Store methods pub(crate) |
| E08 | [r2 handoff (184–243)](https://github.com/itismyfield/AgentDesk/blob/08e2336e47986d70d0cbe95e56215858377aed5c/src/db/intake_outbox/consent_handoff.rs#L184-L243) | LockOwner, ReadForUpdate, PrepareCAS, Commit(rollback) | A가 same row/stamp/marker/live owner 재확인, bookkeeping 유지 | DB rollback pub(crate), tuple ctor 접근0 |
| E09 | 같은 파일 `:246–315` | ObserveAndMove → locked hold CAS | FS 관측은 S helper로만 이동. A는 opaque proof와 checked values, 기존 CAS/상위 retirement 결합 유지 | DB hold pub(crate), proof 필수 |
| E10 | 같은 파일 `:107–180`; `services/discord/router/herdr_consent/custody.rs:205–268` | Release / releases<=1 | S exact durable release receipt → A. nonce만으로 row 생성/증식/새 proof 발급 없음 | DB release pub(crate), LiveSink service scope |
| E11 | `services/session_host/herdr/policy/consent/settlement.rs:62–105,224–288` | unresolved eligibility / release boundary | D predicate, S Applied/preview/settlement guard·ack는 유지. returned/adopted는 unresolved지만 releasable과 다름 | Consent methods/Store pub(crate) |
| E12 | `services/discord/router/herdr_consent.rs:24–29,79–90,388–407` | FS inventory / release replay | S store factory/load_all/load_each. 재시작에 persisted IntentRef로 proof 복원 0 | store factory pub(in discord), recover pub(super) |
| E13 | `services/discord/router/herdr_consent/startup.rs:52–87,91–150` | Restart; boot recovery OOS | S local node/user/socket/session 필터, BootState claim/complete, Accepted interruption → existing recovery. 새 Issue 없음 | restore pub(in discord), interrupt private |
| E14 | `services/session_host/herdr/policy/consent/engine.rs:44–75,129–171,272`; `workflow.rs:93,137,158,222` | FS observation / consent revision OOS | S 복구·click·retry의 기존 load 및 stale-decision guard, DB가 callback으로 호출하지 않음 | runtime service methods, proof factory 없음 |
| E15 | `config/session_hosts/migration/consent_holds.rs:26–30,61,90` | unresolved inventory OOS | S migration blocking/return 판정. legacy 파일 편집 제외, proof 만들지 못함 | migration service scope, authority 없음 |
| E16 | `services/discord/router/herdr_consent/return_input.rs:8,47–71,118–201` | returned/adopted unresolved OOS | S owner/provider/channel 확인 + exact intent FS settlements. before-record rollback과 다른 legacy-return workflow, 변경 제외 | handle pub(super), reserve/close private |
| E17 | `services/cluster/intake_worker.rs:269–311,366–381,406–413,529–566` | AbortStale / clean drain vs Restart | S pre-accept cancel은 claimed rearm, accepted 뒤에는 lifecycle drain. boot에 Spawned proof 재발급 금지 | worker loop/tick service; release_cancelled_claim private |
| E18 | `services/discord/commands/stop.rs:52–105`; `services/session_host/herdr_gate.rs:86–92,1025–1045` | Cancel/Terminate OOS | 다른 native/Herdr gate authority, BeforeEffects 소비/발급 없음. #7651 gate/legacy 파일은 읽기 증거만, 편집 제외 | gate/command scoped API, BeforeEffects 없음 |
| E19 | `services/discord/commands/control.rs:510–559,715–755`; `control/clear_report.rs:128–192` | mailbox clear/cancel OOS, FS eligibility | clear 후 원본 source+queued generation 부재를 확인하고 **mailbox intent만** acknowledge_discard. outbox proof가 되지 않음 | command service scoped, settlement은 mailbox만 |
| E20 | `db/intake_outbox_force_fail.rs:55–121` | Drift(forceFail) | 운영자가 row lock으로 terminalize하고 별도 child INSERT; 새 child는 다른 row/attempt. 이번 설계가 새 운영 권한을 호출/추가하지 않음 | force-fail DB pub(crate), proof 없음 |
| E21 | `services/maintenance/jobs/db_retention.rs:1–4,463–535` | OOS retention observation | intake_outbox는 후보 count만, 삭제 없음. 모델/제안에서 delete나 TTL 초기화 추가 0 | maintenance observation, proof 없음 |
| E22 | `db/intake_outbox/worker_status.rs:379–414`; `services/cluster/intake_router_hook/owner_record.rs:814–819` | ReclaimPreAccept / AbortStale | 실제 stamped sweep은 claimed만. cfg(test) legacy sweep을 runtime 정리로 활성화하지 않음 | stamped sweep pub(crate), legacy leaf cfg(test) |
| E23 | `services/cluster/intake_router_hook/owner_record.rs:304–322,382–435` | Drift(retire), live | production promote는 같은 tx에서 old open row retire→owner transfer. OwnerOnly 진단 뮤턴트는 이 결합을 제거한 운영 가정 밖 trace | transfer pub(crate), retire_open_on_tx private |
| E24 | `services/cluster/intake_worker/consent_scope_tests.rs:4–43`; `db/intake_outbox/consent_handoff_tests.rs:260–275` | test-only issue / take mismatch | test ctor·test wrapper만 존재. prod 발급 경로 집계와 분리, 테스트 0건/skip으로 I1 증명하지 않음 | test-only for_test pub(crate), production 없음 |

| E25 | 신규 `intake_consent_authority.rs` | opaque proof actual definition | A 값의 private field, raw ctor/convert0 | type/getter pub(crate), field private |
| E26 | `o_input.rs` 기존 scope/take 안의 신규 WorkerScopeKey 생성2곳 | R2PrivateScopeKey / private take permission | scope/take wrapper 내부만, key 반환0 | key type pub(crate), tuple ctor private |
| E27 | 신규 `services/cluster/intake_worker/consent_scope.rs`(capsule issuer child) | R2EnterKeyedScope / destructive take | o_input key를 by value 소비, private slot, same future result | functions pub(crate) + inaccessible key; slot private |
| E28 | `db/intake_outbox/worker_status.rs:330–336` | CAS bool only | 기존 API 그대로, receipt/key 발급0 | pub(crate) bool |
| E29 | d1 design :185–192의 SpawnPermit/into_before_effects | D1RetainReceipt / D1ConvertOutsideScope(진단 모델만) | **r2 API에서 삭제**, production에는 원래 없음 | r2에는 타입/함수 모두 없음 |

### 2.1 Store load 전수표

`git grep ConsentStore`의 production 소비자는 DB handoff, router, startup, config consent_holds, engine/workflow/settlement였다. `.load(`, `.load_all(`, `.load_each(`, `.unresolved(` 호출도 별도 로그로 고정했다. cfg(test) 시험들은 raw proof-consumers 로그에 남겼으며 runtime으로 계산하지 않는다. 각 행은 FS 값/ObserveAndMove 또는 명시한 OOS다.

| file:line | 관측 진입 / 모델 | 제안 경계 |
|---|---|---|
| [r2 handoff (255–259)](https://github.com/itismyfield/AgentDesk/blob/08e2336e47986d70d0cbe95e56215858377aed5c/src/db/intake_outbox/consent_handoff.rs#L255-L259) | 정확한 expected snapshot+unresolved intent / ObserveAndMove | 이 1개 load만 새 S worker_handoff로 이동, begin보다 앞 |
| `services/discord/router/herdr_consent.rs:89,407` | load_all / load_each inventory, OOS | S 유지 |
| `services/discord/router/herdr_consent/startup.rs:61` | boot interrupt inventory / Restart 경계 | S 유지 |
| `services/session_host/herdr/policy/consent/engine.rs:149,171,272` | card reserve/retry/apply 최신 관측, OOS | S 유지 |
| `services/session_host/herdr/policy/consent/workflow.rs:93,137,158,222` | decision/retry/boot 최신 관측, OOS | S 유지 |
| `services/session_host/herdr/policy/consent/settlement.rs:191,195–209,215,269` | load_each/load_all/unresolved/release snapshot | S 유지, unresolved predicate만 D |
| `config/session_hosts/migration/consent_holds.rs:61,90` | unresolved inventory, OOS | S 유지, 편집 제외 |
| `services/discord/commands/control/clear_report.rs:182–192` | unresolved inventory 후 mailbox discard, OOS | S 유지; 새 proof 발급 없음 |
| `services/session_host/herdr/policy/consent.rs:282–290`; `durable.rs:193–203` | 모든 load의 leaf normalize/read_stable/decode | S Store/Directory 유지. DB 안에 trait/closure 주입하지 않음 |

### 2.2 같은 outbox row의 다른 SQL writer 전수표

`git grep`의 UPDATE/INSERT/DELETE를 시작점으로 multiline 및 `public.intake_outbox`를 추가 검색했다. 공식 cap 도구의 test-module 판별과 inline test-block 제외를 사용해 후보 35곳을 얻었다(명시적 cfg(test) leaf 1곳은 아래 표시). schema trigger/boot migration도 별도 행에 적었다. 단순 SQL 패턴의 개수만으로 0/PASS를 선언하지 않는다. 각 다른 writer가 존재할 수 있으므로 proof 검사는 transaction 안에서 재확인한다.

| file:line | 모델 값·행동 | 제안 경계 / 분류 |
| `db/intake_outbox.rs:121` | Claim 초기 row / retry child OOS | A 일반 insert/retry, proof 발급 없음 |
| `db/intake_outbox.rs:502` | Claim 초기 row / retry child OOS | A 일반 insert/retry, proof 발급 없음 |
| `db/intake_outbox.rs:514` | Claim 초기 row / retry child OOS | A 일반 insert/retry, proof 발급 없음 |
| [r2 handoff (175)](https://github.com/itismyfield/AgentDesk/blob/08e2336e47986d70d0cbe95e56215858377aed5c/src/db/intake_outbox/consent_handoff.rs#L175) | Release | A 구현 대상, 원본 row CAS |
| [r2 handoff (240)](https://github.com/itismyfield/AgentDesk/blob/08e2336e47986d70d0cbe95e56215858377aed5c/src/db/intake_outbox/consent_handoff.rs#L240) | Commit(rollback) | A 구현 대상, 원본 row CAS |
| [r2 handoff (308)](https://github.com/itismyfield/AgentDesk/blob/08e2336e47986d70d0cbe95e56215858377aed5c/src/db/intake_outbox/consent_handoff.rs#L308) | Commit(hold) | A 구현 대상, 원본 row CAS |
| `db/intake_outbox/worker_status.rs:164` | Claim / AdvanceStatus / pending rearm | A, stamp/live-owner fence 유지 |
| `db/intake_outbox/worker_status.rs:260` | Claim / AdvanceStatus / pending rearm | A, stamp/live-owner fence 유지 |
| `db/intake_outbox/worker_status.rs:385` | Claim / AdvanceStatus / pending rearm | A, stamp/live-owner fence 유지 — :385 함수는 cfg(test), runtime 아님 |
| `db/intake_outbox_delivery_proof.rs:186` | Drift(status), terminal settlement | A 기존 delivery/unknown/operator CAS, 변경 제외 |
| `db/intake_outbox_delivery_proof.rs:258` | Drift(status), terminal settlement | A 기존 delivery/unknown/operator CAS, 변경 제외 |
| `db/intake_outbox_delivery_proof.rs:294` | Drift(status), terminal settlement | A 기존 delivery/unknown/operator CAS, 변경 제외 |
| `db/intake_outbox_delivery_proof.rs:311` | Drift(status), terminal settlement | A 기존 delivery/unknown/operator CAS, 변경 제외 |
| `db/intake_outbox_force_fail.rs:93` | Drift(forceFail); child row OOS | A 기존 operator transaction, 변경 제외 |
| `db/intake_outbox_force_fail.rs:121` | Drift(forceFail); child row OOS | A 기존 operator transaction, 변경 제외 |
| `db/replay_disposition/write.rs:208` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write.rs:221` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write/hold_notice.rs:97` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write/hold_notice.rs:254` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write/settle.rs:12` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write/start.rs:151` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write/start.rs:235` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `db/replay_disposition/write/start.rs:299` | Drift(status/payload); replay metadata OOS | A 기존 replay receipt CAS, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record.rs:456` | 새 route row OOS | S→A owner transfer transaction, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record.rs:708` | Claim | A 호환 owner-fenced claim SQL, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record.rs:847` | Drift(status) | A stale pending retirement, spawned authority 아님 |
| `services/cluster/intake_router_hook/owner_record/cleanup.rs:223` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/custody.rs:117` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/custody.rs:221` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/custody.rs:244` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/custody.rs:376` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/custody.rs:386` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/custody.rs:438` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/plan.rs:62` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `services/cluster/intake_router_hook/owner_record/plan.rs:199` | Drift(status/stamp/payload); routing metadata OOS | S→A routing custody/cleanup/plan, 변경 제외 |
| `migrations/postgres/0052_intake_node_routing.sql:136–147` | updated_at OOS | 모든 UPDATE의 touch trigger. rollback 보존 대상에 updated_at을 잘못 넣지 않음 |
| `migrations/postgres/0136_request_replay_disposition.sql:127,208–234`; `0137_request_replay_exact_attempt.sql:7–40`; `0149_intake_outbox_replay_fence_update_when.sql:4–15` | replay guards/project OOS | 기존 trigger가 SQL을 거절할 수 있음. gate 삭제/우회/새 schema 없음 |
| `migrations/postgres/0166_execution_cleanup.sql:29–43` | attachment retention OOS | replay_disposition 변경 때 attachment bundle TTL만 늘림, proof/custody authority 아님 |
| `migrations/postgres/0080_intake_outbox_provider.sql:20,29,41,64` | historical boot migration OOS | migration-time provider 보정, runtime proof 복원 아님; 새 migration 없음 |

`intake_outbox_delivery_proof.rs`의 qualified UPDATE 4개(:186,258,294,311)를 빠뜨리지 않도록 후속 검색했다. routing/replay/force-fail writer가 owner advisory lock을 모두 취한다고 가정하지 않는다. SQL row lock/status/claim/marker CAS가 이 인접 경로와의 최종 fence다. owner transfer에는 별도의 owner advisory lock 직렬화가 필요하다.

### 2.3 입력 출처와 소유자 분류

| 입력 | 누가 proof를 만드나 / 어느 row인가 | 모델 / 거절·경계 |
|---|---|---|
| 성공한 worker claimed→accepted→spawned | 현재 private scope, r2는 기존 o_input의 private key를 가진 동일 scope. claim_owner는 worker의 restart token; target/provider/channel/stamp는 그 원본 row | stamped live case 기본 모델. copy/DTO/IntentRef만 가진 caller는 proof 생성 불가 |
| legacy `(owner_instance_id,owner_generation)=(None,None)` | 기존 worker가 발급, 같은 original row | current rollback/release의 legacy true 분기 유지; active owner 신설/강제 불가. 모델의 stamped-live 추상화 밖, PG legacy control 필수 |
| partial/invalid stamp, 빈 claim, retired/moved owner, stale payload/source | 정상 CAS가 거절하거나 이후 strict current-row 검사가 거절 | Drift / AbortStale / NoStaleProofWrite; half-stamp는 모델 밖 DB validator 시험 |
| direct/live mailbox·slash·queued submission | `hold_live:admission.rs:371–410`, mailbox source+queued_generation(:269–309); BeforeEffects 없음 | OOS mailbox workflow. row id나 request.intake_outbox_id가 proof가 되지 않음 |
| runtime/core 직접 호출, detached task, wrong request | scope 밖에는 task-local 없음; 다른 task로 상속하지 않음 | Take(FALSE)/DropForNormalExecution. cfg(unix) 밖에 proof API 신설하지 않음 |
| Applied consent release / 새 nonce / 재시작 inventory | persisted IntentRef+stable consent digest+durable marker; proof 발급 없음 | ReleaseOnce. receipt nonce는 다른 generation의 release 결과이지 새 claim 권한 아님 |
| operator force-fail+retry child | 다른 row id/attempt/parent, 새 worker CAS를 거쳐야 함 | forceFail Drift / child OOS. 이전 proof를 child에 이식하지 못함 |
| test-only for_test | cfg(test) helper만 raw row로 생성 | production unauthorized 경로로 세지 않으며 non-test compile에서 접근 실패해야 함 |


## 3. 검증 계획과 뮤턴트

PLAN이라 cargo/PG 실행은 없다. 이미 Current에서 봉쇄된 P1을 main bug로 보고 수정 전 RED를 꾸미지 않는다. d1은 구현하지 않은 설계여서 d1 RED는 모델/공개 API 가설의 반증으로 먼저 얻고, Current/r2는 보존 시험을 한다.

### 3.1 같은 crate non-test compiler probes — 완전한 API 조합

외부 integration test의 crate-private import 실패는 증거가 아니다. 기존처럼 임시 lib.rs sibling overlay를 **non-test cargo check --lib --message-format=json**으로 검사한 뒤 바이트 동일 원복한다. 부정 attack probe의 인수는 pool/row/request/deps뿐이며 key/proof를 이미 가졌다고 가정하지 않는다. 각 probe는 positive control로 실제 살아 있는 imports/signatures가 접근 가능함을 확인하고, 기대 오류만 판정한다. key/privacy는 cfg(test) for_test로 우회하지 않는다.

1. **d1 조합 그대로:** `return_before_record(pool, permit.into_before_effects())` 및 `handoff(pool, permit.into_before_effects(), …)`를 CAS-return API와 함께 검사한다. r2에는 그 CAS receipt API/변환이 **없음**을 명시적으로 검증한다. 제거된 심볼의 E0432/E0599는 이 한 항목의 API-absence 증거이고 다른 privacy probe를 대신하지 않는다.
2. **surviving CAS 조합:** 접근 가능한 기존 mark_spawned의 bool 결과에 `.into_before_effects()`를 연결해 actual rollback/hold 인수로 사용한다. E0599 또는 정확한 type 오류가 나야 하며, bare `handoff(permit)` 하나만 검사하지 않는다.
3. **r2 우회 조합:** actual capsule BeforeEffects, issuer::scope/take, WorkerScopeKey 타입, 기존 public take/DB consumer를 모두 import하는 positive control은 통과해야 한다. sibling이 `WorkerScopeKey(())` 또는 struct literal로 key를 만들어 synthetic scope에서 proof를 꺼내 보관하고 unscoped core effects 뒤 rollback하는 전체 body는 **key ctor privacy 오류(E0423/E0451 등)**로 실패해야 한다. unresolved module/import만 실패하면 무효다.
4. **root-cause visibility mutant:** `WorkerScopeKey(())`의 field만 pub(crate)로 넓히면 3번의 동일 body가 compile 성공하여 negative harness가 RED가 되어야 한다. scope/take는 이미 key-gated pub(crate)이므로 이 한 변경이 실제 우회를 복원한다. private-field guard를 문서로만 선언하지 않음.
5. **direct factory와 raw-value 우회:** `BeforeEffects(row)`/필드 literal/From<Row>/Default/Deserialize/clone 및 issuer::scope((),row,fut), issuer::scope(cas_bool,row,fut) 등은 각각 privacy/type/trait 오류. proof/key 재사용은 E0382. test helper는 non-test에서 부재. type reexport가 private tuple ctor를 공개하지 않는지도 검사.
6. probe마다 원인 하나만 실패하게 구성하고 restored byte SHA256을 확인한다. unsafe zeroed/transmute 등으로 모든 Rust 타입을 위조하는 경로는 Current와 같은 범위 밖이다.

### 3.2 실제 PG 경계와 effects 뒤 rollback 거절

- **Current/r2 preservation PG:** 실제 spawned row와 original request로 **scope 밖 실제 executor**를 실행하고 실제 provider/input adapter mock의 effect count=1을 확인한다(test_executor가 worker_entry/admission을 우회하는 stub이나 수동 counter만으로 대체하지 않음). mark_done 전에 row가 여전히 spawned이고 payload/claim/owner/marker가 같은 시점에서 actual public `take_before_effects(&request)`를 다시 시도한다. 반드시 None, 시험 driver의 rollback-attempt 결과는 RejectedMissingAuthority, rollback SQL 호출0, row byte/columns 그대로, effects1이어야 한다. 없는 proof를 for_test로 만들거나 DB 함수가 effects를 탐지해 false를 반환한다고 주장하지 않는다.
- **Scoped normal-entry PG:** 실제 worker scope→first-entry take→normal branch가 authority를 폐기한 뒤 effects를 내는 경로를 통과시킨다. 밖에서 늦은 take/rollback attempt가 거절되고 second execution0인지 확인한다. wrong request와 detached task control도 유지.
- **adversarial visibility-mutant PG:** non-test composition probe로 막힌 동일 sibling body를 key-wide mutant에서만 compile 가능한 fixture로 올린다. proof 없는 실제 effects 후 synthetic keyed scope에서 late take→rollback을 시도한다. “row pending 전환0/effects1” oracle은 RED여야 한다(필요하면 원본 재claim 후 effects2도 확인). 이 fixture는 정상 r2에서 의도적으로 compile 불가이므로, 정상 PG test에 for_test 우회를 섞어 GREEN인 것처럼 하지 않는다. normal compiler rejection + normal preservation PG GREEN + mutant compile/runtime RED를 별도 증거로 보고한다.
- **재시작/반복 claim PG:** before-record rollback1→same row reclaim→새 scoped proof, 동일 process claim_owner 재사용 control과 restart token 변경 control을 모두 확인한다. marker가 남아 두 번째 before-record rearm0; process loss 뒤 row/IntentRef/nonce로 proof 복원0. claimed sweep과 spawned 보존을 구분한다.
- 기존 T-consent 4개는 그대로: `consent_handoff_requires_exact_payload_source_and_live_owner_pg`, `consent_handoff_stale_before_effects_proof_never_changes_row_pg`, `consent_before_record_rollback_consumes_exact_authority_and_preserves_bookkeeping_pg`, `consent_before_record_rearm_is_once_across_concurrency_and_reclaim_pg`.
- 기존 scope 시험 `before_effects_authority_is_exact_consumed_and_scoped_pg`와 admission :1488–1758의 record 전/후 실패 경계도 유지. record 시도/publish 후에는 rollback0, missing/different Consent/unresolved guard 제거 mutant RED.
- owner 전환은 actual promote retirement+transfer 경합으로 stale hold 보존 시험. 기존 main도 GREEN이며 retire_open_on_tx 제거 mutant RED. raw owner-only SQL을 production 재현이라고 보고하지 않음.

기존 exact-row/marker/one-rearm/bookkeeping guard 제거 mutant와 wire(27-slot digest, serde deny_unknown_fields, schema1–3, stable consent/nonce, Returned/Adopted unresolved) control도 유지한다. 새 durable marker/effects ledger/DB migration/추가 rearm 정책은 없다.

cargo는 PATH shim, DEBUG=0/STRIP=none의 dev/test profile, CARGO_TARGET_DIR 변경0. PG는 throwaway + AGENTDESK_REQUIRE_PG=1, 기본 병렬, --test-threads=1 금지. 실제 이름/수/failed/skip와 binary SHA256/로그를 구현 REVIEW_READY에 남긴다. compiler probe가 0개 실행되거나 PG skip이면 PASS가 아니다. 구현 뒤 static prepush/source-scan failed0/self-file clippy warning0을 갖춰야 REVIEW_READY다. PLAN에서 이를 실행했다고 주장하지 않는다.

## 4. 크기·리뷰 집중 항목·분할선

| production 파일 | 예상 순 코드 | r2 책임 |
|---|---:|---|
| src/domain/intake_consent.rs | +160 | 실제 중립 wire/projection/digest/binding |
| src/domain/intake_consent/decision.rs | +60 | pure unresolved/marker/source 판정 |
| src/domain/mod.rs | +1 | 등록만 |
| src/db/intake_outbox/consent_projection.rs | +65 | 실제 row→projection |
| src/intake_consent_authority.rs | +40 | 실제 opaque proof 정의와 keyed issuer child 선언 |
| src/services/cluster/intake_worker/consent_scope.rs | +65 | key-gated service task-local scope/take |
| src/lib.rs | +2 | Unix capsule 등록만 |
| src/db/intake_outbox.rs | +2 | mapper 등록만 |
| src/db/intake_outbox/consent_handoff.rs | -35 | FS/pure 이동, proof 실제 정의 경로 변경 |
| src/services/cluster/intake_worker/o_input.rs | -15 | private key, 기존 private wrapper/filter, proof/slot 본체 이동 |
| src/services/cluster/intake_worker.rs | +2 | key typename reexport, 기존 worker 흐름 유지 |
| src/services/discord/router/herdr_consent/worker_handoff.rs | +85 | old call 자리 FS load→DB helper |
| src/services/discord/router/herdr_consent/admission.rs | +8 | private helper 등록/호출 |
| src/services/session_host/herdr/policy/consent.rs | -9 | IntentRef 실제 정의 이동/호환 reexport |
| src/services/session_host/herdr/policy/consent/settlement.rs | +9 | domain predicate 위임 |
| scripts/audit_maintainability/layers.toml | +10 | 실제 capsule/issuer/domain ownership 분류 |
| 소계 / 보수 여유 | +450 / +130 | 추가 파일 없음 |
| **합계** | **16 / +580** | 범위 +400~750 |

d1의 worker_status.rs 수정과 worker_status/authority.rs 신설 계획을 제거(-2파일)하고, actual proof capsule/service scope child/lib 등록 3파일과 ownership mapping 1파일을 추가(+4파일)해 16파일이다. CAS receipt/API는 없어지고 private key/capsule 경계 및 conditional imports 감사 여유가 늘어 d1 +520보다 +60을 잡았다. 시험/공식 생성 목록은 별도 cap 계산이며 손으로 baseline을 올리지 않는다. 1,372줄 intake_outbox.rs는 등록만, 새 독립 책임은 전용 파일에 둔다. 기존 worker 963줄에는 key typename wiring만 더한다.

16파일·+580은 측정 구현 diff가 아니다. +1200 또는 30파일이 예상되거나 capsule child 배치가 더 큰 module relocation을 요구하면 중단하고 별도 설계한다. 안전 분할은 pure wire/projection을 기존 소비자와 함께 먼저 옮긴 조각, 이어 proof 봉인+issuer+service load+DB consumer를 동시 활성화한 조각이다. naked factory나 dormant public receipt를 중간 단계로 만들지 않는다.

| 리뷰 집중 항목 | r2에서 막는 방법 / 남은 확인 |
|---|---|
| P1 효과 뒤 rollback | no CAS receipt; key ctor private; scope/take key mandatory; full composed API compile failure + current/r2 PG preservation + visibility mutant RED |
| 권한 범위 | 위 Current/d1/r2 caller 표. CAS 성공 caller를 권한자로 재정의하지 않음; key/proof 생성·변환 모든 API의 visibility 명시 |
| 경계 시각 | 기존 private scope에서 key 생성, same slot issue, first entry take, old by-value handoff 지점 FS 관측. FS latest-at-commit은 보장하지 않음 |
| 동시성/reclaim | actual same row, repeatable claim_owner, epoch/serial 별 proof, marker once와 owner retirement coupled CAS. 새 자동 rearm0 |
| 문서로만 선언한 가드 | private field의 compiler 오류 원인 확인, issuer 공개 surface 전수표, actual key widening 원인 제거 mutant. Capsule child layer 분류도 독립 리뷰 |
| 옛 형식 복원 | wire/digest/null slot/marker/schema/Returned/Adopted 의미 유지, #7651 legacy 파일 변경0 |
| 잘못된 0/PASS | model state count N/A, nine expected cfg는 예측; old 4,372 재사용 금지. API absence probe와 privacy probe 구분, skip/0-test 불인정 |
| 모델 차이 | Boundary-controlled Current/D1/R2 행동 목록과 구체 P1 witness. 실제 state counts/trace는 Mac에서 측정 |

## 5. 활성 ref 겹침

fetch/read-only ref 및 merged PR head 조사는 이번 레그에서 갱신했다. 원격 ref가 존재한다는 이유만으로 실행 중인 레인이라고 단정하지 않으며, main/merged PR head 조상·superseded ref를 제외한 보수적 활성 후보를 표로 남긴다. squash 상속 diff는 가장 가까운 merged-head/main merge-base로 제외한다. 다른 worktree, 특히 `/home/adk/adk/wt/i7353-bundle4`는 접근/편집하지 않는다.

원격 snapshot 124 ref: 105 merged/main 조상 제외, 13 superseded 제외, 보수적 활성 후보 6개(같은 head alias 포함). origin/main fetch snapshot은 `1fe3a57d266e1e7fe384796d22a925076f1d99fc`다.

| ref / head | patch 기준 / 파일 수 | r2 제안 파일 겹침 | 조정 |
|---|---|---|---|
| `refs/heads/feat/i7353-bundle4` / `ffe1a63fc60767fd5a7d39aeeb78e48f9b3b8eb3` | `5cf9d2909ed251c82dd7ec53e8fb8a79d1aa8a50` / 110 | `docs/statecharts/README.md`, `docs/statecharts/expected.tsv`, `scripts/audit_maintainability/layers.toml`, `src/db/intake_outbox.rs`, `src/domain/mod.rs` | 착지 active; 공유 등록/ownership/index/manifest는 구현 전 직렬 조정, worktree 접근0 |
| `refs/heads/fix/i7651-b2-p3b` / `0c196080d219fd33d3415ff91630e8b40c9bf2d4` | `685992a136405203d6e8678912fc3310a944d84a` / 30 | `docs/statecharts/expected.tsv` | 공유 docs 행은 coordinator merge; production 범위는 표대로 예약 |
| `refs/review/i7353-bundle4-1` / `ffe1a63fc60767fd5a7d39aeeb78e48f9b3b8eb3` | `5cf9d2909ed251c82dd7ec53e8fb8a79d1aa8a50` / 110 | `docs/statecharts/README.md`, `docs/statecharts/expected.tsv`, `scripts/audit_maintainability/layers.toml`, `src/db/intake_outbox.rs`, `src/domain/mod.rs` | 착지 active; 공유 등록/ownership/index/manifest는 구현 전 직렬 조정, worktree 접근0 |
| `refs/review/i7353-s18-0` / `1cad7f32644496c60a118cb72d9b4ea95a2a7ed0` | `92ef5b00b205679965c24c5c017f7237e8b5d57f` / 14 | `docs/statecharts/expected.tsv` | 공유 docs 행은 coordinator merge; production 범위는 표대로 예약 |
| `refs/review/i7353-s4-07-0` / `4bf484ed8363bdbdc7734e21013bd157ffc43bd6` | `5cf9d2909ed251c82dd7ec53e8fb8a79d1aa8a50` / 31 | `docs/statecharts/README.md`, `docs/statecharts/expected.tsv`, `docs/statecharts/intake-consent/IntakeConsent.tla`, `docs/statecharts/intake-consent/IntakeConsentBookkeeping.cfg`, `docs/statecharts/intake-consent/IntakeConsentCurrent.cfg`, `docs/statecharts/intake-consent/IntakeConsentForge.cfg`, `docs/statecharts/intake-consent/IntakeConsentOwnerOnly.cfg`, `docs/statecharts/intake-consent/IntakeConsentProposed.cfg`, `docs/statecharts/intake-consent/IntakeConsentReuse.cfg`, `docs/statecharts/intake-consent/IntakeConsentStale.cfg`, `docs/statecharts/intake-consent/design.md`, `docs/statecharts/intake-consent/intake-consent.chart.mmd`, `docs/statecharts/intake-consent/intake-consent.report.md`, `docs/statecharts/intake-consent/intake-consent.view.mjs`, `scripts/audit_maintainability/layers.toml` | 우리 d1 선행 ref, 외부 레인 아님. r2 대체 |
| `refs/review/i8057-h2-0` / `dfba87a0ae2a576507f0863e81cfb8f1ead8bfdb` | `289f41b1d0f48fab0dc426e652308eced4326fce` / 19 | `docs/statecharts/README.md`, `docs/statecharts/expected.tsv` | 공유 docs 행은 coordinator merge; production 범위는 표대로 예약 |

공유 등록/manifest/index는 구현 전 coordinator가 직렬 조정한다. 이번 레그는 domain/lib/DB production parent를 수정하지 않고 statechart 문서만 갱신한다. #7651 legacy_return, herdr_gate 및 migration/return 파일은 production 쓰기 범위 밖이다. main에 이미 충족한 owner retirement/직접 scope 발급 guard는 위 SHA·좌표로 표시하고 재구현하지 않는다.

## 증거 목록

- logs `/home/adk/adk/lane-runs/i7353/logs/s4-07-2/`; d1 source-only 진입점 조사 자료는 production SHA가 같으므로 재사용하고 새 visibility/권한 경계를 이 문서에 추가했다.
- review-s4-07-d1-pro.out.md 원문 P1/P2, 최초 precedence 수정 commit e8f13ac6a744b27bf85da308c8be5ea40766904c.
- 모델/9 cfg/view/chart/report, Current/D1Public/Proposed 분리, 기존6+신규1 invariant 및 PROPERTY2; WSL SC_RUN=mac-needed. 모든 결과/상태 수는 새 실행 전 N/A.
- remote-refs.txt/pr-heads.json/overlap-audit.json: 새 fetch 뒤 겹침. 전수표의 기존 source SHA 불변, 생산 코드 diff0.
- non-test compiler probe/PG/cargo/clippy/source-scan/static prepush 실행0(PLAN). 환경과 실제 파일/cap/push/read-back 증거는 r2 보고서에 남긴다.
