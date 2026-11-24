// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { request } from "../src/api/httpClient";
import { AppSidebar } from "../src/app/AppSidebar";
import { AppTopBar } from "../src/app/AppTopBar";
import { TABLET_RAIL_MEDIA_QUERY } from "../src/app/breakpoints";
import { PageFrame } from "../src/app/PageFrame";
import { PRIMARY_ROUTES } from "../src/app/routes";
import { ShellWindowFocusContext } from "../src/app/shellFocus";
import { MOBILE_PRIMARY_ROUTE_IDS, SIDEBAR_SECTION_ORDER } from "../src/app/shellNavigationConfig";
import { iconForRoute } from "../src/app/shellRouteIcons";
import CommandPalette from "../src/components/CommandPalette";
import { HomeAgentsCard, HomeSystemCard, HomeTodoCard } from "../src/components/home/HomeSummaryCards";
import {
  buildHomeAgents,
  buildRunningNowTasks,
  buildTriageAlerts,
  buildUsageRows,
  describeHealthReasons,
  formatKstReset,
  monitorView,
  type HomeHealth,
} from "../src/components/home/homeModel";
import { CompactBreakdown } from "../src/kit/pro/compact-breakdown";
import { UsageMeter } from "../src/kit/pro/usage-meter";
import type { Agent, Department, DispatchedSession, KanbanCard } from "../src/types";
import { transformKitSource } from "../scripts/sync-ui-kit.mjs";

vi.mock("../src/api/httpClient", () => ({
  ApiRequestError: class extends Error {
    status = 500;
    body = null;
  },
  readCachedGet: vi.fn(() => null),
  readCachedSnapshot: vi.fn(() => null),
  request: vi.fn(async (path: string) => {
    if (String(path).includes("/api/health/detail")) {
      return {
        status: "degraded",
        db: true,
        degraded_reasons: ["db_slow", "provider:claude:disconnected"],
        providers: [{ name: "claude", connected: false, active_turns: 0, queue_depth: 0, sessions: 0, restart_pending: false, last_turn_at: null }],
        dispatch_outbox: { pending: 0, retrying: 2, permanent_failures: 1, oldest_pending_age: 30 },
      };
    }
    if (String(path).includes("/turn")) {
      return {
        status: "working",
        started_at: "2026-11-24T04:00:00Z",
        updated_at: "2026-11-24T04:05:00Z",
        provider: "claude",
        current_tool_line: "npm test",
        tool_events: [
          { kind: "tool", status: "success", tool_name: "Read", summary: "Read homeModel.ts", line: "Read homeModel.ts" },
          { kind: "tool", status: "running", tool_name: "Bash", summary: "npm test", line: "npm test" },
        ],
      };
    }
    return {};
  }),
}));

const tr = (ko: string, _en?: string) => ko;

let container: HTMLDivElement;
let root: Root;

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const dialog = HTMLDialogElement.prototype as HTMLDialogElement & {
    showModal?: () => void;
    show?: () => void;
    close?: () => void;
  };
  if (typeof dialog.showModal !== "function") {
    dialog.showModal = function showModal(this: HTMLDialogElement) { this.setAttribute("open", ""); };
  }
  if (typeof dialog.show !== "function") {
    dialog.show = function show(this: HTMLDialogElement) { this.setAttribute("open", ""); };
  }
  if (typeof dialog.close !== "function") {
    dialog.close = function close(this: HTMLDialogElement) { this.removeAttribute("open"); };
  }
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
  vi.clearAllMocks();
});

function renderWithProviders(ui: React.ReactNode) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return act(async () => {
    root.render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/home"]}>{ui}</MemoryRouter>
      </QueryClientProvider>,
    );
  });
}

const makeAgent = (id: string, extra: Partial<Agent> = {}): Agent => ({
  id,
  name: id,
  name_ko: id,
  department_id: null,
  avatar_emoji: "🤖",
  personality: null,
  status: "idle",
  stats_tasks_done: 0,
  stats_tokens: 0,
  created_at: 1_763_000_000_000,
  ...extra,
});

