import { describe, expect, it } from "vitest";
import type { SkillRankingResponse } from "../../api";
import type { TokenAnalyticsResponse } from "../../types";
import {
  buildAgentCacheRows,
  buildSavingsDelta,
  buildSkillRows,
  buildWindowDelta,
  computeCacheHitRate,
  formatCurrency,
  formatTokens,
  periodDayCount,
  resolveLocaleTag,
} from "./statsModel";

function analyticsFixture(): TokenAnalyticsResponse {
  return {
    period: "7d",
    period_label: "7d",
    days: 7,
    generated_at: "2026-05-16T00:00:00Z",
    summary: {
      total_tokens: 1000,
      total_cost: 8,
      cache_discount: 2,
      total_messages: 10,
      total_sessions: 2,
      active_days: 4,
      average_daily_tokens: 250,
      peak_day: null,
    },
    receipt: {
      period_label: "7d",
      period_start: "2026-05-10",
      period_end: "2026-05-16",
      models: [],
      subtotal: 10,
      cache_discount: 2,
      total: 8,
      stats: { total_messages: 10, total_sessions: 2 },
      providers: [],
      agents: [
        {
          agent: "pm",
          tokens: 700,
          cost: 4,
          cost_without_cache: 5,
          input_tokens: 200,
          cache_read_tokens: 300,
          cache_creation_tokens: 100,
          percentage: 70,
        },
        {
          agent: "qa",
          tokens: 300,
          cost: 2,
          input_tokens: 100,
          cache_read_tokens: 0,
          cache_creation_tokens: 0,
          percentage: 30,
        },
      ],
    },
    daily: [],
    heatmap: [],
  };
}

describe("statsModel", () => {
  it("formats compact dashboard values", () => {
    expect(formatTokens(1200)).toBe("1.2K");
    expect(formatTokens(2_500_000)).toBe("2.5M");
    expect(formatCurrency(0.0042)).toBe("$0.0042");
    expect(formatCurrency(12.3)).toBe("$12.30");
  });

  it("resolves periods and locale tags", () => {
    expect(periodDayCount("7d")).toBe(7);
    expect(periodDayCount("30d")).toBe(30);
    expect(periodDayCount("90d")).toBe(90);
    expect(resolveLocaleTag("ko")).toBe("ko-KR");
    expect(resolveLocaleTag("zh")).toBe("zh-CN");
  });

  it("computes cache hit rates and savings deltas", () => {
    expect(computeCacheHitRate(200, 300, 100)).toBe(50);
    expect(buildSavingsDelta(analyticsFixture().summary)).toEqual({
      value: "-20%",
      tone: "up",
    });
  });

  it("builds window momentum from split daily averages", () => {
    expect(
      buildWindowDelta([
        { date: "2026-05-10", total_tokens: 100, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0, cost: 0 },
        { date: "2026-05-11", total_tokens: 100, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0, cost: 0 },
        { date: "2026-05-12", total_tokens: 200, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0, cost: 0 },
        { date: "2026-05-13", total_tokens: 200, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0, cost: 0 },
      ]),
    ).toEqual({ value: "+100%", tone: "up" });
  });

  it("sorts agent cache rows by prompt volume", () => {
    expect(buildAgentCacheRows(analyticsFixture()).map((row) => row.id)).toEqual(["pm", "qa"]);
    expect(buildAgentCacheRows(analyticsFixture())[0]).toMatchObject({
      id: "pm",
      promptTokens: 600,
      hitRate: 50,
      savedCost: 1,
    });
  });

  it("prefers catalog descriptions for skill rows", () => {
    const ranking: SkillRankingResponse = {
      window: "7d",
      overall: [
        { skill_name: "deploy", skill_desc_ko: "배포 기본", calls: 12, last_used_at: 0 },
      ],
      byAgent: [],
    };
    expect(
      buildSkillRows(
        ranking,
        [{
          name: "deploy",
          description: "Deploy",
          description_ko: "배포",
          total_calls: 12,
          last_used_at: null,
        }],
        "ko",
      )[0],
    ).toMatchObject({ id: "deploy", description: "배포", windowCalls: 12 });
  });
});
