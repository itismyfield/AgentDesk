// @vitest-environment happy-dom
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, useState } from "react";
import { createRoot, type Root } from "react-dom/client";
import { MemoryRouter, useLocation } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import * as api from "../src/api";
import { ApiRequestError, request } from "../src/api/httpClient";
import { getAgentAnchors, getPromptAudit } from "../src/api/promptAudit";
import AgentFormModal from "../src/components/agent-manager/AgentFormModal";
import { AgentInfoCardContent } from "../src/components/agent-manager/AgentInfoCardContent";
import { turnMonitorSteps } from "../src/components/agent-manager/AgentInfoCardModel";
import AgentPromptEditor, { hasMaskedPromptValue, parsePromptHeadings } from "../src/components/agent-manager/AgentPromptEditor";
import { AgentsSummaryPane } from "../src/components/agent-manager/AgentsSummaryPane";
import { useDiscordChannels } from "../src/components/agent-manager/useAgentDetailData";
import { AnchorAuditDetail } from "../src/components/audit/AnchorAuditPanel";
import { PromptAuditDetail } from "../src/components/audit/PromptAuditPanel";
import { UnlinkedSessions } from "../src/components/session-panel/SessionPanel";
import type { Agent, DispatchedSession } from "../src/types";

vi.mock("../src/api/httpClient", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../src/api/httpClient")>();
  return { ...actual, request: vi.fn() };
});

vi.mock("../src/api", () => ({
  getSkillRanking: vi.fn(async () => ({
    byAgent: [
      { agent_role_id: "agent-ada", agent_name: "Ada", skill_name: "agents-hub", calls: 7 },
      { agent_role_id: "agent-ada", agent_name: "Ada", skill_name: "shared-ops", calls: 3 },
    ],
  })),
  getAgentCron: vi.fn(async () => []),
  getAgentSkills: vi.fn(async () => ({
    skills: [{ name: "agents-hub", description: "Dedicated roster hub skill" }],
    sharedSkills: [{ name: "shared-ops", description: "Shared ops runbook skill" }],
  })),
  getAgentDispatchedSessions: vi.fn(async () => []),
  getDiscordBindings: vi.fn(async () => []),
  getAgentTimeline: vi.fn(async () => []),
  getAuditLogs: vi.fn(async () => []),
  getDiscordChannelInfo: vi.fn(async () => null),
  getTaskDispatches: vi.fn(async () => []),
  updateAgent: vi.fn(async () => ({})),
}));

vi.mock("../src/api/promptAudit", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../src/api/promptAudit")>();
  return {
    ...actual,
    getPromptAudit: vi.fn(),
    getAgentAnchors: vi.fn(),
  };
});

