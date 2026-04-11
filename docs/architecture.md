# AgentDesk Architecture

> AI 에이전트 조직을 원격으로 운영하는 단일 바이너리 오케스트레이션 플랫폼

## 설계 원칙

1. **Single Binary** — Rust 바이너리 하나로 설치/배포
2. **Single Process** — 프로세스 간 통신 없음, 장애 지점 최소화
3. **Single DB** — SQLite 하나에 모든 상태
4. **Hot-Reloadable Policies** — 비즈니스 로직은 JS 파일로 분리, 재빌드 없이 변경
5. **Self-Contained** — Node.js, Python 등 외부 런타임 불필요

---

## 시스템 구조도

```
┌─────────────────────────────────────────────────────────┐
│                    AgentDesk Binary (Rust)               │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────┐  │
│  │ Discord  │  │ Session  │  │   HTTP   │  │ GitHub │  │
│  │ Gateway  │  │ Manager  │  │ Server   │  │  Sync  │  │
│  │ (serenity│  │ (tmux)   │  │ (axum)   │  │ (gh)   │  │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘  └───┬────┘  │
│       │              │             │             │       │
│  ┌────┴──────────────┴─────────────┴─────────────┴────┐  │
│  │              Core Event Bus (channels)              │  │
│  └────┬──────────────┬─────────────┬─────────────┬────┘  │
│       │              │             │             │       │
│  ┌────┴─────┐  ┌─────┴────┐  ┌────┴─────┐  ┌───┴────┐  │
│  │ Dispatch │  │  Policy   │  │ Database │  │   WS   │  │
│  │ Engine   │  │  Engine   │  │ (SQLite) │  │Broadcast│  │
│  │          │  │(QuickJS)  │  │(rusqlite)│  │        │  │
│  └──────────┘  └──────────┘  └──────────┘  └────────┘  │
│                     │                                    │
│              ┌──────┴──────┐                             │
│              │  policies/  │  ← JS 파일 (hot-reload)     │
│              │  *.js       │                             │
│              └─────────────┘                             │
└─────────────────────────────────────────────────────────┘
         │
    ┌────┴────┐
    │ Static  │  ← React 프론트엔드 (빌드 산출물)
    │Dashboard│
    └─────────┘
```

---

## 모듈 상세

드리프트가 잦은 사실값은 generated docs를 source of truth로 관리한다. 아래 파일들은 `python3 scripts/generate_inventory_docs.py`로 갱신되며, CI가 stale 상태를 실패로 처리한다.

- [Module inventory](generated/module-inventory.md)
- [Route inventory](generated/route-inventory.md)
- [Bootstrap worker inventory](generated/worker-inventory.md)

### 1. Discord Runtime (`src/services/discord/`)

Serenity/Poise 기반 Discord gateway, message router, turn bridge, tmux watcher, recovery, meeting orchestration이 이 트리에 모여 있다. 세부 파일 목록과 giant-file 현황은 generated module inventory를 참고한다.

핵심 역할:
- Discord 이벤트 수신과 명령 처리
- provider turn lifecycle과 tmux/process backend handoff
- restart recovery, inflight restore, watcher cleanup
- round-table meeting orchestration과 Discord side-effect delivery

### 2. Session + Provider Runtime (`src/services/`, `src/dispatch/`)

세션 실행 백엔드, provider adapter, dispatch lifecycle, runtime/platform abstraction이 여기에 있다. 이 영역은 "어떤 provider 프로세스를 어떻게 띄우고, 결과를 어떻게 회수하는가"를 담당한다.

핵심 역할:
- Claude/Codex/Gemini/Qwen provider 실행과 stderr/stdout 처리
- tmux/process backend wrapper와 세션 진단
- dispatch 생성, chaining, 상태 반영
- OS/runtime path, binary, shell abstraction

### 3. HTTP Server (`src/server/`)

Axum router, WebSocket broadcast, auth middleware, REST API, bootstrap worker 시작점이 여기에 있다. 정확한 endpoint 목록은 generated route inventory를, `server::run`에서 시작되는 background task/thread 목록은 generated worker inventory를 참고한다.

핵심 역할:
- `/ws` WebSocket endpoint와 `/api/*` REST surface 제공
- AppState 조립과 route handler wiring
- startup reconcile, outbox loop, policy tick, background sync bootstrap
- dashboard 정적 파일 서빙

