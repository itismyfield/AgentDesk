import { expect, test, type Locator, type Page } from "@playwright/test";

/** Card detail: the activity list (dispatches, transitions, GitHub comments) and the turn transcript laid out as a run. */
const SHOTS = process.env.CARD_DETAIL_SHOTS;
const NOW_MS = Date.parse("2026-11-17T10:30:00+09:00");
const MIN = 60_000;
const HOUR = 60 * MIN;
const iso = (ms: number) => new Date(ms).toISOString();

test.use({ timezoneId: "Asia/Seoul", locale: "ko-KR" });

const agent = (id: string, name: string, nameKo: string) => ({
  id, role_id: id, name, alias: null, name_ko: nameKo, name_ja: null, name_zh: null, department_id: null, cli_provider: "codex",
  avatar_emoji: "🛠️", sprite_number: 3, personality: null, status: "idle", current_task_id: null, workflow_pack_key: "development",
  stats_tasks_done: 0, stats_tokens: 0, activity_source: "agentdesk", created_at: 1_790_000_000_000,
});
const AGENTS = [agent("agent-ada", "Ada Dashboard", "아다 대시보드"), agent("agent-luna", "Luna Ops", "루나 옵스")];
const issue = (number: number) => `https://github.com/itismyfield/AgentDesk/issues/${number}`;

const ISSUE_BODY = [
  "## 배경",
  "재전송 큐가 세션 복구와 겹치면 같은 메시지가 두 번 나갑니다. #agent-ops에서 3건 재현했습니다.",
  "## 내용",
  "- 재시도 간격을 큐 틱보다 길게 맞추기\n- 세션 복구 뒤 릴레이 워처를 다시 붙이기",
  "## DoD",
  "- [x] 재전송 50회 중 중복 0회\n- [ ] 세션 ses-412 복구 뒤 워처 재부착 확인\n- [ ] agentdesk-relay-regression-transcript-2026-11-17-final-with-a-very-long-identifier.json 첨부",
  "## 의존성",
  "#7272 무중단 재시작 체크포인트",
  "## 리스크",
  "Discord 429가 겹치면 지연이 늘 수 있음",
].join("\n");

const card = (id: string, number: number | null, title: string, patch: Record<string, unknown> = {}) => ({
  id, title, description: null, status: "backlog", github_repo: "itismyfield/AgentDesk", owner_agent_id: null, requester_agent_id: null,
  assignee_agent_id: null, parent_card_id: null, latest_dispatch_id: null, sort_order: 0, priority: "medium", depth: 0, blocked_reason: null,
  review_notes: null, github_issue_number: number, github_issue_url: number ? issue(number) : null, metadata_json: null, pipeline_stage_id: null,
  review_status: null, created_at: NOW_MS - 30 * HOUR, updated_at: NOW_MS - 40 * MIN, started_at: null, requested_at: null, completed_at: null, ...patch,
});
const CARDS = [
  card("card-8001", 8001, "릴레이 재전송 큐에서 중복 전송을 막고 세션 복구 뒤 워처를 다시 붙이기", {
    status: "in_progress", priority: "high", assignee_agent_id: "agent-ada", latest_dispatch_id: "d-impl-0002", latest_dispatch_status: "in_progress",
    description: ISSUE_BODY, requested_at: NOW_MS - 26 * HOUR, started_at: NOW_MS - 26 * HOUR + 2 * MIN,
    metadata_json: JSON.stringify({ review_checklist: [
      { id: "c1", label: "재전송 50회 중 중복 0회", done: true },
      { id: "c2", label: "세션 ses-412 복구 뒤 워처 재부착 확인", done: false },
      { id: "c3", label: "agentdesk-relay-regression-transcript-2026-11-17-final-with-a-very-long-identifier.json 첨부", done: false },
    ] }),
  }),
  card("card-8002", 8002, "리뷰 제안 결정 대기 카드", { status: "review", review_status: "suggestion_pending", assignee_agent_id: "agent-luna" }),
  card("card-8003", null, "활동이 아직 없는 메모 카드", { priority: "low" }),
];

