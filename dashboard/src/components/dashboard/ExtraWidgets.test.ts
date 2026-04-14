import { describe, expect, it } from "vitest";
import {
  buildCronTimelineMetrics,
  describeCronSchedule,
  formatCompactDuration,
} from "./ExtraWidgets";

describe("ExtraWidgets cron helpers", () => {
  it("formats compact intervals for repeating schedules", () => {
    expect(formatCompactDuration(15 * 60_000)).toBe("15m");
    expect(formatCompactDuration(2 * 60 * 60_000)).toBe("2h");
    expect(
      describeCronSchedule({
        kind: "every",
        everyMs: 15 * 60_000,
      }),
    ).toBe("Every 15m");
  });

  it("maps last, now, and next run points into a stable timeline window", () => {
    const metrics = buildCronTimelineMetrics(
      {
        id: "cron-sync",
        name: "Sync",
        enabled: true,
        schedule: {
          kind: "every",
          everyMs: 900_000,
        },
        state: {
          lastRunAtMs: 1_000_000,
          nextRunAtMs: 1_900_000,
          lastStatus: "ok",
        },
      },
      1_450_000,
    );

    expect(metrics.lastPercent).toBeCloseTo(0);
    expect(metrics.nowPercent).toBeCloseTo(50);
    expect(metrics.nextPercent).toBeCloseTo(100);
    expect(metrics.overdue).toBe(false);
  });
});
