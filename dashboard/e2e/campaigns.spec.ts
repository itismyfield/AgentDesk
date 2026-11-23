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
      evidence: [1, 2, 3, 4, 5].map((n) => `${n} 체크포인트 시험 ${n}회차 통과`.repeat(n === 5 ? 12 : 1)),
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

/** Enabled controls under 44px on either side, and fields typed in under 16px, inside `root`. */
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
  // The campaign's card splits its bar by task status, each part named with its count.
  await expect(panel.getByRole("img", { name: "상태별 작업: 완료 1, 진행 중 2, 막힘 1, 대기 3, 건너뜀 1" })).toBeVisible();
  await expect(panel.locator(".campaign-detail-status")).toHaveText("진행 중 · 2개 진행, 1개 막힘");

  const attention = panel.getByRole("region", { name: "주의" });
  await expect(attention.locator('[data-task-id="i7336"] .campaign-blocker')).toContainText("tailscale 인증이 만료");
  const running = panel.locator('[data-task-id="i7272"]');
  await expect(running).toContainText("리뷰 진행 중");
  await expect(running).toContainText("끝나면: 배포 중 끊긴 턴을 다시 시작하지 않아도 됩니다.");
  // The card reads the step, owner and session; the coordinator's stage note stays in the detail.
  await expect(running.locator(".campaign-task-progress")).toHaveText("진행 중 · 리뷰");
  await expect(running.locator(".campaign-task-foot")).toContainText(/세션 작업 중.*담당 아다 대시보드/);
  await expect(running).not.toContainText("재시작 프로토콜 리뷰 2회차");
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

test("campaign tab: a task opens in the pinned pane on wide screens and as its own screen on phones", async ({ page }, testInfo) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  const pane = page.getByRole("complementary", { name: "선택 작업" });
  await expect(pane).toHaveCount(0);
  const card = panel.locator('[data-task-id="i7272"]');
  await card.click();
  await expect(page).toHaveURL(/[?&]node=i7272/);
  await expect(card).toHaveAttribute("aria-current", "true");
  const detail = page.locator(".campaign-node-detail");
  await expect(page.getByRole("heading", { name: /무중단 재시작/ })).toBeVisible();
  // TaskPhone order: the stage record reads oldest first; only the newest three evidence lines show at first.
  await expect(detail.locator("ol > li")).toHaveText([/설계 리뷰 CLEAN/, /리뷰 2회차 시작/]);
  const evidence = detail.getByRole("region", { name: "검증 근거" });
  await expect(evidence.locator("li")).toHaveText([/^5 /, /^4 /, /^3 /]);
  await evidence.getByRole("button", { name: "2개 더 보기" }).click();
  await expect(evidence.locator("li")).toHaveCount(5);
  await expectNoHorizontalOverflow(page);
  if (testInfo.project.name === "desktop") {
    const box = (await pane.boundingBox())!;
    expect(box.x + box.width).toBeGreaterThan(1440 - 4);
    expect((await card.boundingBox())!.x + (await card.boundingBox())!.width).toBeLessThanOrEqual(box.x);
    // Non-modal: the overview beside it still works.
    await panel.locator('[data-task-id="i7336"]').click();
    await expect(pane.getByRole("heading", { name: /꺼진 맥의 채널 메시지/ })).toBeVisible();
    await page.getByRole("button", { name: "상세 닫기" }).click();
  } else {
    const screen = page.getByRole("dialog", { name: /무중단 재시작/ });
    expect(await screen.boundingBox()).toEqual({ x: 0, y: 0, width: 390, height: 844 });
    await expect(screen.locator("header")).toContainText("열린 이슈 DAG · #7272");
    // The browser's Back leaves the task, not the page; the screen's own back button does the same.
    await page.goBack();
    await expect(screen).toHaveCount(0);
    await expect(page).toHaveURL(/\/campaigns\?mode=legacy&campaign=open-issues-dag$/);
    await card.click();
    await page.getByRole("button", { name: "목록으로" }).click();
  }
  await expect(detail).toHaveCount(0);
  await expect(page).not.toHaveURL(/[?&]node=/);
  await expect(pane).toHaveCount(0);
});

test("phone: a quick second tap on back, or a reload with the task open, still leaves one Back to the page before", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name === "desktop", "Phone task screen");
  await mockApi(page);
  await page.goto("/home");
  await page.goto("/campaigns?mode=legacy&campaign=open-issues-dag");
  const card = page.locator('[data-task-id="i7272"]');
  const back = page.getByRole("button", { name: "목록으로" });
  await card.click();
  await back.dblclick();
  await expect(page).toHaveURL(/\/campaigns\?mode=legacy&campaign=open-issues-dag$/);
  await card.click();
  await page.reload();
  await back.click();
  await expect(page).toHaveURL(/\/campaigns\?mode=legacy&campaign=open-issues-dag$/);
  await page.goBack();
  await expect(page).toHaveURL(/\/home$/);
});

