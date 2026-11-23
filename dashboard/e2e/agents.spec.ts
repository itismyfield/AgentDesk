import { expect, test, type Locator, type Page, type TestInfo } from "@playwright/test";

// Agents tab: what needs a look, the roster, detail open/close (URL `?agent=`), create and edit windows at 1440x900
// (desktop) and 390x844 (mobile).
// Set AGENTS_SHOT_DIR to also capture 390/820/1440 x light/dark screenshots outside the test output folder.

const NOW = 1760918400000;

const DEPARTMENTS = [
  { id: "dept-platform", name: "Platform", name_ko: "플랫폼", name_ja: null, name_zh: null, icon: "🧱", color: "#38bdf8", description: null, prompt: null, sort_order: 0, created_at: NOW },
  { id: "dept-product", name: "Product", name_ko: "프로덕트", name_ja: null, name_zh: null, icon: "🧭", color: "#f97316", description: null, prompt: null, sort_order: 1, created_at: NOW },
];

const agent = (overrides: { id: string; name: string; name_ko: string } & Record<string, unknown>) => ({
  role_id: null,
  alias: null,
  name_ja: null,
  name_zh: null,
  department_id: "dept-platform",
  cli_provider: "claude",
  avatar_emoji: "🤖",
  personality: null,
  status: "idle",
  current_task_id: null,
  workflow_pack_key: null,
  stats_tasks_done: 0,
  stats_tokens: 0,
  activity_source: "idle",
  created_at: NOW - 86_400_000,
  ...overrides,
});

const AGENTS = [
  agent({
    id: "agent-ada", role_id: "adk-dashboard", name: "Ada Dashboard", alias: "ada", name_ko: "아다 대시보드",
    cli_provider: "codex", status: "working", current_task_id: "#786", stats_tasks_done: 48, activity_source: "agentdesk",
    personality: "회의 허브와 운영 화면을 정리 중입니다. 변경한 파일과 근거 로그를 함께 보고하고, 재현이 안 되면 남은 질문을 목록으로 정리합니다. 디스코드 릴레이 도착 시각과 세션 트랜스크립트를 대조해 누락 구간을 찾고 원인 후보를 세 가지 이하로 좁혀 보고합니다.",
  }),
  agent({ id: "agent-luna", role_id: "project-agentdesk", name: "Luna Ops", name_ko: "루나 옵스", department_id: "dept-product", workflow_pack_key: "development", stats_tasks_done: 31 }),
  agent({ id: "agent-nova", name: "Nova Watch", name_ko: "노바 감시", status: "break", cli_provider: "gemini", stats_tasks_done: 1204 }),
  agent({ id: "agent-echo", name: "Echo Routine", name_ko: "에코 루틴", status: "offline", department_id: null, cli_provider: "opencode" }),
  agent({
    id: "agent-long", name: "Production relay duplicate delivery investigation agent with a very long name",
    name_ko: "프로덕션 릴레이의 누락과 중복 전달을 함께 조사하는 아주 긴 이름의 에이전트", alias: "relay-gap-diagnose-long-alias",
    session_info: "agentdesk://relay/sessions/ses-412/transcripts/2026-11-17T09:41:00Z-final-regression-check", stats_tasks_done: 999999,
  }),
  agent({ id: "e2e-agent-fixture", name: "E2E Fixture", name_ko: "E2E 테스트", role_id: "e2e-fixture" }),
];

const STATS = {
  agents: { total: 6, working: 1, idle: 3, break: 1, offline: 1 },
  departments: [],
  dispatched_count: 0,
  github_closed_today: 0,
  kanban: {
    open_total: 0, review_queue: 0, blocked: 0, failed: 0, waiting_acceptance: 0, stale_in_progress: 0,
    by_status: { backlog: 0, ready: 0, requested: 0, blocked: 0, in_progress: 0, review: 0, done: 0, qa_pending: 0, qa_in_progress: 0, qa_failed: 0 },
    top_repos: [],
  },
};

const PROVIDERS = ["claude", "codex", "gemini", "opencode"].map((id) => ({
  id, display_name: id[0].toUpperCase() + id.slice(1), channel_suffix: null, binary_name: id, execution_surface: "cli",
  supports_resume: true, supports_structured_output: true, supports_tool_stream: true, supports_restricted_tool_policy: false,
  supports_tui_hosting: true, system_prompt_transport: "file", context_window_tokens: null,
}));

