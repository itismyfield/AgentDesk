import type { ReactNode } from "react";
import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../../api";
import type {
  AutoQueueStatus,
  DispatchQueueEntry as DispatchQueueEntryType,
  AutoQueueRun,
  AutoQueueThreadLink,
  PhaseGateInfo,
} from "../../api";

import type { Agent, UiLanguage } from "../../types";
import { localeName } from "../../i18n";
import { useLocalStorage } from "../../lib/useLocalStorage";
import { STORAGE_KEYS } from "../../lib/storageKeys";
import {
  createEmptyAutoQueueStatus,
  getAutoQueuePrimaryAction,
  normalizeAutoQueueStatus,
  shouldClearSuppressedAutoQueueRun,
} from "./auto-queue-panel-state";
import { buildDiscordThreadLinks } from "./discord-routing";
import { buildGitHubIssueUrl } from "./kanban-utils";

interface Props {
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  agents: Agent[];
  selectedRepo: string;
  selectedAgentId?: string | null;
}

type ViewMode = "all" | "agent" | "thread";

function formatTs(
  value: number | null | undefined,
  locale: UiLanguage,
): string {
  if (!value) return "-";
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  }).format(value);
}

const ENTRY_STATUS_STYLE: Record<
  string,
  { bg: string; text: string; label: string; labelEn: string }
> = {
  pending: {
    bg: "rgba(100,116,139,0.18)",
    text: "#94a3b8",
    label: "대기",
    labelEn: "Pending",
  },
  dispatched: {
    bg: "rgba(245,158,11,0.18)",
    text: "#fbbf24",
    label: "진행",
    labelEn: "Active",
  },
  done: {
    bg: "rgba(34,197,94,0.22)",
    text: "#4ade80",
    label: "완료",
    labelEn: "Done",
  },
  review: {
    bg: "rgba(139,92,246,0.22)",
    text: "#a78bfa",
    label: "리뷰",
    labelEn: "Review",
  },
  rework: {
    bg: "rgba(236,72,153,0.22)",
    text: "#f472b6",
    label: "리뷰 반영",
    labelEn: "Rework",
  },
  skipped: {
    bg: "rgba(107,114,128,0.18)",
    text: "#9ca3af",
    label: "건너뜀",
    labelEn: "Skipped",
  },
  failed: {
    bg: "rgba(239,68,68,0.18)",
    text: "#f87171",
    label: "실패",
    labelEn: "Failed",
  },
};

const RUN_STATUS_STYLE: Record<AutoQueueRun["status"], { bg: string; text: string; label: string; labelEn: string }> = {
  generated: { bg: "rgba(59,130,246,0.18)", text: "#60a5fa", label: "생성됨", labelEn: "Generated" },
  pending: { bg: "rgba(56,189,248,0.2)", text: "#38bdf8", label: "PMD 대기", labelEn: "Awaiting PMD" },
  active: { bg: "rgba(16,185,129,0.2)", text: "#10b981", label: "실행 중", labelEn: "Active" },
  paused: { bg: "rgba(245,158,11,0.2)", text: "#fbbf24", label: "일시정지", labelEn: "Paused" },
  completed: { bg: "rgba(34,197,94,0.2)", text: "#4ade80", label: "완료", labelEn: "Done" },
  cancelled: { bg: "rgba(248,113,113,0.18)", text: "#f87171", label: "취소됨", labelEn: "Cancelled" },
};

function reorderPendingIds(ids: string[], fromId: string, toId: string): string[] | null {
  const fromIdx = ids.indexOf(fromId);
  const toIdx = ids.indexOf(toId);
  if (fromIdx === -1 || toIdx === -1 || fromIdx === toIdx) return null;

  const nextIds = [...ids];
  nextIds.splice(fromIdx, 1);
  nextIds.splice(toIdx, 0, fromId);
  return nextIds;
}

function shiftPendingId(
  ids: string[],
  entryId: string,
  offset: -1 | 1,
): string[] | null {
  const fromIdx = ids.indexOf(entryId);
  if (fromIdx === -1) return null;
  const toIdx = fromIdx + offset;
  if (toIdx < 0 || toIdx >= ids.length) return null;
  return reorderPendingIds(ids, entryId, ids[toIdx]);
}

// ── Draggable Entry Row ──

const THREAD_GROUP_COLORS = [
  "#10b981",
  "#38bdf8",
  "#f59e0b",
  "#fbbf24",
  "#4ade80",
  "#fb923c",
  "#ef4444",
  "#22d3ee",
  "#a3e635",
  "#f87171",
];

function threadGroupColor(group: number): string {
  return THREAD_GROUP_COLORS[group % THREAD_GROUP_COLORS.length];
}

function batchPhaseColor(phase: number): string {
  if (phase <= 0) return "#94a3b8";
  return THREAD_GROUP_COLORS[(phase - 1) % THREAD_GROUP_COLORS.length];
}

function batchPhaseLabel(phase: number): string {
  return `P${phase}`;
}

function isCompletedEntry(entry: DispatchQueueEntryType): boolean {
  return (
    entry.status === "done"
    || entry.status === "skipped"
    || entry.status === "failed"
  );
}

function sortEntriesForDisplay(entries: DispatchQueueEntryType[]): DispatchQueueEntryType[] {
  const statusOrder: Record<string, number> = {
    dispatched: 0,
    pending: 1,
    failed: 2,
    done: 3,
    skipped: 4,
  };

  return [...entries].sort((a, b) => {
    const sa = statusOrder[a.status] ?? 1;
    const sb = statusOrder[b.status] ?? 1;
    if (sa !== sb) return sa - sb;
    return a.priority_rank - b.priority_rank;
  });
}

function formatThreadLinkLabel(
  link: AutoQueueThreadLink,
  tr: (ko: string, en: string) => string,
): string {
  const key = (link.label || link.role || "").trim().toLowerCase();
  if (key === "work") return tr("작업", "Work");
  if (key === "review") return tr("리뷰", "Review");
  if (key === "active") return tr("활성", "Active");
  return (link.label || link.role || tr("스레드", "Thread")).trim();
}

