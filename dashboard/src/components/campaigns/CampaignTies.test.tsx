// @vitest-environment happy-dom
import { act, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import type { Edge, Node } from "@xyflow/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getCampaigns, setCampaignAutoQueue, updateCampaignNode, type Campaign } from "../../api/campaigns";
import { createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand } from "../../api/serverCampaigns";
import type { RawEdgeKey, ViewScope } from "./campaignGraphRouting";
import { campaignViewKey, editorViewScope, groupViewScope } from "./campaignViewStorage";
import CampaignRelations from "./CampaignRelations";
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

type FlowProps = { nodes: Node[]; edges: Edge[]; children: ReactNode };
vi.mock("@xyflow/react", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@xyflow/react")>()),
  ReactFlow: (props: FlowProps) => <div data-testid="flow">
    {props.nodes.map((node) => <button key={node.id} data-node={node.id} data-type={node.type} aria-label={node.ariaLabel} />)}
    {props.edges.map((edge) => <output key={edge.id} data-source={edge.source} data-target={edge.target} data-edges={JSON.stringify(edge.data?.edges)} />)}
    {props.children}
  </div>,
  Background: () => null, Controls: () => null, Handle: () => null,
}));

// A and B in G1, C and D in G2: A → C and B → D cross between the groups.
const nodes = [
  makeCampaignNode("A", { group: "G1" }), makeCampaignNode("B", { group: "G1" }),
  makeCampaignNode("C", { group: "G2", dependencies: ["A"] }), makeCampaignNode("D", { group: "G2", dependencies: ["B"] }),
];
const campaign: Campaign = {
  id: "tie-campaign", title: "Ties", description: "", status: "active", round: 1, revision: 7, auto_queue: true,
  created_at: "2026-10-01T00:00:00Z", updated_at: "2026-10-01T00:00:00Z", nodes,
};
const groupScope = groupViewScope(campaign.id);
const key = campaignViewKey(groupScope);
const AC: RawEdgeKey = ["A", "C"], BD: RawEdgeKey = ["B", "D"];
/** Stores one tie over `edges`, as the editor or the old group graph left it; `revision` undefined is the pre-revision format. */
const seed = (scope: ViewScope, edges: RawEdgeKey[], revision?: number) => localStorage.setItem(campaignViewKey(scope), JSON.stringify({
  schemaVersion: 1, scope, ties: [{ id: "t1", position: { x: 300, y: 60 }, strandOrder: edges }], routes: edges.map((edge) => ({ edge, via: ["t1"] })), showOriginal: false, ...revision ? { revision } : {},
}));

let container: HTMLDivElement;
let root: Root;
let localStorage: Storage;
let fetchSpy: ReturnType<typeof vi.fn>;
beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  localStorage = installMemoryStorage();
  fetchSpy = vi.fn(() => Promise.reject(new Error("no network in tie tests")));
  vi.stubGlobal("fetch", fetchSpy);
  container = document.createElement("div"); document.body.appendChild(container); root = createRoot(container);
});
afterEach(async () => {
  await act(async () => root.unmount()); container.remove();
  vi.unstubAllGlobals(); vi.restoreAllMocks(); vi.clearAllMocks();
});

const render = (shown: Campaign = campaign) => act(async () => root.render(<CampaignRelations campaign={shown} selectedId={null} onOpen={vi.fn()} tr={(_: string, en: string) => en} />));
const button = (name: string) => [...container.querySelectorAll("button")].find((element) => element.textContent?.trim() === name || element.getAttribute("aria-label") === name)!;
const click = (element: HTMLElement) => act(async () => element.click());
const tieNodes = () => [...container.querySelectorAll<HTMLElement>('[data-type="relationsTie"]')];
type Segment = { source: string; target: string; edges: string[] };
const segments = (): Segment[] => [...container.querySelectorAll<HTMLElement>("output")].map((element) => ({
  source: element.dataset.source!, target: element.dataset.target!, edges: (JSON.parse(element.dataset.edges!) as RawEdgeKey[]).map(([from, to]) => `${from}->${to}`),
}));
/** Follows one original's drawn segments from its group through its ties to the group it ends in. */
function pathOf(edge: string): string[] {
  const own = segments().filter((segment) => segment.edges.includes(edge));
  let segment = own.find((candidate) => candidate.source.startsWith("campaign-group:"));
  const hops = [segment?.source];
  for (let guard = 0; segment && guard < 10; guard++) {
    hops.push(segment.target);
    if (segment.target.startsWith("campaign-group:")) return hops as string[];
    const from = segment;
    segment = own.find((candidate) => candidate.source === from.target);
  }
  throw new Error(`${edge} does not reach a group: ${hops.join(" → ")}`);
}
const stored = () => JSON.parse(localStorage.getItem(key)!) as { revision?: number; ties: { strandOrder: RawEdgeKey[] }[] };

