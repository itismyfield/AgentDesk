# #7336 설계 r3 — 공유 home, 채널 execution owner, 입력 gateway 분리

## 0. 기준과 결론

- 사용자 정본: [12:2x 입력별 라우팅 정의](https://github.com/itismyfield/AgentDesk/issues/7336#issuecomment-6407581059), [12:4x 양방향 대칭 보충](https://github.com/itismyfield/AgentDesk/issues/7336#issuecomment-6407642601).
- 리뷰 입력: `/home/adk/adk/lane-runs/i7336/review-design-r2-webgpt.out.md`의 **“ChatGPT 답변:” 뒤**, P1 3건/P2 3건 및 새 방향 제안. 이번 문서는 그 구조로 다시 쓴 완전한 대체 설계다. r1/r2의 상충 내용은 승계하지 않는다.
- 코드 좌표의 정본은 리뷰와 같은 `c5477168cfdb2591aeebb77aa091d3fe13bca545`. 읽기 전용 worktree HEAD는 `fb2219535d889de858a3d9e1429d32aa6e31d309`로 유지했다. `git show c547…:<path>` snapshot으로 아래 좌표를 확인했으며 merge/reset/코드 변경은 없다.
- **채택 구조:** gateway는 입력을 받아 판정·commit한다. 실행자는 `home` 또는 `peer(home)`이다. O/legacy 실행과 local transcript 읽기·모델 답 전송은 채널 execution owner가 맡는다. `gateway_preferred_instance_id`는 채널 home/실행자를 결정하지 않는다.
- **복귀:** 제어 가능한 이전 owner는 현재 작업·답 완료 뒤 정상 local session-end. 응답하지 않는 이전 owner는 generation을 폐기하고 새 source만 건강한 target에 보낸다. 물리적 session-end는 지연하며 성공으로 기록하지 않는다. 양 방향에 같은 전환 함수를 사용한다.
- **착수:** 판정·공유 home 저장 모델·대칭 fixture·알림 identity/renderer는 지금 별도 준비 조각으로 착수할 수 있다. #7272/#7330의 전체 종료를 기다리지 않는다. 실제 전환 활성화에는 source/효과 경계와 **PG를 통한 Mac 간 입력 custody**가 필요하다.
- 크기 계획은 4개 리뷰 단위, prod 순 **+2,150~3,150 / 고유 약 34~44파일**이다. r2의 +1,350~1,950보다 늘어난 이유는 누락됐던 공유 `/node`, 비-gateway O writer, shared pending admission과 지연 정리를 이번에 실제 구현 항목으로 잡았기 때문이다. 전원 증명·node 전체 quiesced protocol은 여전히 없다.

| r1/r2 항목 | r3의 선택 |
|---|---|
| heartbeat/readiness, canonical source, owner CAS/stamp, BundleV1, local 세션 key | 재사용하되 실제 source/submit/출력·PG custody까지 연결 |
| power witness·정상 종료/maintenance별 통지·node 전체 quiesced ACK | 폐기. 입력 수신 가능성을 관측하며 전원을 증명하지 않음 |
| 복귀 barrier·명시 `/node-return` | node 전체 barrier/새 명령 폐기. 제어 가능한 **채널**의 현재 작업·답 완료만 대기; `/node`는 공유 home 설정 |
| desired=gateway, O 복귀의 global handback, local home map 우선 | 폐기. H/peer 선택, 채널 owner REST, shared PG home으로 대체 |
| 정상 end 확인을 모든 복귀의 선행조건으로 둠 | 제어 가능한 old owner에만 적용. unavailable은 논리적 revoke 뒤 물리 정리 deferred |
| 세대별 nonce·채널별 allowlist | nonce는 실제 persistent dedupe K와 연결. allowlist는 pilot만, 최종 대상은 home/binding에서 파생 |

## 1. 역할과 불변식

| 역할 | 정본·책임 | 맡지 않는 것 |
|---|---|---|
| Home H | 공유 PG의 채널 home override 또는 PG agent default. 정상 입력 목적지 | 현재 gateway, 최근 fallback Mac에서 역추론하지 않음 |
| Peer P | 승인된 두 Mac instance ID 쌍에서 H의 상대편. 실행 가능 여부는 heartbeat/readiness로 확인 | gateway라는 이유로 선택하지 않음. WSL 등 임의의 세 번째 worker로 확대하지 않음 |
| Gateway G | 기존 token별 PG gateway lease 보유자. 새 입력 수신, shared home/availability 읽기, canonical source/custody/route/admission commit | peer의 provider를 자기 세션으로 resume하지 않음. 채널 복귀 때문에 global gateway를 이동시키지 않음 |
| Execution owner E@g | 기존 `intake_session_owners`의 active owner/generation. E의 local session·source·writer 실행 | gateway lease를 채널 실행권으로 대신 쓰지 않음 |
| 효과 guard | source의 exact begin과 실제 submit/POST/EDIT/DELETE에서 원 owner generation·session incarnation 확인 | 현재 node나 현재 generation을 옛 출력에 사후 부착하지 않음 |
| 입력 custody | canonical receipt와 **실행 가능한 admission으로 승격될 PG staged row**, BundleV1 | 한 Mac의 로컬 queue 파일 저장만으로 인계 완료를 선언하지 않음 |
| 정리 책임 | superseded owner 세대의 exact local session 참조와 기존 normal end 결과 | 새 kill·삭제·peer 파일 정리 권한 없음 |

핵심 불변식은 다음과 같다.

1. canonical source의 시작은 한 번이다. target 변경은 started/withheld 요청을 재실행하는 허가가 아니다.
2. 한 채널 identity의 새 효과를 허가하는 generation은 하나다. old generation은 native 프로세스가 살아 있어도 새로운 출력 효과를 만들 수 없다.
3. home 변경 revision과 실행 owner generation은 다르다. `/node` 성공만으로 owner/전환 알림을 바꾸지 않는다.
4. 실제 routing Mac이 같으면 전환 event/알림을 만들지 않는다. 유효한 active E@g는 재사용한다. release 후 재획득 등으로 실행 generation이 올라가더라도 같은 Mac의 세션 재생성·gateway 교체는 routing 전환으로 세지 않는다.
5. B 종료 확인을 받을 수 없다는 이유로 건강한 A의 **새 입력**을 무한히 막지 않는다. 원 요청/미결 출력·물리 정리 미완료를 보존한 채 논리적 실행권을 넘길 수 있다.
6. 새 입력이 없으면 availability 변화만으로 route 변경/normal end/전환 알림을 만들지 않는다. 이미 PG에 보관된 입력의 재개와 과거 정리 재시도는 가능하다.

## 2. 현재 코드와 바꿀 경계

모두 source SHA `c5477168…` 기준이며 운영 설정/DB 내용은 읽지 않았다.

| 사실 | file:line | r3에서의 처리 |
|---|---|---|
| `/node` 읽기는 process-local map, 저장은 local settings | `src/services/discord/commands/node.rs:67`, `:78`, `:109`, `:314`, `:421` | 모든 결정/명령 read를 shared PG home resolver로 변경. 로컬 early-return 제거 |
| local settings는 YAML/runtime JSON, 재부팅 때 legacy override 복원 | `src/services/discord/settings/write.rs:447`, `src/services/discord/settings/read.rs:240`, `:283` | 기존 loaded 값은 migration 후보로만 사용. PG 우선, reset tombstone 후 재이관 금지 |
| agent 기본 실행 노드는 이미 PG | `src/services/cluster/agent_execution_node.rs:31`, `:45`, `:61` | default 정본 재사용. 복수 상충 agent 해석은 임의 LIMIT 1 대신 conflict 처리 |
| node offline은 heartbeat timeout | `src/services/cluster/node_registry.rs:737`, `src/services/cluster/readiness.rs:84` | 입력 수신 가능성 판단에 재사용. 전원 증명을 요구하지 않음 |
| token gateway lock/자동 preferred yield | `src/services/discord/runtime_bootstrap/gateway_lease.rs:252`, `:589` | 입력 gateway 선출용으로만 유지. 채널 반환 코드에서 yield 호출/선호값 변경 없음 |
| O home은 global preferred 값, foreign placement 거절 | `src/services/tui_o/channel_policy.rs:357`, `src/services/tui_o/cutover.rs:131`, `:143` | routing-managed 채널은 execution owner가 O site를 결정. old delegated home 테이블을 되살리지 않음 |
| O writer 시작은 pg_gateway=false면 실패 | `src/services/tui_o/writer/host.rs:474`, `:505` | 채널 OwnerPermit로 start. gateway bool로 강제 통과시키는 방식 금지 |
| production O port는 cached Serenity context 대기, REST helper는 test-only | `src/services/discord/runtime_bootstrap/o_writer_host.rs:67`, `:88` | owner runtime의 REST client/bot identity로 production port를 공급. ctx 없음이 정상인 remote-owner 시험 필요 |
| 기존 worker는 gateway/standby에서 돌 수 있고 local REST runtime을 구함 | `src/services/discord/runtime_bootstrap/intake.rs:3`, `src/services/discord/health/runtime_resolve.rs:345`, `:354` | owner reconciler/host도 양 역할에서 시작. owner readiness가 gateway 준비를 기다리지 않도록 함 |
| O gate/DeliveryLeaseCell은 local | `src/services/tui_o/ownership.rs:124`, `src/services/discord/outbound/o_writer_io.rs:145` | actor lifecycle·같은 Mac 내 writer/legacy 경합용으로 유지. Mac 간 fence는 PG generation guard |
| acquire/stamped admission 생성은 dormant, CAS와 worker stamp는 존재 | `src/services/cluster/intake_router_hook/owner_record.rs:1`, `:407`, `:542`, `src/db/intake_outbox/worker_status.rs:55` | live 연결을 구현. helper 존재를 완료로 세지 않음 |
| live injection은 일반 dispatch보다 앞 | `src/services/discord/router/intake_gate.rs:1117`, `src/services/discord/router/intake_dispatch.rs:431` | #7272의 공통 입력 경계에서 source/target permit을 소비. Discord hook 하나만 활성화하지 않음 |
| 로컬 pending queue 경로 | `src/services/turn_orchestrator/pending_queue_persistence.rs:88`, `:92`, `src/services/discord/runtime_store.rs:103` | 다른 Mac의 custody가 아님. routing 입력은 PG stage/admission으로 확정 |
| CanonicalInput은 source·원문·첨부를 저장하지만 등록 row는 replay_only/unknown | `src/db/replay_disposition/write/start.rs:8`, `:32`, `:135` | 등록만으로 worker가 실행한다고 하지 않음. 명시적으로 regular pending admission에 연결 |
| dormant의 새 source는 LegacyUnprotected | `src/services/discord/router/intake_dispatch/replay_admission.rs:35`, `:60`, `:253`, `:302` | 보호 reader만 연결한 상태에서는 기능 on 금지. routed input의 register/begin producer도 필요 |
| host별 local session key | `src/services/discord/adk_session.rs:118` | 기존 namespace 재사용. local locator/native nonce를 함께 pin |
| strict native 완료/다음 dequeue | `src/services/discord/turn_finalizer.rs:538`, `src/services/discord/turn_bridge/finalize_epilogue.rs:69` | healthy owner 전환은 current native 작업·답 완료 뒤. receipt done과 구분 |
| clear 상위는 queue discard/timeout-overtake, 하위 end는 local | `src/services/discord/commands/control.rs:279`, `:290`, `src/services/discord/commands/control/clear_teardown.rs:59` | 하위 normal end만 exact local pin에 적용. 상위 clear 재사용 금지 |
| managed end와 Herdr end의 권한 범위 | `src/services/discord/commands/control/managed_reset.rs:71`, `src/services/discord/herdr_terminate.rs:383` | 기존 local 종료/거절 정책 유지. A가 B process/tmux를 대신 종료하지 않음 |
| persistent outbox tx helper가 중복 시 id 반환 | `src/services/message_outbox.rs:246`, `:270` | 전환 event를 session/reason identity에 넣어 사용. TTL helper None 반환을 대신 쓰지 않음 |
| 실제 outbox dedupe는 target/session/reason으로 계산 | `src/services/message_outbox/identity.rs:107` | §8의 정확한 필드 매핑 사용. nonce만 세대별이어서는 부족 |
| 오래된 POST의 nonce/history 복구 | `src/services/discord/task_notification_delivery/card_post.rs:29`, `src/services/discord/task_notification_delivery/gateway.rs:184` | 작은 routing adapter로 재사용. unknown을 blind repost/sent로 처리하지 않음 |
| BundleV1 byte 저장/검증, put/load/cleanup의 만료 처리 | `src/services/cluster/attachment_transfer.rs:18`, `src/services/cluster/attachment_transfer/store.rs:53`, `:83`, `:123`, `:207` | 공유 custody 참조의 수명 pin을 handoff 조각에 포함. body만 실행하는 강등 없음 |

`o_channel_homes`(`migrations/postgres/0133_o_channel_homes.sql:3`)는 옛 위임 실행 상태이고, `channel_recovery_custody`(`migrations/postgres/0140_channel_recovery_custody.sql:3`)는 복구 소비 권위다. 어느 것도 새 `/node` 설정 정본으로 과부하하거나 활성화하지 않는다. 별도 실행 owner 테이블도 만들지 않는다.

## 3. 공유 home 계약 — P1-2

### 3.1 저장 위치와 우선순위

신규 PG 설정 테이블 **`channel_home_preferences`**를 권고한다. 현재 PG agent default는 재사용하되, local `/node` override를 공유할 정본이 필요하다. migration 번호는 [U03]이다.

- PK: 정규화한 `(provider, raw_channel_id)`; `mode = override | inherit | migration_pending | conflict`; `home_instance_id`는 override에서만 non-null; `revision >= 1`; 마지막 명령 operation ID; migration 후보(node ID/값)만 저장.
- provider는 `/node`를 받은 `Data.provider`와 기존 OwnerIdentity 정규화를 따른다. picker에도 provider/expected revision을 보존한다. bot-local legacy 후보는 그 bot의 loaded provider와 결합하며, provider를 알 수 없는 값을 다른 provider에 추정 복제하지 않는다.
- 이것은 **설정**이다. active owner/generation/native session을 저장하지 않는다.
- 해석 순서: actual channel의 PG override → reset/inherit면 기존 policy parent의 PG override → PG agent default. parent 상속은 기존 thread 정책을 사용하고 actual raw channel identity는 바꾸지 않는다. DM은 parent 상속 없이 자신의 binding/default를 해석한다.
- migration_pending/conflict, 서로 다른 agent default, home 식별 실패는 `HomeUnknown`이다. `HomeUnavailable`로 바꿔 임의 peer를 고르지 않는다.
- 공유 row가 없는 새 정상 채널은 PG binding/default에서 파생한다. 현재 execution owner, gateway, local legacy map, `gateway_preferred_instance_id`는 home 해석 fallback이 아니다.
- pair는 승인된 두 Mac의 stable instance ID다. H가 pair의 한쪽일 때 peer는 정확히 상대편이다. 로컬 hostname 두 개를 임의 비교하거나 전체 worker 목록의 첫 노드를 고르지 않는다([U01]).

### 3.2 `/node` read/write/reset

`update_channel_node_override`는 async PG write로 바꾼다. local map이 같은 값이라는 이유로 저장을 생략하지 않는다. 명령은 같은 OwnerIdentity serialization key 아래 PG home revision을 확인/갱신하고 **commit 또는 동일 operation read-back 확인 뒤에만** “저장했습니다”를 응답한다. cache는 그 후 read-through 결과로 갱신할 수 있지만 결정 권위는 아니다.

- `/node A`: owner 행이 아직 없어도 즉시 shared preference를 upsert. 다음 일반 입력까지 저장을 미루지 않는다. known pair/등록된 node인지 검증하되 offline이라는 이유만으로 home 설정을 없애지 않는다.
- `/node reset`: row를 삭제하지 않고 `mode=inherit, home_instance_id=NULL, revision++`로 남긴다. 이후 PG parent/agent default를 사용하며 옛 local override는 부활하지 않는다. 기본 home이 없으면 저장된 inherit 상태와 home 미확정을 구분해 표시한다.
- picker가 가진 expected revision과 충돌하면 현재 값을 다시 보여 준다. PG 오류/ambiguous commit은 성공을 추정하지 않고 operation ID로 재조회한다. 판정 불가면 저장 미확정이다.
- `/node` 변경 자체는 owner CAS·세션 종료·routing 알림을 만들지 않는다. “home을 저장했으며 다음 새 입력부터 적용”이라고 응답한다. 다음 입력의 실제 실행 Mac이 그대로면 알림0이다.
- 결정 read 소비점은 현재 `commands/node.rs:314`, `intake_dispatch.rs:382`, `o_writer_host.rs:145`, `:168`를 포함해 모두 PG resolver로 전환한다. local `channel_node_override()`를 routing 권위로 남기지 않는다.

### 3.3 옛 local 값 이관

운영 파일을 읽는 scanner를 추가하지 않는다. 기존 runtime이 **이미 로드한 legacy override map** 또는 코디네이터가 승인한 node-ID 요약을 후보로 받는다. 이 레그는 그 운영 값을 읽지 않는다.

1. 전개 중에는 등록된 기존 채널별 migration_pending row를 먼저 둔다. 각 Mac은 `(instance ID, candidate home 또는 명시적 없음)`을 제출한다.
2. 양쪽 값 동일 → 그 override import. 한쪽만 값 있고 다른 쪽 명시적으로 없음 → 유일한 override import. 둘 다 없음 → inherit. 서로 다른 값 → conflict. 아직 한쪽 보고가 없으면 pending을 유지한다.
3. pending/conflict는 coordinator/operator가 `/node`로 shared 값을 명시해 해결할 수도 있다. 이 명령의 revision/operation이 migration보다 우선하므로, 늦은 후보가 명시 선택을 덮어쓰지 못한다.
4. import는 migration 상태인 row에만 CAS한다. reset tombstone이나 명령으로 확정한 row에 재적용하지 않는다. old local 파일/백업을 자동 수정·삭제하지 않는다. 다른 설정 저장이 옛 값을 round-trip해도 PG보다 우선하지 않는다.
5. 양 Mac이 PG home을 읽는 binary floor가 되기 전에 production derived routing을 켜지 않는다. 공유 home이 미해결인 기존 채널은 silent default로 처리하지 않고 pending/conflict를 표시한다.

증명은 서로 다른 runtime root+공유 PG에서 **A의 `/node` 저장 → 일반 입력 없이 gateway B로 교체 → 새 입력**이다. reset·반대 방향·늦은 import·PG ACK 유실·owner row 없음까지 시험한다. 로컬 우선/첫 admission까지 쓰기 지연/reset row 삭제 mutant가 모두 RED여야 한다.

## 4. 입력별 판단과 양방향 배치 — P1-1

새 canonical input에 대해 G가 shared home과 두 Mac의 수신 가능성을 읽는다.

`desired = H if available(H) else peer(H) if available(peer(H)) else Held`

available은 기존 DB NOW 기준 node heartbeat/lease TTL + 해당 provider intake/control/REST 준비·auth/workspace 요구를 사용한다. 단순 native busy/capacity 점유는 unavailable이 아니며 같은 owner의 정상 입력 처리 규칙을 따른다. PG 오류·home conflict는 Unknown/Held다. G 자신도 실행 서비스가 준비되지 않았으면 unavailable일 수 있다. **`desired = gateway` 예외는 없다.**

home이 실제로 켜져 있어도 heartbeat/control 연결만 끊기면 peer를 선택할 수 있다. 이는 전원 오판정이 아니라 관측한 수신 가능성에 따른 라우팅이다. 같은 shared PG에 접근하는 old home은 원 generation이 폐기되면 새 효과가 차단되고, PG에 접근하지 못해도 새 효과를 시작하지 않는다. 이미 시작된 원 작업은 새 Mac에 복제하지 않는다. 따라서 알림도 “전원이 꺼졌다”가 아니라 “입력을 받을 수 없다”로 쓴다.

source가 이미 begun/withheld/accepted이면 기존 target/처분을 먼저 따르고 새 요청으로 다시 고르지 않는다. 아직 시작 전인 held source는 admission 직전에 home을 다시 읽을 수 있다. 선택과 실제 effect는 §6의 source/owner guard로 묶는다.

### 대칭 배치 행렬

A/B는 Mac 이름이 아니라 서로 다른 stable instance다. 각 행은 preferred gateway 값을 A와 B로 각각 놓고 반복한다.

| Case | Home | 입력 gateway | home 수신 상태 | 선택/실행 위치 | O writer/답 위치 |
|---|---|---|---|---|---|
| M1 | A | A | 가능 | A | A local source + owner REST |
| M2 | A | B | 가능 | **A(remote)** | **A**, B websocket 이동 없음 |
| M3 | A | A | gateway는 생존, A 실행 서비스 불가 | **B(remote)** | **B**, gateway A 유지 |
| M4 | A | B | A offline | B | B |
| M5 | B | A | 가능 | **B(remote)** | **B**, A가 preferred여도 동일 |
| M6 | B | B | 가능 | B | B |
| M7 | B | A | B offline | A | A |
| M8 | B | B | gateway는 생존, B 실행 서비스 불가 | **A(remote)** | **A**, gateway B 유지 |

G=H인데 home unavailable인 M3/M8은 **Mac 전체 전원 off가 아니라 실행 서비스 불가**다. Mac 전체가 꺼져 gateway도 없으면 살아 있는 쪽이 기존 gateway lease를 얻은 뒤 M4/M7 등으로 판단한다. gateway가 아직 없으면 Discord 수신/admission을 할 수 없으며 이미 PG에 보관된 입력은 그대로 남는다.

필수 통합 fixture는 같은 token의 `X.home=A, Y.home=B`다. G=A에 고정해 두 채널 각각 home에서 실행 → B unavailable이면 Y만 A → B 복귀 새 입력이면 Y만 B, X는 A 유지. A/B와 G를 바꿔 반복한다. **O의 한 방향을 off로 제외해 대칭 수용 완료라고 하지 않는다.**

## 5. execution owner의 O/local 세션·출력

### 5.1 비-gateway owner도 production writer를 시작

routing-managed O 채널에서는 `gateway_preferred_instance_id`와 channel home의 결합을 끊는다. 그 설정은 token gateway preference에만 남는다. 기존 O output membership/provider 분류와 native source 검증은 유지한다.

선택 전 readiness는 **owner writer를 만들 수 있는 capability/REST/control 준비**이며 해당 generation의 actor가 이미 돌아간다는 조건이 아니다. `owner CAS → 그 permit으로 writer 준비 → pending claim/효과` 순서다. 아직 owner가 아니어서 writer가 없다는 사실을 unavailable로 읽으면 순환 대기가 되므로 그렇게 구현하지 않는다. 준비 실패는 pending/구체적 오류를 보존하고 다음 bounded reconcile에서 시작 전 source만 다시 판단한다.

변경 계약은 다음과 같다.

1. `cutover::claim_for_placement`와 `intake_route`는 local/foreign **execution owner permit**을 해석한다. G에서 local writer가 안 보인다고 remote owner를 unavailable로 분류하지 않는다. worker의 `hold_off_o_gateway`도 target owner의 local writer 준비를 검사하는 계약으로 바꾼다.
2. `writer::host::start_managed`의 `pg_gateway: bool` 대신 routing-managed 채널에 `OwnerPermit(provider, channel, E, generation, session_ref)`를 전달한다. `pg_gateway=true`를 위조하거나 gateway Owned를 가짜로 올려 우회하지 않는다. 비-routing Legacy 경로의 기존 gate는 전개 기간 구분한다.
3. owner host는 `resolve_intake_worker_runtime`이 구한 **E의 REST client와 bot identity**로 port를 만든다. `cached_serenity_ctx`를 기다리지 않는다. 현재 test-only REST port를 production의 bounded 준비/오류 처리 경로에 연결한다. token/auth가 없으면 readiness 불가이며 준비 완료로 광고하지 않는다.
4. owner reconciler/actor는 gateway와 standby 양 역할에서 시작된다. actor의 local ownership gate는 PG channel owner에서 파생되고 provider-global gateway gate와 별개다. 마지막 외부 효과는 언제나 §6.2의 PG guard를 거친다. local DeliveryLeaseCell만으로 Mac 간 배타성을 주장하지 않는다.
5. 부팅/rebind 시 PG owner/generation을 **local watcher/actor 재개보다 먼저** 읽는다. current owner인 자기 local 세션만 복원한다. foreign/superseded이면 실행/출력은 닫고 §7의 정리 상태만 재개한다.
6. gateway 선호에 의한 기존 transport role 교체는 별개다. channel return이 gateway yield/선호값 수정을 호출하지 않는다. transport 재시작이 발생해도 native local 세션·PG custody는 보존하고 같은 owner generation/로컬 O 영수증에서 복원한다. `self_fence_gateway`가 채널 owner를 바꾸거나 owner의 normal end를 호출하지 않도록 lifecycle을 분리한다.

E는 자기 Mac의 local session/source만 읽는다. local session이 없으면 정상 fresh bootstrap을 사용하고 home의 provider ID/transcript/inflight를 복사하지 않는다. 같은 E@g의 유효한 local 세션은 재사용 가능하다. 이전에 폐기된 generation의 살아 있는 세션을 새 generation으로 재표시하지 않는다. target 준비에 그 exact local 세션의 종료가 필요하면 기존 normal end 뒤 fresh를 사용한다. home 반환에서도 **새 세션 선택이 더 간단한 경우 fresh를 권고**하며 peer session resume은 없다.

### 5.2 old 출력의 generation을 보존

stamp는 actor/세션이 최신 PG generation을 조회해 붙이는 값이 아니다. **원 source/admission과 native runtime binding에서 받은 값**이다. sealed O piece/legacy reply/outbox가 처음 만들어질 때 그 origin stamp를 보존한다. 재시작/replay도 그 stamp를 사용한다.

여기서 g는 PG `intake_session_owners.generation`이다. O readiness claim/로컬 DeliveryLease의 generation 카운터와 이름이 같아도 서로 대체하지 않는다. local actor 수명과 PG 실행권을 별도 필드로 유지한다.

새 writer가 같은 local transcript를 읽더라도 old piece를 현재 generation으로 바꾸지 않는다. source/session incarnation을 식별할 수 없는 old pending output은 withheld/unknown으로 남긴다. 이미 배달된 unit은 기존 영수증으로 수렴한다. direct TUI의 revoked session output도 그 binding의 old stamp로 막힌다. 이 연결을 빼고 “current owner가 B니까 B의 모든 출력 허용”이라고 구현하면 P1-3이 다시 생긴다.

## 6. source·PG 보관·실제 효과 계약

### 6.1 Mac 간 custody: 로컬 queue와 구분

새 return 전용 원장을 추가하지 않고 기존 `intake_outbox`/CanonicalInput/BundleV1을 연결한다.

- canonical receipt: 기존 `replay_only=true, registered_not_started` row에 원 source·본문·첨부 refs를 보존한다. 이 row만으로 worker 실행이 되지는 않는다.
- **staged regular admission:** 같은 canonical receipt를 참조하는 non-replay delivery row를 PG에 만든다. 제안 `routing_state=held` 동안 status는 기존 `unknown`, claim/accepted/spawned는 비어 있다. 원 source/attempt/idempotency key·full payload·BundleV1 refs·proposed target·wait owner generation을 담고 local queue와 독립적으로 조회된다. 이는 자동 재실행 금지인 ordinary unknown row와 새 routing_state로 구분한다.
- admission 승격: switch/같은-owner 허가 시 동일 row를 원자적으로 `pending`, target E, owner stamp E@g로 바꾼다. local G=E도 같은 claim 경계를 소비한다. 기존 one-open-route unique는 held rows를 제외하므로 B native 작업 중 새 입력들을 PG에 보관할 수 있다. 기존 open delivery가 있으면 먼저 그 delivery/처분을 확정한다. delivery done이 native turn 완료를 뜻하지는 않는다.
- 필요 최소 schema는 regular row의 `routing_state`, `routing_receipt_id`, `routing_home_revision`, `routing_wait_owner_generation`이다. `routing_receipt_id`는 FK 및 non-null regular row의 unique로 같은 receipt의 두 실행 admission을 막는다. held는 `status=unknown/replay_only=false/미claim` CHECK로 구분하고 worker가 선택하지 않는다. 원문/첨부를 별도 반환 테이블이나 파일에 복사하지 않는다. 승격은 oldest held 순서를 지키며 m3가 m2를 건너뛰어 별도 switch를 만들지 않는다.
- checkpoint/기존 local held head 해제는 **PG의 full payload+refs commit/read-back 성공 뒤**에만 한다. “B 로컬 파일 저장 성공”은 handoff ACK가 아니다. crash/ACK 유실이면 source/idempotency로 같은 staged/admitted row를 찾는다.
- worker는 새 target에서 row를 claim하고 body/refs를 복원해 같은 source begin으로 실행한다. started/withheld source는 새 node/attempt로 등록하지 않는다. old owner의 accepted/spawned row가 남았으면 exact source 처분 API로 보존·종결하고 **새 source만** 승격한다. force-fail-and-retry-as-new의 payload 복사는 사용하지 않는다.
- Bundle pin: routing-held/admitted row가 참조하는 bundle은 **put 중 만료 삭제(:83), load(:123), 주기 cleanup(:207) 모두**에서 유효 custody를 인정한다. load/cleanup 한쪽만 고치지 않는다. PG stage 조각이 이 작은 수명 연결을 소유하며, row/receipt의 retention도 미완료 custody를 보존한다. source/provider/channel/hash 검증과 기존 용량 한도는 유지한다. 만료/손상/보관 실패를 빈 첨부로 바꾸지 않는다.

필수 수직 시험은 **m2 PG stage → target 전환 → 이전 runtime 완전 종료 → 다른 runtime root의 새 target이 동일 source/body/첨부 hash로 실행1**이다. 원 runtime queue 파일을 읽을 수 없게 만든 fixture여야 한다.

### 6.2 효과 경계와 bounded switch

기존 OwnerIdentity advisory key·unique active·owner CAS를 재사용한다. 추가 execution epoch는 없다.

- source의 begin은 실제 raw submit 전 durable하게 확정한다. 그 뒤 actual first submit/inject는 같은 owner key의 짧은 PG effect transaction에서 E@g와 exact source/session을 재검사하고 handoff까지 guard를 유지한다. begin과 effect 사이 owner가 바뀌면 raw 효과0이고 exact no-effect 결과만 기록한다. 새로운 source/attempt를 추측 생성하지 않는다. crash로 no-effect를 증명하지 못하면 unknown/withheld다.
- 모델 답·tool/terminal 표시의 POST/EDIT/DELETE 및 delayed outbox도 원 stamp로 같은 guard를 소비한다. PG 오류/old generation은 raw 외부 요청0이다. check 후 guard를 풀고 HTTP를 나중에 시작하지 않는다.
- 외부 요청은 기존 bounded submit/POST timeout 안에서 끝낸다. lock을 얻기 전 다른 pool lookup을 마치며 guard 안에서 별도 pool acquire를 기다리지 않는다. source begin의 tx variant/동일 connection 사용 여부도 공통 API에서 고정한다.
- effect transaction에는 operation timeout보다 긴 **local idle-in-transaction timeout**을 설정해, 사라진 client가 owner lock을 무기한 유지하지 않게 한다. O의 POST_TIMEOUT은 현재 `src/services/tui_o/writer/deliver.rs:33`의 60초이므로 기본 최대 guard 창은 60초+5초 정리 여유다. 기능 readiness는 이 local timeout이 실제 적용됨을 확인한다. DB global 설정/세션 kill/권한 확대는 하지 않는다.
- switch는 같은 key 아래 fresh snapshot으로 previous owner를 supersede하고 new E@g를 만든다. old pending request를 새 세대에 stamp하지 않는다. row-first worker는 main의 try-owner 실패 rollback을 유지해 lock 순서 역전을 피한다.
- 이미 wire에 나간 HTTP를 generation 변경이 취소하지는 못한다. timeout/ACK 불명은 기존 uncertain 영수증에 남겨 blind retransmit을 막는다. 이 외부 결과 불명은 visible 0/PASS로 세지 않는다. 리뷰가 인정한 transport 잔여 한계이며 전원 증명이나 전체 node quiesced ACK를 되살리지 않는다.

새 source의 source producer/first-effect 연결이 없는 `LegacyUnprotected` 상태에서는 이 기능을 켤 수 없다. 이것은 #7330 전체 retry producer를 켜라는 조건이 아니다. routed TUI 입력의 register/begin producer와 공통 보호 read만 해당 범위에서 연결한다.

| 경합 | 승자와 나머지 처리 | 증명 |
|---|---|---|
| 같은 source가 live/catch-up/두 gateway에서 도착 | receipt key+regular receipt unique+begin CAS로 같은 admission/attempt에 수렴 | R10, 중복 producer 동시 barrier |
| old submit/POST와 owner switch | 같은 OwnerIdentity lock 선점자가 먼저. old guard 뒤 switch, 또는 switch 뒤 old raw0 | R16, raw 호출 직전 latch |
| home 변경과 입력 admission | 같은 key 아래 home revision을 재검사; 설정 변경이 먼저면 새 H, admission이 먼저면 그 입력의 저장된 H/target | R03/R10, revision 재검사 제거 mutant |
| B control 응답과 unavailable revoke | expected old generation/operation CAS. 늦은 응답이 새 owner를 복원하지 않음 | R06/R09, deadline 직전·직후 응답 |
| old close callback과 B 새 session 생성 | local exact native pin+operation 비교. old callback은 새 incarnation 종료0 | R07, native ref 검사 제거 mutant |

## 7. 전환·복귀·지연 정리 — P1-3/P2-1

### 7.1 상태를 나누는 기준

H/peer 어느 방향이든 `old E → desired D` 전환 함수는 같다. gateway는 old owner에 물리적 종료를 강제로 요청할 권한을 새로 얻지 않는다.

| old E의 상태 | 새 입력/전환 | 세션 정리 |
|---|---|---|
| 같은 desired | 같은 E@g에서 처리 | 불필요 |
| 제어 가능·native 작업/답 진행 중 | PG staged row로 대기. current 작업 끝까지 두고 새 입력은 주입하지 않음 | exact native/답 완료 후 normal end |
| 제어 가능·idle·normal end 성공 | old end 결과 확인 뒤 CAS+pending admission+알림 commit | 완료 |
| node stale/offline 또는 bounded control 응답 없음 | source/미결 응답 보호 → old generation 폐기 → **새 source만** 건강한 D에 admission | **deferred**, 종료 성공으로 기록하지 않음 |
| idle이나 normal end가 일시 실패/확인 불명 | bounded 재시도. 예산 내 성공하면 정상 전환, 예산 초과면 old generation 폐기 후 새 입력 진행 | 실패/불명은 deferred로 남김 |
| idle normal end의 영구 거절/사용자 확인 필요 | 무리하게 종료하지 않고 generation 폐기 후 새 source 진행 가능 | needs_operator, 자동 종료 반복 없음 |
| shared PG/target D가 불가 | PG source 보존·Held | healthy target이라고 가장하지 않음 |

물리 종료보다 새 source 진행을 앞세우는 예외는 home→peer와 peer→home 모두 동일하다. 이미 실행된 old source를 새 Mac에서 replay하지 않으며, old 작업 결과가 불명한 사실도 그대로 보존한다.

### 7.2 재개 주체·기한·idempotency

제안 `channel_execution_owner_reconcile`은 node-local 관리 worker다. gateway/standby 모두에서 동작하며 **이미 PG에 기록된 pending 입력/종료 요청/지연 정리**만 방문한다. availability만 보고 자발적으로 owner를 바꾸는 감시 loop가 아니다. long provider execute를 기다리는 intake tick 안에 가두지 않고, 같은 provider lifecycle의 독립적인 bounded pass로 둔다.

- pass 주기는 기존 intake idle cadence(현재 `intake_worker.rs:227`의 2초)를 사용한다. native 완료 callback·node 재등록은 같은 pass를 깨우는 추가 신호다. callback 하나만을 유일한 재개 계기로 삼지 않는다.
- old owner row에 desired/home revision, canonical pending row id, exact local session/native nonce, control operation ID, response state, deadline/next_attempt_at를 보존한다. generation history가 deferred cleanup의 저장소다. 별도 반환 queue/host lifecycle ledger는 없다.
- 제어 응답의 기한은 요청 생성 시 DB NOW+기존 `forward_pre_claim_timeout_secs`(기본 12초, `src/config.rs:1000`)로 고정한다. Busy 응답은 exact current 작업/배달 상태와 갱신된 control freshness를 가져야 한다. **native 턴 자체의 12초 제한이 아니다.** 정상 응답 중인 Busy 작업은 끝까지 기다린다.
- node/제어 응답이 끊기면 마지막 control freshness/요청 기한 뒤 unavailable 분기로 간다. 한 번의 endpoint/worker 불응에 무한 재요청하지 않는다. m3/m4가 들어왔다고 old generation의 기존 요청 deadline을 매번 새로 늘리지 않는다. 기본 control 불응은 12초+다음 pass, 이미 잡힌 output guard가 있으면 그 bounded 창 뒤 새 target이 진행한다. old session-end ACK 자체를 기다리지는 않는다.
- idle의 transient normal-end 실패는 같은 operation/session ref로 다음 pass에 재확인한다. 새 입력 없이도 재개한다. automatic attempt는 최대 3회이며 횟수/next_attempt_at는 재시작 후에도 유지한다. deadline/예산 초과는 deferred로 옮겨 새 입력을 풀어 준다. 영구 거절은 needs_operator로 두고 자동 시그널을 반복하지 않는다.
- local end future는 exact session transition/pin을 끝날 때까지 소유한다. coordinator의 control timeout이 local guard를 조기 해제해 늦은 종료가 새 세션을 때리지 않도록 한다. callback은 old generation+operation+native ref의 CAS 결과만 기록한다.

### 7.3 old Mac 재접속과 physical cleanup

재접속한 B는 actor/restore 전에 PG를 읽는다. latest owner가 A이거나 g가 다르면 old B submit/POST를 허용하지 않는다. old g의 deferred row를 자기 node-local reconciler가 조회한다.

- exact old session이 이미 없으면 기존 확인 경로로 종료를 확정한다.
- 같은 old session이 idle이면 기존 normal end만 호출한다. Busy면 old 작업 종료를 기다리되 old 출력은 계속 fenced다. 그 기다림이 A의 새 입력을 막지는 않는다.
- native nonce/host pin이 달라 새 B 세션이 있으면 old cleanup은 그 세션을 종료하지 않는다. identity_changed/needs_operator로 보존하고 같은 이름만으로 kill하지 않는다.
- old cleanup이 아직 끝나지 않은 local slot을 새 generation의 세션으로 재표시하지 않는다. target 준비가 그 slot을 필요로 하면 normal cleanup 뒤 fresh를 사용한다.
- 상위 `/clear`의 queue 삭제·clear boundary·checkpoint 변경은 쓰지 않는다. 기존 local normal end의 process/tmux/launch-file 처리 범위를 넘는 API·삭제 대상은 추가하지 않는다.

필수 순차 재현은 **A 없음→m1 B 완료→A 복귀(입력 없음)→B 종료→m2**다. m2는 A에서 실행1/답1이어야 하며 B close ACK를 요구하지 않는다. B 재접속 시 old output0, old source replay0, 새 B 세션 종료0을 확인한다. 역할을 뒤집어 반복한다.

## 8. 세대별 전환 알림과 outbox identity — P2-2

전환 분류는 `to == home`인 ToHome, `to == peer(home)`인 ToPeer 두 가지다. 문구는 **`from, to, home, created_new_session, reason` 인수**로 렌더한다. Mac 이름은 worker display label 또는 stable instance ID를 사용한다.

- ToPeer: `“{home}에서 입력을 받을 수 없어 {to}에서 처리합니다.”` 새 세션을 실제 선택했을 때만 `“{to}의 새 세션에서…”`로 바꾼다.
- ToHome: `“{home} 연결이 돌아와 이 채널의 입력을 다시 {home}에서 처리합니다.”` `/node` 설정 변경으로 목적지가 바뀐 경우에는 “home 설정을 적용해 …”로 바꿔 복귀 사실을 꾸미지 않는다.
- config 변경·세션 재생성·gateway 교체·같은 B 계속 사용·close retry/대기 중에는 전환 알림0이다. 실제 owner Mac 변경 commit만 이벤트다.
- owner 이력이 전혀 없는 채널은 설정 H를 초기 routing 기준으로 삼는다. 첫 입력이 H면 알림0, H 불가로 peer를 처음 선택하면 `H→peer` 알림1이다. 이력이 있으면 released 상태까지 포함한 마지막 실행 Mac과 비교해 단순 재시작을 새 전환으로 만들지 않는다.

### 실제 중복키 매핑

`K = encode_v1(OwnerIdentity.provider, raw_channel_id, new_generation, from_instance, to_instance)`를 길이 구분 인코딩하고 `F = full_fingerprint(K)`라 둔다.

| 값 | 실제 저장/사용 |
|---|---|
| immutable event_key | owner 새 세대 행에 K(또는 전체 F+원 좌표)를 보존 |
| outbox target | `channel:<raw_channel_id>` |
| outbox source | `discord/outbound/source_registry.rs`에 등록한 routing transition source; 일반 모델 출력 source로 위장하지 않음 |
| outbox session_key | **`routing-transition:<F>`** (provider 세션 선택용 locator와 혼용하지 않는 domain event identity) |
| outbox reason_code | `routing.to_peer` 또는 `routing.to_home` |
| 실제 dedupe_key | 기존 `dedupe_key_for_message(target, content, reason_code, session_key)` 사용. **F가 session_key에 들어가므로 generation 포함** |
| Discord nonce | `fingerprint("routing-notice-nonce-v1", K)`로 고정, enforce_nonce 사용 |
| read-back 포인터 | owner 세대 행의 notice_outbox_id, post_started_at/state, notice_message_id |

owner CAS+새 input admission+**`enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx`**(`message_outbox.rs:246`)+포인터 기록을 한 tx로 commit한다. 이 helper는 중복 시 기존 id를 반환한다. TTL helper의 None을 새 id 생성 허가로 해석하지 않는다.

failed/cancelled outbox는 기존 partial unique에서 빠질 수 있으므로 owner의 notice_outbox_id 포인터를 먼저 확인한다. 같은 event 재개는 그 row/id/nonce를 사용하고 다른 세대는 다른 K/id/nonce다. failed를 이유로 세대 포인터를 비워 알림을 새로 만들지 않는다. event session_key를 실제 native session으로 해석하는 delivery routing을 사용하지 않고 routing 전용 sender에서 읽는다.

POST는 기존 nonce/time/history reconcile transport를 재사용한다. 이미 sent/조회로 확인된 같은 nonce는 기존 message_id로 복구한다. nonce 창 밖 history 불명은 uncertain·blind repost0이며 sent1로 세지 않는다. 정상/조회 성공은 visible1, 외부 결과 불명은 visible≤1+uncertain이다. 운영 알림 provenance를 붙여 입력 되먹임을 막는다. 모델 output의 current-generation fence와 **역사적 전환 event의 pointer fence**를 구분해 과거 알림을 현재 세대로 재작성하지 않는다.

sender의 outbox claim과 event pointer 검사는 같은 event에 두 전송자가 붙는 것을 막는다. A→B→A가 빠르게 일어나도 이전 event를 “현재 generation이 아님”으로 취소하지 않으며 generation 순서로 원 event를 배달/uncertain 확정한다. 지연 전송은 commit 시 기록한 from/to와 전환 시각을 사용한다.

필수 시험은 **동일 native session·동일 문구·TTL 안 A→B→A→B**다. 세 event는 서로 다른 outbox id/nonce이고 각각 알림1이다. 같은 event의 ACK 유실 재개는 같은 id/nonce다. dedupe session_key에서 generation/F를 빼는 mutant는 마지막 고지 누락으로 RED여야 한다.

## 9. 전개와 완성 후 정책 — P2-3

설정 위치는 기존 `cluster.intake_routing` 아래 제안 키 `input_fallback_enabled: false`, `input_fallback_scope: pilot|derived`, `input_fallback_pilot_channel_ids: []`다. 운영 파일을 읽거나 수정하지 않는다. 초기에는 scope=pilot과 검증 channel/parent 목록으로만 실제 동작을 켠다. 준비 조각은 어떤 목록/flag로도 submit·POST·session-end를 켤 수 없는 PreviewOnly 상태다.

완성 후 기본은 **`scope=derived`**다. 기존 shared home/agent binding에서 home을 해석할 수 있는 정상 사용자 채널은 자동 대상이 된다. 새 채널마다 별도 fallback allowlist를 유지하지 않는다. thread는 기존 policy parent 상속을 따라 자동 포함하고 raw source/owner identity는 그 thread를 유지한다. DM은 자체 shared home/기본 agent home을 해석할 수 있을 때 포함한다.

- 관리 대상인데 HomeUnknown/conflict인 채널은 held와 이유를 표시한다. 임의 gateway로 실행하지 않는다. 본래 home이 없는 비관리 agentless 경로를 fallback으로 새 편입하지 않는다.
- 전체 `input_fallback_enabled=false` kill switch는 유지한다. 실제 rollout에서는 coordinator가 pilot으로 배포하고 검증 후 derived로 전환한다. 완성 제품에서 pilot 목록을 필수 설정으로 남기지 않는다.
- off는 **새 fallback 선택**을 멈춘다. begun source는 원 target에서 끝내고, PG pending 입력·old generation fence·지연 정리·미배달 event는 계속 해석한다. off가 동시에 A/B를 열거나 custody를 버리는 우회가 아니다.
- off에서 새 입력은 H가 가능하면 보호된 정상 home 경로로 보내고, H가 불가하면 held다. 기존 peer owner가 있다는 이유로 새 fallback 입력을 계속 admission하거나 LegacyUnprotected로 우회하지 않는다. 이미 begun인 peer 턴은 위 규칙대로 마친다.
- 양 Mac의 shared-home reader, routed source producer, PG custody promotion, first-effect/output fence, owner REST host/reconciler capability가 모두 확인된 뒤 pilot을 허용한다. derived 완료는 M1~M8/O 양방향 수용 뒤다. 반대 방향을 off로 남긴 상태는 준비 상태일 뿐 완료가 아니다.
- 활성화 전에 기존 local live session은 exact native binding으로 owner adoption/보관 상태를 확정한다. origin stamp가 없는 옛 output을 현재 g로 채우지 않으며 unknown/미확정 row를 unprotected로 통과시키지 않는다. 같은 runtime의 예전 writer와 새 owner writer를 동시에 열지 않는 boot/adoption fixture를 포함한다.

## 10. 외부 의존의 최소 계약과 선행 착수

#7272/#7330 전체 이슈 완료는 선행조건이 아니다. 아래 공통 계약의 해당 조각만 필요하다. 기존 코드가 이미 전부 제공한다고 가정하지 않는다.

| 계약 | 현재 근거 file:line | 담당·r3에서 필요한 최소 결과 |
|---|---|---|
| C1 canonical source와 원 입력 보존 | `src/db/replay_disposition/write/start.rs:8`, `:32`, `:135` | #7272 owner 통합 조각 + #7336 P-B. source/원문/refs를 등록하고 같은 source를 target/generation 변경으로 새로 만들지 않는 공통 API |
| C2 실제 first submit/inject 경계 | `src/services/discord/router/intake_gate.rs:1117`, `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:49`, `src/services/cluster/intake_worker.rs:434` | #7272 adapter/bootstrap/owner 조각. Discord·HTTP·routine의 TUI 입력이 같은 source/OwnerPermit를 실제 raw 효과 전 소비. 마지막 불용 분기 정리까지 기다릴 필요 없음 |
| C3 PG staged→pending Mac 간 handoff | `src/services/turn_orchestrator/pending_queue_persistence.rs:92`, `src/services/discord/router/intake_dispatch.rs:373`, `src/services/cluster/intake_router_hook/owner_record.rs:542` | **#7336 P-B가 소유**. #7272 local queue를 shared로 오인하지 않음. stage와 promotion·checkpoint ACK, source receipt 연결, Bundle pin을 직접 연결 |
| C4 claim/효과/출력의 generation fence | `src/db/intake_outbox/worker_status.rs:55`, `src/services/discord/outbound/o_writer_io.rs:88`, `src/services/discord/turn_bridge/headless_delivery.rs:276` | #7336 P-B/P-C, #7272 output context 접합. 원 artifact stamp 보존·마지막 raw 요청 guard. 단순 planning stamp로 완료 처리 금지 |
| C5 보호 reader와 새 routed source producer | `src/services/discord/router/intake_dispatch/replay_admission.rs:35`, `:253`, `:302` | #7272 source owner가 routed TUI register/begin을 공급. #7330의 reader/처분 API 재사용. headless retry/복구 producer 전체 활성화는 별도이고 이 feature의 선행 아님 |
| C6 normal 완료·종료·재개 | `src/services/discord/turn_finalizer.rs:538`, `src/services/discord/turn_bridge/finalize_epilogue.rs:69`, `src/services/discord/commands/control/clear_teardown.rs:59` | #7336 P-C reconciler+기존 #7272 완료 observer. source/본문을 보관한 채 callback/periodic pass로 재개. /clear 상위 호출로 대체하지 않음 |
| C7 비-gateway owner REST/O | `src/services/tui_o/writer/host.rs:505`, `src/services/discord/runtime_bootstrap/o_writer_host.rs:88`, `src/services/discord/health/runtime_resolve.rs:354` | **#7336 P-C가 소유**. production REST client와 channel gate, gateway/standby boot·restart 복원. 일반 #7272 완료를 기다리며 공백을 남기지 않음 |

지금 가능한 준비 작업은 세 가지다.

1. **A1 판단·대칭 fixture:** pure `choose_target(home, peer, availability)`와 M1~M8 및 gateway/preferred 독립성. health/preview read에만 연결하고 owner 변경/실행0.
2. **A2 shared home 저장 모델:** schema, revision/reset/import 우선순위, 두 runtime root+throwaway PG 명령 저장 fixture. production `/node` writer·legacy import는 아직 연결하지 않아 운영 설정 변경0.
3. **A3 notice 구성 요소:** K/nonce/실제 outbox dedupe 매핑·문구·POST recovery adapter fixture. 운영 sender·실제 전환 이벤트에는 연결하지 않아 채팅 POST0.

A2/A3의 mutation helper는 테스트에서 사용하거나 최종 caller와 함께 노출한다. 새 allow/baseline 인상으로 dormant 경고를 숨기지 않는다. 준비 단계 status가 출력할 수 있는 것은 preview이며 성공한 라우팅/배달 계수를 올리지 않는다. 이 준비 착지가 이후 C1~C7 활성화 증거를 대신하지 않는다.

## 11. 수용 시험·뮤턴트

이번에는 설계 명세만 작성했으며 시험을 실행하지 않았다. 정적 검토가 시험 PASS로 둔갑하지 않게 양성 효과 계수와 금지 계수를 함께 요구한다.

| ID | fixture와 기대 | RED 조건 / mutant |
|---|---|---|
| R01 대칭 | M1~M8 × preferred A/B × Claude/Codex × Legacy/O = 최소 64 배치 | target=gateway, home=preferred, Mac 이름 하드코딩 복원 시 하나라도 잘못된 노드/remote O POST0이면 RED |
| R02 같은 token 두 home | X.home=A/Y.home=B, G=A 고정→Y만 fallback/복귀; G=B·역방향 반복 | 다른 채널 이동/전역 handback 요구/반대 O off 처리면 RED |
| R03 shared /node | 분리 root A/B, 공유 PG. /node A 저장 후 일반 입력 없이 G=B, 첫 입력 A만 실행. reset·역방향·no owner row 반복 | local map 우선, 최초 admission까지 PG write 지연, reset tombstone 삭제 mutant RED |
| R04 legacy import | 동일/한쪽만/충돌/보고 누락·늦은 import·명시 설정/PG ACK 유실 | 임의 winner/명령 overwrite/실패를 saved로 표시하면 RED |
| R05 정상 복귀 | old B 제어 가능 Busy, m2 home A 대기, B native+답 완료 후 normal end→A 실행1 | m2를 B 주입/A 조기 실행/상위 clear의 queue 삭제/중간 prose만 완료면 RED |
| R06 불응 fallback | A 없음→m1 B 완료→A 복귀 입력 없음→B 종료→m2; 역할 반전 | close ACK를 무조건 요구해 m2가 deadline 뒤에도 A에서 실행0이면 RED. source m1 재실행도 RED |
| R07 old B 복귀 | R06 뒤 B 재접속, old O/legacy piece와 outbox, 새 B incarnation fixture | old generation submit/POST0, old cleanup의 새 session 종료0. current generation 재부착/이름만 end mutant RED |
| R08 close 재개 | 첫 close transient 실패→해소, 추가 사용자 입력 없음 | reconciler가 같은 m2를 재개해 A 실행1 못 하면 RED. callback만 재개/실패 row 제외 mutant RED |
| R09 budget/영구 거절 | control 불응·3회 transient·confirmation_required, healthy home/PG | 무한 대기·자동 kill 확대·종료 성공 허위표시·매 입력 deadline 연장이면 RED. 물리 cleanup은 deferred여야 함 |
| R10 same source | 두 ingress·live/catch-up/worker·G 교체·commit ACK 유실·node 변경 | 실행/주입>1, new attempt로 old started/withheld 재실행이면 RED. source/owner fence 각각 제거 mutant RED |
| R11 shared custody | m2 PG stage→switch→이전 runtime 종료/로컬 queue 접근 불가→다른 root target 실행 | 동일 source/body/첨부 hash·실행1 미달, receipt만 만들고 pending 승격 없음, local 파일 ACK만 사용하면 RED |
| R12 bundle 수명 | attachment-only/본문+첨부, held/remote, bundle expiry clock 진행 | 참조중 bytes 삭제·load 실패를 빈 첨부로 통과·normal end가 m2 첨부 삭제하면 RED |
| R13 nonce/dedupe | 같은 session/reason/문구로 A→B→A→B, TTL 안/재시작/ACK 유실 | event별 id/nonce/visible1 불일치, same event 새 id면 RED. dedupe에서 generation 제거 mutant RED |
| R14 외부 POST 불명 | nonce 창 안 재시도·밖 history 조회 Found/불명/절단 | Found=기존 message1, 불명=visible≤1+uncertain. blind repost/uncertain→sent/추가 bot fallback이면 RED |
| R15 owner REST 복원 | E≠G, cached_serenity_ctx=None, 실제 production port/mock REST; G 교체·E 재시작 | E local native/source에서 POST1 못 하거나 G port 대기, peer transcript 읽기, source 재실행이면 RED |
| R16 gate 실제 소비 | submit·O POST·legacy EDIT/DELETE·outbox에 old g, PG 오류, effect 직전 switch | old raw 요청0. guard를 check 뒤 해제하거나 artifact에 최신 g 붙이면 RED. bounded DB lock 해소도 확인 |
| R17 derived/off | 새 정상 home 채널/thread/DM, shared conflict, global off 중 begun/held/deferred | allowlist 누락으로 정상 대상 제외, conflict→임의 실행, off→기존 source/epoch 보호 유실이면 RED |
| R18 입력 없음/같은 target | home down/up·gateway 변경만, 또는 m2/m3 계속 peer | source 없는데 route/close/notice 발생, 같은 active E@g를 불필요하게 교체하거나 같은 Mac에 전환 알림을 내면 RED |
| R19 local fresh/reuse | peer에 local session 없음/동일 E@g 유효 session 있음, H 복귀 fresh 선택 | peer fresh1 또는 유효 local 재사용·응답1. home/peer native ID 이식·옛 generation 재표시·첫 fallback 알림 누락이면 RED |
| R20 전개/adoption | 기능 off 때 생긴 live native session·pending legacy output과 새 owner boot | exact binding 확정 전 unprotected submit/POST, old output에 current g 채우기, 구/신 writer 동시 시작이면 RED |
| L01 실제 두 Mac | 양 방향 home/peer, G와 E 다른 배치 포함, busy 복귀·fallback off 후 healthy home 새 입력 | 실제 native ID/답 출처/세션 종료 또는 deferred/기기 전환 알림 수를 연결하지 못하면 NOT_RUN. unit만으로 live 완료 금지 |
| L02 실제 첨부 | gateway 공백 포함 attachment-only와 본문+첨부, PG stage 후 다른 Mac | source→bundle hash→target local file→provider 읽기·응답1 실측. text path만 확인하면 RED |

## 12. 구현 단위·크기·giant

상한은 `/home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py:14`의 prod 30파일/순 +1500. **각 조각 +1200 예상 전에 PLAN_READY로 재분할**한다. 아래 수치는 코드가 없는 설계 추정이며 cap PASS가 아니다.

| 단위 | 내용·파일 후보 | prod 추정 / 선행 |
|---|---|---|
| P-A 즉시 준비 | 신규 shared-home schema/helper·pure decision·event identity/render, cluster 모듈/preview, 대칭/import/notice fixture | 10~14파일 / +550~750. #7272/#7330 착지 불필요. provider/POST/normal end/production home write0 |
| P-B 공통 입력·공유 custody | `/node` PG writer/read·legacy import 연결, intake_dispatch/worker_entry/worker_status, staged admission/source begin, Bundle pin, #7272 공통 입력 effect adapter | 12~18파일 / +600~900. C1~C3/C5 필요한 interface만. 실제 fallback은 아직 off |
| P-C 채널 owner 실행·정리 | O channel policy/cutover/intake_route, owner REST port/host/부트, origin output stamp/PG effect guard, node-local reconciler·normal end/deferred/retry | 13~19파일 / +650~950. C4/C6/C7. 양방향 fixture를 여기서 닫음 |
| P-D 결합·전개 | persistent notice tx/포인터·실제 sender, availability→owner CAS→promotion 결합, pilot/derived/off, shared home 최종 read 소비, 수직/live 수용 | 9~15파일 / +350~550. P-A~C의 활성화 계약 충족 뒤 |

총 **고유 약 34~44 prod 파일 / +2,150~3,150**, tests 약 +1,000~1,700 raw, docs 별도. 같은 hot file이 여러 단위에 나타나므로 행 파일 수 합계와 고유 수는 다르다. r2 추정 증가를 숨기지 않는다. 실제 activated helper/등록 분량도 해당 PR cap에 포함하며 cfg(test) 이동으로 장부만 맞추지 않는다.

#7415는 조사 시 open이며 source SHA의 giant 검사는 실패형이다. 착지 뒤에는 줄 수로 막지 않고 경고/책임 리뷰를 따른다. 그 전에 줄 수 맞춤 child 이동을 하지 않는다.

| 기존 giant/임계 파일 | source raw | 예상 순 raw 증가 |
|---|---:|---:|
| `src/services/cluster/intake_router_hook.rs` | 2773 | +20~40 |
| `src/services/cluster/intake_router_hook/owner_record.rs` | 2398 | +10~25 |
| `src/db/intake_outbox.rs` | 1373 | +15~35 |
| `src/services/discord/session_runtime.rs` | 1182 | +10~25 |
| `src/services/discord/outbound/delivery.rs` | 1481 | +10~25 |
| `src/services/discord/gateway.rs` | 1562 | +10~25 |
| `src/services/message_outbox.rs` | 1355 | 기본 +0, metadata 연결 필요시 +10~20 |
| `src/server/mod.rs` | 3032 | 기본 +0, outbox stamp decode 필요시 +5~15 |
| `src/config.rs` | 3227 | +10~25, pilot/derived 및 kill switch |
| `src/services/discord/router/intake_gate.rs` | 1602 | +10~25, 공통 permit 접합; #7272 변경과 중복 계수하지 않음 |
| `src/services/discord/runtime_bootstrap.rs` | 1188 | +5~15, owner lifecycle 시작 |
| `src/services/discord/turn_finalizer.rs` | 4889 | 기존 완료 observer 재사용 +0, 연결 필요시 +8~20 |
| `src/services/turn_orchestrator/pending_queue_persistence.rs` | 1199 | 기존 local 저장 유지 +0, PG ACK 연결 필요시 +10~25 |
| intake_worker.rs / intake_dispatch.rs / writer/host.rs | 978 / 927 / 913 | 각각 +25~50 / +20~40 / +65~110; 새 1,000줄 경고 가능 |

`node_registry.rs`(1085)·`cli/args.rs`(1694)는 각각 기존 heartbeat 재사용·power/종료 producer 폐기로 r3 raw 증가0이다.

주요 병행 파일은 #7272의 intake_gate/busy_inject·intake_dispatch·worker_entry·turn_start/완료 재개, #7330의 replay_admission·source 처분·headless outbox다. P-A는 이 hot effect 경로를 바꾸지 않고 시작할 수 있다. P-B/C의 정확한 FILES와 공통 타입은 해당 레인 최신 SHA로 코디네이터가 고정한다. #7330의 TUI 밖 stale-resume/복구 활성화 자체를 기다리지 않는다.

| 겹치는 실제 경계 | 병행 레인 | 합칠 순서·#7336 책임 |
|---|---|---|
| `router/intake_gate.rs:1117`, `router/message_handler/intake_turn/worker_entry.rs:49` | #7272 adapter/bootstrap/owner | 공통 permit signature만 먼저 고정→P-B source/PG custody 접합. routine/HTTP도 같은 효과 API여야 함 |
| `router/intake_dispatch.rs:373`, `cluster/intake_worker.rs:434`, `db/intake_outbox/worker_status.rs:55` | #7272 owner, #7330 source 처분 | 공통 source/claim 계약→P-B stage/promotion. #7330 복구 활성화 전체와 별개 |
| `router/intake_dispatch/replay_admission.rs:253`, `turn_bridge/headless_delivery.rs:276` | #7330 보호 read·headless output | 보호 API+origin stamp 타입 고정→P-B/C 해당 caller 접합. headless retry producer는 그 레인 소유 |
| `turn_finalizer.rs:538`, `turn_bridge/finalize_epilogue.rs:69`, `session_runtime.rs` | #7272 완료/다음 입력 재개 | 기존 observer signature 고정→P-C wake/reconcile. 새 사용자 입력이 있어야 재개되는 조건 금지 |
| `runtime_bootstrap/o_writer_host.rs:88`, `tui_o/writer/host.rs:505` | #7272 bootstrap 주변과 인접 | 공통 lifecycle 접점만 협의→P-C가 production owner REST/PG output fence 구현 |

위 표의 축약 경로는 각각 `src/services/discord/`, `src/services/cluster/`, `src/db/`, `src/services/tui_o/`의 기존 전체 경로다. 착지 순서는 **P-A 즉시 → #7272 최소 source/effect interface와 P-B → P-C → P-D pilot → derived**다. #7415는 giant 증가가 있는 조각 앞에서 착지 여부를 확인한다. #7330 보호 API는 재사용하되 전체 이슈 완료 대기로 P-A를 막지 않는다.

## 13. [U] — 값/실측/착지 조정만 남김

아래는 정해야 할 핵심 동작을 미루는 목록이 아니다. shared home·불응 owner 예외·O REST·dedupe·재개 주체는 위에서 선택했다.

| ID | 미확정 대상 | 실측/확정 방법 |
|---|---|---|
| U01 | 두 Mac pair ID·shared PG 생존·양쪽 provider REST/auth/workspace·local DB timeout 적용 | 코디네이터가 금지 설정 파일 없이 승인 health/build-info/capability·시험 환경으로 확인. 서로 다른 host-qualified key·non-gateway REST와 A/B 전원 종료 뒤 PG 접근을 L01로 검증 |
| U02 | 운영의 옛 local home 후보 값/충돌 | 이미 로드한 map의 node-ID 요약만 수집해 §3.3 규칙 적용. 충돌/누락은 shared pending/conflict, 명시 `/node`로 확정. 이 레그에서 설정·사본을 읽지 않음 |
| U03 | migration 번호·공통 API의 최신 명칭/FILES·#7415 착지 | 번호/소유권은 coordinator 배정. 기존 helper와 새 tx/source adapter의 실제 분량을 구현 SHA에서 측정. 각 +1200 전에 분할 |
| U04 | #7272 source/effect 부분 조각의 최신 구현 SHA | C1/C2/C5만 상대 레인과 고정. 미착지면 P-A 계속 가능, P-B를 공통 계약 조각으로 같이 제공. 전체 이슈 완료를 기다리지 않음 |
| U05 | owner O source/binding·native 종료/normal end의 host별 실제 경로 | R01/R05/R07/R15/R16을 Claude/Codex·Legacy/O 및 현재 tmux/Herdr 지원 조합에서 실행. 한 방향을 off로 제외해 완료하지 않음 |
| U06 | 외부 ACK/history 권한·전송 중 PG 소실 잔여 | mock POST/transaction timeout과 승인 채널 nonce/history 실측. unknown을 sent/PASS로 쓰지 않으며 새 power/quiesced 체계는 추가하지 않음 |
| U07 | catch-up/thread/첨부의 실제 PG→다른 Mac 전달 | 원 runtime 종료 fixture R11/R12와 L02. source/custody/byte hash를 끝까지 연결하고 미실행은 NOT_RUN |

## 14. 리뷰 집중 항목·인계

- 경계 시각: DB home revision/owner generation/source와 고정된 control deadline을 사용한다. m3가 기존 timeout을 갱신해 무한 대기를 만들지 않는다. native Busy는 control timeout과 구분한다.
- 동시성: source begin의 durable commit과 raw 효과 guard를 분리해도 같은 expected g/attempt를 소비한다. owner CAS와 마지막 send는 같은 key로 직렬화하고 bounded tx로 사라진 client의 무한 lock을 막는다. old close callback은 exact native pin을 비교한다.
- 옛 형식 복원: PG home 우선·reset tombstone·늦은 import CAS, source/old output generation unknown은 추정하지 않음. old delegated `home_epoch`를 새 실행권으로 재활용하지 않는다.
- 문서로만 선언한 가드: production non-gateway REST, O start의 owner permit, staged→pending promotion, routed 새 source producer를 각각 실제 호출로 증명해야 한다. `ctx=None`/원 root 제거 fixture가 통과해야 한다.
- 잘못된 0/PASS: 같은 peer 알림0은 실제 다음 source 실행과 짝지음. enqueue와 visible 메시지/세션 종료와 deferred를 구분한다. preview·unknown·PG/live 미실행을 완료로 세지 않는다.
- 범위: 기존 local normal end보다 넓은 대상/시그널/삭제 없음. unavailable 분기는 논리적 generation 폐기와 지연 정리이며 B의 원 source·미결 답을 자동 rerun하지 않는다.

이 문서는 **독립 설계 리뷰 2회차 입력용 r3**다. 지적별 처리표는 report-design-r3.md에 있다. 코드는 바꾸지 않았고 cargo/PG/clippy/prepush를 돌리지 않았다. 구현 준비 조각과 기능 활성화의 증거를 분리해 인계한다.
