import { expect, test, type Locator, type Page, type Route, type TestInfo } from "@playwright/test";

// Settings catalog checks at 1440x900 (desktop project) and 390x844 (mobile project).
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

const NOW_SECONDS = Math.floor(Date.now() / 1000);
const ALERT_CHANNEL_ID = "1234567890123456789";
const live = { editable: true, restart_behavior: "persist-live-override" };

const CONFIG_ENTRIES = [
  { key: "deadlock_manager_channel_id", value: "", category: "pipeline", label_ko: "데드락 매니저 채널 ID", label_en: "Deadlock Manager Channel ID", override_active: true, ...live },
  { key: "agent_quality_monitoring_channel_id", value: ALERT_CHANNEL_ID, category: "quality", label_ko: "GitHub 동기화 알림 채널 ID", label_en: "GitHub Sync Alert Channel ID", override_active: true, ...live },
  { key: "review_enabled", value: "true", category: "review", label_ko: "리뷰 활성화", label_en: "Review Enabled", override_active: true, ...live },
  { key: "max_review_rounds", value: "30", baseline: "3", baseline_source: "hardcoded", override_active: true, category: "review", label_ko: "최대 리뷰 라운드", label_en: "Max Review Rounds", ...live },
  { key: "server_port", value: "8791", baseline: "8791", baseline_source: "config", category: "system", label_ko: "서버 포트", label_en: "Server Port", editable: false, restart_behavior: "config-only" },
  { key: "requested_timeout_min", value: "45", baseline: "45", baseline_source: "hardcoded", category: "timeout", label_ko: "요청됨 타임아웃 (분)", label_en: "Requested Timeout (min)", ...live },
  { key: "context_compact_percent", value: "60", baseline: "60", baseline_source: "hardcoded", category: "context", label_ko: "컨텍스트 compact 임계값 (%)", label_en: "Context Compact Threshold (%)", ...live },
  { key: "context_compact_percent_codex", value: null, category: "context", label_ko: "Codex 컨텍스트 compact 임계값 (%)", label_en: "Codex Context Compact Threshold (%)", ...live },
  { key: "context_compact_percent_claude", value: null, baseline: null, override_active: false, category: "context", label_ko: "Claude 컨텍스트 compact 임계값 (%)", label_en: "Claude Context Compact Threshold (%)", ...live },
];

const RUNTIME_CONFIG = {
  maxRetries: 3,
  maxEntryRetries: 3,
  dispatchRateLimitGateEnabled: true,
  dispatchRateLimitGateDangerPct: 100,
  rateLimitWarningPct: 80,
  rateLimitDangerPct: 95,
  rateLimitStaleSec: 600,
};

const CONNECTORS = {
  connectors: [
    { id: "obsidian_agent_prompts", name: "Obsidian agent prompts", state: "ready", optional: true, env_var: "AGENTDESK_OBSIDIAN_AGENTS_SRC", source: "/Users/me/ObsidianVault/RemoteVault/adk-config/agents", reason: null, detail: "state=ready", setup_actions: [], capabilities: ["obsidian_agent_prompts"] },
    { id: "kakao_scheduled_delivery", name: "Kakao default-account delivery", state: "skipped", optional: true, env_var: "AGENTDESK_KAKAO_ENABLED", source: null, reason: "disabled", detail: "state=skipped reason=disabled", setup_actions: ["Set AGENTDESK_KAKAO_ENABLED=true after configuring Kakao credentials."], capabilities: ["kakao_friend_message"] },
  ],
  summary: { ready: 1, skipped: 1, missing_config: 0, missing_path: 0, missing_provider: 0, invalid_config: 0, invalid: 0, total: 2, core_runtime_blocking: false },
};

const ONBOARDING_STATUS = {
  completed: true,
  guild_id: "1111111111111111111",
  owner_id: "2222222222222222222",
  bot_providers: { command: "claude", command2: "codex" },
};

const HEALTH = {
  status: "degraded",
  uptime_secs: 3_600,
  global_active: 3,
  global_finalizing: 1,
  queue_depth: 5,
  degraded_reasons: ["tui_o:boundary_pending:1509350778043895902"],
  dispatch_outbox: { pending: 2, retrying: 1, permanent_failures: 0, oldest_pending_age: 12 },
  providers: [
    { name: "claude", connected: true, active_turns: 0, queue_depth: 2, sessions: 1, restart_pending: false, last_turn_at: null },
    { name: "codex", connected: false, active_turns: 0, queue_depth: 4, sessions: 1, restart_pending: false, last_turn_at: null },
  ],
};

