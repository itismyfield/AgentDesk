import { expect, it } from "vitest";
import { serverCampaignFixtures } from "../../api/serverCampaignsFixtures";
import type { ServerCampaignSummary } from "../../api/serverCampaigns";
import { serverEntry } from "./campaignOverviewModel";

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
