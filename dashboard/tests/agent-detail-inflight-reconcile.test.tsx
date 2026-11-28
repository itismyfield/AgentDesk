// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { getDispatchedSessions } from "../src/api/agentActivity";
import { beginAuthentication, getAuthSnapshot, resolveAuthentication } from "../src/api/authState";
import { AgentInfoCardContent } from "../src/components/agent-manager/AgentInfoCardContent";
import type { Agent, DispatchedSession } from "../src/types";

// Keep ../src/api, ../src/api/agentActivity, and ../src/api/httpClient 100% REAL so
// httpClient.request() and its module-level inflightGets map execute end-to-end.
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
  stats_tasks_done: 5,
  stats_tokens: 1000,
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
  stats_tasks_done: 3,
  stats_tokens: 500,
  created_at: NOW - 86_400_000,
};

const ALPHA_MAIN_KEY = "claude/discord_fixture01/fixture-host-a:AgentDesk-claude-alpha-chan";
const ALPHA_THREAD_KEY = "claude/discord_fixture01/fixture-host-a:AgentDesk-claude-alpha-chan-t1000000000000000002";

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

function jsonResponse(payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}

interface PendingFetch {
  url: string;
  resolveJson: (payload: unknown) => void;
}

function installControlledFetch() {
  const queues = new Map<string, PendingFetch[]>();
  const callCounts = new Map<string, number>();

  const fetchMock = vi.fn((input: RequestInfo | URL) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    callCounts.set(url, (callCounts.get(url) ?? 0) + 1);

    if (url.startsWith("/api/agents/") && url.endsWith("/dispatched-sessions")) {
      return new Promise<Response>((resolve) => {
        const list = queues.get(url) ?? [];
        list.push({ url, resolveJson: (payload) => resolve(jsonResponse(payload)) });
        queues.set(url, list);
      });
    }
    if (url.startsWith("/api/dispatched-sessions")) {
      return new Promise<Response>((resolve) => {
        const list = queues.get(url) ?? [];
        list.push({ url, resolveJson: (payload) => resolve(jsonResponse(payload)) });
        queues.set(url, list);
      });
    }
    if (url.endsWith("/skills")) {
      return Promise.resolve(jsonResponse({ skills: [], sharedSkills: [] }));
    }
    if (url.startsWith("/api/discord-bindings")) {
      return Promise.resolve(jsonResponse({ bindings: [] }));
    }
    if (url.startsWith("/api/discord/channels/")) {
      return Promise.resolve(jsonResponse({ id: null }));
    }
    return Promise.resolve(jsonResponse({}));
  });

  vi.stubGlobal("fetch", fetchMock);

  return {
    fetchMock,
    countFor: (url: string) => callCounts.get(url) ?? 0,
    pendingFor: (url: string) => queues.get(url) ?? [],
  };
}

async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

let root: Root;
let container: HTMLDivElement;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: false,
    media: query,
    addEventListener: () => {},
    removeEventListener: () => {},
    addListener: () => {},
    removeListener: () => {},
    onchange: null,
    dispatchEvent: () => false,
  }));
  if (typeof globalThis.ResizeObserver === "undefined") {
    vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  }
  if (!Element.prototype.scrollIntoView) {
    Element.prototype.scrollIntoView = () => {};
  }
  // Reset real httpClient module caches & inflightGets cleanly before every test.
  beginAuthentication(null);
  resolveAuthentication(getAuthSnapshot().generation, true, false);

  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  document.body.innerHTML = "";
  beginAuthentication(null);
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});

