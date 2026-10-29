// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Edge } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { rawEdgeId } from "./campaignGraphRouting";
import CampaignNeighborhood from "./CampaignNeighborhood";
import { makeCampaignNode } from "./campaignTestFixtures";

const drawn: { edges: Edge[] } = { edges: [] };
vi.mock("@xyflow/react", () => ({
  ReactFlow: ({ edges }: { edges: Edge[] }) => { drawn.edges = edges; return null; },
  Background: () => null, Controls: () => null,
  MarkerType: { ArrowClosed: "closed" }, Position: { Right: "right", Left: "left" },
}));

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => { await act(async () => root.unmount()); container.remove(); });

it("draws each visible connection as its directed raw edge and leaves hidden ones out", async () => {
  // Nine dependencies exceed the per-side limit of eight, so u8 -> s is hidden, not drawn.
  const upstream = Array.from({ length: 9 }, (_, index) => makeCampaignNode(`u${index}`));
  const nodes = [...upstream, makeCampaignNode("s", { dependencies: upstream.map((node) => node.id) }), makeCampaignNode("d", { dependencies: ["s"] })];
  await act(async () => root.render(<CampaignNeighborhood nodes={nodes} selectedId="s" visibleIds={new Set(nodes.map((node) => node.id))} onSelect={vi.fn()} tr={(_, en) => en} />));
  const expected = [...upstream.slice(0, 8).map((node) => [node.id, "s"] as const), ["s", "d"] as const];
  expect(drawn.edges.map(({ id, source, target }) => ({ id, source, target }))).toEqual(
    expected.map((edge) => ({ id: rawEdgeId(edge), source: edge[0], target: edge[1] })),
  );
  expect(container.textContent).toContain("Outside this graph: 1 dependencies · 0 dependents.");
});
