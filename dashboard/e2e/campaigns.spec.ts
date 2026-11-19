import { expect, test, type Page } from "@playwright/test";
import { serverCampaignFixtures } from "../src/api/serverCampaignsFixtures";
import type { ServerCampaignView } from "../src/api/serverCampaigns";

// Set CAMPAIGNS_SHOTS to a directory to also save 390/1440 x light/dark screenshots, plus dark fixture shots of a server campaign.
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
      status: "running", stage: "review: 재시작 프로토콜 리뷰 2회차", issue_url: issue(7272), assignee: "agent-ada", round: 2, session_id: "01a180b1-e645",
      summary: "dcserver 재시작 뒤에도 마지막 체크포인트에서 이어 갑니다.", benefit: "배포 중 끊긴 턴을 다시 시작하지 않아도 됩니다.", pr_url: "https://github.com/itismyfield/AgentDesk/pull/8240",
      evidence_records: [
        { summary: "리뷰 2회차 시작", command: null, result: null, head_sha: null, recorded_at: "2026-11-18T07:40:00.000Z", references: [] },
        { summary: "설계 리뷰 CLEAN", command: null, result: "P0·P1 없음", head_sha: "60aac7044", recorded_at: "2026-11-18T06:10:00.000Z", references: [] },
      ],
    }),
    node("i7336", "#7336 꺼진 맥의 채널 메시지를 다른 맥이 이어받기", {
      status: "blocked", stage: "design", issue_url: issue(7336), group: "Cluster", blocker: "mac-book 쪽 tailscale 인증이 만료되어 확인을 기다립니다.",
    }),
    node("i7340", "#7340 릴레이 재전송 큐 지표", { status: "running", stage: "impl · 테스트 보강", issue_url: issue(7340), group: "Relay", assignee: "agent-luna", updated_at: "2026-11-18T07:00:00.000Z", blocker: " None " }),
    node("i7401", "#7401 게이트웨이 재연결", { status: "completed", stage: "merged", issue_url: issue(7401) }),
    node("i7402", "#7402 게이트웨이 헬스 체크 간격 조정", { stage: "investigate", issue_url: issue(7402), blocker: "선행: #7401 게이트웨이 재연결" }),
    node("i7403", "#7403 Discord 스레드 재생성 시 세션 매핑 유지", { stage: "implement", issue_url: issue(7403), group: "Relay" }),
    node("i7404", "#7404 오래된 실행 로그 압축", { status: "skipped", stage: "dropped", issue_url: issue(7404), group: null }),
    node("cli-update", "Update CLIs on every node", { stage: "verify", group: null }),
  ], { description: "열린 이슈를 의존 순서대로 처리합니다.\n막힌 작업은 담당자 확인 뒤 다시 진행합니다." }),
  campaign("release-train", "릴리스 트레인", [node("i7500", "#7500 릴리스 노트 정리", { issue_url: issue(7500), group: "Release" })], { round: 1, status: "paused" }),
];
const LIVE = {
  "open-issues-dag": {
    i7272: { card_id: "card-7272", card_status: "review", dispatch_type: "review", dispatch_status: "dispatched", session_status: "turn_active", session_seen_at: NOW, running: true, queue_status: "dispatched" },
  },
};
const agent = (id: string, name: string, nameKo: string) => ({
  id, role_id: id, name, alias: null, name_ko: nameKo, name_ja: null, name_zh: null, department_id: null, cli_provider: "codex",
  avatar_emoji: "🛠️", sprite_number: 3, personality: null, status: "working", current_task_id: null, workflow_pack_key: "development",
  stats_tasks_done: 0, stats_tokens: 0, activity_source: "agentdesk", created_at: 1_790_000_000_000,
});

