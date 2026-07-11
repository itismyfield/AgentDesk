# Scheduled Messages (예약 메시지 풀) — DB & API 설계

## 목표

에이전트(또는 사람/시스템)가 **예약 메시지 풀**에 메시지를 저장해 두면, 지정한
날짜·시간에:

- **push 모드**: 에이전트 개입 없이 시스템이 `notify` bot으로 곧바로 Discord
  채널에 전송. 수신 에이전트를 의도적으로 깨워야 할 때만 `announce`를 명시
- **agent 모드**: 지정된 에이전트의 headless 턴을 대상 Discord 채널에서
  시작하고, 에이전트의 relay된 assistant 답변 자체를 전달 메시지로 사용

push 경로와 agent 실패 시 `push_raw` 강등 경로는 기존
`message_outbox` → `message_outbox_loop`을 재사용한다. agent 정상 경로는
outbox를 거치지 않고 기존 headless turn relay로 Discord에 게시된다. 예약
계층은 이 두 기존 전달 경로 위에 "언제/누가 보낼지"만 추가한다.

## 기존 컴포넌트와의 관계

| 기존 컴포넌트 | 역할 재사용 |
|---|---|
| `message_outbox` (0001, 0042, 0066) | 최종 Discord 전송 큐. push 모드 발화 시 여기로 enqueue. **outbox drain claim이 이미 `next_attempt_at <= NOW()`를 게이트하므로(`server/mod.rs` claim_pending_message_outbox_batch_pg) 지연 전송·재시도·claim을 outbox가 온전히 소유** — handoff 이후 스케줄러는 관여하지 않는다 |
| headless agent turn (`services/discord/health`) | agent 모드에서 턴 ID를 먼저 예약하고 `start_reserved_headless_agent_turn_with_owner_channel`로 대상 채널에 relay. 완료는 routines와 같은 transcript/quality-event 증거 모델로 판정 |
| `routines` / `routine_runs` (0035) | 스키마 패턴 차용: 정의 row + 실행 이력 row 분리, `next_due_at` partial index due-scan, lease 기반 중복 실행 방지, `schedule` 파서 재사용 |
| `worker_registry` + `message_outbox_loop` 패턴 | `scheduled_message_loop` 워커를 기존 등록 패턴으로 추가. adaptive backoff(500ms–5s) 폴링 패턴 동일 적용 |
| agent channel bindings | agent 모드는 명시적 `target_channel_id` 유무와 무관하게 primary Discord 채널을 필수로 한다. primary는 turn owner/session 컨텍스트이고, target 미지정 시 delivery 채널로도 사용 |
| `outbound/source_registry.rs` | push/강등 outbox enqueue 소스 `scheduled_message`를 `LoopbackInternal`로만 허용 |

### 검토했으나 채택하지 않은 대안 (재사용 극대화 관점)

- **outbox 단독 설계 (0-테이블)**: push 1회성 예약은 `INSERT INTO message_outbox
  (..., next_attempt_at = scheduled_at)` 하나로 이미 동작한다. 그러나 outbox는
  발송 대기 큐이지 예약 풀이 아니다 — 수정/취소/목록 API, 반복, agent 모드,
  작성자·이력이 전부 없다. 요구사항에 agent 모드가 명시돼 있어 정의 테이블은
  불가피하다. 대신 이 native 지연 능력을 push 모드 handoff 단순화에 활용한다
  (아래 상태 기계 참조).
- **routine으로 표현**: agent 모드 예약은 개념상 1회성 routine이지만,
  `routines.script_ref NOT NULL`(스크립트 중심 실행 모델)에 메시지 원문
  보관·수정 UX를 우겨넣으면 두 도메인이 모두 오염된다. 실행기만 재사용하고
  데이터 모델은 분리한다. 구현은 routine row를 위조하지 않고 그 아래
  headless-turn primitive와 완료 증거 모델만 재사용한다.

recurring 자동화 전반은 `routines`의 영역이다. 이 테이블은 "이 내용을 이 시각에
이 채널로"라는 **메시지 중심** 예약에 특화하되, `routines.schedule`과 같은
표현(`@every` duration 또는 5-field cron)으로 선택적 반복을 지원한다.

## DB 설계

