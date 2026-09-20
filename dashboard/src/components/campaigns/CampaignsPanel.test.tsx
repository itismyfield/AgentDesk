// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaigns, updateCampaignNode, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import CampaignsPanel from "./CampaignsPanel";
import { makeLargeCampaign } from "./campaignTestFixtures";

vi.mock("../../api/campaigns", () => ({ getCampaigns: vi.fn(), updateCampaignNode: vi.fn() }));
vi.mock("@xyflow/react", () => ({
  ReactFlow: ({ nodes, onNodeClick }: { nodes: Array<{ id: string; data: { group?: string } }>; onNodeClick?: (event: unknown, node: unknown) => void }) => <div data-graph-nodes={nodes.length}>Task graph{nodes.filter((node) => node.data.group !== undefined).map((node) => <button key={node.id} data-graph-group={node.data.group} onClick={() => onNodeClick?.({}, node)}>{node.data.group || "Ungrouped"}</button>)}</div>, Background: () => null, Controls: () => null,
  MarkerType: { ArrowClosed: "closed" }, Position: { Right: "right", Left: "left" },
}));

const campaign: Campaign = {
  id: "ongoing", title: "Long campaign", description: "Recover after compaction", status: "active", round: 3, revision: 5,
  created_at: "2026-09-20T00:00:00Z", updated_at: "2026-09-20T00:00:00Z",
  nodes: [{ id: "review", title: "Review current head", status: "running", stage: "review", group: null, round: 3,
    assignee: "reviewer", session_id: "session-42", provider: "codex", dependencies: [], issue_url: null, pr_url: null,
    head_sha: "abc123", evidence: ["Unit tests passed"], next_action: "Inspect the latest diff", blocker: null, updated_at: "2026-09-20T00:00:00Z", details: "", acceptance: [], findings: [], evidence_records: [] }],
};
let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  const storage = new Map<string, string>();
  Object.defineProperty(window, "localStorage", { configurable: true, value: {
    getItem: (key: string) => storage.get(key) ?? null,
    setItem: (key: string, value: string) => storage.set(key, value),
    removeItem: (key: string) => storage.delete(key), clear: () => storage.clear(),
  } });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
  vi.mocked(getCampaigns).mockResolvedValue([campaign]);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); vi.clearAllMocks(); });
async function render() { await act(async () => root.render(<CampaignsPanel language="en" />)); }
function button(label: string) { return Array.from(container.querySelectorAll("button")).find((value) => value.textContent === label)!; }
async function selectTask(id = "review") {
  if (!container.querySelector(`[data-node-id="${id}"]`)) await act(async () => button("Expand all").click());
  await act(async () => container.querySelector<HTMLButtonElement>(`[data-node-id="${id}"]`)!.click());
}
function selectFor(label: string): HTMLSelectElement { return Array.from(container.querySelectorAll("label")).find((element) => element.querySelector("span")?.textContent === label)!.querySelector("select")!; }
async function changeSelect(select: HTMLSelectElement, value: string) { await act(async () => { select.value = value; select.dispatchEvent(new Event("change", { bubbles: true })); }); }
async function typeValue(input: HTMLInputElement | HTMLTextAreaElement, value: string) {
  const prototype = input.tagName === "TEXTAREA" ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
  await act(async () => { Object.getOwnPropertyDescriptor(prototype, "value")!.set!.call(input, value); input.dispatchEvent(new Event("input", { bubbles: true })); });
}

it("restores the selected campaign and exposes the durable continuation checkpoint", async () => {
  const other = { ...campaign, id: "other", title: "Other", nodes: [] };
  window.localStorage.setItem(STORAGE_KEYS.dashboardActiveCampaign, JSON.stringify(campaign.id));
  vi.mocked(getCampaigns).mockResolvedValue([other, campaign]);
  await render();
  expect(selectFor("Select campaign").value).toBe("ongoing");
  expect(container.querySelector(".campaign-node-detail")).toBeNull();
  await selectTask();
  expect(container.textContent).toContain("session-42");
  expect(container.textContent).toContain("Inspect the latest diff");
  expect(container.querySelector("progress")?.value).toBe(0);
});

it("retains the prior checkpoint and explicitly marks a failed refresh", async () => {
  await render();
  await selectTask();
  vi.mocked(getCampaigns).mockRejectedValue(new Error("Offline"));
  await act(async () => button("Refresh").click());
  expect(container.textContent).toContain("Showing the previous snapshot");
  expect(container.textContent).toContain("Inspect the latest diff");
});

