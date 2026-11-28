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
  mergeRawSessions,
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

const AGENT_ALPHA: Agent = {
  id: "agent-alpha",
  name: "Alpha Agent",
  name_ko: "알파 에이전트",
  department_id: null,
  cli_provider: "claude",
  avatar_emoji: "🤖",
  personality: null,
  status: "idle",
  stats_tasks_done: 14,
  stats_tokens: 2400,
  created_at: NOW - 86_400_000,
};

const AGENT_BETA: Agent = {
  id: "agent-beta",
  name: "Beta Agent",
  name_ko: "베타 에이전트",
  department_id: null,
  cli_provider: "codex",
  avatar_emoji: "🛠️",
  personality: null,
  status: "idle",
  stats_tasks_done: 8,
  stats_tokens: 1200,
  created_at: NOW - 86_400_000,
};

const ALPHA_MAIN_KEY = "claude/discord_fixture01/fixture-host-a:AgentDesk-claude-alpha-chan";
const ALPHA_THREAD_KEY = "claude/discord_fixture01/fixture-host-a:AgentDesk-claude-alpha-chan-t1000000000000000002";
const ALPHA_SECOND_THREAD_KEY = "codex/discord_fixture01/fixture-host-a:AgentDesk-codex-alpha-chan-t1000000000000000003";

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

function createDeferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
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
    const mainSession = makeSession({
      id: "sess-main",
      session_key: ALPHA_MAIN_KEY,
      linked_agent_id: "agent-alpha",
      status: "turn_active",
      session_info: "Synthetic main turn summary",
    });
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValue([mainSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValue([mainSession]);

    const turns = new Map<string, HomeTurn>([
      [
        "agent-alpha",
        {
          status: "working",
          session_key: ALPHA_MAIN_KEY,
          started_at: new Date(NOW - 12_000).toISOString(),
          updated_at: new Date(NOW - 1_000).toISOString(),
          provider: "claude",
          current_tool_line: "Bash: npm test",
          tool_events: [
            { kind: "tool", status: "running", tool_name: "Bash", summary: "Run unit suite", line: "npm test" },
          ],
        },
      ],
    ]);

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={{ ...AGENT_ALPHA, status: "working" }}
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

  it("infers parent agent for unlinked Discord thread sessions with namespace-aware slug isolation and no orphan pseudo-agents", () => {
    const mainSession = makeSession({
      id: "s-main",
      session_key: ALPHA_MAIN_KEY,
      linked_agent_id: "agent-alpha",
      status: "idle",
      last_seen_at: NOW - 120_000,
    });
    const threadSession = makeSession({
      id: "s-thread",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: null,
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Synthetic thread task summary",
      last_seen_at: NOW - 2_000,
    });

    expect(isThreadSession(mainSession)).toBe(false);
    expect(isThreadSession(threadSession)).toBe(true);
    expect(
      stripThreadSuffixFromSessionKey(`${threadSession.session_key}:child:toolu_01Fixture`),
    ).toBe(ALPHA_MAIN_KEY);
    expect(
      extractParentChannelSlugFromThreadSessionKey(`${threadSession.session_key}:child:toolu_01Fixture`),
    ).toBe("alpha-chan");
    expect(extractParentChannelSlugFromThreadSessionKey(mainSession.session_key)).toBeNull();

    const agentMap = buildEffectiveSessionAgentMap([mainSession, threadSession]);
    expect(agentMap.get("s-thread")).toBe("agent-alpha");

    // Namespace-aware slug isolation: same channel slug in two distinct namespaces maps to each namespace's own agent
    const nsBetaMain = makeSession({
      id: "s-beta-main",
      session_key: "claude/discord_fixture02/fixture-host-b:AgentDesk-claude-shared-chan",
      linked_agent_id: "agent-beta",
      status: "idle",
    });
    const nsAlphaMain = makeSession({
      id: "s-alpha-shared",
      session_key: "claude/discord_fixture01/fixture-host-a:AgentDesk-claude-shared-chan",
      linked_agent_id: "agent-alpha",
      status: "idle",
    });
    const nsBetaThreadDiffHostKey = makeSession({
      id: "s-beta-thread",
      session_key: "claude/discord_fixture02/fixture-host-b:tmux:AgentDesk-claude-shared-chan-t1000000000000000005",
      linked_agent_id: null,
      thread_channel_id: "1000000000000000005",
      status: "turn_active",
    });
    const nsThirdUnknownThread = makeSession({
      id: "s-third-thread",
      session_key: "claude/discord_fixture03/fixture-host-c:AgentDesk-claude-shared-chan-t1000000000000000006",
      linked_agent_id: null,
      thread_channel_id: "1000000000000000006",
      status: "turn_active",
    });
    const nsMap = buildEffectiveSessionAgentMap([
      nsAlphaMain,
      nsBetaMain,
      nsBetaThreadDiffHostKey,
      nsThirdUnknownThread,
    ]);
    expect(nsMap.get("s-beta-thread")).toBe("agent-beta");
    expect(nsMap.get("s-third-thread")).toBeUndefined();

    // Binding hint fallback when sibling parent session is absent
    const fromBinding = withInferredLinkedAgents([threadSession], [
      { agentId: "agent-alpha", channelId: "1000000000000000001", channelName: "#alpha-chan" },
    ]);
    expect(fromBinding[0].linked_agent_id).toBe("agent-alpha");
    expect(fromBinding[0].inferred_linked_agent).toBe(true);

    const overlaid = applySessionOverlay([AGENT_ALPHA], [mainSession, threadSession]);
    expect(overlaid[0]).toMatchObject({
      id: "agent-alpha",
      status: "working",
      agentdesk_working_count: 1,
      session_info: "Synthetic thread task summary",
      current_thread_channel_id: "1000000000000000002",
      activity_source: "agentdesk",
    });

    const classified = classifyAgents([AGENT_ALPHA], [mainSession, threadSession], false);
    expect(classified).toHaveLength(1);
    expect(classified[0].state).toBe("working");
    expect(classified[0].info).toBe("Synthetic thread task summary");

    const homeRoster = buildHomeAgents([AGENT_ALPHA], [mainSession, threadSession], false, NOW);
    expect(homeRoster.map((entry) => entry.agent.id)).toEqual(["agent-alpha"]);
    expect(homeRoster[0].state).toBe("working");
    expect(homeRoster.some((entry) => entry.agent.id.startsWith("dispatched:"))).toBe(false);
  });

  it("preserves explicit linked_agent_id over parent-inferred attribution regardless of order and re-filters live on WS reassignment", async () => {
    const parentAlpha = makeSession({
      id: "p-alpha",
      session_key: ALPHA_MAIN_KEY,
      linked_agent_id: "agent-alpha",
      status: "idle",
      last_seen_at: NOW - 30_000,
    });
    const explicitCopy = makeSession({
      id: "shared-thread-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-beta",
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Explicit beta assignment",
      last_seen_at: NOW - 10_000,
    });
    const unlinkedNewerCopy = makeSession({
      id: "shared-thread-1-alias",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: null,
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Fresher runtime summary",
      last_seen_at: NOW - 1_000,
    });

    // Order 1: [explicitFirst, unlinkedSecond]
    const forward = withInferredLinkedAgents([parentAlpha, explicitCopy, unlinkedNewerCopy]);
    // Order 2: [unlinkedFirst, explicitSecond]
    const reverse = withInferredLinkedAgents([parentAlpha, unlinkedNewerCopy, explicitCopy]);

    // Unified by session_key (no duplicate rows) and explicit "agent-beta" wins in BOTH orders!
    expect(forward).toHaveLength(2);
    expect(reverse).toHaveLength(2);
    const resolvedForward = forward.find((s) => s.session_key === ALPHA_THREAD_KEY)!;
    const resolvedReverse = reverse.find((s) => s.session_key === ALPHA_THREAD_KEY)!;
    expect(resolvedForward.linked_agent_id).toBe("agent-beta");
    expect(resolvedForward.inferred_linked_agent).toBe(false);
    expect(resolvedForward.session_info).toBe("Fresher runtime summary");
    expect(resolvedReverse.linked_agent_id).toBe("agent-beta");
    expect(resolvedReverse.inferred_linked_agent).toBe(false);
    expect(resolvedReverse.session_info).toBe("Fresher runtime summary");
    expect(mergeRawSessions([explicitCopy, unlinkedNewerCopy])).toHaveLength(1);
    expect(mergeRawSessions([unlinkedNewerCopy, explicitCopy])).toHaveLength(1);

    // Live WS reassignment A -> B across open AgentInfoCardContent views
    const movingSession = makeSession({
      id: "move-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-alpha",
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Initially assigned to alpha",
      last_seen_at: NOW - 5_000,
    });

    vi.mocked(api.getAgentDispatchedSessions).mockImplementation(async (agentId: string) =>
      agentId === "agent-alpha" ? [movingSession] : [],
    );
    vi.mocked(api.getDispatchedSessions).mockResolvedValue([movingSession]);

    act(() => {
      root.render(
        <MemoryRouter>
          <div>
            <div data-testid="alpha-detail">
              <AgentInfoCardContent agent={AGENT_ALPHA} isKo={false} locale="en" tr={en} />
            </div>
            <div data-testid="beta-detail">
              <AgentInfoCardContent agent={AGENT_BETA} isKo={false} locale="en" tr={en} />
            </div>
          </div>
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();

    const alphaPane = container.querySelector<HTMLElement>('[data-testid="alpha-detail"]')!;
    const betaPane = container.querySelector<HTMLElement>('[data-testid="beta-detail"]')!;
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);

    // Emit WS update reassigning movingSession from agent-alpha to agent-beta
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: {
            type: "dispatched_session_update",
            payload: {
              ...movingSession,
              linked_agent_id: "agent-beta",
              session_info: "Reassigned to beta",
              last_seen_at: NOW,
            },
          },
        }),
      );
    });
    await flush();

    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(betaPane.textContent).toContain("Reassigned to beta");
  });

  it("reconciles silent server thread_gc deletions on focus/poll without clobbering concurrent in-flight WS events", async () => {
    const gcThreadSession = makeSession({
      id: "gc-thread-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-alpha",
      thread_channel_id: "1000000000000000002",
      status: "idle",
      session_info: "Idle thread before GC",
    });

    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([gcThreadSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValueOnce([gcThreadSession]);

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent agent={AGENT_ALPHA} isKo={false} locale="en" tr={en} />
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();

    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);

    // Simulate silent server thread_gc deletion (both endpoints now return []) and focus refresh
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValueOnce([]);

    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    await flush();
    await flush();

    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);

    // Now test in-flight REST vs concurrent WS race:
    // Start a slow REST refresh on focus, emit a WS dispatched_session_new while pending, then resolve stale REST []
    const slowAgentFetch = createDeferred<DispatchedSession[]>();
    const slowGlobalFetch = createDeferred<DispatchedSession[]>();
    vi.mocked(api.getAgentDispatchedSessions).mockReturnValueOnce(slowAgentFetch.promise);
    vi.mocked(api.getDispatchedSessions).mockReturnValueOnce(slowGlobalFetch.promise);

    act(() => {
      window.dispatchEvent(new Event("focus"));
    });

    const concurrentWsSession = makeSession({
      id: "ws-inflight-1",
      session_key: ALPHA_SECOND_THREAD_KEY,
      linked_agent_id: "agent-alpha",
      provider: "codex",
      thread_channel_id: "1000000000000000003",
      status: "turn_active",
      session_info: "Arrived via WS during slow REST",
      last_seen_at: NOW + 5_000,
    });
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_new", payload: concurrentWsSession },
        }),
      );
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);

    // Resolve the older in-flight REST call with stale empty snapshot -> concurrent WS session MUST survive!
    await act(async () => {
      slowAgentFetch.resolve([]);
      slowGlobalFetch.resolve([]);
    });
    await flush();

    const survivingRows = container.querySelectorAll('[data-testid="agent-now-session-row"]');
    expect(survivingRows).toHaveLength(1);
    expect(survivingRows[0].textContent).toContain("Arrived via WS during slow REST");
  });

  it("surfaces unlinked Discord thread sessions in AgentNowSection ('지금') and updates live on pcd-ws-event without reopening", async () => {
    const mainSession = makeSession({
      id: "10",
      session_key: ALPHA_MAIN_KEY,
      linked_agent_id: "agent-alpha",
      status: "turn_active",
      session_info: "Synthetic main channel check",
      last_seen_at: NOW - 10_000,
    });
    const threadSession = makeSession({
      id: "11",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: null,
      thread_channel_id: "1000000000000000002",
      status: "idle",
      session_info: "Initial synthetic thread prompt",
      last_seen_at: NOW - 5_000,
    });

    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValue([mainSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValue([mainSession, threadSession]);

    const turns = new Map<string, HomeTurn>([
      [
        "agent-alpha",
        {
          status: "working",
          session_key: ALPHA_MAIN_KEY,
          started_at: new Date(NOW - 20_000).toISOString(),
          updated_at: new Date(NOW - 2_000).toISOString(),
          provider: "claude",
          current_tool_line: "Read: README.md",
        },
      ],
    ]);

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={{ ...AGENT_ALPHA, status: "working" }}
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
    const threadRow = Array.from(rows).find((row) => row.textContent?.includes("1000000000000000002"))!;
    expect(threadRow).toBeDefined();
    expect(threadRow.textContent).toContain("#alpha-chan-t1000000000000000002");
    expect(threadRow.textContent).toContain("스레드");
    expect(threadRow.textContent).toContain("스레드 대기");
    expect(threadRow.textContent).toContain("Initial synthetic thread prompt");

    // Live WS update flips the unlinked thread session to turn_active without reopening
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: {
            type: "dispatched_session_update",
            payload: {
              ...threadSession,
              status: "turn_active",
              session_info: "Updated synthetic thread step",
              last_seen_at: NOW,
            },
          },
        }),
      );
    });
    await flush();

    const updatedThreadRow = Array.from(
      container.querySelectorAll<HTMLElement>('[data-testid="agent-now-session-row"]'),
    ).find((row) => row.textContent?.includes("1000000000000000002"))!;
    expect(updatedThreadRow).toBeDefined();
    expect(updatedThreadRow.textContent).toContain("작업 중");
    expect(updatedThreadRow.textContent).toContain("Updated synthetic thread step");

    // Live WS new thread session appears immediately via parent channel inference
    const secondThread = makeSession({
      id: "12",
      session_key: ALPHA_SECOND_THREAD_KEY,
      linked_agent_id: null,
      provider: "codex",
      thread_channel_id: "1000000000000000003",
      status: "awaiting_user",
      session_info: "Synthetic awaiting input state",
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

  it("resolves thread channel info on mobile while Connections is folded and hides Stop Turn when only inferred unlinked thread sessions run", async () => {
    setViewport(true);
    const mainSession = makeSession({
      id: "20",
      session_key: ALPHA_MAIN_KEY,
      linked_agent_id: "agent-alpha",
      status: "idle",
    });
    const threadSession = makeSession({
      id: "21",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: null,
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Synthetic thread task summary",
    });

    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValue([mainSession]);
    vi.mocked(api.getDispatchedSessions).mockResolvedValue([mainSession, threadSession]);
    vi.mocked(api.getDiscordChannelInfo).mockImplementation(async (id: string) => {
      if (id === "1000000000000000002") {
        return { id, name: "Synthetic thread title", guild_id: "1000000000000000099", parent_id: "1000000000000000001" };
      }
      if (id === "1000000000000000001") {
        return { id, name: "alpha-chan", guild_id: "1000000000000000099", parent_id: null };
      }
      return null as never;
    });

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={AGENT_ALPHA}
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
    expect(nowList.textContent).toContain("Synthetic thread title · #alpha-chan");
    expect(nowList.textContent).toContain("Thread");
    expect(nowList.textContent).toContain("Working");
    expect(nowList.textContent).toContain("Synthetic thread task summary");
    // Stale idle agent snapshot flipped to Working because workingSessions.length > 0,
    // while Stop Turn stays hidden because the only working session is frontend-inferred (s.agent_id IS NULL in DB).
    expect(container.querySelector('[data-testid="agent-detail-now"] dd')?.textContent).toContain("Working");
    expect(container.querySelector('[data-testid="agent-stop-turn"]')).toBeNull();
  });

  it("preserves associativity and permutation invariance across all 6 permutations when merging 3+ copies with unattributed newer timestamps", () => {
    const t1 = NOW - 20_000;
    const t2 = NOW - 10_000;
    const t3 = NOW - 1_000;

    const explicitAlphaT1 = makeSession({
      id: "assoc-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-alpha",
      thread_channel_id: "1000000000000000002",
      status: "idle",
      session_info: "Alpha copy at t1",
      last_seen_at: t1,
    });
    const unattributedT3 = makeSession({
      id: "assoc-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: null,
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Unattributed runtime update at t3",
      last_seen_at: t3,
    });
    const explicitBetaT1 = makeSession({
      id: "assoc-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-beta",
      thread_channel_id: "1000000000000000002",
      status: "working",
      session_info: "Beta copy at t1",
      last_seen_at: t1,
    });

    const tiedPermutations: DispatchedSession[][] = [
      [explicitAlphaT1, unattributedT3, explicitBetaT1],
      [explicitAlphaT1, explicitBetaT1, unattributedT3],
      [unattributedT3, explicitAlphaT1, explicitBetaT1],
      [unattributedT3, explicitBetaT1, explicitAlphaT1],
      [explicitBetaT1, explicitAlphaT1, unattributedT3],
      [explicitBetaT1, unattributedT3, explicitAlphaT1],
    ];

    for (const perm of tiedPermutations) {
      const merged = mergeRawSessions(perm);
      expect(merged).toHaveLength(1);
      expect(merged[0].linked_agent_id).toBe("agent-alpha");
      expect(merged[0].inferred_linked_agent).toBe(false);
      expect(merged[0].status).toBe("turn_active");
      expect(merged[0].session_info).toBe("Unattributed runtime update at t3");
      expect(merged[0].last_seen_at).toBe(t3);
    }

    // Also verify when explicitBeta has newer owner timestamp t2 (t1 < t2 < t3):
    // unattributedT3 must NOT inflate explicitAlphaT1's owner provenance above explicitBetaT2!
    const explicitBetaT2 = { ...explicitBetaT1, last_seen_at: t2, session_info: "Beta copy at t2" };
    const staggeredPermutations: DispatchedSession[][] = [
      [explicitAlphaT1, unattributedT3, explicitBetaT2],
      [explicitAlphaT1, explicitBetaT2, unattributedT3],
      [unattributedT3, explicitAlphaT1, explicitBetaT2],
      [unattributedT3, explicitBetaT2, explicitAlphaT1],
      [explicitBetaT2, explicitAlphaT1, unattributedT3],
      [explicitBetaT2, unattributedT3, explicitAlphaT1],
    ];

    for (const perm of staggeredPermutations) {
      const merged = mergeRawSessions(perm);
      expect(merged).toHaveLength(1);
      expect(merged[0].linked_agent_id).toBe("agent-beta");
      expect(merged[0].inferred_linked_agent).toBe(false);
      expect(merged[0].status).toBe("turn_active");
      expect(merged[0].session_info).toBe("Unattributed runtime update at t3");
      expect(merged[0].last_seen_at).toBe(t3);
    }
  });
});
