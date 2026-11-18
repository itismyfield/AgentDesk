import { expect, test, type Locator, type Page, type TestInfo } from "@playwright/test";

// Agents tab: list, detail open/close, create and edit windows at 1440x900 (desktop) and 390x844 (mobile).
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
  overall: [],
  byAgent: [
    { agent_role_id: "adk-dashboard", agent_name: "Ada Dashboard", skill_name: "agents-hub", skill_desc_ko: "Agents Hub", calls: 4, last_used_at: NOW - 3_600_000 },
  ],
};

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
  await page.route(/\/api\/agents\/[^/]+\/cron$/, (route) => route.fulfill({ json: { jobs: [] } }));
  await page.route(/\/api\/agents\/[^/]+\/skills$/, (route) => route.fulfill({ json: { skills: [{ name: "agents-hub", description: "Agents hub" }], sharedSkills: [], totalCount: 1 } }));
  await page.route(/\/api\/agents\/[^/]+\/dispatched-sessions$/, (route) => route.fulfill({ json: { sessions: [] } }));
  await page.route(/\/api\/agents\/[^/]+\/timeline\?limit=\d+$/, (route) => route.fulfill({ json: { events: [] } }));
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

async function openAgents(page: Page, theme: "dark" | "light" = "dark") {
  await mockShell(page, theme);
  await page.goto("/agents");
  await expect(page.getByTestId("agents-tab")).toBeVisible({ timeout: 15000 });
  await expect(page.getByTestId("agents-card-agent-ada").or(page.getByTestId("agents-grid-card-agent-ada"))).toBeVisible();
}

const shotDir = process.env.AGENTS_SHOT_DIR;
async function shot(page: Page, testInfo: TestInfo, name: string) {
  const file = `${name}.png`;
  await page.waitForTimeout(450);
  await page.screenshot({ path: shotDir ? `${shotDir}/${file}` : testInfo.outputPath(file) });
}

test.describe("Agents tab", () => {
  test("lists agents with status tabs, search and an empty state", async ({ page }, testInfo) => {
    await openAgents(page);
    const isMobile = testInfo.project.name === "mobile";
    await expect(page.getByTestId(isMobile ? "agents-view-list" : "agents-view-grid")).toBeVisible();
    // Test agents stay hidden until asked for.
    await expect(page.getByText("5명 표시")).toBeVisible();
    await expect(page.getByTestId("agents-card-e2e-agent-fixture").or(page.getByTestId("agents-grid-card-e2e-agent-fixture"))).toHaveCount(0);
    await page.getByTestId("agents-include-tests").check();
    await expect(page.getByText("6명 표시")).toBeVisible();
    await page.getByTestId("agents-include-tests").uncheck();

    const tabs = page.getByRole("tablist", { name: "에이전트 상태" });
    await expect(tabs.getByRole("tab", { name: /전체/ })).toHaveAttribute("aria-selected", "true");
    await tabs.getByRole("tab", { name: /휴식/ }).click();
    await expect(page.getByText("1명 표시")).toBeVisible();
    await expect(page.getByLabel(/노바 감시/).first()).toBeVisible();
    await tabs.getByRole("tab", { name: /전체/ }).click();

    await page.getByRole("searchbox", { name: "에이전트 검색" }).fill("없는 이름");
    await expect(page.getByText("검색 결과 없음")).toBeVisible();
    await page.getByRole("button", { name: "조건 지우기" }).click();
    await expect(page.getByText("5명 표시")).toBeVisible();

    if (!isMobile) {
      await page.getByRole("radio", { name: "리스트" }).click();
      await expect(page.getByTestId("agents-view-list")).toBeVisible();
      await page.getByRole("radio", { name: "그리드" }).click();
      await expect(page.getByTestId("agents-view-grid")).toBeVisible();
    }
    await expectNoHorizontalOverflow(page);
  });

  test("opens and closes the agent detail", async ({ page }, testInfo) => {
    await openAgents(page);
    const isMobile = testInfo.project.name === "mobile";
    const card = page.getByTestId(isMobile ? "agents-card-agent-ada" : "agents-grid-card-agent-ada");
    if (!isMobile) {
      const pane = page.getByTestId("agents-detail-pane");
      await expect(pane).toContainText("선택한 에이전트가 없어요");
      await card.click();
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
      await page.keyboard.press("ArrowRight");
      await expect(page.getByTestId("agents-grid-card-agent-luna")).toBeFocused();
      await page.getByRole("button", { name: "상세 닫기" }).click();
      await expect(pane).toContainText("선택한 에이전트가 없어요");
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
      await card.click();
      await expect(drawer).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(drawer).toHaveCount(0);
    }
    await expectNoHorizontalOverflow(page);
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

    await page.getByTestId(isMobile ? "agents-card-agent-luna" : "agents-grid-card-agent-luna").click();
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
  test("opens from the home working list and closes", async ({ page }, testInfo) => {
    await mockShell(page);
    await page.goto("/home");
    const isMobile = testInfo.project.name === "mobile";
    await page.getByTestId("home-working-agents").getByRole("button").first().click();
    const overlay = page.getByRole("dialog", { name: /ada — 에이전트 상세/ });
    await expect(overlay).toBeVisible();
    await expect(overlay.getByRole("heading", { name: "ada" })).toBeVisible();
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

test.describe("Agents tab on a touch phone", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("keeps 44px targets, 16px inputs and no horizontal overflow", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone-only check");
    await openAgents(page);
    await expectNoHorizontalOverflow(page);
    const tab = page.getByTestId("agents-tab");
    expect(await smallTargets(tab)).toEqual([]);
    expect(await smallInputFonts(tab)).toEqual([]);

    await page.getByTestId("agents-card-agent-ada").tap();
    const drawer = page.getByRole("dialog", { name: /에이전트 상세/ });
    await expect(drawer).toBeVisible();
    await expectNoHorizontalOverflow(page);
    expect(await smallTargets(drawer)).toEqual([]);
    expect(await smallInputFonts(drawer)).toEqual([]);

    await drawer.getByRole("button", { name: "편집" }).tap();
    const form = page.getByRole("dialog", { name: "에이전트 수정" });
    await expect(form).toBeVisible();
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
        await page.getByTestId(phoneLayout ? "agents-card-agent-ada" : "agents-grid-card-agent-ada").click();
        await shot(page, testInfo, `${prefix}-detail`);
        if (!phoneLayout) {
          await page.getByRole("radio", { name: "리스트" }).click();
          await page.getByTestId("agents-card-agent-long").click();
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
