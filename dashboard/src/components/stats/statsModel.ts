import type { SkillRankingResponse } from "../../api";
import { getProviderMeta, getProviderSeries } from "../../app/providerTheme";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  SkillCatalogEntry,
  TokenAnalyticsDailyPoint,
  TokenAnalyticsResponse,
} from "../../types";
import type { TFunction } from "../dashboard/model";

export type Period = "7d" | "30d" | "90d";
export type DailySeriesKey =
  | "cache_read_tokens"
  | "cache_creation_tokens"
  | "input_tokens"
  | "output_tokens";

export interface SkillUsageRow {
  id: string;
  name: string;
  description: string;
  windowCalls: number;
}

export interface AgentSkillRow {
  id: string;
  agentId: string;
  agentName: string;
  skillName: string;
  description: string;
  calls: number;
}

export interface DailySeriesDescriptor {
  key: DailySeriesKey;
  label: string;
  color: string;
}

export type MetricDeltaTone = "up" | "down" | "flat";

export interface MetricDelta {
  value: string;
  tone: MetricDeltaTone;
}

/** Data series for agents and token kinds (light, dark). The kit has no series tokens; these keep clear of status and
 * provider colors, and neighbours differ in hue and lightness in both themes (at least 3:1 on cards). */
const DATA_SERIES = [
  "light-dark(oklch(0.5 0.17 294), oklch(0.7 0.15 294))",
  "light-dark(oklch(0.62 0.07 172), oklch(0.85 0.09 172))",
  "light-dark(oklch(0.5 0.18 352), oklch(0.66 0.16 352))",
  "light-dark(oklch(0.6 0.13 118), oklch(0.87 0.14 118))",
  "light-dark(oklch(0.42 0.08 30), oklch(0.6 0.08 30))",
];

export function msg(ko: string, en = ko, ja = ko, zh = ko) {
  return { ko, en, ja, zh };
}

export function resolveLocaleTag(language: CompanySettings["language"]): string {
  switch (language) {
    case "ja":
      return "ja-JP";
    case "zh":
      return "zh-CN";
    case "en":
      return "en-US";
    default:
      return "ko-KR";
  }
}

export function periodDayCount(period: Period): number {
  switch (period) {
    case "7d":
      return 7;
    case "90d":
      return 90;
    default:
      return 30;
  }
}

export function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return Math.round(value).toString();
}

/** Chart axis money: a plain "$0" at the baseline, `formatCurrency` above it. */
export function formatAxisCurrency(value: number): string {
  return value === 0 ? "$0" : formatCurrency(value);
}

export function formatCurrency(value: number): string {
  if (value >= 100) return `$${Math.round(value).toLocaleString("en-US")}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
}

/** Catalog descriptions are markdown; a stats row needs one plain line. */
export function plainSkillDescription(text: string): string {
  const firstLine = text.split("\n").map((line) => line.trim()).find(Boolean) ?? "";
  return firstLine
    .replace(/^#+\s*/, "")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/(\*\*|__|\*|_|`)/g, "")
    .trim();
}

export function formatPercent(value: number, digits = 1): string {
  return `${value.toFixed(digits)}%`;
}

