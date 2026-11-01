// @vitest-environment happy-dom
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Edge, Node } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, onTestFinished, vi } from "vitest";
import { getCampaigns, setCampaignAutoQueue, updateCampaignNode, type Campaign } from "../../api/campaigns";
import { createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand } from "../../api/serverCampaigns";
import { campaignRawEdges, type RawEdgeKey } from "./campaignGraphRouting";
import { campaignViewKey } from "./campaignViewStorage";
import CampaignExplorer from "./CampaignExplorer";
import CampaignsPanel from "./CampaignsPanel";
import { installMemoryStorage, makeCampaignNode } from "./campaignTestFixtures";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()), updateCampaignNode: vi.fn(), setCampaignAutoQueue: vi.fn(), getCampaigns: vi.fn(),
}));
vi.mock("../../api/serverCampaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/serverCampaigns")>()),
  createServerCampaign: vi.fn(), editServerCampaignDefinition: vi.fn(), sendServerCampaignCommand: vi.fn(),
  getServerCampaignSummaries: vi.fn().mockResolvedValue([]),
}));

type FlowProps = {
  nodes: Node[]; edges: Edge[]; children: ReactNode;
  onInit?: (instance: unknown) => void;
  onNodeDragStart?: (event: unknown, node: Node) => void;
  onNodeDrag?: (event: unknown, node: Node) => void;
  onNodeDragStop?: (event: unknown, node: Node) => void;
};
const flow: { props: FlowProps | null } = { props: null };
vi.mock("@xyflow/react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@xyflow/react")>()),
  ReactFlow: (props: FlowProps) => {
    flow.props = props;
    props.onInit?.({ screenToFlowPosition: (point: unknown) => point, getNode: () => undefined });
    return <div data-testid="flow">
      {props.nodes.map((node) => <button key={node.id} data-node={node.id} data-type={node.type ?? "group"} data-x={node.position.x} data-y={node.position.y} aria-label={node.ariaLabel}>{node.type ? null : node.data.label as ReactNode}</button>)}
      {props.edges.map((edge) => <output key={edge.id} data-source={edge.source} data-target={edge.target} data-source-handle={edge.sourceHandle ?? ""} data-target-handle={edge.targetHandle ?? ""}
        data-stroke={String(edge.style?.stroke)} data-edges={JSON.stringify(edge.data?.edges)} />)}
      {props.children}
    </div>;
  },
  Background: () => null, Controls: () => null, Handle: () => null,
}));

const nodes = [
  makeCampaignNode("A", { group: "G1" }), makeCampaignNode("B", { group: "G1" }),
  makeCampaignNode("C", { group: "G2", dependencies: ["A"] }), makeCampaignNode("D", { group: "G2", dependencies: ["B"] }),
];
const campaign: Campaign = {
  id: "tie-campaign", title: "Ties", description: "", status: "active", round: 1, revision: 7, auto_queue: true,
  created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z", nodes,
};
const key = campaignViewKey({ mode: "legacy", campaignId: campaign.id, projection: "groups", taskScope: "all" });

let container: HTMLDivElement;
let root: Root;
let localStorage: Storage;
let fetchSpy: ReturnType<typeof vi.fn>;
let xhrSpy: ReturnType<typeof vi.spyOn>;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  localStorage = installMemoryStorage();
  fetchSpy = vi.fn(() => Promise.reject(new Error("no network in tie tests")));
  vi.stubGlobal("fetch", fetchSpy);
  xhrSpy = vi.spyOn(XMLHttpRequest.prototype, "open");
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.unstubAllGlobals(); vi.restoreAllMocks(); vi.clearAllMocks();
});

const render = (shown: Campaign) => act(async () => root.render(<CampaignExplorer campaign={shown} live={{}} tr={(_, en) => en} onSaved={vi.fn()} drafts={{}} onDraftChange={vi.fn()} />));
async function openGraph(shown = campaign) {
  await render(shown);
  await click(button("Connections"));
}
const button = (name: string, scope: ParentNode = container) => [...scope.querySelectorAll("button")].find((element) => element.textContent?.trim() === name || element.getAttribute("aria-label") === name)!;
const checkbox = (label: string, scope: ParentNode = container) => [...scope.querySelectorAll("label")].find((element) => element.textContent?.startsWith(label))!.querySelector("input")!;
const click = (element: HTMLElement) => act(async () => element.click());
const ties = () => [...container.querySelectorAll<HTMLElement>('[data-type="campaignTie"]')].map((element) => ({ x: Number(element.dataset.x), y: Number(element.dataset.y) }));
/** Every original edge behind the drawn lines, as dependency→dependent pairs. */
const drawnEdges = () => new Set([...container.querySelectorAll("output")].flatMap((element) => (JSON.parse(element.dataset.edges!) as RawEdgeKey[]).map(([from, to]) => `${from}->${to}`)));
async function tieBoth() {
  await click(button("Tie lines"));
  await click(checkbox("A → C")); await click(checkbox("B → D"));
  await click(button("Tie 2 lines"));
}