### 4. Policy Engine (`src/engine/`)

QuickJS (rquickjs 크레이트) 기반 내장 JS 런타임.

| 파일 | 역할 |
|------|------|
| `runtime.rs` | QuickJS 런타임 초기화, 글로벌 객체 주입 |
| `ops.rs` | Rust ↔ JS 브릿지 함수 정의 |
| `loader.rs` | policies/ 디렉토리 감시, 핫 리로드 |
| `hooks.rs` | 라이프사이클 훅 정의 및 실행 |

#### Bridge Ops (JS에서 호출 가능한 Rust 함수)

```javascript
// DB 접근
agentdesk.db.query("SELECT * FROM kanban_cards WHERE status = ?", ["ready"])
agentdesk.db.execute("UPDATE kanban_cards SET status = ? WHERE id = ?", ["in_progress", id])

// Discord
agentdesk.discord.send(channelId, "메시지")
agentdesk.discord.sendToAgent(agentId, "메시지")

// GitHub
agentdesk.github.closeIssue(repo, number)
agentdesk.github.comment(repo, number, body)
agentdesk.github.getIssue(repo, number)

// 디스패치
agentdesk.dispatch.create({ from, to, type, title, context })
agentdesk.dispatch.complete(dispatchId, result)

// 칸반 (상태 전환 + 부수효과)
agentdesk.kanban.transition(cardId, newStatus, reason)

// 에이전트
agentdesk.agent.get(agentId)
agentdesk.agent.list({ status: "working" })

// 설정
agentdesk.config.get("review.maxRounds")

// 실시간
agentdesk.ws.broadcast("kanban_card_updated", payload)

// 로깅
agentdesk.log.info("message")
agentdesk.log.warn("message")
```

#### 라이프사이클 훅

Policy JS 파일에서 등록하는 이벤트 핸들러:

```javascript
// policies/kanban-rules.js
export default {
  name: "kanban-rules",
  priority: 10,

  onSessionStatusChange({ agentId, status, dispatchId }) {
    // working → in_progress 승격
    if (status === "working" && dispatchId) {
      const card = agentdesk.kanban.getByDispatchId(dispatchId);
      if (card && card.status === "requested") {
        agentdesk.kanban.transition(card.id, "in_progress", "agent_started");
      }
    }
    // idle (from working) → review
    if (status === "idle" && dispatchId) {
      const card = agentdesk.kanban.getByDispatchId(dispatchId);
      if (card && card.status === "in_progress") {
        agentdesk.kanban.transition(card.id, "review", "agent_completed");
      }
    }
  },

  onCardTransition({ card, from, to }) {
    // done → GitHub issue close + XP reward
    if (to === "done" && card.github_issue_url) {
      agentdesk.github.closeIssue(card.repo, card.issue_number);
      agentdesk.kanban.reward(card.id);
    }
  },

  onDispatchCompleted({ dispatchId, result }) {
    // follow-up request → auto-chain
    if (result.follow_up_request) {
      agentdesk.dispatch.create(result.follow_up_request);
    }
  },
};
```

```javascript
// policies/review-policy.js
export default {
  name: "counter-model-review",
  priority: 100,

  onReviewEnter({ card }) {
    const maxRounds = agentdesk.config.get("review.maxRounds") || 3;
    if (card.review_round >= maxRounds) {
      agentdesk.kanban.transition(card.id, "dilemma_pending");
      return;
    }
    // counter-model dispatch
    const counterChannel = card.provider === "claude" ? card.codex_channel : card.claude_channel;
    agentdesk.dispatch.create({
      from: "system",
      to: counterChannel,
      type: "review",
      title: `Review: ${card.title}`,
      context: card.review_context,
    });
  },

  onReviewVerdict({ card, verdict }) {
    if (verdict.overall === "pass") {
      agentdesk.kanban.transition(card.id, "done", "review_passed");
    } else {
      agentdesk.kanban.transition(card.id, "suggestion_pending", verdict);
    }
  },
};
```