const SKILL_RANKING = {
  window: "30d",
  overall: [
    { skill_name: "agents-hub", skill_desc_ko: "Agents Hub", calls: 6, last_used_at: NOW - 3_600_000 },
    { skill_name: "relay-audit", skill_desc_ko: "Relay audit", calls: 2, last_used_at: NOW - 7_200_000 },
  ],
  byAgent: [
    { agent_role_id: "agent-ada", agent_name: "Ada Dashboard", skill_name: "agents-hub", skill_desc_ko: "Agents Hub", calls: 4, last_used_at: NOW - 3_600_000 },
    { agent_role_id: "agent-luna", agent_name: "Luna Ops", skill_name: "agents-hub", skill_desc_ko: "Agents Hub", calls: 2, last_used_at: NOW - 3_600_000 },
  ],
};

const SKILL_CATALOG = [
  { name: "agents-hub", description: "Agents **hub** for the roster", description_ko: "에이전트 **허브**", total_calls: 40, last_used_at: NOW - 3_600_000 },
  { name: "relay-audit", description: "Watch relay delivery and recover gaps", description_ko: "릴레이 전달을 지켜보고 빈 곳을 복구", total_calls: 9, last_used_at: NOW - 7_200_000 },
  { name: "family-profile-probe", description: "Keep family profiles fresh", description_ko: "가족 프로필을 새로 유지", total_calls: 0, last_used_at: null },
];

/** Daily calls on the last UTC dates, as the server sends them (days without calls are left out). */
const skillTrend = () => [5, 0, 7].flatMap((count, back) => (count ? [{ day: new Date(Date.now() - back * 86_400_000).toISOString().slice(0, 10), count }] : []));

async function mockShell(page: Page, theme: "dark" | "light" = "dark") {
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
  await page.route(/\/api\/settings$/, (route) => route.fulfill({ json: { language: "ko", theme } }));
  await page.route(/\/api\/agents(?:\?.*)?$/, (route) => route.fulfill({ json: { agents: AGENTS } }));
  await page.route(/\/api\/agents\/[^/?]+$/, (route) => {
    const id = route.request().url().split("/").pop();
    return route.fulfill({ json: AGENTS.find((item) => item.id === id) ?? AGENTS[0] });
  });
  await page.route(/\/api\/departments(?:\?.*)?$/, (route) => route.fulfill({ json: { departments: DEPARTMENTS } }));
  await page.route(/\/api\/dispatched-sessions(?:\?.*)?$/, (route) => route.fulfill({ json: { sessions: [] } }));
  await page.route(/\/api\/stats(?:\?.*)?$/, (route) => route.fulfill({ json: STATS }));
  await page.route(/\/api\/audit-logs(?:\?.*)?$/, (route) => route.fulfill({ json: { logs: [] } }));
  await page.route(/\/api\/dispatches(?:\?.*)?$/, (route) => route.fulfill({ json: { dispatches: [] } }));
  await page.route(/\/api\/kanban-cards(?:\?.*)?$/, (route) => route.fulfill({ json: { cards: [] } }));
  await page.route(/\/api\/kanban-repos$/, (route) => route.fulfill({ json: { repos: [] } }));
  await page.route(/\/api\/skills\/ranking\?/, (route) => route.fulfill({ json: SKILL_RANKING }));
  await page.route(/\/api\/skills\/catalog$/, (route) => route.fulfill({ json: { catalog: SKILL_CATALOG } }));
  await page.route(/\/api\/skills-trend\?days=30$/, (route) => route.fulfill({ json: { trend: skillTrend() } }));
  await page.route(/\/api\/agents\/[^/]+\/cron$/, (route) => route.fulfill({ json: { jobs: [] } }));
  await page.route(/\/api\/agents\/[^/]+\/skills$/, (route) => route.fulfill({ json: { skills: [{ name: "agents-hub", description: "Agents hub" }], sharedSkills: [], totalCount: 1 } }));
  await page.route(/\/api\/agents\/[^/]+\/dispatched-sessions$/, (route) => route.fulfill({ json: { sessions: [] } }));
  await page.route(/\/api\/agents\/[^/]+\/timeline\?limit=\d+$/, (route) => route.fulfill({ json: { events: [] } }));
  await page.route(/\/api\/agents\/[^/]+\/transcripts\?limit=\d+$/, (route) => route.fulfill({ json: { transcripts: [] } }));
  await page.route(/\/api\/agents\/[^/]+\/execution-node$/, (route) => route.fulfill({ json: { default_node_id: null, routing_enforced: true } }));
  await page.route(/\/api\/discord\/bindings$/, (route) => route.fulfill({ json: { bindings: [] } }));
  await page.route(/\/api\/provider-auth-profiles$/, (route) => route.fulfill({ json: { providers: [], agent_profile_overrides: [] } }));
  await page.route(/\/api\/provider-cli$/, (route) => route.fulfill({ json: { catalog: PROVIDERS } }));
  await page.route(/\/api\/cluster\/nodes$/, (route) => route.fulfill({ json: { cluster: { enabled: false, local_instance_id: "single-node" }, nodes: [] } }));
}

