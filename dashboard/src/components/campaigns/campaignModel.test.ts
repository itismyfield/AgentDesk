import { describe, expect, it } from "vitest";
import type { CampaignNode } from "../../api/campaigns";
import { campaignIssueLabel, campaignProgress, dependencyNeighborhood, EMPTY_FILTERS, filterCampaignNodes, groupCampaignNodes, safeCampaignLink } from "./campaignModel";
import { makeCampaignNode, makeLargeCampaign } from "./campaignTestFixtures";

function node(id: string, dependencies: string[] = [], status: CampaignNode["status"] = "pending"): CampaignNode {
  return makeCampaignNode(id, { dependencies, status });
}

describe("campaign progress", () => {
  it("never shows 100 percent while a task remains unfinished", () => {
    const nodes = Array.from({ length: 300 }, (_, index) => node(String(index), [], index ? "completed" : "running"));
    expect(campaignProgress(nodes).percent).toBe(99);
  });
  it("keeps skipped, failed and blocked tasks distinct from actual completion", () => {
    const result = campaignProgress([node("a", [], "completed"), node("b", [], "skipped"), node("c", [], "failed"), node("d", [], "blocked")]);
    expect(result.percent).toBe(25);
    expect(result.counts).toEqual({ completed: 1, skipped: 1, failed: 1, blocked: 1, running: 0, pending: 0 });
    expect(campaignProgress([]).percent).toBe(0);
  });
});

describe("large campaign exploration", () => {
  it("groups only explicit categories and keeps unclassified work separate from status and stage", () => {
    const groups = groupCampaignNodes(makeLargeCampaign().nodes);
    expect(groups).toHaveLength(4);
    expect(groups.at(-1)?.key).toBe("");
    expect(groups.every((group) => group.nodes.length === 30)).toBe(true);
    expect(groups.every((group) => group.progress.counts.running === 5)).toBe(true);
  });
  it("finds issue numbers independently of the canonical node ID and composes filters", () => {
    const nodes = makeLargeCampaign().nodes;
    expect(filterCampaignNodes(nodes, { ...EMPTY_FILTERS, query: "#5701" }).map((value) => value.id)).toEqual(["task-1"]);
    expect(filterCampaignNodes(nodes, { ...EMPTY_FILTERS, query: "5701" }).map((value) => value.id)).toEqual(["task-1"]);
    expect(filterCampaignNodes(nodes, { ...EMPTY_FILTERS, group: "Gateway", status: "blocked" })).toHaveLength(5);
    expect(filterCampaignNodes(nodes, { ...EMPTY_FILTERS, hideCompleted: true })).toHaveLength(100);
  });
  it("bounds a 120-node fan-in/out and reports omitted and external connections", () => {
    const upstream = Array.from({ length: 100 }, (_, index) => node(`up-${index}`));
    const selected = node("selected", upstream.map((value) => value.id));
    const downstream = Array.from({ length: 19 }, (_, index) => node(`down-${index}`, ["selected"]));
    const context = dependencyNeighborhood([...upstream, selected, ...downstream], "selected");
    expect(context.visible).toHaveLength(17);
    expect(context.omittedUpstream).toBe(92);
    expect(context.omittedDownstream).toBe(11);
    expect(context.externalDependencies.get("selected")).toHaveLength(92);
    expect(context.visible.map((value) => value.id)).toEqual(dependencyNeighborhood([...upstream, selected, ...downstream], "selected").visible.map((value) => value.id));
  });
  it("never turns an unsafe issue URL into a displayed issue number", () => {
    expect(campaignIssueLabel(makeCampaignNode("safe-id", { issue_url: "javascript:/issues/5701" }))).toBe("safe-id");
  });
});

it("allows only safe issue and PR links", () => {
  expect(safeCampaignLink("javascript:alert(1)")).toBeUndefined();
  expect(safeCampaignLink("data:text/html,bad")).toBeUndefined();
  expect(safeCampaignLink("https://github.com/org/repo/pull/1")).toBe("https://github.com/org/repo/pull/1");
});
