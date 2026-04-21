import { expect, test, type Page } from "@playwright/test";

const ROUTES = [
  { path: "/home", label: /홈|Home/ },
  { path: "/office", label: /오피스|Office/ },
  { path: "/agents", label: /에이전트|Agents/ },
  { path: "/kanban", label: /칸반|Kanban/ },
  { path: "/stats", label: /통계|Stats/ },
  { path: "/ops", label: /운영|Ops/ },
  { path: "/meetings", label: /회의|Meetings/ },
  { path: "/achievements", label: /업적|Achievements/ },
  { path: "/settings", label: /설정|Settings/ },
];

const DEFAULT_HOME_WIDGET_ORDER = [
  "m_tokens",
  "m_cost",
  "m_progress",
  "m_streak",
  "office",
  "missions",
  "roster",
  "activity",
  "kanban",
];
const CUSTOM_HOME_WIDGET_ORDER = [
  "kanban",
  "activity",
  "roster",
  "missions",
  "office",
  "m_streak",
  "m_progress",
  "m_cost",
  "m_tokens",
];
const DRAGGED_HOME_WIDGET_ORDER = [
  "m_tokens",
  "m_cost",
  "m_progress",
  "m_streak",
  "missions",
  "office",
  "roster",
  "activity",
  "kanban",
];

async function expectNoHorizontalOverflow(page: Page) {
  const metrics = await page.evaluate(() => ({
    viewportWidth: window.innerWidth,
    bodyScrollWidth: document.body.scrollWidth,
    rootScrollWidth: document.documentElement.scrollWidth,
  }));

  expect(
    Math.max(metrics.bodyScrollWidth, metrics.rootScrollWidth),
  ).toBeLessThanOrEqual(metrics.viewportWidth + 1);
}

async function getHomeWidgetOrder(page: Page) {
  return page.locator('[data-testid^="home-widget-"]').evaluateAll((elements) =>
    elements
      .map((element) => element.getAttribute("data-testid") ?? "")
      .filter(Boolean)
      .map((value) => value.replace("home-widget-", "")),
  );
}

const MOCK_MEETINGS = [
  {
    id: "meeting-smoke-1",
    agenda: "로드밸런서 경고 플로우 리뷰",
    summary:
      "경고 bubble 노출 기준과 모바일 disclosure 상호작용을 정리했습니다.",
    selection_reason: "Office 경고 플로우 확인 필요",
    status: "completed",
    primary_provider: "claude",
    reviewer_provider: "codex",
    participant_names: ["adk-dashboard", "project-agentdesk"],
    total_rounds: 2,
    issues_created: 1,
    proposed_issues: [
      {
        title: "Fix office warning disclosure copy",
        body: "Adjust copy and add coverage.",
        assignee: "adk-dashboard",
      },
    ],
    issue_creation_results: null,
    issue_repo: "itismyfield/AgentDesk",
    started_at: 1760918400000,
    completed_at: 1760919300000,
    created_at: 1760918400000,
    entries: [],
  },
  {
    id: "meeting-smoke-2",
    agenda: "Meetings hub read-only QA",
    summary: "회의 상세 drawer와 스킬 카탈로그 병행 노출을 점검했습니다.",
    selection_reason: null,
    status: "in_progress",
    primary_provider: "codex",
    reviewer_provider: "gemini",
    participant_names: ["adk-dashboard"],
    total_rounds: 1,
    issues_created: 0,
    proposed_issues: null,
    issue_creation_results: null,
    issue_repo: null,
    started_at: 1760922000000,
    completed_at: null,
    created_at: 1760922000000,
    entries: [],
  },
];

const MOCK_MEETING_DETAIL = {
  ...MOCK_MEETINGS[0],
  meeting_hash: "abcd1234",
  thread_hash: "efgh5678",
  entries: [
    {
      id: 1,
      meeting_id: "meeting-smoke-1",
      seq: 1,
      round: 1,
      speaker_role_id: "adk-dashboard",
      speaker_name: "adk-dashboard",
      content: "모바일에서 warning disclosure는 별도 버튼으로 분리합니다.",
      is_summary: 0,
      created_at: 1760918460000,
    },
    {
      id: 2,
      meeting_id: "meeting-smoke-1",
      seq: 2,
      round: 1,
      speaker_role_id: "project-agentdesk",
      speaker_name: "project-agentdesk",
      content: "hover/focus desktop bubble은 유지하고 tap persistent를 허용합니다.",
      is_summary: 0,
      created_at: 1760918520000,
    },
  ],
};

const MOCK_SKILLS = [
  {
    name: "office-warning-disclosure",
    description: "Office warning disclosure patterns",
    description_ko: "Office 경고 disclosure 패턴",
    total_calls: 12,
    last_used_at: 1760919000000,
  },
  {
    name: "meetings-readonly-hub",
    description: "Read-only meetings + skills layout",
    description_ko: "읽기 전용 회의 + 스킬 허브 레이아웃",
    total_calls: 7,
    last_used_at: 1760919300000,
  },
];

const MOCK_OPS_HEALTH_BASE = {
  status: "degraded",
  uptime_secs: 14_400,
  global_active: 3,
  global_finalizing: 1,
  deferred_hooks: 4,
  queue_depth: 5,
  watcher_count: 6,
  recovery_duration: 240,
  outbox_age: 75,
  degraded_reasons: ["dispatch_outbox_oldest_pending_age:75"],
  dispatch_outbox: {
    pending: 7,
    retrying: 2,
    permanent_failures: 1,
    oldest_pending_age: 75,
  },
  providers: [
    {
      name: "codex",
      connected: true,
      active_turns: 2,
      queue_depth: 1,
      sessions: 3,
      restart_pending: false,
      last_turn_at: "2026-04-20T03:15:00Z",
    },
    {
      name: "claude",
      connected: false,
      active_turns: 1,
      queue_depth: 2,
      sessions: 2,
      restart_pending: true,
      last_turn_at: "2026-04-20T03:12:00Z",
    },
  ],
};

const AGENTS_HUB_NOW = 1760918400000;
const MOCK_OFFICES = [
  {
    id: "office-agentdesk",
    name: "AgentDesk",
    name_ko: "에이전트데스크",
    icon: "🏢",
    color: "#38bdf8",
    description: "AgentDesk release office",
    sort_order: 0,
    created_at: AGENTS_HUB_NOW - 604_800_000,
    agent_count: 2,
    department_count: 2,
  },
];

