import { expect, test, type Locator, type Page, type TestInfo } from "@playwright/test";

// Settings IA checks at 1440x900 (desktop project) and 390x844 (mobile project).
// Set SETTINGS_SHOT_DIR to collect screenshots outside the test output folder.

// Shell fixtures copied from smoke.spec.ts so this file stays independent of it.
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

const NOW_SECONDS = Math.floor(Date.now() / 1000);
const GIB = 1024 ** 3;
const ALERT_CHANNEL_ID = "1234567890123456789";

const CONFIG_ENTRIES = [
  { key: "deadlock_manager_channel_id", value: "", category: "pipeline", label_ko: "데드락 매니저 채널 ID", label_en: "Deadlock Manager Channel ID", editable: true, restart_behavior: "persist-live-override" },
  { key: "agent_quality_monitoring_channel_id", value: ALERT_CHANNEL_ID, category: "quality", label_ko: "에이전트 품질 알림 채널 ID", label_en: "Agent Quality Alert Channel ID", editable: true },
  { key: "review_enabled", value: "true", category: "review", label_ko: "리뷰 활성화", label_en: "Review Enabled", editable: true, restart_behavior: "persist-live-override" },
  { key: "max_review_rounds", value: "3", default: "3", category: "review", label_ko: "최대 리뷰 라운드", label_en: "Max Review Rounds", editable: true, restart_behavior: "persist-live-override" },
  { key: "server_port", value: "8791", category: "system", label_ko: "서버 포트", label_en: "Server Port", editable: false, restart_behavior: "config-only" },
  { key: "requested_timeout_min", value: "45", default: "45", category: "timeout", label_ko: "요청됨 타임아웃 (분)", label_en: "Requested Timeout (min)", editable: true, restart_behavior: "persist-live-override" },
  { key: "in_progress_stale_min", value: "120", default: "120", category: "timeout", label_ko: "진행 중 정체 판정 (분)", label_en: "In-Progress Stale (min)", editable: true, restart_behavior: "persist-live-override" },
  { key: "context_compact_percent", value: "60", default: "60", category: "context", label_ko: "컨텍스트 compact 임계값 (%)", label_en: "Context Compact Threshold (%)", editable: true, restart_behavior: "persist-live-override" },
  { key: "context_compact_percent_codex", value: null, category: "context", label_ko: "Codex 컨텍스트 compact 임계값 (%)", label_en: "Codex Context Compact Threshold (%)", editable: true },
  { key: "context_compact_percent_claude", value: null, category: "context", label_ko: "Claude 컨텍스트 compact 임계값 (%)", label_en: "Claude Context Compact Threshold (%)", editable: true },
  { key: "context_compact_lower_bound_tokens", value: "300000", default: "300000", category: "context", label_ko: "컨텍스트 compact 최소 토큰", label_en: "Context Compact Lower Bound (tokens)", editable: true },
];

const RUNTIME_CONFIG = {
  maxRetries: 3,
  maxEntryRetries: 3,
  dispatchRateLimitGateEnabled: true,
  dispatchRateLimitGateDangerPct: 95,
  rateLimitWarningPct: 80,
  rateLimitDangerPct: 95,
  rateLimitStaleSec: 600,
};

const CONNECTORS = {
  connectors: [
    { id: "obsidian_agent_prompts", name: "Obsidian agent prompts", state: "ready", optional: true, env_var: "AGENTDESK_OBSIDIAN_AGENTS_SRC", source: "/Users/me/ObsidianVault/RemoteVault/adk-config/agents", reason: null, detail: "state=ready", setup_actions: [], capabilities: ["obsidian_agent_prompts"] },
    { id: "obsidian_skill_root", name: "Obsidian skill root", state: "ready", optional: true, env_var: "AGENTDESK_OBSIDIAN_SKILL_ROOT", source: "/Users/me/ObsidianVault/RemoteVault/99_Skills", reason: null, detail: "state=ready", setup_actions: [], capabilities: ["obsidian_skill_root"] },
    { id: "kakao_scheduled_delivery", name: "Kakao default-account delivery", state: "skipped", optional: true, env_var: "AGENTDESK_KAKAO_ENABLED", source: null, reason: "disabled", detail: "state=skipped reason=disabled", setup_actions: ["Set AGENTDESK_KAKAO_ENABLED=true after configuring Kakao credentials."], capabilities: ["kakao_friend_message", "kakao_self_message"] },
  ],
  summary: { ready: 2, skipped: 1, missing_config: 0, missing_path: 0, missing_provider: 0, invalid_config: 0, invalid: 0, total: 3, core_runtime_blocking: false },
};

