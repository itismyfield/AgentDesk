import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { readCachedGet } from "./httpClient";
import { campaignSchema, getCampaigns, updateCampaignNode } from "./campaigns";

const timestamp = "2026-09-20T00:00:00Z";
const campaignPayload = {
  id: "release-one", title: "Release", description: "", status: "active", round: 2, revision: 7,
  created_at: timestamp, updated_at: timestamp,
  nodes: [
    { id: "review", title: "Review", status: "running", stage: "review", round: 2, updated_at: timestamp,
      evidence_records: [{ summary: "CI", command: null, result: "Passed", head_sha: null, recorded_at: null, references: [] }] },
    { id: "deploy", title: "Deploy", status: "pending", stage: "deploy", round: 2, updated_at: timestamp, dependencies: ["review"] },
  ],
};
const campaign = campaignSchema.parse(campaignPayload);
const fetchMock = vi.fn<typeof fetch>();
function response(payload: unknown) { return new Response(JSON.stringify(payload), { status: 200, headers: { "Content-Type": "application/json" } }); }
beforeEach(() => { fetchMock.mockReset(); vi.stubGlobal("fetch", fetchMock); });
afterEach(() => vi.unstubAllGlobals());

it("includes older campaigns beyond the first page with validated pagination metadata", async () => {
  const first = Array.from({ length: 100 }, (_, id) => ({ ...campaignPayload, id: String(id) }));
  fetchMock.mockResolvedValueOnce(response({ campaigns: first, limit: 100, offset: 0 }))
    .mockResolvedValueOnce(response({ campaigns: [{ ...campaignPayload, id: "older-campaign" }], limit: 100, offset: 100 }));
  const result = await getCampaigns();
  expect(result).toHaveLength(101);
  expect(result.at(-1)?.id).toBe("older-campaign");
  expect(fetchMock.mock.calls[1][0]).toBe("/api/campaigns?limit=100&offset=100");
});

it("saves against the edited revision while preserving sibling tasks and nullable evidence fields", async () => {
  fetchMock.mockResolvedValue(response({ campaign }));
  const result = await updateCampaignNode(campaign, { ...campaign.nodes[0], next_action: "Inspect CI" });
  const [url, options] = fetchMock.mock.calls[0];
  expect(url).toBe("/api/campaigns/release-one");
  expect(options?.method).toBe("PUT");
  const body = JSON.parse(options?.body as string);
  expect(body.expected_revision).toBe(7);
  expect(body.nodes[1]).toEqual(campaign.nodes[1]);
  expect(body.nodes[0].evidence_records).toEqual(campaign.nodes[0].evidence_records);
  expect(body.nodes[0].next_action).toBe("Inspect CI");
  expect(result.nodes[0].evidence_records[0].command).toBeNull();
});

it("applies serde defaults for absent arrays and optional text while retaining additive fields", () => {
  const parsed = campaignSchema.parse({ ...campaignPayload, nodes: [{ ...campaignPayload.nodes[1], future_checkpoint: "keep" }] });
  expect(parsed.nodes[0]).toMatchObject({ details: "", acceptance: [], findings: [], evidence: [], evidence_records: [], assignee: null, future_checkpoint: "keep" });
  expect(campaignSchema.parse({ ...campaignPayload, nodes: [{ ...campaignPayload.nodes[0], evidence_records: [{ summary: "Minimal evidence" }] }] }).nodes[0].evidence_records[0])
    .toEqual({ summary: "Minimal evidence", command: null, result: null, head_sha: null, recorded_at: null, references: [] });
});

it("rejects malformed node and evidence fields rather than passing unsafe values to rendering", () => {
  for (const patch of [
    { dependencies: null }, { evidence: null }, { acceptance: null }, { evidence_records: null },
    { evidence_records: [{ summary: "CI", references: null }] },
    { evidence_records: [{ summary: "CI", recorded_at: "yesterday" }] },
    { round: 0 }, { round: 1.5 }, { round: 4_294_967_296 }, { status: "unknown" }, { stage: " " }, { updated_at: "invalid" },
  ]) {
    expect(campaignSchema.safeParse({ ...campaignPayload, nodes: [{ ...campaignPayload.nodes[0], ...patch }] }).success).toBe(false);
  }
  for (const patch of [{ round: 0 }, { revision: 0 }, { revision: 0.5 }, { revision: Number.MAX_SAFE_INTEGER + 1 }, { nodes: null }]) {
    expect(campaignSchema.safeParse({ ...campaignPayload, ...patch }).success).toBe(false);
  }
});

it("rejects malformed list payloads before caching or returning them", async () => {
  const url = "/api/campaigns?limit=100&offset=0";
  const priorCache = readCachedGet(url);
  fetchMock.mockImplementation(async () => response({ campaigns: [{ ...campaignPayload, nodes: [{ ...campaignPayload.nodes[0], evidence_records: [{ summary: "CI", references: null }] }] }], limit: 100, offset: 0 }));
  await expect(getCampaigns()).rejects.toThrow();
  expect(readCachedGet(url)).toEqual(priorCache);
});

it("validates PUT response shape before accepting a successful save", async () => {
  fetchMock.mockResolvedValue(response({ campaign: { ...campaignPayload, revision: 0 } }));
  await expect(updateCampaignNode(campaign, campaign.nodes[0])).rejects.toThrow();
  expect(fetchMock).toHaveBeenCalledTimes(1);
});