test("campaign tab: closing a task returns focus to the card, list row or connections row that opened it", async ({ page }, testInfo) => {
  await openCampaigns(page);
  const panel = page.locator(".campaigns-panel");
  const closeButton = page.getByRole("button", { name: testInfo.project.name === "desktop" ? "상세 닫기" : "목록으로" });
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
  const desktop = testInfo.project.name === "desktop";
  if (desktop) {
    await panel.locator(".react-flow__node").filter({ hasText: "Relay" }).first().click();
    await panel.getByRole("button", { name: "그룹 열기" }).click();
  } else await panel.locator('[data-group="Relay"]').click();
  // Wide screens open the task from its card in the group's graph, phones from its row in the group's list.
  const relayRow = panel.locator(desktop ? '.react-flow__node[data-task-id="i7340"]' : '.campaign-relations .campaign-task-row[data-task-id="i7340"]');
  for (const close of [() => closeButton.click(), escape]) {
    await relayRow.click();
    if (desktop) await panel.getByRole("button", { name: "작업 상세 열기" }).click();
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
  // Stages before the server's current one are done; the current one says how long the gate has waited.
  const stages = page.getByRole("region", { name: "단계" });
  await expect(stages.locator("li")).toHaveText([/요구사항/, /^끝남설계$/, /설계 리뷰/, /구현/, /리뷰/, /착지승인 대기 23분/, /main 확인 · 최종 확인 · 완료/]);
  await expect(stages.locator('[aria-current="step"]')).toContainText("착지");
  await page.getByRole("button", { name: "승인 검토" }).click();
  const sheet = page.getByRole("dialog", { name: "착지 승인" });
  await expect(sheet).toContainText("11월 18일 오후 04:37"); // the dashboard's language in KST, whatever the browser's locale
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
    await page.locator(".campaign-find").click(); // the search field it opens follows the same rule
    await expect(page.getByRole("searchbox", { name: "작업 찾기" })).toBeVisible();
    const tab = page.locator("#campaign-panel-campaigns");
    expect(await tab.evaluate(small)).toEqual([]);
    expect(await tab.evaluate(tiny)).toEqual([]);

    // The task screen and its edit form follow the same rule.
    await page.locator('[data-task-id="i7272"]').click();
    const sheet = page.getByRole("dialog", { name: /무중단 재시작/ });
    expect(await sheet.evaluate(small)).toEqual([]);
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
  if (phone) {
    expect(await openGroups(page)).toEqual([true, true, true, false, false, false]);
    // Group headers are kit rows; the board's element rules must not draw a box around them.
    expect(await page.locator(".campaign-board section > button[aria-expanded]").evaluateAll((nodes) => nodes.map((node) => getComputedStyle(node).borderTopWidth))).toEqual(Array(6).fill("0px"));
  }
  for (const [column, id, chip] of [["막힘", "i7336", "막힘"], ["실패", "i7350", "실패"]]) {
    const card = board.getByRole("region", { name: column }).locator(`[data-board-card="${id}"]`);
    await expect(card.locator("[data-status-chip]")).toHaveText(chip);
    await expect(card.locator(".campaign-live")).toHaveText("리뷰 진행 중");
  }
  // A running task with a blocker keeps its own column and word; only its frame and reason say it is stuck.
  const stuck = board.getByRole("region", { name: "진행 중" }).locator('[data-board-card="i7360"]');
  await expect(stuck.locator("[data-status-chip]")).toHaveText("진행 중");
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
    await expect(card("F").locator("[data-status-chip]")).toHaveText("사용자 중단");
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

/** A ledger whose Relay and Cluster groups loop, with a tie saved on the group graph over both lines into Release. */
async function openRelations(page: Page, theme: "dark" | "light" = "dark") {
  const value = campaign("relations-dag", "관계도 시험", [
    node("g1", "#7401 게이트웨이 재연결", { status: "completed" }), node("g2", "#7402 헬스 체크 간격", { status: "running", dependencies: ["g1"] }),
    node("r1", "#7340 재전송 큐 지표", { group: "Relay", dependencies: ["g1", "r3"] }), node("r2", "#7403 세션 매핑 유지", { group: "Relay", status: "blocked", dependencies: ["r1"], blocker: "릴리스 승인을 기다려요" }),
    node("c1", "#7336 채널 이어받기", { group: "Cluster", dependencies: ["r1"] }), node("r3", "#7404 로그 압축", { group: "Relay", dependencies: ["c1"] }),
    node("rel1", "#7500 릴리스 노트", { group: "Release", dependencies: ["r2", "c1"] }), node("i1", "#7510 배포 경로", { group: "Infra", dependencies: ["g2"] }),
    node("d1", "#7520 문서 정리", { group: "Docs", status: "completed" }), node("a1", "#7530 감사 후속", { group: "Audit", status: "completed" }), node("q1", "#7540 대기열 표시", { group: "Queue" }),
  ]);
  const scope = { mode: "legacy", campaignId: value.id, projection: "groups", taskScope: "all" }, edges = [["r2", "rel1"], ["c1", "rel1"]];
  const tie = { schemaVersion: 1, scope, ties: [{ id: "t1", position: { x: 0, y: 0 }, strandOrder: edges }], routes: edges.map((edge) => ({ edge, via: ["t1"] })), showOriginal: false, revision: 7 };
  const calls = await mockApi(page, theme);
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: [value], live: {}, limit: 100, offset: 0 });
  await json(page, /\/api\/campaigns\/relations-dag$/, { campaign: value, live: {} });
  await page.addInitScript(([key, stored]) => window.localStorage.setItem(key, stored), [`agentdesk.campaign-graph-view.v1:${JSON.stringify(["legacy", value.id, "groups", "all"])}`, JSON.stringify(tie)]);
  await page.goto(`/campaigns?mode=legacy&campaign=${value.id}`);
  await page.getByRole("tab", { name: "관계도" }).click();
  return { view: page.locator(".campaign-relations"), calls };
}
/** With CAMPAIGNS_SHOTS set, the relations tab in dark and light at the page's size. */
async function relationsShots(page: Page, name: string) {
  for (const theme of SHOTS ? ["dark", "light"] as const : []) {
    await page.unrouteAll({ behavior: "ignoreErrors" });
    await openRelations(page, theme);
    await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
    await page.waitForTimeout(500);
    await page.screenshot({ path: `${SHOTS}/relations-${name}-${theme}.png` });
    const zoom = page.getByRole("button", { name: "확대" });
    if (await zoom.isVisible()) {
      await zoom.click(); await zoom.click(); await page.waitForTimeout(400); await page.screenshot({ path: `${SHOTS}/relations-${name}-${theme}-zoomed.png` });
      await page.getByRole("button", { name: "화면 맞춤" }).click();
      await page.locator('.react-flow__node[data-id="campaign-group:Relay"]').dblclick();
      await page.locator('.react-flow__node[data-id="r2"]').click();
    } else { await page.locator('[data-group="Relay"]').click(); await page.locator("[data-relations-list]").evaluate((list) => list.scrollIntoView()); }
    await page.waitForTimeout(500);
    await page.screenshot({ path: `${SHOTS}/relations-${name}-${theme}-group.png` });
  }
}

test("relations view: group cards by dependency depth, a dashed loop, the saved tie, an inspector a group is chosen into with the keyboard, and zoom tools", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "The canvas is for wide screens");
  await page.clock.install({ time: new Date(NOW) });
  await page.setViewportSize({ width: 1440, height: 980 });
  const { view, calls } = await openRelations(page);
  const card = (key: string) => view.locator(`.react-flow__node[data-id="campaign-group:${key}"]`);
  await expect(view.locator('.react-flow__node[data-id^="campaign-group:"]')).toHaveCount(8);
  await expect(view.locator('.react-flow__node[data-id^="campaign-tie:"]')).toHaveCount(1);
  await expect(view.locator("[data-relations-canvas]").getByText("순환 · Cluster → Relay → Cluster")).toBeVisible();
  const left = async (key: string) => (await card(key).boundingBox())!.x;
  expect([await left("Gateway") < await left("Relay"), await left("Relay") < await left("Release")]).toEqual([true, true]);
  await expect(view.getByRole("complementary", { name: "캠페인 요약" })).toContainText("그룹 8개 · 작업 11개");
  // Fitted past 75% the cards wear covers; zooming in drops them, and Tab walks the drawn columns then the shelf, outlined near and far.
  const covers = view.locator("[data-far]"), tools = view.getByRole("toolbar", { name: "보기 도구" });
  const walk = async (count = 8) => {
    await view.locator(".react-flow__node").first().focus();
    await page.keyboard.press("Shift+Tab");
    const seen = [];
    for (let step = 0; step < count; step += 1) { await page.keyboard.press("Tab"); seen.push(await page.evaluate(() => [document.activeElement?.getAttribute("data-id"), getComputedStyle(document.activeElement!.firstElementChild!).outlineStyle])); }
    return seen;
  };
  await expect(covers).toHaveCount(8);
  for (const zoom of ["확대", "확대"]) await tools.getByRole("button", { name: zoom }).click();
  await expect(covers).toHaveCount(0);
  expect(await walk()).toEqual(["Cluster", "Gateway", "Relay", "Infra", "Release", "Audit", "Docs", "Queue"].map((key) => [`campaign-group:${key}`, "solid"]));
  for (const zoom of ["축소", "축소", "축소"]) await tools.getByRole("button", { name: zoom }).click();
  await expect(covers).toHaveCount(8);
  expect((await walk()).map(([, outline]) => outline)).toEqual(Array(8).fill("solid"));
  // A double-click on a card not yet chosen opens it, and back returns the focus to that card.
  await card("Gateway").dblclick();
  await expect(view.getByRole("navigation", { name: "이동 줄" })).toHaveText("전체 그룹›Gateway");
  await view.locator("[data-relations-back]").click();
  await expect(card("Gateway")).toBeFocused();
  await card("Relay").focus();
  await page.keyboard.press("Enter");
  const aside = view.getByRole("complementary", { name: "선택 그룹" });
  await expect(aside.getByRole("list", { name: "막힌 작업" })).toContainText("#7403 세션 매핑 유지");
  await expect(aside.getByRole("list", { name: "다음 그룹" })).toContainText("Release의존성 1");
  await expect(view.getByRole("toolbar", { name: "보기 도구" }).getByRole("button")).toHaveCount(3);
  // The opened group's tasks take the canvas, with faint tails for the work outside it on either side.
  await aside.getByRole("button", { name: "그룹 열기" }).click();
  await expect(view.getByRole("navigation", { name: "이동 줄" })).toHaveText("전체 그룹›Relay");
  await expect(view.locator("[data-relations-back]")).toBeFocused();
  const task = (id: string) => view.locator(`.react-flow__node[data-id="${id}"]`);
  await expect(view.locator(".react-flow__node[data-task-id]")).toHaveCount(3);
  expect(await view.locator('.react-flow__node[data-id^="tail:"]').evaluateAll((tails) => tails.map((tail) => tail.getAttribute("data-id")).sort())).toEqual(["tail:c1", "tail:g1", "tail:rel1"]);
  expect([(await task("tail:g1").boundingBox())!.x < (await task("r1").boundingBox())!.x, (await task("tail:rel1").boundingBox())!.x > (await task("r2").boundingBox())!.x]).toEqual([true, true]);
  // c1 comes before and after Relay, so r1 → c1 → r3 → r1 loops through its tail: every line is drawn and the loop labelled once.
  await expect(view.locator(".react-flow__edge")).toHaveCount(6);
  await expect(view.locator("[data-relations-canvas]").getByText(/^순환 · /)).toHaveCount(1);
  expect(await walk(6)).toEqual(["tail:g1", "tail:c1", "r1", "r2", "r3", "tail:rel1"].map((id) => [id, "solid"]));
  await task("r2").focus();
  await page.keyboard.press("Enter");
  const picked = view.getByRole("complementary", { name: "선택 작업" });
  await expect(picked).toContainText("막힘 사유릴리스 승인을 기다려요");
  await expect(picked.getByRole("list", { name: "후행 작업" })).toContainText("릴리스 노트");
  await picked.getByRole("button", { name: "작업 상세 열기" }).click();
  await expect(page.locator(".campaign-node-detail")).toBeVisible();
  await page.getByRole("button", { name: "상세 닫기" }).click();
  await expect(task("r2")).toBeFocused();
  // A tail goes to its own group with that task chosen, and 편집 opens the editor on that group.
  await task("tail:rel1").click();
  await expect(view.getByRole("navigation", { name: "이동 줄" })).toHaveText("전체 그룹›Release");
  await expect(view.getByRole("complementary", { name: "선택 작업" })).toContainText("릴리스 노트");
  await page.getByRole("button", { name: "편집", exact: true }).click();
  await expect(page).toHaveURL(/edit=relations&scope=group%3ARelease/);
  expect(calls.filter((call) => !call.startsWith("GET") && !call.startsWith("POST /api/auth/ws-ticket"))).toEqual([]);
  await relationsShots(page, "1440x980");
});