const MOCK_DASHBOARD_STATS = {
  agents: {
    total: 2,
    working: 1,
    idle: 1,
    break: 0,
    offline: 0,
  },
  top_agents: [
    {
      id: "agent-ada",
      name: "Ada Dashboard",
      alias: "ada",
      name_ko: "아다 대시보드",
      avatar_emoji: "🛠️",
      stats_tasks_done: 48,
      stats_xp: 1620,
      stats_tokens: 124000,
    },
  ],
  departments: [
    {
      id: "dept-platform",
      name: "Platform",
      name_ko: "플랫폼",
      icon: "🧱",
      color: "#38bdf8",
      total_agents: 1,
      working_agents: 1,
      sum_xp: 1620,
    },
    {
      id: "dept-product",
      name: "Product",
      name_ko: "프로덕트",
      icon: "🧭",
      color: "#f97316",
      total_agents: 1,
      working_agents: 0,
      sum_xp: 980,
    },
  ],
  dispatched_count: 1,
  github_closed_today: 3,
  kanban: {
    open_total: 6,
    review_queue: 1,
    blocked: 1,
    failed: 0,
    waiting_acceptance: 1,
    stale_in_progress: 0,
    by_status: {
      backlog: 1,
      ready: 1,
      requested: 1,
      blocked: 1,
      in_progress: 1,
      review: 1,
      done: 0,
      qa_pending: 0,
      qa_in_progress: 0,
      qa_failed: 0,
    },
    top_repos: [
      {
        github_repo: "itismyfield/AgentDesk",
        open_count: 6,
        pressure_count: 2,
      },
    ],
  },
};

function buildMockTokenAnalytics(period: "7d" | "30d" | "90d" = "30d") {
  const days = period === "7d" ? 7 : period === "90d" ? 90 : 30;
  const periodLabel = period === "7d" ? "Last 7 days" : period === "90d" ? "Last 90 days" : "Last 30 days";
  const daily = Array.from({ length: Math.min(days, 7) }, (_, index) => {
    const date = new Date(AGENTS_HUB_NOW - (6 - index) * 86_400_000);
    const isoDate = date.toISOString().slice(0, 10);
    const inputTokens = 1200 + index * 120;
    const outputTokens = 700 + index * 80;
    const cacheReadTokens = 320 + index * 25;
    const cacheCreationTokens = 110 + index * 10;
    const totalTokens =
      inputTokens + outputTokens + cacheReadTokens + cacheCreationTokens;
    return {
      date: isoDate,
      input_tokens: inputTokens,
      output_tokens: outputTokens,
      cache_read_tokens: cacheReadTokens,
      cache_creation_tokens: cacheCreationTokens,
      total_tokens: totalTokens,
      cost: Number((0.18 + index * 0.01).toFixed(3)),
    };
  });

  const totalTokens = daily.reduce((sum, row) => sum + row.total_tokens, 0);
  const totalCost = daily.reduce((sum, row) => sum + row.cost, 0);
  const cacheDiscount = Number((totalCost * 0.22).toFixed(3));

  return {
    period,
    period_label: periodLabel,
    days,
    generated_at: new Date(AGENTS_HUB_NOW).toISOString(),
    summary: {
      total_tokens: totalTokens,
      total_cost: Number(totalCost.toFixed(3)),
      cache_discount: cacheDiscount,
      total_messages: 42,
      total_sessions: 9,
      active_days: daily.length,
      average_daily_tokens: Math.round(totalTokens / daily.length),
      peak_day: {
        date: daily[daily.length - 1]?.date ?? "2026-04-20",
        total_tokens: daily[daily.length - 1]?.total_tokens ?? totalTokens,
        cost: daily[daily.length - 1]?.cost ?? Number(totalCost.toFixed(3)),
      },
    },
    receipt: {
      period_label: periodLabel,
      period_start: daily[0]?.date ?? "2026-04-14",
      period_end: daily[daily.length - 1]?.date ?? "2026-04-20",
      models: [
        {
          model: "gpt-5.4",
          display_name: "GPT-5.4",
          input_tokens: 5400,
          output_tokens: 3200,
          cache_read_tokens: 1400,
          cache_creation_tokens: 520,
          total_tokens: 10520,
          cost: 1.48,
          cost_without_cache: 1.86,
          provider: "openai",
        },
        {
          model: "claude-4.1-opus",
          display_name: "Claude 4.1 Opus",
          input_tokens: 3600,
          output_tokens: 2100,
          cache_read_tokens: 780,
          cache_creation_tokens: 260,
          total_tokens: 6740,
          cost: 0.94,
          cost_without_cache: 1.17,
          provider: "anthropic",
        },
      ],
      subtotal: Number((totalCost + cacheDiscount).toFixed(3)),
      cache_discount: cacheDiscount,
      total: Number(totalCost.toFixed(3)),
      stats: {
        total_messages: 42,
        total_sessions: 9,
      },
      providers: [
        { provider: "openai", tokens: 10520, percentage: 61 },
        { provider: "anthropic", tokens: 6740, percentage: 39 },
      ],
      agents: [
        {
          agent: "adk-dashboard",
          tokens: 9700,
          cost: 1.31,
          cost_without_cache: 1.61,
          input_tokens: 4800,
          cache_read_tokens: 1240,
          cache_creation_tokens: 430,
          percentage: 56,
        },
        {
          agent: "project-agentdesk",
          tokens: 7560,
          cost: 1.11,
          cost_without_cache: 1.42,
          input_tokens: 4200,
          cache_read_tokens: 940,
          cache_creation_tokens: 350,
          percentage: 44,
        },
      ],
    },
    daily,
    heatmap: daily.map((row, index) => ({
      date: row.date,
      week_index: Math.floor(index / 7),
      weekday: index % 7,
      total_tokens: row.total_tokens,
      cost: row.cost,
      level: Math.min(4, 1 + Math.floor(index / 2)),
      future: false,
    })),
  };
}

function buildMockSkillRanking(window: "7d" | "30d" | "90d" | "all" = "30d") {
  return {
    ...MOCK_AGENT_SKILL_RANKING,
    window,
    overall: [
      {
        skill_name: "office-warning-disclosure",
        skill_desc_ko: "Office warning disclosure patterns",
        calls: window === "7d" ? 4 : window === "90d" ? 19 : 12,
        last_used_at: AGENTS_HUB_NOW - 3_600_000,
      },
      {
        skill_name: "meetings-readonly-hub",
        skill_desc_ko: "Read-only meetings + skills layout",
        calls: window === "7d" ? 2 : window === "90d" ? 11 : 7,
        last_used_at: AGENTS_HUB_NOW - 7_200_000,
      },
    ],
  };
}

const MOCK_AGENT_DEPARTMENTS = [
  {
    id: "dept-platform",
    name: "Platform",
    name_ko: "플랫폼",
    name_ja: null,
    name_zh: null,
    icon: "🧱",
    color: "#38bdf8",
    description: "플랫폼 자동화와 파이프라인을 담당합니다.",
    prompt: null,
    office_id: "office-agentdesk",
    sort_order: 0,
    created_at: AGENTS_HUB_NOW - 86_400_000,
  },
  {
    id: "dept-product",
    name: "Product",
    name_ko: "프로덕트",
    name_ja: null,
    name_zh: null,
    icon: "🧭",
    color: "#f97316",
    description: "운영 UI와 backlog triage를 담당합니다.",
    prompt: null,
    office_id: "office-agentdesk",
    sort_order: 1,
    created_at: AGENTS_HUB_NOW - 86_400_000,
  },
];

