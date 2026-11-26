// @vitest-environment happy-dom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { beginAuthentication } from "../src/api/authState";
import { ToastProvider } from "../src/components/common/overlay/ToastProvider";
import type { TFunction } from "../src/components/dashboard/model";
import ReceiptWidget from "../src/components/dashboard/ReceiptWidget";
import { buildScheduledRunsModel, RoutinesTimelineWidget } from "../src/components/dashboard/RoutinesTimelineWidget";
import SettingsView from "../src/components/SettingsView";
import DailyTokenCompositionChart from "../src/components/stats/DailyTokenCompositionChart";
import { StatsAccountLimits } from "../src/components/stats/StatsAccountLimits";
import { StatsSummaryGrid } from "../src/components/stats/StatsSummaryCards";
import { dailySeries } from "../src/components/stats/statsModel";

const TOKEN = "slice4-test-token";
const koT: TFunction = (m) => m.ko;

type Handler = (init: RequestInit) => { status?: number; body: unknown };
let routes: Record<string, Handler> = {};
let calls: Array<{ key: string; body?: unknown }> = [];
let root: Root | undefined;
let container: HTMLDivElement | undefined;

function serve(table: Record<string, Handler>) {
  routes = table;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init: RequestInit = {}) => {
      const cleanUrl = String(url);
      const key = `${(init.method ?? "GET").toUpperCase()} ${cleanUrl}`;
      calls.push({ key, body: typeof init.body === "string" && init.body ? JSON.parse(init.body) : undefined });
      const reply = routes[key]?.(init) ?? { body: {} };
      return new Response(JSON.stringify(reply.body), { status: reply.status ?? 200 });
    }),
  );
}

async function settle(rounds = 8) {
  for (let i = 0; i < rounds; i += 1) {
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
  }
}

async function render(element: ReactNode) {
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
  await act(async () => {
    root!.render(element);
  });
  await settle();
  return container;
}

function typeInto(input: HTMLInputElement, value: string) {
  const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
  setter.call(input, value);
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  if (!("ResizeObserver" in window)) {
    vi.stubGlobal("ResizeObserver", class { observe() {} disconnect() {} unobserve() {} });
  }
  calls = [];
  beginAuthentication(TOKEN);
});

afterEach(async () => {
  await act(async () => root?.unmount());
  root = undefined;
  container?.remove();
  container = undefined;
  beginAuthentication(null);
  vi.unstubAllGlobals();
});

