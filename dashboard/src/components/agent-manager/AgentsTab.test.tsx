// @vitest-environment happy-dom
import { act, useState } from "react";
import * as api from "../../api";
import type { DiscordBinding } from "../../api/client";
import { ApiRequestError } from "../../api/httpClient";
import * as providers from "../../api/providers";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Agent, Department, DispatchedSession } from "../../types";
import AgentsTab from "./AgentsTab";
import AgentFormModal from "./AgentFormModal";
import AgentSetupWizard from "./AgentSetupWizard";
import { AgentsUnlinkedSessions } from "./AgentsSummaryPane";
import { agentSecondaryLine } from "./AgentRow";
import { classifyRoster, filterAndSortAgents, isStandIn, type AgentSortMode, type AgentStatusFilter } from "./agent-list-controls";

vi.mock("../../api", () => ({
  getSkillRanking: vi.fn(async () => ({ byAgent: [] })),
  getAgentCron: vi.fn(async () => []),
  getAgentSkills: vi.fn(async () => ({ skills: [], sharedSkills: [] })),
  getAgentDispatchedSessions: vi.fn(async () => []),
  getDiscordBindings: vi.fn(async () => []),
  getAgentTimeline: vi.fn(async () => []),
  getAuditLogs: vi.fn(async () => []),
  getDiscordChannelInfo: vi.fn(async () => null),
  getTaskDispatches: vi.fn(async () => []),
  updateAgent: vi.fn(async () => ({})),
  setupAgent: vi.fn(async () => ({ ok: true })),
  duplicateAgent: vi.fn(async () => ({ ok: true })),
}));
vi.mock("../../api/agentsSetup", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/agentsSetup")>()),
  dryRunSetupAgent: vi.fn(async () => ({ ok: true })),
  dryRunDuplicateAgent: vi.fn(async () => ({ ok: true })),
}));
vi.mock("../../contexts/KanbanContext", () => ({ useKanban: () => ({ kanbanCards: [], kanbanCardsState: "ready" }) }));
vi.mock("../home/useHomeData", () => ({ useAgentTurns: () => new Map() }));
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
  { ...base, id: "agent-nova", name: "Nova Watch", name_ko: "노바", status: "idle", cli_provider: "claude", visual_status_code: "stuck-suspect" } as Agent,
];
// Unlinked sessions remain separate from the agent roster even if legacy department_id is set.
const SESSIONS = [
  { id: "s1", linked_agent_id: "agent-luna", status: "idle", last_seen_at: 0 },
  { id: "s9", session_key: "s9", name: "adk-cc", department_id: "dev", linked_agent_id: null, status: "idle", last_seen_at: 0 },
] as DispatchedSession[];
const tr = (_ko: string, en: string) => en;

