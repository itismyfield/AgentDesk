// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as api from "../src/api";
import { AgentInfoCardContent } from "../src/components/agent-manager/AgentInfoCardContent";
import { buildHomeAgents, classifyAgents, type HomeTurn } from "../src/components/home/homeModel";
import {
  applySessionOverlay,
  buildEffectiveSessionAgentMap,
  extractParentChannelSlugFromThreadSessionKey,
  isThreadSession,
  stripThreadSuffixFromSessionKey,
  withInferredLinkedAgents,
} from "../src/contexts/session-overlay";
import type { Agent, DispatchedSession } from "../src/types";

vi.mock("../src/api", () => ({
  getSkillRanking: vi.fn(async () => ({ byAgent: [] })),
  getAgentCron: vi.fn(async () => []),
  getAgentSkills: vi.fn(async () => ({ skills: [], sharedSkills: [] })),
  getAgentDispatchedSessions: vi.fn(async () => []),
  getDispatchedSessions: vi.fn(async () => []),
  getDiscordBindings: vi.fn(async () => []),
  getAgentTimeline: vi.fn(async () => []),
  getAuditLogs: vi.fn(async () => []),
  getDiscordChannelInfo: vi.fn(async () => null),
  getTaskDispatches: vi.fn(async () => []),
  updateAgent: vi.fn(async () => ({})),
}));

vi.mock("../src/contexts/KanbanContext", () => ({
  useKanban: () => ({ kanbanCards: [], kanbanCardsState: "ready" }),
}));

vi.mock("../src/components/home/useHomeData", () => ({
  useAgentTurns: () => new Map(),
}));

vi.mock("../src/api/providers", () => ({
  catalogLabel: (_entries: unknown, id: string) => id,
  getProviderAuthProfiles: vi.fn(async () => ({ providers: [] })),
  useProviderCatalog: () => ({ selectableIds: ["claude", "codex"], entries: [], loading: false, error: null }),
}));

vi.mock("../src/api/agentExecutionNode", () => ({
  getAgentExecutionNode: vi.fn(async () => ({ default_node_id: null, routing_enforced: true })),
  setAgentExecutionNode: vi.fn(),
}));

vi.mock("../src/api/clusterNodes", () => ({
  getClusterNodes: vi.fn(async () => ({ cluster: { enabled: false }, nodes: [] })),
}));

const ko = (k: string, _en?: string) => k;
const en = (_k: string, e: string) => e;
const NOW = Date.now();

const AGENT_ADK: Agent = {
  id: "project-agentdesk",
  name: "AgentDesk",
  name_ko: "AgentDesk",
  department_id: null,
  cli_provider: "claude",
  avatar_emoji: "🤖",
  personality: null,
  status: "idle",
  stats_tasks_done: 24,
  stats_tokens: 5000,
  created_at: NOW - 86_400_000,
};

function makeSession(overrides: Partial<DispatchedSession> & Pick<DispatchedSession, "id" | "session_key">): DispatchedSession {
  return {
    name: overrides.session_key,
    department_id: null,
    linked_agent_id: null,
    provider: "claude",
    model: null,
    status: "idle",
    session_info: null,
    avatar_emoji: "🤖",
    tokens: 0,
    connected_at: NOW - 60_000,
    last_seen_at: NOW - 5_000,
    thread_channel_id: null,
    ...overrides,
  };
}

let root: Root;
let container: HTMLDivElement;

function setViewport(mobile: boolean) {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query.includes("max-width: 899px") ? mobile : false,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    onchange: null,
    dispatchEvent: () => false,
  }));
}

async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  setViewport(false);
  if (typeof globalThis.ResizeObserver === "undefined") {
    vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  }
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = () => {};
  }
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  document.body.innerHTML = "";
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

