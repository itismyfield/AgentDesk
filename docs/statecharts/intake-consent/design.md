# #7353 S4-07 설계 1 — consent 관측과 소비형 authority

- 레그: PLAN 전용. production 변경 0, 코드 스파이크·cargo·PG 실행 0.
- 코드 기준: `08e2336e47986d70d0cbe95e56215858377aed5c`, `feat/i7353-s4-07`.
- fetch 뒤 main: `1fe3a57d266e1e7fe384796d22a925076f1d99fc`. #8076은 2026-11-11 04:23:10 UTC에 MERGED였다(브리프의 미머지 정보보다 최신). S4-03/OwnerIdentity는 main에도 있고, 이 worktree에는 S4-06 CLEAN head도 포함한다. main merge/rebase를 하지 않았다.
- 모델 조사/초안: 04:29~04:44 UTC, 운영 진입점 반례 대조/수정 완료 04:53 UTC; cfg/manifest 정적 파싱 수행. 30분 모델링 정지선 이내. `SC_RUN=mac-needed`; TLC/SANY 실행·상태 수·GREEN 판정 없음.
- 정본 계획 `plan-s4-1.md` S4-07(:153), 권한 주의(:177), 소비 좌표(:58), 타입(:95–98), T-consent(:202)를 이 SHA의 아래 좌표로 재측정했다.
- 선택: **안 B, DB adapter가 봉인된 capability를 소유하고 domain은 중립 값·순수 판정을 소유**. 예상 production **14파일 / +350~650 순 코드**, 계획값 **14/+520**. hard cap 30/+1500, PLAN 중단선 +1200.
- 현행 hold에는 직접 active-owner query가 없지만 production owner 전환은 같은 transaction에서 open outbox row를 먼저 retire한다. 이 결합을 코드 좌표로 확인해 모델과 보존 시험에 반영했다. 새 active-owner query는 추가하지 않고 발급·take·소비·FS 관측의 시점도 바꾸지 않는다. 독립 설계 리뷰 전 구현하지 않는다.

## 0. 상태 차트와 불변식

기존 `docs/statecharts/execution-authority/ExecutionAuthority.tla`의 owner/permit 갱신·custody 경합, `input-single-path/InputPath.tla`의 첫 키/stop·restart/boot, `InputNativeStop.tla`의 native authority를 읽었다. 두 폴더의 TLA/Markdown에 `consent|before.?effects|before.?record`를 검색한 결과 해당 행동은 없었다. 따라서 기존 전이를 복제/교체하지 않고 README 레이아웃에 맞춰 `docs/statecharts/intake-consent/`를 추가한다. 기존 모델의 gateway lease, host permit, native stop authority와 before-effects capability를 같은 권한으로 합치지 않는다.

| 규칙 | 모델 정의 | 코드/제안의 의무 |
|---|---|---|
| I1 발급 권한 없는 caller의 proof 생성 경로 0 | INVARIANT `NoUnauthorizedIssue`: 발급한 proof에는 spawned CAS 성공 이력이 있다 | private tuple, 봉인된 CAS 영수증, 무 Clone/Default/Deserialize/From<Row>; 같은 crate의 비인가 형제 모듈 컴파일 실패 검사 |
| I2 한 proof는 한 번만 소비 | INVARIANT `ProofConsumedOnce`: slot/held는 소비 0, spent는 정확히 1, 최대 1 | task-local `Option::take`; 불일치도 slot을 비운 뒤 폐기; DB/rollback 인수는 소유값; 정상 경로·실패·process loss는 drop. CAS가 반드시 한 번 성공한다는 뜻은 아니다 |
| I3 stale proof로 row 변경 0 | INVARIANT `NoStaleProofWrite`: commit 시 payload/source/claim/owner generation/status/live owner 모두 일치 | 현재 CAS 가드와 owner 전환의 row retirement 결합 보존. stale payload/claim/generation/terminal row를 변경하지 않으며, owner만 바꾸는 운영 가정 밖 writer는 진단 뮤턴트로 분리 |
| I4 rollback이 정확한 authority를 소비하고 bookkeeping 보존 | INVARIANT `RollbackExactAndBookkeeping`, 보조 `ReleaseOnce` | record 시도 전만 rollback; marker 없음·live owner·정확한 payload/stamp; 원본 row만 pending, retry/completed/created/TTL 유지, rearm marker 영구 보존 |
| 멈춤 없음 | PROPERTY `ProofEventuallySettles`, `TransactionEventuallyEnds` | 각 worker의 활성 내부 단계와 release의 약한 공정성. 오류/폐기는 완료로 인정. 영구 DB 장애·무한 lock 점유·crash-stranded Spawned 자동 재시도까지 약속하지 않음 |

Current cfg는 현재 구조, Proposed cfg는 제안 구조이며 정상 production owner 전환의 coupled row retirement를 둘 다 반영한다. 다섯 mutant cfg(Forge/Reuse/Stale/Bookkeeping/OwnerOnly)는 해당 가드/결합 제거를 예측한다. `expected.tsv`는 **미실행 예측**이며 Current/Proposed의 규칙 위반은 예상하지 않는다. manifest를 통과 실적으로 인용하지 않는다. `.chart.mmd`는 손으로 작성한 설계도다.

C-owner 후보를 실제 코드에 재생한 결과: 단독 `transfer_owner_in_tx`는 outbox row를 바꾸지 않지만 production 호출자 `src/services/cluster/intake_router_hook/owner_record/custody.rs:133–210`는 같은 owner lock/transaction에서 **:190–192 retire_open_on_tx → :201–203 transfer_owner_in_tx** 순서다. `retire_open_on_tx:323–324`는 NOT replay_only인 모든 pending/claimed/accepted/spawned row를 FOR UPDATE로 잡고, :383–392는 receipt 유무에 따라 status를 unknown으로 바꾼다(후자는 `src/db/replay_disposition/write.rs:217–228`). replay_only=true row는 schema CHECK(`migrations/postgres/0136_request_replay_disposition.sql:15–25`)와 writer(`src/db/replay_disposition/write/start.rs:151–158`)가 unknown으로 제한하므로 정상 spawned proof row에서 제외되지 않는다. 따라서 전환이 commit된 뒤 old hold의 `status='spawned'` CAS(`consent_handoff.rs:308`)는 실패한다. **이 후보는 현재 production bug/실제 반례로 확인되지 않았다.**

main `1fe3a57d266e1e7fe384796d22a925076f1d99fc`의 같은 custody/replay/hold 좌표가 이미 이 운영 경로의 stale-owner 차단을 충족한다. 재구현하지 않는다. 모델의 OwnerOnly cfg는 이 결합을 제거한 진단 뮤턴트이며 기준 cfg의 운영 전제가 아니다. 상위 retirement 없이 직접 owner table만 변경하는 기존 테스트 helper 또는 임의 SQL은 하위 hold API가 독자적으로 막는다고 주장하지 않는다. 이 잔여는 이번 변경으로 넓히지 않으며, 미래 production caller가 생기면 owner-valid guard/범위의 별도 설계가 필요하다. 기존 live_owner test(:185–255)는 release의 query만 검증하므로 상위 결합 보존 시험을 추가한다.