Postgres 전용 (messages 라우트와 동일하게 pg pool 필수). 마이그레이션:
`migrations/postgres/0083_scheduled_messages.sql`부터
`0087_scheduled_message_launch_commit_and_runtime_defer.sql`까지
사용한다 (`0079`~`0081`은 최신 upstream 계열이 선점). 라이브에 적용된 0082와
이어지는 0083의 원문은 immutable하게 유지하고, recurrence anchor 컬럼과 최종
non-null invariant는 0084/0085에서 additive하게 적용한다. 0086은 agent launch의
3-state barrier, rolling-deploy legacy backfill, runtime defer gate를 fix-forward한다.

### `scheduled_messages` — 예약 정의(풀)

```sql
CREATE TABLE IF NOT EXISTS scheduled_messages (
    id                 TEXT PRIMARY KEY,            -- 'smsg_' + uuid
    -- 내용
    content            TEXT NOT NULL,
    title              TEXT,                        -- 목록/로그 표시용 (선택)
    -- 대상
    target_channel_id  TEXT,                        -- Discord 채널 ID.
                                                    -- agent 모드에서 NULL이면 primary channel 사용.
                                                    -- push 모드에서는 DB CHECK로 필수
    bot                TEXT NOT NULL DEFAULT 'notify',    -- info-only 기본값. announce는 agent turn trigger
    -- 전달 방식
    delivery_kind      TEXT NOT NULL DEFAULT 'push',      -- 'push' | 'agent'
    agent_id           TEXT REFERENCES agents(id),        -- delivery_kind='agent'일 때 필수
    agent_instruction  TEXT,                        -- agent 모드에서 메시지와 함께 주입할 지시문 (선택)
    on_agent_failure   TEXT NOT NULL DEFAULT 'fail',      -- 'fail' | 'push_raw'
                                                    -- push_raw: 에이전트 전달이 최종 실패하면 원문을 push 모드로 강등 전송
    -- 스케줄
    scheduled_at       TIMESTAMPTZ NOT NULL,        -- 다음(또는 유일한) 발화 시각
    runtime_defer_until TIMESTAMPTZ,                -- runtime prerequisite 재확인 not-before
    schedule           TEXT,                        -- 반복 규칙. NULL=1회성.
                                                    -- routines.schedule과 동일 문법 (@every 10m | 5-field cron)
    timezone           TEXT NOT NULL DEFAULT 'Asia/Seoul', -- cron 해석용
    expires_at         TIMESTAMPTZ,                 -- 반복 예약 종료 시각 (선택)
    -- 상태
    -- 'scheduled' | 'firing' | 'sent' | 'failed' | 'canceled' | 'expired'
    -- 반복 예약은 발화 후 다시 'scheduled'로 돌아간다 (터미널: sent/failed/canceled/expired)
    status             TEXT NOT NULL DEFAULT 'scheduled',
    in_flight_delivery_id TEXT,                     -- firing 중 delivery row ID (routines.in_flight_run_id 패턴)
    fire_count         BIGINT NOT NULL DEFAULT 0,
    last_fired_at      TIMESTAMPTZ,
    last_error         TEXT,
    -- 출처
    source             TEXT NOT NULL DEFAULT 'api', -- 'api' | 'agent' | 'discord' | 'system'
    created_by         TEXT,                        -- 생성 주체 (agent id 또는 사용자 식별자)
    -- 메타
    dedupe_key         TEXT,                        -- 생성 시점 중복 방지 (선택, 활성 상태에서 unique)
    created_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_smsg_delivery_kind CHECK (delivery_kind IN ('push', 'agent')),
    CONSTRAINT chk_smsg_on_agent_failure CHECK
        (on_agent_failure IN ('fail', 'push_raw')),
    CONSTRAINT chk_smsg_status CHECK (status IN
        ('scheduled', 'firing', 'sent', 'failed', 'canceled', 'expired')),
    CONSTRAINT chk_smsg_agent_required CHECK
        (delivery_kind <> 'agent' OR agent_id IS NOT NULL),
    CONSTRAINT chk_smsg_push_target_required CHECK
        (delivery_kind <> 'push' OR target_channel_id IS NOT NULL)
);

-- due-scan 전용 partial index (routines idx_routines_due_scan 패턴)
CREATE INDEX IF NOT EXISTS idx_scheduled_messages_due_scan
    ON scheduled_messages(scheduled_at)
    WHERE status = 'scheduled';

CREATE INDEX IF NOT EXISTS idx_scheduled_messages_agent
    ON scheduled_messages(agent_id, status)
    WHERE agent_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_scheduled_messages_channel
    ON scheduled_messages(target_channel_id)
    WHERE target_channel_id IS NOT NULL;

-- 활성(비터미널) 예약에 한해 dedupe_key 유일 (message_outbox uq 패턴)
CREATE UNIQUE INDEX IF NOT EXISTS uq_scheduled_messages_active_dedupe
    ON scheduled_messages(dedupe_key)
    WHERE dedupe_key IS NOT NULL
      AND status IN ('scheduled', 'firing');
```