describe("Slice 4 — Stats: one-click Claude account switch, email masking, BreakdownChart & KpiChart defaultIndex", () => {
  it("masks emails by default, sorts measured spare accounts ahead of unmeasured ones, maps 7d Fable, and switches accounts", async () => {
    let activeNum = 1;
    serve({
      "GET /api/claude-accounts": () => ({
        body: {
          hostname: "node-alpha",
          fetchedAt: new Date().toISOString(),
          accounts: [
            {
              number: 1,
              email: "operator.long@example.com",
              active: activeNum === 1,
              usage: {
                fiveHour: { pct: 98, resetsAt: new Date(Date.now() + 3600_000).toISOString() },
                sevenDay: { pct: 85, resetsAt: new Date(Date.now() + 86400_000).toISOString() },
                "7d Fable": { pct: 64, resetsAt: new Date(Date.now() + 86400_000).toISOString() },
              },
            },
            {
              number: 2,
              email: "unmeasured@example.com",
              active: activeNum === 2,
              usage: null,
            },
            {
              number: 3,
              email: "spare@example.com",
              active: activeNum === 3,
              usage: { fiveHour: { pct: 18 }, sevenDay: { pct: 25 } },
            },
          ],
        },
      }),
      "POST /api/claude-accounts/switch": (init) => {
        const parsed = JSON.parse(String(init.body)) as { account: string };
        activeNum = Number(parsed.account);
        return { body: { status: "ok", switched: true, to: activeNum } };
      },
    });

    const view = await render(<StatsAccountLimits t={koT} locale="ko-KR" allowSwitch defaultMasked />);

    expect(view.textContent).toContain("oper***@example.com");
    expect(view.textContent).not.toContain("operator.long@example.com");
    expect(view.textContent).toContain("7일 상위 모델 (Fable)");
    expect(view.textContent).toContain("비활성 슬롯 · 전환하거나 새로고침하면 측정돼요");

    // Measured spare account (#3, 25%) sorts ahead of unmeasured account (#2, null -> Infinity)
    const switchButtons = Array.from(view.querySelectorAll("button[data-testid^='stats-claude-switch-']"))
      .map((el) => el.getAttribute("data-testid"));
    expect(switchButtons).toEqual(["stats-claude-switch-3", "stats-claude-switch-2"]);

    const maskToggle = view.querySelector<HTMLButtonElement>("[data-testid='stats-claude-mask-toggle']")!;
    await act(async () => maskToggle.click());
    expect(view.textContent).toContain("operator.long@example.com");
    await act(async () => maskToggle.click());
    expect(view.textContent).toContain("oper***@example.com");

    const switchBtn = view.querySelector<HTMLButtonElement>("[data-testid='stats-claude-switch-3']")!;
    await act(async () => switchBtn.click());
    const confirmBtn = view.querySelector<HTMLButtonElement>("[data-testid='stats-claude-switch-submit']")!;
    await act(async () => confirmBtn.click());
    await settle();

    const switchCalls = calls.filter((c) => c.key === "POST /api/claude-accounts/switch");
    expect(switchCalls).toHaveLength(1);
    expect(switchCalls[0].body).toEqual({ account: "3" });
    expect(view.querySelector("[data-testid='stats-claude-switch-feedback']")?.textContent).toContain("계정 #3로 전환했어요.");
  });

  it("renders BreakdownChart for daily token composition, passes defaultIndex to KpiChart, and uses v3 KitPrimitives in ReceiptWidget", async () => {
    serve({
      "GET /api/receipt?period=month": () => ({
        body: {
          period_label: "이번 달",
          period_start: "2026-11-01",
          period_end: "2026-11-24",
          models: [{ model: "claude-4", display_name: "Claude Sonnet", total_tokens: 24000, cost: 1.25, provider: "claude" }],
          subtotal: 1.5,
          cache_discount: 0.25,
          total: 1.25,
          stats: { total_messages: 12, total_sessions: 3 },
          providers: [{ provider: "claude", tokens: 24000, percentage: 100 }],
          agents: [],
        },
      }),
    });

    const daily = [
      { date: "2026-11-23", input_tokens: 1000, output_tokens: 500, cache_read_tokens: 2000, cache_creation_tokens: 300, total_tokens: 3800, cost: 0.42 },
      { date: "2026-11-24", input_tokens: 2200, output_tokens: 900, cache_read_tokens: 4100, cache_creation_tokens: 400, total_tokens: 7600, cost: 0.88 },
    ];

    const view = await render(
      <div>
        <StatsSummaryGrid
          t={koT}
          numberFormatter={new Intl.NumberFormat("ko-KR")}
          localeTag="ko-KR"
          rangeDays={7}
          summary={{
            total_tokens: 11400,
            total_cost: 1.3,
            cache_discount: 0.2,
            active_days: 2,
            average_daily_tokens: 5700,
            peak_day: { date: "2026-11-24", total_tokens: 7600 },
          }}
          daily={daily}
          loading={false}
          failed={false}
          tokenMomentumDelta={null}
          cacheSavingsDelta={null}
        />
        <DailyTokenCompositionChart t={koT} daily={daily} series={dailySeries((m) => m.ko)} />
        <ReceiptWidget t={koT} />
      </div>,
    );

    expect(view.querySelector(".aui-breakdown-chart")).not.toBeNull();
    const reading = view.querySelector("[data-testid='stats-summary-total-tokens'] .aui-kpi-chart-reading")!;
    expect(reading.getAttribute("data-visible")).toBe("true");
    expect(reading.querySelector(".aui-kpi-chart-reading-content")?.textContent).toContain("7.6K");
    expect(view.textContent).toContain("토큰 영수증");
    expect(view.textContent).toContain("$1.25");
  });
});

