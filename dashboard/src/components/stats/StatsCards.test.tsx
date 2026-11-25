// @vitest-environment happy-dom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { TokenAnalyticsDailyPoint, TokenAnalyticsResponse } from "../../types";
import { DailyTokenCompositionCard } from "./StatsTokenCards";
import { StatsSummaryGrid } from "./StatsSummaryCards";
import { dailySeries, msg } from "./statsModel";

let container: HTMLDivElement;
let root: Root;
const t = (messages: ReturnType<typeof msg>) => messages.ko;
const formatter = new Intl.NumberFormat("ko-KR");

const DAILY: TokenAnalyticsDailyPoint[] = [
  {
    date: "2026-11-23",
    input_tokens: 400,
    output_tokens: 200,
    cache_read_tokens: 300,
    cache_creation_tokens: 100,
    total_tokens: 1000,
    cost: 1.25,
  },
  {
    date: "2026-11-24",
    input_tokens: 800,
    output_tokens: 500,
    cache_read_tokens: 900,
    cache_creation_tokens: 300,
    total_tokens: 2500,
    cost: 2.25,
  },
];

const SUMMARY: TokenAnalyticsResponse["summary"] = {
  total_tokens: 3500,
  total_cost: 3.5,
  cache_discount: 0.8,
  total_messages: 12,
  total_sessions: 3,
  active_days: 2,
  average_daily_tokens: 1750,
  peak_day: DAILY[1],
};

beforeEach(() => {
  Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });
  container = document.createElement("div");
  document.body.appendChild(container);
  root = createRoot(container);
});

afterEach(async () => {
  await act(async () => root.unmount());
  container.remove();
});

describe("DailyTokenCompositionCard", () => {
  it("renders BreakdownChart by default and switches to MeteringChart via SegmentedControl", async () => {
    await act(async () => {
      root.render(
        <DailyTokenCompositionCard
          t={t}
          localeTag="ko-KR"
          loading={false}
          failed={false}
          daily={DAILY}
          series={dailySeries(t)}
        />,
      );
    });
    // Flush lazy DailyTokenCompositionChart import under Suspense.
    await act(async () => {
      await import("./DailyTokenCompositionChart");
    });

    expect(container.querySelector('[data-testid="stats-daily-token-chart"]')).not.toBeNull();
    expect(container.querySelector('[data-testid="stats-daily-token-scroll"] .aui-breakdown-chart')).not.toBeNull();
    expect(container.querySelector(".aui-metering-chart")).toBeNull();

    const meteringTab = Array.from(container.querySelectorAll<HTMLButtonElement>('[role="tab"]')).find((tab) =>
      tab.textContent?.includes("미터링"),
    )!;
    await act(async () => meteringTab.click());

    const metering = container.querySelector<HTMLElement>('[data-testid="stats-daily-token-scroll"] .aui-metering-chart')!;
    expect(metering).not.toBeNull();
    expect(metering.textContent).toContain("3.5K");
    expect(metering.textContent).toContain("$3.50");
    expect(metering.querySelectorAll(".aui-metering-chart-bar")).toHaveLength(2);
  });

  it("preserves empty and all-zero daily token states", async () => {
    await act(async () => {
      root.render(
        <DailyTokenCompositionCard
          t={t}
          localeTag="ko-KR"
          loading={false}
          failed={false}
          daily={[]}
          series={dailySeries(t)}
        />,
      );
    });
    expect(container.querySelector('[data-testid="stats-daily-empty"]')).not.toBeNull();

    await act(async () => {
      root.render(
        <DailyTokenCompositionCard
          t={t}
          localeTag="ko-KR"
          loading={false}
          failed={false}
          daily={[{ ...DAILY[0], total_tokens: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, cache_creation_tokens: 0 }]}
          series={dailySeries(t)}
        />,
      );
    });
    expect(container.querySelector('[data-testid="stats-daily-zero"]')?.textContent).toContain("0");
  });
});

describe("StatsSummaryGrid", () => {
  it("renders StatCard summary facts with TrendChart sparklines when daily.length > 1", async () => {
    await act(async () => {
      root.render(
        <StatsSummaryGrid
          t={t}
          numberFormatter={formatter}
          localeTag="ko-KR"
          rangeDays={7}
          summary={SUMMARY}
          daily={DAILY}
          loading={false}
          failed={false}
          tokenMomentumDelta={null}
          cacheSavingsDelta={null}
        />,
      );
    });

    const activeDays = container.querySelector<HTMLElement>('[data-testid="stats-summary-cache-saved"]')!;
    const peakDay = container.querySelector<HTMLElement>('[data-testid="stats-summary-cache-hit"]')!;
    expect(activeDays.querySelector(".aui-stat-card")).not.toBeNull();
    expect(activeDays.textContent).toContain("활성 일수");
    expect(activeDays.textContent).toContain("2 / 7");
    expect(activeDays.textContent).toContain("일 평균 1.8K");
    expect(activeDays.querySelector(".aui-trend-chart[data-spark]")).not.toBeNull();

    expect(peakDay.querySelector(".aui-stat-card")).not.toBeNull();
    expect(peakDay.textContent).toContain("피크 데이");
    expect(peakDay.textContent).toContain("11-24");
    expect(peakDay.textContent).toContain("2.5K");
    expect(peakDay.querySelector(".aui-trend-chart[data-spark]")).not.toBeNull();

    // Sparklines are omitted when fewer than 2 daily points exist.
    await act(async () => {
      root.render(
        <StatsSummaryGrid
          t={t}
          numberFormatter={formatter}
          localeTag="ko-KR"
          rangeDays={7}
          summary={SUMMARY}
          daily={DAILY.slice(0, 1)}
          loading={false}
          failed={false}
          tokenMomentumDelta={null}
          cacheSavingsDelta={null}
        />,
      );
    });
    expect(container.querySelector('[data-testid="stats-summary-cache-saved"] .aui-trend-chart')).toBeNull();
  });
});
