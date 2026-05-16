import { type CSSProperties } from "react";

import { getProviderSeries } from "../../app/providerTheme";
import type {
  ReceiptSnapshotAgentShare,
  ReceiptSnapshotModelLine,
  TokenAnalyticsDailyPoint,
  TokenAnalyticsHeatmapCell,
  TokenAnalyticsResponse,
} from "../../types";
import type { TFunction } from "./model";

export type Period = "7d" | "30d" | "90d";

export interface CachedAnalyticsEntry {
  data: TokenAnalyticsResponse;
  fetchedAt: number;
}

export interface ModelSegment {
  id: string;
  label: string;
  provider: string;
  tokens: number;
  percentage: number;
  color: string;
}

export interface AgentCacheRow {
  id: string;
  label: string;
  promptTokens: number;
  cacheReadTokens: number;
  savings: number;
  hitRate: number;
}

export interface DailyCacheHitPoint {
  date: string;
  promptTokens: number;
  cacheReadTokens: number;
  hitRate: number;
}

export type TrendSeriesKey =
  | "input_tokens"
  | "output_tokens"
  | "cache_read_tokens"
  | "cache_creation_tokens";
export type TrendPattern = "diagonal" | "dots" | "horizontal" | "cross";

export interface TrendLegendItem {
  key: TrendSeriesKey;
  color: string;
  label: string;
  pattern: TrendPattern;
}

export const ANALYTICS_CACHE_TTL = 5 * 60_000;
export const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];
export const TREND_PLOT_HEIGHT_PX = 144;
export const HEATMAP_COLORS = [
  "rgba(148,163,184,0.08)",
  "rgba(14,165,233,0.24)",
  "rgba(34,197,94,0.38)",
  "rgba(245,158,11,0.52)",
  "rgba(249,115,22,0.72)",
];
export const DAILY_TREND_CHART_HEIGHT_PX = 160;
export const DAILY_CACHE_HIT_CHART_HEIGHT_PX = 152;

export function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return String(value);
}

export function formatCost(value: number): string {
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
}

export function formatPercentage(value: number): string {
  return `${value.toFixed(1)}%`;
}

export function cacheHitRatePct(
  inputTokens: number,
  cacheReadTokens: number,
  cacheCreationTokens = 0,
): number {
  const promptTokens = inputTokens + cacheReadTokens + cacheCreationTokens;
  if (promptTokens <= 0) return 0;
  return (cacheReadTokens / promptTokens) * 100;
}

export function cacheSavingsRatePct(
  costWithoutCache: number,
  actualCost: number,
): number {
  if (costWithoutCache <= 0) return 0;
  return (Math.max(costWithoutCache - actualCost, 0) / costWithoutCache) * 100;
}

export function buildAgentCacheRows(
  agents: ReceiptSnapshotAgentShare[],
): AgentCacheRow[] {
  return [...agents]
    .map((agent) => {
      const inputTokens = agent.input_tokens ?? 0;
      const cacheReadTokens = agent.cache_read_tokens ?? 0;
      const cacheCreationTokens = agent.cache_creation_tokens ?? 0;
      const costWithoutCache = agent.cost_without_cache ?? agent.cost;
      return {
        id: agent.agent,
        label: agent.agent,
        promptTokens: inputTokens + cacheReadTokens + cacheCreationTokens,
        cacheReadTokens,
        savings: Math.max(costWithoutCache - agent.cost, 0),
        hitRate: cacheHitRatePct(
          inputTokens,
          cacheReadTokens,
          cacheCreationTokens,
        ),
      };
    })
    .filter((row) => row.promptTokens > 0)
    .sort(
      (left, right) =>
        right.promptTokens - left.promptTokens || right.savings - left.savings,
    )
    .slice(0, 8);
}

export function buildDailyCacheHitPoints(
  daily: TokenAnalyticsDailyPoint[],
): DailyCacheHitPoint[] {
  return daily.map((day) => ({
    date: day.date,
    promptTokens:
      day.input_tokens + day.cache_read_tokens + day.cache_creation_tokens,
    cacheReadTokens: day.cache_read_tokens,
    hitRate: cacheHitRatePct(
      day.input_tokens,
      day.cache_read_tokens,
      day.cache_creation_tokens,
    ),
  }));
}

export function normalizeModelProviderLabel(provider: string): string {
  const normalized = provider.trim().toLowerCase();
  switch (normalized) {
    case "claude":
      return "Claude";
    case "codex":
      return "Codex";
    case "gemini":
      return "Gemini";
    case "qwen":
      return "Qwen";
    default:
      return provider ? provider.charAt(0).toUpperCase() + provider.slice(1) : provider;
  }
}

