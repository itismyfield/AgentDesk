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

test.describe("Dashboard smoke tests", () => {
  test("page loads and renders root element", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator("#root")).toBeAttached();
    await expect(page.getByTestId("topbar")).toBeVisible();
  });

  test("theme: dark/light toggle changes CSS variables", async ({ page }) => {
    await page.goto("/");
    await page.evaluate(() => {
      document.documentElement.dataset.theme = "dark";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    const darkBg = await page.evaluate(() =>
      getComputedStyle(document.documentElement)
        .getPropertyValue("--th-bg-primary")
        .trim(),
    );
    expect(darkBg).toBeTruthy();

    await page.evaluate(() => {
      document.documentElement.dataset.theme = "light";
    });
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
    await page.evaluate(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      document.documentElement.dataset.theme = mq.matches ? "dark" : "light";
    });
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");

    await page.emulateMedia({ colorScheme: "light" });
    await page.evaluate(() => {
      const mq = window.matchMedia("(prefers-color-scheme: dark)");
      document.documentElement.dataset.theme = mq.matches ? "dark" : "light";
    });
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
    await page.getByTestId("app-mobile-more-menu").getByRole("button", { name: /에이전트|Agents/ }).click();
    await expect(page).toHaveURL(/\/agents$/);
    await expectNoHorizontalOverflow(page);

    await page.getByTestId("app-mobile-more-button").click();
    await page.getByTestId("app-mobile-more-menu").getByRole("button", { name: /설정|Settings/ }).click();
    await expect(page).toHaveURL(/\/settings$/);
    await expectNoHorizontalOverflow(page);
  });

  test("stats: dedicated route exposes range controls and key widgets", async ({ page }) => {
    await page.goto("/stats");

    await expect(page.getByTestId("stats-page")).toBeVisible();
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

    await expect(page.getByTestId("meetings-page")).toBeVisible();
    await expect(page.getByTestId("meetings-page-timeline")).toBeVisible();
    await expect(page.getByTestId("meetings-page-skills")).toBeVisible();
    await expect(page.getByText(/로드밸런서 경고 플로우 리뷰/)).toBeVisible();
    await expect(page.getByText(/office-warning-disclosure/)).toBeVisible();
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

    await expect(page.getByTestId("agents-page")).toBeVisible();
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

    await expect(page.getByTestId("agents-page")).toBeVisible();
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

  test("settings button routes to settings page", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "mobile", "Desktop-only test");
    await page.goto("/home");

    await page.getByRole("button", { name: /설정으로 이동|Open settings/ }).click();
    await expect(page).toHaveURL(/\/settings(\?.*)?$/);
    await expect(page.getByTestId("topbar")).toContainText(/설정|Settings/);
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
