import { useCallback, useMemo, useRef, useState } from "react";
import {
  closestCenter,
  DndContext,
  KeyboardSensor,
  MouseSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragOverEvent,
  type DragStartEvent,
} from "@dnd-kit/core";
import { SortableContext, arrayMove, sortableKeyboardCoordinates, useSortable, verticalListSortingStrategy } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripVertical } from "lucide-react";
import type { DispatchQueueEntry as DispatchQueueEntryType } from "../../api";
import type { UiLanguage } from "../../types";
import { EntryRow } from "./AutoQueueEntryRow";
import { isCompletedEntry, reorderPendingIds, shiftPendingId } from "./auto-queue-panel-utils";

export function useSortableReorder(
  entries: DispatchQueueEntryType[],
  onReorder: (orderedIds: string[], agentId?: string | null) => Promise<void>,
  agentId?: string | null,
) {
  const [activeId, setActiveId] = useState<string | null>(null);
  const [overId, setOverId] = useState<string | null>(null);
  const reorderingRef = useRef(false);
  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const pendingIds = useMemo(
    () => entries.filter((entry) => entry.status === "pending").map((entry) => entry.id),
    [entries],
  );

  const guardedReorder = useCallback(async (orderedIds: string[]) => {
    if (reorderingRef.current) return;
    reorderingRef.current = true;
    try {
      await onReorder(orderedIds, agentId);
    } finally {
      reorderingRef.current = false;
    }
  }, [agentId, onReorder]);

  const handleDragStart = useCallback(
    (event: DragStartEvent) => {
      const id = String(event.active.id);
      if (!pendingIds.includes(id)) return;
      setActiveId(id);
      setOverId(null);
    },
    [pendingIds],
  );

  const handleDragOver = useCallback(
    (event: DragOverEvent) => {
      setOverId(event.over ? String(event.over.id) : null);
    },
    [],
  );

  const handleDragEnd = useCallback(
    (event: DragEndEvent) => {
      const fromId = String(event.active.id);
      const toId = event.over ? String(event.over.id) : null;
      setActiveId(null);
      setOverId(null);
      if (!toId || fromId === toId) return;

      const fromIndex = pendingIds.indexOf(fromId);
      const toIndex = pendingIds.indexOf(toId);
      if (fromIndex === -1 || toIndex === -1) return;
      void guardedReorder(arrayMove(pendingIds, fromIndex, toIndex));
    },
    [guardedReorder, pendingIds],
  );

  const handleDragCancel = useCallback(() => {
    setActiveId(null);
    setOverId(null);
  }, []);

  const makeMoveControls = (entry: DispatchQueueEntryType) => {
    if (entry.status !== "pending") return undefined;

    const index = pendingIds.indexOf(entry.id);
    if (index === -1) return undefined;

    return {
      canMoveUp: index > 0 && !reorderingRef.current,
      canMoveDown: index < pendingIds.length - 1 && !reorderingRef.current,
      onMoveUp: () => {
        const reorderedIds = shiftPendingId(pendingIds, entry.id, -1);
        if (reorderedIds) void guardedReorder(reorderedIds);
      },
      onMoveDown: () => {
        const reorderedIds = shiftPendingId(pendingIds, entry.id, 1);
        if (reorderedIds) void guardedReorder(reorderedIds);
      },
    };
  };

  return {
    activeId,
    overId,
    pendingIds,
    sensors,
    handleDragStart,
    handleDragOver,
    handleDragEnd,
    handleDragCancel,
    makeMoveControls,
  };
}

type SortableReorderController = ReturnType<typeof useSortableReorder>;

export function SortableEntryRow({
  entry,
  idx,
  tr,
  locale,
  onUpdateStatus,
  drag,
  showThreadGroup,
  showBatchPhase,
}: {
  entry: DispatchQueueEntryType;
  idx: number;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onUpdateStatus: (id: string, status: "pending" | "skipped") => void;
  drag: SortableReorderController;
  showThreadGroup?: boolean;
  showBatchPhase?: boolean;
}) {
  if (entry.status !== "pending") {
    return (
      <EntryRow
        entry={entry}
        idx={idx}
        tr={tr}
        locale={locale}
        onUpdateStatus={onUpdateStatus}
        showThreadGroup={showThreadGroup}
        showBatchPhase={showBatchPhase}
        moveControls={drag.makeMoveControls(entry)}
      />
    );
  }

  return (
    <SortablePendingEntryRow
      entry={entry}
      idx={idx}
      tr={tr}
      locale={locale}
      onUpdateStatus={onUpdateStatus}
      drag={drag}
      showThreadGroup={showThreadGroup}
      showBatchPhase={showBatchPhase}
    />
  );
}

function SortablePendingEntryRow({
  entry,
  idx,
  tr,
  locale,
  onUpdateStatus,
  drag,
  showThreadGroup,
  showBatchPhase,
}: {
  entry: DispatchQueueEntryType;
  idx: number;
  tr: (ko: string, en: string) => string;
  locale: UiLanguage;
  onUpdateStatus: (id: string, status: "pending" | "skipped") => void;
  drag: SortableReorderController;
  showThreadGroup?: boolean;
  showBatchPhase?: boolean;
}) {
  const {
    attributes,
    isDragging,
    listeners,
    setActivatorNodeRef,
    setNodeRef,
    transform,
    transition,
  } = useSortable({ id: entry.id });
  const isActive = drag.activeId === entry.id || isDragging;
  const isDropTarget = drag.overId === entry.id && drag.activeId !== entry.id;

  return (
    <div
      ref={setNodeRef}
      style={{
        transform: CSS.Transform.toString(transform),
        transition: transition ?? undefined,
      }}
    >
      <EntryRow
        entry={entry}
        idx={idx}
        tr={tr}
        locale={locale}
        onUpdateStatus={onUpdateStatus}
        isDragging={isActive}
        isDropTarget={isDropTarget}
        showThreadGroup={showThreadGroup}
        showBatchPhase={showBatchPhase}
        moveControls={drag.makeMoveControls(entry)}
        dragHandle={
          <button
            ref={setActivatorNodeRef}
            type="button"
            data-testid={`autoqueue-drag-handle-${entry.id}`}
            aria-label={tr("큐 항목 순서 변경", "Reorder queue item")}
            className="shrink-0 rounded-md p-1 transition-colors hover:bg-white/10"
            style={{
              color: "var(--th-text-muted)",
              cursor: "grab",
              touchAction: "none",
            }}
            {...attributes}
            {...listeners}
          >
            <GripVertical size={13} />
          </button>
        }
      />
    </div>
  );
}

// ── Main Panel ──

export function AgentSubQueue({
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
  const drag = useSortableReorder(agentEntries, onReorder, agentId);

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
      <DndContext
        sensors={drag.sensors}
        collisionDetection={closestCenter}
        onDragStart={drag.handleDragStart}
        onDragOver={drag.handleDragOver}
        onDragEnd={drag.handleDragEnd}
        onDragCancel={drag.handleDragCancel}
      >
        <SortableContext items={drag.pendingIds} strategy={verticalListSortingStrategy}>
          {agentEntries.map((entry, idx) => (
            <SortableEntryRow
              key={entry.id}
              entry={entry}
              idx={idx}
              tr={tr}
              locale={locale}
              onUpdateStatus={onUpdateStatus}
              drag={drag}
              showBatchPhase={showBatchPhase}
            />
          ))}
        </SortableContext>
      </DndContext>
    </div>
  );
}