const makeSession = (id: string, extra: Partial<DispatchedSession> = {}): DispatchedSession => ({
  id,
  session_key: `mac-book:tmux:AgentDesk-claude-${id}`,
  name: null,
  department_id: null,
  linked_agent_id: null,
  provider: "claude",
  model: "claude-opus-4-6",
  status: "idle",
  session_info: null,
  avatar_emoji: "📡",
  tokens: 0,
  connected_at: 1_763_000_000_000,
  last_seen_at: 1_763_000_000_000,
  ...extra,
});

describe("Slice 1 — sync-ui-kit & CSS @layer kit", () => {
  it("declares @layer theme, base, kit, components, utilities at the top of main.css", () => {
    const css = readFileSync(resolve(import.meta.dirname, "../src/styles/main.css"), "utf8");
    const firstNonEmpty = css.split("\n").find((line) => line.trim().length > 0);
    expect(firstNonEmpty).toBe("@layer theme, base, kit, components, utilities;");
  });

  it("adds provenance header, strips *Demo exports, and wraps <style> CSS in @layer kit across synced kit files", () => {
    const sample = `"use client";\nimport { Check, Trash2 } from "lucide-react";\nexport function Widget() { return <><style href="w">{css}</style><span><Check /></span></>; }\nconst css = \`.aui-w { color: red; }\n.aui-w-demo { padding: 8px; }\`;\nexport type WidgetDemoState = "default";\nexport function WidgetDemo() { return <Trash2 />; }\n`;
    const out = transformKitSource(sample, "free/widget.tsx", "b6d2ab1");
    expect(out.startsWith("// Kit provenance: itismyfield/agentdesk-ui-kit@b6d2ab1\n")).toBe(true);
    expect(out).toContain("@layer kit {");
    expect(out).not.toContain("WidgetDemo");
    expect(out).not.toContain("Trash2");
    expect(out).not.toContain(".aui-w-demo");
  });

  it("prevents initial UsageMeter 0%->60%->100% interpolation illusion and preserves CompactBreakdown boundaries", async () => {
    await renderWithProviders(
      <div>
        <UsageMeter
          layout="limits"
          title="공급자별 한도"
          limits={[{ group: "Codex", label: "7일", used: 100, total: 100, unit: "%" }]}
        />
        <CompactBreakdown
          items={[
            { id: "d1", label: "매우긴부서이름말줄임검증용테스트부서명", value: 999 },
            { id: "d2", label: "소규모팀", value: 1 },
          ]}
          format={(v) => v.toLocaleString()}
          onSelect={() => {}}
          selectedId="d1"
        />
      </div>,
    );
    expect(container.querySelector(".aui-usage-meter-mini-value")?.textContent).toContain("100%");
    const shares = Array.from(container.querySelectorAll(".aui-compact-breakdown-share")).map((el) => el.textContent);
    expect(shares).toEqual([">99%", "<1%"]);
    const readout = container.querySelector(".aui-compact-breakdown-full-readout");
    expect(readout?.getAttribute("data-long")).toBe("true");
    expect(readout?.textContent).toContain("매우긴부서이름말줄임검증용테스트부서명");
  });
});