describe("Slice 4 — Routines: v3 ScheduledRuns board + one-click POST /api/routines/{id}/run-now", () => {
  it("excludes >=7d routines from ScheduledRuns reserved cells and handles completed/failed run-now outcomes", async () => {
    const baseNow = Date.parse("2026-11-24T03:00:00Z");
    const model = buildScheduledRunsModel(
      [
        {
          id: "rt-far",
          agent_id: "ops",
          script_ref: "far.js",
          name: "far-routine",
          status: "enabled",
          execution_strategy: "fresh",
          schedule: "0 9 15 * *",
          next_due_at: new Date(baseNow + 10 * 86_400_000).toISOString(),
          last_run_at: null,
          last_result: null,
          checkpoint: null,
          discord_thread_id: null,
          timeout_secs: 300,
          in_flight_run_id: null,
          created_at: "2026-11-01T00:00:00Z",
          updated_at: "2026-11-24T00:00:00Z",
        },
      ],
      baseNow,
      "ko",
      koT,
    );
    expect(model.tasks[0]?.reserved).toBeUndefined();

    let nextOutcome = "completed";
    serve({
      "GET /api/routines": () => ({
        body: {
          routines: [
            {
              id: "rt-briefing",
              agent_id: "obujang",
              script_ref: "morning-briefing.js",
              name: "morning-briefing",
              status: "enabled",
              execution_strategy: "fresh",
              schedule: "0 9 * * *",
              next_due_at: new Date(Date.now() + 7200_000).toISOString(),
              last_run_at: null,
              last_result: "어제 브리핑 완료",
              checkpoint: null,
              discord_thread_id: null,
              timeout_secs: 300,
              in_flight_run_id: null,
              created_at: "2026-11-01T00:00:00Z",
              updated_at: "2026-11-24T00:00:00Z",
            },
          ],
        },
      }),
      "GET /api/routines/rt-briefing/runs?limit=8": () => ({ body: { runs: [] } }),
      "POST /api/routines/rt-briefing/run-now": () => ({ body: { outcome: { status: nextOutcome, error: nextOutcome === "failed" ? "script exit 1" : null } } }),
    });

    const view = await render(
      <ToastProvider>
        <RoutinesTimelineWidget t={koT} localeTag="ko-KR" language="ko" />
      </ToastProvider>,
    );

    expect(view.querySelector(".aui-scheduled-runs")).not.toBeNull();
    const rowRunBtn = view.querySelector<HTMLButtonElement>("[data-testid='routine-run-now-rt-briefing']")!;
    await act(async () => rowRunBtn.click());
    await settle();
    expect(calls.filter((c) => c.key === "POST /api/routines/rt-briefing/run-now")).toHaveLength(1);
    expect(document.body.textContent).toContain("morning-briefing 루틴을 바로 실행했어요.");

    nextOutcome = "failed";
    await act(async () => view.querySelector<HTMLButtonElement>("[data-testid='routine-row-rt-briefing']")!.click());
    await settle();
    const detailRunBtn = view.querySelector<HTMLButtonElement>("[data-testid='routine-detail-run-now']")!;
    await act(async () => detailRunBtn.click());
    await settle();
    expect(document.body.textContent).toContain("morning-briefing 실행이 실패로 끝났어요: script exit 1");
  });
});

