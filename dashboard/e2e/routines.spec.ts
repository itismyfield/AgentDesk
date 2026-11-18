import { expect, test, type Page } from "@playwright/test";

// /routines at 1440x900 (desktop project) and 390x844 (mobile project): schedule rows, timeline markers, detail.
// Set ROUTINES_SHOTS to a directory to also save 390/820/1440 x light/dark screenshots.

const SHOTS = process.env.ROUTINES_SHOTS;
const NOW = Date.parse("2026-11-18T03:00:00.000Z"); // 12:00 in Asia/Seoul
const HOUR = 3_600_000;
const iso = (offsetMs: number) => new Date(NOW + offsetMs).toISOString();

test.use({ timezoneId: "Asia/Seoul", locale: "ko-KR" });

const run = (routineId: string, status: string, startedOffset: number, patch: Record<string, unknown> = {}) => ({
  id: `${routineId}-run-${status}`,
  routine_id: routineId,
  status,
  action: null,
  turn_id: null,
  lease_expires_at: null,
  result_json: null,
  error: null,
  discord_log_status: null,
  discord_log_error: null,
  discord_message_id: null,
  discord_log_sections: null,
  started_at: iso(startedOffset),
  finished_at: status === "running" ? null : iso(startedOffset + 90_000),
  created_at: iso(startedOffset),
  updated_at: iso(startedOffset),
  ...patch,
});

const routine = (id: string, name: string, patch: Record<string, unknown>) => ({
  id,
  agent_id: "family-counsel",
  fallback_agent_id: null,
  max_retries: 1,
  script_ref: `${name}.js`,
  name,
  status: "enabled",
  execution_strategy: "fresh",
  schedule: null,
  next_due_at: null,
  last_run_at: null,
  last_result: null,
  checkpoint: null,
  discord_thread_id: null,
  timeout_secs: 600,
  in_flight_run_id: null,
  pause_reason: null,
  last_run: null,
  created_at: iso(-30 * 24 * HOUR),
  updated_at: iso(-HOUR),
  ...patch,
});

const ROUTINES = [
  routine("rt-queue", "queue-stability-check", {
    agent_id: "adk-ops",
    schedule: "0 */2 * * *",
    next_due_at: iso(2 * HOUR),
    last_run_at: iso(-2 * HOUR),
    last_run: run("rt-queue", "succeeded", -2 * HOUR),
    last_result: "**Queue stable**: no stuck turns.",
    discord_thread_id: "1505086338448363660",
  }),
  routine("rt-memory", "memory-merge", {
    agent_id: "adk-memory",
    schedule: "0 14 * * *",
    next_due_at: iso(2 * HOUR),
    last_run_at: iso(-22 * HOUR),
    last_run: run("rt-memory", "succeeded", -22 * HOUR),
  }),
  routine("rt-relay", "relay-gap-audit", {
    agent_id: "adk-relay",
    schedule: "40 8,20 * * *",
    next_due_at: iso(8 * HOUR + 40 * 60_000),
    last_run_at: iso(-(3 * HOUR + 20 * 60_000)),
    last_run: run("rt-relay", "failed", -(3 * HOUR + 20 * 60_000), { error: "Discord API 429 — audit stopped" }),
    last_result: "Discord API 429 — audit stopped.\n\nRetry after the rate limit window.",
    discord_thread_id: "1505086366654926908",
  }),
  routine("rt-token", "token-daily-report", {
    schedule: "50 11 * * *",
    next_due_at: iso(23.8 * HOUR),
    last_run_at: iso(-0.2 * HOUR),
    in_flight_run_id: "rt-token-run-running",
    last_run: run("rt-token", "running", -0.2 * HOUR),
  }),
  routine("rt-deps", "dependency-update-watch", {
    agent_id: "codex",
    schedule: "0 9 * * 1",
    next_due_at: iso(21 * HOUR),
    last_run_at: iso(-27 * HOUR),
    last_run: run("rt-deps", "succeeded", -27 * HOUR),
  }),
  routine("rt-briefing", "family-morning-briefing-obujang", {
    status: "paused",
    schedule: "30 6 * * *",
    pause_reason: "calendar token expired",
    last_run_at: iso(-3 * 24 * HOUR),
    last_run: run("rt-briefing", "interrupted", -3 * 24 * HOUR),
  }),
  routine("rt-janitor", "local-worktree-gc", {
    status: "detached",
    agent_id: null,
    schedule: null,
  }),
];