describe("Slice 1 — Home 3-second triage, unlinked sessions, activity sort, and turn stop", () => {
  it("includes unlinked active & stalled sessions in buildHomeAgents and sorts by activity recency instead of Korean alphabetical order", () => {
    const now = Date.parse("2026-11-24T04:15:00Z");
    const agents = [
      makeAgent("a-chef", { name_ko: "가나다 셰프" }),
      makeAgent("z-recent", { name_ko: "하하 최근봇" }),
      makeAgent("m-working", { name_ko: "마바 작업봇", status: "working" }),
    ];
    const sessions: DispatchedSession[] = [
      makeSession("s-old", { linked_agent_id: "a-chef", status: "idle", last_seen_at: now - 5 * 86_400_000 }),
      makeSession("s-new", { linked_agent_id: "z-recent", status: "idle", last_seen_at: now - 60_000 }),
      makeSession("s-work", { linked_agent_id: "m-working", status: "turn_active", last_seen_at: now - 10_000, session_info: "빌드 중" }),
      makeSession("adk-cc-t1", {
        linked_agent_id: null,
        department_id: null,
        status: "turn_active",
        last_seen_at: now - 5_000,
        session_info: "코디네이터 레인 실행",
      }),
      makeSession("game-ag", {
        linked_agent_id: null,
        department_id: null,
        status: "idle",
        last_seen_at: now - 8 * 86_400_000,
      }),
    ];

    const entries = buildHomeAgents(agents, sessions, false, now);
    expect(entries.map((e) => [e.agent.name_ko, e.state, Boolean(e.stalled)])).toEqual([
      ["#adk-cc-t1", "working", false],
      ["마바 작업봇", "working", false],
      ["#game-ag", "resting", true],
      ["하하 최근봇", "resting", false],
      ["가나다 셰프", "resting", false],
    ]);
  });

  it("collapses HomeTodoCard to a 1-line banner when there are 0 todos and 0 triage alerts", async () => {
    await renderWithProviders(
      <HomeTodoCard
        items={[]}
        alerts={[]}
        agents={[]}
        cards="ok"
        campaigns="ok"
        isMobile={false}
        tr={tr}
      />,
    );
    const todo = container.querySelector('[data-testid="home-todo"]')!;
    expect(todo.getAttribute("data-collapsed")).toBe("true");
    expect(todo.textContent).toContain("할 일 0");
  });

  it("promotes quota pause with KST reset, server degraded health detail, and 1-click stale-session reconcile", async () => {
    const now = Date.parse("2026-11-24T04:15:00Z");
    const resetEpochSec = Math.floor(Date.parse("2026-11-25T00:00:00Z") / 1000);
    const usageRows = buildUsageRows([
      {
        provider: "codex",
        buckets: [{ name: "7d", limit: 100, used: 100, remaining: 0, reset: resetEpochSec, utilization: 100 }],
        stale: false,
        fetched_at: Math.floor(now / 1000),
      },
    ], true);
    expect(formatKstReset(resetEpochSec * 1000, tr)).toBe("11/25 09:00 KST 초기화");

    const staleSession = makeSession("camping-ag", {
      status: "idle",
      last_seen_at: now - 9 * 86_400_000,
    });
    const health: HomeHealth = {
      status: "degraded",
      degraded_reasons: ["db_slow"],
      providers: [{ name: "claude", connected: false, active_turns: 0, queue_depth: 0, sessions: 0, restart_pending: false, last_turn_at: null }],
    };
    const alerts = buildTriageAlerts({
      usageRows,
      rule: { enabled: true, pct: 95 },
      sessions: [staleSession],
      health,
      tr,
      isKo: true,
      now,
    });
    expect(alerts.map((a) => [a.kind, a.tone, a.chip])).toEqual([
      ["quota", "err", "자동 진행 멈춤"],
      ["health", "warn", "서버 주의"],
      ["stale-session", "warn", "멈춘 세션"],
    ]);
    expect(alerts[0].detail).toContain("11/25 09:00 KST 초기화");
    expect(describeHealthReasons(health, tr)).toContain("claude Discord 봇 연결 끊김");

    const onReconcile = vi.fn();
    await renderWithProviders(
      <div>
        <HomeTodoCard
          items={[]}
          alerts={alerts}
          agents={[]}
          cards="ok"
          campaigns="ok"
          isMobile={false}
          tr={tr}
          onReconcileSession={onReconcile}
        />
        <HomeSystemCard
          campaigns={{ data: { legacy: [], summaries: [], documents: [], detailFailed: 0, detailsCut: 0 }, failed: false, at: now }}
          healthState={{ data: health, failed: false, at: now }}
          tr={tr}
        />
      </div>,
    );

    const cleanBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("정리"))!;
    await act(async () => { cleanBtn.click(); });
    expect(onReconcile).toHaveBeenCalledWith(staleSession.session_key);

    const whyBtn = container.querySelector<HTMLButtonElement>('[data-testid="home-system-reasons-toggle"]')!;
    await act(async () => { whyBtn.click(); });
    expect(request).toHaveBeenCalledWith("/api/health/detail", expect.anything());
    expect(container.querySelector('[data-testid="home-health-reasons"]')?.textContent).toContain("데이터베이스 응답 지연");
  });

  it("populates AgentMonitor steps from tool_events, renders folded monitor on mobile, and triggers turn/stop", async () => {
    const now = Date.parse("2026-11-24T04:06:00Z");
    const ada = makeAgent("ada", { name_ko: "아다", status: "working" });
    const [entry] = buildHomeAgents([ada], [makeSession("s1", { linked_agent_id: "ada", status: "turn_active", last_seen_at: now })], false, now);
    const view = monitorView({
      status: "working",
      started_at: "2026-11-24T04:00:00Z",
      updated_at: "2026-11-24T04:05:00Z",
      provider: "claude",
      current_tool_line: "npm test",
      tool_events: [
        { kind: "tool", status: "success", tool_name: "Read", summary: "Read file" },
        { kind: "tool", status: "running", tool_name: "Bash", summary: "npm test" },
      ],
    }, entry, tr, now);
    expect(view?.steps?.length).toBe(2);

    const onStop = vi.fn();
    await renderWithProviders(
      <HomeAgentsCard
        entries={[entry]}
        isMobile={true}
        tr={tr}
        isKo={true}
        onOpenAgent={() => {}}
        onStopAgentTurn={onStop}
      />,
    );
    expect(container.textContent).not.toContain("모두 쉬는 중이에요.");
    const monitor = container.querySelector('[data-testid="home-agent-monitor-ada"] .aui-agent-monitor')!;
    expect(monitor.hasAttribute("data-folded")).toBe(true);

    const expandBtn = monitor.querySelector<HTMLButtonElement>(".aui-agent-monitor-expand")!;
    await act(async () => { expandBtn.click(); });
    const stopBtn = Array.from(container.querySelectorAll("button")).find((b) => b.textContent?.includes("중단"))!;
    await act(async () => { stopBtn.click(); });
    expect(onStop).toHaveBeenCalledWith(ada);
  });
});

