import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { readCachedGet } from "./httpClient";
import { campaignNodeLiveSchema, campaignSchema, getCampaigns, setCampaignAutoQueue, updateCampaignNode } from "./campaigns";

const timestamp = "2026-09-20T00:00:00Z";
const campaignPayload = {
  id: "release-one",
  title: "Release",
  description: "",
  status: "active",
  round: 2,
  revision: 7,
  created_at: timestamp,
  updated_at: timestamp,
  nodes: [
    {
      id: "review",
      title: "Review",
      status: "running",
      stage: "review",
      round: 2,
      updated_at: timestamp,
      evidence_records: [
        {
          summary: "CI",
          command: null,
          result: "Passed",
          head_sha: null,
          recorded_at: null,
          references: [],
        },
      ],
    },
    {
      id: "deploy",
      title: "Deploy",
      status: "pending",
      stage: "deploy",
      round: 2,
      updated_at: timestamp,
      dependencies: ["review"],
    },
  ],
};
const campaign = campaignSchema.parse(campaignPayload);
const fetchMock = vi.fn<typeof fetch>();
function response(payload: unknown) {
  return new Response(JSON.stringify(payload), {
    status: 200,
    headers: { "Content-Type": "application/json" },
  });
}
beforeEach(() => {
  fetchMock.mockReset();
  vi.stubGlobal("fetch", fetchMock);
});
afterEach(() => vi.unstubAllGlobals());

it("includes older campaigns beyond the first page with validated pagination metadata", async () => {
  const first = Array.from({ length: 100 }, (_, id) => ({
    ...campaignPayload,
    id: String(id),
  }));
  const live = {
    card_id: "card-7",
    card_status: "in_progress",
    dispatch_type: "review",
    dispatch_status: "dispatched",
    session_status: "turn_active",
    session_seen_at: "2026-09-27T12:00:00Z",
    running: true,
    queue_status: null,
  };
  fetchMock
    .mockResolvedValueOnce(
      response({
        campaigns: first,
        live: { "0": { review: live } },
        limit: 100,
        offset: 0,
      }),
    )
    .mockResolvedValueOnce(
      response({
        campaigns: [{ ...campaignPayload, id: "older-campaign" }],
        limit: 100,
        offset: 100,
      }),
    );
  const result = await getCampaigns();
  expect(result.campaigns).toHaveLength(101);
  expect(result.campaigns.at(-1)?.id).toBe("older-campaign");
  expect(result.live).toEqual({ "0": { review: live } });
  expect(fetchMock.mock.calls[1][0]).toBe("/api/campaigns?limit=100&offset=100");
});