const dispatch = (id: string, type: string, status: string, at: number, patch: Record<string, unknown> = {}) => ({
  id, kanban_card_id: "card-8001", from_agent_id: "pipeline", to_agent_id: "agent-ada", dispatch_type: type, status, title: "", context_file: null,
  result_file: null, result_summary: null, parent_dispatch_id: null, chain_depth: 0, created_at: at, dispatched_at: at, completed_at: null, ...patch,
});
const DISPATCHES = [
  dispatch("d-impl-0001-aaaa", "implementation", "failed", NOW_MS - 26 * HOUR, {
    title: "[구현] #8001 릴레이 재전송 중복 막기",
    result_summary: "tmux 세션 ses-412를 찾지 못했어요 (exit 1).\n재현 환경이 정리된 것으로 보입니다.\n다음: 세션 복구 경로 추가",
  }),
  dispatch("d-review-0001-bbbb", "review", "completed", NOW_MS - 25 * HOUR, { to_agent_id: "agent-luna", chain_depth: 1, title: "[리뷰] #8001 1차 구현 검토", result_summary: "blocking 1건: retry_interval < queue_tick" }),
  dispatch("d-impl-0002", "implementation", "in_progress", NOW_MS - 40 * MIN, { title: "[구현] #8001 세션 복구 뒤 워처 재부착" }),
];

const AUDIT = [
  { id: 1, card_id: "card-8001", from_status: "backlog", to_status: "ready", source: "dashboard", result: "OK", created_at: iso(NOW_MS - 27 * HOUR) },
  { id: 2, card_id: "card-8001", from_status: "ready", to_status: "requested", source: "pipeline", result: null, created_at: iso(NOW_MS - 26 * HOUR) },
  { id: 3, card_id: "card-8001", from_status: "requested", to_status: "in_progress", source: "pipeline", result: null, created_at: iso(NOW_MS - 26 * HOUR + 2 * MIN) },
  { id: 4, card_id: "card-8001", from_status: "in_progress", to_status: "blocked", source: "watchdog", result: "BLOCKED: tmux 세션 ses-412 없음", created_at: iso(NOW_MS - 25 * HOUR - 30 * MIN) },
  { id: 5, card_id: "card-8001", from_status: "blocked", to_status: "in_progress", source: "pm", result: JSON.stringify({ decision: "rework", comment: "세션 복구 경로를 추가해서 다시 진행" }), created_at: iso(NOW_MS - 45 * MIN) },
];

const COMMENTS = [
  { author: { login: "luna-ops" }, createdAt: iso(NOW_MS - 25 * HOUR + 5 * MIN), body: "리뷰했습니다. blocking finding 1건\n\n1. `src/relay/retry.rs:88` 재시도 간격이 큐 틱보다 짧습니다\n2. `src/relay/watch.rs:41` 복구 뒤 워처가 붙지 않습니다" },
  { author: { login: "pmd-bot" }, createdAt: iso(NOW_MS - 24 * HOUR), body: "## PM 결정: 재작업\n- 세션 복구 뒤 워처 재부착을 같은 PR에서 처리\n- 재전송 테스트 50회 유지" },
  { author: { login: "ada-dashboard" }, createdAt: iso(NOW_MS - 44 * MIN), body: "상태를 in_progress로 변경했습니다" },
  { author: { login: "ada-dashboard" }, createdAt: iso(NOW_MS - 43 * MIN), body: "우선순위를 high로 변경했습니다" },
  { author: { login: "ada-dashboard" }, createdAt: iso(NOW_MS - 30 * MIN), body: "## 완료 보고\n### 변경\n- retry_interval=1500ms\n### 검증\n- 재전송 50회 중복 0회" },
  { author: { login: "itismyfield" }, createdAt: iso(NOW_MS - 12 * MIN), body: "로그 묶음은 https://github.com/itismyfield/AgentDesk/actions/runs/123456789012/attempts/2/logs/agentdesk-relay-regression-transcript-2026-11-17-final.json 에 있어요. 확인 부탁드려요." },
];

