import { expect, test, type Page } from "@playwright/test";

test.use({ timezoneId: "Asia/Seoul", locale: "ko-KR" });

const SHOTS = process.env.AUDIT_SHOTS;
const both = { discord_channel_id: "100", discord_channel_cc: "100", discord_channel_cdx: "200", discord_channel_alt: "200" };
const agent = (id: string, name_ko: string, channels: Record<string, string>, cli_provider = "claude") => ({
  id, name: id, name_ko, alias: null, cli_provider, department_id: null, avatar_emoji: "", personality: null,
  status: "idle", stats_tasks_done: 0, stats_tokens: 0, created_at: 0, ...channels,
});
const AGENTS = [
  agent("relay-e2e", "릴레이 시험", { discord_channel_cc: "900" }),
  agent("adk-dashboard", "ADK 대시보드", both),
  agent("no-channel", "채널 없음", {}),
  agent("slow-agent", "느린 에이전트", { discord_channel_cc: "300" }),
  agent("saved-agent", "저장본 에이전트", { discord_channel_cdx: "400" }, "codex"),
  agent("empty-agent", "빈 에이전트", { discord_channel_cc: "500" }),
  agent("unbound-agent", "연결 끊긴 에이전트", { discord_channel_id: "600" }),
  agent("broken-agent", "오류 에이전트", { discord_channel_cc: "700" }),
];

const sec = (name: string, source: string, text: string, extra: Record<string, unknown> = {}) => ({ name, source, path: null, masked_count: 0, text, chars: text.length, ...extra });
const SECTIONS = [
  sec("runtime", "runtime", "This session is also connected to a Discord channel. token=***", { masked_count: 1 }),
  sec("tool_output_efficiency", "runtime", "[Tool Output Efficiency]\nConstrain output before invoking a tool."),
  sec("context_compression", "conditional", "[Context Compression]\nOnly when asked to write a continuation summary."),
  sec("shared_agent_rules", "canon_file", "[Shared Agent Rules]\n## 정본과 편집 경로\n- 에이전트 프롬프트는 정본에서 고친다.", { path: "agents/_shared.prompt.md" }),
  sec("role_binding_contract", "runtime", "[Channel Role Binding]\nThe following role definition is authoritative."),
  sec("role_prompt", "canon_file", `# adk-dashboard\n- role: ADK Dashboard frontend specialist\n${"no-break-".repeat(60)}`, { path: "agents/role.prompt.md" }),
  sec("runtime", "runtime", "\n\n[Peer Agent Directory]\nOther specialist agents work in this AgentDesk."),
  sec("memory_guidance", "conditional", "[Proactive Memory Guidance]\nmemento memory is available."),
  sec("queued_turn_rules", "conditional", "[Queued Turn Rules]\nThis user message was queued."),
  sec("memento_anchors", "memento", "[Memento anchor context]\nThese are retrieved historical memories.\n".repeat(12)),
];
const audit = (agentId: string, provider: string, origin: string, sections = SECTIONS) => ({
  agent_id: agentId, provider, origin, session_id: origin === "unavailable" ? null : "session-1",
  session_started_at: origin === "unavailable" ? null : "2026-11-17T08:17:24Z",
  masked_count: sections.reduce((sum, item) => sum + item.masked_count, 0), sections: origin === "unavailable" ? [] : sections,
});