모델에서 `ticket`은 현행의 성공한 spawned-CAS 제어 흐름을 제안에서 비복제 영수증으로 표현한 것이다. `cas`, `attempted`는 모델의 이력 변수이고 production ledger/schema가 아니다. 코드의 private 필드/가시성은 모델의 가정이므로 Rust 컴파일 검사로 따로 증명한다. 현행/제안이 같은 Issue/Take/ObserveAndMove 위치를 갖는 것은 모델의 추상 대응이며 비트 단위 구현 동등성 검증 결과가 아니다.

## 1. 현재 경계와 순서

### 1.1 타입·가시성·wire

| 대상 | 현재 file:line | 현재 경계 / 목표 |
|---|---|---|
| `BeforeEffects(IntakeOutboxRow)` | `src/services/cluster/intake_worker/o_input.rs:73–87` | 타입 pub(crate), tuple field private. Clone/Copy/serde/Default 없음. `row(&self)`는 pub(crate) 빌림만. `for_test`만 cfg(test) 임의 생성 가능. domain에 DB row를 들여오지 않음 |
| 실 발급 | 같은 파일 `scope_before_effects:107–118`, 호출 `execute_before_effects:121–138` | 두 함수는 private/pub(super). row clone의 status만 Spawned로 바꾸고 task-local scope에 1개. worker `intake_worker.rs:458–480` 성공 분기 뒤만 호출 |
| take | 같은 파일 `BEFORE_EFFECTS:89–92`, `take_before_effects:96–104` | task-local Option을 먼저 take하고 원래 `IntakeRequest` 전체 동등성으로 filter. wrong request도 proof 복원 없음. worker reexport `intake_worker.rs:37–38` |
| 첫 소비자 | `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:49–74` | executor 첫 진입 :58에서 take; consent worker에 move; consent가 없으면 drop 후 replay/input mutation. 정상 경로에서도 authority가 남지 않음 |
| DB 소비 | `src/db/intake_outbox/consent_handoff.rs:184–188,246–254` | rollback/hold 둘 다 proof by value. proof 복제·재발급 API 없음. release :107–115는 proof 재생성이 아니라 durable custody receipt 소비 |
| `IntentRef` wire | `src/services/session_host/herdr/policy/consent.rs:73–82` | serde deny_unknown_fields; `store,id,provider,channel:u64,source_message:u64,fifo:u64` 그대로 domain 소유. 필드명/순서/숫자 타입/JSON 값 변경 없음. persisted reference는 authority가 아님 |
| `Consent` | 같은 파일 `:128–154` | schema/key/preview/owner/bot/state/nonce/intents/history/settlements를 가진 runtime store 값. 닫힌 의존인 ServerWitness 등을 이번에 통째 이동하지 않음 |
| unresolved 판정 | `src/services/session_host/herdr/policy/consent/settlement.rs:62–81` | `(store,id)`로 settlement 조회, None/Returned/Adopted만 unresolved. Released/Discarded 등은 제외. domain 순수 predicate로 본체 이동, service 메서드는 기존 runtime 객체에서 매핑/위임 |
| Store/load | `consent.rs:215–219,221–290`; `durable.rs:193–203` | ConsentStore private Directory tuple, normalized key + read_stable + decode. Directory의 파일 shared lock은 read 안에서 풀림. DB owner/row lock과 동시에 잡는 장기 FS lock 아님. service가 load 호출을 소유 |
| payload/marker | `consent_handoff.rs:10–104` | payload JSON 배열 :64–93의 **27개 슬롯과 끝 Null**을 유지해 SHA256. custody/rearm prefix v1, deny_unknown_fields, consent stable key와 nonce의 역할 구분 유지 |

### 1.2 순서 고정

| 경로 | 실제 순서(기준 SHA) | 제안 뒤 |
|---|---|---|
| worker | `worker_status.rs:145–181` claim/commit → `intake_worker.rs:434` accepted CAS/commit → :458 spawned CAS/commit → :472 executor wrapper → `o_input.rs:111–118` issue → `worker_entry.rs:58` take | SQL·commit 동일. 성공 CAS 제어 흐름을 receipt로 전달. **BeforeEffects는 여전히 기존 scope 위치에서만 생성** |
| hold | `admission.rs:475` recording=true → :477 record(Directory exclusive lock/atomic publication, admission :228–242) → :481 proof move → DB `:255–267` FS load·full Consent equality·unresolved/exact intent → :273 begin → :275 owner advisory lock → :283 row FOR UPDATE → :288 payload → :291 marker → :308 CAS → :312 commit → admission :492 card effect | by-value service helper가 기존 :481 호출 자리를 차지하고 FS 관측/동등성 검사를 그대로 먼저 실행. DB는 그 뒤 begin/lock/read/CAS/commit. owner/row lock과 현행 stamp/marker/status CAS, coupled retirement의 순서를 보존 |
| pre-record rollback | `admission.rs:499–503` `!recording && proof.take()` → DB :188 expected 확인 → :192 begin → :194 owner lock → :200 row lock → :208–226 exact row/marker/live owner → :228 rearm marker(time sample 포함) → :240 pending CAS → :242 commit | 같은 consume·시각 샘플·transaction 범위. generic return_claimed_to_pending으로 치환하지 않음 |
| release | DB :117 begin → :119 owner lock → :125 row lock → :131–145 stable source/payload/consent 확인 → :146 released면 commit no-op → :151–167 stamp/status/live owner → :169 marker released → :175 pending CAS → :180 commit | 동일. 이미 released인 idempotent branch를 active-owner 재검사 뒤로 옮기지 않음 |
| wake/ack | `custody.rs:236–255` DB release commit 뒤 FS custody receipt commit → :258–268 one wake → `settlement.rs:284–286` release, wake, acknowledge | release receipt·wake·ack는 범위 밖, 순서·실패 보존 유지 |

FS load를 transaction 전으로 새로 당기는 변경이 아니다. **현재도 FS 관측이 begin 전에 있다.** 새 helper가 store.load를 미리 admission.check 시점에 캐시하거나 DB lock을 잡은 뒤 호출하면 이 설계 위반이다. 관측 후 파일이 바뀌는 기존 간극을 cross-store atomicity로 숨기지 않는다. 파일 변경을 CAS까지 봉쇄하려면 FS/PG lock 순서와 publication protocol을 별도 설계하고 멈춘다.

### 1.3 진입점 전수표 — proof/consent/재시작/인접 제어

표의 S=service orchestration, D=domain pure values/predicate, A=DB CAS adapter다. src/ 상대 경로로 표시한다. 모델 밖은 OOS를 명시하며 새 proof 발급 근거로 쓰지 않는다.