const event = (kind: string, content: string, patch: Record<string, unknown> = {}) => ({ kind, tool_name: null, summary: null, content, status: null, is_error: false, ...patch });
const LONG_PROMPT = Array.from({ length: 28 }, (_, index) => `${index + 1}. #agent-ops relay_watcher turn=${4100 + index} 상태를 확인하고 중복 전송이 있으면 원인을 적어 주세요.`).join("\n");
const TRANSCRIPTS = [
  {
    id: 3, turn_id: "turn-3", session_key: null, channel_id: null, agent_id: "agent-ada", provider: "claude", dispatch_id: "d-impl-0002", kanban_card_id: "card-8001",
    dispatch_title: "[구현] #8001 세션 복구 뒤 워처 재부착", card_title: null, github_issue_number: 8001, user_message: LONG_PROMPT, assistant_message: "",
    duration_ms: 18 * MIN, created_at: iso(NOW_MS - 5 * MIN),
    events: [
      event("thinking", "세션 복구 경로부터 확인한다."),
      ...Array.from({ length: 18 }, (_, index) => [
        event("tool_use", `rg -n "reattach" src/relay --glob '*.rs' # pass ${index + 1}`, { tool_name: "Bash", summary: `rg reattach #${index + 1}` }),
        event("tool_result", `src/relay/watch.rs:${41 + index}: fn reattach_watcher(session: &Session)`, { tool_name: "Bash" }),
      ]).flat(),
      event("assistant", "워처 재부착을 복구 직후로 옮기고 테스트를 추가했습니다."),
      event("result", "", { summary: "completed", status: "success" }),
    ],
  },
  {
    id: 2, turn_id: "turn-2", session_key: null, channel_id: null, agent_id: "agent-luna", provider: "codex", dispatch_id: "d-review-0001-bbbb", kanban_card_id: "card-8001",
    dispatch_title: "[리뷰] #8001 1차 구현 검토", card_title: null, github_issue_number: 8001, user_message: "1차 구현을 리뷰해 주세요.", assistant_message: "",
    duration_ms: 3 * MIN + 40_000, created_at: iso(NOW_MS - 25 * HOUR + 4 * MIN),
    events: [
      event("assistant", "리뷰를 시작합니다."),
      event("tool_use", "git diff origin/main...HEAD -- src/relay", { tool_name: "Bash" }),
      event("tool_result", "+ const RETRY_INTERVAL_MS: u64 = 800;", { tool_name: "Bash" }),
      event("result", "blocking 1건: retry_interval < queue_tick", { summary: "completed" }),
    ],
  },
  {
    id: 1, turn_id: "turn-1", session_key: null, channel_id: null, agent_id: "agent-ada", provider: "claude", dispatch_id: "d-impl-0001-aaaa", kanban_card_id: "card-8001",
    dispatch_title: "[구현] #8001 릴레이 재전송 중복 막기", card_title: null, github_issue_number: 8001, user_message: "재전송 중복을 막아 주세요.", assistant_message: "",
    duration_ms: 6 * MIN + 12_000, created_at: iso(NOW_MS - 26 * HOUR + 8 * MIN),
    events: [
      event("thinking", "재현부터 한다."),
      event("tool_use", "tmux attach -t ses-412", { tool_name: "Bash" }),
      event("error", "tmux 세션 ses-412를 찾지 못했어요 (exit 1). 재현 환경이 정리된 것으로 보입니다.", { is_error: true }),
      event("assistant", "세션이 없어 재현하지 못했습니다."),
    ],
  },
];

const REVIEW = {
  id: "review-1", card_id: "card-8002", round: 1, original_dispatch_id: null, original_agent_id: null, original_provider: null, review_dispatch_id: null,
  reviewer_agent_id: "agent-ada", reviewer_provider: "codex", verdict: "improve", github_comment_id: null, created_at: NOW_MS - HOUR, completed_at: null,
  items_json: JSON.stringify([
    { id: "r1", category: "improve", summary: "재시도 간격을 상수로 빼기", detail: "retry.rs의 800ms가 두 곳에 있습니다.", suggestion: "RETRY_INTERVAL_MS 하나로 모으기" },
    { id: "r2", category: "dilemma", summary: "429 응답 시 큐를 비울지", pros: "지연이 짧아짐", cons: "메시지 유실 가능" },
    { id: "r3", category: "pass", summary: "테스트 범위는 충분" },
  ]),
};

const json = (page: Page, pattern: RegExp, body: unknown) => page.route(pattern, (route) => route.fulfill({ json: body }));

