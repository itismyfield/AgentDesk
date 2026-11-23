import { expect, test, type Page } from "@playwright/test";

const SHOTS = process.env.STATS_SHOTS;
const START = Date.UTC(2026, 9, 20);

type Period = "7d" | "30d" | "90d";

const dayCount = (period: Period) => (period === "7d" ? 7 : period === "90d" ? 90 : 30);

function buildDaily(period: Period, zero = false) {
  return Array.from({ length: dayCount(period) }, (_, index) => {
    const wave = zero ? 0 : 1 + Math.sin(index / 2.3) * 0.45 + (index % 7 === 5 ? 0.6 : 0);
    const input = Math.round(42_000 * wave);
    const output = Math.round(18_000 * wave);
    const cacheRead = Math.round(96_000 * wave);
    const cacheWrite = Math.round(12_000 * wave);
    return {
      date: new Date(START + index * 86_400_000).toISOString().slice(0, 10),
      input_tokens: input,
      output_tokens: output,
      cache_read_tokens: cacheRead,
      cache_creation_tokens: cacheWrite,
      total_tokens: input + output + cacheRead + cacheWrite,
      cost: zero ? 0 : Number((1.2 * wave).toFixed(3)),
    };
  });
}

const MODELS = [
  { model: "claude-opus-4-6", display_name: "Claude Opus 4.6", provider: "claude", input_tokens: 610_000, output_tokens: 260_000, cache_read_tokens: 1_420_000, cache_creation_tokens: 180_000, cost: 31.4, cost_without_cache: 44.9 },
  { model: "gpt-5.4", display_name: "GPT-5.4", provider: "codex", input_tokens: 380_000, output_tokens: 150_000, cache_read_tokens: 610_000, cache_creation_tokens: 0, cost: 12.2, cost_without_cache: 15.8 },
  { model: "claude-haiku-4-5", display_name: "Claude Haiku 4.5", provider: "claude", input_tokens: 120_000, output_tokens: 60_000, cache_read_tokens: 90_000, cache_creation_tokens: 20_000, cost: 1.1, cost_without_cache: 1.3 },
].map((model) => ({ ...model, total_tokens: model.input_tokens + model.output_tokens + model.cache_read_tokens + model.cache_creation_tokens }));

const AGENTS = [
  { agent: "adk-dashboard", tokens: 1_740_000, cost: 22.6, cost_without_cache: 31.2, input_tokens: 420_000, cache_read_tokens: 980_000, cache_creation_tokens: 110_000, percentage: 44 },
  { agent: "project-agentdesk", tokens: 1_210_000, cost: 15.1, cost_without_cache: 20.4, input_tokens: 360_000, cache_read_tokens: 610_000, cache_creation_tokens: 60_000, percentage: 31 },
  { agent: "td-릴레이-품질-감시-야간-대조-에이전트", tokens: 640_000, cost: 5.4, input_tokens: 220_000, cache_read_tokens: 280_000, cache_creation_tokens: 20_000, percentage: 16 },
  { agent: "qa-bot", tokens: 360_000, cost: 1.6, percentage: 9 },
];

