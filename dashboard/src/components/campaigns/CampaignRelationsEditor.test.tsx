// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Connection, Edge, Node, NodeChange } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaignDetail, saveCampaignNodes, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import CampaignRelationsEditor from "./CampaignRelationsEditor";
import { makeCampaignNode as node } from "./campaignTestFixtures";
import { dropKeptDraft, keptDraft, type EditScope } from "./relationsEditorModel";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()), getCampaignDetail: vi.fn(), saveCampaignNodes: vi.fn(),
}));
type FlowProps = { nodes: Node[]; edges: Edge[]; onNodesChange: (changes: NodeChange[]) => void; onConnect: (connection: Connection) => void; onEdgesDelete: (edges: Edge[]) => void };
const flow: { props: FlowProps | null } = { props: null };
vi.mock("@xyflow/react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@xyflow/react")>()),
  ReactFlow: (props: FlowProps) => {
    flow.props = props;
    return <div>{props.nodes.map((value) => <button key={value.id} className="react-flow__node" data-node={value.id} data-id={value.id} onClick={() => props.onNodesChange([{ type: "select", id: value.id, selected: true }])}>{value.ariaLabel}</button>)}</div>;
  },
  Background: () => null, Controls: () => null,
}));

// A done → B → C, and B → D skipped.
const base: Campaign = {
  id: "rel", title: "Relations", description: "", status: "active", round: 1, revision: 4, auto_queue: false,
  created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z",
  nodes: [node("A", { status: "completed", group: "G" }), node("B", { group: "G", dependencies: ["A"] }), node("C", { group: "G", dependencies: ["B"] }), node("D", { status: "skipped", group: "G", dependencies: ["B"] })],
};
const G: EditScope = { kind: "group", key: "G" };
let container: HTMLDivElement;
let root: Root;
const leave: { current: ((proceed: () => void) => void) | null } = { current: null };
const onSaved = vi.fn();
const onExit = vi.fn();
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove(); dropKeptDraft(base.id); vi.clearAllMocks();
});
const render = (campaign = base, scope = G) => act(async () => root.render(<CampaignRelationsEditor campaign={campaign} scope={scope} onScope={vi.fn()} onExit={onExit} onSaved={onSaved} leave={leave} tr={(_: string, en: string) => en} />));
const text = () => document.body.textContent ?? "";
const named = (name: string) => [...document.querySelectorAll<HTMLElement>("button, select, input")].find((element) => element.getAttribute("aria-label") === name || element.textContent?.trim() === name)!;
const click = (element: HTMLElement) => act(async () => element.click());
const pick = (id: string) => click(container.querySelector<HTMLElement>(`[data-node="${id}"]`)!);
const choose = (name: string, value: string) => act(async () => {
  const select = named(name) as HTMLSelectElement;
  select.value = value; select.dispatchEvent(new Event("change", { bubbles: true }));
});
const saveButton = () => [...container.querySelectorAll("button")].find((element) => element.textContent === "Save")!;
const typeTitle = (value: string) => act(async () => { const input = container.querySelector<HTMLInputElement>("aside input")!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value); input.dispatchEvent(new Event("input", { bubbles: true })); });

it("edits dependencies with the inspector's chips alone and blocks saving while a cycle stands", async () => {
  await render();
  await pick("B");
  expect(named("Remove dependency A A")).toBeTruthy();
  await choose("Add a dependency", "C");
  expect(text()).toContain("Cycle: B → C → B");
  expect(text()).toContain("Remove the dependency on C to break it.");
  expect(text()).toContain("1 errors");
  expect(saveButton().disabled).toBe(true);
  await click(named("Remove dependency C C"));
  await click(named("Remove dependency A A"));
  expect(text()).toContain("Checks pass");
  expect(text()).toContain("Unsaved · 1 dependencies");
  expect(saveButton().disabled).toBe(false);
  // Two undo steps go back past the empty draft to the cycle.
  await click(named("Undo"));
  expect(text()).not.toContain("Unsaved");
  await click(named("Undo"));
  expect(text()).toContain("Cycle: B → C → B");
});

it("keeps completed and skipped tasks unchanged from the canvas, the inspector and line removal", async () => {
  await render();
  await pick("A");
  expect(text()).toContain("Completed · cannot change");
  expect(container.querySelector<HTMLInputElement>("aside input")!.disabled).toBe(true);
  expect(named("Add a dependency")).toBeUndefined();
  await act(async () => flow.props!.onConnect({ source: "C", target: "A", sourceHandle: null, targetHandle: null }));
  await act(async () => flow.props!.onEdgesDelete([{ id: "B->D", source: "B", target: "D" }]));
  expect(text()).not.toContain("Unsaved");
  expect(flow.props!.edges.find((edge) => edge.id === "B->D")?.deletable).toBe(false);
  expect(flow.props!.edges.find((edge) => edge.id === "A->B")?.deletable).toBe(true);
  await act(async () => flow.props!.onEdgesDelete([{ id: "A->B", source: "A", target: "B" }]));
  expect(text()).toContain("Unsaved · 1 dependencies");
});