const RUNS = [
  run("rt-queue", "succeeded", -2 * HOUR, { result_json: { outcome_summary: "큐 정상 · 멈춘 턴 없음", duration_ms: 4200 } }),
  run("rt-queue", "failed", -4 * HOUR, { id: "rt-queue-run-older", error: "timeout after 600s" }),
];

const json = (page: Page, pattern: RegExp, body: unknown) => page.route(pattern, (route) => route.fulfill({ json: body }));

/** Minimal API: everything else answers 404, never the real operator API. */
async function mockApi(page: Page, theme: "dark" | "light" = "dark") {
  const calls: string[] = [];
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (url.pathname.startsWith("/api/")) calls.push(`${request.method()} ${url.pathname}${url.search}`);
  });
  await page.route(/\/api\//, (route) => route.fulfill({ status: 404, json: { error: "not mocked" } }));
  await page.addInitScript((value) => {
    window.localStorage.setItem("agentdesk.theme", value);
    class MockWebSocket {
      static readonly CONNECTING = 0; static readonly OPEN = 1; static readonly CLOSING = 2; static readonly CLOSED = 3;
      readyState = 0; onopen: ((event: Event) => void) | null = null; onmessage: ((event: MessageEvent) => void) | null = null;
      onerror: ((event: Event) => void) | null = null; onclose: ((event: Event) => void) | null = null;
      constructor(public url: string) { setTimeout(() => { if (this.readyState === 3) return; this.readyState = 1; this.onopen?.(new Event("open")); }, 0); }
      send() {}
      close() { if (this.readyState === 3) return; this.readyState = 3; this.onclose?.(new Event("close")); }
    }
    Object.defineProperty(window, "WebSocket", { configurable: true, writable: true, value: MockWebSocket });
  }, theme);
  await json(page, /\/api\/auth\/session$/, { ok: true, authenticated: true, auth_enabled: false, csrf_token: "" });
  await json(page, /\/api\/auth\/ws-ticket$/, { ticket: "s".repeat(64), expires_in: 15 });
  await json(page, /\/api\/agents(?:\?.*)?$/, { agents: [] });
  await json(page, /\/api\/departments(?:\?.*)?$/, { departments: [] });
  await json(page, /\/api\/dispatched-sessions(?:\?.*)?$/, { sessions: [] });
  await json(page, /\/api\/settings$/, { language: "ko", theme });
  await json(page, /\/api\/audit-logs(?:\?.*)?$/, { logs: [] });
  await json(page, /\/api\/dispatches(?:\?.*)?$/, { dispatches: [] });
  await json(page, /\/api\/kanban-cards(?:\?.*)?$/, { cards: [] });
  await page.route(/\/api\/routines(?:\?.*)?$/, (route) => {
    const status = new URL(route.request().url()).searchParams.get("status");
    return route.fulfill({ json: { routines: status ? ROUTINES.filter((item) => item.status === status) : ROUTINES } });
  });
  await page.route(/\/api\/routines\/[^/]+\/runs(?:\?.*)?$/, (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/")[3] ?? "");
    return route.fulfill({ json: { runs: RUNS.filter((item) => item.routine_id === id) } });
  });
  await page.route(/\/api\/discord\/channels\/\d+$/, (route) => {
    const id = new URL(route.request().url()).pathname.split("/").pop();
    return route.fulfill({ json: { id, guild_id: "1469870512812462284", name: "routine-thread" } });
  });
  return calls;
}

async function openRoutines(page: Page, theme: "dark" | "light" = "dark") {
  await page.clock.install({ time: NOW });
  const calls = await mockApi(page, theme);
  await page.goto("/routines");
  await expect(page.getByTestId("routine-row-rt-queue")).toBeVisible({ timeout: 15_000 });
  return calls;
}

async function expectNoHorizontalOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
}

const rowIds = (page: Page) =>
  page.getByTestId("routines-timeline-list").locator("[data-row]").evaluateAll((rows) => rows.map((row) => row.getAttribute("data-row")));

