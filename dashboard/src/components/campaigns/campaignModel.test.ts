import { describe, expect, it } from "vitest";
import type { CampaignNode } from "../../api/campaigns";
import { campaignPositions, campaignProgress, safeCampaignLink } from "./campaignModel";

function node(id: string, dependencies: string[] = [], status: CampaignNode["status"] = "pending"): CampaignNode {
  return { id, title: id, dependencies, status, stage: "review", round: 2, assignee: null, session_id: null, provider: null, issue_url: null, pr_url: null, head_sha: null, evidence: [], next_action: null, blocker: null, updated_at: "" };
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

describe("campaign DAG layout", () => {
  it("places a diamond join after all its predecessors regardless of input order", () => {
    const positions = campaignPositions([node("join", ["left", "right"]), node("right", ["root"]), node("root"), node("left", ["root"])]);
    expect(positions.get("join")!.x).toBeGreaterThan(positions.get("left")!.x);
    expect(positions.get("join")!.x).toBeGreaterThan(positions.get("right")!.x);
    expect(positions.get("left")!.x).toBeGreaterThan(positions.get("root")!.x);
    expect(positions.get("left")!.y).not.toBe(positions.get("right")!.y);
  });
  it("does not hang on malformed legacy dependencies", () => {
    expect(campaignPositions([node("a", ["b"]), node("b", ["a"]), node("c", ["missing"])]).size).toBe(3);
  });
});

it("allows only safe issue and PR links", () => {
  expect(safeCampaignLink("javascript:alert(1)")).toBeUndefined();
  expect(safeCampaignLink("data:text/html,bad")).toBeUndefined();
  expect(safeCampaignLink("https://github.com/org/repo/pull/1")).toBe("https://github.com/org/repo/pull/1");
});