async function expectNoHorizontalOverflow(page: Page) {
  const metrics = await page.evaluate(() => ({
    viewport: window.innerWidth,
    width: Math.max(document.body.scrollWidth, document.documentElement.scrollWidth),
  }));
  expect(metrics.width).toBeLessThanOrEqual(metrics.viewport);
}

/** Visible controls inside `scope` that are smaller than 44px on either side. */
async function smallTargets(scope: Locator) {
  return scope.evaluate((root) => {
    const selector = 'button, a[href], input:not([type="hidden"]), select, textarea, [role="tab"], [role="radio"], [role="option"], [role="checkbox"]';
    const out: string[] = [];
    for (const node of Array.from(root.querySelectorAll<HTMLElement>(selector))) {
      const rect = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      if (!rect.width || !rect.height || style.visibility === "hidden" || node.closest("[hidden], dialog:not([open]), [popover]:not(:popover-open)")) continue;
      // Kit fields focus their input from the whole field box, and a checkbox toggles from its label row.
      const owner = node.matches("input, textarea")
        ? node.closest<HTMLElement>('[class*="-clean__box"], label') ?? node
        : node;
      const ownerRect = owner.getBoundingClientRect();
      const box = { width: Math.max(rect.width, ownerRect.width), height: Math.max(rect.height, ownerRect.height) };
      if (Math.min(box.width, box.height) < 44) out.push(`${node.tagName.toLowerCase()}[${node.getAttribute("aria-label") ?? node.textContent?.trim().slice(0, 24)}] ${Math.round(box.width)}x${Math.round(box.height)}`);
    }
    return out;
  });
}

async function smallInputFonts(scope: Locator) {
  return scope.evaluate((root) =>
    Array.from(root.querySelectorAll<HTMLElement>('input:not([type="checkbox"]):not([type="hidden"]), textarea, select'))
      .filter((node) => node.getBoundingClientRect().width > 0 && parseFloat(getComputedStyle(node).fontSize) < 16)
      .map((node) => `${node.getAttribute("aria-label") ?? node.id} ${getComputedStyle(node).fontSize}`),
  );
}

async function openAgents(page: Page, theme: "dark" | "light" = "dark", extra?: (page: Page) => Promise<void>) {
  await mockShell(page, theme);
  await extra?.(page);
  await page.goto("/agents");
  await expect(page.getByTestId("agents-tab")).toBeVisible({ timeout: 15000 });
  await expect(page.getByTestId("agents-row-agent-ada")).toBeVisible();
}

const shotDir = process.env.AGENTS_SHOT_DIR;
async function shot(page: Page, testInfo: TestInfo, name: string) {
  const file = `${name}.png`;
  await page.waitForTimeout(450);
  await page.screenshot({ path: shotDir ? `${shotDir}/${file}` : testInfo.outputPath(file) });
}