function EntryRow({
  entry,
  idx,
  tr,
  locale,
  onUpdateStatus,
  isDragging,
  isDropTarget,
  dragHandlers,
  moveControls,
  showThreadGroup,
  showBatchPhase,
}: {
  entry: DispatchQueueEntryType;
  idx: number;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onUpdateStatus: (id: string, status: "pending" | "skipped") => void;
  isDragging?: boolean;
  isDropTarget?: boolean;
  showThreadGroup?: boolean;
  showBatchPhase?: boolean;
  dragHandlers?: {
    draggable: boolean;
    onDragStart: (e: React.DragEvent) => void;
    onDragOver: (e: React.DragEvent) => void;
    onDragLeave: (e: React.DragEvent) => void;
    onDrop: (e: React.DragEvent) => void;
    onDragEnd: () => void;
  };
  moveControls?: {
    canMoveUp: boolean;
    canMoveDown: boolean;
    onMoveUp: () => void;
    onMoveDown: () => void;
  };
}) {
  const effectiveDisplayStatus =
    entry.status === "dispatched" && (entry.card_status === "review" || entry.card_status === "rework")
      ? entry.card_status
      : entry.status;
  const sty = ENTRY_STATUS_STYLE[effectiveDisplayStatus] ?? ENTRY_STATUS_STYLE.pending;
  const isPending = entry.status === "pending";
  const isFailed = entry.status === "failed";
  const retryCount = entry.retry_count ?? 0;
  const showReviewRound = (entry.card_status === "review" || entry.card_status === "rework") && (entry.review_round ?? 0) > 0;
  const githubIssueUrl = buildGitHubIssueUrl(entry.github_repo, entry.github_issue_number);
  const threadLinks = (entry.thread_links ?? []).filter(
    (link) => Boolean(link.url || link.thread_id),
  );

  return (
    <div
      className="flex flex-wrap items-start gap-2 rounded-xl border px-3 py-2 transition-all sm:flex-nowrap sm:items-center"
      style={{
        borderColor: isDropTarget
          ? "rgba(16,185,129,0.6)"
          : isFailed
            ? "rgba(239,68,68,0.35)"
          : entry.status === "dispatched"
            ? "rgba(245,158,11,0.3)"
            : "rgba(148,163,184,0.15)",
        backgroundColor: isDragging
          ? "rgba(16,185,129,0.12)"
          : isDropTarget
            ? "rgba(16,185,129,0.08)"
            : isFailed
              ? "rgba(239,68,68,0.08)"
            : entry.status === "dispatched"
              ? "rgba(245,158,11,0.06)"
              : "var(--th-overlay-medium)",
        opacity: isDragging ? 0.5 : 1,
        cursor: isPending && dragHandlers?.draggable ? "grab" : undefined,
      }}
      {...(dragHandlers ?? {})}
    >
      <div className="flex min-w-0 flex-1 items-start gap-2">
        {isPending && dragHandlers?.draggable && (
          <span
            className="shrink-0 select-none text-xs"
            style={{ color: "var(--th-text-muted)", cursor: "grab" }}
          >
            ⠿
          </span>
        )}
        <span
          className="w-5 shrink-0 text-center font-mono text-xs"
          style={{ color: "var(--th-text-muted)" }}
        >
          {idx + 1}
        </span>
        <div className="min-w-0 flex-1">
          <div
            className="text-sm font-medium leading-snug sm:text-xs"
            style={{
              color: "var(--th-text-primary)",
              display: "-webkit-box",
              WebkitLineClamp: 2,
              WebkitBoxOrient: "vertical",
              overflow: "hidden",
            }}
          >
            {showBatchPhase && (
              <span
                className="mr-1 rounded px-1 py-0.5 font-mono text-xs"
                style={{
                  backgroundColor: `${batchPhaseColor(entry.batch_phase ?? 0)}22`,
                  color: batchPhaseColor(entry.batch_phase ?? 0),
                }}
              >
                {batchPhaseLabel(entry.batch_phase ?? 0)}
              </span>
            )}
            {showThreadGroup && entry.thread_group != null && (
              <span
                className="mr-1 rounded px-1 py-0.5 font-mono text-xs"
                style={{
                  backgroundColor: `${threadGroupColor(entry.thread_group)}22`,
                  color: threadGroupColor(entry.thread_group),
                }}
              >
                G{entry.thread_group}
              </span>
            )}
            {entry.github_issue_number && (
              githubIssueUrl ? (
                <a
                  href={githubIssueUrl}
                  target="_blank"
                  rel="noopener noreferrer"
                  className="mr-1 font-medium hover:underline"
                  style={{ color: "#60a5fa" }}
                  onClick={(e) => e.stopPropagation()}
                >
                  #{entry.github_issue_number}
                </a>
              ) : (
                <span
                  className="mr-1 font-medium"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  #{entry.github_issue_number}
                </span>
              )
            )}
            {entry.card_title ?? entry.card_id.slice(0, 8)}
          </div>
          {entry.reason && (
            <div
              className="mt-1 text-[11px] leading-snug sm:text-xs"
              style={{
                color: "var(--th-text-muted)",
                display: "-webkit-box",
                WebkitLineClamp: 2,
                WebkitBoxOrient: "vertical",
                overflow: "hidden",
              }}
            >
              {entry.reason}
            </div>
          )}
          {threadLinks.length > 0 && (
            <div className="mt-2 flex flex-wrap gap-1.5">
              {threadLinks.map((link) => {
                const label = formatThreadLinkLabel(link, tr);
                const key = `${entry.id}:${link.role}:${link.thread_id}`;
                const resolvedLink = link.url ? buildDiscordThreadLinks(link) : null;
                const href = resolvedLink
                  ? (resolvedLink.deepLink ?? resolvedLink.webUrl)
                  : null;
                const content = (
                  <>
                    <span>{label}</span>
                    {href ? (
                      <span aria-hidden="true">↗</span>
                    ) : (
                      <span className="font-mono opacity-70">
                        #{link.thread_id.slice(-4)}
                      </span>
                    )}
                  </>
                );

                if (href) {
                  return (
                    <a
                      key={key}
                      href={href}
                      target="_blank"
                      rel="noreferrer"
                      onClick={(event) => event.stopPropagation()}
                      className="inline-flex items-center gap-1 rounded-full px-2 py-1 text-[11px] font-medium transition-colors hover:brightness-110"
                      style={{
                        backgroundColor: "rgba(59,130,246,0.14)",
                        color: "#93c5fd",
                      }}
                    >
                      {content}
                    </a>
                  );
                }

                return (
                  <span
                    key={key}
                    className="inline-flex items-center gap-1 rounded-full px-2 py-1 text-[11px] font-medium"
                    style={{
                      backgroundColor: "rgba(148,163,184,0.12)",
                      color: "var(--th-text-muted)",
                    }}
                  >
                    {content}
                  </span>
                );
              })}
            </div>
          )}
        </div>
      </div>
      <div className="ml-auto flex shrink-0 items-center gap-1.5 self-start sm:self-center">
        <div
          className="shrink-0 rounded px-1.5 py-0.5 text-xs"
          style={{ backgroundColor: sty.bg, color: sty.text }}
        >
          {tr(sty.label, sty.labelEn)}
          {showReviewRound && ` R${entry.review_round}`}
        </div>
        {retryCount > 0 && (
          <span
            className="shrink-0 rounded px-1.5 py-0.5 text-[11px] font-mono"
            style={{
              backgroundColor: isFailed ? "rgba(239,68,68,0.12)" : "rgba(148,163,184,0.12)",
              color: isFailed ? "#f87171" : "var(--th-text-muted)",
            }}
            title={tr("누적 재시도 횟수", "Accumulated retry count")}
          >
            R{retryCount}
          </span>
        )}
        {isPending && moveControls && (
          <div
            className="inline-flex shrink-0 overflow-hidden rounded-md border"
            style={{ borderColor: "rgba(148,163,184,0.2)" }}
          >
            <button
              type="button"
              onClick={moveControls.onMoveUp}
              disabled={!moveControls.canMoveUp}
              aria-label={tr("위로 이동", "Move up")}
              title={tr("위로 이동", "Move up")}
              className="px-1.5 py-0.5 text-xs"
              style={{
                color: moveControls.canMoveUp
                  ? "var(--th-text-secondary)"
                  : "var(--th-text-muted)",
                backgroundColor: "var(--th-bg-surface)",
                opacity: moveControls.canMoveUp ? 1 : 0.45,
                touchAction: "manipulation",
              }}
            >
              ↑
            </button>
            <button
              type="button"
              onClick={moveControls.onMoveDown}
              disabled={!moveControls.canMoveDown}
              aria-label={tr("아래로 이동", "Move down")}
              title={tr("아래로 이동", "Move down")}
              className="border-l px-1.5 py-0.5 text-xs"
              style={{
                borderColor: "rgba(148,163,184,0.2)",
                color: moveControls.canMoveDown
                  ? "var(--th-text-secondary)"
                  : "var(--th-text-muted)",
                backgroundColor: "var(--th-bg-surface)",
                opacity: moveControls.canMoveDown ? 1 : 0.45,
                touchAction: "manipulation",
              }}
            >
              ↓
            </button>
          </div>
        )}
        {isPending && (
          <button
            onClick={() => onUpdateStatus(entry.id, "skipped")}
            className="shrink-0 rounded border px-1.5 py-0.5 text-xs"
            style={{
              borderColor: "rgba(148,163,184,0.2)",
              color: "var(--th-text-muted)",
            }}
          >
            {tr("건너뛰기", "Skip")}
          </button>
        )}
        {isFailed && (
          <button
            onClick={() => onUpdateStatus(entry.id, "pending")}
            className="shrink-0 rounded border px-1.5 py-0.5 text-xs"
            style={{
              borderColor: "rgba(239,68,68,0.35)",
              color: "#fca5a5",
              backgroundColor: "rgba(239,68,68,0.08)",
            }}
          >
            {tr("재시도", "Retry")}
          </button>
        )}
        {isFailed && (
          <button
            onClick={() => onUpdateStatus(entry.id, "skipped")}
            className="shrink-0 rounded border px-1.5 py-0.5 text-xs"
            style={{
              borderColor: "rgba(148,163,184,0.2)",
              color: "var(--th-text-muted)",
            }}
          >
            {tr("제외", "Dismiss")}
          </button>
        )}
        {entry.dispatched_at && (
          <span
            className="hidden shrink-0 text-xs sm:inline"
            style={{ color: "var(--th-text-muted)" }}
          >
            {formatTs(entry.dispatched_at, locale)}
          </span>
        )}
      </div>
    </div>
  );
}