it("ties, moves, removes and undoes A→C and B→D without new dependencies, definition changes or server writes", async () => {
  const setItem = vi.spyOn(localStorage, "setItem");
  const before = structuredClone(campaign);
  await openGraph();
  const expectUnchanged = () => {
    expect(drawnEdges()).toEqual(new Set(["A->C", "B->D"]));
    expect(campaignRawEdges(campaign.nodes)).toEqual([["A", "C"], ["B", "D"]]);
    expect(campaign).toEqual(before);
  };
  expectUnchanged();
  await tieBoth();
  expect(ties()).toHaveLength(1);
  expect(container.querySelectorAll("output")).toHaveLength(2);
  expectUnchanged();
  await click(button("Move tie right"));
  await click(button("Move tie down"));
  const moved = ties()[0];
  expectUnchanged();
  await click(button("Remove tie"));
  expect(ties()).toEqual([]);
  expectUnchanged();
  await click(button("Undo"));
  expect(ties()).toEqual([moved]);
  await click(button("Clear all ties"));
  await click(button("Undo"));
  expect(ties()).toEqual([moved]);
  expectUnchanged();

  expect(fetchSpy).not.toHaveBeenCalled();
  expect(xhrSpy).not.toHaveBeenCalled();
  for (const write of [updateCampaignNode, setCampaignAutoQueue, createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand]) {
    expect(vi.mocked(write)).not.toHaveBeenCalled();
  }
  expect(new Set(setItem.mock.calls.map(([name]) => name))).toEqual(new Set([key]));
});

it("keeps the drag preview and status refreshes out of undo, so undo returns the tie to where the drag began", async () => {
  await openGraph();
  await tieBoth();
  const start = ties()[0];
  const tieNode = flow.props!.nodes.find((node) => node.type === "campaignTie")!;
  const at = (x: number, y: number) => ({ ...tieNode, position: { x, y } });
  await act(async () => flow.props!.onNodeDragStart!({}, at(start.x, start.y)));
  await act(async () => flow.props!.onNodeDrag!({}, at(start.x + 50, start.y + 50)));
  await act(async () => flow.props!.onNodeDrag!({}, at(start.x + 90, start.y + 90)));
  expect(ties()).toEqual([{ x: start.x + 90, y: start.y + 90 }]);
  await act(async () => flow.props!.onNodeDragStop!({}, at(start.x + 100, start.y + 100)));
  expect(ties()).toEqual([{ x: start.x + 100, y: start.y + 100 }]);
  // A status refresh delivers new node objects; neither the layout nor its undo history resets.
  const refreshed = { ...campaign, nodes: campaign.nodes.map((node) => ({ ...node, status: "running" as const })) };
  await act(async () => root.render(<CampaignExplorer campaign={refreshed} live={{}} tr={(_, en) => en} onSaved={vi.fn()} drafts={{}} onDraftChange={vi.fn()} />));
  expect(ties()).toEqual([{ x: start.x + 100, y: start.y + 100 }]);
  await click(button("Undo"));
  expect(ties()).toEqual([start]);
  await click(button("Undo"));
  expect(ties()).toEqual([]);
});

it("keeps the shown view and undo when the browser refuses to save, without falling back to a definition write", async () => {
  await openGraph();
  const setItem = vi.spyOn(localStorage, "setItem").mockImplementation(() => { throw new DOMException("full", "QuotaExceededError"); });
  await tieBoth();
  expect(ties()).toHaveLength(1);
  expect(container.querySelector('[role="alert"]')?.textContent).toContain("Could not save the view");
  expect(button("Undo").disabled).toBe(false);
  expect(vi.mocked(updateCampaignNode)).not.toHaveBeenCalled();
  expect(fetchSpy).not.toHaveBeenCalled();
  setItem.mockRestore();
  await click(button("Retry save"));
  expect(container.querySelector('[role="alert"]')).toBeNull();
  expect(JSON.parse(localStorage.getItem(key)!).ties).toHaveLength(1);
});

