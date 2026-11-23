// @vitest-environment happy-dom
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Edge, Node, NodeChange, NodeProps } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { campaignNodeSchema, setCampaignAutoQueue, updateCampaignNode } from "../../api/campaigns";
import { createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand } from "../../api/serverCampaigns";
import { campaignRawEdges, type RawEdgeKey } from "./campaignGraphRouting";
import CampaignGroupOverview, { buildCampaignGroupOverview } from "./CampaignGroupOverview";
import type { LineData } from "./RelationsCanvasChrome";
import { RelationsGroupNode, type GroupNodeData } from "./RelationsTaskNode";

vi.mock("../../api/campaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/campaigns")>()), updateCampaignNode: vi.fn(), setCampaignAutoQueue: vi.fn(),
}));
vi.mock("../../api/serverCampaigns", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../api/serverCampaigns")>()),
  createServerCampaign: vi.fn(), editServerCampaignDefinition: vi.fn(), sendServerCampaignCommand: vi.fn(),
}));

const view = vi.hoisted(() => ({ zoom: 1 }));
vi.mock("@xyflow/react", () => ({
  useStore: (select: (state: { transform: number[] }) => unknown) => select({ transform: [0, 0, view.zoom] }),
  Handle: () => null, Position: { Left: "left", Right: "right" },
  ReactFlow: ({ nodes, edges, onNodesChange, children }: { nodes: Node[]; edges: Edge[]; onNodesChange: (changes: NodeChange[]) => void; children: ReactNode }) => <div data-testid="group-graph">
    {nodes.map((node) => <button key={node.id} aria-label={node.ariaLabel} onClick={() => onNodesChange([{ type: "select", id: node.id, selected: true }])} />)}
    {edges.map((edge) => { const line = edge.data as LineData; return <output key={edge.id} data-error={line.error}>{line.count}{line.cycle}</output>; })}
    {children}
  </div>,
  Background: () => null,
  MarkerType: { ArrowClosed: "closed" },
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
  expect(graph.cycles).toEqual([[a.id, b.id, a.id]]);
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

it("lays linked groups out left to right by dependency depth and groups without a line below them", () => {
  const graph = buildCampaignGroupOverview([node("a", "A"), node("b", "B", ["a"]), node("c", "C", ["b", "a"]), node("s", "Solo")]);
  const at = Object.fromEntries(graph.groups.map((group) => [group.key, group.position]));
  expect([at.A.x < at.B.x, at.B.x < at.C.x, at.Solo.y > Math.max(at.A.y, at.B.y, at.C.y)]).toEqual([true, true, true]);
});

const en = (_: string, value: string) => value;
async function render(nodes: ReturnType<typeof node>[], chosen: string | null) {
  const calls = { choose: vi.fn(), open: vi.fn(), task: vi.fn() };
  await act(async () => root.render(<CampaignGroupOverview campaignId="c1" nodes={nodes} chosen={chosen} onChoose={calls.choose} onOpenGroup={calls.open} onOpenTask={calls.task} tr={en} />));
  return calls;
}
const press = (name: string, scope: Element = container) => act(async () => [...scope.querySelectorAll<HTMLButtonElement>("button")]
  .find((element) => (element.getAttribute("aria-label") ?? element.textContent ?? "").startsWith(name))!.click());

it("draws counts on the lines between groups and chooses a group through its selection, null groups as the empty string", async () => {
  const nodes = [node("a1", "A", [], "completed"), node("a2", "A", [], "running"), node("b1", null, ["a1", "a2"], "blocked")];
  const { choose } = await render(nodes, null);
  expect([...container.querySelectorAll("output")].map((line) => [line.textContent, line.dataset.error])).toEqual([["2", "false"]]);
  expect(container.querySelector('button[aria-label^="Ungrouped,"]')?.getAttribute("aria-label")).toBe("Ungrouped, 1 tasks, 0% completed, 0 running, 1 blocked");
  expect(container.querySelector('[aria-label="Campaign summary"]')?.textContent).toContain("2 groups · 3 tasks");
  await press("Ungrouped,");
  await press("A,");
  expect(choose.mock.calls).toEqual([[""], ["A"]]);
});

it("sums up the chosen group with its blocked tasks and neighbours, and opens it or one of them", async () => {
  const calls = await render([node("a1", "A"), node("b1", "B", ["a1"], "blocked"), node("b2", "B", ["a1"]), node("c1", "C", ["b2"])], "B");
  const aside = container.querySelector('[aria-label="Selected group"]')!;
  expect([...aside.querySelectorAll("li")].map((row) => row.textContent)).toEqual(["b1", "A2 dependencies", "C1 dependencies"]);
  for (const name of ["Open group", "b1", "C"]) await press(name, aside);
  expect([calls.open.mock.calls, calls.task.mock.calls, calls.choose.mock.calls]).toEqual([[["B"]], [["b1"]], [["C"]]]);
});

it("draws every line inside a looped part of the groups red, labels the part once and warns on each of its groups", async () => {
  // A → B, B → A, B → C and C → A: the shortest loop is A → B → A, yet C → A closes a loop too.
  await render([node("a1", "A"), node("b1", "B", ["a1"]), node("a2", "A", ["b1"]), node("c1", "C", ["b1"]), node("a3", "A", ["c1"])], "C");
  const lines = [...container.querySelectorAll("output")];
  expect([lines.map((line) => line.dataset.error), lines.filter((line) => line.textContent?.includes("Cycle")).length]).toEqual([["true", "true", "true", "true"], 1]);
  expect(container.querySelector('[aria-label="Selected group"]')?.textContent).toContain("Cycle · A → B → A");
});

it("covers a group card with its name and pins past 75% out, keeping the red edge inside the blue band when it is chosen and blocked", async () => {
  const data: GroupNodeData = { title: "Relay", meta: "", tone: "warn", ratio: 0.5, pins: ["Needs", "Next"], summary: "", blocked: "1 blocked tasks", far: "50% · 1 blocked" };
  const draw = async (zoom: number) => { view.zoom = zoom; await act(async () => root.render(<RelationsGroupNode {...{ id: "g", data, selected: true } as unknown as NodeProps} />)); };
  await draw(0.8);
  expect(container.querySelector("[data-far]")).toBeNull();
  await draw(0.7);
  expect(container.querySelector("[data-far]")?.textContent).toBe("RelayNeedsNext50% · 1 blocked");
  await draw(0.5);
  expect((container.firstElementChild as HTMLElement).style.boxShadow).toMatch(/^0 0 0 2px var\(--err\), 0 0 0 6px var\(--info\)/);
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
  await render(nodes, null);
  expect(nodes).toEqual(snapshot);
  expect(campaignRawEdges(nodes)).toEqual([["a1", "b1"], ["a2", "b1"]]);
  expect(fetchSpy).not.toHaveBeenCalled();
  for (const write of [updateCampaignNode, setCampaignAutoQueue, createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand]) {
    expect(vi.mocked(write)).not.toHaveBeenCalled();
  }
  vi.unstubAllGlobals();
});