test.describe("relations at 390px touch", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });
  test("relations view: phones list the groups under the loop warning, open a group's tasks, with no sideways scroll and 44px targets", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "Phone width");
    await page.clock.install({ time: new Date(NOW) });
    const { view } = await openRelations(page);
    await expect(view.locator(".react-flow")).toHaveCount(0);
    await expect(view.getByRole("list", { name: "그룹 목록" }).getByRole("button")).toHaveCount(8);
    await expect(view.getByText("순환 · Cluster → Relay → Cluster")).toBeVisible();
    await view.locator('[data-group="Relay"]').click();
    await expect(view.locator('.campaign-task-row[data-task-id="r2"]')).toContainText("선행 1 · 후행 1");
    await expectNoHorizontalOverflow(page);
    expect([await view.evaluate(small), await view.evaluate(tiny)]).toEqual([[], []]);
    await relationsShots(page, "390x844");
  });
});

test("relations editor: opens from the relations tab, edits dependencies in the inspector, blocks a cycle, saves on the newest copy and asks before leaving", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "The editor is for wide screens");
  await openCampaigns(page);
  let current = { ...CAMPAIGNS[0] };
  const puts: { expected_revision: number; nodes: { id: string; dependencies: string[] }[] }[] = [];
  await page.route(/\/api\/campaigns\/open-issues-dag$/, (route) => {
    if (route.request().method() !== "PUT") return route.fulfill({ json: { campaign: current, live: LIVE["open-issues-dag"] } });
    puts.push(route.request().postDataJSON());
    // The first write meets a newer revision; the second lands.
    current = { ...current, revision: current.revision + 1, ...puts.length > 1 ? { nodes: puts.at(-1)!.nodes } : {} };
    return puts.length > 1 ? route.fulfill({ json: { campaign: current } }) : route.fulfill({ status: 409, json: { error: "revision conflict" } });
  });
  const panel = page.locator(".campaigns-panel");
  await panel.getByRole("tab", { name: "관계도" }).click();
  await panel.getByRole("button", { name: "편집", exact: true }).click();
  await expect(page).toHaveURL(/edit=relations&scope=group%3AGateway/);
  const editor = page.locator(".campaign-relations-editor");
  const task = (id: string) => editor.locator(`.react-flow__node[data-id="${id}"]`);
  await expect(editor.locator(".react-flow__node")).toHaveCount(3);
  // The editor fills the window: no rail or top bar, so the first Tab lands on its way back.
  await expect(page.getByTestId("app-sidebar-nav")).toHaveCount(0);
  await expect(page.getByTestId("topbar")).toHaveCount(0);
  expect(await editor.boundingBox()).toEqual({ x: 0, y: 0, width: 1440, height: 900 });
  await page.keyboard.press("Tab");
  await expect(editor.getByRole("button", { name: "캠페인", exact: true })).toBeFocused();
  await expect(editor.getByRole("complementary", { name: "범위 요약" })).toContainText("그룹 · Gateway");
  await task("i7401").click();
  await expect(editor.getByRole("complementary")).toContainText("완료 · 바꿀 수 없음");
  await expect(editor.getByLabel("선행 추가")).toHaveCount(0);
  for (const [id, dependency] of [["i7402", "i7401"], ["i7272", "i7402"], ["i7402", "i7272"]]) {
    await task(id).click();
    await editor.getByLabel("선행 추가").selectOption(dependency);
  }
  await expect(editor.getByRole("alert").first()).toContainText("순환: #7272 → #7402 → #7272");
  const save = editor.getByRole("button", { name: "저장", exact: true });
  await expect(save).toBeDisabled();
  await editor.getByRole("button", { name: /^선행 #7272 .+ 빼기$/ }).click();
  await expect(editor.getByText("저장 안 됨 · 선행 2")).toBeVisible();
  // With nothing chosen the column lists what changed, and a row opens its task.
  await editor.locator("[data-editor-canvas]").click({ position: { x: 40, y: 300 } });
  const changed = editor.getByRole("complementary", { name: "범위 요약" }).getByRole("button");
  await expect(changed).toHaveText([/^#7402 .+선행$/, /^#7272 .+선행$/]);
  await changed.first().click();
  await expect(editor.getByRole("complementary")).toContainText("#7402 · Gateway");
  await save.click();
  await expect(editor.getByText(/^저장 안 됨/)).toHaveCount(0);
  expect(puts.map((body) => body.expected_revision)).toEqual([7, 8]);
  const deps = (id: string) => puts[1].nodes.find((value) => value.id === id)?.dependencies;
  expect([deps("i7402"), deps("i7272"), deps("i7401")]).toEqual([["i7401"], ["i7402"], []]);
  if (SHOTS) await page.screenshot({ path: `${SHOTS}/relations-editor-fixture-1440x900-dark.png` });

  // Keyboard only: Enter on a focused task opens it, Enter on a chip's button removes that dependency.
  // Each Enter waits for the one before it to land, so the graph has taken the new selection before the next key.
  for (const id of ["7272", "7402"]) { await task(`i${id}`).focus(); await page.keyboard.press("Enter"); await expect(editor.getByRole("complementary")).toContainText(`#${id} · Gateway`); }
  await editor.getByRole("button", { name: /^선행 #7401 .+ 빼기$/ }).focus();
  await page.keyboard.press("Enter");
  await page.goBack();
  const dialog = page.getByRole("dialog", { name: "저장하지 않은 변경이 1건 있어요" });
  await expect(dialog).toBeVisible();
  await expect(page).toHaveURL(/edit=relations/);
  await dialog.getByRole("button", { name: "계속 편집" }).click();
  await editor.getByRole("button", { name: "캠페인", exact: true }).click();
  await expect(dialog).toBeVisible();
  await dialog.getByRole("button", { name: "버리고 나가기" }).click();
  await expect(editor).toHaveCount(0);
  await expect(page).not.toHaveURL(/edit=relations/);
  await expect(page.getByTestId("app-sidebar-nav")).toBeVisible();
});

test("relations editor: phones get no way in, a direct link only shows relations, and another campaign picked there opens no editor once widened", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "mobile", "Phone width");
  await openCampaigns(page);
  await page.locator(".campaigns-panel").getByRole("tab", { name: "관계도" }).click();
  await expect(page.locator(".campaign-relations")).toBeVisible();
  await expect(page.getByRole("button", { name: "편집", exact: true })).toHaveCount(0);
  await page.goto("/campaigns?mode=legacy&campaign=open-issues-dag&edit=relations&scope=group%3AGateway");
  await expect(page.getByText("관계도 편집은 900px 넘는 화면에서 열려요. 여기서는 보기만 해요.")).toBeVisible();
  await expect(page.locator(".campaign-relations-editor")).toHaveCount(0);
  await expectNoHorizontalOverflow(page);
  await page.locator(".campaign-picker").click();
  const sheet = page.getByRole("dialog", { name: "캠페인 고르기" });
  await sheet.getByRole("radio", { name: /준비/ }).click();
  await sheet.locator('[data-campaign-key="legacy:release-train"]').click();
  await page.setViewportSize({ width: 1440, height: 900 });
  await expect(page.locator(".campaign-detail-status")).toHaveText("멈춤 · 진행 중인 작업 없음");
  await expect(page.locator(".campaign-relations-editor")).toHaveCount(0);
  await expect(page).not.toHaveURL(/edit=relations/);
});

/** Opens the editor on Gateway with #7401 → #7402 → #7272 and #7401 → #7272 linked, and helpers for its lines and ties. */
async function openLinkedEditor(page: Page) {
  const linked = { ...CAMPAIGNS[0], nodes: CAMPAIGNS[0].nodes.map((value) => ({ ...value, dependencies: ({ i7402: ["i7401"], i7272: ["i7401", "i7402"] } as Record<string, string[]>)[value.id] ?? [] })) };
  await page.clock.install({ time: new Date(NOW) });
  const calls = await mockApi(page);
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: [linked, CAMPAIGNS[1]], live: LIVE, limit: 100, offset: 0 });
  await json(page, /\/api\/campaigns\/open-issues-dag$/, { campaign: linked, live: LIVE["open-issues-dag"] });
  await page.goto("/campaigns?mode=legacy&campaign=open-issues-dag&edit=relations&scope=group%3AGateway");
  const editor = page.locator(".campaign-relations-editor");
  const line = (id: string) => editor.locator(`.react-flow__edge[data-id="${id}"]`);
  await expect(editor.locator(".react-flow__edge")).toHaveCount(3);
  return {
    calls, editor, line, tie: editor.locator('.react-flow__node[data-id^="cable-tie:"]'),
    lines: () => editor.locator(".react-flow__edge").evaluateAll((all) => all.map((value) => value.getAttribute("data-id")).filter((id) => !id!.includes("#")).sort()),
    /** A point on a line's drawn path, `share` of the way along, in page pixels. */
    pointOn: (id: string, share = 0.5) => line(id).locator(".react-flow__edge-path").evaluate((path: SVGPathElement, at) => {
      const point = path.getPointAtLength(path.getTotalLength() * at), m = path.getScreenCTM()!;
      return { x: point.x * m.a + m.e, y: point.y * m.d + m.f };
    }, share),
  };
}