const anc = (text: string, extra: Record<string, unknown> = {}) => ({
  id: null, type: null, scope: null, limit: null, masked_count: 0, overlap_candidates: [], receiving_agents: ["adk-dashboard"], text, chars: text.length, ...extra,
});
const ANCHORS = [
  anc("코딩 원칙 — 과도한 방어 금지, 경계에서 한 번만 검증", { type: "선호", scope: "모든 에이전트", chars: 286, limit: 300, overlap_candidates: [{ source: "anchor", index: 6 }] }),
  anc("코드 작업 원칙 — 근본 원인, 주석은 무엇과 왜", { type: "선호", scope: "모든 에이전트", chars: 310, limit: 300 }),
  anc("기억 검색·저장 기준 — 과거 결정 전엔 recall", { type: "절차", scope: "모든 에이전트", chars: 281, limit: 400, overlap_candidates: [{ source: "canon_file", path: "agents/_shared.prompt.md" }] }),
  anc("기억 도구 규칙 — Memento에만 둔다", { type: "절차", scope: "모든 에이전트", chars: 262, limit: 400, overlap_candidates: [{ source: "canon_file", path: "agents/_shared.prompt.md" }, { source: "anchor", index: 4 }] }),
  anc("작업 태도 — 판단이 갈려도 자율 결정 후 근거와 보고", { type: "선호", scope: "모든 에이전트", chars: 276, limit: 400 }),
  anc("설계 원칙 — 소규모 개인 운영 시스템", { type: "선호", scope: "AgentDesk", chars: 298, limit: 300, receiving_agents: ["adk-dashboard", "saved-agent"] }),
  anc("작업 규칙 — 캠페인은 issue-pipeline부터", { type: "절차", scope: "AgentDesk", chars: 231, limit: 400, overlap_candidates: [{ source: "anchor", index: 0 }] }),
  anc("\n응답 방식 — 질문엔 바로 ACK\n둘째 줄은 펼쳐야 보여요"),
];
const anchorsFor = (agentId: string, provider: string, source: string) => ({
  agent_id: agentId, provider, source, session_id: source === "unavailable" ? null : "session-1",
  session_started_at: source === "unavailable" ? null : "2026-11-17T08:17:24Z",
  memento: agentId === "saved-agent" ? { status: "disconnected", checked_at: "2026-11-21T03:12:00Z" } : { status: "connected", checked_at: "2026-11-21T08:00:00Z" },
  anchor_count: source === "unavailable" ? 0 : 8,
  anchors: source === "unavailable" ? [] : agentId === "saved-agent" ? [anc("비밀값 규칙 — 1Password에서 읽기"), anc("작업 호스트 — mac-mini 상주")] : ANCHORS,
});