/** A server campaign with a failed and a blocked task; its events open a landing gate on C. */
function gatedServerCampaign() {
  const release = structuredClone(serverCampaignFixtures.failedRunning);
  release.definition.title = "대시보드 v3 착지";
  release.definition.goal = "키트로 대시보드를 다시 짜서 main에 올린다";
  release.definition.nodes[0].title = "설정 카탈로그";
  release.definition.nodes[1].title = "단계 보드";
  release.definition.nodes[2].title = "편집기 케이블타이";
  release.view.nodes[2].runtime.stage = "landing";
  return release;
}

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
  await json(page, /\/api\/agents(?:\?.*)?$/, { agents: [agent("agent-ada", "Ada Dashboard", "아다 대시보드"), agent("agent-luna", "Luna Ops", "루나 옵스")] });
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
  await expect(page.locator(".campaign-overview")).toBeVisible({ timeout: 15_000 });
  return calls;
}

async function serveServerCampaign(page: Page, release = gatedServerCampaign()) {
  await json(page, /\/api\/server-campaigns\?.*$/, { campaigns: [release] });
  await json(page, /\/api\/server-campaigns\/release$/, { campaign: release });
  await json(page, /\/api\/server-campaigns\/release\/events(?:\?.*)?$/, { events: [
    { sequence: 19, run_id: "run-1", command_id: "c-19", observed_at: "2026-11-18T07:37:00.000Z", kind: { event: "approval_gate_opened", binding: {
      run_id: "run-1", node_id: "C", gate: "landing", definition_revision: 7, target: { kind: "candidate", base_sha: "a1b2c3d", head_sha: "e4f5a6b" }, policy_hash: "f".repeat(64) } } },
    { sequence: 20, run_id: "run-1", command_id: "c-20", observed_at: "2026-11-18T07:52:00.000Z", kind: { event: "node_failed" } },
  ] });
  return release;
}

async function expectNoHorizontalOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
}

test("campaign tab: the list or picker, one status sentence, what needs someone, running cards and status lists", async ({ page }, testInfo) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  await expect(page.getByRole("tab", { name: "캠페인" })).toHaveAttribute("aria-selected", "true");
  await expect(page).toHaveURL(/[?&]campaign=open-issues-dag/);
  if (testInfo.project.name === "desktop") {
    await expect(panel.locator('[data-campaign-key="legacy:open-issues-dag"]')).toHaveAttribute("aria-current", "true");
    await expect(panel.getByRole("radio", { name: /진행/ })).toHaveAttribute("aria-checked", "true");
  } else {
    await expect(panel.locator(".campaign-picker")).toContainText("열린 이슈 DAG");
    await expect(panel.locator(".campaign-picker")).toContainText("작업 1/8 완료 · 캠페인 2개 중");
  }
  await expect(panel.locator(".campaign-detail-status")).toHaveText("진행 중 · 2개 진행, 1개 막힘");

  const attention = panel.getByRole("region", { name: "주의" });
  await expect(attention.locator('[data-task-id="i7336"] .campaign-blocker')).toContainText("tailscale 인증이 만료");
  const running = panel.locator('[data-task-id="i7272"]');
  await expect(running).toContainText("리뷰 진행 중");
  await expect(running).toContainText("끝나면: 배포 중 끊긴 턴을 다시 시작하지 않아도 됩니다.");
  await expect(running.getByRole("img", { name: /아다 대시보드/ })).toBeVisible();
  await expect(panel.getByRole("region", { name: "요약" })).toContainText("PR 연결1 / 2");
  await expect(panel.getByRole("switch", { name: "자동 진행" })).toHaveAttribute("aria-checked", "false");

  const chips = panel.locator(".campaign-status-chips button:not(.campaign-find)");
  await expect(chips).toHaveText([/^대기 3$/, /^완료 1$/, /^건너뜀 1$/]);
  await chips.first().click();
  await expect(panel.getByRole("list", { name: "대기" }).locator(".campaign-task-row")).toHaveCount(3);
  await expect(panel.locator('[data-task-id="i7402"] .campaign-reason')).toHaveText("대기 사유: 선행: #7401 게이트웨이 재연결");
  await expect(panel.locator(".campaign-recent-heading")).toHaveText("최근 바뀐 작업 · 5");
  await expectNoHorizontalOverflow(page);

  // Picking another campaign keeps the choice; a phone picks from a bottom sheet.
  if (testInfo.project.name === "mobile") {
    await panel.locator(".campaign-picker").click();
    const sheet = page.getByRole("dialog", { name: "캠페인 고르기" });
    await sheet.getByRole("radio", { name: /준비/ }).click();
    await sheet.locator('[data-campaign-key="legacy:release-train"]').click();
    await expect(panel.locator(".campaign-picker")).toContainText("릴리스 트레인");
  } else {
    await panel.getByRole("radio", { name: /준비/ }).click();
    await panel.locator('[data-campaign-key="legacy:release-train"]').click();
  }
  await expect(panel.locator(".campaign-detail-status")).toHaveText("멈춤 · 진행 중인 작업 없음");
  await expect(page).toHaveURL(/[?&]campaign=release-train/);
  expect(await page.evaluate(() => window.localStorage.getItem("agentdesk.dashboard.active-campaign"))).toBe(JSON.stringify("release-train"));
});

