import { expect, test, type Page } from "@playwright/test";

const SHOTS = process.env.CAMPAIGN_QUEUE_SHOTS;
const NOW = "2026-10-31T08:00:00.000Z";

const agent = (id: string, name: string, nameKo: string) => ({
  id, role_id: id, name, alias: null, name_ko: nameKo, name_ja: null, name_zh: null, department_id: null, cli_provider: "codex",
  avatar_emoji: "🛠️", sprite_number: 3, personality: null, status: "idle", current_task_id: null, workflow_pack_key: "development",
  stats_tasks_done: 0, stats_tokens: 0, activity_source: "agentdesk", created_at: 1_790_000_000_000,
});
const AGENTS = [agent("agent-ada", "Ada Dashboard", "아다 대시보드"), agent("agent-luna", "Luna Ops", "루나 옵스")];

const node = (id: string, title: string, patch: Record<string, unknown> = {}) => ({
  id, title, status: "pending", stage: "implement", group: "Gateway", round: 1, assignee: null, session_id: null, provider: null,
  dependencies: [], issue_url: null, pr_url: null, head_sha: null, evidence: [], next_action: null, blocker: null, summary: null,
  benefit: null, post_merge_hold: null, updated_at: NOW, details: "", acceptance: [], findings: [], evidence_records: [], ...patch,
});
const campaign = (id: string, title: string, nodes: unknown[]) => ({
  id, title, description: "", status: "active", round: 2, revision: 7, auto_queue: false, nodes, created_at: NOW, updated_at: NOW,
});
const issue = (number: number) => `https://github.com/itismyfield/AgentDesk/issues/${number}`;
const CAMPAIGNS = [
  campaign("open-issues-dag", "열린 이슈 DAG", [
    node("i7272", "#7272 무중단 재시작 체크포인트를 단계마다 남기고 실패한 단계부터 다시 이어가기", {
      status: "running", stage: "Absorbed into #7272 acceptance: restart protocol keeps the durable checkpoint across every handoff", issue_url: issue(7272), assignee: "agent-ada",
    }),
    node("i7336", "#7336 꺼진 맥의 채널 메시지를 다른 맥이 이어받기", { stage: "review", issue_url: issue(7336), group: "Cluster" }),
    node("i7401", "#7401 게이트웨이 재연결", { status: "completed", stage: "done", issue_url: issue(7401) }),
    node("cli-update", "Update CLIs", { stage: "verify", group: null }),
  ]),
  campaign("release-train", "릴리스 트레인", [node("i7500", "#7500 릴리스 노트", { issue_url: issue(7500) })]),
];

const card = (id: string, number: number | null, title: string, patch: Record<string, unknown> = {}) => ({
  id, title, description: null, status: "backlog", github_repo: "itismyfield/AgentDesk", owner_agent_id: null, requester_agent_id: null,
  assignee_agent_id: null, parent_card_id: null, latest_dispatch_id: null, sort_order: 0, priority: "medium", depth: 0, blocked_reason: null,
  review_notes: null, github_issue_number: number, github_issue_url: number ? issue(number) : null, metadata_json: null, pipeline_stage_id: null,
  review_status: null, created_at: 1_790_000_000_000, updated_at: 1_793_000_000_000, started_at: null, requested_at: null, completed_at: null, ...patch,
});
const CARDS = [
  card("card-7272", 7272, "무중단 재시작 체크포인트", { status: "in_progress" }),
  card("card-7336", 7336, "꺼진 맥의 채널 메시지"),
  card("card-7610", 7610, "대시보드 대기열\\n카드 제목이 두 줄로 들어온 경우를 한 줄로 보여 주기", { priority: "high", assignee_agent_id: "agent-luna" }),
  card("card-7611", 7611, "캠페인 화면 모바일 헤더를 한 줄로 줄이고 위험한 동작은 메뉴 뒤로 옮기기"),
  card("card-7612", 7612, "준비된 카드", { status: "ready", assignee_agent_id: "agent-ada" }),
  card("card-7401", 7401, "이미 끝난 일", { status: "done" }),
  card("card-local", null, "GitHub 이슈가 없는 메모 카드", { priority: "low" }),
];

const json = (page: Page, pattern: RegExp, body: unknown) => page.route(pattern, (route) => route.fulfill({ json: body }));

