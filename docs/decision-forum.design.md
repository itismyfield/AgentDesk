# Decision 포럼 설계 (#8211)

<!-- doc-path-refs: proposed src/cli/args/decision.rs src/cli/decision.rs src/cli/decision/owner_cache.rs src/db/decisions/followups.rs src/db/decisions/messages.rs src/db/decisions/selection.rs src/server/routes/agents_turn_deliver/decision.rs src/server/routes/decisions.rs src/services/discord/decisions/action_handle.rs src/services/discord/decisions/classify.rs src/services/discord/decisions/component.rs src/services/discord/decisions/followup.rs src/services/discord/decisions/ingress.rs src/services/discord/decisions/lifecycle.rs src/services/discord/decisions/opinion.rs src/services/discord/decisions/requester.rs src/services/discord/decisions/scan.rs src/services/discord/decisions/thread_target.rs src/services/discord/decisions/transport.rs src/services/discord/decisions/worker.rs -->

설계 기준 코드는 `1161162a18`이다. 이 문서는 #8211 설계 개정 3(독립 리뷰 P0 0·P1 0·P2 3)의 요지이며, 리뷰 P2 3건을 다음 조각의 수용 조건으로 넣었다. 상태 모델은 [decision-request 모델](statecharts/decision-request/decision-request.report.md)에 있다. 줄 번호는 설계 기준 코드의 좌표이며 이후 편집으로 밀릴 수 있다.

사용자 원칙: 거부된 입력은 옛 경로로 보내지도 보관하지도 않는다. 정상화가 먼저이고, 실패하면 이유를 분명히 보이고 버린다. 선택지 밖 의견 입력과 요청자의 reply/close도 이 기능 범위다.

## 설계

### 1. 계약·수명

- 사용자가 고른 선택은 PG 사실 기록으로 남는다. 주입은 승인된 event당 workflow 하나다. 정상 busy는 기존 original 하나로 턴 경계를 기다린다. 세션 부재·거부는 한 번 정상화하고, 실패하면 사유를 표시한 뒤 입력 실행권을 버린다. 실패 입력 retry queue·주기 재주입·재전송 버튼은 없다.
- 의견은 source message ID마다 event 하나다. 버튼·숫자·이름 선택은 request당 choice 하나로 합친다.
- 논리 상태는 Open → Selecting → Closed, 또는 Open/Selecting → 명시 Closed다. Open만 새 의견·선택을 받는다. 유효 선택 commit은 즉시 Selecting으로 신규 접수를 봉인하고 기존 choice 전달은 계속한다. 선택 전달 workflow가 terminal이면(실패·unknown 포함) 결과를 표시하고 archive+lock한다. 같은 선택을 다시 실행하지 않는다.
- 명시 close는 새 입력과 writer를 먼저 막고 미시도 event를 종결한 뒤 결론을 출력하고 archive+lock한다.
- 의견만 전달되면 Open을 유지하고 그 source reply에 "의견 전달됨"을 표시한다. 다른 message ID면 같은 문장도 별도 입력이다. 같은 ID의 재수신·수정·재스캔은 새 입력이 아니다. 실패 의견의 runnable payload는 지우고 ID/hash/분류/결과 사실만 남긴다. Discord 원문은 지우지 않는다.
- 고정하는 것은 요청 agent와 논리 channel/thread다. 요청 시점 session은 감사용이다. 주입·drain 때 현재 세션을 쓰고 없으면 정상화한다.

### 2. 설정·category→forum·멱등 생성 (조각 1)

- 설정 타입은 `src/config/decisions.rs`의 `discord.decisions`다. 조각 1은 `forums_by_category`(category ID→forum ID 덮어쓰기)만 둔다. ask가 쓰는 `forum_name`(기본 `decision`), tag 필수 포럼용 `required_tag_ids_by_category`, 스캔 주기 `reply_scan_interval_secs`(기본 15초)는 그 값을 읽는 조각에서 더한다. CLI는 `--name`으로 이름을 받는다. 운영자와 멘션 원천은 기존 `discord.owner_id` 하나다. 실제 운영 ID는 설계·코드·시험에 넣지 않는다.
- 요청 target의 실제 channel에서 category를 구한다. thread는 thread→parent→category다. category가 없거나 부모 조회가 실패하면 `target_category_unavailable`로 돌려주며 임의 공용 forum으로 보내지 않는다. 세션 부재는 요청 거부 사유가 아니다. 세션 교체만으로 다른 category에 다시 게시하지 않는다.
- 해석 순서: 설정 덮어쓰기를 먼저 검증한다(guild·category·Forum 종류). 그다음 PG `decision_forums` cache, 그다음 REST 목록이다. cache는 PK(guild_id, category_id), UNIQUE(forum_id)이고 행에 포럼 이름을 기록한다. 같은 category의 다른 이름 요청은 `forum_name_mismatch`다.
- cache 적중은 그 forum을 다시 조회해 같은 guild·category·Forum 종류일 때만 쓴다. 지워졌거나(404) 다른 category로 옮겨졌으면 cache를 버리고 다시 해석한다. 조회 권한 오류는 cache로 가리지 않고 오류로 돌려준다.
- 같은 category·같은 이름·Forum 종류의 기존 채널을 재사용한다(사용자가 미리 만든 포럼 편입). 후보가 여럿이면 `forum_ambiguous`이며 삭제·합치기는 하지 않는다. 같은 이름의 Text 채널은 Forum으로 치지 않는다.
- Discord 채널 목록은 VIEW_CHANNEL이 없는 채널을 빼고 돌려준다. 그래서 announce 봇의 관리자 권한 증거 없이 빈 목록을 "없음"으로 해석하지 않는다(`forum_inventory_unavailable`).
- API·CLI는 같은 PG advisory lock(namespace + guild/category, `PooledAdvisoryLock`)과 같은 PG 행을 쓴다. lock 안에서 cache·REST를 다시 읽는다. 네트워크 왕복 동안 SQL transaction을 열지 않는다.
- 생성은 `create_may_have_sent`를 먼저 commit하고 CreateChannel(kind=Forum)을 한 번 호출한다. 성공하면 ready/forum_id를 저장한다. 확정 400/401/403은 사유를 저장·반환한다. 응답 유실·5xx·재시작은 다음 호출에서 같은 category를 다시 조회해 편입할 뿐 새 POST를 하지 않는다. 한 번의 빈 조회만으로 다시 만들지 않는다(`forum_create_unknown`).
- CLI: `agentdesk discord channel-create --kind text|forum --name <name> --category-id <id> [--guild-id ...] [--topic ...]`. 기본은 text이고 forum에는 category가 필수다. 응답은 id/name/kind/category_id/created이며 반복 호출은 같은 ID·`created=false`다. Text 경로의 list-then-create 동시성은 이 범위에서 고치지 않는다. thread-create의 파일 잠금은 한 호스트 범위라 category 생성 잠금으로 쓰지 않는다.