### `scheduled_message_deliveries` — 발화 이력 (routine_runs 패턴)

반복 예약은 발화 slot당 1 row를 사용한다. 중단된 slot의 재시도는
새 row를 만들지 않고 같은 row를 re-arm하며, 매 attempt마다 `claim_token`을
교체해 만료된 워커의 느린 완료 쓰기를 fencing한다.

```sql
CREATE TABLE IF NOT EXISTS scheduled_message_deliveries (
    id                  TEXT PRIMARY KEY,           -- 'smdel_' + uuid
    scheduled_message_id TEXT NOT NULL REFERENCES scheduled_messages(id),
    fire_scheduled_at   TIMESTAMPTZ NOT NULL,       -- 이 발화가 예정됐던 시각 (dedupe 축)
    resume_scheduled_at TIMESTAMPTZ NOT NULL,       -- trigger-now retry 뒤 복원할 정규 recurrence anchor
    delivery_kind       TEXT NOT NULL,              -- 발화 시점 스냅샷
    -- 'running' | 'sent' | 'failed' | 'interrupted'
    status              TEXT NOT NULL DEFAULT 'running',
    -- 다중 노드 안전: claim + lease (routine_runs.lease_expires_at 패턴)
    claim_owner         TEXT,
    claim_token         TEXT NOT NULL,              -- attempt별 fencing token
    lease_expires_at    TIMESTAMPTZ,
    -- push 경로 추적
    outbox_id           BIGINT,                     -- 생성된 message_outbox row
    -- agent 경로 추적
    turn_id             TEXT,                       -- durable 에이전트 launch intent
    turn_intent_at      TIMESTAMPTZ,                -- 0086 writer가 기록한 확실한 pre-call intent
    launch_committed_at TIMESTAMPTZ,                -- at-most-once 외부 launch barrier
    turn_started_at     TIMESTAMPTZ,                -- runtime이 실제 Started를 반환한 시각
    fallback_outbox_id  BIGINT,                     -- on_agent_failure='push_raw' 강등 시
    retry_count         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at     TIMESTAMPTZ,                 -- durable retry not-before; re-arm 시 NULL로 clear
    error               TEXT,
    started_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    finished_at         TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),

    CONSTRAINT chk_smdel_status CHECK (status IN
        ('running', 'sent', 'failed', 'interrupted')),
    -- 동일 발화 시각 중복 발화 방지 (at-most-once per fire slot)
    CONSTRAINT uq_smdel_fire_slot UNIQUE (scheduled_message_id, fire_scheduled_at)
);

CREATE INDEX IF NOT EXISTS idx_smdel_parent
    ON scheduled_message_deliveries(scheduled_message_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_smdel_running_lease
    ON scheduled_message_deliveries(lease_expires_at)
    WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_smdel_turn_id
    ON scheduled_message_deliveries(turn_id)
    WHERE turn_id IS NOT NULL;
```

## 상태 기계

```
scheduled ──(due, claim 성공)──▶ firing ──(전송 확인)──▶ sent        (1회성)
    ▲                              │       └(schedule 있음)─▶ scheduled (다음 fire 시각 재계산)
    │                              ├──(재시도 소진)──▶ failed
    └──(PATCH로 시각/내용 수정)      └──(expires_at 경과)─▶ expired
scheduled ──(DELETE)──▶ canceled
firing    ──(DELETE)──▶ canceled   (진행 중 delivery는 interrupted 마킹)
```

