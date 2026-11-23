import { expect, test, type Page } from "@playwright/test";
import { serverCampaignFixtures } from "../src/api/serverCampaignsFixtures";

const ROUTES = [
  { path: "/home", label: /홈|Home/ },
  { path: "/campaigns", label: /캠페인|Campaigns/ },
  { path: "/routines", label: /루틴|Routines/ },
  { path: "/audit", label: /감사|Audit/ },
  { path: "/stats", label: /통계|Stats/ },
  { path: "/agents", label: /에이전트|Agents/ },
  { path: "/settings", label: /설정|Settings/ },
];

// Removed pages keep their links working by redirecting into the new IA.
const LEGACY_REDIRECTS = [
  { path: "/kanban", to: /\/campaigns\?tab=queue$/ },
  { path: "/meetings", to: /\/agents$/ },
  { path: "/achievements", to: /\/stats$/ },
  { path: "/voice", to: /\/home$/ },
  { path: "/ops", to: /\/settings\?category=connections$/ },
  { path: "/control", to: /\/settings\?category=connections$/ },
];

// v3 home is a fixed order: to-dos, system, agents, campaigns, today's routines, usage.
const HOME_SECTIONS = ["할 일", "시스템", "에이전트", "캠페인", "오늘 루틴", "사용량"];

async function openAppearancePanel(page: Page, isMobile: boolean) {
  if (isMobile) {
    await page.getByTestId("app-mobile-more-button").click();
    await page.getByTestId("app-mobile-more-tweaks").click();
    return;
  }
  await page.getByRole("button", { name: /^화면 꾸미기$|^Appearance$/ }).click();
}
const PIPELINE_VISUAL_CACHE_KEY = "agentdesk.settings.pipeline.visual-cache.v1";

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

async function mockHomeSources(page: Page) {
  await page.route(/\/api\/rate-limits$/, (route) => route.fulfill({ json: { providers: [
    { provider: "claude", profile_id: "default", stale: false, fetched_at: 1_795_000_000, buckets: [{ name: "7d", limit: 100, used: 83, remaining: 17, reset: 0 }] },
  ] } }));
  await page.route(/\/api\/settings\/runtime-config$/, (route) => route.fulfill({ json: {
    current: { dispatchRateLimitGateEnabled: true, dispatchRateLimitGateDangerPct: 90 }, defaults: {},
  } }));
  await page.route(/\/api\/campaigns\?/, (route) => route.fulfill({ json: { campaigns: [], live: {}, limit: 100, offset: 0 } }));
  await page.route(/\/api\/server-campaigns\?/, (route) => route.fulfill({ json: { campaigns: [] } }));
}

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

const MOCK_ROUTINES = [
  {
    id: "routine-late",
    agent_id: "family-counsel",
    script_ref: "family-profile-probe-yohoejang.js",
    name: "요회장 프로필 점검",
    status: "enabled",
    execution_strategy: "fresh",
    schedule: "0 15 * * *",
    next_due_at: "2026-05-17T06:00:00.000Z",
    last_run_at: "2026-05-16T06:07:37.573Z",
    last_result: "**Profile probe failed**: calendar token expired.\n\nRetry after re-auth.",
    last_run: { id: "run-late-1", routine_id: "routine-late", status: "failed", started_at: "2026-05-16T06:07:00.000Z", finished_at: "2026-05-16T06:07:37.573Z" },
    checkpoint: null,
    discord_thread_id: "1505086366654926908",
    timeout_secs: 600,
    in_flight_run_id: null,
    created_at: "2026-05-16T05:55:52.782Z",
    updated_at: "2026-05-16T06:07:37.573Z",
  },
  {
    id: "routine-soon",
    agent_id: "family-counsel",
    script_ref: "family-profile-probe-obujang.js",
    name: "오부장 프로필 점검",
    status: "enabled",
    execution_strategy: "fresh",
    schedule: "0 13 * * *",
    next_due_at: "2026-05-17T04:00:00.000Z",
    last_run_at: null,
    last_result: null,
    checkpoint: null,
    discord_thread_id: "1505086338448363660",
    timeout_secs: 600,
    in_flight_run_id: null,
    created_at: "2026-05-16T05:55:46.055Z",
    updated_at: "2026-05-16T05:55:46.437Z",
  },
  {
    id: "routine-manual",
    agent_id: "codex",
    script_ref: "manual-review.js",
    name: "수동 리뷰",
    status: "paused",
    execution_strategy: "persistent",
    schedule: null,
    next_due_at: null,
    last_run_at: "2026-05-15T01:00:00.000Z",
    last_result: "ok",
    checkpoint: null,
    discord_thread_id: null,
    timeout_secs: null,
    in_flight_run_id: null,
    created_at: "2026-05-14T05:00:00.000Z",
    updated_at: "2026-05-15T01:00:00.000Z",
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
const MOCK_DASHBOARD_STATS = {
  agents: {
    total: 2,
    working: 1,
    idle: 1,
    break: 0,
    offline: 0,
  },
  departments: [
    {
      id: "dept-platform",
      name: "Platform",
      name_ko: "플랫폼",
      icon: "🧱",
      color: "#38bdf8",
      total_agents: 1,
      working_agents: 1,
    },
    {
      id: "dept-product",
      name: "Product",
      name_ko: "프로덕트",
      icon: "🧭",
      color: "#f97316",
      total_agents: 1,
      working_agents: 0,
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

const MOCK_SETTINGS_FSM_PIPELINE = {
  name: "agentdesk-default",
  version: 1,
  states: [
    { id: "backlog", label: "backlog", terminal: false },
    { id: "ready", label: "ready", terminal: false },
    { id: "in_progress", label: "progress", terminal: false },
    { id: "review", label: "review", terminal: false },
    { id: "done", label: "done", terminal: true },
    { id: "failed", label: "failed", terminal: false },
  ],
  transitions: [
    { from: "backlog", to: "ready", type: "free", gates: [] },
    { from: "ready", to: "in_progress", type: "free", gates: [] },
    { from: "in_progress", to: "review", type: "free", gates: [] },
    { from: "review", to: "done", type: "gated", gates: ["review_verdict_pass"] },
    { from: "review", to: "in_progress", type: "gated", gates: ["review_verdict_rework"] },
    { from: "review", to: "failed", type: "force_only", gates: [] },
    { from: "failed", to: "ready", type: "free", gates: [] },
  ],
  gates: {
    review_verdict_pass: {
      type: "builtin",
      check: "review_verdict_pass",
      description: "Review approved",
    },
    review_verdict_rework: {
      type: "builtin",
      check: "review_verdict_rework",
      description: "Changes requested",
    },
  },
  hooks: {
    backlog: { on_enter: [], on_exit: [] },
    ready: { on_enter: [], on_exit: [] },
    in_progress: { on_enter: [], on_exit: [] },
    review: { on_enter: [], on_exit: [] },
    done: { on_enter: [], on_exit: [] },
    failed: { on_enter: [], on_exit: [] },
  },
  events: {
    on_enqueue: ["OnQueueReady"],
    on_dispatch: ["OnDispatchRequested"],
    on_submit: ["OnDispatchCompleted"],
    on_approve: ["OnReviewApproved"],
    on_changes_request: ["OnChangesRequested"],
    on_error: ["OnPipelineError"],
    on_recover: ["OnRecoverFromFailure"],
  },
  clocks: {},
  phase_gate: {
    dispatch_to: "project-agentdesk",
    dispatch_type: "review",
  },
};

const MOCK_SETTINGS_PIPELINE_STAGES = [
  {
    stage_name: "implementation",
    provider: null,
    agent_override_id: null,
    skip_condition: null,
    trigger_after: "ready",
  },
];

const MOCK_SETTINGS_CONFIG_ENTRIES = [
  {
    key: "max_review_rounds",
    value: "3",
    default: "3",
    baseline: "3",
    baseline_source: "hardcoded",
    override_active: false,
    editable: true,
    restart_behavior: "persist-live-override",
    category: "review",
    label_ko: "최대 리뷰 라운드",
    label_en: "Max Review Rounds",
  },
];

async function mockAgentsHubApis(page: Page) {
  await page.route(/\/api\/agents\/[^/]+\/execution-node$/, async (route) => {
    await route.fulfill({ json: { default_node_id: null, routing_enforced: true } });
  });
  await page.route(/\/api\/cluster\/nodes$/, async (route) => {
    await route.fulfill({ json: {
      cluster: { enabled: true, local_instance_id: "fixture-hub" },
      nodes: [
        { instance_id: "fixture-hub", hostname: "Hub fixture", effective_role: "hub", status: "online", os: "macos" },
        { instance_id: "windows-1", hostname: "Windows PC", effective_role: "worker", status: "online", os: "windows" },
        { instance_id: "linux-2", hostname: "Linux PC", effective_role: "runner", status: "offline", os: "linux" },
      ].map(({ os, ...node }) => ({ ...node,
        capabilities: { execution_readiness: {
          os, arch: "x86_64", runtime_profile: "full", observed_at_ms: Date.now(),
          expires_at_ms: Date.now() + 60_000, backends: ["process"],
        } },
        execution_readiness: { providers: { codex: { eligible: true, reasons: [] } } },
      })),
    } });
  });
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

  await page.route(/\/api\/kanban-cards(?:\?.*)?$/, async (route) => {
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
  await page.route(/\/api\/health(?:\/detail)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(getPayload()),
    });
  });
  await page.route(/\/api\/cluster\/nodes$/, route => route.fulfill({
    json: { cluster: { enabled: false, local_instance_id: "single-node" }, nodes: [] },
  }));
}

async function mockDashboardBootstrap(page: Page) {
  // Fixture tests must never fall through Vite's proxy to a real operator API.
  await page.route(/\/api\//, (route) => route.fulfill({
    status: 404, json: { error: "API route is not configured in this browser fixture" },
  }));
  await page.addInitScript(() => {
    const sockets: MockWebSocket[] = [];
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
        sockets.push(this);
        setTimeout(() => {
          if (this.readyState === MockWebSocket.CLOSED) return;
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
    Object.defineProperty(window, "__dashboardTestSockets", { value: sockets });
  });

  await page.route(/\/api\/auth\/session$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ ok: true, authenticated: true, auth_enabled: false, csrf_token: "" }),
    });
  });
  await page.route(/\/api\/auth\/ws-ticket$/, async (route) => {
    await route.fulfill({ json: { ticket: "s".repeat(64), expires_in: 15 } });
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

  await page.route(/\/api\/kanban-cards(?:\?.*)?$/, async (route) => {
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

  await page.route(/\/api\/routines(?:\?.*)?$/, async (route) => {
    const requestUrl = new URL(route.request().url());
    const status = requestUrl.searchParams.get("status");
    const routines = status
      ? MOCK_ROUTINES.filter((routine) => routine.status === status)
      : MOCK_ROUTINES;
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ routines }),
    });
  });

  await page.route(/\/api\/discord\/channels\/\d+$/, async (route) => {
    const id = new URL(route.request().url()).pathname.split("/").pop();
    await route.fulfill({ json: { id, guild_id: "1469870512812462284", name: "routine-thread" } });
  });

  await page.route(/\/api\/routines\/[^/]+\/runs(?:\?.*)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ runs: [] }),
    });
  });

  await page.route(/\/api\/github-repos$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ viewer_login: "", repos: [] }),
    });
  });

  await page.route(/\/api\/health(?:\/detail)?$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify(MOCK_OPS_HEALTH_BASE),
    });
  });
}

