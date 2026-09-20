import type { CampaignNode, CampaignNodeStatus } from "../../api/campaigns";

export const NODE_STATUSES: CampaignNodeStatus[] = ["pending", "running", "blocked", "completed", "failed", "skipped"];

export function campaignProgress(nodes: CampaignNode[]) {
  const counts = Object.fromEntries(NODE_STATUSES.map((status) => [status, 0])) as Record<CampaignNodeStatus, number>;
  for (const node of nodes) counts[node.status]++;
  return { counts, total: nodes.length, percent: nodes.length ? Math.floor(100 * counts.completed / nodes.length) : 0 };
}

/** Longest dependency depth puts every predecessor to the left, including joins. */
export function campaignPositions(nodes: CampaignNode[]): Map<string, { x: number; y: number }> {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const depths = new Map<string, number>();
  const visiting = new Set<string>();
  const depth = (id: string): number => {
    if (depths.has(id)) return depths.get(id)!;
    if (visiting.has(id)) return 0; // Do not hang if an older server supplies invalid data.
    visiting.add(id);
    const dependencies = byId.get(id)?.dependencies.filter((dependency) => byId.has(dependency)) ?? [];
    const value = dependencies.length ? Math.max(...dependencies.map(depth)) + 1 : 0;
    visiting.delete(id);
    depths.set(id, value);
    return value;
  };
  const rows = new Map<number, number>();
  return new Map(nodes.map((node) => {
    const column = depth(node.id);
    const row = rows.get(column) ?? 0;
    rows.set(column, row + 1);
    return [node.id, { x: column * 290, y: row * 130 }];
  }));
}

export function safeCampaignLink(value: string | null): string | undefined {
  if (!value) return undefined;
  try {
    const url = new URL(value);
    return ["https:", "http:"].includes(url.protocol) ? url.href : undefined;
  } catch { return undefined; }
}