it("does not let another tab's save replace a selection in progress or the undo history", async () => {
  await openGraph();
  await tieBoth();
  await click(button("Tie lines"));
  await click(checkbox("B → D"));
  const otherTab = { schemaVersion: 1, scope: { mode: "legacy", campaignId: campaign.id, projection: "groups", taskScope: "all" }, ties: [{ id: "tie-1", position: { x: -300, y: -300 }, strandOrder: [["A", "C"]] }], routes: [{ edge: ["A", "C"], via: ["tie-1"] }], showOriginal: false };
  await act(async () => {
    localStorage.setItem(key, JSON.stringify(otherTab));
    window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage }));
  });
  expect(checkbox("B → D").checked).toBe(true);
  expect(container.textContent).toContain("Another tab changed this view");
  expect(ties()).toHaveLength(1);
  expect(ties()[0]).not.toEqual({ x: -300, y: -300 });

  await click(button("Tie 1 lines"));
  // The new tie lands on the other tab's view; the view before it stays undoable.
  const stored = JSON.parse(localStorage.getItem(key)!);
  expect(stored.ties.map((tie: { position: unknown; strandOrder: unknown }) => tie.strandOrder)).toEqual([[["A", "C"]], [["B", "D"]]]);
  expect(stored.ties[0].position).toEqual({ x: -300, y: -300 });
  await click(button("Undo"));
  expect(ties()).toHaveLength(1);
  expect(JSON.parse(localStorage.getItem(key)!).ties[0].strandOrder).toEqual([["A", "C"], ["B", "D"]]);
});

it("restores ties after reopening and recovers damaged settings as original lines", async () => {
  await openGraph();
  await tieBoth();
  await click(button("Move tie left"));
  const placed = ties();
  await click(button("Show original lines"));
  expect(ties()).toEqual([]);
  await act(async () => root.unmount());
  root = createRoot(container);
  await openGraph();
  expect(ties()).toEqual([]);
  await click(button("Show original lines"));
  expect(ties()).toEqual(placed);

  await act(async () => root.unmount());
  localStorage.setItem(key, JSON.stringify({ ...JSON.parse(localStorage.getItem(key)!), schemaVersion: 2 }));
  root = createRoot(container);
  await openGraph();
  expect(ties()).toEqual([]);
  expect(container.textContent).toContain("original lines are shown");
  expect(drawnEdges()).toEqual(new Set(["A->C", "B->D"]));
  expect(campaign.revision).toBe(7);
});

it("selects lines by a touch stroke across the graph and finishes every step with buttons on a device without hover", async () => {
  const media = Object.getOwnPropertyDescriptor(window, "matchMedia");
  Object.defineProperty(window, "matchMedia", { configurable: true, value: (query: string) => ({ matches: /hover: none|pointer: coarse|max-width/.test(query), media: query, addEventListener() {}, removeEventListener() {} }) });
  onTestFinished(() => { if (media) Object.defineProperty(window, "matchMedia", media); });
  await openGraph();
  await click(button("Tie lines"));
  const surface = container.querySelector<HTMLElement>(".campaign-tie-stroke")!;
  // The G1→G2 line runs at y≈72 between the group boxes at x 244 and 310.
  await act(async () => {
    surface.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 1, pointerType: "touch", clientX: 280, clientY: 0 }));
    surface.dispatchEvent(new PointerEvent("pointermove", { bubbles: true, pointerId: 1, pointerType: "touch", clientX: 280, clientY: 150 }));
    surface.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 1, pointerType: "touch", clientX: 280, clientY: 150 }));
  });
  expect(checkbox("A → C").checked).toBe(true);
  expect(checkbox("B → D").checked).toBe(true);
  await click(button("Tie 2 lines"));
  expect(container.querySelector(".campaign-tie-stroke")).toBeNull();
  expect(button("Tie 1 · 2 strands").getAttribute("aria-pressed")).toBe("true");
  const tie = ties()[0];
  await click(button("Move tie up"));
  expect(ties()).toEqual([{ x: tie.x, y: tie.y - 40 }]);
  await click(button("A → C"));
  expect(container.textContent).toContain("Dependency A → dependent C");
  await click(button("Show original lines"));
  expect(ties()).toEqual([]);
  await click(button("Show original lines"));
  await click(button("Remove tie"));
  expect(ties()).toEqual([]);
  await click(button("Undo"));
  expect(ties()).toEqual([{ x: tie.x, y: tie.y - 40 }]);
});

