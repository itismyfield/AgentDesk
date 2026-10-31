import type { Campaign, CampaignNode } from "../../api/campaigns";
import type { KanbanCard } from "../../types";
import { cardTitle, isOpenKanbanCard } from "../agent-manager/kanban-utils";
import { campaignTaskNumber } from "./campaignModel";

const QUEUE_TAB = "queue";

/** `?tab=queue`, optionally with the open card. */
export function parseQueueLocation(search: string): { card: string | null } | null {
  const query = new URLSearchParams(search);
  return query.get("tab") === QUEUE_TAB ? { card: query.get("card") || null } : null;
}
export function queueLocationQuery(card: string | null): string {
  const query = new URLSearchParams({ tab: QUEUE_TAB });
  if (card) query.set("card", card);
  return `?${query}`;
}

const CLOSED_CAMPAIGNS = new Set<Campaign["status"]>(["completed", "cancelled"]);
const CLOSED_NODES = new Set<CampaignNode["status"]>(["completed", "skipped"]);
const ISSUE_URL = /github\.com\/([^/]+\/[^/]+)\/issues\/(\d+)/i;

/** Whether an open task of an open campaign already holds the card's issue. A task without an issue link
 * still names its issue by its "#123" title or "i123" id; that number matches any repository.
 * `heldIssueUrls` adds the issues server campaigns hold. */
export function campaignCardMatcher(campaigns: Campaign[], heldIssueUrls: readonly string[] = []): (card: KanbanCard) => boolean {
  const linked = new Set<string>();
  const numbered = new Set<number>();
  for (const url of heldIssueUrls) {
    const match = url.match(ISSUE_URL);
    if (match) linked.add(`${match[1].toLowerCase()}#${match[2]}`);
  }
  for (const campaign of campaigns) {
    if (CLOSED_CAMPAIGNS.has(campaign.status)) continue;
    for (const node of campaign.nodes) {
      if (CLOSED_NODES.has(node.status)) continue;
      const url = node.issue_url?.match(ISSUE_URL);
      const number = campaignTaskNumber(node);
      if (url) linked.add(`${url[1].toLowerCase()}#${url[2]}`);
      else if (number) numbered.add(Number(number.slice(1)));
    }
  }
  return (card) => {
    const number = card.github_issue_number;
    if (!number) return false;
    return numbered.has(number) || (!!card.github_repo && linked.has(`${card.github_repo.toLowerCase()}#${number}`));
  };
}

/** Open cards no campaign holds: the queue a person hands to ready or puts into a campaign. */
export function queueCards(cards: KanbanCard[], campaigns: Campaign[], heldIssueUrls: readonly string[] = []): KanbanCard[] {
  const inCampaign = campaignCardMatcher(campaigns, heldIssueUrls);
  return cards.filter((card) => isOpenKanbanCard(card) && !inCampaign(card));
}

/** The list's summary view drops the description key; full reads and write answers carry it (null when empty). */
export function isFullCard(card: KanbanCard): boolean {
  return Object.prototype.hasOwnProperty.call(card, "description");
}

/** The card to edit: a full card as it is, or a summary carrying the body and metadata of the last full read of it,
 * so a list refresh never empties the editor and the next save cannot wipe the body. */
export function cardWithBody(card: KanbanCard | undefined, full: KanbanCard | null): KanbanCard | undefined {
  if (!card) return undefined;
  if (isFullCard(card)) return card;
  return full?.id === card.id ? { ...card, description: full.description, metadata: full.metadata } : undefined;
}

export type QueueSort = "priority" | "recent" | "oldest" | "number";
export interface QueueFilters { query: string; status: string; repo: string; sort: QueueSort }
export const EMPTY_QUEUE_FILTERS: QueueFilters = { query: "", status: "all", repo: "all", sort: "priority" };

const PRIORITY_RANK: Record<string, number> = { urgent: 0, high: 1, medium: 2, low: 3 };
const time = (value: string | null | undefined) => (value ? Date.parse(value) : 0) || 0;

export function filterQueueCards(cards: KanbanCard[], filters: QueueFilters): KanbanCard[] {
  const query = filters.query.trim().toLocaleLowerCase();
  const matched = cards.filter((card) => (filters.status === "all" || card.status === filters.status)
    && (filters.repo === "all" || card.github_repo === filters.repo)
    && (!query || [cardTitle(card.title), card.github_issue_number ? `#${card.github_issue_number}` : "", card.github_repo, card.assignee_agent_id]
      .some((value) => value?.toLocaleLowerCase().includes(query))));
  const byRecent = (a: KanbanCard, b: KanbanCard) => time(b.updated_at) - time(a.updated_at);
  const order: Record<QueueSort, (a: KanbanCard, b: KanbanCard) => number> = {
    priority: (a, b) => (PRIORITY_RANK[a.priority] ?? 4) - (PRIORITY_RANK[b.priority] ?? 4) || byRecent(a, b),
    recent: byRecent,
    oldest: (a, b) => time(a.created_at) - time(b.created_at),
    number: (a, b) => (b.github_issue_number ?? 0) - (a.github_issue_number ?? 0),
  };
  return matched.sort(order[filters.sort]);
}

/** A pending task for the card's issue, numbered like the campaign's other tasks ("i123", title "#123 …"). */
export function campaignNodeFromCard(card: KanbanCard & { github_issue_number: number }, campaign: Campaign, issueUrl: string, stage: string, now: string): CampaignNode {
  const taken = new Set(campaign.nodes.map((node) => node.id));
  let id = `i${card.github_issue_number}`;
  for (let copy = 2; taken.has(id); copy++) id = `i${card.github_issue_number}_${copy}`;
  return {
    id, title: `#${card.github_issue_number} ${cardTitle(card.title)}`.slice(0, 512), status: "pending", stage, group: null, round: campaign.round,
    assignee: card.assignee_agent_id, session_id: null, provider: null, dependencies: [], issue_url: issueUrl,
    pr_url: null, head_sha: null, evidence: [], next_action: null, blocker: null, summary: null, benefit: null,
    post_merge_hold: null, updated_at: now, details: "", acceptance: [], findings: [], evidence_records: [],
  };
}
