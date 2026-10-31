// @vitest-environment happy-dom
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Edge, Node } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, onTestFinished, vi } from "vitest";
import { setCampaignAutoQueue, updateCampaignNode, type Campaign } from "../../api/campaigns";
import { createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand } from "../../api/serverCampaigns";
import { campaignRawEdges, type RawEdgeKey } from "./campaignGraphRouting";
import { campaignViewKey } from "./campaignViewStorage";
import CampaignExplorer from "./CampaignExplorer";
import { installMemoryStorage, makeCampaignNode } from "./campaignTestFixtures";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()), updateCampaignNode: vi.fn(), setCampaignAutoQueue: vi.fn(),
}));
vi.mock("../../api/serverCampaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/serverCampaigns")>()),
  createServerCampaign: vi.fn(), editServerCampaignDefinition: vi.fn(), sendServerCampaignCommand: vi.fn(),
}));

type FlowProps = {
  nodes: Node[]; edges: Edge[]; children: ReactNode;
  onInit?: (instance: unknown) => void;
  onNodeDragStart?: (event: unknown, node: Node) => void;
  onNodeDrag?: (event: unknown, node: Node) => void;
  onNodeDragStop?: (event: unknown, node: Node) => void;
};
const flow: { props: FlowProps | null } = { props: null };
vi.mock("@xyflow/react", () => ({
  ReactFlow: (props: FlowProps) => {
    flow.props = props;
    props.onInit?.({ screenToFlowPosition: (point: unknown) => point, getNode: () => undefined });
    return <div data-testid="flow">
      {props.nodes.map((node) => <button key={node.id} data-node={node.id} data-type={node.type ?? "group"} data-x={node.position.x} data-y={node.position.y} aria-label={node.ariaLabel} />)}
      {props.edges.map((edge) => <output key={edge.id} data-source={edge.source} data-target={edge.target} data-edges={JSON.stringify(edge.data?.edges)} />)}
      {props.children}
    </div>;
  },
  Background: () => null, Controls: () => null, Handle: () => null,
  MarkerType: { ArrowClosed: "closed" }, Position: { Right: "right", Left: "left" },
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

async function openGraph() {
  await act(async () => root.render(<CampaignExplorer campaign={campaign} live={{}} tr={(_, en) => en} onSaved={vi.fn()} drafts={{}} onDraftChange={vi.fn()} />));
  await click(button("Connections"));
}
const button = (name: string) => [...container.querySelectorAll("button")].find((element) => element.textContent?.trim() === name || element.getAttribute("aria-label") === name)!;
const checkbox = (label: string) => [...container.querySelectorAll("label")].find((element) => element.textContent?.startsWith(label))!.querySelector("input")!;
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