function Harness({ sessionsFailed = false, agents = AGENTS }: { sessionsFailed?: boolean; agents?: Agent[] }) {
  const entries = classifyRoster(agents, sessionsFailed ? [] : SESSIONS, sessionsFailed);
  const [search, setSearch] = useState("");
  const [statusFilter, setStatusFilter] = useState<AgentStatusFilter>("all");
  const [sortMode, setSortMode] = useState<AgentSortMode>("status");
  const [includeTests, setIncludeTests] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const sortedEntries = filterAndSortAgents(entries.filter((entry) => !isStandIn(entry.agent)), { statusFilter, search, includeTests }, sortMode);
  return (
    <AgentsTab
      tr={tr} locale="en" isKo={false} entries={entries} sortedEntries={sortedEntries}
      search={search} setSearch={setSearch}
      statusFilter={statusFilter} setStatusFilter={setStatusFilter} sortMode={sortMode} setSortMode={setSortMode}
      includeTests={includeTests} setIncludeTests={setIncludeTests} selectedAgentId={selected} onSelectAgent={setSelected}
      onCreateAgent={vi.fn()} onEditAgent={vi.fn()} onDuplicateAgent={vi.fn()}
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
  it("counts home's states per tab, filters on press and lists a stuck agent to look at", async () => {
    setViewport(false);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    expect(tab(/^All/).textContent).toContain("3");
    expect(tab(/^Working/).textContent).toContain("1");
    expect(tab(/^Resting/).textContent).toContain("1");
    expect(document.querySelectorAll("[data-agent-item]")).toHaveLength(3);
    expect(tab(/^No session/).getAttribute("aria-selected")).toBe("false");
    expect(document.querySelector('[data-testid="agents-attention"]')!.textContent).toContain("Maybe stuck");
    expect(document.querySelector('[data-testid="agents-summary"]')!.textContent).toContain("Resting 1");

    act(() => tab(/^No session/).click());
    expect(tab(/^No session/).getAttribute("aria-selected")).toBe("true");
    expect(Array.from(document.querySelectorAll("[data-agent-item]"), (node) => node.getAttribute("data-agent-item"))).toEqual(["agent-nova"]);
  });

  it("shows the detail beside the list on wide screens and closes it", async () => {
    setViewport(false);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    const pane = document.querySelector<HTMLElement>('[data-testid="agents-detail-pane"]')!;
    expect(pane.querySelector('[data-testid="agents-summary"]')).not.toBeNull();

    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    await flush();
    expect(pane.getAttribute("aria-label")).toBe("ada — Agent details");
    expect(pane.querySelector("h2")?.textContent).toBe("ada");
    expect(document.querySelector('[data-testid="agents-row-agent-ada"]')!.getAttribute("aria-current")).toBe("true");
    expect(document.querySelector('[role="dialog"]')).toBeNull();

    act(() => pane.querySelector<HTMLButtonElement>('button[aria-label="Close details"]')!.click());
    expect(pane.querySelector('[data-testid="agents-summary"]')).not.toBeNull();
  });

  it("opens the detail as a full-screen dialog with a back button on phones", async () => {
    setViewport(true);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    expect(document.querySelector('[data-testid="agents-list"]')).not.toBeNull();
    expect(document.querySelector('[data-testid="agents-detail-pane"]')).toBeNull();

    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-luna"]')!.click());
    await flush();
    const dialog = document.querySelector<HTMLElement>('[role="dialog"]')!;
    expect(dialog.getAttribute("aria-label")).toBe("Luna Ops — Agent details");
    const back = Array.from(dialog.querySelectorAll("button")).find((node) => node.textContent === "Back to list")!;
    expect(document.activeElement).toBe(back);
    act(() => back.click());
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });
});

