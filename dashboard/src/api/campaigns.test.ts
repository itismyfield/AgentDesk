import { afterEach, expect, it, vi } from "vitest";
import { request } from "./httpClient";
import { getCampaigns, updateCampaignNode, type Campaign } from "./campaigns";

vi.mock("./httpClient", () => ({ request: vi.fn() }));
afterEach(() => vi.clearAllMocks());

it("includes older campaigns beyond the first page so a remembered selection can be restored", async () => {
  const first = Array.from({ length: 100 }, (_, id) => ({ id: String(id) }));
  vi.mocked(request).mockResolvedValueOnce({ campaigns: first }).mockResolvedValueOnce({ campaigns: [{ id: "older-campaign" }] });
  const result = await getCampaigns();
  expect(result).toHaveLength(101);
  expect(result.at(-1)?.id).toBe("older-campaign");
  expect(vi.mocked(request).mock.calls[1][0]).toBe("/api/campaigns?limit=100&offset=100");
});

it("saves against the edited revision while preserving sibling tasks and structured evidence", async () => {
  const campaign = {
    id: "release/one", title: "Release", description: "", status: "active", round: 2, revision: 7,
    nodes: [
      { id: "review", status: "running", evidence_records: [{ summary: "CI", references: [] }] },
      { id: "deploy", status: "pending", dependencies: ["review"] },
    ],
  } as unknown as Campaign;
  vi.mocked(request).mockResolvedValue({ campaign });
  await updateCampaignNode(campaign, { ...campaign.nodes[0], next_action: "Inspect CI" });
  const [url, options] = vi.mocked(request).mock.calls[0];
  expect(url).toBe("/api/campaigns/release%2Fone");
  expect(options?.method).toBe("PUT");
  const body = JSON.parse(options?.body as string);
  expect(body.expected_revision).toBe(7);
  expect(body.nodes[1]).toEqual(campaign.nodes[1]);
  expect(body.nodes[0].evidence_records).toEqual(campaign.nodes[0].evidence_records);
  expect(body.nodes[0].next_action).toBe("Inspect CI");
});
