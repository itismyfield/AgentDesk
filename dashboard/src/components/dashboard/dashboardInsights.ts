import type {
  Agent,
  KanbanCard,
  KanbanCardMetadata,
  ReceiptSnapshotAgentShare,
} from "../../types";

export const DAY_MS = 24 * 60 * 60 * 1000;
export const REVIEW_DELAY_DAYS = 2;
export const LONG_BLOCKED_DAYS = 3;
export const REWORK_ALERT_THRESHOLD = 3;

interface AgentIdentity {
  id: string;
  label: string;
}

export interface AgentRoiRow {
  id: string;
  label: string;
  completed_cards: number;
  tokens: number;
  cost: number;
  cards_per_million_tokens: number;
}

export interface BottleneckRow {
  id: string;
  title: string;
  repo: string | null;
  github_issue_number: number | null;
  age_days: number;
  rework_count: number;
}

export interface BottleneckGroups {
  review_delay: BottleneckRow[];
  repeat_rework: BottleneckRow[];
  long_blocked: BottleneckRow[];
}

function normalizeAgentKey(value: string): string {
  return value.trim().toLowerCase();
}

function addAgentKeys(index: Map<string, AgentIdentity>, agent: Agent) {
  const label = agent.alias || agent.name_ko || agent.name;
  const values = [
    agent.id,
    agent.alias,
    agent.name,
    agent.name_ko,
    agent.name_ja,
    agent.name_zh,
  ];

  for (const value of values) {
    if (!value) continue;
    index.set(normalizeAgentKey(value), { id: agent.id, label });
  }
}

export function parseDashboardTimestamp(value: unknown): number | null {
  if (typeof value === "number" && Number.isFinite(value)) {
    if (value > 1e12) return value;
    if (value > 1e9) return value * 1000;
    return value;
  }
  if (typeof value !== "string" || value.trim().length === 0) return null;

  const trimmed = value.trim();
  const numeric = Number(trimmed);
  if (Number.isFinite(numeric)) return parseDashboardTimestamp(numeric);

  const parsed = Date.parse(trimmed.includes("T") ? trimmed : `${trimmed}T00:00:00`);
  return Number.isNaN(parsed) ? null : parsed;
}

export function parseKanbanMetadata(card: Pick<KanbanCard, "metadata_json" | "metadata">): KanbanCardMetadata | null {
  if (card.metadata && typeof card.metadata === "object") return card.metadata;
  if (!card.metadata_json) return null;
  try {
    return JSON.parse(card.metadata_json) as KanbanCardMetadata;
  } catch {
    return null;
  }
}

function parsePeriodBoundary(value: string | undefined, endOfDay: boolean): number | null {
  if (!value) return null;
  if (/^\d{4}-\d{2}-\d{2}$/.test(value)) {
    return Date.parse(`${value}T${endOfDay ? "23:59:59.999" : "00:00:00.000"}`);
  }
  const parsed = Date.parse(value);
  return Number.isNaN(parsed) ? null : parsed;
}

function buildAgentIndex(agents: Agent[]): Map<string, AgentIdentity> {
  const index = new Map<string, AgentIdentity>();
  for (const agent of agents) addAgentKeys(index, agent);
  return index;
}

function resolveAgentIdentity(raw: string, index: Map<string, AgentIdentity>): AgentIdentity {
  return index.get(normalizeAgentKey(raw)) ?? { id: raw, label: raw };
}

function buildCompletionCounts(
  cards: KanbanCard[],
  periodStart: number | null,
  periodEnd: number | null,
): Map<string, number> {
  const counts = new Map<string, number>();

  for (const card of cards) {
    const completedAt =
      parseDashboardTimestamp(card.completed_at) ??
      (card.status === "done" ? parseDashboardTimestamp(card.updated_at) : null);
    if (!completedAt) continue;
    if (periodStart != null && completedAt < periodStart) continue;
    if (periodEnd != null && completedAt > periodEnd) continue;

    const agentId = card.assignee_agent_id || card.owner_agent_id;
    if (!agentId) continue;
    counts.set(agentId, (counts.get(agentId) ?? 0) + 1);
  }

  return counts;
}