describe("AgentsTab detail and filters", () => {
  it("lists live sessions only, counts finished ones, looks up only their channels and speaks home's state", async () => {
    setViewport(false);
    vi.mocked(api.getDiscordChannelInfo).mockClear();
    const row = (id: string, status: string, thread: string) => ({ id, status, thread_channel_id: thread, provider: "claude", session_info: id });
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([
      row("gone", "aborted", "100000000000000001"), row("lost", "disconnected", "100000000000000002"),
      row("rest", "idle", "100000000000000003"), row("turn", "turn_active", "100000000000000004"),
    ] as DispatchedSession[]);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    await flush();
    await flush();
    const pane = document.querySelector<HTMLElement>('[data-testid="agents-detail-pane"]')!;
    expect(pane.textContent).toContain("2 live sessions");
    expect(pane.textContent).toContain("2 finished sessions");
    expect(vi.mocked(api.getDiscordChannelInfo).mock.calls.map(([id]) => id).sort()).toEqual(["100000000000000003", "100000000000000004"]);

    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-nova"]')!.click());
    await flush();
    expect(pane.querySelector("dd")!.textContent).toBe("No session");
  });

  it("reads what details share once per page, folded sections on opening and finished sessions twenty at a time", async () => {
    setViewport(false);
    const shared = [api.getDiscordBindings, api.getSkillRanking, providers.getProviderAuthProfiles];
    const lazy = [api.getAgentCron, api.getAgentTimeline, api.getAuditLogs];
    for (const read of [...shared, ...lazy]) vi.mocked(read).mockClear();
    const calls = (reads: Array<(...args: never[]) => unknown>) => reads.map((read) => vi.mocked(read).mock.calls.length);
    const finished = Array.from({ length: 25 }, (_, index) => ({ id: `old-${index}`, status: "aborted", provider: "claude", session_key: `k${index}` }));
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    expect(calls(shared)).toEqual([1, 0, 0]);
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    await flush();
    const pane = document.querySelector<HTMLElement>('[data-testid="agents-detail-pane"]')!;
    const fold = (name: string) => Array.from(pane.querySelectorAll<HTMLButtonElement>("button[aria-expanded]")).find((node) => node.textContent?.startsWith(name))!;
    expect(calls(shared)).toEqual([1, 1, 1]);
    expect(calls(lazy)).toEqual([0, 0, 0]);
    expect(pane.querySelector('[data-testid="turn-transcript-panel"]')).toBeNull();
    act(() => fold("Cron").click());
    act(() => fold("Activity").click());
    await flush();
    expect(calls(lazy)).toEqual([1, 1, 1]);

    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce(finished as DispatchedSession[]);
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-luna"]')!.click());
    await flush();
    expect(calls(shared)).toEqual([1, 1, 1]);
    const more = () => Array.from(pane.querySelectorAll("button")).find((node) => /finished sessions$|left$/.test(node.textContent ?? ""))!;
    act(() => more().click());
    expect(pane.querySelectorAll('[data-testid="agent-detail-finished"] li')).toHaveLength(20);
    expect(more().textContent).toBe("Show 5 more · 5 left");
    act(() => more().click());
    expect(pane.querySelectorAll('[data-testid="agent-detail-finished"] li')).toHaveLength(25);
  });

  it("shows the account override of the current Provider only and reads routes again for a new or re-routed agent", async () => {
    setViewport(false);
    vi.mocked(api.getDiscordBindings).mockClear().mockReturnValueOnce(new Promise(() => {}));
    vi.mocked(providers.getProviderAuthProfiles).mockResolvedValueOnce({
      providers: [{ id: "codex", default_home: "", accounts: [{ id: "default", home: "" }, { id: "work", home: "" }] }],
      agent_profile_overrides: [{ agent_id: "agent-ada", provider: "codex", profile_id: "work" }],
    });
    const render = (agents: Agent[]) => act(() => root.render(<MemoryRouter><Harness agents={agents} /></MemoryRouter>));
    render(AGENTS);
    await flush();
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    await flush();
    const pane = document.querySelector<HTMLElement>('[data-testid="agents-detail-pane"]')!;
    const account = () => Array.from(pane.querySelectorAll('[role="combobox"]')).find((node) => node.getAttribute("aria-labelledby")?.split(" ").some((id) => document.getElementById(id)?.textContent === "Provider Account"))!;
    expect(account().textContent).toBe("work");
    // Routes still unread: the summary leaves their count out instead of saying zero.
    expect(pane.querySelector('[data-testid="agent-detail-connections"]')!.textContent).not.toContain("Discord routes");

    // The edit form moved Ada to Claude and a new agent arrived; the codex override stays on the server.
    const moved = AGENTS.map((agent) => (agent.id === "agent-ada" ? { ...agent, cli_provider: "claude" as const } : agent));
    render([...moved, { ...AGENTS[1], id: "agent-new", name: "New" }]);
    await flush();
    expect(account().textContent).toBe("Provider primary account");
    expect(vi.mocked(api.getDiscordBindings)).toHaveBeenCalledTimes(2);
    expect(pane.querySelector('[data-testid="agent-detail-connections"]')!.textContent).toContain("0 Discord routes");
    render([...moved, { ...AGENTS[1], id: "agent-new", name: "New", status: "working" }]);
    render([...moved, { ...AGENTS[1], id: "agent-new", name: "New", discord_channel_id: "1490145076658307100" }]);
    await flush();
    expect(vi.mocked(api.getDiscordBindings)).toHaveBeenCalledTimes(3);
  });

  it("does not read the channels again when a phone folds connections and opens them again", async () => {
    setViewport(true);
    vi.mocked(api.getDiscordChannelInfo).mockClear();
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([{ id: "turn", status: "turn_active", thread_channel_id: "100000000000000004", provider: "claude" }] as DispatchedSession[]);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    await flush();
    const connections = () => Array.from(document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button[aria-expanded]')).find((node) => node.textContent?.startsWith("Connections"))!;
    expect(vi.mocked(api.getDiscordChannelInfo)).not.toHaveBeenCalled();
    act(() => connections().click());
    await flush();
    expect(vi.mocked(api.getDiscordChannelInfo)).toHaveBeenCalledTimes(1);
    act(() => connections().click());
    act(() => connections().click());
    await flush();
    expect(vi.mocked(api.getDiscordChannelInfo)).toHaveBeenCalledTimes(1);
  });

  it("keeps the newest routes when an older read lands late, and the last good list when a re-read fails", async () => {
    setViewport(false);
    const resolvers: Array<(value: DiscordBinding[]) => void> = [];
    const later = () => new Promise<DiscordBinding[]>((resolve) => resolvers.push(resolve));
    vi.mocked(api.getDiscordBindings).mockClear().mockReturnValueOnce(later()).mockReturnValueOnce(later()).mockRejectedValueOnce(new Error("down"));
    const render = (agents: Agent[]) => act(() => root.render(<MemoryRouter><Harness agents={agents} /></MemoryRouter>));
    render(AGENTS);
    await flush();
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    render([...AGENTS, { ...AGENTS[1], id: "agent-new", name: "New" }]);
    await flush();
    await act(async () => resolvers[1]([{ agentId: "agent-ada", channelId: "100000000000000005", source: "primary" }]));
    await act(async () => resolvers[0]([]));
    const connections = () => document.querySelector('[data-testid="agent-detail-connections"]')!.textContent;
    expect(connections()).toContain("1 Discord routes");
    render([...AGENTS, { ...AGENTS[1], id: "agent-new", name: "New" }, { ...AGENTS[1], id: "agent-newer", name: "Newer" }]);
    await flush();
    expect(vi.mocked(api.getDiscordBindings)).toHaveBeenCalledTimes(3);
    expect(connections()).toContain("1 Discord routes");
  });

  it("asks again for a channel whose read failed, not for one the server refused", async () => {
    setViewport(true);
    vi.mocked(api.getDiscordChannelInfo).mockClear()
      .mockRejectedValueOnce(new Error("timeout"))
      .mockRejectedValueOnce(new ApiRequestError("channel not in role-map", { status: 403 }));
    const thread = (id: string) => ({ id, status: "turn_active", thread_channel_id: id, provider: "claude" });
    vi.mocked(api.getAgentDispatchedSessions).mockResolvedValueOnce([thread("100000000000000004"), thread("100000000000000006")] as DispatchedSession[]);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    act(() => document.querySelector<HTMLButtonElement>('[data-testid="agents-row-agent-ada"]')!.click());
    await flush();
    const connections = () => Array.from(document.querySelectorAll<HTMLButtonElement>('[role="dialog"] button[aria-expanded]')).find((node) => node.textContent?.startsWith("Connections"))!;
    for (let open = 0; open < 3; open += 1) {
      if (open) act(() => connections().click());
      act(() => connections().click());
      await flush();
    }
    expect(vi.mocked(api.getDiscordChannelInfo).mock.calls.map(([id]) => id)).toEqual(["100000000000000004", "100000000000000006", "100000000000000004"]);
  });

  it("keeps a picked status tab after its count drops to zero", async () => {
    setViewport(false);
    act(() => root.render(<MemoryRouter><Harness sessionsFailed /></MemoryRouter>));
    await flush();
    act(() => tab(/^Unknown/).click());
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    expect(tab(/^Unknown/).getAttribute("aria-selected")).toBe("true");
    expect(tab(/^Unknown/).textContent).toContain("0");
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

describe("provider summary, attention header, and active provider resolution", () => {
  it("renders provider distribution in summary pane without queue link on attention card", async () => {
    setViewport(false);
    act(() => root.render(<MemoryRouter><Harness /></MemoryRouter>));
    await flush();
    const summaryText = document.querySelector('[data-testid="agents-summary"]')!.textContent ?? "";
    expect(summaryText).toContain("Agents by provider");
    expect(summaryText).toContain("Claude");
    expect(summaryText).toContain("Codex");
    expect(document.querySelector('[data-testid="agents-attention"]')!.textContent).not.toContain("Open queue");
  });

  it("prefers Antigravity agent provider over default claude turn fallback when no -cc channel exists", async () => {
    const { resolveActiveAgentProvider } = await import("./AgentInfoCardContent");
    expect(
      resolveActiveAgentProvider(
        { cli_provider: "antigravity", discord_channel_cc: null },
        [],
        "claude",
      ),
    ).toBe("antigravity");
    expect(
      resolveActiveAgentProvider(
        { cli_provider: "codex", discord_channel_cc: "123456789012345678" },
        [{ provider: "claude" }],
        "claude",
      ),
    ).toBe("claude");
  });

  it("hides retired department controls in AgentsTab/detail and formats raw Discord channel snowflakes cleanly", async () => {
    setViewport(false);
    const ko = (k: string) => k;
    vi.mocked(api.getDiscordBindings).mockResolvedValueOnce([
      { agentId: "agent-ada", channelId: "1490145076658307999", counterModelChannelId: "1490145076658307888", source: "primary" },
    ]);
    vi.mocked(api.getDiscordChannelInfo).mockResolvedValue(null as never);
    const entries = classifyRoster([{ ...AGENTS[0], department_id: "dept-infra" }, AGENTS[1]], SESSIONS, false);
    act(() => {
      root.render(
        <MemoryRouter>
          <AgentsTab
            tr={ko} locale="ko" isKo entries={entries} sortedEntries={entries.filter((e) => !isStandIn(e.agent))}
            search="" setSearch={vi.fn()}
            statusFilter="all" setStatusFilter={vi.fn()} sortMode="status" setSortMode={vi.fn()}
            includeTests={false} setIncludeTests={vi.fn()} selectedAgentId="agent-ada" onSelectAgent={vi.fn()}
            onCreateAgent={vi.fn()} onEditAgent={vi.fn()} onDuplicateAgent={vi.fn()}
            onDeleteAgent={vi.fn()} onAgentUpdated={vi.fn()} saving={false}
          />
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();

    expect(container.querySelector('[aria-label="소속 부서"]')).toBeNull();
    expect(container.textContent).not.toContain("전체 부서");
    expect(container.querySelector('[data-testid="agent-detail-run"]')?.textContent).not.toContain("소속 부서");
    expect(container.querySelector('[data-testid="agent-detail-now"]')?.textContent).not.toContain("부서 미배정");

    const conn = container.querySelector('[data-testid="agent-detail-connections"]')!;
    const monoSpan = conn.querySelector<HTMLElement>('span[title="1490145076658307999"]')!;
    expect(monoSpan).not.toBeNull();
    expect(monoSpan.textContent).toBe("#채널 · ...307999");
    expect(monoSpan.className).toContain("font-mono");
    expect(monoSpan.className).toContain("text-xs");
    const counterSpan = conn.querySelector<HTMLElement>('span[title="1490145076658307888"]')!;
    expect(counterSpan?.textContent).toBe("#채널 · ...307888");
  });

  it("suppresses premature Step 1/2 errors and hides retired department field in AgentSetupWizard", async () => {
    setViewport(false);
    const ko = (k: string) => k;
    const dept: Department = {
      id: "dept-infra", name: "Infra", name_ko: "인프라팀", icon: "🛠️", color: "#38bdf8",
      description: null, prompt: null, sort_order: 0, created_at: 0,
    };
    act(() => {
      root.render(
        <MemoryRouter>
          <AgentSetupWizard
            open
            mode="create"
            departments={[dept]}
            locale="ko"
            tr={ko}
            onClose={vi.fn()}
            onDone={vi.fn()}
          />
        </MemoryRouter>,
      );
    });
    await flush();

    const wizard = document.querySelector('[data-testid="agent-setup-wizard"]')!;
    const roleIdInput = wizard.querySelector<HTMLInputElement>("#wizard-role-id")!;
    expect(roleIdInput).not.toBeNull();
    expect(document.activeElement).toBe(roleIdInput);
    expect(wizard.textContent).not.toContain("역할 ID는 영문·숫자·_·- 2자 이상이어야 해요.");
    expect(Array.from(wizard.querySelectorAll("label"), (l) => l.textContent)).not.toContain("부서");
    const nextBtn = () => Array.from(wizard.querySelectorAll<HTMLButtonElement>("button")).find((b) => b.textContent === "다음")!;
    expect(nextBtn().disabled).toBe(true);

    const inputByPlaceholder = (ph: string) => wizard.querySelector<HTMLInputElement>(`input[placeholder="${ph}"]`)!;
    const setInput = (el: HTMLInputElement, val: string) => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
      setter?.call(el, val);
      el.dispatchEvent(new Event("input", { bubbles: true }));
      el.dispatchEvent(new Event("change", { bubbles: true }));
    };

    act(() => setInput(inputByPlaceholder("adk-researcher"), "a"));
    await flush();
    expect(wizard.textContent).toContain("역할 ID는 영문·숫자·_·- 2자 이상이어야 해요.");

    act(() => {
      setInput(inputByPlaceholder("adk-researcher"), "adk-researcher");
      setInput(inputByPlaceholder("Researcher"), "Researcher");
    });
    await flush();
    expect(wizard.textContent).not.toContain("역할 ID는 영문·숫자·_·- 2자 이상이어야 해요.");
    expect(nextBtn().disabled).toBe(false);

    act(() => nextBtn().click());
    await flush();
    expect(wizard.textContent).not.toContain("채널 ID는 숫자여야 해요.");
  });

  it("uses sm segmented tabs with compact 대기 label on Korean mobile and deduplicates AgentFormModal personality label", async () => {
    setViewport(true);
    const ko = (k: string) => k;
    const entries = classifyRoster(AGENTS, SESSIONS, false);
    act(() => {
      root.render(
        <MemoryRouter>
          <AgentsTab
            tr={ko} locale="ko" isKo entries={entries} sortedEntries={entries.filter((e) => !isStandIn(e.agent))}
            search="" setSearch={vi.fn()}
            statusFilter="all" setStatusFilter={vi.fn()} sortMode="status" setSortMode={vi.fn()}
            includeTests={false} setIncludeTests={vi.fn()} selectedAgentId={null} onSelectAgent={vi.fn()}
            onCreateAgent={vi.fn()} onEditAgent={vi.fn()} onDuplicateAgent={vi.fn()}
            onDeleteAgent={vi.fn()} onAgentUpdated={vi.fn()} saving={false}
          />
          <AgentFormModal
            isKo locale="ko" tr={ko} isEdit={false} saving={false}
            onSave={vi.fn()} onClose={vi.fn()}
            form={{
              name: "", name_ko: "", name_ja: "", name_zh: "", department_id: "",
              cli_provider: "claude", avatar_emoji: "🤖", personality: "", prompt_content: "", auto_commit: false,
            }}
          />
        </MemoryRouter>,
      );
    });
    await flush();

    const seg = container.querySelector<HTMLElement>(".aui-segmented-control-clean")!;
    expect(seg.getAttribute("data-size")).toBe("sm");
    const tabLabels = Array.from(seg.querySelectorAll(".aui-segmented-control-clean__label"), (el) => el.textContent);
    expect(tabLabels).toEqual(["전체", "작업 중", "대기", "쉬는 중", "세션 없음"]);

    const detailsHeading = document.body.querySelector("#agent-form-details")!;
    expect(detailsHeading.textContent).toBe("역할 · 성격");
    expect(document.body.querySelector('label[for="agent-personality"]')).toBeNull();
    const personalityArea = document.body.querySelector<HTMLTextAreaElement>("#agent-personality")!;
    expect(personalityArea.getAttribute("aria-label")).toBe("성격");
  });

  it("removes department copy and collapses idle toolu_* subagent sessions in AgentsUnlinkedSessions modal", async () => {
    setViewport(false);
    const ko = (k: string) => k;
    const baseKey = "claude/discord_1/mac-mini:AgentDesk-claude-adk-cc-t1570308839910412348";
    const unlinked: DispatchedSession[] = [
      { id: "p1", session_key: baseKey, name: "adk-cc", provider: "claude", status: "working", last_seen_at: Date.now() } as DispatchedSession,
      ...["01A", "01B", "01C", "01D"].map((suffix, idx) => ({
        id: `t${idx}`,
        session_key: `${baseKey}:child:toolu_${suffix}`,
        name: `${baseKey}:child:toolu_${suffix}`,
        provider: "claude",
        status: "idle",
        last_seen_at: Date.now() - 10_000,
      } as DispatchedSession)),
    ];
    act(() => {
      root.render(<AgentsUnlinkedSessions sessions={unlinked} locale="ko" tr={ko} isKo />);
    });
    await flush();

    act(() => {
      Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "보기")!.click();
    });
    await flush();

    const modal = document.querySelector('[data-testid="agents-sessions"]')!;
    expect(modal.textContent).toContain("세션을 에이전트에 붙이는 저장은 아직 지원하지 않으며");
    expect(modal.textContent).not.toContain("나 부서");
    expect(modal.querySelector('[data-session-row="p1"]')).not.toBeNull();
    expect(modal.querySelector('[data-session-row="t0"]')).toBeNull();

    const toggle = modal.querySelector<HTMLButtonElement>('[data-testid="toggle-subtool-sessions"]')!;
    expect(toggle.textContent).toContain("하위 도구 세션 4개 보기");
    act(() => toggle.click());
    await flush();
    expect(modal.querySelectorAll('[data-testid="agents-subtool-sessions"] li')).toHaveLength(4);
    expect(toggle.textContent).toContain("하위 도구 세션 4개 접기");
  });
});
