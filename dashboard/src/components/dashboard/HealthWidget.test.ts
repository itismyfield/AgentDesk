import { describe, expect, it } from "vitest";

import {
  HEALTH_STALE_AFTER_MS,
  describeMetricTooltip,
  derivePollState,
  describeDegradedReason,
  isHealthResponseEmpty,
  metricLevel,
} from "./HealthWidget";

describe("HealthWidget helpers", () => {
  it("promotes metric levels using warning and danger thresholds", () => {
    expect(metricLevel(0, { warning: 1, danger: 3 })).toBe("normal");
    expect(metricLevel(1, { warning: 1, danger: 3 })).toBe("warning");
    expect(metricLevel(3, { warning: 1, danger: 3 })).toBe("danger");
  });

  it("detects empty health responses when the key metrics are absent", () => {
    expect(isHealthResponseEmpty(null)).toBe(true);
    expect(isHealthResponseEmpty({ status: "healthy" })).toBe(true);
    expect(isHealthResponseEmpty({ status: "healthy", deferred_hooks: 0, queue_depth: 0 })).toBe(false);
  });

  it("marks cached data as stale after the stale threshold", () => {
    const now = 1_000_000;
    expect(
      derivePollState({
        data: { status: "healthy", deferred_hooks: 0, queue_depth: 0 },
        error: null,
        isRefreshing: false,
        lastSuccessAt: now - HEALTH_STALE_AFTER_MS - 1,
        now,
      }),
    ).toBe("stale");
  });

  it("returns empty when the payload is present but all required metrics are missing", () => {
    expect(
      derivePollState({
        data: { status: "healthy", degraded_reasons: [] },
        error: null,
        isRefreshing: false,
        lastSuccessAt: 100,
        now: 200,
      }),
    ).toBe("empty");
  });

  it("humanizes provider and outbox degraded reasons", () => {
    expect(describeDegradedReason("provider:codex:pending_queue_depth:2")).toBe("CODEX queue depth 2");
    expect(describeDegradedReason("dispatch_outbox_oldest_pending_age:61")).toBe("Dispatch outbox age 1m 1s");
  });

  it("provides per-metric tooltip copy", () => {
    const t = (messages: { ko: string; en: string; ja: string; zh: string }) => messages.ko;
    expect(describeMetricTooltip("deferred-hooks", t)).toContain("hook backlog");
    expect(describeMetricTooltip("outbox-age", t)).toContain("dispatch outbox");
  });
});