- **push 모드의 "sent" 판정 = outbox handoff 성공**: `message_outbox` INSERT가
  성공하면 delivery는 즉시 터미널(`sent`, 의미상 "handed off"). 부모와
  delivery를 먼저 lock하고 outbox INSERT와 두 상태 전이를 한 transaction으로
  commit하므로, 취소가 먼저 이기면 outbox side effect가 생기지 않는다. 이후
  재시도·최종 실패는 outbox가 자체 소유하며(retry_count/next_attempt_at),
  스케줄러는 outbox 상태를 다시 폴링하지 않는다 — 감시 책임을 이중으로 두지
  않는다. 최종 전송 결과는 `deliveries.outbox_id` 조인으로 조회 가능
  (GET deliveries 응답에 outbox 상태를 lazy join으로 포함). slot dedupe row는
  `dedupe_expires_at IS NULL`인 영구 sentinel이므로 실시간 outbox GC와 정기 DB
  retention 모두 이를 삭제하지 않는다. 이는 fire slot당 row 하나가 계속 남는
  저장공간 trade-off이며, 별도 compact dedupe ledger가 생기기 전까지 at-most-once
  계약을 우선한다.
- **agent 모드의 "sent" 판정 = relay 증거 확인**: 예약한 turn ID의
  `session_transcripts`에서 non-empty assistant 메시지를 확인하면 relay된
  답변이 게시됐다고 판정한다. evidence 시각 하한은 늦은 runtime ack가 아니라
  `launch_committed_at`이므로 매우 빠른 reply/error도 놓치지 않는다.
  `NO_REPLY`(대소문자/주변 공백 무시)는 전달
  성공이 아니다. `NO_REPLY`와 `empty_response`처럼 turn이 끝났다는 확정 증거가
  있을 때만 `push_raw`로 강등한다. 30분 내 terminal evidence가 없으면 아직
  살아 있는 turn이 늦게 relay할 수 있으므로 raw fallback 없이 fail-closed한다.
  정의가 그 사이 만료됐더라도 아직 살아 있는 evidence 없는 turn은 조기 종료하지
  않고, 확정 실패 또는 30분 timeout 시점에만 fallback 없이 `expired`로 닫는다.
  별도 ack API는 구현되어 있지 않다.
- **`in_flight_delivery_id`는 모든 claim의 원자적 부모↔delivery 축**이다.
  push는 claim→outbox handoff→터미널을 즉시 완료하고, agent는 완료 증거가
  나올 때까지 `firing`/`running`을 유지한다.
- **만료는 실제 발화 시각에도 재검사**한다. due claim 후 `expires_at <=
  claim time`이면 이미 생성한 delivery를 `interrupted`(사유: definition expired)로
  닫고 부모를 `expired`로 만든다. 반복 전달이 성공했더라도 다음 anchored
  slot이 `expires_at` 이상이면 다음 slot을 잡지 않고 부모를 `expired`로 종료한다.

## 스케줄러 워커 — `scheduled_message_loop`

`worker_registry`에 기존 `message_outbox_loop`와 동일 패턴으로 등록
(adaptive backoff 500ms–5s — 예약 메시지는 분 단위 정밀도면 충분하므로
idle 시 5s 상한까지 늘어나는 기존 백오프 로직을 그대로 사용).