```javascript
// policies/auto-queue.js
export default {
  name: "auto-queue",
  priority: 200,

  onCardTerminal({ card }) {
    // 다음 ready 카드 자동 dispatch
    const next = agentdesk.db.query(
      `SELECT * FROM kanban_cards
       WHERE repo_id = ? AND status = 'ready'
       ORDER BY priority DESC, created_at ASC LIMIT 1`,
      [card.repo_id]
    );
    if (next) {
      agentdesk.kanban.transition(next.id, "requested", "auto_queue");
    }
  },
};
```

```javascript
// policies/timeout-policy.js
export default {
  name: "timeout-policy",

  // 1분 주기로 호출
  onTick() {
    const now = Date.now();

    // requested 45분 초과 → failed
    const staleRequested = agentdesk.db.query(
      `SELECT * FROM kanban_cards WHERE status = 'requested'
       AND updated_at < ?`, [now - 45 * 60000]
    );
    for (const card of staleRequested) {
      agentdesk.kanban.transition(card.id, "failed", "timeout_requested");
    }

    // in_progress 100분 초과 → blocked
    const staleProgress = agentdesk.db.query(
      `SELECT * FROM kanban_cards WHERE status = 'in_progress'
       AND updated_at < ?`, [now - 100 * 60000]
    );
    for (const card of staleProgress) {
      agentdesk.kanban.transition(card.id, "blocked", "timeout_in_progress");
    }
  },
};
```

### 5. Dispatch Engine (`src/dispatch/`)

| 파일 | 역할 | 출처 |
|------|------|------|
| `executor.rs` | 디스패치 생성, 라우팅, Discord 전송 | PCD `dispatch-watcher.ts` |
| `result.rs` | 결과 수신 및 처리 | PCD `dispatch-watcher.ts` |
| `chain.rs` | follow-up 자동 chaining | PCD `dispatch-watcher.ts` |

**변경점:**
- 파일 기반 handoff/result → 직접 함수 호출
- PCD의 dispatch-watcher 파일 폴링 → Rust 내부 이벤트
- `createDispatchForKanbanCard()` → Dispatch Engine + Policy 조합

### 6. GitHub Integration (`src/github/`)

| 파일 | 역할 | 출처 |
|------|------|------|
| `sync.rs` | issue 상태 양방향 동기화 | PCD `kanban-github.ts` |
| `triage.rs` | 미분류 이슈 자동 분류 | PCD `issue-triage.ts` |
| `dod.rs` | DoD 체크리스트 미러링 | PCD `kanban-github.ts` |

**구현:** `gh` CLI 호출 또는 GitHub REST API 직접 호출

### 7. Database (`src/db/`)

| 파일 | 역할 |
|------|------|
| `schema.rs` | 테이블 정의 + 마이그레이션 |
| `dao.rs` | 공통 DAO 함수 |
| `migration.rs` | 버전 기반 스키마 마이그레이션 |

---

## 통합 DB 스키마