async function mockApi(page: Page, theme: "dark" | "light" = "dark") {
  // Fixture tests must never reach a real operator API.
  await page.route(/\/api\//, (route) => route.fulfill({ status: 404, json: { error: "not mocked" } }));
  await page.addInitScript((value) => {
    localStorage.setItem("agentdesk.theme", value);
    class MockWebSocket {
      readyState = 0; onopen: ((event: Event) => void) | null = null; onclose: ((event: Event) => void) | null = null;
      onmessage = null; onerror = null;
      constructor(public url: string) { setTimeout(() => { this.readyState = 1; this.onopen?.(new Event("open")); }, 0); }
      send() {}
      close() { this.readyState = 3; this.onclose?.(new Event("close")); }
    }
    Object.defineProperty(window, "WebSocket", { configurable: true, writable: true, value: MockWebSocket });
  }, theme);
  const json = (pattern: RegExp, body: unknown) => page.route(pattern, (route) => route.fulfill({ json: body }));
  await json(/\/api\/auth\/session$/, { ok: true, authenticated: true, auth_enabled: false, csrf_token: "" });
  await json(/\/api\/auth\/ws-ticket$/, { ticket: "s".repeat(64), expires_in: 15 });
  await json(/\/api\/settings$/, { language: "ko", theme });
  await json(/\/api\/agents(?:\?.*)?$/, { agents: AGENTS });
  await json(/\/api\/departments(?:\?.*)?$/, { departments: [] });
  await json(/\/api\/dispatched-sessions(?:\?.*)?$/, { sessions: [] });
  await page.route(/\/api\/agents\/[^/]+\/(prompt-audit|anchors)\?/, async (route) => {
    const url = new URL(route.request().url());
    const [, , , id, kind] = url.pathname.split("/");
    const provider = url.searchParams.get("provider") ?? "";
    if (id === "broken-agent") return route.abort("connectionrefused");
    if (id === "unbound-agent") return route.fulfill({ status: 404, json: { code: "config", context: {}, error: "agent binding unavailable" } });
    if (id === "slow-agent") await new Promise((resolve) => setTimeout(resolve, 1200));
    const origin = id === "empty-agent" ? "unavailable" : id === "saved-agent" || provider === "codex" ? "latest_saved" : "current_session";
    if (kind === "anchors") return route.fulfill({ json: anchorsFor(id, provider, origin) });
    return route.fulfill({ json: audit(id, provider, origin, provider === "codex" ? SECTIONS.slice(0, 4) : SECTIONS) });
  });
}

const detail = (page: Page) => page.getByTestId("audit-prompt-detail");

test("desktop: bound agents, sections by source, copy and the provider key", async ({ page, context }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "desktop layout");
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await mockApi(page);
  await page.goto("/audit");
  const list = page.getByTestId("audit-agent-list");
  await expect(list.getByRole("button")).toHaveCount(7);
  await expect(list.getByRole("button").first()).toHaveAttribute("data-testid", "audit-agent-adk-dashboard");
  await expect(list.getByRole("button").last()).toHaveAttribute("data-testid", "audit-agent-relay-e2e");
  await expect(detail(page)).toHaveAttribute("data-state", "ready");
  await expect(page.getByTestId("audit-detail-meta")).toHaveText(/Claude · 11월 17일 오후 05:17 시작 · 현재 세션/);
  await expect(page.getByTestId("audit-summary")).toContainText(/10\s*섹션/);
  await expect(page.getByRole("region", { name: "프롬프트 요약" })).toHaveCSS("padding", "14px 16px");
  await expect(page.getByTestId("audit-share-bar")).toHaveAttribute("aria-label", /정본 파일 \d+%, 메멘토 \d+%, 런타임 \d+%, 조건부 \d+%/);
  const rows = page.getByTestId("audit-section");
  await expect(rows.nth(0)).toContainText("런타임 문맥 1");
  await expect(rows.nth(0)).toContainText("가림 1");
  await expect(rows.nth(6)).toContainText("런타임 문맥 2");
  await expect(rows.nth(5)).toContainText("agents/role.prompt.md");
  await expect(page.getByTestId("audit-source-summary")).toContainText("agents/role.prompt.md");
  await rows.nth(5).getByRole("button").click();
  await expect(rows.nth(5).locator("pre")).toContainText("ADK Dashboard frontend specialist");
  await page.getByTestId("audit-copy").click();
  await expect(page.getByTestId("audit-copy")).toHaveText("복사했어요");
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  expect(copied.startsWith(SECTIONS[0].text)).toBe(true);
  expect(copied).toContain("[Memento anchor context]");
  const total = SECTIONS.reduce((sum, item) => sum + item.chars, 0).toLocaleString("ko-KR");
  await expect(page.getByTestId("audit-agent-adk-dashboard")).toContainText(`${total}자`);

  await page.getByRole("radio", { name: "Codex" }).click();
  await expect(page).toHaveURL(/provider=codex/);
  await expect(page.getByTestId("audit-detail-meta")).toContainText("세션 없음 · 최근 저장본");
  await page.getByRole("radio", { name: "Claude" }).click();
  await expect(page).toHaveURL(/\?agent=adk-dashboard$/);
  await page.getByTestId("audit-agent-saved-agent").click();
  await expect(page).toHaveURL(/agent=saved-agent/);
  await expect(page.getByTestId("audit-provider")).toHaveCount(0);
  await page.goBack();
  await expect(page).toHaveURL(/\?agent=adk-dashboard$/);
  await expect(page.getByRole("heading", { name: "ADK 대시보드" })).toBeVisible();
});