```text
매 tick:
1. due-claim (다중 노드 안전):
   scheduled + due 부모를 FOR UPDATE SKIP LOCKED로 잠그고 slot row를 arm.
   → 첫 attempt: delivery row INSERT, claim_owner/claim_token/2분 lease 기록.
   → 중단된 같은 slot: `next_attempt_at <= claim time`일 때만 ON CONFLICT ...
     WHERE status='interrupted'로 같은 delivery row를 re-arm. claim_token 교체,
     retry_count + 1,
     outbox_id/turn_id/turn_intent_at/launch_committed_at/turn_started_at/
     fallback_outbox_id/error/완료 시각을 초기화.
   → 부모를 firing으로 바꾸고 in_flight_delivery_id를 연결.

2. 발화:
   - claim time에 expires_at 경과 → delivery=interrupted, 부모=expired
   - push:  active parent+delivery lock → message_outbox INSERT
            (target=channel_id, content, bot, source='scheduled_message',
             slot 단위의 만료 없는 durable dedupe key)
            → 새 row 또는 기존 활성 row ID 확보 + delivery/parent 종료를 같은
              transaction으로 commit하면 delivery=sent(터미널, "handed off").
              이후 재시도/실패는 outbox_loop 소유 — 여기서 다시 감시하지 않는다.
   - agent: primary channel/provider binding을 해석하고 target(미지정 시 primary)에서
            headless turn을 시작. 예약한 turn ID와 `turn_intent_at`을 claim_token
            조건으로 먼저 기록하되 lease는 갱신하지 않는다. 외부 runtime 호출
            직전에 parent/claim/intent를 다시 fencing하고 `launch_committed_at`을
            at-most-once barrier로 commit한다. runtime이 `Started`를 반환한 뒤에만
            `turn_started_at`을 기록한다. barrier 뒤 프로세스가 죽으면 실제 launch
            여부가 모호하므로 replacement 없이 poll/timeout fail-closed한다.
            assistant reply는 target 채널에 그대로 relay된다.

3. 완료 감시 (agent 모드 running delivery만 해당):
   - `message_outbox_loop`와 같은 process-wide 계약으로 Discord runtime이 있는
     프로세스만 poll한다. runtime 없는 leader는 durable turn을 그대로 두고,
     runtime 복구 후 다른 leader가 adopt한다.
   - turn ID의 non-empty assistant transcript → sent.
   - NO_REPLY, empty_response → 확정 terminal evidence를 parent→delivery lock 아래
     재검증하고, push_raw outbox INSERT + delivery/parent 종료를 한 transaction으로
     commit. fail 정책이면 outbox 없이 failed.
   - 30분 timeout인데 terminal evidence가 없음 → 늦은 agent relay와 raw push의
     이중 전달을 막기 위해 fallback 없이 failed(fail-closed).
   - 확정 실패 뒤 push_raw 강등도 active parent+delivery lock, outbox INSERT,
     delivery/parent 종료를 한 transaction으로 commit한다.
   - poll owner는 자기 active lease 또는 만료/unowned row만 가져간다. 다른
     leader의 active lease는 건너뛰며, takeover 시 claim_token을 교체해 stale
     poller를 fencing한다. lease 만료가 가까운 순서로 batch를 순환한다.

4. 재시도/부모 갱신:
   - launch commit 전 lease 만료 또는 provider/bridge spawn 전 반환된 오류 → 부모를
     먼저 lock한 뒤 delivery를
     interrupted로 바꾸고, 부모를 같은 fire_scheduled_at의 scheduled로 rewind.
     `next_attempt_at`의 durable not-before를 due scan이 지키며 1분→5분→15분 뒤
     같은 row를 fenced re-arm한다. 마지막 실패 뒤에는 추가 지연 없이 다음 claim이
     3회 re-arm 예산 소진을 판정해 터미널 실패(또는 push_raw)로 닫는다. 백오프
     중인 오래된 row는 다른 due row의 claim을 막지 않는다.
     push_raw handoff가 성공하면 현재 delivery는 sent이지만, 재시도 소진은
     정의 단위 터미널 사건이므로 반복 부모도 failed에 머물고 다음 slot을 잡지 않는다.
   - schedule NULL → status='sent'|'failed'
   - schedule 있음 → 완료 시각이 아니라 현재 scheduled slot을 anchor로
     `next_due_after_anchor` 계산. 놓친 주기는 건너뛰고 now 이후의 첫
     anchored slot으로 scheduled_at 갱신, status='scheduled',
     fire_count += 1, in_flight_delivery_id = NULL
```

만료 lease 복구는 부모→delivery 순서로 lock하고 만료 조건을 다시
확인한다. turn ID가 없거나 0086 writer의 `turn_intent_at IS NOT NULL`이면서 아직
`launch_committed_at IS NULL`인 확실한 pre-call row만 re-arm한다. launch commit이
있거나 rolling deploy 중 구버전 writer가 남긴 marker 없는 turn ID는 새 turn을
만들지 않고 다음 leader의 poller가 같은 durable turn을 adopt한다. runtime ack가
없어도 timeout까지 fail-closed하며, 이는 claim token으로 막을 수 없는 기존 turn의
늦은 Discord relay와 replacement turn의 중복 발화를 방지한다.
pre-call rewind는 모든 turn phase marker를 같은 transaction에서 clear한다. 따라서
이후 구버전 leader가 같은 slot을 re-arm해도 stale 0086 intent가 남지 않는다.
Discord runtime 자체가 없는 프로세스는 push와 agent 정의 모두 claim하지 않는다.
runtime이 booting/cached-context/token-unavailable 상태인 direct agent fire도 retry
budget을 소비하지 않고 정의를 되돌린다. 원래 recurrence anchor는 유지하고 별도
`runtime_defer_until`을 15초 뒤로 설정하므로 overdue 정의가 500ms마다 hot-loop하지
않으며, successful claim 또는 operator PATCH가 이 gate를 clear한다.