async function mockSettingsPipelineEditorApis(page: Page) {
  await page.route(/\/api\/settings\/config$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ entries: MOCK_SETTINGS_CONFIG_ENTRIES }),
    });
  });

  await page.route(/\/api\/github-repos$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        viewer_login: "itismyfield",
        repos: [
          {
            nameWithOwner: "itismyfield/AgentDesk",
            updatedAt: "2026-04-21T00:00:00Z",
            isPrivate: true,
            viewerPermission: "ADMIN",
          },
        ],
      }),
    });
  });

  await page.route(/\/api\/pipeline\/config\/effective\?repo=itismyfield%2FAgentDesk$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        pipeline: MOCK_SETTINGS_FSM_PIPELINE,
        layers: { default: true, repo: false, agent: false },
      }),
    });
  });

  await page.route(/\/api\/pipeline\/config\/repo\/itismyfield\/AgentDesk$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        repo: "itismyfield/AgentDesk",
        pipeline_config: {
          states: MOCK_SETTINGS_FSM_PIPELINE.states,
          transitions: MOCK_SETTINGS_FSM_PIPELINE.transitions,
          gates: MOCK_SETTINGS_FSM_PIPELINE.gates,
          hooks: MOCK_SETTINGS_FSM_PIPELINE.hooks,
          events: MOCK_SETTINGS_FSM_PIPELINE.events,
          clocks: MOCK_SETTINGS_FSM_PIPELINE.clocks,
          phase_gate: MOCK_SETTINGS_FSM_PIPELINE.phase_gate,
        },
      }),
    });
  });

  await page.route(/\/api\/pipeline\/stages\?repo=itismyfield%2FAgentDesk$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ stages: MOCK_SETTINGS_PIPELINE_STAGES }),
    });
  });
}