```sql
-- 에이전트 정의 (기존 PCD agents + RCC org.yaml 통합)
CREATE TABLE agents (
  id          TEXT PRIMARY KEY,           -- role_id (예: ch-td)
  name        TEXT NOT NULL,
  name_ko     TEXT,
  department  TEXT,
  provider    TEXT DEFAULT 'claude',      -- claude/codex/gemini
  discord_channel_id    TEXT,             -- primary channel (claude)
  discord_channel_alt   TEXT,             -- alt channel (codex)
  avatar_emoji TEXT,
  status      TEXT DEFAULT 'idle',        -- idle/working/offline
  xp          INTEGER DEFAULT 0,
  skills      TEXT,                       -- JSON array
  created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
  updated_at  DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 칸반 카드 (기존 PCD kanban_cards 유지)
CREATE TABLE kanban_cards (
  id                  TEXT PRIMARY KEY,
  repo_id             TEXT,
  title               TEXT NOT NULL,
  status              TEXT DEFAULT 'backlog',
  priority            TEXT DEFAULT 'medium',
  assigned_agent_id   TEXT REFERENCES agents(id),
  github_issue_url    TEXT,
  github_issue_number INTEGER,
  latest_dispatch_id  TEXT,
  review_round        INTEGER DEFAULT 0,
  metadata            TEXT,               -- JSON (review_checklist, reward, etc.)
  created_at          DATETIME DEFAULT CURRENT_TIMESTAMP,
  updated_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 디스패치 (기존 PCD task_dispatches 유지)
CREATE TABLE task_dispatches (
  id                  TEXT PRIMARY KEY,
  kanban_card_id      TEXT REFERENCES kanban_cards(id),
  from_agent_id       TEXT,
  to_agent_id         TEXT,
  dispatch_type       TEXT,               -- implementation/review/test/rework
  status              TEXT DEFAULT 'pending',
  title               TEXT,
  context             TEXT,               -- JSON
  result              TEXT,               -- JSON
  parent_dispatch_id  TEXT,
  chain_depth         INTEGER DEFAULT 0,
  created_at          DATETIME DEFAULT CURRENT_TIMESTAMP,
  updated_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 세션 (기존 PCD dispatched_sessions 유지)
CREATE TABLE sessions (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  session_key         TEXT UNIQUE,         -- hostname:tmux-session
  agent_id            TEXT REFERENCES agents(id),
  provider            TEXT DEFAULT 'claude',
  status              TEXT DEFAULT 'disconnected',
  active_dispatch_id  TEXT,
  model               TEXT,
  session_info        TEXT,
  tokens              INTEGER DEFAULT 0,
  cwd                 TEXT,
  last_heartbeat      DATETIME,
  created_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 회의 (기존 PCD meetings 확장)
CREATE TABLE meetings (
  id                  TEXT PRIMARY KEY,
  channel_id          TEXT,
  title               TEXT,
  status              TEXT,                -- in_progress/completed/cancelled
  effective_rounds    INTEGER,
  started_at          DATETIME,
  completed_at        DATETIME,
  summary             TEXT
);

CREATE TABLE meeting_transcripts (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  meeting_id          TEXT REFERENCES meetings(id),
  seq                 INTEGER,
  round               INTEGER,
  speaker_agent_id    TEXT,
  speaker_name        TEXT,
  content             TEXT,
  is_summary          BOOLEAN DEFAULT FALSE
);

-- GitHub 레포 등록
CREATE TABLE github_repos (
  id                  TEXT PRIMARY KEY,    -- owner/repo
  display_name        TEXT,
  sync_enabled        BOOLEAN DEFAULT TRUE,
  last_synced_at      DATETIME
);

-- 디스패치 큐 (auto-queue)
CREATE TABLE dispatch_queue (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  kanban_card_id      TEXT REFERENCES kanban_cards(id),
  priority_score      REAL,
  queued_at           DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 파이프라인 스테이지
CREATE TABLE pipeline_stages (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  repo_id             TEXT,
  stage_name          TEXT,
  stage_order         INTEGER,
  trigger_after       TEXT,                -- review_pass, stage_X_pass
  entry_skill         TEXT,
  timeout_minutes     INTEGER DEFAULT 60,
  on_failure          TEXT DEFAULT 'fail', -- fail/retry/goto
  skip_condition      TEXT                 -- JSON
);

-- 스킬
CREATE TABLE skills (
  id                  TEXT PRIMARY KEY,
  name                TEXT,
  description         TEXT,
  source_path         TEXT,
  trigger_patterns    TEXT,                -- JSON array
  updated_at          DATETIME
);

CREATE TABLE skill_usage (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  skill_id            TEXT,
  agent_id            TEXT,
  session_key         TEXT,
  used_at             DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 메시지 (채팅)
CREATE TABLE messages (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  sender_type         TEXT,                -- ceo/agent/system
  sender_id           TEXT,
  receiver_type       TEXT,
  receiver_id         TEXT,
  content             TEXT,
  message_type        TEXT DEFAULT 'chat',
  created_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- 오피스
CREATE TABLE offices (
  id                  TEXT PRIMARY KEY,
  name                TEXT,
  layout              TEXT                 -- JSON
);

CREATE TABLE departments (
  id                  TEXT PRIMARY KEY,
  name                TEXT,
  office_id           TEXT REFERENCES offices(id)
);

-- KV 메타 (설정, 마이그레이션 트래킹)
CREATE TABLE kv_meta (
  key                 TEXT PRIMARY KEY,
  value               TEXT
);

-- 리뷰 결정
CREATE TABLE review_decisions (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  kanban_card_id      TEXT REFERENCES kanban_cards(id),
  dispatch_id         TEXT,
  item_index          INTEGER,
  decision            TEXT,                -- accept/reject
  decided_at          DATETIME DEFAULT CURRENT_TIMESTAMP
);

-- Rate limit 캐시
CREATE TABLE rate_limit_cache (
  provider            TEXT PRIMARY KEY,
  data                TEXT,                -- JSON
  fetched_at          INTEGER
);
```