### 3. 봇 선택·권한

- announce 봇은 포럼 생성과 등록된 thread의 archive/unlock/lock만 맡는다. 요청 채널을 소유한 gateway 연결 provider 봇이 글·버튼·편집·interaction을 맡는다. announce Http가 있다는 사실은 gateway interaction 처리의 증거가 아니다.
- provider는 원래 논리 channel binding의 explicit/live ownership으로 고른다(health/runtime_resolve.rs의 소유 판정). forum ID로 provider를 고르지 않는다. gateway-ready 증거가 없으면 `decision_bot_unavailable`이다. 작성한 app/bot/guild/thread/starter ID를 PG에 고정해 클릭·편집을 검증한다.
- 설정의 bots/allowed_channel_ids는 AgentDesk 라우팅이며 Discord 권한 bit를 주지 않는다. provider에는 VIEW_CHANNEL, SEND_MESSAGES, SEND_MESSAGES_IN_THREADS, READ_MESSAGE_HISTORY가 필요하고, announce 생성에는 MANAGE_CHANNELS와 완전 열람 증거가 필요하다. 권한을 자동으로 올리지 않는다.
- 게시 403은 `decision_post_forbidden`을 요청 API/CLI/GET에 돌려준다. announce로 대신 게시하지 않는다. moderation 권한이 없으면 logical Closed는 유지하고 `moderation_error`를 남긴다.
- 자유 답글에는 MESSAGE_CONTENT와 READ_MESSAGE_HISTORY가 필요하다. 읽지 못한 본문을 빈 의견으로 처리하지 않고 `capture_error`를 표시한다.

### 4. PG 스키마

| 테이블 | 필드·제약 |
|---|---|
| decision_forums | PK(guild, category), forum_id UNIQUE, 포럼 이름, ready/create_may_have_sent/failed, 오류 |
| decision_requests | requester agent/logical channel, 게시 category/provider/app/bot snapshot, ref/idempotency hash, immutable title/context/options, forum/thread/starter, 감사용 session snapshot, lifecycle, close_reason/conclusion, requester capability hash, interface_version, desired/published revision, projection/moderation 오류 |
| decision_options | request/option PK, ordinal/label/key, 추천 bps, opaque handle UNIQUE, consumed_at |
| decision_choices | request_id PK, actor/time, winning source, canonical selection_event_id. 최초 INSERT 승자만 선택 |
| decision_messages | UNIQUE(thread_id, message_id), actor/bot/webhook, content hash, raw_content nullable, kind, 결과·사유, input_event_id |
| decision_input_events | event_id PK, selection/opinion, source key UNIQUE, 승인 actor/text hash, admission_sequence, origin UNIQUE |
| decision_delivery_attempts | event_id PK, workflow ticket, not_started/resolving/normal_wait/normalizing/may_have_sent/closing/delivered/failed/unknown_closed, normalization_used, 시도 session 증거. retry_at 없음 |
| decision_scan_cursors | thread PK, covered_through, scan_anchor_high/next_before, version, scan error |
| decision_followups | request+idempotency key UNIQUE, reply/close/opinion_notice, post_state, nonce/message ID |

- 시각 TIMESTAMPTZ, request/event ID UUID, Discord ID 문자열, hash SHA256. 테이블은 그 값을 쓰는 조각에서 만든다.
- source key: 선택은 request UUID, 의견은 (request UUID, message ID). origin은 `decision:<UUID>`와 `decision:<UUID>:opinion:<message-id>`다.
- 메시지 분류·event 생성·choice CAS·수명 확인은 같은 request row lock transaction에서 선형화한다. 버튼과 숫자 답글이 겹치면 첫 유효 commit이 이긴다.
- 첫 입력 시작은 event별 not_started CAS 한 번이다. 부팅은 시작 못 한 event만 같은 CAS로 한 번 시작한다. may_have_sent는 증거 확인 뒤 닫고 다시 쓰지 않는다.

### 5. API·CLI·requester capability

- `POST /api/decisions`, `GET /api/decisions/{id}`는 기존 HumanInputCaller 인증을 쓴다. body는 agent_id, target.channel_id, provider_hint, ref/idempotency_key, title/context, options 2~5개, recommendation이다. category·forum·operator·선택 결과는 body가 정하지 못한다.
- ask CLI는 요청 전에 256-bit requester capability를 만들어 현재 agent/logical channel 사설 cache(디렉터리 0700, 파일 0600)에 저장하고 `X-AgentDesk-Decision-Owner` 헤더로 보낸다. 서버는 hash와 scope만 저장한다. 같은 idempotency key는 같은 cap hash를 요구한다. GET·표시·오류·기본 출력·로그에는 cap을 내보내지 않는다.
- `POST /api/decisions/{id}/reply`({text, idempotency_key}), `POST /api/decisions/{id}/close`({conclusion, idempotency_key})는 HumanInputCaller와 그 요청의 cap이 모두 필요하다. 다른 agent·다른 요청의 cap이나 cap 없음은 403이고 게시·수명 변경은 0이다.
- CLI: `agentdesk decision ask --json-file <path|->`, `get <id>`, `reply <ref> "<답>" [--request-id]`, `close <ref> "<결론>" [--request-id]`. ref는 현재 requester scope에서만 해석하고 모호하면 `ambiguous_ref`다.
- ask 응답: 201=해당 interface 활성까지 확인, 202=저장·게시·비활성 준비 중, 4xx=검증·권한, 502/503=확정 외부 실패·미준비. 같은 key/hash/cap은 같은 request, 다른 payload/cap은 409다. 초기 게시 확정 실패는 자동 재POST하지 않는다.
- reply는 Open/Selecting/Closed 모두 허용하고 논리 상태를 바꾸지 않는다. close는 Open/Selecting을 Closed로 만든다. 같은 close key/body는 같은 결과, 다른 결론의 같은 key는 409, 이미 결론이 있으면 `already_concluded`다. 공개 choose/retry/force/reopen API는 없다.
- 글은 제목·배경·결정 이유·선택별 결과/비용/위험·Gemini 추천 %·agent/ref·owner 멘션·상태로 그 글만 읽어도 이해되게 쓴다. 2,000 UTF-16 unit 한도를 넘으면 422로 돌린다. Gemini %는 helper가 주고 서버는 범위·합계만 검사한다. 미확보면 "추천 확인 불가"다.

### 6. 게시·버튼·물리 lifecycle