| ID | file:line / 진입점 | 모델 값·행동 | 제안 경계·입력 출처 |
|---|---|---|---|
| E01 | `services/cluster/intake_worker.rs:434,458,472` | AdvanceStatus → ticket → Issue | A의 successful spawned receipt → S의 기존 scope. claim owner는 이 worker restart token |
| E02 | `services/cluster/intake_worker/o_input.rs:75,79,84,107–118` | Issue, snap, proof=slot | A 봉인 타입/빌린 row로 이동. cfg(test) ctor는 production 부재 유지 |
| E03 | 같은 파일 `:96–104`, `intake_worker.rs:37` | Take(TRUE/FALSE), spent | S task-local take·전체 request 동등성 유지. wrong source도 slot 복원 0 |
| E04 | `services/discord/router/message_handler/intake_turn/worker_entry.rs:58–74` | Take → Entry / DropNormal | S 최초 entry, 일반 executor 요청은 proof 없음. consent 없을 때 먼저 drop |
| E05 | `services/discord/router/herdr_consent/admission.rs:438–491` | RecordAttempt → ObserveAndMove | S만 check/record/load, A에 proof 소유값 + D checked binding 전달 |
| E06 | 같은 파일 `:499–509` | RollbackMove | recording 시작 전만 같은 proof를 A rollback에 move, 다른 row id fallback은 authority 아님 |
| E07 | 같은 파일 `:215–242`; `consent.rs:304–334,479–499` | attempted / FS record | S Store.question/append, source별 참조만 저장. payload/upload 복사 없음 |
| E08 | `db/intake_outbox/consent_handoff.rs:184–243` | LockOwner, ReadForUpdate, PrepareCAS, Commit(rollback) | A가 same row/stamp/marker/live owner 재확인, bookkeeping 유지 |
| E09 | 같은 파일 `:246–315` | ObserveAndMove → locked hold CAS | FS 관측은 S helper로만 이동. A는 opaque proof와 checked values, 기존 CAS/상위 retirement 결합 유지 |
| E10 | 같은 파일 `:107–180`; `services/discord/router/herdr_consent/custody.rs:205–268` | Release / releases<=1 | S exact durable release receipt → A. nonce만으로 row 생성/증식/새 proof 발급 없음 |
| E11 | `services/session_host/herdr/policy/consent/settlement.rs:62–105,224–288` | unresolved eligibility / release boundary | D predicate, S Applied/preview/settlement guard·ack는 유지. returned/adopted는 unresolved지만 releasable과 다름 |
| E12 | `services/discord/router/herdr_consent.rs:24–29,79–90,388–407` | FS inventory / release replay | S store factory/load_all/load_each. 재시작에 persisted IntentRef로 proof 복원 0 |
| E13 | `services/discord/router/herdr_consent/startup.rs:52–87,91–150` | Restart; boot recovery OOS | S local node/user/socket/session 필터, BootState claim/complete, Accepted interruption → existing recovery. 새 Issue 없음 |
| E14 | `services/session_host/herdr/policy/consent/engine.rs:44–75,129–171,272`; `workflow.rs:93,137,158,222` | FS observation / consent revision OOS | S 복구·click·retry의 기존 load 및 stale-decision guard, DB가 callback으로 호출하지 않음 |
| E15 | `config/session_hosts/migration/consent_holds.rs:26–30,61,90` | unresolved inventory OOS | S migration blocking/return 판정. legacy 파일 편집 제외, proof 만들지 못함 |
| E16 | `services/discord/router/herdr_consent/return_input.rs:8,47–71,118–201` | returned/adopted unresolved OOS | S owner/provider/channel 확인 + exact intent FS settlements. before-record rollback과 다른 legacy-return workflow, 변경 제외 |
| E17 | `services/cluster/intake_worker.rs:269–311,366–381,406–413,529–566` | AbortStale / clean drain vs Restart | S pre-accept cancel은 claimed rearm, accepted 뒤에는 lifecycle drain. boot에 Spawned proof 재발급 금지 |
| E18 | `services/discord/commands/stop.rs:52–105`; `services/session_host/herdr_gate.rs:86–92,1025–1045` | Cancel/Terminate OOS | 다른 native/Herdr gate authority, BeforeEffects 소비/발급 없음. #7651 gate/legacy 파일은 읽기 증거만, 편집 제외 |
| E19 | `services/discord/commands/control.rs:510–559,715–755`; `control/clear_report.rs:128–192` | mailbox clear/cancel OOS, FS eligibility | clear 후 원본 source+queued generation 부재를 확인하고 **mailbox intent만** acknowledge_discard. outbox proof가 되지 않음 |
| E20 | `db/intake_outbox_force_fail.rs:55–121` | Drift(forceFail) | 운영자가 row lock으로 terminalize하고 별도 child INSERT; 새 child는 다른 row/attempt. 이번 설계가 새 운영 권한을 호출/추가하지 않음 |
| E21 | `services/maintenance/jobs/db_retention.rs:1–4,463–535` | OOS retention observation | intake_outbox는 후보 count만, 삭제 없음. 모델/제안에서 delete나 TTL 초기화 추가 0 |
| E22 | `db/intake_outbox/worker_status.rs:379–414`; `services/cluster/intake_router_hook/owner_record.rs:814–819` | pending reclaim / AbortStale | 실제 stamped sweep은 claimed만. cfg(test) legacy sweep을 runtime 정리로 활성화하지 않음 |
| E23 | `services/cluster/intake_router_hook/owner_record.rs:304–322,382–435` | Drift(retire), live | production promote는 같은 tx에서 old open row retire→owner transfer. OwnerOnly 진단 뮤턴트는 이 결합을 제거한 운영 가정 밖 trace |
| E24 | `services/cluster/intake_worker/consent_scope_tests.rs:4–43`; `db/intake_outbox/consent_handoff_tests.rs:260–275` | test-only issue / take mismatch | test ctor·test wrapper만 존재. prod 발급 경로 집계와 분리, 테스트 0건/skip으로 I1 증명하지 않음 |

### 1.4 Store load 전수표

`git grep ConsentStore`의 production 소비자는 DB handoff, router, startup, config consent_holds, engine/workflow/settlement였다. `.load(`, `.load_all(`, `.load_each(`, `.unresolved(` 호출도 별도 로그로 고정했다. cfg(test) 시험들은 raw proof-consumers 로그에 남겼으며 runtime으로 계산하지 않는다. 각 행은 FS 값/ObserveAndMove 또는 명시한 OOS다.