test("relations editor: cable ties bundle lines, move on the grid and untie without touching a dependency, and save in this browser alone", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "The editor is for wide screens");
  const { calls, editor, line, tie, lines, pointOn } = await openLinkedEditor(page);
  await editor.getByRole("button", { name: "케이블타이", exact: true }).click();
  for (const id of ["i7401->i7272", "i7402->i7272"]) { await line(id).focus(); await page.keyboard.press("Enter"); }
  await expect(editor.getByText("선 2개를 골랐어요.")).toBeVisible();
  await page.keyboard.press("b");
  await expect(tie).toHaveText("2");
  await expect(editor.getByText("저장 안 됨 · 케이블타이 1")).toBeVisible();
  // The new tie holds the focus, so H previews its lines and Escape closes its menu straight away.
  await expect(tie).toBeFocused();
  await page.keyboard.press("h");
  await expect(editor.locator('.react-flow__edge[data-id^="preview:"]')).toHaveCount(2);
  await page.keyboard.press("Escape");
  await expect(editor.getByRole("group", { name: "케이블타이 메뉴" })).toHaveCount(0);
  // A drag lands on the 20px grid; Delete then unties it and leaves every dependency as it was.
  const at = async () => (await tie.evaluate((node) => getComputedStyle(node).transform)).match(/-?[\d.]+/g)!.slice(4).map(Number);
  const [before, box] = [await at(), (await tie.boundingBox())!];
  await page.mouse.move(box.x + 22, box.y + 22); await page.mouse.down(); await page.mouse.move(box.x + 140, box.y + 97, { steps: 8 }); await page.mouse.up();
  const after = await at();
  expect([after[0] % 20, after[1] % 20, after[0] > before[0] && after[1] > before[1]]).toEqual([0, 0, true]);
  await page.keyboard.press("Delete");
  await expect(tie).toHaveCount(0);
  await expect(editor.getByText(/^저장 안 됨/)).toHaveCount(0);
  expect(await lines()).toEqual(["i7401->i7272", "i7401->i7402", "i7402->i7272"]);
  // One line double-clicked is tied where it was clicked; saving writes the browser only, and a reload draws it again.
  const middle = await pointOn("i7401->i7402");
  await page.mouse.dblclick(middle.x, middle.y);
  await expect(tie).toHaveText("1");
  await editor.getByRole("button", { name: "저장", exact: true }).click();
  await expect(editor.getByText(/^저장 안 됨/)).toHaveCount(0);
  expect(calls.filter((call) => call.startsWith("PUT"))).toEqual([]);
  const saved = await page.evaluate(() => Object.entries(localStorage).filter(([key]) => key.startsWith("agentdesk.campaign-graph-view.v1:")).map(([, raw]) => JSON.parse(raw)));
  expect(saved.map((value) => [value.scope.taskScope, value.revision, value.ties[0].strandOrder])).toEqual([["group:Gateway", 7, [["i7401", "i7402"]]]]);
  await page.reload();
  await expect(tie).toHaveText("1");
  expect(await lines()).toEqual(["i7401->i7272", "i7401->i7402", "i7402->i7272"]);
});