async function mockApi(page: Page, theme: "dark" | "light" = "dark") {
  const decisions: unknown[] = [];
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
  await json(page, /\/api\/settings$/, { language: "ko", theme });
  await json(page, /\/api\/audit-logs(?:\?.*)?$/, { logs: [] });
  await json(page, /\/api\/dispatches(?:\?.*)?$/, { dispatches: DISPATCHES });
  await json(page, /\/api\/kanban-cards(?:\?.*)?$/, { cards: CARDS });
  await page.route(/\/api\/kanban-cards\/[^/?]+$/, (route) => {
    const id = decodeURIComponent(new URL(route.request().url()).pathname.split("/").pop() ?? "");
    const found = CARDS.find((value) => value.id === id);
    return found ? route.fulfill({ json: { card: found } }) : route.fulfill({ status: 404, json: { error: "card not found" } });
  });
  await page.route(/\/api\/kanban-cards\/[^/]+\/audit-log$/, (route) => route.fulfill({ json: { logs: route.request().url().includes("card-8001") ? AUDIT : [] } }));
  await page.route(/\/api\/kanban-cards\/[^/]+\/comments$/, (route) => {
    const own = route.request().url().includes("card-8001");
    return route.fulfill({ json: own ? { comments: COMMENTS, body: ISSUE_BODY } : { comments: [], body: "" } });
  });
  await page.route(/\/api\/kanban-cards\/[^/]+\/reviews$/, (route) => route.fulfill({ json: { reviews: route.request().url().includes("card-8002") ? [REVIEW] : [] } }));
  await page.route(/\/api\/kanban-reviews\/[^/]+\/decisions$/, (route) => { decisions.push(route.request().postDataJSON()); return route.fulfill({ json: { ok: true } }); });
  await page.route(/\/api\/pipeline\/cards\/[^/]+\/transcripts(?:\?.*)?$/, (route) => route.fulfill({ json: { transcripts: route.request().url().includes("card-8001") ? TRANSCRIPTS : [] } }));
  await json(page, /\/api\/campaigns\?.*$/, { campaigns: [], live: {}, limit: 100, offset: 0 });
  await json(page, /\/api\/server-campaigns\?.*$/, { campaigns: [] });
  return decisions;
}

async function openCard(page: Page, projectName: string, id = "card-8001", theme: "dark" | "light" = "dark") {
  if (projectName === "desktop") await page.setViewportSize({ width: 1440, height: 900 });
  await page.clock.setFixedTime(NOW_MS);
  const decisions = await mockApi(page, theme);
  await page.goto(`/campaigns?tab=queue&card=${id}`);
  const drawer = page.getByTestId("kanban-card-drawer");
  await expect(drawer).toBeVisible({ timeout: 15_000 });
  return { drawer, decisions };
}

async function expectNoHorizontalOverflow(page: Page) {
  await expect.poll(() => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth)).toBeLessThanOrEqual(0);
}

const rows = (activity: Locator) => activity.locator("[data-activity-row]");

test("activity lists dispatches, transitions and comments as dated rows with filters, search and the full record", async ({ page }, testInfo) => {
  const { drawer } = await openCard(page, testInfo.project.name);
  const activity = drawer.getByTestId("card-activity");
  await activity.scrollIntoViewIfNeeded();
  // 3 dispatches + 5 transitions + 5 comment rows (two status comments within two minutes coalesce).
  await expect(rows(activity)).toHaveCount(13);
  await expect(activity.getByRole("tab", { name: /전체/ })).toHaveAttribute("aria-selected", "true");
  await expect(activity.getByRole("heading", { name: /^오늘/ })).toBeVisible();
  await expect(activity.getByRole("heading", { name: /^어제/ })).toBeVisible();
  // Newest first: the last comment leads today's group.
  await expect(rows(activity).first()).toHaveAttribute("aria-label", /로그 묶음/);

  await activity.getByRole("tab", { name: /디스패치/ }).click();
  await expect(rows(activity)).toHaveCount(3);
  await activity.getByRole("tab", { name: /리뷰/ }).click();
  await expect(rows(activity)).toHaveCount(1);
  await activity.getByRole("tab", { name: /전체/ }).click();

  const search = activity.getByRole("searchbox", { name: "활동 검색" });
  // The query matches the sentence, actor and second fact: all three dispatch titles carry the issue number.
  await search.fill("#8001");
  await expect(rows(activity)).toHaveCount(3);
  await search.fill("검색어와 맞는 활동 없음");
  await expect(activity.getByText("조건에 맞는 활동이 없어요")).toBeVisible();
  await activity.getByRole("button", { name: "전체 보기" }).click();
  await expect(rows(activity)).toHaveCount(13);

  // Opening a dispatch shows its full multi-line summary in a capped, copyable well.
  const failed = activity.locator('[data-activity-id="dispatch:d-impl-0001-aaaa"] [data-activity-row]');
  await failed.click();
  await expect(failed).toHaveAttribute("aria-expanded", "true");
  const detail = activity.getByTestId("card-activity-detail");
  await expect(detail).toContainText("다음: 세션 복구 경로 추가");
  await expect(detail.getByRole("button", { name: /결과 요약 전체 복사/ })).toBeVisible();

  // The coalesced status comments open as their two entries.
  const coalesced = activity.locator('[data-source="general"] [data-activity-row]', { hasText: "2개 변경" });
  await coalesced.click();
  await expect(activity.getByTestId("card-activity-detail")).toContainText("우선순위를 high로 변경했습니다");

  const sentence = rows(activity).first().locator("[data-activity-sentence]");
  const time = rows(activity).first().locator("time");
  if (testInfo.project.name === "mobile") {
    // Narrow panel: no time column, the sentence wraps to two lines.
    await expect(time).toBeHidden();
    expect(await sentence.evaluate((node) => getComputedStyle(node).webkitLineClamp)).toBe("2");
  } else {
    await expect(time).toBeVisible();
    expect(await sentence.evaluate((node) => getComputedStyle(node).whiteSpace)).toBe("nowrap");
  }
  await expectNoHorizontalOverflow(page);
});