## API 설계

베이스: `/api/scheduled-messages`. 응답/요청 JSON은 camelCase
(`routes/messages.rs` 컨벤션). pg pool 없으면 503 (기존과 동일).
새 파일 `src/server/routes/scheduled_messages.rs`, DB 계층 `src/db/scheduled_messages.rs`.
현재 제공 범위는 protected ops 도메인의 REST API뿐이며, Dashboard UI나
전용 CLI 커맨드는 구현하지 않았다.

### POST `/api/scheduled-messages` — 예약 생성

```jsonc
// Request
{
  "content": "내일 오전 스탠드업 안건: ...",         // 필수
  "title": "스탠드업 리마인더",                      // 선택
  "targetChannelId": "1492021444308238487",        // push면 필수, agent면 선택
  "bot": "notify",                                 // 선택, 기본 notify
  "deliveryKind": "agent",                         // 'push' | 'agent', 기본 push
  "agentId": "coder",                              // agent 모드 필수
  "agentInstruction": "핵심만 3줄로 요약해서 보내줘", // 선택
  "onAgentFailure": "push_raw",                    // 선택, 기본 fail
  "scheduledAt": "2026-07-08T09:00:00+09:00",      // 필수, ISO 8601
  "schedule": "0 9 * * 1-5",                       // 선택 (반복), @every 24h 도 가능
  "timezone": "Asia/Seoul",                        // 선택, 기본 Asia/Seoul
  "expiresAt": "2026-08-01T00:00:00+09:00",        // 선택
  "source": "agent",                               // 선택
  "createdBy": "planner",                          // 선택
  "dedupeKey": "standup-reminder-w28"              // 선택
}
// 201 Response
{ "scheduledMessage": { "id": "smsg_...", "status": "scheduled", ... } }
```

검증 (400):
- `scheduledAt`이 과거(허용 오차 60s 초과) && `schedule` 없음 → 거부
  (`schedule` 있으면 다음 cron 시각으로 자동 보정 후 응답에 보정값 반환)
- `deliveryKind='agent'` && `agentId` 없음/미존재 → 거부
- `deliveryKind='push'` && `targetChannelId` 없음 → 거부
- `deliveryKind='push'`인데 `agentId`/`agentInstruction` 값 또는 명시적
  `onAgentFailure`를 보내면 거부한다. PATCH의 명시적 `null`은 기존 agent 전용
  값을 clear하는 요청으로 허용하고, 일반 metadata PATCH는 DB 기본값 `fail`을
  명시 입력으로 오인하지 않는다.
- 명시한 `targetChannelId`가 양의 Discord channel ID 또는 알려진 alias가 아님 →
  거부. alias는 생성/수정 시 numeric ID로 정규화해 push와 `push_raw`가 같은
  durable target을 사용한다.
- `deliveryKind='agent'` && 해당 agent의 primary provider/channel이 없거나
  channel을 유효한 ID/alias로 해석할 수 없음 →
  `targetChannelId` 명시 여부와 무관하게 거부. primary channel은 headless turn의
  owner/session 컨텍스트이고, `targetChannelId`는 relay 대상이다.
- `expiresAt <= scheduledAt` → 거부
- 활성 `dedupeKey` 충돌 → 409 + 기존 row 반환 (idempotent create)

`bot`을 생략하면 info-only sink인 `notify`를 사용한다. `announce`는 AgentDesk의
authoritative turn-trigger bot이므로 명시적으로 선택하면 push executor 자체는
agent를 호출하지 않더라도 agent-bound 대상 채널의 수신 에이전트가 메시지를 새
지시로 처리할 수 있다.