| file:line | 관측 진입 / 모델 | 제안 경계 |
|---|---|---|
| `db/intake_outbox/consent_handoff.rs:255–259` | 정확한 expected snapshot+unresolved intent / ObserveAndMove | 이 1개 load만 새 S worker_handoff로 이동, begin보다 앞 |
| `services/discord/router/herdr_consent.rs:89,407` | load_all / load_each inventory, OOS | S 유지 |
| `services/discord/router/herdr_consent/startup.rs:61` | boot interrupt inventory / Restart 경계 | S 유지 |
| `services/session_host/herdr/policy/consent/engine.rs:149,171,272` | card reserve/retry/apply 최신 관측, OOS | S 유지 |
| `services/session_host/herdr/policy/consent/workflow.rs:93,137,158,222` | decision/retry/boot 최신 관측, OOS | S 유지 |
| `services/session_host/herdr/policy/consent/settlement.rs:191,195–209,215,269` | load_each/load_all/unresolved/release snapshot | S 유지, unresolved predicate만 D |
| `config/session_hosts/migration/consent_holds.rs:61,90` | unresolved inventory, OOS | S 유지, 편집 제외 |
| `services/discord/commands/control/clear_report.rs:182–192` | unresolved inventory 후 mailbox discard, OOS | S 유지; 새 proof 발급 없음 |
| `services/session_host/herdr/policy/consent.rs:282–290`; `durable.rs:193–203` | 모든 load의 leaf normalize/read_stable/decode | S Store/Directory 유지. DB 안에 trait/closure 주입하지 않음 |

### 1.5 같은 outbox row의 다른 SQL writer 전수표

`git grep`의 UPDATE/INSERT/DELETE를 시작점으로 multiline 및 `public.intake_outbox`를 추가 검색했다. 공식 cap 도구의 test-module 판별과 inline test-block 제외를 사용해 후보 35곳을 얻었다(명시적 cfg(test) leaf 1곳은 아래 표시). schema trigger/boot migration도 별도 행에 적었다. 단순 SQL 패턴의 개수만으로 0/PASS를 선언하지 않는다. 각 다른 writer가 존재할 수 있으므로 proof 검사는 transaction 안에서 재확인한다.

| file:line | 모델 값·행동 | 제안 경계 / 분류 |
| `db/intake_outbox.rs:121` | Claim 초기 row / retry child OOS | A 일반 insert/retry, proof 발급 없음 |
| `db/intake_outbox.rs:502` | Claim 초기 row / retry child OOS | A 일반 insert/retry, proof 발급 없음 |
| `db/intake_outbox.rs:514` | Claim 초기 row / retry child OOS | A 일반 insert/retry, proof 발급 없음 |
| `db/intake_outbox/consent_handoff.rs:175` | Release | A 구현 대상, 원본 row CAS |
| `db/intake_outbox/consent_handoff.rs:240` | Commit(rollback) | A 구현 대상, 원본 row CAS |
| `db/intake_outbox/consent_handoff.rs:308` | Commit(hold) | A 구현 대상, 원본 row CAS |
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

### 1.6 입력 출처와 소유자 분류

| 입력 | 누가 proof를 만드나 / 어느 row인가 | 모델 / 거절·경계 |
|---|---|---|
| 성공한 worker claimed→accepted→spawned | 현재 private scope, 제안은 성공 CAS receipt를 가진 scope. claim_owner는 worker의 restart token; target/provider/channel/stamp는 그 원본 row | stamped live case 기본 모델. copy/DTO/IntentRef만 가진 caller는 proof 생성 불가 |
| legacy `(owner_instance_id,owner_generation)=(None,None)` | 기존 worker가 발급, 같은 original row | current rollback/release의 legacy true 분기 유지; active owner 신설/강제 불가. 모델의 stamped-live 추상화 밖, PG legacy control 필수 |
| partial/invalid stamp, 빈 claim, retired/moved owner, stale payload/source | 정상 CAS가 거절하거나 이후 strict current-row 검사가 거절 | Drift / AbortStale / NoStaleProofWrite; half-stamp는 모델 밖 DB validator 시험 |
| direct/live mailbox·slash·queued submission | `hold_live:admission.rs:371–410`, mailbox source+queued_generation(:269–309); BeforeEffects 없음 | OOS mailbox workflow. row id나 request.intake_outbox_id가 proof가 되지 않음 |
| runtime/core 직접 호출, detached task, wrong request | scope 밖에는 task-local 없음; 다른 task로 상속하지 않음 | Take(FALSE)/DropNormal. cfg(unix) 밖에 proof API 신설하지 않음 |
| Applied consent release / 새 nonce / 재시작 inventory | persisted IntentRef+stable consent digest+durable marker; proof 발급 없음 | ReleaseOnce. receipt nonce는 다른 generation의 release 결과이지 새 claim 권한 아님 |
| operator force-fail+retry child | 다른 row id/attempt/parent, 새 worker CAS를 거쳐야 함 | forceFail Drift / child OOS. 이전 proof를 child에 이식하지 못함 |
| test-only for_test | cfg(test) helper만 raw row로 생성 | production unauthorized 경로로 세지 않으며 non-test compile에서 접근 실패해야 함 |

## 2. 두 안 비교와 선택

| 안 | 흐름 | 비용 | 위험 / 결론 |
|---|---|---|---|
| A domain projection + domain 봉인 proof | D proof가 중립 row만 감쌈 → S issue → A consume | 중립 payload 이동 외에 발급을 허용할 sealed issuer/새 crate 경계 필요 | Rust의 pub(in)은 조상 모듈만 허용하므로 `domain`에서 `services::...::o_input` 또는 DB 형제만 friend로 지정할 수 없다. pub(crate) new(row)로 타협하면 임의 발급. domain이 service/DB 토큰을 참조하면 역의존. 이 레그에서 채택하지 않음 |
| **B adapter capability + domain pure rule** | A 성공 spawned CAS가 nonclone receipt → S 기존 scope가 receipt를 proof로 변환 → S take/record/load → A by-value proof + checked values → exact CAS | 14파일, +350~650. DB status adapter에 작은 sealed authority child와 domain projection mapper 추가 | 기존 full DB row는 adapter proof 내부에 남아 domain 역의존 없음. naked new(row) 불필요. receipt와 proof를 구별하고 conversion/take 시점을 실제 진입 시험으로 증명해야 함. **채택** |

A의 생성자를 pub(crate)로 넓히거나 `BeforeEffects`를 plain row/bool/serde DTO로 바꾸는 안은 비용 절감 대안으로 인정하지 않는다. B의 receipt는 proof가 아니다. handoff/rollback은 receipt를 받지 않으며 CAS 때 BeforeEffects를 미리 만들지 않는다. 성공 CAS라는 현행 제어 흐름의 비복제 전달 수단만 추가한다. 독립 리뷰가 이를 발급 시점 이동으로 판단하거나 receipt를 old scope보다 앞에서 DB write에 소비해야 한다면 구현을 중단하고 별도 DESIGN_DELTA가 필요하다.

## 3. 모듈·타입·시그니처와 봉인

이 절은 **설계 시그니처**이며 코드 스파이크가 아니다.