test.describe("Agents tab", () => {
  test("lists agents with home's states, search and an empty state", async ({ page }) => {
    await openAgents(page);
    // Test agents stay hidden until asked for.
    const rows = page.getByTestId("agents-list").locator("[data-agent-item]");
    await expect(rows).toHaveCount(5);
    await expect(page.getByTestId("agents-row-e2e-agent-fixture")).toHaveCount(0);
    await page.getByTestId("agents-include-tests").check();
    await expect(rows).toHaveCount(6);
    await page.getByTestId("agents-include-tests").uncheck();
    await expect(page.getByTestId("agents-attention-empty")).toBeVisible();

    const tabs = page.getByRole("tablist", { name: "에이전트 상태" });
    await expect(tabs.getByRole("tab", { name: /전체/ })).toHaveAttribute("aria-selected", "true");
    await expect(tabs.getByRole("tab", { name: /휴식|오프라인/ })).toHaveCount(0);
    await tabs.getByRole("tab", { name: /작업 중/ }).click();
    await expect(rows).toHaveCount(1);
    await expect(page.getByLabel(/^아다 대시보드|^ada/).first()).toBeVisible();
    await tabs.getByRole("tab", { name: /전체/ }).click();

    await page.getByRole("searchbox", { name: "에이전트 검색" }).fill("없는 이름");
    await expect(page.getByText("검색 결과 없음")).toBeVisible();
    await page.getByRole("button", { name: "조건 지우기" }).click();
    await expect(rows).toHaveCount(5);
    await expectNoHorizontalOverflow(page);
  });

  test("puts stuck and failing agents in a card above the roster", async ({ page }, testInfo) => {
    await mockShell(page);
    await page.route(/\/api\/agents(?:\?.*)?$/, (route) => route.fulfill({
      json: { agents: AGENTS.map((item) => (item.id === "agent-nova" ? { ...item, visual_status_code: "stuck-suspect" } : item)) },
    }));
    await page.route(/\/api\/kanban-cards(?:\?.*)?$/, (route) => route.fulfill({ json: { cards: [{
      id: "card-9", title: "릴레이 재시도", status: "failed", assignee_agent_id: "agent-luna", github_issue_number: 9012,
      github_repo: "acme/agentdesk", latest_dispatch_result_summary: "테스트 실패", created_at: NOW, updated_at: NOW,
    }] } }));
    await page.goto("/agents");
    const card = page.getByTestId("agents-attention");
    await expect(card).toContainText("살펴볼 것");
    await expect(card).toContainText("막힘 의심");
    await expect(card).toContainText("#9012 릴레이 재시도");
    await expect(page.getByTestId("agents-attention-empty")).toHaveCount(0);
    if (testInfo.project.name === "desktop") {
      await card.getByRole("button", { name: "상세" }).click();
      await expect(page.getByRole("region", { name: /노바 감시 — 에이전트 상세/ })).toBeVisible();
    } else {
      await card.getByRole("button", { name: /노바 감시/ }).click();
      await expect(page.getByRole("dialog", { name: /노바 감시 — 에이전트 상세/ })).toBeVisible();
    }
    await expect(page).toHaveURL(/\?agent=agent-nova$/);
  });

  test("opens and closes the agent detail, with the agent in the URL", async ({ page }, testInfo) => {
    await openAgents(page);
    const isMobile = testInfo.project.name === "mobile";
    const card = page.getByTestId("agents-row-agent-ada");
    if (!isMobile) {
      const pane = page.getByTestId("agents-detail-pane");
      await expect(pane.getByTestId("agents-summary")).toContainText("부서별 인원");
      await card.click();
      await expect(page).toHaveURL(/\/agents\?agent=agent-ada$/);
      const detail = page.getByRole("region", { name: /ada — 에이전트 상세/ });
      await expect(detail).toBeVisible();
      await expect(card).toHaveAttribute("aria-current", "true");
      await expect(detail.getByRole("heading", { name: "ada" })).toBeVisible();
      await expect(detail.getByRole("button", { name: "편집" })).toBeVisible();
      await expect(detail.getByText("Agents Hub").or(detail.getByText("agents-hub")).first()).toBeVisible();
      // Long personality text clamps with a toggle.
      await detail.getByRole("button", { name: "전체 보기" }).click();
      await expect(detail.getByRole("button", { name: "접기" })).toHaveAttribute("aria-expanded", "true");
      // Arrow keys walk the list without opening anything.
      await card.focus();
      await page.keyboard.press("ArrowDown");
      await expect(page.getByTestId("agents-list").locator("[data-agent-item]").nth(1)).toBeFocused();
      await page.getByRole("button", { name: "상세 닫기" }).click();
      await expect(page).toHaveURL(/\/agents$/);
      await expect(pane.getByTestId("agents-summary")).toBeVisible();
      // Switching tabs with a detail open and going back lands on the tab the address names.
      await card.click();
      await page.getByRole("tab", { name: /^부서/ }).click();
      await expect(page).toHaveURL(/agent=agent-ada&tab=departments$/);
      await page.goBack();
      await expect(page).toHaveURL(/\/agents$/);
      await expect(page.getByRole("tab", { name: /^에이전트/ })).toHaveAttribute("aria-selected", "true");
    } else {
      await card.click();
      const drawer = page.getByRole("dialog", { name: /ada — 에이전트 상세/ });
      await expect(drawer).toBeVisible();
      const box = await drawer.boundingBox();
      expect(box).toMatchObject({ x: 0, y: 0, width: 390, height: 844 });
      const back = drawer.getByRole("button", { name: "목록으로" });
      await expect(back).toBeFocused();
      await back.click();
      await expect(drawer).toHaveCount(0);
      await expect(card).toBeFocused();
      // The browser back button closes the detail too, and a link opens it directly.
      await card.click();
      await expect(drawer).toBeVisible();
      await page.goBack();
      await expect(drawer).toHaveCount(0);
      await expect(page).toHaveURL(/\/agents$/);
      await page.goto("/agents?agent=agent-ada");
      await expect(drawer).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(drawer).toHaveCount(0);
      await expect(page).toHaveURL(/\/agents$/);
      // Two closes in one frame step back once: the list stays, not the page before it.
      await page.goto("/home");
      await page.goto("/agents");
      await card.click();
      await drawer.evaluate((node) => {
        for (let i = 0; i < 2; i += 1) node.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
      });
      await expect(drawer).toHaveCount(0);
      await expect(page).toHaveURL(/\/agents$/);
      await expect(card).toBeVisible();
    }
    await expectNoHorizontalOverflow(page);
  });

  test("detail sections keep their order, fold on phones and read folded ones only when opened", async ({ page }, testInfo) => {
    const isMobile = testInfo.project.name === "mobile";
    await openAgents(page);
    const reads: string[] = [];
    page.on("request", (request) => reads.push(new URL(request.url()).pathname));
    const lazy = () => reads.filter((path) => /\/(cron|timeline|transcripts)$|\/audit-logs$/.test(path)).sort();
    await page.getByTestId("agents-row-agent-ada").click();
    const detail = isMobile ? page.getByRole("dialog", { name: /ada — 에이전트 상세/ }) : page.getByRole("region", { name: /ada — 에이전트 상세/ });
    await expect(detail.getByTestId("agent-detail-now")).toBeVisible();
    expect(await detail.locator('section[data-testid^="agent-detail-"]').evaluateAll((nodes) => nodes.map((node) => node.getAttribute("data-testid")?.slice(13))))
      .toEqual(["now", "run", "connections", "turns", "skills", "activity", "cron", "source"]);
    const folds = detail.locator("section h3 > button[aria-expanded]");
    await expect(folds).toHaveCount(isMobile ? 7 : 3);
    if (isMobile) expect(Math.min(...await folds.evaluateAll((nodes) => nodes.map((node) => node.getBoundingClientRect().height)))).toBeGreaterThanOrEqual(48);
    expect(lazy()).toEqual([]);
    await detail.getByRole("button", { name: /^최근 턴/ }).click();
    await expect(detail.getByTestId("turn-transcript-panel")).toBeVisible();
    await detail.getByRole("button", { name: /^크론/ }).click();
    await detail.getByRole("button", { name: /^활동/ }).click();
    await expect.poll(lazy).toEqual(["/api/agents/agent-ada/cron", "/api/agents/agent-ada/timeline", "/api/agents/agent-ada/transcripts", "/api/audit-logs"]);
  });

  test("create window and edit form open as sheets on phones", async ({ page }, testInfo) => {
    await openAgents(page);
    const isMobile = testInfo.project.name === "mobile";
    await page.getByRole("button", { name: /에이전트 추가/ }).click();
    const wizard = page.getByRole("dialog", { name: "에이전트 설정 마법사" });
    await expect(wizard).toBeVisible();
    await expect(wizard.getByRole("heading", { name: "에이전트 생성" })).toBeVisible();
    // Sheets slide in from the bottom; measure once the motion settles.
    const bottomOf = async (locator: Locator) => {
      const box = await locator.boundingBox();
      return box ? Math.round(box.y + box.height) : -1;
    };
    if (isMobile) {
      await expect.poll(() => bottomOf(wizard)).toBe(844);
      expect((await wizard.boundingBox())!.width).toBe(390);
    } else {
      expect((await wizard.boundingBox())!.y).toBeGreaterThan(0);
    }
    await expectNoHorizontalOverflow(page);
    await page.keyboard.press("Escape");
    await expect(wizard).toHaveCount(0);

    await page.getByTestId("agents-row-agent-luna").click();
    const detail = isMobile
      ? page.getByRole("dialog", { name: /루나 옵스 — 에이전트 상세/ })
      : page.getByRole("region", { name: /루나 옵스 — 에이전트 상세/ });
    await detail.getByRole("button", { name: "편집" }).click();
    const form = page.getByRole("dialog", { name: "에이전트 수정" });
    await expect(form).toBeVisible();
    await expect(form.getByLabel(/영문 이름/)).toHaveValue("Luna Ops");
    await expect(form.getByLabel(/영문 이름/)).toBeFocused();
    if (isMobile) await expect.poll(() => bottomOf(form)).toBe(844);
    await form.getByRole("button", { name: "취소" }).click();
    await expect(form).toHaveCount(0);
    await expect(detail).toBeVisible();
  });
});

