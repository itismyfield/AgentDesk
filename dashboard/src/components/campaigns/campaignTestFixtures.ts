import type { Campaign, CampaignNode } from "../../api/campaigns";
import { NODE_STATUSES } from "./campaignModel";

export function makeCampaignNode(id: string, patch: Partial<CampaignNode> = {}): CampaignNode {
  return {
    id, title: id, status: "pending", stage: "implement", group: null, round: 1,
    assignee: null, session_id: null, provider: null, dependencies: [], issue_url: null, pr_url: null,
    head_sha: null, evidence: [], next_action: "Inspect the latest diff", blocker: null, summary: null, benefit: null,
    updated_at: "2026-09-20T00:00:00Z", details: "", acceptance: [], findings: [], evidence_records: [], ...patch,
  };
}

export function makeLargeCampaign(): Campaign {
  return {
    id: "large-campaign", title: "120 issue campaign", description: "Grouped work", status: "active", round: 3, revision: 5,
    created_at: "2026-09-20T00:00:00Z", updated_at: "2026-09-20T00:00:00Z",
    nodes: Array.from({ length: 120 }, (_, index) => makeCampaignNode(`task-${index}`, {
      title: `Task ${index}: keep the durable checkpoint`,
      group: ["Restart protocol", "Gateway", "Routines", null][Math.floor(index / 30)],
      status: NODE_STATUSES[index % NODE_STATUSES.length], stage: ["implement", "review", "verify"][index % 3],
      round: index % 4 + 1, assignee: `agent-${index % 5}`, session_id: `session-${index}`,
      issue_url: `https://github.com/example/project/issues/${5700 + index}`,
      dependencies: index ? [`task-${index - 1}`] : [],
    })),
  };
}
