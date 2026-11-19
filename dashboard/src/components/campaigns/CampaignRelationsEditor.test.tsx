// @vitest-environment happy-dom
import { act, useEffect, type ComponentType, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Connection, Edge, EdgeChange, Node, NodeChange, NodeProps } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaignDetail, saveCampaignNodes, type Campaign } from "../../api/campaigns";
import { ApiRequestError } from "../../api/httpClient";
import CampaignRelationsEditor from "./CampaignRelationsEditor";
import { campaignViewKey, editorViewScope } from "./campaignViewStorage";
import { installMemoryStorage, makeCampaignNode as node } from "./campaignTestFixtures";
import type { TieNodeData } from "./RelationsTaskNode";
import { dropKeptDraft, keptDraft, type EditScope } from "./relationsEditorModel";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()), getCampaignDetail: vi.fn(), saveCampaignNodes: vi.fn(),
}));
type FlowProps = {
  nodes: Node[]; edges: Edge[]; nodeTypes: Record<string, ComponentType<NodeProps>>; onInit: (instance: unknown) => void; onNodesChange: (changes: NodeChange[]) => void;
  onEdgesChange: (changes: EdgeChange[]) => void; onEdgeDoubleClick: (event: { clientX: number; clientY: number }, edge: Edge) => void;
  onNodeDragStop: (event: unknown, node: Node) => void; onConnect: (connection: Connection) => void; onEdgesDelete: (edges: Edge[]) => void;
};
const flow: { props: FlowProps | null } = { props: null };
vi.mock("@xyflow/react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@xyflow/react")>()),
  // Tasks as buttons that select themselves; ties through their own node type, so their menu can be pressed.
  ReactFlow: (props: FlowProps) => {
    flow.props = props;
    useEffect(() => props.onInit({ screenToFlowPosition: (point: unknown) => point, setViewport: vi.fn(), setCenter: vi.fn(), fitView: vi.fn(), getViewport: () => ({ x: 0, y: 0, zoom: 1 }) }), []); // eslint-disable-line react-hooks/exhaustive-deps
    const Tie = props.nodeTypes.relationsTie;
    return <div>{props.nodes.map((value) => value.type === "relationsTie" ? <Tie key={value.id} {...{ id: value.id, data: value.data, selected: value.selected } as NodeProps} />
      : <button key={value.id} className="react-flow__node" data-node={value.id} data-id={value.id} onClick={() => props.onNodesChange([{ type: "select", id: value.id, selected: true }])}>{value.ariaLabel}</button>)}</div>;
  },
  NodeToolbar: ({ children, isVisible }: { children: ReactNode; isVisible: boolean }) => isVisible ? <div>{children}</div> : null,
  Background: () => null, Controls: () => null, Handle: () => null,
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
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true }); installMemoryStorage();
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove(); dropKeptDraft(base.id); vi.clearAllMocks();
});
const render = (campaign = base, scope: EditScope = G) => act(async () => root.render(<CampaignRelationsEditor campaign={campaign} scope={scope} onScope={vi.fn()} onExit={onExit} onSaved={onSaved} leave={leave} tr={(_: string, en: string) => en} />));
const text = () => document.body.textContent ?? "";
const named = (name: string) => [...document.querySelectorAll<HTMLElement>("button, select, input")].find((element) => element.getAttribute("aria-label") === name || element.textContent?.trim() === name)!;
const click = (element: HTMLElement) => act(async () => element.click());
const pick = (id: string) => click(container.querySelector<HTMLElement>(`[data-node="${id}"]`)!);
const choose = (name: string, value: string) => act(async () => {
  const select = named(name) as HTMLSelectElement;
  select.value = value; select.dispatchEvent(new Event("change", { bubbles: true }));
});
const saveButton = () => [...container.querySelectorAll("button")].find((element) => element.textContent === "Save")!;
const asks = () => { const event = new Event("beforeunload", { cancelable: true }); window.dispatchEvent(event); return event.defaultPrevented; };
const typeTitle = (value: string) => act(async () => { const input = container.querySelector<HTMLInputElement>("aside input")!;
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!.call(input, value); input.dispatchEvent(new Event("input", { bubbles: true })); });
const edge = (id: string) => flow.props!.edges.find((value) => value.id === id)!;
const ties = () => flow.props!.nodes.filter((value) => value.type === "relationsTie");
/** Double-clicks a line; the fake canvas maps the pointer 1:1, so (129, 75) snaps the tie to (100, 60). */
const tieAt = (id: string) => act(async () => flow.props!.onEdgeDoubleClick({ clientX: 129, clientY: 75 }, edge(id)));
const press = (key: string) => act(async () => { container.querySelector("section")!.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true })); });
const menu = () => [...document.querySelectorAll<HTMLElement>('[aria-label="Cable tie menu"] button')];
const unsaved = () =>/Unsaved · ([^U]*)/.exec(text())?.[1] ?? null;
const tieKey = campaignViewKey(editorViewScope(base.id, "group:G"));
const stored = (name = tieKey) => JSON.parse(localStorage.getItem(name)!) as { revision?: number; ties: { strandOrder: string[][] }[] };

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
  await act(async () => flow.props!.onEdgesDelete([edge("B->D")]));
  expect(text()).not.toContain("Unsaved");
  expect(flow.props!.edges.find((edge) => edge.id === "B->D")?.deletable).toBe(false);
  expect(flow.props!.edges.find((edge) => edge.id === "A->B")?.deletable).toBe(true);
  await act(async () => flow.props!.onEdgesDelete([edge("A->B")]));
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
  await act(async () => flow.props!.onEdgesDelete([edge("A->C")]));
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
  await tieAt("B->C"); await pick("B"); await choose("Add a dependency", "E"); await click(named("Remove dependency E E"));
  await render(withNodes(5, { B: { dependencies: ["A", "F"] } }));
  // The tie stays drafted; only its undo step goes with the old base.
  expect([(named("Undo") as HTMLButtonElement).disabled, ties().length]).toEqual([true, 1]);
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
  await act(async () => flow.props!.onEdgesDelete([edge("F->E")]));
  await click(saveButton());
  expect(vi.mocked(saveCampaignNodes).mock.calls[0][1].filter((value) => "EF".includes(value.id)).map((value) => value.dependencies)).toEqual([[], ["E"]]);
});

