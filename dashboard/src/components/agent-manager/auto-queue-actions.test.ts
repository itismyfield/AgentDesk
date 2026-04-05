import { describe, expect, it, vi } from "vitest";

import {
  generateAutoQueueForSelection,
  resetAutoQueueForSelection,
} from "./auto-queue-actions";

describe("auto-queue-actions", () => {
  it("passes the selected agent to reset and generate", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });
    const generateAutoQueue = vi
      .fn()
      .mockResolvedValue({ ok: true, entries: [] });

    await generateAutoQueueForSelection(
      { resetAutoQueue, generateAutoQueue },
      "test-repo",
      "agent-selected",
      "priority-sort",
    );

    expect(resetAutoQueue).toHaveBeenCalledWith("agent-selected");
    expect(generateAutoQueue).toHaveBeenCalledWith(
      "test-repo",
      "agent-selected",
      "priority-sort",
    );
  });

  it("passes the selected agent to reset-only actions", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

    await resetAutoQueueForSelection({ resetAutoQueue }, "agent-selected");

    expect(resetAutoQueue).toHaveBeenCalledWith("agent-selected");
  });
});