it("sends a 409 again up to three times, each on a fresh read, and takes the saved copy as the new base", async () => {
  let revision = base.revision;
  vi.mocked(getCampaignDetail).mockImplementation(async () => ({ campaign: { ...base, revision: ++revision }, live: {}, read: revision }));
  const conflict = new ApiRequestError("conflict", { status: 409 });
  vi.mocked(saveCampaignNodes).mockRejectedValueOnce(conflict).mockRejectedValueOnce(conflict).mockRejectedValueOnce(conflict)
    .mockImplementationOnce(async (campaign, nodes) => ({ ...campaign, revision: campaign.revision + 1, nodes }));
  await render();
  await act(async () => flow.props!.onConnect({ source: "A", target: "C", sourceHandle: null, targetHandle: null }));
  await click(saveButton());
  expect(vi.mocked(saveCampaignNodes).mock.calls.map(([campaign]) => campaign.revision)).toEqual([5, 6, 7, 8]);
  expect(vi.mocked(saveCampaignNodes).mock.calls[3][1].find((value) => value.id === "C")?.dependencies).toEqual(["B", "A"]);
  expect(onSaved).toHaveBeenCalledWith(expect.objectContaining({ revision: 9 }));
  expect(text()).not.toContain("Unsaved");
  expect(text()).toContain("rev 9");

  vi.mocked(saveCampaignNodes).mockRejectedValue(conflict);
  await act(async () => flow.props!.onEdgesDelete([{ id: "A->C", source: "A", target: "C" }]));
  await click(saveButton());
  expect(vi.mocked(saveCampaignNodes)).toHaveBeenCalledTimes(8);
  expect(text()).toContain("The campaign keeps changing.");
});

it("saves nothing when the server changed the same field, and can keep the newest value there", async () => {
  const latest = { ...base, revision: 9, nodes: base.nodes.map((value) => value.id === "C" ? { ...value, title: "Server C" } : value) };
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: latest, live: {}, read: 1 });
  await render();
  await pick("C"); await typeTitle("Mine C");
  await click(saveButton());
  expect(saveCampaignNodes).not.toHaveBeenCalled();
  expect(text()).toContain("Changed elsewhere too, so nothing was saved: C title");
  await click(named("Keep the newest values there"));
  expect(text()).not.toContain("Unsaved");
  expect(text()).toContain("rev 9");
});

// Two free tasks for the server to point B at, or to close a loop with.
const extra: Campaign = { ...base, nodes: [...base.nodes, node("E", { group: "G" }), node("F", { group: "G" })] };
const withNodes = (revision: number, patch: Record<string, Partial<Campaign["nodes"][number]>>) => ({ ...extra, revision, nodes: extra.nodes.map((value) => ({ ...value, ...patch[value.id] })) });

it("drops undo steps when a newer base arrives, so undo cannot put back what the server replaced", async () => {
  await render(extra);
  await pick("B"); await choose("Add a dependency", "E"); await click(named("Remove dependency E E"));
  await render(withNodes(5, { B: { dependencies: ["A", "F"] } }));
  expect((named("Undo") as HTMLButtonElement).disabled).toBe(true);
  expect(named("Remove dependency F F")).toBeTruthy();
});

it("closes a conflict notice on undo, so the next save judges the restored draft again", async () => {
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: withNodes(9, { B: { dependencies: ["A", "F"] }, C: { title: "Server C" } }), live: {}, read: 1 });
  await render(extra);
  await pick("B"); await choose("Add a dependency", "E");
  await pick("C"); await typeTitle("Mine C");
  await pick("B"); await click(named("Remove dependency E E"));
  await click(saveButton());
  expect(text()).toContain("nothing was saved: C title");
  await click(named("Undo"));
  expect(named("Keep the newest values there")).toBeUndefined();
  await click(saveButton());
  expect(text()).toContain("nothing was saved: B dependencies · C title");
});

