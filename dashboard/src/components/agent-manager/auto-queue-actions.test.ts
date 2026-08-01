import { describe, expect, it, vi } from "vitest";

import {
  buildRequestGenerateGroups,
  generateAutoQueueForSelection,
  resetAutoQueueForSelection,
} from "./auto-queue-actions";

describe("auto-queue-actions", () => {
  it("uses the selected repo for generation but not as a reset ownership claim", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });
    const generateAutoQueue = vi
      .fn()
      .mockResolvedValue({ ok: true, entries: [] });

    await generateAutoQueueForSelection(
      { resetAutoQueue, generateAutoQueue },
      "test-repo",
      "agent-selected",
      "run-123",
    );

    expect(resetAutoQueue).toHaveBeenCalledWith({
      agentId: "agent-selected",
      runId: "run-123",
    });
    expect(generateAutoQueue).toHaveBeenCalledWith("test-repo", "agent-selected");
  });

  it("omits the dashboard repo filter when resetting a multi-repo run", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

    await resetAutoQueueForSelection(
      { resetAutoQueue },
      "run-agent",
      "run-multi-repo",
    );

    expect(resetAutoQueue).toHaveBeenCalledWith({
      agentId: "run-agent",
      runId: "run-multi-repo",
    });
    expect(resetAutoQueue).toHaveBeenCalledTimes(1);
  });

  it("rejects a reset without the active run scope", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

    await expect(
      resetAutoQueueForSelection(
        { resetAutoQueue },
        "agent-selected",
        null,
      ),
    ).rejects.toThrow("run_id is required for reset");
    expect(resetAutoQueue).not.toHaveBeenCalled();
  });

  it("groups request-generate candidates by repo and agent", () => {
    expect(
      buildRequestGenerateGroups(
        [
          { repo: "repo-a", agentId: "agent-a", issueNumber: 3 },
          { repo: "repo-a", agentId: "agent-a", issueNumber: 1 },
          { repo: "repo-a", agentId: "agent-b", issueNumber: 2 },
          { repo: "repo-b", agentId: "agent-a", issueNumber: 5 },
          { repo: null, agentId: "agent-a", issueNumber: 8 },
        ],
        "fallback",
      ),
    ).toEqual([
      { repo: "fallback", agentId: "agent-a", issueNumbers: [8] },
      { repo: "repo-a", agentId: "agent-a", issueNumbers: [1, 3] },
      { repo: "repo-a", agentId: "agent-b", issueNumbers: [2] },
      { repo: "repo-b", agentId: "agent-a", issueNumbers: [5] },
    ]);
  });

  it("uses the selected repo when a ready entry has an empty repo", () => {
    expect(
      buildRequestGenerateGroups(
        [{ repo: "", agentId: "agent-a", issueNumber: 9 }],
        "fallback",
      ),
    ).toEqual([
      { repo: "fallback", agentId: "agent-a", issueNumbers: [9] },
    ]);
  });
});
