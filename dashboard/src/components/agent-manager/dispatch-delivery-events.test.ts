import { describe, expect, it } from "vitest";

import {
  DELIVERY_EVENT_STATUS_STYLE,
  createDeliveryEventsLoadState,
  deliveryEventMessagesCount,
  finishDeliveryEventsLoadError,
  finishDeliveryEventsLoadSuccess,
  startDeliveryEventsLoad,
  summarizeDeliveryError,
} from "./dispatch-delivery-events";

describe("dispatch delivery event helpers", () => {
  it("keeps the documented statuses visually distinct", () => {
    expect(Object.keys(DELIVERY_EVENT_STATUS_STYLE).sort()).toEqual([
      "duplicate",
      "failed",
      "fallback",
      "reserved",
      "sent",
      "skipped",
    ]);
    expect(DELIVERY_EVENT_STATUS_STYLE.reserved.text).toBe(
      DELIVERY_EVENT_STATUS_STYLE.skipped.text,
    );
    expect(DELIVERY_EVENT_STATUS_STYLE.duplicate.text).not.toBe(
      DELIVERY_EVENT_STATUS_STYLE.fallback.text,
    );
    expect(DELIVERY_EVENT_STATUS_STYLE.failed.text).not.toBe(
      DELIVERY_EVENT_STATUS_STYLE.sent.text,
    );
  });

  it("summarizes error cells and message arrays for compact tables", () => {
    expect(deliveryEventMessagesCount([{ id: 1 }, { id: 2 }])).toBe(2);
    expect(deliveryEventMessagesCount({ id: 1 })).toBe(0);
    expect(summarizeDeliveryError(null)).toBe("-");
    expect(summarizeDeliveryError("  Discord\n\nrate   limited  ")).toBe(
      "Discord rate limited",
    );
    expect(summarizeDeliveryError("x".repeat(120))).toHaveLength(96);
  });

  it("updates reserved rows to sent rows on background polling", () => {
    let state = createDeliveryEventsLoadState<{ id: string; status: string }>();

    state = startDeliveryEventsLoad(state, "dispatch-a", true);
    state = finishDeliveryEventsLoadSuccess(state, "dispatch-a", [
      { id: "event-1", status: "reserved" },
    ]);
    state = startDeliveryEventsLoad(state, "dispatch-a", false);
    state = finishDeliveryEventsLoadSuccess(state, "dispatch-a", [
      { id: "event-1", status: "sent" },
    ]);

    expect(state.events).toEqual([{ id: "event-1", status: "sent" }]);
    expect(state.loading).toBe(false);
    expect(state.error).toBeNull();
    expect(state.loadedDispatchId).toBe("dispatch-a");
  });

  it("keeps the last successful rows on visibility refreshes and polling errors", () => {
    let state = createDeliveryEventsLoadState<{ id: string; status: string }>();
    state = finishDeliveryEventsLoadSuccess(state, "dispatch-a", [
      { id: "event-1", status: "reserved" },
    ]);

    state = startDeliveryEventsLoad(state, "dispatch-a", false);
    expect(state.loading).toBe(false);
    expect(state.events).toEqual([{ id: "event-1", status: "reserved" }]);

    state = finishDeliveryEventsLoadError(state, "temporary 502", false);
    expect(state.events).toEqual([{ id: "event-1", status: "reserved" }]);
    expect(state.error).toBe("temporary 502");
    expect(state.loadedDispatchId).toBe("dispatch-a");
  });
});
