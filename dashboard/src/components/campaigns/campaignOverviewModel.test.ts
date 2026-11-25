import { expect, it } from "vitest";
import { campaignNodeSchema, type Campaign } from "../../api/campaigns";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import type { ServerCampaignSummary } from "../../api/serverCampaigns";
import { effectiveRelationCounts, legacyEntry, legacyTask, serverEntry } from "./campaignOverviewModel";
import { makeCampaignNode } from "./campaignTestFixtures";

const tr = (ko: string) => ko;

it("splits a server campaign's bar by stage, drawing a moving stage of a stopping, stopped or inspection-needed campaign as waiting", () => {
  const doc = serverCampaignFixtures.stoppingUnknown;
  const split = (display: string) => {
    const view = { ...doc.view, display, failed_count: 0 } as ServerCampaignSummary["view"];
    return serverEntry({ definition: doc.definition, view } as ServerCampaignSummary, { ...doc, view } as typeof doc, tr).split;
  };
  expect(split("running")?.map(({ key, count, tone }) => `${key} ${count} ${tone}`)).toEqual(["requirements 1 todo", "implementation 1 now", "complete 1 done"]);
  expect(["stopping", "stopped", "needs_inspection", "waiting"].map((display) => split(display)?.[1].tone)).toEqual(["wait", "wait", "wait", "wait"]);
  expect(serverEntry({ definition: doc.definition, view: doc.view } as ServerCampaignSummary, null, tr).split).toBeNull();
});

it("never throws in legacyTask or legacyEntry when partial summary nodes have undefined stage or evidence", () => {
  const partialNode = {
    ...makeCampaignNode("i99", { status: "running", blocker: "CI hold" }),
    stage: undefined as unknown as string,
    evidence: undefined as unknown as string[],
    dependencies: undefined as unknown as string[],
  };
  const campaign: Campaign = {
    id: "partial-summary",
    title: "Partial summary",
    description: "",
    status: "active",
    round: 1,
    revision: 1,
    auto_queue: false,
    nodes_partial: true,
    nodes: [partialNode],
    created_at: "2026-10-01T00:00:00Z",
    updated_at: "2026-10-01T00:00:00Z",
  };
  const task = legacyTask(partialNode);
  expect(task.stage).toBe("");
  expect(task.step).toBeNull();
  const entry = legacyEntry(campaign, undefined, tr);
  expect(entry.attention).toBe("막힘 1");
  expect(effectiveRelationCounts([partialNode]).counts.blocked).toBe(1);
  expect(campaignNodeSchema.parse({ id: "s1", title: "Summary", status: "pending", round: 1, updated_at: "2026-10-01T00:00:00Z" }).stage).toBe("");
});