---

## 설정 파일

```yaml
# agentdesk.yaml — 단일 설정 파일
server:
  port: 8791
  host: "0.0.0.0"
  auth_token: "your-secret-token"

discord:
  bots:
    command:
      token: "MTQ3OT..."
      description: "에이전트에게 명령 전달"
    notify:
      token: "MTQ4MT..."
      description: "정보 알림 전용"
  guild_id: "1234567890"

agents:
  - id: ch-td
    name: "TD"
    name_ko: "테크니컬 디렉터"
    provider: claude
    channels:
      claude: "td-cc"
      codex: "td-cdx"
    department: engineering
    avatar_emoji: "🔧"

  - id: ch-dd
    name: "DD"
    name_ko: "디자인 디렉터"
    provider: claude
    channels:
      claude: "dd-cc"
    department: design
    avatar_emoji: "🎨"

github:
  repos:
    - "owner/repo-a"
    - "owner/repo-b"
  sync_interval_minutes: 10
  triage_interval_minutes: 5

memory:
  backend: auto
  file:
    sak_path: "memories/shared-agent-knowledge/shared_knowledge.md"
    sam_path: "memories/shared-agent-memory"
    ltm_root: "memories/long-term"
    auto_memory_root: "~/.claude/projects/*{workspace}*/memory/"
  mcp:
    endpoint: "http://127.0.0.1:8765"
    access_key_env: "MEMENTO_API_KEY"

policies:
  dir: "./policies"              # JS 정책 파일 디렉토리
  hot_reload: true               # 파일 변경 시 자동 리로드

data:
  dir: "~/.adk"                 # DB, 로그, 캐시 저장소
  db_name: "agentdesk.sqlite"

kanban:
  timeout_requested_minutes: 45
  timeout_in_progress_minutes: 100
  max_review_rounds: 3

auto_queue:
  enabled: true
  dod_timeout_minutes: 15

rate_limits:
  poll_interval_seconds: 120
  warning_percent: 60
  danger_percent: 85
```

### 설정 정본(surface) 구분

- `kv_meta['settings']`
  - 회사 설정 JSON 정본
  - `/api/settings`는 patch merge가 아니라 full replace 계약
  - 저장 시 legacy key는 서버에서 제거
- `kv_meta['runtime-config']`
  - 폴링 주기, cache TTL 같은 즉시 반영 숫자 설정
  - hardcoded default < `agentdesk.yaml runtime:` < `kv_meta['runtime-config']` override 순서로 해석
  - `/api/settings/runtime-config`는 `current`와 `defaults`를 함께 돌려준다
- 개별 `kv_meta` 키
  - 리뷰, 타임아웃, context compact, merge automation, Discord 채널 ID
  - `/api/settings/config` whitelist를 통해서만 노출/수정
  - 응답에는 `baseline`, `override_active`, `editable`, `restart_behavior`가 포함되어 baseline과 live override를 구분한다
  - YAML-backed key는 재시작 시 YAML baseline이 다시 seed되고, hardcoded-only key는 reset flag가 꺼져 있으면 기존 override를 유지한다
- `kv_meta['escalation-settings-override']`
  - PM/owner escalation 라우팅용 전용 override surface
  - baseline은 `escalation:` + `discord.owner_id` + `kanban.manager_channel_id`
  - `/api/settings/escalation`은 `current`와 `defaults`를 함께 돌려준다
- 온보딩 전용 key/API
  - 토큰, provider, 초기 Discord 설정
  - 일반 settings form이 아니라 `/api/onboarding/*`와 wizard가 정본

세부 계약은 `docs/adr-settings-precedence.md`를 기준으로 유지한다.

---

## 디렉토리 구조