describe("Slice 4 — Settings: live runtime health truth, kit IntegrationSettings, 2-tier catalog & single-line chips", () => {
  it("suppresses false disconnected warnings, hides demo collapse fields, omits disabledReason on ready connectors, and auto-expands advanced tier on search", async () => {
    serve({
      "GET /api/settings/config": () => ({
        body: {
          entries: [
            { key: "review_enabled", value: "true", baseline: "false", baseline_source: "hardcoded", override_active: true, category: "review", label_ko: "리뷰 활성화", label_en: "Enable review", restart_behavior: "persist-live-override" },
            { key: "server_port", value: "8791", baseline: "8791", baseline_source: "yaml", category: "system", label_ko: "서버 포트", label_en: "Server port", editable: false, restart_behavior: "config-only" },
          ],
        },
      }),
      "GET /api/settings/runtime-config": () => ({
        body: {
          current: {
            dispatchRateLimitGateEnabled: true,
            staleDispatchedTerminalStatuses: "completed,failed,cancelled",
          },
          defaults: {
            dispatchRateLimitGateEnabled: true,
            staleDispatchedTerminalStatuses: "completed,failed",
          },
        },
      }),
      "GET /api/onboarding/status": () => ({
        body: { guild_id: null, owner_id: null, bot_providers: {} },
      }),
      "GET /api/health/detail": () => ({
        body: {
          status: "healthy",
          providers: [
            { name: "claude", connected: true, active_turns: 1, queue_depth: 0, sessions: 3, restart_pending: false, last_turn_at: null },
            { name: "codex", connected: true, active_turns: 0, queue_depth: 0, sessions: 2, restart_pending: false, last_turn_at: null },
          ],
        },
      }),
      "GET /api/settings/operator-connectors": () => ({
        body: {
          connectors: [
            {
              id: "obsidian_agent_prompts",
              name: "Obsidian prompts",
              state: "ready",
              optional: true,
              env_var: "AGENTDESK_OBSIDIAN_PROMPTS",
              source: "/srv/vault",
              reason: null,
              detail: "",
              setup_actions: [],
              capabilities: ["prompts"],
            },
          ],
          summary: { ready: 1, skipped: 0, missing_config: 0, missing_path: 0, missing_provider: 0, invalid_config: 0, invalid: 0, total: 1, core_runtime_blocking: false },
        },
      }),
      "GET /api/settings/prompt-manifest-retention": () => ({
        body: { enabled: true, retention_days: 30, restart_required_for_config_changes: true, total_stored_bytes: 1024, layer_count: 4, truncated_count: 0 },
      }),
      "GET /api/github-repos": () => ({ body: { repos: [], viewer_login: "" } }),
      "GET /api/agents": () => ({ body: { agents: [] } }),
    });

    const queries = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const view = await render(
      <QueryClientProvider client={queries}>
        <MemoryRouter>
          <SettingsView isKo={true} language="ko" theme="dark" onSaveSettings={vi.fn()} />
        </MemoryRouter>
      </QueryClientProvider>,
    );

    expect(view.textContent).not.toContain("아직 연결하지 않았어요");
    expect(view.textContent).not.toContain("아직 고르지 않았어요");

    const integrationGrid = view.querySelector("[data-testid='settings-integrations-grid']")!;
    expect(integrationGrid.querySelector(".aui-integration-settings-collapse")).toBeNull();
    // Ready connector has no disabled reason banner
    expect(integrationGrid.querySelector(".aui-integration-settings-reason")).toBeNull();

    expect(view.querySelector("[data-testid='settings-tier-essential']")).not.toBeNull();
    const advToggle = view.querySelector<HTMLButtonElement>("[data-testid='settings-advanced-toggle']")!;
    expect(advToggle.getAttribute("aria-expanded")).toBe("false");

    const chipsWrap = view.querySelector("[data-testid='setting-chips-review_enabled']")!;
    expect(chipsWrap.className).toContain("flex-nowrap");
    expect(chipsWrap.className).not.toContain("flex-col");

    const searchInput = view.querySelector<HTMLInputElement>("[data-testid='settings-search-input']")!;
    await act(async () => typeInto(searchInput, "staleDispatchedTerminalStatuses"));
    await settle(2);
    expect(view.querySelector("[data-testid='setting-row-staleDispatchedTerminalStatuses']")).not.toBeNull();
  });
});
