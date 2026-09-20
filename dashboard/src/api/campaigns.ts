import { request } from "./httpClient";

export type CampaignStatus = "planned" | "active" | "paused" | "completed" | "cancelled";
export type CampaignNodeStatus = "pending" | "running" | "blocked" | "completed" | "failed" | "skipped";

export interface CampaignNode {
  id: string;
  title: string;
  status: CampaignNodeStatus;
  stage: string;
  round: number;
  assignee: string | null;
  session_id: string | null;
  provider: string | null;
  dependencies: string[];
  issue_url: string | null;
  pr_url: string | null;
  head_sha: string | null;
  evidence: string[];
  next_action: string | null;
  blocker: string | null;
  updated_at: string;
  details?: string;
  acceptance?: string[];
  findings?: string[];
  evidence_records?: Array<{ summary: string; command?: string; result?: string; head_sha?: string; recorded_at?: string; references: string[] }>;
}

export interface Campaign {
  id: string;
  title: string;
  description: string;
  status: CampaignStatus;
  round: number;
  revision: number;
  nodes: CampaignNode[];
  created_at: string;
  updated_at: string;
}

export async function getCampaigns(): Promise<Campaign[]> {
  const campaigns = new Map<string, Campaign>();
  const limit = 100;
  for (let offset = 0; ; offset += limit) {
    const result = await request<{ campaigns: Campaign[] }>(`/api/campaigns?limit=${limit}&offset=${offset}`, { suppressErrorToast: true });
    for (const campaign of result.campaigns) campaigns.set(campaign.id, campaign);
    if (result.campaigns.length < limit) return Array.from(campaigns.values());
  }
}

export async function updateCampaignNode(campaign: Campaign, updated: CampaignNode): Promise<Campaign> {
  const result = await request<{ campaign: Campaign }>(`/api/campaigns/${encodeURIComponent(campaign.id)}`, {
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
  });
  return result.campaign;
}
