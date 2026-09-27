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
    && (!query || [node.id, campaignIssueLabel(node), node.title, node.summary, node.stage, node.group, node.assignee, node.session_id].some((value) => value?.toLocaleLowerCase().includes(query))));
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

export const CAMPAIGN_STAGES: Array<{ key: string; label: [string, string]; keywords: string[] }> = [
  { key: "investigate", label: ["조사", "Investigate"], keywords: ["audit", "investigat", "research", "triage", "조사", "감사", "진단", "재판정"] },
  { key: "design", label: ["설계", "Design"], keywords: ["design", "설계", "계획"] },
  { key: "implement", label: ["구현", "Build"], keywords: ["impl", "구현"] },
  { key: "review", label: ["리뷰", "Review"], keywords: ["review", "ready_for_pr", "리뷰", "검토"] },
  { key: "repair", label: ["수리", "Fix"], keywords: ["fix", "repair", "rework", "review_fix", "수리", "리뷰 반영", "재작업"] },
  { key: "merge", label: ["머지", "Merge"], keywords: ["merge", "ready(held)", "review_clean", "ci", "머지"] },
  { key: "deploy", label: ["배포 확인", "Deploy check"], keywords: ["deploy", "rollout", "observ", "post-merge", "post_merge", "merged", "머지됨", "머지 완료", "배포", "관측"] },
];

/** Stage is free text: the stage keyword written first wins, and a longer keyword wins a tie. */
export function campaignStageStep(stage: string): { index: number; raw: string } {
  const text = stage.toLocaleLowerCase();
  let best = { index: -1, at: Infinity, length: 0 };
  CAMPAIGN_STAGES.forEach(({ keywords }, index) => {
    for (const keyword of keywords) {
      // English keywords must start a word so "suffix" is not "fix"; "ci" must also end one.
      const at = /^[a-z]/.test(keyword) ? text.search(new RegExp(`(?<![a-z])${keyword.replace(/[()]/g, "\\$&")}${keyword === "ci" ? "(?![a-z])" : ""}`)) : text.indexOf(keyword);
      if (at >= 0 && (at < best.at || (at === best.at && keyword.length > best.length))) best = { index, at, length: keyword.length };
    }
  });
  const plain = stage.replace(/\b[0-9a-f]{7,40}\b/gi, "").replace(/\s+/g, " ").trim();
  return { index: best.index, raw: plain.length > 28 ? `${plain.slice(0, 27)}…` : plain };
}

/** Running then blocked work leads the first screen; every other status is only counted. */
export function campaignGlance(nodes: CampaignNode[]) {
  const active = [...nodes.filter((node) => node.status === "running"), ...nodes.filter((node) => node.status === "blocked")];
  const buckets = (["pending", "completed", "skipped", "failed"] as const)
    .map((status) => ({ status, nodes: nodes.filter((node) => node.status === status) }))
    .filter((bucket) => bucket.nodes.length > 0);
  return { active, running: active.filter((node) => node.status === "running").length, buckets };
}
