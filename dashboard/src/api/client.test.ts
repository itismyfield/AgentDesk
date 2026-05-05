import { afterEach, describe, expect, it, vi } from "vitest";

import {
  assignKanbanIssue,
  redispatchKanbanCard,
  retryKanbanCard,
} from "./client";

const card = {
  id: "card-1",
  title: "Contract card",
  status: "requested",
  priority: "medium",
};

function mockJsonResponse(body: unknown): Response {
  return {
    ok: true,
    status: 200,
    json: vi.fn().mockResolvedValue(body),
  } as unknown as Response;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("kanban dispatch mutation responses", () => {
  it("rejects assign issue responses missing stable transition fields", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        mockJsonResponse({
          card,
          assignment: { ok: true, agent_id: "agent-1" },
          transition: {
            attempted: true,
            ok: true,
            target_status: "requested",
            next_action: "none_required",
          },
        }),
      ),
    );

    await expect(
      assignKanbanIssue({
        github_repo: "itismyfield/AgentDesk",
        github_issue_number: 1733,
        title: "Contract card",
        assignee_agent_id: "agent-1",
      }),
    ).rejects.toThrow("missing required field 'error'");
  });

  it("returns the full retry contract instead of dropping dispatch fields", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      mockJsonResponse({
        card,
        new_dispatch_id: "dispatch-new",
        cancelled_dispatch_id: null,
        next_action: "none_required",
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await retryKanbanCard("card-1", { request_now: true });

    expect(result.card.id).toBe("card-1");
    expect(result.new_dispatch_id).toBe("dispatch-new");
    expect(result.cancelled_dispatch_id).toBeNull();
    expect(result.next_action).toBe("none_required");
  });

  it("rejects redispatch responses that omit required contract fields", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        mockJsonResponse({
          card,
          new_dispatch_id: "dispatch-new",
          cancelled_dispatch_id: "dispatch-old",
        }),
      ),
    );

    await expect(redispatchKanbanCard("card-1")).rejects.toThrow(
      "missing required field 'next_action'",
    );
  });
});
