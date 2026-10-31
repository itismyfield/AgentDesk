// @vitest-environment happy-dom
import { act, type MouseEvent, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Edge, Node } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { campaignNodeSchema, setCampaignAutoQueue, updateCampaignNode } from "../../api/campaigns";
import { createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand } from "../../api/serverCampaigns";
import { campaignRawEdges, type RawEdgeKey } from "./campaignGraphRouting";
import CampaignGroupOverview, { buildCampaignGroupOverview } from "./CampaignGroupOverview";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()), updateCampaignNode: vi.fn(), setCampaignAutoQueue: vi.fn(),
}));
vi.mock("../../api/serverCampaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/serverCampaigns")>()),
  createServerCampaign: vi.fn(), editServerCampaignDefinition: vi.fn(), sendServerCampaignCommand: vi.fn(),
}));

vi.mock("@xyflow/react", () => ({
  ReactFlow: ({ nodes, edges, onNodeClick, minZoom, children }: {
    nodes: Node[]; edges: Edge[]; minZoom: number; children: ReactNode;
    onNodeClick: (event: MouseEvent, node: Node) => void;
  }) => <div data-testid="group-graph" data-min-zoom={minZoom}>
    {nodes.map((node) => <button key={node.id} aria-label={node.ariaLabel} onClick={(event) => onNodeClick(event, node)}>{node.data.label as ReactNode}</button>)}
    {edges.map((edge) => <output key={edge.id} data-source={edge.source} data-target={edge.target}>{edge.label as ReactNode}</output>)}
    {children}
  </div>,
  Background: () => null,
  Controls: () => null,
  MarkerType: { ArrowClosed: "closed" }, Position: { Right: "right", Left: "left" },
}));

function node(id: string, group: string | null, dependencies: string[] = [], status = "pending") {
  return campaignNodeSchema.parse({ id, group, dependencies, status, title: id, stage: "review", round: 1, updated_at: "2026-09-20T00:00:00Z" });
}

it("aggregates cross-group dependency counts while retaining internal summaries", () => {
  const graph = buildCampaignGroupOverview([
    node("a1", "A", [], "completed"), node("a2", "A", ["a1"], "running"),
    node("b1", "B", ["a1", "a2"], "blocked"), node("b2", "B", ["a2"]),
  ]);
  expect(graph.connections).toEqual([{
    id: expect.any(String), source: graph.groups[0].id, target: graph.groups[1].id, via: [], count: 3,
    edges: [["a1", "b1"], ["a2", "b1"], ["a2", "b2"]],
  }]);
  expect(graph.internalDependencies).toBe(1);
  expect(graph.groups[0].progress).toMatchObject({ total: 2, percent: 50, counts: { completed: 1, running: 1 } });
  expect(graph.groups[1].progress.counts.blocked).toBe(1);
});

it("keeps an unclassified group and displays both directions of a cyclic group projection", () => {
  // Task DAG: a1 -> b1 -> a2 -> unclassified. Collapsing A yields A <-> B.
  const nodes = [node("a1", "A"), node("b1", "B", ["a1"]), node("a2", "A", ["b1"]), node("unclassified", null, ["a2"])];
  const graph = buildCampaignGroupOverview(nodes);
  expect(graph.groups.map((group) => group.key)).toEqual(["A", "B", ""]);
  const [a, b, unclassified] = graph.groups;
  expect(graph.connections.map(({ source, target, edges }) => ({ source, target, edges }))).toEqual(expect.arrayContaining([
    { source: a.id, target: b.id, edges: [["a1", "b1"]] },
    { source: b.id, target: a.id, edges: [["b1", "a2"]] },
    { source: a.id, target: unclassified.id, edges: [["a2", "unclassified"]] },
  ]));
  expect(new Set(graph.groups.map((group) => `${group.position.x}:${group.position.y}`)).size).toBe(3);
  expect(buildCampaignGroupOverview([...nodes].reverse()).groups.map((group) => group.position)).toEqual(graph.groups.map((group) => group.position));
});

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });

it("renders counts and aggregated edges and selects null groups through the empty-string callback", async () => {
  const select = vi.fn();
  const nodes = [node("a1", "A", [], "completed"), node("a2", "A", [], "running"), node("b1", null, ["a1", "a2"], "blocked")];
  await act(async () => root.render(<CampaignGroupOverview campaignId="c1" nodes={nodes} onSelectGroup={select} tr={(_, en) => en} />));
  expect(container.textContent).toContain("2 groups · 3 tasks");
  expect(container.textContent).toContain("2 cross-group dependencies");
  expect(container.querySelectorAll("output")).toHaveLength(1);
  expect(container.querySelector("output")?.textContent).toBe("2");
  expect(container.querySelector("progress")?.value).toBe(50);
  const ungrouped = container.querySelector<HTMLButtonElement>('button[aria-label^="Ungrouped,"]')!;
  expect(ungrouped.textContent).toContain("1 blocked");
  await act(async () => ungrouped.click());
  expect(select).toHaveBeenCalledWith("");
  await act(async () => container.querySelector<HTMLButtonElement>('button[aria-label^="A,"]')!.click());
  expect(select).toHaveBeenLastCalledWith("A");
  expect(Number(container.querySelector('[data-testid="group-graph"]')?.getAttribute("data-min-zoom"))).toBeGreaterThanOrEqual(0.6);
});

it("renders an empty state without mounting the graph", async () => {
  await act(async () => root.render(<CampaignGroupOverview campaignId="c1" nodes={[]} onSelectGroup={vi.fn()} tr={(_, en) => en} />));
  expect(container.textContent).toContain("No tasks to display.");
  expect(container.querySelector('[data-testid="group-graph"]')).toBeNull();
});

it("names a group by its key, so adding another group does not rename it", () => {
  const before = buildCampaignGroupOverview([node("b1", "B"), node("c1", "C", ["b1"])]);
  const after = buildCampaignGroupOverview([node("a1", "A"), node("b1", "B"), node("c1", "C", ["b1"])]);
  expect(after.groups.slice(1).map((group) => group.id)).toEqual(before.groups.map((group) => group.id));
  expect(after.connections.map((line) => line.id)).toEqual(before.connections.map((line) => line.id));
});

it("splits an aggregate line by its ties without touching the task graph, the network or any write", async () => {
  const fetchSpy = vi.fn();
  vi.stubGlobal("fetch", fetchSpy);
  const nodes = [node("a1", "A"), node("a2", "A"), node("b1", "B", ["a1", "a2"])];
  const snapshot = structuredClone(nodes);
  const tiedEdge: RawEdgeKey = ["a1", "b1"];
  const view = { ties: [{ id: "t1", position: { x: 10, y: 20 }, strandOrder: [tiedEdge] }], routes: [{ edge: tiedEdge, via: ["t1"] }], showOriginal: false };
  const graph = buildCampaignGroupOverview(nodes, view);
  expect(graph.connections.map(({ via, edges }) => ({ via, edges }))).toEqual([
    { via: ["t1"], edges: [["a1", "b1"]] },
    { via: [], edges: [["a2", "b1"]] },
  ]);
  await act(async () => root.render(<CampaignGroupOverview campaignId="c1" nodes={nodes} onSelectGroup={vi.fn()} tr={(_, en) => en} />));
  expect(nodes).toEqual(snapshot);
  expect(campaignRawEdges(nodes)).toEqual([["a1", "b1"], ["a2", "b1"]]);
  expect(fetchSpy).not.toHaveBeenCalled();
  for (const write of [updateCampaignNode, setCampaignAutoQueue, createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand]) {
    expect(vi.mocked(write)).not.toHaveBeenCalled();
  }
  vi.unstubAllGlobals();
});