// ── Drag & drop hook for a list of entries ──

function useDragReorder(
  entries: DispatchQueueEntryType[],
  onReorder: (orderedIds: string[], agentId?: string | null) => Promise<void>,
  agentId?: string | null,
) {
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropTargetId, setDropTargetId] = useState<string | null>(null);
  const dragIdRef = useRef<string | null>(null);
  const reorderingRef = useRef(false);

  const pendingEntries = entries.filter((e) => e.status === "pending");

  const guardedReorder = async (orderedIds: string[]) => {
    if (reorderingRef.current) return;
    reorderingRef.current = true;
    try {
      await onReorder(orderedIds, agentId);
    } finally {
      reorderingRef.current = false;
    }
  };

  const makeDragHandlers = (entry: DispatchQueueEntryType) => {
    if (entry.status !== "pending") return undefined;

    return {
      draggable: true,
      onDragStart: (e: React.DragEvent) => {
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", entry.id);
        dragIdRef.current = entry.id;
        setDragId(entry.id);
      },
      onDragOver: (e: React.DragEvent) => {
        e.preventDefault();
        e.dataTransfer.dropEffect = "move";
        if (entry.status === "pending" && entry.id !== dragIdRef.current) {
          setDropTargetId(entry.id);
        }
      },
      onDragLeave: (e: React.DragEvent) => {
        // Only clear if leaving this element entirely
        const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
        const { clientX, clientY } = e;
        if (
          clientX < rect.left ||
          clientX > rect.right ||
          clientY < rect.top ||
          clientY > rect.bottom
        ) {
          setDropTargetId((prev) => (prev === entry.id ? null : prev));
        }
      },
      onDrop: (e: React.DragEvent) => {
        e.preventDefault();
        const fromId = e.dataTransfer.getData("text/plain");
        const toId = entry.id;
        if (!fromId || fromId === toId || entry.status !== "pending") {
          setDragId(null);
          setDropTargetId(null);
          dragIdRef.current = null;
          return;
        }

        const ids = pendingEntries.map((pe) => pe.id);
        const reorderedIds = reorderPendingIds(ids, fromId, toId);
        if (!reorderedIds) {
          setDragId(null);
          setDropTargetId(null);
          dragIdRef.current = null;
          return;
        }

        setDragId(null);
        setDropTargetId(null);
        dragIdRef.current = null;

        void guardedReorder(reorderedIds);
      },
      onDragEnd: () => {
        setDragId(null);
        setDropTargetId(null);
        dragIdRef.current = null;
      },
    };
  };

  const makeMoveControls = (entry: DispatchQueueEntryType) => {
    if (entry.status !== "pending") return undefined;

    const ids = pendingEntries.map((pendingEntry) => pendingEntry.id);
    const index = ids.indexOf(entry.id);
    if (index === -1) return undefined;

    return {
      canMoveUp: index > 0 && !reorderingRef.current,
      canMoveDown: index < ids.length - 1 && !reorderingRef.current,
      onMoveUp: () => {
        const reorderedIds = shiftPendingId(ids, entry.id, -1);
        if (reorderedIds) void guardedReorder(reorderedIds);
      },
      onMoveDown: () => {
        const reorderedIds = shiftPendingId(ids, entry.id, 1);
        if (reorderedIds) void guardedReorder(reorderedIds);
      },
    };
  };

  return { dragId, dropTargetId, makeDragHandlers, makeMoveControls };
}

// ── Main Panel ──

