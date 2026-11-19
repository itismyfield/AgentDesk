import { expect, test, type Page, type TestInfo } from "@playwright/test";

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
  degraded_reasons: ["tui_o:boundary_pending:1509350778043895902"],
  dispatch_outbox: { pending: 2, retrying: 1, permanent_failures: 0, oldest_pending_age: 12 },
  providers: [
    { name: "claude", connected: true, active_turns: 0, queue_depth: 0, sessions: 1, restart_pending: false, last_turn_at: null },
    { name: "codex", connected: false, active_turns: 0, queue_depth: 0, sessions: 1, restart_pending: false, last_turn_at: null },
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

async function mockSettings(page: Page) {
  const writes: Captured[] = [];
  const capture = (route: Parameters<Parameters<Page["route"]>[1]>[0]) => {
    const request = route.request();
    writes.push({ method: request.method(), url: request.url(), body: request.postDataJSON() });
  };
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
  await page.route(/\/api\/prompt-manifest\/retention$/, (route) => route.fulfill({ json: RETENTION }));
  await page.route(/\/api\/onboarding\/status$/, (route) => route.fulfill({ json: ONBOARDING_STATUS }));
  await page.route(/\/api\/discord\/channels\/\d+$/, (route) => {
    const id = route.request().url().split("/").pop();
    return id === ALERT_CHANNEL_ID
      ? route.fulfill({ json: { id, name: "adk-alerts", guild_id: ONBOARDING_STATUS.guild_id } })
      : route.fulfill({ status: 404, json: { error: "channel not found" } });
  });
  await page.route(/\/api\/provider-auth-profiles$/, (route) => route.fulfill({ json: PROVIDER_PROFILES }));
  return writes;
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

test.describe("settings catalog", () => {
  test.beforeEach(async ({ page }, testInfo) => {
    if (testInfo.project.name === "desktop") await page.setViewportSize({ width: 1440, height: 900 });
    await page.addInitScript(() => window.localStorage.clear());
    await mockShell(page);
  });

  test("desktop: categories, filters and search narrow one list; the retired deadlock key never shows", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop layout");
    await mockSettings(page);
    await openSettings(page);
    const nav = page.getByRole("navigation", { name: "분류" });
    await expect(nav.getByRole("button")).toHaveText(["전체24", "작업 흐름3", "리뷰2", "컨텍스트2", "사용량5", "알림 채널1", "시스템3", "연결8"]);
    await expect(page.getByText("deadlock_manager_channel_id")).toHaveCount(0);
    await expect(page.getByText("데드락")).toHaveCount(0);

    await page.getByRole("switch", { name: /기본값과 다름/ }).check();
    await expect(page.getByTestId("settings-list-caption")).toHaveText("24개 중 3개 · 기본값과 다른 것만");
    await expect(row(page, "max_review_rounds")).toContainText("기본 3 · 코드");
    await expect(row(page, "max_review_rounds")).toContainText("덮어씀");
    await page.getByRole("switch", { name: /기본값과 다름/ }).uncheck();

    await page.getByRole("switch", { name: /재시작 필요/ }).check();
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(2);
    await expect(row(page, "server_port")).toContainText("설정 파일에서만");
    await page.getByRole("switch", { name: /재시작 필요/ }).uncheck();

    await page.getByTestId("settings-search-input").fill("사람 판단");
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(2);
    await page.getByTestId("settings-search-input").fill("");
    await page.getByTestId("settings-category-context").click();
    await expect(page).toHaveURL(/category=context$/);
    await expect(page.locator("[data-testid^=setting-row-]")).toHaveCount(2);
    await expectNoHorizontalOverflow(page);
    await shot(page, testInfo, "catalog");
  });

  test("desktop: inline edits wait in the pending bar and save through the existing APIs", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop layout");
    const writes = await mockSettings(page);
    await openSettings(page);
    const bar = page.getByTestId("settings-save-bar");
    await expect(bar).toHaveCount(0);

    await row(page, "max_review_rounds").getByRole("button").first().click();
    const inspector = page.getByTestId("settings-inspector");
    await expect(inspector).toContainText("한 카드를 고쳐서 다시 리뷰받을 수 있는 최대 횟수예요.");
    await expect(inspector).toContainText("바로 · 재시작해도 유지");
    await expect(inspector.getByRole("button", { name: "리뷰 활성화" })).toBeVisible();
    await row(page, "max_review_rounds").locator("input").fill("10");
    await expect(bar).toContainText("변경 1건");
    await expect(bar).toContainText("최대 리뷰 라운드 30 → 10 · 저장하면 바로 적용돼요");
    await expect(row(page, "max_review_rounds")).toContainText("바뀜");
    await expect(inspector).toContainText("고칠 값10");

    await row(page, "maxEntryRetries").locator("input").fill("");
    await expect(bar.getByRole("button", { name: "저장" })).toBeDisabled();
    await row(page, "maxEntryRetries").locator("input").fill("5");
    await expect(bar).toContainText("변경 2건");
    await shot(page, testInfo, "pending");

    await bar.getByRole("button", { name: "저장" }).click();
    await expect(bar).toHaveCount(0);
    expect(writes.find((write) => write.method === "PATCH")?.body).toEqual({ max_review_rounds: "10" });
    expect(writes.find((write) => write.method === "PUT")?.body).toMatchObject({ maxEntryRetries: 5, __runtimeConfigExplicitKeys: ["maxEntryRetries"] });
  });

  test("desktop: reset to the default and confirm a dangerous change before saving", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop layout");
    const writes = await mockSettings(page);
    await openSettings(page);
    await row(page, "max_review_rounds").getByRole("button").first().click();
    await page.getByRole("button", { name: "기본값 3으로 되돌리기" }).click();
    await expect(row(page, "max_review_rounds").locator("input")).toHaveValue("3");
    await page.getByRole("switch", { name: "리뷰 활성화" }).click();
    const bar = page.getByTestId("settings-save-bar");
    await bar.getByRole("button", { name: "저장" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("리뷰 게이트");
    expect(writes).toHaveLength(0);
    await dialog.getByRole("button", { name: "확인하고 저장" }).click();
    await expect(bar).toHaveCount(0);
    expect(writes.find((write) => write.method === "PATCH")?.body).toEqual({ max_review_rounds: "3", review_enabled: "false" });
  });

  test("connections: the old ops URL lands on health, delivery, setup and connectors", async ({ page }, testInfo) => {
    await mockSettings(page);
    await page.goto("/ops");
    await expect(page).toHaveURL(/\/settings\?category=connections$/);
    await expect(row(page, "health.status")).toContainText("주의");
    await expect(row(page, "health.providers")).toContainText("연결 끊김: Codex");
    await expect(row(page, "health.dispatch_outbox")).toContainText("2건");
    await expect(row(page, "kakao_scheduled_delivery")).toContainText("꺼짐");
    await expect(page.getByTestId("cluster-nodes-panel")).toBeAttached();
    await expect(page.getByRole("button", { name: "초기 설정 다시 하기" })).toBeVisible();

    await row(page, "health.status").click();
    const facts = testInfo.project.name === "mobile" ? page.getByTestId("settings-edit-sheet") : page.getByTestId("settings-inspector");
    await expect(facts).toContainText("가동 시간1시간 0분");
    await expect(facts.getByText("TUI 출력 마무리 대기")).toHaveAttribute("title", "tui_o:boundary_pending:1509350778043895902");
    if (testInfo.project.name === "mobile") await page.keyboard.press("Escape");

    await row(page, "kakao_scheduled_delivery").click();
    await expect(page.getByText("Kakao 인증 정보를 넣은 뒤 AGENTDESK_KAKAO_ENABLED=true로 켜세요.")).toBeVisible();
    await expectNoHorizontalOverflow(page);
    await shot(page, testInfo, "connections");
  });

  test("phone: differing rows, category drill-down and a sheet that saves one row", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone layout");
    const writes = await mockSettings(page);
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
    const save = sheet.getByRole("button", { name: "4로 저장" });
    expect((await save.boundingBox())!.height).toBeGreaterThanOrEqual(48);
    await shot(page, testInfo, "sheet");
    await save.click();
    await expect(sheet).toHaveCount(0);
    expect(writes.find((write) => write.method === "PATCH")?.body).toEqual({ max_review_rounds: "4" });
    await page.getByRole("button", { name: "분류 목록으로" }).click();
    await expect(page.getByTestId("settings-differing")).toBeVisible();
    await expectNoHorizontalOverflow(page);
  });

  test("usage keeps the AI account bars next to their thresholds", async ({ page }) => {
    await mockSettings(page);
    await openSettings(page, "?category=usage");
    await expect(row(page, "rateLimitWarningPct")).toContainText("사용량 주의 표시 (%)");
    await expect(page.getByTestId("settings-providers-panel").getByText("7일 한도 · Fable 모델")).toBeVisible();
    await expect(page.getByTestId("settings-provider-usage-bar").first()).toHaveAttribute("data-level", "danger");
  });

  test("system: the language row switches the copy as soon as it is saved", async ({ page }, testInfo) => {
    await mockSettings(page);
    let stored: Record<string, unknown> = { companyName: "AgentDesk", ceoName: "윤호", language: "ko", theme: "dark" };
    const puts: unknown[] = [];
    await page.route(/\/api\/settings$/, (route) => {
      if (route.request().method() !== "PUT") return route.fulfill({ json: stored });
      stored = route.request().postDataJSON();
      puts.push(stored);
      return route.fulfill({ json: { ok: true } });
    });
    await openSettings(page, "?category=system");
    await expect(row(page, "language")).toContainText("기본 한국어 · 코드");
    if (testInfo.project.name === "mobile") {
      await row(page, "language").click();
      const sheet = page.getByTestId("settings-edit-sheet");
      await sheet.getByRole("radio", { name: "English" }).click();
      await sheet.getByRole("button", { name: "저장", exact: true }).click();
    } else {
      await row(page, "language").getByRole("radio", { name: "English" }).click();
      await page.getByTestId("settings-save-bar").getByRole("button", { name: "저장" }).click();
    }
    await expect(row(page, "language")).toContainText("Language");
    await expect(page.getByTestId("settings-search-input")).toHaveAttribute("placeholder", "Search by name, key or description");
    expect(puts).toEqual([expect.objectContaining({ language: "en", theme: "dark" })]);
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