const ONBOARDING_STATUS = {
  completed: true,
  guild_id: "1111111111111111111",
  owner_id: "2222222222222222222",
  bot_providers: { command: "claude", command2: "codex" },
};

const HEALTH = {
  status: "healthy",
  providers: [
    { name: "claude", connected: true, active_turns: 0, queue_depth: 0, sessions: 1, restart_pending: false, last_turn_at: null },
    { name: "codex", connected: true, active_turns: 0, queue_depth: 0, sessions: 1, restart_pending: false, last_turn_at: null },
  ],
};

const PROVIDER_PROFILES = {
  providers: [
    {
      id: "claude",
      default_home: "/Users/me/.claude",
      primary_profile_id: "default",
      accounts: [
        {
          id: "default",
          home: "/Users/me/.claude",
          bound_agents: ["agentdesk"],
          usage: { buckets: [
            { name: "5h", limit: 100, used: 62, utilization: 62, reset: NOW_SECONDS + 7_200 },
            { name: "7d", limit: 100, used: 40, utilization: 40, reset: NOW_SECONDS + 86_400 },
            { name: "7d Fable", limit: 100, used: 0, utilization: 0, reset: NOW_SECONDS + 86_400 },
          ], stale: false, unsupported: false, reason: null },
        },
        {
          id: "work",
          home: "/Users/me/.adk/provider-profiles/claude/work",
          usage: { buckets: [{ name: "5h", limit: 100, used: 97, utilization: 97, reset: NOW_SECONDS + 3_600 }], stale: false, unsupported: false, reason: null },
        },
      ],
    },
    {
      id: "qwen",
      default_home: "/Users/me/.qwen",
      accounts: [
        { id: "default", home: "/Users/me/.qwen", usage: { buckets: [], stale: false, unsupported: true, reason: "No Qwen rate-limit telemetry source is implemented yet." } },
      ],
    },
  ],
};

function machineResources(now: number, cpu: number, usedMemory: number) {
  return {
    schema: 1, observed_at_ms: now, expires_at_ms: now + 30_000, sample_interval_ms: 5_000,
    cpu: { model: "Apple M4 Max", physical_cores: 16, logical_cores: 16, usage_percent: cpu },
    memory: { total_bytes: 32 * GIB, used_bytes: usedMemory, available_bytes: 32 * GIB - usedMemory },
    disks: [{ name: "Macintosh HD", mount_point: "/", kind: "SSD", total_bytes: 1024 * GIB, used_bytes: 400 * GIB, available_bytes: 624 * GIB }],
    gpus: [],
    network: { interface: "en0", wired: false, received_bytes_per_sec: 4096, transmitted_bytes_per_sec: 2048 },
  };
}