export function modelColor(provider: string, index: number): string {
  const palette = getProviderSeries(provider);
  return palette[index % palette.length];
}

export function buildModelSegments(
  models: ReceiptSnapshotModelLine[],
): ModelSegment[] {
  const totalTokens = models.reduce(
    (sum, model) => sum + model.total_tokens,
    0,
  );
  if (totalTokens === 0) return [];

  const sorted = [...models].sort((a, b) => b.total_tokens - a.total_tokens);
  const visible = sorted.slice(0, 6);
  const remainder = sorted.slice(6);
  const segments = visible.map((model, index) => ({
    id: `${model.provider}-${model.model}`,
    label: model.display_name,
    provider: normalizeModelProviderLabel(model.provider),
    tokens: model.total_tokens,
    percentage: (model.total_tokens / totalTokens) * 100,
    color: modelColor(model.provider, index),
  }));

  if (remainder.length > 0) {
    const remainderTokens = remainder.reduce(
      (sum, model) => sum + model.total_tokens,
      0,
    );
    segments.push({
      id: "other",
      label: "Other",
      provider: "Mixed",
      tokens: remainderTokens,
      percentage: (remainderTokens / totalTokens) * 100,
      color: "#94a3b8",
    });
  }

  return segments;
}

export function buildDonutBackground(segments: ModelSegment[]): string {
  if (segments.length === 0) {
    return "conic-gradient(rgba(148,163,184,0.18) 0deg 360deg)";
  }
  let cursor = 0;
  const stops = segments.map((segment) => {
    const start = cursor;
    cursor += segment.percentage * 3.6;
    return `${segment.color} ${start}deg ${cursor}deg`;
  });
  if (cursor < 360) stops.push(`rgba(148,163,184,0.08) ${cursor}deg 360deg`);
  return `conic-gradient(${stops.join(", ")})`;
}

export function buildWeekLabels(
  cells: TokenAnalyticsHeatmapCell[],
): Array<{ week: number; label: string }> {
  const formatter = new Intl.DateTimeFormat("en-US", { month: "short" });
  const byWeek = new Map<number, TokenAnalyticsHeatmapCell[]>();
  for (const cell of cells) {
    const list = byWeek.get(cell.week_index) ?? [];
    list.push(cell);
    byWeek.set(cell.week_index, list);
  }

  let lastMonth = "";
  return Array.from(byWeek.entries())
    .sort((a, b) => a[0] - b[0])
    .map(([week, entries]) => {
      const first = [...entries].sort((a, b) => a.weekday - b.weekday)[0];
      const month = formatter.format(new Date(`${first.date}T00:00:00`));
      const label = month !== lastMonth ? month : "";
      lastMonth = month;
      return { week, label };
    });
}

export function periodLabel(period: Period, t: TFunction): string {
  switch (period) {
    case "7d":
      return t({ ko: "7일", en: "7d", ja: "7日", zh: "7天" });
    case "90d":
      return t({ ko: "90일", en: "90d", ja: "90日", zh: "90天" });
    default:
      return t({ ko: "30일", en: "30d", ja: "30日", zh: "30天" });
  }
}

export function patternFillStyle(color: string, pattern: TrendPattern): CSSProperties {
  switch (pattern) {
    case "dots":
      return {
        backgroundColor: color,
        backgroundImage:
          "radial-gradient(circle at 2px 2px, rgba(255,255,255,0.38) 0 1.25px, transparent 1.35px)",
        backgroundSize: "7px 7px",
      };
    case "horizontal":
      return {
        backgroundColor: color,
        backgroundImage:
          "repeating-linear-gradient(0deg, rgba(255,255,255,0.34) 0 2px, transparent 2px 6px)",
      };
    case "cross":
      return {
        backgroundColor: color,
        backgroundImage: [
          "repeating-linear-gradient(45deg, rgba(255,255,255,0.26) 0 2px, transparent 2px 7px)",
          "repeating-linear-gradient(-45deg, rgba(255,255,255,0.18) 0 2px, transparent 2px 7px)",
        ].join(", "),
      };
    case "diagonal":
    default:
      return {
        backgroundColor: color,
        backgroundImage:
          "repeating-linear-gradient(135deg, rgba(255,255,255,0.34) 0 2px, transparent 2px 7px)",
      };
  }
}

export function hasDailyTrendData(daily: TokenAnalyticsDailyPoint[]): boolean {
  return daily.some((day) => day.total_tokens > 0);
}

export function dailyTrendBarHeightPx(
  totalTokens: number,
  trendMax: number,
): number {
  if (totalTokens <= 0 || trendMax <= 0) return 0;
  return Math.max(
    8,
    Math.round((totalTokens / trendMax) * DAILY_TREND_CHART_HEIGHT_PX),
  );
}