it("shows an empty state and distinguishes initial fetch failure", async () => {
  vi.mocked(getCampaigns).mockResolvedValue([]);
  await render();
  expect(container.textContent).toContain("No campaigns yet");
  vi.mocked(getCampaigns).mockRejectedValue(new Error("Forbidden"));
  await act(async () => button("Refresh").click());
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("Forbidden");
  expect(container.textContent).not.toContain("No campaigns yet");
});

it("preserves the edit revision and the draft when another session changes the campaign", async () => {
  await render();
  await selectTask();
  await act(async () => button("Edit task").click());
  vi.mocked(getCampaigns).mockResolvedValue([{ ...campaign, revision: 6 }]);
  await act(async () => button("Refresh").click());
  vi.mocked(updateCampaignNode).mockRejectedValue(new ApiRequestError("conflict", { status: 409 }));
  await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(vi.mocked(updateCampaignNode).mock.calls[0][0].revision).toBe(5);
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("changed elsewhere");
  expect(container.querySelector("textarea")?.value).toBe("Inspect the latest diff");
});

it("browses 120 tasks through group collapse, issue search and combined filters without a full graph", async () => {
  vi.mocked(getCampaigns).mockResolvedValue([makeLargeCampaign()]);
  await render();
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(0);
  expect(container.querySelectorAll(".campaign-group-heading")).toHaveLength(4);
  expect(container.querySelector("[data-graph-nodes]")).toBeNull();
  expect(container.textContent).toContain("5 running");
  await act(async () => button("Expand all").click());
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(120);
  await act(async () => button("Collapse all").click());
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(0);
  await act(async () => button("Expand all").click());
  await typeValue(container.querySelector<HTMLInputElement>('input[type="search"]')!, "#5701");
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(1);
  expect(container.querySelector(".campaign-task-row")?.getAttribute("data-node-id")).toBe("task-1");
  await typeValue(container.querySelector<HTMLInputElement>('input[type="search"]')!, "");
  await changeSelect(selectFor("Group filter"), JSON.stringify("Gateway"));
  await changeSelect(selectFor("Status filter"), "blocked");
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(5);
  await changeSelect(selectFor("Group filter"), "null");
  await changeSelect(selectFor("Status filter"), "all");
  await act(async () => container.querySelector<HTMLInputElement>('input[type="checkbox"]')!.click());
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(100);
});

it("preserves selection, filters, collapse and a group edit draft across polls, task changes and campaign switches", async () => {
  const large = makeLargeCampaign();
  vi.mocked(getCampaigns).mockResolvedValue([large, campaign]);
  await render();
  await selectTask("task-31");
  await act(async () => button("Edit task").click());
  const groupInput = Array.from(container.querySelectorAll("form label")).find((label) => label.textContent?.startsWith("Group"))!.querySelector("input")!;
  await typeValue(groupInput, "Gateway follow-up");
  await act(async () => container.querySelector<HTMLButtonElement>('[aria-label="Close details"]')!.click());
  await selectTask("task-32");
  await selectTask("task-31");
  expect(container.querySelector("form")?.textContent).toContain("This draft stays available");
  expect(Array.from(container.querySelectorAll("form input")).some((input) => (input as HTMLInputElement).value === "Gateway follow-up")).toBe(true);
  await changeSelect(selectFor("Group filter"), JSON.stringify("Gateway"));
  await act(async () => container.querySelector<HTMLButtonElement>(".campaign-group-heading")!.click());
  vi.mocked(getCampaigns).mockResolvedValue([{ ...large, revision: 6 }, campaign]);
  await act(async () => button("Refresh").click());
  expect(selectFor("Group filter").value).toBe(JSON.stringify("Gateway"));
  expect(container.querySelector(".campaign-group-heading")?.getAttribute("aria-expanded")).toBe("false");
  expect(container.querySelector(".campaign-node-detail h3")?.textContent).toContain("Task 31");
  await changeSelect(selectFor("Select campaign"), campaign.id);
  await changeSelect(selectFor("Select campaign"), large.id);
  await selectTask("task-31");
  vi.mocked(updateCampaignNode).mockRejectedValue(new ApiRequestError("conflict", { status: 409 }));
  await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(vi.mocked(updateCampaignNode).mock.calls[0][0].revision).toBe(5);
  expect(vi.mocked(updateCampaignNode).mock.calls[0][1].group).toBe("Gateway follow-up");
  expect(container.textContent).toContain("changed elsewhere");
});