### GET `/api/scheduled-messages` — 목록

쿼리: `status`, `deliveryKind`, `agentId`, `targetChannelId`, `dueBefore`,
`dueAfter`, `limit`(기본 50, 최대 200), `before`(created_at 커서 — messages
라우트와 동일 페이지네이션).

```jsonc
{ "scheduledMessages": [ ... ], "nextCursor": "2026-07-07T14:00:00Z" }
```

### GET `/api/scheduled-messages/:id`

정의 + 최근 delivery 5건 포함.

```jsonc
{ "scheduledMessage": { ... }, "recentDeliveries": [ ... ] }
```

### PATCH `/api/scheduled-messages/:id` — 수정

`status IN ('scheduled')`일 때만 허용 (firing/터미널이면 409).
수정 가능 필드: `content`, `title`, `targetChannelId`, `bot`, `agentId`,
`agentInstruction`, `onAgentFailure`, `scheduledAt`, `schedule`, `timezone`,
`expiresAt`. 검증은 POST와 동일. 반복 정의의 효과적 `scheduledAt`이
과거면 PATCH도 기존 `scheduledAt`을 anchor로 현재 시각 이후의 다음 schedule
시각으로 보정해 metadata-only 수정이 `@every` cadence를 edit 시각으로 옮기지
않는다. `schedule`을
제거한 결과 효과적인 one-shot 시각이 과거가 되는 PATCH는 `scheduledAt`을 함께
보내지 않았더라도 거부한다.

### DELETE `/api/scheduled-messages/:id` — 취소

- `scheduled` → `canceled` (200)
- `firing` → `canceled` + 진행 delivery `interrupted` 마킹. push/fallback handoff와
  같은 parent-first lock을 사용하므로 취소가 먼저 commit되면 outbox는 생성되지
  않는다. handoff transaction이 먼저 commit된 경우에는 이미 완료된 전달이다.
  agent 전달의 handoff 안내는 단순 turn intent가 아니라 at-most-once
  `launch_committed_at`(또는 rolling legacy ambiguous turn ID)을 기준으로 한다.
- 터미널 상태 → 409

### POST `/api/scheduled-messages/:id/trigger-now` — 즉시 발화 (테스트/수동)

`scheduled` 상태에서만. `fire_scheduled_at = NOW()`로 delivery를 즉시 생성하고
발화 경로를 비동기로 태우고 202를 반환한다. 반복 예약이고 원래
`scheduledAt`이 미래면 이를 delivery의 `resume_scheduled_at`에 영속해,
즉시 발화가 transient retry를 거쳐도 완료 후 원래 미래 slot을 유지한다.
이 프로세스에 Discord runtime이 없으면 delivery 종류와 관계없이 claim 전에 503을
반환한다.

```jsonc
{ "delivery": { "id": "smdel_...", "status": "running" } }
```

### GET `/api/scheduled-messages/:id/deliveries` — 발화 이력

쿼리: `limit`(기본 20), `before` 커서.

### 에이전트가 예약을 넣는 경로

에이전트는 위와 동일한 HTTP API를 사용한다 (`source: "agent"`,
`createdBy: <agent id>`). 별도 내부 API를 만들지 않는다 — 기존 스킬/도구에서
`POST /api/scheduled-messages` 하나로 충분하다. 이는 REST 호출 규약이지 별도
예약-message CLI/스킬을 이 기능이 새로 제공한다는 의미는 아니다.

## 결정 사항 요약

1. **push outbox 재사용 + 감시 책임 단일화**: push와 `push_raw`는
   slot 단위 persistent dedupe로 `message_outbox`에 handoff한 뒤 즉시 손을 떼고,
   재시도/최종 전송은 outbox가 소유한다. agent 정상 전달은 outbox가 아닌
   headless turn relay와 transcript 증거를 사용한다.
2. **정의/이력 분리 + fenced lease claim**: `uq_smdel_fire_slot` +
   `FOR UPDATE SKIP LOCKED`로 slot을 하나로 유지하고, 중단 slot은 같은 row에
   새 `claim_token`을 부여해 re-arm한다. attempt 워커의
   완료/중단/turn-ID 쓰기가 token으로 fencing되므로 lease를 잃은 워커가
   교체 attempt를 덮어쓸 수 없다. agent poll도 active owner lease를 독점하고
   takeover 때 token을 회전한다.