| 소유 층 / 모듈 | 제안 API·타입 | 가시성·책임 |
|---|---|---|
| D `src/domain/intake_consent.rs` | 실제 `IntentRef` 정의; `PayloadProjection<'a>`; `ConsentRowProjection<'a>`; `HoldInput`; `payload_digest(&PayloadProjection)->Result<String,String>` | std/serde/sha2만. DB/status row 타입·Directory·current time·config·task-local 없음. 기존 payload 27슬롯, owner/source/claim coordinate를 값으로 받음 |
| D `src/domain/intake_consent/decision.rs` | `is_unresolved(Option<ResolutionClass>)->bool`; `check_hold_input(&ConsentRowProjection,&IntentRef,stable_consent:String)->Result<HoldInput,String>`; marker/identity 순수 비교 | Returned/Adopted unresolved 의미 보존. `HoldInput`은 pure checked binding(직접 필드는 private), **권한 proof가 아님**. 이 DTO만으로 DB를 변경할 API 없음 |
| A `src/db/intake_outbox/consent_projection.rs` | `project(&IntakeOutboxRow)->ConsentRowProjection<'_>` | 실제 DB row를 중립 projection으로 매핑. status==Spawned/claim 존재 판정은 A에서 유지. normalized owner key는 기존 domain OwnerIdentity 사용 |
| A `src/db/intake_outbox/worker_status/authority.rs` | `pub(crate) struct SpawnPermit(IntakeOutboxRow)`; `pub(crate) struct BeforeEffects(IntakeOutboxRow)`; `SpawnPermit::into_before_effects(self)->BeforeEffects`; `BeforeEffects::row(&self)->&IntakeOutboxRow` | **두 tuple field private**, Clone/Copy/Default/serde/Deref/AsRef/mutable getter 금지. `SpawnPermit::after_committed_spawn(row)` 생성자는 `pub(super)`로 worker_status subtree만. BeforeEffects 직접 생성자는 authority child 안에서만. 공개 conversion은 unforgeable receipt를 소모해야만 가능 |
| A `worker_status.rs` | `mark_spawned_before_effects(pool:&PgPool, expected:&IntakeOutboxRow, claim_owner:&str)->Result<Option<SpawnPermit>,sqlx::Error>` | 기존 `mark_spawned/transition(Change::Spawned)` 한 번과 같은 SQL/commit, 성공 뒤만 expected claim snapshot을 receipt 안에 보관. bool/row로 receipt를 복원하는 API 0. old mark_spawned를 테스트/기존 호출용으로 남길 때도 receipt 생성 우회 금지 |
| S `intake_worker/o_input.rs` | `scope_before_effects(permit:SpawnPermit, execution:impl Future<Output=T>)->T`; 기존 `take_before_effects(&IntakeRequest)->Option<BeforeEffects>` | 기존 scope 위치에서 `permit.into_before_effects()`. receipt 보관/재사용/직렬화 없음. task-local, 전체 request filter는 S에 그대로. trusted production caller는 worker의 CAS-success 분기 1개 |
| S `herdr_consent/worker_handoff.rs` | `pub(super) async fn hold(pool:&PgPool, proof:BeforeEffects, store:&ConsentStore, expected:&Consent, intent:&IntentRef)->Result<(),String>` | admission의 child(명시 path 등록)로 소유. 기존 DB hold 호출 자리에 by-value 호출. load+`current == *expected`+`current.unresolved_intents().contains(intent)`를 실제 코드로 먼저 실행하고 stable digest를 계산한 뒤 D checked input 생성. 그 뒤 A 호출 |
| A `consent_handoff.rs` | `hold_before_effects(pool:&PgPool, proof:BeforeEffects, input:HoldInput)->Result<(),String>`; `return_before_record(pool:&PgPool,proof:BeforeEffects)->Result<bool,String>`; release 기존 시그니처 | DB는 service Consent/Store/IntentRef 경로 참조 0. proof 소유권 유지; proof를 row로 풀어 반환하거나 raw-row overload 추가 0. current row 재검증과 marker CAS/transaction만 |
| S `policy/consent.rs`, `settlement.rs` | `IntentRef` 호환 reexport; 기존 `Consent::unresolved_intents`는 D pure predicate에 위임 | ConsentStore/Directory와 full Consent/Preview/ServerWitness/서명·schema는 runtime 소유로 남김. 타입 전부를 순수하다고 표시하지 않음 |

중요한 봉인 기준은 “pub(crate) 타입 이름을 아는가”가 아니라 **성공 CAS의 nonclone receipt를 소유하는가**다. CAS를 수행하지 않고 DB pool/row/id/IntentRef/nonce/bool만 가진 형제 모듈은 proof를 만들 수 없다. `mark_spawned_before_effects`를 호출해 실제 SQL CAS를 성공시킨 caller는 정의상 발급 권한을 얻은 caller다. 정상 worker 이외 production 호출자가 새로 생기면 권한 변경으로 리뷰한다. `into_before_effects`는 임의 row를 인수로 받지 않고 receipt가 가진 claim-time snapshot만 쓰며, 기존 scope처럼 status만 Spawned로 설정한다. DB에서 더 늦게 읽은 row로 snapshot을 바꾸면 stale-payload 검사가 약해지므로 금지한다.

`HoldInput`은 FS 관측의 출처를 암호학적으로/타입만으로 증명하는 새 capability가 아니다. FS 관측의 실제 guard는 **유일 service helper의 실행 코드와 순서**이며 load/equality/unresolved 검사 삭제 뮤턴트로 증명한다. 이 checked DTO는 혼자서는 authority가 없고, DB는 별도의 opaque BeforeEffects를 반드시 요구한다. 공개 unchecked constructor, `bool consent_is_valid`, raw-row consumer를 추가하지 않는다. review는 이 사실 전달 경계와 단일 production caller와 상위 row retirement 결합을 확인한다. FS snapshot 자체의 type-level unforgeability까지 요구하려면 Directory/ConsentStore의 중립 adapter 소유 분리라는 별도 범위가 필요하며, 이를 수행했다고 주장하지 않는다.

플랫폼 경계도 보존한다. BeforeEffects/SpawnPermit와 새 receipt API는 기존 proof와 같이 cfg(unix)에 둔다. non-unix worker는 기존 mark_spawned bool과 direct executor future 경로를 유지한다. Unix-only argument/import가 Windows에 새 dormant/unused 코드를 만들지 않게 실제 check/clippy/Windows prepush로 검증한다. task-local scope나 request matcher를 DB adapter로 옮기지 않는다. DB authority child에는 타입/선형 conversion만 두며 runtime Future 실행을 숨기지 않는다.

### DB가 맡는 범위

hold는 begin → 같은 provider/channel owner advisory lock → original row FOR UPDATE → neutral payload/source/claim/stamp/marker 비교 → 기존 marker serialize/CAS rows_affected==1 → commit. service 관측은 DB callback/trait로 숨기지 않는다. marker validation·stable consent digest의 입력 확인은 pure D에서, row status/lock/live query/UPDATE는 A에서 수행한다. 현재 `hold`의 에러/rows_affected 반환 의미를 유지한다.