test("relations editor: below 1280px a double-click on a line where the inspector opens ties it, and a single click opens the inspector once no second click comes", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "The editor is for wide screens");
  // The canvas spans the whole window now, so a narrow one keeps the line under the overlay.
  await page.setViewportSize({ width: 920, height: 900 });
  const { calls, editor, tie, lines, pointOn } = await openLinkedEditor(page);
  const canvas = (await editor.locator("[data-editor-canvas]").boundingBox())!;
  // A point of #7402 → #7272 inside the 320px the overlay inspector covers.
  const under = await pointOn("i7402->i7272", 0.8);
  expect(under.x).toBeGreaterThan(canvas.x + canvas.width - 320);
  await page.mouse.dblclick(under.x, under.y);
  await expect(tie).toHaveText("1");
  await page.waitForTimeout(500); // past the double-click window: no inspector comes after the tie
  await expect(editor.getByRole("complementary")).toHaveCount(0);
  expect(await lines()).toEqual(["i7401->i7272", "i7401->i7402", "i7402->i7272"]);
  const single = await pointOn("i7401->i7402");
  await page.mouse.click(single.x, single.y);
  await expect(editor.getByRole("complementary", { name: "선행 연결" })).toBeVisible();
  expect(calls.filter((call) => call.startsWith("PUT"))).toEqual([]);
});

