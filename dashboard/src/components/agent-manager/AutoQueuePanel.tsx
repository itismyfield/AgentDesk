import { useCallback, useEffect, useRef, useState } from "react";
import * as api from "../../api";
import type {
  AutoQueueStatus,
  DispatchQueueEntry as DispatchQueueEntryType,
  AutoQueueRun,
} from "../../api";

import type { Agent, UiLanguage } from "../../types";
import { localeName } from "../../i18n";
import {
  createEmptyAutoQueueStatus,
  getAutoQueuePrimaryAction,
  normalizeAutoQueueStatus,
  shouldClearSuppressedAutoQueueRun,
} from "./auto-queue-panel-state";
import {
  generateAutoQueueForSelection,
  resetAutoQueueForSelection,
} from "./auto-queue-actions";

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
  skipped: {
    bg: "rgba(107,114,128,0.18)",
    text: "#9ca3af",
    label: "건너뜀",
    labelEn: "Skipped",
  },
};

const RUN_STATUS_STYLE: Record<
  AutoQueueRun["status"],
  { bg: string; text: string; label: string; labelEn: string }
> = {
  generated: {
    bg: "rgba(59,130,246,0.18)",
    text: "#60a5fa",
    label: "생성됨",
    labelEn: "Generated",
  },
  pending: {
    bg: "rgba(56,189,248,0.2)",
    text: "#38bdf8",
    label: "PMD 대기",
    labelEn: "Awaiting PMD",
  },
  active: {
    bg: "rgba(139,92,246,0.2)",
    text: "#a78bfa",
    label: "실행 중",
    labelEn: "Active",
  },
  paused: {
    bg: "rgba(245,158,11,0.2)",
    text: "#fbbf24",
    label: "일시정지",
    labelEn: "Paused",
  },
  completed: {
    bg: "rgba(34,197,94,0.2)",
    text: "#4ade80",
    label: "완료",
    labelEn: "Done",
  },
};

function reorderPendingIds(
  ids: string[],
  fromId: string,
  toId: string,
): string[] | null {
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
  "#a78bfa",
  "#38bdf8",
  "#f472b6",
  "#fbbf24",
  "#4ade80",
  "#fb923c",
  "#e879f9",
  "#22d3ee",
  "#a3e635",
  "#f87171",
];

function threadGroupColor(group: number): string {
  return THREAD_GROUP_COLORS[group % THREAD_GROUP_COLORS.length];
}