- 최초 Forum starter는 provider Http 어댑터로 만든다. CLI subprocess를 부르지 않는다. intent/handle을 PG에 먼저 저장하고 비활성 버튼 글을 만든 뒤 같은 starter의 버튼을 활성화한다. timeout 뒤에는 request ID marker·author·app으로 같은 글만 회수하고 새 글을 POST하지 않는다.
- 버튼 handle은 ActionHandle 코덱에 `decision:v1` namespace를 더한다. 기존 `msteer:v1` 직렬화는 바이트 호환이다. 실제 일회성은 choice PK/row CAS다.
- component는 빠른 deferred ACK 뒤 owner·글·handle을 검증하고 선택부터 commit한다. live target 검사를 INSERT 앞에 두지 않는다. 재클릭은 최초 선택만 알린다.
- 물리 목표: Open=열림, Selecting=버튼 비활성, Closed=archived+locked. reply 정비 중에는 잠시 editable일 수 있으나 작업 뒤 최신 lifecycle 목표로 복원한다. 출력 순서는 바인딩 확인 → announce unlock/unarchive → provider PATCH/POST → 최신 PG lifecycle 재조회 → archive+lock 복원이다.
- 출력 POST는 followup key/nonce를 먼저 기록한다. 응답 유실은 같은 message를 marker/nonce로 확인할 뿐 주기 재POST하지 않는다. logical/physical 상태를 분리하고 즉시 화면 일치를 주장하지 않는다.

### 7. 단일 전달 workflow·현재 세션·정상화

- 입력 형식: `[사용자 결정 ref=<ref> decision=<id>] <질문> → 선택: <label> (Gemini 추천 <n>% / 추천 확인 불가)`, 의견은 `[사용자 의견 ref=<ref>] <제목> → 선택지 밖 의견: <원문>`. author=실제 owner, author_is_bot=false, trust=user_instruction, source=decision.
- `InputOrigin::Decision`과 검증된 provenance를 더한다. imessage로 위장하지 않고 전용 domain prefix로 stable message/ticket을 만든다. API body의 source/metadata는 권한 증거가 아니다.
- not_started→resolving CAS로 한 번 시작한다. 주입·drain 시점에 현재 binding/provider/session을 해석한다. S1 종료·S2 교체는 정상적인 현재 S2 선택이다.
- 정상 busy는 기존 original 하나로 턴 경계를 기다린다. 세션 부재나 기동 확정 거부는 normalization_used=false일 때 같은 workflow에서 한 번 정상화한다. 정상화가 그 event를 첫 prompt로 이미 전달했으면 두 번째 deliver는 없다.
- 확정 NotSentProof만 정상화 continuation의 근거다. admitting/unknown/timeout/MayHaveSent는 근거가 아니다. 실제 쓰기는 최대 한 번이다. 정상 busy와 실패는 typed 결과로 구별하며 decision adapter가 그 분기를 소유한다.

### 8. 실패 입력의 실행권 종결

- 같은 origin/workflow의 durable closing fence를 먼저 기록한다. 실제 쓰기 전에 may_have_sent를 기록한다. lock 유실 뒤 may_have_sent를 새 권한으로 재발급하지 않는다.
- 정확한 source/ticket으로 그 event의 runnable slot·미시도 head·KnownNotSent·Unknown retained 원본만 제거한다. channel 전체 clear는 하지 않는다. decision은 merge_consecutive=false다.
- 이미 물리 작업을 시작한 head는 기존 permit/physical pin으로 정산한다. 실패 표시를 위해 writer를 kill하지 않는다. 결과 불명은 unknown_closed로 새 writer만 금지한다.
- NotSentProof(effect.rs:105)는 decision이면 hold/retain으로 보내지 않는다. retained requeue와 boot recovery(recovery.rs:23)는 decision closed 원본을 제외·폐기한다. 일반·non-owner 경로의 restore_dequeued_head/race-loss도 failed 원본을 되돌리지 않는다.
- 이후 준비 회복·새 입력 wake·worker 재생성·재시작·callback 지연은 failed/unknown_closed를 다시 실행시키지 못한다. PG와 queue snapshot의 원자 commit을 가정하지 않는다. closing fence 뒤 snapshot 제거 전 crash는 부팅 소비 경로가 실행 없이 제거한다.
- 이 종결 계약은 reserve 이전 경계(아래 §13의 P2-1)에도 똑같이 적용한다.

### 9. non-owner drain·handoff·재시작

- non-owner 시작 경계(queue_dispatch/kickoff.rs, intake_dispatch/queued.rs, queued_head_inject.rs)는 decision 원본을 일반 입력으로 오인하기 쉽다. 안정 queue identity로 PG 승인 event를 복원하고 text/ref 추측으로 권한을 만들지 않는다.
- handoff는 source=decision을 HTTP(agents.rs)와 service 진입(agent_handoff.rs) 모두에서 backend 호출 전에 거절한다.
- 재시작: not_started는 부팅에서 한 번 시작, resolving/normalizing 중 종료는 `interrupted_before_delivery`로 닫음, 정상 busy original은 같은 ticket으로 이어감, may_have_sent는 증거가 있으면 delivered 아니면 unknown_closed(재전송 0), Cancel/clear는 종결, Stop은 기존 실행기 결과를 따른다.

### 10. 답글 분류·의견 전달

- 등록된 decision thread의 MessageCreate는 일반 thread 승격·command·60초 메모리 dedup 앞에서 decision ingress가 소비한다. 다른 bot runtime도 일반 intake로 흘리지 않는다.
- 작성자가 configured owner이고 bot·webhook이 아닐 때만 분류한다. 숫자는 ASCII 1..선택지 수와 선택적 "번"의 전체 일치, 이름은 공개 label/key 전체 일치다. 그 밖(`2번은 별로고…`, `2.`, `02`, 전각 숫자, 여러 선택지 혼합)은 의견이다. 공백뿐·본문 없는 첨부·봇·타인은 ignored다.
- 의견 payload suffix는 받은 message.content 전체 그대로다. 분류용 trim과 분리한다. 길이 한도를 넘으면 자르거나 나누지 않고 사유를 표시·폐기한다. 원문 보존 경계는 §13의 P2-2를 따른다.
- 같은 ID 수정은 감사용 ignored_edit이다. 실패·unknown_closed 의견은 실행권·원본·raw payload를 지우고 "세션에 전달하지 못함: <이유>"를 표시한다.

### 11. 주기 스캔·cursor

- gateway가 즉시 경로, 주기(기본 15초)·부팅 scan이 누락 보완이다. 모든 경로가 같은 observe_once/classify/choice/event 서비스를 부른다. scan은 실패 입력의 재시도가 아니다.
- thread별 covered_through=C에서 snapshot high H를 고정하고 before 페이지로 (C, H]를 채운다. 모든 결과가 durable해진 뒤 같은 transaction/version CAS로 C=H를 commit한다. gateway high ID로 cursor를 올리지 않는다. 403/429/5xx는 cursor를 올리지 않고 scan_error를 남긴다.
- Closed thread는 close 뒤 최종 catch-up 한 번만 하고 ignored_closed만 기록한다.

