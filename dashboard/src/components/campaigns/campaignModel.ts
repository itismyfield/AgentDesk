import type { CampaignNode, CampaignNodeStatus } from "../../api/campaigns";

export const NODE_STATUSES: CampaignNodeStatus[] = ["pending", "running", "blocked", "completed", "failed", "skipped"];

export interface CampaignFilters {
  query: string;
  status: CampaignNodeStatus | "all";
  group: string | null;
  hideCompleted: boolean;
}
export const EMPTY_FILTERS: CampaignFilters = { query: "", status: "all", group: null, hideCompleted: false };
export function campaignGroup(node: CampaignNode): string { return node.group || ""; }
export function campaignIssueLabel(node: CampaignNode): string {
  const safe = safeCampaignLink(node.issue_url);
  const issue = safe ? new URL(safe).pathname.match(/\/issues\/(\d+)\/?$/)?.[1] : null;
  return issue ? `#${issue}` : node.id;
}
export function filterCampaignNodes(nodes: CampaignNode[], filters: CampaignFilters): CampaignNode[] {
  const query = filters.query.trim().toLocaleLowerCase();
  return nodes.filter((node) => (!filters.hideCompleted || node.status !== "completed")
    && (filters.status === "all" || node.status === filters.status)
    && (filters.group === null || campaignGroup(node) === filters.group)
    && (!query || [node.id, campaignIssueLabel(node), node.title, node.stage, node.group, node.assignee, node.session_id].some((value) => value?.toLocaleLowerCase().includes(query))));
}
export function groupCampaignNodes(nodes: CampaignNode[]) {
  const groups = new Map<string, CampaignNode[]>();
  for (const node of nodes) {
    const key = campaignGroup(node);
    const entries = groups.get(key) ?? [];
    entries.push(node);
    groups.set(key, entries);
  }
  return Array.from(groups, ([key, entries]) => ({ key, nodes: entries, progress: campaignProgress(entries) }))
    .sort((a, b) => a.key === "" ? 1 : b.key === "" ? -1 : a.key.localeCompare(b.key));
}

/** Limit each side independently so high fan-in never hides every successor. */
export function dependencyNeighborhood(nodes: CampaignNode[], selectedId: string, perSide = 8) {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const selected = byId.get(selectedId);
  const upstream = selected?.dependencies.flatMap((id) => byId.has(id) ? [byId.get(id)!] : []) ?? [];
  const downstream = nodes.filter((node) => node.dependencies.includes(selectedId));
  const visibleUpstream = upstream.slice(0, perSide);
  const visibleDownstream = downstream.slice(0, perSide);
  const visible = selected ? [...visibleUpstream, selected, ...visibleDownstream] : [];
  const visibleIds = new Set(visible.map((node) => node.id));
  return {
    selected, upstream, downstream, visibleUpstream, visibleDownstream, visible,
    omittedUpstream: upstream.length - visibleUpstream.length,
    omittedDownstream: downstream.length - visibleDownstream.length,
    externalDependencies: new Map(visible.map((node) => [node.id, node.dependencies.filter((id) => !visibleIds.has(id))])),
  };
}

export function campaignProgress(nodes: CampaignNode[]) {
  const counts = Object.fromEntries(NODE_STATUSES.map((status) => [status, 0])) as Record<CampaignNodeStatus, number>;
  for (const node of nodes) counts[node.status]++;
  return { counts, total: nodes.length, percent: nodes.length ? Math.floor(100 * counts.completed / nodes.length) : 0 };
}

export function safeCampaignLink(value: string | null): string | undefined {
  if (!value) return undefined;
  try {
    const url = new URL(value);
    return ["https:", "http:"].includes(url.protocol) ? url.href : undefined;
  } catch { return undefined; }
}
