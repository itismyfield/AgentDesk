import { useEffect, useMemo, useRef, useState } from "react";
import * as api from "../../api";
import type { GitHubIssue } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { useLocalStorage } from "../../lib/useLocalStorage";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "../../app/breakpoints";
import KanbanTabView from "./KanbanTabView";
import type {
  Agent,
  Department,
  KanbanCard,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import { localeName } from "../../i18n";
import {
  EMPTY_EDITOR,
  coerceEditor,
  getBoardColumnStatus,
  hasManualInterventionReason,
  type KanbanBoardColumnStatus,
  type EditorState,
} from "./kanban-utils";
import { useKanbanFilterState } from "./kanban-filter-state";
import { useKanbanBoardModel } from "./useKanbanBoardModel";
import { useKanbanRepoData } from "./useKanbanRepoData";
import { reviewDecisionMap, useKanbanCardActivity } from "./useKanbanCardActivity";

interface KanbanTabProps {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  cards: KanbanCard[];
  dispatches: TaskDispatch[];
  agents: Agent[];
  departments: Department[];
  onAssignIssue: (payload: {
    github_repo: string;
    github_issue_number: number;
    github_issue_url?: string | null;
    title: string;
    description?: string | null;
    assignee_agent_id: string;
  }) => Promise<void>;
  onUpdateCard: (
    id: string,
    patch: Partial<KanbanCard> & { before_card_id?: string | null },
  ) => Promise<void>;
  onRetryCard: (
    id: string,
    payload?: { assignee_agent_id?: string | null; request_now?: boolean },
  ) => Promise<void>;
  onRedispatchCard: (
    id: string,
    payload?: { reason?: string | null },
  ) => Promise<void>;
  onDeleteCard: (id: string) => Promise<void>;
  onPatchDeferDod: (
    id: string,
    payload: Parameters<typeof api.patchKanbanDeferDod>[1],
  ) => Promise<void>;
  externalStatusFocus?: "review" | "blocked" | "requested" | "stalled" | null;
  onClearSignalFocus?: () => void;
}

const STALE_IN_PROGRESS_MS = 100 * 60_000;

const SURFACE_FIELD_STYLE = {
  background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
} as const;

const SURFACE_PANEL_STYLE = {
  background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
} as const;

const SURFACE_CHIP_STYLE = {
  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
} as const;

const SURFACE_GHOST_BUTTON_STYLE = {
  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
} as const;

const SURFACE_MODAL_CARD_STYLE = {
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
} as const;

export default function KanbanTab({
  tr,
  locale,
  cards,
  dispatches,
  agents,
  departments,
  onAssignIssue,
  onUpdateCard,
  onRetryCard,
  onRedispatchCard,
  onDeleteCard,
  onPatchDeferDod,
  externalStatusFocus,
  onClearSignalFocus,
}: KanbanTabProps) {
  const LIVE_TURN_POLL_MS = 4_000;
  const [repoInput, setRepoInput] = useState("");
  const [selectedRepo, setSelectedRepo] = useState("");
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [agentPipelineStages, setAgentPipelineStages] = useState<import("../../types").PipelineStage[]>([]);
  const {
    activeFilterCount,
    advancedFilterDirty,
    agentFilter,
    cardTypeFilter,
    deptFilter,
    resetAdvancedFilters,
    search,
    setAgentFilter,
    setCardTypeFilter,
    setDeptFilter,
    setSearch,
    setShowClosed,
    setSignalStatusFilter,
    showClosed,
    signalStatusFilter,
  } = useKanbanFilterState();
  const [storedSelectedCardId, setSelectedCardId] = useLocalStorage<string | null>(STORAGE_KEYS.kanbanDrawerLastId, null);
  const [editor, setEditor] = useState<EditorState>(EMPTY_EDITOR);
  const [assignIssue, setAssignIssue] = useState<GitHubIssue | null>(null);
  const [assignAssigneeId, setAssignAssigneeId] = useState("");
  const [savingCard, setSavingCard] = useState(false);
  const [retryingCard, setRetryingCard] = useState(false);
  const [redispatching, setRedispatching] = useState(false);
  const [redispatchReason, setRedispatchReason] = useState("");
  const [assigningIssue, setAssigningIssue] = useState(false);
  const [repoBusy, setRepoBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [compactBoard, setCompactBoard] = useState(false);
  const [mobileColumnStatus, setMobileColumnStatus] = useState<KanbanCardStatus | KanbanBoardColumnStatus>("backlog");
  const [retryAssigneeId, setRetryAssigneeId] = useState("");
  const [closingIssueNumber, setClosingIssueNumber] = useState<number | null>(null);
  const [selectedBacklogIssue, setSelectedBacklogIssue] = useState<GitHubIssue | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [scopeOpen, setScopeOpen] = useLocalStorage<boolean>(STORAGE_KEYS.kanbanScopeOpen, true);
  const [headerOpen, setHeaderOpen] = useLocalStorage<boolean>(STORAGE_KEYS.kanbanHeaderOpen, false);
  const [advancedFiltersOpen, setAdvancedFiltersOpen] = useState(false);
  const advancedFiltersRef = useRef<HTMLDivElement | null>(null);
  const [reviewDecisions, setReviewDecisions] = useState<Record<string, "accept" | "reject">>({});
  const [reviewBusy, setReviewBusy] = useState(false);
  const [recentDonePage, setRecentDonePage] = useState(0);
  const [recentDoneOpen, setRecentDoneOpen] = useState(false);
  const [stalledPopup, setStalledPopup] = useState(false);
  const [stalledSelected, setStalledSelected] = useState<Set<string>>(new Set());
  const [bulkBusy, setBulkBusy] = useState(false);
  const [deferredDodPopup, setDeferredDodPopup] = useState(false);
  const [verifyingDeferredDodIds, setVerifyingDeferredDodIds] = useState<Set<string>>(new Set());
  const [assignBeforeReady, setAssignBeforeReady] = useState<{ cardId: string; agentId: string } | null>(null);
  const [cancelConfirm, setCancelConfirm] = useState<{ cardIds: string[]; source: "bulk" | "single" } | null>(null);
  const [cancelBusy, setCancelBusy] = useState(false);
  const [timelineFilter, setTimelineFilter] = useState<"review" | "pm" | "work" | "general" | null>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const {
    availableRepos,
    handleAddRepo,
    handleCloseIssue,
    handleRemoveRepo,
    initialLoading,
    issues,
    loadingIssues,
    repoSources,
    updateRepoDefaultAgent,
  } = useKanbanRepoData({
    repoInput,
    selectedRepo,
    setActionError,
    setClosingIssueNumber,
    setRepoBusy,
    setRepoInput,
    setSelectedRepo,
    tr,
  });

  const agentMap = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);
  const cardsById = useMemo(() => new Map(cards.map((card) => [card.id, card])), [cards]);

  /** Resolve agent from `agent:*` GitHub labels by matching role_id. */
  const resolveAgentFromLabels = useMemo(() => {
    const roleIdMap = new Map<string, Agent>();
    const suffixMap = new Map<string, Agent>();
    for (const agent of agents) {
      // Use agent.id as primary key (role_id may be null from API)
      const key = agent.role_id || agent.id;
      if (key) {
        roleIdMap.set(key, agent);
        // Also map by agent.id if different from role_id
        if (agent.id && agent.id !== key) roleIdMap.set(agent.id, agent);
        // Also map the suffix after last hyphen (e.g. "ch-dd" → "dd")
        const lastDash = key.lastIndexOf("-");
        if (lastDash >= 0) {
          const suffix = key.slice(lastDash + 1);
          if (!suffixMap.has(suffix)) suffixMap.set(suffix, agent);
        }
      }
    }
    return (labels: Array<{ name: string; color: string }>): Agent | null => {
      for (const label of labels) {
        if (label.name.startsWith("agent:")) {
          const roleId = label.name.slice("agent:".length).trim();
          const matched = roleIdMap.get(roleId) ?? suffixMap.get(roleId);
          if (matched) return matched;
        }
      }
      return null;
    };
  }, [agents]);

  const selectedCardId = typeof storedSelectedCardId === "string" ? storedSelectedCardId : null;
  const selectedCard = selectedCardId ? cardsById.get(selectedCardId) ?? null : null;
  const {
    activityRefreshTick,
    auditLog,
    clearCardReviews,
    ghComments,
    githubIssueBody,
    invalidateCardActivity,
    reviewData,
  } = useKanbanCardActivity({ selectedCard, selectedCardId });

  const STALLED_REVIEW_STATUSES = new Set(["awaiting_dod", "suggestion_pending", "dilemma_pending", "reviewing"]);
  const stalledCards = useMemo(
    () => cards.filter((c) => c.status === "review" && c.review_status && STALLED_REVIEW_STATUSES.has(c.review_status)),
    [cards],
  );

  const handleBulkAction = async (action: "pass" | "reset" | "cancel") => {
    if (stalledSelected.size === 0) return;
    if (action === "cancel") {
      // Show confirmation modal for cancel — check if any selected cards have GitHub issues
      setCancelConfirm({ cardIds: Array.from(stalledSelected), source: "bulk" });
      return;
    }
    setBulkBusy(true);
    try {
      await api.bulkKanbanAction(action, Array.from(stalledSelected));
      setStalledSelected(new Set());
      setStalledPopup(false);
    } catch (e) {
      setActionError((e as Error).message);
    } finally {
      setBulkBusy(false);
    }
  };

  const executeBulkCancel = async () => {
    if (!cancelConfirm) return;
    setCancelBusy(true);
    try {
      // Both bulk and single cancel use bulkKanbanAction which calls
      // transition_status with force=true, avoiding blocked transitions.
      // GitHub issues are automatically closed server-side when status → done.
      await api.bulkKanbanAction("cancel", cancelConfirm.cardIds);
      cancelConfirm.cardIds.forEach((cardId) => invalidateCardActivity(cardId));
      if (cancelConfirm.source === "bulk") {
        setStalledSelected(new Set());
        setStalledPopup(false);
      } else {
        setSelectedCardId(null);
      }
      setCancelConfirm(null);
    } catch (e) {
      setActionError((e as Error).message);
    } finally {
      setCancelBusy(false);
    }
  };

  useEffect(() => {
    setEditor(coerceEditor(selectedCard));
    setRetryAssigneeId(selectedCard?.assignee_agent_id ?? "");
    setReviewDecisions({});
    setTimelineFilter(null);
  }, [selectedCard]);

  useEffect(() => {
    if (!selectedCard || githubIssueBody == null) return;
    setEditor((prev) => ({ ...prev, description: githubIssueBody }));
  }, [githubIssueBody, selectedCard?.id]);

  useEffect(() => {
    setReviewDecisions(reviewDecisionMap(reviewData));
  }, [reviewData?.id]);

  useEffect(() => {
    const media = window.matchMedia(MOBILE_LAYOUT_MEDIA_QUERY);
    const apply = () => setCompactBoard(media.matches);
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, []);

  useEffect(() => {
    const timer = window.setInterval(() => setNowMs(Date.now()), 30_000);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    if (!advancedFiltersOpen) return;
    const handle = (event: MouseEvent) => {
      if (!advancedFiltersRef.current) return;
      if (!advancedFiltersRef.current.contains(event.target as Node)) {
        setAdvancedFiltersOpen(false);
      }
    };
    window.addEventListener("mousedown", handle);
    return () => window.removeEventListener("mousedown", handle);
  }, [advancedFiltersOpen]);

  useEffect(() => {
    if (!showClosed && mobileColumnStatus === "done") {
      setMobileColumnStatus("backlog");
    }
  }, [mobileColumnStatus, showClosed]);

  useEffect(() => {
    if (!externalStatusFocus) return;
    setSettingsOpen(true);
    setSignalStatusFilter(externalStatusFocus);
    if (externalStatusFocus === "review") {
      setCardTypeFilter("review");
      setMobileColumnStatus("review");
    } else if (externalStatusFocus === "blocked") {
      const focusStatus =
        cards.find((card) => card.review_status === "dilemma_pending")?.status
        ?? cards.find((card) => hasManualInterventionReason(card) && card.status === "requested")?.status
        ?? cards.find((card) => card.status === "qa_failed")?.status
        ?? cards.find((card) => hasManualInterventionReason(card) && card.status === "in_progress")?.status
        ?? "in_progress";
      setMobileColumnStatus(
        focusStatus === "review" ? "review" : getBoardColumnStatus(focusStatus),
      );
    } else if (externalStatusFocus === "requested") {
      setMobileColumnStatus("requested");
    } else {
      setMobileColumnStatus("in_progress");
    }
    onClearSignalFocus?.();
  }, [cards, externalStatusFocus, onClearSignalFocus]);

  const getAgentLabel = (agentId: string | null | undefined) => {
    if (!agentId) return tr("미할당", "Unassigned");
    const agent = agentMap.get(agentId);
    if (!agent) return agentId;
    return localeName(locale, agent);
  };

  const getAgentProvider = (agentId: string | null | undefined) => {
    if (!agentId) return null;
    return agentMap.get(agentId)?.cli_provider ?? null;
  };

  const {
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
  } = useKanbanBoardModel({
    agentFilter,
    agentMap,
    agents,
    agentPipelineStages,
    cardTypeFilter,
    cards,
    cardsById,
    deptFilter,
    getAgentLabel,
    issues,
    mobileColumnStatus,
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
    staleInProgressMs: STALE_IN_PROGRESS_MS,
    tr,
  });

  /** Assign a backlog issue directly (auto-assign from agent:* label). */
  const handleDirectAssignIssue = async (issue: GitHubIssue, agentId: string) => {
    if (!selectedRepo) return;
    setAssigningIssue(true);
    setActionError(null);
    try {
      await onAssignIssue({
        github_repo: selectedRepo,
        github_issue_number: issue.number,
        github_issue_url: issue.url,
        title: issue.title,
        description: issue.body || null,
        assignee_agent_id: agentId,
      });
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("이슈 할당에 실패했습니다.", "Failed to assign issue."));
    } finally {
      setAssigningIssue(false);
    }
  };

  const handleUpdateCardStatus = async (cardId: string, targetStatus: KanbanCardStatus) => {
    setActionError(null);
    // When moving to "ready" without an assignee, show assignee selection modal
    if (targetStatus === "ready") {
      const card = cardsById.get(cardId);
      if (card && !card.assignee_agent_id) {
        setAssignBeforeReady({ cardId, agentId: "" });
        return;
      }
    }
    try {
      if (targetStatus === "requested") {
        // requested 전환은 POST /api/dispatches로만 가능
        const card = cardsById.get(cardId);
        await api.createDispatch({
          kanban_card_id: cardId,
          to_agent_id: card?.assignee_agent_id ?? "",
          title: card?.title ?? "Dispatch",
        });
        window.location.reload();
      } else {
        await onUpdateCard(cardId, { status: targetStatus });
        invalidateCardActivity(cardId);
      }
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
    }
  };

  const handleAssignIssue = async () => {
    if (!assignIssue || !selectedRepo || !assignAssigneeId) return;
    setAssigningIssue(true);
    setActionError(null);
    try {
      await onAssignIssue({
        github_repo: selectedRepo,
        github_issue_number: assignIssue.number,
        github_issue_url: assignIssue.url,
        title: assignIssue.title,
        description: assignIssue.body || null,
        assignee_agent_id: assignAssigneeId,
      });
      setAssignIssue(null);
      setAssignAssigneeId("");
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("issue 할당에 실패했습니다.", "Failed to assign issue."));
    } finally {
      setAssigningIssue(false);
    }
  };

  const handleOpenAssignModal = (issue: GitHubIssue) => {
    setAssignIssue(issue);
    const repoSource = repoSources.find((s) => s.repo === selectedRepo);
    setAssignAssigneeId(repoSource?.default_agent_id ?? "");
  };

  useEffect(() => {
    const fallbackStatus = boardColumns[0]?.status ?? "backlog";
    if (!boardColumns.some((column) => column.status === mobileColumnStatus)) {
      setMobileColumnStatus(fallbackStatus);
    }
  }, [boardColumns, mobileColumnStatus]);

  const focusMobileColumn = (status: KanbanCardStatus | KanbanBoardColumnStatus, scrollToSection: boolean) => {
    setMobileColumnStatus(status);
    if (!scrollToSection || typeof document === "undefined") return;
    window.requestAnimationFrame(() => {
      document
        .getElementById(`kanban-mobile-${status}`)
        ?.scrollIntoView({ behavior: "smooth", block: "nearest", inline: "start" });
    });
  };
  const handleCardOpen = (cardId: string) => {
    const card = cardsById.get(cardId);
    if (card) {
      setMobileColumnStatus(getBoardColumnStatus(card.status));
    }
    setSelectedBacklogIssue(null);
    setSelectedCardId(cardId);
  };
  const handleBacklogIssueOpen = (issue: GitHubIssue) => {
    setMobileColumnStatus("backlog");
    setSelectedCardId(null);
    setSelectedBacklogIssue(issue);
  };
  const signalFilterLabel =
    signalStatusFilter === "review" ? tr("리뷰 대기", "Review queue")
      : signalStatusFilter === "blocked" ? tr("수동 개입", "Manual intervention")
        : signalStatusFilter === "requested" ? tr("준비됨", "Ready")
          : signalStatusFilter === "stalled" ? tr("진행 정체", "Stale in progress")
            : null;

  return (
    <KanbanTabView
      ctx={{
        activeFilterCount, actionError, advancedFilterDirty, advancedFiltersOpen, advancedFiltersRef, agentFilter, agentPipelineStages, agents, assignAssigneeId, assignBeforeReady, assignIssue, assigningIssue, auditLog, availableRepos, backlogIssues, boardColumns, bulkBusy, cancelBusy, cancelConfirm, cardTypeFilter, cards, cardsById, cardsByStatus, clearCardReviews, closingIssueNumber, compactBoard, deferredDodCount, departments, deptFilter, dispatches, editor, executeBulkCancel, filteredCards, focusMobileColumn, focusedMobileSummary, getAgentLabel, getAgentProvider, ghComments, handleAddRepo, handleAssignIssue, handleBacklogIssueOpen, handleBulkAction, handleCardOpen, handleCloseIssue, handleDirectAssignIssue, handleOpenAssignModal, handleRemoveRepo, handleUpdateCardStatus, headerOpen, initialLoading, invalidateCardActivity, loadingIssues, locale, mobileColumnStatus, mobileColumnSummaries, onDeleteCard, onPatchDeferDod, onRedispatchCard, onRetryCard, onUpdateCard, openCount, pipelineHookEntries, pipelineHookNames, readyCount, recentDoneCards, recentDoneOpen, recentDonePage, repoAgentEntries, repoBusy, repoCards, repoInput, repoSources, resetAdvancedFilters, resolveAgentFromLabels, reviewData, reviewDecisions, reviewQueueCount, scopeOpen, search, selectedAgentId, selectedAgentScopeLabel, selectedBacklogIssue, selectedCard, selectedRepo, selectedRepoLabel, selectedRepoSource, setActionError, setAdvancedFiltersOpen, setAgentFilter, setAssignAssigneeId, setAssignBeforeReady, setAssignIssue, setCancelConfirm, setCardTypeFilter, setDeferredDodPopup, setDeptFilter, setEditor, setHeaderOpen, setRecentDoneOpen, setRecentDonePage, setRedispatchReason, setRepoInput, setRetryAssigneeId, setRetryingCard, setReviewDecisions, setScopeOpen, setSearch, setSelectedAgentId, setSelectedBacklogIssue, setSelectedCardId, setSelectedRepo, setSettingsOpen, setShowClosed, setSignalStatusFilter, setStalledPopup, setStalledSelected, setTimelineFilter, setVerifyingDeferredDodIds, settingsOpen, showClosed, signalFilterLabel, signalStatusFilter, stalledCards, stalledPopup, stalledSelected, SURFACE_CHIP_STYLE, SURFACE_FIELD_STYLE, SURFACE_GHOST_BUTTON_STYLE, SURFACE_MODAL_CARD_STYLE, SURFACE_PANEL_STYLE, timelineFilter, totalVisible, tr, updateRepoDefaultAgent, verifyingDeferredDodIds, visibleColumns, redispatching, redispatchReason, retryAssigneeId, retryingCard, savingCard, setRedispatching, setSavingCard,
      }}
    />
  );
}