vi.mock("../src/contexts/KanbanContext", () => ({
  useKanban: () => ({ kanbanCards: [], kanbanCardsState: "ready" }),
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

const ko = (k: string) => k;
const NOW = Date.now();
const DAY = 86_400_000;

const ADA: Agent = {
  id: "agent-ada",
  name: "Ada",
  name_ko: "아다",
  alias: "ada",
  department_id: null,
  cli_provider: "claude",
  avatar_emoji: "🤖",
  personality: null,
  status: "working",
  stats_tasks_done: 12,
  stats_tokens: 1000,
  created_at: NOW - DAY,
  discord_channel_cc: "100",
};

let root: Root;
let container: HTMLDivElement;
const confirmMock = vi.fn<() => boolean>(() => true);

async function flush() {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
}

beforeEach(() => {
  confirmMock.mockReset().mockReturnValue(true);
  window.confirm = confirmMock;
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query.includes("prefers-reduced-motion"),
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

describe("Slice 2 — Agents summary, unlinked sessions, live turn control, prompt editor & audit kit", () => {
  it("renders provider distribution in AgentsSummaryPane and rounds CompactBreakdown headcount to whole integers", async () => {
    const entries = [
      { agent: { ...ADA, id: "a1", cli_provider: "claude" as const }, state: "working" as const, info: null, seenAt: null },
      { agent: { ...ADA, id: "a2", cli_provider: "claude" as const }, state: "resting" as const, info: null, seenAt: null },
      { agent: { ...ADA, id: "a3", cli_provider: "codex" as const }, state: "resting" as const, info: null, seenAt: null },
    ];
    act(() => {
      root.render(
        <AgentsSummaryPane
          counted={entries}
          entries={entries}
          locale="ko"
          tr={ko}
          isKo
          unlinkedSessions={[]}
        />,
      );
    });
    await flush();
    const cardText = Array.from(
      container.querySelectorAll(".aui-compact-breakdown-number, .aui-compact-breakdown-value, .aui-compact-breakdown-label"),
      (el) => el.textContent ?? "",
    ).join(" | ");
    expect(cardText).toContain("Claude");
    expect(cardText).toContain("Codex");
    expect(cardText).not.toContain("부서");
    expect(cardText).not.toMatch(/\d+\.\d+/);
  });

  it("shows honest outer unlinked-session counts without re-flagging reconciled idle notes, previews /output?lines=80, and reconciles stale turns", async () => {
    const baseKey = "claude/discord_1/mac-mini:AgentDesk-claude-adk-cc-t1570308839910412348";
    const sessions = [
      {
        id: "101",
        session_key: `${baseKey}:child:toolu_01Alpha`,
        name: `${baseKey}:child:toolu_01Alpha`,
        provider: "claude",
        status: "turn_active",
        session_info: "Sublane running",
        avatar_emoji: "📡",
        tokens: 0,
        connected_at: NOW - 60_000,
        last_seen_at: NOW - 30_000,
        department_id: null,
        linked_agent_id: null,
      },
      {
        id: "102",
        session_key: "antigravity/discord_1/mac-mini:AgentDesk-ag-game-ag",
        name: "game-ag",
        provider: "antigravity",
        status: "turn_active",
        session_info: "Stuck turn",
        avatar_emoji: "📡",
        tokens: 0,
        connected_at: NOW - 2 * 3_600_000,
        last_seen_at: NOW - 45 * 60_000,
        department_id: null,
        linked_agent_id: null,
      },
      {
        id: "103",
        session_key: "claude/discord_1/mac-mini:AgentDesk-claude-already-reconciled",
        name: "already-reconciled",
        provider: "claude",
        status: "idle",
        session_info: "reconciled stale turn_active",
        avatar_emoji: "📡",
        tokens: 0,
        connected_at: NOW - 300_000,
        last_seen_at: NOW - 120_000,
        department_id: null,
        linked_agent_id: null,
      },
      {
        id: "104",
        session_key: "antigravity/discord_1/mac-mini:AgentDesk-ag-old-idle",
        name: "old-idle",
        provider: "antigravity",
        status: "idle",
        session_info: null,
        avatar_emoji: "📡",
        tokens: 0,
        connected_at: NOW - 10 * DAY,
        last_seen_at: NOW - 9 * DAY,
        department_id: null,
        linked_agent_id: null,
      },
    ] as DispatchedSession[];

    vi.mocked(request).mockImplementation(async (url: string) => {
      if (url.includes("/api/sessions/101/output?lines=80")) {
        return {
          recent_output: "cargo test --workspace\ntest result: ok.",
          backend: "tmux",
          available: true,
          unavailable_reason: null,
          output_format: "text",
          captured_at_ms: NOW,
        };
      }
      if (url.includes("/reconcile-stale-turn")) {
        return { ok: true, reconciled: true, status: "idle" };
      }
      throw new Error(`Unexpected request: ${url}`);
    });

    const onSessionsChange = vi.fn();
    act(() => {
      root.render(
        <UnlinkedSessions
          sessions={sessions}
          departments={[]}
          locale="ko"
          tr={ko}
          isKo
          onSessionsChange={onSessionsChange}
        />,
      );
    });
    await flush();

    // Row 103 (`reconciled stale ...` with fresh signal) counts as resting; 102 (45m turn_active) + 104 (9d idle) count as stale
    expect(container.querySelector('[data-testid="unlinked-sessions-summary"]')?.textContent).toBe("작업 중 1 · 멈춤 2 · 쉬는 중 1");

    act(() => {
      Array.from(container.querySelectorAll("button")).find((b) => b.textContent === "보기")!.click();
    });
    await flush();

    const sheet = document.body;
    expect(sheet.textContent).not.toContain("#8373");
    expect(sheet.textContent).toContain("#adk-cc-t1570308839910412348 · toolu_01Alpha");
    expect(sheet.textContent).toContain("7일 넘게 멈춤");
    // Already-reconciled row 103 and 9d-idle row 104 must NOT show a reconcile button (only stale active turns do)
    expect(sheet.querySelector('[data-testid="reconcile-session-103"]')).toBeNull();
    expect(sheet.querySelector('[data-testid="reconcile-session-104"]')).toBeNull();

    act(() => {
      sheet.querySelector<HTMLButtonElement>('[data-session-row="101"]')!.click();
    });
    await flush();
    await flush();
    expect(vi.mocked(request)).toHaveBeenCalledWith(
      "/api/sessions/101/output?lines=80",
      expect.objectContaining({ cache: "no-store", suppressErrorToast: true }),
      expect.anything(),
    );
    expect(sheet.querySelector('[data-testid="session-output-101"]')?.textContent).toContain("cargo test --workspace");

    // Reconcile genuinely stuck active turn row 102 (turn_active, 45m without heartbeat)
    const reconcileBtn = sheet.querySelector<HTMLButtonElement>('[data-testid="reconcile-session-102"]')!;
    expect(reconcileBtn).not.toBeNull();
    await act(async () => {
      reconcileBtn.click();
    });
    await flush();
    expect(vi.mocked(request)).toHaveBeenCalledWith(
      `/api/sessions/${encodeURIComponent(sessions[1].session_key)}/reconcile-stale-turn`,
      expect.objectContaining({ method: "POST" }),
      expect.anything(),
    );
    expect(onSessionsChange).toHaveBeenCalledTimes(1);
    expect(sheet.textContent).toContain("정리됨");
    expect(sheet.querySelector('[data-testid="reconcile-session-102"]')).toBeNull();
  });

  it("confirms before stopping a turn, wires skill drilldown onSelect, recovers 401 channel lookups, and retries failed open sections", async () => {
    const turns = new Map([
      [
        "agent-ada",
        {
          status: "working",
          started_at: new Date(NOW - 15_000).toISOString(),
          updated_at: new Date(NOW - 1_000).toISOString(),
          provider: "claude",
          current_tool_line: "Bash: npm test",
          tool_events: [
            { kind: "tool", status: "done", tool_name: "Read", summary: "Read AgentsTab.tsx", line: "Read AgentsTab.tsx" },
            { kind: "tool", status: "running", tool_name: "Bash", summary: "Run vitest", line: "npx vitest run" },
          ],
        },
      ],
    ]);

    expect(turnMonitorSteps(turns.get("agent-ada"), ko)).toMatchObject([
      { label: "Read AgentsTab.tsx", kind: "read", status: "done", tool: "Read" },
      { label: "Run vitest", kind: "run", status: "running", tool: "Bash", output: "npx vitest run" },
    ]);

    vi.mocked(api.getAgentCron)
      .mockRejectedValueOnce(new Error("transient network error"))
      .mockResolvedValueOnce([{ id: "c1", name: "Nightly check", enabled: true, schedule: { kind: "every", everyMs: 3_600_000 } }] as never);

    vi.mocked(request).mockImplementation(async (url: string) => {
      if (url === "/api/agents/agent-ada/turn/stop") return { ok: true };
      throw new Error(`Unexpected: ${url}`);
    });

    confirmMock.mockClear().mockReturnValueOnce(false).mockReturnValueOnce(true);
    const onUpdated = vi.fn();
    act(() => {
      root.render(
        <MemoryRouter>
          <AgentInfoCardContent
            agent={ADA}
            state="working"
            isKo
            locale="ko"
            tr={ko}
            turns={turns}
            onAgentUpdated={onUpdated}
          />
        </MemoryRouter>,
      );
    });
    await flush();
    await flush();

    expect(container.textContent).toContain("Run vitest");
    const cancelBtn = Array.from(container.querySelectorAll<HTMLButtonElement>("button.aui-agent-monitor-action"))
      .find((b) => /취소|중단|Cancel/i.test(b.textContent ?? b.getAttribute("aria-label") ?? ""))!;
    expect(cancelBtn).toBeDefined();

    // Declining AlertDialog does NOT call turn/stop
    await act(async () => { cancelBtn.click(); });
    const stopDialog = document.body.querySelector<HTMLDialogElement>('dialog[role="alertdialog"][open]')!;
    expect(stopDialog).not.toBeNull();
    expect(confirmMock).not.toHaveBeenCalled();
    await act(async () => {
      stopDialog.querySelector<HTMLButtonElement>(".aui-dialog-clean__btn:not([data-tone])")!.click();
    });
    await flush();
    expect(vi.mocked(request)).not.toHaveBeenCalled();

    // Accepting AlertDialog calls POST /api/agents/agent-ada/turn/stop
    await act(async () => { cancelBtn.click(); });
    const stopDialog2 = document.body.querySelector<HTMLDialogElement>('dialog[role="alertdialog"][open]')!;
    expect(stopDialog2).not.toBeNull();
    await act(async () => {
      stopDialog2.querySelector<HTMLButtonElement>('.aui-dialog-clean__btn[data-tone="danger"]')!.click();
    });
    await flush();
    expect(vi.mocked(request)).toHaveBeenCalledWith(
      "/api/agents/agent-ada/turn/stop",
      expect.objectContaining({ method: "POST" }),
    );
    expect(container.querySelector('[data-testid="agent-stop-feedback"]')?.textContent).toContain("턴 중단 요청을 보냈어요");
    expect(onUpdated).toHaveBeenCalled();

    // Skill CompactBreakdown onSelect opens drilldown and filters skills
    const skillBtn = Array.from(container.querySelectorAll<HTMLButtonElement>('[data-testid="agent-detail-skills"] button'))
      .find((b) => (b.getAttribute("aria-label") ?? "").startsWith("shared-ops,"))!;
    act(() => skillBtn.click());
    await flush();
    const drill = container.querySelector('[data-testid="agent-skill-drilldown"]')!;
    expect(drill.textContent).toContain("공유 스킬");
    expect(drill.textContent).toContain("Shared ops runbook skill");

    // Open Cron section -> fails once -> click Retry while keeping section open -> loads job
    const cronToggle = Array.from(container.querySelectorAll<HTMLButtonElement>('[data-testid="agent-detail-cron"] h3 button'))[0]!;
    act(() => cronToggle.click());
    await flush();
    expect(container.querySelector('[data-testid="agent-detail-cron"]')?.textContent).toContain("크론을 불러오지 못했어요");
    const retryBtn = Array.from(container.querySelectorAll<HTMLButtonElement>('[data-testid="agent-detail-cron"] button'))
      .find((b) => b.textContent?.includes("다시 시도"))!;
    act(() => retryBtn.click());
    await flush();
    expect(container.querySelector('[data-testid="agent-detail-cron"]')?.textContent).toContain("Nightly check");

    // 401 channel lookup is NOT cached permanently and retries on auth recovery (focus)
    vi.mocked(api.getDiscordChannelInfo)
      .mockReset()
      .mockRejectedValueOnce(new ApiRequestError("unauthorized", { status: 401 }))
      .mockResolvedValueOnce({ id: "1490145076658307999", name: "recovered-channel", guild_id: "1", parent_id: null });

    function ChannelProbe() {
      const map = useDiscordChannels(["1490145076658307999"]);
      return <div data-testid="channel-probe">{map["1490145076658307999"]?.name ?? "fallback-id"}</div>;
    }
    act(() => root.render(<ChannelProbe />));
    await flush();
    expect(container.querySelector('[data-testid="channel-probe"]')?.textContent).toBe("fallback-id");
    act(() => { window.dispatchEvent(new Event("focus")); });
    await flush();
    await flush();
    expect(container.querySelector('[data-testid="channel-probe"]')?.textContent).toBe("recovered-channel");
  });

  it("preserves undecided draft edits when accepting one proposal in InlineDiff, avoids Markdown *** false positives, and confirms dirty audit navigation", async () => {
    expect(hasMaskedPromptValue("# Heading\n\n***\n\nSection with ***bold italic*** text.")).toBe(false);
    expect(hasMaskedPromptValue("   *****   ")).toBe(false);
    expect(hasMaskedPromptValue("token=***")).toBe(true);
    expect(hasMaskedPromptValue("Authorization: Bearer ***")).toBe(true);

    const headings = parsePromptHeadings("# Role Overview\nIntro\n## Safety Rules\n```md\n# ignored in fence\n```\n## Output Style");
    expect(headings.tree).toHaveLength(1);
    expect(headings.tree[0].children?.map((c) => c.label)).toEqual(["## Safety Rules", "## Output Style"]);

    const initial = "# Role\nAlpha old.\n\n## Constraints\nBeta old.";
    let latestDraft = "# Role\nAlpha new.\n\n## Constraints\nBeta new.";

    function EditorHarness() {
      const [val, setVal] = useState(latestDraft);
      latestDraft = val;
      return (
        <MemoryRouter initialEntries={["/agents"]}>
          <AgentPromptEditor
            label="prompt.md"
            value={val}
            initialValue={initial}
            agentId="agent-ada"
            onChange={setVal}
            tr={ko}
          />
        </MemoryRouter>
      );
    }

    act(() => root.render(<EditorHarness />));
    await flush();

    // Switch to InlineDiff split tab and accept ONLY the first proposal with `Y`
    const diffTab = Array.from(container.querySelectorAll<HTMLElement>('[role="tab"]')).find((el) => el.textContent?.includes("변경점 검증"))!;
    act(() => diffTab.click());
    await flush();
    const firstProposal = container.querySelector<HTMLElement>('[data-testid="prompt-inline-diff"] [data-change-index="0"]')!;
    expect(firstProposal).not.toBeNull();
    act(() => {
      firstProposal.focus();
      firstProposal.dispatchEvent(new KeyboardEvent("keydown", { key: "y", bubbles: true }));
    });
    await flush();

    // Second undecided edit ("Beta new.") MUST NOT be overwritten with "Beta old."
    expect(latestDraft).toContain("Alpha new.");
    expect(latestDraft).toContain("Beta new.");

    // Switch back to Edit tab: both edits remain intact
    const editTab = Array.from(container.querySelectorAll<HTMLElement>('[role="tab"]')).find((el) => el.textContent === "편집")!;
    act(() => editTab.click());
    await flush();
    expect(container.querySelector<HTMLTextAreaElement>('textarea[aria-label="prompt.md"]')?.value).toContain("Beta new.");

    // AgentFormModal: dirty navigation confirmation + masked save guard inside handleSave
    const onSave = vi.fn();
    const onClose = vi.fn();
    confirmMock.mockClear().mockReturnValueOnce(false).mockReturnValueOnce(true);

    function LocationProbe() {
      const loc = useLocation();
      return <output data-testid="router-loc">{`${loc.pathname}${loc.search}`}</output>;
    }

    act(() => {
      root.render(
        <MemoryRouter initialEntries={["/agents"]}>
          <LocationProbe />
          <AgentFormModal
            isKo
            locale="ko"
            tr={ko}
            isEdit
            agentId="agent-ada"
            saving={false}
            onSave={onSave}
            onClose={onClose}
            form={{
              name: "Ada",
              name_ko: "아다",
              name_ja: "",
              name_zh: "",
              department_id: "",
              cli_provider: "claude",
              avatar_emoji: "🤖",
              personality: "",
              prompt_content: "# Clean prompt\nNo masks here.",
              auto_commit: false,
            }}
          />
        </MemoryRouter>,
      );
    });
    await flush();

    const promptArea = document.body.querySelector<HTMLTextAreaElement>('textarea[aria-label="prompt.md"]')!;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set;
      setter?.call(promptArea, "# Clean prompt\nsecret=***");
      promptArea.dispatchEvent(new Event("input", { bubbles: true }));
      promptArea.dispatchEvent(new Event("change", { bubbles: true }));
    });
    await flush();

    // Direct form submit is blocked inside handleSave until confirmed
    const formEl = document.body.querySelector("form")!;
    await act(async () => {
      formEl.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    });
    await flush();
    expect(onSave).not.toHaveBeenCalled();

    const saveBtn = Array.from(document.body.querySelectorAll<HTMLButtonElement>('button[type="submit"]'))[0]!;
    expect(saveBtn.disabled).toBe(true);

    // Clicking audit jump while dirty opens AlertDialog: declining stays on /agents, accepting navigates
    const auditBtn = document.body.querySelector<HTMLButtonElement>('[data-testid="prompt-open-audit"]')!;
    act(() => auditBtn.click());
    const auditDialog = document.body.querySelector<HTMLDialogElement>('dialog[role="alertdialog"][open]')!;
    expect(auditDialog).not.toBeNull();
    expect(confirmMock).not.toHaveBeenCalled();
    act(() => {
      auditDialog.querySelector<HTMLButtonElement>(".aui-dialog-clean__btn:not([data-tone])")!.click();
    });
    await flush();
    expect(onClose).not.toHaveBeenCalled();
    expect(container.querySelector('[data-testid="router-loc"]')?.textContent).toBe("/agents");

    act(() => auditBtn.click());
    const auditDialog2 = document.body.querySelector<HTMLDialogElement>('dialog[role="alertdialog"][open]')!;
    expect(auditDialog2).not.toBeNull();
    act(() => {
      auditDialog2.querySelector<HTMLButtonElement>('.aui-dialog-clean__btn[data-tone="danger"]')!.click();
    });
    await flush();
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(container.querySelector('[data-testid="router-loc"]')?.textContent).toBe("/audit?agent=agent-ada");
  });

  it("aligns PromptAuditDetail TreeView line numbers with trimmed CodeBlock lines and suppresses false session-copy notice on 0-anchor sessions", async () => {
    vi.mocked(getPromptAudit).mockResolvedValue({
      agent_id: "agent-ada",
      provider: "claude",
      origin: "current_session",
      session_id: "s-1",
      session_started_at: "2026-11-24T01:00:00Z",
      masked_count: 0,
      sections: [
        { name: "role_prompt", source: "canon_file", path: "agents/ada.prompt.md", chars: 38, masked_count: 0, text: "\n\n# Ada Role\n## Mission\nShip cleanly.\n" },
      ],
    });
    vi.mocked(getAgentAnchors).mockResolvedValue({
      agent_id: "agent-ada",
      provider: "claude",
      source: "latest_saved",
      session_id: "s-1",
      session_started_at: "2026-11-24T01:00:00Z",
      memento: { status: "disconnected", checked_at: "2026-11-24T02:00:00Z" },
      anchor_count: 0,
      anchors: [],
    });

    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    act(() => {
      root.render(
        <QueryClientProvider client={client}>
          <PromptAuditDetail
            agent={ADA}
            provider="claude"
            providers={["claude"]}
            onProvider={vi.fn()}
            phone={false}
            isKo
            tr={ko}
            onOpenAnchors={vi.fn()}
          />
          <AnchorAuditDetail
            agent={ADA}
            provider="claude"
            providers={["claude"]}
            onProvider={vi.fn()}
            phone={false}
            isKo
            tr={ko}
            agents={[ADA]}
          />
        </QueryClientProvider>,
      );
    });
    await flush();
    await flush();

    // Expand section node in TreeView: leading \n\n was trimmed so `# Ada Role` MUST be L1, not L3
    const tocRow = container.querySelector<HTMLElement>('[data-testid="audit-prompt-toc"] .aui-tree-view-clean__row')!;
    act(() => tocRow.click());
    await flush();
    const metas = Array.from(container.querySelectorAll('[data-testid="audit-prompt-toc"] .aui-tree-view-clean__meta'), (el) => el.textContent);
    expect(metas).toContain("L1");
    expect(metas).toContain("L2");
    expect(metas).not.toContain("L3");
    expect(container.querySelector(".aui-code-block-clean__ln")?.textContent).toBe("1");

    const notice = container.querySelector('[data-testid="audit-memento-notice"]')!;
    expect(notice.textContent).not.toContain("저장한 사본이에요");
    const headerMetas = Array.from(container.querySelectorAll('[data-testid="audit-detail-meta"]'), (n) => n.textContent).join(" | ");
    expect(headerMetas).toContain("저장된 앵커 없음");
    expect(headerMetas).not.toContain("세션 사본");
  });
});
