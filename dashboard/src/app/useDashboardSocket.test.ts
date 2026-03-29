import { describe, expect, it } from "vitest";
import { normalizeDashboardSocketEvent } from "./useDashboardSocket";

describe("normalizeDashboardSocketEvent", () => {
  it("maps backend data field to payload", () => {
    expect(
      normalizeDashboardSocketEvent({
        type: "dispatched_session_update",
        data: { id: "42", status: "idle" },
      }),
    ).toEqual({
      type: "dispatched_session_update",
      payload: { id: "42", status: "idle" },
      ts: undefined,
    });
  });

  it("preserves explicit payload field", () => {
    expect(
      normalizeDashboardSocketEvent({
        type: "kanban_card_updated",
        payload: { id: "card-1" },
        data: { id: "wrong" },
        ts: 123,
      }),
    ).toEqual({
      type: "kanban_card_updated",
      payload: { id: "card-1" },
      ts: 123,
    });
  });

  it("returns null for malformed events", () => {
    expect(normalizeDashboardSocketEvent(null)).toBeNull();
    expect(normalizeDashboardSocketEvent({})).toBeNull();
    expect(normalizeDashboardSocketEvent({ type: 123 })).toBeNull();
  });
});
