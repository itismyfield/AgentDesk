// @vitest-environment happy-dom

import { act } from "react";
import { createRoot } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, describe, expect, it, vi } from "vitest";
import { getDispatchedSessions } from "../../api/agentActivity";
import type { Agent, DispatchedSession } from "../../types";
import HomePageView from "./HomePageView";
import { cleanupStaleIdleSession, reconcileStaleSessionTurn, stopAgentTurn } from "./useHomeData";

const setSessionsMock = vi.fn();
let mockSessions: DispatchedSession[] = [];

vi.mock("../../api/agentActivity", () => ({
  getDispatchedSessions: vi.fn(async () => mockSessions),
}));
vi.mock("../../contexts/KanbanContext", () => ({
  useKanban: () => ({ kanbanCards: [], kanbanCardsState: "ready" }),
}));
vi.mock("../../contexts/WorkspaceContext", () => ({
  useWorkspaceAgents: () => [],
  useWorkspace: () => ({
    sessions: mockSessions,
    setSessions: setSessionsMock,
    sessionsFailed: false,
    refreshAgents: vi.fn(),
  }),
}));
vi.mock("./useHomeData", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./useHomeData")>();
  return {
    ...actual,
    usePolled: () => ({ data: null, failed: false, at: 1 }),
    useAgentTurns: () => new Map(),
    cleanupStaleIdleSession: vi.fn(async () => ({ disconnected: true })),
    reconcileStaleSessionTurn: vi.fn(async () => ({ ok: true, reconciled: false, reason: "still live" })),
    stopAgentTurn: vi.fn(async () => ({ ok: true, session_key: "s1" })),
  };
});
vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);

const TEN_DAYS_AGO = new Date(Date.now() - 10 * 86_400_000).toISOString();

describe("HomePageView stale session cleanup", () => {
  afterEach(() => {
    vi.clearAllMocks();
    mockSessions = [];
    document.body.innerHTML = "";
  });

  it("cleans up a 7+ day idle unlinked session via kill-tmux and refreshes with includeMerged=true", async () => {
    const agKey = "antigravity/discord_fb5/MacBook-Pro-14:AgentDesk-antigravity-게임-ag";
    mockSessions = [{
      id: "144925",
      session_key: agKey,
      name: agKey,
      provider: "antigravity",
      status: "idle",
      linked_agent_id: null,
      last_seen_at: TEN_DAYS_AGO,
      connected_at: null,
    } as unknown as DispatchedSession];

    const container = document.createElement("div");
    const root = createRoot(container);
    await act(async () => {
      root.render(
        <MemoryRouter>
          <HomePageView isKo isMobileViewport={false} agents={[]} onOpenAgent={vi.fn()} />
        </MemoryRouter>,
      );
    });

    const btn = Array.from(container.querySelectorAll("button")).find((el) => el.textContent?.trim() === "정리");
    expect(btn).toBeTruthy();
    await act(async () => { btn!.click(); });

    expect(cleanupStaleIdleSession).toHaveBeenCalledWith(agKey);
    expect(reconcileStaleSessionTurn).not.toHaveBeenCalled();
    expect(getDispatchedSessions).toHaveBeenCalledWith(true, { suppressErrorToast: true, maxRetries: 0 });
    await act(async () => root.unmount());
  });

  it("keeps the alert visible when reconcileStaleSessionTurn reports reconciled=false", async () => {
    const busyKey = "claude/discord_c2/MacBook-Pro-14:AgentDesk-claude-lane-1";
    mockSessions = [{
      id: "99",
      session_key: busyKey,
      name: busyKey,
      provider: "claude",
      status: "turn_active",
      linked_agent_id: null,
      last_seen_at: TEN_DAYS_AGO,
      connected_at: null,
    } as unknown as DispatchedSession];

    const container = document.createElement("div");
    const root = createRoot(container);
    await act(async () => {
      root.render(
        <MemoryRouter>
          <HomePageView isKo isMobileViewport={false} agents={[]} onOpenAgent={vi.fn()} />
        </MemoryRouter>,
      );
    });

    const btn = Array.from(container.querySelectorAll("button")).find((el) => el.textContent?.trim() === "정리");
    await act(async () => { btn!.click(); });

    expect(reconcileStaleSessionTurn).toHaveBeenCalledWith(busyKey);
    expect(container.querySelector('[data-testid="home-triage-stale-session"]')).toBeTruthy();
    await act(async () => root.unmount());
  });

  it("renders a single working agent monitor in 1-col grid and confirms turn stop via AlertDialog", async () => {
    const confirmSpy = vi.fn(() => true);
    Object.defineProperty(window, "confirm", { value: confirmSpy, configurable: true, writable: true });
    const workingAgent = {
      id: "ada",
      name: "Ada",
      name_ko: "아다",
      status: "working",
      session_info: "검증 실행 중",
      cli_provider: "codex",
    } as Agent;

    const container = document.createElement("div");
    document.body.appendChild(container);
    const root = createRoot(container);
    await act(async () => {
      root.render(
        <MemoryRouter>
          <HomePageView isKo isMobileViewport={false} agents={[workingAgent]} onOpenAgent={vi.fn()} />
        </MemoryRouter>,
      );
    });

    const monitorItem = container.querySelector('[data-testid="home-agent-monitor-ada"]');
    expect(monitorItem).toBeTruthy();
    expect(monitorItem?.parentElement?.className).toContain("grid-cols-1");
    expect(monitorItem?.parentElement?.className).not.toContain("sm:grid-cols-2");

    const cancelBtn = Array.from(container.querySelectorAll("button")).find((el) => el.textContent?.trim() === "중단");
    expect(cancelBtn).toBeTruthy();
    await act(async () => { cancelBtn!.click(); });

    expect(confirmSpy).not.toHaveBeenCalled();
    expect(stopAgentTurn).not.toHaveBeenCalled();

    const confirmBtn = Array.from(document.querySelectorAll("button")).find((el) => el.textContent?.trim() === "턴 중단");
    expect(confirmBtn).toBeTruthy();
    await act(async () => { confirmBtn!.click(); });

    expect(stopAgentTurn).toHaveBeenCalledWith("ada");
    await act(async () => root.unmount());
  });
});