export function formatCompactDate(value: string): string {
  const date = new Date(`${value}T00:00:00`);
  if (Number.isNaN(date.getTime())) return value;
  return `${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}

export function formatDateLabel(value: string, localeTag: string): string {
  const date = new Date(`${value}T00:00:00`);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(localeTag, {
    month: "2-digit",
    day: "2-digit",
  }).format(date);
}

export function computeCacheHitRate(
  inputTokens: number,
  cacheReadTokens: number,
  cacheCreationTokens = 0,
): number {
  const promptTokens = inputTokens + cacheReadTokens + cacheCreationTokens;
  if (promptTokens <= 0) return 0;
  return (cacheReadTokens / promptTokens) * 100;
}

export function buildWindowDelta(
  daily: TokenAnalyticsDailyPoint[],
): MetricDelta | null {
  if (daily.length < 4) return null;

  const midpoint = Math.floor(daily.length / 2);
  const previousWindow = daily.slice(0, midpoint);
  const recentWindow = daily.slice(midpoint);
  if (previousWindow.length === 0 || recentWindow.length === 0) return null;

  const previousAverage =
    previousWindow.reduce((sum, day) => sum + day.total_tokens, 0) /
    previousWindow.length;
  const recentAverage =
    recentWindow.reduce((sum, day) => sum + day.total_tokens, 0) /
    recentWindow.length;

  if (previousAverage <= 0) return null;

  const change = ((recentAverage - previousAverage) / previousAverage) * 100;
  if (!Number.isFinite(change)) return null;

  const rounded = Math.round(change * 10) / 10;
  const tone: MetricDeltaTone =
    rounded > 1 ? "up" : rounded < -1 ? "down" : "flat";
  const sign = rounded > 0 ? "+" : "";

  return {
    value: `${sign}${rounded.toFixed(Math.abs(rounded) >= 10 ? 0 : 1)}%`,
    tone,
  };
}

export function buildSavingsDelta(
  summary: TokenAnalyticsResponse["summary"] | null | undefined,
): MetricDelta | null {
  if (!summary) return null;
  const uncachedBaseline = summary.total_cost + summary.cache_discount;
  if (uncachedBaseline <= 0 || summary.cache_discount <= 0) return null;

  const savingsRate = (summary.cache_discount / uncachedBaseline) * 100;
  return {
    value: `-${savingsRate.toFixed(savingsRate >= 10 ? 0 : 1)}%`,
    tone: "up",
  };
}

export function buildSkillRows(
  ranking: SkillRankingResponse | null,
  catalog: SkillCatalogEntry[],
  language: CompanySettings["language"],
): SkillUsageRow[] {
  if (!ranking) return [];
  const catalogMap = new Map(catalog.map((entry) => [entry.name, entry]));

  return ranking.overall.slice(0, 8).map((row) => {
    const catalogEntry = catalogMap.get(row.skill_name);
    const description =
      language === "ko"
        ? catalogEntry?.description_ko ||
          row.skill_desc_ko ||
          catalogEntry?.description ||
          row.skill_name
        : catalogEntry?.description ||
          catalogEntry?.description_ko ||
          row.skill_desc_ko ||
          row.skill_name;

    return {
      id: row.skill_name,
      name: row.skill_name,
      description: plainSkillDescription(description),
      windowCalls: row.calls,
    };
  });
}

export function buildAgentSkillRows(
  ranking: SkillRankingResponse | null,
  catalog: SkillCatalogEntry[],
  language: CompanySettings["language"],
): AgentSkillRow[] {
  if (!ranking) return [];
  const catalogMap = new Map(catalog.map((entry) => [entry.name, entry]));

  return ranking.byAgent.slice(0, 8).map((row, index) => {
    const catalogEntry = catalogMap.get(row.skill_name);
    const description =
      language === "ko"
        ? catalogEntry?.description_ko ||
          row.skill_desc_ko ||
          catalogEntry?.description ||
          row.skill_name
        : catalogEntry?.description ||
          catalogEntry?.description_ko ||
          row.skill_desc_ko ||
          row.skill_name;

    return {
      id: `${row.agent_role_id}-${row.skill_name}-${index}`,
      agentId: row.agent_role_id,
      agentName: row.agent_name,
      skillName: row.skill_name,
      description: plainSkillDescription(description),
      calls: row.calls,
    };
  });
}

export type BreakdownCategory = "models" | "agents";

export interface TokenComposition {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** One row of the "where tokens went" list: a model or an agent for the selected period. */
export interface BreakdownItem {
  id: string;
  category: BreakdownCategory;
  label: string;
  sublabel: string;
  tokens: number;
  /** Percent of the category total, 0–100. */
  share: number;
  cost: number;
  costWithoutCache: number | null;
  savedCost: number | null;
  color: string;
  composition: TokenComposition | null;
  promptTokens: number | null;
  cacheHitRate: number | null;
}

function withShares(items: Omit<BreakdownItem, "share">[]): BreakdownItem[] {
  const total = items.reduce((sum, item) => sum + Math.max(0, item.tokens), 0);
  return items.map((item) => ({
    ...item,
    share: total > 0 ? (Math.max(0, item.tokens) / total) * 100 : 0,
  }));
}

/** Models by tokens, colored like the model share bar (provider series by rank). */
export function buildModelItems(
  data: TokenAnalyticsResponse | null,
): BreakdownItem[] {
  const sorted = [...(data?.receipt.models ?? [])].sort(
    (left, right) => right.total_tokens - left.total_tokens,
  );
  return withShares(
    sorted.map((model, index) => {
      const series = getProviderSeries(model.provider);
      const promptTokens =
        model.input_tokens + model.cache_read_tokens + model.cache_creation_tokens;
      return {
        id: `${model.provider}-${model.model}-${index}`,
        category: "models" as const,
        label: model.display_name || model.model,
        sublabel: getProviderMeta(model.provider).label,
        tokens: model.total_tokens,
        cost: model.cost,
        costWithoutCache: model.cost_without_cache,
        savedCost: Math.max(0, model.cost_without_cache - model.cost),
        color: series[index % series.length],
        composition: {
          input: model.input_tokens,
          output: model.output_tokens,
          cacheRead: model.cache_read_tokens,
          cacheWrite: model.cache_creation_tokens,
        },
        promptTokens,
        cacheHitRate: computeCacheHitRate(
          model.input_tokens,
          model.cache_read_tokens,
          model.cache_creation_tokens,
        ),
      };
    }),
  );
}

/** Agents by tokens with their prompt volume, cache hit rate and cache savings. */
export function buildAgentItems(
  data: TokenAnalyticsResponse | null,
): BreakdownItem[] {
  const sorted = [...(data?.receipt.agents ?? [])].sort(
    (left, right) => right.tokens - left.tokens,
  );
  return withShares(
    sorted.map((agent, index) => {
      const hasPromptDetail =
        agent.input_tokens != null ||
        agent.cache_read_tokens != null ||
        agent.cache_creation_tokens != null;
      const input = agent.input_tokens ?? 0;
      const cacheRead = agent.cache_read_tokens ?? 0;
      const cacheWrite = agent.cache_creation_tokens ?? 0;
      const promptTokens = input + cacheRead + cacheWrite;
      return {
        id: agent.agent,
        category: "agents" as const,
        label: agent.agent,
        sublabel: "",
        tokens: agent.tokens,
        cost: agent.cost,
        costWithoutCache: agent.cost_without_cache ?? null,
        savedCost:
          agent.cost_without_cache != null
            ? Math.max(0, agent.cost_without_cache - agent.cost)
            : null,
        color: DATA_SERIES[index % DATA_SERIES.length],
        composition: hasPromptDetail
          ? {
              input,
              output: Math.max(0, agent.tokens - promptTokens),
              cacheRead,
              cacheWrite,
            }
          : null,
        promptTokens: hasPromptDetail ? promptTokens : null,
        cacheHitRate: hasPromptDetail
          ? computeCacheHitRate(input, cacheRead, cacheWrite)
          : null,
      };
    }),
  );
}

/** Skill pairs recorded for one agent (matched by role id or display name). */
export function skillsForAgent(rows: AgentSkillRow[], agent: string): AgentSkillRow[] {
  return rows.filter((row) => row.agentId === agent || row.agentName === agent);
}

// Daily chart geometry: the plot sits between the y-axis and the right margin.
export const DAILY_CHART_Y_AXIS = 46;
export const DAILY_CHART_RIGHT = 12;
/** Narrowest day column before the chart scrolls sideways. */
export const DAILY_CHART_MIN_DAY = 11;

export function dailyChartMinWidth(count: number): number {
  return count > 7
    ? count * DAILY_CHART_MIN_DAY + DAILY_CHART_Y_AXIS + DAILY_CHART_RIGHT
    : 0;
}

/** Day index under an x offset inside the chart, clamped to the plot. */
export function dailyIndexAt(x: number, width: number, count: number): number {
  if (count <= 0) return -1;
  const plot = Math.max(1, width - DAILY_CHART_Y_AXIS - DAILY_CHART_RIGHT);
  const index = Math.floor(((x - DAILY_CHART_Y_AXIS) / plot) * count);
  return Math.min(count - 1, Math.max(0, index));
}

/** Left edge and width of a day column, for scrolling it into view. */
export function dailyColumn(index: number, width: number, count: number) {
  const plot = Math.max(1, width - DAILY_CHART_Y_AXIS - DAILY_CHART_RIGHT);
  const band = plot / Math.max(1, count);
  return { left: DAILY_CHART_Y_AXIS + index * band, width: band };
}

export type DailyStep = "prev" | "next" | "first" | "last" | "clear";

/** Readout stepping: with nothing picked, both directions start from the latest day. */
export function stepDailySelection(
  selected: number | null,
  step: DailyStep,
  count: number,
): number | null {
  if (count <= 0) return null;
  const last = count - 1;
  const base = selected === null ? last : Math.min(selected, last);
  switch (step) {
    case "prev":
      return selected === null ? Math.max(0, last - 1) : Math.max(0, base - 1);
    case "next":
      return selected === null ? last : Math.min(last, base + 1);
    case "first":
      return 0;
    case "last":
      return last;
    default:
      return null;
  }
}

export function dailySeries(t: TFunction): DailySeriesDescriptor[] {
  return [
    {
      key: "cache_read_tokens",
      label: t(msg("cache R", "cache R")),
      color: DATA_SERIES[0],
    },
    {
      key: "cache_creation_tokens",
      label: t(msg("cache W", "cache W")),
      color: DATA_SERIES[1],
    },
    {
      key: "input_tokens",
      label: t(msg("input", "input")),
      color: DATA_SERIES[2],
    },
    {
      key: "output_tokens",
      label: t(msg("output", "output")),
      color: DATA_SERIES[3],
    },
  ];
}
