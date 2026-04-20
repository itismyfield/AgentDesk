import { useEffect, useMemo, useRef, useState } from "react";
import * as api from "../../api";
import type { GitHubIssue, GitHubRepoOption, KanbanRepoSource } from "../../api";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import { useLocalStorage } from "../../lib/useLocalStorage";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "../../app/breakpoints";
import AutoQueuePanel from "./AutoQueuePanel";
import PipelineVisualEditor from "./PipelineVisualEditor";
import CardTimeline from "./CardTimeline";
import MarkdownContent from "../common/MarkdownContent";
import KanbanColumn from "./KanbanColumn";
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSegmentButton,
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
import type { KanbanReview } from "../../api";
import { localeName } from "../../i18n";
import {
  COLUMN_DEFS,
  EMPTY_EDITOR,
  PRIORITY_OPTIONS,
  QA_STATUSES,
  STATUS_TRANSITIONS,
  TERMINAL_STATUSES,
  TRANSITION_STYLE,
  coerceEditor,
  createChecklistItem,
  formatIso,
  formatTs,
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
  type EditorState,
} from "./kanban-utils";
import {
  formatAuditResult,
  formatDispatchSummary,
} from "./card-detail-activity";

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

const TIMELINE_KIND_STYLE: Record<string, { bg: string; text: string }> = {
  review: { bg: "rgba(20,184,166,0.16)", text: "#5eead4" },
  pm: { bg: "rgba(249,115,22,0.16)", text: "#fdba74" },
  work: { bg: "rgba(96,165,250,0.16)", text: "#93c5fd" },
  general: { bg: "rgba(148,163,184,0.10)", text: "#94a3b8" },
};

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
  const [repoSources, setRepoSources] = useState<KanbanRepoSource[]>([]);
  const [repoInput, setRepoInput] = useState("");
  const [selectedRepo, setSelectedRepo] = useState("");
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [agentPipelineStages, setAgentPipelineStages] = useState<import("../../types").PipelineStage[]>([]);
  const [availableRepos, setAvailableRepos] = useState<GitHubRepoOption[]>([]);
  const [issues, setIssues] = useState<GitHubIssue[]>([]);
  const [agentFilter, setAgentFilter] = useState("all");
  const [deptFilter, setDeptFilter] = useState("all");
  const [cardTypeFilter, setCardTypeFilter] = useState<"all" | "issue" | "review">("all");
  const [signalStatusFilter, setSignalStatusFilter] = useState<"all" | "review" | "blocked" | "requested" | "stalled">("all");
  const [search, setSearch] = useState("");
  const [showClosed, setShowClosed] = useState(false);
  const [storedSelectedCardId, setSelectedCardId] = useLocalStorage<string | null>(STORAGE_KEYS.kanbanDrawerLastId, null);
  const [editor, setEditor] = useState<EditorState>(EMPTY_EDITOR);
  const [assignIssue, setAssignIssue] = useState<GitHubIssue | null>(null);
  const [assignAssigneeId, setAssignAssigneeId] = useState("");
  const [loadingIssues, setLoadingIssues] = useState(false);
  const [initialLoading, setInitialLoading] = useState(true);
  const [savingCard, setSavingCard] = useState(false);
  const [retryingCard, setRetryingCard] = useState(false);
  const [redispatching, setRedispatching] = useState(false);
  const [redispatchReason, setRedispatchReason] = useState("");
  const [assigningIssue, setAssigningIssue] = useState(false);
  const [repoBusy, setRepoBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [compactBoard, setCompactBoard] = useState(false);
  const [mobileColumnStatus, setMobileColumnStatus] = useState<KanbanCardStatus>("backlog");
  const [retryAssigneeId, setRetryAssigneeId] = useState("");
  const [newChecklistItem, setNewChecklistItem] = useState("");
  const [closingIssueNumber, setClosingIssueNumber] = useState<number | null>(null);
  const [selectedBacklogIssue, setSelectedBacklogIssue] = useState<GitHubIssue | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [reviewData, setReviewData] = useState<KanbanReview | null>(null);
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
  const [auditLog, setAuditLog] = useState<api.CardAuditLogEntry[]>([]);
  const [ghComments, setGhComments] = useState<api.GitHubComment[]>([]);
  const [timelineFilter, setTimelineFilter] = useState<"review" | "pm" | "work" | "general" | null>(null);
  const [activityRefreshTick, setActivityRefreshTick] = useState(0);
  const [nowMs, setNowMs] = useState(() => Date.now());
  const [liveTurnsByAgentId, setLiveTurnsByAgentId] = useState<Record<string, api.AgentTurnState>>({});
  const ghCommentsCache = useRef<Map<string, { comments: api.GitHubComment[]; body: string; ts: number }>>(new Map());
  const detailRequestSeq = useRef(0);

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
  const invalidateCardActivity = (cardId: string) => {
    ghCommentsCache.current.delete(cardId);
    if (selectedCardId === cardId) {
      setActivityRefreshTick((prev) => prev + 1);
    }
  };

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
    const requestSeq = detailRequestSeq.current + 1;
    detailRequestSeq.current = requestSeq;
    const isCurrentRequest = () => detailRequestSeq.current === requestSeq;

    setEditor(coerceEditor(selectedCard));
    setRetryAssigneeId(selectedCard?.assignee_agent_id ?? "");
    setNewChecklistItem("");
    setReviewData(null);
    setReviewDecisions({});
    setAuditLog([]);
    setGhComments([]);
    setTimelineFilter(null);
    // Fetch audit log and GitHub comments for selected card
    if (selectedCard) {
      api.getCardAuditLog(selectedCard.id).then((logs) => {
        if (isCurrentRequest()) setAuditLog(logs);
      }).catch(() => {});
      if (selectedCard.github_issue_number) {
        const CACHE_TTL = 5 * 60 * 1000; // 5 minutes
        const cached = ghCommentsCache.current.get(selectedCard.id);
        if (cached && Date.now() - cached.ts < CACHE_TTL) {
          if (isCurrentRequest()) {
            setGhComments(cached.comments);
            if (cached.body != null) setEditor((prev) => ({ ...prev, description: cached.body }));
          }
        } else {
          api.getCardGitHubComments(selectedCard.id).then((result) => {
            if (!isCurrentRequest()) return;
            ghCommentsCache.current.set(selectedCard.id, { comments: result.comments, body: result.body, ts: Date.now() });
            setGhComments(result.comments);
            if (result.body != null) setEditor((prev) => ({ ...prev, description: result.body }));
          }).catch(() => {});
        }
      }
    }
    // Fetch review data for suggestion_pending/dilemma_pending cards
    if (selectedCard?.review_status === "suggestion_pending" || selectedCard?.review_status === "dilemma_pending" || selectedCard?.review_status === "decided") {
      api.getKanbanReviews(selectedCard.id).then((reviews) => {
        if (!isCurrentRequest()) return;
        const latest = reviews.filter((r) => r.verdict === "improve" || r.verdict === "dilemma" || r.verdict === "mixed" || r.verdict === "decided")
          .sort((a, b) => b.round - a.round)[0];
        if (latest) {
          setReviewData(latest);
          // Restore existing decisions
          try {
            const items = latest.items_json ? JSON.parse(latest.items_json) as Array<{ id: string; category: string; decision?: string }> : [];
            const existing: Record<string, "accept" | "reject"> = {};
            for (const item of items) {
              if (item.decision === "accept" || item.decision === "reject") {
                existing[item.id] = item.decision;
              }
            }
            setReviewDecisions(existing);
          } catch { /* ignore */ }
        }
      }).catch(() => {});
    }
  }, [activityRefreshTick, selectedCard]);

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
    Promise.all([
      api.getKanbanRepoSources().catch(() => [] as KanbanRepoSource[]),
      api.getGitHubRepos().then((result) => result.repos).catch(() => [] as GitHubRepoOption[]),
    ]).then(([sources, repos]) => {
      setRepoSources(sources);
      setAvailableRepos(repos);
      if (!selectedRepo && sources[0]?.repo) {
        setSelectedRepo(sources[0].repo);
      }
    }).finally(() => setInitialLoading(false));
  }, []);

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
    if (!selectedRepo) {
      setIssues([]);
      setLoadingIssues(false);
      return;
    }

    let stale = false;
    setIssues([]);
    setLoadingIssues(true);
    setActionError(null);
    api.getGitHubIssues(selectedRepo, "open", 100)
      .then((result) => {
        if (stale) return;
        setIssues(result.issues);
        if (result.error) {
          setActionError(result.error);
        }
      })
      .catch((error) => {
        if (stale) return;
        setIssues([]);
        setActionError(error instanceof Error ? error.message : "Failed to load GitHub issues.");
      })
      .finally(() => { if (!stale) setLoadingIssues(false); });
    return () => { stale = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedRepo]);

  useEffect(() => {
    if (!showClosed && TERMINAL_STATUSES.has(mobileColumnStatus)) {
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
        ?? cards.find((card) => hasManualInterventionReason(card) && card.status === "in_progress")?.status
        ?? "in_progress";
      setMobileColumnStatus(
        focusStatus === "review" || focusStatus === "requested" ? focusStatus : "in_progress",
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
    switch (status) {
      case "reviewing":
        return { bg: "rgba(20,184,166,0.16)", text: "#5eead4" };
      case "changes_requested":
        return { bg: "rgba(251,113,133,0.16)", text: "#fda4af" };
      case "passed":
        return { bg: "rgba(34,197,94,0.18)", text: "#86efac" };
      case "decision":
        return { bg: "rgba(249,115,22,0.16)", text: "#fdba74" };
      case "completed":
        return { bg: "rgba(96,165,250,0.16)", text: "#93c5fd" };
      case "comment":
        return { bg: "rgba(148,163,184,0.12)", text: "#94a3b8" };
    }
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
  const selectedParentCard = selectedCard?.parent_card_id
    ? repoCardsById.get(selectedCard.parent_card_id) ?? null
    : null;
  const selectedChildCards = selectedCard ? childCardsByParentId.get(selectedCard.id) ?? [] : [];
  const selectedLiveToolState = selectedCard ? liveToolStateByCardId.get(selectedCard.id) ?? null : null;
  const selectedLatestDispatch = selectedCard?.latest_dispatch_id
    ? dispatchMap.get(selectedCard.latest_dispatch_id) ?? null
    : null;

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
    const needle = search.trim().toLowerCase();
    return repoCards.filter((card) => {
      if (!showClosed && TERMINAL_STATUSES.has(card.status)) {
        return false;
      }
      // Per-agent kanban view filter (top-level agent selector)
      if (selectedAgentId && card.assignee_agent_id !== selectedAgentId) {
        return false;
      }
      if (agentFilter !== "all" && card.assignee_agent_id !== agentFilter) {
        return false;
      }
      if (deptFilter !== "all" && agentMap.get(card.assignee_agent_id ?? "")?.department_id !== deptFilter) {
        return false;
      }
      if (cardTypeFilter === "issue" && isReviewCard(card)) return false;
      if (cardTypeFilter === "review" && !isReviewCard(card)) return false;
      if (signalStatusFilter === "review" && card.status !== "review") return false;
      if (signalStatusFilter === "blocked" && !isManualInterventionCard(card)) return false;
      if (signalStatusFilter === "requested" && card.status !== "requested") return false;
      if (
        signalStatusFilter === "stalled"
        && !(card.status === "in_progress" && Boolean(card.started_at) && nowMs - ((card.started_at ?? 0) < 1e12 ? (card.started_at ?? 0) * 1000 : (card.started_at ?? 0)) > STALE_IN_PROGRESS_MS)
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
    if (!selectedAgentId || !agentPipelineStages.length) return COLUMN_DEFS;
    const base = COLUMN_DEFS.filter((c) => !QA_STATUSES.has(c.status));
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
    const grouped = new Map<KanbanCardStatus, KanbanCard[]>();
    for (const column of effectiveColumnDefs) {
      grouped.set(column.status, []);
    }
    for (const card of filteredCards) {
      grouped.get(card.status)?.push(card);
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
  const hasQaCards = filteredCards.some((c) => QA_STATUSES.has(c.status));
  const boardColumns = useMemo(() => effectiveColumnDefs.filter((column) =>
    (showClosed || !TERMINAL_STATUSES.has(column.status))
    && (!QA_STATUSES.has(column.status) || hasQaCards),
  ), [effectiveColumnDefs, hasQaCards, showClosed]);
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
      setRepoSources((prev) => prev.some((source) => source.id === created.id) ? prev : [...prev, created]);
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
      setRepoSources((prev) => prev.filter((item) => item.id !== source.id));
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("repo 제거에 실패했습니다.", "Failed to remove repo."));
    } finally {
      setRepoBusy(false);
    }
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
      setIssues((prev) => prev.filter((i) => i.number !== issue.number));
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

  const showDesktopDetailPanel = Boolean(selectedCard && !compactBoard);

  useEffect(() => {
    const fallbackStatus = boardColumns[0]?.status ?? "backlog";
    if (!boardColumns.some((column) => column.status === mobileColumnStatus)) {
      setMobileColumnStatus(fallbackStatus);
    }
  }, [boardColumns, mobileColumnStatus]);

  const focusMobileColumn = (status: KanbanCardStatus, scrollToSection: boolean) => {
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
      setMobileColumnStatus(card.status);
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
        : signalStatusFilter === "requested" ? tr("수락 대기", "Waiting acceptance")
          : signalStatusFilter === "stalled" ? tr("진행 정체", "Stale in progress")
            : null;

  return (
    <div className="space-y-4 pb-24 md:pb-0 min-w-0 overflow-x-hidden" style={{ paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))" }}>
      <SurfaceSection
        eyebrow={tr("워크 오케스트레이션", "Work orchestration")}
        title={tr("칸반", "Kanban")}
        description={tr(
          "Repo intake, backlog triage, dispatch, review 흐름을 한 표면에서 다룹니다.",
          "Handle repo intake, backlog triage, dispatch, and review flow from one surface.",
        )}
        badge={tr(`${openCount}건 진행`, `${openCount} active`)}
        className="rounded-[30px] p-4 sm:p-5"
        style={{
          borderColor: "color-mix(in srgb, var(--th-accent-info) 16%, var(--th-border) 84%)",
          background:
            "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-info) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
        }}
      >
        <div className="mt-4 flex flex-wrap gap-2">
          <SurfaceMetricPill
            tone="info"
            label={tr("가시 범위", "Visible scope")}
            value={initialLoading ? "…" : `${totalVisible}${tr("건", " items")}`}
            className="flex-1 sm:flex-none"
          />
          <SurfaceMetricPill
            tone="accent"
            label={tr("Repo 초점", "Repo focus")}
            value={selectedRepoLabel}
            className="flex-1 sm:flex-none"
          />
          <SurfaceMetricPill
            tone="neutral"
            label={tr("담당 범위", "Agent scope")}
            value={selectedAgentScopeLabel}
            className="flex-1 sm:flex-none"
          />
        </div>

        <div className="mt-4 flex flex-wrap items-center gap-2">
          {stalledCards.length > 0 && (
            <SurfaceActionButton
              tone="danger"
              onClick={() => { setStalledPopup(true); setStalledSelected(new Set()); }}
              className="animate-pulse"
            >
              {tr(`정체 ${stalledCards.length}건`, `${stalledCards.length} stalled`)}
            </SurfaceActionButton>
          )}
          {deferredDodCount > 0 && (
            <SurfaceActionButton tone="warn" onClick={() => setDeferredDodPopup(true)}>
              {tr(`미검증 DoD ${deferredDodCount}건`, `${deferredDodCount} deferred DoD`)}
            </SurfaceActionButton>
          )}
          <SurfaceActionButton
            tone={settingsOpen ? "info" : "neutral"}
            onClick={() => setSettingsOpen((prev) => !prev)}
          >
            {settingsOpen ? tr("설정 접기", "Close settings") : tr("설정 열기", "Open settings")}
          </SurfaceActionButton>
        </div>

        <div className="mt-4 flex flex-col gap-3 min-w-0">
          <div className="hidden min-w-0 flex-wrap items-center gap-1.5 sm:flex">
            {repoSources.length >= 1 && repoSources.map((source) => (
              <SurfaceSegmentButton
                key={source.id}
                onClick={() => setSelectedRepo(source.repo)}
                active={selectedRepo === source.repo}
                tone="info"
                className="max-w-[180px] truncate"
              >
                {source.repo.split("/")[1] ?? source.repo}
              </SurfaceSegmentButton>
            ))}
            {selectedRepo && (() => {
              const agentEntries = Array.from(repoAgentCounts.entries()).sort((a, b) => b[1] - a[1]);
              if (agentEntries.length <= 1) return null;
              if (agentEntries.length <= 4) {
                return (<>
                  {repoSources.length > 1 && (
                    <span className="px-1 text-xs" style={{ color: "var(--th-text-subtle)" }}>
                      /
                    </span>
                  )}
                  <SurfaceSegmentButton
                    onClick={() => setSelectedAgentId(null)}
                    active={!selectedAgentId}
                    tone="accent"
                  >
                    {tr(`전체`, `All`)}
                  </SurfaceSegmentButton>
                  {agentEntries.map(([aid, count]) => (
                    <SurfaceSegmentButton
                      key={aid}
                      onClick={() => setSelectedAgentId(aid)}
                      active={selectedAgentId === aid}
                      tone="accent"
                      className="max-w-[160px] truncate"
                    >
                      {getAgentLabel(aid)} ({count})
                    </SurfaceSegmentButton>
                  ))}
                </>);
              }
              return (
                <select
                  value={selectedAgentId ?? ""}
                  onChange={(e) => setSelectedAgentId(e.target.value || null)}
                  className="text-xs px-2.5 py-1.5 rounded-lg border bg-transparent min-w-0 max-w-[180px]"
                  style={{
                    borderColor: selectedAgentId
                      ? "color-mix(in srgb, var(--th-accent-primary) 40%, transparent)"
                      : "rgba(148,163,184,0.22)",
                    color: selectedAgentId ? "var(--th-accent-primary)" : "var(--th-text-muted)",
                  }}
                >
                  <option value="">{tr(`전체`, `All`)}</option>
                  {agentEntries.map(([aid, count]) => (
                    <option key={aid} value={aid}>{getAgentLabel(aid)} ({count})</option>
                  ))}
                </select>
              );
            })()}
          </div>
        </div>

        {signalFilterLabel && (
          <SurfaceNotice
            tone="warn"
            className="mt-1"
            compact
            action={(
              <SurfaceActionButton
                type="button"
                tone="warn"
                compact
                onClick={() => setSignalStatusFilter("all")}
              >
                {tr("해제", "Clear")}
              </SurfaceActionButton>
            )}
          >
            <div className="text-xs leading-5">
              {tr("대시보드 포커스", "Dashboard focus")}: {signalFilterLabel}
            </div>
          </SurfaceNotice>
        )}

        {/* Row 2 (mobile only): Repo tabs + Agent selector — on desktop these are in Row 1 */}
        <div className="mt-1 flex gap-1.5 overflow-x-auto min-w-0 sm:hidden">
          {repoSources.length >= 1 && repoSources.map((source) => (
            <SurfaceSegmentButton
              key={source.id}
              onClick={() => setSelectedRepo(source.repo)}
              active={selectedRepo === source.repo}
              tone="info"
              className="max-w-[180px] truncate"
            >
              {source.repo.split("/")[1] ?? source.repo}
            </SurfaceSegmentButton>
          ))}
        </div>

        {/* Mobile-only agent selector row */}
        <div className="sm:hidden">
        {selectedRepo && (() => {
          const agentEntries = Array.from(repoAgentCounts.entries()).sort((a, b) => b[1] - a[1]);
          const agentCount = agentEntries.length;
          if (agentCount <= 1) return null; // 1 agent or less: hide
          if (agentCount <= 4) {
            // Tab buttons
            return (
              <div className="mt-1 flex gap-1.5 overflow-x-auto min-w-0">
                <SurfaceSegmentButton
                  onClick={() => setSelectedAgentId(null)}
                  active={!selectedAgentId}
                  tone="accent"
                >
                  {tr(`전체 (${repoCards.length})`, `All (${repoCards.length})`)}
                </SurfaceSegmentButton>
                {agentEntries.map(([aid, count]) => (
                  <SurfaceSegmentButton
                    key={aid}
                    onClick={() => setSelectedAgentId(aid)}
                    active={selectedAgentId === aid}
                    tone="accent"
                    className="max-w-[160px] truncate"
                  >
                    {getAgentLabel(aid)} ({count})
                  </SurfaceSegmentButton>
                ))}
              </div>
            );
          }
          // Dropdown for >4 agents
          return (
            <div className="flex items-center gap-2 -mt-1">
              <select
                value={selectedAgentId ?? ""}
                onChange={(e) => setSelectedAgentId(e.target.value || null)}
                className="text-xs px-3 py-2 rounded-lg border bg-transparent min-w-0 max-w-[220px]"
                style={{
                  borderColor: selectedAgentId
                    ? "color-mix(in srgb, var(--th-accent-primary) 40%, transparent)"
                    : "rgba(148,163,184,0.22)",
                  color: selectedAgentId ? "var(--th-accent-primary)" : "var(--th-text-muted)",
                  backgroundColor: selectedAgentId ? "var(--th-accent-primary-soft)" : "transparent",
                  minHeight: 44,
                }}
              >
                <option value="">{tr(`전체 (${repoCards.length})`, `All (${repoCards.length})`)}</option>
                {agentEntries.map(([aid, count]) => (
                  <option key={aid} value={aid}>
                    {getAgentLabel(aid)} ({count})
                  </option>
                ))}
              </select>
            </div>
          );
        })()}
        </div>

        {settingsOpen && (
          <div className="mt-4 space-y-3 min-w-0 overflow-hidden">
            <div className="flex flex-wrap gap-2">
              {repoSources.length === 0 && (
                <span className="px-3 py-2 rounded-xl text-sm border border-dashed" style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-muted)" }}>
                  {tr("먼저 backlog repo를 추가하세요.", "Add a backlog repo first.")}
                </span>
              )}
              {repoSources.map((source) => (
                <div
                  key={source.id}
                  className="inline-flex items-center gap-2 rounded-xl border px-3 py-2 text-sm"
                  style={{
                    borderColor: selectedRepo === source.repo
                      ? "color-mix(in srgb, #60a5fa 52%, transparent)"
                      : SURFACE_CHIP_STYLE.borderColor,
                    background: selectedRepo === source.repo
                      ? "color-mix(in srgb, var(--th-badge-sky-bg) 78%, var(--th-card-bg) 22%)"
                      : SURFACE_CHIP_STYLE.background,
                  }}
                >
                  <button
                    onClick={() => setSelectedRepo(source.repo)}
                    className="text-left truncate"
                    style={{ color: selectedRepo === source.repo ? "#dbeafe" : "var(--th-text-primary)" }}
                  >
                    {source.repo}
                  </button>
                  <button
                    onClick={() => void handleRemoveRepo(source)}
                    disabled={repoBusy}
                    className="text-xs"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("삭제", "Remove")}
                  </button>
                </div>
              ))}
            </div>

            <div className="grid gap-2 sm:grid-cols-[minmax(0,1fr)_auto]">
              <input
                list="kanban-repo-options"
                value={repoInput}
                onChange={(event) => setRepoInput(event.target.value)}
                placeholder={tr("owner/repo 입력 또는 선택", "Type or pick owner/repo")}
                className="min-w-0 rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
              />
              <datalist id="kanban-repo-options">
                {availableRepos.map((repo) => (
                  <option key={repo.nameWithOwner} value={repo.nameWithOwner} />
                ))}
              </datalist>
              <button
                onClick={() => void handleAddRepo()}
                disabled={repoBusy || !repoInput.trim()}
                className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50 w-full sm:w-auto"
                style={{ backgroundColor: "#2563eb" }}
              >
                {repoBusy ? tr("처리 중", "Working") : tr("Repo 추가", "Add repo")}
              </button>
            </div>

            <div className="flex flex-col gap-2 w-full">
              <label
                className="flex items-center gap-2 rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-secondary)" }}
              >
                <input
                  type="checkbox"
                  checked={showClosed}
                  onChange={(event) => setShowClosed(event.target.checked)}
                />
                {tr("닫힌 컬럼 표시", "Show closed columns")}
              </label>
              {selectedRepo && (() => {
                const currentSource = repoSources.find((s) => s.repo === selectedRepo);
                if (!currentSource) return null;
                return (
                  <label
                    className="flex items-center gap-2 rounded-xl border px-3 py-2 text-sm"
                    style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-secondary)" }}
                  >
                    <span className="shrink-0">{tr("기본 담당자", "Default agent")}</span>
                    <select
                      value={currentSource.default_agent_id ?? ""}
                      onChange={(event) => {
                        const value = event.target.value || null;
                        void api.updateKanbanRepoSource(currentSource.id, { default_agent_id: value });
                        setRepoSources((prev) => prev.map((s) => s.id === currentSource.id ? { ...s, default_agent_id: value } : s));
                      }}
                      className="min-w-0 flex-1 rounded-lg border px-2 py-1 text-xs"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      <option value="">{tr("없음", "None")}</option>
                      {agents.map((agent) => (
                        <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                      ))}
                    </select>
                  </label>
                );
              })()}
            </div>

            <div className="grid gap-2 md:grid-cols-4">
              <input
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder={tr("제목 / 설명 / 담당자 검색", "Search title / description / assignee")}
                className="rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
              />
              <select
                value={agentFilter}
                onChange={(event) => setAgentFilter(event.target.value)}
                className="rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
              >
                <option value="all">{tr("전체 에이전트", "All agents")}</option>
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </select>
              <select
                value={deptFilter}
                onChange={(event) => setDeptFilter(event.target.value)}
                className="rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
              >
                <option value="all">{tr("전체 부서", "All departments")}</option>
                {departments.map((department) => (
                  <option key={department.id} value={department.id}>{localeName(locale, department)}</option>
                ))}
              </select>
              <select
                value={cardTypeFilter}
                onChange={(event) => setCardTypeFilter(event.target.value as "all" | "issue" | "review")}
                className="rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
              >
                <option value="all">{tr("전체 카드", "All cards")}</option>
                <option value="issue">{tr("이슈만", "Issues only")}</option>
                <option value="review">{tr("리뷰만", "Reviews only")}</option>
              </select>
              <select
                value={signalStatusFilter}
                onChange={(event) => setSignalStatusFilter(event.target.value as "all" | "review" | "blocked" | "requested" | "stalled")}
                className="rounded-xl px-3 py-2 text-sm bg-black/20 border"
                style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-primary)" }}
              >
                <option value="all">{tr("대시보드 신호 전체", "All dashboard signals")}</option>
                <option value="review">{tr("리뷰 대기", "Review queue")}</option>
                <option value="blocked">{tr("수동 개입", "Manual intervention")}</option>
                <option value="requested">{tr("수락 대기", "Waiting acceptance")}</option>
                <option value="stalled">{tr("진행 정체", "Stale in progress")}</option>
              </select>
            </div>
          </div>
        )}

        {actionError && (
          <SurfaceNotice tone="danger" className="mt-4">
            {actionError}
          </SurfaceNotice>
        )}

        {/* Assignee selection modal: shown when moving to "ready" without an assignee */}
        {assignBeforeReady && (
          <div className="fixed inset-0 z-50 flex items-center justify-center p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={() => setAssignBeforeReady(null)}>
            <SurfaceCard
              onClick={(e) => e.stopPropagation()}
              className="w-full max-w-sm space-y-4 rounded-[28px] p-5"
              style={{
                background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
                borderColor: "color-mix(in srgb, var(--th-accent-info) 18%, var(--th-border) 82%)",
              }}
            >
              <h3 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>{tr("담당자 할당", "Assign Agent")}</h3>
              <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>{tr("준비됨 상태로 이동하려면 담당자를 지정해야 합니다.", "Assign an agent before moving to ready.")}</p>
              <select
                value={assignBeforeReady.agentId}
                onChange={(e) => setAssignBeforeReady((prev) => prev ? { ...prev, agentId: e.target.value } : null)}
                className="w-full rounded-xl border px-3 py-2 text-sm"
                style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
              >
                <option value="">{tr("선택...", "Select...")}</option>
                {agents.map((a) => (
                  <option key={a.id} value={a.id}>{a.name_ko || a.name} ({a.id})</option>
                ))}
              </select>
              <div className="flex justify-end gap-2">
                <SurfaceActionButton
                  onClick={() => setAssignBeforeReady(null)}
                  tone="neutral"
                >
                  {tr("취소", "Cancel")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  disabled={!assignBeforeReady.agentId}
                  tone="success"
                  onClick={async () => {
                    const { cardId, agentId } = assignBeforeReady;
                    setAssignBeforeReady(null);
                    try {
                      await onUpdateCard(cardId, { status: "ready", assignee_agent_id: agentId });
                    } catch (error) {
                      setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
                    }
                  }}
                >
                  {tr("할당 후 준비됨", "Assign & Ready")}
                </SurfaceActionButton>
              </div>
            </SurfaceCard>
          </div>
        )}

        {deferredDodPopup && (() => {
          const deferredItems = cards.flatMap((c) => {
            const meta = parseCardMetadata(c.metadata_json);
            return (meta.deferred_dod ?? []).map((d) => ({ ...d, cardId: c.id, cardTitle: c.title, issueNumber: c.github_issue_number }));
          }).filter((d) => !d.verified);
          return (
            <SurfaceCard
              className="space-y-3 rounded-[24px] p-4"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-warn) 32%, var(--th-border) 68%)",
                background: "color-mix(in srgb, var(--th-badge-amber-bg) 72%, var(--th-card-bg) 28%)",
              }}
            >
              <div className="flex items-center justify-between">
                <span className="text-sm font-semibold" style={{ color: "#fbbf24" }}>
                  {tr(`미검증 DoD (${deferredItems.length}건)`, `Deferred DoD (${deferredItems.length})`)}
                </span>
                <SurfaceActionButton tone="warn" compact onClick={() => setDeferredDodPopup(false)}>
                  {tr("닫기", "Close")}
                </SurfaceActionButton>
              </div>
              {deferredItems.length === 0 ? (
                <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("미검증 항목 없음", "No deferred items")}</p>
              ) : (
                <div className="space-y-2 max-h-60 overflow-y-auto">
                  {deferredItems.map((item) => (
                    <label key={item.id} className="flex items-start gap-2 text-xs cursor-pointer">
                      <input
                        type="checkbox"
                        checked={verifyingDeferredDodIds.has(item.id)}
                        disabled={verifyingDeferredDodIds.has(item.id)}
                        onChange={async () => {
                          setActionError(null);
                          setVerifyingDeferredDodIds((prev) => {
                            const next = new Set(prev);
                            next.add(item.id);
                            return next;
                          });
                          try {
                            await onPatchDeferDod(item.cardId, { verify: item.id });
                          } catch (error) {
                            setActionError(error instanceof Error ? error.message : tr("DoD 검증에 실패했습니다.", "Failed to verify deferred DoD."));
                          } finally {
                            setVerifyingDeferredDodIds((prev) => {
                              const next = new Set(prev);
                              next.delete(item.id);
                              return next;
                            });
                          }
                        }}
                        className="mt-0.5"
                      />
                      <span style={{ color: "var(--th-text-primary)" }}>
                        {item.issueNumber ? `#${item.issueNumber} ` : ""}{item.label}
                        <span className="ml-1" style={{ color: "var(--th-text-muted)" }}>({item.cardTitle})</span>
                      </span>
                    </label>
                  ))}
                </div>
              )}
            </SurfaceCard>
          );
        })()}

        {stalledPopup && (
          <SurfaceCard
            className="space-y-3 rounded-[24px] p-4"
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-danger) 32%, var(--th-border) 68%)",
              background: "color-mix(in srgb, rgba(255, 107, 107, 0.18) 76%, var(--th-card-bg) 24%)",
            }}
          >
            <div className="flex items-center justify-between">
              <h3 className="text-sm font-semibold" style={{ color: "#fca5a5" }}>
                {tr(`정체 카드 ${stalledCards.length}건`, `${stalledCards.length} Stalled Cards`)}
              </h3>
              <div className="flex gap-2">
                <SurfaceActionButton
                  onClick={() => setStalledSelected(stalledSelected.size === stalledCards.length ? new Set() : new Set(stalledCards.map((c) => c.id)))}
                  tone="neutral"
                  compact
                >
                  {stalledSelected.size === stalledCards.length ? tr("해제", "Deselect") : tr("전체 선택", "Select all")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => setStalledPopup(false)}
                  tone="neutral"
                  compact
                >
                  {tr("닫기", "Close")}
                </SurfaceActionButton>
              </div>
            </div>
            <div className="space-y-1 max-h-60 overflow-y-auto">
              {stalledCards.map((card) => (
                <label key={card.id} className="flex cursor-pointer items-center gap-2 rounded-lg px-2 py-1.5 text-sm transition-opacity hover:opacity-90" style={{ color: "var(--th-text-primary)" }}>
                  <input
                    type="checkbox"
                    checked={stalledSelected.has(card.id)}
                    onChange={() => {
                      setStalledSelected((prev) => {
                        const next = new Set(prev);
                        next.has(card.id) ? next.delete(card.id) : next.add(card.id);
                        return next;
                      });
                    }}
                    className="accent-red-400"
                  />
                  <span className="truncate flex-1">{card.title}</span>
                  <span className="text-[10px] px-1.5 py-0.5 rounded-full shrink-0" style={{ backgroundColor: "rgba(239,68,68,0.15)", color: "#f87171" }}>
                    {card.review_status}
                  </span>
                  <span className="text-[10px] shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {card.github_repo ? card.github_repo.split("/")[1] : ""}
                  </span>
                </label>
              ))}
            </div>
            {stalledSelected.size > 0 && (
              <div className="flex gap-2 pt-1">
                <SurfaceActionButton
                  onClick={() => void handleBulkAction("pass")}
                  disabled={bulkBusy}
                  tone="success"
                  compact
                >
                  {bulkBusy ? "…" : tr(`일괄 Pass (${stalledSelected.size})`, `Pass All (${stalledSelected.size})`)}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => void handleBulkAction("reset")}
                  disabled={bulkBusy}
                  tone="info"
                  compact
                >
                  {bulkBusy ? "…" : tr(`일괄 Reset (${stalledSelected.size})`, `Reset All (${stalledSelected.size})`)}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => void handleBulkAction("cancel")}
                  disabled={bulkBusy}
                  tone="danger"
                  compact
                >
                  {bulkBusy ? "…" : tr(`일괄 Cancel (${stalledSelected.size})`, `Cancel All (${stalledSelected.size})`)}
                </SurfaceActionButton>
              </div>
            )}
          </SurfaceCard>
        )}
      </SurfaceSection>

      {/* Cancel confirmation modal — ask whether to also close GitHub issues */}
      {cancelConfirm && (() => {
        const ghCards = cancelConfirm.cardIds
          .map((id) => cardsById.get(id))
          .filter((c): c is KanbanCard => !!(c?.github_repo && c.github_issue_number));
        return (
          <div className="fixed inset-0 z-50 flex items-center justify-center p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }}>
            <SurfaceCard
              onClick={(e) => e.stopPropagation()}
              className="w-full max-w-md space-y-4 rounded-[28px] p-5"
              style={{
                background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
                borderColor: "color-mix(in srgb, var(--th-accent-danger) 18%, var(--th-border) 82%)",
              }}
            >
              <h3 className="text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
                {tr("카드 취소 확인", "Cancel cards")}
              </h3>
              <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>
                {tr(
                  `${cancelConfirm.cardIds.length}건의 카드를 취소합니다.`,
                  `Cancel ${cancelConfirm.cardIds.length} card(s).`,
                )}
              </p>
              {ghCards.length > 0 && (
                <div className="space-y-2">
                  <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>
                    {tr(
                      `GitHub 이슈가 연결된 카드 ${ghCards.length}건:`,
                      `${ghCards.length} card(s) linked to GitHub issues:`,
                    )}
                  </p>
                  <ul className="text-xs space-y-1 pl-2" style={{ color: "var(--th-text-muted)" }}>
                    {ghCards.map((c) => (
                      <li key={c.id}>
                        #{c.github_issue_number} — {c.title}
                      </li>
                    ))}
                  </ul>
                  <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {tr(
                      "※ GitHub 이슈는 카드 완료 시 자동으로 닫힙니다.",
                      "※ GitHub issues are automatically closed when the card is completed.",
                    )}
                  </p>
                </div>
              )}
              <div className="flex justify-end gap-2 pt-2">
                <SurfaceActionButton
                  onClick={() => setCancelConfirm(null)}
                  disabled={cancelBusy}
                  tone="neutral"
                >
                  {tr("돌아가기", "Go back")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => void executeBulkCancel()}
                  disabled={cancelBusy}
                  tone="danger"
                >
                  {cancelBusy ? tr("처리 중…", "Processing…") : tr("취소 확정", "Confirm cancel")}
                </SurfaceActionButton>
              </div>
            </SurfaceCard>
          </div>
        );
      })()}

      <div className={showDesktopDetailPanel ? "grid min-w-0 items-start gap-4 md:grid-cols-[minmax(0,1fr)_24rem] xl:grid-cols-[minmax(0,1fr)_28rem]" : "min-w-0"}>
        <div className="min-w-0 space-y-4">
          {selectedRepo && (
            <>
              <AutoQueuePanel
                tr={tr}
                locale={locale}
                agents={agents}
                selectedRepo={selectedRepo}
                selectedAgentId={selectedAgentId}
              />
              <PipelineVisualEditor
                tr={tr}
                locale={locale}
                repo={selectedRepo}
                agents={agents}
                selectedAgentId={selectedAgentId}
              />
            </>
          )}

          {/* ── Recent completions ── */}
          {selectedRepo && recentDoneCards.length > 0 && (() => {
            const PAGE_SIZE = 10;
            const totalPages = Math.ceil(recentDoneCards.length / PAGE_SIZE);
            const page = Math.min(recentDonePage, totalPages - 1);
            const pageCards = recentDoneCards.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);
            return (
              <SurfaceCard
                className="rounded-[24px] px-4 py-3"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 16%, var(--th-border) 84%)",
                  background: "color-mix(in srgb, var(--th-badge-emerald-bg) 56%, var(--th-card-bg) 44%)",
                }}
              >
                <button
                  onClick={() => setRecentDoneOpen((v) => !v)}
                  className="flex w-full items-center gap-2 text-left"
                >
                  <span className="text-xs font-semibold uppercase" style={{ color: "var(--th-text-muted)" }}>
                    {tr("최근 완료", "Recent Completions")}
                  </span>
                  <span className="rounded-full px-1.5 py-0.5 text-[10px] font-bold" style={{ background: "rgba(34,197,94,0.18)", color: "#4ade80" }}>
                    {recentDoneCards.length}
                  </span>
                  <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {recentDoneOpen ? "▲" : "▼"}
                  </span>
                </button>
                {recentDoneOpen && (
                  <div className="mt-2 space-y-1.5">
                    {pageCards.map((card) => {
                      const statusDef = COLUMN_DEFS.find((c) => c.status === card.status);
                      const agentName = getAgentLabel(card.assignee_agent_id);
                      const completedDate = card.completed_at
                        ? new Date(card.completed_at).toLocaleDateString(locale === "ko" ? "ko-KR" : "en-US", { month: "short", day: "numeric" })
                        : "";
                      return (
                        <button
                          key={card.id}
                          onClick={() => setSelectedCardId(card.id)}
                          className="flex w-full items-center gap-2 rounded-xl px-3 py-2 text-left text-sm transition-colors hover:brightness-125"
                          style={{ background: "rgba(148,163,184,0.06)" }}
                        >
                          <span
                            className="shrink-0 rounded-full px-1.5 py-0.5 text-[10px] font-semibold"
                            style={{ background: `${statusDef?.accent ?? "#22c55e"}22`, color: statusDef?.accent ?? "#22c55e" }}
                          >
                            {card.status === "done" ? tr("완료", "Done") : tr("취소", "Cancelled")}
                          </span>
                          {card.github_issue_number && (
                            <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>#{card.github_issue_number}</span>
                          )}
                          <span className="min-w-0 flex-1 truncate" style={{ color: "var(--th-text-primary)" }}>{card.title}</span>
                          <span className="shrink-0 text-[11px]" style={{ color: "var(--th-text-muted)" }}>{agentName}</span>
                          <span className="shrink-0 text-[11px]" style={{ color: "var(--th-text-muted)" }}>{completedDate}</span>
                        </button>
                      );
                    })}
                    {totalPages > 1 && (
                      <div className="flex items-center justify-center gap-3 pt-1">
                        <button
                          disabled={page === 0}
                          onClick={() => setRecentDonePage((p) => Math.max(0, p - 1))}
                          className="rounded px-2 py-0.5 text-xs disabled:opacity-30"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          ← {tr("이전", "Prev")}
                        </button>
                        <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {page + 1} / {totalPages}
                        </span>
                        <button
                          disabled={page >= totalPages - 1}
                          onClick={() => setRecentDonePage((p) => Math.min(totalPages - 1, p + 1))}
                          className="rounded px-2 py-0.5 text-xs disabled:opacity-30"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {tr("다음", "Next")} →
                        </button>
                      </div>
                    )}
                  </div>
                )}
              </SurfaceCard>
            );
          })()}

          {!selectedRepo ? (
            <SurfaceEmptyState
              className="rounded-[24px] px-4 py-10 text-center text-sm"
              style={{ borderColor: "rgba(148,163,184,0.22)", color: "var(--th-text-muted)" }}
            >
              {tr("repo를 추가하면 repo별 backlog와 칸반을 볼 수 있습니다.", "Add a repo to view its backlog and board.")}
            </SurfaceEmptyState>
          ) : (
            <div className="space-y-3">
              {compactBoard && (
                <>
                  <div className="space-y-3">
                    <div className="flex min-w-0 gap-2 overflow-x-auto pb-1">
                      {mobileColumnSummaries.map(({ column, count }) => (
                        <button
                          key={column.status}
                          type="button"
                          onClick={() => focusMobileColumn(column.status, true)}
                          className="min-w-[7rem] rounded-2xl border px-3 py-2 text-left"
                          style={{
                            borderColor: mobileColumnStatus === column.status ? `${column.accent}88` : "rgba(148,163,184,0.24)",
                            backgroundColor: mobileColumnStatus === column.status ? `${column.accent}22` : "rgba(255,255,255,0.04)",
                            color: mobileColumnStatus === column.status ? "white" : "var(--th-text-secondary)",
                          }}
                        >
                          <div className="text-[11px] font-semibold uppercase" style={{ color: mobileColumnStatus === column.status ? "white" : "var(--th-text-muted)" }}>
                            {tr(column.labelKo, column.labelEn)}
                          </div>
                          <div className="mt-1 text-lg font-semibold">{count}</div>
                        </button>
                      ))}
                    </div>
                    <div className="rounded-xl border px-3 py-2 text-xs" style={{ borderColor: "rgba(148,163,184,0.18)", color: "var(--th-text-muted)", backgroundColor: "rgba(15,23,42,0.35)" }}>
                      {focusedMobileSummary
                        ? tr(
                            `${focusedMobileSummary.column.labelKo} lane으로 바로 이동할 수 있습니다. 보드는 가로 스크롤되고 카드 상세는 시트에서 엽니다.`,
                            `Jump straight to the ${focusedMobileSummary.column.labelEn} lane. The board scrolls horizontally and card details open in a sheet.`,
                          )
                        : tr(
                            "보드는 가로 스크롤되고 카드 상세는 시트에서 엽니다.",
                            "The board scrolls horizontally and card details open in a sheet.",
                          )}
                    </div>
                  </div>
                </>
              )}

              <div className="pb-2" style={{ overflowX: "auto", overflowY: "visible" }}>
                <div className="flex items-start gap-4 min-w-max">
                  {visibleColumns.map((column) => {
                    const columnCards = cardsByStatus.get(column.status) ?? [];
                    const backlogCount = column.status === "backlog" ? columnCards.length + backlogIssues.length : columnCards.length;
                    return (
                      <div key={column.status} id={`kanban-mobile-${column.status}`}>
                        <KanbanColumn
                          column={column}
                          columnCards={columnCards}
                          backlogIssues={backlogIssues}
                          backlogCount={backlogCount}
                          tr={tr}
                          compactBoard={false}
                          initialLoading={initialLoading}
                          loadingIssues={loadingIssues}
                          closingIssueNumber={closingIssueNumber}
                          assigningIssue={assigningIssue}
                          getAgentLabel={getAgentLabel}
                          getAgentProvider={getAgentProvider}
                          resolveAgentFromLabels={resolveAgentFromLabels}
                          onCardClick={handleCardOpen}
                          onBacklogIssueClick={handleBacklogIssueOpen}
                          onCloseIssue={handleCloseIssue}
                          onDirectAssignIssue={handleDirectAssignIssue}
                          onOpenAssignModal={handleOpenAssignModal}
                          onUpdateCardStatus={handleUpdateCardStatus}
                          onSetActionError={setActionError}
                        />
                      </div>
                    );
                  })}
                </div>
              </div>
            </div>
          )}
        </div>

        {selectedCard && (
          <div
            className={compactBoard ? "fixed inset-0 z-50" : "hidden min-w-0 md:block"}
            style={compactBoard ? { backgroundColor: "var(--th-modal-overlay)" } : undefined}
            onClick={compactBoard ? () => setSelectedCardId(null) : undefined}
          >
            <div className={compactBoard ? "h-full w-full" : "sticky top-0"}>
              <div
                onClick={compactBoard ? (e) => e.stopPropagation() : undefined}
                className={compactBoard
                  ? "h-[100svh] w-full overflow-y-auto px-5 py-5 space-y-4"
                  : "w-full max-h-[calc(100svh-7rem)] overflow-y-auto rounded-3xl border p-5 lg:p-6 space-y-4 shadow-2xl"}
                style={{
                  background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
                  borderColor: compactBoard ? "transparent" : "color-mix(in srgb, var(--th-border) 72%, transparent)",
                  paddingTop: compactBoard
                    ? "max(1.25rem, calc(1.25rem + env(safe-area-inset-top)))"
                    : undefined,
                  paddingBottom: compactBoard
                    ? "max(6rem, calc(6rem + env(safe-area-inset-bottom)))"
                    : "max(2rem, calc(2rem + env(safe-area-inset-bottom)))",
                }}
              >
                <div className="flex items-start justify-between gap-3">
                  <div>
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                        {labelForStatus(selectedCard.status, tr)}
                      </span>
                      <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                        {priorityLabel(selectedCard.priority, tr)}
                      </span>
                      {selectedCard.github_repo && (
                        <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                          {selectedCard.github_repo}
                        </span>
                      )}
                    </div>
                    <h3 className="mt-2 text-xl font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {selectedCard.title}
                    </h3>
                  </div>
                  <SurfaceActionButton
                    tone="neutral"
                    onClick={() => setSelectedCardId(null)}
                    className="shrink-0 whitespace-nowrap"
                    style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
                  >
                    {tr("닫기", "Close")}
                  </SurfaceActionButton>
                </div>

                <div className="grid gap-3 md:grid-cols-2">
                  <label className="space-y-1">
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("제목", "Title")}</span>
                    <input
                      value={editor.title}
                      onChange={(event) => setEditor((prev) => ({ ...prev, title: event.target.value }))}
                      className="w-full rounded-xl border px-3 py-2 text-sm"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    />
                  </label>
                  <div className="space-y-1">
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("상태 전환", "Status")}</span>
                    <div className="flex flex-wrap gap-1.5">
                      {(STATUS_TRANSITIONS[selectedCard.status] ?? []).map((target) => {
                        const style = TRANSITION_STYLE[target] ?? TRANSITION_STYLE.backlog;
                        return (
                          <SurfaceActionButton
                            key={target}
                            type="button"
                            disabled={savingCard}
                            onClick={async () => {
                              if (target === "done" && editor.review_checklist.some((item) => !item.done)) {
                                setActionError(tr("review checklist를 모두 완료해야 done으로 이동할 수 있습니다.", "Complete the review checklist before moving to done."));
                                return;
                              }
                              setSavingCard(true);
                              setActionError(null);
                              try {
                                await onUpdateCard(selectedCard.id, { status: target });
                                invalidateCardActivity(selectedCard.id);
                                setEditor((prev) => ({ ...prev, status: target }));
                              } catch (error) {
                                setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
                              } finally {
                                setSavingCard(false);
                              }
                            }}
                            className="whitespace-nowrap"
                            style={{
                              background: style.bg,
                              borderColor: style.text,
                              color: style.text,
                            }}
                          >
                            → {labelForStatus(target, tr)}
                          </SurfaceActionButton>
                        );
                      })}
                    </div>
                  </div>
                </div>

                <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-3">
                  <label className="space-y-1">
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignee")}</span>
                    <select
                      value={editor.assignee_agent_id}
                      onChange={(event) => setEditor((prev) => ({ ...prev, assignee_agent_id: event.target.value }))}
                      className="w-full rounded-xl border px-3 py-2 text-sm"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      <option value="">{tr("없음", "None")}</option>
                      {agents.map((agent) => (
                        <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                      ))}
                    </select>
                  </label>
                  <label className="space-y-1">
                    <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("우선순위", "Priority")}</span>
                    <select
                      value={editor.priority}
                      onChange={(event) => setEditor((prev) => ({ ...prev, priority: event.target.value as KanbanCardPriority }))}
                      className="w-full rounded-xl border px-3 py-2 text-sm"
                      style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                    >
                      {PRIORITY_OPTIONS.map((priority) => (
                        <option key={priority} value={priority}>{priorityLabel(priority, tr)}</option>
                      ))}
                    </select>
                  </label>
                  <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("GitHub", "GitHub")}</div>
                    <div style={{ color: "var(--th-text-primary)" }}>
                      {selectedCard.github_issue_url ? (
                        <a href={selectedCard.github_issue_url} target="_blank" rel="noreferrer" className="hover:underline" style={{ color: "#93c5fd" }}>
                          #{selectedCard.github_issue_number ?? "-"}
                        </a>
                      ) : (
                        selectedCard.github_issue_number ? `#${selectedCard.github_issue_number}` : "-"
                      )}
                    </div>
                  </SurfaceCard>
                </div>

                {/* Blocked reason */}
                {hasManualInterventionReason(selectedCard) && selectedCard.blocked_reason && (
                  <SurfaceNotice tone="danger" className="items-start">
                    <div className="space-y-2">
                      <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-accent-danger)" }}>
                        {tr("수동 개입 사유", "Manual Intervention Reason")}
                      </div>
                      <div className="text-sm whitespace-pre-wrap break-words" style={{ color: "var(--th-text-primary)" }}>
                        {selectedCard.blocked_reason}
                      </div>
                    </div>
                  </SurfaceNotice>
                )}

                {/* Review status */}
                {selectedCard.status === "review" && selectedCard.review_status && (() => {
                  const reviewTone =
                    selectedCard.review_status === "dilemma_pending" || selectedCard.review_status === "suggestion_pending"
                      ? "warn"
                      : selectedCard.review_status === "improve_rework"
                        ? "danger"
                        : "info";
                  return (
                    <SurfaceNotice tone={reviewTone} className="items-start">
                      <div className="space-y-2">
                        <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                          {tr("카운터 모델 리뷰", "Counter-Model Review")}
                        </div>
                        <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                          {selectedCard.review_status === "reviewing" && (() => {
                            const reviewDispatch = dispatches.find(
                              (d) => d.parent_dispatch_id === selectedCard.latest_dispatch_id && d.dispatch_type === "review",
                            );
                            const verdictStatus = !reviewDispatch
                              ? tr("verdict 대기중", "verdict pending")
                              : reviewDispatch.status === "completed"
                                ? tr("verdict 전달됨", "verdict delivered")
                                : tr("verdict 미전달 — 에이전트가 아직 회신하지 않음", "verdict not delivered — agent hasn't responded");
                            return <>{tr("카운터 모델이 코드를 리뷰하고 있습니다...", "Counter model is reviewing...")} <span style={{ opacity: 0.7 }}>({verdictStatus})</span></>;
                          })()}
                          {selectedCard.review_status === "awaiting_dod" && tr("DoD 항목이 모두 완료되면 자동 리뷰가 시작됩니다.", "Auto review starts when all DoD items are complete.")}
                          {selectedCard.review_status === "improve_rework" && tr("개선 사항이 발견되어 원본 모델에 재작업을 요청했습니다.", "Improvements needed — rework dispatched to original model.")}
                          {selectedCard.review_status === "suggestion_pending" && tr("카운터 모델이 검토 항목을 추출했습니다. 수용/불수용을 결정해 주세요.", "Counter model extracted review findings. Decide accept/reject for each.")}
                          {selectedCard.review_status === "dilemma_pending" && tr("판단이 어려운 항목이 있습니다. 수동으로 결정해 주세요.", "Dilemma items found — manual decision needed.")}
                          {selectedCard.review_status === "decided" && tr("리뷰 결정이 완료되었습니다.", "Review decision completed.")}
                        </div>
                      </div>
                    </SurfaceNotice>
                  );
                })()}

                {/* Review suggestion decision UI */}
                {(selectedCard.review_status === "suggestion_pending" || selectedCard.review_status === "dilemma_pending") && reviewData && (() => {
                  const items: Array<{ id: string; category: string; summary: string; detail?: string; suggestion?: string; pros?: string; cons?: string; decision?: string }> =
                    reviewData.items_json ? JSON.parse(reviewData.items_json) : [];
                  const actionableItems = items.filter((i) => i.category !== "pass");
                  if (actionableItems.length === 0) return null;
                  const allDecided = actionableItems.every((i) => reviewDecisions[i.id]);
                  return (
                    <SurfaceCard className="space-y-4" style={{
                      borderColor: "rgba(234,179,8,0.35)",
                      backgroundColor: "rgba(234,179,8,0.06)",
                    }}>
                      <div className="flex items-center justify-between gap-2">
                        <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#eab308" }}>
                          {tr("리뷰 제안 사항", "Review Suggestions")}
                        </div>
                        <span className="text-xs px-2 py-0.5 rounded-full" style={{
                          backgroundColor: allDecided ? "rgba(34,197,94,0.18)" : "rgba(234,179,8,0.18)",
                          color: allDecided ? "#4ade80" : "#fde047",
                        }}>
                          {Object.keys(reviewDecisions).filter((k) => actionableItems.some((d) => d.id === k)).length}/{actionableItems.length}
                        </span>
                      </div>
                      <div className="space-y-3">
                        {actionableItems.map((item) => {
                          const decision = reviewDecisions[item.id];
                          return (
                            <SurfaceCard key={item.id} className="space-y-2 p-3" style={{
                              borderColor: decision === "accept" ? "rgba(34,197,94,0.35)" : decision === "reject" ? "rgba(239,68,68,0.35)" : "rgba(148,163,184,0.22)",
                              backgroundColor: decision === "accept" ? "rgba(34,197,94,0.06)" : decision === "reject" ? "rgba(239,68,68,0.06)" : "rgba(255,255,255,0.03)",
                            }}>
                              <div className="text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                                {item.summary}
                              </div>
                              {item.detail && (
                                <div className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                                  {item.detail}
                                </div>
                              )}
                              {item.suggestion && (
                                <div className="text-xs px-2 py-1 rounded-lg" style={{ backgroundColor: "rgba(96,165,250,0.08)", color: "#93c5fd" }}>
                                  {tr("제안", "Suggestion")}: {item.suggestion}
                                </div>
                              )}
                              {(item.pros || item.cons) && (
                                <div className="grid grid-cols-2 gap-2 text-xs">
                                  {item.pros && (
                                    <div className="px-2 py-1 rounded-lg" style={{ backgroundColor: "rgba(34,197,94,0.08)", color: "#86efac" }}>
                                      {tr("장점", "Pros")}: {item.pros}
                                    </div>
                                  )}
                                  {item.cons && (
                                    <div className="px-2 py-1 rounded-lg" style={{ backgroundColor: "rgba(239,68,68,0.08)", color: "#fca5a5" }}>
                                      {tr("단점", "Cons")}: {item.cons}
                                    </div>
                                  )}
                                </div>
                              )}
                              <div className="flex gap-2 pt-1">
                                <SurfaceActionButton
                                  onClick={() => {
                                    setReviewDecisions((prev) => ({ ...prev, [item.id]: "accept" }));
                                    void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision: "accept" }]).catch(() => {});
                                  }}
                                  tone={decision === "accept" ? "success" : "neutral"}
                                  className="flex-1 justify-center"
                                  style={{
                                    borderColor: decision === "accept" ? "rgba(34,197,94,0.6)" : undefined,
                                    background: decision === "accept" ? "rgba(34,197,94,0.2)" : undefined,
                                    color: decision === "accept" ? "#4ade80" : undefined,
                                  }}
                                >
                                  {tr("수용", "Accept")}
                                </SurfaceActionButton>
                                <SurfaceActionButton
                                  onClick={() => {
                                    setReviewDecisions((prev) => ({ ...prev, [item.id]: "reject" }));
                                    void api.saveReviewDecisions(reviewData.id, [{ item_id: item.id, decision: "reject" }]).catch(() => {});
                                  }}
                                  tone={decision === "reject" ? "danger" : "neutral"}
                                  className="flex-1 justify-center"
                                  style={{
                                    borderColor: decision === "reject" ? "rgba(239,68,68,0.6)" : undefined,
                                    background: decision === "reject" ? "rgba(239,68,68,0.2)" : undefined,
                                    color: decision === "reject" ? "#f87171" : undefined,
                                  }}
                                >
                                  {tr("불수용", "Reject")}
                                </SurfaceActionButton>
                              </div>
                            </SurfaceCard>
                          );
                        })}
                      </div>
                      <SurfaceActionButton
                        disabled={!allDecided || reviewBusy}
                        onClick={async () => {
                          setReviewBusy(true);
                          setActionError(null);
                          try {
                            await api.triggerDecidedRework(reviewData.id);
                            setReviewData(null);
                            setReviewDecisions({});
                          } catch (error) {
                            setActionError(error instanceof Error ? error.message : tr("재디스패치에 실패했습니다.", "Failed to trigger rework."));
                          } finally {
                            setReviewBusy(false);
                          }
                        }}
                        tone="warn"
                        className="w-full justify-center py-2.5 text-sm"
                      >
                        {reviewBusy
                          ? tr("재디스패치 중...", "Dispatching rework...")
                          : allDecided
                            ? tr("결정 완료 → 재디스패치", "Decisions Complete → Dispatch Rework")
                            : tr("모든 항목에 결정을 내려주세요", "Decide all items first")}
                      </SurfaceActionButton>
                    </SurfaceCard>
                  );
                })()}

                {(() => {
                  const hasSummaryChips = Boolean(
                    selectedCard.depth > 0
                    || selectedCardMetadata?.retry_count
                    || selectedCardMetadata?.failover_count
                    || selectedCardMetadata?.redispatch_count
                    || selectedCardChecklistSummary
                    || selectedCardDwellBadge
                    || selectedCardDelayBadge,
                  );
                  const hasDetailBlocks = Boolean(
                    selectedParentCard
                    || selectedChildCards.length > 0
                    || selectedLiveToolState
                    || selectedLatestDispatch
                    || selectedCard.latest_dispatch_status
                    || selectedCard.latest_dispatch_type
                    || selectedCardMetadata?.reward,
                  );
                  if (!hasSummaryChips && !hasDetailBlocks) return null;

                  return (
                    <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE }}>
                      <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                        {tr("카드 메타", "Card metadata")}
                      </div>

                      {hasSummaryChips && (
                        <div className="flex flex-wrap gap-2">
                          {selectedCard.depth > 0 && (
                            <span className="rounded-full border px-2 py-1 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                              {tr("체인", "Chain")} {selectedCard.depth}
                            </span>
                          )}
                          {selectedCardMetadata?.retry_count ? (
                            <span className="rounded-full border px-2 py-1 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                              {tr("재시도", "Retry")} {selectedCardMetadata.retry_count}
                            </span>
                          ) : null}
                          {selectedCardMetadata?.failover_count ? (
                            <span className="rounded-full border px-2 py-1 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "#fca5a5" }}>
                              {tr("Failover", "Failover")} {selectedCardMetadata.failover_count}
                            </span>
                          ) : null}
                          {selectedCardMetadata?.redispatch_count ? (
                            <span className="rounded-full border px-2 py-1 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "#fbbf24" }}>
                              {tr("재디스패치", "Redispatch")} {selectedCardMetadata.redispatch_count}
                            </span>
                          ) : null}
                          {selectedCardChecklistSummary && (
                            <span className="rounded-full border px-2 py-1 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "#99f6e4" }}>
                              {tr("체크리스트", "Checklist")} {selectedCardChecklistSummary}
                            </span>
                          )}
                          {selectedCardDwellBadge && (
                            <span
                              className="rounded-full border px-2 py-1 text-xs"
                              style={{
                                color: selectedCardDwellBadge.textColor,
                                backgroundColor: selectedCardDwellBadge.backgroundColor,
                                borderColor: selectedCardDwellBadge.borderColor,
                              }}
                            >
                              {selectedCardDwellBadge.label} {selectedCardDwellBadge.detail}
                            </span>
                          )}
                          {selectedCardDelayBadge && (
                            <span
                              className="rounded-full px-2 py-1 text-xs"
                              style={{ color: "white", backgroundColor: selectedCardDelayBadge.tone }}
                            >
                              {selectedCardDelayBadge.label} {selectedCardDelayBadge.detail}
                            </span>
                          )}
                        </div>
                      )}

                      {hasDetailBlocks && (
                        <div className="grid gap-3 lg:grid-cols-2">
                          {(selectedLatestDispatch || selectedCard.latest_dispatch_status || selectedCard.latest_dispatch_type) && (
                            <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                                {tr("최신 디스패치", "Latest dispatch")}
                              </div>
                              <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                                {selectedCard.latest_dispatch_id ? `#${selectedCard.latest_dispatch_id.slice(0, 8)}` : "-"}
                              </div>
                              <div className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                                {tr("상태", "Status")}: {selectedCard.latest_dispatch_status ?? selectedLatestDispatch?.status ?? "-"}
                              </div>
                              <div className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                                {tr("유형", "Type")}: {selectedCard.latest_dispatch_type ?? selectedLatestDispatch?.dispatch_type ?? "-"}
                              </div>
                            </SurfaceCard>
                          )}

                          {selectedCardMetadata?.reward && (
                            <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                                {tr("완료 보상", "Completion reward")}
                              </div>
                              <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                                +{selectedCardMetadata.reward.xp} XP
                              </div>
                              <div className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                                {getAgentLabel(selectedCardMetadata.reward.agent_id)}
                              </div>
                              <div className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                                {tr("완료 작업 수", "Tasks done")}: {selectedCardMetadata.reward.tasks_done}
                              </div>
                            </SurfaceCard>
                          )}

                          {selectedLiveToolState && (
                            <SurfaceCard className="space-y-1.5 p-3" style={{ borderColor: "rgba(59,130,246,0.28)", backgroundColor: "rgba(59,130,246,0.08)" }}>
                              <div className="text-xs" style={{ color: "#bfdbfe" }}>
                                {tr("실행 중 도구", "Live tool")}
                              </div>
                              <div className="text-sm" style={{ color: "#eff6ff" }}>
                                {selectedLiveToolState.line}
                              </div>
                              <div className="text-xs" style={{ color: "rgba(191,219,254,0.84)" }}>
                                {getAgentLabel(selectedLiveToolState.agentId)}
                                {selectedLiveToolState.updatedAt ? ` · ${formatIso(selectedLiveToolState.updatedAt, locale)}` : ""}
                              </div>
                            </SurfaceCard>
                          )}

                          {selectedParentCard && (
                            <SurfaceCard className="space-y-2 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                                {tr("상위 카드", "Parent card")}
                              </div>
                              <button
                                type="button"
                                onClick={() => setSelectedCardId(selectedParentCard.id)}
                                className="flex w-full items-center gap-2 rounded-xl border px-3 py-2 text-left transition-colors hover:brightness-110"
                                style={{ ...SURFACE_CHIP_STYLE }}
                              >
                                <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>
                                  {selectedParentCard.github_issue_number ? `#${selectedParentCard.github_issue_number}` : `#${selectedParentCard.id.slice(0, 6)}`}
                                </span>
                                <span className="min-w-0 truncate text-sm" style={{ color: "var(--th-text-primary)" }}>
                                  {selectedParentCard.title}
                                </span>
                              </button>
                            </SurfaceCard>
                          )}

                          {selectedChildCards.length > 0 && (
                            <SurfaceCard className="space-y-2 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                              <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                                {tr("하위 카드", "Child cards")} ({selectedChildCards.length})
                              </div>
                              <div className="space-y-2 max-h-48 overflow-y-auto">
                                {selectedChildCards.map((childCard) => (
                                  <button
                                    key={childCard.id}
                                    type="button"
                                    onClick={() => setSelectedCardId(childCard.id)}
                                    className="flex w-full items-center gap-2 rounded-xl border px-3 py-2 text-left transition-colors hover:brightness-110"
                                    style={{ ...SURFACE_CHIP_STYLE }}
                                  >
                                    <span className="shrink-0 text-xs" style={{ color: "var(--th-text-muted)" }}>
                                      {childCard.github_issue_number ? `#${childCard.github_issue_number}` : `#${childCard.id.slice(0, 6)}`}
                                    </span>
                                    <span className="min-w-0 truncate text-sm" style={{ color: "var(--th-text-primary)" }}>
                                      {childCard.title}
                                    </span>
                                  </button>
                                ))}
                              </div>
                            </SurfaceCard>
                          )}
                        </div>
                      )}
                    </SurfaceCard>
                  );
                })()}

                {/* Description / Issue Sections */}
                {(() => {
                  const parsed = parseIssueSections(editor.description);
                  if (!parsed) {
                    // Fallback: non-PMD format → show as markdown
                    return (
                      <div className="space-y-1">
                        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("설명", "Description")}</span>
                        {editor.description ? (
                          <SurfaceCard className="text-sm" style={{ ...SURFACE_PANEL_STYLE, color: "var(--th-text-primary)" }}>
                            <MarkdownContent content={editor.description} />
                          </SurfaceCard>
                        ) : (
                          <SurfaceEmptyState className="px-3 py-4 text-center text-xs">
                            {tr("설명이 없습니다.", "No description.")}
                          </SurfaceEmptyState>
                        )}
                      </div>
                    );
                  }

                  // Structured view for PMD-format issues
                  return (
                    <div className="space-y-3">
                      {/* 배경 */}
                      {parsed.background && (
                        <SurfaceCard className="space-y-2" style={{ ...SURFACE_PANEL_STYLE }}>
                          <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                            {tr("배경", "Background")}
                          </div>
                          <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                            <MarkdownContent content={parsed.background} />
                          </div>
                        </SurfaceCard>
                      )}

                      {/* 내용 */}
                      {parsed.content && (
                        <SurfaceCard className="space-y-2" style={{ ...SURFACE_PANEL_STYLE }}>
                          <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                            {tr("내용", "Content")}
                          </div>
                          <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                            <MarkdownContent content={parsed.content} />
                          </div>
                        </SurfaceCard>
                      )}

                      {/* DoD Checklist */}
                      {editor.review_checklist.length > 0 && (() => {
                        const isGitHubLinked = Boolean(selectedCard.github_issue_number);
                        return (
                          <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE, borderColor: "rgba(20,184,166,0.3)" }}>
                            <div className="flex items-center justify-between gap-3">
                              <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#2dd4bf" }}>
                                DoD (Definition of Done)
                                {isGitHubLinked && (
                                  <span className="ml-2 text-[9px] font-normal normal-case tracking-normal" style={{ color: "var(--th-text-muted)" }}>
                                    {tr("(GitHub 정본)", "(synced from GitHub)")}
                                  </span>
                                )}
                              </div>
                              <SurfaceMetricPill
                                label={tr("완료", "Done")}
                                value={`${editor.review_checklist.filter((item) => item.done).length}/${editor.review_checklist.length}`}
                                tone="success"
                                className="min-w-[92px] px-2.5 py-1.5"
                              />
                            </div>
                            <div className="space-y-2">
                              {editor.review_checklist.map((item) => (
                                <label
                                  key={item.id}
                                  className="flex items-center gap-3 rounded-2xl border px-3 py-2.5"
                                  style={{
                                    borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                                    backgroundColor: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
                                    opacity: isGitHubLinked ? 0.85 : 1,
                                  }}
                                >
                                  <input
                                    type="checkbox"
                                    checked={item.done}
                                    disabled={isGitHubLinked}
                                    onChange={isGitHubLinked ? undefined : (event) => setEditor((prev) => ({
                                      ...prev,
                                      review_checklist: prev.review_checklist.map((current) =>
                                        current.id === item.id ? { ...current, done: event.target.checked } : current,
                                      ),
                                    }))}
                                  />
                                  <span
                                    className="min-w-0 flex-1 text-sm"
                                    style={{
                                      color: item.done ? "var(--th-text-secondary)" : "var(--th-text-primary)",
                                      textDecoration: item.done ? "line-through" : "none",
                                    }}
                                  >
                                    {item.label}
                                  </span>
                                </label>
                              ))}
                            </div>
                          </SurfaceCard>
                        );
                      })()}

                      {/* 의존성 */}
                      {parsed.dependencies && (
                        <SurfaceNotice tone="info" className="items-start">
                          <div className="space-y-1.5">
                            <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#93c5fd" }}>
                              {tr("의존성", "Dependencies")}
                            </div>
                            <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                              <MarkdownContent content={parsed.dependencies} />
                            </div>
                          </div>
                        </SurfaceNotice>
                      )}

                      {/* 리스크 */}
                      {parsed.risks && (
                        <SurfaceNotice tone="danger" className="items-start">
                          <div className="space-y-1.5">
                            <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#fca5a5" }}>
                              {tr("리스크", "Risks")}
                            </div>
                            <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                              <MarkdownContent content={parsed.risks} />
                            </div>
                          </div>
                        </SurfaceNotice>
                      )}
                    </div>
                  );
                })()}

                {canRedispatchCard(selectedCard) && (
                  <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div>
                      <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                        {tr("이슈 변경 후 재전송", "Resend with Updated Issue")}
                      </h4>
                      <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {tr(
                          "이슈 본문을 수정한 뒤, 기존 dispatch를 취소하고 새로 전송합니다.",
                          "Cancel current dispatch and resend with the updated issue body.",
                        )}
                      </p>
                    </div>
                    <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto]">
                      <input
                        type="text"
                        placeholder={tr("사유 (선택)", "Reason (optional)")}
                        value={redispatchReason}
                        onChange={(e) => setRedispatchReason(e.target.value)}
                        className="w-full rounded-xl border px-3 py-2 text-sm"
                        style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                      />
                      <SurfaceActionButton
                        type="button"
                        onClick={() => void handleRedispatch()}
                        disabled={redispatching}
                        tone="warn"
                        className="whitespace-nowrap px-4 py-2 text-sm"
                        style={{ background: "#d97706", borderColor: "#d97706", color: "white" }}
                      >
                        {redispatching ? tr("전송 중...", "Sending...") : tr("재전송", "Resend")}
                      </SurfaceActionButton>
                    </div>
                  </SurfaceCard>
                )}

                {canRetryCard(selectedCard) && (
                  <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div>
                      <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                        {tr("재시도 / 담당자 변경", "Retry / Change Assignee")}
                      </h4>
                      <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {tr("동일 내용으로 재전송하거나 다른 에이전트에게 전환합니다.", "Resend as-is or switch to another agent.")}
                      </p>
                    </div>
                    <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto]">
                      <select
                        value={retryAssigneeId}
                        onChange={(event) => setRetryAssigneeId(event.target.value)}
                        className="w-full rounded-xl border px-3 py-2 text-sm"
                        style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                      >
                        {agents.map((agent) => (
                          <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                        ))}
                      </select>
                      <SurfaceActionButton
                        type="button"
                        onClick={() => void handleRetryCard()}
                        disabled={retryingCard || !(retryAssigneeId || selectedCard.assignee_agent_id)}
                        tone="accent"
                        className="whitespace-nowrap px-4 py-2 text-sm"
                        style={{ background: "#7c3aed", borderColor: "#7c3aed", color: "white" }}
                      >
                        {retryingCard ? tr("전송 중...", "Sending...") : tr("재시도", "Retry")}
                      </SurfaceActionButton>
                    </div>
                  </SurfaceCard>
                )}

                <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4 text-sm">
                  <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("생성", "Created")}</div>
                    <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.created_at, locale)}</div>
                  </SurfaceCard>
                  <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("요청", "Requested")}</div>
                    <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.requested_at, locale)}</div>
                  </SurfaceCard>
                  <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("시작", "Started")}</div>
                    <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.started_at, locale)}</div>
                  </SurfaceCard>
                  <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("완료", "Completed")}</div>
                    <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedCard.completed_at, locale)}</div>
                  </SurfaceCard>
                </div>

                {/* Dispatch history — all dispatches for this card */}
                {(() => {
                  const cardDispatches = dispatches
                    .filter((d) => d.kanban_card_id === selectedCard.id)
                    .sort((a, b) => {
                      const ta = typeof a.created_at === "number" ? a.created_at : new Date(a.created_at).getTime();
                      const tb = typeof b.created_at === "number" ? b.created_at : new Date(b.created_at).getTime();
                      return tb - ta;
                    });
                  const hasAny = cardDispatches.length > 0 || selectedCard.latest_dispatch_status;
                  if (!hasAny) return null;

                  const dispatchStatusColor: Record<string, string> = {
                    pending: "#fbbf24",
                    dispatched: "#38bdf8",
                    in_progress: "#f59e0b",
                    completed: "#4ade80",
                    failed: "#f87171",
                    cancelled: "#9ca3af",
                  };

                  return (
                    <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE }}>
                      <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                        {tr("Dispatch 이력", "Dispatch history")}
                        {cardDispatches.length > 0 && (
                          <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
                            ({cardDispatches.length})
                          </span>
                        )}
                      </h4>
                      {parseCardMetadata(selectedCard.metadata_json).timed_out_reason && (
                        <SurfaceNotice tone="warn" compact>
                          {parseCardMetadata(selectedCard.metadata_json).timed_out_reason}
                        </SurfaceNotice>
                      )}
                      {cardDispatches.length > 0 ? (
                        <div className="space-y-2 max-h-64 overflow-y-auto">
                          {cardDispatches.map((d) => (
                            <SurfaceCard
                              key={d.id}
                              className="space-y-2 p-3 text-sm"
                              style={{ borderColor: "rgba(148,163,184,0.12)", backgroundColor: d.id === selectedCard.latest_dispatch_id ? "rgba(37,99,235,0.08)" : "transparent" }}
                            >
                              <div className="flex items-center gap-2 flex-wrap">
                                <span
                                  className="inline-block w-2 h-2 rounded-full shrink-0"
                                  style={{ backgroundColor: dispatchStatusColor[d.status] ?? "#94a3b8" }}
                                />
                                <span className="font-mono text-xs" style={{ color: "var(--th-text-muted)" }}>
                                  #{d.id.slice(0, 8)}
                                </span>
                                <span
                                  className="px-1.5 py-0.5 rounded text-[10px] font-medium"
                                  style={{ backgroundColor: "rgba(148,163,184,0.12)", color: dispatchStatusColor[d.status] ?? "#94a3b8" }}
                                >
                                  {d.status}
                                </span>
                                {d.dispatch_type && (
                                  <span className="px-1.5 py-0.5 rounded text-[10px]" style={{ backgroundColor: "rgba(148,163,184,0.08)", color: "var(--th-text-secondary)" }}>
                                    {d.dispatch_type}
                                  </span>
                                )}
                                {d.to_agent_id && (
                                  <span className="text-xs" style={{ color: "var(--th-text-secondary)" }}>
                                    → {getAgentLabel(d.to_agent_id)}
                                  </span>
                                )}
                              </div>
                              <div className="flex items-center gap-3 mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                                <span>{formatIso(d.created_at, locale)}</span>
                                {d.chain_depth > 0 && <span>depth {d.chain_depth}</span>}
                              </div>
                              {(() => {
                                const dispatchSummary = formatDispatchSummary(d.result_summary);
                                if (!dispatchSummary) return null;
                                return (
                                  <SurfaceNotice
                                    compact
                                    className="mt-1 whitespace-pre-wrap break-words"
                                    style={{ color: "var(--th-text-secondary)" }}
                                  >
                                    {dispatchSummary}
                                  </SurfaceNotice>
                                );
                              })()}
                            </SurfaceCard>
                        ))}
                      </div>
                      ) : (
                        <div className="grid gap-2 md:grid-cols-2 text-sm">
                          <div>{tr("dispatch 상태", "Dispatch status")}: {selectedCard.latest_dispatch_status ?? "-"}</div>
                          <div>{tr("최신 dispatch", "Latest dispatch")}: {selectedCard.latest_dispatch_id ? `#${selectedCard.latest_dispatch_id.slice(0, 8)}` : "-"}</div>
                        </div>
                      )}
                    </SurfaceCard>
                  );
                })()}

                {/* State transition history (audit log) */}
                {auditLog.length > 0 && (
                  <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE }}>
                    <h4 className="font-medium" style={{ color: "var(--th-text-heading)" }}>
                      {tr("상태 전환 이력", "State Transition History")}
                      <span className="ml-2 text-xs font-normal" style={{ color: "var(--th-text-muted)" }}>
                        ({auditLog.length})
                      </span>
                    </h4>
                    <div className="space-y-1.5 max-h-48 overflow-y-auto">
                      {auditLog.map((log) => {
                        const resultPresentation = formatAuditResult(log.result, tr);
                        return (
                          <SurfaceCard
                            key={log.id}
                            className="space-y-1.5 p-3 text-xs"
                            style={{ backgroundColor: "rgba(255,255,255,0.03)" }}
                          >
                            <div className="flex items-center gap-2">
                              <span className="shrink-0" style={{ color: "var(--th-text-muted)" }}>
                                {formatIso(log.created_at, locale)}
                              </span>
                              <span
                                className="ml-auto px-1.5 py-0.5 rounded text-[10px]"
                                style={{ backgroundColor: "rgba(148,163,184,0.12)", color: "var(--th-text-muted)" }}
                              >
                                {log.source}
                              </span>
                            </div>
                            <div className="flex items-center gap-2 flex-wrap">
                              <span style={{ color: TRANSITION_STYLE[log.from_status ?? ""]?.text ?? "var(--th-text-secondary)" }}>
                                {log.from_status ? labelForStatus(log.from_status as KanbanCardStatus, tr) : "—"}
                              </span>
                              <span style={{ color: "var(--th-text-muted)" }}>→</span>
                              <span style={{ color: TRANSITION_STYLE[log.to_status ?? ""]?.text ?? "var(--th-text-secondary)" }}>
                                {log.to_status ? labelForStatus(log.to_status as KanbanCardStatus, tr) : "—"}
                              </span>
                            </div>
                            {resultPresentation && (
                              <div
                                className="rounded-md border px-2 py-1.5 text-[11px] leading-relaxed whitespace-pre-wrap break-words"
                                style={ACTIVITY_RESULT_TONE_STYLE[resultPresentation.tone]}
                              >
                                {resultPresentation.text}
                              </div>
                            )}
                          </SurfaceCard>
                        );
                      })}
                    </div>
                  </SurfaceCard>
                )}

                {/* Unified GitHub comment timeline */}
                <CardTimeline
                  ghComments={ghComments}
                  timelineFilter={timelineFilter}
                  setTimelineFilter={setTimelineFilter}
                  tr={tr}
                  locale={locale}
                  onRefresh={() => selectedCard && invalidateCardActivity(selectedCard.id)}
                />

                <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
                  <div className="flex gap-2">
                    <SurfaceActionButton
                      onClick={handleDeleteCard}
                      disabled={savingCard}
                      tone="danger"
                      className="px-4 py-2 text-sm"
                      style={{ color: "#fecaca", background: "rgba(127,29,29,0.32)", borderColor: "rgba(239,68,68,0.28)" }}
                    >
                      {tr("카드 삭제", "Delete card")}
                    </SurfaceActionButton>
                    {selectedCard.status !== "done" && (
                      <SurfaceActionButton
                        onClick={() => setCancelConfirm({ cardIds: [selectedCard.id], source: "single" })}
                        disabled={savingCard}
                        tone="neutral"
                        className="px-4 py-2 text-sm"
                        style={{ color: "#9ca3af", background: "rgba(107,114,128,0.18)" }}
                      >
                        {tr("카드 취소", "Cancel card")}
                      </SurfaceActionButton>
                    )}
                  </div>
                  <div className="flex flex-col-reverse gap-2 sm:flex-row">
                    <SurfaceActionButton
                      onClick={() => setSelectedCardId(null)}
                      tone="neutral"
                      className="px-4 py-2 text-sm"
                      style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
                    >
                      {tr("닫기", "Close")}
                    </SurfaceActionButton>
                    <SurfaceActionButton
                      onClick={() => void handleSaveCard()}
                      disabled={savingCard || !editor.title.trim()}
                      tone="accent"
                      className="px-4 py-2 text-sm"
                      style={{ background: "#2563eb", borderColor: "#2563eb", color: "white" }}
                    >
                      {savingCard ? tr("저장 중", "Saving") : tr("저장", "Save")}
                    </SurfaceActionButton>
                  </div>
                </div>
              </div>
            </div>
          </div>
        )}
      </div>

      {assignIssue && (
        <div className="fixed inset-0 z-50 flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }}>
          <SurfaceCard
            className="w-full max-w-lg rounded-t-3xl p-5 sm:rounded-3xl sm:p-6 space-y-4"
            style={{
              ...SURFACE_MODAL_CARD_STYLE,
              paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
            }}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0 space-y-2">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                    {selectedRepo}
                  </span>
                  <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                    #{assignIssue.number}
                  </span>
                </div>
                <h3 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {assignIssue.title}
                </h3>
              </div>
              <SurfaceActionButton
                onClick={() => setAssignIssue(null)}
                tone="neutral"
                className="shrink-0 whitespace-nowrap"
                style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
              >
                {tr("닫기", "Close")}
              </SurfaceActionButton>
            </div>

            <SurfaceNotice tone="info" compact>
              {tr("할당 시 카드는 ready로 생성되며, 저장된 repo 기본 담당자가 있으면 미리 선택됩니다.", "Assigning creates a ready card and preselects the repo default assignee when available.")}
            </SurfaceNotice>

            <SurfaceCard className="space-y-2 p-4" style={{ ...SURFACE_PANEL_STYLE }}>
              <label className="space-y-2 block">
                <span className="text-xs font-medium" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignee")}</span>
                <select
                  value={assignAssigneeId}
                  onChange={(event) => setAssignAssigneeId(event.target.value)}
                  className="w-full rounded-xl border px-3 py-2 text-sm"
                  style={{ ...SURFACE_FIELD_STYLE, color: "var(--th-text-primary)" }}
                >
                  <option value="">{tr("에이전트 선택", "Select an agent")}</option>
                  {agents.map((agent) => (
                    <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                  ))}
                </select>
              </label>
            </SurfaceCard>

            <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
              <SurfaceActionButton
                onClick={() => setAssignIssue(null)}
                tone="neutral"
                className="px-4 py-2 text-sm"
                style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
              >
                {tr("취소", "Cancel")}
              </SurfaceActionButton>
              <SurfaceActionButton
                onClick={() => void handleAssignIssue()}
                disabled={assigningIssue || !assignAssigneeId}
                tone="accent"
                className="px-4 py-2 text-sm"
                style={{ backgroundColor: "#2563eb", borderColor: "#2563eb", color: "white" }}
              >
                {assigningIssue ? tr("할당 중", "Assigning") : tr("ready로 할당", "Assign to ready")}
              </SurfaceActionButton>
            </div>
          </SurfaceCard>
        </div>
      )}

      {selectedBacklogIssue && (
        <div className="fixed inset-0 z-50 flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={() => setSelectedBacklogIssue(null)}>
          <SurfaceCard
            onClick={(e) => e.stopPropagation()}
            className="w-full max-w-3xl max-h-[88svh] overflow-y-auto rounded-t-3xl p-5 sm:max-h-[90vh] sm:rounded-3xl sm:p-6 space-y-4"
            style={{
              ...SURFACE_MODAL_CARD_STYLE,
              paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
            }}
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="rounded-full border px-2 py-0.5 text-xs" style={{ ...SURFACE_CHIP_STYLE, color: "var(--th-text-secondary)" }}>
                    #{selectedBacklogIssue.number}
                  </span>
                  <span className="px-2 py-0.5 rounded-full text-xs" style={{ backgroundColor: "#64748b33", color: "#64748b" }}>
                    {tr("백로그", "Backlog")}
                  </span>
                  {selectedBacklogIssue.labels.map((label) => (
                    <span
                      key={label.name}
                      className="px-2 py-0.5 rounded-full text-xs"
                      style={{ backgroundColor: `#${label.color}22`, color: `#${label.color}` }}
                    >
                      {label.name}
                    </span>
                  ))}
                </div>
                <h3 className="mt-2 text-xl font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {selectedBacklogIssue.title}
                </h3>
              </div>
              <SurfaceActionButton
                onClick={() => setSelectedBacklogIssue(null)}
                tone="neutral"
                className="shrink-0"
                style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "var(--th-text-secondary)" }}
              >
                {tr("닫기", "Close")}
              </SurfaceActionButton>
            </div>

            {selectedBacklogIssue.assignees.length > 0 && (
              <SurfaceNotice tone="info" compact className="items-start">
                <div className="flex flex-wrap items-center gap-2 text-sm" style={{ color: "var(--th-text-secondary)" }}>
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignees")}:</span>
                  {selectedBacklogIssue.assignees.map((a) => (
                    <span key={a.login} className="rounded-full border px-2 py-0.5 text-xs" style={SURFACE_CHIP_STYLE}>{a.login}</span>
                  ))}
                </div>
              </SurfaceNotice>
            )}

            <div className="grid gap-3 md:grid-cols-2 text-sm">
              <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("생성", "Created")}</div>
                <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedBacklogIssue.createdAt, locale)}</div>
              </SurfaceCard>
              <SurfaceCard className="space-y-1.5 p-3" style={{ ...SURFACE_PANEL_STYLE }}>
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("업데이트", "Updated")}</div>
                <div style={{ color: "var(--th-text-primary)" }}>{formatIso(selectedBacklogIssue.updatedAt, locale)}</div>
              </SurfaceCard>
            </div>

            {(() => {
              const parsed = parseIssueSections(selectedBacklogIssue.body);
              if (!parsed) {
                // Fallback: non-PMD format
                return selectedBacklogIssue.body ? (
                  <SurfaceCard className="text-sm" style={{ ...SURFACE_PANEL_STYLE, color: "var(--th-text-primary)" }}>
                    <MarkdownContent content={selectedBacklogIssue.body} />
                  </SurfaceCard>
                ) : (
                  <SurfaceEmptyState className="px-3 py-4 text-center text-xs">
                    {tr("이슈 본문이 없습니다.", "No issue body.")}
                  </SurfaceEmptyState>
                );
              }
              // Structured view for PMD-format issues
              return (
                <div className="space-y-3">
                  {parsed.background && (
                    <SurfaceCard className="space-y-2" style={{ ...SURFACE_PANEL_STYLE }}>
                      <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                        {tr("배경", "Background")}
                      </div>
                      <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                        <MarkdownContent content={parsed.background} />
                      </div>
                    </SurfaceCard>
                  )}
                  {parsed.content && (
                    <SurfaceCard className="space-y-2" style={{ ...SURFACE_PANEL_STYLE }}>
                      <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "var(--th-text-muted)" }}>
                        {tr("내용", "Content")}
                      </div>
                      <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                        <MarkdownContent content={parsed.content} />
                      </div>
                    </SurfaceCard>
                  )}
                  {parsed.dodItems.length > 0 && (
                    <SurfaceCard className="space-y-3" style={{ ...SURFACE_PANEL_STYLE, borderColor: "rgba(20,184,166,0.3)" }}>
                      <div className="flex items-center justify-between gap-3">
                        <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#2dd4bf" }}>
                          DoD (Definition of Done)
                        </div>
                        <SurfaceMetricPill
                          label={tr("항목", "Items")}
                          value={`${parsed.dodItems.length}`}
                          tone="success"
                          className="min-w-[76px] px-2.5 py-1.5"
                        />
                      </div>
                      <div className="space-y-1.5">
                        {parsed.dodItems.map((item, idx) => (
                          <div
                            key={idx}
                            className="flex items-center gap-2 rounded-2xl border px-3 py-2.5 text-sm"
                            style={{
                              color: "var(--th-text-primary)",
                              borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                              backgroundColor: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
                            }}
                          >
                            <span className="text-[10px]" style={{ color: "var(--th-text-muted)" }}>☐</span>
                            {item}
                          </div>
                        ))}
                      </div>
                    </SurfaceCard>
                  )}
                  {parsed.dependencies && (
                    <SurfaceNotice tone="info" className="items-start">
                      <div className="space-y-1.5">
                        <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#93c5fd" }}>
                          {tr("의존성", "Dependencies")}
                        </div>
                        <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                          <MarkdownContent content={parsed.dependencies} />
                        </div>
                      </div>
                    </SurfaceNotice>
                  )}
                  {parsed.risks && (
                    <SurfaceNotice tone="danger" className="items-start">
                      <div className="space-y-1.5">
                        <div className="text-[10px] font-semibold uppercase tracking-widest" style={{ color: "#fca5a5" }}>
                          {tr("리스크", "Risks")}
                        </div>
                        <div className="text-sm" style={{ color: "var(--th-text-primary)" }}>
                          <MarkdownContent content={parsed.risks} />
                        </div>
                      </div>
                    </SurfaceNotice>
                  )}
                </div>
              );
            })()}

            <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
              <a
                href={selectedBacklogIssue.url}
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center justify-center rounded-xl border px-4 py-2 text-sm text-center transition-colors hover:brightness-110"
                style={{ ...SURFACE_GHOST_BUTTON_STYLE, color: "#93c5fd" }}
              >
                {tr("GitHub에서 보기", "View on GitHub")}
              </a>
              <div className="flex flex-col-reverse gap-2 sm:flex-row">
                <SurfaceActionButton
                  onClick={() => {
                    setSelectedBacklogIssue(null);
                    void handleCloseIssue(selectedBacklogIssue);
                  }}
                  disabled={closingIssueNumber === selectedBacklogIssue.number}
                  tone="neutral"
                  className="px-4 py-2 text-sm"
                  style={{ borderColor: "rgba(148,163,184,0.28)", color: "var(--th-text-muted)" }}
                >
                  {closingIssueNumber === selectedBacklogIssue.number ? tr("닫는 중", "Closing") : tr("이슈 닫기", "Close issue")}
                </SurfaceActionButton>
                <SurfaceActionButton
                  onClick={() => {
                    setSelectedBacklogIssue(null);
                    setAssignIssue(selectedBacklogIssue);
                    const repoSource = repoSources.find((s) => s.repo === selectedRepo);
                    setAssignAssigneeId(repoSource?.default_agent_id ?? "");
                  }}
                  tone="accent"
                  className="px-4 py-2 text-sm"
                  style={{ backgroundColor: "#2563eb", borderColor: "#2563eb", color: "white" }}
                >
                  {tr("할당", "Assign")}
                </SurfaceActionButton>
              </div>
            </div>
          </SurfaceCard>
        </div>
      )}
    </div>
  );
}