test("routines: rows follow next run with repeat, next time, latest result and a read-only state switch", async ({ page }, testInfo) => {
  const calls = await openRoutines(page);
  expect(await rowIds(page)).toEqual(["rt-queue", "rt-memory", "rt-relay", "rt-deps", "rt-token", "rt-janitor", "rt-briefing"]);

  const queue = page.getByTestId("routine-row-rt-queue");
  await expect(queue).toContainText("큐 안정성 신호");
  await expect(queue).toContainText("queue-stability-check");
  await expect(queue).toContainText("2시간마다 정각");
  await expect(queue).toContainText("오늘 14:00");
  await expect(queue).toHaveAttribute("aria-label", /queue-stability-check.*다음 오늘 14:00.*최근 성공/);
  await expect(page.getByTestId("routine-row-rt-briefing")).toContainText("일시정지");
  await expect(page.getByTestId("routine-row-rt-janitor")).toContainText("분리됨");
  await expect(page.getByTestId("routine-row-rt-janitor")).toContainText("수동 실행");

  await expect(page.getByTestId("routine-switch-rt-queue")).toBeChecked();
  await expect(page.getByTestId("routine-switch-rt-queue")).toBeDisabled();
  await expect(page.getByTestId("routine-switch-rt-briefing")).not.toBeChecked();
  await expect(page.getByRole("switch", { name: "family-morning-briefing-obujang 자동 실행 꺼짐" })).toBeVisible();
  await expect(page.getByTestId("routines-active-count")).toHaveText("활성 5/7");
  await expect(page.getByTestId("routine-thread-rt-relay")).toHaveAttribute("href", "https://discord.com/channels/1469870512812462284/1505086366654926908");

  // Latest result and relative time: shown in a wide list, left to the detail at 640px and below; stacked at 440px.
  const result = queue.getByText("최근 성공", { exact: true });
  const stackedNext = queue.locator("span.hidden").filter({ hasText: "오늘 14:00" });
  if (testInfo.project.name === "desktop") {
    await expect(result).toBeVisible();
    await expect(queue.getByText("2시간 후")).toBeVisible();
    await expect(stackedNext).toBeHidden();
  } else {
    await expect(result).toBeHidden();
    await expect(stackedNext).toBeVisible();
  }

  // Filters go to the server as before.
  await page.getByRole("radio", { name: "일시정지" }).click();
  await expect.poll(() => rowIds(page)).toEqual(["rt-briefing"]);
  expect(calls).toContain("GET /api/routines?status=paused");
  // No mutation calls: the dashboard only reads routines.
  expect(calls.filter((call) => call.includes("/api/routines") && !call.startsWith("GET "))).toEqual([]);
  await expectNoHorizontalOverflow(page);
});

test("routines: timeline marks last and next runs, and close markers open one list", async ({ page }, testInfo) => {
  await openRoutines(page);
  const timeline = page.getByTestId("routines-run-timeline");
  await expect(timeline.getByTestId("routines-timeline-readout")).toContainText("토큰 사용량과 비용");
  await expect(timeline.getByTestId("routines-timeline-readout")).toContainText("실행 중");

  // Every target is 44x44 and none overlap.
  const targets = timeline.locator("[data-testid^='routine-marker']");
  const boxes = await targets.evaluateAll((nodes) => nodes.map((node) => node.getBoundingClientRect().toJSON() as DOMRect));
  expect(boxes.length).toBeGreaterThanOrEqual(3);
  for (const box of boxes) {
    expect(box.width).toBeGreaterThanOrEqual(44);
    expect(box.height).toBeGreaterThanOrEqual(44);
  }
  const sorted = [...boxes].sort((a, b) => a.x - b.x);
  for (let index = 1; index < sorted.length; index += 1) expect(sorted[index].x).toBeGreaterThanOrEqual(sorted[index - 1].x + 44 - 0.5);

  await expect(timeline.getByRole("button", { name: /^relay gap audit.*, 오늘 08:40, 실패$/ })).toBeVisible();
  await expect(timeline.getByRole("button", { name: /^큐 안정성 신호.*, 오늘 10:00, 지난 실행 · 2시간 전$/ })).toBeVisible();
  await expect(timeline.getByRole("button", { name: /^토큰 사용량과 비용.*, 오늘 11:48, 실행 중$/ })).toBeVisible();

  // Two routines due at the same 14:00 share one target that opens a list.
  const cluster = timeline.getByRole("button", { name: /가까운 실행 \d+개 목록 열기/ }).first();
  await expect(cluster).toBeVisible();
  await cluster.click();
  const list = page.getByTestId("routine-overlap-list").last();
  await expect(list).toBeVisible();
  await expect(list.getByRole("button")).toHaveCount(2);
  const memory = list.getByRole("button", { name: /에이전트 메모리 파일/ });
  await expect(memory).toBeVisible();
  await memory.click();

  if (testInfo.project.name === "desktop") {
    const pane = page.getByTestId("routine-detail-pane");
    await expect(pane.getByRole("heading", { level: 2 })).toContainText("에이전트 메모리 파일");
    await expect(page.getByTestId("routine-row-rt-memory")).toHaveAttribute("aria-current", "true");
    await pane.getByRole("button", { name: "상세 닫기" }).click();
  } else {
    const sheet = page.getByRole("dialog");
    await expect(sheet).toContainText("에이전트 메모리 파일");
    await expect(sheet.getByTestId("routine-detail")).toBeVisible();
    await sheet.getByRole("button", { name: "Close" }).click();
    await expect(sheet).toHaveCount(0);
  }
  await expect(cluster).toBeFocused();
  await expectNoHorizontalOverflow(page);
});