it("asks before leaving with changes, warns on reload, and keeps an undecided draft for the next visit", async () => {
  await render();
  expect(leave.current).toBeNull();
  await act(async () => flow.props!.onConnect({ source: "A", target: "C", sourceHandle: null, targetHandle: null }));
  await tieAt("B->C");
  const reload = new Event("beforeunload", { cancelable: true });
  window.dispatchEvent(reload);
  expect(reload.defaultPrevented).toBe(true);
  await click(named("Campaign"));
  expect(text()).toContain("2 unsaved changes");
  await click(named("Keep editing"));
  const proceed = vi.fn();
  await act(async () => leave.current!(proceed));
  await click(named("Discard and leave"));
  expect(proceed).toHaveBeenCalled();
  expect(onExit).not.toHaveBeenCalled();

  await act(async () => root.unmount());
  expect([keptDraft(base.id)?.edits, Object.keys(keptDraft(base.id)!.ties)]).toEqual([{ C: { dependencies: ["B", "A"] } }, ["group:G"]]);
  expect(asks()).toBe(true);
  root = createRoot(container);
  await render();
  expect(text()).toContain("Your unsaved edits are back.");
  expect([unsaved(), ties().length]).toEqual(["1 dependencies · 1 cable ties", 1]);
  expect(asks()).toBe(true);
  await click(named("Discard"));
  expect(asks()).toBe(false);
});

it("keeps a kept draft's reload warning while another campaign's editor opens, saves and closes", async () => {
  await render();
  await act(async () => flow.props!.onConnect({ source: "A", target: "C", sourceHandle: null, targetHandle: null }));
  await act(async () => root.unmount());
  const other = { ...base, id: "other" };
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: other, live: {}, read: 1 });
  vi.mocked(saveCampaignNodes).mockImplementation(async (campaign, nodes) => ({ ...campaign, revision: campaign.revision + 1, nodes }));
  root = createRoot(container);
  await render(other);
  await act(async () => flow.props!.onConnect({ source: "A", target: "C", sourceHandle: null, targetHandle: null }));
  await click(saveButton());
  expect([vi.mocked(saveCampaignNodes).mock.calls.length, text().includes("Unsaved"), asks()]).toEqual([1, false, true]);
  await act(async () => root.unmount());
  root = createRoot(container);
  expect(asks()).toBe(true);
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

it("ties lines with the tool and B or a double-click, moves a tie onto the grid, unties, and undoes ties and fields in one order, never touching a dependency", async () => {
  await render();
  await click(named("Cable ties"));
  for (const id of ["B->C", "B->D"]) await act(async () => flow.props!.onEdgesChange([{ type: "select", id, selected: true }]));
  expect(text()).toContain("2 lines chosen.");
  await press("b");
  expect([ties().map((tie) => (tie.data as TieNodeData).count), edge("B->C").target, edge("B->C#1").target, unsaved()]).toEqual([[2], ties()[0].id, "C", "1 cable ties"]);
  await pick("C"); await typeTitle("Mine C");
  await tieAt("A->B");
  expect(ties().find((tie) => (tie.data as TieNodeData).count === 1)?.position).toEqual({ x: 100, y: 60 });
  const bundled = ties().find((tie) => (tie.data as TieNodeData).count === 2)!;
  await act(async () => flow.props!.onNodeDragStop({}, { ...bundled, position: { x: 1013, y: 1011 } }));
  expect(ties().find((tie) => tie.id === bundled.id)?.position).toEqual({ x: 1020, y: 1020 });
  // The moved tie is chosen: take B → C out of it, preview its lines with the menu and H, then untie it with Delete.
  await click(named("Take one line out"));
  await click(menu()[2]);
  expect([(ties().find((tie) => tie.id === bundled.id)?.data as TieNodeData).count, edge("B->C").target]).toEqual([1, "C"]);
  await click(menu().at(-1)!);
  expect(edge("preview:B->D")).toBeTruthy();
  await press("h");
  expect(edge("preview:B->D")).toBeUndefined();
  await press("Delete");
  expect([ties().length, unsaved()]).toEqual([1, "1 titles · 1 cable ties"]);
  expect(flow.props!.edges.filter((value) => !value.id.includes("#")).map((value) => value.id).sort()).toEqual(["A->B", "B->C", "B->D"]);
  const steps = [];
  for (let step = 0; step < 6; step++) {
    await click(named("Undo"));
    steps.push([ties().map((tie) => `${(tie.data as TieNodeData).count}${tie.position.x === 1020 ? " moved" : ""}`).sort().join(), unsaved()]);
  }
  expect(steps).toEqual([
    ["1,1 moved", "1 titles · 2 cable ties"], ["1,2 moved", "1 titles · 2 cable ties"], ["1,2", "1 titles · 2 cable ties"],
    ["2", "1 titles · 1 cable ties"], ["2", "1 cable ties"], ["", null],
  ]);
  expect(saveCampaignNodes).not.toHaveBeenCalled();
});