describe("Slice 1 — TopBar RunningNow, Responsive Shell, inert, Mobile Stats tab, and CommandPalette", () => {
  it("promotes stats to mobile primary tabs and defines the 900px-1120px tablet rail query (including 1032px)", () => {
    expect(MOBILE_PRIMARY_ROUTE_IDS).toEqual(["home", "campaigns", "agents", "stats"]);
    expect(TABLET_RAIL_MEDIA_QUERY).toBe("(min-width: 900px) and (max-width: 1120px)");
  });

  it("renders AppSidebar as a 64px icon-only rail when collapsed and marks PageFrame header inert during full-window focus", async () => {
    const onToggle = vi.fn();
    await renderWithProviders(
      <ShellWindowFocusContext.Provider value={true}>
        <AppSidebar
          collapsed={true}
          onToggleCollapsed={onToggle}
          currentRouteId="campaigns"
          iconForRoute={iconForRoute}
          isKo={true}
          routeBadge={(id) => (id === "campaigns" ? 3 : undefined)}
          routes={PRIMARY_ROUTES}
          sections={SIDEBAR_SECTION_ORDER}
          setAgentsPageTab={() => {}}
          wsConnected={true}
          wsLastEventTs={Date.now()}
        />
        <PageFrame testId="test-frame" title="캠페인" actions={<button type="button">탭</button>}>
          <div>본문</div>
        </PageFrame>
      </ShellWindowFocusContext.Provider>,
    );
    const nav = container.querySelector('[data-testid="app-sidebar-nav"]')!;
    expect(nav.getAttribute("data-collapsed")).toBe("true");
    expect(nav.className).toContain("w-16");
    await act(async () => {
      container.querySelector<HTMLButtonElement>('[data-testid="app-sidebar-toggle"]')!.click();
    });
    expect(onToggle).toHaveBeenCalledTimes(1);

    const header = container.querySelector('[data-testid="test-frame"] > header') as HTMLElement;
    expect(header.inert).toBe(true);
  });

  it("wires RunningNow in AppTopBar with active agents + unlinked lanes and stop/open callbacks", async () => {
    const ada = makeAgent("ada", { name_ko: "아다", status: "working" });
    const unlinked = makeSession("adk-cc-lane", { status: "turn_active", session_info: "PR 검증 중" });
    const tasks = buildRunningNowTasks({
      agents: [ada],
      sessions: [unlinked],
      sessionsFailed: false,
      tr,
      isKo: true,
    });
    expect(tasks.map((t) => [t.id, t.status])).toEqual([
      ["agent:ada", "running"],
      [`session:${unlinked.session_key}`, "running"],
    ]);

    const onStop = vi.fn();
    const onOpen = vi.fn();
    await renderWithProviders(
      <AppTopBar
        currentRoute={PRIMARY_ROUTES[0]}
        wsConnected={true}
        dismissNotification={() => {}}
        isKo={true}
        isMobileViewport={false}
        headerZIndex={20}
        notificationBadgeCount={0}
        popoverZIndex={40}
        recentNotifications={[]}
        resolvedTheme="dark"
        runningTasks={tasks}
        onOpenRunningTask={onOpen}
        onStopRunningTask={onStop}
        setShowCommandPalette={() => {}}
        setShowNotificationPanel={() => {}}
        setShowTweaksPanel={() => {}}
        showNotificationPanel={false}
        toggleShellTheme={() => {}}
      />,
    );

    const trigger = container.querySelector<HTMLButtonElement>('[data-testid="topbar-running-now"] .aui-running-now-trigger')!;
    expect(trigger.textContent).toContain("2");
    await act(async () => {
      trigger.dispatchEvent(new MouseEvent("click", { bubbles: true, detail: 1 }));
    });
    const pauseButtons = container.querySelectorAll<HTMLButtonElement>(".aui-running-now-icon-button");
    expect(pauseButtons.length).toBe(2);
    await act(async () => { pauseButtons[0].click(); });
    expect(onStop).toHaveBeenCalledWith("agent:ada");
  });

  it("indexes campaigns, #issue tasks, #adk-cc sessions, settings keys, and routine run-now in CommandPalette", async () => {
    const onNavigate = vi.fn();
    const onNotify = vi.fn();
    const onClose = vi.fn();
    const departments: Department[] = [{ id: "dev", name: "Dev", name_ko: "개발팀", icon: "💻", color: "#6366f1", description: null, prompt: null, sort_order: 0, created_at: 0 }];
    const cards: KanbanCard[] = [{
      id: "card-8369",
      title: "대시보드 v3 후속 묶음",
      status: "in_progress",
      github_repo: "itismyfield/AgentDesk",
      github_issue_number: 8369,
    } as KanbanCard];

    await renderWithProviders(
      <CommandPalette
        agents={[makeAgent("ada", { name_ko: "아다" })]}
        departments={departments}
        sessions={[makeSession("adk-cc-main", { status: "turn_active", session_info: "스웜 조율" })]}
        kanbanCards={cards}
        campaigns={[{ id: "camp-1", title: "릴리스 1124 캠페인", status: "active", updated_at: "2026-11-24T00:00:00Z", nodes: [] } as never]}
        serverCampaigns={[]}
        routines={[{ id: "rt-1", name: "야간 헬스체크", status: "enabled", script_ref: "health.py", schedule: "0 3 * * *", execution_strategy: "fresh", agent_id: null, next_due_at: null, last_run_at: null, last_result: null, checkpoint: null, discord_thread_id: null, timeout_secs: null, in_flight_run_id: null, created_at: "", updated_at: "" }]}
        configEntries={[]}
        isKo={true}
        onSelectAgent={() => {}}
        onNavigate={onNavigate}
        onNotify={onNotify}
        onClose={onClose}
        routes={[{ id: "/home", labelKo: "홈", labelEn: "Home", icon: <span>H</span> }]}
      />,
    );

    const text = container.textContent ?? "";
    expect(text).toContain("#adk-cc-main");
    expect(text).toContain("릴리스 1124 캠페인");
    expect(text).toContain("#8369 대시보드 v3 후속 묶음");
    expect(text).toContain("지금 실행: 야간 헬스체크");

    const input = container.querySelector<HTMLInputElement>(".aui-command-menu-search input")!;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(input, "staleDispatchedTerminalStatuses");
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.dispatchEvent(new Event("change", { bubbles: true }));
    });
    expect(container.textContent).toContain("staleDispatchedTerminalStatuses");

    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
      setter.call(input, "지금 실행");
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.dispatchEvent(new Event("change", { bubbles: true }));
    });
    const runRow = Array.from(container.querySelectorAll<HTMLButtonElement>(".aui-command-menu-row")).find((r) =>
      r.textContent?.includes("지금 실행: 야간 헬스체크"),
    )!;
    await act(async () => { runRow.click(); });
    expect(request).toHaveBeenCalledWith("/api/routines/rt-1/run-now", expect.objectContaining({ method: "POST" }));
  });
});
