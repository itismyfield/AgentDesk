import type { GitHubIssue } from "../../api";
import type { KanbanCard } from "../../types";

export const BACKLOG_PAGE_SIZE = 8;

export type KanbanBacklogEntry =
  | { kind: "card"; key: string; card: KanbanCard }
  | { kind: "issue"; key: string; issue: GitHubIssue };

export interface AdvancedKanbanFilterState {
  showClosed: boolean;
  agentFilter: string;
  deptFilter: string;
  cardTypeFilter: "all" | "issue" | "review";
  signalStatusFilter: "all" | "review" | "blocked" | "requested" | "stalled";
}

export function buildKanbanBacklogEntries(
  backlogCards: readonly KanbanCard[],
  backlogIssues: readonly GitHubIssue[],
): KanbanBacklogEntry[] {
  return [
    ...backlogCards.map((card) => ({ kind: "card" as const, key: `card-${card.id}`, card })),
    ...backlogIssues.map((issue) => ({ kind: "issue" as const, key: `issue-${issue.number}`, issue })),
  ];
}

export function paginateKanbanBacklogEntries(
  entries: readonly KanbanBacklogEntry[],
  requestedPage: number,
  pageSize = BACKLOG_PAGE_SIZE,
): {
  page: number;
  pageCount: number;
  items: KanbanBacklogEntry[];
} {
  if (entries.length === 0) {
    return { page: 0, pageCount: 0, items: [] };
  }

  const pageCount = Math.ceil(entries.length / pageSize);
  const page = Math.min(Math.max(requestedPage, 0), pageCount - 1);
  const start = page * pageSize;
  return {
    page,
    pageCount,
    items: entries.slice(start, start + pageSize),
  };
}

export function countActiveKanbanAdvancedFilters(
  state: AdvancedKanbanFilterState,
): number {
  let count = 0;
  if (state.showClosed) count += 1;
  if (state.agentFilter !== "all") count += 1;
  if (state.deptFilter !== "all") count += 1;
  if (state.cardTypeFilter !== "all") count += 1;
  if (state.signalStatusFilter !== "all") count += 1;
  return count;
}