type Segment = { source: string; target: string; sourceHandle: string; targetHandle: string; stroke: string; edges: string[] };
const segments = (): Segment[] => [...container.querySelectorAll<HTMLElement>("output")].map((element) => ({
  source: element.dataset.source!, target: element.dataset.target!, sourceHandle: element.dataset.sourceHandle!, targetHandle: element.dataset.targetHandle!,
  stroke: element.dataset.stroke!, edges: (JSON.parse(element.dataset.edges!) as RawEdgeKey[]).map(([from, to]) => `${from}->${to}`),
}));
/** Originals drawn in the highlight colour. */
const lit = () => new Set(segments().filter((segment) => segment.stroke === "var(--th-accent-info)").flatMap((segment) => segment.edges));
const groupNode = (key: string) => container.querySelector<HTMLElement>(`[data-node="campaign-group:${key}"]`)!;
/** Follows one original's drawn segments from its group through tie slots; every hop must end on a drawn node. */
function pathOf(edge: string): string[] {
  const own = segments().filter((segment) => segment.edges.includes(edge));
  const drawn = new Set([...container.querySelectorAll<HTMLElement>("[data-node]")].map((element) => element.dataset.node!));
  let segment = own.find((candidate) => candidate.source.startsWith("campaign-group:"));
  const hops = [segment?.source];
  for (let guard = 0; segment && guard < 10; guard++) {
    expect(drawn.has(segment.source) && drawn.has(segment.target), `${edge}: ${segment.source} → ${segment.target}`).toBe(true);
    hops.push(segment.targetHandle ? `${segment.target}[${segment.targetHandle}]` : segment.target);
    if (!segment.targetHandle) return hops as string[];
    const from = segment;
    segment = own.find((candidate) => candidate.source === from.target && candidate.sourceHandle === from.targetHandle.replace("in-", "out-"));
  }
  throw new Error(`${edge} does not reach a group: ${hops.join(" → ")}`);
}

const wiring: Campaign = {
  ...campaign, id: "tie-wiring",
  nodes: [makeCampaignNode("A", { group: "G1" }), makeCampaignNode("B", { group: "G1" }), makeCampaignNode("C", { group: "G2", dependencies: ["A"] }), makeCampaignNode("E", { group: "G3", dependencies: ["B"] })],
};

it("wires each tied original from its own group through its own tie slot to its own group, and lights only the chosen original", async () => {
  await openGraph(wiring);
  await click(button("Tie lines")); await click(checkbox("A → C")); await click(checkbox("B → E")); await click(button("Tie 2 lines"));
  const tie = flow.props!.nodes.find((node) => node.type === "campaignTie")!.id;
  expect(pathOf("A->C")).toEqual(["campaign-group:G1", `${tie}[in-0]`, "campaign-group:G2"]);
  expect(pathOf("B->E")).toEqual(["campaign-group:G1", `${tie}[in-1]`, "campaign-group:G3"]);
  expect(lit()).toEqual(new Set(["A->C", "B->E"]));
  await click(button("A → C"));
  expect(lit()).toEqual(new Set(["A->C"]));
});

it("separates a chosen original from its aggregate line and marks only its two end tasks", async () => {
  await openGraph();
  await tieBoth();
  await click(button("A → C"));
  expect(lit()).toEqual(new Set(["A->C"]));
  expect(segments().every((segment) => !(segment.edges.includes("A->C") && segment.edges.includes("B->D")))).toBe(true);
  expect(pathOf("A->C")[2]).toBe("campaign-group:G2");
  expect(groupNode("G1").textContent).toContain("Dependency · A");
  expect(groupNode("G2").textContent).toContain("Dependent · C");
  expect(groupNode("G1").textContent).not.toContain("Dependency · B");
  expect(groupNode("G2").textContent).not.toContain("Dependent · D");
});

it("lights only the lines being chosen for a new tie, not the members of the tie still selected", async () => {
  await openGraph(wiring);
  await click(button("Tie lines")); await click(checkbox("A → C")); await click(button("Tie 1 lines"));
  expect(lit()).toEqual(new Set(["A->C"]));
  await click(button("Tie lines")); await click(checkbox("B → E"));
  expect(lit()).toEqual(new Set(["B->E"]));
});