test.describe("Shared agent detail overlay", () => {
  test("opens from the home agents card and closes", async ({ page }, testInfo) => {
    await mockShell(page);
    await page.goto("/home");
    const isMobile = testInfo.project.name === "mobile";
    await page.getByTestId("home-agents").getByRole("button", { name: /^(아다 대시보드|Ada Dashboard)/ }).click();
    const overlay = page.getByRole("dialog", { name: /ada — 에이전트 상세/ });
    await expect(overlay).toBeVisible();
    await expect(overlay.getByRole("heading", { name: "ada" })).toBeVisible();
    // Phones fold every section but "now".
    if (isMobile) await overlay.getByRole("button", { name: /^실행 설정/ }).click();
    await expect(overlay.getByLabel(/우선 실행 장비/)).toBeVisible();
    const box = (await overlay.boundingBox())!;
    if (isMobile) {
      expect(box).toMatchObject({ x: 0, y: 0, width: 390, height: 844 });
      await overlay.getByRole("button", { name: "돌아가기" }).click();
    } else {
      expect(box.y).toBeGreaterThan(0);
      expect(box.width).toBeLessThan(1440);
      await overlay.getByRole("button", { name: "닫기" }).click();
    }
    await expect(overlay).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
  });
});

test.describe("Departments and skills tabs", () => {
  /** Departments served from a list the reorder route rewrites, as the server does. */
  async function mockDepartments(page: Page) {
    let current = DEPARTMENTS;
    const writes: Array<{ method: string; url: string; body: unknown }> = [];
    await page.route(/\/api\/departments(?:\?.*)?$/, (route) => route.fulfill({ json: { departments: current } }));
    await page.route(/\/api\/departments\/[^/?]+$/, (route) => {
      const request = route.request();
      writes.push({ method: request.method(), url: request.url().replace(/^.*\/api/, "/api"), body: request.postDataJSON() });
      if (request.url().endsWith("/reorder")) {
        const order = (request.postDataJSON() as { order: Array<{ id: string; sort_order: number }> }).order;
        current = order.map(({ id, sort_order }) => ({ ...DEPARTMENTS.find((dept) => dept.id === id)!, sort_order }));
      }
      return route.fulfill({ json: { ok: true } });
    });
    return writes;
  }

  test("departments: one row each, arrows reorder and the order saves through the reorder route", async ({ page }, testInfo) => {
    await mockShell(page);
    const writes = await mockDepartments(page);
    await page.goto("/agents?tab=departments");
    const rows = page.getByTestId("agents-departments").locator("li");
    await expect(rows).toHaveCount(2);
    await expect(rows.first()).toContainText("플랫폼");
    await expect(rows.first()).toContainText("dept-platform · 순서 1");
    await expect(page.getByTestId("agents-departments-order-bar")).toHaveCount(0);

    if (testInfo.project.name === "desktop") {
      // Dragging a row onto the top half of another drops it before that row; undo puts the server order back.
      await rows.nth(1).dragTo(rows.first(), { targetPosition: { x: 40, y: 4 } });
      await expect(rows.first()).toContainText("프로덕트");
      await page.getByRole("button", { name: "되돌리기" }).click();
      await expect(rows.first()).toContainText("플랫폼");
    }
    await page.getByRole("button", { name: "플랫폼 아래로" }).click();
    await expect(rows.first()).toContainText("프로덕트");
    // At the bottom the down arrow is off, so the focus moves to the same row's up arrow.
    await expect(page.getByRole("button", { name: "플랫폼 위로" })).toBeFocused();
    const bar = page.getByTestId("agents-departments-order-bar");
    await bar.getByRole("button", { name: "순서 저장" }).click();
    await expect(bar).toHaveCount(0);
    await expect(rows.first()).toContainText("프로덕트");
    expect(writes).toEqual([{
      method: "PATCH",
      url: "/api/departments/reorder",
      body: { order: [{ id: "dept-product", sort_order: 0 }, { id: "dept-platform", sort_order: 1 }] },
    }]);
    await expectNoHorizontalOverflow(page);
  });

  test("departments: the edit window saves the name, shows the rest read-only and deletes after a warning", async ({ page }) => {
    await mockShell(page);
    const writes = await mockDepartments(page);
    await page.goto("/agents?tab=departments");
    await page.getByRole("button", { name: /^플랫폼 편집/ }).click();
    const form = page.getByRole("dialog", { name: "부서 편집" });
    const name = form.getByLabel(/영문 이름/);
    await expect(name).toHaveValue("Platform");
    await expect(name).toBeFocused();
    await expect(form.getByRole("button", { name: "이름 저장" })).toBeDisabled();
    await expect(form.getByLabel("한글 이름")).toHaveValue("플랫폼");
    await expect(form.getByLabel("한글 이름")).toBeDisabled();
    await expect(form.getByRole("button", { name: /아이콘/ })).toBeDisabled();
    await name.fill("Runtime");
    await form.getByRole("button", { name: "이름 저장" }).click();
    await expect(form).toHaveCount(0);
    // The page stays put: the form submits through the app, not as a browser navigation.
    await expect(page).toHaveURL(/\/agents\?tab=departments$/);
    expect(writes).toEqual([{ method: "PATCH", url: "/api/departments/dept-platform", body: { name: "Runtime" } }]);

    await page.getByRole("button", { name: /^플랫폼 편집/ }).click();
    await form.getByRole("button", { name: "부서 삭제" }).click();
    await expect(form).toContainText("소속 에이전트 4명은 미배정이 돼요");
    await form.getByRole("button", { name: "삭제", exact: true }).click();
    await expect(form).toHaveCount(0);
    expect(writes.at(-1)).toMatchObject({ method: "DELETE", url: "/api/departments/dept-platform" });

    await page.getByRole("button", { name: "부서 추가" }).click();
    const create = page.getByRole("dialog", { name: "새 부서" });
    await create.getByLabel(/영문 이름/).fill("Research");
    await create.getByRole("button", { name: "부서 추가" }).click();
    await expect(create).toHaveCount(0);
    await expectNoHorizontalOverflow(page);
  });

  test("skills: 30-day chart, most used skills with their agents, and the catalog in the settings language", async ({ page }, testInfo) => {
    await mockShell(page);
    await page.goto("/agents?tab=skills");
    const tab = page.getByTestId("skills-catalog");
    // Settings say Korean while the browser is en-US.
    await expect(tab.getByRole("searchbox", { name: "스킬 검색" })).toBeVisible();
    await expect(page.getByText(/Search skills|Last Used/)).toHaveCount(0);
    await expect(page.getByTestId("skills-trend")).toContainText("12회");
    await expect(tab.getByTestId("skills-row-agents-hub")).toContainText("에이전트 허브");
    await expect(tab.getByTestId("skills-row-agents-hub")).toContainText("누적 40회");
    await expect(tab.getByTestId("skills-row-family-profile-probe")).toContainText("메모리");

    await page.getByTestId("skills-top").getByRole("button", { name: /^agents-hub, 6회/ }).click();
    const picked = page.getByTestId("skills-picked");
    await expect(picked.getByRole("button")).toHaveCount(2);
    await picked.getByRole("button", { name: /^ada 4회/ }).click();
    await expect(page).toHaveURL(/\/agents\?agent=agent-ada$/);
    await expect(testInfo.project.name === "desktop"
      ? page.getByRole("region", { name: /ada — 에이전트 상세/ })
      : page.getByRole("dialog", { name: /ada — 에이전트 상세/ })).toBeVisible();
    await page.goBack();
    await expect(page).toHaveURL(/\/agents\?tab=skills$/);

    await tab.getByRole("searchbox", { name: "스킬 검색" }).fill("relay");
    await expect(page.getByTestId("skills-list").locator("li")).toHaveCount(1);
    await tab.getByRole("searchbox", { name: "스킬 검색" }).fill("");
    await tab.getByRole("tab", { name: /^메모리/ }).click();
    await expect(page.getByTestId("skills-list").locator("li")).toHaveCount(1);
    await expectNoHorizontalOverflow(page);
  });
});

