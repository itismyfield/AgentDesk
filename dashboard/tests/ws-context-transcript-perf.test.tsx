// @vitest-environment happy-dom
import { act, memo, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { SessionTranscript, SessionTranscriptEvent } from "../src/api";
import { beginAuthentication } from "../src/api/authState";
import { getDashboardSocketTicket } from "../src/api/dashboardAuth";
import * as apiClient from "../src/api/client";
import { isNumericEventId, useDashboardSocket } from "../src/app/useDashboardSocket";
import { COPY_WELL_PREVIEW_LIMIT, CopyWell } from "../src/components/agent-manager/CardDetailParts";
import TurnTranscriptPanel, { buildTurnToolSummary } from "../src/components/agent-manager/TurnTranscriptPanel";
import { KanbanProvider, useKanban } from "../src/contexts/KanbanContext";
import { SettingsProvider, useSettings } from "../src/contexts/SettingsContext";
import { WorkspaceProvider, useWorkspace } from "../src/contexts/WorkspaceContext";
import type { Agent, CompanySettings, DashboardStats, DispatchedSession, KanbanCard, WSEvent } from "../src/types";

const getCardTranscripts = vi.fn();
const getAgentTranscripts = vi.fn();

vi.mock("../src/api/dashboardAuth", () => ({
  getDashboardSocketTicket: vi.fn(),
}));

vi.mock("../src/api/client", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../src/api/client")>();
  return {
    ...actual,
    getAgents: vi.fn(async () => []),
    getDispatchedSessions: vi.fn(async () => []),
    getStats: vi.fn(async () => null),
    getKanbanCards: vi.fn(async () => []),
    getTaskDispatches: vi.fn(async () => []),
  };
});

vi.mock("../src/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../src/api")>();
  return {
    ...actual,
    getCardTranscripts: (...args: unknown[]) => getCardTranscripts(...args),
    getAgentTranscripts: (...args: unknown[]) => getAgentTranscripts(...args),
  };
});

vi.mock("@tanstack/react-virtual", () => ({
  useVirtualizer: ({ count }: { count: number }) => ({
    getTotalSize: () => count * 44,
    getVirtualItems: () => Array.from({ length: count }, (_, index) => ({ index, key: index, start: index * 44, size: 44 })),
    measureElement: () => {},
  }),
}));

function createMemoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => {
      values.delete(key);
    },
    setItem: (key, value) => {
      values.set(key, String(value));
    },
  };
}

class MockWebSocket {
  static instances: MockWebSocket[] = [];
  readonly url: string;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;
  closed = false;

  constructor(url: string) {
    this.url = url;
    MockWebSocket.instances.push(this);
  }

  emitOpen() {
    this.onopen?.();
  }

  emitMessage(payload: unknown) {
    this.onmessage?.({ data: JSON.stringify(payload) });
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    this.onclose?.();
  }
}

const tr = (_ko: string, en: string) => en;

