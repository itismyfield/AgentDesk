import { useEffect, useMemo, useRef, useState, type DragEvent } from "react";
import * as api from "../../api";
import type { GitHubIssue, GitHubRepoOption, KanbanRepoSource } from "../../api";
import BacklogIssueDetail from "./BacklogIssueDetail";
import KanbanBoard from "./KanbanBoard";
import KanbanCardDetail from "./KanbanCardDetail";
import KanbanPipelinePanel from "./KanbanPipelinePanel";
import KanbanSettings from "./KanbanSettings";
import type {
  Agent,
  Department,
  KanbanCard,
  KanbanCardStatus,
  TaskDispatch,
  UiLanguage,
} from "../../types";
import type { KanbanReview } from "../../api";
import { localeName } from "../../i18n";
import {
  COLUMN_DEFS,
  EMPTY_EDITOR,
  QA_STATUSES,
  TERMINAL_STATUSES,
  coerceEditor,
  formatIso,
  formatTs,
  isReviewCard,
  labelForStatus,
  parseCardMetadata,
  parseIssueSections,
  priorityLabel,
  type EditorState,
} from "./kanban-utils";

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
}


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
}: KanbanTabProps) {
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
  const [search, setSearch] = useState("");
  const [showClosed, setShowClosed] = useState(false);
  const [selectedCardId, setSelectedCardId] = useState<string | null>(null);
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
  const [draggingCardId, setDraggingCardId] = useState<string | null>(null);
  const [dragOverStatus, setDragOverStatus] = useState<KanbanCardStatus | null>(null);
  const [dragOverCardId, setDragOverCardId] = useState<string | null>(null);
  const [compactBoard, setCompactBoard] = useState(false);
  const [mobileColumnStatus, setMobileColumnStatus] = useState<KanbanCardStatus>("backlog");
  const [retryAssigneeId, setRetryAssigneeId] = useState("");
  const [newChecklistItem, setNewChecklistItem] = useState("");
  const [closingIssueNumber, setClosingIssueNumber] = useState<number | null>(null);
  const [selectedBacklogIssue, setSelectedBacklogIssue] = useState<GitHubIssue | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [reviewData, setReviewData] = useState<KanbanReview | null>(null);
  const [reviewDecisions, setReviewDecisions] = useState<Record<string, "accept" | "reject">>({});
  const [recentDonePage, setRecentDonePage] = useState(0);
  const [recentDoneOpen, setRecentDoneOpen] = useState(false);
  const [stalledPopup, setStalledPopup] = useState(false);
  const [stalledSelected, setStalledSelected] = useState<Set<string>>(new Set());
  const [bulkBusy, setBulkBusy] = useState(false);
  const [deferredDodPopup, setDeferredDodPopup] = useState(false);
  const [assignBeforeReady, setAssignBeforeReady] = useState<{ cardId: string; agentId: string } | null>(null);
  const [cancelConfirm, setCancelConfirm] = useState<{ cardIds: string[]; source: "bulk" | "single" } | null>(null);
  const [cancelBusy, setCancelBusy] = useState(false);
  const [auditLog, setAuditLog] = useState<api.CardAuditLogEntry[]>([]);
  const [ghComments, setGhComments] = useState<api.GitHubComment[]>([]);
  const [timelineFilter, setTimelineFilter] = useState<"review" | "pm" | "work" | "general" | null>(null);
  const [activityRefreshTick, setActivityRefreshTick] = useState(0);
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
    const media = window.matchMedia("(max-width: 767px)");
    const apply = () => setCompactBoard(media.matches);
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
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

  const getAgentLabel = (agentId: string | null | undefined) => {
    if (!agentId) return tr("미할당", "Unassigned");
    const agent = agentMap.get(agentId);
    if (!agent) return agentId;
    return localeName(locale, agent);
  };

  const repoCards = useMemo(() => {
    if (!selectedRepo) return [] as KanbanCard[];
    return cards.filter((card) => card.github_repo === selectedRepo);
  }, [cards, selectedRepo]);

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
      if (!needle) return true;
      return (
        card.title.toLowerCase().includes(needle) ||
        (card.description ?? "").toLowerCase().includes(needle) ||
        getAgentLabel(card.assignee_agent_id).toLowerCase().includes(needle)
      );
    });
  }, [agentFilter, agentMap, cardTypeFilter, deptFilter, getAgentLabel, repoCards, search, selectedAgentId, showClosed]);

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
      accent: "#e879f9",
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
    for (const column of effectiveColumnDefs) {
      grouped.get(column.status)?.sort((a, b) => {
        if (a.sort_order !== b.sort_order) return a.sort_order - b.sort_order;
        return b.updated_at - a.updated_at;
      });
    }
    return grouped;
  }, [filteredCards, effectiveColumnDefs]);

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
  const openCount = filteredCards.filter((card) => !TERMINAL_STATUSES.has(card.status)).length + backlogIssues.length;
  const hasQaCards = filteredCards.some((c) => QA_STATUSES.has(c.status));
  const visibleColumns = compactBoard
    ? effectiveColumnDefs.filter((column) => column.status === mobileColumnStatus)
    : effectiveColumnDefs.filter((column) =>
        (showClosed || !TERMINAL_STATUSES.has(column.status))
        && (!QA_STATUSES.has(column.status) || hasQaCards),
      );

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

  const handleDrop = async (
    targetStatus: KanbanCardStatus,
    beforeCardId: string | null,
    event: DragEvent<HTMLElement>,
  ) => {
    event.preventDefault();
    setDragOverStatus(null);
    setDragOverCardId(null);
    setActionError(null);

    // --- Backlog issue drop ---
    const issueJson = event.dataTransfer.getData("application/x-backlog-issue");
    if (issueJson) {
      setDraggingCardId(null);
      if (targetStatus === "backlog") return; // no-op: dropped back on backlog
      try {
        const issue = JSON.parse(issueJson) as GitHubIssue;
        const autoAgent = resolveAgentFromLabels(issue.labels);
        if (autoAgent) {
          await handleDirectAssignIssue(issue, autoAgent.id);
        } else {
          // Open modal for manual agent selection
          setAssignIssue(issue);
          const repoSource = repoSources.find((s) => s.repo === selectedRepo);
          setAssignAssigneeId(repoSource?.default_agent_id ?? "");
        }
      } catch (error) {
        setActionError(error instanceof Error ? error.message : tr("이슈 할당에 실패했습니다.", "Failed to assign issue."));
      }
      return;
    }

    // --- Existing card drag ---
    const draggedId = draggingCardId;
    setDraggingCardId(null);
    if (!draggedId) return;
    if (beforeCardId === draggedId) return;
    try {
      if (targetStatus === "requested") {
        const card = cardsById.get(draggedId);
        await api.createDispatch({
          kanban_card_id: draggedId,
          to_agent_id: card?.assignee_agent_id ?? "",
          title: card?.title ?? "Dispatch",
        });
        window.location.reload();
      } else {
        await onUpdateCard(draggedId, {
          status: targetStatus,
          before_card_id: beforeCardId,
        });
        invalidateCardActivity(draggedId);
      }
    } catch (error) {
      setActionError(error instanceof Error ? error.message : tr("카드 이동에 실패했습니다.", "Failed to move card."));
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

  return (
    <div className="space-y-4 pb-24 md:pb-0 min-w-0 overflow-x-hidden" style={{ paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))" }}>
      <section
        className="rounded-2xl border p-4 sm:p-5 space-y-4 min-w-0 overflow-hidden"
        style={{
          background: "linear-gradient(135deg, var(--th-bg-surface), var(--th-bg-surface-hover))",
          borderColor: "rgba(148,163,184,0.28)",
        }}
      >
        {/* Row 1: 칸반 title + count + stalled + settings (settings always right-aligned) */}
        <div className="flex items-center justify-between gap-2 min-w-0">
          <div className="flex items-center gap-2 min-w-0">
            <h2 className="text-base font-semibold shrink-0" style={{ color: "var(--th-text-heading)" }}>
              {tr("칸반", "Kanban")}
            </h2>
            <span className="text-xs shrink-0 px-2 py-0.5 rounded-full bg-surface-medium" style={{ color: "var(--th-text-muted)" }}>
              {initialLoading ? "…" : `${openCount}${tr("건", "")}`}
            </span>
            {stalledCards.length > 0 && (
              <button
                onClick={() => { setStalledPopup(true); setStalledSelected(new Set()); }}
                className="shrink-0 text-xs px-3 py-2 rounded-full font-medium animate-pulse"
                style={{ backgroundColor: "rgba(239,68,68,0.2)", color: "#f87171", border: "1px solid rgba(239,68,68,0.4)", minHeight: 44 }}
              >
                {tr(`정체 ${stalledCards.length}건`, `${stalledCards.length} stalled`)}
              </button>
            )}
            {(() => {
              const deferredCount = cards.reduce((sum, c) => {
                const meta = parseCardMetadata(c.metadata_json);
                return sum + (meta.deferred_dod?.filter((d) => !d.verified).length ?? 0);
              }, 0);
              return deferredCount > 0 ? (
                <button
                  onClick={() => setDeferredDodPopup(true)}
                  className="shrink-0 text-xs px-3 py-2 rounded-full font-medium"
                  style={{ backgroundColor: "rgba(245,158,11,0.2)", color: "#fbbf24", border: "1px solid rgba(245,158,11,0.4)", minHeight: 44 }}
                >
                  {tr(`미검증 DoD ${deferredCount}건`, `${deferredCount} deferred DoD`)}
                </button>
              ) : null;
            })()}
          </div>
          {/* Desktop-only inline repo tabs + agent selector */}
          <div className="hidden sm:flex items-center gap-1.5 overflow-x-auto min-w-0">
            {repoSources.length >= 1 && repoSources.map((source) => (
              <button
                key={source.id}
                onClick={() => setSelectedRepo(source.repo)}
                className="shrink-0 text-xs px-2.5 py-1.5 rounded-full border truncate max-w-[160px]"
                style={{
                  borderColor: selectedRepo === source.repo ? "rgba(59,130,246,0.6)" : "rgba(148,163,184,0.22)",
                  backgroundColor: selectedRepo === source.repo ? "rgba(59,130,246,0.25)" : "transparent",
                  color: selectedRepo === source.repo ? "#3b82f6" : "var(--th-text-muted)",
                }}
              >
                {source.repo.split("/")[1] ?? source.repo}
              </button>
            ))}
            {selectedRepo && (() => {
              const agentEntries = Array.from(repoAgentCounts.entries()).sort((a, b) => b[1] - a[1]);
              if (agentEntries.length <= 1) return null;
              if (agentEntries.length <= 4) {
                return (<>
                  {repoSources.length > 1 && <span className="text-slate-600 mx-0.5">|</span>}
                  <button
                    onClick={() => setSelectedAgentId(null)}
                    className="shrink-0 text-xs px-2.5 py-1.5 rounded-full border"
                    style={{
                      borderColor: !selectedAgentId ? "rgba(139,92,246,0.6)" : "rgba(148,163,184,0.22)",
                      backgroundColor: !selectedAgentId ? "rgba(139,92,246,0.25)" : "transparent",
                      color: !selectedAgentId ? "#7c3aed" : "var(--th-text-muted)",
                    }}
                  >
                    {tr(`전체`, `All`)}
                  </button>
                  {agentEntries.map(([aid, count]) => (
                    <button
                      key={aid}
                      onClick={() => setSelectedAgentId(aid)}
                      className="shrink-0 text-xs px-2.5 py-1.5 rounded-full border truncate max-w-[140px]"
                      style={{
                        borderColor: selectedAgentId === aid ? "rgba(139,92,246,0.6)" : "rgba(148,163,184,0.22)",
                        backgroundColor: selectedAgentId === aid ? "rgba(139,92,246,0.25)" : "transparent",
                        color: selectedAgentId === aid ? "#7c3aed" : "var(--th-text-muted)",
                      }}
                    >
                      {getAgentLabel(aid)} ({count})
                    </button>
                  ))}
                </>);
              }
              return (
                <select
                  value={selectedAgentId ?? ""}
                  onChange={(e) => setSelectedAgentId(e.target.value || null)}
                  className="text-xs px-2.5 py-1.5 rounded-lg border bg-transparent min-w-0 max-w-[180px]"
                  style={{
                    borderColor: selectedAgentId ? "rgba(139,92,246,0.6)" : "rgba(148,163,184,0.22)",
                    color: selectedAgentId ? "#7c3aed" : "var(--th-text-muted)",
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
          <button
            onClick={() => setSettingsOpen((prev) => !prev)}
            className="shrink-0 rounded-lg px-3 py-2 text-xs border"
            style={{
              borderColor: settingsOpen ? "rgba(96,165,250,0.5)" : "rgba(148,163,184,0.22)",
              color: settingsOpen ? "#93c5fd" : "var(--th-text-muted)",
              backgroundColor: settingsOpen ? "rgba(59,130,246,0.12)" : "transparent",
              minHeight: 44,
            }}
          >
            {settingsOpen ? tr("접기", "Close") : tr("설정", "Settings")}
          </button>
        </div>

        {/* Row 2 (mobile only): Repo tabs + Agent selector — on desktop these are in Row 1 */}
        <div className="flex gap-1.5 overflow-x-auto min-w-0 -mt-1 sm:hidden">
          {repoSources.length >= 1 && repoSources.map((source) => (
            <button
              key={source.id}
              onClick={() => setSelectedRepo(source.repo)}
              className="shrink-0 text-xs px-3 py-2 rounded-full border truncate max-w-[180px]"
              style={{
                borderColor: selectedRepo === source.repo ? "rgba(59,130,246,0.6)" : "rgba(148,163,184,0.22)",
                backgroundColor: selectedRepo === source.repo ? "rgba(59,130,246,0.25)" : "transparent",
                color: selectedRepo === source.repo ? "#3b82f6" : "var(--th-text-muted)",
                minHeight: 44,
              }}
            >
              {source.repo.split("/")[1] ?? source.repo}
            </button>
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
              <div className="flex gap-1.5 overflow-x-auto min-w-0 -mt-1">
                <button
                  onClick={() => setSelectedAgentId(null)}
                  className="shrink-0 text-xs px-3 py-2 rounded-full border"
                  style={{
                    borderColor: !selectedAgentId ? "rgba(139,92,246,0.6)" : "rgba(148,163,184,0.22)",
                    backgroundColor: !selectedAgentId ? "rgba(139,92,246,0.25)" : "transparent",
                    color: !selectedAgentId ? "#7c3aed" : "var(--th-text-muted)",
                    minHeight: 44,
                  }}
                >
                  {tr(`전체 (${repoCards.length})`, `All (${repoCards.length})`)}
                </button>
                {agentEntries.map(([aid, count]) => (
                  <button
                    key={aid}
                    onClick={() => setSelectedAgentId(aid)}
                    className="shrink-0 text-xs px-3 py-2 rounded-full border truncate max-w-[160px]"
                    style={{
                      borderColor: selectedAgentId === aid ? "rgba(139,92,246,0.6)" : "rgba(148,163,184,0.22)",
                      backgroundColor: selectedAgentId === aid ? "rgba(139,92,246,0.25)" : "transparent",
                      color: selectedAgentId === aid ? "#7c3aed" : "var(--th-text-muted)",
                      minHeight: 44,
                    }}
                  >
                    {getAgentLabel(aid)} ({count})
                  </button>
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
                  borderColor: selectedAgentId ? "rgba(139,92,246,0.6)" : "rgba(148,163,184,0.22)",
                  color: selectedAgentId ? "#7c3aed" : "var(--th-text-muted)",
                  backgroundColor: selectedAgentId ? "rgba(139,92,246,0.2)" : "transparent",
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
          <KanbanSettings
            tr={tr}
            locale={locale}
            repoSources={repoSources}
            selectedRepo={selectedRepo}
            availableRepos={availableRepos}
            agents={agents}
            departments={departments}
            showClosed={showClosed}
            agentFilter={agentFilter}
            deptFilter={deptFilter}
            cardTypeFilter={cardTypeFilter}
            search={search}
            repoInput={repoInput}
            repoBusy={repoBusy}
            setSelectedRepo={setSelectedRepo}
            setShowClosed={setShowClosed}
            setAgentFilter={setAgentFilter}
            setDeptFilter={setDeptFilter}
            setCardTypeFilter={setCardTypeFilter}
            setSearch={setSearch}
            setRepoInput={setRepoInput}
            onAddRepo={handleAddRepo}
            onRemoveRepo={handleRemoveRepo}
            onUpdateRepoSource={(sourceId, patch) => {
              void api.updateKanbanRepoSource(sourceId, patch);
              setRepoSources((prev) => prev.map((s) => s.id === sourceId ? { ...s, ...patch } : s));
            }}
            getAgentLabel={getAgentLabel}
          />
        )}

        {actionError && (
          <div className="rounded-xl px-3 py-2 text-sm border" style={{ borderColor: "rgba(248,113,113,0.45)", color: "#fecaca", backgroundColor: "rgba(127,29,29,0.22)" }}>
            {actionError}
          </div>
        )}

        {/* Assignee selection modal: shown when moving to "ready" without an assignee */}
        {assignBeforeReady && (
          <div className="fixed inset-0 z-50 backdrop-blur-sm flex items-center justify-center p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }} onClick={() => setAssignBeforeReady(null)}>
            <div onClick={(e) => e.stopPropagation()} className="w-full max-w-sm rounded-2xl border p-5 space-y-4" style={{ backgroundColor: "var(--th-bg-surface)", borderColor: "rgba(148,163,184,0.24)" }} role="dialog" aria-modal="true" aria-label="Assign agent">
              <h3 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>{tr("담당자 할당", "Assign Agent")}</h3>
              <p className="text-sm" style={{ color: "var(--th-text-secondary)" }}>{tr("준비됨 상태로 이동하려면 담당자를 지정해야 합니다.", "Assign an agent before moving to ready.")}</p>
              <select
                value={assignBeforeReady.agentId}
                onChange={(e) => setAssignBeforeReady((prev) => prev ? { ...prev, agentId: e.target.value } : null)}
                className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
                style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
              >
                <option value="">{tr("선택...", "Select...")}</option>
                {agents.map((a) => (
                  <option key={a.id} value={a.id}>{a.name_ko || a.name} ({a.id})</option>
                ))}
              </select>
              <div className="flex justify-end gap-2">
                <button onClick={() => setAssignBeforeReady(null)} className="rounded-xl px-4 py-2 text-sm bg-surface-medium" style={{ color: "var(--th-text-secondary)" }}>{tr("취소", "Cancel")}</button>
                <button
                  disabled={!assignBeforeReady.agentId}
                  onClick={async () => {
                    const { cardId, agentId } = assignBeforeReady;
                    setAssignBeforeReady(null);
                    try {
                      await onUpdateCard(cardId, { status: "ready", assignee_agent_id: agentId });
                    } catch (error) {
                      setActionError(error instanceof Error ? error.message : tr("상태 전환에 실패했습니다.", "Failed to change status."));
                    }
                  }}
                  className="rounded-xl px-4 py-2 text-sm font-medium"
                  style={{ backgroundColor: !assignBeforeReady.agentId ? "rgba(34,197,94,0.2)" : "rgba(34,197,94,0.8)", color: "#fff" }}
                >{tr("할당 후 준비됨", "Assign & Ready")}</button>
              </div>
            </div>
          </div>
        )}

        {deferredDodPopup && (() => {
          const deferredItems = cards.flatMap((c) => {
            const meta = parseCardMetadata(c.metadata_json);
            return (meta.deferred_dod ?? []).map((d) => ({ ...d, cardId: c.id, cardTitle: c.title, issueNumber: c.github_issue_number }));
          }).filter((d) => !d.verified);
          return (
            <div className="rounded-xl border p-4 space-y-3" style={{ borderColor: "rgba(245,158,11,0.35)", backgroundColor: "rgba(120,53,15,0.18)" }}>
              <div className="flex items-center justify-between">
                <span className="text-sm font-semibold" style={{ color: "#fbbf24" }}>
                  {tr(`미검증 DoD (${deferredItems.length}건)`, `Deferred DoD (${deferredItems.length})`)}
                </span>
                <button onClick={() => setDeferredDodPopup(false)} className="text-xs px-2 py-1 rounded" style={{ color: "var(--th-text-muted)" }}>
                  {tr("닫기", "Close")}
                </button>
              </div>
              {deferredItems.length === 0 ? (
                <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("미검증 항목 없음", "No deferred items")}</p>
              ) : (
                <div className="space-y-2 max-h-60 overflow-y-auto">
                  {deferredItems.map((item) => (
                    <label key={item.id} className="flex items-start gap-2 text-xs cursor-pointer">
                      <input
                        type="checkbox"
                        checked={false}
                        onChange={async () => {
                          await api.patchKanbanDeferDod(item.cardId, { verify: item.id });
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
            </div>
          );
        })()}

        {stalledPopup && (
          <div className="rounded-xl border p-4 space-y-3" style={{ borderColor: "rgba(239,68,68,0.35)", backgroundColor: "rgba(127,29,29,0.18)" }}>
            <div className="flex items-center justify-between">
              <h3 className="text-sm font-semibold" style={{ color: "#fca5a5" }}>
                {tr(`정체 카드 ${stalledCards.length}건`, `${stalledCards.length} Stalled Cards`)}
              </h3>
              <div className="flex gap-2">
                <button
                  onClick={() => setStalledSelected(stalledSelected.size === stalledCards.length ? new Set() : new Set(stalledCards.map((c) => c.id)))}
                  className="text-xs px-2 py-0.5 rounded border"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-muted)" }}
                >
                  {stalledSelected.size === stalledCards.length ? tr("해제", "Deselect") : tr("전체 선택", "Select all")}
                </button>
                <button
                  onClick={() => setStalledPopup(false)}
                  className="text-xs px-2 py-0.5 rounded border"
                  style={{ borderColor: "rgba(148,163,184,0.3)", color: "var(--th-text-muted)" }}
                >
                  {tr("닫기", "Close")}
                </button>
              </div>
            </div>
            <div className="space-y-1 max-h-60 overflow-y-auto">
              {stalledCards.map((card) => (
                <label key={card.id} className="flex items-center gap-2 rounded-lg px-2 py-1.5 cursor-pointer hover:bg-surface-subtle text-sm" style={{ color: "var(--th-text-primary)" }}>
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
                  <span className="text-xs px-1.5 py-0.5 rounded-full shrink-0" style={{ backgroundColor: "rgba(239,68,68,0.15)", color: "#f87171" }}>
                    {card.review_status}
                  </span>
                  <span className="text-xs shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {card.github_repo ? card.github_repo.split("/")[1] : ""}
                  </span>
                </label>
              ))}
            </div>
            {stalledSelected.size > 0 && (
              <div className="flex gap-2 pt-1">
                <button
                  onClick={() => void handleBulkAction("pass")}
                  disabled={bulkBusy}
                  className="text-xs px-3 py-1 rounded-lg border font-medium"
                  style={{ borderColor: "rgba(34,197,94,0.4)", color: "#4ade80", backgroundColor: "rgba(34,197,94,0.12)" }}
                >
                  {bulkBusy ? "…" : tr(`일괄 Pass (${stalledSelected.size})`, `Pass All (${stalledSelected.size})`)}
                </button>
                <button
                  onClick={() => void handleBulkAction("reset")}
                  disabled={bulkBusy}
                  className="text-xs px-3 py-1 rounded-lg border font-medium"
                  style={{ borderColor: "rgba(14,165,233,0.4)", color: "#38bdf8", backgroundColor: "rgba(14,165,233,0.12)" }}
                >
                  {bulkBusy ? "…" : tr(`일괄 Reset (${stalledSelected.size})`, `Reset All (${stalledSelected.size})`)}
                </button>
                <button
                  onClick={() => void handleBulkAction("cancel")}
                  disabled={bulkBusy}
                  className="text-xs px-3 py-1 rounded-lg border font-medium"
                  style={{ borderColor: "rgba(107,114,128,0.4)", color: "#9ca3af", backgroundColor: "rgba(107,114,128,0.12)" }}
                >
                  {bulkBusy ? "…" : tr(`일괄 Cancel (${stalledSelected.size})`, `Cancel All (${stalledSelected.size})`)}
                </button>
              </div>
            )}
          </div>
        )}
      </section>

      {/* Cancel confirmation modal — ask whether to also close GitHub issues */}
      {cancelConfirm && (() => {
        const ghCards = cancelConfirm.cardIds
          .map((id) => cardsById.get(id))
          .filter((c): c is KanbanCard => !!(c?.github_repo && c.github_issue_number));
        return (
          <div className="fixed inset-0 z-50 backdrop-blur-sm flex items-center justify-center p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }}>
            <div
              onClick={(e) => e.stopPropagation()}
              className="w-full max-w-md rounded-2xl border p-5 space-y-4"
              style={{ backgroundColor: "var(--th-bg-surface)", borderColor: "rgba(148,163,184,0.24)" }}
              role="dialog" aria-modal="true" aria-label="Cancel cards confirmation"
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
                <button
                  onClick={() => setCancelConfirm(null)}
                  disabled={cancelBusy}
                  className="rounded-xl px-4 py-2 text-sm bg-surface-medium"
                  style={{ color: "var(--th-text-secondary)" }}
                >
                  {tr("돌아가기", "Go back")}
                </button>
                <button
                  onClick={() => void executeBulkCancel()}
                  disabled={cancelBusy}
                  className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50"
                  style={{ backgroundColor: "#dc2626" }}
                >
                  {cancelBusy ? tr("처리 중…", "Processing…") : tr("취소 확정", "Confirm cancel")}
                </button>
              </div>
            </div>
          </div>
        );
      })()}

      {selectedRepo && (
        <KanbanPipelinePanel
          tr={tr}
          locale={locale}
          agents={agents}
          selectedRepo={selectedRepo}
          selectedAgentId={selectedAgentId}
        />
      )}

      <KanbanBoard
        tr={tr}
        locale={locale}
        selectedRepo={selectedRepo}
        compactBoard={compactBoard}
        showClosed={showClosed}
        initialLoading={initialLoading}
        loadingIssues={loadingIssues}
        hasQaCards={hasQaCards}
        effectiveColumnDefs={effectiveColumnDefs}
        visibleColumns={visibleColumns}
        cardsByStatus={cardsByStatus}
        backlogIssues={backlogIssues}
        recentDoneCards={recentDoneCards}
        recentDonePage={recentDonePage}
        recentDoneOpen={recentDoneOpen}
        mobileColumnStatus={mobileColumnStatus}
        draggingCardId={draggingCardId}
        dragOverStatus={dragOverStatus}
        dragOverCardId={dragOverCardId}
        closingIssueNumber={closingIssueNumber}
        assigningIssue={assigningIssue}
        dispatchMap={dispatchMap}
        dispatches={dispatches}
        repoSources={repoSources}
        setRecentDonePage={setRecentDonePage}
        setRecentDoneOpen={setRecentDoneOpen}
        setMobileColumnStatus={setMobileColumnStatus}
        setDraggingCardId={setDraggingCardId}
        setDragOverStatus={setDragOverStatus}
        setDragOverCardId={setDragOverCardId}
        setActionError={setActionError}
        getAgentLabel={getAgentLabel}
        resolveAgentFromLabels={resolveAgentFromLabels}
        onCardClick={setSelectedCardId}
        onBacklogIssueClick={setSelectedBacklogIssue}
        onDrop={handleDrop}
        onCloseIssue={handleCloseIssue}
        onDirectAssignIssue={handleDirectAssignIssue}
        onOpenAssignModal={handleOpenAssignModal}
        onUpdateCardStatus={handleUpdateCardStatus}
      />

      {assignIssue && (
        <div className="fixed inset-0 z-50 backdrop-blur-sm flex items-end justify-center sm:items-center p-0 sm:p-4" style={{ backgroundColor: "var(--th-modal-overlay)" }}>
          <div
            className="w-full max-w-lg rounded-t-3xl border p-5 sm:rounded-3xl sm:p-6 space-y-4"
            style={{
              backgroundColor: "var(--th-bg-surface)",
              borderColor: "rgba(148,163,184,0.24)",
              paddingBottom: "max(6rem, calc(6rem + env(safe-area-inset-bottom)))",
            }}
            role="dialog" aria-modal="true" aria-label="Assign issue"
          >
            <div className="flex items-start justify-between gap-3">
              <div>
                <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {selectedRepo} #{assignIssue.number}
                </div>
                <h3 className="mt-1 text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {assignIssue.title}
                </h3>
              </div>
              <button
                onClick={() => setAssignIssue(null)}
                className="shrink-0 whitespace-nowrap rounded-xl px-3 py-2 text-sm bg-surface-medium"
                style={{ color: "var(--th-text-secondary)" }}
              >
                {tr("닫기", "Close")}
              </button>
            </div>

            <label className="space-y-1 block">
              <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>{tr("담당자", "Assignee")}</span>
              <select
                value={assignAssigneeId}
                onChange={(event) => setAssignAssigneeId(event.target.value)}
                className="w-full rounded-xl px-3 py-2 text-sm bg-surface-light border"
                style={{ borderColor: "rgba(148,163,184,0.24)", color: "var(--th-text-primary)" }}
              >
                <option value="">{tr("에이전트 선택", "Select an agent")}</option>
                {agents.map((agent) => (
                  <option key={agent.id} value={agent.id}>{getAgentLabel(agent.id)}</option>
                ))}
              </select>
            </label>

            <div className="flex flex-col-reverse gap-2 sm:flex-row sm:justify-end">
              <button
                onClick={() => setAssignIssue(null)}
                className="rounded-xl px-4 py-2 text-sm bg-surface-medium"
                style={{ color: "var(--th-text-secondary)" }}
              >
                {tr("취소", "Cancel")}
              </button>
              <button
                onClick={() => void handleAssignIssue()}
                disabled={assigningIssue || !assignAssigneeId}
                className="rounded-xl px-4 py-2 text-sm font-medium text-white disabled:opacity-50"
                style={{ backgroundColor: "#2563eb" }}
              >
                {assigningIssue ? tr("할당 중", "Assigning") : tr("ready로 할당", "Assign to ready")}
              </button>
            </div>
          </div>
        </div>
      )}

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
          setReviewData={setReviewData}
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
            const repoSource = repoSources.find((s) => s.repo === selectedRepo);
            setAssignAssigneeId(repoSource?.default_agent_id ?? "");
          }}
        />
      )}
    </div>
  );
}
