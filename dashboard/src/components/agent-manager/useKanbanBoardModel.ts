import { useEffect, useMemo } from "react";
import * as api from "../../api";
import type { KanbanCard, KanbanCardStatus } from "../../types";
import {
  BOARD_COLUMN_DEFS,
  TERMINAL_STATUSES,
  getBoardColumnStatus,
  isManualInterventionCard,
  isReviewCard,
} from "./kanban-utils";
import { filterKanbanCards } from "./kanban-filter-state";

interface UseKanbanBoardModelParams {
  [key: string]: any;
}

export function useKanbanBoardModel(params: UseKanbanBoardModelParams) {
  const {
    agentFilter,
    agentMap,
    agents,
    cardTypeFilter,
    cards,
    cardsById,
    deptFilter,
    getAgentLabel,
    nowMs,
    repoSources,
    search,
    selectedAgentId,
    selectedCardId,
    selectedRepo,
    setAgentPipelineStages,
    setRecentDonePage,
    setSelectedAgentId,
    setSelectedCardId,
    showClosed,
    signalStatusFilter,
    staleInProgressMs,
    tr,
    issues,
    agentPipelineStages,
    mobileColumnStatus,
  } = params;

  const repoCards = useMemo<KanbanCard[]>(() => {
    if (!selectedRepo) return [] as KanbanCard[];
    return cards.filter((card: KanbanCard) => card.github_repo === selectedRepo);
  }, [cards, selectedRepo]);

  const repoCardsById = useMemo(
    () => new Map<string, KanbanCard>(repoCards.map((card: KanbanCard) => [card.id, card])),
    [repoCards],
  );

  // Agents that have cards in the current repo (for the per-agent dropdown)
  const repoAgentCounts = useMemo(() => {
    const counts = new Map<string, number>();
    for (const card of repoCards) {
      if (card.assignee_agent_id) {
        counts.set(card.assignee_agent_id, (counts.get(card.assignee_agent_id) ?? 0) + 1);
      }
    }
    return counts;
  }, [repoCards]);
  const repoAgentEntries = useMemo(
    () => Array.from(repoAgentCounts.entries()).sort((a, b) => b[1] - a[1]),
    [repoAgentCounts],
  );
  const selectedRepoSource = useMemo(
    () => repoSources.find((source: any) => source.repo === selectedRepo) ?? null,
    [repoSources, selectedRepo],
  );
  const pipelineHookEntries = useMemo(() => {
    const hooks = selectedRepoSource?.pipeline_config?.hooks;
    if (!hooks) return [];
    return (Object.entries(hooks) as Array<[string, any]>).flatMap(([state, config]) => {
      const entries: Array<{ state: string; phase: "on_enter" | "on_exit"; hook: string }> = [];
      for (const hook of config.on_enter ?? []) {
        entries.push({ state, phase: "on_enter", hook });
      }
      for (const hook of config.on_exit ?? []) {
        entries.push({ state, phase: "on_exit", hook });
      }
      return entries;
    }).sort((a, b) =>
      a.state.localeCompare(b.state)
      || a.phase.localeCompare(b.phase)
      || a.hook.localeCompare(b.hook),
    );
  }, [selectedRepoSource]);
  const pipelineHookNames = useMemo(
    () => Array.from(new Set(pipelineHookEntries.map((entry) => entry.hook))).sort((a, b) => a.localeCompare(b)),
    [pipelineHookEntries],
  );

  // Fetch per-agent pipeline stages when agent is selected
  useEffect(() => {
    if (!selectedAgentId || !selectedRepo) {
      setAgentPipelineStages([]);
      return;
    }
    let stale = false;
    api.getPipelineStagesForAgent(selectedRepo, selectedAgentId)
      .then((stages) => { if (!stale) setAgentPipelineStages(stages); })
      .catch(() => { if (!stale) setAgentPipelineStages([]); });
    return () => { stale = true; };
  }, [selectedAgentId, selectedRepo]);

  // Reset selected agent when repo changes
  useEffect(() => { setSelectedAgentId(null); }, [selectedRepo]);

  useEffect(() => {
    if (!selectedCardId) return;
    const card = cardsById.get(selectedCardId);
    if (!card || (selectedRepo && card.github_repo !== selectedRepo)) {
      setSelectedCardId(null);
    }
  }, [cardsById, selectedCardId, selectedRepo]);

  const filteredCards = useMemo(() => {
    return filterKanbanCards({
      agentMap,
      filters: {
        agentFilter,
        deptFilter,
        cardTypeFilter,
        signalStatusFilter,
        search,
        showClosed,
      },
      getAgentLabel,
      nowMs,
      repoCards,
      selectedAgentId,
      staleInProgressMs: staleInProgressMs,
    });
  }, [agentFilter, agentMap, cardTypeFilter, deptFilter, getAgentLabel, nowMs, signalStatusFilter, repoCards, search, selectedAgentId, showClosed]);

  const recentDoneCards = useMemo(() => {
    return repoCards
      .filter((c: KanbanCard) => {
        if (c.status !== "done") return false;
        if (c.parent_card_id) return false;
        if (cardTypeFilter === "issue" && isReviewCard(c)) return false;
        if (cardTypeFilter === "review" && !isReviewCard(c)) return false;
        return true;
      })
      .sort((a: KanbanCard, b: KanbanCard) => (b.completed_at ?? 0) - (a.completed_at ?? 0));
  }, [repoCards, cardTypeFilter]);

  useEffect(() => { setRecentDonePage(0); }, [selectedRepo]);

  // Compute dynamic columns: inject pipeline stage columns when an agent is selected
  const effectiveColumnDefs = useMemo(() => {
    if (!selectedAgentId || !agentPipelineStages.length) return BOARD_COLUMN_DEFS;
    const base = BOARD_COLUMN_DEFS.filter((c: any) => c.status !== "qa_pending" && c.status !== "qa_in_progress");
    const reviewPassStages = agentPipelineStages.filter((s: any) => s.trigger_after === "review_pass");
    if (reviewPassStages.length === 0) return base;
    const reviewIdx = base.findIndex((c) => c.status === "review");
    if (reviewIdx < 0) return base;
    const pipelineCols = reviewPassStages.map((s: any) => ({
      status: s.stage_name as KanbanCardStatus,
      labelKo: s.stage_name,
      labelEn: s.stage_name,
      accent: "#06b6d4",
    }));
    return [...base.slice(0, reviewIdx + 1), ...pipelineCols, ...base.slice(reviewIdx + 1)];
  }, [selectedAgentId, agentPipelineStages]);

  const cardsByStatus = useMemo(() => {
    const grouped = new Map<string, KanbanCard[]>();
    for (const column of effectiveColumnDefs) {
      grouped.set(column.status, []);
    }
    for (const card of filteredCards) {
      grouped.get(getBoardColumnStatus(card.status))?.push(card);
    }

    const isAncestor = (possibleAncestorId: string, card: KanbanCard): boolean => {
      let parentId = card.parent_card_id;
      let depthGuard = 0;
      while (parentId && depthGuard < 12) {
        if (parentId === possibleAncestorId) return true;
        parentId = repoCardsById.get(parentId)?.parent_card_id ?? null;
        depthGuard += 1;
      }
      return false;
    };

    const getRootCard = (card: KanbanCard): KanbanCard => {
      let current = card;
      let depthGuard = 0;
      while (current.parent_card_id && depthGuard < 12) {
        const parent = repoCardsById.get(current.parent_card_id);
        if (!parent) break;
        current = parent;
        depthGuard += 1;
      }
      return current;
    };

    for (const column of effectiveColumnDefs) {
      grouped.get(column.status)?.sort((a, b) => {
        if (isAncestor(a.id, b)) return -1;
        if (isAncestor(b.id, a)) return 1;

        const aRoot = getRootCard(a);
        const bRoot = getRootCard(b);
        if (aRoot.sort_order !== bRoot.sort_order) return aRoot.sort_order - bRoot.sort_order;
        if (aRoot.updated_at !== bRoot.updated_at) return bRoot.updated_at - aRoot.updated_at;

        if (a.parent_card_id !== b.parent_card_id) {
          if (!a.parent_card_id) return -1;
          if (!b.parent_card_id) return 1;
          if (a.parent_card_id < b.parent_card_id) return -1;
          if (a.parent_card_id > b.parent_card_id) return 1;
        }
        if (a.sort_order !== b.sort_order) return a.sort_order - b.sort_order;
        return b.updated_at - a.updated_at;
      });
    }
    return grouped;
  }, [effectiveColumnDefs, filteredCards, repoCardsById]);

  // Include ALL cards (including terminal) to prevent done issues
  // from reappearing in the backlog when the done column is hidden.
  const activeIssueNumbers = useMemo(() => {
    const set = new Set<number>();
    for (const card of repoCards) {
      if (card.github_issue_number) {
        set.add(card.github_issue_number);
      }
    }
    return set;
  }, [repoCards]);

  const backlogIssues = useMemo(() => {
    if (cardTypeFilter === "review") return []; // backlog issues are never review cards
    return issues.filter((issue: any) => !activeIssueNumbers.has(issue.number));
  }, [issues, activeIssueNumbers, cardTypeFilter]);

  const totalVisible = filteredCards.length + backlogIssues.length;
  const selectedRepoLabel = selectedRepo || tr("전체", "All");
  const selectedAgentScopeLabel = selectedAgentId
    ? (agents.find((a: any) => a.id === selectedAgentId)?.name ?? selectedAgentId)
    : tr("전체", "All");
  const deferredDodCount = filteredCards.filter((c: KanbanCard) => (c as any).dod_status === "deferred").length;
  const openCount = filteredCards.filter((card) => !TERMINAL_STATUSES.has(card.status)).length + backlogIssues.length;
  const reviewQueueCount = filteredCards.filter((card) => card.status === "review").length;
  const inProgressCount = filteredCards.filter((card) => card.status === "in_progress").length;
  const readyCount = filteredCards.filter((card) => getBoardColumnStatus(card.status) === "requested").length;
  const manualInterventionCount = filteredCards.filter((card) => isManualInterventionCard(card)).length;
  const hasQaCards = filteredCards.some((card) => card.status === "qa_pending" || card.status === "qa_in_progress");
  const hasFailedCards = filteredCards.some((card) => getBoardColumnStatus(card.status) === "failed");
  const boardColumns = useMemo(() => effectiveColumnDefs.filter((column) =>
    (showClosed || column.status !== "done")
    && (column.status !== "failed" || hasFailedCards)
    && ((column.status !== "qa_pending" && column.status !== "qa_in_progress") || hasQaCards),
  ), [effectiveColumnDefs, hasFailedCards, hasQaCards, showClosed]);
  const mobileColumnSummaries = useMemo(() => boardColumns.map((column) => {
    const columnCards = cardsByStatus.get(column.status) ?? [];
    return {
      column,
      count: column.status === "backlog" ? columnCards.length + backlogIssues.length : columnCards.length,
    };
  }), [backlogIssues.length, boardColumns, cardsByStatus]);
  const focusedMobileSummary = mobileColumnSummaries.find(
    ({ column }) => column.status === mobileColumnStatus,
  ) ?? mobileColumnSummaries[0] ?? null;
  const visibleColumns = boardColumns;



  return {
    backlogIssues,
    boardColumns,
    cardsByStatus,
    deferredDodCount,
    effectiveColumnDefs,
    filteredCards,
    focusedMobileSummary,
    inProgressCount,
    manualInterventionCount,
    mobileColumnSummaries,
    openCount,
    pipelineHookEntries,
    pipelineHookNames,
    readyCount,
    recentDoneCards,
    repoAgentEntries,
    repoCards,
    reviewQueueCount,
    selectedAgentScopeLabel,
    selectedRepoLabel,
    selectedRepoSource,
    totalVisible,
    visibleColumns,
  };
}
