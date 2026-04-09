import { describe, expect, it, vi } from "vitest";

import {
  generateAutoQueueForSelection,
  resetAutoQueueForSelection,
} from "./auto-queue-actions";

describe("auto-queue-actions", () => {
  it("passes the selected repo and agent to reset and generate", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });
    const generateAutoQueue = vi
      .fn()
      .mockResolvedValue({ ok: true, entries: [] });

    await generateAutoQueueForSelection(
      { resetAutoQueue, generateAutoQueue },
      "test-repo",
      "agent-selected",
      "priority-sort",
      true,
    );

    expect(resetAutoQueue).toHaveBeenCalledWith({
      repo: "test-repo",
      agentId: "agent-selected",
    });
    expect(generateAutoQueue).toHaveBeenCalledWith(
      "test-repo",
      "agent-selected",
      "priority-sort",
      true,
    );
  });

  it("passes the selected scope to reset-only actions", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

    await resetAutoQueueForSelection(
      { resetAutoQueue },
      "test-repo",
      "agent-selected",
      "run-123",
    );

    expect(resetAutoQueue).toHaveBeenCalledWith({
      repo: "test-repo",
      agentId: "agent-selected",
      runId: "run-123",
    });
  });
});
