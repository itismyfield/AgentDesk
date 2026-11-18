import { expect, test, type Page } from "@playwright/test";
import { serverCampaignFixtures } from "../src/api/serverCampaignsFixtures";

// Set CAMPAIGNS_SHOTS to a directory to also save 390/820/1440 x light/dark screenshots of the campaign tab.
const SHOTS = process.env.CAMPAIGNS_SHOTS;
const NOW = "2026-11-18T08:00:00.000Z";
const issue = (number: number) => `https://github.com/itismyfield/AgentDesk/issues/${number}`;

const node = (id: string, title: string, patch: Record<string, unknown> = {}) => ({
  id, title, status: "pending", stage: "implement", group: "Gateway", round: 1, assignee: null, session_id: null, provider: null,
  dependencies: [], issue_url: null, pr_url: null, head_sha: null, evidence: [], next_action: null, blocker: null, summary: null,
  benefit: null, post_merge_hold: null, updated_at: NOW, details: "", acceptance: [], findings: [], evidence_records: [], ...patch,
});
const campaign = (id: string, title: string, nodes: unknown[], patch: Record<string, unknown> = {}) => ({
  id, title, description: "", status: "active", round: 2, revision: 7, auto_queue: false, nodes, created_at: NOW, updated_at: NOW, ...patch,
});
const CAMPAIGNS = [
  campaign("open-issues-dag", "열린 이슈 DAG", [
    node("i7272", "#7272 무중단 재시작 체크포인트를 단계마다 남기고 실패한 단계부터 다시 이어가기", {
      status: "running", stage: "review: 재시작 프로토콜 리뷰 2회차", issue_url: issue(7272), assignee: "agent-ada", round: 2,
      summary: "dcserver 재시작 뒤에도 마지막 체크포인트에서 이어 갑니다.", benefit: "배포 중 끊긴 턴을 다시 시작하지 않아도 됩니다.",
    }),
    node("i7336", "#7336 꺼진 맥의 채널 메시지를 다른 맥이 이어받기", {
      status: "blocked", stage: "design", issue_url: issue(7336), group: "Cluster", blocker: "mac-book 쪽 tailscale 인증이 만료되어 확인을 기다립니다.",
    }),
    node("i7340", "#7340 릴레이 재전송 큐 지표", { status: "running", stage: "impl · 테스트 보강", issue_url: issue(7340), group: "Relay", assignee: "agent-luna" }),
    node("i7401", "#7401 게이트웨이 재연결", { status: "completed", stage: "merged", issue_url: issue(7401) }),
    node("i7402", "#7402 게이트웨이 헬스 체크 간격 조정", { stage: "investigate", issue_url: issue(7402) }),
    node("i7403", "#7403 Discord 스레드 재생성 시 세션 매핑 유지", { stage: "implement", issue_url: issue(7403), group: "Relay" }),
    node("i7404", "#7404 오래된 실행 로그 압축", { status: "skipped", stage: "dropped", issue_url: issue(7404), group: null }),
    node("cli-update", "Update CLIs on every node", { stage: "verify", group: null }),
  ], { description: "열린 이슈를 의존 순서대로 처리합니다.\n막힌 작업은 담당자 확인 뒤 다시 진행합니다." }),
  campaign("release-train", "릴리스 트레인", [node("i7500", "#7500 릴리스 노트 정리", { issue_url: issue(7500), group: "Release" })], { round: 1 }),
];
const LIVE = {
  "open-issues-dag": {
    i7272: { card_id: "card-7272", card_status: "review", dispatch_type: "review", dispatch_status: "dispatched", session_status: "turn_active", session_seen_at: NOW, running: true, queue_status: "dispatched" },
  },
};

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
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: CAMPAIGNS, live: LIVE, limit: 100, offset: 0 });
  await page.route(/\/api\/campaigns\/[^/?]+$/, (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").pop() ?? "");
    const found = CAMPAIGNS.find((value) => value.id === id);
    return found ? route.fulfill({ json: { campaign: found, live: LIVE[id as keyof typeof LIVE] ?? {} } }) : route.fulfill({ status: 404, json: { error: "not found" } });
  });
  await json(page, /\/api\/server-campaigns\?.*$/, { campaigns: [] });
  return calls;
}

async function openCampaigns(page: Page, theme: "dark" | "light" = "dark") {
  await page.clock.install({ time: new Date(NOW) });
  const calls = await mockApi(page, theme);
  await page.goto("/campaigns");
  await expect(page.locator(".campaign-task-row").first()).toBeVisible({ timeout: 15_000 });
  return calls;
}