rollback과 release는 기존 owner_valid SQL(legacy None/None=true, stamped active exact owner/gen=true, partial=false)과 transaction/CAS 순서를 유지한다. hold에는 새 query를 추가하지 않는다. production owner 전환이 같은 transaction에서 old open row를 retire하는 현행 결합이 I3의 운영 전제이며 E23과 SQL writer 표에 좌표를 고정했다. release의 released=true no-op은 기존처럼 source/payload/stable-consent 비교 뒤, active owner 검사 전에 둔다. `Utc::now()`의 rollback rearmed_at은 기존 row 검증 후 같은 위치에서 샘플링한다. commit 전에 wake/card/new row INSERT를 하지 않는다.

## 4. 크기·파일 범위와 분할선

| production 파일 | 예상 순 코드 | 책임 |
|---|---:|---|
| `src/domain/intake_consent.rs` | +160 | 실제 중립 wire/projection/digest/checked binding |
| `src/domain/intake_consent/decision.rs` | +60 | unresolved/identity/marker 순수 판정 |
| `src/domain/mod.rs` | +1 | 모듈 등록만, S3 공유 예약 |
| `src/db/intake_outbox/consent_projection.rs` | +65 | DB row→projection mapping |
| `src/db/intake_outbox/worker_status/authority.rs` | +80 | sealed receipt/proof, 소비 conversion |
| `src/db/intake_outbox/worker_status.rs` | +35 | 기존 spawned CAS의 성공 영수증 반환 |
| `src/db/intake_outbox.rs` | +3 | mapper/authority reexport 등록만 |
| `src/db/intake_outbox/consent_handoff.rs` | -35 | FS/pure 로직의 기존 순서 보존 이동 |
| `src/services/cluster/intake_worker/o_input.rs` | -10 | local proof 정의 제거, 기존 scope에서 receipt 변환 |
| `src/services/cluster/intake_worker.rs` | +8 | CAS receipt를 기존 wrapper까지 move |
| `src/services/discord/router/herdr_consent/worker_handoff.rs` | +85 | 기존 위치의 by-value load/validate→DB orchestration |
| `src/services/discord/router/herdr_consent/admission.rs` | +8 | private helper 등록·call 변경 |
| `src/services/session_host/herdr/policy/consent.rs` | -9 | IntentRef 실제 정의 이동, 호환 reexport |
| `src/services/session_host/herdr/policy/consent/settlement.rs` | +9 | 중립 resolution projection→domain predicate |
| 소계 / wiring 보수 여유 | +460 / +60 | 추가 파일 없음 |
| **계획 합계** | **+520** | 보수 범위 +350~650, 14파일 |

시험 3개 기존 파일(`consent_handoff_tests.rs`, `consent_scope_tests.rs`, `admission_tests.rs`), 새 compiler probe harness/fixture, 공식 생성 목록은 production cap에서 별도다. 추가 책임이 생기면 파일 수를 다시 잰다. 10–18파일/+300–750 원 계획 범위 안, hard cap 도구 `pr_cap_prod.py:15` FILES=30,NET=1500, PLAN 정지선 +1200보다 낮다. 산술은 estimate이지 측정된 구현 diff가 아니다.

기존 giant `src/db/intake_outbox.rs`는 **1372줄**, 독립 책임을 더하지 않고 module/reexport 몇 줄만 등록한다. mapping·authority·FS orchestration은 모두 작은 전용 파일이다. `intake_worker.rs`는 963줄이며 +8줄 정도의 기존 흐름 wiring만 한다. 나머지 큰 production 대상 consent.rs 699, consent_handoff.rs 320, worker_status.rs 415. 줄 수 맞춤 책임 이동은 하지 않는다.

만약 closed Consent 값 이동/Directory adapter 재배치 등으로 +1200 또는 18파일 예상선을 넘으면 중단하고 재설계한다. 독립적으로 안전한 분할은 (1) 동일 wire의 IntentRef+순수 unresolved/digest projection과 기존 direct consumer 연결, (2) receipt/proof 봉인+서비스 관측+DB CAS의 **동시 활성화**다. (2)의 producer와 consumer를 dormant scaffold/공개 raw-row fallback으로 쪼개지 않는다. owner 전환을 row retirement에서 분리하는 요구가 생기면 현재 전제의 변경이므로 별도 authority/CAS 설계로 중단한다.

## 5. 대상 시험과 뮤턴트 계획

이번 PLAN에서 아래 시험은 실행하지 않았다. 구현 뒤 실제 PG는 throwaway만 사용하며 default parallelism을 유지한다.

| 대상 | 기존 좌표 / 검증 | 확장·뮤턴트 |
|---|---|---|
| `consent_handoff_requires_exact_payload_source_and_live_owner_pg` | `consent_handoff_tests.rs:185` source/payload/claim/gen/consent/live owner release 검증 | wire projection 이동 뒤 그대로; payload/intent/stable key 조건 하나 제거하면 RED |
| `consent_handoff_stale_before_effects_proof_never_changes_row_pg` | 같은 파일 `:383` stale payload/claim/gen hold 거절·whole row 비교 | DB lock 전/후 barrier 경합, status/target/provider/source 변경 사례. exact CAS 조건 제거 RED |
| `consent_before_record_rollback_consumes_exact_authority_and_preserves_bookkeeping_pg` | 같은 파일 `:415` payload/claim/gen/owner/marker/status exact rollback, 원본 bookkeeping | whole row 비교에서 의도적으로 바뀌는 status/claim/time/marker/updated_at만 허용. retry_count/completed_at/created_at/attempt/parent/payload 보존. retry increment 또는 marker 허용 확대 RED |
| `consent_before_record_rearm_is_once_across_concurrency_and_reclaim_pg` | 같은 파일 `:513` 병렬 same row rollback/reclaim, second rearm false, retry hold | proof 재사용 대신 test-only 독립 중복 authority를 사용한 DB CAS 경합. marker 영속 검사 제거 RED |
| `before_effects_authority_is_exact_consumed_and_scoped_pg` | `intake_worker/consent_scope_tests.rs:4` wrong request도 한 번 소비, scope 밖 None | 실제 spawned 성공 receipt 전달, spawn CAS 0이면 issue 0, detached task 0, normal entry 뒤 0. slot.take를 borrow/clone로 바꾸는 뮤턴트 RED |
| owner-retirement 결합에서 도출할 `consent_handoff_owner_switch_retires_original_before_stale_hold_pg`(이름 잠정) | **신규 보존 시험**, production `custody::promote` 경계의 실제 transaction | FS 관측 뒤 실제 promote(정상 retirement+transfer)를 완료하고 stale hold 재개, retirement가 만든 row를 추가 변경하지 못함. main도 GREEN인 보존 시험이다. promote에서 retire_open_on_tx 호출을 제거한 root-cause mutant는 RED여야 함. 단독 owner table SQL 변경을 production 재현으로 오인하지 않음 |
| observation/publication 경계 시험 | 기존 `admission_tests.rs:1488–1632,1758` before_record fault/재시도 기반 | record 시도 직전 failure는 exact rollback 1; record가 publish하고 Err 반환하면 rollback 0; load Missing/Err/different full Consent/settled intent면 DB mutation 0/card 0. equality/unresolved/load guard 제거 각각 RED |
| wire/legacy/source class | schema/IntentRef/marker fixture와 payload fixed digest | deny_unknown_fields, 끝 Null, None/Some, Returned/Adopted unresolved, stable consent vs nonce, half-stamp 거절, legacy None/None 허용, old released/rearm marker 해석 동일. source classifier를 raw bool/claim-only로 치환 RED |