it("holds every edit while a save is out and sends what the screen shows, keeping a link it did not touch", async () => {
  const linked = { ...base, nodes: base.nodes.map((value) => value.id === "C" ? { ...value, issue_url: "owner/repo/issues/9" } : value) };
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: linked, live: {}, read: 1 });
  let land!: (saved: Campaign) => void; vi.mocked(saveCampaignNodes).mockReturnValue(new Promise((resolve) => { land = resolve; }));
  await render(linked);
  await pick("C"); await typeTitle("Mine C");
  await click(saveButton());
  expect((named("Add a dependency") as HTMLSelectElement).disabled).toBe(true);
  expect((named("Remove dependency B B") as HTMLButtonElement).disabled).toBe(true);
  await act(async () => flow.props!.onConnect({ source: "A", target: "C", sourceHandle: null, targetHandle: null }));
  const sent = vi.mocked(saveCampaignNodes).mock.calls[0][1];
  await act(async () => land({ ...linked, revision: 5, nodes: sent }));
  expect(sent.find((value) => value.id === "C")).toMatchObject({ title: "Mine C", dependencies: ["B"], issue_url: "owner/repo/issues/9" });
  expect(text()).not.toContain("Unsaved");
  expect(flow.props!.edges.map((edge) => edge.id)).not.toContain("A->C");
});

it("brings a cycle that only the merge makes onto the canvas, and saves once one side of it is removed", async () => {
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: withNodes(9, { E: { dependencies: ["F"] } }), live: {}, read: 1 });
  vi.mocked(saveCampaignNodes).mockImplementation(async (campaign, nodes) => ({ ...campaign, revision: campaign.revision + 1, nodes }));
  await render(extra);
  await pick("F"); await choose("Add a dependency", "E");
  await click(saveButton());
  expect(saveCampaignNodes).not.toHaveBeenCalled();
  expect(flow.props!.edges.filter((edge) => edge.style?.strokeDasharray).map((edge) => edge.id).sort()).toEqual(["E->F", "F->E"]);
  expect([/Cycle: (E → F → E|F → E → F)/.test(text()), text().includes("1 errors"), saveButton().disabled]).toEqual([true, true, true]);
  await act(async () => flow.props!.onEdgesDelete([{ id: "F->E", source: "F", target: "E" }]));
  await click(saveButton());
  expect(vi.mocked(saveCampaignNodes).mock.calls[0][1].filter((value) => "EF".includes(value.id)).map((value) => value.dependencies)).toEqual([[], ["E"]]);
});

it("asks before leaving with changes, warns on reload, and keeps an undecided draft for the next visit", async () => {
  await render();
  expect(leave.current).toBeNull();
  await act(async () => flow.props!.onConnect({ source: "A", target: "C", sourceHandle: null, targetHandle: null }));
  const reload = new Event("beforeunload", { cancelable: true });
  window.dispatchEvent(reload);
  expect(reload.defaultPrevented).toBe(true);
  await click(named("Campaign"));
  expect(text()).toContain("1 unsaved changes");
  await click(named("Keep editing"));
  const proceed = vi.fn();
  await act(async () => leave.current!(proceed));
  await click(named("Discard and leave"));
  expect(proceed).toHaveBeenCalled();
  expect(onExit).not.toHaveBeenCalled();

  await act(async () => root.unmount());
  expect(keptDraft(base.id)?.edits).toEqual({ C: { dependencies: ["B", "A"] } });
  const asks = () => { const event = new Event("beforeunload", { cancelable: true }); window.dispatchEvent(event); return event.defaultPrevented; };
  expect(asks()).toBe(true);
  root = createRoot(container);
  await render();
  expect(text()).toContain("Your unsaved edits are back.");
  expect(text()).toContain("Unsaved · 1 dependencies");
  await click(named("Discard"));
  expect(asks()).toBe(false);
});

it("below 1280px opens the inspector over the canvas only for a choice, and Escape or the close button returns focus to it", async () => {
  await render();
  expect(container.querySelector("aside")).toBeNull();
  for (const close of [() => click(named("Close the inspector")), () => act(async () => {
    container.querySelector("aside")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  })]) {
    await pick("C");
    expect(container.querySelector("aside")?.getAttribute("aria-label")).toBe("Selected task");
    await close();
    expect(container.querySelector("aside")).toBeNull();
    expect(document.activeElement).toBe(container.querySelector('[data-node="C"]'));
  }
});

it("draws at most 120 tasks of a scope and says how many were left out", async () => {
  await render({ ...base, nodes: Array.from({ length: 125 }, (_, index) => node(`n${index}`, { group: "big" })) }, { kind: "group", key: "big" });
  expect(flow.props!.nodes).toHaveLength(120);
  expect(text()).toContain("Drawing 120 of 125 tasks.");
});
