import { describe, expect, it, vi } from "vitest";

import {
  buildRequestGenerateGroups,
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
    );

    expect(resetAutoQueue).toHaveBeenCalledWith({
      repo: "test-repo",
      agentId: "agent-selected",
    });
    expect(generateAutoQueue).toHaveBeenCalledWith("test-repo", "agent-selected");
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