/** Minimal API: everything else answers 404, never the real operator API. */
async function mockApi(page: Page) {
  const calls: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/api/")) calls.push(url.pathname + url.search);
  });
  await page.route(/\/api\//, (route) => route.fulfill({ status: 404, json: { error: "not mocked" } }));
  await page.addInitScript(() => {
    class MockWebSocket {
      static readonly CONNECTING = 0; static readonly OPEN = 1; static readonly CLOSING = 2; static readonly CLOSED = 3;
      readyState = 0; onopen: ((event: Event) => void) | null = null; onmessage: ((event: MessageEvent) => void) | null = null;
      onerror: ((event: Event) => void) | null = null; onclose: ((event: Event) => void) | null = null;
      constructor(public url: string) { setTimeout(() => { if (this.readyState === 3) return; this.readyState = 1; this.onopen?.(new Event("open")); }, 0); }
      send() {}
      close() { if (this.readyState === 3) return; this.readyState = 3; this.onclose?.(new Event("close")); }
    }
    Object.defineProperty(window, "WebSocket", { configurable: true, writable: true, value: MockWebSocket });
  });
  await json(page, /\/api\/auth\/session$/, { ok: true, authenticated: true, auth_enabled: false, csrf_token: "" });
  await json(page, /\/api\/auth\/ws-ticket$/, { ticket: "s".repeat(64), expires_in: 15 });
  await json(page, /\/api\/agents(?:\?.*)?$/, { agents: AGENTS });
  await json(page, /\/api\/departments(?:\?.*)?$/, { departments: [] });
  await json(page, /\/api\/dispatched-sessions(?:\?.*)?$/, { sessions: [] });
  await json(page, /\/api\/settings$/, { language: "ko", theme: "dark" });
  await json(page, /\/api\/audit-logs(?:\?.*)?$/, { logs: [] });
  await json(page, /\/api\/dispatches(?:\?.*)?$/, { dispatches: [] });
  await json(page, /\/api\/kanban-cards(?:\?.*)?$/, { cards: CARDS });
  await page.route(/\/api\/kanban-cards\/[^/?]+$/, (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").pop() ?? "");
    const found = CARDS.find((value) => value.id === id);
    return found ? route.fulfill({ json: { card: found } }) : route.fulfill({ status: 404, json: { error: "card not found" } });
  });
  await json(page, /\/api\/kanban-cards\/[^/]+\/(?:audit-log)$/, { logs: [] });
  await json(page, /\/api\/kanban-cards\/[^/]+\/comments$/, { comments: [], body: "" });
  await json(page, /\/api\/kanban-cards\/[^/]+\/reviews$/, { reviews: [] });
  await json(page, /\/api\/pipeline\/cards\/[^/]+\/transcripts(?:\?.*)?$/, { transcripts: [] });
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: CAMPAIGNS, live: {}, limit: 100, offset: 0 });
  await page.route(/\/api\/campaigns\/[^/?]+$/, (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").pop() ?? "");
    const found = CAMPAIGNS.find((value) => value.id === id);
    return found ? route.fulfill({ json: { campaign: found, live: {} } }) : route.fulfill({ status: 404, json: { error: "not found" } });
  });
  await json(page, /\/api\/server-campaigns\?.*$/, { campaigns: [] });
  return calls;
}

async function prepare(page: Page, projectName: string) {
  if (projectName === "desktop") await page.setViewportSize({ width: 1440, height: 900 });
  return mockApi(page);
}

async function expectNoHorizontalOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
}