const MOCK_AGENTS = [
  {
    id: "agent-ada",
    role_id: "adk-dashboard",
    name: "Ada Dashboard",
    alias: "ada",
    name_ko: "아다 대시보드",
    name_ja: null,
    name_zh: null,
    department_id: "dept-platform",
    cli_provider: "codex",
    avatar_emoji: "🛠️",
    sprite_number: 12,
    personality: "회의 허브와 운영 화면을 정리 중입니다.",
    status: "working",
    current_task_id: "#786",
    workflow_pack_key: "development",
    stats_tasks_done: 48,
    stats_xp: 1620,
    stats_tokens: 124000,
    activity_source: "agentdesk",
    created_at: AGENTS_HUB_NOW - 604_800_000,
  },
  {
    id: "agent-luna",
    role_id: "project-agentdesk",
    name: "Luna Ops",
    alias: null,
    name_ko: "루나 옵스",
    name_ja: null,
    name_zh: null,
    department_id: "dept-product",
    cli_provider: "claude",
    avatar_emoji: "🌙",
    sprite_number: 3,
    personality: "운영 대시보드와 백로그 정합성을 확인합니다.",
    status: "idle",
    current_task_id: null,
    workflow_pack_key: "development",
    stats_tasks_done: 31,
    stats_xp: 980,
    stats_tokens: 88000,
    activity_source: "idle",
    created_at: AGENTS_HUB_NOW - 518_400_000,
  },
];

const MOCK_AGENT_SKILL_RANKING = {
  window: "30d",
  overall: [],
  byAgent: [
    {
      agent_role_id: "adk-dashboard",
      agent_name: "Ada Dashboard",
      skill_name: "agents-hub",
      skill_desc_ko: "Agents Hub",
      calls: 14,
      last_used_at: AGENTS_HUB_NOW - 3_600_000,
    },
    {
      agent_role_id: "adk-dashboard",
      agent_name: "Ada Dashboard",
      skill_name: "pixi-office",
      skill_desc_ko: "Pixi Office",
      calls: 9,
      last_used_at: AGENTS_HUB_NOW - 7_200_000,
    },
    {
      agent_role_id: "project-agentdesk",
      agent_name: "Luna Ops",
      skill_name: "backlog-triage",
      skill_desc_ko: "Backlog Triage",
      calls: 11,
      last_used_at: AGENTS_HUB_NOW - 5_400_000,
    },
  ],
};

const MOCK_BACKLOG_CARDS = [
  {
    id: "card-786-1",
    title: "Agents hub smoke regression",
    description: "3탭 구조와 drawer drill-in을 회귀 테스트로 고정합니다.",
    status: "in_progress",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: "agent-ada",
    requester_agent_id: "agent-luna",
    assignee_agent_id: "agent-ada",
    parent_card_id: null,
    latest_dispatch_id: null,
    sort_order: 0,
    priority: "high",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 786,
    github_issue_url: "https://github.com/itismyfield/AgentDesk/issues/786",
    metadata: null,
    metadata_json: JSON.stringify({ summary: "agents smoke coverage" }),
    pipeline_stage_id: "implementation",
    review_status: null,
    created_at: AGENTS_HUB_NOW - 172_800_000,
    updated_at: AGENTS_HUB_NOW - 1_800_000,
    started_at: AGENTS_HUB_NOW - 86_400_000,
    requested_at: AGENTS_HUB_NOW - 172_800_000,
    review_entered_at: null,
    completed_at: null,
    latest_dispatch_status: null,
    latest_dispatch_title: null,
    latest_dispatch_type: null,
    latest_dispatch_result_summary: null,
    latest_dispatch_chain_depth: null,
    child_count: 0,
  },
  {
    id: "card-786-2",
    title: "Office warning disclosure polish",
    description: "모바일 warning disclosure copy를 미세 조정합니다.",
    status: "review",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: "agent-luna",
    requester_agent_id: "agent-ada",
    assignee_agent_id: "agent-luna",
    parent_card_id: null,
    latest_dispatch_id: null,
    sort_order: 1,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 661,
    github_issue_url: "https://github.com/itismyfield/AgentDesk/issues/661",
    metadata: null,
    metadata_json: JSON.stringify({ summary: "office warning disclosure" }),
    pipeline_stage_id: "review",
    review_status: null,
    created_at: AGENTS_HUB_NOW - 259_200_000,
    updated_at: AGENTS_HUB_NOW - 5_400_000,
    started_at: AGENTS_HUB_NOW - 172_800_000,
    requested_at: AGENTS_HUB_NOW - 259_200_000,
    review_entered_at: AGENTS_HUB_NOW - 21_600_000,
    completed_at: null,
    latest_dispatch_status: null,
    latest_dispatch_title: null,
    latest_dispatch_type: null,
    latest_dispatch_result_summary: null,
    latest_dispatch_chain_depth: null,
    child_count: 0,
  },
  {
    id: "card-779-1",
    title: "Kanban failed lane audit visibility",
    description: "실패 lane에서 dispatch/audit/comment trace를 한 번에 검증합니다.",
    status: "qa_failed",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: "agent-ada",
    requester_agent_id: "agent-luna",
    assignee_agent_id: "agent-ada",
    parent_card_id: null,
    latest_dispatch_id: "dispatch-779-1",
    sort_order: 2,
    priority: "urgent",
    depth: 0,
    blocked_reason: "Review hook timed out while collecting evidence.",
    review_notes: null,
    github_issue_number: 779,
    github_issue_url: "https://github.com/itismyfield/AgentDesk/issues/779",
    metadata: null,
    metadata_json: JSON.stringify({
      summary: "kanban failed lane trace",
      timed_out_reason: "Review hook timed out while collecting failure evidence.",
    }),
    pipeline_stage_id: "qa",
    review_status: null,
    created_at: AGENTS_HUB_NOW - 86_400_000,
    updated_at: AGENTS_HUB_NOW - 900_000,
    started_at: AGENTS_HUB_NOW - 43_200_000,
    requested_at: AGENTS_HUB_NOW - 86_400_000,
    review_entered_at: AGENTS_HUB_NOW - 7_200_000,
    completed_at: null,
    latest_dispatch_status: "failed",
    latest_dispatch_title: "Review evidence sweep",
    latest_dispatch_type: "review",
    latest_dispatch_result_summary: "pipeline hook timed out after evidence capture step",
    latest_dispatch_chain_depth: 1,
    child_count: 0,
  },
];

const MOCK_KANBAN_REPO_SOURCES = [
  {
    id: "repo-agentdesk",
    repo: "itismyfield/AgentDesk",
    default_agent_id: "agent-ada",
    pipeline_config: {
      hooks: {
        qa_failed: {
          on_enter: ["capture_failure_bundle", "notify_ops"],
          on_exit: ["clear_failure_alert"],
        },
      },
    },
    created_at: AGENTS_HUB_NOW - 259_200_000,
  },
];

