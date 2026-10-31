// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { addCampaignNode, getCampaign, type Campaign } from "../../api/campaigns";
import { updateKanbanCard } from "../../api/kanban";
import { KanbanProvider } from "../../contexts/KanbanContext";
import type { Agent, KanbanCard } from "../../types";
import CampaignQueueTab from "./CampaignQueueTab";
import QueueAddToCampaign from "./QueueAddToCampaign";
import { makeCampaignNode } from "./campaignTestFixtures";

const agent = { id: "agent-1", name: "Builder" } as Agent;
vi.mock("../../contexts/WorkspaceContext", () => ({ useWorkspace: () => ({ agents: [agent] }) }));
vi.mock("../../api/kanban", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/kanban")>()),
  updateKanbanCard: vi.fn(), getKanbanCard: vi.fn(),
}));
vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()),
  getCampaign: vi.fn(), addCampaignNode: vi.fn(),
}));

function card(id: string, patch: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id, title: id, description: null, status: "backlog", github_repo: "itismyfield/AgentDesk", owner_agent_id: null, requester_agent_id: null,
    assignee_agent_id: null, parent_card_id: null, latest_dispatch_id: null, sort_order: 0, priority: "medium", depth: 0, blocked_reason: null,
    review_notes: null, github_issue_number: null, github_issue_url: null, metadata_json: null, pipeline_stage_id: null, review_status: null,
    created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z", started_at: null, requested_at: null, completed_at: null, ...patch,
  };
}
const campaign: Campaign = {
  id: "dag", title: "Open issues DAG", description: "", status: "active", round: 1, revision: 4, auto_queue: false,
  created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z",
  nodes: [makeCampaignNode("i10", { title: "#10 already planned" })],
};
const tr = (_: string, en: string) => en;
let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.clearAllMocks(); });
const rowFor = (id: string) => container.querySelector(`[data-card-id="${id}"]`);
const buttonIn = (scope: ParentNode | null, text: string) => [...(scope?.querySelectorAll("button") ?? [])].find((button) => button.textContent === text)!;

async function renderQueue(cards: KanbanCard[]) {
  await act(async () => root.render(<KanbanProvider initialCards={cards} initialDispatches={[]}>
    <CampaignQueueTab language="en" tr={tr} campaigns={[campaign]} campaignsLoaded campaignsError={null} onRetryCampaigns={vi.fn()} selectedCardId={null} onSelectCard={vi.fn()} onCampaignSaved={vi.fn()} />
  </KanbanProvider>));
}

it("lists open cards no campaign holds, one line per title", async () => {
  await renderQueue([
    card("held", { github_issue_number: 10 }),
    card("free", { github_issue_number: 11, title: "Relay\\nrestart" }),
    card("done", { github_issue_number: 12, status: "done" }),
  ]);
  expect([...container.querySelectorAll("[data-card-id]")].map((row) => row.getAttribute("data-card-id"))).toEqual(["free"]);
  expect(rowFor("free")?.querySelector("strong")?.textContent).toBe("Relay restart");
  expect(container.querySelector('[role="status"]')?.textContent).toBe("1 cards outside campaigns");
});

it("moves an assigned card to ready, and asks for an assignee first when it has none", async () => {
  vi.mocked(updateKanbanCard).mockImplementation(async (id, patch) => ({ ...card(id), ...patch }) as KanbanCard);
  await renderQueue([card("assigned", { assignee_agent_id: "agent-1" }), card("open")]);
  await act(async () => buttonIn(rowFor("assigned"), "To ready").click());
  expect(updateKanbanCard).toHaveBeenCalledExactlyOnceWith("assigned", { status: "ready" });
  expect(rowFor("assigned")?.textContent).toContain("Ready");

  await act(async () => buttonIn(rowFor("open"), "To ready").click());
  const dialog = document.querySelector('[role="dialog"]')!;
  const select = dialog.querySelector("select")!;
  await act(async () => { select.value = "agent-1"; select.dispatchEvent(new Event("change", { bubbles: true })); });
  await act(async () => buttonIn(dialog, "Move to ready").click());
  expect(vi.mocked(updateKanbanCard).mock.calls.slice(1)).toEqual([["open", { assignee_agent_id: "agent-1" }], ["open", { status: "ready" }]]);
});

it("puts a card into the newest revision of a campaign as a pending task", async () => {
  const latest = { ...campaign, revision: 5 };
  vi.mocked(getCampaign).mockResolvedValue(latest);
  vi.mocked(addCampaignNode).mockImplementation(async (value, node) => ({ ...value, revision: 6, nodes: [...value.nodes, node] }));
  const onAdded = vi.fn();
  await act(async () => root.render(<QueueAddToCampaign card={card("c", { github_issue_number: 77, assignee_agent_id: "agent-1" })} campaigns={[campaign]} tr={tr} onAdded={onAdded} />));
  await act(async () => buttonIn(container, "Add").click());
  expect(getCampaign).toHaveBeenCalledWith("dag");
  expect(vi.mocked(addCampaignNode).mock.calls[0][0]).toBe(latest);
  expect(vi.mocked(addCampaignNode).mock.calls[0][1]).toMatchObject({
    id: "i77", title: "#77 c", status: "pending", issue_url: "https://github.com/itismyfield/AgentDesk/issues/77", assignee: "agent-1",
  });
  expect(onAdded).toHaveBeenCalledWith(expect.objectContaining({ revision: 6 }));
  expect(container.querySelector('[role="status"]')?.textContent).toBe("Added to 'Open issues DAG'.");
});

it("says it cannot tell which cards are free when the campaigns did not load", async () => {
  const retry = vi.fn();
  await act(async () => root.render(<KanbanProvider initialCards={[card("free", { github_issue_number: 11 })]} initialDispatches={[]}>
    <CampaignQueueTab language="en" tr={tr} campaigns={[]} campaignsLoaded={false} campaignsError="Failed to fetch" onRetryCampaigns={retry} selectedCardId={null} onSelectCard={vi.fn()} onCampaignSaved={vi.fn()} />
  </KanbanProvider>));
  expect(container.querySelector("[data-card-id]")).toBeNull();
  expect(container.textContent).toContain("Campaigns did not load");
  await act(async () => buttonIn(container, "Retry").click());
  expect(retry).toHaveBeenCalledOnce();
});
