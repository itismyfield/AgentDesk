import { z } from "zod";
import { request } from "./httpClient";

const campaignStatusSchema = z.enum(["planned", "active", "paused", "completed", "cancelled"]);
const campaignNodeStatusSchema = z.enum(["pending", "running", "blocked", "completed", "failed", "skipped"]);
const idSchema = z.string().min(1).max(128).regex(/^[A-Za-z0-9_.-]+$/);
const roundSchema = z.number().int().positive().max(4_294_967_295);
const timestampSchema = z.iso.datetime({ offset: true });
const nullableText = z.string().nullable().default(null);
const textList = z.array(z.string()).default([]);

// Preserve additive server fields because edits replace the complete aggregate.
export const campaignEvidenceSchema = z.looseObject({
  summary: z.string(),
  command: nullableText,
  result: nullableText,
  head_sha: nullableText,
  recorded_at: timestampSchema.nullable().default(null),
  references: textList,
});

export const campaignNodeSchema = z.looseObject({
  id: idSchema,
  title: z.string().min(1).max(512).refine((value) => value.trim().length > 0),
  status: campaignNodeStatusSchema,
  stage: z.string().min(1).max(128).refine((value) => value.trim().length > 0),
  group: nullableText,
  round: roundSchema,
  assignee: nullableText,
  session_id: nullableText,
  provider: nullableText,
  dependencies: z.array(idSchema).default([]),
  issue_url: nullableText,
  pr_url: nullableText,
  head_sha: nullableText,
  evidence: textList,
  next_action: nullableText,
  blocker: nullableText,
  summary: nullableText,
  benefit: nullableText,
  updated_at: timestampSchema,
  details: z.string().default(""),
  acceptance: textList,
  findings: textList,
  evidence_records: z.array(campaignEvidenceSchema).default([]),
});

export const campaignSchema = z.looseObject({
  id: idSchema,
  title: z.string().min(1).max(512).refine((value) => value.trim().length > 0),
  description: z.string().default(""),
  status: campaignStatusSchema,
  round: roundSchema,
  revision: z.number().int().positive(),
  nodes: z.array(campaignNodeSchema).max(1000).default([]),
  created_at: timestampSchema,
  updated_at: timestampSchema,
});

// Read from the node's issue card on every request; never sent back on save.
export const campaignNodeLiveSchema = z.looseObject({
  card_id: z.string(),
  card_status: z.string(),
  dispatch_type: nullableText,
  dispatch_status: nullableText,
  queue_status: nullableText,
});
const nodeLiveMapSchema = z.record(z.string(), campaignNodeLiveSchema);

const campaignListResponseSchema = z.looseObject({
  campaigns: z.array(campaignSchema),
  live: z.record(z.string(), nodeLiveMapSchema).default({}),
  limit: z.number().int().positive().max(500),
  offset: z.number().int().nonnegative(),
});
const campaignResponseSchema = z.looseObject({ campaign: campaignSchema });

export type CampaignStatus = z.infer<typeof campaignStatusSchema>;
export type CampaignNodeStatus = z.infer<typeof campaignNodeStatusSchema>;
export type CampaignNode = z.infer<typeof campaignNodeSchema>;
export type Campaign = z.infer<typeof campaignSchema>;
export type CampaignNodeLive = z.infer<typeof campaignNodeLiveSchema>;
/** Campaign id, then node id. */
export type CampaignLive = Record<string, Record<string, CampaignNodeLive>>;

export async function getCampaigns(): Promise<{ campaigns: Campaign[]; live: CampaignLive }> {
  const campaigns = new Map<string, Campaign>();
  const live: CampaignLive = {};
  const limit = 100;
  for (let offset = 0; ; offset += limit) {
    const result = await request(`/api/campaigns?limit=${limit}&offset=${offset}`, { suppressErrorToast: true }, campaignListResponseSchema);
    for (const campaign of result.campaigns) campaigns.set(campaign.id, campaign);
    Object.assign(live, result.live);
    if (result.campaigns.length < limit) return { campaigns: Array.from(campaigns.values()), live };
  }
}

export async function updateCampaignNode(campaign: Campaign, updated: CampaignNode): Promise<Campaign> {
  const result = await request(`/api/campaigns/${encodeURIComponent(campaign.id)}`, {
    method: "PUT",
    suppressErrorToast: true,
    body: JSON.stringify({
      expected_revision: campaign.revision,
      title: campaign.title,
      description: campaign.description,
      status: campaign.status,
      round: campaign.round,
      nodes: campaign.nodes.map((node) => node.id === updated.id ? updated : node),
    }),
  }, campaignResponseSchema);
  return result.campaign;
}