test("turn transcript puts turns on one time axis and shows each step's input or output in a capped well", async ({ page, context }, testInfo) => {
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  const { drawer } = await openCard(page, testInfo.project.name);
  const panel = drawer.getByTestId("turn-transcript-panel");
  await panel.scrollIntoViewIfNeeded();
  const turns = panel.getByTestId("turn-transcript-turns").locator("[data-turn]");
  await expect(turns).toHaveCount(3);
  await expect(turns.first()).toHaveAttribute("aria-current", "true");
  await expect(panel.getByTestId("turn-transcript-time-axis")).toContainText("0초");

  const summary = panel.getByTestId("turn-transcript-summary");
  await expect(summary).toContainText("39");
  // The long prompt stays inside a 240px well that scrolls.
  const prompt = panel.getByTestId("turn-transcript-prompt").locator("pre");
  const size = await prompt.evaluate((node) => ({ height: node.clientHeight, scroll: node.scrollHeight }));
  expect(size.height).toBeLessThanOrEqual(240);
  expect(size.scroll).toBeGreaterThan(size.height);

  // Steps: pick the first tool call; its body is the input.
  const steps = panel.getByTestId("turn-transcript-steps");
  await steps.locator('[data-step="1"]').click();
  const stepDetail = panel.getByTestId("turn-transcript-step-detail");
  await expect(stepDetail.getByRole("heading", { name: "Bash" })).toBeVisible();
  const body = stepDetail.getByTestId("turn-transcript-step-body");
  await expect(body).toContainText("입력");
  await expect(body.locator("pre")).toContainText("rg -n \"reattach\"");
  const copy = body.getByRole("button", { name: /입력 전체 복사/ });
  const copyBox = await copy.boundingBox();
  expect(Math.min(copyBox?.width ?? 0, copyBox?.height ?? 0)).toBeGreaterThanOrEqual(44);
  await copy.click();
  await expect(copy).toContainText(/복사됨|복사 실패/);

  // The oldest turn ended in an error: its row says so and its summary counts it.
  await turns.nth(2).click();
  await expect(turns.nth(2)).toHaveAttribute("aria-current", "true");
  await expect(summary.locator("div").nth(3)).toContainText("1");
  await steps.locator('[data-step="2"]').click();
  await expect(stepDetail.getByTestId("turn-transcript-step-body")).toContainText("오류");
  await expectNoHorizontalOverflow(page);
});

test("review suggestions decide with kit buttons and unlock rework only when every item is decided", async ({ page }, testInfo) => {
  const { drawer, decisions } = await openCard(page, testInfo.project.name, "card-8002");
  const suggestions = drawer.getByTestId("kanban-review-suggestions");
  await expect(suggestions).toBeVisible();
  const rework = suggestions.getByRole("button", { name: "모든 항목에 결정을 내려주세요" });
  await expect(rework).toBeDisabled();
  await suggestions.getByRole("button", { name: "수용" }).first().click();
  await expect(suggestions.getByRole("button", { name: "수용" }).first()).toHaveAttribute("aria-pressed", "true");
  await suggestions.getByRole("button", { name: "불수용" }).nth(1).click();
  await expect(suggestions.getByRole("button", { name: "결정 완료 → 재디스패치" })).toBeEnabled();
  await expect.poll(() => decisions.length).toBe(2);
});

test("a card with no activity says so instead of leaving a gap", async ({ page }, testInfo) => {
  const { drawer } = await openCard(page, testInfo.project.name, "card-8003");
  await expect(drawer.getByTestId("card-activity")).toContainText("아직 활동 기록이 없어요");
  await expect(drawer.getByTestId("turn-transcript-panel")).toContainText("표시할 턴 로그가 없습니다.");
});