const MOCK_KANBAN_DISPATCHES = [
  {
    id: "dispatch-779-1",
    kanban_card_id: "card-779-1",
    from_agent_id: "agent-luna",
    to_agent_id: "agent-ada",
    dispatch_type: "review",
    status: "failed",
    title: "Review evidence sweep",
    context_file: null,
    result_file: null,
    result_summary: "Trace bundle capture timed out after review artifact upload.",
    parent_dispatch_id: null,
    chain_depth: 1,
    created_at: AGENTS_HUB_NOW - 7_200_000,
    dispatched_at: AGENTS_HUB_NOW - 7_100_000,
    completed_at: AGENTS_HUB_NOW - 6_900_000,
  },
];

const MOCK_KANBAN_AUDIT_LOGS = {
  "card-779-1": [
    {
      id: 1,
      card_id: "card-779-1",
      from_status: "qa_in_progress",
      to_status: "qa_failed",
      source: "playwright-smoke",
      result: "Failure evidence attached; manual review required.",
      created_at: new Date(AGENTS_HUB_NOW - 6_800_000).toISOString(),
    },
  ],
};

const MOCK_KANBAN_COMMENTS = {
  "card-779-1": {
    body: "Failed lane smoke context",
    comments: [
      {
        author: { login: "adk-dashboard" },
        body: "Collected review logs and attached the failed trace bundle.",
        createdAt: new Date(AGENTS_HUB_NOW - 6_700_000).toISOString(),
      },
    ],
  },
};

const MOCK_GITHUB_ISSUES = [
  {
    number: 779,
    title: "Kanban failed lane fidelity follow-up",
    body: "Keep the failed lane and trace surfaces visible in smoke.",
    state: "open",
    url: "https://github.com/itismyfield/AgentDesk/issues/779",
    labels: [{ name: "adk-dashboard", color: "2563eb" }],
    assignees: [{ login: "itismyfield" }],
    createdAt: new Date(AGENTS_HUB_NOW - 172_800_000).toISOString(),
    updatedAt: new Date(AGENTS_HUB_NOW - 3_600_000).toISOString(),
  },
];

const MOCK_ACHIEVEMENTS = [
  {
    id: "achievement-first-task-ada",
    agent_id: "agent-ada",
    type: "first_task",
    name: "첫 번째 태스크",
    description: "첫 번째 dashboard milestone unlock",
    earned_at: AGENTS_HUB_NOW - 259_200_000,
    agent_name: "Ada Dashboard",
    agent_name_ko: "에이다 대시보드",
    avatar_emoji: "📊",
    rarity: "rare",
    progress: null,
  },
];

const MOCK_STREAKS = [
  {
    agent_id: "agent-ada",
    name: "Ada Dashboard",
    avatar_emoji: "📊",
    streak: 12,
    last_active: "2026-04-21",
  },
];

const MOCK_DAILY_MISSIONS = [
  {
    id: "dispatches_today",
    label: "Complete 5 dispatches today",
    current: 3,
    target: 5,
    completed: false,
  },
  {
    id: "active_agents_today",
    label: "Get 3 agents shipping today",
    current: 2,
    target: 3,
    completed: false,
  },
  {
    id: "review_queue_zero",
    label: "Drain the review queue",
    current: 0,
    target: 1,
    completed: true,
  },
];

async function mockMeetingsHubApis(page: Page) {
  await page.route(/\/api\/round-table-meetings\/channels$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ channels: [] }),
    });
  });

  await page.route(/\/api\/github-repos$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ viewer_login: "", repos: [] }),
    });
  });

  await page.route(/\/api\/skills\/catalog$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ catalog: MOCK_SKILLS }),
    });
  });

  await page.route(/\/api\/round-table-meetings\/meeting-smoke-1$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ meeting: MOCK_MEETING_DETAIL }),
    });
  });

  await page.route(/\/api\/round-table-meetings$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ meetings: MOCK_MEETINGS }),
    });
  });
}

async function mockAgentsHubApis(page: Page) {
  await page.route(/\/api\/agents\/[^/]+\/cron$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ jobs: [] }),
    });
  });

  await page.route(/\/api\/agents\/[^/]+\/skills$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ skills: [], sharedSkills: [], totalCount: 0 }),
    });
  });

  await page.route(/\/api\/agents\/[^/]+\/dispatched-sessions$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ sessions: [] }),
    });
  });

  await page.route(/\/api\/agents\/[^/]+\/offices$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ offices: [] }),
    });
  });

  await page.route(/\/api\/agents\/[^/]+\/timeline\?limit=\d+$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ events: [] }),
    });
  });

  await page.route(/\/api\/discord-bindings$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ bindings: [] }),
    });
  });

  await page.route(/\/api\/skills\/ranking\?window=30d&limit=120$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(MOCK_AGENT_SKILL_RANKING),
    });
  });

  await page.route(/\/api\/kanban-cards$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ cards: MOCK_BACKLOG_CARDS }),
    });
  });

  await page.route(/\/api\/departments(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ departments: MOCK_AGENT_DEPARTMENTS }),
    });
  });

  await page.route(/\/api\/agents(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ agents: MOCK_AGENTS }),
    });
  });
}

async function mockOpsHealthApi(page: Page, getPayload: () => Record<string, unknown>) {
  await page.route(/\/api\/health$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(getPayload()),
    });
  });
}

