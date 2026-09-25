import { describe, expect, it, vi } from "vitest";

import {
  buildRequestGenerateGroups,
  resetAutoQueueForSelection,
} from "./auto-queue-actions";

describe("auto-queue-actions", () => {
  it("resets the shown run with exactly one run-pinned call", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

    await expect(
      resetAutoQueueForSelection(
        { resetAutoQueue },
        "test-repo",
        "agent-selected",
        "run-123",
      ),
    ).resolves.toBe(true);

    expect(resetAutoQueue).toHaveBeenCalledExactlyOnceWith({
      runId: "run-123",
      repo: "test-repo",
      agentId: "agent-selected",
    });
  });

  it("resets a NULL-agent run once without inventing an agent scope", async () => {
    const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

    await resetAutoQueueForSelection({ resetAutoQueue }, "test-repo", null, "run-123");

    expect(resetAutoQueue).toHaveBeenCalledExactlyOnceWith({
      runId: "run-123",
      repo: "test-repo",
      agentId: undefined,
    });
  });

  it.each([null, undefined, ""])(
    "does not call reset when there is no run (runId=%j)",
    async (runId) => {
      const resetAutoQueue = vi.fn().mockResolvedValue({ ok: true });

      await expect(
        resetAutoQueueForSelection({ resetAutoQueue }, "test-repo", "agent-selected", runId),
      ).resolves.toBe(false);

      expect(resetAutoQueue).not.toHaveBeenCalled();
    },
  );

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