```
AgentDesk/
├── Cargo.toml
├── Cargo.lock
├── build.rs                     # 프론트엔드 빌드 + 임베딩
│
├── src/
│   ├── main.rs                  # 엔트리포인트, 모듈 조합
│   ├── config.rs                # agentdesk.yaml 파싱
│   │
│   ├── db/
│   │   ├── mod.rs
│   │   ├── schema.rs            # 테이블 + 마이그레이션
│   │   └── dao.rs               # 공통 쿼리 함수
│   │
│   ├── discord/
│   │   ├── mod.rs
│   │   ├── gateway.rs           # serenity 봇 (← RCC)
│   │   ├── router.rs            # 메시지 라우팅 (← RCC)
│   │   ├── turn_bridge.rs       # 턴 관리 (← RCC)
│   │   ├── meeting.rs           # 라운드테이블 (← RCC)
│   │   └── multi_bot.rs         # 다중 봇 관리 (← RCC)
│   │
│   ├── session/
│   │   ├── mod.rs
│   │   ├── tmux.rs              # tmux 생명주기 (← RCC)
│   │   ├── tracker.rs           # 세션 상태 추적 (← PCD)
│   │   └── agent_link.rs        # 세션↔에이전트 매핑 (← PCD)
│   │
│   ├── server/
│   │   ├── mod.rs
│   │   ├── http.rs              # axum HTTP 서버
│   │   ├── ws.rs                # WebSocket 브로드캐스트
│   │   ├── auth.rs              # 인증
│   │   └── routes/
│   │       ├── agents.rs
│   │       ├── kanban.rs
│   │       ├── dispatches.rs
│   │       ├── sessions.rs
│   │       ├── github.rs
│   │       ├── meetings.rs
│   │       ├── settings.rs
│   │       ├── discord_proxy.rs
│   │       ├── rate_limits.rs
│   │       └── health.rs
│   │
│   ├── engine/
│   │   ├── mod.rs
│   │   ├── runtime.rs           # QuickJS 초기화
│   │   ├── ops.rs               # Rust↔JS 브릿지 (~30 ops)
│   │   ├── loader.rs            # policies/ 핫 리로드
│   │   └── hooks.rs             # 라이프사이클 훅 정의
│   │
│   ├── dispatch/
│   │   ├── mod.rs
│   │   ├── executor.rs          # 생성 + 라우팅 (← PCD)
│   │   ├── result.rs            # 결과 처리 (← PCD)
│   │   └── chain.rs             # auto-chaining (← PCD)
│   │
│   └── github/
│       ├── mod.rs
│       ├── sync.rs              # issue 동기화 (← PCD)
│       ├── triage.rs            # 자동 분류 (← PCD)
│       └── dod.rs               # DoD 미러링 (← PCD)
│
├── policies/                    # 기본 정책 (JS, 핫 리로드)
│   ├── kanban-rules.js
│   ├── review-policy.js
│   ├── auto-queue.js
│   ├── pipeline.js
│   ├── triage-rules.js
│   ├── reward-policy.js
│   └── timeout-policy.js
│
├── dashboard/                   # React 프론트엔드
│   ├── src/                     # (← PCD src/ 이관)
│   ├── package.json
│   ├── vite.config.ts
│   └── index.html
│
├── migrations/
│   ├── 001_initial.sql
│   └── ...
│
└── scripts/
    ├── migrate-from-rcc-pcd.ts  # 레거시 데이터 이관
    └── install.sh               # curl 기반 설치 스크립트
```

---

## 데이터 이관 전략

### Phase 1: DB 통합
```
PCD SQLite (agents, kanban_cards, task_dispatches, sessions, ...)
    + RCC org.yaml (agent 정의)
    + RCC role_map.json (채널 매핑)
    + PCD .env (봇 토큰)
    → AgentDesk SQLite + agentdesk.yaml
```

### Phase 2: 이관 스크립트
1. PCD SQLite 테이블 → AgentDesk 스키마로 매핑 (대부분 1:1)
2. org.yaml agents → agentdesk.yaml agents 섹션
3. role_map.json channels → agents[].channels 필드
4. .env 토큰 → agentdesk.yaml discord.bots 섹션
5. rate-limit-cache.json → rate_limit_cache 테이블

### Phase 3: 정책 이관
```
PCD kanban-dispatch.ts    → policies/kanban-rules.js
PCD kanban-review.ts      → policies/review-policy.js
PCD kanban-timeouts.ts    → policies/timeout-policy.js
PCD auto-queue.ts         → policies/auto-queue.js
PCD pipeline.ts           → policies/pipeline.js
PCD issue-triage.ts       → policies/triage-rules.js
PCD kanban-crud.ts reward → policies/reward-policy.js
```

---
