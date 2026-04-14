import { describe, expect, it } from "vitest";
import { dailyTrendBarHeightPx, hasDailyTrendData } from "./TokenAnalyticsSection";

describe("TokenAnalyticsSection helpers", () => {
  it("detects when the daily trend payload is effectively empty", () => {
    expect(
      hasDailyTrendData([
        {
          date: "2026-04-01",
          input_tokens: 0,
          output_tokens: 0,
          cache_read_tokens: 0,
          cache_creation_tokens: 0,
          total_tokens: 0,
          cost: 0,
        },
      ]),
    ).toBe(false);

    expect(
      hasDailyTrendData([
        {
          date: "2026-04-02",
          input_tokens: 120,
          output_tokens: 80,
          cache_read_tokens: 10,
          cache_creation_tokens: 5,
          total_tokens: 215,
          cost: 0.42,
        },
      ]),
    ).toBe(true);
  });

  it("converts daily totals into visible bar heights", () => {
    expect(dailyTrendBarHeightPx(0, 100)).toBe(0);
    expect(dailyTrendBarHeightPx(1, 1_000)).toBe(8);
    expect(dailyTrendBarHeightPx(500, 1_000)).toBe(80);
    expect(dailyTrendBarHeightPx(1_000, 1_000)).toBe(160);
  });
});