### 임의 발급 불가 / 컴파일 가시성 시험

외부 integration test가 `pub(crate)` 모듈을 못 찾는 실패만으로 봉인을 증명하면 안 된다. **현재 소스의 같은 crate 비인가 형제 모듈**에서 probe한다. 구현 레그의 test harness는 임시 overlay로 lib.rs에 작은 sibling probe를 넣고 `cargo check --lib --message-format=json`를 non-test 구성으로 실행한 뒤 원본 바이트를 복원한다(그 사이 다른 검증/뮤턴트와 병렬 편집하지 않는다). compile-fail 코드는 git에 production으로 남기지 않는다. normal build에서 for_test가 없다는 점도 검증한다.

1. 같은 import/type와 빌린 row 읽기를 쓰는 positive control은 compile 성공해야 한다.
2. `BeforeEffects(row)`/필드 literal, `SpawnPermit(row)`/private after_committed_spawn 호출은 E0423/E0451/E0624 등 해당 privacy 오류여야 한다. unresolved import/E0433만 난 결과는 실패다.
3. proof/receipt clone, serde Deserialize, Default, raw-row From/Into는 trait/method 오류여야 한다. 두 consumer에 같은 proof를 move하면 E0382여야 한다.
4. SpawnPermit를 handoff/rollback에 직접 전달하면 type mismatch(E0308)여야 한다. 따라서 영수증이 조기 proof가 될 수 없다.
5. 생성자/필드를 pub(crate)로 넓힌 root-cause mutant에서는 해당 부정 probe가 compile 성공해서 harness가 RED가 되어야 한다. private ctor가 필요 없다는 가짜 PASS를 막는다. 부정 snippet이 여러 원인으로 실패하지 않게 각 우회 경로를 별도 probe로 나눈다.
6. overlay/뮤턴트 복구 후 원본 각 파일 SHA256가 같아야 하며 cargo profile·shim·test cfg 여부를 로그에 남긴다. production에서는 새 #[allow]/baseline 인상 없음.

### 실행 환경과 REVIEW_READY 전 조건

`cargo`는 PATH의 cargo-shim만 사용. `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_STRIP=none CARGO_PROFILE_TEST_STRIP=none`, `CARGO_TARGET_DIR` 변경 0, `--test-threads=1` 금지. PG는 `bash /home/adk/skills/agentdesk-issue-pipeline/scripts/pg_throwaway.sh cargo test --lib <filter>`로 실제 AGENTDESK_REQUIRE_PG=1에서 수행한다. 네 T-consent 이름 각각/관련 scope·service 시험의 실행 수, failed/skip, 바이너리 경로/SHA256, 원시 로그를 보존한다. 필터 0건이나 PG skip은 성공으로 세지 않는다.

모델 Mac 실제 실행(규칙별 실제 결과·상태 수·시간), 대상 PG/순수/compiler probe/필수 root-cause 뮤턴트, 필요한 check/clippy 검증 뒤 `prepush_gates.sh <wt> --static-only` 및 `source_scan_readers.py <wt> --base origin/main --run <log>` failed0이 구현 REVIEW_READY 조건이다. clippy workspace/all-targets/all-features JSON에서 자기 파일 warning0을 확인한다. 같은 head의 증거만 재사용한다. PLAN은 REVIEW_READY가 아니므로 이 레그에서 cargo/source-scan/prepush를 실행했다고 주장하지 않는다.

## 6. 독립 리뷰 집중 항목 초안

| 집중 항목 | 차단 방법 / 남은 검증 |
|---|---|
| 경계 시각 | old :481 by-value helper → FS load → begin → owner/row locks 순서를 코드와 barrier 시험으로 확인. CAS-success receipt는 proof가 아니며 Issue는 old :115 scope. 이미 있던 관측 후 파일 변경 간극은 남음; cross-store 원자성 주장 금지 |
| 동시성·reclaim | 같은 row 중복 proof의 DB CAS, marker retained across reclaim, owner transfer actual lock race, force-fail/delivery/replay status 경합. model/PG 둘 다 필요. clean restart와 process loss를 혼동하지 않음 |
| 권한 경계 | constructor private, no Clone/serde/raw-row fallback, CAS 1일 때만 receipt, receipt와 proof 타입 불일치, normal/wrong-request take 후 폐기. 실제 production caller grep. friend visibility를 문서로만 선언하지 않음 |
| 문서로만 선언한 가드 | Store load/equality/unresolved는 service helper의 실제 실행문이어야 함. 기존 production retire_open_on_tx→transfer_owner_in_tx 결합과 hold status CAS가 실제 코드에 있어야 함. checked DTO가 FS 출처 증명을 대신한다고 주장하지 않음 |
| 옛 형식 복원 | IntentRef field/serde, schema1–3 reader, stable key/nonce 구별, rearm/custody prefix/count/time/null slot. revision/settlement의 Returned/Adopted 의미 보존. legacy rollback 파일은 편집 제외 |
| 잘못된 0/PASS | cfg 파싱≠TLC, predicted expected.tsv≠실행 결과, type import 실패≠private ctor 증명, 필터0건/PG skip≠통과. existing live_owner test가 hold 경계를 검증하지 않음 명시 |
| 모델 밖 경로 | Cancel/Terminate/clear/stop/legacy return/retention은 위 표 좌표대로 유지. old spawned crash 보존을 해결한다며 cleanup/delete/kill/retry를 새로 추가하지 않음 |

독립 리뷰의 결정 질문: B의 CAS receipt→old-scope proof가 기존 발급 권한·시점 보존으로 충분한가; service FS 사실 전달과 opaque BeforeEffects의 구분이 수용 가능한가; 상위 coupled retirement가 모든 production owner 전환 경로에서 유지되는가(단독 helper/임의 SQL은 별도 경계인가). 어느 하나라도 다른 발급/소비 시점이나 cross-store lock 설계를 요구하면 구현하지 않고 별도 설계로 돌아간다.

## 7. 활성 ref 겹침과 파일 소유