3. **반복은 선택 기능이고 slot 기준으로 anchor**: 1회성 예약이 1급
   시민. 반복 문법은 `routines.schedule` 파서를 재사용하고 다음 시각은
   완료 시각이 아닌 예정 slot에서 계산해 지연이 축적되지 않게 한다.
4. **agent 모드 실패 강등 옵션**(`on_agent_failure='push_raw'`): "반드시 나가야
   하는 공지"와 "에이전트 가공이 의미인 메시지"를 예약 단위로 구분한다.
   단, fallback은 turn 종료가 확정된 실패에만 허용하고 outbox/상태 전이를
   원자적으로 commit한다. 단순 timeout은 중복 전달보다 fail-closed를 택한다.
5. **Postgres 전용**: messages 라우트와 동일하게 pg pool 필수, sqlite 호환
   마이그레이션은 만들지 않는다.

## 구현 파일 맵

| 파일 | 내용 |
|---|---|
| `migrations/postgres/0083_scheduled_messages.sql` ~ `0087_scheduled_message_launch_commit_and_runtime_defer.sql` | 기존 0082 checksum 보존 + info-only bot 기본값 + recurrence anchor 보정 + launch/rolling-writer ambiguity barrier + runtime defer gate (+ immutable-checksums.json 갱신) |
| `src/db/scheduled_messages.rs`, `src/db/scheduled_messages/{agent,outbox}.rs` | CRUD + due-claim + delivery/agent-poll/outbox 조회 쿼리 |
| `src/server/routes/scheduled_messages.rs` | 위 7개 핸들러 |
| `src/server/routes/mod.rs`, `domains/ops.rs` | 라우트 등록 (protected ops 도메인) |
| `src/server/routes/docs/inventory/endpoints/part_09.rs` | API docs 인벤토리 항목 (coverage 가드 필수) |
| `src/services/scheduled_messages.rs`, `src/services/scheduled_messages/evidence.rs` | `scheduled_message_loop` 워커 (fire/감시/복구) + launch-anchor evidence 조회 |
| `src/server/worker_registry.rs` | 워커 등록 항목 추가 (`ScheduledMessages`, leader-only) |
| `src/server/outbox_gc.rs`, `src/services/maintenance/jobs/db_retention.rs` | 영구 slot dedupe sentinel을 GC/retention에서 보존 |
| `src/services/routines/store.rs` | `next_due_after_anchor`를 `pub(crate)`로 공개 (스케줄 문법 + slot anchor 재사용) |
| `src/services/discord/outbound/source_registry.rs` | `scheduled_message`를 LoopbackInternal source로 등록 |

구현 노트 (설계와의 차이):
- agent 모드 실행은 `RoutineAgentExecutor::start_agent_run` 직접 호출 대신 그 아래
  프리미티브인 `start_reserved_headless_agent_turn_with_owner_channel`을 사용한다 —
  executor는 `RoutineStore`/`ClaimedRoutineRun`에 강결합돼 있어 가짜 routine row가
  필요해진다. 완료 증거 쿼리(session_transcripts/agent_quality_event)는 동일 모델.
- agent 턴은 대상 채널에서 직접 시작한다: 릴레이된 assistant 응답 자체가 전달된
  메시지이고, `NO_REPLY`가 아닌 non-empty assistant transcript가 전달 증거다.
- 중복 turn 방지를 위해 예약한 turn ID를 intent로 기록한 뒤, 외부 turn start
  직전에 별도 launch commit barrier를 통과한다. intent 전/확실한 intent-only
  crash는 re-arm할 수 있지만 barrier 이후 crash는 새 turn을 시작하지 않고
  기록된 turn을 adopt해 evidence timeout까지 fail-closed한다. 공용 runtime에는
  idempotent `start_or_get(turn_id)`가 없으므로 이 구간은 중복보다 zero-delivery를
  택하는 at-most-once 선택이다.
- agent의 primary Discord channel은 항상 필수다. 명시적 target이 있어도
  primary를 owner channel로 사용하고 target을 turn/relay channel로 사용한다.
- push_raw 강등 시 대상 채널이 없으면 `agent:<id>` outbox 타깃(에이전트 기본 채널)
  으로 보낸다.
