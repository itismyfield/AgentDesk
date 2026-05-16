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

export interface ShareSegment {
  id: string;
  label: string;
  tokens: number;
  percentage: number;
  color: string;
  sublabel?: string;
}

export interface AgentSpendRow {
  id: string;
  label: string;
  tokens: number;
  cost: number;
  share: number;
  color: string;
}

export interface AgentCacheRow {
  id: string;
  label: string;
  promptTokens: number;
  hitRate: number;
  savedCost: number | null;
}

export interface SkillUsageRow {
  id: string;
  name: string;
  description: string;
  windowCalls: number;
}

export interface AgentSkillRow {
  id: string;
  agentName: string;
  skillName: string;
  description: string;
  calls: number;
}

export interface LeaderboardRow {
  id: string;
  label: string;
  agent: (Pick<Agent, "id" | "name"> & { sprite_number?: number | null }) | null;
  tasksDone: number;
  xp: number;
  tokens: number;
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

const AGENT_BAR_COLORS = [
  "var(--claude)",
  "var(--codex)",
  "oklch(0.72 0.14 265)",
  "oklch(0.78 0.1 235)",
  "color-mix(in oklch, var(--accent) 70%, white 30%)",
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

export function formatCurrency(value: number): string {
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
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

export function computeDailyHitRate(day: TokenAnalyticsDailyPoint): number {
  return computeCacheHitRate(
    day.input_tokens,
    day.cache_read_tokens,
    day.cache_creation_tokens,
  );
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

export function buildModelSegments(
  data: TokenAnalyticsResponse | null,
): ShareSegment[] {
  const models = data?.receipt.models ?? [];
  const totalTokens = models.reduce((sum, item) => sum + item.total_tokens, 0);
  if (totalTokens <= 0) return [];

  const sorted = [...models].sort(
    (left, right) => right.total_tokens - left.total_tokens,
  );
  const visible = sorted.slice(0, 5);
  const overflow = sorted.slice(5);

  const segments = visible.map((model, index) => ({
    id: `${model.provider}-${model.model}-${index}`,
    label: model.display_name,
    tokens: model.total_tokens,
    percentage: (model.total_tokens / totalTokens) * 100,
    color: getProviderSeries(model.provider)[
      index % getProviderSeries(model.provider).length
    ],
    sublabel: getProviderMeta(model.provider).label,
  }));

  if (overflow.length > 0) {
    const overflowTokens = overflow.reduce(
      (sum, item) => sum + item.total_tokens,
      0,
    );
    segments.push({
      id: "other-models",
      label: "Other",
      tokens: overflowTokens,
      percentage: (overflowTokens / totalTokens) * 100,
      color: "var(--fg-muted)",
      sublabel: `${overflow.length} models`,
    });
  }

  return segments;
}

export function buildProviderSegments(
  data: TokenAnalyticsResponse | null,
): ShareSegment[] {
  const providers = data?.receipt.providers ?? [];
  if (providers.length > 0) {
    const totalTokens = providers.reduce((sum, item) => sum + item.tokens, 0);
    return providers
      .filter((provider) => provider.tokens > 0)
      .sort((left, right) => right.tokens - left.tokens)
      .map((provider) => {
        const meta = getProviderMeta(provider.provider);
        const series = getProviderSeries(provider.provider);
        return {
          id: provider.provider,
          label: meta.label,
          tokens: provider.tokens,
          percentage:
            provider.percentage || (provider.tokens / totalTokens) * 100,
          color: series[0] ?? "var(--accent)",
          sublabel: provider.provider,
        };
      });
  }

  const models = data?.receipt.models ?? [];
  const byProvider = new Map<string, number>();
  for (const model of models) {
    byProvider.set(
      model.provider,
      (byProvider.get(model.provider) ?? 0) + model.total_tokens,
    );
  }
  const totalTokens = Array.from(byProvider.values()).reduce(
    (sum, value) => sum + value,
    0,
  );
  if (totalTokens <= 0) return [];

  return Array.from(byProvider.entries())
    .sort((left, right) => right[1] - left[1])
    .map(([provider, tokens]) => {
      const meta = getProviderMeta(provider);
      const series = getProviderSeries(provider);
      return {
        id: provider,
        label: meta.label,
        tokens,
        percentage: (tokens / totalTokens) * 100,
        color: series[0] ?? "var(--accent)",
        sublabel: provider,
      };
    });
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
      description,
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
      agentName: row.agent_name,
      skillName: row.skill_name,
      description,
      calls: row.calls,
    };
  });
}

export function buildAgentSpendRows(
  data: TokenAnalyticsResponse | null,
): AgentSpendRow[] {
  return [...(data?.receipt.agents ?? [])]
    .sort((left, right) => right.cost - left.cost)
    .slice(0, 5)
    .map((agent, index) => ({
      id: `${agent.agent}-${index}`,
      label: agent.agent,
      tokens: agent.tokens,
      cost: agent.cost,
      share: agent.percentage,
      color: AGENT_BAR_COLORS[index % AGENT_BAR_COLORS.length],
    }));
}

export function buildAgentCacheRows(
  data: TokenAnalyticsResponse | null,
): AgentCacheRow[] {
  return [...(data?.receipt.agents ?? [])]
    .map((agent) => {
      const promptTokens =
        (agent.input_tokens ?? 0) +
        (agent.cache_read_tokens ?? 0) +
        (agent.cache_creation_tokens ?? 0);
      return {
        id: agent.agent,
        label: agent.agent,
        promptTokens,
        hitRate: computeCacheHitRate(
          agent.input_tokens ?? 0,
          agent.cache_read_tokens ?? 0,
          agent.cache_creation_tokens ?? 0,
        ),
        savedCost:
          agent.cost_without_cache != null
            ? Math.max(0, agent.cost_without_cache - agent.cost)
            : null,
      };
    })
    .sort((left, right) => right.promptTokens - left.promptTokens)
    .slice(0, 5);
}

export function buildLeaderboardRows(
  stats: DashboardStats | null | undefined,
  agents: Agent[] | undefined,
): LeaderboardRow[] {
  const topAgents = stats?.top_agents ?? [];
  if (topAgents.length > 0) {
    return topAgents.slice(0, 5).map((agent) => ({
      id: agent.id,
      label: agent.alias?.trim() || agent.name_ko || agent.name,
      agent,
      tasksDone: agent.stats_tasks_done,
      xp: agent.stats_xp,
      tokens: agent.stats_tokens,
    }));
  }

  return [...(agents ?? [])]
    .sort((left, right) => right.stats_tokens - left.stats_tokens)
    .slice(0, 5)
    .map((agent) => ({
      id: agent.id,
      label: agent.alias?.trim() || agent.name_ko || agent.name,
      agent,
      tasksDone: agent.stats_tasks_done,
      xp: agent.stats_xp,
      tokens: agent.stats_tokens,
    }));
}

export function dailySeries(t: TFunction): DailySeriesDescriptor[] {
  return [
    {
      key: "cache_read_tokens",
      label: t(msg("cache R", "cache R")),
      color: "var(--codex)",
    },
    {
      key: "cache_creation_tokens",
      label: t(msg("cache W", "cache W")),
      color: "oklch(0.72 0.14 265)",
    },
    {
      key: "input_tokens",
      label: t(msg("input", "input")),
      color: "oklch(0.78 0.1 235)",
    },
    {
      key: "output_tokens",
      label: t(msg("output", "output")),
      color: "var(--claude)",
    },
  ];
}