it("draws the group graph's saved ties as they were left, even with an old showOriginal, with no tie controls, writes or network", async () => {
  seed(groupScope, [AC, BD], 7);
  localStorage.setItem(key, JSON.stringify({ ...stored(), showOriginal: true })); // left by the removed "show original lines" switch
  const writes = vi.spyOn(localStorage, "setItem");
  await render();
  expect(pathOf("A->C")).toEqual(["campaign-group:G1", "campaign-tie:t1", "campaign-group:G2"]);
  expect(tieNodes().map((element) => element.getAttribute("aria-label"))).toEqual(["Cable tie, 2 lines"]);
  for (const name of ["Tie lines", "Show original lines", "Undo", "Clear all ties"]) expect(button(name)).toBeUndefined();
  expect([writes.mock.calls.length, fetchSpy.mock.calls.length]).toEqual([0, 0]);
  for (const write of [updateCampaignNode, setCampaignAutoQueue, createServerCampaign, editServerCampaignDefinition, sendServerCampaignCommand]) expect(vi.mocked(write)).not.toHaveBeenCalled();
  // Another tab's save shows up; a damaged or other-version value falls back to the original lines.
  seed(groupScope, [BD], 7);
  await act(async () => { window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage })); });
  expect(pathOf("A->C")).toEqual(["campaign-group:G1", "campaign-group:G2"]);
  localStorage.setItem(key, JSON.stringify({ ...stored(), schemaVersion: 2 }));
  await act(async () => { window.dispatchEvent(new StorageEvent("storage", { key, storageArea: localStorage })); });
  expect([tieNodes().length, pathOf("B->D")]).toEqual([0, ["campaign-group:G1", "campaign-group:G2"]]);
});

it("cleans saved group ties as the campaign list arrives: deleted dependencies, a view from before revisions, and a campaign that is gone", async () => {
  const load = (...shown: Campaign[]) => vi.mocked(getCampaigns).mockResolvedValue({ campaigns: shown, live: {}, read: 1 });
  seed(groupScope, [AC, BD]);
  load({ ...campaign, revision: 1, nodes: nodes.map((node) => node.id === "D" ? { ...node, dependencies: [] } : node) });
  await act(async () => root.render(<CampaignsPanel language="en" />));
  expect([stored().ties.map((tie) => tie.strandOrder), stored().revision]).toEqual([[[AC]], 1]);
  load({ ...campaign, id: "other-campaign" });
  await click(button("Refresh"));
  // Gone: nothing outranks that, so the recorded revision goes too and an id made again can clean from a lower one.
  expect([stored().ties, stored().revision]).toEqual([[], undefined]);
});

it("lists a phone's saved editor ties for the campaign, one line each, without members the campaign no longer has", async () => {
  vi.spyOn(window, "matchMedia").mockImplementation((query) => ({ matches: query.includes("max-width"), media: query, addEventListener: vi.fn(), removeEventListener: vi.fn() }) as unknown as MediaQueryList);
  seed(editorViewScope(campaign.id, "group:G2"), [AC, BD, ["A", "gone"]], 7);
  seed(groupScope, [AC], 7);
  await render();
  await click(button("List"));
  const list = container.querySelector('[aria-label="Cable ties"]')!;
  expect([...list.querySelectorAll("li")].map((item) => item.textContent)).toEqual(["Bundle of 2 · A and 1 moreLeads toC · DView only"]);
  await render({ ...campaign, nodes: nodes.map((node) => ({ ...node, dependencies: [] })) });
  expect(container.querySelector('[aria-label="Cable ties"]')).toBeNull();
});