test("campaign tab: a task opens in the pinned pane on wide screens and in a sheet on phones", async ({ page }, testInfo) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  const pane = page.getByRole("complementary", { name: "선택 작업" });
  await expect(pane).toHaveCount(0);
  const card = panel.locator('[data-task-id="i7272"]');
  await card.click();
  await expect(page).toHaveURL(/[?&]node=i7272/);
  await expect(card).toHaveAttribute("aria-current", "true");
  const detail = page.locator(".campaign-node-detail");
  await expect(detail.getByRole("heading", { name: /무중단 재시작/ })).toBeVisible();
  // TaskPhone order: the stage record reads oldest first.
  await expect(detail.locator("ol > li")).toHaveText([/설계 리뷰 CLEAN/, /리뷰 2회차 시작/]);
  if (testInfo.project.name === "desktop") {
    const box = (await pane.boundingBox())!;
    expect(box.x + box.width).toBeGreaterThan(1440 - 4);
    expect((await card.boundingBox())!.x + (await card.boundingBox())!.width).toBeLessThanOrEqual(box.x);
    // Non-modal: the overview beside it still works.
    await panel.locator('[data-task-id="i7336"]').click();
    await expect(pane.getByRole("heading", { name: /꺼진 맥의 채널 메시지/ })).toBeVisible();
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
  await expect(pane).toHaveCount(0);
});

test("campaign tab: closing a task returns focus to the card, list row or connections row that opened it", async ({ page }) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  const closeButton = page.getByRole("button", { name: "상세 닫기" });
  const escape = async () => { await closeButton.focus(); await page.keyboard.press("Escape"); }; // a docked pane takes Escape from inside
  const card = panel.locator('[data-task-id="i7272"]');
  await card.click();
  await closeButton.click();
  await expect(card).toBeFocused();
  await panel.locator(".campaign-status-chips button").first().click();
  const row = panel.getByRole("list", { name: "대기" }).locator('[data-task-id="i7402"]');
  await row.click();
  await escape();
  await expect(row).toBeFocused();
  await panel.getByRole("tab", { name: "관계도" }).click();
  await panel.locator(".react-flow__node").filter({ hasText: "Relay" }).first().click();
  const relayRow = panel.locator('.campaign-relations [data-task-id="i7340"]');
  for (const close of [() => closeButton.click(), escape]) {
    await relayRow.click();
    await expect(page.locator(".campaign-node-detail")).toBeVisible();
    await close();
    await expect(relayRow).toBeFocused();
  }
});