it("saves against the edited revision while preserving sibling tasks and nullable evidence fields", async () => {
  fetchMock.mockResolvedValue(response({ campaign }));
  const result = await updateCampaignNode(campaign, {
    ...campaign.nodes[0],
    next_action: "Inspect CI",
  });
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

it("leaves auto_queue out of node saves and sends it only from the auto-run switch", async () => {
  fetchMock.mockResolvedValue(response({ campaign }));
  await updateCampaignNode(campaign, { ...campaign.nodes[0], next_action: "Inspect CI" });
  expect(JSON.parse(fetchMock.mock.calls[0][1]?.body as string)).not.toHaveProperty("auto_queue");
  expect(campaign.auto_queue).toBe(false);

  fetchMock.mockResolvedValue(response({ campaign: { ...campaignPayload, auto_queue: true, revision: 8 },
    handoff: { queued: [{ node_id: "deploy", card_id: "card-9", run_id: "run-3" }], waiting: [] } }));
  const result = await setCampaignAutoQueue(campaign, true);
  const body = JSON.parse(fetchMock.mock.calls[1][1]?.body as string);
  expect(body).toMatchObject({ expected_revision: 7, auto_queue: true });
  expect(body.nodes).toEqual(campaign.nodes);
  expect(result.campaign.auto_queue).toBe(true);
  expect(result.handoff?.queued[0].node_id).toBe("deploy");
  expect(result.handoffError).toBeNull();
});

it("applies serde defaults for absent arrays and optional text while retaining additive fields", () => {
  const parsed = campaignSchema.parse({
    ...campaignPayload,
    nodes: [{ ...campaignPayload.nodes[1], future_checkpoint: "keep" }],
  });
  expect(parsed.nodes[0]).toMatchObject({
    group: null,
    details: "",
    acceptance: [],
    findings: [],
    evidence: [],
    evidence_records: [],
    assignee: null,
    future_checkpoint: "keep",
  });
  expect(
    campaignSchema.parse({
      ...campaignPayload,
      nodes: [{ ...campaignPayload.nodes[1], group: "Gateway" }],
    }).nodes[0].group,
  ).toBe("Gateway");
  expect(
    campaignSchema.parse({
      ...campaignPayload,
      nodes: [
        {
          ...campaignPayload.nodes[0],
          evidence_records: [{ summary: "Minimal evidence" }],
        },
      ],
    }).nodes[0].evidence_records[0],
  ).toEqual({
    summary: "Minimal evidence",
    command: null,
    result: null,
    head_sha: null,
    recorded_at: null,
    references: [],
  });
});

it("rejects malformed node and evidence fields rather than passing unsafe values to rendering", () => {
  for (const patch of [
    { dependencies: null },
    { evidence: null },
    { acceptance: null },
    { evidence_records: null },
    { evidence_records: [{ summary: "CI", references: null }] },
    { evidence_records: [{ summary: "CI", recorded_at: "yesterday" }] },
    { round: 0 },
    { round: 1.5 },
    { round: 4_294_967_296 },
    { status: "unknown" },
    { stage: " " },
    { group: 42 },
    { updated_at: "invalid" },
  ]) {
    expect(
      campaignSchema.safeParse({
        ...campaignPayload,
        nodes: [{ ...campaignPayload.nodes[0], ...patch }],
      }).success,
    ).toBe(false);
  }
  for (const patch of [{ round: 0 }, { revision: 0 }, { revision: 0.5 }, { revision: Number.MAX_SAFE_INTEGER + 1 }, { nodes: null }]) {
    expect(campaignSchema.safeParse({ ...campaignPayload, ...patch }).success).toBe(false);
  }
});

it("rejects malformed list payloads before caching or returning them", async () => {
  const url = "/api/campaigns?limit=100&offset=0";
  const priorCache = readCachedGet(url);
  fetchMock.mockImplementation(async () =>
    response({
      campaigns: [
        {
          ...campaignPayload,
          nodes: [
            {
              ...campaignPayload.nodes[0],
              evidence_records: [{ summary: "CI", references: null }],
            },
          ],
        },
      ],
      limit: 100,
      offset: 0,
    }),
  );
  await expect(getCampaigns()).rejects.toThrow();
  expect(readCachedGet(url)).toEqual(priorCache);
});

it("validates PUT response shape before accepting a successful save", async () => {
  fetchMock.mockResolvedValue(response({ campaign: { ...campaignPayload, revision: 0 } }));
  await expect(updateCampaignNode(campaign, campaign.nodes[0])).rejects.toThrow();
  expect(fetchMock).toHaveBeenCalledTimes(1);
});

it("validates additive working fields while accepting legacy live responses", () => {
  const legacy = {
    card_id: "card-7",
    card_status: "in_progress",
    dispatch_type: "consultation",
    dispatch_status: "pending",
    session_status: null,
    session_seen_at: null,
    running: true,
    queue_status: null,
  };
  expect(campaignNodeLiveSchema.parse(legacy)).toEqual(legacy);
  const working = {
    dispatch_id: "D2",
    working_dispatch_id: "D1",
    working_dispatch_type: "implementation",
    working_session_id: "42",
    working_session_status: "turn_active",
    working_session_seen_at: timestamp,
  };
  expect(campaignNodeLiveSchema.parse({ ...legacy, ...working })).toEqual({
    ...legacy,
    ...working,
  });
  for (const field of Object.keys(working)) {
    expect(campaignNodeLiveSchema.safeParse({ ...legacy, ...working, [field]: 42 }).success).toBe(false);
    expect(campaignNodeLiveSchema.safeParse({ ...legacy, ...working, [field]: null }).success).toBe(true);
  }
});