test("at 390px every control in the card detail is a 44px target and fields use 16px text", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "mobile", "phone width only");
  const { drawer } = await openCard(page, testInfo.project.name);
  await drawer.getByTestId("card-activity").scrollIntoViewIfNeeded();
  await expect(drawer.getByTestId("turn-transcript-turns").locator("[data-turn]")).toHaveCount(3);
  const report = await drawer.evaluate((root) => {
    const small: string[] = [];
    const fonts: string[] = [];
    const selector = "button, a[href], input:not([type=checkbox]):not([type=radio]), select, textarea, [role=tab], [tabindex='0']";
    for (const node of Array.from(root.querySelectorAll<HTMLElement>(selector))) {
      const box = node.getBoundingClientRect();
      const style = getComputedStyle(node);
      if (box.width === 0 || box.height === 0 || style.visibility === "hidden" || node.closest(".sr-only")) continue;
      // The kit input sits inside its 44px box; the box is the target.
      const target = node.matches(".aui-text-field-clean__input") ? node.closest<HTMLElement>(".aui-text-field-clean__box")!.getBoundingClientRect() : box;
      if (!node.matches("pre") && Math.min(target.width, target.height) < 43.5) small.push(`${node.tagName.toLowerCase()} ${node.getAttribute("aria-label") ?? node.textContent?.trim().slice(0, 30)} ${Math.round(target.width)}x${Math.round(target.height)}`);
      if (node.matches("input, select, textarea") && parseFloat(style.fontSize) < 16) fonts.push(`${node.tagName} ${style.fontSize}`);
    }
    for (const row of Array.from(root.querySelectorAll<HTMLElement>(".aui-checkbox-clean__row"))) {
      const box = row.getBoundingClientRect();
      if (box.height < 43.5) small.push(`checkbox ${row.textContent?.slice(0, 30)} ${Math.round(box.height)}`);
    }
    return { small, fonts };
  });
  expect(report.small).toEqual([]);
  expect(report.fonts).toEqual([]);
  await expectNoHorizontalOverflow(page);
});

test("card detail screenshots at phone, tablet and desktop in light and dark", async ({ page }, testInfo) => {
  test.skip(!SHOTS || testInfo.project.name !== "desktop", "set CARD_DETAIL_SHOTS to capture");
  test.setTimeout(180_000);
  for (const [width, height] of [[390, 844], [820, 1180], [1440, 900]] as const) {
    for (const theme of ["light", "dark"] as const) {
      await page.unrouteAll({ behavior: "ignoreErrors" });
      await page.setViewportSize({ width, height });
      await page.evaluate(() => window.localStorage.clear()).catch(() => {});
      await page.clock.setFixedTime(NOW_MS);
      await mockApi(page, theme);
      await page.goto(`/campaigns?tab=queue&card=card-8001`);
      const drawer = page.getByTestId("kanban-card-drawer");
      await expect(drawer).toBeVisible({ timeout: 15_000 });
      await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
      await expect(drawer.getByTestId("turn-transcript-turns").locator("[data-turn]")).toHaveCount(3);
      const name = `${width}x${height}-${theme}`;
      const settle = async () => { await page.evaluate(() => document.fonts.ready); await page.waitForTimeout(400); };
      await settle();
      await page.screenshot({ path: `${SHOTS}/${name}-1-top.png` });
      await drawer.getByTestId("turn-transcript-panel").evaluate((node) => node.scrollIntoView({ block: "start" }));
      await settle();
      await page.screenshot({ path: `${SHOTS}/${name}-2-turns.png` });
      await drawer.getByTestId("turn-transcript-steps").locator('[data-step="1"]').click();
      await drawer.getByTestId("turn-transcript-step-detail").scrollIntoViewIfNeeded();
      await expect(drawer.getByTestId("turn-transcript-step-body").locator("pre")).toContainText("reattach");
      await settle();
      await page.screenshot({ path: `${SHOTS}/${name}-3-step.png` });
      const activity = drawer.getByTestId("card-activity");
      await activity.scrollIntoViewIfNeeded();
      await activity.locator('[data-activity-id="dispatch:d-impl-0001-aaaa"] [data-activity-row]').click();
      await activity.evaluate((node) => node.scrollIntoView({ block: "start" }));
      await settle();
      await page.screenshot({ path: `${SHOTS}/${name}-4-activity.png` });
    }
  }
});