test("campaign tab: below 1280px an open task folds the list into the picker, and the overview beside the pane stays clickable", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "Wide layouts");
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  const pane = page.getByRole("complementary", { name: "선택 작업" });
  for (const width of [900, 1024, 1280, 1440]) {
    await page.setViewportSize({ width, height: 900 });
    await panel.locator('[data-task-id="i7272"]').click();
    await expect(panel.locator(".campaign-picker")).toBeVisible({ visible: width < 1280 });
    await expect(panel.locator("aside .campaign-list")).toBeVisible({ visible: width >= 1280 });
    const edge = (await pane.boundingBox())!.x;
    for (const target of [panel.getByRole("switch", { name: "자동 진행" }), panel.locator(".campaign-status-chips button").first(), panel.locator('[data-task-id="i7340"]')]) {
      const box = (await target.boundingBox())!;
      expect(box.x + box.width, `${width}px`).toBeLessThanOrEqual(edge);
      await target.click({ trial: true });
    }
    if (width === 1024) {
      await panel.locator(".campaign-picker").click();
      await expect(page.getByRole("dialog", { name: "캠페인 고르기" }).locator('[data-campaign-key="legacy:open-issues-dag"]')).toBeVisible();
      await page.keyboard.press("Escape");
    }
    await page.getByRole("button", { name: "상세 닫기" }).click();
  }
});

test("server campaign: a waiting approval and blocked work lead, and approving stays off until its API lands", async ({ page }, testInfo) => {
  await page.clock.install({ time: new Date(NOW) });
  await mockApi(page);
  await serveServerCampaign(page);
  await page.goto("/campaigns?mode=server&campaign=release");
  const panel = page.locator(".campaigns-panel");
  await expect(panel.locator(".campaign-detail-status")).toHaveText("실행 중 · 1개 진행, 1개 승인 대기, 1개 막힘, 1개 실패", { timeout: 15_000 });
  const gate = panel.locator(".campaign-gate");
  await expect(gate).toContainText("착지 승인 대기 · 편집기 케이블타이");
  await expect(gate).toContainText("23분째 기다려요");
  await expect(gate.getByRole("button", { name: "승인", exact: true })).toBeDisabled();
  await expect(gate).toContainText("승인 API 준비 중(#8249)");
  await expect(panel.getByRole("region", { name: "주의" }).locator(".campaign-task-row")).toHaveCount(testInfo.project.name === "desktop" ? 2 : 1);
  await expect(panel.locator(".campaign-recent-heading")).toHaveText("최근 이벤트 · 2");
  await expectNoHorizontalOverflow(page);
  await gate.locator('[data-task-id="C"]').click();
  await page.getByRole("button", { name: "승인 검토" }).click();
  const sheet = page.getByRole("dialog", { name: "착지 승인" });
  await expect(sheet.getByRole("button", { name: "승인", exact: true })).toBeDisabled();
  await sheet.getByRole("button", { name: "나중에" }).click();
  await expect(sheet).toHaveCount(0);
});

test.describe("touch at 390px", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("campaign tab: status, attention and a running task on the first screen; no sideways scroll, 44px targets and 16px inputs", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone width check");
    await openCampaigns(page);
    expect(await page.evaluate(() => matchMedia("(pointer: coarse)").matches)).toBe(true);
    // #6270: the first screen shows the status sentence, what needs someone and a running task without scrolling.
    for (const target of [".campaign-detail-status", ".campaign-attention", '[data-task-id="i7272"]']) {
      const box = (await page.locator(target).first().boundingBox())!;
      expect(box.y + box.height, target).toBeLessThanOrEqual(844 - 64);
    }
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
    await page.locator(".campaign-find").click(); // the search field it opens follows the same rule
    await expect(page.getByRole("searchbox", { name: "작업 찾기" })).toBeVisible();
    const tab = page.locator("#campaign-panel-campaigns");
    expect(await tab.evaluate(small)).toEqual([]);
    expect(await tab.evaluate(tiny)).toEqual([]);

    // The task sheet and its edit form follow the same rule.
    await page.locator('[data-task-id="i7272"]').click();
    const sheet = page.getByRole("dialog", { name: "선택 작업" });
    await expect(sheet.locator(".campaign-node-detail")).toBeVisible();
    await sheet.getByRole("button", { name: "작업 수정" }).click();
    await expect(sheet.locator("form")).toBeVisible();
    expect(await sheet.evaluate(small)).toEqual([]);
    expect(await sheet.evaluate(tiny)).toEqual([]);
    await expectNoHorizontalOverflow(page);
  });
});