it("shows a stored tie whose members disagree with its routes as plain original lines", async () => {
  const scope = { mode: "legacy", campaignId: campaign.id, projection: "groups", taskScope: "all" };
  localStorage.setItem(key, JSON.stringify({ schemaVersion: 1, scope, ties: [{ id: "t1", position: { x: 100, y: 100 }, strandOrder: [] }], routes: [{ edge: ["A", "C"], via: ["t1"] }], showOriginal: false }));
  await openGraph();
  expect(ties()).toEqual([]);
  expect(pathOf("A->C")).toEqual(["campaign-group:G1", "campaign-group:G2"]);
  expect(container.textContent).toContain("original lines are shown");
  expect(fetchSpy).not.toHaveBeenCalled();
});

it("forgets a deleted dependency's membership in the saved view and undo, so adding it back leaves it untied", async () => {
  await openGraph();
  await tieBoth();
  await click(button("Move tie right"));
  const stored = () => localStorage.getItem(key)!;
  await render({ ...campaign, nodes: campaign.nodes.map((node) => node.id === "D" ? { ...node, dependencies: [] } : node) });
  expect(stored()).not.toContain('"D"');
  await click(button("Undo"));
  expect(stored()).not.toContain('"D"');
  expect(ties()).toHaveLength(1);
  await act(async () => root.unmount());
  root = createRoot(container);
  await openGraph();
  expect(pathOf("B->D")).toEqual(["campaign-group:G1", "campaign-group:G2"]);
  expect(pathOf("A->C")[1]).toMatch(/^campaign-tie:/);
  expect(stored()).not.toContain('"D"');
});

it("holds another tab's save while the first stroke across the graph is still being drawn", async () => {
  await openGraph();
  await click(button("Tie lines")); await click(checkbox("A → C")); await click(button("Tie 1 lines"));
  const [tie] = ties();
  await click(button("Tie lines"));
  const surface = container.querySelector<HTMLElement>(".campaign-tie-stroke")!;
  // Between the G1 box's right side (x 244) and the tie, the stroke crosses A→C's tied segment and B→D.
  const x = (244 + tie.x) / 2;
  const pointer = (type: string, y: number) => surface.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId: 3, pointerType: "mouse", clientX: x, clientY: y }));
  await act(async () => { pointer("pointerdown", 0); pointer("pointermove", 150); });
  const moved = JSON.parse(localStorage.getItem(key)!);
  moved.ties[0].position = { x: -300, y: -300 };
  await act(async () => {
    localStorage.setItem(key, JSON.stringify(moved));
    window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage }));
  });
  expect(ties()).toEqual([tie]);
  expect(container.textContent).toContain("Another tab changed this view");
  await act(async () => { pointer("pointerup", 150); });
  expect(checkbox("A → C").checked).toBe(true);
  expect(checkbox("B → D").checked).toBe(true);
  await click(button("Tie 2 lines"));
  const saved = JSON.parse(localStorage.getItem(key)!);
  expect(saved.ties.map((stored: { position: unknown }) => stored.position)[0]).toEqual({ x: -300, y: -300 });
  expect(saved.ties).toHaveLength(2);
});

it("keeps a tie drag's preview through a status poll and saves only on release", async () => {
  await openGraph();
  await tieBoth();
  const start = ties()[0];
  const setItem = vi.spyOn(localStorage, "setItem");
  const tieNode = flow.props!.nodes.find((node) => node.type === "campaignTie")!;
  const at = (x: number, y: number) => ({ ...tieNode, position: { x, y } });
  await act(async () => flow.props!.onNodeDragStart!({}, at(start.x, start.y)));
  await act(async () => flow.props!.onNodeDrag!({}, at(start.x + 50, start.y + 50)));
  await render({ ...campaign, nodes: campaign.nodes.map((node) => ({ ...node, status: "running" as const })) });
  expect(ties()).toEqual([{ x: start.x + 50, y: start.y + 50 }]);
  expect(setItem).not.toHaveBeenCalled();
  await act(async () => flow.props!.onNodeDragStop!({}, at(start.x + 100, start.y + 100)));
  expect(setItem).toHaveBeenCalledTimes(1);
  await click(button("Undo"));
  expect(ties()).toEqual([start]);
});

