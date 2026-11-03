import { expect, it } from "vitest";
import type { Campaign } from "../../api/campaigns";
import { makeCampaignNode } from "./campaignTestFixtures";
import { newest } from "./useLegacyCampaigns";

const campaign = (revision: number, partial: boolean): Campaign => ({
  id: "c", title: "C", description: "", status: "active", round: 1, revision, auto_queue: false,
  created_at: "2026-09-20T00:00:00Z", updated_at: "2026-09-20T00:00:00Z",
  nodes: [makeCampaignNode("n", { details: partial ? "" : "full notes" })], ...(partial ? { nodes_partial: true } : {}),
});

it("keeps the full copy over a list summary of the same revision, but takes any newer revision", () => {
  const full = campaign(3, false);
  expect(newest([full], [campaign(3, true)])[0]).toBe(full);
  expect(newest([full], [campaign(4, true)])[0].revision).toBe(4);
  expect(newest([campaign(3, true)], [full])[0]).toBe(full);
  expect(newest([full], [campaign(2, false)])[0]).toBe(full);
});