test("phone: the More sheet closes when Back changes only the query", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "desktop", "Mobile-only test");
  await mockApi(page);
  await page.goto("/campaigns?tab=queue");
  await expect(page.getByRole("tab", { name: "대기열" })).toHaveAttribute("aria-selected", "true", { timeout: 15_000 });
  // The tab bar pushes the same path with another query, so Back stays on /campaigns.
  await page.getByTestId("app-mobile-tab-campaigns").click();
  await page.getByTestId("app-mobile-more-button").click();
  await expect(page.getByTestId("app-mobile-more-menu")).toBeVisible();
  await page.goBack();
  await expect(page).toHaveURL(/\/campaigns\?tab=queue$/);
  await expect(page.getByTestId("app-mobile-more-menu")).toHaveCount(0);
});

test("campaign tab: screenshots at 390 and 1440 in light and dark, and dark fixture shots of a gated server campaign", async ({ page }, testInfo) => {
  test.skip(!SHOTS || testInfo.project.name !== "desktop", "Set CAMPAIGNS_SHOTS to collect screenshots");
  test.setTimeout(180_000);
  await page.clock.install({ time: new Date(NOW) });
  for (const theme of ["light", "dark"] as const) {
    await page.unrouteAll({ behavior: "ignoreErrors" });
    await mockApi(page, theme);
    for (const [width, height] of [[390, 844], [1440, 900]] as const) {
      await page.setViewportSize({ width, height });
      await page.goto("/campaigns?mode=legacy&campaign=open-issues-dag");
      await expect(page.locator(".campaign-overview")).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      await page.mouse.move(0, 0);
      await page.screenshot({ path: `${SHOTS}/campaigns-${width}x${height}-${theme}.png` });
      await page.locator('[data-task-id="i7272"]').click();
      await expect(page.locator(".campaign-node-detail")).toBeVisible();
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${SHOTS}/campaigns-${width}x${height}-${theme}-detail.png` });
    }
  }
  await serveServerCampaign(page);
  for (const [width, height] of [[390, 844], [1440, 900]] as const) {
    await page.setViewportSize({ width, height });
    await page.goto("/campaigns?mode=server&campaign=release");
    await expect(page.locator(".campaign-gate")).toBeVisible({ timeout: 15_000 });
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${SHOTS}/campaigns-fixture-server-${width}x${height}-dark.png` });
    await page.locator('.campaign-gate [data-task-id="C"]').click();
    await page.getByRole("button", { name: "승인 검토" }).click();
    await page.waitForTimeout(400);
    await page.screenshot({ path: `${SHOTS}/campaigns-fixture-server-${width}x${height}-dark-approval.png` });
  }
});

/** The gated campaign over all nine stages: an approval wait on a task not running, a failure the server would retry, a block
 * before any work, a repair waiting again, a user stop and an empty main check. */