test.describe("Agents tab on a touch phone", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("keeps 44px targets, 16px inputs and no horizontal overflow", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone-only check");
    // Ada has a route, a live thread with app links and more finished sessions than one page.
    await openAgents(page, "dark", async (page) => {
      const thread = (id: string, status: string) => ({ id, session_key: `codex/x/adk-cdx-t${id}`, status, provider: "codex", thread_channel_id: id, thread_deeplink_url: `discord://discord.com/channels/1400000000000000000/${id}` });
      await page.route(/\/api\/agents\/agent-ada\/dispatched-sessions$/, (route) => route.fulfill({ json: { sessions: [thread("1490145076658307001", "turn_active"), ...Array.from({ length: 21 }, (_, index) => thread(`14901450766583072${String(index).padStart(2, "0")}`, "aborted"))] } }));
      await page.route(/\/api\/discord\/bindings$/, (route) => route.fulfill({ json: { bindings: [{ agentId: "agent-ada", channelId: "1490145076658307100", source: "primary" }] } }));
      await page.route(/\/api\/discord\/channels\/\d+$/, (route) => route.fulfill({ json: { id: route.request().url().split("/").pop(), name: "adk-cc", guild_id: "1400000000000000000", parent_id: null } }));
    });
    await expectNoHorizontalOverflow(page);
    const tab = page.getByTestId("agents-tab");
    expect(await smallTargets(tab)).toEqual([]);
    expect(await smallInputFonts(tab)).toEqual([]);

    await page.getByTestId("agents-row-agent-ada").tap();
    const drawer = page.getByRole("dialog", { name: /에이전트 상세/ });
    await expect(drawer).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(drawer)).toEqual([]);
    expect(await smallInputFonts(drawer)).toEqual([]);
    // Every section open, finished sessions included: links, app chips and buttons stay 44px.
    const folded = drawer.locator('section h3 > button[aria-expanded="false"]');
    while (await folded.count()) await folded.first().tap();
    await drawer.getByRole("button", { name: "끝난 세션 21개" }).tap();
    await expect(drawer.getByTestId("agent-detail-finished").locator("li")).toHaveCount(20);
    await expect(drawer.getByRole("link", { name: "앱" }).first()).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(drawer)).toEqual([]);
    expect(await smallInputFonts(drawer)).toEqual([]);

    await drawer.getByRole("button", { name: "편집" }).tap();
    const form = page.getByRole("dialog", { name: "에이전트 수정" });
    await expect(form).toBeVisible();
    // Measure once the sheet and its fields stop moving.
    await form.evaluate((node) => Promise.all(node.getAnimations({ subtree: true }).map((animation) => animation.finished)));
    expect(await smallTargets(form)).toEqual([]);
    expect(await smallInputFonts(form)).toEqual([]);

    await form.getByRole("button", { name: "취소" }).tap();
    await drawer.getByRole("button", { name: "목록으로" }).tap();
    await page.getByRole("button", { name: /에이전트 추가/ }).tap();
    const wizard = page.getByRole("dialog", { name: "에이전트 설정 마법사" });
    await expect(wizard).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(wizard)).toEqual([]);
    expect(await smallInputFonts(wizard)).toEqual([]);
  });
});