async function mockSettingsPipelineEditorApisWithRefreshDelay(page: Page, delayMs = 700) {
  await page.route(/\/api\/settings\/config$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ entries: MOCK_SETTINGS_CONFIG_ENTRIES }),
    });
  });

  await page.route(/\/api\/github-repos$/, async (route) => {
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        viewer_login: "itismyfield",
        repos: [
          {
            nameWithOwner: "itismyfield/AgentDesk",
            updatedAt: "2026-04-21T00:00:00Z",
            isPrivate: true,
            viewerPermission: "ADMIN",
          },
        ],
      }),
    });
  });

  await page.route(/\/api\/pipeline\/config\/effective\?repo=itismyfield%2FAgentDesk$/, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, delayMs));
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        pipeline: MOCK_SETTINGS_FSM_PIPELINE,
        layers: { default: true, repo: false, agent: false },
      }),
    });
  });

  await page.route(/\/api\/pipeline\/config\/repo\/itismyfield\/AgentDesk$/, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, delayMs));
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        repo: "itismyfield/AgentDesk",
        pipeline_config: {
          states: MOCK_SETTINGS_FSM_PIPELINE.states,
          transitions: MOCK_SETTINGS_FSM_PIPELINE.transitions,
          gates: MOCK_SETTINGS_FSM_PIPELINE.gates,
          hooks: MOCK_SETTINGS_FSM_PIPELINE.hooks,
          events: MOCK_SETTINGS_FSM_PIPELINE.events,
          clocks: MOCK_SETTINGS_FSM_PIPELINE.clocks,
          phase_gate: MOCK_SETTINGS_FSM_PIPELINE.phase_gate,
        },
      }),
    });
  });

  await page.route(/\/api\/pipeline\/stages\?repo=itismyfield%2FAgentDesk$/, async (route) => {
    await new Promise((resolve) => setTimeout(resolve, delayMs));
    await route.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({ stages: MOCK_SETTINGS_PIPELINE_STAGES }),
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

  test("authentication: login, reconnect, logout and reload clear credentials", async ({ page }, testInfo) => {
    let acceptedToken = "synthetic-browser-credential";
    let ticketCount = 0;
    let rejectedTickets = 0;
    let protectedRequests = 0;
    await page.addInitScript(() => {
      sessionStorage.setItem("stats:token-analytics:7d", '{"previous":"private"}');
      localStorage.setItem("agentdesk.settings.pipeline.repo-cache.v1", '{"previous":"private"}');
    });
    page.on("request", (request) => {
      if (new URL(request.url()).pathname === "/api/agents") protectedRequests++;
    });
    await page.route(/\/api\/auth\/session$/, async (route) => {
      await route.fulfill({ json: {
        ok: true, auth_enabled: true, csrf_token: "",
        authenticated: route.request().headers().authorization === `Bearer ${acceptedToken}`,
      } });
    });
    await page.route(/\/api\/auth\/ws-ticket$/, async (route) => {
      if (route.request().headers().authorization !== `Bearer ${acceptedToken}`) {
        rejectedTickets++;
        await route.fulfill({ status: 401, json: { error: "unauthorized" } });
        return;
      }
      ticketCount++;
      await route.fulfill({ json: { ticket: String(ticketCount).padStart(64, "t"), expires_in: 15 } });
    });
    await page.goto("/home");
    await expect(page.getByRole("heading", { name: "AgentDesk 로그인" })).toBeVisible();
    await expect(page.getByLabel("서버 토큰")).toBeEnabled();
    expect(protectedRequests).toBe(0);
    expect(await page.evaluate(() => sessionStorage.getItem("stats:token-analytics:7d"))).toBeNull();
    expect(await page.evaluate(() => localStorage.getItem("agentdesk.settings.pipeline.repo-cache.v1"))).toBeNull();
    await expectNoHorizontalOverflow(page);
    await page.screenshot({ path: testInfo.outputPath("login.png"), fullPage: true });
    await page.getByLabel("서버 토큰").fill("invalid");
    await page.getByRole("button", { name: "로그인", exact: true }).click();
    await expect(page.getByLabel("서버 토큰")).toBeEnabled();
    await expect(page.getByTestId("topbar")).toHaveCount(0);
    await page.getByLabel("서버 토큰").fill(acceptedToken);
    await page.getByRole("button", { name: "로그인", exact: true }).click();
    await expect(page.getByTestId("topbar")).toBeVisible();
    await expect.poll(() => ticketCount).toBe(1);
    await expectNoHorizontalOverflow(page);
    const inspectSockets = () => page.evaluate(() => {
      const sockets = (window as unknown as { __dashboardTestSockets: WebSocket[] }).__dashboardTestSockets;
      return sockets.map((socket) => ({ url: socket.url, readyState: socket.readyState }));
    });
    expect(JSON.stringify(await inspectSockets())).not.toContain(acceptedToken);
    expect(await page.evaluate(() => JSON.stringify([localStorage, sessionStorage]))).not.toContain(acceptedToken);
    await page.evaluate(() => {
      (window as unknown as { __dashboardTestSockets: WebSocket[] }).__dashboardTestSockets.at(-1)!.close();
    });
    await expect.poll(() => ticketCount).toBe(2);
    const sockets = await inspectSockets();
    expect(sockets[0].url).not.toBe(sockets[1].url);
    acceptedToken = "rotated-synthetic-browser-credential";
    await page.evaluate(() => {
      (window as unknown as { __dashboardTestSockets: WebSocket[] }).__dashboardTestSockets.at(-1)!.close();
    });
    await expect(page.getByRole("heading", { name: "AgentDesk 로그인" })).toBeVisible();
    expect(rejectedTickets).toBe(1);
    await page.getByLabel("서버 토큰").fill(acceptedToken);
    await page.getByRole("button", { name: "로그인", exact: true }).click();
    await expect(page.getByTestId("topbar")).toBeVisible();
    await page.getByRole("button", { name: "로그아웃", exact: true }).click();
    await expect(page.getByRole("heading", { name: "AgentDesk 로그인" })).toBeVisible();
    expect((await inspectSockets()).every((socket) => socket.readyState === 3)).toBe(true);
    await page.getByLabel("서버 토큰").fill(acceptedToken);
    await page.getByRole("button", { name: "로그인", exact: true }).click();
    await expect(page.getByTestId("topbar")).toBeVisible();
    await page.reload();
    await expect(page.getByRole("heading", { name: "AgentDesk 로그인" })).toBeVisible();
    await expect(page.getByTestId("topbar")).toHaveCount(0);
  });

  test("theme: dark/light toggle changes CSS variables", async ({ page }, testInfo) => {
    await page.goto("/home");
    await openAppearancePanel(page, testInfo.project.name === "mobile");
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

  test("theme: auto mode responds to prefers-color-scheme", async ({ page }, testInfo) => {
    await page.emulateMedia({ colorScheme: "dark" });
    await page.goto("/home");
    await openAppearancePanel(page, testInfo.project.name === "mobile");
    await page.getByRole("button", { name: /^자동$|^Auto$/ }).click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

    await page.emulateMedia({ colorScheme: "light" });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });

  test("command palette searches with missing localized labels and closes accessibly", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop keyboard shortcut coverage");
    await page.route("**/api/agents", (route) => route.fulfill({
      json: { agents: MOCK_AGENTS.map(agent => ({ ...agent, name_ko: null })) },
    }));
    await page.goto("/home");

    await page.getByLabel(/^찾기$|^Search$/).click();
    const palette = page.getByRole("dialog", {
      name: /명령 팔레트|Command Palette/,
    });
    await expect(palette).toBeVisible();

    const search = palette.getByPlaceholder(/검색|Search/);
    await expect(search).toBeFocused();
    await search.fill("stats");
    await expect(palette.getByText(/통계|Stats/)).toBeVisible();

    await page.keyboard.press("Enter");
    await expect(page).toHaveURL(/\/stats$/);

    await page.getByLabel(/^찾기$|^Search$/).click();
    await expect(palette).toBeVisible();
    // Esc closes at once even with a query typed (the kit alone would clear the query first).
    await search.fill("zz");
    await page.keyboard.press("Escape");
    await expect(palette).toHaveCount(0);
  });

  test("command palette opens and closes on the phone with touch-size controls", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== "mobile", "Phone layout coverage");
    await page.goto("/home");
    await page.getByTestId("topbar-search").click();
    const palette = page.getByRole("dialog", { name: /명령 팔레트|Command Palette/ });
    const search = palette.getByPlaceholder(/검색|Search/);
    await expect(search).toBeFocused();
    expect(parseFloat(await search.evaluate((input) => getComputedStyle(input).fontSize))).toBeGreaterThanOrEqual(16);
    const close = palette.getByRole("button", { name: /명령 팔레트 닫기|Close command palette/ });
    expect((await close.boundingBox())?.height).toBeGreaterThanOrEqual(44);
    expect((await palette.getByRole("option").first().boundingBox())?.height).toBeGreaterThanOrEqual(44);
    await expectNoHorizontalOverflow(page);
    await close.click();
    await expect(palette).toHaveCount(0);
  });

  test("desktop: sidebar renders at the full shell width", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    const sidebar = page.getByTestId("app-sidebar-nav");
    await expect(sidebar).toBeVisible();
    const box = await sidebar.boundingBox();
    expect(box?.width).toBeGreaterThan(228);
    expect(box?.width).toBeLessThan(236);
    // v3 rail order: settings sits below a divider and ops is off the menu.
    await expect(sidebar.getByRole("link")).toHaveText([/홈|Home/, /캠페인|Campaigns/, /에이전트|Agents/, /루틴|Routines/, /감사|Audit/, /통계|Stats/, /설정|Settings/]);
    await expect(sidebar.getByRole("link", { name: /^홈$|^Home$/ })).toHaveAttribute("aria-current", "page");
    await expect(sidebar.getByRole("link", { name: /캠페인, 확인할 일 2건|Campaigns, 2 need attention/ })).toContainText("2");
    await expect(page.getByTestId("topbar-search").locator("kbd")).toContainText(/Command K|Control K/);
  });

  test("desktop: a short window keeps the rail status in view and the menu scrollable", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await mockOpsHealthApi(page, () => MOCK_OPS_HEALTH_BASE);
    await page.setViewportSize({ width: 950, height: 345 });
    await page.goto("/home");
    const sidebar = page.getByTestId("app-sidebar-nav");
    const menu = page.getByTestId("app-sidebar-menu");
    expect(await menu.evaluate((node) => node.scrollHeight > node.clientHeight)).toBe(true);
    const status = await page.getByTestId("sidebar-connection-status").boundingBox();
    expect(status!.y + status!.height).toBeLessThanOrEqual(345);
    await sidebar.getByRole("link", { name: /^설정$|^Settings$/ }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expect(page.getByTestId("settings-page")).toBeVisible();
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
    for (const id of ["home", "campaigns", "agents", "audit"]) {
      await expect(page.getByTestId(`app-mobile-tab-${id}`)).toBeVisible();
    }
    await expect(page.getByTestId("app-mobile-more-button")).toContainText(/더 보기|More/);
    await expect(page.getByTestId("app-mobile-tab-home")).toHaveAttribute("aria-current", "page");
    await expect(page.getByTestId("app-mobile-tab-campaigns")).toHaveAttribute("aria-label", /캠페인, 확인할 일 2건|Campaigns, 2 need attention/);
    await expect(page.getByTestId("app-mobile-tab-campaigns")).toContainText("2");

    const tabMetrics = await bottomNav.locator("button").evaluateAll((buttons) =>
      buttons.map((button) => {
        const node = button as HTMLElement;
        const box = node.getBoundingClientRect();
        const iconBox = node.querySelector("svg")?.getBoundingClientRect();
        const label = node.querySelector("span:not([class*='absolute'])") as HTMLElement | null;
        return {
          height: box.height,
          iconWidth: iconBox?.width ?? 0,
          labelClientWidth: label?.clientWidth ?? 0,
          labelScrollWidth: label?.scrollWidth ?? 0,
        };
      }),
    );
    for (const metric of tabMetrics) {
      expect(metric.height).toBeGreaterThanOrEqual(44);
      expect(metric.iconWidth).toBeGreaterThanOrEqual(20);
      expect(metric.labelScrollWidth).toBeLessThanOrEqual(metric.labelClientWidth + 1);
    }

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
    await expect(moreMenu.getByRole("button", { name: /더 보기 닫기|Close/ })).toBeFocused();
    for (const id of ["routines", "stats", "settings"]) {
      await expect(page.getByTestId(`app-mobile-more-${id}`)).toBeVisible();
    }
    await expect(page.getByTestId("app-mobile-more-theme")).toBeVisible();
    await expect(page.getByTestId("app-mobile-more-tweaks")).toBeVisible();
    await expect(moreMenu.getByRole("button", { name: /회의|Meetings|업적|Achievements|음성|Voice|칸반|Kanban|운영|Ops/ })).toHaveCount(0);

    const moreMenuBackdropZIndex = await moreMenu.evaluate((element) =>
      Number(window.getComputedStyle(element.parentElement as HTMLElement).zIndex),
    );
    expect(moreMenuBackdropZIndex).toBeGreaterThan(tabbarZIndex);

    const shellStyles = await page.getByTestId("app-main-scroll").evaluate((element) => ({
      marginBottom: (element as HTMLElement).style.marginBottom,
    }));
    expect(shellStyles.marginBottom).toContain("env(safe-area-inset-bottom)");

    await page.getByTestId("app-mobile-more-stats").click();
    await expect(page).toHaveURL(/\/stats$/);
    await expect(moreMenu).toHaveCount(0);
    // Pages behind More light up the More tab instead of an unrelated one.
    await expect(page.getByTestId("app-mobile-more-button")).toHaveAttribute("data-active", "true");

    await page.getByTestId("app-mobile-more-button").click();
    await expect(moreMenu).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(moreMenu).toHaveCount(0);
    await expect(page.getByTestId("app-mobile-more-button")).toBeFocused();
  });

  test("responsive: the More sheet closes on back navigation and marks the current page", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");
    await page.getByTestId("app-mobile-tab-audit").click();
    await expect(page).toHaveURL(/\/audit$/);
    await page.getByTestId("app-mobile-more-button").click();
    await expect(page.getByTestId("app-mobile-more-menu")).toBeVisible();
    await page.goBack();
    await expect(page).toHaveURL(/\/home$/);
    await expect(page.getByTestId("app-mobile-more-menu")).toHaveCount(0);

    await page.goto("/routines");
    await page.getByTestId("app-mobile-more-button").click();
    await expect(page.getByTestId("app-mobile-more-routines")).toHaveAttribute("aria-current", "page");
    await expect(page.getByTestId("app-mobile-more-stats")).not.toHaveAttribute("aria-current", "page");
  });

  test("responsive: mobile topbar is a single row with title and search", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/routines");

    const topbar = page.getByTestId("topbar");
    const title = page.getByTestId("topbar-title");
    const search = page.getByTestId("topbar-search");
    await expect(title).toHaveText(/루틴|Routines/);
    await expect(search).toBeVisible();
    await expect(page.getByTestId("topbar-breadcrumb")).toHaveCount(0);

    const [topbarBox, titleBox, searchBox] = await Promise.all([
      topbar.boundingBox(),
      title.boundingBox(),
      search.boundingBox(),
    ]);
    expect(topbarBox!.height).toBeLessThanOrEqual(64);
    expect(Math.abs((titleBox!.y + titleBox!.height / 2) - (searchBox!.y + searchBox!.height / 2))).toBeLessThanOrEqual(4);
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
      Number(window.getComputedStyle(element.closest('[role="dialog"]') as HTMLElement).zIndex),
    );
    expect(modalZIndex).toBeGreaterThan(tabbarZIndex);
  });

  test("responsive: mobile routes avoid horizontal overflow", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/home");
    await expectNoHorizontalOverflow(page);

    for (const id of ["campaigns", "agents", "audit"]) {
      await page.getByTestId(`app-mobile-tab-${id}`).click();
      await expect(page).toHaveURL(new RegExp(`/${id}$`));
      await expect(page.getByTestId(`app-mobile-tab-${id}`)).toHaveAttribute("aria-current", "page");
      await expectNoHorizontalOverflow(page);
    }

    for (const id of ["routines", "stats", "settings"]) {
      await page.getByTestId("app-mobile-more-button").click();
      await expect(page.getByTestId("app-mobile-more-menu")).toBeVisible({ timeout: 15000 });
      await page.getByTestId(`app-mobile-more-${id}`).click();
      // Settings adds its open panel to the URL once mounted, so the query is allowed.
      await expect(page).toHaveURL(new RegExp(`/${id}(\\?.*)?$`));
      await expectNoHorizontalOverflow(page);
    }
  });

  test("home: fixed v3 order with to-dos first and no token cards or layout editing", async ({ page }) => {
    await mockHomeSources(page);
    await page.goto("/home");

    const home = page.getByTestId("home-page");
    // The kit's limit list: one heading per provider and a meter per window. Its visible percent counts up only once the
    // card scrolls into view, so the meter value is read instead.
    await expect(page.getByTestId("home-usage").getByRole("meter", { name: "Claude 7일" })).toHaveAttribute("aria-valuenow", "83", { timeout: 15000 });
    expect(await home.locator("section[aria-label]").evaluateAll((nodes) => nodes.map((node) => node.getAttribute("aria-label")))).toEqual(HOME_SECTIONS);
    // Only what the server leaves to a person: the blocked failed card, not the review in progress or new requests.
    await expect(page.getByTestId("home-todo")).toContainText("판단 필요");
    await expect(page.getByTestId("home-todo")).not.toContainText(/검토 대기|수락 대기/);
    await expect(page.getByTestId("home-system-memento")).toContainText("확인 불가");
    await expect(page.getByTestId("home-usage")).toContainText("90% 이상이면 자동 진행을 멈춰요");
    await expect(page.getByTestId("home-usage").getByRole("link", { name: "설정" })).toHaveAttribute("href", "/settings?category=usage");

    // Token and cost cards live on the stats page; v3 home has no layout editing or live-connection line.
    await expect(home.getByText(/오늘 토큰|API 비용|실시간 연결/)).toHaveCount(0);
    for (const removed of ["home-edit-toggle", "home-reset-order", "home-widget-m_tokens", "sidebar-user-level-ring", "home-daily-missions"]) {
      await expect(page.getByTestId(removed)).toHaveCount(0);
    }
    await expect(page.getByText(/Lv\.\s?\d|XP/)).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
  });

  test("home: agents fold the rest on desktop and list only busy agents on phones", async ({ page }, testInfo) => {
    const isMobile = testInfo.project.name === "mobile";
    const extras = Array.from({ length: 9 }, (_, index) => ({ ...MOCK_AGENTS[1], id: `agent-extra-${index}`, name: `Extra ${index}`, name_ko: `추가 ${index}` }));
    await page.route(/\/api\/agents(?:\?.*)?$/, (route) => route.fulfill({ json: { agents: [...MOCK_AGENTS, ...extras] } }));
    await mockHomeSources(page);
    await page.goto("/home");

    const card = page.getByTestId("home-agents");
    const toggle = card.getByRole("button", { name: isMobile ? "10명 더 보기" : "나머지 4명" });
    await expect(toggle).toBeVisible({ timeout: 15000 });
    await expect(card.locator("li button")).toHaveCount(isMobile ? 2 : 8);
    await toggle.click();
    await expect(card.locator("li button")).toHaveCount(12);
    await expect(card.getByRole("button", { name: "접기" })).toHaveAttribute("aria-expanded", "true");
    await expectNoHorizontalOverflow(page);
    if (isMobile) return;
    // Desktop starts at 900px; the first four agents share a row there too.
    await page.setViewportSize({ width: 960, height: 900 });
    await expect.poll(() => card.locator("li").evaluateAll((nodes) => new Set(nodes.slice(0, 4).map((node) => node.getBoundingClientRect().top)).size)).toBe(1);
  });

  test("home: the server row follows a healthy answer, a 503 status and a lost connection", async ({ page }) => {
    let mode: "healthy" | "unhealthy" | "down" = "healthy";
    await page.clock.install();
    await page.route(/\/api\/health(?:\/detail)?$/, (route) => (mode === "down"
      ? route.abort()
      : route.fulfill({ status: mode === "healthy" ? 200 : 503, json: { status: mode, providers: [] } })));
    await page.goto("/home");

    const server = page.getByTestId("home-system-server");
    await expect(server).toContainText("정상", { timeout: 15000 });
    mode = "unhealthy";
    await page.clock.runFor(30_000);
    await expect(server).toContainText("장애", { timeout: 15000 });
    mode = "down";
    await page.clock.runFor(30_000);
    await expect(server).toContainText("확인 불가", { timeout: 15000 });
    await expect(page.getByTestId("home-system")).toContainText("마지막 확인");
  });

  test("home: phones show the system card only while a line is off or unknown, desktop always", async ({ page }, testInfo) => {
    let queue = 2;
    await page.clock.install();
    await mockHomeSources(page);
    await page.route(/\/api\/health(?:\/detail)?$/, (route) => route.fulfill({ json: { status: "healthy", queue_depth: queue, providers: [{ connected: true }] } }));
    await page.goto("/home");
    await expect(page.getByTestId("home-system-queue")).toContainText("2건", { timeout: 15000 });
    queue = 0;
    await page.clock.runFor(30_000);
    await expect(page.getByTestId("home-system")).toHaveCount(testInfo.project.name === "mobile" ? 0 : 1, { timeout: 15000 });
  });

  test("home: usage marks Gemini's limit-only buckets unmeasured and keeps the settings link without the rule", async ({ page }) => {
    await mockHomeSources(page);
    await page.route(/\/api\/rate-limits$/, (route) => route.fulfill({ json: { providers: [
      { provider: "claude", profile_id: "default", stale: false, fetched_at: 1_795_000_000, buckets: [{ name: "5h", limit: 100, used: 0, remaining: 100, reset: 0 }] },
      { provider: "gemini", profile_id: "default", stale: false, fetched_at: 1_795_000_060, buckets: [
        { name: "rpm", limit: 15, used: 0, remaining: 15, reset: 0 }, { name: "rpd", limit: 1500, used: 0, remaining: 1500, reset: 0 },
      ] },
    ] } }));
    await page.route(/\/api\/settings\/runtime-config$/, (route) => route.fulfill({ status: 503, json: { error: "unavailable" } }));
    await page.goto("/home");

    const usage = page.getByTestId("home-usage");
    await expect(usage.locator("li")).toHaveCount(3, { timeout: 15000 });
    await expect(usage.locator("li").first()).toContainText("0%");
    await expect(usage.getByText("측정 안 됨")).toHaveCount(2);
    await expect(usage).toContainText("자동 진행을 멈추는 기준은 확인 불가예요", { timeout: 15000 });
    await expect(usage.getByRole("link", { name: "설정" })).toHaveAttribute("href", "/settings?category=usage");
  });

  test("home: after empty lists a failed campaign poll reads unknown on both cards until it recovers", async ({ page }) => {
    let down = false;
    await page.clock.install();
    await mockHomeSources(page);
    await page.route(/\/api\/kanban-cards(?:\?.*)?$/, (route) => route.fulfill({ json: { cards: [] } }));
    for (const [pattern, body] of [[/\/api\/campaigns\?/, { campaigns: [], live: {}, limit: 100, offset: 0 }], [/\/api\/server-campaigns\?/, { campaigns: [] }]] as const) {
      await page.route(pattern, (route) => (down ? route.fulfill({ status: 503, json: { error: "unavailable" } }) : route.fulfill({ json: body })));
    }
    await page.goto("/home");

    const todo = page.getByTestId("home-todo");
    const campaigns = page.getByTestId("home-campaigns");
    await expect(todo).toContainText("지금 할 일이 없어요.", { timeout: 15000 });
    await expect(campaigns).toContainText("진행 중인 캠페인이 없어요.");
    down = true;
    await page.clock.runFor(60_000);
    await expect(todo).toContainText("캠페인 요약을 읽지 못해 할 일 일부는 확인 불가예요.", { timeout: 15000 });
    await expect(todo).not.toContainText("지금 할 일이 없어요.");
    await expect(todo.locator("h2")).toHaveText("할 일");
    await expect(campaigns).toContainText("캠페인 목록을 읽지 못해 확인 불가예요.");
    await expect(campaigns.locator("h2")).toHaveText("캠페인");
    down = false;
    await page.clock.runFor(60_000);
    await expect(todo).toContainText("지금 할 일이 없어요.", { timeout: 15000 });
    await expect(campaigns).toContainText("진행 중인 캠페인이 없어요.");
  });

  test("home: server nodes waiting for a person lead the to-dos; approval stays disabled with its reason", async ({ page }, testInfo) => {
    const isMobile = testInfo.project.name === "mobile";
    const release = structuredClone(serverCampaignFixtures.prepared);
    const statuses = ["decision_required", "awaiting_deploy", "awaiting_approval"];
    release.view.nodes.forEach((node, index) => { node.runtime = { ...node.runtime, human_status: statuses[index], wait_started_at: null }; });
    await mockHomeSources(page);
    await page.route(/\/api\/kanban-cards(?:\?.*)?$/, (route) => route.fulfill({ json: { cards: [] } }));
    await page.route(/\/api\/server-campaigns\?/, (route) => route.fulfill({ json: { campaigns: [release] } }));
    await page.route(/\/api\/server-campaigns\/release$/, (route) => route.fulfill({ json: { campaign: release } }));
    await page.goto("/home");

    const todo = page.getByTestId("home-todo");
    await expect(todo.locator("li")).toHaveCount(3, { timeout: 15000 });
    await expect(todo.locator("li").first()).toContainText("승인 대기");
    await expect(todo).toContainText("결정 포럼에서 승인해요");
    if (isMobile) {
      await expect(todo.getByRole("button")).toHaveCount(0);
    } else {
      await expect(todo.getByRole("button", { name: "승인" })).toBeDisabled();
      await todo.getByRole("button", { name: "판단하기" }).click();
      await expect(page).toHaveURL(/\/campaigns\?mode=server&campaign=release&node=A$/);
    }
  });

  test("home: a working agent's live turn shows as a kit monitor without controls on desktop, a row on phones", async ({ page }, testInfo) => {
    await mockHomeSources(page);
    await page.route(/\/api\/agents\/agent-ada\/turn$/, (route) => route.fulfill({ json: {
      agent_id: "agent-ada", status: "working", started_at: new Date(Date.now() - 95_000).toISOString(), updated_at: new Date().toISOString(),
      provider: "codex", current_tool_line: "npm run build", tool_events: [{ kind: "tool", status: "running", tool_name: "Bash", summary: "", line: "" }], tool_count: 1,
    } }));
    await page.goto("/home");

    const monitor = page.getByTestId("home-agent-monitor-agent-ada");
    if (testInfo.project.name === "mobile") {
      await expect(page.getByTestId("home-agents").locator("li button", { hasText: "아다 대시보드" })).toBeVisible({ timeout: 15000 });
      await expect(monitor).toHaveCount(0);
      return;
    }
    await expect(monitor).toContainText("npm run build", { timeout: 15000 });
    await expect(monitor).toContainText(/1:3\d/);
    // Fold and the log link to the agent detail only; no pause, cancel or approval without a server action behind them.
    await expect(monitor.getByRole("button")).toHaveCount(2);
    await monitor.getByRole("button", { name: "로그" }).click();
    await expect(page.getByRole("dialog", { name: /에이전트 상세/ })).toBeVisible();
    await expect(page.getByTestId("home-agents").locator("li button", { hasText: "아다 대시보드" })).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
    // The kit texts follow the English UI.
    await page.route(/\/api\/settings$/, (route) => route.fulfill({ json: { language: "en", theme: "dark" } }));
    await page.reload();
    await expect(monitor.getByRole("button", { name: "Log" })).toBeVisible({ timeout: 15000 });
    await expect(monitor).toContainText("Running");
    await expect(page.getByTestId("home-usage").getByRole("group", { name: "Limits by provider" })).toBeVisible();
  });

  test("home: on a phone a failed session list reads unknown where an empty one reads resting", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await mockHomeSources(page);
    await page.route(/\/api\/agents(?:\?.*)?$/, (route) => route.fulfill({ json: { agents: MOCK_AGENTS.map((agent) => ({ ...agent, status: "idle" })) } }));
    const card = page.getByTestId("home-agents");
    await page.goto("/home");
    await expect(card).toContainText("모두 쉬는 중이에요.", { timeout: 15000 });
    await expect(card).toContainText("세션 없음 2");

    await page.route(/\/api\/dispatched-sessions(?:\?.*)?$/, (route) => route.fulfill({ status: 503, json: { error: "unavailable" } }));
    await page.reload();
    await expect(card).toContainText("세션을 읽지 못해 상태는 확인 불가예요.", { timeout: 15000 });
    await expect(card).toContainText("확인 불가 2");
    await card.getByRole("button", { name: "2명 더 보기" }).click();
    await expect(card.locator("li button").first()).toContainText("확인 불가");
  });

  test("stats: dedicated route exposes range controls and key widgets", async ({ page }) => {
    await page.goto("/stats");

    await expect(page.getByTestId("stats-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("stats-range-controls")).toBeVisible();
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("stats-range-30d")).toHaveAttribute("aria-checked", "false");
    await expect(page.getByTestId("stats-range-90d")).toHaveAttribute("aria-checked", "false");

    await expect(page.getByTestId("stats-summary-total-tokens")).toBeVisible();
    await expect(page.getByTestId("stats-summary-api-spend")).toBeVisible();
    await expect(page.getByTestId("stats-summary-cache-saved")).toBeVisible();
    await expect(page.getByTestId("stats-summary-cache-hit")).toBeVisible();
    await expect(page.getByTestId("stats-daily-token-chart")).toBeVisible();
    await expect(page.getByTestId("stats-model-share")).toBeVisible();
    await expect(page.getByTestId("stats-skill-usage")).toBeVisible();
    // One distribution chart, no leaderboard, no per-agent spend mix and no account switch.
    for (const removed of ["stats-provider-share", "stats-agent-leaderboard", "stats-agent-cost", "stats-agent-quality"]) {
      await expect(page.getByTestId(removed)).toHaveCount(0);
    }
    await expect(page.getByTestId("stats-claude-accounts").getByRole("button", { name: /^(Switch|Active)$/ })).toHaveCount(0);

    await page.getByTestId("stats-range-7d").click();
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("stats-range-30d")).toHaveAttribute("aria-checked", "false");

    await page.getByTestId("stats-range-90d").click();
    await expect(page.getByTestId("stats-range-90d")).toHaveAttribute("aria-checked", "true");
    await expect(page.getByTestId("stats-range-7d")).toHaveAttribute("aria-checked", "false");
  });

  test("stats: weekly chart fits and longer series support horizontal keyboard navigation", async ({ page }, testInfo) => {
    await page.route(/\/api\/token-analytics(?:\?.*)?$/, async (route) => {
      const period = new URL(route.request().url()).searchParams.get("period") === "30d" ? "30d" : "7d";
      const data = buildMockTokenAnalytics(period);
      if (period === "30d") data.daily = Array.from({ length: 30 }, (_, index) => ({ ...data.daily[index % 7], date: `2026-10-${String(index + 1).padStart(2, "0")}` }));
      await route.fulfill({ json: data });
    });
    await page.goto("/stats");
    const chart = page.getByTestId("stats-daily-token-scroll");
    await chart.scrollIntoViewIfNeeded();
    await expect(chart.locator("svg.recharts-surface")).toBeVisible();
    await expect.poll(() => chart.evaluate((el) => el.scrollWidth <= el.clientWidth + 1)).toBe(true);
    await page.getByTestId("stats-range-30d").click();
    await expect(page.getByText("날짜가 가려지면 차트를 좌우로 움직여 확인하세요.", { exact: true })).toBeVisible();
    if (testInfo.project.name === "mobile") {
      await expect.poll(() => chart.evaluate((el) => el.scrollWidth > el.clientWidth)).toBe(true);
      await chart.focus();
      await page.keyboard.press("ArrowRight");
      await expect.poll(() => chart.evaluate((el) => el.scrollLeft)).toBeGreaterThan(0);
    }
    await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  });

  test("stats: failed analytics stops the loading placeholder without reporting zero", async ({ page }) => {
    await page.route(/\/api\/token-analytics(?:\?.*)?$/, (route) =>
      route.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ error: "Aggregation unavailable" }) }),
    );
    await page.goto("/stats");
    await expect(page.getByText("일부 통계를 불러오지 못했어요.", { exact: true })).toBeVisible();
    await expect(page.getByTestId("stats-summary-total-tokens")).toContainText("—");
    await expect(page.getByTestId("stats-summary-total-tokens")).not.toContainText("…");
    await expect(page.getByTestId("stats-summary-api-spend")).toContainText("데이터 확인 불가");
  });

  test("stats: mobile pairs summary cards without horizontal overflow", async ({ page }, testInfo) => {
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
    expect(Math.abs(secondBox!.y - firstBox!.y)).toBeLessThanOrEqual(1);
    expect(secondBox!.x).toBeGreaterThan(firstBox!.x + firstBox!.width);
    await expectNoHorizontalOverflow(page);
  });

  test("home: today's routines show names, run results and later due times only", async ({ page }) => {
    const noon = new Date();
    noon.setHours(12, 0, 0, 0);
    const at = (hours: number, days = 0) => new Date(noon.getTime() + days * 86_400_000 + (hours - 12) * 3_600_000).toISOString();
    const base = MOCK_ROUTINES[1];
    await page.clock.setFixedTime(noon);
    // Same purpose for every routine, so only the names tell the rows apart.
    await page.route(/\/api\/routines(?:\?.*)?$/, (route) => route.fulfill({ json: { routines: [
      { ...base, id: "r-fail", name: "fail-check", next_due_at: at(9, 1), last_run: { ...MOCK_ROUTINES[0].last_run, status: "failed", started_at: at(9) } },
      { ...base, id: "r-sent", name: "sent-brief", next_due_at: at(6, 1), last_run: { ...MOCK_ROUTINES[0].last_run, status: "succeeded", action: "agent", started_at: at(6) } },
      { ...base, id: "r-twice", name: "twice-a-day", next_due_at: at(13), last_run: { ...MOCK_ROUTINES[0].last_run, status: "succeeded", action: "complete", started_at: at(11) } },
      { ...base, id: "r-later", name: "evening-run", next_due_at: at(18), last_run: null },
      { ...base, id: "r-old", name: "old-run", next_due_at: at(9, 1), last_run: { ...MOCK_ROUTINES[0].last_run, status: "succeeded", started_at: at(9, -1) } },
    ] } }));
    await page.goto("/home");

    const card = page.getByTestId("home-routines");
    await expect(card.locator("[data-testid^='home-routine-']")).toHaveCount(5, { timeout: 15000 });
    const rows = await card.locator("[data-testid^='home-routine-']").evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-testid")));
    expect(rows).toEqual(["home-routine-r-sent:run", "home-routine-r-fail:run", "home-routine-r-twice:run", "home-routine-r-twice:due", "home-routine-r-later:due"]);
    await expect(card.getByTestId("home-routine-r-sent:run")).toContainText(/sent-brief.*보냄/);
    await expect(card.getByTestId("home-routine-r-fail:run")).toContainText(/fail-check.*실패/);
    await expect(card.getByTestId("home-routine-r-twice:run")).toContainText(/11:00.*twice-a-day.*끝남/);
    await expect(card.getByTestId("home-routine-r-twice:due")).toContainText(/13:00.*twice-a-day.*예정/);
    await expect(card).toContainText("5개 중 3개 끝남");
    await expectNoHorizontalOverflow(page);
  });

  test("routines: page lists jobs chronologically and the detail leads with thread and result", async ({ page }, testInfo) => {
    await page.goto("/routines");

    await expect(page.getByTestId("routines-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByTestId("routines-filter-controls")).toBeVisible();
    await expect(page.getByTestId("routine-row-routine-soon")).toBeVisible({ timeout: 15000 });

    const rowIds = await page
      .getByTestId("routines-timeline-list")
      .locator("[data-testid^='routine-row-']")
      .evaluateAll((rows) => rows.map((row) => row.getAttribute("data-testid")));
    expect(rowIds).toEqual([
      "routine-row-routine-soon",
      "routine-row-routine-late",
      "routine-row-routine-manual",
    ]);
    await expect(page.getByText("매일 13:00")).toBeVisible();
    await expect(page.getByText("수동 실행")).toBeVisible();
    // Slug stays visible as secondary text under the plain-language title.
    await expect(page.getByTestId("routine-row-routine-soon")).toContainText("오부장 프로필 점검");

    await page.getByTestId("routine-row-routine-late").click();
    // Wide screens show the detail beside the list; below 900px it opens as a sheet.
    const drawer = testInfo.project.name === "desktop" ? page.getByTestId("routine-detail-pane") : page.getByRole("dialog");
    const threadLink = drawer.getByTestId("routine-thread-link");
    await expect(threadLink).toHaveAttribute("href", "https://discord.com/channels/1469870512812462284/1505086366654926908");
    await expect(drawer.getByTestId("routine-last-result")).toContainText("Profile probe failed");
    await expect(drawer.getByTestId("routine-last-result")).not.toContainText("**");
    await expect(drawer.getByRole("button", { name: /펼치기|Show more/ })).toBeVisible();
    for (const label of ["대체 담당", "제한 시간", "고급 정보"]) {
      await expect(drawer.getByText(label, { exact: true })).toBeVisible();
    }
    await expect(drawer.getByText(/^(FALLBACK|TIMEOUT|SCRIPT_REF|DISCORD_THREAD_ID|CHECKPOINT|fallback|timeout)$/)).toHaveCount(0);

    // The thread link and latest result come before the field grid, so they are the first things in view.
    const [linkBox, fieldBox] = await Promise.all([
      threadLink.boundingBox(),
      drawer.getByText("대체 담당", { exact: true }).boundingBox(),
    ]);
    expect(linkBox!.y).toBeLessThan(fieldBox!.y);
    await expectNoHorizontalOverflow(page);
  });

  test("agents: default execution device requires save and survives reopening", async ({ page }, testInfo) => {
    const pageErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    await mockAgentsHubApis(page);
    let saved: string | null = null;
    const writes: unknown[] = [];
    await page.route(/\/api\/agents\/agent-ada\/execution-node$/, async (route) => {
      if (route.request().method() === "PUT") {
        const body = route.request().postDataJSON() as { default_node_id: string | null };
        writes.push(body);
        saved = body.default_node_id;
      }
      await route.fulfill({ json: { default_node_id: saved, routing_enforced: true } });
    });
    await page.goto("/agents");
    // Phones always use the list; the grid/list switch exists on desktop only.
    if (testInfo.project.name === "desktop") await page.getByRole("radio", { name: /리스트|List/ }).click();
    await page.getByTestId("agents-card-agent-ada").click();
    // Desktop shows the detail beside the list; phones open it as a full-screen dialog.
    const dialog = testInfo.project.name === "desktop"
      ? page.getByRole("region", { name: /에이전트 상세|Agent details/ })
      : page.getByRole("dialog", { name: /에이전트 상세|Agent details/ });
    const device = dialog.getByLabel(/우선 실행 장비|Preferred execution device/);
    const save = dialog.getByRole("button", { name: /^(저장|Save)$/ });
    await expect(device).toBeEnabled();
    await expect(device).toHaveValue("");
    await expect(device.locator('option[value="fixture-hub"]')).toContainText(/(?:허브|Hub) · macOS/);
    await expect(device.locator('option[value="windows-1"]')).toContainText(/(?:실행 노드|Runner) · Windows/);
    await device.selectOption("windows-1");
    expect(writes).toEqual([]);
    await save.click();
    await expect(dialog.getByRole("status")).toContainText(/새 세션부터|new sessions/);
    expect(writes).toEqual([{ default_node_id: "windows-1" }]);
    await page.getByRole("button", { name: /닫기|Close|목록으로|Back to list/ }).click();
    await page.getByTestId("agents-card-agent-ada").click();
    await expect(device).toHaveValue("windows-1");
    await expect(save).toBeDisabled();
    await device.selectOption("linux-2");
    await expect(dialog.getByText(/선택한 장비의 실행 준비|The selected device is not ready/)).toBeVisible();
    expect(writes).toHaveLength(1);
    await device.selectOption("");
    await save.click();
    await expect(dialog.getByRole("status")).toContainText(/새 세션부터|new sessions/);
    expect(writes).toEqual([{ default_node_id: "windows-1" }, { default_node_id: null }]);
    await expectNoHorizontalOverflow(page);
    expect(pageErrors).toEqual([]);
    await page.screenshot({ path: testInfo.outputPath("agent-execution-device.png"), fullPage: true });
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
    await expect(page.getByRole("img", { name: /아다 대시보드|Ada Dashboard/ }).first()).toBeVisible();

    await page.getByRole("radio", { name: /리스트|List/ }).click();
    await expect(page.getByTestId("agents-view-list")).toBeVisible();

    await page.getByTestId("agents-card-agent-ada").click();
    const agentDetailDialog = page.getByRole("region", { name: /에이전트 상세|Agent details/ });
    await expect(agentDetailDialog).toBeVisible();
    await expect(agentDetailDialog).toContainText("Ada Dashboard");
    await expect(agentDetailDialog.getByText(/Lv\.\s?\d|XP/)).toHaveCount(0);
    await page.getByRole("button", { name: /닫기|Close/ }).click();

    // Header actions stay put while the tab changes.
    const addButton = page.getByRole("button", { name: /에이전트 추가|Add agent/ });
    const before = await addButton.boundingBox();

    await page.getByTestId("agents-tab-button-departments").click();
    await expect(page.getByTestId("agents-departments-tab")).toBeVisible();
    await expect(page.getByTestId("agents-department-card-dept-platform")).toBeVisible();
    expect(await addButton.boundingBox()).toEqual(before);

    await page.getByTestId("agents-tab-button-skills").click();
    await expect(page.getByPlaceholder(/스킬 검색|Search skills/).first()).toBeVisible();
    expect(await addButton.boundingBox()).toEqual(before);
    await expect(page.getByTestId("agents-tab-button-backlog")).toHaveCount(0);
  });

  test("agents: old meetings link opens the skills tab", async ({ page }) => {
    await mockAgentsHubApis(page);
    await page.goto("/meetings");
    await expect(page).toHaveURL(/\/agents$/);
    await expect(page.getByTestId("agents-tab-button-skills")).toHaveAttribute("aria-selected", "true");
  });

  test("connections: server status carries the old ops signals and bottlenecks", async ({ page }, testInfo) => {
    await mockOpsHealthApi(page, () => ({
      ...MOCK_OPS_HEALTH_BASE,
      degraded_reasons: [...MOCK_OPS_HEALTH_BASE.degraded_reasons, "tui_o:boundary_pending:1509350778043895902"],
    }));
    await page.goto("/settings?category=connections");

    await expect(page.getByTestId("setting-row-health.status")).toContainText("주의");
    await expect(page.getByTestId("setting-row-health.dispatch_outbox")).toContainText("7건");
    await page.getByTestId("setting-row-health.status").click();
    const facts = page.getByTestId("settings-facts-health.status");
    for (const label of ["후처리 대기", "전달 지연", "처리 대기", "감시 중"]) await expect(facts).toContainText(label);
    await expect(facts).toContainText("복구 시간4분 0초");
    await expect(facts).toContainText("가동 시간4시간 0분");
    await expect(facts.getByText("프로바이더 연결 끊김")).toBeVisible();

    // Internal keys become readable titles; the raw key stays as a tooltip only.
    const rawKey = "tui_o:boundary_pending:1509350778043895902";
    await expect(facts.getByText("TUI 출력 마무리 대기")).toHaveAttribute("title", rawKey);
    await expect(page.getByText(/Tui O:|Boundary Pending/)).toHaveCount(0);
    for (const english of [/^LIVE$/, /uptime/, /boot snapshot/, /provider 추적/, /회의 타임라인/]) {
      await expect(page.getByText(english)).toHaveCount(0);
    }
    if (testInfo.project.name === "mobile") await page.keyboard.press("Escape");
    await expectNoHorizontalOverflow(page);
  });

  test("connections: a tailnet device without detail access sees the summary, not zeros or error toasts", async ({ page }) => {
    await mockOpsHealthApi(page, () => ({ status: "healthy", version: "2.4.1", db: true }));
    let detailCalls = 0;
    await page.route(/\/api\/health\/detail$/, (route) => {
      detailCalls += 1;
      return route.fulfill({ status: 403, json: { error: "auth_token required for non-loopback host" } });
    });
    await page.goto("/settings?category=connections");

    await expect(page.getByTestId("setting-row-health.providers")).toContainText("이 기기에서는 확인할 수 없어요");
    await expect(page.getByTestId("setting-row-health.dispatch_outbox")).toContainText("—");
    await page.getByTestId("setting-row-health.status").click();
    await expect(page.getByTestId("settings-facts-health.status")).toContainText("서버가 도는 맥");
    await expect(page.getByText(/API 오류/)).toHaveCount(0);
    // One probe per page load; polling reuses the public summary after the 403.
    await page.waitForTimeout(6_000);
    expect(detailCalls).toBe(1);
  });

  test("connections: cluster controls distinguish readiness, bound output and stale state", async ({ page }, testInfo) => {
    await page.route(/\/api\/settings$/, route => route.fulfill({ json: {
      language: testInfo.project.name === "desktop" ? "en" : "ko", theme: "dark",
    } }));
    await page.route(/\/api\/prompt-manifest\/retention$/, route => route.fulfill({ json: {
      total_stored_bytes: 0, total_original_bytes: 0, truncated_count: 0,
      manifest_count: 0, layer_count: 0, oldest_full_content_at: null, retention_horizon_at: null,
      retention_days: 30, per_layer_max_bytes_adk_provided: 65536, per_layer_max_bytes_user_derived: 8192,
      enabled: true, restart_required_for_config_changes: true, config_applied_at: "2026-09-22T00:00:00Z",
      config_source: "fixture", hot_reload: false,
    } }));
    let failRefresh = false;
    let clusterEnabled = true;
    let stopCount = 0;
    await page.route(/\/api\/cluster\/nodes$/, async route => {
      if (failRefresh) return route.fulfill({ status: 503, json: { error: "fixture unavailable" } });
      const now = Date.now();
      await route.fulfill({ json: {
        cluster: { enabled: clusterEnabled, local_instance_id: "fixture-hub" },
        nodes: ["windows-worker", "linux-worker"].map((id, index) => ({
          instance_id: id, status: "online", effective_role: index ? "worker" : "runner", active_dispatch_count: 0,
          execution_active: 1, execution_occupied: 2,
          capabilities: { execution_capacity: { version: 1, slots: 2 }, execution_readiness: {
            os: index ? "linux" : "windows", arch: "x86_64", runtime_profile: index ? "full" : "runner",
            observed_at_ms: now, expires_at_ms: now + 120_000, backends: ["process"],
          } },
          execution_readiness: { providers: { codex: { eligible: index === 0,
            reasons: index ? ["provider_cli_unavailable"] : [],
          } } },
          forwarding_diagnostics: { advertised: true, configured: true, trust_validated: true,
            reachability_verified: true, reachability_status: "verified", expires_at_ms: now + 45_000,
          },
        })),
      } });
    });
    await page.route(/\/api\/dispatched-sessions$/, route => route.fulfill({ json: { sessions: [{
      id: 17, session_key: "codex/token/windows:fixture", instance_id: "windows-worker",
      name: "Windows build", provider: "codex", status: stopCount ? "disconnected" : "working",
    }] } }));
    await page.route(/\/api\/sessions\/17\/output\?lines=100$/, route => route.fulfill({ json: {
      recent_output: `{"text":"worker output 한글 ${"a".repeat(200)}"}`, backend: "process",
      available: true, unavailable_reason: null, output_format: "jsonl", captured_at_ms: Date.now(),
    } }));
    await page.route(/\/api\/sessions\/[^/]+\/force-kill$/, async route => {
      expect(route.request().method()).toBe("POST");
      expect(route.request().postDataJSON().retry).toBe(false);
      stopCount++;
      await route.fulfill({ json: { ok: true } });
    });
    await page.goto("/settings?category=connections");
    const panel = page.getByTestId("cluster-nodes-panel");
    await expect(panel.getByText("Windows / x86_64", { exact: false })).toBeVisible();
    await expect(panel.getByText(/(?:실행 노드.*실행 전용|Runner.*Execution only)/)).toBeVisible();
    await expect(panel.getByText(/(?:실행 노드.*전체 기능|Runner.*Full features)/)).toBeVisible();
    await expect(panel.getByText(/실행 용량 대기|Waiting for capacity/)).toBeVisible();
    await expect(panel.getByText(/CLI 실행 불가|CLI unavailable/)).toBeVisible();
    await panel.getByRole("button", { name: /출력 보기|View output/ }).click();
    await expect(panel.getByText(/worker output 한글/)).toBeVisible();
    await panel.getByRole("button", { name: /실행 중지|Stop execution/, exact: true }).click();
    expect(stopCount).toBe(0);
    await panel.getByRole("button", { name: /중지 확인|Confirm stop/ }).click();
    await expect.poll(() => stopCount).toBe(1);
    await expect(panel.getByText(/disconnected/, { exact: false })).toBeVisible();
    await expectNoHorizontalOverflow(page);
    await page.screenshot({ path: testInfo.outputPath("cluster-nodes.png"), fullPage: true });
    failRefresh = true;
    await expect(panel.getByRole("button", { name: /실행 중지|Stop execution/, exact: true })).toBeDisabled({ timeout: 20_000 });
    await expect(panel.getByText(/노드 갱신 실패|Node refresh failed/)).toBeVisible();
    await expect(panel.getByText(/worker output 한글/)).toBeVisible();
    await expectNoHorizontalOverflow(page);
    failRefresh = false;
    clusterEnabled = false;
    await expect(panel.getByText(/이 컴퓨터 · 단독 운영|This computer · Standalone/)).toBeVisible({ timeout: 15_000 });
    await expect(panel.locator("article")).toHaveCount(0);
    await expect(panel.getByRole("button", { name: /실행 중지|Stop execution/, exact: true })).toHaveCount(0);
  });

  test("connections: server status refreshes while the category is open", async ({ page }) => {
    let currentHealth: Record<string, unknown> = {
      ...MOCK_OPS_HEALTH_BASE,
      status: "healthy",
      degraded_reasons: [],
      dispatch_outbox: { pending: 1, retrying: 0, permanent_failures: 0, oldest_pending_age: 12 },
    };
    await mockOpsHealthApi(page, () => currentHealth);
    await page.goto("/settings?category=connections");

    const status = page.getByTestId("setting-row-health.status");
    await expect(status).toContainText("정상");
    await expect(page.getByTestId("setting-row-health.dispatch_outbox")).toContainText("1건");

    currentHealth = {
      ...currentHealth,
      status: "degraded",
      degraded_reasons: ["dispatch_outbox_oldest_pending_age:45"],
      dispatch_outbox: { pending: 4, retrying: 1, permanent_failures: 0, oldest_pending_age: 45 },
    };
    // The ops page waited for WS events; the open connections category polls instead.
    await expect(status).toContainText("주의", { timeout: 10_000 });
    await expect(page.getByTestId("setting-row-health.dispatch_outbox")).toContainText("4건");
  });

  test("settings route is reachable from the desktop shell", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    await page
      .getByTestId("app-sidebar-nav")
      .getByRole("link", { name: /^설정$|^Settings$/ })
      .click();
    await expect(page).toHaveURL(/\/settings(\?.*)?$/);
    await expect(page.getByTestId("topbar")).toContainText(/설정|Settings/);
    // Ops left the menu; its status now lives in the connections category.
    await page.getByTestId("settings-category-connections").click();
    await expect(page).toHaveURL(/\/settings\?category=connections$/);
    await expect(page.getByTestId("setting-row-health.status")).toBeVisible();
  });

  test("bootstrap: the loading screen shows text without emoji", async ({ page }) => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    await page.route(/\/api\/settings$/, async (route) => { await gate; await route.fallback(); });
    await page.goto("/home");
    await expect(page.getByTestId("bootstrap-loading")).toHaveText(/AgentDesk 대시보드 로딩 중|Loading AgentDesk Dashboard/);
    await expect(page.locator("body")).not.toContainText("🐾");
    release();
    await expect(page.getByTestId("topbar")).toBeVisible();
  });

  test("command palette: departments get an icon instead of their emoji and ops is not offered", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop keyboard shortcut coverage");
    await page.route(/\/api\/departments(?:\?.*)?$/, (route) => route.fulfill({
      json: { departments: [{ ...MOCK_AGENT_DEPARTMENTS[0], icon: "📁" }] },
    }));
    await page.goto("/home");
    await expect(page.getByTestId("topbar")).toBeVisible();
    await page.keyboard.press("ControlOrMeta+k");
    const palette = page.getByRole("dialog", { name: /명령 팔레트|Command Palette/ });
    await expect(palette.getByRole("option", { name: /^(통계|Stats)$/ })).toHaveCount(1);
    await expect(palette.getByRole("option", { name: /^(운영|Ops)$/ })).toHaveCount(0);

    await palette.getByPlaceholder(/검색|Search/).fill("플랫폼");
    const department = palette.getByRole("option", { name: /^플랫폼/ });
    await expect(department).toBeVisible();
    await expect(palette).not.toContainText("📁");
    // Rows run through their own onSelect only, so no assistant row is offered.
    await expect(palette.getByRole("option")).toHaveCount(1);
    await department.click();
    await expect(page).toHaveURL(/\/agents$/);
  });

  test("ops: the /control bookmark lands on the settings connections category", async ({ page }) => {
    await mockOpsHealthApi(page, () => MOCK_OPS_HEALTH_BASE);
    await page.goto("/control");
    await expect(page).toHaveURL(/\/settings\?category=connections$/);
    await expect(page.getByTestId("setting-row-health.status")).toBeVisible();
  });

  test("audit: agents without a channel leave the prompt tab empty and the anchor tab keeps its URL key", async ({ page }) => {
    await page.goto("/audit");
    await expect(page.getByTestId("audit-no-agents")).toBeVisible();
    await page.getByRole("tab", { name: /메멘토 앵커|Memento anchors/ }).click();
    await expect(page).toHaveURL(/\/audit\?tab=anchors$/);
    await expect(page.getByTestId("audit-panel-anchors")).toBeVisible();
    await expectNoHorizontalOverflow(page);
  });

  test("settings: search narrows the catalog and the copy stays plain", async ({ page }, testInfo) => {
    const runtime = { maxRetries: 3, maxEntryRetries: 3 };
    await page.route(/\/api\/settings\/runtime-config$/, route => route.fulfill({ json: { current: runtime, defaults: runtime } }));
    await page.goto("/settings?settingsPanel=runtime");

    await expect(page.getByTestId("settings-page")).toBeVisible({ timeout: 15000 });
    if (testInfo.project.name === "desktop") await expect(page.getByTestId("settings-category-workflow")).toHaveAttribute("aria-current", "true");
    else await expect(page.getByTestId("settings-phone-title")).toHaveText("작업 흐름");
    await expect(page.getByTestId("setting-row-maxRetries")).toContainText("작업 재시도 횟수");
    await expect(page.getByText(/용어 빠른 정의|Quick term definitions/)).toHaveCount(0);
    await expect(page.getByText("kv_meta")).toHaveCount(0);

    await page.getByTestId("settings-search-input").fill("대기열 재시도");
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(1);
    await expectNoHorizontalOverflow(page);
    await page.getByTestId("settings-search-input").fill("");
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(2);
  });

  test("settings: runtime fetch errors do not expose empty editable defaults", async ({ page }) => {
    await page.route(/\/api\/settings\/runtime-config$/, route => route.fulfill({ status: 503, json: { error: "unavailable" } }));
    await page.goto("/settings?settingsPanel=runtime");
    await expect(page.getByRole("alert")).toContainText(/불러오지 못했어요|could not be loaded/);
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(0);
  });

  test("settings: mobile page remains scrollable", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await page.goto("/settings");

    const settingsPage = page.getByTestId("settings-page");
    await expect(settingsPage).toBeVisible({ timeout: 15000 });

    const heights = await page.getByRole("navigation", { name: /분류|Categories/ }).getByRole("button")
      .evaluateAll((nodes) => nodes.map((node) => node.getBoundingClientRect().height));
    expect(heights.length).toBeGreaterThanOrEqual(7);
    expect(Math.min(...heights)).toBeGreaterThanOrEqual(44);

    const before = await settingsPage.evaluate((node) => ({
      clientHeight: node.clientHeight,
      scrollHeight: node.scrollHeight,
      scrollTop: node.scrollTop,
    }));

    expect(before.scrollHeight).toBeGreaterThanOrEqual(before.clientHeight);
    if (before.scrollHeight <= before.clientHeight + 1) return;

    await settingsPage.evaluate((node) => {
      node.scrollTop = node.scrollHeight;
      node.dispatchEvent(new Event("scroll"));
    });

    const after = await settingsPage.evaluate((node) => node.scrollTop);
    expect(after).toBeGreaterThan(0);
  });

  test("settings: mobile pipeline editor uses the detailed editor only", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Mobile-only test");
    await mockSettingsPipelineEditorApis(page);
    await page.goto("/settings?settingsPanel=pipeline");

    await expect(page.getByTestId("settings-page")).toBeVisible({ timeout: 15000 });
    await expect(page.getByText(/세부 흐름 편집기|Detailed workflow editor/).first()).toBeVisible();
    await expect(page.getByTestId("pipeline-flow-canvas")).toHaveCount(0);
    await page.getByRole("button", { name: /고급 편집|Advanced editing/ }).click();
    await expect(page.getByText(/FSM 비주얼 에디터|FSM visual editor/)).toHaveCount(0);
    await expect(page.getByTestId("fsm-mobile-selector")).toHaveCount(0);
    await expect(page.getByTestId("pipeline-flow-canvas")).toBeVisible();
    await expect(page.getByTestId("pipeline-selection-title")).toContainText(/상태 · backlog|State · backlog/);
    await expect(page.locator(".react-flow__controls")).toHaveCount(0);
  });

  test("settings: pipeline editor keeps cached content visible while refreshing", async ({ page }) => {
    await page.addInitScript(({ cacheKey, cacheValue }) => {
      window.localStorage.setItem(cacheKey, JSON.stringify(cacheValue));
    }, {
      cacheKey: PIPELINE_VISUAL_CACHE_KEY,
      cacheValue: {
        version: 1,
        entries: {
          "itismyfield/AgentDesk::repo::repo": {
            repo: "itismyfield/AgentDesk",
            level: "repo",
            agentId: null,
            updatedAtMs: Date.now(),
            snapshot: {
              pipeline: MOCK_SETTINGS_FSM_PIPELINE,
              layers: { default: true, repo: false, agent: false },
              rawOverride: {
                states: MOCK_SETTINGS_FSM_PIPELINE.states,
                transitions: MOCK_SETTINGS_FSM_PIPELINE.transitions,
                gates: MOCK_SETTINGS_FSM_PIPELINE.gates,
                hooks: MOCK_SETTINGS_FSM_PIPELINE.hooks,
                events: MOCK_SETTINGS_FSM_PIPELINE.events,
                clocks: MOCK_SETTINGS_FSM_PIPELINE.clocks,
                phase_gate: MOCK_SETTINGS_FSM_PIPELINE.phase_gate,
              },
              repoStages: MOCK_SETTINGS_PIPELINE_STAGES,
            },
          },
        },
      },
    });
    await mockSettingsPipelineEditorApisWithRefreshDelay(page);
    await page.goto("/settings?settingsPanel=pipeline");

    await expect(page.getByTestId("settings-page")).toBeVisible({ timeout: 15000 });
    await page.getByRole("button", { name: /고급 편집|Advanced editing/ }).click();
    await expect(page.getByTestId("pipeline-selection-title")).toBeVisible({ timeout: 1500 });
    await expect(page.getByTestId("pipeline-refresh-indicator")).toBeVisible();
    await expect(page.getByTestId("pipeline-selection-title")).toContainText(/상태 · backlog|State · backlog/);
    await expect(page.locator(".react-flow__controls")).toHaveCount(0);

    const canvas = page.getByTestId("pipeline-flow-canvas");
    const viewport = page.locator(".react-flow__viewport").first();
    const beforeWheelTransform = await viewport.evaluate((node) =>
      node.getAttribute("transform") ?? window.getComputedStyle(node).transform,
    );
    const canvasBox = await canvas.boundingBox();
    expect(canvasBox).not.toBeNull();
    await page.mouse.move(canvasBox!.x + canvasBox!.width / 2, canvasBox!.y + canvasBox!.height / 2);
    await page.mouse.wheel(0, 500);
    await page.waitForTimeout(120);
    const afterWheelTransform = await viewport.evaluate((node) =>
      node.getAttribute("transform") ?? window.getComputedStyle(node).transform,
    );
    expect(afterWheelTransform).toBe(beforeWheelTransform);

    await expect(page.getByTestId("settings-audit-notes")).toHaveCount(0);
    await expect(page.getByText(/audit 노트|Audit notes/)).toHaveCount(0);
    await expect(page.getByText("kv_meta")).toHaveCount(0);
    await expect(page.getByTestId("pipeline-refresh-indicator")).toBeHidden({ timeout: 3000 });
  });

  test("settings: machines show Hub and Runner details and honest connection states", async ({ page }, testInfo) => {
    const pageErrors: string[] = [];
    page.on("pageerror", error => pageErrors.push(error.message));
    await page.route(/\/api\/settings\/runtime-config$/, route => route.fulfill({ json: { current: {}, defaults: {} } }));
    await page.route(/\/api\/settings$/, route => route.fulfill({ json: {
      language: testInfo.project.name === "desktop" ? "en" : "ko", theme: "dark",
    } }));
    let fail = false;
    let enabled = true;
    let offline = false;
    let cpuUsage = 37.5;
    let includeRunner = true;
    await page.route(/\/api\/cluster\/machine-resources\/history\?/, route => {
      const now = Date.now();
      const instance_id = new URL(route.request().url()).searchParams.get("instance_id");
      return route.fulfill({ json: { instance_id, samples: Array.from({ length: 180 }, (_, index) => ({
        schema: 1, observed_at_ms: now - (180 - index) * 5_000, expires_at_ms: now - (180 - index) * 5_000 + 30_000, sample_interval_ms: 5_000,
        cpu: { model: "Example CPU", physical_cores: 8, logical_cores: 16, usage_percent: 35 + 20 * Math.sin(index / 8) },
        memory: { total_bytes: 32 * 1024 ** 3, used_bytes: (16 + Math.sin(index / 20)) * 1024 ** 3, available_bytes: (16 - Math.sin(index / 20)) * 1024 ** 3 },
        disks: [{ name: "Data", mount_point: instance_id === "runner-example" ? "C:\\" : "/", kind: "SSD", total_bytes: 1024 ** 4, used_bytes: 512 * 1024 ** 3, available_bytes: 512 * 1024 ** 3 }],
        gpus: [{ name: "Example GPU", usage_percent: 25 + 10 * Math.sin(index / 4), memory_used_bytes: 4 * 1024 ** 3, memory_total_bytes: 16 * 1024 ** 3, shared_memory: false }],
        network: { interface: "Ethernet", wired: true,
          received_bytes_per_sec: 2_048 + 1_024 * Math.sin(index / 6), transmitted_bytes_per_sec: 1_024 + 512 * Math.cos(index / 5) },
      })) } });
    });
    await page.route(/\/api\/cluster\/nodes$/, route => {
      if (fail) return route.fulfill({ status: 503, json: { error: "fixture unavailable" } });
      const now = Date.now();
      return route.fulfill({ json: {
        cluster: { enabled, local_instance_id: "hub-example", lease_ttl_secs: 30, heartbeat_interval_secs: 5 },
        nodes: (includeRunner ? ["hub-example", "runner-example"] : ["hub-example"]).map((id, index) => ({
          instance_id: id, hostname: index ? "Build runner" : "Control hub",
          status: index && offline ? "offline" : "online", role: index ? "runner" : "hub",
          effective_role: index ? "worker" : "leader", process_id: 100 + index,
          last_heartbeat_at: new Date(now).toISOString(), started_at: new Date(now - 60_000).toISOString(),
          api_base_url: `https://${id}.example.invalid`, labels: [index ? "build" : "control"],
          execution_active: 1, execution_occupied: 1, active_session_count: 1, active_dispatch_count: 0,
          capabilities: { machine_resources: {
            schema: 1, observed_at_ms: now, expires_at_ms: now + 30_000, sample_interval_ms: 5_000,
            cpu: { model: "Example CPU", physical_cores: 8, logical_cores: 16, usage_percent: cpuUsage },
            memory: { total_bytes: 32 * 1024 ** 3, used_bytes: 16 * 1024 ** 3, available_bytes: 16 * 1024 ** 3 },
            disks: [{ name: "Data", mount_point: index ? "C:\\" : "/", kind: "SSD", total_bytes: 1024 ** 4, used_bytes: 512 * 1024 ** 3, available_bytes: 512 * 1024 ** 3 }],
            gpus: [{ name: "Example GPU", usage_percent: 25, memory_used_bytes: 4 * 1024 ** 3, memory_total_bytes: 16 * 1024 ** 3, shared_memory: false }],
            network: { interface: "Ethernet", wired: true, received_bytes_per_sec: 4096, transmitted_bytes_per_sec: 2048 },
          }, execution_capacity: { version: 1, slots: 2 }, execution_readiness: {
            os: index ? "windows" : "linux", arch: "x86_64", runtime_profile: index ? "runner" : "full",
            observed_at_ms: now, expires_at_ms: now + 60_000, backends: ["process"],
            providers: { codex: { cli_installed: true, cli_usable: true }, antigravity: { cli_installed: false, cli_usable: false } },
          } },
          execution_readiness: { providers: { codex: { eligible: true, reasons: [] } } },
          forwarding_diagnostics: { advertised: true, configured: true, trust_validated: true,
            reachability_verified: true, expires_at_ms: now + 30_000 },
        })),
      } });
    });
    await page.goto("/settings?settingsPanel=machine");
    const panel = page.getByTestId("settings-machine-panel");
    await expect(panel).toBeVisible();
    const desktop = testInfo.project.name === "desktop";
    if (desktop) await expect(page.getByTestId("settings-category-system")).toHaveAttribute("aria-current", "true");
    else await expect(page.getByTestId("settings-phone-title")).toHaveText("시스템");
    await expect(panel.getByText(/온라인 2 \/ 전체 2|Online 2 \/ total 2/)).toBeVisible();
    const runner = page.getByTestId("machine-node-runner-example");
    await expect(runner.getByText("Runner", { exact: true }).first()).toBeVisible();
    await expect(runner.getByText(/Windows \/ x86_64/)).toBeVisible();
    await expect(runner.getByText(/연결 확인됨|Connection verified/, { exact: true })).toBeVisible();
    await expect(runner.getByText(/신규 실행 가능|Ready for new work/, { exact: true })).toBeVisible();
    await expect(runner.getByText("Example CPU", { exact: true })).toBeVisible();
    await expect(runner.getByText("Example GPU", { exact: true })).toBeVisible();
    await expect(runner.getByTestId("machine-trend-cpu").locator("svg")).toBeVisible();
    // Trend frames sit on the theme chart surface (--bg-1), not a fixed gray.
    const chartSurface = await page.evaluate(() => {
      const probe = document.createElement("div");
      probe.style.backgroundColor = "var(--bg-1)";
      document.body.append(probe);
      const color = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return color;
    });
    for (const kind of ["cpu", "memory", "disk", "gpu"]) {
      const chart = runner.getByTestId(`machine-trend-${kind}`);
      await expect(chart).toHaveCSS("background-color", chartSurface);
      await expect(chart.locator('[data-layer="line"]')).toHaveCount(1);
      await expect(chart.locator('[data-layer="area"]')).toHaveCount(1);
    }
    const networkChart = runner.getByTestId("machine-trend-network");
    await expect(networkChart).toHaveCSS("background-color", chartSurface);
    await expect(networkChart.locator('[data-layer="line"]')).toHaveCount(2);
    await expect(networkChart.locator('[data-layer="area"]')).toHaveCount(2);
    await expect(networkChart.locator('[data-layer="line"][data-series="primary"]')).toHaveAttribute("stroke", "#f472b6");
    await expect(networkChart.locator('[data-layer="area"][data-series="primary"]')).toHaveAttribute("fill-opacity", "0.22");
    await expect(networkChart.locator('[data-layer="line"][data-series="secondary"]')).toHaveAttribute("stroke-dasharray", "3 2");
    await expect(runner.getByRole("meter", { name: /CPU 사용률|CPU utilization/ })).toHaveAttribute("aria-valuenow", "37.5");
    await expect(runner.getByText("antigravity", { exact: true })).toHaveCount(0);
    await expect(runner.getByText(/설정된 역할|Configured role/, { exact: true })).toHaveCount(0);
    cpuUsage = 62.5;
    await panel.getByRole("button", { name: /상태 새로고침|Refresh status/ }).click();
    await expect(runner.getByRole("meter", { name: /CPU 사용률|CPU utilization/ })).toHaveAttribute("aria-valuenow", "62.5");
    await expectNoHorizontalOverflow(page);
    await page.screenshot({ path: testInfo.outputPath("machine-settings.png"), fullPage: true });
    await runner.getByRole("region", { name: /하드웨어 및 사용량|Hardware and utilization/ }).screenshot({ path: testInfo.outputPath("hardware-utilization.png") });
    if (desktop) await page.getByTestId("settings-category-workflow").click();
    else await page.getByRole("button", { name: "분류 목록으로" }).click();
    await expect(panel).toHaveCount(0);
    await page.goBack();
    await expect(panel).toBeVisible();
    await page.reload();
    await expect(panel).toBeVisible();
    await expect(runner.getByTestId("machine-trend-cpu").locator("svg")).toBeVisible();
    fail = true;
    await panel.getByRole("button", { name: /상태 새로고침|Refresh status/ }).click();
    await expect(panel.getByRole("alert")).toContainText(/마지막 조회|last snapshot/);
    await expect(runner.getByText(/연결 확인됨|Connection verified/, { exact: true })).toHaveCount(0);
    await expect(runner.getByText(/신규 실행 가능|Ready for new work/, { exact: true })).toHaveCount(0);
    await expect(runner.getByRole("meter", { name: /CPU 사용률|CPU utilization/ })).toHaveCount(0);
    fail = false;
    offline = true;
    await panel.getByRole("button", { name: /상태 새로고침|Refresh status/ }).click();
    await expect(runner.getByText(/오프라인|Offline/, { exact: true })).toBeVisible();
    includeRunner = false;
    await panel.getByRole("button", { name: /상태 새로고침|Refresh status/ }).click();
    await expect(panel.getByText(/온라인 1 \/ 전체 1|Online 1 \/ total 1/)).toBeVisible();
    enabled = false;
    await panel.getByRole("button", { name: /상태 새로고침|Refresh status/ }).click();
    await expect(panel.getByText(/단독 운영 중|runs standalone/)).toBeVisible();
    await expect(panel.locator("article")).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
    expect(pageErrors).toEqual([]);
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

  test("removed pages redirect into the new navigation", async ({ page }) => {
    for (const redirect of LEGACY_REDIRECTS) {
      await page.goto(redirect.path);
      await expect(page).toHaveURL(redirect.to);
    }
  });

});