test("relations editor: a draft kept while another page shows asks before a reload, and stops once discarded", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "The editor is for wide screens");
  await openCampaigns(page);
  const [panel, editor, asked] = [page.locator(".campaigns-panel"), page.locator(".campaign-relations-editor"), [] as string[]];
  const open = async (name: string) => { await panel.getByRole("tab", { name: "관계도" }).click(); await panel.getByRole("button", { name, exact: true }).click(); };
  const reload = async (home = true) => {
    // The full-window editor has no rail, so the home page is reached with its shortcut.
    if (home) { await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur()); await page.keyboard.press("Alt+1"); await expect(page).toHaveURL(/\/home/); }
    page.once("dialog", (dialog) => { asked.push(dialog.type()); void dialog.dismiss(); });
    await page.reload({ timeout: 5_000 }).catch(() => undefined);
  };
  await open("편집");
  await editor.locator('.react-flow__node[data-id="i7402"]').click(); await editor.getByLabel("선행 추가").selectOption("i7401");
  await reload();
  expect(asked).toEqual(["beforeunload"]);
  await page.getByRole("link", { name: "캠페인" }).click();
  await open("이어서 편집");
  await expect(editor.getByText("저장하지 않은 편집을 다시 불러왔어요.")).toBeVisible();
  // Right after the draft comes back the editor itself still asks, and a cancelled reload keeps the edit.
  await reload(false);
  await expect(editor.getByText("저장 안 됨 · 선행 1")).toBeVisible();
  await editor.getByRole("button", { name: "버리기", exact: true }).click();
  await reload();
  expect(asked).toEqual(["beforeunload", "beforeunload"]);
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