describe("Real httpClient + useAgentDetailData in-flight GET reconciliation races", () => {
  const ALPHA_URL = "/api/agents/agent-alpha/dispatched-sessions";
  const BETA_URL = "/api/agents/agent-beta/dispatched-sessions";
  const GLOBAL_URL = "/api/dispatched-sessions?includeMerged=1";

  it("Scenario 1: R1 -> WS dispatched_session_new -> focus R2 -> stale R1 resolves does NOT wipe out the newer WS thread session", async () => {
    const net = installControlledFetch();

    // Simulate an external caller (e.g. global workspace poll) ALSO holding a shared in-flight GET in inflightGets.
    const externalPollPromise = getDispatchedSessions(true);
    expect(net.countFor(GLOBAL_URL)).toBe(1);

    // Mount Agent Detail -> R1 starts (must bypass externalPollPromise via shareInflight: false -> count becomes 2).
    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent agent={AGENT_ALPHA} isKo={false} locale="en" tr={en} />
        </MemoryRouter>,
      );
    });
    expect(net.countFor(ALPHA_URL)).toBe(1);
    expect(net.countFor(GLOBAL_URL)).toBe(2);

    // Resolve initial mount R1 with only idle parent session so restSnapshot is initialized,
    // then start a slow background R1b before WS arrives.
    const parentIdle = makeSession({
      id: "parent-1",
      session_key: ALPHA_MAIN_KEY,
      linked_agent_id: "agent-alpha",
      status: "idle",
    });
    await act(async () => {
      net.pendingFor(ALPHA_URL)[0].resolveJson({ sessions: [parentIdle] });
      net.pendingFor(GLOBAL_URL)[1].resolveJson({ sessions: [parentIdle] });
    });
    await flush();

    // Start R1b (focus #1) BEFORE WS mutation (fetchStartSeq = 0).
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(2);
    expect(net.countFor(GLOBAL_URL)).toBe(3);

    // WS mutation arrives while R1b (and externalPollPromise) are still in-flight (mutationSeq = 1).
    const wsNewThread = makeSession({
      id: "thread-new-1",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: null,
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Live synthetic thread turn",
      last_seen_at: NOW + 2_000,
    });
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_new", payload: wsNewThread },
        }),
      );
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(container.textContent).toContain("Live synthetic thread turn");

    // Focus #2 triggers R2 (fetchStartSeq = 1) while R1b is STILL in-flight.
    // Because shareInflight: false is passed, httpClient MUST issue brand-new fetch calls (#3 and #4), NOT reuse R1b!
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(3);
    expect(net.countFor(GLOBAL_URL)).toBe(4);

    // Stale R1b (and externalPollPromise) resolve FIRST with pre-WS state (no wsNewThread)!
    await act(async () => {
      net.pendingFor(GLOBAL_URL)[0].resolveJson({ sessions: [parentIdle] });
      await externalPollPromise;
      net.pendingFor(ALPHA_URL)[1].resolveJson({ sessions: [parentIdle] });
      net.pendingFor(GLOBAL_URL)[2].resolveJson({ sessions: [parentIdle] });
    });
    await flush();

    // wsNewThread MUST survive stale R1b resolution!
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(container.textContent).toContain("Live synthetic thread turn");

    // Finally R2 resolves with post-WS server state -> wsNewThread stays rendered cleanly.
    await act(async () => {
      net.pendingFor(ALPHA_URL)[2].resolveJson({ sessions: [parentIdle] });
      net.pendingFor(GLOBAL_URL)[3].resolveJson({ sessions: [parentIdle, wsNewThread] });
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(container.textContent).toContain("Live synthetic thread turn");
  });

  it("Scenario 2: R1 -> WS A->B reassignment -> focus R2 resolves -> stale R1 resolves late keeps A at 0 and B at 1", async () => {
    const net = installControlledFetch();

    const sessionOnAlpha = makeSession({
      id: "move-99",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-alpha",
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Assigned to alpha before WS",
      last_seen_at: NOW - 5_000,
    });
    const sessionOnBeta: DispatchedSession = {
      ...sessionOnAlpha,
      linked_agent_id: "agent-beta",
      session_info: "Reassigned to beta",
      last_seen_at: NOW + 1_000,
    };

    // Mount both Alpha and Beta detail panes; initial mount GETs (index 0 for Alpha, 0 for Beta, 0 & 1 for Global) start.
    act(() => {
      root.render(
        <MemoryRouter>
          <div>
            <div data-testid="alpha-pane">
              <AgentInfoCardContent agent={AGENT_ALPHA} isKo={false} locale="en" tr={en} />
            </div>
            <div data-testid="beta-pane">
              <AgentInfoCardContent agent={AGENT_BETA} isKo={false} locale="en" tr={en} />
            </div>
          </div>
        </MemoryRouter>,
      );
    });
    await act(async () => {
      net.pendingFor(ALPHA_URL)[0].resolveJson({ sessions: [sessionOnAlpha] });
      net.pendingFor(BETA_URL)[0].resolveJson({ sessions: [] });
      net.pendingFor(GLOBAL_URL)[0].resolveJson({ sessions: [sessionOnAlpha] });
      net.pendingFor(GLOBAL_URL)[1].resolveJson({ sessions: [sessionOnAlpha] });
    });
    await flush();

    const alphaPane = container.querySelector<HTMLElement>('[data-testid="alpha-pane"]')!;
    const betaPane = container.querySelector<HTMLElement>('[data-testid="beta-pane"]')!;
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);

    // Focus #1 starts R1 while server still has sessionOnAlpha.
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(2);
    expect(net.countFor(BETA_URL)).toBe(2);
    expect(net.countFor(GLOBAL_URL)).toBe(4);

    // WS reassigns session A -> B while R1 is in-flight!
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_update", payload: sessionOnBeta },
        }),
      );
    });
    await flush();
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);

    // Focus #2 starts R2 while R1 is still in-flight (shareInflight: false creates distinct requests).
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(3);
    expect(net.countFor(BETA_URL)).toBe(3);
    expect(net.countFor(GLOBAL_URL)).toBe(6);

    // R2 resolves FIRST with post-reassignment state, then stale R1 resolves LATE with sessionOnAlpha!
    await act(async () => {
      net.pendingFor(ALPHA_URL)[2].resolveJson({ sessions: [] });
      net.pendingFor(BETA_URL)[2].resolveJson({ sessions: [sessionOnBeta] });
      net.pendingFor(GLOBAL_URL)[4].resolveJson({ sessions: [sessionOnBeta] });
      net.pendingFor(GLOBAL_URL)[5].resolveJson({ sessions: [sessionOnBeta] });
    });
    await flush();
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);

    // Late stale R1 arrives with sessionOnAlpha -> must be discarded without resurrecting on Alpha!
    await act(async () => {
      net.pendingFor(ALPHA_URL)[1].resolveJson({ sessions: [sessionOnAlpha] });
      net.pendingFor(BETA_URL)[1].resolveJson({ sessions: [] });
      net.pendingFor(GLOBAL_URL)[2].resolveJson({ sessions: [sessionOnAlpha] });
      net.pendingFor(GLOBAL_URL)[3].resolveJson({ sessions: [sessionOnAlpha] });
    });
    await flush();
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(betaPane.textContent).toContain("Reassigned to beta");

    // Also verify the reverse completion order: R3 starts -> WS B->A reassignment -> R4 starts -> stale R3 resolves FIRST!
    const sessionBackOnAlpha: DispatchedSession = {
      ...sessionOnBeta,
      linked_agent_id: "agent-alpha",
      session_info: "Returned to alpha",
      last_seen_at: NOW + 3_000,
    };
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_update", payload: sessionBackOnAlpha },
        }),
      );
    });
    await flush();
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(5);
    expect(net.countFor(BETA_URL)).toBe(5);

    // Stale R3 resolves BEFORE R4 -> without shareInflight: false, R4 would share R3 and prune WS B->A!
    await act(async () => {
      net.pendingFor(ALPHA_URL)[3].resolveJson({ sessions: [] });
      net.pendingFor(BETA_URL)[3].resolveJson({ sessions: [sessionOnBeta] });
      net.pendingFor(GLOBAL_URL)[6].resolveJson({ sessions: [sessionOnBeta] });
      net.pendingFor(GLOBAL_URL)[7].resolveJson({ sessions: [sessionOnBeta] });
    });
    await flush();
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);

    await act(async () => {
      net.pendingFor(ALPHA_URL)[4].resolveJson({ sessions: [sessionBackOnAlpha] });
      net.pendingFor(BETA_URL)[4].resolveJson({ sessions: [] });
      net.pendingFor(GLOBAL_URL)[8].resolveJson({ sessions: [sessionBackOnAlpha] });
      net.pendingFor(GLOBAL_URL)[9].resolveJson({ sessions: [sessionBackOnAlpha] });
    });
    await flush();
    expect(alphaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(betaPane.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
  });

  it("Scenario 3: R1 -> WS disconnect -> focus R2 -> stale R1 resolves does NOT resurrect disconnected session, while newer reconnect WS update succeeds", async () => {
    const net = installControlledFetch();

    const activeThread = makeSession({
      id: "77",
      session_key: ALPHA_THREAD_KEY,
      linked_agent_id: "agent-alpha",
      thread_channel_id: "1000000000000000002",
      status: "turn_active",
      session_info: "Running before disconnect",
    });

    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent agent={AGENT_ALPHA} isKo={false} locale="en" tr={en} />
        </MemoryRouter>,
      );
    });
    await act(async () => {
      net.pendingFor(ALPHA_URL)[0].resolveJson({ sessions: [activeThread] });
      net.pendingFor(GLOBAL_URL)[0].resolveJson({ sessions: [activeThread] });
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(container.querySelector('[data-testid="agent-detail-now"] dd')?.textContent).toContain("Working");

    // R1 starts on focus #1 BEFORE disconnect (server read activeThread as turn_active).
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(2);
    expect(net.countFor(GLOBAL_URL)).toBe(2);

    // WS disconnect arrives by id only while R1 is in-flight!
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: { type: "dispatched_session_disconnect", payload: { id: "77" } },
        }),
      );
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
    expect(container.querySelector('[data-testid="agent-detail-now"] dd')?.textContent).toContain("Idle");

    // Focus #2 triggers R2 while R1 is still in-flight (shareInflight: false issues new GETs #3).
    act(() => {
      window.dispatchEvent(new Event("focus"));
    });
    expect(net.countFor(ALPHA_URL)).toBe(3);
    expect(net.countFor(GLOBAL_URL)).toBe(3);

    // Stale R1 resolves first with activeThread still turn_active -> MUST NOT resurrect!
    await act(async () => {
      net.pendingFor(ALPHA_URL)[1].resolveJson({ sessions: [activeThread] });
      net.pendingFor(GLOBAL_URL)[1].resolveJson({ sessions: [activeThread] });
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(0);
    expect(container.querySelector('[data-testid="agent-detail-now"] dd')?.textContent).toContain("Idle");

    // Before R2 resolves, suppose a newer WS reconnect update arrives keyed by session_key only:
    // sequence-aware disconnect handling (disc.seq < upsert.seq) must allow the reconnect to surface!
    act(() => {
      window.dispatchEvent(
        new CustomEvent("pcd-ws-event", {
          detail: {
            type: "dispatched_session_update",
            payload: {
              session_key: ALPHA_THREAD_KEY,
              linked_agent_id: "agent-alpha",
              thread_channel_id: "1000000000000000002",
              status: "turn_active",
              session_info: "Reconnected synthetic turn",
              last_seen_at: NOW + 5_000,
            },
          },
        }),
      );
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(container.textContent).toContain("Reconnected synthetic turn");

    // R2 (started BEFORE the reconnect WS update, after the disconnect) resolves empty ->
    // reconnect WS update (seq > R2.fetchStartSeq) still survives!
    await act(async () => {
      net.pendingFor(ALPHA_URL)[2].resolveJson({ sessions: [] });
      net.pendingFor(GLOBAL_URL)[2].resolveJson({ sessions: [] });
    });
    await flush();
    expect(container.querySelectorAll('[data-testid="agent-now-session-row"]')).toHaveLength(1);
    expect(container.textContent).toContain("Reconnected synthetic turn");
  });
});