async function mockDashboardBootstrap(page: Page) {
  await page.addInitScript(() => {
    class MockWebSocket {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSING = 2;
      static readonly CLOSED = 3;

      url: string;
      readyState = MockWebSocket.CONNECTING;
      onopen: ((event: Event) => void) | null = null;
      onmessage: ((event: MessageEvent) => void) | null = null;
      onerror: ((event: Event) => void) | null = null;
      onclose: ((event: Event) => void) | null = null;

      constructor(url: string) {
        this.url = url;
        setTimeout(() => {
          this.readyState = MockWebSocket.OPEN;
          this.onopen?.(new Event("open"));
        }, 0);
      }

      send() {}

      close() {
        if (this.readyState === MockWebSocket.CLOSED) return;
        this.readyState = MockWebSocket.CLOSED;
        this.onclose?.(new Event("close"));
      }
    }

    Object.defineProperty(window, "WebSocket", {
      configurable: true,
      writable: true,
      value: MockWebSocket,
    });
  });

  await page.route(/\/api\/auth\/session$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ ok: true, csrf_token: "smoke-csrf-token" }),
    });
  });

  await page.route(/\/api\/offices$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ offices: MOCK_OFFICES }),
    });
  });

  await page.route(/\/api\/agents(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ agents: MOCK_AGENTS }),
    });
  });

  await page.route(/\/api\/departments(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ departments: MOCK_AGENT_DEPARTMENTS }),
    });
  });

  await page.route(/\/api\/dispatched-sessions(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ sessions: [] }),
    });
  });

  await page.route(/\/api\/stats(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(MOCK_DASHBOARD_STATS),
    });
  });

  await page.route(/\/api\/settings$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ language: "ko", theme: "dark" }),
    });
  });

  await page.route(/\/api\/audit-logs(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ logs: [] }),
    });
  });

  await page.route(/\/api\/dispatches(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ dispatches: MOCK_KANBAN_DISPATCHES }),
    });
  });

  await page.route(/\/api\/kanban-cards$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ cards: MOCK_BACKLOG_CARDS }),
    });
  });

  await page.route(/\/api\/kanban-repos$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ repos: MOCK_KANBAN_REPO_SOURCES }),
    });
  });

  await page.route(/\/api\/kanban-cards\/[^/]+\/audit-log$/, async (route) => {
    const cardId = route.request().url().match(/\/api\/kanban-cards\/([^/]+)\/audit-log$/)?.[1] ?? "";
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ logs: MOCK_KANBAN_AUDIT_LOGS[cardId as keyof typeof MOCK_KANBAN_AUDIT_LOGS] ?? [] }),
    });
  });

  await page.route(/\/api\/kanban-cards\/[^/]+\/comments$/, async (route) => {
    const cardId = route.request().url().match(/\/api\/kanban-cards\/([^/]+)\/comments$/)?.[1] ?? "";
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(MOCK_KANBAN_COMMENTS[cardId as keyof typeof MOCK_KANBAN_COMMENTS] ?? { comments: [], body: "" }),
    });
  });

  await page.route(/\/api\/kanban-cards\/[^/]+\/reviews$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ reviews: [] }),
    });
  });

  await page.route(/\/api\/github-issues(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ issues: MOCK_GITHUB_ISSUES, repo: "itismyfield/AgentDesk" }),
    });
  });

  await page.route(/\/api\/streaks$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ streaks: MOCK_STREAKS }),
    });
  });

  await page.route(/\/api\/v1\/achievements(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ achievements: MOCK_ACHIEVEMENTS, daily_missions: MOCK_DAILY_MISSIONS }),
    });
  });

  await page.route(/\/api\/skills\/catalog$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ catalog: MOCK_SKILLS }),
    });
  });

  await page.route(/\/api\/skills\/ranking(?:\?.*)?$/, async (route) => {
    const requestUrl = new URL(route.request().url());
    const window = (requestUrl.searchParams.get("window") ?? "30d") as
      | "7d"
      | "30d"
      | "90d"
      | "all";
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(buildMockSkillRanking(window)),
    });
  });

  await page.route(/\/api\/token-analytics(?:\?.*)?$/, async (route) => {
    const requestUrl = new URL(route.request().url());
    const period = (requestUrl.searchParams.get("period") ?? "30d") as
      | "7d"
      | "30d"
      | "90d";
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(buildMockTokenAnalytics(period)),
    });
  });

  await page.route(/\/api\/round-table-meetings\/channels$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ channels: [] }),
    });
  });

  await page.route(/\/api\/github-repos$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ viewer_login: "", repos: [] }),
    });
  });

  await page.route(/\/api\/round-table-meetings\/meeting-smoke-1$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ meeting: MOCK_MEETING_DETAIL }),
    });
  });

  await page.route(/\/api\/round-table-meetings$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ meetings: MOCK_MEETINGS }),
    });
  });

  await page.route(/\/api\/health$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(MOCK_OPS_HEALTH_BASE),
    });
  });
}