it("keeps hidden cross-group dependencies explicit and keyboard navigation skips collapsed rows", async () => {
  vi.mocked(getCampaigns).mockResolvedValue([makeLargeCampaign()]);
  await render();
  await selectTask("task-30");
  await changeSelect(selectFor("Group filter"), JSON.stringify("Gateway"));
  expect(container.textContent).toContain("1 dependencies are outside the filters");
  await act(async () => button("Connections").click());
  expect(Number(container.querySelector("[data-graph-nodes]")?.getAttribute("data-graph-nodes"))).toBeLessThanOrEqual(17);
  expect(container.textContent).toContain("including other groups and tasks outside your filters");
  await act(async () => button("Grouped list").click());
  const first = container.querySelector<HTMLButtonElement>('[data-node-id="task-30"]')!;
  first.focus();
  await act(async () => first.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true })));
  expect(document.activeElement?.getAttribute("data-node-id")).toBe("task-31");
  await changeSelect(selectFor("Group filter"), "null");
  await act(async () => Array.from(container.querySelectorAll<HTMLButtonElement>(".campaign-group-heading")).find((heading) => heading.textContent?.includes("Restart protocol"))!.click());
  const lastGateway = container.querySelector<HTMLButtonElement>('[data-node-id="task-59"]')!;
  await act(async () => lastGateway.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true })));
  expect(document.activeElement?.getAttribute("data-node-id")).toBe("task-60");
});

it("opens a canonical group from the overview and keeps advanced mobile filters opt-in", async () => {
  vi.mocked(getCampaigns).mockResolvedValue([makeLargeCampaign()]);
  await render();
  expect(button("Filters").getAttribute("aria-expanded")).toBe("false");
  await act(async () => button("Filters").click());
  expect(button("Filters").getAttribute("aria-expanded")).toBe("true");
  await act(async () => button("Connections").click());
  expect(container.querySelector("[data-graph-nodes]")?.getAttribute("data-graph-nodes")).toBe("4");
  await act(async () => container.querySelector<HTMLButtonElement>('[data-graph-group="Gateway"]')!.click());
  expect(selectFor("Group filter").value).toBe(JSON.stringify("Gateway"));
  expect(container.querySelectorAll(".campaign-task-row")).toHaveLength(30);
  expect(button("Grouped list").getAttribute("aria-pressed")).toBe("true");
});

it("does not discard a newer draft when an older save resolves after switching away and back", async () => {
  const twoTasks = { ...campaign, nodes: [...campaign.nodes, { ...campaign.nodes[0], id: "other", title: "Other task" }] };
  vi.mocked(getCampaigns).mockResolvedValue([twoTasks]);
  let resolveSave!: (value: Campaign) => void;
  vi.mocked(updateCampaignNode).mockImplementationOnce(() => new Promise((resolve) => { resolveSave = resolve; }));
  await render();
  await selectTask();
  await act(async () => button("Edit task").click());
  await typeValue(container.querySelector("textarea")!, "first saved text");
  await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(vi.mocked(updateCampaignNode).mock.calls[0][1].next_action).toBe("first saved text");
  await selectTask("other");
  await selectTask();
  await typeValue(container.querySelector("textarea")!, "NEW UNSAVED DRAFT");
  await act(async () => resolveSave({ ...twoTasks, revision: 6, nodes: [{ ...twoTasks.nodes[0], next_action: "first saved text" }, twoTasks.nodes[1]] }));
  expect(container.querySelector("form")).not.toBeNull();
  expect(container.querySelector("textarea")?.value).toBe("NEW UNSAVED DRAFT");
  await act(async () => button("Cancel").click());
  expect(container.querySelector("form")).toBeNull();
  expect(container.querySelector(".campaign-next")?.textContent).toContain("first saved text");
});

it("never regresses a newer polled revision when an older save response arrives", async () => {
  let resolveSave!: (value: Campaign) => void;
  vi.mocked(updateCampaignNode).mockImplementationOnce(() => new Promise((resolve) => { resolveSave = resolve; }));
  await render();
  await selectTask();
  await act(async () => button("Edit task").click());
  await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  const latest = { ...campaign, revision: 9, nodes: [{ ...campaign.nodes[0], next_action: "Newer authoritative checkpoint" }] };
  vi.mocked(getCampaigns).mockResolvedValue([latest]);
  await act(async () => button("Refresh").click());
  await act(async () => resolveSave({ ...campaign, revision: 6 }));
  expect(container.querySelector("form")).toBeNull();
  expect(container.querySelector(".campaign-next")?.textContent).toContain("Newer authoritative checkpoint");
  // A stale read response is fenced too, regardless of its request arrival order.
  vi.mocked(getCampaigns).mockResolvedValue([{ ...campaign, revision: 7 }]);
  await act(async () => button("Refresh").click());
  expect(container.querySelector(".campaign-next")?.textContent).toContain("Newer authoritative checkpoint");
  await act(async () => button("Edit task").click());
  vi.mocked(updateCampaignNode).mockResolvedValue(latest);
  await act(async () => container.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true })));
  expect(vi.mocked(updateCampaignNode).mock.calls[1][0].revision).toBe(9);
});
