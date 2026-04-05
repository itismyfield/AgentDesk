import { describe, expect, it } from "vitest";

import type { AutoQueueRun, AutoQueueStatus } from "../../api";
import {
  createEmptyAutoQueueStatus,
  getAutoQueuePrimaryAction,
  normalizeAutoQueueStatus,
  shouldClearSuppressedAutoQueueRun,
} from "./auto-queue-panel-state";

function makeRun(
  status: AutoQueueRun["status"],
  id = "run-1",
): AutoQueueRun {
  return {
    id,
    repo: "test-repo",
    agent_id: "agent-1",
    status,
    ai_model: null,
    ai_rationale: null,
    timeout_minutes: 120,
    unified_thread: false,
    unified_thread_id: null,
    created_at: 0,
    completed_at: null,
  };
}

function makeStatus(run: AutoQueueRun | null, entryCount = 0): AutoQueueStatus {
  return {
    run,
    entries: Array.from({ length: entryCount }, (_, index) => ({
      id: `entry-${index}`,
      agent_id: "agent-1",
      card_id: `card-${index}`,
      priority_rank: index,
      reason: null,
      status: "pending",
      created_at: 0,
      dispatched_at: null,
      completed_at: null,
    })),
    agents: {},
    thread_groups: {},
  };
}

describe("auto-queue-panel-state", () => {
  it("suppresses a reset-cleared run until a new run appears", () => {
    const status = makeStatus(makeRun("generated", "run-reset"), 0);

    expect(normalizeAutoQueueStatus(status, "run-reset")).toEqual(createEmptyAutoQueueStatus());
  });

  it("keeps a PM-assisted pending run when it was not suppressed", () => {
    const status = makeStatus(makeRun("pending", "run-pmd"), 0);

    expect(normalizeAutoQueueStatus(status, null)).toEqual(status);
  });

  it("keeps suppression while the server still returns the reset-cleared run", () => {
    const status = makeStatus(makeRun("generated", "run-reset"), 0);

    expect(shouldClearSuppressedAutoQueueRun(status, "run-reset")).toBe(false);
  });

  it("clears suppression when the server returns a different run or no run", () => {
    expect(shouldClearSuppressedAutoQueueRun(makeStatus(makeRun("generated", "run-next"), 2), "run-reset")).toBe(true);
    expect(shouldClearSuppressedAutoQueueRun(createEmptyAutoQueueStatus(), "run-reset")).toBe(true);
  });

  it("returns the correct primary action per run state", () => {
    expect(getAutoQueuePrimaryAction(null, 0)).toBe("generate");
    expect(getAutoQueuePrimaryAction(makeRun("completed"), 0)).toBe("generate");
    expect(getAutoQueuePrimaryAction(makeRun("generated"), 2)).toBe("start");
    expect(getAutoQueuePrimaryAction(makeRun("active"), 2)).toBe("dispatch");
    expect(getAutoQueuePrimaryAction(makeRun("generated"), 0)).toBeNull();
  });
});