export function buildAgentRoiRows(input: {
  cards: KanbanCard[];
  agentShares: ReceiptSnapshotAgentShare[];
  agents: Agent[];
  periodStart?: string;
  periodEnd?: string;
}): AgentRoiRow[] {
  const periodStart = parsePeriodBoundary(input.periodStart, false);
  const periodEnd = parsePeriodBoundary(input.periodEnd, true);
  const completionCounts = buildCompletionCounts(input.cards, periodStart, periodEnd);
  const agentIndex = buildAgentIndex(input.agents);
  const rows = new Map<string, AgentRoiRow>();

  for (const share of input.agentShares) {
    const identity = resolveAgentIdentity(share.agent, agentIndex);
    rows.set(identity.id, {
      id: identity.id,
      label: identity.label,
      completed_cards: completionCounts.get(identity.id) ?? 0,
      tokens: share.tokens,
      cost: share.cost,
      cards_per_million_tokens: share.tokens > 0
        ? ((completionCounts.get(identity.id) ?? 0) / share.tokens) * 1_000_000
        : 0,
    });
  }

  for (const [agentId, completedCards] of completionCounts) {
    if (rows.has(agentId)) continue;
    const identity = resolveAgentIdentity(agentId, agentIndex);
    rows.set(agentId, {
      id: agentId,
      label: identity.label,
      completed_cards: completedCards,
      tokens: 0,
      cost: 0,
      cards_per_million_tokens: 0,
    });
  }

  return Array.from(rows.values())
    .filter((row) => row.tokens > 0 || row.completed_cards > 0)
    .sort((a, b) => {
      if (b.cards_per_million_tokens !== a.cards_per_million_tokens) {
        return b.cards_per_million_tokens - a.cards_per_million_tokens;
      }
      if (b.completed_cards !== a.completed_cards) {
        return b.completed_cards - a.completed_cards;
      }
      return b.tokens - a.tokens;
    });
}

export function estimateReworkCount(card: KanbanCard): number {
  const metadata = parseKanbanMetadata(card);
  const reviewRounds = Math.max(0, (card.review_round ?? 0) - 1);
  return Math.max(
    reviewRounds,
    metadata?.retry_count ?? 0,
    metadata?.redispatch_count ?? 0,
  );
}

function buildBottleneckRow(card: KanbanCard, ageDays: number): BottleneckRow {
  return {
    id: card.id,
    title: card.title,
    repo: card.github_repo,
    github_issue_number: card.github_issue_number,
    age_days: ageDays,
    rework_count: estimateReworkCount(card),
  };
}

export function buildBottleneckGroups(cards: KanbanCard[], now = Date.now()): BottleneckGroups {
  const reviewDelay: BottleneckRow[] = [];
  const repeatRework: BottleneckRow[] = [];
  const longBlocked: BottleneckRow[] = [];

  for (const card of cards) {
    const updatedAt = parseDashboardTimestamp(card.updated_at) ?? now;
    const ageDays = Math.max(0, Math.floor((now - updatedAt) / DAY_MS));
    const reworkCount = estimateReworkCount(card);

    if (card.status === "review" && ageDays >= REVIEW_DELAY_DAYS) {
      reviewDelay.push(buildBottleneckRow(card, ageDays));
    }
    if (reworkCount >= REWORK_ALERT_THRESHOLD) {
      repeatRework.push(buildBottleneckRow(card, ageDays));
    }
    if ((card.status as string) === "blocked" && ageDays >= LONG_BLOCKED_DAYS) {
      longBlocked.push(buildBottleneckRow(card, ageDays));
    }
  }

  const byAgeDesc = (a: BottleneckRow, b: BottleneckRow) =>
    b.age_days - a.age_days || b.rework_count - a.rework_count || a.title.localeCompare(b.title);
  const byReworkDesc = (a: BottleneckRow, b: BottleneckRow) =>
    b.rework_count - a.rework_count || b.age_days - a.age_days || a.title.localeCompare(b.title);

  reviewDelay.sort(byAgeDesc);
  repeatRework.sort(byReworkDesc);
  longBlocked.sort(byAgeDesc);

  return {
    review_delay: reviewDelay,
    repeat_rework: repeatRework,
    long_blocked: longBlocked,
  };
}