function boardServerCampaign() {
  const release = gatedServerCampaign();
  const base = release.view.nodes[2];
  for (const [id, title, runtime] of [
    ["D", "모바일 셸", { stage: "design", disposition: "pending", repair_cycles: 1 }], ["E", "프롬프트 감사", { stage: "design_review", disposition: "running", series: 2 }],
    ["F", "앵커 감사", { stage: "review", disposition: "interrupted" }], ["G", "재전송 큐 지표", { stage: "final_verification", disposition: "running" }],
    ["H", "게이트웨이 복구", { stage: "complete", disposition: "complete" }],
  ] as [string, string, Partial<ServerCampaignView["nodes"][number]["runtime"]>][]) {
    release.definition.nodes.push({ ...release.definition.nodes[2], id, title, issue_url: issue(7441 + release.definition.nodes.length) });
    release.view.nodes.push({ ...base, id, runtime: { ...base.runtime, current_attempt: null, ...runtime } });
  }
  release.view.commands = release.view.commands.map((entry) => entry.command === "prepare_retry" ? { ...entry, rejection: null } : entry);
  release.view.nodes[2].runtime.disposition = "pending"; // only the gate makes its stage need someone
  return release;
}
/** One ledger campaign in place of the default list, with its live card reads. */
async function serveLedger(page: Page, value: ReturnType<typeof campaign>, live: Record<string, unknown> = {}) {
  await page.clock.install({ time: new Date(NOW) });
  await mockApi(page);
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: [value], live: { [value.id]: live }, limit: 100, offset: 0 });
  await json(page, new RegExp(`/api/campaigns/${value.id}$`), { campaign: value, live });
  await page.goto(`/campaigns?mode=legacy&campaign=${value.id}`);
  await page.getByRole("tab", { name: "단계 보드" }).click();
  return page.locator(".campaign-board");
}
const columnNames = (page: Page) => page.locator(".campaign-board section[aria-label]").evaluateAll((nodes) => nodes.map((node) => node.getAttribute("aria-label")));
const openGroups = (page: Page) => page.locator(".campaign-board section > button[aria-expanded]").evaluateAll((nodes) => nodes.map((node) => node.getAttribute("aria-expanded") === "true"));
const workingLive = { ...LIVE["open-issues-dag"].i7272 };

test("stage board: ledger columns keep each task's own status, live work adds a mark, and phones lead with running and stuck groups", async ({ page }, testInfo) => {
  const [ledger] = structuredClone(CAMPAIGNS);
  ledger.nodes.push(node("i7350", "#7350 실패한 재시작 점검", { status: "failed", stage: "verify", issue_url: issue(7350) }),
    node("i7360", "#7360 배포 확인", { status: "running", stage: "deploy", blocker: "외부 확인", issue_url: issue(7360) }));
  const board = await serveLedger(page, ledger, { ...LIVE["open-issues-dag"], i7336: workingLive, i7350: workingLive });
  await expect(board).toContainText("10 작업5 진행 중3 대기0 승인 대기0 실패2 막힘");
  const phone = testInfo.project.name === "mobile";
  expect(await columnNames(page)).toEqual(phone ? ["진행 중", "막힘", "실패", "대기", "완료", "건너뜀"] : ["대기", "진행 중", "막힘", "실패", "완료", "건너뜀"]);
  if (phone) expect(await openGroups(page)).toEqual([true, true, true, false, false, false]);
  for (const [column, id, chip] of [["막힘", "i7336", "막힘"], ["실패", "i7350", "실패"]]) {
    const card = board.getByRole("region", { name: column }).locator(`[data-board-card="${id}"]`);
    await expect(card.locator(".aui-badge-clean").first()).toHaveText(chip);
    await expect(card.locator(".campaign-live")).toHaveText("리뷰 진행 중");
  }
  // A running task with a blocker keeps its own column and word; only its frame and reason say it is stuck.
  const stuck = board.getByRole("region", { name: "진행 중" }).locator('[data-board-card="i7360"]');
  await expect(stuck.locator(".aui-badge-clean").first()).toHaveText("진행 중");
  await expect(stuck).toContainText("막힘: 외부 확인");
  const done = board.getByRole("region", { name: "완료" });
  await done.getByRole("button", phone ? { name: /^완료/ } : { name: "최근 1개 보기" }).click();
  await expect(done.locator('[data-task-id="i7401"]')).toBeVisible();
  await expectNoHorizontalOverflow(page);
  await board.locator('[data-task-id="i7272"]').click();
  await expect(page.locator(".campaign-node-detail")).toBeVisible();
});

