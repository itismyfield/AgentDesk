import type { AutoQueueRun, AutoQueueStatus } from "../../api";

export type AutoQueuePrimaryAction = "generate" | "start" | "dispatch" | null;

export function createEmptyAutoQueueStatus(): AutoQueueStatus {
  return {
    run: null,
    entries: [],
    agents: {},
    thread_groups: {},
  };
}

export function normalizeAutoQueueStatus(
  status: AutoQueueStatus,
  suppressedRunId: string | null,
): AutoQueueStatus {
  if (!status.run) return status;
  if (suppressedRunId && status.run.id === suppressedRunId && status.entries.length === 0) {
    return createEmptyAutoQueueStatus();
  }
  return status;
}

export function shouldClearSuppressedAutoQueueRun(
  status: AutoQueueStatus,
  suppressedRunId: string | null,
): boolean {
  if (!suppressedRunId) return false;
  if (!status.run) return true;
  return status.run.id !== suppressedRunId;
}

export function getAutoQueuePrimaryAction(
  run: AutoQueueRun | null,
  pendingCount: number,
): AutoQueuePrimaryAction {
  if (!run || run.status === "completed") return "generate";
  if (run.status === "generated" && pendingCount > 0) return "start";
  if (run.status === "active" && pendingCount > 0) return "dispatch";
  return null;
}

/** Mirrors `LIVE_RUN_STATUSES` in `src/db/auto_queue/run_status.rs`. */
const LIVE_AUTO_QUEUE_RUN_STATUSES: readonly string[] = ["active", "paused", "restoring"];

/** A live run still owns dispatches and slots, so the server refuses to reset it. */
export function isLiveAutoQueueRunStatus(status: string): boolean {
  return LIVE_AUTO_QUEUE_RUN_STATUSES.includes(status);
}