test("routines: detail splits beside the list on wide screens, opens as a sheet below 900px, and returns focus", async ({ page }, testInfo) => {
  await openRoutines(page);
  const row = page.getByTestId("routine-row-rt-relay");
  await row.click();

  const detail = testInfo.project.name === "desktop" ? page.getByTestId("routine-detail-pane") : page.getByRole("dialog");
  if (testInfo.project.name === "desktop") {
    await expect(page.getByRole("dialog")).toHaveCount(0);
    const [listBox, paneBox] = await Promise.all([page.getByTestId("routines-timeline-list").boundingBox(), detail.boundingBox()]);
    expect(paneBox!.x).toBeGreaterThan(listBox!.x + listBox!.width);
    expect(paneBox!.y).toBeLessThan(listBox!.y);
  } else {
    const box = await detail.boundingBox();
    expect(box!.width).toBeGreaterThan(page.viewportSize()!.width - 4);
  }
  await expect(detail).toContainText("relay-gap-audit");
  const threadLink = detail.getByTestId("routine-thread-link");
  await expect(threadLink).toHaveAttribute("href", "https://discord.com/channels/1469870512812462284/1505086366654926908");
  await expect(detail.getByTestId("routine-last-result")).toContainText("Discord API 429");
  await expect(detail.getByRole("button", { name: "펼치기" })).toBeVisible();
  await expect(detail.getByText("오늘 20:40", { exact: true })).toBeVisible();
  for (const label of ["대체 담당", "제한 시간", "고급 정보"]) await expect(detail.getByText(label, { exact: true })).toBeVisible();

  if (testInfo.project.name === "desktop") {
    // The split pane stays; its close button hands focus back to the row.
    await page.getByTestId("routine-row-rt-queue").click();
    await expect(page.getByTestId("routine-run-history")).toContainText("큐 정상");
    await expect(page.getByTestId("routine-run-history")).toContainText("timeout after 600s");
    await detail.getByRole("button", { name: "상세 닫기" }).click();
    await expect(detail).toContainText("선택한 루틴이 없어요");
    await expect(page.getByTestId("routine-row-rt-queue")).toBeFocused();
  } else {
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(row).toBeFocused();
  }

  // Arrow keys move between rows.
  await page.getByTestId("routine-row-rt-queue").focus();
  await page.keyboard.press("ArrowDown");
  await expect(page.getByTestId("routine-row-rt-memory")).toBeFocused();
  await page.keyboard.press("k");
  await expect(page.getByTestId("routine-row-rt-queue")).toBeFocused();
  // From the page background, J enters the list.
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.keyboard.press("j");
  await expect(page.getByTestId("routine-row-rt-queue")).toBeFocused();
});

