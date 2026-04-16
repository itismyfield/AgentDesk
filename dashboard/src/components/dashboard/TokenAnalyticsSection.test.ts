import { describe, expect, it } from "vitest";
import {
  buildModelSegments,
  buildAgentCacheRows,
  buildDailyCacheHitPoints,
  cacheHitRatePct,
  cacheSavingsRatePct,
  dailyTrendBarHeightPx,
  hasDailyTrendData,
} from "./TokenAnalyticsSection";

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

  it("calculates cache hit and savings rates safely", () => {
    expect(cacheHitRatePct(0, 0, 0)).toBe(0);
    expect(cacheHitRatePct(200, 100, 50)).toBeCloseTo(28.5714, 4);
    expect(cacheSavingsRatePct(0, 0)).toBe(0);
    expect(cacheSavingsRatePct(40, 30)).toBe(25);
  });

  it("derives agent cache rows from receipt agent shares", () => {
    const rows = buildAgentCacheRows([
      {
        agent: "alpha",
        tokens: 2_000,
        cost: 12,
        cost_without_cache: 20,
        input_tokens: 300,
        cache_read_tokens: 900,
        cache_creation_tokens: 0,
        percentage: 60,
      },
      {
        agent: "beta",
        tokens: 1_000,
        cost: 8,
        cost_without_cache: 10,
        input_tokens: 400,
        cache_read_tokens: 100,
        cache_creation_tokens: 0,
        percentage: 40,
      },
    ]);

    expect(rows).toHaveLength(2);
    expect(rows[0]?.label).toBe("alpha");
    expect(rows[0]?.savings).toBe(8);
    expect(rows[0]?.hitRate).toBeCloseTo(75, 4);
  });

  it("converts daily analytics payload into cache-hit points", () => {
    const rows = buildDailyCacheHitPoints([
      {
        date: "2026-04-03",
        input_tokens: 100,
        output_tokens: 40,
        cache_read_tokens: 300,
        cache_creation_tokens: 100,
        total_tokens: 540,
        cost: 1.2,
      },
    ]);

    expect(rows[0]?.promptTokens).toBe(500);
    expect(rows[0]?.cacheReadTokens).toBe(300);
    expect(rows[0]?.hitRate).toBeCloseTo(60, 4);
  });

  it("normalizes Gemini and Qwen providers in the model distribution", () => {
    const segments = buildModelSegments([
      {
        provider: "gemini",
        model: "gemini-2.5-pro",
        display_name: "Gemini 2.5 Pro",
        total_tokens: 700,
        cost: 0,
      },
      {
        provider: "qwen",
        model: "coder-model",
        display_name: "Qwen",
        total_tokens: 300,
        cost: 0,
      },
    ]);

    expect(segments).toHaveLength(2);
    expect(segments[0]).toMatchObject({
      provider: "Gemini",
      label: "Gemini 2.5 Pro",
      percentage: 70,
    });
    expect(segments[1]).toMatchObject({
      provider: "Qwen",
      label: "Qwen",
      percentage: 30,
    });
  });
});