it("writes ties only once the ledger holds the definition: no ledger call for ties alone, nothing on a conflict, a failed save or while one is out", async () => {
  localStorage.setItem(tieKey, "{not json"); // a damaged value: drawn as no ties and replaced on save
  await render();
  expect(ties()).toEqual([]);
  await tieAt("B->C");
  await click(saveButton());
  expect([vi.mocked(getCampaignDetail).mock.calls.length, vi.mocked(saveCampaignNodes).mock.calls.length, unsaved()]).toEqual([0, 0, null]);
  expect(stored()).toMatchObject({ revision: 4, ties: [{ strandOrder: [["B", "C"]] }] });
  const written = localStorage.getItem(tieKey);
  await tieAt("A->B"); await pick("C"); await typeTitle("Mine C");
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: { ...base, revision: 9, nodes: base.nodes.map((value) => value.id === "C" ? { ...value, title: "Server C" } : value) }, live: {}, read: 1 });
  await click(saveButton());
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: base, live: {}, read: 1 });
  vi.mocked(saveCampaignNodes).mockRejectedValueOnce(new Error("Server down"));
  await click(saveButton());
  expect([localStorage.getItem(tieKey) === written, text().includes("Server down"), unsaved()]).toEqual([true, true, "1 titles · 1 cable ties"]);
  let land!: (saved: Campaign) => void; vi.mocked(saveCampaignNodes).mockReturnValueOnce(new Promise((resolve) => { land = resolve; }));
  await click(saveButton());
  await tieAt("B->D");
  expect([ties().length, localStorage.getItem(tieKey) === written]).toEqual([2, true]);
  await act(async () => land({ ...base, revision: 5, nodes: vi.mocked(saveCampaignNodes).mock.calls.at(-1)![1] }));
  expect([stored().revision, stored().ties.length, unsaved()]).toEqual([5, 2, null]);
});

it("hides a tie whose lines are gone, keeps another scope's draft counted, and drops on save the members the saved definition lacks", async () => {
  const otherScope = editorViewScope(base.id, "task:C:1"), other = campaignViewKey(otherScope);
  localStorage.setItem(other, JSON.stringify({ schemaVersion: 1, scope: otherScope, ties: [{ id: "t1", position: { x: 0, y: 0 }, strandOrder: [["B", "C"]] }], routes: [{ edge: ["B", "C"], via: ["t1"] }], showOriginal: false, revision: 4 }));
  vi.mocked(getCampaignDetail).mockResolvedValue({ campaign: base, live: {}, read: 1 });
  vi.mocked(saveCampaignNodes).mockImplementation(async (campaign, nodes) => ({ ...campaign, revision: campaign.revision + 1, nodes }));
  await render();
  await tieAt("B->C");
  await act(async () => flow.props!.onEdgesDelete([edge("B->C")]));
  expect([ties(), unsaved()]).toEqual([[], "1 dependencies · 1 cable ties"]);
  await click(named("Undo"));
  expect([ties().length, unsaved()]).toEqual([1, "1 cable ties"]);
  await act(async () => flow.props!.onEdgesDelete([edge("B->C")]));
  await render(base, { kind: "task", id: "C", depth: 1 });
  expect([ties(), unsaved()]).toEqual([[], "1 dependencies · 1 cable ties"]);
  await click(saveButton());
  expect([stored().ties, stored(other).ties, unsaved()]).toEqual([[], [], null]);
});

it("draws at most 120 tasks of a scope and says how many were left out", async () => {
  await render({ ...base, nodes: Array.from({ length: 125 }, (_, index) => node(`n${index}`, { group: "big" })) }, { kind: "group", key: "big" });
  expect(flow.props!.nodes).toHaveLength(120);
  expect(text()).toContain("Drawing 120 of 125 tasks.");
});