describe("Agent Detail duplicate avatar removal & Discord thread session surfacing", () => {
  it("renders [data-bot-avatar] only ONCE in the 44px header and hides .aui-agent-monitor-mark inside AgentMonitor", async () => {
    const mainKey = "claude/discord_1/MacBook-Pro-14:AgentDesk-claude-adk-cc";
    const mainSession = makeSession({
      id: "sess-main",
      session_key: mainKey,
      linked_agent_id: "project-agentdesk",
      status: "turn_active",
      session_info: "cargo test 실행 중",
    });
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([mainSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValueOnce([mainSession]);

    const turns = new Map<string, HomeTurn>([
      [
        "project-agentdesk",
        {
          status: "working",
          session_key: mainKey,
          started_at: new Date(NOW - 12_000).toISOString(),
          updated_at: new Date(NOW - 1_000).toISOString(),
          provider: "claude",
          current_tool_line: "Bash: cargo test",
          tool_events: [
            { kind: "tool", status: "running", tool_name: "Bash", summary: "Run tests", line: "cargo test" },
          ],
        },
      ],
    ]);

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={{ ...AGENT_ADK, status: "working" }}
            state="working"
            isKo
            locale="ko"
            tr={ko}
            turns={turns}
          />
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();

    const monitorEl = container.querySelector<HTMLElement>(".aui-agent-monitor")!;
    expect(monitorEl).not.toBeNull();
    expect(monitorEl.className).toContain("[&_.aui-agent-monitor-mark]:hidden");
    expect(monitorEl.querySelector("[data-bot-avatar]")).toBeNull();

    const allAvatars = container.querySelectorAll<HTMLElement>("[data-bot-avatar]");
    expect(allAvatars).toHaveLength(1);
    expect(allAvatars[0].style.width).toBe("44px");
    // Single non-thread main session already shown in AgentMonitor is not duplicated below it.
    expect(container.querySelector('[data-testid="agent-now-sessions"]')).toBeNull();
  });

  it("infers parent agent for unlinked Discord thread sessions across session-overlay and homeModel without orphan pseudo-agents", () => {
    const mainSession = makeSession({
      id: "s-main",
      session_key: "claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc",
      linked_agent_id: "project-agentdesk",
      status: "idle",
      last_seen_at: NOW - 120_000,
    });
    const threadSession = makeSession({
      id: "s-thread",
      session_key: "claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc-t1570308839910412348",
      linked_agent_id: null,
      thread_channel_id: "1570308839910412348",
      status: "turn_active",
      session_info: "지금 줄서있는 빌드/ci는 이제 없어?",
      last_seen_at: NOW - 2_000,
    });

    expect(isThreadSession(mainSession)).toBe(false);
    expect(isThreadSession(threadSession)).toBe(true);
    expect(
      stripThreadSuffixFromSessionKey(`${threadSession.session_key}:child:toolu_01Alpha`),
    ).toBe("claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc");
    expect(
      extractParentChannelSlugFromThreadSessionKey(`${threadSession.session_key}:child:toolu_01Alpha`),
    ).toBe("adk-cc");
    expect(extractParentChannelSlugFromThreadSessionKey(mainSession.session_key)).toBeNull();

    const agentMap = buildEffectiveSessionAgentMap([mainSession, threadSession]);
    expect(agentMap.get("s-thread")).toBe("project-agentdesk");

    // Binding hint fallback when sibling parent session is absent
    const fromBinding = withInferredLinkedAgents([threadSession], [
      { agentId: "project-agentdesk", channelId: "1490000000000000001", channelName: "#adk-cc" },
    ]);
    expect(fromBinding[0].linked_agent_id).toBe("project-agentdesk");

    const overlaid = applySessionOverlay([AGENT_ADK], [mainSession, threadSession]);
    expect(overlaid[0]).toMatchObject({
      id: "project-agentdesk",
      status: "working",
      agentdesk_working_count: 1,
      session_info: "지금 줄서있는 빌드/ci는 이제 없어?",
      current_thread_channel_id: "1570308839910412348",
      activity_source: "agentdesk",
    });

    const classified = classifyAgents([AGENT_ADK], [mainSession, threadSession], false);
    expect(classified).toHaveLength(1);
    expect(classified[0].state).toBe("working");
    expect(classified[0].info).toBe("지금 줄서있는 빌드/ci는 이제 없어?");

    const homeRoster = buildHomeAgents([AGENT_ADK], [mainSession, threadSession], false, NOW);
    expect(homeRoster.map((entry) => entry.agent.id)).toEqual(["project-agentdesk"]);
    expect(homeRoster[0].state).toBe("working");
    expect(homeRoster.some((entry) => entry.agent.id.startsWith("dispatched:"))).toBe(false);
  });

  it("surfaces unlinked Discord thread sessions in AgentNowSection ('지금') and updates live on pcd-ws-event without reopening", async () => {
    const mainKey = "claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc";
    const mainSession = makeSession({
      id: "10",
      session_key: mainKey,
      linked_agent_id: "project-agentdesk",
      status: "turn_active",
      session_info: "메인 채널 점검 중",
      last_seen_at: NOW - 10_000,
    });
    const threadSession = makeSession({
      id: "11",
      session_key: "claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc-t1570308839910412348",
      linked_agent_id: null,
      thread_channel_id: "1570308839910412348",
      status: "idle",
      session_info: "초기 스레드 대기 질문",
      last_seen_at: NOW - 5_000,
    });

    // /api/agents/{id}/dispatched-sessions only returns explicitly linked mainSession;
    // /api/dispatched-sessions?includeMerged=1 includes the unlinked thread session!
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([mainSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValueOnce([mainSession, threadSession]);

    const turns = new Map<string, HomeTurn>([
      [
        "project-agentdesk",
        {
          status: "working",
          session_key: mainKey,
          started_at: new Date(NOW - 20_000).toISOString(),
          updated_at: new Date(NOW - 2_000).toISOString(),
          provider: "claude",
          current_tool_line: "Read: src/main.rs",
        },
      ],
    ]);

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={{ ...AGENT_ADK, status: "working" }}
            state="working"
            isKo
            locale="ko"
            tr={ko}
            turns={turns}
          />
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();

    const nowList = container.querySelector<HTMLElement>('[data-testid="agent-now-sessions"]')!;
    expect(nowList).not.toBeNull();
    expect(nowList.textContent).toContain("진행·스레드 세션 2");

    const rows = nowList.querySelectorAll<HTMLElement>('[data-testid="agent-now-session-row"]');
    expect(rows).toHaveLength(2);
    const threadRow = Array.from(rows).find((row) => row.textContent?.includes("1570308839910412348"))!;
    expect(threadRow).toBeDefined();
    expect(threadRow.textContent).toContain("#adk-cc-t1570308839910412348");
    expect(threadRow.textContent).toContain("스레드");
    expect(threadRow.textContent).toContain("스레드 대기");
    expect(threadRow.textContent).toContain("초기 스레드 대기 질문");

    // Live WS update flips the unlinked thread session to turn_active without reopening the modal
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: {
            type: "dispatched_session_update",
            payload: {
              ...threadSession,
              status: "turn_active",
              session_info: "스레드 작업 진행 중",
              last_seen_at: NOW,
            },
          },
        }),
      );
    });
    await flush();

    const updatedThreadRow = Array.from(
      container.querySelectorAll<HTMLElement>('[data-testid="agent-now-session-row"]'),
    ).find((row) => row.textContent?.includes("1570308839910412348"))!;
    expect(updatedThreadRow).toBeDefined();
    expect(updatedThreadRow.textContent).toContain("작업 중");
    expect(updatedThreadRow.textContent).toContain("스레드 작업 진행 중");

    // Live WS new thread session appears immediately via parent channel inference
    const secondThread = makeSession({
      id: "12",
      session_key: "codex/discord_123/MacBook-Pro-14:AgentDesk-codex-adk-cc-t1570308839910499999",
      linked_agent_id: null,
      provider: "codex",
      thread_channel_id: "1570308839910499999",
      status: "awaiting_user",
      session_info: "사용자 응답 대기 중",
      last_seen_at: NOW + 1_000,
    });
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_new", payload: secondThread },
        }),
      );
    });
    await flush();
    expect(container.querySelector('[data-testid="agent-now-sessions"]')?.textContent).toContain("진행·스레드 세션 3");
    expect(container.querySelector('[data-testid="agent-now-sessions"]')?.textContent).toContain("입력 대기");

    // Live WS disconnect removes disconnected thread from active/thread list
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_disconnect", payload: { id: "12" } },
        }),
      );
    });
    await flush();
    expect(container.querySelector('[data-testid="agent-now-sessions"]')?.textContent).toContain("진행·스레드 세션 2");
  });

  it("resolves thread channel info in 'Now' even when Connections is folded on mobile and marks stale-idle agent working", async () => {
    setViewport(true);
    const mainSession = makeSession({
      id: "20",
      session_key: "claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc",
      linked_agent_id: "project-agentdesk",
      status: "idle",
    });
    const threadSession = makeSession({
      id: "21",
      session_key: "claude/discord_123/MacBook-Pro-14:AgentDesk-claude-adk-cc-t1570308839910412348",
      linked_agent_id: null,
      thread_channel_id: "1570308839910412348",
      status: "turn_active",
      session_info: "Thread prompt running",
    });

    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([mainSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValueOnce([mainSession, threadSession]);
    vi.mocked(api.getDiscordChannelInfo).mockImplementation(async (id: string) => {
      if (id === "1570308839910412348") {
        return { id, name: "CI queue check thread", guild_id: "1490141479707086938", parent_id: "1490145076658307999" };
      }
      if (id === "1490145076658307999") {
        return { id, name: "adk-cc", guild_id: "1490141479707086938", parent_id: null };
      }
      return null as never;
    });

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={AGENT_ADK}
            isKo={false}
            locale="en"
            tr={en}
          />
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();
    await flush();

    // Connections is folded on mobile, yet thread channel title + parent resolved in Now!
    const nowList = container.querySelector<HTMLElement>('[data-testid="agent-now-sessions"]')!;
    expect(nowList).not.toBeNull();
    expect(nowList.textContent).toContain("CI queue check thread · #adk-cc");
    expect(nowList.textContent).toContain("Thread");
    expect(nowList.textContent).toContain("Working");
    expect(nowList.textContent).toContain("Thread prompt running");
    // Stale idle agent snapshot flipped to Working because workingSessions.length > 0
    expect(container.querySelector('[data-testid="agent-detail-now"] dd')?.textContent).toContain("Working");
    expect(container.querySelector('[data-testid="agent-stop-turn"]')).not.toBeNull();
  });
});
