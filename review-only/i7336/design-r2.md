# #7336 설계 r2 — 입력마다 home/다른 Mac 선택

- 기준 SHA: `fb2219535d889de858a3d9e1429d32aa6e31d309`. r1 이후 이 worktree의 변경은 없다. 작성 중 origin/main은 `c5477168cfdb2591aeebb77aa091d3fe13bca545`(#7416/#7390)로 전진했다. intake_dispatch의 ConsentHeld 오류 타입이 String→Refusal로 바뀐 diff를 읽었다. 아래 좌표는 worktree의 고정 SHA이며 구현 시 최신 인터페이스로 다시 고정한다. merge/reset은 하지 않았다.
- 정본: [2026-10-26 12:2x 사용자 정의](https://github.com/itismyfield/AgentDesk/issues/7336#issuecomment-6407581059). 10:5x 댓글의 계약 ②~④와 r1의 power witness 전제는 폐기한다. [이슈 본문](https://github.com/itismyfield/AgentDesk/issues/7336)의 첨부 전달은 유지한다.
- **권고:** 새 입력을 받을 때 기존 heartbeat/readiness로 home의 수신 가능 여부를 판단한다. home에 보낼 수 없으면 현재 gateway Mac으로 보낸다. 같은 입력의 실행자는 durable admission으로 고정한다. 다음 새 입력에서는 home을 다시 본다.
- home이 복귀했는데 다른 Mac의 턴이 진행 중이면 그 턴과 응답을 마친 뒤 기존 세션 종료 경로를 호출하고, 기다리던 새 입력을 home에 보낸다. 입력이 없으면 장치 복귀만으로 세션을 닫거나 알리지 않는다.
- 채널별 on을 권고하며 전체 kill switch를 둔다. 전환 알림은 owner가 다른 Mac으로 바뀌어 commit될 때 한 번 생성한다.
- 구현 추정: **prod 고유 26~32파일 / 순 +1,350~1,950, 리뷰 2개 단위**. r1의 38~50파일/+2,700~3,900/4단위에서 코드량과 리뷰 단위를 약 절반으로 줄인다. #7272의 공통 입력 입구와 #7330의 source 보호를 재사용하는 전제이며 [U02/U03] 확정 뒤 다시 잰다. 전원 판정·새 노드 lifecycle·명시 반환 명령은 만들지 않는다.

## 1. r1에서 재사용/폐기/축소

| 항목 | r2 결정 | 이유와 남는 범위 |
|---|---|---|
| 입력이 없어도 꺼짐을 감시해 인수 | 폐기 | 새 입력과 이미 보관 중인 입력의 재개만 라우팅을 발동한다. 별도 failover 감시 loop 없음 |
| power witness·shutdown_ready·boot incarnation 증명 | 폐기 | 이번 입력을 home에 보낼 수 있는지만 본다. 물리 전원 상태를 주장하지 않음 |
| 정상 종료/maintenance/restart별 새 알림 | 폐기 | 새 라우팅 알림은 두 종류만. 같은 Mac으로 계속 보내는 동안 알림0 |
| 노드 전체 quiesced ACK·입출력 scope 집계 프로토콜 | 폐기 | 노드 종료 확인/host 전체 drain ACK를 만들지 않음. 현재 채널의 기존 native 완료·배달 상태와 정상 session-end 결과만 사용 |
| source 집합·시각 cutoff를 새로 저장하는 return barrier | 대부분 폐기 | 복귀를 본 새 입력은 기존 durable 입력 보관소에서 기다림. `pending_home`과 닫을 exact local session 참조만 필요. 새 source 원장/cutoff 목록 없음 |
| `/node-return`, 즉시 취소 인계 | 폐기 | 다음 입력이 자동 복귀를 요청한다. 별도 전환 명령·중간 턴 취소 옵션 없음 |
| 채널별 PG 정책 테이블·별도 설정 UI | 폐기 | 기존 intake routing config의 채널 목록과 on/off 필드를 이용. `/node`와 agent default가 home을 정함 |
| 새 인수 전용 세션 namespace | 폐기 | `adk_session.rs:118`의 기존 키가 이미 hostname을 포함. 다른 Mac의 세션 ID를 읽지 않는 local 선택만 보강 |
| 인수 세대마다 무조건 새 native 세션 | 축소 | 받은 Mac에 적격 local 세션이 있으면 사용. 없으면 fresh. home의 옛 세션을 다른 Mac에서 resume0 |
| home 반환 시 기존 세션 resume | 허용, 권고 | home 자신의 정상 local restore가 쉬운 경로다. 없거나 복원 불가하면 정상 fresh bootstrap. 다른 Mac 세션을 home으로 복사하지 않음 |
| 기존 OwnerIdentity advisory/CAS/stamp/unique active | 재사용 | 채널의 현재 실행 노드 권위. 같은 노드면 generation 유지, 노드가 바뀔 때만 증가 |
| 입력 source idempotency·replay/withheld 보호 | 재사용 | node 변경을 동일 입력 재실행의 허가로 만들지 않음. 노드별 중복 원장을 추가하지 않음 |
| byte attachment bundle | 재사용 | 새 메시지 첨부를 실행 Mac에 전달하는 기존 경로 유지 |
| O home 예외·restore/output gate | 축소해 유지 | 채널 routing owner를 소비하는 연결은 필요. 고정 O home guard 전체를 지우지는 않음 |
| 임시 세션을 retired만 하고 남겨 둠 | 변경 | 복귀 전 기존 정상 session-end를 호출해 다른 Mac 세션을 실제로 끝냄. 새 kill/delete 구현은 없음 |
| delayed reply/HTTP 불명 처리 | 필요한 최소 유지 | 턴 완료와 응답 완료는 다르다. 출력이 남았는데 home의 새 답을 시작하지 않음 |

`adk_session.rs`는 `src/services/discord/adk_session.rs`다. 이 표와 충돌하는 r1 내용은 r2에 승계하지 않는다. 특히 전원 증명이 없다는 이유로 인수를 막지 않는다.

## 2. 코드 사실과 이번 변경 경계

아래는 위 SHA의 사실이다. 기존 구현 전체가 이미 수용 조건을 충족한다는 뜻은 아니다.

| 사실 | file:line | 설계에 반영 |
|---|---|---|
| node 읽기의 online/offline은 heartbeat 신선도로 합성 | `src/services/cluster/node_registry.rs:724`, `:737` | 기존 cluster.lease_ttl_secs를 재사용. 새 전원 probe/새 timeout 수치 없음 |
| node GC도 heartbeat timeout | `src/services/cluster/node_registry.rs:313`, `:487` | 두 offline 신호를 독립 증거로 세지 않음. 이제 timeout은 라우팅 판단에 사용 가능 |
| gateway singleton과 cluster 리더는 별도 PG lock | `src/services/discord/runtime_bootstrap/gateway_lease.rs:252`, `src/services/cluster/node_registry.rs:12` | 입력을 받는 현 gateway가 결정. global cluster leader라는 이유만으로 실행 권한을 주지 않음 |
| gateway keepalive 불명 시 O gate Unknown | `src/services/discord/runtime_bootstrap/gateway_lease.rs:619`, `src/services/tui_o/ownership.rs:104` | gateway Owned와 PG 쓰기가 확보되기 전 새 routing/admission commit 없음 |
| foreign stale session owner는 지금 차단 | `src/services/cluster/intake_router_hook.rs:179`, `:207` | 선택 채널에서 새 입력 routing 결정을 legacy session owner 조회보다 앞에 둬야 함 |
| live owner가 /node와 agent default보다 우선 | `src/services/cluster/intake_router_hook.rs:312`, `:360` | fallback B의 세션이 있다는 이유로 home 복귀를 영구 무시하지 않도록 선택 채널만 우선순위 변경 |
| home 후보는 기존 /node 또는 agent 기본 노드 | `src/services/discord/commands/node.rs:67`, `src/services/cluster/agent_execution_node.rs:66` | home은 `/node` 명시값 → agent default. 현재 실행 owner에서 home을 추론하지 않음 |
| acquire/stamped admission 생성은 아직 dormant | `src/services/cluster/intake_router_hook/owner_record.rs:1`, `:232`, `:542` | live 연결 비용을 포함. 헬퍼가 존재한다는 이유로 동시 실행 방지가 이미 완료됐다고 하지 않음 |
| 같은 identity의 active owner 하나·세대 CAS | `migrations/postgres/0094_intake_session_owners.sql:50`, `src/services/cluster/intake_router_hook/owner_record.rs:407` | 별도 routing owner/epoch 테이블 없이 재사용 |
| admission idempotency는 target/generation과 독립 | `src/services/cluster/intake_router_hook/owner_record.rs:76`, `:627` | 같은 source/attempt는 routing이 달라져도 기존 row를 반환. 노드 변경으로 attempt 증가 금지 |
| tuple unique와 replay source fence | `migrations/postgres/0136_request_replay_disposition.sql:33`, `:66` | source/attempt와 merged source 보호를 함께 사용. 전체 prompt를 다른 Mac에서 retry하지 않음 |
| stamped worker는 owner key/세대를 확인 | `src/db/intake_outbox/worker_status.rs:55`, `:145`, `src/services/cluster/intake_worker.rs:434`, `:456` | pending→claim→accepted→spawned의 같은 stamp 사용. local 실행도 빠지지 않음 |
| live 즉시 주입은 일반 intake dispatch보다 앞 | `src/services/discord/router/intake_gate.rs:1117`, `:1541` | #7272 공통 입력 경로에서 routing permit을 먼저 소비. hook 한 곳만 바꾸고 끝내지 않음 |
| 새 attachment를 portable bundle로 준비 | `src/services/discord/router/intake_dispatch.rs:368`, `src/services/cluster/attachment_transfer.rs:18`, `:54` | held/remote/local 모두 bundle 수명 보존. 텍스트-only 강등 금지 |
| 기존 세션 키가 host별 | `src/services/discord/adk_session.rs:118`, `:133` | provider/token/hostname/tmux namespace 재사용. 실제 local locator·instance도 대조 |
| local restore는 기존 path/세션을 사용 | `src/services/discord/session_runtime.rs:285`, `:330`, `:473` | B에서는 B의 key만, A에서는 A의 key만. session ID를 peer에서 가져오는 분기 없음 |
| O는 고정 home 외 placement 거절 | `src/services/tui_o/channel_policy.rs:357`, `src/services/tui_o/cutover.rs:143` | routing owner permit에 한해 새 local source/host를 열어야 함. 실제 적용 조합 [U04] |
| O writer는 gateway에서만 입력·출력 가능 | `src/services/tui_o/topology.rs:29`, `src/services/tui_o/cutover/intake_route.rs:126` | O home으로 복귀할 때 transport gateway handback도 완료되어야 그 입력 실행 |
| native 완료는 strict terminator 필요 | `src/services/discord/turn_finalizer.rs:538` | tool 전 prose/화면 idle은 완료 아님. #7272가 제공하는 기존 exact session/episode 판정 소비 |
| legacy 다음 dequeue는 finalization 뒤 | `src/services/discord/turn_bridge/finalize_epilogue.rs:69` | home 대기 입력을 B의 다음 턴으로 꺼내기 전에 복귀 경로로 보냄 |
| writer stop/join은 현재 test-only | `src/services/tui_o/writer/host.rs:406`, `:443`, `:444` | 필요한 부분의 production caller 연결만 포함. host 전체 quiesced protocol을 추가하지 않음 |
| clear 상위 함수는 큐를 비우고 transition timeout 뒤 진행 가능 | `src/services/discord/commands/control.rs:279`, `:290`, `:320` | 복귀에 상위 `/clear` 호출 금지. 새 입력/첨부/체크포인트를 지우지 않음 |
| 기존 session-end 하위 경로가 존재 | `src/services/discord/commands/control/clear_teardown.rs:57`, `:97`, `:132` | idle인 exact B session을 닫는 adapter로 재사용. unconfirmed면 home 실행 보류 |
| 기존 managed end도 실제 process/tmux 종료·launch 파일 정리 수행 | `src/services/discord/commands/control/managed_reset.rs:71` | 기존 경로의 권한을 숨기지 않음. 새로운 kill/삭제 대상·알고리즘·fallback은 만들지 않음 |
| gateway preferred 복귀는 지금 즉시 양보/cancel | `src/services/discord/runtime_bootstrap/gateway_lease.rs:589`, `:505` | B 작업/응답이 남은 동안 voluntary yield만 연기. 실제 lease 상실에 우회 권한을 주지 않음 |
| outbox enqueue를 다른 state와 같은 tx에 넣을 API 존재 | `src/services/message_outbox.rs:448` | owner 변경+새 입력 admission+전환 알림 enqueue를 원자 commit |
| 일반 outbox dedup은 만료/failed 재등록 가능 | `src/services/message_outbox.rs:168`, `:339`, `:478` | 이것만으로 영구 1회 알림을 주장하지 않음. owner generation 행에 배달 포인터 보관 |
| 일반 nonce resolver는 headless 등 한정 | `src/services/discord/outbound/manual_delivery/headless_nonce.rs:31`, `:69` | 새 routing notice를 일반 outbox에 넣었다고 nonce가 자동 적용된다고 가정하지 않음 |
| stable nonce·오래된 POST history reconcile의 기존 구현 | `src/services/discord/task_notification_delivery/card_post.rs:10`, `:29`, `src/services/discord/task_notification_delivery/gateway.rs:184`, `:241` | routing 전용 작은 adapter에서 같은 transport/복구 정책 재사용. TTL 초과의 무조건 재POST 금지 |

## 3. 입력 시점의 home 없음 판정

### 3.1 세 가지 판단을 분리

`home`은 사용자 채널의 정상 실행 대상이고, `owner`는 현재 라우팅 실행 Mac이며, `gateway`는 입력 수신권이다. B가 fallback owner가 됐다고 home을 B로 덮어쓰지 않는다.

새 source에 대해 **현 gateway가 동일 PG를 읽고 쓸 수 있을 때** 다음을 평가한다. home 판단에는 기존 node_registry의 DB NOW 기준 heartbeat와 provider intake/readiness를 쓴다. 새 ping/HTTP 사전 탐지 loop는 만들지 않는다.

| 입력 시 관찰 | 결과 |
|---|---|
| home node online, 해당 provider worker가 수신 가능 | desired=home. 기존 local session이 busy인 것은 “home 없음”이 아니며 정상 즉시 주입/보관 정책을 따름 |
| home heartbeat가 기존 lease TTL보다 오래됨, offline, 또는 유효한 home의 registry row가 현재 없음 | 이번 입력에 `home_unavailable`로 판단. 현재 gateway가 다른 Mac이고 실행 준비/auth/workspace/첨부 capability가 맞으면 desired=gateway |
| home이 online이나 worker의 수신 readiness가 stale/실행 서비스가 없음 | `home_unavailable_for_input`. 준비된 다른 Mac으로 보낼 수 있음. 설정 오류/provider 불일치·home 식별 모순은 routing 장애로 held하며 임의 Mac을 고르지 않음 |
| home은 수신 가능하나 capacity가 차 있음 | home 유지. busy/capacity만으로 기기를 바꾸지 않음 |
| 현재 gateway가 home 자신임 | 자기 runtime/readiness를 함께 확인. stale DB heartbeat 하나 때문에 자기 입력을 불필요하게 peer로 왕복시키지 않음 |
| PG 조회/쓰기 실패, home 설정 미확정, gateway Unknown/Lost | 판단 불가. 기존 durable 보관/재개 경로로 held. standalone local 실행으로 우회하지 않음 |
| home 없음 + 현 gateway도 실행 준비 안 됨 | held. 제3 노드를 탐색하거나 임의 headless fallback을 추가하지 않음 |

heartbeat row 부재는 유효한 home ID를 이미 설정한 채널의 수신 불가다. home 오타/서로 다른 `/node` 설정을 수신 불가로 가려 버리지 않도록 enable 시 home 매핑을 검증한다([U01]). 원래 전원 상태·maintenance 여부를 새로 구분하지 않는다.

### 3.2 오판의 의미

home이 실제로 켜져 있어도 home→PG 연결이 끊기면 heartbeat가 stale이 되어 B로 보낼 수 있다. 이는 최신 정의에서 허용할 라우팅 전환이며, 알림도 “꺼졌다”가 아니라 **연결/수신 불가**라고 쓴다. 복구 뒤 다음 새 입력은 home을 다시 선택한다. TTL 부근의 진동은 실제 routing 변경 횟수만큼 알림을 만들 수 있지만 매 턴 알림은 없다. 별도 hysteresis·전원 증명은 추가하지 않는다.

잘못 분류해도 **같은 입력을 A와 B에서 각각 실행하지 않는 것**은 availability 추정이 아닌 source/admission 권위로 보장한다. 이미 A에서 accepted/started인 입력은 B로 복사하거나 자동 재실행하지 않는다. 연결만 끊긴 A가 예전 출력 경로로 계속 답하지 못하도록 §4.3의 owner generation 검사도 필요하다.

## 4. 입력마다 한 실행자를 고정

### 4.1 기존 상태에 붙이는 최소 metadata

권위는 `intake_session_owners` 하나다. 신규 migration에서 기존 세대 행에 `home_instance_id`, `local_session_ref`, 선택적 `pending_home`, `closing_session_ref`, `session_end_state`, `previous_instance_id`, `notice_outbox_id`, `notice_post_state`, `notice_message_id`, `notice_post_started_at`를 붙이는 안이다. 정규 owner 상태 active/superseded/released와 generation은 기존 그대로다. 별도 failover owner/별도 epoch/노드 생명주기 테이블을 만들지 않는다. 알림 nonce는 `(provider, raw_channel_id, generation, from, to)`에서 계산한다.

`pending_home`/session_end_state는 입력이 요구한 **채널 세션 종료**의 진행만 기록한다. 노드 전체 quiesced ACK가 아니다. 대기 입력과 첨부 본문은 #7272의 기존 보관소에만 있고 이 행에 복사하지 않는다. 실제 schema 필드 조합은 최신 공통 owner API와 겹치면 기존 필드를 사용한다([U02/U06]).

### 4.2 admission 순서

1. 새 Discord source ID/merged source 집합을 기존 canonical 입력 authority에 잡는다. duplicate/redelivery면 기존 target/attempt/처분을 따른다. **동일 source가 재도착했다는 이유로 새로운 routing 결정을 만들지 않는다.**
2. 새 입력은 home 수신 가능 여부를 읽어 desired를 정한다. 아직 session close를 기다리는 입력은 durable held이며 provider 실행 admission 전이다. held 재개 직전에 home을 한 번 더 읽어 최종 target을 정할 수 있다.
3. OwnerIdentity advisory lock 아래 최신 owner와 source 보호를 재확인한다. owner와 desired가 같으면 generation/notice를 바꾸지 않고 기존 local 세션을 사용한다. 없으면 받은 Mac에서 정상 fresh bootstrap한다.
4. owner와 desired가 다르면 아래의 switch 조건을 만족한 뒤 기존 `transfer_owner_in_tx`를 호출한다. CAS conflict면 전체 rollback·재조회다. 처음 입력에서 owner 이력이 없다면 home을 초기 비교 기준으로 삼아 target!=home인 경우에만 전환 알림을 만든다.
5. 같은 tx에서 stamp가 있는 local/forwarded admission을 insert하고, 실제 Mac 변경이면 알림 outbox를 enqueue·그 id를 owner 행에 저장한다. 기존 idempotency key에는 target/generation이 없으므로 그대로 사용한다. 라우팅 변경을 이유로 attempt_no를 올리거나 child retry를 생성하지 않는다.
6. local permit도 source/target/generation에 묶는다. worker는 기존 claimed/accepted/spawned stamp 검사를 사용한다. #7272의 live 즉시 주입·HTTP·큐·bootstrap 공통 입구도 이 permit을 소비해야 하며 old NULL stamp를 새 권한으로 삼지 않는다.
7. accepted/started/withheld인 source를 다른 Mac에 재등록하지 않는다. native 생성 결과/commit ACK가 불명인 같은 입력은 보관·조회하며 재실행하지 않는다. 일반 stale-resume retry로 다른 Mac의 세션을 열지 않는다. 실제 첫 submit/inject도 expected generation을 확인한 guard 아래 source begun 확정과 API handoff까지 연결한다. DB status 갱신만 하고 guard를 푼 뒤 늦게 paste하는 우회를 두지 않는다.

기존 open route가 남은 채 home이 unavailable이면 old row가 새 입력을 영구 막지 않도록 exact old row를 기존 source 처분 API에 넘긴다. 실행 전인지 확정되면 기존 보관 상태를 유지해 재선정할 수 있고, 시작 여부가 불명/이미 시작이면 **원 source는 withheld/unknown으로 보존하고 새 source만 라우팅**한다. `force_fail_and_retry_as_new`의 payload 복사 경로는 사용하지 않는다. 이 작업은 row/source 처분이며 원래 Mac의 프로세스를 kill하거나 파일을 지우지 않는다([U03]).

### 4.3 연결 오판·경합에도 응답이 겹치지 않는 경계

- 이미 begun인 입력의 target은 immutable이다. 다른 Mac이 같은 source를 받더라도 idempotent hit 또는 protected/held로 끝난다. CAS만으로 이미 시작한 provider 효과를 되돌릴 수 있다고 주장하지 않는다.
- routing이 바뀐 채널은 restore/rebind·새 submit과 모델 답 배달에서 **원 요청의 owner generation**을 소비한다. 현재 node 이름만 비교하면 B→A→B 때 오래된 B 출력이 통과하므로 금지한다.
- 기존 OwnerIdentity key를 실행 admission과 응답의 마지막 send 경계에서도 재사용한다. `with_current_owner_output(stamp, send)`가 current generation을 확인하고 **그 응답 요청/settlement가 끝날 때까지** switch와 직렬화한다. check 후 guard를 풀고 HTTP를 나중에 시작하는 구현은 허용하지 않는다. 이 작은 adapter는 기존 O/legacy transport·receipt를 감싸며 새 출력 원장/host 전체 ACK를 만들지 않는다.
- delayed headless/outbox 답이 있는 조합은 enqueue 때 동일 stamp를 보존하고 drain에서 확인해야 한다. 필요한 최소 metadata 전달까지 크기 추정에 포함한다. source/세대가 없는 old delayed 답을 현재 generation으로 추정하지 않는다.
- 따라서 A가 연결만 끊긴 채 살아 있어도 PG 권위를 확인할 수 없으면 새로운 입력/출력을 하지 않는다. B의 switch가 commit된 뒤에는 예전 A generation의 submit/POST가 거절된다. **PG lock은 전원 증명이 아니라 DB에 참여하는 효과 순서를 정하는 도구다.**
- 진행 중인 B에서 home으로 정상 복귀할 때는 현재 작업/출력 완료와 session-end를 먼저 기다리므로 정상 경로에서 A/B 응답 턴이 겹치지 않는다. 이미 wire에 나간 HTTP와 PG 연결 동시 소실처럼 외부 ACK가 불명인 경계는 기존 uncertain 배달 정책으로 held/재조회한다. timeout을 “답 완료”로 세지 않는다. DB 연결 소실 뒤 외부 서버에 남은 요청까지 PG lock 하나가 취소한다고 주장하지 않는다. 이 잔여 반례는 기존 transport의 재현 fixture로 확인할 [U05]이며 새 전원/전체 quiescence 체계를 부활시키는 이유로 삼지 않는다.

| 경합 | 허용 결과 / 0이어야 하는 효과 |
|---|---|
| 두 gateway task가 같은 source를 서로 다른 target으로 선택 | owner lock+source idempotency+unique active에서 하나만 commit. loser provider 실행0 |
| A submit 직후 같은 입력이 B에서 재도착 | A의 원 receipt/target 유지. B 자동 실행0; 새 attempt 생성0 |
| A의 pending claim과 node 변경 | 같은 owner key와 row CAS에서 claim 또는 변경 하나만 승리. accepted/started source 재배치0 |
| A heartbeat stale지만 A 프로세스 생존 | B는 새 source만 실행 가능. A old generation의 추가 submit/답0. A가 이미 끝낸 원 source를 B에서 재실행0 |
| B busy 중 A 복귀+새 입력 | 새 입력은 held, B 턴 종료/정상 세션 종료 전 A submit0 |
| B 완료와 새 입력·다음 dequeue | pending_home을 같은 channel transition/admission에서 먼저 처리. 새 입력이 B의 다음 턴으로 잘못 들어가는 효과0 |
| owner 변경 commit ACK 유실 | source/owner/notice_outbox_id read-back. 새 세대·같은 알림 재생성0 |
| old B generation의 완료/닫기 callback이 이후 B 새 세션에 도착 | closing_session_ref/expected generation 불일치 no-op. 새 세션 종료0 |

## 5. home 복귀와 다른 Mac 세션 종료

### 5.1 권고 경로

입력 `m1`을 B에서 처리 중이고 다음 입력 `m2`가 올 때 A가 수신 가능하면:

1. `m2`를 첨부와 함께 기존 durable owner에 보관하고 pending_home=A를 남긴다. B의 진행 native 작업에 `m2`를 즉시 주입하지 않는다. 별도 복귀 채팅 알림은 아직0이며 기존 보관 반응/상태만 사용한다.
2. B는 `m1`의 strict native 완료와 이미 제출한 후속 입력의 작업·terminal reply/미결 HTTP·outbox 처리를 마친다. mailbox 해제·worker 함수 반환·outbox done·짧은 prose만으로 완료시키지 않는다. 기존 완료 관찰이 Unknown이면 기다린다.
3. B의 local session transition lock을 **확실히 얻은 뒤** exact local session을 닫는다. `clear_teardown::end_session`과 기존 writer stop/join·selector 해제의 필요한 부분을 재사용하는 좁은 adapter를 둔다. 상위 `clear_channel_session_state_with_session_key`는 호출하지 않는다. 큐 전체 삭제·채널 clear boundary·catch-up checkpoint 건너뛰기·첨부 폐기는 없다.
4. 종료 대상은 owner generation이 가리키는 B의 session key/native locator/host pin 하나다. 현재 main의 managed 종료에는 process/tmux 종료·launch 파일 정리가 이미 포함돼 있으므로 그 기존 동작만 정확한 local 대상에 적용한다. Herdr/Codex의 기존 거절·확인 가드를 우회하지 않는다. 새 kill_pid_tree/kill-session/rm fallback·전역 세션 스캔은 만들지 않는다.
5. 종료가 불명/거절이면 m2를 보관하고 A에서는 실행하지 않는다. 종료 성공 또는 동일 session의 이미 종료를 확인한 경우에만 다음 단계로 간다. crash 뒤에는 closing_session_ref를 확인해 같은 세션 종료를 재확인하고, 새로 생긴 세션에 같은 종료를 반복하지 않는다.
6. 최종 admission 직전에 A를 다시 본다. 여전히 가능하면 B→A CAS와 m2 admission/알림을 commit하고 A에서 정상 local resume 또는 fresh를 사용한다. A가 다시 없으면 B에서 정상 세션이 없을 때 fresh로 시작할 수 있다. 이때 실제 routing owner가 B로 유지됐다면 전환 알림0이다.

A에 연결만 끊겼던 예전 native 작업이 남아 있다면 그 old-generation 답을 새 권한으로 재표시하지 않는다. 정상 local bootstrap이 idle한 자기 세션을 사용할 수 있을 때 resume하고, old episode가 아직 끝나지 않았으면 그 준비가 될 때까지 새 입력을 보관한다. peer의 작업을 resume하는 방식으로 빨리 통과시키지 않는다.

m2 뒤에 온 입력도 같은 보관소에 남는다. 이미 provider에 제출된 입력은 진행 중 작업에 포함하고, 아직 제출하지 않은 입력은 중복 source 등록 없이 최종 target으로 보낸다. 별도의 return cutoff 시각·source 목록·반환용 새 queue는 없다. native 완료 observer는 이미 대기 중인 입력만 재개하며, 입력이 없는데 home heartbeat만 보고 복귀 작업을 만들지 않는다.

### 5.2 gateway와 O의 기존 제약

현재 preferred gateway 양보는 watcher를 취소할 수 있다. B의 native 작업/응답이 남으면 voluntary yield를 연기한다. 이 연기는 gateway preference의 안전한 handback 조건이며 새 node lifecycle가 아니다. 실제 lease loss에서는 기존 gate를 닫고 source를 보호한다.

O 선정 채널은 현재 gateway에 writer가 있어야 한다. B fallback에서는 stamped routing owner의 **B local source**로 writer를 시작하는 예외가 필요하며 A의 store/transcript를 채택하지 않는다. A 복귀 때 B writer/세션 종료 후 gateway handback과 A writer 준비를 기다려 m2를 실행한다. 고정 O home과 채널 home이 모순되는 조합을 아무 조건 없이 켜지 않는다([U04]). O 전체 home guard를 지우는 설계가 아니다.

다른 안(복귀 즉시 cancel/강제 종료)은 새 입력을 조금 빨리 home에 보내지만 기존 B 작업의 답을 잃게 한다. 사용자가 요구한 채널 응답 겹침 방지와 기존 정상 종료만 사용한다는 조건에는 **현재 턴 완료 뒤 전환**이 가장 작고 명확하다. r2는 이 안을 권고한다.

## 6. 전환 알림 2종과 재시작 후 1회

| 실제 commit한 전환 | 문구 |
|---|---|
| home A → 다른 Mac B | `맥북에 연결할 수 없어 이 채널의 입력을 맥미니에서 처리합니다.` |
| 다른 Mac B → home A | `맥북 연결이 돌아와 이 채널의 입력을 다시 맥북에서 처리합니다.` |

B에 세션을 새로 만든 첫 인수만 `맥미니의 새 세션에서 처리합니다.`로 변형할 수 있다. 기존 B local 세션을 재사용한 경우 새 세션이라고 쓰지 않는다. 본문에 전원 꺼짐 확정·maintenance·epoch를 노출하지 않는다. 렌더 위치는 제안 `src/services/discord/router/routing_notice.rs`, enqueue 위치는 routing owner CAS tx다. 같은 B로 계속 보내면 새 이벤트·새 outbox·알림 모두0이다. native 세션 재생성만으로도 알림을 만들지 않는다.

**1회 보장은 세 층이다.**

1. **전환 생성:** 같은 Mac이면 owner generation 유지. Mac이 바뀐 한 CAS에만 `(provider, channel, generation, from, to)` 이벤트를 대응한다. owner 변경+새 입력 admission+기존 `enqueue_outbox_pg_on_tx_with_ttl` 호출+notice_outbox_id 기록을 한 tx에 넣는다. 초기 owner=home은 알리지 않고, 처음부터 B를 골랐으면 home→B로 한 번 알린다. DB rollback/commit ACK 유실은 같은 row read-back으로 해결한다.
2. **재시작/TTL:** 영속 owner 세대 행의 notice_outbox_id와 notice_message_id를 먼저 읽는다. 이미 같은 이벤트를 enqueue/배달했으면 재생성하지 않는다. 일반 dedupe TTL이나 process-local map을 영구 증거로 쓰지 않는다. failed outbox를 재시도해도 새 owner 세대/새 nonce를 만들지 않는다. owner 이력을 정리할 때도 배달 증거가 함께 사라져 같은 이벤트를 재생성하는 정책을 추가하지 않는다.
3. **외부 POST ACK:** routing notice 전용 분기에서 event별 stable nonce·enforce_nonce와 persisted post_started_at/message_id를 사용한다. generic headless nonce flag에 기대지 않는다. 기존 task-card POST의 짧은 nonce 재시도/오래된 POST history reconcile transport를 재사용한다. ACK 유실 후 history에서 같은 nonce+bot/channel/content hash를 찾으면 그 message_id를 기록하고 재POST0이다. nonce 보장 창 밖에서 history가 불명/잘렸으면 **blind repost0, uncertain 보존**이다. 이 상태는 알림 완료1로 세지 않는다([U05]).

권위적으로 같은 전환을 새로 만들어 두 번 알리는 것은 금지한다. 정상 배달/정상 재시작에서는 visible 알림1이며, 외부 POST 결과 자체가 증명 불가한 경우에는 at-most-one을 지키면서 상태를 불명으로 남긴다. “outbox enqueue1”을 “사용자가 알림을 봄1”로 보고하지 않는다. 배달은 기존 운영 알림 provenance를 붙여 새 입력으로 되먹임하지 않으며, 같은 이벤트를 다른 bot으로 무조건 다시 보내는 fallback도 사용하지 않는다.

## 7. 켜는 범위와 kill switch

**권고는 채널별 on, 기본 off**다. home 매핑·받는 Mac의 auth/workspace·O/host 모드가 채널마다 달라 사용자 채널 전체를 한 번에 켜는 것은 이 작은 변경의 수용 실측과 맞지 않는다. 채널별 on은 사용자에게 매번 인수를 누르게 하는 기능이 아니며, 한 번 on이면 이후 입력마다 자동 선택한다.

제안 config 키는 기존 `cluster.intake_routing` 아래 `input_fallback_enabled: false` (전체 kill switch), `input_fallback_channel_ids: []` (채널별 on)다. 신규 `channel_fresh_takeover_policies` 테이블이나 별도 UI/API는 없다. 기존 `/node`/agent default를 home으로 쓰고, 현 gateway가 설정 snapshot을 해석한다. channel ID는 actual raw destination이다. parent 한 개를 넣었다고 모든 thread/DM을 자동 확대하지 않는다. 필요하면 각각 같은 목록에 넣는다.

두 Mac이 source/owner stamp를 소비하는 바이너리여야 한다. 최초 enable은 기존 입력 custody/미결 배달을 공통 owner에 연결하고 local transition이 idle인 경계에서 적용한다. 미등록 과거 작업을 새 source로 재생하는 backfill은 하지 않는다. Enforce·공유 PG·지원 host/O 조합을 확인한 채널만 on으로 인정한다. 목록·enable을 읽지 못했으면 새 선택을 held하고, 기록된 routing owner가 있는 채널은 old fallback으로 우회하지 않는다. 설정 내용은 본 레그에서 열거나 수정하지 않았다.

kill switch off는 **새 fallback 선택을 중단**한다. 이미 begun인 입력은 원 target에서 끝내고, 대기 새 입력은 home을 대상으로 기다린다. B가 진행 중이면 정상 종료 뒤 home에 보낼 수 있을 때만 보낸다. active owner/source fence·세션 종료 상태·미배달 알림은 off여도 읽고 마무리한다. off를 이유로 A를 동시에 열거나 held source/첨부를 지우지 않는다. 이전 바이너리로 rollback하는 것과 kill switch는 동일하지 않다.

## 8. 수용 시험 — RED 조건

이 표는 다음 구현 레그의 명세이며 이번 설계에서 실행한 시험이 아니다. fixture는 두 runtime+throwaway PG+provider/HTTP recording sink이며, L01/L02는 코디네이터의 승인된 Mac/채널 실측이다.

| ID | 시나리오와 관측 | RED 조건 / 원인 제거 뮤턴트 |
|---|---|---|
| T01 | home A heartbeat 만료, B gateway, 새 source, B 세션 없음 → B fresh1·답1, home native ID resume0, 전환 알림1 | B 실행0/옛 A resume/알림0 또는2면 RED. unavailable branch 제거 시 RED |
| T02 | A 계속 없음, m2/m3 각각 도착 → B local 세션 유지, 입력마다 B 실행1, **추가 알림0** | 입력마다 새 routing generation/알림 생성하면 RED. same-target 비교 삭제 mutant RED |
| T03 | A 복귀+새 입력, B idle → B exact session-end1, 그 뒤 A normal resume/fresh1, 반환 알림1 | A를 먼저 실행/B 세션 미종료/계속 B/알림2면 RED. close-before-transfer 제거 mutant RED |
| T04 | B native/tool 턴 중 A 복귀와 새 입력 → B 기존 작업·답 완료, 새 입력 held, 정상 close 뒤 A 실행 | 중간 prose·idle만으로 close, m2 B 주입, A/B 응답 overlap, held 첨부/입력 소실이면 RED |
| T05 | 같은 source를 A/B ingress·live/catch-up·worker로 동시에 재도착, owner 변경 전후/commit ACK 유실 포함 | provider 시작/주입 합계>1, node 변경으로 새 attempt, started/withheld의 peer 재실행이면 RED. idempotency/owner stamp 각각 제거 mutant RED |
| T06 | A 프로세스는 생존하나 heartbeat/PG 경로 단절, B가 새 source 선택 | 새 source는 B만 실행, A old generation 추가 출력0. DB 오류를 NoOwner/local로 바꿔 old A 답이 나오면 RED |
| T07 | B cleanup 중 crash, 또는 old cleanup callback이 B 재생성 뒤 도착 | closing ref 재확인, 같은 종료 결과 재사용, 새 B 세션 종료0. 이름만 보고 end하는 mutant RED |
| T08 | A→B 알림 commit 전/후 crash, POST ACK 유실, send 완료 뒤 프로세스 재시작, dedupe TTL 경과 | event/nonce 고정·visible message1, continuation B 알림0. memory map만 사용/TTL 뒤 재생성/새 nonce로 retry면 RED. uncertain을 sent로 세도 RED |
| T09 | B→A→B 실제 세 번 전환, 중간 native 세션만 재생성 | 실제 변경별 알림1, 총 전환수와 일치. 과거 B 이벤트를 전역 dedup해 새 전환 고지0이 되거나 세션만 바뀌어 고지하면 RED |
| T10 | 첨부-only/본문+첨부, remote/held-return, bundle 손상 | 실행 Mac 파일 byte/hash/개수 동일. 손상 시 submit0+보관. text-only 실행/정상 close가 대기 첨부 삭제하면 RED |
| T11 | mode/PG/readiness/home 설정 불명, 다른 Mac도 준비 안 됨 | held, source 유지, arbitrary local 실행0. lookup error를 absent/home=false와 같게 취급하면 RED |
| T12 | fallback global off/채널 목록 제외 중 B begun, 뒤 새 입력 | begun 입력 유지, 새 fallback0, 정상 close/기존 owner fence 유지. off에서 A 병행 실행/held 삭제면 RED |
| T13 | stale pending/accepted/spawned old route가 새 입력을 막는 상태 | exact source 처분 후 새 source만 진행, old payload copy0. `force_fail_and_retry_as_new` 호출 또는 open row 무시로 duplicate면 RED |
| T14 | O foreign-home fallback와 복귀, B native 작업 중 preferred waiter | permit 있는 새 B local source만 허용, A store resume0, B 작업 중 voluntary cancel0. 단순 global home guard 삭제/조기 yield면 RED |
| T15 | B local 세션 있음/없음, A local 세션 ID만 PG에 있음, host-qualified legacy key | B local만 재사용 또는 새 생성. A ID 읽기/resume/host 간 key 혼용이면 RED. 새 namespace 없이 기존 host key를 고정하는 회귀 |
| T16 | HTTP·queue·routine/TUI·restore/rebind·legacy/O/outbox 각 입구의 old generation | old submit/POST0, source는 보존. 중앙 hook만 소비하고 즉시 주입이나 output 하나가 우회하면 RED |
| T17 | B 종료 실패/unconfirmed, lock 경합, native/응답 ACK 불명 | 새 A 실행0, held 입력 유지. 상위 /clear의 timeout-overtake/queue discard로 대체하면 RED |
| T18 | 입력 없이 A가 없어지거나 복귀하고 heartbeat/gateway waiter만 변함 | routing owner/세션 close/전환 알림0. heartbeat observer가 인수/반환 이벤트를 만들면 RED |
| L01 | 실제 A 정상 종료→새 입력 B fresh→계속 B→A 복귀 새 입력, busy 복귀도 포함 | 실제 native ID/답 출처/기기 변경 알림 수/정상 B 종료를 연결. PG 미실행·source 미도달을 0/PASS로 세면 RED |
| L02 | A 없음과 gateway 교체 공백에서 보낸 실제 첨부 | source ID→bundle→받은 Mac 파일 hash→provider 읽기를 확인. mock bundle만으로 실측 완료 표시 금지 |

양성 계수(입구 도달·선택된 provider 실제 submit·정상 답)를 음성 계수(다른 Mac submit0·추가 알림0)와 짝지어 기록한다. 수정 전 RED→수정 후 GREEN→원인 제거 mutant RED와 바이트 동일 복원을 남긴다. 전원 증명 없는 timeout fixture 자체를 RED로 정하던 r1 시험은 폐기한다.

## 9. 구현 단위·증가량·병행 파일

### 9.1 두 리뷰 단위

cap 정본은 `/home/adk/skills/agentdesk-issue-pipeline/scripts/pr_cap_prod.py:14`의 prod 30파일/순 +1500이다. +1200 예상 전에 PLAN_READY로 재분할한다. 아래는 사전 추정이고 실행 cap 결과가 아니다. tests/docs는 별도다.

| 단위 | 내용 / 주요 소유 파일 후보 | prod 추정 |
|---|---|---|
| P1 입력별 routing과 source/owner 결합 | 신규 `cluster/input_routing.rs`·migration, cluster/mod.rs, config/runtime_profile.rs·intake_routing_config.rs, intake_router_hook.rs/model.rs, intake_dispatch.rs, intake_worker.rs·worker_entry.rs, #7272 공통 입력 adapter, session_runtime.rs/turn_start.rs의 local selector, 모델 응답 stamp 소비의 공통 adapter | 14~19파일 / +700~1,000 |
| P2 정상 복귀·O 연결·알림·최종 on | 신규 `router/routing_notice.rs`와 등록, control.rs/clear_teardown.rs, finalize_epilogue.rs, gateway_lease.rs, O cutover/channel_policy/o_writer_host/writer host, legacy/O/outbox 배달 hook, task-card nonce/history transport의 좁은 재사용 | 15~21파일 / +650~950 |

고유 prod **26~32파일 / +1,350~1,950** (중복 파일 재등장), 시험 약 +700~1,200 raw, docs 별도. owner_record.rs/worker_status.rs/adk_session.rs/BundleV1은 기본적으로 기존 구현을 사용하고 재구현하지 않는다. 후속 인터페이스상 직접 필드 확장이 필요하면 그 파일은 실제 크기에 추가한다.

P1은 공통 permit이 연결돼도 production fallback을 off로 유지한다. P2의 close·출력·알림 연결까지 완료한 뒤 on을 허용한다. 사용되지 않는 helper를 새 allow/baseline 인상으로 숨기지 않으며 cfg(test) 또는 실제 운영 caller와 함께 둔다. 합산을 하나의 PR로 만들면 +1500을 넘길 수 있으므로 **두 PR을 권고**한다. 각 PR은 실제 FILES·공식 cap으로 다시 측정한다.

축소 이유는 새 node lifecycle/power producer/PG 정책 UI/명시 전환/새 세션 namespace/큰 return barrier를 모두 없앴기 때문이다. 남는 비용은 입력 source의 한 실행자 연결, 기존 세션 종료의 안전한 호출, O 고정 home, 전환 알림의 재시작 배달이다. 이들을 빼고 +400~900으로 선언하면 수용 조건의 우회 입구나 중복 알림이 남는다.

### 9.2 giant 증가 예상

#7415가 머지되면 줄 수만으로 차단하지 않는다. 조사 시 GitHub 이슈는 open이고 이 worktree의 script는 아직 실패형이므로 머지 여부는 구현 착수 SHA에서 확인한다. 기존 책임의 수정 줄 수를 맞추려고 child로 이동하지 않는다.

| 파일 | 현재 raw | 예상 순 raw 증가 |
|---|---:|---:|
| `src/services/cluster/intake_router_hook.rs` | 2773 | +20~40 |
| `src/services/cluster/intake_router_hook/owner_record.rs` | 2398 | 기본 +0, snapshot 확장이 필요하면 +5~15 |
| `src/services/discord/session_runtime.rs` | 1182 | +10~20 |
| `src/services/discord/outbound/delivery.rs` | 1481 | +8~20 (공통 output guard 소비점) |
| `src/services/discord/gateway.rs` | 1562 | +8~20 (stamp carry/기존 transport 연결 필요시) |
| `src/services/message_outbox.rs` | 1355 | 기본 +0, 모델 답 stamp 전달 API 필요시 +10~25 |
| `src/server/mod.rs` | 3032 | 기본 +0, drain metadata decode 연결 필요시 +5~15 |

intake_worker.rs(978), intake_dispatch.rs(923), writer/host.rs(913), adk_session.rs(957)는 변경에 따라 1,000줄을 넘을 수 있다. #7415 뒤 경고·책임 리뷰로 다루며 별도 숫자 상한이나 기계적 이동을 만들지 않는다. node_registry.rs의 lifecycle hook·cli/args.rs·shutdown.rs·restart_report.rs는 r2 변경 후보에서 뺐다.

### 9.3 #7272·#7330과 겹침/머지 순서

| 현재 파일:line | 겹치는 책임 | 머지 순서/소유권 |
|---|---|---|
| `src/services/discord/router/intake_gate.rs:1117`, `src/services/discord/router/intake_gate/busy_inject.rs:198` | #7272 즉시 주입·bootstrap/owner 통합 | #7272의 공통 입구 확정 뒤 P1이 node 선택 permit을 연결. 옛 branch별 라우터를 새로 만들지 않음 |
| `src/services/discord/router/intake_dispatch.rs:307`, `:588`, `src/services/discord/router/message_handler/intake_turn/worker_entry.rs:49` | #7272 단일 owner와 #7330 source preflight | 공통 입력 shape → #7330 보호 API/활성 조합 확인 → P1. source permit과 node permit을 같은 입구에서 소비 |
| `src/services/cluster/intake_worker.rs:434`, `src/db/intake_outbox/worker_status.rs:88` | forwarded claim·source 보호·owner stamp | #7330의 withheld across-node 판정을 유지. unsupported forwarded 조합을 #7336이 몰래 활성화하지 않음 |
| `src/services/discord/router/turn_start.rs:218`, `src/services/discord/session_runtime.rs:285` | #7272 local bootstrap·#7330 stale-resume recovery | 기존 local session 선택을 유지하고 peer native ID를 가져오는 fallback0. 최신 caller에서 작은 delta |
| `src/services/discord/turn_bridge/finalize_epilogue.rs:69` | #7272 보관 입력 재개·next dequeue, #7330 처분 settle | P2에서 current completion 뒤 pending_home을 먼저 처리. 새 완료 authority를 만들지 않음 |
| `src/services/discord/turn_bridge/headless_delivery.rs:276`, `src/server/outbox_actionable_delivery.rs:62` | delayed 응답/nonce/처분 경계 | P2가 원 request의 routing stamp만 전달/소비. #7330 retry/withheld 권위와 섞지 않음 |
| `src/services/tui_o/cutover.rs:143`, `src/services/discord/runtime_bootstrap/o_writer_host.rs:160`, `src/services/tui_o/writer/host.rs:443` | #7272 O bootstrap와 native 완료/출력 관찰 | 최신 O 접합점에서 P2가 owner-gated local source와 normal close 연결 |
| `src/services/discord/runtime_bootstrap/gateway_lease.rs:589` | preference yield가 입력/출력 생존에 영향 | P2가 busy/return close 중 voluntary yield만 연기. 별도 restart replay를 추가하지 않음 |

권고 순서는 **#7415와 #7272의 공통 입력 계약 확정/착지 → #7330 최종 보호 API·조합 확정 → P1 → P2 → 기능 실측/on**이다. #7415는 기능 의존성은 아니며 숫자 맞춤 작업을 없애는 선행 착지다. #7272·#7330 전체 이슈를 새로 해결하는 조건은 아니고 실제 공유 파일/API가 안정돼야 한다. 코디네이터가 최신 SHA·FILES 소유표를 고정한다. 그 전에 다른 레인 파일을 선점하지 않는다.

## 10. [U]와 실측 방법

| ID | 미확정 | 확인 방법 / 해소 전 처리 |
|---|---|---|
| U01 | 운영 home 매핑, 공유 PG 생존 topology, 두 Mac gateway 자격/실행 준비·workspace | 금지 설정 파일을 열지 않고 코디네이터가 승인된 health/build-info/node capability·비밀 없는 실행 인자로 확인. A를 내려도 B가 같은 시험 PG·bundle에 접근하는지 L01에서 확인. 불명 채널은 off. **전원 witness는 질문 목록에서 삭제** |
| U02 | #7272 최종 공통 입력 permit·held 입력 custody와 active native 완료 callback | 최신 착지 SHA/API 확보, live/HTTP/queue/routine/worker caller census. native 종료 전/후 및 B busy+A ready를 recording provider로 T04/T16 검증. source 목록을 복제한 return 원장은 만들지 않음 |
| U03 | #7330 최신 source 처분 API와 local/forwarded 활성 허용 조합 | coordinator 제공 최신 SHA와 보호 source cross-node 시험을 확인. main에서는 replay supplier가 false(`replay_admission.rs:60`). P1이 신규 target을 정했다고 unsupported retry producer를 켜지 않음. old open row 처분/같은 source 재전달 T05/T13을 통과해야 함 |
| U04 | 정상 session-end adapter의 host별 확인 결과와 O 고정 home 접합 | Claude/Codex·tmux/Herdr의 현재 지원 조합에서 T03/T07/T14/T17. 기존 거절/확인 절차·native/pane ref를 유지하며, 종료 실패를 성공으로 가장하지 않음. O gateway/home 충돌 채널은 호환 연결 전 off |
| U05 | nonce ACK 유실/history 권한·page 경계, 기존 POST와 PG 연결 동시 소실의 효과 | T06/T08의 fake transport로 먼저 재현, 승인 시험 채널에서 동일 nonce 재시작/ACK 유실 후 message ID 복구 확인. nonce 창 밖 history 불명은 uncertain이며 재POST/PASS0. 이미 wire에 나간 요청을 PG lock이 취소한다고 가정하지 않음 |
| U06 | migration 번호·최신 #7415 상태·실제 파일 수/증가량 | 구현 SHA에서 공식 cap/giant와 raw 줄 수 측정, 번호/FILES는 coordinator 배정. 26~32파일은 계획 범위이며 한 PR 상한이 아님 |
| U07 | gateway 공백/catch-up에서 attachment-only·thread 첨부 전달 | L02에서 입력 ID→bundle byte/hash→받은 Mac local file→provider 읽기 연결. 기존 bundle 함수 존재만으로 완료 표시하지 않음. 별도 누락 수리가 필요하면 최소 delta 재산정 |

## 11. 리뷰 집중 항목과 인계

- **경계 시각:** home 판단은 입력 admission 직전의 기존 DB heartbeat/readiness. TTL 딱 경계의 비교·held 재개 직전 재검사를 fixture로 고정한다. 전원 상태·벽시계 cutoff로 확대하지 않는다.
- **동시성 경합:** source/attempt idempotency와 owner CAS/stamp를 결합한다. routing tx는 owner advisory→source/outbox row 순서이며, main의 row-first worker는 try-owner 실패 rollback을 유지한다. 네트워크 guard를 얻기 전에 pool lookup을 마치고 guard 안에서 다른 연결을 기다리는 순환을 만들지 않는다. node 변경이 already-started 재실행을 허용하지 않음. busy B에서 새 A 입력이 dequeue되기 전 normal end 확인이 필요하다. output check와 실제 send 사이 guard를 잃으면 T06/T16 RED다.
- **옛 형식 복원:** host-qualified local/legacy key 재사용, peer native ID resume0. NULL stamp/historical home_epoch·old output의 generation 추정은 held. r1 새 namespace·node incarnation 이관을 부활시키지 않는다.
- **문서로만 선언한 가드:** dormant stamped admission, test-only writer join, headless 전용 nonce 제한을 사실 표에 표시했다. P1/P2가 실제 소비점을 연결하는지 리뷰하며 helper 존재만으로 PASS를 내지 않는다.
- **잘못된 0/PASS:** 추가 전환 없음은 실제 m2/m3 provider 실행과 함께 측정. 알림 outbox enqueue와 visible message를 따로 센다. unknown POST/history·PG/live 미실행은 완료로 세지 않는다.
- **기존 위험을 넓히지 않음:** clear 상위의 queue discard/timeout-overtake(`control.rs:279`)를 재사용하지 않는다. 기존 managed end(`managed_reset.rs:71`)보다 넓은 대상·새 signal/삭제 권한 없음. gateway의 DB 불명 유지 경로(`gateway_lease.rs:660`)를 전원 판정이나 stale source replay에 이용하지 않는다.
- **크기:** P1/P2 둘 다 +1200 전에 재계수. 공통 입력/출력 경계가 예상과 달라 branch별 보강이 필요하면 +1,950 계획을 사실처럼 유지하지 않고 범위/FILES를 다시 보고한다. giant raw 수치는 줄 맞춤 이동 사유가 아니다.

r2는 설계 산출물이며 구현 완료/REVIEW_READY가 아니다. 이 worktree는 읽기 전용으로 유지하고 산출물과 로그만 lane-runs에 썼다. 구현 레그는 독립 설계 리뷰 후 실제 RED/GREEN/뮤턴트·required test 선택·source_scan_readers failed0·clippy no-growth·static PREPUSH_STATIC_OK를 수행한다. 이번에는 cargo/PG/clippy/prepush를 실행하지 않았다.