export default function AutoQueuePanel({
  tr,
  locale,
  agents,
  selectedRepo,
  selectedAgentId,
}: Props) {
  const [status, setStatus] = useState<AutoQueueStatus | null>(null);
  const [expanded, setExpanded] = useLocalStorage<boolean>(STORAGE_KEYS.kanbanAutoQueueOpen, true);
  const [generating, setGenerating] = useState(false);
  const [activating, setActivating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [noReadyCards, setNoReadyCards] = useState(false);
  const [viewMode, setViewMode] = useState<ViewMode>("thread");

  const agentMap = new Map(agents.map((a) => [a.id, a]));
  const suppressedRunIdRef = useRef<string | null>(null);

  const resetPanelState = useCallback(() => {
    setStatus(createEmptyAutoQueueStatus());
    setError(null);
    setNoReadyCards(false);
    setViewMode("thread");
    setGenerating(false);
    setActivating(false);
  }, []);

  const fetchStatus = useCallback(async () => {
    try {
      const s = await api.getAutoQueueStatus(selectedRepo || null, selectedAgentId);
      const normalized = normalizeAutoQueueStatus(s, suppressedRunIdRef.current);
      if (shouldClearSuppressedAutoQueueRun(s, suppressedRunIdRef.current)) {
        suppressedRunIdRef.current = null;
      }
      setStatus(normalized);
      // Only reset noReadyCards when a run with entries exists
      if (!normalized.run || normalized.entries.length > 0) setNoReadyCards(false);
    } catch {
      // silent
    }
  }, [selectedRepo, selectedAgentId]);

  useEffect(() => {
    void fetchStatus();
    const timer = setInterval(() => void fetchStatus(), 30_000);
    return () => clearInterval(timer);
  }, [fetchStatus]);

  const getAgentLabel = (agentId: string) => {
    const agent = agentMap.get(agentId);
    return agent ? localeName(locale, agent) : agentId.slice(0, 8);
  };

  const handleGenerate = async () => {
    setGenerating(true);
    setError(null);
    try {
      suppressedRunIdRef.current = null;
      if (!resetAgentId) {
        throw new Error("agent_id is required for reset");
      }
      await api.resetAutoQueue({
        runId: status?.run?.id ?? null,
        repo: selectedRepo || null,
        agentId: resetAgentId,
      });
      const result = await api.generateAutoQueue(
        selectedRepo || null,
        selectedAgentId,
      ) as Record<string, unknown>;
      if (result.entries && Array.isArray(result.entries) && result.entries.length === 0) {
        const counts = result.counts as Record<string, number> | undefined;
        const backlog = counts?.backlog ?? 0;
        const hint =
          backlog > 0
            ? tr(
                `준비됨 상태의 카드가 없습니다. 백로그에 ${backlog}개 카드가 있습니다 — 준비됨으로 이동하세요.`,
                `No ready cards. ${backlog} cards in backlog — move them to ready first.`,
              )
            : tr("준비됨 상태의 카드가 없습니다.", "No ready cards found.");
        setError(hint);
        setNoReadyCards(true);
        setGenerating(false);
        return; // Don't fetchStatus — it would reset noReadyCards
      }
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : tr("큐 생성 실패", "Queue generation failed"),
      );
    } finally {
      setGenerating(false);
    }
  };

  const handleReset = async () => {
    setError(null);
    setNoReadyCards(false);
    suppressedRunIdRef.current = status?.run?.id ?? null;
    try {
      if (!resetAgentId) {
        throw new Error("agent_id is required for reset");
      }
      await api.resetAutoQueue({
        runId: status?.run?.id ?? null,
        repo: selectedRepo || null,
        agentId: resetAgentId,
      });
      resetPanelState();
    } catch (e) {
      suppressedRunIdRef.current = null;
      setError(e instanceof Error ? e.message : tr("초기화 실패", "Reset failed"));
    }
  };

  const handleActivate = async () => {
    setActivating(true);
    setError(null);
    try {
      await api.activateAutoQueue(selectedRepo || null, selectedAgentId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : tr("활성화 실패", "Activation failed"),
      );
    } finally {
      setActivating(false);
    }
  };

  /** Pending run → activate immediately with default order, then dispatch first entry */
  const handleFallbackActivate = async (runId: string) => {
    setActivating(true);
    setError(null);
    try {
      await api.updateAutoQueueRun(runId, "active");
      await api.activateAutoQueue(selectedRepo || null, selectedAgentId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : tr("기본 순서 시작 실패", "Default order start failed"),
      );
    } finally {
      setActivating(false);
    }
  };

  const handleEntryStatusUpdate = async (
    entryId: string,
    status: "pending" | "skipped",
  ) => {
    try {
      await api.updateAutoQueueEntry(entryId, { status });
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : status === "pending"
            ? tr("재시도 실패", "Retry failed")
            : tr("상태 변경 실패", "Status change failed"),
      );
    }
  };

  const handleRunAction = async (
    run: AutoQueueRun,
    action: "paused" | "active" | "completed",
  ) => {
    try {
      await api.updateAutoQueueRun(run.id, action);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error
          ? e.message
          : tr("상태 변경 실패", "Status change failed"),
      );
    }
  };

  const handleReorder = async (
    orderedIds: string[],
    agentId?: string | null,
  ) => {
    try {
      await api.reorderAutoQueueEntries(orderedIds, agentId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : tr("순서 변경 실패", "Reorder failed"),
      );
    }
  };

  const run = status?.run ?? null;
  const entries = status?.entries ?? [];
  const phaseGates = status?.phase_gates ?? [];
  const deployPhases = new Set(run?.deploy_phases ?? []);
  const resetAgentId = selectedAgentId ?? run?.agent_id ?? null;
  const gatesByPhase = new Map<number, PhaseGateInfo[]>();
  for (const gate of phaseGates) {
    const list = gatesByPhase.get(gate.phase) ?? [];
    list.push(gate);
    gatesByPhase.set(gate.phase, list);
  }
  const agentStats: Record<
    string,
    { pending: number; dispatched: number; done: number; skipped: number; failed: number }
  > = status?.agents ?? {};

  const pendingCount = entries.filter((e) => e.status === "pending").length;
  const dispatchedCount = entries.filter(
    (e) => e.status === "dispatched",
  ).length;
  const doneCount = entries.filter((e) => e.status === "done").length;
  const failedCount = entries.filter((e) => e.status === "failed").length;
  const skippedCount = entries.filter((e) => e.status === "skipped").length;
  const completedCount = entries.filter(isCompletedEntry).length;
  const totalCount = entries.length;
  const primaryAction = getAutoQueuePrimaryAction(run, pendingCount);
  const showRunStartControls = !!run && (run.status === "generated" || run.status === "active") && pendingCount > 0;
  const startActionLabel = run?.status === "generated" ? tr("시작", "Start") : tr("디스패치", "Dispatch");

  // Group entries by agent
  const entriesByAgent = new Map<string, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const list = entriesByAgent.get(entry.agent_id) ?? [];
    list.push(entry);
    entriesByAgent.set(entry.agent_id, list);
  }

  // Thread group info
  const threadGroups = status?.thread_groups ?? {};
  const threadGroupCount = run?.thread_group_count ?? 0;
  const hasThreadGroups =
    threadGroupCount > 1 || Object.keys(threadGroups).length > 1;
  const maxConcurrent = run?.max_concurrent_threads ?? 1;
  const hasBatchPhases = entries.some((entry) => (entry.batch_phase ?? 0) > 0);
  // Earliest phase that still has work to do (pending or in-flight).
  // Previously this excluded phase 0 ("if (phase <= 0)"), so a queue with
  // P0 entries still pending and P1 entries also pending was reported as
  // "currently P1". Phase 0 is a real phase — include it.
  const currentBatchPhase = entries.reduce<number | null>((minPhase, entry) => {
    const phase = entry.batch_phase ?? 0;
    if (entry.status !== "pending" && entry.status !== "dispatched") return minPhase;
    return minPhase == null ? phase : Math.min(minPhase, phase);
  }, null);

  // Group entries by thread_group
  const entriesByThreadGroup = new Map<number, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const g = entry.thread_group ?? 0;
    const list = entriesByThreadGroup.get(g) ?? [];
    list.push(entry);
    entriesByThreadGroup.set(g, list);
  }

  const entriesByBatchPhase = new Map<number, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const phase = entry.batch_phase ?? 0;
    const list = entriesByBatchPhase.get(phase) ?? [];
    list.push(entry);
    entriesByBatchPhase.set(phase, list);
  }
  const phaseSections = Array.from(entriesByBatchPhase.entries()).sort(
    ([left], [right]) => left - right,
  );

  // All-queue view: merge all entries sorted by status then rank
  const allEntriesSorted = sortEntriesForDisplay(entries);

  // Drag & drop for "all" view (pending only, no agent scope)
  const allDrag = useDragReorder(allEntriesSorted, handleReorder);

  const renderPhaseBlock = (
    phase: number,
    phaseEntries: DispatchQueueEntryType[],
    content: ReactNode,
  ) => {
    const activePhase = currentBatchPhase === phase;
    const phaseColor = batchPhaseColor(phase);
    const doneInPhase = phaseEntries.filter(isCompletedEntry).length;

    return (
      <div
        key={phase}
        className="rounded-xl border p-2 space-y-2"
        style={{
          borderColor: activePhase
            ? `${phaseColor}66`
            : "rgba(148,163,184,0.16)",
          backgroundColor: activePhase ? `${phaseColor}10` : "transparent",
        }}
      >
        <div className="flex items-center gap-2 px-1">
          <span
            className="text-xs font-mono font-bold px-2 py-0.5 rounded"
            style={{ backgroundColor: `${phaseColor}26`, color: phaseColor }}
          >
            {batchPhaseLabel(phase)}
          </span>
          <span
            className="text-xs px-1.5 py-0.5 rounded"
            style={{
              backgroundColor: activePhase
                ? "rgba(245,158,11,0.18)"
                : "rgba(100,116,139,0.18)",
              color: activePhase ? "#fbbf24" : "var(--th-text-muted)",
            }}
          >
            {activePhase
              ? tr("현재 phase", "Current phase")
              : phase <= 0
                ? tr("즉시 가능", "Always eligible")
                : doneInPhase === phaseEntries.length
                  ? tr("완료", "Completed")
                  : currentBatchPhase != null && phase < currentBatchPhase
                    ? tr("완료", "Completed")
                    : tr("대기 phase", "Queued phase")}
          </span>
          <div
            className="flex-1 h-px"
            style={{ backgroundColor: `${phaseColor}40` }}
          />
          <span
            className="text-xs font-mono"
            style={{ color: "var(--th-text-muted)" }}
          >
            {doneInPhase}/{phaseEntries.length}
          </span>
        </div>
        {content}
      </div>
    );
  };

  const renderPhaseGateIndicator = (phase: number) => {
    const gates = gatesByPhase.get(phase) ?? [];
    const isDeploy = deployPhases.has(phase);

    const gate = gates[0];
    const gateStatus = gate?.status ?? "pending";
    const isPassed = gateStatus === "passed";
    const isFailed = gateStatus === "failed";
    const isPending = !isPassed && !isFailed;
    const isActive = isPending && currentBatchPhase === phase;

    const baseColor = isPassed
      ? "#4ade80"
      : isFailed
        ? "#ef4444"
        : isActive
          ? isDeploy ? "#60a5fa" : "#f59e0b"
          : "#6b7280";
    const statusIcon = isPassed ? "✓" : isFailed ? "✗" : isActive ? (isDeploy ? "🚀" : "⏳") : "○";
    const statusLabel = isPassed
      ? tr("통과", "Passed")
      : isFailed
        ? tr("실패", "Failed")
        : isActive
          ? tr("진행중", "In Progress")
          : tr("대기", "Pending");
    const gateLabel = isDeploy ? tr("배포 게이트", "Deploy Gate") : tr("게이트", "Gate");

    return (
      <div
        key={`gate-${phase}`}
        className="flex items-center gap-2 px-3 py-1.5"
      >
        <div
          className="flex-1 h-px"
          style={{ backgroundColor: `${baseColor}40` }}
        />
        <div
          className={`flex items-center gap-1.5 px-2.5 py-1 rounded-lg border${isActive ? " animate-pulse" : ""}`}
          style={{
            borderColor: `${baseColor}40`,
            backgroundColor: `${baseColor}10`,
          }}
        >
          <span style={{ color: baseColor, fontSize: 14 }}>
            {statusIcon}
          </span>
          <span
            className="text-xs font-mono font-semibold"
            style={{ color: baseColor }}
          >
            {gateLabel}
          </span>
          {gate && (
            <span
              className="text-xs px-1.5 py-0.5 rounded"
              style={{
                backgroundColor: `${baseColor}18`,
                color: baseColor,
              }}
            >
              {statusLabel}
            </span>
          )}
          {gate?.failure_reason && (
            <span
              className="text-xs truncate max-w-[200px]"
              style={{ color: "#f87171" }}
              title={gate.failure_reason}
            >
              {gate.failure_reason}
            </span>
          )}
        </div>
        <div
          className="flex-1 h-px"
          style={{ backgroundColor: `${baseColor}40` }}
        />
      </div>
    );
  };

  const renderThreadGroupCard = (
    groupNum: number,
    groupEntries: DispatchQueueEntryType[],
  ) => {
    const isActive = groupEntries.some((entry) => entry.status === "dispatched");
    const hasPending = groupEntries.some((entry) => entry.status === "pending");
    const hasFailed = groupEntries.some((entry) => entry.status === "failed");
    const completedEntries = groupEntries.filter(isCompletedEntry).length;
    const isDone = completedEntries === groupEntries.length && !hasFailed;
    const groupStatusLabel = isActive
      ? tr("진행", "Active")
      : hasPending
        ? tr("대기", "Pending")
        : hasFailed
          ? tr("실패", "Failed")
          : isDone
            ? tr("완료", "Done")
        : tr("대기", "Pending");
    const color = threadGroupColor(groupNum);
    const reason =
      groupEntries.find((entry) => !!entry.reason)?.reason ??
      threadGroups[String(groupNum)]?.reason;
    const headerColor = isActive ? "#fbbf24" : hasFailed ? "#f87171" : isDone ? "#4ade80" : "#94a3b8";
    const borderColor = isActive
      ? `${color}55`
      : hasFailed
        ? "rgba(239,68,68,0.28)"
        : isDone
          ? "rgba(34,197,94,0.2)"
          : "rgba(148,163,184,0.12)";

    return (
      <div
        key={groupNum}
        className="rounded-xl border p-2 space-y-1"
        style={{
          borderColor,
          backgroundColor: isActive
            ? `${color}0a`
            : hasFailed
              ? "rgba(239,68,68,0.04)"
              : "transparent",
        }}
      >
        <div className="flex items-center gap-2 px-1 mb-1">
          <span
            className="text-xs font-mono font-bold px-2 py-0.5 rounded"
            style={{ backgroundColor: `${color}30`, color }}
          >
            G{groupNum}
          </span>
          <span
            className="text-xs px-1.5 py-0.5 rounded"
            style={{
              backgroundColor: isActive
                ? "rgba(245,158,11,0.18)"
                : hasFailed
                  ? "rgba(239,68,68,0.16)"
                : isDone
                  ? "rgba(34,197,94,0.18)"
                  : "rgba(100,116,139,0.18)",
              color: headerColor,
            }}
          >
            {groupStatusLabel}
          </span>
          <div
            className="flex-1 h-px"
            style={{ backgroundColor: `${color}40` }}
          />
          <span
            className="text-xs font-mono"
            style={{ color: "var(--th-text-muted)" }}
          >
            {completedEntries}/{groupEntries.length}
          </span>
        </div>
        {reason && (
          <div
            className="px-1 text-[10px]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {reason}
          </div>
        )}
        {groupEntries.map((entry, idx) => (
          <EntryRow
            key={entry.id}
            entry={entry}
            idx={idx}
            tr={tr}
            locale={locale}
            onUpdateStatus={handleEntryStatusUpdate}
            showBatchPhase={hasBatchPhases}

          />
        ))}
      </div>
    );
  };

  return (
    <section
      className="rounded-2xl border px-3 py-2 sm:px-4 sm:py-2.5 space-y-2"
      style={{
        borderColor: run ? "rgba(16,185,129,0.35)" : "rgba(148,163,184,0.22)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      {/* Header — single-line on all widths to avoid vertical text spread */}
      <div className="flex items-center justify-between gap-2 min-w-0">
        <button
          onClick={() => setExpanded((p) => !p)}
          className="flex items-center gap-1.5 min-w-0 flex-1"
        >
          <span className="text-sm shrink-0" style={{ color: "var(--th-text-muted)" }}>
            {expanded ? "▾" : "▸"}
          </span>
          <h3
            className="text-sm font-semibold shrink-0"
            style={{ color: "var(--th-text-heading)" }}
          >
            {tr("자동 큐", "Auto Queue")}
          </h3>
          {run && (
            <span
              className="text-[11px] px-1.5 py-0.5 rounded-full shrink-0"
              style={{
                backgroundColor: RUN_STATUS_STYLE[run.status].bg,
                color: RUN_STATUS_STYLE[run.status].text,
              }}
            >
              {tr(RUN_STATUS_STYLE[run.status].label, RUN_STATUS_STYLE[run.status].labelEn)}
            </span>
          )}
          {totalCount > 0 && (
            <span
              className="text-[11px] px-1.5 py-0.5 rounded bg-surface-medium shrink-0"
              style={{ color: "var(--th-text-muted)" }}
            >
              {completedCount}/{totalCount}
            </span>
          )}
        </button>

        <div className="flex items-center gap-1.5 shrink-0">
          {showRunStartControls && (
            <>
              <button
                onClick={() => void handleActivate()}
                disabled={activating}
                className="text-xs px-2.5 py-1 rounded-lg border font-medium"
                style={{
                  borderColor: "rgba(245,158,11,0.4)",
                  color: "#fbbf24",
                  backgroundColor: "rgba(245,158,11,0.1)",
                }}
              >
                {activating ? "…" : startActionLabel}
              </button>
            </>
          )}
          {primaryAction === "generate" && (
            <>
              <button
                onClick={() => void handleGenerate()}
                disabled={generating || noReadyCards}
                className="text-xs px-2.5 py-1 rounded-lg border font-medium"
                style={{
                  borderColor: noReadyCards
                    ? "rgba(148,163,184,0.2)"
                    : "rgba(16,185,129,0.4)",
                  color: noReadyCards ? "var(--th-text-muted)" : "#10b981",
                  backgroundColor: noReadyCards
                    ? "rgba(148,163,184,0.05)"
                    : "rgba(16,185,129,0.1)",
                  cursor: noReadyCards ? "not-allowed" : undefined,
                }}
                title={
                  noReadyCards
                    ? tr(
                        "준비됨 상태의 카드가 없습니다",
                        "No ready cards available",
                      )
                    : undefined
                }
              >
                {generating
                  ? tr("분석 중…", "Analyzing…")
                  : tr("큐 생성", "Generate")}
              </button>
            </>
          )}
          {run && (
            <button
              onClick={() => void handleReset()}
              className="text-[11px] px-2 py-1 rounded-lg border"
              style={{
                borderColor: "rgba(248,113,113,0.3)",
                color: "#f87171",
                backgroundColor: "rgba(248,113,113,0.08)",
              }}
            >
              {tr("초기화", "Reset")}
            </button>
          )}
          {run?.status === "active" && (
            <button
              onClick={() => void handleRunAction(run, "paused")}
              className="text-xs px-2 py-1 rounded-lg border"
              style={{
                borderColor: "rgba(148,163,184,0.22)",
                color: "var(--th-text-muted)",
              }}
            >
              {tr("일시정지", "Pause")}
            </button>
          )}
          {run?.status === "paused" && (
            <button
              onClick={() => void handleRunAction(run, "active")}
              className="text-xs px-2 py-1 rounded-lg border"
              style={{ borderColor: "rgba(16,185,129,0.3)", color: "#10b981" }}
            >
              {tr("재개", "Resume")}
            </button>
          )}
        </div>
      </div>

      {error && (
        <div
          className="rounded-lg px-3 py-2 text-xs border"
          style={{
            borderColor: "rgba(248,113,113,0.4)",
            color: "#fecaca",
            backgroundColor: "rgba(127,29,29,0.2)",
          }}
        >
          {error}
        </div>
      )}

      {run?.ai_rationale && (
        <div
          className="rounded-lg px-3 py-2 text-[11px] border"
          style={{
            borderColor: "rgba(96,165,250,0.22)",
            color: "var(--th-text-secondary)",
            backgroundColor: "rgba(30,41,59,0.45)",
          }}
        >
          {run.ai_rationale}
        </div>
      )}

      {/* PMD pending state */}
      {run?.status === "pending" && expanded && (
        <div
          className="rounded-xl p-3 space-y-2 border"
          style={{
            borderColor: "rgba(56,189,248,0.25)",
            backgroundColor: "rgba(56,189,248,0.06)",
          }}
        >
          <div className="flex items-center gap-2">
            <span className="animate-pulse text-lg">⏳</span>
            <span className="text-sm font-medium" style={{ color: "#7dd3fc" }}>
              {tr("PMD 순서 분석 대기 중", "Awaiting PMD order analysis")}
            </span>
          </div>
          <div
            className="text-xs space-y-1"
            style={{ color: "var(--th-text-muted)" }}
          >
            <div>
              {tr("요청 시각", "Requested")}:{" "}
              {run.created_at ? formatTs(run.created_at, locale) : "-"}
            </div>
            {run.repo && (
              <div>
                {tr("대상 레포", "Target repo")}: {run.repo}
              </div>
            )}
            <div>
              {tr(
                "PMD가 순서를 결정하면 자동으로 활성화됩니다.",
                "Queue will activate automatically when PMD submits the order.",
              )}
            </div>
          </div>
          {run.ai_rationale && (
            <div
              className="text-xs italic"
              style={{ color: "var(--th-text-muted)" }}
            >
              {run.ai_rationale}
            </div>
          )}
          <button
            onClick={() => void handleFallbackActivate(run.id)}
            disabled={activating}
            className="text-xs px-2.5 py-1 rounded-lg border font-medium"
            style={{
              borderColor: "rgba(148,163,184,0.3)",
              color: "var(--th-text-secondary)",
            }}
          >
            {tr("기본 순서로 바로 시작", "Start with default order")}
          </button>
        </div>
      )}

      {/* Progress bar */}
      {totalCount > 0 && (
        <div className="flex gap-0.5 h-1.5 rounded-full overflow-hidden bg-surface-subtle">
          {doneCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(doneCount / totalCount) * 100}%`,
                backgroundColor: "#4ade80",
              }}
            />
          )}
          {dispatchedCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(dispatchedCount / totalCount) * 100}%`,
                backgroundColor: "#fbbf24",
              }}
            />
          )}
          {failedCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(failedCount / totalCount) * 100}%`,
                backgroundColor: "#ef4444",
              }}
            />
          )}
          {skippedCount > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(skippedCount / totalCount) * 100}%`,
                backgroundColor: "#6b7280",
              }}
            />
          )}
        </div>
      )}

      {/* Expanded: queue entries */}
      {expanded && (
        <div className="space-y-3">
          {/* View mode toggle + Agent summary chips */}
          {totalCount > 0 && (
            <div className="flex items-center justify-between gap-2 flex-wrap">
              <div className="flex flex-wrap gap-1.5">
                {Object.entries(agentStats).map(([agentId, stats]) => (
                  <div
                    key={agentId}
                    className="inline-flex items-center gap-1.5 text-xs px-2 py-1 rounded-lg border"
                    style={{
                      borderColor: "rgba(148,163,184,0.18)",
                      backgroundColor: "var(--th-overlay-medium)",
                    }}
                  >
                    <span style={{ color: "var(--th-text-secondary)" }}>
                      {getAgentLabel(agentId)}
                    </span>
                    {stats.dispatched > 0 && (
                      <span style={{ color: "#fbbf24" }}>
                        {stats.dispatched}
                      </span>
                    )}
                    {stats.pending > 0 && (
                      <span style={{ color: "#94a3b8" }}>{stats.pending}</span>
                    )}
                    <span style={{ color: "#4ade80" }}>{stats.done}</span>
                    {stats.failed > 0 && (
                      <span style={{ color: "#f87171" }}>!{stats.failed}</span>
                    )}
                    {stats.skipped > 0 && (
                      <span style={{ color: "#6b7280" }}>-{stats.skipped}</span>
                    )}
                  </div>
                ))}
              </div>

              <div className="flex items-center gap-2 flex-wrap justify-end">
                {pendingCount > 1 && (
                  <span
                    className="text-xs"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {tr("순서 변경: 드래그 또는 ↑↓", "Reorder: drag or ↑↓")}
                  </span>
                )}

                {/* View mode toggle */}
                {(Object.keys(agentStats).length > 1 || hasThreadGroups) && (
                  <div
                    className="inline-flex rounded-lg border overflow-hidden"
                    style={{ borderColor: "rgba(148,163,184,0.22)" }}
                  >
                    {hasThreadGroups && (
                      <button
                        onClick={() => setViewMode("thread")}
                        className="text-xs px-2 py-1 transition-colors"
                        style={{
                          backgroundColor:
                            viewMode === "thread"
                              ? "rgba(16,185,129,0.2)"
                              : "transparent",
                          color:
                            viewMode === "thread"
                              ? "#10b981"
                              : "var(--th-text-muted)",
                        }}
                      >
                        {tr("스레드", "Thread")}
                      </button>
                    )}
                    <button
                      onClick={() => setViewMode("all")}
                      className="text-xs px-2 py-1 transition-colors"
                        style={{
                          backgroundColor:
                            viewMode === "all"
                            ? "rgba(16,185,129,0.2)"
                            : "transparent",
                          color:
                            viewMode === "all"
                            ? "#10b981"
                            : "var(--th-text-muted)",
                        }}
                    >
                      {tr("전체", "All")}
                    </button>
                    {Object.keys(agentStats).length > 1 && (
                      <button
                        onClick={() => setViewMode("agent")}
                        className="text-xs px-2 py-1 transition-colors"
                        style={{
                          backgroundColor:
                            viewMode === "agent"
                              ? "rgba(16,185,129,0.2)"
                              : "transparent",
                          color:
                            viewMode === "agent"
                              ? "#10b981"
                              : "var(--th-text-muted)",
                        }}
                      >
                        {tr("에이전트별", "By Agent")}
                      </button>
                    )}
                  </div>
                )}
              </div>
            </div>
          )}

          {/* ── All view: merged list with drag & drop ── */}
          {viewMode === "all" && (
            hasBatchPhases ? (
              <div className="space-y-3">
                {phaseSections.map(([phase, phaseEntries]) => (
                  <div key={`phase-section-${phase}`}>
                    {renderPhaseBlock(
                      phase,
                      phaseEntries,
                      <div className="space-y-1">
                        {sortEntriesForDisplay(phaseEntries).map((entry, idx) => (
                          <div key={entry.id} className="flex items-center gap-1">
                            <span
                              className="text-xs px-1.5 py-0.5 rounded shrink-0 max-w-[60px] truncate"
                              style={{
                                backgroundColor: "rgba(139,92,246,0.12)",
                                color: "#a78bfa",
                              }}
                            >
                              {getAgentLabel(entry.agent_id)}
                            </span>
                            <div className="flex-1 min-w-0">
                              <EntryRow
                                entry={entry}
                                idx={idx}
                                tr={tr}
                                locale={locale}
                                onUpdateStatus={handleEntryStatusUpdate}
                                showThreadGroup={hasThreadGroups}
                                showBatchPhase={hasBatchPhases}
                                isDragging={allDrag.dragId === entry.id}
                                isDropTarget={allDrag.dropTargetId === entry.id}
                                dragHandlers={allDrag.makeDragHandlers(entry)}
                                moveControls={allDrag.makeMoveControls(entry)}
                    
                              />
                            </div>
                          </div>
                        ))}
                      </div>,
                    )}
                    {renderPhaseGateIndicator(phase)}
                  </div>
                ))}
              </div>
            ) : (
              <div className="space-y-1">
                {allEntriesSorted.map((entry, idx) => (
                  <div key={entry.id} className="flex items-center gap-1">
                    <span
                      className="text-xs px-1.5 py-0.5 rounded shrink-0 max-w-[60px] truncate"
                      style={{
                        backgroundColor: "rgba(139,92,246,0.12)",
                        color: "#a78bfa",
                      }}
                    >
                      {getAgentLabel(entry.agent_id)}
                    </span>
                    <div className="flex-1 min-w-0">
                      <EntryRow
                        entry={entry}
                        idx={idx}
                        tr={tr}
                        locale={locale}
                        onUpdateStatus={handleEntryStatusUpdate}
                        showThreadGroup={hasThreadGroups}
                        isDragging={allDrag.dragId === entry.id}
                        isDropTarget={allDrag.dropTargetId === entry.id}
                        dragHandlers={allDrag.makeDragHandlers(entry)}
                        moveControls={allDrag.makeMoveControls(entry)}
            
                      />
                    </div>
                  </div>
                ))}
              </div>
            )
          )}

          {/* ── Thread group view ── */}
          {viewMode === "thread" && (
            <div className="space-y-3">
              {hasThreadGroups && (
                <div
                  className="flex items-center gap-2 text-xs px-1"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  <span>
                    {tr(
                      `동시 ${maxConcurrent}그룹 실행`,
                      `${maxConcurrent} concurrent groups`,
                    )}
                  </span>
                  <span style={{ color: "#4ade80" }}>
                    {entriesByThreadGroup.size}
                    {tr("그룹", " groups")}
                  </span>
                </div>
              )}
              {hasBatchPhases
                ? phaseSections.map(([phase, phaseEntries]) => {
                    const groupsInPhase = new Map<number, DispatchQueueEntryType[]>();
                    for (const entry of phaseEntries) {
                      const groupNum = entry.thread_group ?? 0;
                      const list = groupsInPhase.get(groupNum) ?? [];
                      list.push(entry);
                      groupsInPhase.set(groupNum, list);
                    }
                    return (
                      <div key={`phase-section-${phase}`}>
                        {renderPhaseBlock(
                          phase,
                          phaseEntries,
                          <div className="space-y-2">
                            {Array.from(groupsInPhase.entries())
                              .sort(([left], [right]) => left - right)
                              .map(([groupNum, groupEntries]) =>
                                renderThreadGroupCard(groupNum, groupEntries),
                              )}
                          </div>,
                        )}
                        {renderPhaseGateIndicator(phase)}
                      </div>
                    );
                  })
                : Array.from(entriesByThreadGroup.entries())
                    .sort(([left], [right]) => left - right)
                    .map(([groupNum, groupEntries]) =>
                      renderThreadGroupCard(groupNum, groupEntries),
                    )}
            </div>
          )}

          {/* ── Agent view: grouped by agent with per-agent drag & drop ── */}
          {viewMode === "agent" &&
            Array.from(entriesByAgent.entries()).map(
              ([agentId, agentEntries]) => (
                <AgentSubQueue
                  key={agentId}
                  agentId={agentId}
                  agentEntries={agentEntries}
                  getAgentLabel={getAgentLabel}
                  tr={tr}
                  locale={locale}
                  onUpdateStatus={handleEntryStatusUpdate}
                  onReorder={handleReorder}
                  showBatchPhase={hasBatchPhases}
      
                />
              ),
            )}

          {/* Run metadata */}
          {run && (
            <div
              className="flex flex-wrap gap-x-4 gap-y-1 text-xs px-1"
              style={{ color: "var(--th-text-muted)" }}
            >
              <span>AI: {run.ai_model ?? "-"}</span>
              <span>
                {tr("생성", "Created")}: {formatTs(run.created_at, locale)}
              </span>
              {hasThreadGroups && (
                <span>
                  {tr("동시", "Concur")}: {maxConcurrent}
                  {tr("그룹", "grp")}
                </span>
              )}
              {hasBatchPhases && (
                <span>
                  {tr("활성 phase", "Active phase")}:{" "}
                  {batchPhaseLabel(currentBatchPhase ?? 0)}
                </span>
              )}
              <span>
                {tr("타임아웃", "Timeout")}: {run.timeout_minutes}
                {tr("분", "m")}
              </span>
              {run.status !== "completed" && (
                <button
                  onClick={() => void handleRunAction(run, "completed")}
                  className="underline"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {tr("큐 종료", "End queue")}
                </button>
              )}
            </div>
          )}

          {entries.length === 0 && !run && (
            <div
              className="text-xs text-center py-3"
              style={{ color: "var(--th-text-muted)" }}
            >
              {tr(
                "활성 큐 없음. 준비됨 상태의 카드가 있으면 큐를 생성할 수 있습니다.",
                "No active queue. Generate one when there are ready cards.",
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}

// ── Agent sub-queue with its own drag & drop scope ──

function AgentSubQueue({
  agentId,
  agentEntries,
  getAgentLabel,
  tr,
  locale,
  onUpdateStatus,
  onReorder,
  showBatchPhase,
}: {
  agentId: string;
  agentEntries: DispatchQueueEntryType[];
  getAgentLabel: (id: string) => string;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onUpdateStatus: (id: string, status: "pending" | "skipped") => void;
  onReorder: (orderedIds: string[], agentId?: string | null) => Promise<void>;
  showBatchPhase?: boolean;
}) {
  const drag = useDragReorder(agentEntries, onReorder, agentId);

  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2 px-1">
        <div
          className="text-xs font-medium"
          style={{ color: "var(--th-text-muted)" }}
        >
          {getAgentLabel(agentId)}
        </div>
        <div
          className="flex-1 h-px"
          style={{ backgroundColor: "rgba(148,163,184,0.15)" }}
        />
        <div className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {agentEntries.filter(isCompletedEntry).length}/
          {agentEntries.length}
        </div>
      </div>
      {/* Per-agent progress bar */}
      {agentEntries.length > 1 && (
        <div className="flex gap-0.5 h-1 rounded-full overflow-hidden bg-surface-subtle mx-1">
          {(() => {
            const ad = agentEntries.filter((e) => e.status === "done").length;
            const aa = agentEntries.filter(
              (e) => e.status === "dispatched",
            ).length;
            const af = agentEntries.filter(
              (e) => e.status === "failed",
            ).length;
            const as_ = agentEntries.filter(
              (e) => e.status === "skipped",
            ).length;
            const at = agentEntries.length;
            return (
              <>
                {ad > 0 && (
                  <div
                    className="rounded-full"
                    style={{
                      width: `${(ad / at) * 100}%`,
                      backgroundColor: "#4ade80",
                    }}
                  />
                )}
                {aa > 0 && (
                  <div
                    className="rounded-full"
                    style={{
                      width: `${(aa / at) * 100}%`,
                      backgroundColor: "#fbbf24",
                    }}
                  />
                )}
                {af > 0 && (
                  <div
                    className="rounded-full"
                    style={{
                      width: `${(af / at) * 100}%`,
                      backgroundColor: "#ef4444",
                    }}
                  />
                )}
                {as_ > 0 && (
                  <div
                    className="rounded-full"
                    style={{
                      width: `${(as_ / at) * 100}%`,
                      backgroundColor: "#6b7280",
                    }}
                  />
                )}
              </>
            );
          })()}
        </div>
      )}
      {agentEntries.map((entry, idx) => (
        <EntryRow
          key={entry.id}
          entry={entry}
          idx={idx}
          tr={tr}
          locale={locale}
          onUpdateStatus={onUpdateStatus}
          showBatchPhase={showBatchPhase}
          isDragging={drag.dragId === entry.id}
          isDropTarget={drag.dropTargetId === entry.id}
          dragHandlers={drag.makeDragHandlers(entry)}
          moveControls={drag.makeMoveControls(entry)}
        />
      ))}
    </div>
  );
}