it("ends a stroke when tie mode closes mid-stroke, so later saves from another tab apply again", async () => {
  await openGraph();
  await tieBoth();
  await click(button("Tie lines"));
  await act(async () => { container.querySelector<HTMLElement>(".campaign-tie-stroke")!.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 4, clientX: 0, clientY: 0 })); });
  await click(button("Tie lines"));
  const moved = JSON.parse(localStorage.getItem(key)!);
  moved.ties[0].position = { x: -300, y: -300 };
  await act(async () => {
    localStorage.setItem(key, JSON.stringify(moved));
    window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage }));
  });
  expect(ties()).toEqual([{ x: -300, y: -300 }]);
  expect(container.textContent).not.toContain("Another tab changed this view");
});

it("forgets every tie when all tasks are deleted, in the saved view and undo, so the same tasks added back stay untied", async () => {
  await openGraph();
  await tieBoth();
  await click(button("Move tie right"));
  await render({ ...campaign, nodes: [] });
  expect(JSON.parse(localStorage.getItem(key)!).ties).toEqual([]);
  await render(campaign);
  await click(button("Undo"));
  expect(ties()).toEqual([]);
  expect(JSON.parse(localStorage.getItem(key)!).ties).toEqual([]);
  await act(async () => root.unmount());
  root = createRoot(container);
  await openGraph();
  expect(ties()).toEqual([]);
  expect(pathOf("A->C")).toEqual(["campaign-group:G1", "campaign-group:G2"]);
});

it("forgets ties of dependencies deleted while the graph is closed or every task is gone", async () => {
  const load = (shown: Campaign) => vi.mocked(getCampaigns).mockResolvedValue({ campaigns: [shown], live: {}, read: 1 });
  load(campaign);
  await act(async () => root.render(<CampaignsPanel language="en" />));
  await click(button("Connections"));
  await tieBoth();
  await click(button("Grouped list"));
  load({ ...campaign, nodes: campaign.nodes.map((node) => node.id === "D" ? { ...node, dependencies: [] } : node) });
  await click(button("Refresh"));
  expect(localStorage.getItem(key)).not.toContain('"D"');
  load({ ...campaign, nodes: [] });
  await click(button("Refresh"));
  expect(JSON.parse(localStorage.getItem(key)!).ties).toEqual([]);
  load(campaign);
  await click(button("Refresh"));
  await click(button("Connections"));
  expect(ties()).toEqual([]);
  expect(pathOf("B->D")).toEqual(["campaign-group:G1", "campaign-group:G2"]);
});

it("leaves another tab's tie on a dependency this tab does not know yet, in storage and on that tab's screen", async () => {
  // Tab B still shows the definition before B→D was added; tab A has the newer one and ties B→D.
  const older = { ...campaign, nodes: campaign.nodes.map((node) => node.id === "D" ? { ...node, dependencies: [] } : node) };
  const otherTab = document.createElement("div");
  document.body.appendChild(otherTab);
  const otherRoot = createRoot(otherTab);
  onTestFinished(async () => { await act(async () => otherRoot.unmount()); otherTab.remove(); });
  await act(async () => otherRoot.render(<CampaignExplorer campaign={older} live={{}} tr={(_, en) => en} onSaved={vi.fn()} drafts={{}} onDraftChange={vi.fn()} />));
  await act(async () => [...otherTab.querySelectorAll("button")].find((element) => element.textContent === "Connections")!.click());
  await openGraph();
  await click(button("Tie lines"));
  await click(checkbox("B → D"));
  await click(button("Tie 1 lines"));
  const deliver = () => act(async () => { window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage })); });
  await deliver();
  expect(JSON.parse(localStorage.getItem(key)!).ties.map((tie: { strandOrder: unknown }) => tie.strandOrder)).toEqual([[["B", "D"]]]);
  await deliver();
  expect(ties()).toHaveLength(1);
  expect(pathOf("B->D")[1]).toMatch(/^campaign-tie:/);
});