/** A server campaign beside the ledger ones: running, one failed and one blocked task. */
async function serveServerCampaign(page: Page) {
  const release = serverCampaignFixtures.failedBlockedComplete;
  await json(page, /\/api\/server-campaigns\?.*$/, { campaigns: [release] });
  await json(page, /\/api\/server-campaigns\/release$/, { campaign: release });
  await json(page, /\/api\/server-campaigns\/release\/events(?:\?.*)?$/, { events: [] });
  return release;
}

async function expectNoHorizontalOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
}

test("campaign tab: the first active campaign with its summary, running work first, then tasks by group", async ({ page }, testInfo) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  await expect(page.getByRole("tab", { name: "캠페인" })).toHaveAttribute("aria-selected", "true");
  await expect(panel.getByLabel("캠페인 선택")).toHaveValue("open-issues-dag");
  await expect(page).toHaveURL(/[?&]campaign=open-issues-dag/);
  const summary = panel.locator(".campaign-summary-header");
  await expect(summary.getByText("활성", { exact: true })).toBeVisible();
  await expect(summary.getByText("라운드 2")).toBeVisible();
  await expect(summary.getByText("완료 1/8")).toBeVisible();
  await expect(panel.getByRole("switch", { name: "자동 진행" })).toHaveAttribute("aria-checked", "false");
  await expect(panel.locator("progress.campaign-progress")).toHaveAttribute("value", "12");

  // The description stays folded until asked for.
  const description = panel.getByText("열린 이슈를 의존 순서대로 처리합니다.", { exact: false });
  await expect(description).toBeHidden();
  await panel.getByText("캠페인 설명").click();
  await expect(description).toBeVisible();

  // Running and blocked work lead: blocked first, with its note; the rest are closed status groups.
  const glance = panel.getByRole("region", { name: "한눈 요약" });
  await expect(glance.getByRole("heading", { name: /지금 진행 중 2/ })).toBeVisible();
  await expect(glance.getByText("막힘 1", { exact: true })).toBeVisible();
  const active = glance.locator("[data-glance-id]");
  await expect(active.first()).toHaveAttribute("data-glance-id", "i7336");
  await expect(glance.locator('[data-glance-id="i7336"] .campaign-glance-blocker')).toContainText("tailscale 인증이 만료");
  await expect(glance.locator('[data-glance-id="i7272"]')).toContainText("리뷰");
  await expect(glance.locator('[data-glance-id="i7272"] .campaign-live')).toHaveText("리뷰 진행 중");
  const pending = glance.getByRole("button", { name: /^대기 3$/ });
  await expect(pending).toHaveAttribute("aria-expanded", "false");
  await pending.click();
  await expect(glance.getByRole("list", { name: "대기" }).getByRole("button")).toHaveCount(3);

  // Tasks by group, with the counts in each group row.
  const groups = panel.getByRole("list", { name: "그룹별 작업" });
  await expect(groups.locator(".campaign-group-heading")).toHaveText([/^Cluster 1/, /^Gateway 3/, /^Relay 2/, /^미분류 2/]);
  const row = groups.locator('.campaign-task-row[data-node-id="i7272"]');
  await expect(row.locator(".campaign-task-id")).toHaveText("#7272");
  await expect(row.locator(".campaign-task-stage")).toHaveText("review: 재시작 프로토콜 리뷰 2회차");
  await expect(row.locator(".campaign-task-title")).toContainText("진행 중");
  expect((await row.locator(".campaign-task-stage").boundingBox())?.height ?? 0).toBeLessThan(24);

  await panel.getByRole("searchbox", { name: "작업 검색" }).fill("#7336");
  await expect(groups.locator(".campaign-task-row")).toHaveCount(1);
  await expect(panel.getByText("작업 1/8개 · 그룹 1개")).toBeVisible();
  await panel.getByRole("searchbox", { name: "작업 검색" }).fill("");
  await expect(groups.locator(".campaign-task-row")).toHaveCount(8);
  await expectNoHorizontalOverflow(page);
  if (testInfo.project.name === "mobile") {
    // A phone keeps the filters behind one switch.
    const filters = panel.getByRole("button", { name: "필터" });
    await expect(panel.getByRole("combobox", { name: "상태 필터" })).toBeHidden();
    await filters.click();
    await expect(filters).toHaveAttribute("aria-expanded", "true");
  }
  await panel.getByRole("combobox", { name: "상태 필터" }).click();
  await page.getByRole("option", { name: "완료" }).click();
  await expect(groups.locator(".campaign-task-row")).toHaveCount(1);
  await expect(groups.locator(".campaign-task-row")).toHaveAttribute("data-node-id", "i7401");
  await expectNoHorizontalOverflow(page);
});

