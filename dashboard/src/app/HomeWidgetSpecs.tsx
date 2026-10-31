import type { ReactNode } from "react";
import { Gauge, Sparkles, Zap } from "lucide-react";

import type { Agent, DashboardStats, TokenAnalyticsResponse } from "../types";
import type { TFunction } from "../components/dashboard/model";
import { RoutinesTimelineWidget } from "../components/dashboard/RoutinesTimelineWidget";
import { MiniRateLimitBar } from "../components/dashboard/MiniRateLimitBar";
import { HomeAttention } from "./HomeAttention";
import { HomeMetricTile } from "./HomeOverviewWidgets";
import { HomeWorkingAgents } from "./HomeWorkingAgents";

export interface HomeWidgetSpec {
  className: string;
  render: () => ReactNode;
}

interface HomeWidgetContext {
  agents: Agent[];
  analytics: TokenAnalyticsResponse | null;
  analyticsAsOf: string | null;
  costTrend: number[];
  formatCompact: (value: number) => string;
  formatCurrency: (value: number) => string;
  isKo: boolean;
  localeTag: string;
  onOpenAgent: (agent: Agent) => void;
  stats: DashboardStats | null;
  t: TFunction;
  tokenTrend: number[];
  tr: (ko: string, en: string) => string;
}

export function buildHomeWidgetSpecs(ctx: HomeWidgetContext): Record<string, HomeWidgetSpec> {
  const { agents, analytics, analyticsAsOf, costTrend, formatCompact, formatCurrency, isKo, localeTag, onOpenAgent, stats, t, tokenTrend, tr } = ctx;
  const latestDay = analytics?.daily.at(-1) ?? null;

  return {
    working: {
      className: "col-span-2 min-w-0 lg:col-span-6",
      render: () => <HomeWorkingAgents agents={agents} isKo={isKo} onOpenAgent={onOpenAgent} />,
    },
    attention: {
      className: "col-span-2 min-w-0 lg:col-span-6",
      render: () => <HomeAttention stats={stats} isKo={isKo} />,
    },
    m_tokens: {
      className: "min-w-0 lg:col-span-3",
      render: () => (
        <HomeMetricTile
          icon={<Zap size={14} />}
          title={tr("오늘 토큰", "Today's tokens")}
          value={analytics ? formatCompact(latestDay?.total_tokens ?? 0) : null}
          sub={
            analytics
              ? tr(
                  `7일 평균 ${formatCompact(Math.round(analytics.summary.average_daily_tokens ?? 0))}`,
                  `7-day avg ${formatCompact(Math.round(analytics.summary.average_daily_tokens ?? 0))}`,
                )
              : tr("집계 중이에요 (최대 1분)", "Calculating (up to 1 min)")
          }
          delta={analyticsAsOf ?? undefined}
          accent="var(--th-accent-primary)"
          trend={tokenTrend}
        />
      ),
    },
    m_cost: {
      className: "min-w-0 lg:col-span-3",
      render: () => (
        <HomeMetricTile
          icon={<Sparkles size={14} />}
          title={tr("오늘 API 비용", "Today's API cost")}
          value={analytics ? formatCurrency(latestDay?.cost ?? 0) : null}
          sub={
            analytics?.summary.total_cost != null
              ? tr(`최근 7일 ${formatCurrency(analytics.summary.total_cost)}`, `Last 7 days ${formatCurrency(analytics.summary.total_cost)}`)
              : tr("집계 중이에요 (최대 1분)", "Calculating (up to 1 min)")
          }
          delta={analyticsAsOf ?? undefined}
          accent="var(--th-accent-success)"
          trend={costTrend}
        />
      ),
    },
    m_rate_limit: {
      className: "col-span-2 min-w-0 lg:col-span-6",
      render: () => (
        <section className="h-full min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface px-4 py-4 sm:px-5" aria-label={tr("계정 사용량", "Account usage")}>
          <div className="flex items-center gap-2 text-xs text-th-text-muted">
            <Gauge size={14} />
            {tr("계정 사용량", "Account usage")}
          </div>
          <MiniRateLimitBar isKo={isKo} layout="homeSummary" />
        </section>
      ),
    },
    routines: {
      className: "col-span-2 min-w-0 lg:col-span-12",
      render: () => (
        <RoutinesTimelineWidget compact t={t} localeTag={localeTag} language={isKo ? "ko" : "en"} />
      ),
    },
  };
}