function EntryRow({
  entry,
  idx,
  tr,
  locale,
  onSkip,
  isDragging,
  isDropTarget,
  dragHandlers,
  moveControls,
  showThreadGroup,
}: {
  entry: DispatchQueueEntryType;
  idx: number;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onSkip: (id: string) => void;
  isDragging?: boolean;
  isDropTarget?: boolean;
  showThreadGroup?: boolean;
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
  const sty = ENTRY_STATUS_STYLE[entry.status] ?? ENTRY_STATUS_STYLE.pending;
  const isPending = entry.status === "pending";

  return (
    <div
      className="flex items-center gap-2 rounded-xl px-3 py-2 border transition-all"
      style={{
        borderColor: isDropTarget
          ? "rgba(139,92,246,0.6)"
          : entry.status === "dispatched"
            ? "rgba(245,158,11,0.3)"
            : "rgba(148,163,184,0.15)",
        backgroundColor: isDragging
          ? "rgba(139,92,246,0.12)"
          : isDropTarget
            ? "rgba(139,92,246,0.08)"
            : entry.status === "dispatched"
              ? "rgba(245,158,11,0.06)"
              : "var(--th-overlay-medium)",
        opacity: isDragging ? 0.5 : 1,
        cursor: isPending && dragHandlers?.draggable ? "grab" : undefined,
      }}
      {...(dragHandlers ?? {})}
    >
      {/* Drag handle for pending items */}
      {isPending && dragHandlers?.draggable && (
        <span
          className="text-xs shrink-0 select-none"
          style={{ color: "var(--th-text-muted)", cursor: "grab" }}
        >
          ⠿
        </span>
      )}
      <span
        className="text-xs font-mono shrink-0 w-5 text-center"
        style={{ color: "var(--th-text-muted)" }}
      >
        {idx + 1}
      </span>
      <div className="flex-1 min-w-0">
        <div
          className="text-xs truncate"
          style={{ color: "var(--th-text-primary)" }}
        >
          {showThreadGroup && entry.thread_group != null && (
            <span
              className="mr-1 text-xs font-mono px-1 py-0.5 rounded"
              style={{
                backgroundColor: `${threadGroupColor(entry.thread_group)}22`,
                color: threadGroupColor(entry.thread_group),
              }}
            >
              G{entry.thread_group}
            </span>
          )}
          {entry.github_issue_number && (
            <span
              className="mr-1 font-medium"
              style={{ color: "var(--th-text-muted)" }}
            >
              #{entry.github_issue_number}
            </span>
          )}
          {entry.card_title ?? entry.card_id.slice(0, 8)}
        </div>
        {entry.reason && (
          <div
            className="text-xs truncate"
            style={{ color: "var(--th-text-muted)" }}
          >
            {entry.reason}
          </div>
        )}
      </div>
      <span
        className="text-xs px-1.5 py-0.5 rounded shrink-0"
        style={{ backgroundColor: sty.bg, color: sty.text }}
      >
        {tr(sty.label, sty.labelEn)}
      </span>
      {isPending && moveControls && (
        <div
          className="inline-flex rounded-md overflow-hidden border shrink-0"
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
            className="px-1.5 py-0.5 text-xs border-l"
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
          onClick={() => onSkip(entry.id)}
          className="text-xs px-1.5 py-0.5 rounded border shrink-0"
          style={{
            borderColor: "rgba(148,163,184,0.2)",
            color: "var(--th-text-muted)",
          }}
        >
          {tr("건너뛰기", "Skip")}
        </button>
      )}
      {entry.dispatched_at && (
        <span
          className="text-xs shrink-0"
          style={{ color: "var(--th-text-muted)" }}
        >
          {formatTs(entry.dispatched_at, locale)}
        </span>
      )}
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
  const [expanded, setExpanded] = useState(false);
  const [generating, setGenerating] = useState(false);
  const [activating, setActivating] = useState(false);
  const [unifiedThread, setUnifiedThread] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [noReadyCards, setNoReadyCards] = useState(false);
  const [viewMode, setViewMode] = useState<ViewMode>("thread");

  const agentMap = new Map(agents.map((a) => [a.id, a]));
  const suppressedRunIdRef = useRef<string | null>(null);

  const resetPanelState = useCallback(() => {
    setStatus(createEmptyAutoQueueStatus());
    setError(null);
    setNoReadyCards(false);
    setUnifiedThread(false);
    setViewMode("thread");
    setGenerating(false);
    setActivating(false);
  }, []);

  const fetchStatus = useCallback(async () => {
    try {
      const s = await api.getAutoQueueStatus(
        selectedRepo || null,
        selectedAgentId,
      );
      const normalized = normalizeAutoQueueStatus(
        s,
        suppressedRunIdRef.current,
      );
      if (shouldClearSuppressedAutoQueueRun(s, suppressedRunIdRef.current)) {
        suppressedRunIdRef.current = null;
      }
      setStatus(normalized);
      // Sync unified_thread toggle from server state
      if (normalized.run?.unified_thread !== undefined) {
        setUnifiedThread(!!normalized.run.unified_thread);
      } else if (!normalized.run) {
        setUnifiedThread(false);
      }
      // Only reset noReadyCards when a run with entries exists
      if (!normalized.run || normalized.entries.length > 0)
        setNoReadyCards(false);
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

  const [generateMode, setGenerateMode] = useState<
    "priority-sort" | "dependency-aware" | "similarity-aware" | "pm-assisted"
  >("priority-sort");

  const handleGenerate = async () => {
    setGenerating(true);
    setError(null);
    try {
      suppressedRunIdRef.current = null;
      const result = (await generateAutoQueueForSelection(
        api,
        selectedRepo || null,
        selectedAgentId,
        generateMode,
      )) as Record<string, unknown>;
      if (
        result.entries &&
        Array.isArray(result.entries) &&
        result.entries.length === 0
      ) {
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
      await resetAutoQueueForSelection(api, selectedAgentId);
      resetPanelState();
    } catch (e) {
      suppressedRunIdRef.current = null;
      setError(
        e instanceof Error ? e.message : tr("초기화 실패", "Reset failed"),
      );
    }
  };

  const handleActivate = async () => {
    setActivating(true);
    setError(null);
    try {
      await api.activateAutoQueue(
        selectedRepo || null,
        selectedAgentId,
        unifiedThread,
      );
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

  const handleSkip = async (entryId: string) => {
    try {
      await api.skipAutoQueueEntry(entryId);
      await fetchStatus();
    } catch (e) {
      setError(
        e instanceof Error ? e.message : tr("건너뛰기 실패", "Skip failed"),
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
  const agentStats: Record<
    string,
    { pending: number; dispatched: number; done: number; skipped: number }
  > = status?.agents ?? {};

  const pendingCount = entries.filter((e) => e.status === "pending").length;
  const dispatchedCount = entries.filter(
    (e) => e.status === "dispatched",
  ).length;
  const doneCount = entries.filter((e) => e.status === "done").length;
  const totalCount = entries.length;
  const primaryAction = getAutoQueuePrimaryAction(run, pendingCount);
  const showRunStartControls =
    !!run &&
    (run.status === "generated" || run.status === "active") &&
    pendingCount > 0;
  const startActionLabel =
    run?.status === "generated"
      ? tr("시작", "Start")
      : tr("디스패치", "Dispatch");

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

  // Group entries by thread_group
  const entriesByThreadGroup = new Map<number, DispatchQueueEntryType[]>();
  for (const entry of entries) {
    const g = entry.thread_group ?? 0;
    const list = entriesByThreadGroup.get(g) ?? [];
    list.push(entry);
    entriesByThreadGroup.set(g, list);
  }

  // All-queue view: merge all entries sorted by status then rank
  const allEntriesSorted = [...entries].sort((a, b) => {
    const statusOrder: Record<string, number> = {
      dispatched: 0,
      pending: 1,
      done: 2,
      skipped: 3,
    };
    const sa = statusOrder[a.status] ?? 1;
    const sb = statusOrder[b.status] ?? 1;
    if (sa !== sb) return sa - sb;
    return a.priority_rank - b.priority_rank;
  });

  // Drag & drop for "all" view (pending only, no agent scope)
  const allDrag = useDragReorder(allEntriesSorted, handleReorder);

  return (
    <section
      className="rounded-2xl border p-3 sm:p-4 space-y-3"
      style={{
        borderColor: run ? "rgba(139,92,246,0.35)" : "rgba(148,163,184,0.22)",
        backgroundColor: "var(--th-bg-surface)",
      }}
    >
      {/* Header */}
      <div className="flex items-center justify-between gap-2 flex-wrap">
        <button
          onClick={() => setExpanded((p) => !p)}
          className="flex items-center gap-2 min-w-0"
        >
          <span className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {expanded ? "▾" : "▸"}
          </span>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {tr("자동 큐", "Auto Queue")}
          </h3>
          {run && (
            <span
              className="text-xs px-2 py-0.5 rounded-full"
              style={{
                backgroundColor: RUN_STATUS_STYLE[run.status].bg,
                color: RUN_STATUS_STYLE[run.status].text,
              }}
            >
              {tr(
                RUN_STATUS_STYLE[run.status].label,
                RUN_STATUS_STYLE[run.status].labelEn,
              )}
            </span>
          )}
          {totalCount > 0 && (
            <span
              className="text-xs px-1.5 py-0.5 rounded bg-surface-medium"
              style={{ color: "var(--th-text-muted)" }}
            >
              {doneCount}/{totalCount}
            </span>
          )}
        </button>

        <div className="flex items-center gap-2">
          {showRunStartControls && (
            <>
              <label
                className="flex items-center gap-1 text-xs cursor-pointer select-none"
                style={{
                  color: unifiedThread ? "#fbbf24" : "var(--th-text-secondary)",
                }}
              >
                <input
                  type="checkbox"
                  checked={unifiedThread}
                  onChange={(e) => {
                    const val = e.target.checked;
                    setUnifiedThread(val);
                    if (run?.id) {
                      void api.updateAutoQueueRun(run.id, undefined, val);
                    }
                  }}
                  style={{ cursor: "pointer", accentColor: "#f59e0b" }}
                />
                {tr("통합 스레드", "Unified Thread")}
              </label>
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
              <select
                value={generateMode}
                onChange={(e) =>
                  setGenerateMode(
                    e.target.value as
                      | "priority-sort"
                      | "dependency-aware"
                      | "similarity-aware"
                      | "pm-assisted",
                  )
                }
                className="text-[11px] px-2 py-1 rounded-lg border bg-transparent"
                style={{
                  borderColor: "rgba(148,163,184,0.22)",
                  color: "var(--th-text-secondary)",
                }}
              >
                <option value="priority-sort">
                  {tr("우선순위 정렬", "Priority Sort")}
                </option>
                <option value="dependency-aware">
                  {tr("의존관계 고려", "Dependency Aware")}
                </option>
                <option value="similarity-aware">
                  {tr("유사도 자동 그룹", "Similarity Aware")}
                </option>
                <option value="pm-assisted">
                  {tr("PM 분석 요청", "PM Assisted")}
                </option>
              </select>
              <button
                onClick={() => void handleGenerate()}
                disabled={generating || noReadyCards}
                className="text-xs px-2.5 py-1 rounded-lg border font-medium"
                style={{
                  borderColor: noReadyCards
                    ? "rgba(148,163,184,0.2)"
                    : "rgba(139,92,246,0.4)",
                  color: noReadyCards ? "var(--th-text-muted)" : "#a78bfa",
                  backgroundColor: noReadyCards
                    ? "rgba(148,163,184,0.05)"
                    : "rgba(139,92,246,0.1)",
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
              style={{ borderColor: "rgba(139,92,246,0.3)", color: "#a78bfa" }}
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
          {entries.filter((e) => e.status === "skipped").length > 0 && (
            <div
              className="rounded-full"
              style={{
                width: `${(entries.filter((e) => e.status === "skipped").length / totalCount) * 100}%`,
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
                              ? "rgba(139,92,246,0.2)"
                              : "transparent",
                          color:
                            viewMode === "thread"
                              ? "#a78bfa"
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
                            ? "rgba(139,92,246,0.2)"
                            : "transparent",
                        color:
                          viewMode === "all"
                            ? "#a78bfa"
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
                              ? "rgba(139,92,246,0.2)"
                              : "transparent",
                          color:
                            viewMode === "agent"
                              ? "#a78bfa"
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
                      onSkip={handleSkip}
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
              {Array.from(entriesByThreadGroup.entries())
                .sort(([a], [b]) => a - b)
                .map(([groupNum, groupEntries]) => {
                  const tgStatus = threadGroups[String(groupNum)];
                  const isActive =
                    tgStatus?.status === "active" ||
                    groupEntries.some((e) => e.status === "dispatched");
                  const isDone =
                    tgStatus?.status === "done" ||
                    groupEntries.every(
                      (e) => e.status === "done" || e.status === "skipped",
                    );
                  const groupStatusLabel = isActive
                    ? tr("진행", "Active")
                    : isDone
                      ? tr("완료", "Done")
                      : tr("대기", "Pending");
                  const color = threadGroupColor(groupNum);
                  return (
                    <div
                      key={groupNum}
                      className="rounded-xl border p-2 space-y-1"
                      style={{
                        borderColor: isActive
                          ? `${color}55`
                          : isDone
                            ? "rgba(34,197,94,0.2)"
                            : "rgba(148,163,184,0.12)",
                        backgroundColor: isActive
                          ? `${color}0a`
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
                              : isDone
                                ? "rgba(34,197,94,0.18)"
                                : "rgba(100,116,139,0.18)",
                            color: isActive
                              ? "#fbbf24"
                              : isDone
                                ? "#4ade80"
                                : "#94a3b8",
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
                          {
                            groupEntries.filter((e) => e.status === "done")
                              .length
                          }
                          /{groupEntries.length}
                        </span>
                      </div>
                      {tgStatus?.reason && (
                        <div
                          className="px-1 text-[10px]"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {tgStatus.reason}
                        </div>
                      )}
                      {groupEntries.map((entry, idx) => (
                        <EntryRow
                          key={entry.id}
                          entry={entry}
                          idx={idx}
                          tr={tr}
                          locale={locale}
                          onSkip={handleSkip}
                        />
                      ))}
                    </div>
                  );
                })}
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
                  onSkip={handleSkip}
                  onReorder={handleReorder}
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
  onSkip,
  onReorder,
}: {
  agentId: string;
  agentEntries: DispatchQueueEntryType[];
  getAgentLabel: (id: string) => string;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onSkip: (id: string) => void;
  onReorder: (orderedIds: string[], agentId?: string | null) => Promise<void>;
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
          {agentEntries.filter((e) => e.status === "done").length}/
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
          onSkip={onSkip}
          isDragging={drag.dragId === entry.id}
          isDropTarget={drag.dropTargetId === entry.id}
          dragHandlers={drag.makeDragHandlers(entry)}
          moveControls={drag.makeMoveControls(entry)}
        />
      ))}
    </div>
  );
}
