import { useState } from "react";

import type { Agent, KanbanCard } from "../../types";
import {
  getBoardColumnStatus,
  isManualInterventionCard,
  isReviewCard,
  TERMINAL_STATUSES,
} from "./kanban-utils";

export type KanbanCardTypeFilter = "all" | "issue" | "review";
export type KanbanSignalStatusFilter = "all" | "review" | "blocked" | "requested" | "stalled";

export interface KanbanFilterValues {
  agentFilter: string;
  deptFilter: string;
  cardTypeFilter: KanbanCardTypeFilter;
  signalStatusFilter: KanbanSignalStatusFilter;
  search: string;
  showClosed: boolean;
}

export interface UseKanbanFilterStateResult extends KanbanFilterValues {
  activeFilterCount: number;
  advancedFilterDirty: boolean;
  resetAdvancedFilters: () => void;
  setAgentFilter: (value: string) => void;
  setDeptFilter: (value: string) => void;
  setCardTypeFilter: (value: KanbanCardTypeFilter) => void;
  setSignalStatusFilter: (value: KanbanSignalStatusFilter) => void;
  setSearch: (value: string) => void;
  setShowClosed: (value: boolean) => void;
}

export interface FilterKanbanCardsOptions {
  agentMap: Map<string, Agent>;
  filters: KanbanFilterValues;
  getAgentLabel: (agentId: string | null | undefined) => string;
  nowMs: number;
  repoCards: KanbanCard[];
  selectedAgentId: string | null;
  staleInProgressMs: number;
}

export function useKanbanFilterState(): UseKanbanFilterStateResult {
  const [agentFilter, setAgentFilter] = useState("all");
  const [deptFilter, setDeptFilter] = useState("all");
  const [cardTypeFilter, setCardTypeFilter] = useState<KanbanCardTypeFilter>("all");
  const [signalStatusFilter, setSignalStatusFilter] = useState<KanbanSignalStatusFilter>("all");
  const [search, setSearch] = useState("");
  const [showClosed, setShowClosed] = useState(false);
  const activeFilterCount = getActiveFilterCount({
    agentFilter,
    deptFilter,
    cardTypeFilter,
    signalStatusFilter,
    search,
    showClosed,
  });
  const advancedFilterDirty =
    deptFilter !== "all" ||
    cardTypeFilter !== "all" ||
    signalStatusFilter !== "all" ||
    showClosed;

  return {
    agentFilter,
    deptFilter,
    cardTypeFilter,
    signalStatusFilter,
    search,
    showClosed,
    activeFilterCount,
    advancedFilterDirty,
    resetAdvancedFilters: () => {
      setDeptFilter("all");
      setCardTypeFilter("all");
      setSignalStatusFilter("all");
      setShowClosed(false);
    },
    setAgentFilter,
    setDeptFilter,
    setCardTypeFilter,
    setSignalStatusFilter,
    setSearch,
    setShowClosed,
  };
}

export function getActiveFilterCount(filters: KanbanFilterValues): number {
  return [
    filters.search.trim().length > 0,
    filters.agentFilter !== "all",
    filters.deptFilter !== "all",
    filters.cardTypeFilter !== "all",
    filters.signalStatusFilter !== "all",
    filters.showClosed,
  ].filter(Boolean).length;
}

export function filterKanbanCards({
  agentMap,
  filters,
  getAgentLabel,
  nowMs,
  repoCards,
  selectedAgentId,
  staleInProgressMs,
}: FilterKanbanCardsOptions): KanbanCard[] {
  const needle = filters.search.trim().toLowerCase();
  return repoCards.filter((card) => {
    if (!filters.showClosed && TERMINAL_STATUSES.has(card.status)) {
      return false;
    }
    if (selectedAgentId && card.assignee_agent_id !== selectedAgentId) {
      return false;
    }
    if (filters.agentFilter !== "all" && card.assignee_agent_id !== filters.agentFilter) {
      return false;
    }
    if (
      filters.deptFilter !== "all" &&
      agentMap.get(card.assignee_agent_id ?? "")?.department_id !== filters.deptFilter
    ) {
      return false;
    }
    if (filters.cardTypeFilter === "issue" && isReviewCard(card)) return false;
    if (filters.cardTypeFilter === "review" && !isReviewCard(card)) return false;
    if (filters.signalStatusFilter === "review" && card.status !== "review") return false;
    if (filters.signalStatusFilter === "blocked" && !isManualInterventionCard(card)) return false;
    if (filters.signalStatusFilter === "requested" && getBoardColumnStatus(card.status) !== "requested") return false;
    if (
      filters.signalStatusFilter === "stalled" &&
      !(card.status === "in_progress" && isStaleInProgress(card.started_at, nowMs, staleInProgressMs))
    ) {
      return false;
    }
    if (!needle) return true;
    return (
      card.title.toLowerCase().includes(needle) ||
      (card.description ?? "").toLowerCase().includes(needle) ||
      getAgentLabel(card.assignee_agent_id).toLowerCase().includes(needle)
    );
  });
}

function isStaleInProgress(startedAt: number | null | undefined, nowMs: number, staleInProgressMs: number): boolean {
  if (!startedAt) return false;
  const startedAtMs = startedAt < 1e12 ? startedAt * 1000 : startedAt;
  return nowMs - startedAtMs > staleInProgressMs;
}
