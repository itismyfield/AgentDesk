import { describe, expect, it } from "vitest";

import {
  DELIVERY_EVENT_STATUS_STYLE,
  summarizeDeliveryError,
} from "./DispatchDeliveryEventsPanel";

describe("DispatchDeliveryEventsPanel helpers", () => {
  it("defines visual styles for every delivery status", () => {
    expect(Object.keys(DELIVERY_EVENT_STATUS_STYLE).sort()).toEqual([
      "duplicate",
      "failed",
      "fallback",
      "reserved",
      "sent",
      "skipped",
    ]);
    expect(DELIVERY_EVENT_STATUS_STYLE.sent.color).not.toBe(
      DELIVERY_EVENT_STATUS_STYLE.failed.color,
    );
    expect(DELIVERY_EVENT_STATUS_STYLE.fallback.color).not.toBe(
      DELIVERY_EVENT_STATUS_STYLE.duplicate.color,
    );
  });

  it("keeps empty and long error cells table-safe", () => {
    expect(summarizeDeliveryError(null)).toBe("-");
    expect(summarizeDeliveryError("  Discord\n\nrate   limited  ")).toBe(
      "Discord rate limited",
    );
    expect(summarizeDeliveryError("x".repeat(120))).toHaveLength(96);
  });
});
