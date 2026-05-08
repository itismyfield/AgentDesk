import { describe, expect, it } from "vitest";

import {
  DELIVERY_EVENT_STATUS_STYLE,
  deliveryEventMessagesCount,
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
});