async function shot(page: Page, name: string, projectName: string) {
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/${projectName}-${name}.png`, fullPage: true });
}

test("campaign tasks show one number, a one-line memo, a styled picker, and only the shown campaign is polled", async ({ page }, testInfo) => {
  await page.clock.install({ time: new Date(NOW) });
  const calls = await prepare(page, testInfo.project.name);
  await page.goto("/campaigns?mode=legacy&campaign=open-issues-dag");

  const panel = page.locator(".campaigns-panel");
  await expect(panel.getByRole("heading", { name: "캠페인", exact: true })).toBeVisible({ timeout: 15_000 });
  const picker = panel.getByLabel("캠페인 선택");
  await expect(picker).toHaveValue("open-issues-dag");
  expect(await picker.evaluate((element) => getComputedStyle(element).appearance)).toBe("none");
  await expect(panel.getByRole("button", { name: "새 캠페인" })).toBeVisible();
  await expect(panel.getByRole("switch", { name: "자동 진행" })).toHaveAttribute("aria-checked", "false");

  const row = panel.locator(".campaign-task-row", { hasText: "무중단 재시작" });
  await expect(row.locator(".campaign-task-id")).toHaveText("#7272");
  await expect(row.locator("strong")).toHaveText(/^무중단 재시작/);
  await expect(row.locator(".campaign-task-stage")).toHaveText(/^Absorbed into #7272 acceptance/);
  const stageBox = await row.locator(".campaign-task-stage").boundingBox();
  expect(stageBox?.height ?? 0).toBeLessThan(24);
  await expectNoHorizontalOverflow(page);
  await shot(page, "campaigns", testInfo.project.name);
  const group = panel.locator(".campaign-task-group").first();
  await group.scrollIntoViewIfNeeded();
  if (SHOTS) await group.screenshot({ path: `${SHOTS}/${testInfo.project.name}-campaign-tasks.png` });

  const initialDetail = calls.filter((url) => url.startsWith("/api/campaigns/")).length;
  await page.clock.runFor(31_000);
  await expect.poll(() => calls.filter((url) => url === "/api/campaigns/open-issues-dag").length).toBeGreaterThanOrEqual(initialDetail + 2);
  expect(calls.filter((url) => url.startsWith("/api/campaigns?"))).toHaveLength(1);
  expect(calls.filter((url) => url.startsWith("/api/server-campaigns"))).toEqual(["/api/server-campaigns?view=summary&limit=100&offset=0"]);
  expect(calls.filter((url) => url.startsWith("/api/campaigns/") && url !== "/api/campaigns/open-issues-dag")).toEqual([]);
});

test("the queue lists open cards outside campaigns and opens a card as a drawer or bottom sheet", async ({ page }, testInfo) => {
  const calls = await prepare(page, testInfo.project.name);
  await page.goto("/campaigns?tab=queue");

  const queue = page.getByRole("list", { name: "대기열 카드" });
  await expect(queue).toBeVisible({ timeout: 15_000 });
  await expect(page.getByRole("tab", { name: "대기열" })).toHaveAttribute("aria-selected", "true");
  await expect(queue.locator("[data-card-id]")).toHaveCount(4);
  for (const held of ["card-7272", "card-7336", "card-7401"]) await expect(queue.locator(`[data-card-id="${held}"]`)).toHaveCount(0);
  await expect(queue.locator('[data-card-id="card-7610"] strong')).toHaveText("대시보드 대기열 카드 제목이 두 줄로 들어온 경우를 한 줄로 보여 주기");
  await expect(queue.locator('[data-card-id="card-7612"]').getByRole("button", { name: "준비됨으로" })).toHaveCount(0);
  await expect(page.getByText("캠페인 밖 카드 4장")).toBeVisible();
  await expectNoHorizontalOverflow(page);
  await shot(page, "queue", testInfo.project.name);

  await queue.locator('[data-card-id="card-7611"]').getByRole("button").first().click();
  await expect(page).toHaveURL(/[?&]card=card-7611/);
  const detail = page.getByTestId("kanban-card-drawer");
  await expect(detail).toBeVisible();
  await expect(detail.getByText("캠페인에 넣기")).toBeVisible();
  await expect(detail.getByRole("button", { name: "카드 메뉴" })).toBeVisible();
  expect(calls).toContain("/api/kanban-cards/card-7611");

  const sheet = page.getByRole("dialog");
  const box = await sheet.boundingBox();
  const viewport = page.viewportSize()!;
  if (testInfo.project.name === "mobile") {
    expect((box?.y ?? 0) + (box?.height ?? 0)).toBeGreaterThan(viewport.height - 12);
    expect(box?.width ?? 0).toBeGreaterThan(viewport.width - 4);
  } else {
    expect((box?.x ?? 0) + (box?.width ?? 0)).toBeGreaterThan(viewport.width - 4);
    expect(box?.width ?? viewport.width).toBeLessThan(viewport.width * 0.7);
  }
  await expectNoHorizontalOverflow(page);
  await shot(page, "queue-card", testInfo.project.name);
});

test("a card without an assignee asks for one before it moves to ready", async ({ page }, testInfo) => {
  const updates: unknown[] = [];
  await prepare(page, testInfo.project.name);
  await page.route(/\/api\/kanban-cards\/card-7611$/, async (route) => {
    if (route.request().method() !== "PATCH") return route.fallback();
    const patch = route.request().postDataJSON();
    updates.push(patch);
    await route.fulfill({ json: { card: { ...CARDS[3], ...patch } } });
  });
  await page.goto("/campaigns?tab=queue");
  const row = page.locator('[data-card-id="card-7611"]');
  await row.getByRole("button", { name: "준비됨으로" }).click({ timeout: 15_000 });
  const dialog = page.getByRole("dialog");
  await dialog.getByRole("combobox").selectOption("agent-ada");
  await shot(page, "queue-ready", testInfo.project.name);
  await dialog.getByRole("button", { name: "준비됨으로 넘기기" }).click();
  await expect.poll(() => updates).toEqual([{ assignee_agent_id: "agent-ada" }, { status: "ready" }]);
});