### 12. 요청자 reply/close·출력 순서

- reply: cap/idempotency/길이/scope 검증 → reply 사실 저장 → 필요하면 announce로 editable → provider가 owner 멘션 답글 POST → 결과 정산, 최신 lifecycle의 archive/lock을 finally 복원. 논리 gate는 그대로다.
- close: 검증 → 결론·Closed gate·실행 fence를 먼저 commit하고 미시도 event 종결 → provider가 결론 답글 게시·starter PATCH → announce가 archived+locked로 만들고 read-back. 게시가 실패해도 Closed를 되돌리지 않고 `conclusion_post_failed`를 표시한다.
- close admission fence는 느린 reply HTTP 뒤로 미루지 않는다. reply의 finally가 Closed를 Open으로 바꾸지 못한다.

### 13. 리뷰 P2 계약 (다음 조각 수용 조건)

#### P2-1 reserve 이전 실패와 지연 복구 (조각 2 수용 조건)

§8의 종결 계약은 reserve에 닿기 전 원본을 보관하거나 후속 실행을 예약하는 경계에도 적용한다. 아래 경계는 진입점 전수표 E62~E66으로 추가했다. 서비스가 확인한 decision provenance를 공통 종결 child로 연결하며 경계마다 따로 복구 정책을 만들지 않는다.

1. 실제 정상 활동이 확인된 대기만 같은 원본·ticket을 유지한다.
2. 확정 미전송이고 아직 정상화를 쓰지 않은 준비·기동 실패는 같은 workflow의 정상화 한 번으로 이어간다.
3. 나머지 실패·unknown·닫힘은 PG fence 뒤 그 원본만 제거하고, 일반 retry_start나 retained 재전송 안내로 넘기지 않는다.

agent_handoff의 source=decision 거절과 owner 경로 안의 claude_handoff 준비 처리는 다른 경계다. 조각 2 종결 시험 목록에 다음을 넣는다.

- 정상 busy로 기다리던 decision을 drain할 때 reserve 이전 기동 실패를 일으킨다. row/token 정리, wake, 재시작 뒤 그 event의 추가 effect 0, runnable 원본 0, retained 안내 0.
- HeldRestore.queue를 확보한 뒤 event가 terminal이 되는 순서를 만든다. 뒤의 merge에서도 실행 0, 원본 제거.
- 같은 시험에서 다른 사용자 입력의 원본과 기존 복구 정책은 그대로다.
- 앞단 종결 호출 또는 복구 때 terminal 검사를 지운 뮤턴트가 RED다.

#### P2-2 원문 보존 경계는 provider 입력 직전까지 (조각 2·5 수용 조건)

원문 보존은 human_frame에서 끝나지 않는다. provider 입력 계획 생성기도 CRLF·단독 CR을 LF로 바꾼다(진입점 E67~E70). 승인된 decision opinion의 보존 정책은 human_frame에서 provider의 plan_prompt_submit/owned_prompt_plan과 실제 transport까지 이어져야 한다. 화면 판독용 정규화와 실제 전송 문자열의 정규화는 구별한다. 일반 입력·iMessage의 기존 정규화와 검증은 그대로다. source=decision 문자열이나 metadata 자기 선언만으로 이 경로에 들어가지 못한다.

- 조각 2: decision 입력 경로가 provider planner까지 provenance를 넘기는 배선을 만들고, 승인 provenance가 없는 source=decision 요청은 보존 경로 진입 0을 시험한다.
- 조각 5: 공백·CR/LF/CRLF·한글·emoji·기존 필터 문구가 든 원문 fixture를 Claude/Codex의 idle, 정상 busy 뒤 drain, cold-start에 넣는다. frame builder 반환값이 아니라 transport가 실제 받은 payload suffix와 receipt를 원문과 비교한다. 보존 경로를 일반 planner로 되돌린 뮤턴트가 RED다.

#### P2-3 자동 활성화 조건과 #8125 제공 계약 (조각 3·6이 이 절을 인용)

새 수동 switch는 없다. 저장된 interface_version과 컴파일된 handler 배선이 활성화를 정한다. 아래 조건이 모두 참일 때만 그 단계의 interface를 노출한다. 이 검사는 기능 배선의 준비 상태에만 쓴다. 이미 노출된 유효 선택은 현재 세션 생존과 관계없이 선저장한다.

| 단계 | 노출 | 조건 |
|---|---|---|
| 조각 3 | 비활성 버튼 글, "아직 선택을 받지 않음" 안내 | 게시·회수 배선, 일반 intake 차단 guard 연결 |
| 조각 4 | 활성 버튼 | choice CAS, 실행권 종결(P2-1 포함), 부팅 복구, selection lifecycle, 그리고 아래 #8125 계약을 소비하는 정상화 helper가 그 target 종류에 연결됨 |
| 조각 5 | 자유 의견 안내 | classifier·gateway/scan/cursor·의견별 전달, P2-2 보존 경로 |
| 조각 6 | reply/close, thread target | requester cap 검증, 같은 글 출력·flags 복원, #8125 thread 계약 |

#8125가 제공해야 하는 최소 계약(조각 6 착수 전 #8125 본문·코드로 확인):

1. thread target 수용: 정확한 thread ID를 대상으로 받는다. parent 본 채널로 fallback하지 않는다.
2. 부모 소유 확인: thread의 현재 parent channel이 그 agent 소유인지 확인하고 아니면 확정 거부를 돌린다.
3. 세션 해석·기동: 그 agent/channel(또는 정확한 thread)의 현재 세션을 고르고, 없으면 기존 시작 경로로 기동하는 helper를 노출한다. 결과는 `Delivered`(첫 prompt로 전달 완료), `Started`(기동만 됨, 전달 필요), `NotSentRefused(이유)`(확정 미전송), `Unknown`으로 구별된다.
4. Herdr 본 채널: 현재 main의 turn_host.rs:196 `ExecutorNotWired` 거절을 우회하지 않는다. #8125가 Herdr 정상화 helper를 제공하지 않으면 그 target 종류에는 활성 버튼·의견 안내를 노출하지 않고 비활성 글과 사유를 보인다.

증명할 시험: 조각 3→4→5 전환에서 ask 응답·버튼·안내·실제 입력 effect를 함께 확인한다. 의존 helper가 미배선인 조합에서 지원 interface처럼 노출하지 않는다. 정상화가 첫 prompt까지 전달했으면 뒤의 deliver는 0회다. 활성화 조건을 지운 뮤턴트가 RED다.

## 분할