test.describe("home kit contract at 390px touch", () => {
  test.use({ viewport: { width: 390, height: 844 }, hasTouch: true, isMobile: true });

  test("home campaigns: row spacing overrides win, coarse pointer pins todo min-height to 44px, and chip contrast meets 4.5 in dark and light", async ({ page }, testInfo) => {
    test.skip(testInfo.project.name === "desktop", "390px touch contract");
    const routine = (id: string, status: string) => ({
      id, agent_id: "agent-ada", script_ref: `${id}.js`, name: id, status: "enabled", execution_strategy: "fresh", schedule: "0 9 * * *",
      next_due_at: null, last_run_at: NOW, last_result: null, checkpoint: null, discord_thread_id: null, timeout_secs: 600, in_flight_run_id: null,
      last_run: { id: `run-${id}`, routine_id: id, status, started_at: NOW, finished_at: NOW }, created_at: NOW, updated_at: NOW,
    });
    for (const theme of ["dark", "light"] as const) {
      await page.unrouteAll({ behavior: "ignoreErrors" });
      await page.clock.setFixedTime(new Date(NOW));
      await mockApi(page, theme);
      await json(page, /\/api\/kanban-cards(?:\?.*)?$/, { cards: [{
        id: "c1", title: "차단 카드", description: null, status: "blocked", github_repo: "itismyfield/AgentDesk", owner_agent_id: null,
        requester_agent_id: null, assignee_agent_id: "agent-ada", parent_card_id: null, latest_dispatch_id: null, sort_order: 0, priority: "high",
        depth: 0, blocked_reason: "승인 필요", review_notes: null, github_issue_number: 1, github_issue_url: issue(1), metadata_json: null,
        pipeline_stage_id: null, review_status: null, created_at: 1_790_000_000_000, updated_at: 1_790_000_000_000, started_at: null, requested_at: null, completed_at: null,
      }] });
      await json(page, /\/api\/routines(?:\?.*)?$/, { routines: [routine("r-ok", "succeeded"), routine("r-info", "running"), routine("r-warn", "paused"), routine("r-err", "failed")] });
      await page.goto("/home");
      await expect(page.getByTestId("home-campaigns").locator("li a").first()).toBeVisible({ timeout: 15_000 });
      await expect(page.getByTestId("home-todo").locator("li a").first()).toBeVisible({ timeout: 15_000 });
      await expect(page.getByTestId("home-routines").locator("li")).toHaveCount(4, { timeout: 15_000 });
      const metrics = await page.evaluate(() => {
        const linear = (c: number) => { const v = c / 255; return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4; };
        const lum = (cssColor: string) => {
          const canvas = document.createElement("canvas"); canvas.width = canvas.height = 1;
          const ctx = canvas.getContext("2d")!; ctx.fillStyle = cssColor; ctx.fillRect(0, 0, 1, 1);
          const [r, g, b] = ctx.getImageData(0, 0, 1, 1).data;
          return 0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b);
        };
        const ratio = (node: Element) => {
          const cs = getComputedStyle(node);
          const [hi, lo] = [lum(cs.color), lum(cs.backgroundColor)].sort((a, b) => b - a);
          return (hi + 0.05) / (lo + 0.05);
        };
        const camp = getComputedStyle(document.querySelector('[data-testid="home-campaigns"] li a')!);
        const todo = getComputedStyle(document.querySelector('[data-testid="home-todo"] li a')!);
        const chips = Array.from(document.querySelectorAll('[data-testid="home-routines"] li > span:last-child'), ratio);
        return { paddingTop: camp.paddingTop, rowGap: camp.rowGap, todoMinHeight: todo.minHeight, minChipContrast: Math.min(...chips) };
      });
      expect(metrics.paddingTop, theme).toBe("10px");
      expect(metrics.rowGap, theme).toBe("6px");
      expect(metrics.todoMinHeight, theme).toBe("44px");
      expect(metrics.minChipContrast, theme).toBeGreaterThanOrEqual(4.5);
    }
  });
});