`git fetch --prune origin` 후 `git ls-remote`로 요청 네 family 120 ref를 수집했다. GitHub connector 검색이 알려진 PR도 빈 목록을 반환하여, 읽기 전용 `gh pr list --state all --limit 400 --json number,state,mergedAt,headRefName,headRefOid`로 merged head를 확인했다(PR/issue/DAG 쓰기 없음). main 조상 또는 머지된 PR head 조상 105 ref 제외, 다른 새 ref 조상인 superseded 7 ref 제외, **보수적인 활성 후보 8 ref**를 비교했다. 원격 ref 존재가 현재 실행 중인 레인이라는 뜻은 아니므로 coordinator의 소유 확인 대상으로 남긴다.

squash-merge로 main에 없는 원 branch 조상을 활성 변경으로 잘못 세지 않도록, 각 후보는 main 및 merged PR head와의 가장 가까운 merge-base에서 실제 branch diff를 계산했다. 기준/전체 경로/제외 사유는 `logs/s4-07-1/overlap-audit.json`에 있다. 이 레그는 다른 worktree를 읽거나 고치지 않았다.

| ref / head | 활성 patch 기준 / 파일 수 | 제안 파일과 겹침 | 조정 |
| `refs/heads/fix/i7651-b2-p3b` / `0c196080d219fd33d3415ff91630e8b40c9bf2d4` | `685992a136405203d6e8678912fc3310a944d84a` / 30 | `docs/statecharts/expected.tsv` | 등록/manifest/index 공유. 구현 시작 전 coordinator에 슬롯/FILES 조정; 이번은 독립 doc 추가 행만 |
| `refs/review/i7353-s16-1` / `a87cb2d1c66f82a2de49622dfc97b9b073034ff7` | `20b91b90a22264c04821f1d9fc6715b11b6f0633` / 24 | `src/domain/mod.rs` | 등록/manifest/index 공유. 구현 시작 전 coordinator에 슬롯/FILES 조정; 이번은 독립 doc 추가 행만 |
| `refs/review/i7353-s18-0` / `1cad7f32644496c60a118cb72d9b4ea95a2a7ed0` | `92ef5b00b205679965c24c5c017f7237e8b5d57f` / 14 | `docs/statecharts/expected.tsv` | 등록/manifest/index 공유. 구현 시작 전 coordinator에 슬롯/FILES 조정; 이번은 독립 doc 추가 행만 |
| `refs/review/i7353-s21-1` / `ae9e46fdfe114591d20bce6abec816c0467f15d4` | `289f41b1d0f48fab0dc426e652308eced4326fce` / 32 | `docs/statecharts/README.md`, `docs/statecharts/expected.tsv`, `src/domain/mod.rs` | 등록/manifest/index 공유. 구현 시작 전 coordinator에 슬롯/FILES 조정; 이번은 독립 doc 추가 행만 |
| `refs/review/i7353-s4-04-1` / `1a3edbac1e905606c2ce89dad2456e11eab19f28` | `cb556699ffc18585d3d1eaa3876763eeab8fd440` / 11 | `src/db/intake_outbox.rs`, `src/domain/mod.rs` | S4-04와 parent 등록 직렬 조정. consent/worker_status 직접 patch는 겹치지 않음 |
| `refs/review/i7353-s4-06-1` / `08e2336e47986d70d0cbe95e56215858377aed5c` | `5cf9d2909ed251c82dd7ec53e8fb8a79d1aa8a50` / 17 | 없음 | 동일 선행 head, 자체 scope 없음. 상속된 bundle3 파일을 겹침으로 오판하지 않음 |
| `refs/review/i7353-s4-09-1` / `efc081bc6614628751a307328a2b87572c482559` | `289f41b1d0f48fab0dc426e652308eced4326fce` / 6 | 없음 | production target 분리 |
| `refs/review/i7353-s4-18-1` / `297b41edd4b8e9d935928ce6afe48227ad30c903` | `289f41b1d0f48fab0dc426e652308eced4326fce` / 8 | `src/domain/mod.rs` | 등록/manifest/index 공유. 구현 시작 전 coordinator에 슬롯/FILES 조정; 이번은 독립 doc 추가 행만 |

#7651 `src/services/session_host/herdr_gate/legacy_return.rs`, `src/db/dispatched_sessions/hosted_execution/legacy_return.rs` 및 migration/return 관련 legacy 소유 파일은 쓰기 범위에서 제외한다. `herdr_gate.rs`·return_input·consent_holds·routing/replay 파일도 이번 production 목록에 없다. i8057 ref1/2/3와 PR #8066 head는 merged ancestry여서 활성 충돌 표에서 제외했다. 브리프가 지정한 `/home/adk/adk/wt/i8057-h2`는 접근·편집 0.

S3의 domain/mod 등록은 파일 분리 조건부 병렬의 예외다. 이번 설계는 이 parent를 수정하지 않았고, 실제 구현 전 coordinator가 직렬 적용/등록 소유를 정해야 한다. 이를 “겹침 0”으로 보고하지 않는다. 기존 shared docs manifest/index의 새 area 행만 이번 plan commit에서 갱신한다.

## 증거 목록

- `/home/adk/adk/lane-runs/i7353/logs/s4-07-1/proof-consumers.txt`: git grep 발급/소비/test-only 좌표.
- 같은 디렉터리 `consent-loads.txt`, `outbox-writers.txt`, `production-writers.json`, `adjacent-controls.txt`: FS load와 인접 SQL/control 원시 조사(후속 clear_report load는 본문에 추가).
- `remote-refs.txt`, `pr-heads.json`, `overlap-audit.json`, `planned-paths.txt`: fetch 뒤 원격 family 120 ref, merged PR head 제외와 실제 patch 겹침.
- `main-hold-before-effects.txt`, `main-owner-retirement.txt`: main의 hold 직접 query 부재와 이를 보완하는 production retirement 결합의 file:line 증거.
- `model-config-static.txt`: repository parse_config/parse_expected로 7 cfg/manifest 대조. safety6/PROPERTY2씩. `model-action-static.txt`: 20 action의 primed assignment/UNCHANGED coverage 정적 대조. 둘 다 **TLC/SANY 아님**.
- 커밋될 model/cfg/view/chart/report와 이 설계 복사본 `docs/statecharts/intake-consent/design.md`. 코드 기준 SHA를 모두 고정했다. 모델 실행은 Mac 필요.
- 이 PLAN에서 cargo/test/PG/clippy/source-scan/prepush 실행 0. 시험 바이너리·SHA256·실제 시험 실행 수는 N/A. 다음 구현 레그에 필요한 검증은 5절에 명시.
- 제한 준수: prod diff0, runtime/운영 DB/15432/o_store/설정 파일 접근0, 다른 레인 worktree 접근0, PR/issue/DAG/Discord 쓰기0, 새 정리·삭제·kill 권한0.