test("campaign tab: a task opens in the pinned pane on wide screens and in a sheet on phones", async ({ page }, testInfo) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  const pane = page.getByRole("complementary", { name: "선택 작업" });
  const row = panel.locator('.campaign-task-row[data-node-id="i7403"]');
  if (testInfo.project.name === "desktop") {
    await expect(pane).toBeVisible();
    await expect(pane).toContainText("고른 작업이 없습니다");
    const box = (await pane.boundingBox())!;
    expect(box.x + box.width).toBeGreaterThan(1440 - 4);
    // The list stays clear of the pane.
    expect(((await row.boundingBox())!.x + (await row.boundingBox())!.width)).toBeLessThanOrEqual(box.x);
  } else {
    await expect(pane).toHaveCount(0);
  }
  await row.click();
  await expect(page).toHaveURL(/[?&]node=i7403/);
  await expect(row).toHaveAttribute("aria-current", "true");
  const detail = page.locator(".campaign-node-detail");
  await expect(detail.getByRole("heading", { name: /Discord 스레드 재생성/ })).toBeVisible();
  if (testInfo.project.name === "desktop") {
    await expect(pane.locator(".campaign-node-detail")).toBeVisible();
    // Non-modal: the list beside it still works.
    await panel.locator('.campaign-task-row[data-node-id="i7402"]').click();
    await expect(pane.getByRole("heading", { name: /헬스 체크 간격/ })).toBeVisible();
  } else {
    const sheet = page.getByRole("dialog", { name: "선택 작업" });
    await expect(sheet).toBeVisible();
    const box = (await sheet.boundingBox())!;
    expect(box.y + box.height).toBeGreaterThan(844 - 12);
  }
  await expectNoHorizontalOverflow(page);
  await page.getByRole("button", { name: "상세 닫기" }).click();
  await expect(detail).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]node=/);
  if (testInfo.project.name === "desktop") await expect(pane).toContainText("고른 작업이 없습니다");

  // A glance row opens the same details.
  await panel.locator('[data-glance-id="i7336"]').click();
  await expect(detail.getByRole("heading", { name: /꺼진 맥의 채널 메시지/ })).toBeVisible();
  await page.getByRole("button", { name: "상세 닫기" }).click();

  // Switching campaigns keeps the choice and reads only the shown campaign.
  await panel.getByLabel("캠페인 선택").selectOption("release-train");
  await expect(panel.locator('.campaign-task-row[data-node-id="i7500"]')).toBeVisible();
  await expect(page).toHaveURL(/[?&]campaign=release-train/);
  expect(await page.evaluate(() => window.localStorage.getItem("agentdesk.dashboard.active-campaign"))).toBe(JSON.stringify("release-train"));
});

test("server campaign: tasks as the same rows, opened in the pinned pane or a sheet", async ({ page }, testInfo) => {
  await page.clock.install({ time: new Date(NOW) });
  await mockApi(page);
  const release = await serveServerCampaign(page);
  await page.goto("/campaigns?mode=server&campaign=release");
  const rows = page.locator(".campaign-server-task-row");
  await expect(rows).toHaveCount(release.definition.nodes.length, { timeout: 15_000 });
  await expect(page.locator(".campaigns-panel").getByLabel("캠페인 선택")).toHaveValue("server:release");
  await expect(rows.first()).toContainText(release.definition.nodes[0].title);
  await expect(rows.first().locator(".aui-badge-clean")).toHaveText("실패");
  await expectNoHorizontalOverflow(page);
  await rows.nth(1).click();
  await expect(page).toHaveURL(/[?&]node=B/);
  const title = release.definition.nodes[1].title;
  if (testInfo.project.name === "desktop") {
    await expect(page.getByRole("complementary", { name: "선택 작업" }).getByRole("heading", { name: title })).toBeVisible();
  } else {
    await expect(page.getByRole("dialog", { name: "선택 작업" }).getByRole("heading", { name: title })).toBeVisible();
  }
  await page.getByRole("button", { name: "상세 닫기" }).click();
  await expect(page).not.toHaveURL(/[?&]node=/);
  await expect(rows.nth(1)).toBeFocused();
});

