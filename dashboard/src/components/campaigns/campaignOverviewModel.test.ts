import { expect, it } from "vitest";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import type { ServerCampaignSummary } from "../../api/serverCampaigns";
import { serverEntry } from "./campaignOverviewModel";

const tr = (ko: string) => ko;

it("draws the current stage of a stopping, stopped or inspection-needed campaign as waiting, not running", () => {
  const doc = serverCampaignFixtures.stoppingUnknown;
  const head = (display: string) => {
    const view = { ...doc.view, display, failed_count: 0 } as ServerCampaignSummary["view"];
    return serverEntry({ definition: doc.definition, view } as ServerCampaignSummary, { ...doc, view } as typeof doc, tr).segments?.find((segment) => segment !== "done" && segment !== "todo");
  };
  expect(["stopping", "stopped", "needs_inspection", "waiting", "running"].map(head)).toEqual(["wait", "wait", "wait", "wait", "now"]);
});