| 조각 | 범위 | 예상 prod 파일/순증 | 수용 조건 추가 |
|---|---|---:|---|
| 1 | forum CLI·category resolver/PG cache·멱등 생성 | 13/+500 | §2 |
| 2 | event별 단일 전달 DDL/권한·현재 session·정상화·실패 폐기·owner/non-owner/boot·handoff 거절 | 28/+865 | §13 P2-1, P2-2(배선) |
| 3 | POST/GET/ask·게시/회수·비활성 버튼·requester cap 준비/저장·일반 intake 차단 | 23/+879 | §13 P2-3 |
| 4 | choice CAS·component 활성·전달/부팅·선택 뒤 archive/lock·공유 lifecycle | 12/+570 | §13 P2-3 표 조건 |
| 5 | 숫자/이름/의견 classifier·gateway/scan/cursor·의견별 전달/표시 | 21/+796 | §13 P2-2 |
| 6 | scoped reply/close API·CLI·같은 글 출력/flags 복원·#8125 thread 현재 session/receipt | 19/+842 | §13 P2-3 #8125 계약 |

- 예상치는 설계 추정이다. 각 조각은 같은 정본 스크립트로 base/head를 다시 잰다.
- 아직 caller가 없는 DAO·renderer·enum variant를 미리 넣지 않는다. 테이블·DAO는 실제 caller가 들어오는 조각에서 만든다.
- 1,000줄 넘는 기존 파일에는 module/re-export/호출 줄만 넣고 본체는 새 child에 둔다.
- 조각 3은 단독으로 기능 완료를 주장하지 않는다. 조각 4 뒤에 활성 버튼, 조각 5 뒤에 자유 의견 안내를 노출한다(§13 P2-3).

수정 전 RED→GREEN 계획:

- 조각 1: forum parse/payload, 기존 재사용, 동시 생성(잠금 제거 대조 → forum 2개), unknown late result(재POST 대조 → forum 2개), PG cache 적중·무효.
- 조각 2: event stable origin, 정상 busy, 정상화 실패 뒤 준비 회복/wake/boot effect 0, closure/retained 필터 제거 RED, S1→S2는 choice 보존·현재 S2 1회, handoff source=decision backend 0, §13 P2-1 목록.
- 조각 3: /decisions·CLI 부재 RED, cap이 먼저 cache되고 hash만 PG에 남음, 다른 cap으로 idempotent ask 탈취 불가, 비활성 버튼 단계, 일반 intake fallthrough 0.
- 조각 4: 버튼/같은·다른 choice CAS, 선택 선저장, 부팅 unknown, 선택 terminal 뒤 archive/lock, 배선 전 활성화·owner/CAS/close fence 누락 RED.
- 조각 5: 분류 표 전수, 원문 exact suffix(§13 P2-2), 같은 ID gateway+REST+restart 1회, 다른 ID 같은 문장 2회, 타인/봇 0, edit 0, failed 원본 0, cursor를 max seen으로 되돌린 대조 RED.
- 조각 6: 본인 cap+같은 ref와 남의 cap/없음/다른 요청/모호 ref, 상태별 reply 불변, close 중 normal_wait 취소, reply/close 경합, 부모 조회 실패·parent fallback 0.

## 진입점 전수표

E행은 설계 기준 코드 좌표, N행은 새 지점이다(`NEW`). 기존 message intake의 60초 메모리 dedup만으로 영속 중복 방지를 주장하지 않으며 decision 글은 일반 agent intake에 넘기지 않는다.