it("keeps lines in place while a stroke previews them, so release selects what the preview showed", async () => {
  await openGraph();
  await tieBoth();
  const [tie] = ties();
  await click(button("Tie lines"));
  await click(checkbox("A → C"));
  const shape = () => segments().map(({ source, target, sourceHandle, targetHandle, edges }) => [source, target, sourceHandle, targetHandle, edges.join()].join("|"));
  const before = shape();
  const surface = container.querySelector<HTMLElement>(".campaign-tie-stroke")!;
  // Just right of G1 the B→D strand runs to the tie's second slot, below A→C's first slot.
  const x = tie.x - 5;
  const pointer = (type: string, y: number) => surface.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId: 5, clientX: x, clientY: y }));
  await act(async () => { pointer("pointerdown", 76); pointer("pointermove", 80); });
  expect(lit()).toEqual(new Set(["A->C", "B->D"]));
  expect(shape()).toEqual(before);
  await act(async () => { pointer("pointerup", 80); });
  expect(checkbox("A → C").checked).toBe(true);
  expect(checkbox("B → D").checked).toBe(true);
});

it("judges a stroke against the lines drawn when it began, even if the draft changes mid-stroke", async () => {
  await openGraph();
  await tieBoth();
  const [tie] = ties();
  await click(button("Tie lines"));
  await click(checkbox("A → C"));
  const surface = container.querySelector<HTMLElement>(".campaign-tie-stroke")!;
  const pointer = (type: string, y: number) => surface.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId: 6, clientX: tie.x - 5, clientY: y }));
  await act(async () => { pointer("pointerdown", 76); pointer("pointermove", 80); });
  // A second finger unticks A→C, which would merge the strands and move B→D away from the stroke.
  await click(checkbox("A → C"));
  await act(async () => { pointer("pointerup", 80); });
  expect(checkbox("B → D").checked).toBe(true);
  expect(checkbox("A → C").checked).toBe(false);
});

it("keeps saved ties until the campaign list loads, then clears those of a campaign that is gone", async () => {
  const load = (...shown: Campaign[]) => vi.mocked(getCampaigns).mockResolvedValue({ campaigns: shown, live: {}, read: 1 });
  await openGraph();
  await tieBoth();
  await act(async () => root.unmount());
  root = createRoot(container);
  load(campaign);
  await act(async () => root.render(<CampaignsPanel language="en" />));
  await click(button("Connections"));
  expect(ties()).toHaveLength(1);
  load({ ...campaign, id: "other-campaign" });
  await click(button("Refresh"));
  expect(JSON.parse(localStorage.getItem(key)!).ties).toEqual([]);
  load(campaign, { ...campaign, id: "other-campaign" });
  await click(button("Refresh"));
  const selector = container.querySelector<HTMLSelectElement>('select[aria-label="Select campaign"]')!;
  await act(async () => { selector.value = campaign.id; selector.dispatchEvent(new Event("change", { bubbles: true })); });
  expect(selector.value).toBe(campaign.id);
  await click(button("Connections"));
  expect(ties()).toEqual([]);
  expect(pathOf("A->C")).toEqual(["campaign-group:G1", "campaign-group:G2"]);
});

/** A definition of the tie campaign at `revision` with the given dependencies. */
const define = (revision: number, dependencies: Record<string, string[]>): Campaign =>
  ({ ...campaign, revision, nodes: campaign.nodes.map((node) => ({ ...node, dependencies: dependencies[node.id] ?? [] })) });
/** Another tab of this browser: the same storage, its own screen. */
function otherTab() {
  const tab = document.createElement("div");
  document.body.appendChild(tab);
  const tabRoot = createRoot(tab);
  onTestFinished(async () => { await act(async () => tabRoot.unmount()); tab.remove(); });
  return { tab, show: (node: ReactNode) => act(async () => tabRoot.render(node)) };
}
const explorer = (shown: Campaign) => <CampaignExplorer campaign={shown} live={{}} tr={(_, en) => en} onSaved={vi.fn()} drafts={{}} onDraftChange={vi.fn()} />;
const deliver = () => act(async () => { window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage })); });
const stored = () => JSON.parse(localStorage.getItem(key)!) as { revision?: number; ties: { strandOrder: RawEdgeKey[] }[] };
const members = () => stored().ties.map((tie) => tie.strandOrder);

it("keeps the newest tab's tie when an older tab, after receiving it, moves to another definition that still lacks it", async () => {
  // The older tab follows revisions 5 and 6, neither with B→D; this tab has revision 7 and ties B→D.
  const older = otherTab();
  await older.show(explorer(define(5, { D: ["A"] })));
  await click(button("Connections", older.tab));
  await openGraph();
  await click(button("Tie lines")); await click(checkbox("B → D")); await click(button("Tie 1 lines"));
  await deliver();
  await older.show(explorer(define(6, { C: ["A"] })));
  await deliver();
  expect(members()).toEqual([[["B", "D"]]]);
  expect(stored().revision).toBe(7);
  expect(pathOf("B->D")[1]).toMatch(/^campaign-tie:/);
});