const RETENTION = {
  total_stored_bytes: 2048, total_original_bytes: 4096, truncated_count: 1, manifest_count: 1, layer_count: 4,
  oldest_full_content_at: null, retention_horizon_at: null, retention_days: 30, per_layer_max_bytes_adk_provided: 65536,
  per_layer_max_bytes_user_derived: 16384, enabled: true, restart_required_for_config_changes: true,
  config_applied_at: "boot", config_source: "agentdesk.yaml boot snapshot", hot_reload: false,
};

const PROVIDER_PROFILES = {
  providers: [{
    id: "claude",
    default_home: "/Users/me/.claude",
    primary_profile_id: "default",
    accounts: [{
      id: "default",
      home: "/Users/me/.claude",
      usage: { buckets: [{ name: "7d Fable", limit: 100, used: 97, utilization: 97, reset: NOW_SECONDS + 86_400 }], stale: false, unsupported: false, reason: null },
    }],
  }],
};

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
  await page.route(/\/api\/settings$/, (route) => route.fulfill({ json: { companyName: "AgentDesk", ceoName: "윤호", language: "ko", theme: "dark" } }));
}

type FakeServer = {
  config: Array<Record<string, unknown>>;
  runtime: Record<string, unknown>;
  explicit: string[];
  settings: Record<string, unknown>;
  retention: typeof RETENTION;
  /** null answers health with 404 (a failed refresh). */
  health: typeof HEALTH | null;
  failNextWrite: boolean;
  /** Writes wait for this before answering (a save still in flight). */
  gate?: Promise<void>;
  writes: Captured[];
};

const fakeServer = (): FakeServer => ({
  config: CONFIG_ENTRIES.map((entry) => ({ ...entry })),
  runtime: {},
  explicit: [],
  settings: { companyName: "AgentDesk", ceoName: "윤호", language: "ko", theme: "dark" },
  retention: { ...RETENTION },
  health: HEALTH,
  failNextWrite: false,
  writes: [],
});

// Follows the server contract: a write lands in state and the next GET returns it (runtime PUT replaces the document).
async function mockSettings(page: Page, server = fakeServer()) {
  const write = async (route: Route, apply: (body: Record<string, unknown>) => void) => {
    const body = route.request().postDataJSON();
    server.writes.push({ method: route.request().method(), url: route.request().url(), body });
    await server.gate;
    if (server.failNextWrite) {
      server.failNextWrite = false;
      return route.fulfill({ status: 503, json: { error: "unavailable" } });
    }
    apply(body);
    return route.fulfill({ json: { ok: true } });
  };
  await page.route(/\/api\/settings\/config$/, (route) => (route.request().method() === "PATCH"
    ? write(route, (body) => {
      for (const [key, value] of Object.entries(body)) {
        const entry = server.config.find((candidate) => candidate.key === key);
        if (entry) Object.assign(entry, { value, override_active: entry.baseline == null || value !== entry.baseline });
      }
    })
    : route.fulfill({ json: { entries: server.config } })));
  await page.route(/\/api\/settings\/runtime-config$/, (route) => (route.request().method() === "PUT"
    ? write(route, ({ __runtimeConfigExplicitKeys, ...values }) => {
      server.runtime = values;
      server.explicit = __runtimeConfigExplicitKeys as string[];
    })
    : route.fulfill({ json: { current: { ...RUNTIME_CONFIG, ...server.runtime }, defaults: RUNTIME_CONFIG, explicit_keys: server.explicit } })));
  await page.route(/\/api\/settings$/, (route) => (route.request().method() === "PUT"
    ? write(route, (body) => { server.settings = body; })
    : route.fulfill({ json: server.settings })));
  await page.route(/\/api\/health(?:\/detail)?$/, (route) => (server.health ? route.fulfill({ json: server.health }) : route.fulfill({ status: 404, json: {} })));
  await page.route(/\/api\/settings\/operator-connectors$/, (route) => route.fulfill({ json: CONNECTORS }));
  await page.route(/\/api\/prompt-manifest\/retention$/, (route) => route.fulfill({ json: server.retention }));
  await page.route(/\/api\/onboarding\/status$/, (route) => route.fulfill({ json: ONBOARDING_STATUS }));
  await page.route(/\/api\/discord\/channels\/\d+$/, (route) => {
    const id = route.request().url().split("/").pop();
    return id === ALERT_CHANNEL_ID
      ? route.fulfill({ json: { id, name: "adk-alerts", guild_id: ONBOARDING_STATUS.guild_id } })
      : route.fulfill({ status: 404, json: { error: "channel not found" } });
  });
  await page.route(/\/api\/provider-auth-profiles$/, (route) => route.fulfill({ json: PROVIDER_PROFILES }));
  return server;
}