| ID | 기존 file:line / 신규 예정 지점 | 모델 행동·값과 배선 |
|---|---|---|
| E01 | 기존 CLI src/cli/args.rs:408, src/cli/run.rs:367, src/cli/direct.rs:800 | PostBegin 전 포럼 생성. 현 Text 전용·동시성 잠금 없음 |
| E02 | 기존 src/cli/discord_thread_create.rs:308 | PostFinish; forum starter payload 재사용 후보. CLI는 announce HTTP이므로 결정 게시에 직접 호출 금지 |
| E03 | 기존 API src/server/routes/agents.rs:659 | 단일 workflow 내부 idle start. 원래 session ID는 조건 아님, 현재 route와 실행권 확인 |
| E04 | 기존 API src/server/routes/agents_turn_deliver.rs:126 | Submit; 사람 입력의 idle/busy 처리 공통 경로 |
| E05 | 기존 src/server/routes/agents_turn_target.rs:15 | 논리 agent/channel 및 thread 부모 권한. 선택 저장 전에 live session을 검사하지 않음 |
| E06 | 기존 src/server/routes/auth.rs:260 | 생성·조회 API 호출자 인증. HumanInputCaller를 새 API에도 적용 |
| E07 | 기존 src/server/routes/agents_turn_deliver/legacy_origin.rs:11 | 동일 origin INSERT 승자만 실행. preflight 거부/unknown을 새 admission으로 재실행하지 않음 |
| E08 | 기존 src/services/imsg_reply/admission.rs:125, src/db/imsg_reply.rs:146 | admission 및 종료 tombstone. not_started만 CAS, failed/unknown_closed는 replay 결과만 |
| E09 | 기존 src/server/routes/agents_input_result.rs:35, src/services/agents/input_result.rs:69 | 전송 중 재시작 때 1회 기존 증거 조회. 주기적 전달 재시도/결과 polling 없음 |
| E10 | 기존 src/services/discord/health/turn_deliver.rs:199, src/services/discord/health/turn_deliver/prepared.rs:11 | 현재 세션 ResolvePresent/NormalizeAbsent/정상 busy. 일반 fallback enqueue와 decision 실패를 분기 |
| E11 | 기존 src/services/discord/health/turn_deliver.rs:459 | 정상 busy 원본 하나: user_instruction/author_is_bot=false, stable origin 보존 |
| E12 | 기존 src/services/discord/queue_io/input_owner/origin.rs:25, :49 | 입력 출처·안정 ID. 지금 사람=Discord/Imessage, identity도 imessage 전용 |
| E13 | 기존 src/services/turn_orchestrator/queued_head/v2.rs:61 | person_input; decision은 현 상태에서 사람 입력으로 분류되지 않음 |
| E14 | 기존 src/services/discord/queue_io/input_owner/start.rs:13, src/services/discord/router/turn_start.rs:81 | Decision 안정 ticket. 요청 세션 identity로 막지 않음, imessage bytes 유지 |
| E15 | 기존 src/services/discord/queue_io/input_owner/admission.rs:85 | Submit/queued; admit_with_context, 같은 ticket 재조회/재접수 |
| E16 | 기존 src/services/discord/queue_io/input_owner/pump.rs:43 | 실제 reserve/physical writer 경계. 정상 대기 후 현재 session 다시 해석 |
| E17 | 기존 src/services/turn_orchestrator/queued_head/v2.rs:229, :245 | original/ticket 한 snapshot. decision 실패만 exact original 제거 |
| E18 | 기존 src/services/turn_orchestrator/queued_head/v2/attempt.rs:92 | 기존 MayHaveSent commit 경계. 요청 당시 nonce 검사 추가 없음, 시도 중 재시작은 재전송 금지 |
| E19 | 기존 src/services/discord/health/turn_deliver/inject/first_input.rs:80 | 실제 production effect. PG single-write/closed 검사와 현 physical pin, 과거 request pin 아님 |
| E20 | 기존 src/services/turn_orchestrator/queued_head/v2/retained.rs:58 | 기존 retain_not_sent(:166)/requeueable(:283)는 decision 종료의 반례 근거. decision 전용 discard 분기 필요 |
| E21 | 기존 src/services/discord/queue_io/input_owner/recovery.rs:8 | Boot의 requeue_known_not_sent(:23) 전에 decision terminal 제외/제거. 원래 입력 재큐잉 금지 |
| E22 | 기존 src/services/discord/runtime_bootstrap/queued_recovery.rs:26 | 일반 큐 복원도 origin→PG 상태 확인. not_started/정상 busy 원본/unknown을 구별 |
| E23 | 기존 src/services/discord/runtime_bootstrap/spawns.rs:211 | worker 시작: 부팅의 미시도 선택 1회, input retry_due 없음. projection repair만 주기 가능 |
| E24 | 기존 src/services/turn_orchestrator/incarnation.rs:18, :32 | mailbox incarnation. 기존 actor handle을 보존하고 callback을 채널로 재해석하지 않음 |
| E25 | 기존 src/services/turn_orchestrator/registry_purge.rs:416 | actor 교체가 새 전달권을 만들지 않음. failed tombstone은 PG에 남음 |
| E26 | 기존 src/services/discord/router/intake_gate.rs:128 | Click 진입: InteractionCreate → message component. 일반 채팅 intake와 분리 |
| E27 | 기존 src/services/discord/router/intake_gate/component_events.rs:12 | 신규 decision namespace를 기존 component 라우팅에 추가할 지점 |
| E28 | 기존 src/services/discord/router/herdr_consent.rs:128 | ACK/글 바인딩 검사 형태 재사용. 원래 target live 검사는 선택 INSERT 조건에서 제외 |
| E29 | 기존 src/services/manual_steer/action_handle.rs:24 | one-time 128-bit handle 생성/코덱. PG 소비 여부는 별도로 필요 |
| E30 | 기존 src/services/discord/router/herdr_consent/transport.rs:49, src/services/discord/http.rs:306 | Render·버튼 비활성화. send helper는 일반 메시지용, 최초 forum post는 별도 어댑터 필요 |
| E31 | 기존 src/services/discord/runtime_bootstrap/gateway_runtime.rs:112, src/services/discord/runtime_bootstrap/orphan_recovery.rs:63 | gateway 이벤트 수신. utility bot은 agent runtime launch 제외 가능 |
| E32 | 기존 src/services/discord/health/runtime_resolve.rs:233 | 요청 채널을 소유하는 provider bot 선택. 포럼 ID로 runtime을 고르면 안 됨 |
| E33 | 기존 src/services/discord/discord_io.rs:64, src/services/discord/settings/read.rs:311 | actor=current owner 검사와 설정 투영. allow_all_users/일반 allowed_user는 결정 권한 아님 |
| E34 | 기존 src/services/discord/commands/control.rs:717, src/services/turn_orchestrator/queued_head/v2/control.rs:191 | CancelClear: 정확한 미시도 decision 원본만 종결. 선택 기록·다른 입력 불변 |
| E35 | 기존 src/services/discord/commands/control.rs:510, src/services/turn_orchestrator/queued_head/v2/clear.rs:17 | 명시 clear로 제거된 원본은 종결. 단순 세션 교체는 현재 세션 해석, request pin 거부 아님 |
| E36 | 기존 src/services/discord/commands/control.rs:619, src/services/turn_orchestrator/herdr_user_stop.rs:14 | Stop은 기존 실행기 intent. 실패 입력 재큐잉/선택 삭제/새 kill 권한 없음 |
| E37 | 기존 API src/server/routes/queue_api.rs:209, src/server/routes/agents_stop.rs:4 | HTTP Cancel/stop 인접 경로. 기존 결과·receipt를 읽어 결정 상태 갱신 |
| E38 | 기존 src/services/tui_input/target_epoch.rs:51, src/services/turn_host.rs:182 | 현재 물리 실행기 fence만 유지. 요청 시점 session fence 제거. Herdr start는 #8125 |
| E39 | 기존 src/services/discord/router/message_handler/intake_turn/adk_thread.rs:82 | thread의 현재 부모/category·정확한 thread 세션 시작은 #8125 계약 소비 |
| E40 | 기존 src/config.rs:195, src/db/postgres.rs:25 | 새 설정 타입 연결·migration 부팅. 운영 설정 파일을 읽거나 바꾸지 않음 |
| E41 | 기존 src/services/discord/queue_dispatch/kickoff.rs:113, :272, :280 | DrainCurrent(non-owner); 실패 뒤 무조건 restore_dequeued_head하지 않도록 decision 결과 분기 |
| E42 | 기존 src/services/discord/router/intake_dispatch/queued.rs:85, :291 | 원본 origin으로 PG 선택/논리 route 복원. queued_submission의 metadata 소실을 source=queued로 우회하지 않음 |
| E43 | 기존 src/services/discord/router/intake_dispatch/queued.rs:337, :46; src/services/discord/router/intake_dispatch/queued/race_loss.rs:17 | finish/restore/race loss 경로. 정상 busy만 원본 복귀, 확정 실패는 Finalize |
| E44 | 기존 src/services/discord/queue_io/queued_head_inject.rs:121 | 현재 source=queued/metadata=None. decision origin 감지 후 검증 context 복원과 closed 검사 |
| E45 | 기존 src/services/discord/queue_dispatch/kickoff.rs:376, src/services/discord/runtime_bootstrap/queued_recovery.rs:70 | 재시작 non-owner drain 진입. failed 원본 제외, 정상 wait는 같은 ticket으로 현재 세션 해석 |
| E46 | 기존 src/server/routes/agents.rs:1077, :1120 | handoff HTTP source 전달. decision source 명시 거절 |
| E47 | 기존 src/services/discord/agent_handoff.rs:529, :564, src/services/discord/health/headless_turn.rs:100 | handoff 내부 source ingress→공통 headless. HTTP 우회 호출도 앞에서 decision 거절; HandoffDecision 대조 |
| E48 | 기존 src/services/discord/router/thread_binding.rs:80, :89, :95; src/services/discord/http.rs:306 | 기존 helper는 locked 거부/unarchive만, edit helper는 message PATCH만. 새 transport가 Unlock→Unarchive→Render 수행 |
| N01 | 신규 src/server/routes/decisions.rs:NEW ask/get | PostBegin/GET/입력별 상태. ask의 requester capability hash를 저장, GET에는 raw cap 없음 |
| N02 | 신규 src/cli/decision.rs:NEW ask/get | ask/get 및 후속 reply/close 래퍼. 같은 요청자 scope의 cap 자동 사용 |
| N03 | 신규 src/services/discord/decision_forums.rs:NEW resolve_or_create | category forum 해석·동시 생성 방지. 원래 provider 앱은 게시 담당 |
| N04 | 신규 src/db/decision_forums.rs:NEW reserve/reconcile | forum 생성 may-have-sent는 재POST 금지 |
| N05 | 신규 src/db/decisions.rs:NEW create; src/db/decisions/selection.rs:NEW choose_once | 선택 CAS는 request당 1개 유지. 의견은 별도 event row; 버튼/숫자/이름 선택이 같은 CAS |
| N06 | 신규 src/services/discord/decisions/action_handle.rs:NEW codec | decision namespace; 최초 choice CAS가 일회성 소비 권한 |
| N07 | 신규 src/services/discord/decisions/component.rs:NEW handle | ACK·정상/동일/다른/타인 클릭, live target 검사 전에 선택 보존 |
| N08 | 신규 src/services/discord/decisions/transport.rs:NEW create/reconcile/edit | provider 최초 글·같은 메시지 편집. 새 글 재게시로 상태 복구 금지 |
| N09 | 신규 src/services/discord/decisions/worker.rs:NEW publish_projection | 입력별 의견/실패 표시와 lifecycle revision 수렴. 입력을 다시 실행하지 않음 |
| N10 | 신규 src/services/discord/decisions/worker.rs:NEW deliver_selected_once | event당 deliver_once. selection 1개, opinion은 message ID별 1개 |
| N11 | 신규 src/services/discord/decisions/worker.rs:NEW recover_once | event별 not_started 1회/정상 busy 원본/unknown 닫기. 스캔은 이미 처리한 event를 재주입하지 않음 |
| N12 | 신규 src/server/routes/agents_turn_deliver/decision.rs:NEW authorize_selected | PG actor/text/원문 hash/논리 route/event kind/closed 상태 검증 |
| N13 | 확장 src/services/discord/queue_io/input_owner/origin.rs:NEW Decision/decision_identity | 사용자 입력 신뢰·stable origin. request-session identity 해시 금지 |
| N14 | 신규 src/services/turn_orchestrator/queued_head/v2/decision.rs:NEW finalize_decision_once | 정확한 event 실행권 종결. request close는 해당 request의 아직 쓰지 않은 event만 종결 |
| N15 | 신규 src/services/discord/decisions/thread_target.rs:NEW resolve_current | #8125 뒤 같은 thread 현재 세션·없으면 정상화, 부모 category 규칙 |
| N16 | 확장 src/db/decisions.rs:NEW close_execution_once | event tombstone와 request ingress/close fence. raw 실패 의견 payload는 폐기 |
| N17 | 신규 src/services/discord/health/turn_deliver/decision_target.rs:NEW resolve_current/normalize_once | 정상화 최대 1회, old request session 재현 요구 없음; 정상화 실패는 폐기 |
| N18 | 신규 src/services/discord/queue_io/decision_drain.rs:NEW recover_context/finish | non-owner 일반·부팅·queued_head_inject의 PG provenance 복원·현재 세션 해석·실패 폐기 |
| N19 | 신규 src/services/discord/queue_io/decision_drain.rs:NEW reject_handoff_source; 기존 agent_handoff.rs에 호출 연결 | source=decision handoff는 backend 호출 전에 거절; 모델 HandoffDecision은 금지 우회 대조 |
| N20 | 신규 src/services/discord/decisions/transport.rs:NEW make_editable | 모더레이터 announce로 동일 thread의 필요한 flags만 변경, provider로 본문 편집; 닫힘 desired flags 재적용 |
| E49 | 기존 src/services/discord/router/intake_gate.rs:165, :179, :227 | MessageCreate의 60초 메모리 dedup/일반 thread 승격 전에 decision ingress로 분기·소비 |
| E50 | 기존 src/services/discord/runtime_bootstrap.rs:103, src/services/discord/runtime_bootstrap/gateway_runtime.rs:80 | GUILD_MESSAGES/MESSAGE_CONTENT 코드 경로. 앱의 실제 승인 상태는 별도이며 REST도 우회 아님 |
| E51 | 기존 src/services/discord/catch_up/phase1_sweep.rs:302, :315, :398 | after/before 페이지 조회 재사용 형태. decision은 별도 PG cursor로 전체 구간 증거를 보존 |
| E52 | 기존 src/services/discord/catch_up/phase2.rs:134, :171 | 기존 checkpoint/봉인 개념 대조. gateway high ID만으로 decision scan cursor를 올리지 않음 |
| E53 | 기존 src/server/routes/auth.rs:213, :260 | HumanInputCaller는 host/server 인증이며 requester ownership 증거는 아님 |
| E54 | 기존 src/services/tui_prompt_dedupe/binding_context.rs:113, :207, :272 | CLI 현재 agent/channel namespace 선택에 사용. base64 context만으로 후속 API 권한 부여 금지 |
| E55 | 기존 src/services/discord/http.rs:174, src/services/discord/outbound/transport.rs:22 | 동일 후속 메시지 nonce 재사용. recent nonce만으로 재시작 후 영구 중복 방지를 주장하지 않음 |
| E56 | 기존 src/services/discord/health.rs:469, src/cli/direct.rs:697 | announce REST client는 scoped thread archive/unlock/lock에만 재사용, 글/버튼/답글은 provider |
| N21 | 신규 src/services/discord/decisions/classify.rs:NEW classify_reply | 숫자/표시 이름 exact match는 선택, 나머지 원문은 의견, 봇·비운영자 무시 |
| N22 | 확장 src/services/discord/decisions/ingress.rs:NEW handle_message | GatewayObserve. 등록된 decision thread는 일반 agent intake로 절대 fallthrough하지 않음 |
| N23 | 신규 src/services/discord/decisions/scan.rs:NEW scan_threads | PeriodicScan. 창 anchor/before/covered cursor와 재스캔, 이미 처리한 message key는 실행하지 않음 |
| N24 | 신규 src/db/decisions/messages.rs:NEW observe_once/commit_scan_window | ledger unique(thread,message_id), 분류/accept/ignore를 durable 저장한 구간만 checkpoint |
| N25 | 신규 src/services/discord/decisions/opinion.rs:NEW forward_once | opinion stable origin·raw content 그대로, 정상화 1회·실패 폐기, 성공 확인 뒤 의견 전달됨 |
| N26 | 신규 src/services/discord/decisions/lifecycle.rs:NEW seal/close_request | 선택은 신규 접수 봉인, explicit close는 새 write 차단·미시도 원본 종결 |
| N27 | 확장 src/server/routes/decisions.rs:NEW reply | POST /api/decisions/{id}/reply. scoped cap+idempotency, 논리 상태 불변 |
| N28 | 확장 src/server/routes/decisions.rs:NEW close | POST /api/decisions/{id}/close. 결론 저장→출력→archive/lock, no input after close |
| N29 | 확장 src/cli/args/decision.rs:NEW Reply/Close, src/cli/decision.rs:NEW reply/close | ref는 현재 requester scope에서만 해석, 모호하면 UUID 선택 필요 |
| N30 | 신규 src/services/discord/decisions/requester.rs:NEW authorize_capability | PG hash로 request/agent/logical channel/action scope 결정. body agent 문자열은 권한 아님 |
| N31 | 신규 src/cli/decision/owner_cache.rs:NEW prepare/store/load | ask 전 자동 cap 생성·사설 저장. 일반 출력/GET/로그에 cap 노출 금지 |
| N32 | 신규 src/db/decisions/followups.rs:NEW begin_once/settle | reply/close idempotency·may-have-posted·동일 message 재확인, 새 POST 재시도 큐 없음 |
| N33 | 신규 src/services/discord/decisions/followup.rs:NEW post/restore_lifecycle | 닫힌 글 reply도 논리 gate 유지, 임시 editable 뒤 기존 archived/locked flags 복원 |
| N34 | 확장 src/services/discord/decisions/scan.rs:NEW boot_scan, src/services/discord/decisions/worker.rs:NEW recover_event | 복구 scan과 입력 attempt 재시작 분리. 닫힌 thread 최종 scan은 ignored_closed만 기록 |
| E57 | 기존 src/server/routes/agents_turn_deliver.rs:136, :226; src/server/routes/agents.rs:659 | 일반 text trim 전 검증된 decision raw 경로. OpinionRawPreserved의 API 경계 |
| E58 | 기존 src/services/discord/router/message_handler/headless_turn.rs:54, :711 | prompt.trim 및 sanitize_user_input 대조. 승인된 opinion만 raw body 유지 child로 호출 교체 |
| E59 | 기존 src/services/discord/router/message_handler/intake_turn.rs:762 | non-owner/일반 drain prompt sanitizer. PG 승인 event 복원 후 같은 raw 정책 |
| E60 | 기존 src/services/tui_input/human_frame.rs:97, :99, :109; src/services/discord/router/response_format.rs:161 | idle/CRLF 정규화·plain_inject·author wrapper 경계. 일반 형식 유지, 승인 의견 원문 정책만 분리 |
| E61 | 기존 src/services/discord/health/turn_deliver/inject/queued_reconcile.rs:374; src/ui/ai_screen.rs:43 | receipt frame 재구성도 전송한 raw 정책과 일치. 일반 sanitizer를 전역 삭제하지 않음 |
| N35 | 확장 src/services/discord/decisions/opinion.rs:NEW validated_raw_payload | 원본/hash/승인 provenance 유지. 분류용 trim과 전달 문자열 분리 |
| N36 | 확장 src/services/tui_input/human_frame.rs:NEW preserved_decision_body | 실제 idle/busy/receipt 모두 같은 원문 frame 정책. metadata 자기 선언만으로 우회 불가 |
| E62 | 기존 src/services/discord/router/message_handler/provider_dispatch/owner_input.rs:179 owner_drive | writer_boot_held면 held_watch/hold_start. 저장 context 읽기보다 앞선다. P2-1 종결 child 연결 |
| E63 | 기존 src/services/discord/router/message_handler/provider_dispatch/start_failure.rs:127, :209 | 시작 실패를 handoff_start_failed로 넘기고 retry_start/owner wake. decision은 P2-1 세 갈래 |
| E64 | 기존 src/services/turn_orchestrator/queued_head/v2/start_failure.rs:100 start_failed | opaque 실패는 원본 유지, 그 밖은 KnownNotSent retained와 retained_retry_required 안내. decision은 종결 |
| E65 | 기존 src/services/discord/runtime_bootstrap/queued_recovery_held.rs:159 settle_once | 확보한 HeldRestore.queue를 나중에 다시 merge. merge 시점에도 terminal 검사 |
| E66 | 기존 src/services/discord/queue_io/input_owner/notice.rs:68 → src/services/discord/queue_io/retained_notice.rs:32 | 원문 보관·재전송 전제 안내. decision 원본에는 내지 않음 |
| E67 | 기존 src/services/claude_tui/input.rs:126 plan_prompt_submit | CRLF·단독 CR→LF. P2-2 보존 정책을 여기까지 전달 |
| E68 | 기존 src/services/codex_tui/input.rs:474 plan_prompt_submit | 같은 정규화. P2-2 |
| E69 | 기존 src/services/codex_tui/input/submission.rs:10 owned_prompt_plan | 위 결과로 bracketed paste 구성. owner 경로도 같은 변형. P2-2 |
| E70 | 기존 src/services/codex_tui/input.rs:1182 admit_owned_codex_prompt | owner admission 연결 지점. P2-2 transport 비교 경계 |