function clusterNodes() {
  const now = Date.now();
  return {
    cluster: { enabled: true, local_instance_id: "hub-mac", lease_ttl_secs: 30, heartbeat_interval_secs: 5 },
    nodes: ["hub-mac", "runner-mac"].map((id, index) => ({
      instance_id: id, hostname: index ? "mac-mini" : "mac-book", status: "online", role: index ? "runner" : "hub",
      effective_role: index ? "worker" : "leader", process_id: 100 + index,
      last_heartbeat_at: new Date(now).toISOString(), started_at: new Date(now - 60_000).toISOString(),
      api_base_url: `https://${id}.example.invalid`, labels: [],
      execution_active: 0, execution_occupied: 0, active_session_count: 1, active_dispatch_count: 0,
      capabilities: {
        machine_resources: index ? machineResources(now, 22.5, 12 * GIB) : machineResources(now, 99.1, 31.2 * GIB),
        execution_capacity: { version: 1, slots: 2 },
        execution_readiness: {
          os: "macos", arch: "aarch64", runtime_profile: index ? "runner" : "full",
          observed_at_ms: now, expires_at_ms: now + 60_000, backends: ["process"],
          providers: { claude: { cli_installed: true, cli_usable: true } },
        },
      },
      execution_readiness: { providers: { claude: { eligible: true, reasons: [] } } },
      forwarding_diagnostics: { advertised: true, configured: true, trust_validated: true, reachability_verified: true, expires_at_ms: now + 30_000 },
    })),
  };
}

type Captured = { method: string; url: string; body: unknown };