test("desktop: empty copy, missing channel, network failure and a late answer stay apart", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "desktop layout");
  await mockApi(page);
  await page.goto("/audit?agent=empty-agent");
  await expect(page.getByTestId("audit-state-unavailable")).toContainText("세션도 저장본도 없어요");
  await page.getByTestId("audit-agent-unbound-agent").click();
  await expect(page.getByTestId("audit-state-unbound")).toContainText("이 provider에 연결된 채널이 없어요");
  await page.getByTestId("audit-agent-broken-agent").click();
  await expect(page.getByTestId("audit-state-error")).toBeVisible({ timeout: 10_000 });
  await page.route(/\/api\/agents\/broken-agent\/prompt-audit\?/, (route) => route.fulfill({ json: audit("broken-agent", "claude", "current_session") }));
  await page.getByTestId("audit-state-error").getByRole("button", { name: "다시 시도" }).click();
  await expect(detail(page)).toHaveAttribute("data-state", "ready");

  await page.getByTestId("audit-agent-slow-agent").click();
  await page.getByTestId("audit-agent-saved-agent").click();
  await expect(page.getByRole("heading", { name: "저장본 에이전트" })).toBeVisible();
  await page.waitForTimeout(1500);
  await expect(page.getByRole("heading", { name: "저장본 에이전트" })).toBeVisible();
  await expect(page.getByTestId("audit-detail-meta")).toContainText("Codex");
});

test("phone: list, detail with five sections, back by button and by browser", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "mobile", "phone layout");
  await mockApi(page);
  await page.goto("/audit");
  await expect(detail(page)).toHaveCount(0);
  await expect(page.getByTestId("audit-tabs")).toBeVisible();
  await expect(page.getByTestId("topbar")).toBeVisible();
  await page.getByTestId("audit-agent-adk-dashboard").click();
  await expect(page).toHaveURL(/agent=adk-dashboard/);
  await expect(page.getByTestId("audit-section")).toHaveCount(5);
  // The detail opens on its 56px head in the app top bar's place: no page tabs, and the provider switch names the provider instead of the meta.
  await expect(page.getByTestId("audit-tabs")).toHaveCount(0);
  await expect(page.getByTestId("topbar")).toHaveCount(0);
  await expect(page.getByTestId("app-mobile-tabbar")).toBeVisible();
  expect(await page.getByTestId("audit-detail-head").boundingBox()).toEqual({ x: 0, y: 0, width: 390, height: 56 });
  await expect(page.getByTestId("audit-detail-meta")).toHaveText("11월 17일 오후 05:17 시작 · 현재 세션");
  await expect(page.getByTestId("audit-provider")).toBeVisible();
  await page.getByRole("button", { name: "섹션 5개 더 보기" }).click();
  await expect(page.getByTestId("audit-section")).toHaveCount(10);
  await page.getByTestId("audit-page").evaluate((el) => { el.scrollTop = 200; });
  expect(await page.getByTestId("audit-page").evaluate((el) => el.scrollTop)).toBeGreaterThan(0);
  expect(await page.getByTestId("audit-detail-head").boundingBox()).toEqual({ x: 0, y: 0, width: 390, height: 56 });
  await page.getByTestId("audit-back").click();
  await expect(page).not.toHaveURL(/agent=/);
  await expect(page.getByTestId("topbar")).toBeVisible();
  await page.getByTestId("audit-agent-saved-agent").click();
  await expect(page.getByTestId("audit-detail-meta")).toContainText("최근 저장본");
  await page.goBack();
  await expect(page.getByTestId("audit-agent-list")).toBeVisible();
  const width = await page.evaluate(() => Math.max(document.body.scrollWidth, document.documentElement.scrollWidth));
  expect(width).toBeLessThanOrEqual(390);
});

const anchorDetail = (page: Page) => page.getByTestId("audit-anchor-detail");
const anchorRow = (page: Page, index: number) => page.locator(`[data-testid=audit-anchor][data-index="${index}"]`);
const openAnchor = (page: Page, index: number) => anchorRow(page, index).getByRole("button").first().click();

