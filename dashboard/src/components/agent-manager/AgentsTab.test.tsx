// @vitest-environment happy-dom
import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Agent } from "../../types";
import AgentsTab from "./AgentsTab";
import { agentSecondaryLine } from "./AgentCard";
import { filterAndSortAgents, type AgentSortMode } from "./agent-list-controls";

vi.mock("../../api", () => ({
  getSkillRanking: vi.fn(async () => ({ byAgent: [] })),
  getAgentCron: vi.fn(async () => []),
  getAgentSkills: vi.fn(async () => ({ skills: [], sharedSkills: [] })),
  getAgentDispatchedSessions: vi.fn(async () => []),
  getDiscordBindings: vi.fn(async () => []),
  getAgentTimeline: vi.fn(async () => []),
  getAuditLogs: vi.fn(async () => []),
  getDiscordChannelInfo: vi.fn(async () => null),
  updateAgent: vi.fn(async () => ({})),
}));
vi.mock("../../api/providers", () => ({
  catalogLabel: (_entries: unknown, id: string) => id,
  getProviderAuthProfiles: vi.fn(async () => ({ providers: [] })),
  useProviderCatalog: () => ({ selectableIds: ["claude", "codex"], entries: [], loading: false, error: null }),
}));
vi.mock("../../api/agentExecutionNode", () => ({
  getAgentExecutionNode: vi.fn(async () => ({ default_node_id: null, routing_enforced: true })),
  setAgentExecutionNode: vi.fn(),
}));
vi.mock("../../api/clusterNodes", () => ({ getClusterNodes: vi.fn(async () => ({ cluster: { enabled: false }, nodes: [] })) }));

const base = {
  name_ja: null, name_zh: null, department_id: null, avatar_emoji: "🤖", personality: null,
  stats_tasks_done: 0, stats_tokens: 0, created_at: 0,
} as const;
const AGENTS: Agent[] = [
  { ...base, id: "agent-ada", name: "Ada Dashboard", alias: "ada", name_ko: "아다", status: "working", cli_provider: "codex" },
  { ...base, id: "agent-luna", name: "Luna Ops", name_ko: "루나", status: "idle", cli_provider: "claude" },
  { ...base, id: "agent-nova", name: "Nova Watch", name_ko: "노바", status: "break", cli_provider: "claude" },
];
const tr = (_ko: string, en: string) => en;

function Harness() {
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState("all");
  const [deptTab, setDeptTab] = useState("all");
  const [sortMode, setSortMode] = useState<AgentSortMode>("status");
  const [includeTests, setIncludeTests] = useState(false);
  const sortedAgents = filterAndSortAgents(AGENTS, { deptTab, statusFilter, search, includeTests }, sortMode);
  return (
    <AgentsTab
      tr={tr} locale="en" isKo={false} agents={AGENTS} departments={[]}
      deptTab={deptTab} setDeptTab={setDeptTab} search={search} setSearch={setSearch}
      statusFilter={statusFilter} setStatusFilter={setStatusFilter} sortMode={sortMode} setSortMode={setSortMode}
      sortedAgents={sortedAgents} includeTests={includeTests} setIncludeTests={setIncludeTests}
      onCreateAgent={vi.fn()} onEditAgent={vi.fn()} onDuplicateAgent={vi.fn()} onEditDepartment={vi.fn()}
      onDeleteAgent={vi.fn()} onAgentUpdated={vi.fn()} saving={false}
    />
  );
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
  if (typeof globalThis.ResizeObserver === "undefined") {
    vi.stubGlobal("ResizeObserver", class { observe() {} unobserve() {} disconnect() {} });
  }
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  document.body.innerHTML = "";
  vi.unstubAllGlobals();
});

const tab = (name: RegExp) =>
  Array.from(document.querySelectorAll<HTMLElement>('[role="tab"]')).find((node) => name.test(node.textContent ?? ""))!;

describe("AgentsTab", () => {
  it("counts each status tab under the other filters and filters on press", async () => {
    setViewport(false);
    act(() => root.render(<Harness />));
    await flush();
    expect(tab(/^All/).textContent).toContain("3");
    expect(tab(/^Working/).textContent).toContain("1");
    expect(tab(/^Break/).getAttribute("aria-selected")).toBe("false");

    act(() => tab(/^Break/).click());
    expect(tab(/^Break/).getAttribute("aria-selected")).toBe("true");
    expect(document.querySelectorAll("[data-agent-item]")).toHaveLength(1);
    expect(document.body.textContent).toContain("1 agents shown");
  });

  it("shows the detail beside the list on wide screens and closes it", async () => {
    setViewport(false);
    act(() => root.render(<Harness />));
    await flush();
    const pane = document.querySelector<HTMLElement>('[data-testid="agents-detail-pane"]')!;
    expect(pane.textContent).toContain("No agent selected");

    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-grid-card-agent-ada"]')!.click());
    await flush();
    expect(pane.getAttribute("aria-label")).toBe("ada — Agent details");
    expect(pane.querySelector("h2")?.textContent).toBe("ada");
    expect(document.querySelector('[data-testid="agents-grid-card-agent-ada"]')!.getAttribute("aria-current")).toBe("true");
    expect(document.querySelector('[role="dialog"]')).toBeNull();

    act(() => pane.querySelector<HTMLButtonElement>('button[aria-label="Close details"]')!.click());
    expect(pane.textContent).toContain("No agent selected");
  });

  it("opens the detail as a full-screen dialog with a back button on phones", async () => {
    setViewport(true);
    act(() => root.render(<Harness />));
    await flush();
    expect(document.querySelector('[data-testid="agents-view-list"]')).not.toBeNull();
    expect(document.querySelector('[data-testid="agents-detail-pane"]')).toBeNull();

    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-card-agent-luna"]')!.click());
    await flush();
    const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
    expect(dialog.getAttribute("aria-label")).toBe("Luna Ops — Agent details");
    const back = Array.from(dialog.querySelectorAll("button")).find((node) => node.textContent === "Back to list")!;
    expect(document.activeElement).toBe(back);
    act(() => back.click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });
});

describe("agentSecondaryLine", () => {
  it("shows the real name under an alias and the other-language name otherwise", () => {
    expect(agentSecondaryLine(AGENTS[0], "ko")).toBe("아다");
    expect(agentSecondaryLine(AGENTS[0], "en")).toBe("Ada Dashboard");
    expect(agentSecondaryLine(AGENTS[1], "ko")).toBe("Luna Ops");
    expect(agentSecondaryLine(AGENTS[1], "en")).toBe("루나");
  });
});