test.describe("touch at 390px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("campaign tab: no sideways scroll, 44px targets and 16px inputs", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone width check");
    await openCampaigns(page);
    expect(await page.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);
    await page.getByRole("button", { name: "필터" }).click();
    await expectNoHorizontalOverflow(page);
    const small = (root: HTMLElement | SVGElement) => {
      const selector = "button, a[href], input, select, textarea, summary, [role='switch'], [role='radio'], [role='combobox'], [tabindex='0']";
      return Array.from(root.querySelectorAll<HTMLElement>(selector))
        .filter((node) => !(node as HTMLButtonElement).disabled && node.getClientRects().length && getComputedStyle(node).visibility !== "hidden")
        .map((node) => ({ node, box: node.getBoundingClientRect() }))
        .filter(({ box }) => box.width > 0 && Math.min(box.width, box.height) < 44)
        .map(({ node, box }) => `${node.tagName}.${node.className.toString().slice(0, 40)} ${Math.round(box.width)}x${Math.round(box.height)} ${node.textContent?.trim().slice(0, 20)}`);
    };
    const tiny = (root: HTMLElement | SVGElement) => Array.from(root.querySelectorAll<HTMLElement>("input:not([type='checkbox']):not([type='radio']), select, textarea"))
      .filter((node) => node.getClientRects().length && parseFloat(getComputedStyle(node).fontSize) < 16)
      .map((node) => node.outerHTML.slice(0, 80));
    const tab = page.locator("#campaign-panel-campaigns");
    expect(await tab.evaluate(small)).toEqual([]);
    expect(await tab.evaluate(tiny)).toEqual([]);

    // The task sheet and its edit form follow the same rule.
    await page.locator('.campaign-task-row[data-node-id="i7403"]').click();
    const sheet = page.getByRole("dialog", { name: "선택 작업" });
    await expect(sheet.locator(".campaign-node-detail")).toBeVisible();
    await sheet.getByRole("button", { name: "작업 수정" }).click();
    await expect(sheet.locator("form")).toBeVisible();
    expect(await sheet.evaluate(small)).toEqual([]);
    expect(await sheet.evaluate(tiny)).toEqual([]);
    await expectNoHorizontalOverflow(page);
  });
});

test("campaign tab: screenshots at 390, 820 and 1440 in light and dark", async ({ page }, testInfo) => {
  test.skip(!SHOTS || testInfo.project.name !== "desktop", "Set CAMPAIGNS_SHOTS to collect screenshots");
  test.setTimeout(180_000);
  for (const theme of ["light", "dark"] as const) {
    await page.unrouteAll({ behavior: "ignoreErrors" });
    await mockApi(page, theme);
    for (const [width, height] of [[390, 844], [820, 1180], [1440, 900]] as const) {
      await page.setViewportSize({ width, height });
      await page.goto("/campaigns?mode=legacy&campaign=open-issues-dag");
      await expect(page.locator(".campaign-task-row").first()).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      await page.mouse.move(0, 0);
      await page.screenshot({ path: `${SHOTS}/campaigns-${width}x${height}-${theme}.png` });
      // The page scrolls inside the app shell, so the lower part gets its own shot.
      await page.locator(".campaign-group-list").scrollIntoViewIfNeeded();
      await page.waitForTimeout(300);
      await page.screenshot({ path: `${SHOTS}/campaigns-${width}x${height}-${theme}-list.png` });
      await page.locator(".campaign-summary-header").scrollIntoViewIfNeeded();
      await page.waitForTimeout(300);
      await page.locator('.campaign-task-row[data-node-id="i7272"]').click();
      await expect(page.locator(".campaign-node-detail")).toBeVisible();
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${SHOTS}/campaigns-${width}x${height}-${theme}-detail.png` });
    }
    await serveServerCampaign(page);
    for (const [width, height] of [[390, 844], [1440, 900]] as const) {
      await page.setViewportSize({ width, height });
      await page.goto("/campaigns?mode=server&campaign=release&node=B");
      await expect(page.locator(".campaign-inspector h4").first()).toBeVisible({ timeout: 15_000 });
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${SHOTS}/campaigns-server-${width}x${height}-${theme}-detail.png` });
    }
  }
});

