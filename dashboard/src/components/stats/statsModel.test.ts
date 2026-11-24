import { describe, expect, it } from "vitest";
import type { SkillRankingResponse } from "../../api";
import type { TokenAnalyticsResponse } from "../../types";
import {
  buildAgentItems,
  buildAgentSkillRows,
  buildModelItems,
  buildSavingsDelta,
  buildSkillRows,
  buildWindowDelta,
  computeCacheHitRate,
  dailySeries,
  formatCurrency,
  formatTokens,
  periodDayCount,
  resolveLocaleTag,
  skillsForAgent,
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

  it("ranks agents by tokens and keeps their prompt volume, cache hit rate and savings", () => {
    const items = buildAgentItems(analyticsFixture());
    expect(items.map((row) => row.id)).toEqual(["pm", "qa"]);
    expect(items[0]).toMatchObject({
      id: "pm",
      promptTokens: 600,
      cacheHitRate: 50,
      savedCost: 1,
      share: 70,
      composition: { input: 200, output: 100, cacheRead: 300, cacheWrite: 100 },
    });
    // Without a no-cache baseline the saving stays unknown instead of zero.
    expect(items[1]).toMatchObject({ id: "qa", savedCost: null, share: 30, cacheHitRate: 0 });
  });

  it("colors agents and token kinds with distinct data series, never status or provider colors", () => {
    const data = analyticsFixture();
    data.receipt.agents = ["a", "b", "c", "d", "e"].map((agent, index) => ({ agent, tokens: 10 - index, cost: 0, percentage: 20 }));
    const agents = buildAgentItems(data).map((item) => item.color);
    const kinds = dailySeries((messages) => messages.en).map((item) => item.color);
    const banned = /--(?:aui-)?(?:ok|warn|err|danger|info|provider-\w+|claude|codex|gemini|qwen|opencode|copilot|antigravity|api)\b/;
    expect([...agents, ...kinds].filter((color) => banned.test(color))).toEqual([]);
    expect([new Set(agents).size, new Set(kinds).size]).toEqual([5, 4]);
  });

  it("leaves agent prompt detail empty when the receipt has no token split", () => {
    const data = analyticsFixture();
    data.receipt.agents = [{ agent: "solo", tokens: 50, cost: 1, percentage: 100 }];
    expect(buildAgentItems(data)[0]).toMatchObject({ composition: null, promptTokens: null, cacheHitRate: null, share: 100 });
  });

  it("orders models by tokens with shares of the model total", () => {
    const data = analyticsFixture();
    data.receipt.models = [
      { model: "small", display_name: "Small", input_tokens: 10, output_tokens: 10, cache_read_tokens: 0, cache_creation_tokens: 0, total_tokens: 20, cost: 0.1, cost_without_cache: 0.1, provider: "openai" },
      { model: "big", display_name: "Big", input_tokens: 20, output_tokens: 20, cache_read_tokens: 30, cache_creation_tokens: 10, total_tokens: 80, cost: 1, cost_without_cache: 1.5, provider: "anthropic" },
    ];
    const items = buildModelItems(data);
    expect(items.map((item) => item.label)).toEqual(["Big", "Small"]);
    expect(items[0]).toMatchObject({ share: 80, savedCost: 0.5, cacheHitRate: 50, promptTokens: 60 });
    expect(items[1].share).toBe(20);
    expect(buildModelItems(null)).toEqual([]);
  });

  it("matches agent skill pairs by role id or name", () => {
    const ranking: SkillRankingResponse = {
      window: "7d",
      overall: [],
      byAgent: [
        { agent_role_id: "pm", agent_name: "PM Bot", skill_name: "plan", skill_desc_ko: "계획", calls: 4, last_used_at: 0 },
        { agent_role_id: "qa", agent_name: "QA", skill_name: "test", skill_desc_ko: "테스트", calls: 2, last_used_at: 0 },
      ],
    };
    const rows = buildAgentSkillRows(ranking, [], "ko");
    expect(skillsForAgent(rows, "pm").map((row) => row.skillName)).toEqual(["plan"]);
    expect(skillsForAgent(rows, "QA").map((row) => row.skillName)).toEqual(["test"]);
    expect(skillsForAgent(rows, "nobody")).toEqual([]);
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