test("desktop anchors: link from the prompt, scope groups, filters, limits and overlap jumps", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "desktop layout");
  await mockApi(page);
  await page.goto("/audit");
  await page.getByTestId("audit-open-anchors").click();
  await expect(page).toHaveURL(/tab=anchors/);
  await expect(anchorDetail(page)).toHaveAttribute("data-state", "ready");
  await expect(page.getByTestId("audit-summary")).toContainText(/8\s*앵커.*6\s*확인할 것/);
  await expect(page.getByTestId("audit-memento-notice")).toHaveCount(0);
  await expect(page.getByTestId("audit-anchor-group")).toHaveCount(3);
  await expect(page.getByTestId("audit-anchor-group").last()).toContainText("범위 없음");
  await expect(anchorRow(page, 7)).toContainText("응답 방식 — 질문엔 바로 ACK");

  const inspector = page.getByTestId("audit-anchor-inspector");
  await page.getByRole("radio", { name: /한도 근접·초과/ }).click();
  await expect(page.getByTestId("audit-anchor")).toHaveCount(3);
  await expect(anchorRow(page, 1)).toContainText("한도 초과");
  await openAnchor(page, 0);
  await expect(anchorRow(page, 0).getByRole("button")).toHaveAttribute("aria-current", "true");
  await expect(inspector.getByTestId("audit-anchor-size")).toHaveText("286 / 300자");
  await page.getByRole("radio", { name: /정본과 겹침/ }).click();
  await expect(page.getByTestId("audit-anchor")).toHaveCount(2);
  await openAnchor(page, 3);
  await expect(inspector.getByTestId("audit-anchor-overlaps")).toContainText("agents/_shared.prompt.md");
  await inspector.getByRole("button", { name: /앵커 5 · 작업 태도/ }).click();
  await expect(page.getByRole("radio", { name: /전체/ })).toHaveAttribute("aria-checked", "true");
  await expect(anchorRow(page, 4).getByRole("button")).toHaveAttribute("aria-current", "true");
  await expect(anchorRow(page, 4).getByRole("button")).toBeFocused();

  await page.getByRole("radio", { name: "Codex" }).click();
  await page.getByRole("tab", { name: "시스템 프롬프트" }).click();
  await expect(page).not.toHaveURL(/tab=/);
  await expect(page).toHaveURL(/provider=codex/);
  await expect(page.getByTestId("audit-detail-meta")).toContainText("Codex");
});

test("desktop anchors: a saved copy while Memento is down, limits left unjudged, no session and no channel", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "desktop", "desktop layout");
  await mockApi(page);
  await page.goto("/audit?tab=anchors&agent=saved-agent");
  await expect(page.getByTestId("audit-memento-notice")).toContainText("Memento에 연결할 수 없어요");
  await expect(page.getByTestId("audit-memento-notice")).toContainText("11월 21일 오후 12:12에 마지막으로 확인했어요");
  await expect(page.getByTestId("audit-detail-meta")).toContainText("세션 없음 · 세션 사본");
  await expect(page.getByTestId("audit-anchor-group").locator("h3")).toHaveCount(0);
  await expect(page.getByRole("radio", { name: /한도 근접·초과/ })).toBeDisabled();
  await openAnchor(page, 0);
  await expect(page.getByTestId("audit-anchor-size")).toHaveText(/^\d+자$/);
  await page.getByTestId("audit-agent-empty-agent").click();
  await expect(page.getByTestId("audit-state-unavailable")).toBeVisible();
  await page.getByTestId("audit-agent-unbound-agent").click();
  await expect(page.getByTestId("audit-state-unbound")).toBeVisible();
});