function buildAnalytics(period: Period, variant: "default" | "zero" | "empty" = "default") {
  const daily = variant === "empty" ? [] : buildDaily(period, variant === "zero");
  const totalTokens = daily.reduce((sum, day) => sum + day.total_tokens, 0);
  const totalCost = daily.reduce((sum, day) => sum + day.cost, 0);
  const peak = daily.reduce<(typeof daily)[number] | null>((best, day) => (!best || day.total_tokens > best.total_tokens ? day : best), null);
  const withData = variant === "default";
  return {
    period,
    period_label: `Last ${dayCount(period)} days`,
    days: dayCount(period),
    generated_at: new Date(START + 40 * 86_400_000).toISOString(),
    summary: {
      total_tokens: totalTokens,
      total_cost: Number(totalCost.toFixed(3)),
      cache_discount: withData ? Number((totalCost * 0.28).toFixed(3)) : 0,
      total_messages: withData ? 812 : 0,
      total_sessions: withData ? 64 : 0,
      active_days: withData ? daily.length : 0,
      average_daily_tokens: daily.length ? Math.round(totalTokens / daily.length) : 0,
      peak_day: withData && peak ? { date: peak.date, total_tokens: peak.total_tokens, cost: peak.cost } : null,
    },
    receipt: {
      period_label: `Last ${dayCount(period)} days`,
      period_start: daily[0]?.date ?? "2026-10-20",
      period_end: daily.at(-1)?.date ?? "2026-10-26",
      models: withData ? MODELS : [],
      subtotal: Number((totalCost * 1.28).toFixed(3)),
      cache_discount: Number((totalCost * 0.28).toFixed(3)),
      total: Number(totalCost.toFixed(3)),
      stats: { total_messages: 812, total_sessions: 64 },
      providers: withData ? [{ provider: "claude", tokens: 2_760_000, percentage: 70 }, { provider: "codex", tokens: 1_140_000, percentage: 30 }] : [],
      agents: withData ? AGENTS : [],
    },
    daily,
    heatmap: [],
  };
}

const SKILL_RANKING = {
  window: "7d",
  overall: [
    { skill_name: "agentdesk-issue-pipeline", skill_desc_ko: "열린 이슈를 조사·구현·리뷰까지 자율 처리", calls: 42, last_used_at: START },
    { skill_name: "relay-gap-diagnose", skill_desc_ko: "디스코드 릴레이 누락 원인을 표준 순서로 진단", calls: 17, last_used_at: START },
    { skill_name: "agentdesk-restart", skill_desc_ko: "launchd로 AgentDesk 재시작", calls: 6, last_used_at: START },
  ],
  byAgent: [
    { agent_role_id: "adk-dashboard", agent_name: "adk-dashboard", skill_name: "agentdesk-issue-pipeline", skill_desc_ko: "열린 이슈 자율 처리", calls: 28, last_used_at: START },
    { agent_role_id: "project-agentdesk", agent_name: "project-agentdesk", skill_name: "relay-gap-diagnose", skill_desc_ko: "릴레이 누락 진단", calls: 12, last_used_at: START },
    { agent_role_id: "adk-dashboard", agent_name: "adk-dashboard", skill_name: "agentdesk-restart", skill_desc_ko: "AgentDesk 재시작", calls: 4, last_used_at: START },
  ],
};

const json = (page: Page, pattern: RegExp, body: unknown) => page.route(pattern, (route) => route.fulfill({ json: body }));