test.describe("Departments and skills on a touch phone", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("keeps 44px targets, 16px inputs and no horizontal overflow", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone-only check");
    await mockShell(page);
    await page.goto("/agents?tab=departments");
    const departments = page.getByTestId("agents-departments-tab");
    await expect(departments.locator("li")).toHaveCount(2);
    await page.getByRole("button", { name: "플랫폼 아래로" }).tap();
    await expect(page.getByTestId("agents-departments-order-bar")).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(departments)).toEqual([]);

    await page.getByRole("button", { name: /^플랫폼 편집/ }).tap();
    const form = page.getByRole("dialog", { name: "부서 편집" });
    await expect(form).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(form)).toEqual([]);
    expect(await smallInputFonts(form)).toEqual([]);
    await form.getByRole("button", { name: "취소" }).tap();

    await page.goto("/agents?tab=skills");
    const skills = page.getByTestId("skills-catalog");
    await expect(skills.getByTestId("skills-row-agents-hub")).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(skills)).toEqual([]);
    expect(await smallInputFonts(skills)).toEqual([]);
  });
});

test.describe("Agents tab screenshots", () => {
  test.skip(!shotDir, "Set AGENTS_SHOT_DIR to capture screenshots");

  for (const theme of ["light", "dark"] as const) {
    for (const size of [{ w: 390, h: 844 }, { w: 820, h: 1180 }, { w: 1440, h: 900 }]) {
      test(`${size.w}x${size.h} ${theme}`, async ({ page }, testInfo) => {
        test.skip(testInfo.project.name === "mobile", "Sizes are set per test");
        await page.setViewportSize({ width: size.w, height: size.h });
        await openAgents(page, theme);
        const prefix = `agents-${size.w}-${theme}`;
        await shot(page, testInfo, `${prefix}-list`);
        const phoneLayout = size.w < 900;
        await page.getByTestId("agents-row-agent-ada").click();
        await shot(page, testInfo, `${prefix}-detail`);
        if (!phoneLayout) {
          await page.getByTestId("agents-row-agent-long").click();
          await shot(page, testInfo, `${prefix}-rows-long`);
        }
        await page.getByRole("button", { name: "편집" }).click();
        await expect(page.getByRole("dialog", { name: "에이전트 수정" })).toBeVisible();
        await shot(page, testInfo, `${prefix}-edit`);
        await page.getByRole("dialog", { name: "에이전트 수정" }).getByRole("button", { name: "취소" }).click();
        if (phoneLayout) await page.getByRole("button", { name: "목록으로" }).click();
        await page.getByRole("button", { name: /에이전트 추가/ }).click();
        await expect(page.getByRole("dialog", { name: "에이전트 설정 마법사" })).toBeVisible();
        await shot(page, testInfo, `${prefix}-create`);
      });
    }
  }
});