test.describe("touch at 390px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("routines: no sideways scroll, 44px targets and 16px inputs", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone width check");
    await openRoutines(page);
    expect(await page.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);
    await expectNoHorizontalOverflow(page);
    const small = await page.getByTestId("routines-page").evaluate((root) => {
      const selector = "button, a[href], input, select, textarea, [role='switch'], [role='radio'], [tabindex='0']";
      return Array.from(root.querySelectorAll<HTMLElement>(selector))
        .filter((node) => !(node as HTMLButtonElement).disabled && node.getClientRects().length && getComputedStyle(node).visibility !== "hidden")
        .map((node) => ({ node, box: node.getBoundingClientRect() }))
        .filter(({ box }) => box.width > 0 && Math.min(box.width, box.height) < 44)
        .map(({ node, box }) => `${node.tagName}.${node.className.toString().slice(0, 40)} ${Math.round(box.width)}x${Math.round(box.height)} ${node.textContent?.trim().slice(0, 20)}`);
    });
    expect(small).toEqual([]);
    const tinyInputs = await page.getByTestId("routines-page").evaluate((root) =>
      Array.from(root.querySelectorAll<HTMLElement>("input:not([type='checkbox']):not([type='radio']), select, textarea"))
        .filter((node) => parseFloat(getComputedStyle(node).fontSize) < 16)
        .map((node) => node.outerHTML.slice(0, 60)),
    );
    expect(tinyInputs).toEqual([]);
    // The read-only switch keeps the kit's 44px hit area on touch screens.
    const hit = await page.getByTestId("routine-switch-rt-queue").boundingBox();
    expect(Math.min(hit!.width, hit!.height)).toBeGreaterThanOrEqual(44);

    // Detail sheet controls follow the same rule.
    await page.getByTestId("routine-row-rt-relay").click();
    const sheet = page.getByRole("dialog");
    await expect(sheet.getByTestId("routine-detail")).toBeVisible();
    const sheetSmall = await sheet.evaluate((root) =>
      Array.from(root.querySelectorAll<HTMLElement>("button, a[href], summary"))
        .filter((node) => node.getClientRects().length)
        .map((node) => ({ node, box: node.getBoundingClientRect() }))
        .filter(({ box }) => Math.min(box.width, box.height) < 44)
        .map(({ node, box }) => `${node.tagName} ${Math.round(box.width)}x${Math.round(box.height)} ${node.textContent?.trim().slice(0, 20)}`),
    );
    expect(sheetSmall).toEqual([]);
    await expectNoHorizontalOverflow(page);
  });
});

test("routines: screenshots at 390, 820 and 1440 in light and dark", async ({ page }, testInfo) => {
  test.skip(!SHOTS || testInfo.project.name !== "desktop", "Set ROUTINES_SHOTS to collect screenshots");
  test.setTimeout(120_000);
  await page.clock.install({ time: NOW });
  for (const theme of ["light", "dark"] as const) {
    await page.unrouteAll({ behavior: "ignoreErrors" });
    await mockApi(page, theme);
    for (const [width, height] of [[390, 844], [820, 1180], [1440, 900]] as const) {
      await page.setViewportSize({ width, height });
      await page.goto("/routines");
      await expect(page.getByTestId("routine-row-rt-queue")).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      await page.mouse.move(0, 0);
      await page.screenshot({ path: `${SHOTS}/routines-${width}x${height}-${theme}.png` });
      await page.getByTestId("routine-row-rt-relay").click();
      await expect(page.getByTestId("routine-detail")).toBeVisible();
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${SHOTS}/routines-${width}x${height}-${theme}-detail.png` });
      if (width !== 820) {
        // Overlap list: popover on wide screens, inside the sheet below 900px.
        await page.goto("/routines");
        await page.getByRole("button", { name: /가까운 실행 \d+개 목록 열기/ }).click();
        await expect(page.getByTestId("routine-overlap-list").last()).toBeVisible();
        await page.waitForTimeout(400);
        await page.screenshot({ path: `${SHOTS}/routines-${width}x${height}-${theme}-overlap.png` });
      }
    }
    // Home keeps its card; only the rows follow the routines page.
    for (const [width, height] of [[390, 844], [1440, 900]] as const) {
      await page.setViewportSize({ width, height });
      await page.goto("/home");
      const card = page.getByTestId("routines-timeline");
      await expect(card.getByTestId("routine-row-rt-relay")).toBeVisible({ timeout: 15_000 });
      await card.scrollIntoViewIfNeeded();
      await card.screenshot({ path: `${SHOTS}/home-routines-${width}x${height}-${theme}.png` });
    }
  }
});
