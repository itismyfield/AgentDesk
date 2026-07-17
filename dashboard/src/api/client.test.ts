import { afterEach, describe, expect, it, vi } from "vitest";
import { z } from "zod";

import {
  assignKanbanIssue,
  getDispatchDeliveryEvents,
  getSkillRanking,
  onApiError,
  readCachedGet,
  redispatchKanbanCard,
  request,
  retryKanbanCard,
} from "./client";

const card = {
  id: "card-1",
  title: "Contract card",
  description: null,
  status: "requested",
  github_repo: "itismyfield/AgentDesk",
  owner_agent_id: null,
  requester_agent_id: null,
  assignee_agent_id: "agent-1",
  parent_card_id: null,
  latest_dispatch_id: null,
  sort_order: 0,
  priority: "medium",
  depth: 0,
  blocked_reason: null,
  review_notes: null,
  github_issue_number: 1733,
  github_issue_url: "https://github.com/itismyfield/AgentDesk/issues/1733",
  metadata_json: null,
  pipeline_stage_id: null,
  review_status: null,
  created_at: "2026-07-17T00:00:00Z",
  updated_at: "2026-07-17T00:00:00Z",
  started_at: null,
  requested_at: null,
  completed_at: null,
};

function mockJsonResponse(body: unknown): Response {
  return {
    ok: true,
    status: 200,
    json: vi.fn().mockResolvedValue(body),
  } as unknown as Response;
}

function mockErrorResponse(status: number, body: unknown): Response {
  return {
    ok: false,
    status,
    json: vi.fn().mockResolvedValue(body),
  } as unknown as Response;
}

afterEach(() => {
  onApiError(null);
  vi.unstubAllGlobals();
});

describe("runtime response parsing", () => {
  it("returns and caches the parser output instead of the raw GET payload", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(mockJsonResponse({ value: "42" })),
    );
    const endpoint = "/api/parser-output";
    const schema = z.object({ value: z.coerce.number() });

    const result = await request(endpoint, undefined, schema);

    expect(result).toEqual({ value: 42 });
    expect(readCachedGet(endpoint)?.data).toEqual({ value: 42 });
  });

  it("rejects invalid GET payloads before they reach the response cache", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(mockJsonResponse({ value: "invalid" })),
    );
    const endpoint = "/api/parser-rejection";
    const schema = z.object({ value: z.number() });

    await expect(request(endpoint, undefined, schema)).rejects.toThrow();
    expect(readCachedGet(endpoint)).toBeNull();
  });
});

describe("global API error reporting", () => {
  it("reports unsuppressed API errors to the toast listener", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(mockErrorResponse(500, { error: "boom" })),
    );
    const listener = vi.fn();
    onApiError(listener);

    await expect(request("/api/failing", { maxRetries: 0 })).rejects.toThrow(
      "boom",
    );

    expect(listener).toHaveBeenCalledWith(
      "/api/failing",
      expect.objectContaining({ message: "boom" }),
    );
  });

  it("keeps auxiliary skill analytics errors out of global toasts", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(mockErrorResponse(503, { error: "slow" })),
    );
    const listener = vi.fn();
    onApiError(listener);

    await expect(
      getSkillRanking("7d", 16, { maxRetries: 0 }),
    ).rejects.toThrow("slow");

    expect(listener).not.toHaveBeenCalled();
  });
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
    ).rejects.toThrow(/transition[\s\S]*error/);
  });

  it("returns the full retry contract without changing server timestamps", async () => {
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
    expect(result.card.created_at).toBe(card.created_at);
    expect(typeof result.card.created_at).toBe("string");
    expect(result.new_dispatch_id).toBe("dispatch-new");
    expect(result.cancelled_dispatch_id).toBeNull();
    expect(result.next_action).toBe("none_required");
  });

  it.each(["failed", "cancelled"])(
    "accepts the %s server status and preserves PostgreSQL timestamps",
    async (status) => {
      const postgresTimestamp = "2026-07-17 00:00:00.123456+00";
      vi.stubGlobal(
        "fetch",
        vi.fn().mockResolvedValue(
          mockJsonResponse({
            card: {
              ...card,
              status,
              created_at: postgresTimestamp,
              updated_at: postgresTimestamp,
            },
            new_dispatch_id: null,
            cancelled_dispatch_id: "dispatch-old",
            next_action: "none_required",
          }),
        ),
      );

      const result = await retryKanbanCard("card-1");

      expect(result.card.status).toBe(status);
      expect(result.card.created_at).toBe(postgresTimestamp);
      expect(typeof result.card.created_at).toBe("string");
    },
  );

  it.each([
    { label: "custom", priority: "critical_path" },
    { label: "empty", priority: "" },
    { label: "whitespace-only", priority: "   " },
  ])("accepts and preserves $label server priority strings", async ({ priority }) => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        mockJsonResponse({
          card: { ...card, priority },
          new_dispatch_id: null,
          cancelled_dispatch_id: null,
          next_action: "none_required",
        }),
      ),
    );

    const result = await retryKanbanCard("card-1");

    expect(result.card.priority).toBe(priority);
  });

  it("rejects retry responses with malformed Kanban cards", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        mockJsonResponse({
          card: { ...card, status: "Unexpected status" },
          new_dispatch_id: "dispatch-new",
          cancelled_dispatch_id: null,
          next_action: "none_required",
        }),
      ),
    );

    await expect(retryKanbanCard("card-1")).rejects.toThrow(/card[\s\S]*status/);
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
      /next_action/,
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