async function mockShell(page: Page) {
  // Fixture tests must never fall through to a real operator API.
  await page.route(/\/api\//, (route) => route.fulfill({ status: 404, json: { error: "not mocked" } }));
  await page.addInitScript(() => {
    class MockWebSocket {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSING = 2;
      static readonly CLOSED = 3;
      url: string;
      readyState = 0;
      onopen: ((event: Event) => void) | null = null;
      onmessage: ((event: MessageEvent) => void) | null = null;
      onerror: ((event: Event) => void) | null = null;
      onclose: ((event: Event) => void) | null = null;
      constructor(url: string) {
        this.url = url;
        setTimeout(() => {
          if (this.readyState === 3) return;
          this.readyState = 1;
          this.onopen?.(new Event("open"));
        }, 0);
      }
      send() {}
      close() {
        this.readyState = 3;
        this.onclose?.(new Event("close"));
      }
    }
    Object.defineProperty(window, "WebSocket", { configurable: true, writable: true, value: MockWebSocket });
  });
  await page.route(/\/api\/auth\/session$/, (route) => route.fulfill({ json: { ok: true, authenticated: true, auth_enabled: false, csrf_token: "" } }));
  await page.route(/\/api\/auth\/ws-ticket$/, (route) => route.fulfill({ json: { ticket: "s".repeat(64), expires_in: 15 } }));
  await page.route(/\/api\/agents(?:\?.*)?$/, (route) => route.fulfill({ json: { agents: MOCK_AGENTS } }));
  await page.route(/\/api\/departments(?:\?.*)?$/, (route) => route.fulfill({ json: { departments: MOCK_AGENT_DEPARTMENTS } }));
  await page.route(/\/api\/dispatched-sessions(?:\?.*)?$/, (route) => route.fulfill({ json: { sessions: [] } }));
  await page.route(/\/api\/stats(?:\?.*)?$/, (route) => route.fulfill({ json: MOCK_DASHBOARD_STATS }));
  await page.route(/\/api\/audit-logs(?:\?.*)?$/, (route) => route.fulfill({ json: { logs: [] } }));
  await page.route(/\/api\/dispatches(?:\?.*)?$/, (route) => route.fulfill({ json: { dispatches: [] } }));
  await page.route(/\/api\/kanban-cards(?:\?.*)?$/, (route) => route.fulfill({ json: { cards: [] } }));
  await page.route(/\/api\/kanban-repos$/, (route) => route.fulfill({ json: { repos: [] } }));
  await page.route(/\/api\/health(?:\/detail)?$/, (route) => route.fulfill({ json: HEALTH }));
}

async function mockSettings(page: Page, overrides: { onboarding?: unknown } = {}) {
  const writes: Captured[] = [];
  const capture = (route: Parameters<Parameters<Page["route"]>[1]>[0]) => {
    const request = route.request();
    writes.push({ method: request.method(), url: request.url(), body: request.postDataJSON() });
  };
  let generalSettings = { companyName: "AgentDesk", ceoName: "윤호", language: "ko", theme: "dark" };
  await page.route(/\/api\/settings$/, (route) => {
    if (route.request().method() === "PUT") {
      capture(route);
      generalSettings = { ...generalSettings, ...(route.request().postDataJSON() as object) };
      return route.fulfill({ json: { ok: true } });
    }
    return route.fulfill({ json: generalSettings });
  });
  await page.route(/\/api\/settings\/config$/, (route) => {
    if (route.request().method() === "PATCH") {
      capture(route);
      return route.fulfill({ json: { ok: true } });
    }
    return route.fulfill({ json: { entries: CONFIG_ENTRIES } });
  });
  await page.route(/\/api\/settings\/runtime-config$/, (route) => {
    if (route.request().method() === "PUT") {
      capture(route);
      return route.fulfill({ json: { ok: true } });
    }
    return route.fulfill({ json: { current: RUNTIME_CONFIG, defaults: RUNTIME_CONFIG, explicit_keys: [] } });
  });
  await page.route(/\/api\/settings\/operator-connectors$/, (route) => route.fulfill({ json: CONNECTORS }));
  await page.route(/\/api\/onboarding\/status$/, (route) => route.fulfill({ json: overrides.onboarding ?? ONBOARDING_STATUS }));
  await page.route(/\/api\/discord\/channels\/\d+$/, (route) => {
    const id = route.request().url().split("/").pop();
    return id === ALERT_CHANNEL_ID
      ? route.fulfill({ json: { id, name: "adk-alerts", guild_id: ONBOARDING_STATUS.guild_id } })
      : route.fulfill({ status: 404, json: { error: "channel not found" } });
  });
  await page.route(/\/api\/provider-auth-profiles$/, (route) => route.fulfill({ json: PROVIDER_PROFILES }));
  await page.route(/\/api\/cluster\/nodes$/, (route) => route.fulfill({ json: clusterNodes() }));
  await page.route(/\/api\/cluster\/machine-resources\/history\?/, (route) => {
    const instance_id = new URL(route.request().url()).searchParams.get("instance_id");
    return route.fulfill({ json: { instance_id, samples: [] } });
  });
  await page.route(/\/api\/github-repos$/, (route) => route.fulfill({ json: {
    viewer_login: "itismyfield",
    repos: [{ nameWithOwner: "itismyfield/AgentDesk", updatedAt: "2026-10-01T00:00:00Z", isPrivate: true, viewerPermission: "ADMIN" }],
  } }));
  await page.route(/\/api\/pipeline\/config\/effective\?repo=itismyfield%2FAgentDesk$/, (route) => route.fulfill({ json: {
    pipeline: MOCK_SETTINGS_FSM_PIPELINE,
    layers: { default: true, repo: false, agent: false },
  } }));
  await page.route(/\/api\/pipeline\/config\/repo\/itismyfield\/AgentDesk$/, (route) => route.fulfill({ json: {
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
  } }));
  await page.route(/\/api\/pipeline\/stages\?repo=itismyfield%2FAgentDesk$/, (route) => route.fulfill({ json: { stages: MOCK_SETTINGS_PIPELINE_STAGES } }));
  return writes;
}

async function expectNoHorizontalOverflow(page: Page) {
  const metrics = await page.evaluate(() => ({
    viewport: window.innerWidth,
    width: Math.max(document.body.scrollWidth, document.documentElement.scrollWidth),
  }));
  expect(metrics.width).toBeLessThanOrEqual(metrics.viewport + 1);
}

async function shot(page: Page, testInfo: TestInfo, name: string, scrollToBottom = false) {
  const settingsPage = page.getByTestId("settings-page");
  await settingsPage.evaluate((node, bottom) => node.scrollTo(0, bottom ? node.scrollHeight : 0), scrollToBottom);
  const dir = process.env.SETTINGS_SHOT_DIR;
  const file = `${testInfo.project.name}-${name}.png`;
  await page.screenshot({ path: dir ? `${dir}/${file}` : testInfo.outputPath(file) });
}

async function shotOf(page: Page, locator: Locator, testInfo: TestInfo, name: string) {
  await locator.scrollIntoViewIfNeeded();
  const dir = process.env.SETTINGS_SHOT_DIR;
  const file = `${testInfo.project.name}-${name}.png`;
  await page.screenshot({ path: dir ? `${dir}/${file}` : testInfo.outputPath(file) });
}

async function openSettings(page: Page, panel?: string) {
  await page.goto(panel ? `/settings?settingsPanel=${panel}` : "/settings");
  await expect(page.getByTestId("settings-page")).toBeVisible({ timeout: 15_000 });
}

const tabs = (page: Page) => page.getByRole("tablist", { name: "설정 패널" }).getByRole("tab");

test.describe("settings", () => {
  test.beforeEach(async ({ page }, testInfo) => {
    if (testInfo.project.name === "desktop") await page.setViewportSize({ width: 1440, height: 900 });
    await page.addInitScript(() => window.localStorage.clear());
    await mockShell(page);
  });

  test("five tabs, in order, each label on one line", async ({ page }, testInfo) => {
    await mockSettings(page);
    await openSettings(page);

    await expect(tabs(page)).toHaveText(["일반", "작업 흐름", "AI 계정", "연결", "머신"]);
    await expect(tabs(page).nth(0)).toHaveAttribute("aria-selected", "true");
    const metrics = await tabs(page).evaluateAll((nodes) => nodes.map((node) => {
      const label = node.querySelector("span") as HTMLElement;
      const lineHeight = parseFloat(getComputedStyle(label).lineHeight) || 20;
      return { height: node.getBoundingClientRect().height, lines: Math.round(label.getBoundingClientRect().height / lineHeight), wordBreak: getComputedStyle(node).wordBreak };
    }));
    for (const metric of metrics) {
      expect(metric.lines).toBe(1);
      expect(metric.height).toBeGreaterThanOrEqual(44);
      expect(metric.height).toBeLessThan(56);
      expect(metric.wordBreak).toBe("keep-all");
    }
    await expect(page.getByTestId("settings-page").getByText(/음성|온보딩/)).toHaveCount(0);
    await expectNoHorizontalOverflow(page);

    for (const [index, name] of ["general", "workflow", "ai", "connections", "machine"].entries()) {
      await tabs(page).nth(index).click();
      await expect(page).toHaveURL(new RegExp(`settingsPanel=${name}`));
      await expect(page.locator("#settings-panel-content .setting-row").first()).toBeVisible();
      await page.waitForTimeout(300);
      const copy = await page.locator("#settings-panel-content").innerText();
      expect(copy).not.toMatch(/auto-queue|telemetry|kv_meta|Rate Limit|compact|즉시 반영|extra/);
      await expectNoHorizontalOverflow(page);
      await shot(page, testInfo, `${index + 1}-${name}`);
      await shot(page, testInfo, `${index + 1}-${name}-bottom`, true);
    }
  });

  test("old panel links open the tab that now holds those settings", async ({ page }) => {
    await mockSettings(page);
    for (const [legacy, current] of [["runtime", "workflow"], ["pipeline", "workflow"], ["providers", "ai"], ["connectors", "connections"], ["onboarding", "connections"], ["voice", "general"]]) {
      await openSettings(page, legacy);
      await expect(page.locator(`#settings-tab-${current}`)).toHaveAttribute("aria-selected", "true");
      await expect(page).toHaveURL(new RegExp(`settingsPanel=${current}`));
    }
  });

  test("one save bar collects changes from every tab", async ({ page }, testInfo) => {
    const writes = await mockSettings(page);
    await openSettings(page);
    const bar = page.getByTestId("settings-save-bar");
    await expect(bar).toHaveCount(0);
    await expect(page.getByRole("button", { name: "저장", exact: true })).toHaveCount(0);

    await page.getByLabel("회사 이름").fill("AgentDesk Lab");
    await expect(bar).toContainText("바뀐 설정 1개");
    await expect(page.getByTestId("setting-row-companyName")).toContainText("바뀜");

    await tabs(page).nth(1).click();
    await page.getByLabel("작업 재시도 횟수 직접 입력").fill("5");
    await expect(bar).toContainText("바뀐 설정 2개");
    await expect(page.getByRole("button", { name: "저장", exact: true })).toHaveCount(1);

    const viewport = page.viewportSize()!;
    const box = (await bar.boundingBox())!;
    expect(box.y + box.height).toBeLessThanOrEqual(viewport.height + 1);
    const onTop = await bar.evaluate((node) => {
      const rect = node.getBoundingClientRect();
      const hit = document.elementFromPoint(rect.left + rect.width / 2, rect.top + rect.height / 2);
      return Boolean(hit && node.contains(hit));
    });
    expect(onTop).toBe(true);
    await shot(page, testInfo, "save-bar");

    await bar.getByRole("button", { name: "되돌리기" }).click();
    await expect(bar).toHaveCount(0);
    await expect(page.getByLabel("작업 재시도 횟수 직접 입력")).toHaveValue("3");

    await page.getByLabel("작업 재시도 횟수 직접 입력").fill("4");
    await page.getByLabel("접수 후 대기 경고 (분)").fill("30");
    await tabs(page).nth(0).click();
    await page.getByLabel("회사 이름").fill("");
    await expect(bar).toContainText("회사 이름을 입력해 주세요.");
    await expect(bar.getByRole("button", { name: "저장" })).toBeDisabled();
    await page.getByLabel("회사 이름").fill("AgentDesk Lab");
    await bar.getByRole("button", { name: "저장" }).click();
    await expect(bar).toHaveCount(0);

    expect(writes.find((write) => write.method === "PUT" && write.url.endsWith("/api/settings"))?.body).toMatchObject({ companyName: "AgentDesk Lab" });
    expect(writes.find((write) => write.url.endsWith("/runtime-config"))?.body).toMatchObject({ maxRetries: 4, __runtimeConfigExplicitKeys: ["maxRetries"] });
    expect(writes.find((write) => write.method === "PATCH")?.body).toEqual({ requested_timeout_min: "30" });
  });

  test("workflow puts frequent settings first and the visual editor behind advanced editing", async ({ page }, testInfo) => {
    await mockSettings(page);
    await openSettings(page, "workflow");

    const panel = page.locator("#settings-panel-content");
    for (const label of ["접수 후 대기 경고 (분)", "진행 멈춤 경고 (분)", "작업 재시도 횟수", "자동 대기열 재시도 횟수", "사용량이 많으면 자동 진행 멈추기", "리뷰 단계 사용"]) {
      await expect(panel.getByText(label, { exact: true })).toBeVisible();
    }
    await expect(page.getByTestId("pipeline-flow-canvas")).toHaveCount(0);
    await expect(panel.getByText("에이전트 품질 알림 채널 ID")).toHaveCount(0);

    const row = page.getByTestId("setting-row-requested_timeout_min");
    await row.getByRole("button", { name: "자세히" }).click();
    await expect(row).toContainText("기본값: 45");
    await expect(row).toContainText("다시 시작해도 여기서 바꾼 값이 유지돼요.");

    await panel.getByRole("button", { name: "고급 편집" }).click();
    await expect(page.getByTestId("pipeline-flow-canvas")).toBeVisible({ timeout: 10_000 });
    await expect(page.getByTestId("pipeline-selection-title")).toContainText(/상태 · backlog/);
    await expect(page.getByRole("button", { name: "이 상태 삭제" })).toHaveCount(0);

    const hooks = page.locator('textarea[placeholder="OnCardTransition, OnReviewEnter"], textarea[placeholder="OnStateExit"]');
    await expect(hooks).toHaveCount(2);
    for (const metric of await hooks.evaluateAll((nodes) => nodes.map((node) => ({ width: node.clientWidth, scroll: node.scrollWidth })))) {
      expect(metric.scroll).toBeLessThanOrEqual(metric.width + 1);
      expect(metric.width).toBeGreaterThanOrEqual(240);
    }

    await page.getByRole("button", { name: "상태 메뉴" }).click();
    await page.getByRole("menuitem", { name: "이 상태 삭제" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("'backlog' 상태를 삭제할까요?");
    await shot(page, testInfo, "workflow-delete-confirm");
    await dialog.getByRole("button", { name: "취소" }).click();
    await expect(page.getByTestId("pipeline-selection-title")).toContainText(/상태 · backlog/);
    await expectNoHorizontalOverflow(page);
    await shotOf(page, page.getByTestId("pipeline-selection-title"), testInfo, "workflow-editor-open");
  });

  test("a failed settings source shows one alert instead of empty editable defaults", async ({ page }) => {
    await mockSettings(page);
    await page.route(/\/api\/settings\/runtime-config$/, (route) => route.fulfill({ status: 503, json: { error: "unavailable" } }));
    await openSettings(page, "workflow");
    const panel = page.locator("#settings-panel-content");
    await expect(panel.getByRole("alert")).toHaveCount(1);
    await expect(panel.getByRole("alert")).toContainText("일부 설정을 불러오지 못했어요");
    await expect(panel.getByText("작업 재시도 횟수", { exact: true })).toHaveCount(0);
    await expect(panel.getByText("접수 후 대기 경고 (분)", { exact: true })).toBeVisible();
    await tabs(page).nth(0).click();
    await expect(panel.getByRole("alert")).toHaveCount(0);
  });

  test("connections shows setup status, channel names and honest connector counts", async ({ page }, testInfo) => {
    await mockSettings(page);
    await openSettings(page, "connections");

    await expect(page.getByTestId("settings-connection-bot")).toContainText("연결됨");
    await expect(page.getByTestId("settings-connection-server")).toContainText("연결됨");
    await expect(page.getByTestId("settings-connection-owner")).toContainText("설정됨");
    await expect(page.getByTestId("settings-connection-ai")).toContainText("Claude, Codex 사용 중");
    await expect(page.getByTestId("settings-channel-name-agent_quality_monitoring_channel_id")).toHaveText("#adk-alerts");
    await expect(page.getByTestId("settings-channel-name-deadlock_manager_channel_id")).toContainText("아직 채널을 정하지 않았어요");
    await page.getByLabel("막힘 감지 알림 채널 ID").fill("12ab");
    await expect(page.getByTestId("settings-channel-name-deadlock_manager_channel_id")).toContainText("17~20자리 숫자");

    await expect(page.getByTestId("settings-connectors-summary")).toHaveText("3개 중 2개 준비됐어요.");
    await expect(page.getByTestId("settings-connector-kakao_scheduled_delivery")).toContainText("Kakao 메시지 보내기");
    await expect(page.getByTestId("settings-connector-kakao_scheduled_delivery")).toContainText("꺼져 있어요.");
    await page.getByTestId("settings-connector-kakao_scheduled_delivery").getByText("설정 방법 보기").click();
    await expect(page.getByTestId("settings-connector-steps-kakao_scheduled_delivery")).toHaveText("Kakao 인증 정보를 넣은 뒤 AGENTDESK_KAKAO_ENABLED=true로 켜세요.");
    await expect(page.getByTestId("settings-tab-attention-connections")).toHaveCount(0);
    await expect(page.getByText("Obsidian agent prompts")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "초기 설정 다시 하기" })).toBeVisible();
    await expectNoHorizontalOverflow(page);
    await shot(page, testInfo, "connections");
  });

  test("connections tab badge counts setup that still needs attention", async ({ page }) => {
    await mockSettings(page, { onboarding: { ...ONBOARDING_STATUS, owner_id: null } });
    await openSettings(page, "general");
    await expect(page.getByTestId("settings-tab-attention-connections")).toHaveText("확인 1");
    await tabs(page).nth(3).click();
    await expect(page.getByTestId("settings-connection-owner")).toHaveAttribute("data-tone", "warn");
  });

  test("AI accounts explains usage windows in plain Korean", async ({ page }, testInfo) => {
    await mockSettings(page);
    await openSettings(page, "ai");

    const providers = page.getByTestId("settings-providers-panel");
    await expect(providers.getByText("Claude", { exact: true })).toBeVisible();
    await expect(providers.getByText("5시간 한도").first()).toBeVisible();
    await expect(providers.getByText("7일 한도 · Fable 모델")).toBeVisible();
    await expect(providers.getByText("Qwen 사용량은 아직 볼 수 없어요.")).toBeVisible();
    await expect(providers.getByText(/telemetry|7d Fable|extra/)).toHaveCount(0);
    await expect(page.getByTestId("settings-provider-account-work").getByTestId("settings-provider-usage-bar")).toHaveAttribute("data-level", "danger");
    await expect(page.getByText("대화 요약 시작 시점 (%)", { exact: true })).toBeVisible();
    await expect(page.getByText("사용량 주의 표시 (%)", { exact: true })).toBeVisible();

    await expect(page.getByText("사용량 정보 유효 시간 (초)")).toHaveCount(0);
    await page.getByRole("button", { name: "고급 설정 보기" }).click();
    await expect(page.getByText("사용량 정보 유효 시간 (초)")).toBeVisible();

    await shotOf(page, page.getByTestId("settings-provider-account-work"), testInfo, "ai-usage");
    await page.getByRole("button", { name: "work 계정 메뉴" }).click();
    await page.getByRole("menuitem", { name: "연결 해제" }).click();
    await expect(page.getByRole("dialog")).toContainText("Claude “work” 계정 연결을 해제할까요?");
    await page.getByRole("dialog").getByRole("button", { name: "취소" }).click();
    await expectNoHorizontalOverflow(page);
    await shot(page, testInfo, "ai");
  });

  test("machines show the server port and flag high usage", async ({ page }, testInfo) => {
    await mockSettings(page);
    await openSettings(page, "machine");

    await expect(page.getByTestId("setting-row-server_port")).toContainText("8791");
    await expect(page.getByTestId("setting-row-server_port")).toContainText("읽기 전용");
    const hub = page.getByTestId("machine-node-hub-mac");
    await expect(hub.getByRole("meter", { name: "CPU 사용률" })).toHaveAttribute("data-level", "high");
    await expect(hub.getByRole("meter", { name: "메모리 사용률" })).toHaveAttribute("data-level", "high");
    await expect(page.getByTestId("machine-node-runner-mac").getByRole("meter", { name: "CPU 사용률" })).toHaveAttribute("data-level", "normal");
    await expect(page.getByText(/사용 가능|바로 쓸 수 있는 메모리/)).toHaveCount(0);
    await expect(page.getByTestId("settings-tab-attention-machine")).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
    await shot(page, testInfo, "machine");
  });

  test("search narrows the tabs and rows", async ({ page }, testInfo) => {
    await mockSettings(page);
    await openSettings(page, "workflow");
    await page.getByTestId("settings-search-input").fill("요약");
    await expect(tabs(page)).toHaveText(["AI 계정"]);
    await expect(page.locator("#settings-panel-content")).toContainText("이 탭에는 “요약” 검색 결과가 없어요.");
    await tabs(page).first().click();
    await expect(page.getByText("대화 요약 시작 시점 (%)", { exact: true })).toBeVisible();
    await expect(page.getByText("사용량 주의 표시 (%)", { exact: true })).toHaveCount(0);
    await expect(page.getByTestId("settings-search-summary")).toContainText("4개 항목이 일치해요");
    await shot(page, testInfo, "search");
    await page.getByRole("button", { name: "검색 지우기" }).click();
    await expect(tabs(page)).toHaveCount(5);
  });
});