async function expectNoHorizontalOverflow(page: Page) {
  const metrics = await page.evaluate(() => ({
    viewport: window.innerWidth,
    width: Math.max(document.body.scrollWidth, document.documentElement.scrollWidth),
  }));
  expect(metrics.width).toBeLessThanOrEqual(metrics.viewport + 1);
}

async function shot(page: Page, testInfo: TestInfo, name: string) {
  const dir = process.env.SETTINGS_SHOT_DIR;
  const file = `${testInfo.project.name}-${name}.png`;
  await page.screenshot({ path: dir ? `${dir}/${file}` : testInfo.outputPath(file) });
}

async function openSettings(page: Page, query = "") {
  await page.goto(`/settings${query}`);
  await expect(page.getByTestId("settings-catalog")).toBeVisible({ timeout: 15_000 });
}

const row = (page: Page, key: string) => page.getByTestId(`setting-row-${key}`);
const isPhone = (testInfo: TestInfo) => testInfo.project.name === "mobile";

/** Types a value and saves it: the phone sheet, or the desktop row and pending bar. */
async function editAndSave(page: Page, key: string, value: string, phone: boolean) {
  if (phone) {
    await row(page, key).click();
    const sheet = page.getByTestId("settings-edit-sheet");
    await sheet.locator("input").fill(value);
    await sheet.getByRole("button", { name: /저장$/ }).click();
    await expect(sheet).toHaveCount(0);
    return;
  }
  await row(page, key).locator("input").fill(value);
  await page.getByTestId("settings-save-bar").getByRole("button", { name: "저장" }).click();
  await expect(page.getByTestId("settings-save-bar")).toHaveCount(0);
}

/** Picks an option in a language/theme row and saves it the same two ways. */
async function pickAndSave(page: Page, key: string, option: string, phone: boolean) {
  if (phone) await row(page, key).click();
  const scope = phone ? page.getByTestId("settings-edit-sheet") : row(page, key);
  await scope.getByRole("combobox").click();
  await page.getByRole("option", { name: option }).click();
  const save = (phone ? scope : page.getByTestId("settings-save-bar")).getByRole("button", { name: /^(저장|Save)$/ });
  await save.click();
  await expect(save).toHaveCount(0);
}