/** Minimal API for /stats: everything else answers 404, never the real operator API. */
async function mockApi(page: Page, options: { theme?: "light" | "dark"; language?: "ko" | "en"; analytics?: (period: Period) => unknown | null } = {}) {
  const theme = options.theme ?? "dark";
  await page.route(/\/api\//, (route) => route.fulfill({ status: 404, json: { error: "not mocked" } }));
  await page.addInitScript((value) => {
    localStorage.setItem("agentdesk.theme", value);
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
  await json(page, /\/api\/settings$/, { language: options.language ?? "ko", theme });
  await json(page, /\/api\/settings\/runtime-config$/, { current: { dispatchRateLimitGateEnabled: true, dispatchRateLimitGateDangerPct: 90 }, defaults: {} });
  await json(page, /\/api\/skills\/catalog$/, { catalog: [] });
  await json(page, /\/api\/skills\/ranking(?:\?.*)?$/, SKILL_RANKING);
  await json(page, /\/api\/claude-accounts$/, {
    schemaVersion: 1, status: "ok", hostname: "mac-mini", fetchedAt: new Date(START).toISOString(), servedAt: new Date(START).toISOString(),
    cacheTtlSeconds: 60, usageDataStale: false, activeAccountNumber: 1,
    accounts: [
      { number: 1, email: "ops@example.com", active: true, usageStatus: "ok", usage: { fiveHour: { pct: 34 }, sevenDay: { pct: 61 } } },
      { number: 2, email: "backup@example.com", active: false, usageStatus: "ok", usage: { fiveHour: { pct: 92 }, sevenDay: { pct: 70 } } },
    ],
  });
  await page.route(/\/api\/receipt(?:\?.*)?$/, (route) => {
    const period = (new URL(route.request().url()).searchParams.get("period") ?? "30d") as Period;
    return route.fulfill({ json: buildAnalytics(period).receipt });
  });
  await page.route(/\/api\/token-analytics(?:\?.*)?$/, (route) => {
    const period = (new URL(route.request().url()).searchParams.get("period") ?? "7d") as Period;
    const body = options.analytics ? options.analytics(period) : buildAnalytics(period);
    return body === null
      ? route.fulfill({ status: 503, json: { error: "Aggregation unavailable" } })
      : route.fulfill({ json: body });
  });
}

async function expectNoHorizontalOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
}

const readout = (page: Page) => page.getByTestId("stats-daily-readout-text");

test.describe("stats page", () => {
  test("daily chart reads one day at a time with the step buttons, keys and pointer", async ({ page }) => {
    await mockApi(page, { analytics: (period) => buildAnalytics(period === "7d" ? "30d" : period) });
    await page.goto("/stats");
    const chart = page.getByTestId("stats-daily-token-scroll");
    await expect(chart.locator("svg.recharts-surface")).toBeVisible({ timeout: 15_000 });
    await expect(readout(page)).toContainText("막대를 누르거나");

    for (const id of ["stats-daily-prev", "stats-daily-next"]) {
      const box = await page.getByTestId(id).boundingBox();
      expect(box?.width ?? 0).toBeGreaterThanOrEqual(44);
      expect(box?.height ?? 0).toBeGreaterThanOrEqual(44);
    }

    // Next starts from the latest day, previous walks back one day.
    await page.getByTestId("stats-daily-next").click();
    await expect(readout(page)).toContainText("11. 18.");
    await page.getByTestId("stats-daily-prev").click();
    await expect(readout(page)).toContainText("11. 17.");
    await expect(readout(page)).toContainText("cache R");

    await chart.focus();
    await page.keyboard.press("Home");
    await expect(readout(page)).toContainText("10. 20.");
    await page.keyboard.press("ArrowRight");
    await expect(readout(page)).toContainText("10. 21.");
    await page.keyboard.press("End");
    await expect(readout(page)).toContainText("11. 18.");
    await expect.poll(() => chart.evaluate((el) => el.scrollLeft + el.clientWidth >= el.scrollWidth - 2)).toBe(true);
    await page.keyboard.press("Escape");
    await expect(readout(page)).toContainText("막대를 누르거나");

    // Pointer: a press on the first day column picks it.
    await chart.evaluate((el) => { el.scrollLeft = 0; });
    const plot = chart.locator('[role="img"]');
    const box = (await plot.boundingBox())!;
    await page.mouse.click(box.x + 46 + 4, box.y + box.height / 2);
    await expect(readout(page)).toContainText("10. 20.");
    await expectNoHorizontalOverflow(page);
  });

  test("touch: tap toggles a day and a sideways scrub follows the finger", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== "mobile", "Touch reading is checked on the phone viewport");
    await mockApi(page);
    await page.goto("/stats");
    const plot = page.getByTestId("stats-daily-token-scroll").locator('[role="img"]');
    await expect(plot.locator("svg.recharts-surface")).toBeVisible({ timeout: 15_000 });
    await expect(plot).toHaveCSS("touch-action", "pan-y");
    const box = (await plot.boundingBox())!;
    const plotWidth = box.width - 46 - 12;
    const columnX = (index: number) => box.x + 46 + (index + 0.5) * (plotWidth / 7);
    const y = box.y + box.height / 2;
    const fire = (type: string, x: number) =>
      plot.dispatchEvent(type, { pointerType: "touch", pointerId: 7, isPrimary: true, clientX: x, clientY: y, bubbles: true });

    await fire("pointerdown", columnX(2));
    await fire("pointerup", columnX(2));
    await expect(readout(page)).toContainText("10. 22.");
    await fire("pointerdown", columnX(2));
    await fire("pointerup", columnX(2));
    await expect(readout(page)).toContainText("막대를 누르거나");

    await fire("pointerdown", columnX(1));
    await fire("pointermove", columnX(3));
    await fire("pointermove", columnX(5));
    await expect(readout(page)).toContainText("10. 25.");
    await fire("pointerup", columnX(5));
    await expect(readout(page)).toContainText("10. 25.");
  });

  test("a model or agent row opens its detail beside the list on desktop and as a sheet on mobile", async ({ page }, testInfo) => {
    const mobile = testInfo.project.name === "mobile";
    await mockApi(page);
    await page.goto("/stats");
    const breakdown = page.getByTestId("stats-breakdown");
    await expect(breakdown.getByRole("tab", { name: /모델/ })).toHaveAttribute("aria-selected", "true", { timeout: 15_000 });
    const panel = page.getByTestId("stats-model-share");
    const kitRows = panel.locator(".aui-compact-breakdown-row");
    await expect(kitRows).toHaveCount(3);

    // The kit share list reads every model's tokens and share; a row opens the cost and cache detail.
    const firstRow = kitRows.first();
    await expect(firstRow).toHaveAttribute("aria-label", "Claude Opus 4.6, 2.5M, 전체의 63%");
    if (!mobile) await expect(page.getByTestId("stats-detail-empty")).toBeVisible();

    await firstRow.click();
    if (mobile) {
      const dialog = page.getByTestId("stats-detail-dialog");
      await expect(dialog).toBeVisible();
      await expect(dialog.getByRole("heading", { name: "Claude Opus 4.6" })).toBeVisible();
      await expect(dialog.getByTestId("stats-detail-body")).toContainText("2.5M");
      await expect(dialog.getByTestId("stats-detail-body")).toContainText("캐시 절감");
      const viewport = page.viewportSize()!;
      await expect.poll(async () => { const box = await dialog.locator(".aui-dialog-clean__panel").boundingBox(); return box ? box.y + box.height : 0; }).toBeGreaterThan(viewport.height - 4);
      const sheet = (await dialog.locator(".aui-dialog-clean__panel").boundingBox())!;
      expect(sheet.width).toBeGreaterThan(viewport.width - 4);
      await dialog.getByRole("button", { name: "닫기" }).click();
      await expect(dialog).toBeHidden();
      await expect(firstRow).toBeFocused();
    } else {
      const detail = page.getByTestId("stats-detail");
      await expect(detail.getByRole("heading", { name: "Claude Opus 4.6" })).toBeVisible();
      await expect(detail.getByTestId("stats-detail-body")).toContainText("$31.40");
      const [listBox, detailBox] = await Promise.all([panel.boundingBox(), detail.boundingBox()]);
      expect(detailBox!.x).toBeGreaterThan(listBox!.x + listBox!.width);
      await expect(page.getByTestId("stats-detail-dialog")).toHaveCount(0);
      // Tab reaches the next row and Enter or Space opens it; the list marks the open row.
      await expect(firstRow).toHaveAttribute("aria-current", "true");
      await page.keyboard.press("Tab");
      await expect(kitRows.nth(1)).toBeFocused();
      await page.keyboard.press("Enter");
      await expect(detail.getByRole("heading", { name: "GPT-5.4" })).toBeVisible();
      await expect(kitRows.nth(1)).toHaveAttribute("aria-current", "true");
      await expect(firstRow).not.toHaveAttribute("aria-current", "true");
      await page.keyboard.press("Tab");
      await page.keyboard.press(" ");
      await expect(detail.getByRole("heading", { name: "Claude Haiku 4.5" })).toBeVisible();
    }

    await breakdown.getByRole("tab", { name: /에이전트/ }).click();
    const agentPanel = page.getByTestId("stats-agent-cache");
    await expect(agentPanel.locator(".aui-compact-breakdown-row")).toHaveCount(4);
    await expect(agentPanel).toContainText("전체 캐시 히트");
    await agentPanel.locator('.aui-compact-breakdown-row[data-item-id="adk-dashboard"]').click();
    const body = mobile ? page.getByTestId("stats-detail-dialog") : page.getByTestId("stats-detail");
    await expect(body.getByTestId("stats-detail-body")).toHaveAttribute("data-item", "adk-dashboard");
    await expect(body).toContainText("agentdesk-issue-pipeline");
    await expect(body).toContainText("28 회");
    if (mobile) await page.keyboard.press("Escape");
    await expectNoHorizontalOverflow(page);
  });

  test("tablet: below 900px the row detail opens as a right-hand drawer", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== "desktop", "Resizes the desktop project to a tablet");
    await page.setViewportSize({ width: 820, height: 1180 });
    await mockApi(page);
    await page.goto("/stats");
    const row = page.getByTestId("stats-model-share").locator(".aui-compact-breakdown-row").first();
    await row.click({ timeout: 15_000 });
    await expect(page.getByTestId("stats-detail")).toHaveCount(0);
    const dialog = page.getByTestId("stats-detail-dialog");
    await expect(dialog.getByRole("heading", { name: "Claude Opus 4.6" })).toBeVisible();
    // Measured after the kit's open animation settles.
    const panelBox = () => dialog.locator(".aui-dialog-clean__panel").boundingBox();
    await expect.poll(async () => {
      const box = await panelBox();
      return box ? { right: Math.round(box.x + box.width), height: Math.round(box.height) } : null;
    }).toEqual({ right: 820, height: 1180 });
    expect((await panelBox())!.width).toBeLessThanOrEqual(421);
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();
    await expect(row).toBeFocused();
  });

  test("empty, zero and failed ranges keep their own messages and retry", async ({ page }) => {
    let mode: "failed" | "empty" | "zero" | "default" = "failed";
    await mockApi(page, { analytics: (period) => (mode === "failed" ? null : buildAnalytics(period, mode)) });
    await page.goto("/stats");
    const alert = page.getByRole("alert").filter({ hasText: "일부 통계를 불러오지 못했어요." });
    await expect(alert).toBeVisible({ timeout: 15_000 });
    await expect(page.getByTestId("stats-daily-empty")).toContainText("일별 토큰을 불러오지 못했어요");
    await expect(page.getByTestId("stats-model-share")).toContainText("분류를 불러오지 못했어요");

    mode = "empty";
    await alert.getByRole("button", { name: "다시 시도" }).click();
    await expect(alert).toBeHidden();
    await expect(page.getByTestId("stats-daily-empty")).toContainText("표시할 일별 토큰 데이터가 없습니다.");
    await expect(page.getByTestId("stats-model-share")).toContainText("표시할 모델 데이터가 없습니다.");

    mode = "zero";
    await page.getByTestId("stats-range-30d").click();
    await expect(page.getByTestId("stats-daily-zero")).toBeVisible();
    await expect(page.getByTestId("stats-summary-total-tokens")).toContainText("0");
    await page.getByTestId("stats-daily-next").click();
    await expect(readout(page)).toContainText("0 tokens");
    await expectNoHorizontalOverflow(page);
  });

  test("phone: no sideways overflow and every stats control is a 44px touch target", async ({ browser }, testInfo) => {
    test.skip(testInfo.project.name !== "mobile", "Phone viewport only");
    const context = await browser.newContext({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true, baseURL: testInfo.project.use.baseURL });
    const page = await context.newPage();
    await mockApi(page);
    await page.goto("/stats");
    await expect(page.getByTestId("stats-daily-token-scroll").locator("svg.recharts-surface")).toBeVisible({ timeout: 15_000 });
    expect(await page.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);
    await expectNoHorizontalOverflow(page);
    const small = await page.evaluate(() => {
      const scopes = ["stats-page-header", "stats-breakdown", "stats-daily-token-chart", "stats-summary-grid", "stats-claude-accounts", "stats-skill-usage"]
        .map((id) => document.querySelector(`[data-testid="${id}"]`))
        .filter((node): node is Element => node !== null);
      const selector = 'button, a[href], input, select, textarea, summary, [tabindex="0"], [role="tab"], [role="radio"]';
      return scopes.flatMap((scope) => Array.from(scope.querySelectorAll<HTMLElement>(selector)))
        .filter((node) => node.offsetParent !== null)
        .map((node) => ({ label: node.getAttribute("aria-label") ?? node.textContent?.trim().slice(0, 24) ?? node.tagName, rect: node.getBoundingClientRect() }))
        .filter(({ rect }) => Math.min(rect.width, rect.height) < 44)
        .map(({ label, rect }) => `${label} ${Math.round(rect.width)}×${Math.round(rect.height)}`);
    });
    expect(small).toEqual([]);
    await context.close();
  });

  test("account limits mark the auto-run pause from the runtime config", async ({ page }) => {
    await mockApi(page);
    await page.goto("/stats");
    const limits = page.getByTestId("stats-claude-accounts");
    await expect(limits.getByRole("meter", { name: "backup@example.com · #2 5시간" })).toHaveAttribute("aria-valuetext", /자동 진행 멈춤 기준 이상/, { timeout: 15_000 });
    await expect(limits.locator('.aui-usage-meter-mini-row[data-state="danger"]')).toHaveCount(1);
  });

  test("English phone: the kit charts and the share list show and speak no Korean", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name !== "mobile", "The narrow chart shows its reading hint");
    await mockApi(page, { language: "en" });
    await page.goto("/stats");
    await expect(page.locator(".aui-kpi-chart-reading-hint").first()).toBeVisible({ timeout: 15_000 });
    const text = await page.locator(".aui-kpi-chart, .aui-compact-breakdown").evaluateAll((nodes) => nodes.flatMap((node) => [
      (node as HTMLElement).innerText,
      ...Array.from(node.querySelectorAll(".aui-kpi-chart-sr, .aui-compact-breakdown-sr"), (sr) => sr.textContent ?? ""),
      ...Array.from(node.querySelectorAll("[aria-label]"), (labelled) => labelled.getAttribute("aria-label") ?? ""),
    ]).join("\n"));
    expect(text.match(/[가-힣]+/g)).toBeNull();
  });

  test("screenshots for review", async ({ browser }, testInfo) => {
    test.skip(!SHOTS || testInfo.project.name !== "desktop", "Set STATS_SHOTS to capture review screenshots");
    for (const theme of ["light", "dark"] as const) {
      for (const [width, height] of [[390, 844], [820, 1180], [1440, 900]] as const) {
        const touch = width < 900;
        const context = await browser.newContext({ viewport: { width, height }, hasTouch: touch, isMobile: width < 600, baseURL: testInfo.project.use.baseURL });
        const page = await context.newPage();
        await mockApi(page, { theme, analytics: (period) => buildAnalytics(period === "7d" ? "30d" : period) });
        await page.goto("/stats");
        await expect(page.getByTestId("stats-daily-token-scroll").locator("svg.recharts-surface")).toBeVisible({ timeout: 15_000 });
        await page.getByTestId("stats-daily-prev").click();
        const name = `stats-${width}x${height}-${theme}`;
        await page.getByTestId("stats-page").evaluate((el) => el.scrollTo(0, 0));
        await page.screenshot({ path: `${SHOTS}/${name}.png` });
        // The page scrolls inside its own container, so a tall viewport captures the whole screen.
        const fullHeight = await page.getByTestId("stats-page").evaluate((el) => el.scrollHeight + el.getBoundingClientRect().top + 80);
        await page.setViewportSize({ width, height: Math.ceil(fullHeight) });
        await page.waitForTimeout(300);
        await page.screenshot({ path: `${SHOTS}/${name}-full.png` });
        await page.setViewportSize({ width, height });
        await page.getByTestId("stats-model-share").locator(".aui-compact-breakdown-row").first().click();
        if (width < 900) await expect(page.getByTestId("stats-detail-dialog")).toBeVisible();
        await page.waitForTimeout(400);
        await page.screenshot({ path: `${SHOTS}/${name}-detail.png` });
        await context.close();
      }
    }
  });
});