test("phone anchors: five rows, then the rest", async ({ page }, testInfo) => {
  test.skip(testInfo.project.name !== "mobile", "phone layout");
  await mockApi(page);
  await page.goto("/audit?tab=anchors");
  await expect(page.getByRole("tab", { name: "메멘토 앵커" })).toHaveAttribute("aria-selected", "true");
  await page.getByTestId("audit-agent-adk-dashboard").click();
  await expect(page.getByTestId("audit-anchor")).toHaveCount(5);
  await expect(page.getByTestId("audit-tabs")).toHaveCount(0);
  await expect(page.getByTestId("topbar")).toHaveCount(0);
  await page.getByRole("button", { name: "3개 더 보기" }).click();
  await expect(page.getByTestId("audit-anchor")).toHaveCount(8);
  await openAnchor(page, 0);
  await expect(anchorRow(page, 0).getByTestId("audit-anchor-size")).toHaveText("286 / 300자");
  await page.getByTestId("audit-page").evaluate((el) => { el.scrollTop = 200; });
  expect(await page.getByTestId("audit-detail-head").boundingBox()).toEqual({ x: 0, y: 0, width: 390, height: 56 });
  const width = await page.evaluate(() => Math.max(document.body.scrollWidth, document.documentElement.scrollWidth));
  expect(width).toBeLessThanOrEqual(390);
});

const SIZES = [
  { name: "1440x900", width: 1440, height: 900, path: "/audit" },
  { name: "1024x768", width: 1024, height: 768, path: "/audit" },
  { name: "390x844-list", width: 390, height: 844, path: "/audit" },
  { name: "390x844", width: 390, height: 844, path: "/audit?agent=adk-dashboard" },
];

test("captures: both tabs at three sizes in both themes with overflow and touch counts", async ({ browser }, testInfo) => {
  test.skip(!SHOTS || testInfo.project.name !== "desktop", "set AUDIT_SHOTS to capture");
  test.setTimeout(180_000);
  const metrics: Record<string, unknown> = {};
  for (const tab of ["audit", "anchors"] as const) {
    for (const { name, path, ...viewport } of SIZES) {
      for (const theme of ["dark", "light"] as const) {
        const touch = viewport.width < 900;
        const context = await browser.newContext({ viewport, hasTouch: touch, isMobile: touch, locale: "ko-KR", timezoneId: "Asia/Seoul" });
        const page = await context.newPage();
        await mockApi(page, theme);
        await page.goto(tab === "anchors" ? `${path}${path.includes("?") ? "&" : "?"}tab=anchors` : path);
        if (name.endsWith("list")) await expect(page.getByTestId("audit-agent-list")).toBeVisible();
        else if (tab === "anchors") {
          await expect(anchorDetail(page)).toHaveAttribute("data-state", "ready");
          await openAnchor(page, 3);
        } else {
          await expect(detail(page)).toHaveAttribute("data-state", "ready");
          await page.getByTestId("audit-section").nth(3).getByRole("button").click();
        }
        await page.screenshot({ path: `${SHOTS}/${tab}-${name}-${theme}.png`, animations: "disabled" });
        metrics[`${tab}-${name}-${theme}`] = await page.evaluate(countLayout);
        await context.close();
      }
    }
  }
  console.log(JSON.stringify(metrics));
});

/** Horizontal overflow, controls under 44px and inputs under 16px on the page as rendered. */
function countLayout() {
  const visible = (el: Element) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== "hidden"; };
  const controls = [...document.querySelectorAll("button, a[href], input, select, textarea, [role=radio], [role=tab]")].filter(visible);
  const small = controls.filter((el) => { const r = el.getBoundingClientRect(); return Math.min(r.width, r.height) < 44; });
  return {
    overflow: Math.max(document.body.scrollWidth, document.documentElement.scrollWidth) - window.innerWidth,
    controls: controls.length,
    small: small.map((el) => `${el.tagName.toLowerCase()}:${el.getAttribute("aria-label") ?? el.textContent?.trim().slice(0, 20)}`),
    inputsUnder16: [...document.querySelectorAll("input")].filter((el) => visible(el) && parseFloat(getComputedStyle(el).fontSize) < 16).length,
  };
}