test.describe("Dashboard smoke tests", () => {
  test.beforeEach(async ({ page }) => {
    await mockDashboardBootstrap(page);
  });

  test("page loads and renders root element", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("#root")).toBeAttached();
    await expect(page.getByTestId("topbar")).toBeVisible();
  });

  test("theme: dark/light toggle changes CSS variables", async ({ page }) => {
    await page.goto("/");
    await page
      .getByRole("button", { name: /^디자인 설정 열기$|^Open tweaks$/ })
      .click();
    await page.getByRole("button", { name: /^다크$|^Dark$/ }).click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    const darkBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--th-bg-primary")
        .trim(),
    );
    expect(darkBg).toBeTruthy();

    await page.getByRole("button", { name: /^라이트$|^Light$/ }).click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    const lightBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--th-bg-primary")
        .trim(),
    );
    expect(lightBg).toBeTruthy();
    expect(lightBg).not.toBe(darkBg);
  });

  test("theme: auto mode responds to prefers-color-scheme", async ({ page }) => {
    await page.emulateMedia({ colorScheme: "dark" });
    await page.goto("/");
    await page
      .getByRole("button", { name: /^디자인 설정 열기$|^Open tweaks$/ })
      .click();
    await page.getByRole("button", { name: /^자동$|^Auto$/ }).click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

    await page.emulateMedia({ colorScheme: "light" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });

  test("desktop: sidebar renders at the full shell width", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    const sidebar = page.getByTestId("app-sidebar-nav");
    await expect(sidebar).toBeVisible();
    const box = await sidebar.boundingBox();
    expect(box?.width).toBeGreaterThan(230);
    expect(box?.width).toBeLessThan(250);
  });

  test("responsive: 900px switches from mobile tab bar to desktop sidebar", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");

    await page.setViewportSize({ width: 899, height: 900 });
    await page.goto("/home");

    await expect(page.getByTestId("app-mobile-tabbar")).toBeVisible();
    await expect(page.getByTestId("app-sidebar-nav")).toHaveCount(0);
    await expect(page.getByTestId("app-mobile-tabbar").locator("button")).toHaveCount(5);

    await page.setViewportSize({ width: 900, height: 900 });

    await expect(page.getByTestId("app-sidebar-nav")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tabbar")).toHaveCount(0);
  });

  test("responsive: mobile viewport shows 5-tab bar and opens the More menu", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    const bottomNav = page.getByTestId("app-mobile-tabbar");
    const topbar = page.getByTestId("topbar");
    await expect(bottomNav).toBeVisible();
    await expect(bottomNav.locator("button")).toHaveCount(5);
    await expect(page.getByTestId("app-mobile-tab-home")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tab-office")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tab-kanban")).toBeVisible();
    await expect(page.getByTestId("app-mobile-tab-stats")).toBeVisible();

    const tabbarZIndex = await bottomNav.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );
    const topbarZIndex = await topbar.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );
    expect(tabbarZIndex).toBeLessThan(50);
    expect(topbarZIndex).toBeLessThan(50);

    await page.getByTestId("app-mobile-more-button").click();

    const moreMenu = page.getByTestId("app-mobile-more-menu");
    await expect(moreMenu).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /에이전트|Agents/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /운영|Ops/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /회의|Meetings/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /업적|Achievements/ })).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /설정|Settings/ })).toBeVisible();

    const moreMenuBackdropZIndex = await moreMenu.evaluate((element) =>
      Number(window.getComputedStyle(element.parentElement as HTMLElement).zIndex),
    );
    expect(moreMenuBackdropZIndex).toBeGreaterThan(tabbarZIndex);

    const shellStyles = await page.getByTestId("app-main-scroll").evaluate((element) => ({
      marginBottom: (element as HTMLElement).style.marginBottom,
    }));
    expect(shellStyles.marginBottom).toContain("env(safe-area-inset-bottom)");

    const tabbarStyles = await bottomNav.evaluate((element) => ({
      height: (element as HTMLElement).style.height,
      paddingBottom: (element as HTMLElement).style.paddingBottom,
      paddingLeft: (element as HTMLElement).style.paddingLeft,
      paddingRight: (element as HTMLElement).style.paddingRight,
    }));
    expect(tabbarStyles.height).toContain("env(safe-area-inset-bottom)");
    expect(tabbarStyles.paddingBottom).toContain("env(safe-area-inset-bottom)");
    expect(tabbarStyles.paddingLeft).toContain("env(safe-area-inset-left)");
    expect(tabbarStyles.paddingRight).toContain("env(safe-area-inset-right)");
  });

  test("responsive: mobile tab bar stays below modal overlays", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    const bottomNav = page.getByTestId("app-mobile-tabbar");
    await expect(bottomNav).toBeVisible();

    const tabbarZIndex = await bottomNav.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );

    await page.evaluate(() => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "?", bubbles: true }));
    });

    const shortcutHelpModal = page.getByTestId("shortcut-help-modal");
    await expect(shortcutHelpModal).toBeVisible();

    const modalZIndex = await shortcutHelpModal.evaluate((element) =>
      Number(window.getComputedStyle(element as HTMLElement).zIndex),
    );
    expect(modalZIndex).toBeGreaterThan(tabbarZIndex);
  });

  test("responsive: mobile routes avoid horizontal overflow", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");

    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-tab-office").click();
    await expect(page).toHaveURL(/\/office$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-tab-kanban").click();
    await expect(page).toHaveURL(/\/kanban$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-tab-stats").click();
    await expect(page).toHaveURL(/\/stats$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-more-button").click();
    await expect(page.getByTestId("app-mobile-more-menu")).toBeVisible({ timeout: 15000 });
    await page.getByTestId("app-mobile-more-menu").getByRole("button", { name: /에이전트|Agents/ }).click();
    await expect(page).toHaveURL(/\/agents$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-more-button").click();
    await expect(page.getByTestId("app-mobile-more-menu")).toBeVisible({ timeout: 15000 });
    await page.getByTestId("app-mobile-more-menu").getByRole("button", { name: /설정|Settings/ }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expectNoHorizontalOverflow(page);
  });

  test("home: widget order persists from storage and reset restores defaults", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");

    await page.addInitScript((order) => {
      window.localStorage.setItem("agentdesk.home.order", JSON.stringify(order));
    }, CUSTOM_HOME_WIDGET_ORDER);

    await page.goto("/home");

    await expect(page.getByTestId("home-widget-kanban")).toBeVisible({ timeout: 15000 });
    await expect(await getHomeWidgetOrder(page)).toEqual(CUSTOM_HOME_WIDGET_ORDER);

    await page.getByTestId("home-edit-toggle").click();
    await expect(page.getByTestId("home-reset-order")).toBeVisible();
    await page.getByTestId("home-reset-order").click();

    await expect.poll(() => getHomeWidgetOrder(page)).toEqual(DEFAULT_HOME_WIDGET_ORDER);
    await expect
      .poll(() =>
        page.evaluate(() => JSON.parse(window.localStorage.getItem("agentdesk.home.order") ?? "[]")),
      )
      .toEqual(DEFAULT_HOME_WIDGET_ORDER);
  });

  test("home: desktop edit mode supports drag reorder and persists layout", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");

    await page.goto("/home");

    await expect(page.getByTestId("home-widget-office")).toBeVisible({ timeout: 15000 });
    await page.getByTestId("home-edit-toggle").click();

    await page.getByTestId("home-widget-missions").dragTo(page.getByTestId("home-widget-office"));

    await expect.poll(() => getHomeWidgetOrder(page)).toEqual(DRAGGED_HOME_WIDGET_ORDER);
    await expect
      .poll(() =>
        page.evaluate(() => JSON.parse(window.localStorage.getItem("agentdesk.home.order") ?? "[]")),
      )
      .toEqual(DRAGGED_HOME_WIDGET_ORDER);
  });

  test("home: renders all handoff widgets and shared gamification blocks", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");

    await page.goto("/home");

    for (const widgetId of DEFAULT_HOME_WIDGET_ORDER) {
      await expect(page.getByTestId(`home-widget-${widgetId}`)).toBeVisible({ timeout: 15000 });
    }

    await expect(page.getByTestId("sidebar-user-level-ring")).toBeVisible();
    await expect(page.getByTestId("home-streak-counter")).toBeVisible();
    await expect(page.getByTestId("home-daily-missions")).toBeVisible();
    await expect(page.getByTestId("home-daily-mission-dispatches_today")).toBeVisible();
    await expect(page.getByTestId("home-daily-mission-dispatches_today")).toContainText(
      /오늘 디스패치 5건 완료|Complete 5 dispatches today/,
    );
  });

  test("home: mobile stacks widgets and disables edit affordance", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");

    await page.goto("/home");

    await expect(page.getByTestId("home-widget-m_tokens")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("home-widget-m_cost")).toBeVisible();
    await expect(page.getByTestId("home-widget-kanban")).toBeVisible();
    await expect(page.getByTestId("home-edit-toggle")).toHaveCount(0);

    const [tokensBox, costBox] = await Promise.all([
      page.getByTestId("home-widget-m_tokens").boundingBox(),
      page.getByTestId("home-widget-m_cost").boundingBox(),
    ]);

    expect(tokensBox).not.toBeNull();
    expect(costBox).not.toBeNull();
    expect(costBox!.y).toBeGreaterThan(tokensBox!.y + tokensBox!.height - 1);
  });

  test("achievements: dedicated page renders sections and shared gamification blocks", async ({ page }) => {
    await page.goto("/achievements");

    await expect(page.getByTestId("achievements-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("achievements-section-earned")).toBeVisible();
    await expect(page.getByTestId("achievements-section-progress")).toBeVisible();
    await expect(page.getByTestId("achievements-section-locked")).toBeVisible();
    await expect(page.getByTestId("achievements-daily-missions")).toBeVisible();
    await expect(page.getByTestId("achievements-streak")).toBeVisible();
    await expect(page.getByTestId("achievements-ranking")).toBeVisible();

    await page.getByTestId("achievement-card-earned-achievement-first-task-ada").click();
    await expect(page.getByTestId("achievements-drawer")).toBeVisible();
    await expect(page.getByTestId("achievements-details")).toBeVisible();
    await expect(page.getByTestId("achievements-timeline")).toBeVisible();
  });

  test("achievements: mobile keeps the badge grid in two columns", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");

    await page.goto("/achievements");

    const lockedCards = page.locator('[data-testid="achievements-grid-locked"] > button');
    await expect(lockedCards.first()).toBeVisible({ timeout: 15000 });
    await expect(lockedCards).toHaveCount(5);
    await expect(lockedCards.nth(1)).toBeVisible();

    await expect
      .poll(async () => {
        const [firstCard, secondCard] = await Promise.all([
          lockedCards.nth(0).evaluate((element) => {
            const rect = element.getBoundingClientRect();
            return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
          }),
          lockedCards.nth(1).evaluate((element) => {
            const rect = element.getBoundingClientRect();
            return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
          }),
        ]);

        return Math.abs(firstCard.y - secondCard.y);
      })
      .toBeLessThan(6);

    await expect
      .poll(async () => {
        const [firstCard, secondCard] = await Promise.all([
          lockedCards.nth(0).evaluate((element) => {
            const rect = element.getBoundingClientRect();
            return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
          }),
          lockedCards.nth(1).evaluate((element) => {
            const rect = element.getBoundingClientRect();
            return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
          }),
        ]);

        return secondCard.x - firstCard.x;
      })
      .toBeGreaterThan(24);
  });

  test("gamification: operational routes keep home and achievements widgets out of the content area", async ({ page }) => {
    for (const route of ["/stats", "/kanban", "/ops"]) {
      await page.goto(route);

      await expect(page.getByTestId("home-daily-missions")).toHaveCount(0);
      await expect(page.getByTestId("home-streak-counter")).toHaveCount(0);
      await expect(page.getByTestId("achievements-daily-missions")).toHaveCount(0);
      await expect(page.getByTestId("achievements-streak")).toHaveCount(0);
      await expect(page.getByTestId("achievements-ranking")).toHaveCount(0);
    }
  });

  test("kanban: failed lane exposes trace, audit history, and pipeline hooks", async ({ page }) => {
    await page.goto("/kanban");

    await expect(page.getByTestId("kanban-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("kanban-column-failed")).toBeVisible();
    await expect(page.getByTestId("kanban-pipeline-hooks")).toBeVisible();

    await page.getByTestId("kanban-card-card-779-1").click();
    await expect(page.getByTestId("kanban-card-drawer")).toBeVisible();
    await expect(page.getByTestId("kanban-execution-trace")).toBeVisible();
    await expect(page.getByTestId("kanban-state-history")).toBeVisible();
  });

  test("kanban: mobile keeps horizontal board scroll and opens card details as a sheet", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");

    await page.goto("/kanban");

    await expect(page.getByTestId("kanban-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("kanban-mobile-summary-failed")).toBeVisible();
    await expect
      .poll(() =>
        page
          .getByTestId("kanban-board-scroll")
          .evaluate((element) => element.scrollWidth > element.clientWidth),
      )
      .toBe(true);

    await page.getByTestId("kanban-mobile-summary-failed").click();
    await page.getByTestId("kanban-card-card-779-1").click();

    const sheet = page.getByRole("dialog", { name: /카드 상세|Card details/ });
    await expect(sheet).toBeVisible();
    await expect(page.getByTestId("kanban-card-drawer")).toBeVisible();

    const sheetBox = await sheet.boundingBox();
    const viewportSize = page.viewportSize();

    expect(sheetBox).not.toBeNull();
    expect(viewportSize).not.toBeNull();
    expect((sheetBox?.y ?? 0)).toBeGreaterThan((viewportSize?.height ?? 0) * 0.08);
    expect((sheetBox?.y ?? 0) + (sheetBox?.height ?? 0)).toBeGreaterThan((viewportSize?.height ?? 0) - 12);
  });

  test("stats: dedicated route exposes range controls and key widgets", async ({ page }) => {
    await page.goto("/stats");

    await expect(page.getByTestId("stats-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("stats-range-controls")).toBeVisible();
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("stats-range-30d")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("stats-range-90d")).toHaveAttribute("aria-pressed", "false");

    await expect(page.getByTestId("stats-summary-total-tokens")).toBeVisible();
    await expect(page.getByTestId("stats-summary-api-spend")).toBeVisible();
    await expect(page.getByTestId("stats-summary-cache-saved")).toBeVisible();
    await expect(page.getByTestId("stats-summary-cache-hit")).toBeVisible();
    await expect(page.getByTestId("stats-daily-token-chart")).toBeVisible();
    await expect(page.getByTestId("stats-model-share")).toBeVisible();
    await expect(page.getByTestId("stats-provider-share")).toBeVisible();
    await expect(page.getByTestId("stats-skill-usage")).toBeVisible();
    await expect(page.getByTestId("stats-agent-leaderboard")).toBeVisible();

    await page.getByTestId("stats-range-7d").click();
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("stats-range-30d")).toHaveAttribute("aria-pressed", "false");

    await page.getByTestId("stats-range-90d").click();
    await expect(page.getByTestId("stats-range-90d")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-pressed", "false");
  });

  test("stats: mobile stacks summary cards vertically", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/stats");

    const totalTokensCard = page.getByTestId("stats-summary-total-tokens");
    const apiSpendCard = page.getByTestId("stats-summary-api-spend");

    await expect(page.getByTestId("stats-page")).toBeVisible({ timeout: 15000 });
    await expect(totalTokensCard).toBeVisible({ timeout: 15000 });
    await expect(apiSpendCard).toBeVisible({ timeout: 15000 });

    const [firstBox, secondBox] = await Promise.all([
      totalTokensCard.boundingBox(),
      apiSpendCard.boundingBox(),
    ]);

    expect(firstBox).not.toBeNull();
    expect(secondBox).not.toBeNull();
    expect(secondBox!.y).toBeGreaterThan(firstBox!.y + firstBox!.height - 1);
  });

  test("meetings: dedicated route renders integrated desktop layout", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await mockMeetingsHubApis(page);
    await page.goto("/meetings");

    await expect(page.getByTestId("meetings-page")).toBeVisible({ timeout: 15000 });
    const timelinePane = page.getByTestId("meetings-page-timeline");
    const skillsPane = page.getByTestId("meetings-page-skills");

    await expect(timelinePane).toBeVisible();
    await expect(skillsPane).toBeVisible();
    await expect(timelinePane.getByText(/로드밸런서 경고 플로우 리뷰/).first()).toBeVisible();
    await expect(skillsPane.getByText(/office-warning-disclosure/).first()).toBeVisible();
  });

  test("meetings: mobile switches between timeline and skills", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await mockMeetingsHubApis(page);
    await page.goto("/meetings");

    await expect(page.getByTestId("meetings-page-timeline")).toBeVisible();
    await expect(page.getByTestId("meetings-page-skills")).toHaveCount(0);

    await page.getByRole("button", { name: /^스킬$|^Skills$/ }).click();
    await expect(page.getByTestId("meetings-page-skills")).toBeVisible();
    await expect(page.getByTestId("meetings-page-timeline")).toHaveCount(0);

    await page.getByRole("button", { name: /^회의$|^Meetings$/ }).click();
    await expect(page.getByTestId("meetings-page-timeline")).toBeVisible();
  });

  test("meetings: detail drawer opens from the timeline", async ({ page }) => {
    await mockMeetingsHubApis(page);
    await page.goto("/meetings");

    await page.getByRole("button", { name: /상세 보기|Details/ }).first().click();

    await expect(
      page.getByRole("dialog", { name: /로드밸런서 경고 플로우 리뷰/ }),
    ).toBeVisible();
    await expect(
      page.getByText(/모바일에서 warning disclosure는 별도 버튼으로 분리합니다/),
    ).toBeVisible();
  });

  test("agents: desktop hub preserves 3-tab drill-ins", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await mockAgentsHubApis(page);
    await page.goto("/agents");

    await expect(page.getByTestId("agents-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("agents-tab-bar")).toBeVisible();
    await expect(page.getByTestId("agents-tab-button-agents")).toHaveAttribute("aria-selected", "true");
    await expect(page.getByTestId("agents-tab")).toBeVisible();
    await expect(page.getByTestId("agents-view-grid")).toBeVisible();
    await expect(page.getByRole("img", { name: /Ada Dashboard/ })).toBeVisible();

    await page.getByRole("button", { name: /리스트|List/ }).click();
    await expect(page.getByTestId("agents-view-list")).toBeVisible();

    await page.getByTestId("agents-card-agent-ada").click();
    const agentDetailDialog = page.getByRole("dialog", { name: /직원 상세|Agent Details/ });
    await expect(agentDetailDialog).toBeVisible();
    await expect(agentDetailDialog).toContainText("Ada Dashboard");
    await page.getByRole("button", { name: /닫기|Close/ }).click();

    await page.getByTestId("agents-tab-button-departments").click();
    await expect(page.getByTestId("agents-departments-tab")).toBeVisible();
    await expect(page.getByTestId("agents-department-card-dept-platform")).toBeVisible();

    await page.getByTestId("agents-tab-button-backlog").click();
    await expect(page.getByTestId("agents-backlog-tab")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-filter-provider")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-filter-severity")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-filter-status")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-sort")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-table")).toBeVisible();

    await page.getByTestId("agents-backlog-row-card-786-1").click();
    await expect(page.getByTestId("agents-backlog-drawer")).toBeVisible();
    await expect(page.getByRole("dialog", { name: /Agents hub smoke regression/ })).toBeVisible();
  });

  test("agents: mobile backlog switches to card stack", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await mockAgentsHubApis(page);
    await page.goto("/agents");

    await expect(page.getByTestId("agents-page")).toBeVisible({ timeout: 15000 });
    await page.getByTestId("agents-tab-button-backlog").click();

    await expect(page.getByTestId("agents-backlog-cards")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-card-card-786-1")).toBeVisible();
    await expect(page.getByTestId("agents-backlog-table")).toBeHidden();

    await page.getByTestId("agents-backlog-card-card-786-1").click();
    await expect(page.getByTestId("agents-backlog-drawer")).toBeVisible();
  });

  test("ops: route surfaces signal cards and bottlenecks", async ({ page }) => {
    await mockOpsHealthApi(page, () => MOCK_OPS_HEALTH_BASE);
    await page.goto("/ops");

    await expect(page.getByTestId("ops-page")).toBeVisible();
    await expect(page.getByTestId("ops-signal-grid")).toBeVisible();
    await expect(page.getByTestId("ops-signal-deferred_hooks")).toBeVisible();
    await expect(page.getByTestId("ops-signal-outbox_age")).toBeVisible();
    await expect(page.getByTestId("ops-signal-pending_queue")).toBeVisible();
    await expect(page.getByTestId("ops-signal-active_watchers")).toBeVisible();
    await expect(page.getByTestId("ops-signal-recovery_seconds")).toBeVisible();

    await expect(page.getByTestId("ops-bottlenecks")).toBeVisible();
    await expect(page.getByTestId("ops-bottleneck-outbox_age")).toBeVisible();
    await expect(page.getByTestId("ops-bottleneck-provider_disconnects")).toBeVisible();

    await expect(page.getByTestId("ops-connection-panel")).toBeVisible();
    await expect(page.getByTestId("ops-websocket-card")).toBeVisible();
    await expect(page.getByTestId("ops-dispatch-outbox-card")).toBeVisible();
    await expect(page.getByTestId("ops-providers-card")).toBeVisible();
  });

  test("ops: ws events resync the health snapshot", async ({ page }) => {
    let currentHealth = {
      ...MOCK_OPS_HEALTH_BASE,
      status: "healthy",
      deferred_hooks: 0,
      queue_depth: 0,
      watcher_count: 2,
      recovery_duration: 30,
      outbox_age: 12,
      degraded_reasons: [],
      dispatch_outbox: {
        pending: 1,
        retrying: 0,
        permanent_failures: 0,
        oldest_pending_age: 12,
      },
      providers: [
        {
          name: "codex",
          connected: true,
          active_turns: 1,
          queue_depth: 0,
          sessions: 2,
          restart_pending: false,
          last_turn_at: "2026-04-20T03:20:00Z",
        },
      ],
    };

    await mockOpsHealthApi(page, () => currentHealth);
    await page.goto("/ops");

    const deferredHooksCard = page.getByTestId("ops-signal-deferred_hooks");
    await expect(deferredHooksCard).toContainText("0");
    await expect(page.getByTestId("ops-bottlenecks-empty")).toBeVisible();

    currentHealth = {
      ...currentHealth,
      status: "degraded",
      deferred_hooks: 22,
      queue_depth: 4,
      outbox_age: 45,
      degraded_reasons: ["dispatch_outbox_oldest_pending_age:45"],
      dispatch_outbox: {
        pending: 4,
        retrying: 1,
        permanent_failures: 0,
        oldest_pending_age: 45,
      },
    };

    await page.evaluate(() => {
      window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: { type: "health.updated" } }));
    });

    await expect(deferredHooksCard).toContainText("22", { timeout: 5000 });
    await expect(page.getByTestId("ops-bottleneck-outbox_age")).toBeVisible();
  });

  test("settings route is reachable from the desktop shell", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    await page
      .getByTestId("app-sidebar-nav")
      .getByRole("button", { name: /^설정$|^Settings$/ })
      .click();
    await expect(page).toHaveURL(/\/settings(\?.*)?$/);
    await expect(page.getByTestId("topbar")).toContainText(/설정|Settings/);
  });

  test("settings: mobile page remains scrollable", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/settings");

    const settingsPage = page.getByTestId("settings-page");
    await expect(settingsPage).toBeVisible({ timeout: 15000 });

    const before = await settingsPage.evaluate((node) => ({
      clientHeight: node.clientHeight,
      scrollHeight: node.scrollHeight,
      scrollTop: node.scrollTop,
    }));

    expect(before.scrollHeight).toBeGreaterThan(before.clientHeight);

    await settingsPage.evaluate((node) => {
      node.scrollTop = node.scrollHeight;
      node.dispatchEvent(new Event("scroll"));
    });

    const after = await settingsPage.evaluate((node) => node.scrollTop);
    expect(after).toBeGreaterThan(0);
  });

  test("all app shell routes are directly reachable", async ({ page }) => {
    for (const route of ROUTES) {
      await page.goto(route.path);
      await expect(page).toHaveURL(
        new RegExp(`${route.path.replace("/", "\\/")}$`),
      );
      await expect(page.getByTestId("topbar")).toContainText(route.label);
    }
  });
});