describe("useDashboardSocket performance & replay contract", () => {
  let container: HTMLDivElement;
  let root: Root;
  let storage: Storage;

  beforeEach(() => {
    MockWebSocket.instances = [];
    storage = createMemoryStorage();
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    vi.stubGlobal("WebSocket", MockWebSocket);
    vi.stubGlobal("localStorage", storage);
    Object.defineProperty(window, "localStorage", { configurable: true, writable: true, value: storage });
    vi.mocked(getDashboardSocketTicket).mockReset().mockResolvedValue({ ticket: "t-1", expires_in: 60 });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it("deduplicates older and duplicate numeric event.id frames and resets live id on credential change", async () => {
    expect(isNumericEventId("12345")).toBe(true);
    expect(isNumericEventId("")).toBe(false);
    expect(isNumericEventId("evt-1")).toBe(false);

    storage.setItem("adk:ws:last-event-id", "100");
    const onEvent = vi.fn();
    const domEvents: WSEvent[] = [];
    const onDomWs = (e: Event) => domEvents.push((e as CustomEvent<WSEvent>).detail);
    window.addEventListener("pcd-ws-event", onDomWs);

    function Probe() {
      useDashboardSocket(onEvent);
      return null;
    }

    try {
      await act(async () => {
        root.render(<Probe />);
      });
      await act(async () => {
        await Promise.resolve();
      });

      expect(MockWebSocket.instances).toHaveLength(1);
      const ws = MockWebSocket.instances[0]!;
      expect(ws.url).toContain("since=100");

      await act(async () => {
        ws.emitOpen();
        ws.emitMessage({ type: "connected" });
        ws.emitMessage({ type: "kanban_card_updated", id: "99", payload: { id: "c-old" } });
        ws.emitMessage({ type: "kanban_card_updated", id: "100", payload: { id: "c-dup-seed" } });
        ws.emitMessage({ type: "kanban_card_updated", id: "101", payload: { id: "c-fresh" } });
        ws.emitMessage({ type: "kanban_card_updated", id: "101", payload: { id: "c-replay-dup" } });
        ws.emitMessage({ type: "kanban_card_updated", id: "9007199254740993", payload: { id: "c-big-1" } });
        ws.emitMessage({ type: "kanban_card_updated", id: "9007199254740993", payload: { id: "c-big-dup" } });
        ws.emitMessage({ type: "kanban_card_updated", id: "9007199254740994", payload: { id: "c-big-2" } });
      });

      expect(onEvent.mock.calls.map(([ev]) => [ev.type, (ev.payload as { id?: string } | null)?.id ?? null])).toEqual([
        ["connected", null],
        ["kanban_card_updated", "c-fresh"],
        ["kanban_card_updated", "c-big-1"],
        ["kanban_card_updated", "c-big-2"],
      ]);
      expect(domEvents).toHaveLength(4);

      await act(async () => {
        beginAuthentication("next-token");
      });
      expect(storage.getItem("adk:ws:last-event-id")).toBeNull();
    } finally {
      window.removeEventListener("pcd-ws-event", onDomWs);
    }
  });

  it("coalesces rapid bursts of WS messages and flushes pending event.id on close and cleanup", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-11-27T12:00:00Z"));
    const setItemSpy = vi.spyOn(storage, "setItem");
    const onEvent = vi.fn();
    let renderCount = 0;
    let observedLastTs: number | null = null;

    function Probe() {
      renderCount += 1;
      const { lastEventTs } = useDashboardSocket(onEvent);
      observedLastTs = lastEventTs;
      return null;
    }

    await act(async () => {
      root.render(<Probe />);
    });
    await act(async () => {
      await Promise.resolve();
    });

    const ws = MockWebSocket.instances[0]!;
    await act(async () => {
      ws.emitOpen();
    });
    const rendersAfterOpen = renderCount;

    // First event updates lastEventTs & localStorage synchronously with zero delay.
    await act(async () => {
      ws.emitMessage({ type: "kanban_card_updated", id: "1", payload: { id: "c-1" } });
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("1");
    expect(setItemSpy).toHaveBeenCalledTimes(1);
    expect(observedLastTs).toBe(Date.now());
    const rendersAfterFirst = renderCount;
    expect(rendersAfterFirst).toBeGreaterThan(rendersAfterOpen);

    // Rapid burst of 40 messages within 40ms must NOT spam localStorage or re-render per frame.
    await act(async () => {
      vi.advanceTimersByTime(40);
      for (let id = 2; id <= 41; id += 1) {
        ws.emitMessage({ type: "kanban_card_updated", id: String(id), payload: { id: `c-${id}` } });
      }
    });
    expect(onEvent).toHaveBeenCalledTimes(41);
    expect(setItemSpy).toHaveBeenCalledTimes(1);
    expect(renderCount).toBe(rendersAfterFirst);

    // Trailing timer flushes once at the 1s boundary.
    await act(async () => {
      vi.advanceTimersByTime(960);
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("41");
    expect(setItemSpy).toHaveBeenCalledTimes(2);

    // Burst followed by socket close flushes newest id immediately before reconnect.
    await act(async () => {
      vi.advanceTimersByTime(50);
      ws.emitMessage({ type: "kanban_card_updated", id: "42", payload: { id: "c-42" } });
      ws.emitMessage({ type: "kanban_card_updated", id: "43", payload: { id: "c-43" } });
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("41");

    await act(async () => {
      ws.close();
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("43");

    // Reconnect and verify unmount cleanup also flushes pending burst id.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    const ws2 = MockWebSocket.instances[1]!;
    expect(ws2.url).toContain("since=43");

    await act(async () => {
      ws2.emitOpen();
      vi.advanceTimersByTime(1_000);
      ws2.emitMessage({ type: "kanban_card_updated", id: "44", payload: { id: "c-44" } });
      vi.advanceTimersByTime(20);
      ws2.emitMessage({ type: "kanban_card_updated", id: "45", payload: { id: "c-45" } });
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("44");

    await act(async () => {
      root.unmount();
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("45");
  });

  it("applies jittered reconnect delay in [0.5 * base, 1.0 * base] and prevents duplicate timers", async () => {
    vi.useFakeTimers();
    const randomSpy = vi.spyOn(Math, "random");
    // Retry 0 (base 1000ms): random = 0 -> 500ms (0.5 * base)
    randomSpy.mockReturnValueOnce(0);

    function Probe() {
      useDashboardSocket(() => {});
      return null;
    }

    await act(async () => {
      root.render(<Probe />);
    });
    await act(async () => {
      await Promise.resolve();
    });
    expect(MockWebSocket.instances).toHaveLength(1);

    const ws1 = MockWebSocket.instances[0]!;
    await act(async () => {
      ws1.close();
    });
    expect(vi.getTimerCount()).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(499);
    });
    expect(MockWebSocket.instances).toHaveLength(1);

    // Next attempt fails ticket fetch; retry 1 (base 2000ms): random = 1 -> 2000ms (1.0 * base).
    vi.mocked(getDashboardSocketTicket).mockRejectedValueOnce(new Error("unavailable"));
    randomSpy.mockReturnValueOnce(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(MockWebSocket.instances).toHaveLength(1);
    expect(vi.getTimerCount()).toBe(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_999);
    });
    expect(MockWebSocket.instances).toHaveLength(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(MockWebSocket.instances).toHaveLength(2);

    // Duplicate close/error callbacks must clear the previous timer rather than stacking parallel reconnects.
    const ws2 = MockWebSocket.instances[1]!;
    randomSpy.mockReturnValue(0.5);
    await act(async () => {
      ws2.emitOpen();
      ws2.onerror?.();
      ws2.onclose?.();
    });
    expect(vi.getTimerCount()).toBe(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(MockWebSocket.instances).toHaveLength(3);
  });
});

describe("Provider context value memoization", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    const storage = createMemoryStorage();
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    vi.stubGlobal("localStorage", storage);
    Object.defineProperty(window, "localStorage", { configurable: true, writable: true, value: storage });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  it("preserves WorkspaceProvider, SettingsProvider, and KanbanProvider context references across parent re-renders", async () => {
    const workspaceRefs: ReturnType<typeof useWorkspace>[] = [];
    const settingsRefs: ReturnType<typeof useSettings>[] = [];
    const kanbanRefs: ReturnType<typeof useKanban>[] = [];
    let memoConsumerRenders = 0;

    const MemoConsumer = memo(function MemoConsumer() {
      memoConsumerRenders += 1;
      workspaceRefs.push(useWorkspace());
      settingsRefs.push(useSettings());
      kanbanRefs.push(useKanban());
      return null;
    });

    const initialAgents: Agent[] = [];
    const initialSessions: DispatchedSession[] = [];
    const initialSettings = { language: "en" } as CompanySettings;
    const initialStats = { total: 5 } as unknown as DashboardStats;
    const initialCards = [{ id: "c-1", title: "Card 1", status: "backlog" } as KanbanCard];
    const pushNotification = vi.fn();
    let bumpParent!: () => void;

    function RootHarness() {
      const [tick, setTick] = useState(0);
      bumpParent = () => setTick((prev) => prev + 1);
      return (
        <div data-tick={tick}>
          <SettingsProvider initialSettings={initialSettings} initialStats={initialStats}>
            <WorkspaceProvider
              initialAgents={initialAgents}
              initialSessions={initialSessions}
              pushNotification={pushNotification}
            >
              <KanbanProvider initialCards={initialCards}>
                <MemoConsumer />
              </KanbanProvider>
            </WorkspaceProvider>
          </SettingsProvider>
        </div>
      );
    }

    await act(async () => {
      root.render(<RootHarness />);
    });
    expect(memoConsumerRenders).toBe(1);
    expect(apiClient.getKanbanCards).not.toHaveBeenCalled();

    const firstWorkspace = workspaceRefs[0]!;
    const firstSettings = settingsRefs[0]!;
    const firstKanban = kanbanRefs[0]!;

    await act(async () => {
      bumpParent();
    });
    await act(async () => {
      bumpParent();
    });

    expect(container.querySelector("[data-tick]")?.getAttribute("data-tick")).toBe("2");
    expect(memoConsumerRenders).toBe(1);
    expect(workspaceRefs.at(-1)).toBe(firstWorkspace);
    expect(settingsRefs.at(-1)).toBe(firstSettings);
    expect(kanbanRefs.at(-1)).toBe(firstKanban);
  });
});

describe("CopyWell preview cap & full copy", () => {
  let container: HTMLDivElement;
  let root: Root;
  const writeText = vi.fn<(_text: string) => Promise<void>>();

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    writeText.mockReset().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  it("truncates pre above COPY_WELL_PREVIEW_LIMIT, toggles expand/collapse, and copies full untruncated value", async () => {
    const fullValue = `${"x".repeat(COPY_WELL_PREVIEW_LIMIT + 600)}TAIL_MARKER`;

    await act(async () => {
      root.render(<CopyWell title="Output" value={fullValue} tr={tr} testId="large-well" />);
    });

    const pre = container.querySelector("pre")!;
    expect(pre.textContent).toBe(`${fullValue.slice(0, COPY_WELL_PREVIEW_LIMIT)}\n…`);
    expect(pre.textContent).not.toContain("TAIL_MARKER");

    // Copy while collapsed must copy the entire untruncated string.
    const copyBtn = container.querySelector<HTMLButtonElement>('button[aria-label="Copy all of Output"]')!;
    await act(async () => {
      copyBtn.click();
    });
    expect(writeText).toHaveBeenCalledExactlyOnceWith(fullValue);

    const toggleBtn = container.querySelector<HTMLButtonElement>("button[aria-expanded]")!;
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("false");
    expect(toggleBtn.textContent).toContain(`Show all (${fullValue.length.toLocaleString()} chars)`);

    await act(async () => {
      toggleBtn.click();
    });
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("true");
    expect(toggleBtn.textContent).toBe("Collapse");
    expect(pre.textContent).toBe(fullValue);
    expect(pre.textContent).toContain("TAIL_MARKER");

    await act(async () => {
      toggleBtn.click();
    });
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("false");
    expect(pre.textContent).toBe(`${fullValue.slice(0, COPY_WELL_PREVIEW_LIMIT)}\n…`);

    // Expand again, then change value prop -> must reset expanded to false.
    await act(async () => {
      toggleBtn.click();
    });
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("true");

    const nextValue = `${"y".repeat(COPY_WELL_PREVIEW_LIMIT + 100)}NEXT_TAIL`;
    await act(async () => {
      root.render(<CopyWell title="Output" value={nextValue} tr={tr} testId="large-well" />);
    });
    expect(container.querySelector("pre")?.textContent).not.toContain("NEXT_TAIL");
    expect(container.querySelector("button[aria-expanded]")?.getAttribute("aria-expanded")).toBe("false");
  });
});

function oracleBuildTurnToolSummary(
  transcript: SessionTranscript,
  events: SessionTranscriptEvent[],
) {
  const usedResults = new Set<number>();
  const paired: Array<{ kind: "use"; i: number; j: number | null } | { kind: "result"; i: number }> = [];
  for (let i = 0; i < events.length; i += 1) {
    const event = events[i]!;
    if (event.kind === "tool_use") {
      let resultIndex: number | null = null;
      for (let j = i + 1; j < events.length; j += 1) {
        const candidate = events[j]!;
        if (candidate.kind === "tool_use" && candidate.tool_name === event.tool_name) break;
        if (
          candidate.kind === "tool_result" &&
          !usedResults.has(j) &&
          (!candidate.tool_name || !event.tool_name || candidate.tool_name === event.tool_name)
        ) {
          resultIndex = j;
          usedResults.add(j);
          break;
        }
      }
      paired.push({ kind: "use", i, j: resultIndex });
    } else if (event.kind === "tool_result" && !usedResults.has(i)) {
      paired.push({ kind: "result", i });
    }
  }
  return paired.map((entry) => {
    if (entry.kind === "use") {
      const ev = events[entry.i]!;
      const res = entry.j != null ? events[entry.j]! : null;
      const toolId = ev.tool_name || res?.tool_name || "Tool";
      const input = (ev.summary || ev.content || res?.summary || res?.content || "—").replace(/\s+/g, " ").trim().slice(0, 140);
      return { id: `${transcript.turn_id}-call`, toolId, input, hasError: Boolean(ev.is_error || res?.is_error) };
    }
    const ev = events[entry.i]!;
    return {
      id: `${transcript.turn_id}-call`,
      toolId: ev.tool_name || "Tool",
      input: (ev.summary || ev.content || "—").replace(/\s+/g, " ").trim().slice(0, 140),
      hasError: Boolean(ev.is_error),
    };
  });
}

describe("TurnTranscriptPanel & buildTurnToolSummary performance", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
    getCardTranscripts.mockReset();
    getAgentTranscripts.mockReset();
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  const makeTranscript = (events: SessionTranscriptEvent[]): SessionTranscript => ({
    id: 1,
    turn_id: "turn-perf",
    session_key: null,
    channel_id: null,
    agent_id: "agent-1",
    provider: "claude",
    dispatch_id: null,
    kanban_card_id: "card-1",
    dispatch_title: "Perf check",
    card_title: null,
    github_issue_number: null,
    user_message: "Run checks",
    assistant_message: "",
    duration_ms: 5_000,
    created_at: "2026-11-27T10:00:00Z",
    events,
  });

  it("pairs consecutive same-name and interleaved tool calls identically to the reference contract in O(N) time", () => {
    const consecutiveEvents: SessionTranscriptEvent[] = [
      { kind: "tool_use", tool_name: "Bash", summary: "first", content: "", status: "running", is_error: false },
      { kind: "tool_use", tool_name: "Bash", summary: "second", content: "", status: "done", is_error: false },
      { kind: "tool_result", tool_name: "Bash", summary: "second failed", content: "", status: "error", is_error: true },
      { kind: "tool_result", tool_name: "Bash", summary: "orphan result", content: "", status: "done", is_error: false },
    ];
    const summary = buildTurnToolSummary(makeTranscript(consecutiveEvents), consecutiveEvents, tr);
    expect(summary.calls.map((c) => ({ toolId: c.toolId, input: c.input, status: c.status }))).toEqual([
      { toolId: "Bash", input: "first", status: "running" },
      { toolId: "Bash", input: "second", status: "failed" },
      { toolId: "Bash", input: "orphan result", status: "done" },
    ]);
    expect(summary.tools).toEqual([{ id: "Bash", name: "Bash", calls: 3, connected: true }]);

    // Deterministic fuzz parity across 80 mixed sequences (named, unnamed, interleaved, errors).
    let seed = 0x12345678;
    const nextRand = () => {
      seed = (seed * 1664525 + 1013904223) >>> 0;
      return seed;
    };
    const toolPool = ["Bash", "Read", "Write", null, ""];
    const kindPool: SessionTranscriptEvent["kind"][] = ["tool_use", "tool_result", "assistant", "thinking"];

    for (let trial = 0; trial < 80; trial += 1) {
      const len = 1 + (nextRand() % 30);
      const seq: SessionTranscriptEvent[] = Array.from({ length: len }, (_, idx) => {
        const kind = kindPool[nextRand() % kindPool.length]!;
        const toolName = kind === "tool_use" || kind === "tool_result" ? toolPool[nextRand() % toolPool.length]! : null;
        const isError = (nextRand() & 7) === 0;
        return {
          kind,
          tool_name: toolName,
          summary: (nextRand() & 1) === 0 ? `sum-${idx}` : null,
          content: `body-${idx}`,
          status: isError ? "error" : "done",
          is_error: isError,
        };
      });
      const tx = makeTranscript(seq);
      const actual = buildTurnToolSummary(tx, seq, tr);
      const expected = oracleBuildTurnToolSummary(tx, seq);
      expect(
        actual.calls.map((c) => ({
          id: `${tx.turn_id}-call`,
          toolId: c.toolId,
          input: c.input,
          hasError: c.status === "failed",
        })),
      ).toEqual(expected);
    }

    // Linear-time scale check on 4,000 tool events with distinct names.
    const largeEvents: SessionTranscriptEvent[] = [];
    for (let i = 0; i < 2_000; i += 1) {
      largeEvents.push({ kind: "tool_use", tool_name: `Tool_${i}`, summary: `in_${i}`, content: "", status: "done", is_error: false });
    }
    for (let i = 1_999; i >= 0; i -= 1) {
      largeEvents.push({ kind: "tool_result", tool_name: `Tool_${i}`, summary: `out_${i}`, content: "", status: "done", is_error: false });
    }
    const start = performance.now();
    const largeSummary = buildTurnToolSummary(makeTranscript(largeEvents), largeEvents, tr);
    expect(performance.now() - start).toBeLessThan(500);
    expect(largeSummary.calls).toHaveLength(2_000);
    expect(largeSummary.tools).toHaveLength(2_000);
  });

  it("debounces rapid pcd-ws-event bursts into a single 350ms refresh and cancels pending refresh on unmount", async () => {
    vi.useFakeTimers();
    getCardTranscripts.mockResolvedValue([
      makeTranscript([
        { kind: "tool_use", tool_name: "Bash", summary: "cargo check", content: "cargo check", status: "done", is_error: false },
        { kind: "tool_result", tool_name: "Bash", summary: "ok", content: "ok", status: "done", is_error: false },
      ]),
    ]);

    await act(async () => {
      root.render(<TurnTranscriptPanel source={{ type: "card", id: "card-1" }} tr={tr} isKo={false} />);
    });
    expect(getCardTranscripts).toHaveBeenCalledTimes(1);

    // Emit 6 matching WS updates spaced 60ms apart + unrelated card updates.
    for (let i = 0; i < 6; i += 1) {
      await act(async () => {
        window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: { type: "dispatch_update", payload: { kanban_card_id: "card-1" } } }));
        window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: { type: "kanban_card_update", payload: { id: "other-card" } } }));
        vi.advanceTimersByTime(60);
      });
    }
    // 60ms < 350ms since the most recent matching event -> still not refreshed yet.
    expect(getCardTranscripts).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(290);
    });
    expect(getCardTranscripts).toHaveBeenCalledTimes(2);

    // Schedule another refresh, then unmount before 350ms -> must not fire after unmount.
    await act(async () => {
      window.dispatchEvent(new CustomEvent("pcd-ws-event", { detail: { type: "kanban_card_update", payload: { id: "card-1" } } }));
      vi.advanceTimersByTime(100);
    });
    await act(async () => {
      root.unmount();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });
    expect(getCardTranscripts).toHaveBeenCalledTimes(2);
  });
});
