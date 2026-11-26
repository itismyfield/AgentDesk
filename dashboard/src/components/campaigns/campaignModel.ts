import type { CampaignNode, CampaignNodeLive, CampaignNodeStatus } from "../../api/campaigns";
import type { RawEdgeKey } from "./campaignGraphRouting";

export const NODE_STATUSES: CampaignNodeStatus[] = ["pending", "running", "blocked", "completed", "failed", "skipped"];

export function campaignGroup(node: CampaignNode): string { return node.group || ""; }
export function campaignIssueNumber(node: CampaignNode): string | null {
  const safe = safeCampaignLink(node.issue_url);
  const issue = safe ? new URL(safe).pathname.match(/\/issues\/(\d+)\/?$/)?.[1] : null;
  return issue ? `#${issue}` : null;
}
const TITLE_NUMBER = /^#(\d+)\b[\s:·-]*/;
/** The issue number shown once per task: the linked issue, else a leading "#123" in the title, else an "i123" node id. */
export function campaignTaskNumber(node: CampaignNode): string | null {
  const title = node.title ?? "";
  const id = node.id ?? "";
  const number = campaignIssueNumber(node)?.slice(1) ?? title.match(TITLE_NUMBER)?.[1] ?? id.match(/^i(\d+)(?:_|$)/)?.[1];
  return number ? `#${number}` : null;
}
/** The title without a leading copy of the task number. */
export function campaignTaskTitle(node: CampaignNode): string {
  const title = node.title ?? "";
  const match = title.match(TITLE_NUMBER);
  const stripped = match && `#${match[1]}` === campaignTaskNumber(node) ? title.slice(match[0].length).trim() : "";
  return stripped || title;
}
export function campaignIssueLabel(node: CampaignNode): string { return campaignTaskNumber(node) ?? (node.id ?? ""); }
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

/** Limit each side independently so high fan-in never hides every successor.
 * Edges past the limit are hidden, not deleted; a dependency on a missing task is no edge at all. */
export function dependencyNeighborhood(nodes: CampaignNode[], selectedId: string, perSide = 8) {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const selected = byId.get(selectedId);
  const upstream = (selected?.dependencies ?? []).flatMap((id) => byId.has(id) ? [byId.get(id)!] : []);
  const downstream = nodes.filter((node) => (node.dependencies ?? []).includes(selectedId));
  const visibleUpstream = upstream.slice(0, perSide);
  const visibleDownstream = downstream.slice(0, perSide);
  const visible = selected ? [...visibleUpstream, selected, ...visibleDownstream] : [];
  const visibleIds = new Set(visible.map((node) => node.id));
  const edgesOf = (before: CampaignNode[], after: CampaignNode[]): RawEdgeKey[] => [
    ...before.map((node): RawEdgeKey => [node.id, selectedId]), ...after.map((node): RawEdgeKey => [selectedId, node.id]),
  ];
  return {
    selected, upstream, downstream, visibleUpstream, visibleDownstream, visible,
    edges: edgesOf(visibleUpstream, visibleDownstream),
    hiddenEdges: edgesOf(upstream.slice(perSide), downstream.slice(perSide)),
    omittedUpstream: upstream.length - visibleUpstream.length,
    omittedDownstream: downstream.length - visibleDownstream.length,
    externalDependencies: new Map(visible.map((node) => [node.id, (node.dependencies ?? []).filter((id) => byId.has(id) && !visibleIds.has(id))])),
  };
}

export function campaignProgress(nodes: ReadonlyArray<Pick<CampaignNode, "status">>) {
  const counts = Object.fromEntries(NODE_STATUSES.map((status) => [status, 0])) as Record<CampaignNodeStatus, number>;
  for (const node of nodes) counts[node.status]++;
  return { counts, total: nodes.length, percent: nodes.length ? Math.floor(100 * counts.completed / nodes.length) : 0 };
}

export function safeCampaignLink(value: string | null | undefined): string | undefined {
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
export function campaignStageStep(stage?: string | null): { index: number; raw: string } {
  const raw = typeof stage === "string" ? stage : "";
  const text = raw.toLocaleLowerCase();
  let best = { index: -1, at: Infinity, length: 0 };
  CAMPAIGN_STAGES.forEach(({ keywords }, index) => {
    for (const keyword of keywords) {
      // English keywords must start a word so "suffix" is not "fix"; "ci" must also end one.
      const at = /^[a-z]/.test(keyword) ? text.search(new RegExp(`(?<![a-z])${keyword.replace(/[()]/g, "\\$&")}${keyword === "ci" ? "(?![a-z])" : ""}`)) : text.indexOf(keyword);
      if (at >= 0 && (at < best.at || (at === best.at && keyword.length > best.length))) best = { index, at, length: keyword.length };
    }
  });
  const plain = raw.replace(/\b[0-9a-f]{7,40}\b/gi, "").replace(/\s+/g, " ").trim();
  return { index: best.index, raw: plain.length > 28 ? `${plain.slice(0, 27)}…` : plain };
}

/** A session is mid-turn on the node's issue card right now (the server checks its heartbeat). */
export function liveIsWorking(live: CampaignNodeLive | undefined): boolean {
  return live?.running === true;
}
