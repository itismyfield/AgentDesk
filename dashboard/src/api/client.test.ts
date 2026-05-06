import { afterEach, describe, expect, it, vi } from "vitest";

import {
  assignKanbanIssue,
  getDispatchDeliveryEvents,
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

describe("dispatch delivery events", () => {
  it("requests the read-only dispatch events endpoint", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      mockJsonResponse({
        dispatch_id: "dispatch-1",
        events: [
          {
            id: 1,
            dispatch_id: "dispatch-1",
            correlation_id: "dispatch:dispatch-1",
            semantic_event_id: "dispatch:dispatch-1:notify",
            operation: "send",
            target_kind: "channel",
            target_channel_id: "1500000000000000000",
            target_thread_id: null,
            status: "sent",
            attempt: 1,
            message_id: "1500000000000000001",
            messages_json: [],
            fallback_kind: null,
            error: null,
            result_json: { status: "success" },
            reserved_until: null,
            created_at: "2026-05-06T08:00:00Z",
            updated_at: "2026-05-06T08:00:01Z",
          },
        ],
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await getDispatchDeliveryEvents("dispatch/needs encode");

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/dispatches/dispatch%2Fneeds%20encode/events",
      expect.objectContaining({ credentials: "include" }),
    );
    expect(result.events[0].status).toBe("sent");
  });
});