it("cleans members the newest definition deleted and records that definition's revision", async () => {
  await openGraph();
  await tieBoth();
  expect(stored().revision).toBe(7);
  await render(define(8, { C: ["A"] }));
  expect(members()).toEqual([[["A", "C"]]]);
  expect(stored().revision).toBe(8);
  await click(button("Undo"));
  expect(members()).toEqual([]);
  expect(stored().revision).toBe(8);
});

it("lets a campaign poll answered before another tab's newer tie keep that tie, and a newer poll clean it", async () => {
  const answer = (shown: Campaign) => ({ campaigns: [shown], live: {}, read: 1 });
  let late!: (value: ReturnType<typeof answer>) => void;
  vi.mocked(getCampaigns).mockResolvedValueOnce(answer(campaign)).mockReturnValueOnce(new Promise((resolve) => { late = resolve; }));
  const panel = otherTab();
  await panel.show(<CampaignsPanel language="en" />);
  await click(button("Refresh", panel.tab));
  // Revision 8 adds A→D; this tab ties it while the panel's poll still carries revision 7.
  await openGraph(define(8, { C: ["A"], D: ["B", "A"] }));
  await click(button("Tie lines")); await click(checkbox("A → D")); await click(button("Tie 1 lines"));
  await act(async () => late(answer(campaign)));
  await deliver();
  expect(members()).toEqual([[["A", "D"]]]);
  expect(pathOf("A->D")[1]).toMatch(/^campaign-tie:/);
  vi.mocked(getCampaigns).mockResolvedValue(answer(define(9, { C: ["A"], D: ["B"] })));
  await click(button("Refresh", panel.tab));
  expect(members()).toEqual([]);
  expect(stored().revision).toBe(9);
});

it("keeps the newest tab's tie through an older tab's own tie and its undo", async () => {
  const older = otherTab();
  await older.show(explorer(define(6, { C: ["A"] })));
  await click(button("Connections", older.tab));
  await openGraph();
  await click(button("Tie lines")); await click(checkbox("B → D")); await click(button("Tie 1 lines"));
  await deliver();
  await click(button("Tie lines", older.tab)); await click(checkbox("A → C", older.tab)); await click(button("Tie 1 lines", older.tab));
  expect(members()).toEqual([[["B", "D"]], [["A", "C"]]]);
  await deliver();
  expect(pathOf("B->D")[1]).toMatch(/^campaign-tie:/);
  expect(pathOf("A->C")[1]).toMatch(/^campaign-tie:/);
  await click(button("Undo", older.tab));
  expect(members()).toEqual([[["B", "D"]]]);
  expect(stored().revision).toBe(7);
  await deliver();
  expect(pathOf("B->D")[1]).toMatch(/^campaign-tie:/);
});

it("cleans a view saved before revisions were recorded as if the oldest definition had judged it", async () => {
  const scope = { mode: "legacy", campaignId: campaign.id, projection: "groups", taskScope: "all" };
  localStorage.setItem(key, JSON.stringify({ schemaVersion: 1, scope, ties: [{ id: "t1", position: { x: 100, y: 100 }, strandOrder: [["A", "C"], ["B", "D"]] }], routes: [{ edge: ["A", "C"], via: ["t1"] }, { edge: ["B", "D"], via: ["t1"] }], showOriginal: false }));
  await openGraph(define(1, { C: ["A"] }));
  expect(members()).toEqual([[["A", "C"]]]);
  expect(stored().revision).toBe(1);
});

it("lets a campaign created again under a gone campaign's id clean its own ties from a lower revision", async () => {
  const load = (...shown: Campaign[]) => vi.mocked(getCampaigns).mockResolvedValue({ campaigns: shown, live: {}, read: 1 });
  await openGraph();
  await tieBoth();
  await act(async () => root.unmount());
  root = createRoot(container);
  load({ ...campaign, id: "other-campaign" });
  await act(async () => root.render(<CampaignsPanel language="en" />));
  load(define(1, { C: ["A"], D: ["B"] }));
  await click(button("Refresh"));
  await click(button("Connections"));
  await tieBoth();
  load(define(2, { C: ["A"] }));
  await click(button("Refresh"));
  expect(members()).toEqual([[["A", "C"]]]);
  expect(stored().revision).toBe(2);
});