test.describe("settings catalog", () => {
  test.beforeEach(async ({ page }, testInfo) => {
    if (testInfo.project.name === "desktop") await page.setViewportSize({ width: 1440, height: 900 });
    await page.addInitScript(() => window.localStorage.clear());
    await mockShell(page);
  });

  test("desktop: categories, filters and search narrow one list; the retired deadlock key never shows", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    await mockSettings(page);
    await openSettings(page);
    const nav = page.getByRole("navigation", { name: "분류" });
    await expect(nav.getByRole("button")).toHaveText(["전체26", "작업 흐름3", "리뷰2", "컨텍스트3", "사용량5", "알림 채널1", "시스템4", "연결8"]);
    await expect(page.getByText("deadlock_manager_channel_id")).toHaveCount(0);
    await expect(page.getByText("데드락")).toHaveCount(0);

    await page.getByRole("switch", { name: /기본값과 다름/ }).check();
    await expect(page.getByTestId("settings-list-caption")).toHaveText("26개 중 3개 · 기본값과 다른 것만");
    await page.getByRole("switch", { name: /기본값과 다름/ }).uncheck();
    await page.getByRole("switch", { name: /재시작 필요/ }).check();
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(2);
    await expect(row(page, "server_port")).toContainText("설정 파일에서만");
    await page.getByRole("switch", { name: /재시작 필요/ }).uncheck();

    // A related row hidden by the search opens anyway: the search is lifted and the row selected.
    await page.getByTestId("settings-search-input").fill("최대 리뷰");
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(1);
    await row(page, "max_review_rounds").getByRole("button").first().click();
    await page.getByTestId("settings-inspector").getByRole("button", { name: /리뷰 활성화/ }).click();
    await expect(page.getByTestId("settings-search-input")).toHaveValue("");
    await expect(row(page, "review_enabled")).toHaveAttribute("data-selected", "true");
    await expect(page.getByTestId("settings-inspector").getByRole("heading")).toHaveText("리뷰 활성화");

    await page.getByTestId("settings-category-context").click();
    await expect(page).toHaveURL(/category=context$/);
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(3);
    await expectNoHorizontalOverflow(page);
    await shot(page, testInfo, "catalog");
  });

  for (const width of [1280, 1440]) {
    test(`desktop ${width}px: rows keep the default, its source and both chips in view`, async ({ page }, testInfo) => {
      test.skip(isPhone(testInfo), "Desktop layout");
      await page.setViewportSize({ width, height: 900 });
      await mockSettings(page);
      await openSettings(page);
      const rounds = page.getByTestId("setting-meta-max_review_rounds");
      for (const text of ["기본 3 · 코드", "덮어씀", "바로 적용"]) await expect(rounds.getByText(text, { exact: true })).toBeVisible();
      await expect(page.getByTestId("setting-meta-maxEntryRetries").getByText("기본 3 · 서버 기본값", { exact: true })).toBeVisible();
    });
  }

  test("desktop: a failed write keeps the draft, a retry saves it, and the saved value survives a reload", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = await mockSettings(page);
    await openSettings(page);
    const bar = page.getByTestId("settings-save-bar");
    await row(page, "max_review_rounds").getByRole("button").first().click();
    const inspector = page.getByTestId("settings-inspector");
    await expect(inspector).toContainText("바로 · 재시작해도 유지");
    await row(page, "max_review_rounds").locator("input").fill("10");
    await expect(bar).toContainText("최대 리뷰 라운드 30 → 10 · 저장하면 바로 적용돼요");
    await expect(inspector).toContainText("고칠 값10");
    await row(page, "maxEntryRetries").locator("input").fill("");
    await expect(bar.getByRole("button", { name: "저장" })).toBeDisabled();
    await row(page, "maxEntryRetries").locator("input").fill("5");
    await expect(bar).toContainText("변경 2건");
    await shot(page, testInfo, "pending");

    server.failNextWrite = true;
    await bar.getByRole("button", { name: "저장" }).click();
    await expect(bar).toContainText("변경 1건");
    await expect(row(page, "maxEntryRetries")).toContainText("바뀜");
    await bar.getByRole("button", { name: "저장" }).click();
    await expect(bar).toHaveCount(0);
    await expect(row(page, "max_review_rounds").locator("input")).toHaveValue("10");
    expect(server.writes.find((write) => write.method === "PATCH")?.body).toEqual({ max_review_rounds: "10" });
    expect(server.writes.at(-1)?.body).toMatchObject({ maxEntryRetries: 5, __runtimeConfigExplicitKeys: ["maxEntryRetries"] });
    await page.reload();
    await expect(row(page, "max_review_rounds").locator("input")).toHaveValue("10");
    await expect(row(page, "maxEntryRetries").locator("input")).toHaveValue("5");
  });

  test("desktop: number settings go out as integers and decimals cannot be saved", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = await mockSettings(page);
    await openSettings(page, "?category=workflow");
    const bar = page.getByTestId("settings-save-bar");
    for (const [key, value] of [["requested_timeout_min", "1e2"], ["maxEntryRetries", "1e1"]]) {
      await row(page, key).locator("input").fill("60.5");
      await expect(bar.getByRole("button", { name: "저장" })).toBeDisabled();
      await row(page, key).locator("input").fill(value);
    }
    await bar.getByRole("button", { name: "저장" }).click();
    await expect(bar).toHaveCount(0);
    expect(server.writes.find((write) => write.method === "PATCH")?.body).toEqual({ requested_timeout_min: "100" });
    expect(server.writes.find((write) => write.method === "PUT")?.body).toMatchObject({ maxEntryRetries: 10 });
  });

  test("desktop: reset to the default and confirm a dangerous change before saving", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = await mockSettings(page);
    await openSettings(page);
    await row(page, "max_review_rounds").getByRole("button").first().click();
    await page.getByRole("button", { name: "기본값 3으로 되돌리기" }).click();
    await expect(row(page, "max_review_rounds").locator("input")).toHaveValue("3");
    await page.getByRole("switch", { name: "리뷰 활성화" }).click();
    const bar = page.getByTestId("settings-save-bar");
    await bar.getByRole("button", { name: "저장" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("리뷰 게이트");
    expect(server.writes).toHaveLength(0);
    await dialog.getByRole("button", { name: "확인하고 저장" }).click();
    await expect(bar).toHaveCount(0);
    expect(server.writes[0]?.body).toEqual({ max_review_rounds: "3", review_enabled: "false" });
  });

  test("desktop: edits typed while a runtime save is in flight stay pending", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = await mockSettings(page);
    let release = () => {};
    server.gate = new Promise((resolve) => { release = resolve; });
    await openSettings(page, "?category=workflow");
    const bar = page.getByTestId("settings-save-bar");
    await row(page, "maxEntryRetries").locator("input").fill("5");
    await bar.getByRole("button", { name: "저장" }).click();
    await expect.poll(() => server.writes.length).toBe(1);
    await row(page, "maxEntryRetries").locator("input").fill("6");
    await row(page, "maxRetries").locator("input").fill("4");
    release();
    await expect.poll(() => server.runtime.maxEntryRetries).toBe(5);
    await expect(bar).toContainText("변경 2건");
    await expect(row(page, "maxEntryRetries").locator("input")).toHaveValue("6");
    await expect(row(page, "maxRetries").locator("input")).toHaveValue("4");
  });

  test("desktop: language and theme stay locked while /api/settings saves", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = await mockSettings(page);
    let release = () => {};
    const hold = () => { server.gate = new Promise((resolve) => { release = resolve; }); };
    const picker = (key: string) => row(page, key).getByRole("combobox");
    const bar = page.getByTestId("settings-save-bar");
    await openSettings(page, "?category=system");
    await picker("theme").click();
    await page.getByRole("option", { name: "밝게" }).click();

    // A failed save unlocks both rows and keeps the draft.
    hold();
    server.failNextWrite = true;
    await bar.getByRole("button", { name: "저장" }).click();
    await expect.poll(() => server.writes.length).toBe(1);
    await expect(picker("theme")).toBeDisabled();
    await expect(picker("language")).toBeDisabled();
    release();
    await expect(picker("language")).toBeEnabled();
    await expect(picker("theme")).toHaveText(/밝게/);
    await expect(bar).toContainText("변경 1건");
    expect(server.settings).toMatchObject({ theme: "dark" });

    // The retry locks again and the saved theme applies once it lands.
    hold();
    await bar.getByRole("button", { name: "저장" }).click();
    await expect.poll(() => server.writes.length).toBe(2);
    await expect(picker("language")).toBeDisabled();
    release();
    await expect(bar).toHaveCount(0);
    await expect(picker("language")).toBeEnabled();
    await expect(picker("theme")).toHaveText(/밝게/);
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    expect(server.settings).toMatchObject({ theme: "light", language: "ko" });
    await pickAndSave(page, "language", "English", false);
    await expect.poll(() => server.settings).toMatchObject({ theme: "light", language: "en" });
  });

  test("desktop: the Claude-only compact value can go back to following the shared one", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = await mockSettings(page);
    await openSettings(page, "?category=context");
    const claude = row(page, "context_compact_percent_claude");
    // A first draft can already go back to following the shared value.
    await claude.locator("input").fill("70");
    await claude.getByRole("button").first().click();
    await page.getByRole("button", { name: "공통 값을 따르게 되돌리기" }).click();
    await expect(claude.locator("input")).toHaveValue("");
    await expect(page.getByTestId("settings-save-bar")).toHaveCount(0);
    await editAndSave(page, "context_compact_percent_claude", "70", false);
    await expect(claude).toContainText("덮어씀");
    await claude.getByRole("button").first().click();
    await page.getByRole("button", { name: "공통 값을 따르게 되돌리기" }).click();
    await page.getByTestId("settings-save-bar").getByRole("button", { name: "저장" }).click();
    expect(server.writes.map((write) => write.body)).toEqual([{ context_compact_percent_claude: "70" }, { context_compact_percent_claude: "" }]);
    await page.reload();
    await row(page, "context_compact_percent_claude").getByRole("button").first().click();
    await expect(page.getByTestId("settings-inspector")).toContainText("저장된 값공통 값 따름");
    await expect(row(page, "context_compact_percent_claude")).not.toContainText("덮어씀");
  });

  test("desktop: with a Claude value in the config file, an emptied field reads as that value", async ({ page }, testInfo) => {
    test.skip(isPhone(testInfo), "Desktop layout");
    const server = fakeServer();
    Object.assign(server.config.find((entry) => entry.key === "context_compact_percent_claude")!, { value: "70", baseline: "70", baseline_source: "yaml" });
    await mockSettings(page, server);
    await openSettings(page, "?category=context");
    await editAndSave(page, "context_compact_percent_claude", "", false);
    expect(server.writes.map((write) => write.body)).toEqual([{ context_compact_percent_claude: "" }]);
    await page.reload();
    await row(page, "context_compact_percent_claude").getByRole("button").first().click();
    const inspector = page.getByTestId("settings-inspector");
    await expect(inspector).toContainText("저장된 값설정 파일 기본값 70");
    await expect(inspector).not.toContainText("공통 값 따름");
    await expect(row(page, "context_compact_percent_claude")).not.toContainText("덮어씀");
  });

  test("runtime saves from two open screens keep each other's values", async ({ page, context }, testInfo) => {
    const server = await mockSettings(page);
    const second = await context.newPage();
    await mockShell(second);
    await mockSettings(second, server);
    await openSettings(page, "?category=workflow");
    await openSettings(second, "?category=usage");
    await editAndSave(page, "maxEntryRetries", "5", isPhone(testInfo));
    await editAndSave(second, "rateLimitWarningPct", "85", isPhone(testInfo));
    expect(server.runtime).toMatchObject({ maxEntryRetries: 5, rateLimitWarningPct: 85 });
    expect(server.explicit).toEqual(["maxEntryRetries", "rateLimitWarningPct"]);
  });

  test("connections: the old ops URL lands on health, delivery, setup and connectors", async ({ page }, testInfo) => {
    const server = await mockSettings(page);
    await page.goto("/ops");
    await expect(page).toHaveURL(/\/settings\?category=connections$/);
    await expect(row(page, "health.status")).toContainText("주의");
    await expect(row(page, "health.providers")).toContainText("연결 끊김: Codex");
    await expect(row(page, "health.dispatch_outbox")).toContainText("2건");
    await expect(row(page, "kakao_scheduled_delivery")).toContainText("꺼짐");
    await expect(page.getByTestId("cluster-nodes-panel")).toBeAttached();
    await expect(page.getByRole("button", { name: "초기 설정 다시 하기" })).toBeVisible();

    await row(page, "health.status").click();
    const facts = page.getByTestId("settings-facts-health.status");
    await expect(facts).toContainText("가동 시간1시간 0분");
    await expect(facts).toContainText("처리 대기5진행 중 3 · 마무리 중 1");
    await expect(facts.locator("li", { hasText: "프로바이더 대기열" })).toContainText("6");
    await expect(facts.getByText("TUI 출력 마무리 대기")).toHaveAttribute("title", "tui_o:boundary_pending:1509350778043895902");
    await expect(facts).toContainText("업데이트");
    server.health = null;
    await facts.getByRole("button", { name: "다시 확인" }).click();
    await expect(facts.getByRole("alert")).toContainText("최근 확인이 실패해서 마지막으로 받은 값을 보여 줘요.");
    await expect(facts).toContainText("진행 중 3 · 마무리 중 1");
    await expect(row(page, "health.status")).toContainText("주의");
    await shot(page, testInfo, "connections");
    if (isPhone(testInfo)) await page.keyboard.press("Escape");

    await row(page, "kakao_scheduled_delivery").click();
    await expect(page.getByText("Kakao 인증 정보를 넣은 뒤 AGENTDESK_KAKAO_ENABLED=true로 켜세요.")).toBeVisible();
    await expectNoHorizontalOverflow(page);
  });

  test("system: prompt retention numbers refresh without a reload", async ({ page }) => {
    const server = await mockSettings(page);
    await openSettings(page, "?category=system");
    await row(page, "prompt_manifest_retention").click();
    const facts = page.getByTestId("settings-facts-prompt_manifest_retention");
    await expect(facts).toContainText("조각4");
    server.retention = { ...server.retention, layer_count: 9 };
    await facts.getByRole("button", { name: "다시 확인" }).click();
    await expect(facts).toContainText("조각9");
  });

  test("system: language and default theme save through /api/settings and reach a fresh browser", async ({ page, browser }, testInfo) => {
    const server = await mockSettings(page);
    await openSettings(page, "?category=system");
    await expect(row(page, "language")).toContainText("기본 한국어 · 코드");
    await pickAndSave(page, "theme", "밝게", isPhone(testInfo));
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
    await pickAndSave(page, "language", "English", isPhone(testInfo));
    await expect(row(page, "language")).toContainText("Language");
    await expect(page.getByTestId("settings-search-input")).toHaveAttribute("placeholder", "Search by name, key or description");
    await page.reload();
    await expect(row(page, "language")).toContainText("Language");
    expect(server.settings).toMatchObject({ language: "en", theme: "light" });

    // A browser that never picked a theme starts on the saved default.
    const other = await browser.newContext();
    const fresh = await other.newPage();
    await mockShell(fresh);
    await mockSettings(fresh, server);
    await fresh.goto("/home");
    await expect(fresh.locator("html")).toHaveAttribute("data-theme", "light");
    await other.close();
  });

  for (const order of [["theme", "language"], ["language", "theme"]] as const) {
    test(`system: two tabs saving ${order.join(" then ")} keep each other's field`, async ({ page, context, browser }, testInfo) => {
      const server = await mockSettings(page);
      const second = await context.newPage();
      await mockShell(second);
      await mockSettings(second, server);
      await openSettings(page, "?category=system");
      await openSettings(second, "?category=system");
      const choice = { theme: "밝게", language: "English" };
      await pickAndSave(page, order[0], choice[order[0]], isPhone(testInfo));
      await pickAndSave(second, order[1], choice[order[1]], isPhone(testInfo));
      expect(server.settings).toMatchObject({ language: "en", theme: "light" });

      const other = await browser.newContext({ viewport: page.viewportSize() });
      const fresh = await other.newPage();
      await mockShell(fresh);
      await mockSettings(fresh, server);
      await openSettings(fresh, "?category=system");
      await expect(row(fresh, "language")).toContainText("Language");
      await expect(fresh.locator("html")).toHaveAttribute("data-theme", "light");
      await other.close();
    });
  }

  test.describe("touch", () => {
    test.use({ hasTouch: true, isMobile: true });

    test("phone: search, text editing and the language and theme pickers use 16px text", async ({ page }, testInfo) => {
      test.skip(!isPhone(testInfo), "Phone layout");
      await mockSettings(page);
      const fontSize = (locator: Locator) => locator.evaluate((node) => getComputedStyle(node).fontSize);
      const sheet = page.getByTestId("settings-edit-sheet");
      await openSettings(page, "?category=system");
      expect(await fontSize(page.getByTestId("settings-search-input"))).toBe("16px");
      for (const key of ["language", "theme"]) {
        await row(page, key).click();
        expect(await fontSize(sheet.getByRole("combobox"))).toBe("16px");
        await page.keyboard.press("Escape");
        await expect(sheet).toHaveCount(0);
      }
      await openSettings(page, "?category=alerts");
      await row(page, "agent_quality_monitoring_channel_id").click();
      expect(await fontSize(sheet.locator("input"))).toBe("16px");
    });

    test("phone: differing rows, category drill-down and a sheet that saves one row", async ({ page }, testInfo) => {
      test.skip(!isPhone(testInfo), "Phone layout");
      const server = await mockSettings(page);
      await openSettings(page);
      const differing = page.getByTestId("settings-differing");
      await expect(differing).toContainText("기본값과 다름3");
      await expect(differing.getByTestId("setting-row-max_review_rounds")).toContainText("30 · 기본 3 · 코드");
      const heights = await page.getByRole("navigation", { name: "분류" }).getByRole("button").evaluateAll((nodes) => nodes.map((node) => node.getBoundingClientRect().height));
      expect(Math.min(...heights)).toBeGreaterThanOrEqual(44);

      await page.getByTestId("settings-category-review").click();
      await expect(page.getByTestId("settings-phone-title")).toHaveText("리뷰");
      await row(page, "max_review_rounds").click();
      const sheet = page.getByTestId("settings-edit-sheet");
      await expect(sheet).toContainText("max_review_rounds");
      await sheet.getByRole("button", { name: "기본값 3" }).click();
      await sheet.getByRole("button", { name: "1 늘리기" }).click();
      for (const name of ["1 줄이기", "1 늘리기", "4로 저장"]) {
        const box = (await sheet.getByRole("button", { name }).boundingBox())!;
        expect(Math.min(box.width, box.height)).toBeGreaterThanOrEqual(48);
      }
      await shot(page, testInfo, "sheet");
      await sheet.getByRole("button", { name: "4로 저장" }).click();
      await expect(sheet).toHaveCount(0);
      expect(server.writes[0]?.body).toEqual({ max_review_rounds: "4" });
      await page.getByRole("button", { name: "분류 목록으로" }).click();
      await expect(page.getByTestId("settings-differing")).toBeVisible();
      await expectNoHorizontalOverflow(page);
    });

    test("phone: a sheet locks while its save is in flight, keeps the value on failure and closes on success", async ({ page }, testInfo) => {
      test.skip(!isPhone(testInfo), "Phone layout");
      const server = await mockSettings(page);
      let release = () => {};
      const hold = () => { server.gate = new Promise((resolve) => { release = resolve; }); };
      const sheet = page.getByTestId("settings-edit-sheet");
      const input = sheet.locator("input");
      const close = page.getByRole("dialog").getByRole("button", { name: "Close" });
      await openSettings(page, "?category=workflow");
      await row(page, "maxEntryRetries").click();
      await input.fill("5");

      hold();
      server.failNextWrite = true;
      await sheet.getByRole("button", { name: "5로 저장" }).click();
      await expect.poll(() => server.writes.length).toBe(1);
      for (const control of [input, sheet.getByRole("button", { name: "1 줄이기" }), sheet.getByRole("button", { name: "1 늘리기" }), sheet.getByRole("button", { name: /^기본값/ })]) {
        await expect(control).toBeDisabled();
      }
      await expect(close).toHaveCSS("pointer-events", "none");
      await shot(page, testInfo, "sheet-saving");
      await page.keyboard.press("Escape");
      await close.dispatchEvent("click");
      await expect(sheet).toBeVisible();
      await expect(input).toHaveValue("5");
      release();
      await expect(input).toBeEnabled();
      await expect(input).toHaveValue("5");
      expect(server.runtime.maxEntryRetries).toBeUndefined();

      hold();
      await sheet.getByRole("button", { name: "5로 저장" }).click();
      await expect.poll(() => server.writes.length).toBe(2);
      await expect(input).toBeDisabled();
      release();
      await expect(sheet).toHaveCount(0);
      expect(server.runtime.maxEntryRetries).toBe(5);
      await row(page, "maxEntryRetries").click();
      await expect(input).toHaveValue("5");
      await page.keyboard.press("Escape");

      // The language and theme pickers lock the same way.
      await openSettings(page, "?category=system");
      await row(page, "theme").click();
      await sheet.getByRole("combobox").click();
      await page.getByRole("option", { name: "밝게" }).click();
      hold();
      await sheet.getByRole("button", { name: "저장" }).click();
      await expect.poll(() => server.writes.length).toBe(3);
      await expect(sheet.getByRole("combobox")).toBeDisabled();
      release();
      await expect(sheet).toHaveCount(0);
      expect(server.settings).toMatchObject({ theme: "light" });
    });
  });

  test("usage keeps the AI account bars next to their thresholds", async ({ page }) => {
    await mockSettings(page);
    await openSettings(page, "?category=usage");
    await expect(row(page, "rateLimitWarningPct")).toContainText("사용량 주의 표시 (%)");
    await expect(page.getByTestId("settings-providers-panel").getByText("7일 한도 · Fable 모델")).toBeVisible();
    await expect(page.getByTestId("settings-provider-usage-bar").first()).toHaveAttribute("data-level", "danger");
  });

  test("a failed source shows one alert instead of editable defaults", async ({ page }) => {
    await mockSettings(page);
    await page.route(/\/api\/settings\/runtime-config$/, (route) => route.fulfill({ status: 503, json: { error: "unavailable" } }));
    await openSettings(page, "?category=workflow");
    await expect(page.getByRole("alert")).toContainText("일부 설정을 불러오지 못했어요");
    await expect(row(page, "maxRetries")).toHaveCount(0);
    await expect(row(page, "requested_timeout_min")).toBeVisible();
  });
});
