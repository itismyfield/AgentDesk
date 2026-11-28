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
import { shouldRefreshCard } from "../src/components/agent-manager/TurnTranscriptModel";
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

  it("resets cursor across server restarts while deduplicating exact per-socket event.id frames (P1-1)", async () => {
    vi.useFakeTimers();
    expect(isNumericEventId("12345")).toBe(true);
    expect(isNumericEventId("")).toBe(false);
    expect(isNumericEventId("A:1")).toBe(false);

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

      // Socket 1 receives id "10000" -> delivered once, cursor becomes "10000".
      const ws1 = MockWebSocket.instances[0]!;
      await act(async () => {
        ws1.emitOpen();
        ws1.emitMessage({ type: "kanban_card_updated", id: "10000", payload: { id: "before-restart" } });
      });
      expect(storage.getItem("adk:ws:last-event-id")).toBe("10000");

      // Socket 1 closes -> Socket 2 reconnects with since=10000 against restarted server (ids reset to 1).
      await act(async () => {
        ws1.close();
        await vi.advanceTimersByTimeAsync(1_500);
      });
      const ws2 = MockWebSocket.instances[1]!;
      expect(ws2.url).toContain("since=10000");

      await act(async () => {
        ws2.emitOpen();
        ws2.emitMessage({ type: "connected" });
        ws2.emitMessage({ type: "kanban_card_updated", id: "1", payload: { id: "after-restart-1" } });
        ws2.emitMessage({ type: "kanban_card_updated", id: "1", payload: { id: "after-restart-dup" } });
      });

      expect(onEvent.mock.calls.map(([ev]) => [ev.type, (ev.payload as { id?: string } | null)?.id ?? null])).toEqual([
        ["kanban_card_updated", "before-restart"],
        ["connected", null],
        ["kanban_card_updated", "after-restart-1"],
      ]);
      expect(domEvents).toHaveLength(3);

      // Socket 2 closes -> Socket 3 must reconnect with since=1 (cursor recovered to restarted server timeline).
      await act(async () => {
        ws2.close();
        await vi.advanceTimersByTimeAsync(1_500);
      });
      const ws3 = MockWebSocket.instances[2]!;
      expect(ws3.url).toContain("since=1");

      // Also verify literal non-numeric A:1 -> reconnect -> B:1 and numeric 1 -> reconnect -> 1 both deliver.
      await act(async () => {
        ws3.emitOpen();
        ws3.emitMessage({ type: "kanban_card_updated", id: "A:1", payload: { id: "A:1" } });
        ws3.emitMessage({ type: "kanban_card_updated", id: "A:1", payload: { id: "A:1-dup" } });
        ws3.close();
        await vi.advanceTimersByTimeAsync(1_500);
      });
      const ws4 = MockWebSocket.instances[3]!;
      await act(async () => {
        ws4.emitOpen();
        ws4.emitMessage({ type: "kanban_card_updated", id: "B:1", payload: { id: "B:1" } });
      });

      expect(onEvent.mock.calls.slice(-2).map(([ev]) => (ev.payload as { id?: string }).id)).toEqual(["A:1", "B:1"]);

      await act(async () => {
        beginAuthentication("next-token");
      });
      expect(storage.getItem("adk:ws:last-event-id")).toBeNull();
    } finally {
      window.removeEventListener("pcd-ws-event", onDomWs);
    }
  });

  it("delivers out-of-order concurrent broadcast frames (102 -> 101) without regressing since cursor (P1-2)", async () => {
    vi.useFakeTimers();
    const onEvent = vi.fn();

    function Probe() {
      useDashboardSocket(onEvent);
      return null;
    }

    await act(async () => {
      root.render(<Probe />);
    });
    await act(async () => {
      await Promise.resolve();
    });

    const ws1 = MockWebSocket.instances[0]!;
    await act(async () => {
      ws1.emitOpen();
      ws1.emitMessage({ type: "kanban_card_updated", id: "102", payload: { id: "card-102" } });
      ws1.emitMessage({ type: "kanban_card_updated", id: "101", payload: { id: "card-101" } });
      // Duplicates of both on the same socket must be ignored.
      ws1.emitMessage({ type: "kanban_card_updated", id: "102", payload: { id: "card-102-dup" } });
      ws1.emitMessage({ type: "kanban_card_updated", id: "101", payload: { id: "card-101-dup" } });
    });

    expect(onEvent.mock.calls.map(([ev]) => (ev.payload as { id?: string }).id)).toEqual(["card-102", "card-101"]);

    await act(async () => {
      ws1.close();
      await vi.advanceTimersByTimeAsync(1_500);
    });
    const ws2 = MockWebSocket.instances[1]!;
    expect(ws2.url).toContain("since=102");
  });

  it("coalesces rapid bursts of WS messages and flushes pending event.id on pagehide, close, and cleanup (P2-1)", async () => {
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

    // Send id "2" within < 1000ms (still pending in throttle window), then fire pagehide -> flushes "2" immediately!
    await act(async () => {
      vi.advanceTimersByTime(20);
      ws.emitMessage({ type: "kanban_card_updated", id: "2", payload: { id: "c-2" } });
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("1");

    await act(async () => {
      window.dispatchEvent(new Event("pagehide"));
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("2");
    expect(setItemSpy).toHaveBeenCalledTimes(2);

    // Advance past 1s window, flush baseline, then verify 40-message burst coalescing.
    await act(async () => {
      vi.advanceTimersByTime(1_000);
      ws.emitMessage({ type: "kanban_card_updated", id: "3", payload: { id: "c-3" } });
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("3");
    expect(setItemSpy).toHaveBeenCalledTimes(3);
    const rendersBeforeBurst = renderCount;

    await act(async () => {
      vi.advanceTimersByTime(40);
      for (let id = 4; id <= 41; id += 1) {
        ws.emitMessage({ type: "kanban_card_updated", id: String(id), payload: { id: `c-${id}` } });
      }
    });
    expect(onEvent).toHaveBeenCalledTimes(41);
    expect(setItemSpy).toHaveBeenCalledTimes(3);
    expect(renderCount).toBe(rendersBeforeBurst);

    await act(async () => {
      vi.advanceTimersByTime(960);
    });
    expect(storage.getItem("adk:ws:last-event-id")).toBe("41");
    expect(setItemSpy).toHaveBeenCalledTimes(4);

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

describe("CopyWell preview cap, synchronous value switch & full copy", () => {
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

  it("truncates pre above COPY_WELL_PREVIEW_LIMIT, collapses synchronously on value switch without full-DOM flash, and copies full value (P2-3)", async () => {
    const valueA = `${"a".repeat(30_000 - 6)}TAIL_A`;
    const valueB = `${"b".repeat(35_000 - 6)}TAIL_B`;

    await act(async () => {
      root.render(<CopyWell title="Output" value={valueA} tr={tr} testId="large-well" />);
    });

    const pre = container.querySelector("pre")!;
    expect(pre.textContent).toBe(`${valueA.slice(0, COPY_WELL_PREVIEW_LIMIT)}\n…`);
    expect(pre.textContent).not.toContain("TAIL_A");

    // Copy while collapsed must copy the entire untruncated valueA.
    const copyBtn = container.querySelector<HTMLButtonElement>('button[aria-label="Copy all of Output"]')!;
    await act(async () => {
      copyBtn.click();
    });
    expect(writeText).toHaveBeenCalledExactlyOnceWith(valueA);

    // Expand valueA so all 30,000 chars are rendered.
    const toggleBtn = container.querySelector<HTMLButtonElement>("button[aria-expanded]")!;
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("false");
    await act(async () => {
      toggleBtn.click();
    });
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("true");
    expect(pre.textContent).toHaveLength(30_000);
    expect(pre.textContent).toContain("TAIL_A");

    // Observe every DOM mutation when switching from expanded valueA (30K) to valueB (35K).
    const observedLengths: number[] = [];
    const observer = new MutationObserver(() => {
      observedLengths.push(container.querySelector("pre")?.textContent?.length ?? 0);
    });
    observer.observe(container, { childList: true, characterData: true, subtree: true });

    try {
      await act(async () => {
        root.render(<CopyWell title="Output" value={valueB} tr={tr} testId="large-well" />);
      });
    } finally {
      observer.disconnect();
    }

    const finalPre = container.querySelector("pre")!;
    expect(finalPre.textContent!.length).toBeLessThanOrEqual(COPY_WELL_PREVIEW_LIMIT + 5);
    expect(finalPre.textContent).toBe(`${valueB.slice(0, COPY_WELL_PREVIEW_LIMIT)}\n…`);
    for (const len of observedLengths) {
      expect(len).toBeLessThanOrEqual(COPY_WELL_PREVIEW_LIMIT + 5);
    }
    expect(container.querySelector("button[aria-expanded]")?.getAttribute("aria-expanded")).toBe("false");

    writeText.mockClear();
    await act(async () => {
      container.querySelector<HTMLButtonElement>('button[aria-label="Copy all of Output"]')!.click();
    });
    expect(writeText).toHaveBeenCalledExactlyOnceWith(valueB);
    expect(valueB).toHaveLength(35_000);
  });
});

function resolveOracleStatus(event: SessionTranscriptEvent, result?: SessionTranscriptEvent | null) {
  const raw = `${event.status ?? ""} ${result?.status ?? ""}`.toLowerCase();
  if (event.is_error || result?.is_error || event.kind === "error" || /fail|error|err|reject|denied|timeout/.test(raw)) return "failed";
  if (/wait|ask|pending|approval|confirm|queued/.test(raw)) return "waiting";
  if (/run|active|in_progress|progress|working/.test(raw)) return "running";
  return "done";
}

function oracleBuildTurnToolSummary(
  transcript: SessionTranscript,
  events: SessionTranscriptEvent[],
) {
  const usedResults = new Set<number>();
  const calls: Array<{ id: string; toolId: string; status: string; input: string; run: string }> = [];
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
      const paired = resultIndex != null ? events[resultIndex]! : null;
      const name = event.tool_name || paired?.tool_name || "Tool";
      const input = (event.summary || event.content || paired?.summary || paired?.content || "—").replace(/\s+/g, " ").trim().slice(0, 140);
      calls.push({
        id: `${transcript.turn_id}-call-${calls.length + 1}`,
        toolId: name,
        status: resolveOracleStatus(event, paired),
        input,
        run: transcript.turn_id,
      });
    } else if (event.kind === "tool_result" && !usedResults.has(i)) {
      const name = event.tool_name || "Tool";
      const input = (event.summary || event.content || "—").replace(/\s+/g, " ").trim().slice(0, 140);
      calls.push({
        id: `${transcript.turn_id}-call-${calls.length + 1}`,
        toolId: name,
        status: resolveOracleStatus(event),
        input,
        run: transcript.turn_id,
      });
    }
  }
  const counts = new Map<string, number>();
  for (const call of calls) counts.set(call.toolId, (counts.get(call.toolId) ?? 0) + 1);
  const tools = Array.from(counts, ([name, count]) => ({ id: name, name, calls: count, connected: true }));
  return { tools, calls };
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

  it("pairs tool_use and tool_result identically across seeded randomized streams and scales linearly to N = 25,000 (P2-4)", () => {
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

    // Xorshift32 PRNG avoids low-bit LCG alternation so all event kinds & tool names mix uniformly.
    let state = 0x9e3779b9;
    const nextU32 = () => {
      state ^= state << 13;
      state ^= state >>> 17;
      state ^= state << 5;
      return state >>> 0;
    };

    const toolPool = ["Bash", "Read", "Write", null, ""];
    const kindPool: SessionTranscriptEvent["kind"][] = [
      "tool_use",
      "tool_use",
      "tool_result",
      "tool_result",
      "assistant",
      "thinking",
    ];
    const statusPool = ["done", "running", "pending", "error", null];
    let totalToolUsesGenerated = 0;
    let totalToolResultsGenerated = 0;

    for (let trial = 0; trial < 100; trial += 1) {
      const len = 15 + (nextU32() % 40);
      const seq: SessionTranscriptEvent[] = Array.from({ length: len }, (_, idx) => {
        const kind = kindPool[nextU32() % kindPool.length]!;
        if (kind === "tool_use") totalToolUsesGenerated += 1;
        if (kind === "tool_result") totalToolResultsGenerated += 1;
        const toolName = kind === "tool_use" || kind === "tool_result" ? toolPool[nextU32() % toolPool.length]! : null;
        const isError = (nextU32() % 7) === 0;
        const status = statusPool[nextU32() % statusPool.length]!;
        return {
          kind,
          tool_name: toolName,
          summary: (nextU32() & 1) === 0 ? `sum-${idx}` : null,
          content: `body-${idx}`,
          status,
          is_error: isError,
        };
      });
      const tx = makeTranscript(seq);
      const actual = buildTurnToolSummary(tx, seq, tr);
      const expected = oracleBuildTurnToolSummary(tx, seq);
      expect(actual.tools).toEqual(expected.tools);
      expect(
        actual.calls.map(({ id, toolId, status, input, run }) => ({ id, toolId, status, input, run })),
      ).toEqual(expected.calls);
    }

    expect(totalToolUsesGenerated).toBeGreaterThan(500);
    expect(totalToolResultsGenerated).toBeGreaterThan(500);

    // Linear-time scaling check on N = 25,000 interleaved/alternating tool_use + tool_result events.
    const largeEvents: SessionTranscriptEvent[] = [];
    for (let block = 0; block < 2_500; block += 1) {
      for (let k = 0; k < 5; k += 1) {
        largeEvents.push({
          kind: "tool_use",
          tool_name: `Tool_${block}_${k}`,
          summary: `in_${block}_${k}`,
          content: "",
          status: "done",
          is_error: false,
        });
      }
      for (let k = 4; k >= 0; k -= 1) {
        largeEvents.push({
          kind: "tool_result",
          tool_name: k === 2 ? null : `Tool_${block}_${k}`,
          summary: `out_${block}_${k}`,
          content: "",
          status: "done",
          is_error: false,
        });
      }
    }
    expect(largeEvents).toHaveLength(25_000);
    const start = performance.now();
    const largeSummary = buildTurnToolSummary(makeTranscript(largeEvents), largeEvents, tr);
    expect(performance.now() - start).toBeLessThan(1_500);
    expect(largeSummary.calls).toHaveLength(12_501);
  });

  it("matches real server card/dispatch WS event names and coalesces refreshSeed + WS bursts into one 350ms refresh (P2-2)", async () => {
    const mkWsEvent = (type: string, payload: Record<string, unknown>) =>
      new CustomEvent("pcd-ws-event", { detail: { type, payload } });

    expect(shouldRefreshCard(mkWsEvent("kanban_card_created", { id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("kanban_card_updated", { id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("task_dispatch_created", { kanban_card_id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("task_dispatch_updated", { kanban_card_id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("kanban_card_new", { id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("kanban_card_update", { id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("dispatch_new", { kanban_card_id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("dispatch_update", { kanban_card_id: "card-1" }), "card-1")).toBe(true);
    expect(shouldRefreshCard(mkWsEvent("kanban_card_updated", { id: "card-2" }), "card-1")).toBe(false);
    expect(shouldRefreshCard(mkWsEvent("task_dispatch_updated", { kanban_card_id: "card-2" }), "card-1")).toBe(false);

    vi.useFakeTimers();
    getCardTranscripts.mockResolvedValue([
      makeTranscript([
        { kind: "tool_use", tool_name: "Bash", summary: "cargo check", content: "cargo check", status: "done", is_error: false },
        { kind: "tool_result", tool_name: "Bash", summary: "ok", content: "ok", status: "done", is_error: false },
      ]),
    ]);

    await act(async () => {
      root.render(
        <TurnTranscriptPanel
          source={{ type: "card", id: "card-1", refreshSeed: "d1:t1" }}
          tr={tr}
          isKo={false}
        />,
      );
    });
    expect(getCardTranscripts).toHaveBeenCalledTimes(1);

    // Rapidly mutate refreshSeed AND dispatch real server kanban_card_updated + task_dispatch_updated events.
    await act(async () => {
      vi.advanceTimersByTime(50);
      window.dispatchEvent(mkWsEvent("kanban_card_updated", { id: "card-1" }));
      root.render(
        <TurnTranscriptPanel
          source={{ type: "card", id: "card-1", refreshSeed: "d1:t2" }}
          tr={tr}
          isKo={false}
        />,
      );
    });
    await act(async () => {
      vi.advanceTimersByTime(50);
      window.dispatchEvent(mkWsEvent("task_dispatch_updated", { kanban_card_id: "card-1" }));
      root.render(
        <TurnTranscriptPanel
          source={{ type: "card", id: "card-1", refreshSeed: "d1:t3" }}
          tr={tr}
          isKo={false}
        />,
      );
    });

    // Neither refreshSeed updates nor WS events should have fired a second fetch before 350ms, nor flashed loading state.
    expect(getCardTranscripts).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[role="status"][aria-label="Loading..."]')).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(349);
    });
    expect(getCardTranscripts).toHaveBeenCalledTimes(1);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(getCardTranscripts).toHaveBeenCalledTimes(2);

    // Schedule another refresh via WS + refreshSeed, then unmount before 350ms -> must not fire after unmount.
    await act(async () => {
      window.dispatchEvent(mkWsEvent("kanban_card_updated", { id: "card-1" }));
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