## 상태 모델

[decision-request 모델 보고서](statecharts/decision-request/decision-request.report.md)가 DecisionRequest(입력 workflow), DecisionForum(포럼 생성), DecisionDiscussion(의견·cursor·reply/close)를 설명한다. 기대값은 [expected.tsv](statecharts/expected.tsv)의 decision-request 행 38개(정상 7, 의도한 반례 31)다. ForumRace는 잠금 없는 생성, ForumUnknown은 결과 불명 뒤 재POST가 NoDuplicateForum을 깨는 대조다. 상징 모델 PASS는 코드 자체의 증명이 아니며 조각별 RED→GREEN 시험이 실제 경계를 확인한다.

## 미확인

- 실제 gateway intent 승인, provider/announce 역할, forum/category 권한은 조회하지 않았다. 권한이 없으면 입력을 꾸며 만들지 않고 capture/projection/moderation 오류를 보인다.
- #8125의 최종 thread/Herdr 정상화 helper 계약은 설계 기준 코드에 아직 없다. §13 P2-3의 최소 계약으로 확인한다.
- 영구 PG/queue/Discord 장애·삭제된 글·moderation 권한 부재에서는 표시 수렴을 무조건 보장하지 않는다. commit 안 된 실행·종결을 성공으로 쓰지 않는다.
