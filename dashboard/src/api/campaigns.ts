import { z } from "zod";
import { ApiRequestError, request } from "./httpClient";

const campaignStatusSchema = z.enum(["planned", "active", "paused", "completed", "cancelled"]);
const campaignNodeStatusSchema = z.enum(["pending", "running", "blocked", "completed", "failed", "skipped"]);
const idSchema = z
  .string()
  .min(1)
  .max(128)
  .regex(/^[A-Za-z0-9_.-]+$/);
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
  title: z
    .string()
    .min(1)
    .max(512)
    .refine((value) => value.trim().length > 0),
  status: campaignNodeStatusSchema,
  stage: z
    .string()
    .min(1)
    .max(128)
    .refine((value) => value.trim().length > 0),
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
  title: z
    .string()
    .min(1)
    .max(512)
    .refine((value) => value.trim().length > 0),
  description: z.string().default(""),
  status: campaignStatusSchema,
  round: roundSchema,
  revision: z.number().int().positive(),
  auto_queue: z.boolean().default(false),
  nodes: z.array(campaignNodeSchema).max(1000).default([]),
  created_at: timestampSchema,
  updated_at: timestampSchema,
});

// Read from the node's issue card on every request; never sent back on save.
export const campaignNodeLiveSchema = z.looseObject({
  card_id: z.string(),
  card_status: z.string(),
  dispatch_id: z.string().nullable().optional(),
  dispatch_type: nullableText,
  dispatch_status: nullableText,
  session_status: nullableText,
  session_seen_at: nullableText,
  working_dispatch_id: z.string().nullable().optional(),
  working_dispatch_type: z.string().nullable().optional(),
  working_session_id: z.string().nullable().optional(),
  working_session_status: z.string().nullable().optional(),
  working_session_seen_at: z.string().nullable().optional(),
  running: z.boolean().default(false),
  queue_status: nullableText,
});
const nodeLiveMapSchema = z.record(z.string(), campaignNodeLiveSchema);

const campaignListResponseSchema = z.looseObject({
  campaigns: z.array(campaignSchema),
  live: z.record(z.string(), nodeLiveMapSchema).default({}),
  limit: z.number().int().positive().max(500),
  offset: z.number().int().nonnegative(),
});
const campaignHandoffSchema = z.looseObject({
  queued: z.array(z.looseObject({ node_id: z.string(), card_id: z.string(), run_id: z.string() })).default([]),
  waiting: z.array(z.looseObject({ node_id: z.string(), reason: z.string(), detail: z.string().nullable().optional() })).default([]),
});
const campaignGetResponseSchema = z.looseObject({ campaign: campaignSchema });
const campaignSaveResponseSchema = z.looseObject({
  campaign: campaignSchema,
  handoff: campaignHandoffSchema.optional(),
  handoff_error: z.string().optional(),
});

export type CampaignStatus = z.infer<typeof campaignStatusSchema>;
export type CampaignNodeStatus = z.infer<typeof campaignNodeStatusSchema>;
export type CampaignNode = z.infer<typeof campaignNodeSchema>;
export type Campaign = z.infer<typeof campaignSchema>;
export type CampaignNodeLive = z.infer<typeof campaignNodeLiveSchema>;
export type CampaignHandoff = z.infer<typeof campaignHandoffSchema>;
/** Campaign id, then node id. */
export type CampaignLive = Record<string, Record<string, CampaignNodeLive>>;

let readsStarted = 0;
/** Number of campaign reads started so far; a read numbered above it starts later. */
export function campaignReadMark(): number {
  return readsStarted;
}

// Reads never join a GET already in flight, so a read's number says when its data was fetched.
export async function getCampaigns(): Promise<{
  campaigns: Campaign[];
  live: CampaignLive;
  read: number;
}> {
  const read = ++readsStarted;
  const campaigns = new Map<string, Campaign>();
  const live: CampaignLive = {};
  const limit = 100;
  for (let offset = 0; ; offset += limit) {
    const result = await request(`/api/campaigns?limit=${limit}&offset=${offset}`, { suppressErrorToast: true, shareInflight: false }, campaignListResponseSchema);
    for (const campaign of result.campaigns) campaigns.set(campaign.id, campaign);
    Object.assign(live, result.live);
    if (result.campaigns.length < limit) return { campaigns: Array.from(campaigns.values()), live, read };
  }
}

export async function getCampaign(id: string): Promise<Campaign> {
  const result = await request(`/api/campaigns/${encodeURIComponent(id)}`, { suppressErrorToast: true, shareInflight: false }, campaignGetResponseSchema);
  return result.campaign;
}

/** True when the server answered with a refusal, so the save is known not to have happened. */
export function isRejectedSave(error: unknown): boolean {
  return error instanceof ApiRequestError && error.status < 500;
}

// Omitting auto_queue keeps the stored value, so node edits never switch it.
function saveCampaign(campaign: Campaign, patch: Partial<Pick<Campaign, "nodes" | "auto_queue">>, timeoutMs?: number) {
  return request(
    `/api/campaigns/${encodeURIComponent(campaign.id)}`,
    {
      method: "PUT",
      suppressErrorToast: true,
      timeoutMs,
      body: JSON.stringify({
        expected_revision: campaign.revision,
        title: campaign.title,
        description: campaign.description,
        status: campaign.status,
        round: campaign.round,
        nodes: campaign.nodes,
        ...patch,
      }),
    },
    campaignSaveResponseSchema,
  );
}

export async function updateCampaignNode(campaign: Campaign, updated: CampaignNode): Promise<Campaign> {
  const result = await saveCampaign(campaign, { nodes: campaign.nodes.map((node) => (node.id === updated.id ? updated : node)) });
  return result.campaign;
}

/** Turning it on also queues the tasks that are ready now, so the save waits longer than the default. */
export async function setCampaignAutoQueue(campaign: Campaign, enabled: boolean) {
  const result = await saveCampaign(campaign, { auto_queue: enabled }, 60_000);
  return { campaign: result.campaign, handoff: result.handoff ?? null, handoffError: result.handoff_error ?? null };
}
