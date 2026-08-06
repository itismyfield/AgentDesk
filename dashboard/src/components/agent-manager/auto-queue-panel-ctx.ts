import type { Dispatch, SetStateAction } from "react";
import type {
  AutoQueueRun,
  DispatchQueueEntry,
  PhaseGateInfo,
  ThreadGroupStatus,
} from "../../api";
import type { UiLanguage } from "../../types";
import type { ReadyAutoQueueEntry } from "./auto-queue-actions";
import type { AutoQueuePrimaryAction } from "./auto-queue-panel-state";
import type { ViewMode } from "./auto-queue-panel-utils";
import type { SortableReorderController } from "./AutoQueueSortableRows";

/**
 * #5131: `AutoQueuePanel` hands a single bag of derived state to its view tree
 * (`AutoQueuePanelView` → `AutoQueuePanelHeader` / `createAutoQueuePhaseRenderers`).
 * That bag used to be typed `any` on every consumer, so deleting a producer key
 * while a consumer still destructured it type-checked cleanly and only blew up
 * at runtime (`deployPhases.has(...)` on `undefined`). These interfaces make the
 * producer/consumer contract explicit: dropping a key from the object literal in
 * `AutoQueuePanel` is now a compile error at the `AutoQueuePanelView` call site.
 */

export interface AutoQueueAgentStats {
  pending: number;
  dispatched: number;
  done: number;
  skipped: number;
  failed: number;
}

export interface AutoQueueRequestProgress {
  startedAt: number;
  baselineEntryIds: Set<string>;
  pendingGroups: Set<string>;
  satisfiedGroups: Set<string>;
  errors: { groupKey: string; message: string }[];
}

/**
 * The exact slice of the panel context consumed by
 * `createAutoQueuePhaseRenderers`. Every member here must be produced by
 * `AutoQueuePanel`; there is deliberately no index signature so that an unknown
 * or removed key fails type checking rather than surfacing as `undefined`.
 */
export interface AutoQueuePhaseRendererCtx {
  currentBatchPhase: number | null;
  gatesByPhase: Map<number, PhaseGateInfo[]>;
  hasBatchPhases: boolean;
  handleEntryStatusUpdate: (entryId: string, status: "pending" | "skipped") => void;
  locale: UiLanguage;
  threadGroups: Record<string, ThreadGroupStatus>;
  tr: (ko: string, en: string) => string;
}

export interface AutoQueuePanelCtx extends AutoQueuePhaseRendererCtx {
  activating: boolean;
  agentStats: Record<string, AutoQueueAgentStats>;
  allDrag: SortableReorderController;
  allEntriesSorted: DispatchQueueEntry[];
  completedCount: number;
  dispatchedCount: number;
  doneCount: number;
  entries: DispatchQueueEntry[];
  entriesByAgent: Map<string, DispatchQueueEntry[]>;
  entriesByThreadGroup: Map<number, DispatchQueueEntry[]>;
  error: string | null;
  expanded: boolean;
  failedCount: number;
  generating: boolean;
  getAgentLabel: (agentId: string) => string;
  handleActivate: () => void;
  handleFallbackActivate: (runId: string) => void;
  handleGenerate: () => void;
  handleReorder: (orderedIds: string[], agentId?: string | null) => Promise<void>;
  handleReset: () => void;
  handleRunAction: (run: AutoQueueRun, action: "pause" | "resume" | "end") => void;
  hasThreadGroups: boolean;
  maxConcurrent: number;
  pendingCount: number;
  phaseSections: Array<[number, DispatchQueueEntry[]]>;
  primaryAction: AutoQueuePrimaryAction;
  readyEntries: ReadyAutoQueueEntry[];
  requestProgress: AutoQueueRequestProgress | null;
  run: AutoQueueRun | null;
  selectedRepo: string;
  setExpanded: Dispatch<SetStateAction<boolean>>;
  setViewMode: Dispatch<SetStateAction<ViewMode>>;
  showRunStartControls: boolean;
  skippedCount: number;
  startActionLabel: string;
  totalCount: number;
  viewMode: ViewMode;
}
