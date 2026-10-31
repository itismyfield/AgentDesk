// @vitest-environment happy-dom
import { beforeEach, expect, it, vi } from "vitest";
import { NO_TIES, type ViewScope } from "./campaignGraphRouting";
import { campaignViewKey, loadCampaignView, saveCampaignView } from "./campaignViewStorage";
import { installMemoryStorage } from "./campaignTestFixtures";

const scope: ViewScope = { mode: "legacy", campaignId: "c1", projection: "groups", taskScope: "all" };
const view = { ties: [{ id: "t1", position: { x: 4, y: 8 }, strandOrder: [["a", "b"] as const] }], routes: [{ edge: ["a", "b"] as const, via: ["t1"] }], showOriginal: false };

let localStorage: Storage;
beforeEach(() => { localStorage = installMemoryStorage(); });

it("restores a saved view only in the same mode, campaign, graph and task scope", () => {
  expect(saveCampaignView(scope, view).ok).toBe(true);
  expect(loadCampaignView(scope)).toMatchObject({ view, recovered: false });
  for (const other of [{ ...scope, mode: "server" as const }, { ...scope, campaignId: "c2" }, { ...scope, taskScope: "group:A" }]) {
    expect(campaignViewKey(other)).not.toBe(campaignViewKey(scope));
    expect(loadCampaignView(other)).toEqual({ view: NO_TIES, raw: null, recovered: false });
  }
});

it("falls back to the original lines for a damaged, other-version or inconsistent value", () => {
  const stored = { schemaVersion: 1, scope, ...view };
  const damaged = [
    "{not json",
    JSON.stringify({ ...stored, schemaVersion: 2 }),
    JSON.stringify({ ...stored, scope: { ...scope, campaignId: "c2" } }),
    JSON.stringify({ ...stored, routes: [{ edge: ["a", "b"], via: ["missing"] }] }),
    JSON.stringify({ ...stored, ties: [{ id: "t1", position: { x: "4", y: 8 }, strandOrder: [] }] }),
    JSON.stringify({ ...stored, ties: [{ id: "t1", position: { x: 4, y: 8 }, strandOrder: ["a:b"] }] }),
  ];
  for (const raw of damaged) {
    localStorage.setItem(campaignViewKey(scope), raw);
    expect(loadCampaignView(scope), raw).toEqual({ view: NO_TIES, raw, recovered: true });
  }
});

it("reports a write the browser refused instead of swallowing it", () => {
  vi.spyOn(localStorage, "setItem").mockImplementation(() => { throw new DOMException("full", "QuotaExceededError"); });
  expect(saveCampaignView(scope, view)).toEqual({ ok: false, reason: "QuotaExceededError" });
});