test("stage board: a 431-task ledger shows the finished count and draws finished cards 20 at a time, latest first", async ({ page }, testInfo) => {
  const statusAt = (index: number) => index < 346 ? "completed" : index < 381 ? "skipped" : index < 406 ? "running" : "pending";
  const board = await serveLedger(page, campaign("big", "큰 원장", Array.from({ length: 431 }, (_, index) => node(`n${index}`, `작업 ${index}`,
    { status: statusAt(index), updated_at: new Date(Date.parse(NOW) - (431 - index) * 60_000).toISOString() }))));
  const done = board.getByRole("region", { name: "완료" });
  const cards = done.locator("[data-board-card]");
  await expect(done.locator(".aui-badge-clean").first()).toHaveText("346");
  await expect(cards).toHaveCount(0);
  await done.getByRole("button", testInfo.project.name === "mobile" ? { name: /^완료/ } : { name: "최근 20개 보기" }).click();
  await expect(cards).toHaveCount(20);
  await expect(cards.first()).toHaveAttribute("data-board-card", "n345");
  await done.getByRole("button", { name: "20개 더 보기" }).click();
  await expect(cards).toHaveCount(40);
});

test("stage board: a server campaign keeps the nine stages on wide screens, leads with live and stuck stages on phones, and keeps the server's words", async ({ page }, testInfo) => {
  await page.clock.install({ time: new Date(NOW) });
  await mockApi(page);
  await serveServerCampaign(page, boardServerCampaign());
  let sent: unknown = null;
  await page.route(/\/api\/server-campaigns\/release\/commands$/, (route) => {
    sent = route.request().postDataJSON();
    return route.fulfill({ json: { result: { definition_revision: 7, run_id: "run-1", control_version: 1, event_sequence: 21, control: "running" } } });
  });
  await page.goto("/campaigns?mode=server&campaign=release");
  await page.getByRole("tab", { name: "단계 보드" }).click();
  const phone = testInfo.project.name === "mobile";
  expect(await columnNames(page)).toEqual(phone ? ["요구사항", "설계 리뷰", "구현", "착지", "최종 확인", "설계", "리뷰", "main 확인", "완료"]
    : ["요구사항", "설계", "설계 리뷰", "구현", "리뷰", "착지", "main 확인", "최종 확인", "완료"]);
  if (phone) expect(await openGroups(page)).toEqual([true, true, true, true, true, false, false, false, false]);
  const board = page.locator(".campaign-board");
  const card = (id: string) => board.locator(`[data-board-card="${id}"]`);
  await expect(card("C").getByRole("button", { name: "승인", exact: true })).toBeDisabled();
  await expect(card("C")).toContainText("승인 API 준비 중(#8249)");
  await expect(card("B")).toContainText("막힘: 선행 실패");
  await expect(card("B")).not.toContainText("계열");
  await expect(card("A")).toContainText("계열 1 · 수리 차수 3");
  if (!phone) {
    await expect(card("D")).toContainText("계열 1 · 수리 차수 1");
    await expect(card("F").locator(".aui-badge-clean").first()).toHaveText("사용자 중단");
    // Finished server tasks have no completion order or time, so the first page claims no recency.
    await expect(board.getByRole("region", { name: "완료" }).getByRole("button", { name: "1개 보기" })).toBeVisible();
  }
  await expectNoHorizontalOverflow(page);
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/board-fixture-server-${page.viewportSize()!.width}x${page.viewportSize()!.height}-dark.png` });
  await card("A").getByRole("button", { name: "다시 시도 준비" }).click();
  await page.getByRole("alertdialog").getByRole("button", { name: "다시 시도 준비" }).click();
  await expect.poll(() => sent).toMatchObject({ command: { command: "retry", node_id: "A" } });
});

test.describe("board at 390px touch", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("stage board: no sideways scroll inside the board, 44px targets on the board and in the retry check", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone width check");
    await page.clock.install({ time: new Date(NOW) });
    await mockApi(page);
    await serveServerCampaign(page, boardServerCampaign());
    await page.goto("/campaigns?mode=server&campaign=release");
    await page.getByRole("tab", { name: "단계 보드" }).click();
    const board = page.locator(".campaign-board");
    await expect(board.locator("[data-board-card]").first()).toBeVisible();
    const small = (root: Element) => [...root.querySelectorAll<HTMLElement>("button, input, a[href]")]
      .filter((node) => !(node as HTMLButtonElement).disabled && node.getClientRects().length)
      .map((node) => node.getBoundingClientRect()).filter((box) => Math.min(box.width, box.height) < 44).length;
    const sideways = (root: Element) => [root, ...root.querySelectorAll("*")]
      .filter((node) => ["auto", "scroll"].includes(getComputedStyle(node).overflowX) && node.scrollWidth > node.clientWidth + 1).length;
    expect(await board.evaluate(small)).toBe(0);
    expect(await board.evaluate(sideways)).toBe(0);
    await board.locator('[data-board-card="A"]').getByRole("button", { name: "다시 시도 준비" }).click();
    expect(await page.getByRole("alertdialog").evaluate(small)).toBe(0);
  });
});

test("home campaigns: campaigns under way lead, unread stages say so, and a row opens its campaign", async ({ page }, testInfo) => {
  await mockApi(page);
  const prepared = ["p1", "p2", "p3", "p4"].map((id) => {
    const value = structuredClone(serverCampaignFixtures.prepared);
    Object.assign(value.view, { campaign_id: id });
    value.definition.title = `준비 ${id}`;
    return value;
  });
  await json(page, /\/api\/server-campaigns\?.*$/, { campaigns: prepared });
  await page.route(/\/api\/server-campaigns\/p\d$/, (route) => {
    const id = new URL(route.request().url()).pathname.split("/").pop();
    const found = prepared.find((value) => value.view.campaign_id === id);
    return id === "p1" ? route.fulfill({ status: 503, json: { error: "unavailable" } }) : route.fulfill({ json: { campaign: found } });
  });
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: [CAMPAIGNS[0]], live: LIVE, limit: 100, offset: 0 });
  await page.goto("/home");

  const rows = page.getByTestId("home-campaigns").locator("li a");
  await expect(rows.first()).toContainText("열린 이슈 DAG", { timeout: 15_000 });
  await expect(rows).toHaveCount(4);
  await expect(page.getByTestId("home-campaigns").getByRole("button", { name: "1개 더 보기" })).toBeVisible();
  await expect(rows.filter({ hasText: "준비 p1" })).toContainText("단계 확인 불가", { timeout: 15_000 });
  await rows.first().click();
  await expect(page).toHaveURL(/[?&]campaign=open-issues-dag/);
  // The opened campaign: the current list row on desktop, the picker on phones.
  const panel = page.locator(".campaigns-panel");
  const opened = testInfo.project.name === "desktop" ? panel.locator('[data-campaign-key][aria-current="true"]') : panel.locator(".campaign-picker");
  await expect(opened).toContainText("열린 이슈 DAG");
});

test("home campaigns: a failed list beside an empty one reads unknown, not empty", async ({ page }) => {
  await mockApi(page);
  await page.route(/\/api\/server-campaigns\?.*$/, (route) => route.fulfill({ status: 503, json: { error: "unavailable" } }));
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: [], live: {}, limit: 100, offset: 0 });
  await page.goto("/home");
  await expect(page.getByTestId("home-campaigns")).toContainText("캠페인 목록을 읽지 못해 확인 불가예요.", { timeout: 15_000 });
});
