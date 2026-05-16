import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import * as api from "../../api";
import type { DispatchDeliveryEvent, GitHubIssue, GitHubRepoOption, KanbanRepoSource } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { useLocalStorage } from "../../lib/useLocalStorage";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "../../app/breakpoints";
import {
  KANBAN_STATUS_TONES,
  TIMELINE_KIND_TONES,
  TIMELINE_STATUS_TONES,
} from "../../theme/statusTokens";
import AutoQueuePanel from "./AutoQueuePanel";
import BacklogIssueDetail from "./BacklogIssueDetail";
import CardTimeline from "./CardTimeline";
import MarkdownContent from "../common/MarkdownContent";
import KanbanAssignIssueModal from "./KanbanAssignIssueModal";
import KanbanBoardSurface from "./KanbanBoardSurface";
import KanbanCardDetail from "./KanbanCardDetail";
import KanbanColumn, { type KanbanColumnProps } from "./KanbanColumn";
import KanbanHeaderSurface from "./KanbanHeaderSurface";
import KanbanPipelineHooksCard from "./KanbanPipelineHooksCard";
import KanbanStatusModals from "./KanbanStatusModals";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "../common/SurfacePrimitives";
import type {
  Agent,
  Department,
  KanbanCard,
  KanbanCardMetadata,
  KanbanCardPriority,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import { localeName } from "../../i18n";
import {
  BOARD_COLUMN_DEFS,
  COLUMN_DEFS,
  EMPTY_EDITOR,
  PRIORITY_OPTIONS,
  QA_STATUSES,
  STATUS_TRANSITIONS,
  TERMINAL_STATUSES,
  TRANSITION_STYLE,
  buildGitHubIssueUrl,
  coerceEditor,
  createChecklistItem,
  formatIso,
  formatTs,
  getBoardColumnStatus,
  getCardDelayBadge,
  getCardDwellBadge,
  getCardMetadata,
  getChecklistSummary,
  hasManualInterventionReason,
  isManualInterventionCard,
  isReviewCard,
  labelForStatus,
  parseCardMetadata,
  parseIssueSections,
  priorityLabel,
  stringifyCardMetadata,
  type KanbanBoardColumnStatus,
  type EditorState,
} from "./kanban-utils";
import { formatAuditResult, formatDispatchSummary } from "./card-detail-activity";
import {
  filterKanbanCards,
  useKanbanFilterState,
  type KanbanCardTypeFilter,
  type KanbanSignalStatusFilter,
} from "./kanban-filter-state";
import { reviewDecisionMap, useKanbanCardActivity } from "./useKanbanCardActivity";
import {
  createDeliveryEventsLoadState,
  compactStringParts,
  deliveryEventMessagesCount,
  finishDeliveryEventsLoadError,
  finishDeliveryEventsLoadSuccess,
  getDeliveryEventStatusStyle,
  startDeliveryEventsLoad,
  summarizeDeliveryError,
  type DeliveryEventsLoadState,
} from "./dispatch-delivery-events";

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

const TIMELINE_KIND_STYLE: Record<string, { bg: string; text: string }> =
  TIMELINE_KIND_TONES;

const STALE_IN_PROGRESS_MS = 100 * 60_000;

const SURFACE_FIELD_STYLE = {
  background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
} as const;

const SURFACE_PANEL_STYLE = {
  background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
} as const;

const ACTIVITY_RESULT_TONE_STYLE = {
  default: {
    backgroundColor: "rgba(148,163,184,0.08)",
    borderColor: "rgba(148,163,184,0.16)",
    color: "var(--th-text-secondary)",
  },
  warn: {
    backgroundColor: "rgba(245,158,11,0.10)",
    borderColor: "rgba(245,158,11,0.24)",
    color: "#fbbf24",
  },
  danger: {
    backgroundColor: "rgba(248,113,113,0.10)",
    borderColor: "rgba(248,113,113,0.24)",
    color: "#fca5a5",
  },
} as const;

const SURFACE_CHIP_STYLE = {
  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
} as const;

const DELIVERY_EVENTS_POLL_MS = 5_000;
const SURFACE_GHOST_BUTTON_STYLE = {
  background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
  borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
} as const;

const SURFACE_MODAL_CARD_STYLE = {
  background:
    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
  borderColor: "color-mix(in srgb, var(--th-border) 72%, transparent)",
} as const;

const kanbanRepoSourcesQueryKey = ["kanban", "repo-sources"] as const;
const kanbanAvailableReposQueryKey = ["kanban", "available-repos"] as const;
const kanbanRepoIssuesQueryKey = (repo: string) =>
  ["kanban", "repo-issues", repo] as const;

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
  const queryClient = useQueryClient();
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
  const [newChecklistItem, setNewChecklistItem] = useState("");
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
  const [deliveryEventsState, setDeliveryEventsState] = useState<DeliveryEventsLoadState<DispatchDeliveryEvent>>(
    () => createDeliveryEventsLoadState(),
  );
  const [deliveryEventsPanelVisible, setDeliveryEventsPanelVisible] = useState(true);
  const [timelineFilter, setTimelineFilter] = useState<"review" | "pm" | "work" | "general" | null>(null);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [liveTurnsByAgentId, setLiveTurnsByAgentId] = useState<Record<string, api.AgentTurnState>>({});
  const deliveryEventsStateRef = useRef(deliveryEventsState);
  const deliveryEventsPanelRef = useRef<HTMLDivElement | null>(null);
  const commitDeliveryEventsState = useCallback((
    updater: (prev: DeliveryEventsLoadState<DispatchDeliveryEvent>) => DeliveryEventsLoadState<DispatchDeliveryEvent>,
  ) => {
    setDeliveryEventsState((prev) => {
      const next = updater(prev);
      deliveryEventsStateRef.current = next;
      return next;
    });
  }, []);
  const deliveryEvents = deliveryEventsState.events;
  const deliveryEventsLoading = deliveryEventsState.loading;
  const deliveryEventsError = deliveryEventsState.error;
  const repoSourcesQuery = useQuery({
    queryKey: kanbanRepoSourcesQueryKey,
    queryFn: () => api.getKanbanRepoSources(),
    staleTime: 60_000,
  });
  const availableReposQuery = useQuery({
    queryKey: kanbanAvailableReposQueryKey,
    queryFn: () => api.getGitHubRepos().then((result) => result.repos),
    staleTime: 5 * 60_000,
  });
  const repoIssuesQuery = useQuery({
    queryKey: selectedRepo
      ? kanbanRepoIssuesQueryKey(selectedRepo)
      : ["kanban", "repo-issues", "none"],
    queryFn: () => api.getGitHubIssues(selectedRepo, "open", 100),
    enabled: Boolean(selectedRepo),
    staleTime: 30_000,
  });
  const repoSources = repoSourcesQuery.data ?? [];
  const availableRepos = availableReposQuery.data ?? [];
  const issues = repoIssuesQuery.data?.issues ?? [];
  const loadingIssues = repoIssuesQuery.isFetching;
  const initialLoading = repoSourcesQuery.isLoading || availableReposQuery.isLoading;

  const agentMap = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);
  const cardsById = useMemo(() => new Map(cards.map((card) => [card.id, card])), [cards]);
  const dispatchMap = useMemo(() => new Map(dispatches.map((dispatch) => [dispatch.id, dispatch])), [dispatches]);

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
    setNewChecklistItem("");
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
    if (!selectedRepo && repoSources[0]?.repo) {
      setSelectedRepo(repoSources[0].repo);
      return;
    }
    if (selectedRepo && !repoSources.some((source) => source.repo === selectedRepo)) {
      setSelectedRepo(repoSources[0]?.repo ?? "");
    }
  }, [repoSources, selectedRepo]);

  useEffect(() => {
    if (repoIssuesQuery.error) {
      setActionError(repoIssuesQuery.error instanceof Error
        ? repoIssuesQuery.error.message
        : "Failed to load GitHub issues.");
      return;
    }
    if (repoIssuesQuery.data?.error) {
      setActionError(repoIssuesQuery.data.error);
    }
  }, [repoIssuesQuery.data?.error, repoIssuesQuery.error]);

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

  const getTimelineKindLabel = (kind: "review" | "pm" | "work" | "general") => {
    switch (kind) {
      case "review":
        return tr("리뷰", "Review");
      case "pm":
        return tr("PM 결정", "PM Decision");
      case "work":
        return tr("작업 이력", "Work Log");
      case "general":
        return tr("코멘트", "Comment");
    }
  };

  const getTimelineStatusLabel = (status: "reviewing" | "changes_requested" | "passed" | "decision" | "completed" | "comment") => {
    switch (status) {
      case "reviewing":
        return tr("진행 중", "In Progress");
      case "changes_requested":
        return tr("수정 필요", "Changes Requested");
      case "passed":
        return tr("통과", "Passed");
      case "decision":
        return tr("결정", "Decision");
      case "completed":
        return tr("완료", "Completed");
      case "comment":
        return tr("일반", "General");
    }
  };

  const getTimelineStatusStyle = (status: "reviewing" | "changes_requested" | "passed" | "decision" | "completed" | "comment") => {
    return TIMELINE_STATUS_TONES[status];
  };

  const repoCards = useMemo(() => {
    if (!selectedRepo) return [] as KanbanCard[];
    return cards.filter((card) => card.github_repo === selectedRepo);
  }, [cards, selectedRepo]);

  const repoCardsById = useMemo(() => new Map(repoCards.map((card) => [card.id, card])), [repoCards]);

  const childCardsByParentId = useMemo(() => {
    const grouped = new Map<string, KanbanCard[]>();
    for (const card of repoCards) {
      if (!card.parent_card_id) continue;
      const siblings = grouped.get(card.parent_card_id) ?? [];
      siblings.push(card);
      grouped.set(card.parent_card_id, siblings);
    }
    for (const siblings of grouped.values()) {
      siblings.sort((a, b) => {
        if (a.sort_order !== b.sort_order) return a.sort_order - b.sort_order;
        return b.updated_at - a.updated_at;
      });
    }
    return grouped;
  }, [repoCards]);

  const inProgressCardsByAgentId = useMemo(() => {
    const grouped = new Map<string, KanbanCard[]>();
    for (const card of repoCards) {
      if (card.status !== "in_progress" || !card.assignee_agent_id) continue;
      const agentCards = grouped.get(card.assignee_agent_id) ?? [];
      agentCards.push(card);
      grouped.set(card.assignee_agent_id, agentCards);
    }
    return grouped;
  }, [repoCards]);

  const liveTurnAgentIds = useMemo(
    () => Array.from(inProgressCardsByAgentId.keys()).sort(),
    [inProgressCardsByAgentId],
  );

  useEffect(() => {
    let disposed = false;
    let requestSeq = 0;
    let scheduledRefresh: number | null = null;

    const refreshLiveTurns = async () => {
      if (liveTurnAgentIds.length === 0) {
        setLiveTurnsByAgentId({});
        return;
      }

      const currentRequest = ++requestSeq;
      const results = await Promise.allSettled(
        liveTurnAgentIds.map((agentId) => api.getAgentTurn(agentId)),
      );

      if (disposed || currentRequest !== requestSeq) return;

      const next: Record<string, api.AgentTurnState> = {};
      results.forEach((result, index) => {
        if (result.status !== "fulfilled") return;
        const turn = result.value;
        if (turn.status === "idle") return;
        next[liveTurnAgentIds[index]!] = turn;
      });
      setLiveTurnsByAgentId(next);
    };

    const scheduleRefresh = (delayMs = 150) => {
      if (scheduledRefresh) window.clearTimeout(scheduledRefresh);
      scheduledRefresh = window.setTimeout(() => {
        scheduledRefresh = null;
        void refreshLiveTurns();
      }, delayMs);
    };

    const handleWSEvent = (event: Event) => {
      const detail = (event as CustomEvent<import("../../types").WSEvent>).detail;
      if (!detail) return;
      switch (detail.type) {
        case "connected":
        case "agent_status":
        case "dispatched_session_update":
        case "task_dispatch_created":
        case "task_dispatch_updated":
        case "kanban_card_created":
        case "kanban_card_updated":
          scheduleRefresh(detail.type === "dispatched_session_update" ? 500 : 150);
          break;
        default:
          break;
      }
    };

    void refreshLiveTurns();
    const pollTimer = window.setInterval(() => scheduleRefresh(0), LIVE_TURN_POLL_MS);
    window.addEventListener("pcd-ws-event", handleWSEvent as EventListener);

    return () => {
      disposed = true;
      requestSeq += 1;
      if (scheduledRefresh) window.clearTimeout(scheduledRefresh);
      window.clearInterval(pollTimer);
      window.removeEventListener("pcd-ws-event", handleWSEvent as EventListener);
    };
  }, [LIVE_TURN_POLL_MS, liveTurnAgentIds]);

  const liveToolStateByCardId = useMemo(() => {
    const mapped = new Map<string, { agentId: string; line: string; updatedAt?: string | null }>();
    for (const agentId of liveTurnAgentIds) {
      const turn = liveTurnsByAgentId[agentId];
      if (!turn) continue;
      const line = turn.current_tool_line?.trim() || turn.prev_tool_status?.trim();
      if (!line) continue;
      const agentCards = inProgressCardsByAgentId.get(agentId) ?? [];
      if (agentCards.length === 0) continue;

      if (turn.active_dispatch_id) {
        const matchedCard = agentCards.find((card) => card.latest_dispatch_id === turn.active_dispatch_id);
        if (matchedCard) {
          mapped.set(matchedCard.id, { agentId, line, updatedAt: turn.updated_at });
        }
        continue;
      }

      if (agentCards.length === 1) {
        mapped.set(agentCards[0]!.id, { agentId, line, updatedAt: turn.updated_at });
      }
    }
    return mapped;
  }, [inProgressCardsByAgentId, liveTurnAgentIds, liveTurnsByAgentId]);

  const selectedCardMetadata = selectedCard ? getCardMetadata(selectedCard) : null;
  const selectedCardChecklistSummary = selectedCard ? getChecklistSummary(selectedCard) : null;
  const selectedCardDelayBadge = selectedCard ? getCardDelayBadge(selectedCard, tr) : null;
  const selectedCardDwellBadge = selectedCard ? getCardDwellBadge(selectedCard, nowMs, tr) : null;
  const selectedCardGitHubIssueUrl = selectedCard
    ? buildGitHubIssueUrl(
        selectedCard.github_repo,
        selectedCard.github_issue_number,
        selectedCard.github_issue_url,
      )
    : null;
  const selectedParentCard = selectedCard?.parent_card_id
    ? repoCardsById.get(selectedCard.parent_card_id) ?? null
    : null;
  const selectedChildCards = selectedCard ? childCardsByParentId.get(selectedCard.id) ?? [] : [];
  const selectedLiveToolState = selectedCard ? liveToolStateByCardId.get(selectedCard.id) ?? null : null;
  const selectedLatestDispatch = selectedCard?.latest_dispatch_id
    ? dispatchMap.get(selectedCard.latest_dispatch_id) ?? null
    : null;
  const selectedDeliveryDispatchId = selectedLatestDispatch?.id ?? selectedCard?.latest_dispatch_id ?? null;

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
  const selectedCardAssigneeLabel = selectedCard?.assignee_agent_id
    ? getAgentLabel(selectedCard.assignee_agent_id)
    : tr("미할당", "Unassigned");
  const selectedCardTransitionTargets = selectedCard
    ? STATUS_TRANSITIONS[selectedCard.status] ?? []
    : [];
  const selectedCardHeroDescription = selectedCard
    ? [
        selectedCard.github_repo,
        selectedCard.github_issue_number ? `#${selectedCard.github_issue_number}` : null,
        selectedParentCard ? tr(`상위 ${selectedParentCard.title}`, `Parent ${selectedParentCard.title}`) : null,
      ].filter(Boolean).join(" · ")
    : "";
  const selectedRepoSource = useMemo(
    () => repoSources.find((source) => source.repo === selectedRepo) ?? null,
    [repoSources, selectedRepo],
  );
  const pipelineHookEntries = useMemo(() => {
    const hooks = selectedRepoSource?.pipeline_config?.hooks;
    if (!hooks) return [];
    return Object.entries(hooks).flatMap(([state, config]) => {
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

  useEffect(() => {
    if (!selectedDeliveryDispatchId) {
      setDeliveryEventsPanelVisible(true);
      return;
    }
    const node = deliveryEventsPanelRef.current;
    if (!node || typeof IntersectionObserver === "undefined") {
      setDeliveryEventsPanelVisible(true);
      return;
    }

    const observer = new IntersectionObserver(
      ([entry]) => setDeliveryEventsPanelVisible(Boolean(entry?.isIntersecting)),
      { threshold: 0.05 },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [selectedDeliveryDispatchId]);

  useEffect(() => {
    if (!selectedDeliveryDispatchId) {
      commitDeliveryEventsState(() => createDeliveryEventsLoadState());
      return;
    }
    if (!deliveryEventsPanelVisible) return;

    let stale = false;
    let pollTimer: number | null = null;
    const resetEvents = deliveryEventsStateRef.current.loadedDispatchId !== selectedDeliveryDispatchId;

    const loadDeliveryEvents = async (showLoading: boolean) => {
      if (showLoading) {
        commitDeliveryEventsState((prev) => (
          startDeliveryEventsLoad(prev, selectedDeliveryDispatchId, resetEvents)
        ));
      }
      try {
        const response = await api.getDispatchDeliveryEvents(selectedDeliveryDispatchId);
        if (stale) return;
        commitDeliveryEventsState((prev) => (
          finishDeliveryEventsLoadSuccess(prev, selectedDeliveryDispatchId, response.events)
        ));
      } catch (error) {
        if (stale) return;
        commitDeliveryEventsState((prev) => (
          finishDeliveryEventsLoadError(
            prev,
            (error as Error).message || "Failed to load delivery events",
            showLoading && resetEvents,
          )
        ));
      }
    };

    void loadDeliveryEvents(true);
    pollTimer = window.setInterval(() => void loadDeliveryEvents(false), DELIVERY_EVENTS_POLL_MS);

    return () => {
      stale = true;
      if (pollTimer !== null) window.clearInterval(pollTimer);
    };
  }, [activityRefreshTick, commitDeliveryEventsState, deliveryEventsPanelVisible, selectedDeliveryDispatchId]);

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
      staleInProgressMs: STALE_IN_PROGRESS_MS,
    });
  }, [agentFilter, agentMap, cardTypeFilter, deptFilter, getAgentLabel, nowMs, signalStatusFilter, repoCards, search, selectedAgentId, showClosed]);

  const recentDoneCards = useMemo(() => {
    return repoCards
      .filter((c) => {
        if (c.status !== "done") return false;
        if (c.parent_card_id) return false;
        if (cardTypeFilter === "issue" && isReviewCard(c)) return false;
        if (cardTypeFilter === "review" && !isReviewCard(c)) return false;
        return true;
      })
      .sort((a, b) => (b.completed_at ?? 0) - (a.completed_at ?? 0));
  }, [repoCards, cardTypeFilter]);

  useEffect(() => { setRecentDonePage(0); }, [selectedRepo]);

  // Compute dynamic columns: inject pipeline stage columns when an agent is selected
  const effectiveColumnDefs = useMemo(() => {
    if (!selectedAgentId || !agentPipelineStages.length) return BOARD_COLUMN_DEFS;
    const base = BOARD_COLUMN_DEFS.filter((c) => c.status !== "qa_pending" && c.status !== "qa_in_progress");
    const reviewPassStages = agentPipelineStages.filter((s) => s.trigger_after === "review_pass");
    if (reviewPassStages.length === 0) return base;
    const reviewIdx = base.findIndex((c) => c.status === "review");
    if (reviewIdx < 0) return base;
    const pipelineCols = reviewPassStages.map((s) => ({
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
    return issues.filter((issue) => !activeIssueNumbers.has(issue.number));
  }, [issues, activeIssueNumbers, cardTypeFilter]);

  const totalVisible = filteredCards.length + backlogIssues.length;
  const selectedRepoLabel = selectedRepo || tr("전체", "All");
  const selectedAgentScopeLabel = selectedAgentId
    ? (agents.find((a) => a.id === selectedAgentId)?.name ?? selectedAgentId)
    : tr("전체", "All");
  const deferredDodCount = filteredCards.filter((c) => (c as any).dod_status === "deferred").length;
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

  const canRetryCard = (card: KanbanCard | null) =>
    Boolean(card && ["blocked", "requested", "in_progress"].includes(card.status));

  const canRedispatchCard = (card: KanbanCard | null) =>
    Boolean(card && ["requested", "in_progress"].includes(card.status));

  const handleRedispatch = async () => {
    if (!selectedCard) return;
    setRedispatching(true);
    setActionError(null);
    try {
      await onRedispatchCard(selectedCard.id, {
        reason: redispatchReason.trim() || null,
      });
      invalidateCardActivity(selectedCard.id);
      setRedispatchReason("");
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("재디스패치에 실패했습니다.", "Failed to redispatch."));
    }
    setRedispatching(false);
  };

  const handleAddRepo = async () => {
    const repo = repoInput.trim();
    if (!repo) return;
    setRepoBusy(true);
    setActionError(null);
    try {
      const created = await api.addKanbanRepoSource(repo);
      queryClient.setQueryData<KanbanRepoSource[]>(kanbanRepoSourcesQueryKey, (prev = []) =>
        prev.some((source) => source.id === created.id) ? prev : [...prev, created],
      );
      setSelectedRepo(created.repo);
      setRepoInput("");
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("repo 추가에 실패했습니다.", "Failed to add repo."));
    } finally {
      setRepoBusy(false);
    }
  };

  const handleRemoveRepo = async (source: KanbanRepoSource) => {
    const confirmed = window.confirm(tr(
      `이 backlog source를 제거할까요? 저장된 카드 자체는 남습니다.\n${source.repo}`,
      `Remove this backlog source? Existing cards stay intact.\n${source.repo}`,
    ));
    if (!confirmed) return;
    setRepoBusy(true);
    setActionError(null);
    try {
      await api.deleteKanbanRepoSource(source.id);
      queryClient.setQueryData<KanbanRepoSource[]>(kanbanRepoSourcesQueryKey, (prev = []) =>
        prev.filter((item) => item.id !== source.id),
      );
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("repo 제거에 실패했습니다.", "Failed to remove repo."));
    } finally {
      setRepoBusy(false);
    }
  };

  const updateRepoDefaultAgent = (source: KanbanRepoSource, defaultAgentId: string | null) => {
    void api.updateKanbanRepoSource(source.id, { default_agent_id: defaultAgentId });
    queryClient.setQueryData<KanbanRepoSource[]>(kanbanRepoSourcesQueryKey, (prev = []) =>
      prev.map((item) => (
        item.id === source.id ? { ...item, default_agent_id: defaultAgentId } : item
      )),
    );
  };

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

  const handleSaveCard = async () => {
    if (!selectedCard) return;
    setSavingCard(true);
    setActionError(null);
    try {
      const metadata = {
        ...parseCardMetadata(selectedCard.metadata_json),
        review_checklist: editor.review_checklist
          .map((item, index) => ({
            id: item.id || `check-${index}`,
            label: item.label.trim(),
            done: item.done,
          }))
          .filter((item) => item.label),
      } satisfies KanbanCardMetadata;

      // Status is managed by quick-transition buttons, not by save.
      // Only send content fields here to avoid race conditions.
      await onUpdateCard(selectedCard.id, {
        title: editor.title.trim(),
        description: editor.description.trim() || null,
        assignee_agent_id: editor.assignee_agent_id || null,
        priority: editor.priority,
        metadata_json: stringifyCardMetadata(metadata),
      });
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 저장에 실패했습니다.", "Failed to save card."));
    } finally {
      setSavingCard(false);
    }
  };

  const handleRetryCard = async () => {
    if (!selectedCard) return;
    setRetryingCard(true);
    setActionError(null);
    try {
      await onRetryCard(selectedCard.id, {
        assignee_agent_id: retryAssigneeId || selectedCard.assignee_agent_id,
        request_now: true,
      });
      invalidateCardActivity(selectedCard.id);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("재시도에 실패했습니다.", "Failed to retry card."));
    } finally {
      setRetryingCard(false);
    }
  };

  const addChecklistItem = () => {
    const label = newChecklistItem.trim();
    if (!label) return;
    setEditor((prev) => ({
      ...prev,
      review_checklist: [...prev.review_checklist, createChecklistItem(label, prev.review_checklist.length)],
    }));
    setNewChecklistItem("");
  };

  const handleDeleteCard = async () => {
    if (!selectedCard) return;
    const confirmed = window.confirm(tr("이 카드를 삭제할까요?", "Delete this card?"));
    if (!confirmed) return;
    setSavingCard(true);
    setActionError(null);
    try {
      await onDeleteCard(selectedCard.id);
      setSelectedCardId(null);
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 삭제에 실패했습니다.", "Failed to delete card."));
    } finally {
      setSavingCard(false);
    }
  };

  const handleCloseIssue = async (issue: GitHubIssue) => {
    if (!selectedRepo) return;
    setClosingIssueNumber(issue.number);
    setActionError(null);
    try {
      await api.closeGitHubIssue(selectedRepo, issue.number);
      queryClient.setQueryData<Awaited<ReturnType<typeof api.getGitHubIssues>>>(
        kanbanRepoIssuesQueryKey(selectedRepo),
        (prev) => prev ? {
          ...prev,
          issues: prev.issues.filter((item) => item.number !== issue.number),
        } : prev,
      );
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("이슈 닫기에 실패했습니다.", "Failed to close issue."));
    } finally {
      setClosingIssueNumber(null);
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
    <div
      data-testid="kanban-page"
      className="mx-auto w-full max-w-6xl min-w-0 space-y-4 overflow-x-hidden pb-24 md:pb-0"
      style={{ paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))" }}
    >
      <KanbanHeaderSurface
        ctx={{
          activeFilterCount,
          actionError,
          advancedFilterDirty,
          advancedFiltersOpen,
          advancedFiltersRef,
          agentFilter,
          agentPipelineStages,
          agents,
          availableRepos,
          bulkBusy,
          cards,
          cardTypeFilter,
          deferredDodCount,
          departments,
          deptFilter,
          getAgentLabel,
          handleAddRepo,
          handleBulkAction,
          handleRemoveRepo,
          headerOpen,
          initialLoading,
          locale,
          onPatchDeferDod,
          openCount,
          repoAgentEntries,
          repoBusy,
          repoCards,
          repoInput,
          repoSources,
          resetAdvancedFilters,
          scopeOpen,
          search,
          selectedAgentId,
          selectedAgentScopeLabel,
          selectedRepo,
          selectedRepoLabel,
          selectedRepoSource,
          setActionError,
          setAdvancedFiltersOpen,
          setAgentFilter,
          setCardTypeFilter,
          setDeferredDodPopup,
          setDeptFilter,
          setHeaderOpen,
          setRepoInput,
          setScopeOpen,
          setSearch,
          setSelectedAgentId,
          setSelectedRepo,
          setSettingsOpen,
          setShowClosed,
          setSignalStatusFilter,
          setStalledPopup,
          setStalledSelected,
          setVerifyingDeferredDodIds,
          settingsOpen,
          showClosed,
          signalFilterLabel,
          signalStatusFilter,
          stalledCards,
          stalledPopup,
          stalledSelected,
          SURFACE_CHIP_STYLE,
          SURFACE_FIELD_STYLE,
          SURFACE_PANEL_STYLE,
          totalVisible,
          tr,
          updateRepoDefaultAgent,
          verifyingDeferredDodIds,
        }}
      />

      <KanbanStatusModals
        ctx={{
          agents,
          assignBeforeReady,
          cancelBusy,
          cancelConfirm,
          cardsById,
          executeBulkCancel,
          invalidateCardActivity,
          onUpdateCard,
          setActionError,
          setAssignBeforeReady,
          setCancelConfirm,
          SURFACE_FIELD_STYLE,
          tr,
        }}
      />

      <KanbanBoardSurface
        ctx={{
          agents,
          assigningIssue,
          backlogIssues,
          cardsByStatus,
          closingIssueNumber,
          compactBoard,
          focusMobileColumn,
          focusedMobileSummary,
          getAgentLabel,
          getAgentProvider,
          handleBacklogIssueOpen,
          handleCardOpen,
          handleCloseIssue,
          handleDirectAssignIssue,
          handleOpenAssignModal,
          handleUpdateCardStatus,
          initialLoading,
          loadingIssues,
          locale,
          mobileColumnStatus,
          mobileColumnSummaries,
          recentDoneCards,
          recentDoneOpen,
          recentDonePage,
          resolveAgentFromLabels,
          selectedAgentId,
          selectedRepo,
          setActionError,
          setRecentDoneOpen,
          setRecentDonePage,
          setSelectedCardId,
          tr,
          visibleColumns,
        }}
      />

        {selectedCard && (
          <KanbanCardDetail
            card={selectedCard}
            tr={tr}
            locale={locale}
            agents={agents}
            dispatches={dispatches}
            editor={editor}
            setEditor={setEditor}
            savingCard={savingCard}
            setSavingCard={setSavingCard}
            retryingCard={retryingCard}
            setRetryingCard={setRetryingCard}
            redispatching={redispatching}
            setRedispatching={setRedispatching}
            redispatchReason={redispatchReason}
            setRedispatchReason={setRedispatchReason}
            retryAssigneeId={retryAssigneeId}
            setRetryAssigneeId={setRetryAssigneeId}
            actionError={actionError}
            setActionError={setActionError}
            auditLog={auditLog}
            ghComments={ghComments}
            reviewData={reviewData}
            setReviewData={() => {
              clearCardReviews(selectedCard.id);
            }}
            reviewDecisions={reviewDecisions}
            setReviewDecisions={setReviewDecisions}
            timelineFilter={timelineFilter}
            setTimelineFilter={setTimelineFilter}
            setCancelConfirm={setCancelConfirm}
            onClose={() => setSelectedCardId(null)}
            onUpdateCard={onUpdateCard}
            onRetryCard={onRetryCard}
            onRedispatchCard={onRedispatchCard}
            onDeleteCard={onDeleteCard}
            invalidateCardActivity={invalidateCardActivity}
          />
        )}

      <KanbanPipelineHooksCard
        ctx={{
          pipelineHookEntries,
          pipelineHookNames,
          selectedRepo,
          SURFACE_CHIP_STYLE,
          tr,
        }}
      />

      {/* #1253 (revised): "최근 완료" is now rendered right above the kanban
          columns inside the board container, so completion history sits
          next to the active board rather than below it. */}

      <KanbanAssignIssueModal
        ctx={{
          agents,
          assignAssigneeId,
          assignIssue,
          assigningIssue,
          getAgentLabel,
          handleAssignIssue,
          selectedRepo,
          setAssignAssigneeId,
          setAssignIssue,
          SURFACE_CHIP_STYLE,
          SURFACE_FIELD_STYLE,
          SURFACE_GHOST_BUTTON_STYLE,
          SURFACE_MODAL_CARD_STYLE,
          SURFACE_PANEL_STYLE,
          tr,
        }}
      />

      {selectedBacklogIssue && (
        <BacklogIssueDetail
          issue={selectedBacklogIssue}
          tr={tr}
          locale={locale}
          closingIssueNumber={closingIssueNumber}
          onClose={() => setSelectedBacklogIssue(null)}
          onCloseIssue={handleCloseIssue}
          onAssign={(issue) => {
            setAssignIssue(issue);
            const repoSource = repoSources.find((source) => source.repo === selectedRepo);
            setAssignAssigneeId(repoSource?.default_agent_id ?? "");
          }}
        />
      )}
    </div>
  );
}
