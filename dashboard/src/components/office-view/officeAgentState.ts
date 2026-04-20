import type { Agent, KanbanCard, KanbanCardStatus } from "../../types";
import {
  hasManualInterventionReason,
  isManualInterventionCard,
} from "../agent-manager/kanban-utils";

export type OfficeSeatStatus = "working" | "idle" | "review" | "offline";

export interface OfficeActiveIssue {
  cardId: string;
  title: string;
  status: KanbanCardStatus;
  number: number | null;
  url: string | null;
  startedAt: number | null;
  updatedAt: number;
}

export interface OfficeManualIntervention {
  cardId: string;
  title: string;
  status: KanbanCardStatus;
  reviewStatus: string | null;
  reason: string | null;
  issueNumber: number | null;
  issueUrl: string | null;
  updatedAt: number;
}

export interface OfficeAgentState {
  activeIssueByAgent: Map<string, OfficeActiveIssue>;
  manualInterventionByAgent: Map<string, OfficeManualIntervention>;
  primaryCardByAgent: Map<string, KanbanCard>;
  seatStatusByAgent: Map<string, OfficeSeatStatus>;
}

const TERMINAL_CARD_STATUSES = new Set<KanbanCardStatus>(["done"]);

const PRIMARY_CARD_PRIORITY: Record<KanbanCardStatus, number> = {
  review: 0,
  in_progress: 1,
  requested: 2,
  blocked: 3,
  qa_in_progress: 4,
  qa_pending: 5,
  qa_failed: 6,
  ready: 7,
  backlog: 8,
  done: 9,
};

const ACTIVE_ISSUE_PRIORITY: Record<KanbanCardStatus, number> = {
  review: 0,
  in_progress: 1,
  requested: 2,
  blocked: 3,
  qa_in_progress: 4,
  qa_pending: 5,
  qa_failed: 6,
  ready: 7,
  backlog: 8,
  done: 9,
};

function normalizeTimestampMs(value: number | string | null | undefined): number | null {
  if (value == null || value === "") return null;
  if (typeof value === "number") {
    return value < 1e12 ? value * 1000 : value;
  }
  const numeric = Number(value);
  if (Number.isFinite(numeric)) {
    return numeric < 1e12 ? numeric * 1000 : numeric;
  }
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

function compareCards(left: KanbanCard, right: KanbanCard, priority: Record<KanbanCardStatus, number>): number {
  const leftPriority = priority[left.status] ?? 99;
  const rightPriority = priority[right.status] ?? 99;
  if (leftPriority !== rightPriority) {
    return leftPriority - rightPriority;
  }
  return right.updated_at - left.updated_at;
}

function selectPreferredCard(current: KanbanCard | undefined, next: KanbanCard): KanbanCard {
  if (!current) return next;
  return compareCards(next, current, PRIMARY_CARD_PRIORITY) < 0 ? next : current;
}

function selectPreferredIssue(current: OfficeActiveIssue | undefined, next: OfficeActiveIssue): OfficeActiveIssue {
  if (!current) return next;
  const currentPriority = ACTIVE_ISSUE_PRIORITY[current.status] ?? 99;
  const nextPriority = ACTIVE_ISSUE_PRIORITY[next.status] ?? 99;
  if (nextPriority !== currentPriority) {
    return nextPriority < currentPriority ? next : current;
  }
  return next.updatedAt > current.updatedAt ? next : current;
}

function selectPreferredManualIntervention(
  current: OfficeManualIntervention | undefined,
  next: OfficeManualIntervention,
): OfficeManualIntervention {
  if (!current) return next;

  const nextHasReason = Boolean(next.reason?.trim());
  const currentHasReason = Boolean(current.reason?.trim());
  if (nextHasReason !== currentHasReason) {
    return nextHasReason ? next : current;
  }

  const currentPriority = ACTIVE_ISSUE_PRIORITY[current.status] ?? 99;
  const nextPriority = ACTIVE_ISSUE_PRIORITY[next.status] ?? 99;
  if (nextPriority !== currentPriority) {
    return nextPriority < currentPriority ? next : current;
  }

  return next.updatedAt > current.updatedAt ? next : current;
}

function buildIssueUrl(card: KanbanCard): string | null {
  if (card.github_issue_url) return card.github_issue_url;
  if (card.github_repo && card.github_issue_number) {
    return `https://github.com/${card.github_repo}/issues/${card.github_issue_number}`;
  }
  return null;
}

export function deriveOfficeAgentState(
  agents: Agent[],
  kanbanCards?: KanbanCard[],
): OfficeAgentState {
  const activeIssueByAgent = new Map<string, OfficeActiveIssue>();
  const manualInterventionByAgent = new Map<string, OfficeManualIntervention>();
  const primaryCardByAgent = new Map<string, KanbanCard>();

  for (const card of kanbanCards ?? []) {
    const agentId = card.assignee_agent_id;
    if (!agentId) continue;
    if (!TERMINAL_CARD_STATUSES.has(card.status)) {
      primaryCardByAgent.set(agentId, selectPreferredCard(primaryCardByAgent.get(agentId), card));
    }

    if (card.status === "review" || card.status === "in_progress") {
      activeIssueByAgent.set(
        agentId,
        selectPreferredIssue(activeIssueByAgent.get(agentId), {
          cardId: card.id,
          title: card.title,
          status: card.status,
          number: card.github_issue_number ?? null,
          url: buildIssueUrl(card),
          startedAt: normalizeTimestampMs(card.started_at),
          updatedAt: card.updated_at,
        }),
      );
    }

    if (isManualInterventionCard(card)) {
      manualInterventionByAgent.set(
        agentId,
        selectPreferredManualIntervention(manualInterventionByAgent.get(agentId), {
          cardId: card.id,
          title: card.title,
          status: card.status,
          reviewStatus: card.review_status ?? null,
          reason: hasManualInterventionReason(card) ? card.blocked_reason?.trim() ?? null : null,
          issueNumber: card.github_issue_number ?? null,
          issueUrl: buildIssueUrl(card),
          updatedAt: card.updated_at,
        }),
      );
    }
  }

  const seatStatusByAgent = new Map<string, OfficeSeatStatus>();
  for (const agent of agents) {
    if (agent.status === "offline") {
      seatStatusByAgent.set(agent.id, "offline");
      continue;
    }
    const activeIssue = activeIssueByAgent.get(agent.id);
    if (activeIssue?.status === "review") {
      seatStatusByAgent.set(agent.id, "review");
      continue;
    }
    if (agent.status === "working") {
      seatStatusByAgent.set(agent.id, "working");
      continue;
    }
    seatStatusByAgent.set(agent.id, "idle");
  }

  return {
    activeIssueByAgent,
    manualInterventionByAgent,
    primaryCardByAgent,
    seatStatusByAgent,
  };
}
