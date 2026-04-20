import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type CSSProperties,
  type ReactNode,
} from "react";
import {
  getSkillCatalog,
  getSkillRanking,
  getTokenAnalytics,
  type SkillRankingResponse,
} from "../api";
import { getProviderMeta, getProviderSeries } from "../app/providerTheme";
import type { TFunction } from "./dashboard/model";
import { DashboardEmptyState, cx } from "./dashboard/ui";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
  SkillCatalogEntry,
  TokenAnalyticsDailyPoint,
  TokenAnalyticsResponse,
} from "../types";
import {
  BarChart3,
  Cpu,
  Gauge,
  Info,
  RefreshCw,
  ShieldAlert,
  Users,
} from "lucide-react";

type Period = "7d" | "30d" | "90d";
type DailySeriesKey =
  | "cache_read_tokens"
  | "cache_creation_tokens"
  | "input_tokens"
  | "output_tokens";

interface StatsPageViewProps {
  settings: CompanySettings;
  stats?: DashboardStats | null;
  agents?: Agent[];
  sessions?: DispatchedSession[];
  meetings?: RoundTableMeeting[];
  requestedTab?: unknown;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (
    signal: "review" | "blocked" | "requested" | "stalled",
  ) => void;
  onOpenDispatchSessions?: () => void;
  onOpenSettings?: () => void;
  onRefreshMeetings?: () => void;
  onRequestedTabHandled?: () => void;
}

interface ShareSegment {
  id: string;
  label: string;
  tokens: number;
  percentage: number;
  color: string;
  sublabel?: string;
}

interface AgentSpendRow {
  id: string;
  label: string;
  tokens: number;
  cost: number;
  share: number;
  color: string;
}

interface AgentCacheRow {
  id: string;
  label: string;
  promptTokens: number;
  hitRate: number;
  savedCost: number | null;
}

interface SkillUsageRow {
  id: string;
  name: string;
  description: string;
  windowCalls: number;
}

interface AgentSkillRow {
  id: string;
  agentName: string;
  skillName: string;
  description: string;
  calls: number;
}

interface LeaderboardRow {
  id: string;
  label: string;
  avatar: string;
  tasksDone: number;
  xp: number;
  tokens: number;
}

interface DailySeriesDescriptor {
  key: DailySeriesKey;
  label: string;
  color: string;
}

type MetricDeltaTone = "up" | "down" | "flat";

interface MetricDelta {
  value: string;
  tone: MetricDeltaTone;
}

const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];
const NUMERIC_STYLE: CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
};
const AGENT_BAR_COLORS = [
  "var(--claude)",
  "var(--codex)",
  "oklch(0.72 0.14 265)",
  "oklch(0.78 0.1 235)",
  "color-mix(in oklch, var(--accent) 70%, white 30%)",
];

const STATS_SHELL_STYLES = `
  .stats-shell .page {
    padding: 24px 28px 48px;
    max-width: 1440px;
    width: 100%;
    margin: 0 auto;
    min-width: 0;
  }

  .stats-shell .page-header {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 16px;
    margin-bottom: 24px;
  }

  .stats-shell .page-title {
    font-family: var(--font-display);
    font-size: 22px;
    font-weight: 600;
    letter-spacing: -0.5px;
    line-height: 1.2;
    color: var(--th-text-heading);
  }

  .stats-shell .page-sub {
    margin-top: 4px;
    font-size: 13px;
    color: var(--th-text-muted);
    line-height: 1.6;
  }

  .stats-shell .page-controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
  }

  .stats-shell .grid {
    display: grid;
    gap: 14px;
  }

  .stats-shell .grid-4 {
    grid-template-columns: repeat(4, minmax(0, 1fr));
  }

  .stats-shell .grid-2 {
    grid-template-columns: repeat(2, minmax(0, 1fr));
  }

  .stats-shell .grid-feature {
    grid-template-columns: minmax(0, 2fr) minmax(0, 1fr);
  }

  .stats-shell .grid-extra {
    grid-template-columns: minmax(0, 1fr) minmax(0, 0.94fr);
  }

  .stats-shell .stack {
    display: grid;
    gap: 14px;
  }

  .stats-shell .card {
    background: var(--th-surface);
    border: 1px solid var(--th-border-subtle);
    border-radius: 18px;
    overflow: hidden;
    box-shadow: 0 10px 30px color-mix(in srgb, var(--th-shadow-color) 8%, transparent);
  }

  .stats-shell .card-head {
    padding: 14px 16px 0;
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }

  .stats-shell .card-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 12.5px;
    font-weight: 500;
    color: var(--th-text-secondary);
    letter-spacing: -0.1px;
  }

  .stats-shell .card-body {
    padding: 10px 16px 16px;
  }

  .stats-shell .metric {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }

  .stats-shell .metric-value {
    font-family: var(--font-display);
    font-size: 28px;
    font-weight: 600;
    letter-spacing: -1px;
    line-height: 1.1;
    font-variant-numeric: tabular-nums;
  }

  .stats-shell .metric-sub {
    display: flex;
    align-items: center;
    gap: 6px;
    font-size: 12px;
    color: var(--th-text-muted);
    font-variant-numeric: tabular-nums;
  }

  .stats-shell .seg {
    display: inline-flex;
    border: 1px solid var(--th-border-subtle);
    border-radius: 10px;
    padding: 2px;
    background: color-mix(in srgb, var(--th-surface-alt) 80%, transparent);
  }

  .stats-shell .seg button {
    padding: 4px 10px;
    border-radius: 8px;
    border: 0;
    background: transparent;
    color: var(--th-text-muted);
    font-size: 11.5px;
    font-variant-numeric: tabular-nums;
    transition: background 0.16s ease, color 0.16s ease;
  }

  .stats-shell .seg button.active {
    background: var(--th-surface);
    color: var(--th-text-primary);
    box-shadow: 0 1px 2px color-mix(in srgb, var(--th-shadow-color) 10%, transparent);
  }

  .stats-shell .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    padding: 2px 8px;
    border-radius: 999px;
    border: 1px solid var(--th-border-subtle);
    background: color-mix(in srgb, var(--th-surface-alt) 86%, transparent);
    color: var(--th-text-secondary);
    font-size: 11px;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
  }

  .stats-shell .chip-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 6px 10px;
    border: 1px solid var(--th-border-subtle);
    border-radius: 999px;
    background: color-mix(in srgb, var(--th-surface-alt) 86%, transparent);
    color: var(--th-text-secondary);
    font-size: 11px;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
    transition:
      background 0.16s ease,
      color 0.16s ease,
      border-color 0.16s ease;
  }

  .stats-shell .chip-btn:hover {
    background: var(--th-surface);
    color: var(--th-text-primary);
  }

  .stats-shell .delta {
    display: inline-flex;
    align-items: center;
    min-height: 20px;
    padding: 1px 5px;
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: 11px;
    letter-spacing: -0.2px;
  }

  .stats-shell .delta.up {
    color: var(--ok);
    background: color-mix(in oklch, var(--ok) 14%, transparent);
  }

  .stats-shell .delta.down {
    color: var(--err);
    background: color-mix(in oklch, var(--err) 14%, transparent);
  }

  .stats-shell .delta.flat {
    color: var(--th-text-muted);
    background: var(--th-overlay-subtle);
  }

  .stats-shell .bar-track {
    height: 6px;
    overflow: hidden;
    border-radius: 3px;
    background: var(--th-overlay-subtle);
  }

  .stats-shell .bar-fill {
    height: 100%;
    border-radius: 3px;
    transition: width 0.6s cubic-bezier(0.22, 1, 0.36, 1);
  }

  .stats-shell .list-section {
    margin-bottom: 10px;
    font-size: 10.5px;
    font-weight: 600;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--th-text-muted);
  }

  .stats-shell .list-card {
    border: 1px solid var(--th-border-subtle);
    border-radius: 14px;
    background: var(--th-bg-surface);
    padding: 12px;
  }

  .stats-shell .list-card.tight {
    padding: 10px 12px;
  }

  .stats-shell .stats-inline-alert {
    border-color: color-mix(in oklch, var(--warn) 30%, var(--th-border) 70%);
    background:
      linear-gradient(
        180deg,
        color-mix(in oklch, var(--warn) 8%, var(--th-surface) 92%) 0%,
        var(--th-surface) 100%
      );
  }

  @media (max-width: 1024px) {
    .stats-shell .page-header {
      align-items: flex-start;
      flex-direction: column;
    }

    .stats-shell .grid-2,
    .stats-shell .grid-feature,
    .stats-shell .grid-extra {
      grid-template-columns: minmax(0, 1fr);
    }
  }

  @media (max-width: 768px) {
    .stats-shell .page {
      padding: 16px 16px calc(9rem + env(safe-area-inset-bottom));
    }

    .stats-shell .grid-4 {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }

  @media (max-width: 520px) {
    .stats-shell .grid-4 {
      grid-template-columns: minmax(0, 1fr);
    }
  }
`;

function msg(ko: string, en = ko, ja = ko, zh = ko) {
  return { ko, en, ja, zh };
}

function resolveLocaleTag(language: CompanySettings["language"]): string {
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

function periodDayCount(period: Period): number {
  switch (period) {
    case "7d":
      return 7;
    case "90d":
      return 90;
    default:
      return 30;
  }
}

function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return Math.round(value).toString();
}

function formatCurrency(value: number): string {
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
}

function formatPercent(value: number, digits = 1): string {
  return `${value.toFixed(digits)}%`;
}

function formatCompactDate(value: string): string {
  const date = new Date(`${value}T00:00:00`);
  if (Number.isNaN(date.getTime())) return value;
  return `${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}

function formatDateLabel(value: string, localeTag: string): string {
  const date = new Date(`${value}T00:00:00`);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(localeTag, {
    month: "2-digit",
    day: "2-digit",
  }).format(date);
}

function computeCacheHitRate(
  inputTokens: number,
  cacheReadTokens: number,
  cacheCreationTokens = 0,
): number {
  const promptTokens = inputTokens + cacheReadTokens + cacheCreationTokens;
  if (promptTokens <= 0) return 0;
  return (cacheReadTokens / promptTokens) * 100;
}

function computeDailyHitRate(day: TokenAnalyticsDailyPoint): number {
  return computeCacheHitRate(
    day.input_tokens,
    day.cache_read_tokens,
    day.cache_creation_tokens,
  );
}

function buildWindowDelta(
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

function buildSavingsDelta(
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

function buildModelSegments(
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

function buildProviderSegments(
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

function buildSkillRows(
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

function buildAgentSkillRows(
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

function buildAgentSpendRows(
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

function buildAgentCacheRows(
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

function buildLeaderboardRows(
  stats: DashboardStats | null | undefined,
  agents: Agent[] | undefined,
): LeaderboardRow[] {
  const topAgents = stats?.top_agents ?? [];
  if (topAgents.length > 0) {
    return topAgents.slice(0, 5).map((agent) => ({
      id: agent.id,
      label: agent.alias?.trim() || agent.name_ko || agent.name,
      avatar: agent.avatar_emoji,
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
      avatar: agent.avatar_emoji,
      tasksDone: agent.stats_tasks_done,
      xp: agent.stats_xp,
      tokens: agent.stats_tokens,
    }));
}

function dailySeries(t: TFunction): DailySeriesDescriptor[] {
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

export default function StatsPageView({
  settings,
  stats,
  agents,
}: StatsPageViewProps) {
  const language = settings.language;
  const localeTag = useMemo(() => resolveLocaleTag(language), [language]);
  const numberFormatter = useMemo(
    () => new Intl.NumberFormat(localeTag),
    [localeTag],
  );
  const t: TFunction = useCallback(
    (messages) => messages[language] ?? messages.ko,
    [language],
  );

  const [period, setPeriod] = useState<Period>("30d");
  const [reloadKey, setReloadKey] = useState(0);
  const [analytics, setAnalytics] = useState<TokenAnalyticsResponse | null>(
    null,
  );
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(
    null,
  );
  const [catalog, setCatalog] = useState<SkillCatalogEntry[]>([]);
  const [loading, setLoading] = useState(true);
  const [skillLoading, setSkillLoading] = useState(true);
  const [catalogLoading, setCatalogLoading] = useState(true);
  const [analyticsError, setAnalyticsError] = useState<string | null>(null);
  const [skillError, setSkillError] = useState<string | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;

    const load = async () => {
      setCatalogLoading(true);
      setCatalogError(null);
      try {
        const next = await getSkillCatalog();
        if (!active) return;
        setCatalog(next);
      } catch {
        if (!active) return;
        setCatalogError(
          t(
            msg(
              "스킬 카탈로그를 불러오지 못했습니다.",
              "Unable to load the skill catalog.",
              "スキルカタログを読み込めませんでした。",
              "无法加载技能目录。",
            ),
          ),
        );
      } finally {
        if (active) setCatalogLoading(false);
      }
    };

    void load();
    return () => {
      active = false;
    };
  }, [t]);

  useEffect(() => {
    let active = true;
    const controller = new AbortController();

    const load = async () => {
      setLoading(true);
      setSkillLoading(true);
      setAnalyticsError(null);
      setSkillError(null);

      const [analyticsResult, skillResult] = await Promise.allSettled([
        getTokenAnalytics(period, { signal: controller.signal }),
        getSkillRanking(period, 16),
      ]);
      if (!active) return;

      if (analyticsResult.status === "fulfilled") {
        setAnalytics(analyticsResult.value);
      } else {
        setAnalyticsError(
          t(
            msg(
              "토큰 분석을 불러오지 못했습니다.",
              "Unable to load token analytics.",
              "トークン分析を読み込めませんでした。",
              "无法加载 Token 分析。",
            ),
          ),
        );
      }

      if (skillResult.status === "fulfilled") {
        setSkillRanking(skillResult.value);
      } else {
        setSkillError(
          t(
            msg(
              "스킬 랭킹을 불러오지 못했습니다.",
              "Unable to load skill ranking.",
              "スキルランキングを読み込めませんでした。",
              "无法加载技能排行。",
            ),
          ),
        );
      }

      setLoading(false);
      setSkillLoading(false);
    };

    void load();
    return () => {
      active = false;
      controller.abort();
    };
  }, [period, reloadKey, t]);

  const summary = analytics?.summary;
  const hasLoadError = Boolean(analyticsError || skillError || catalogError);
  const combinedError = [analyticsError, skillError, catalogError]
    .filter(Boolean)
    .join(" ");
  const series = useMemo(() => dailySeries(t), [t]);
  const totalInputTokens = useMemo(
    () => analytics?.daily.reduce((sum, day) => sum + day.input_tokens, 0) ?? 0,
    [analytics],
  );
  const totalCacheReadTokens = useMemo(
    () =>
      analytics?.daily.reduce((sum, day) => sum + day.cache_read_tokens, 0) ??
      0,
    [analytics],
  );
  const totalCacheCreationTokens = useMemo(
    () =>
      analytics?.daily.reduce(
        (sum, day) => sum + day.cache_creation_tokens,
        0,
      ) ?? 0,
    [analytics],
  );
  const overallCacheHitRate = useMemo(
    () =>
      computeCacheHitRate(
        totalInputTokens,
        totalCacheReadTokens,
        totalCacheCreationTokens,
      ),
    [totalCacheCreationTokens, totalCacheReadTokens, totalInputTokens],
  );
  const averageDailyHitRate = useMemo(() => {
    if (!analytics?.daily.length) return 0;
    const total = analytics.daily.reduce(
      (sum, day) => sum + computeDailyHitRate(day),
      0,
    );
    return total / analytics.daily.length;
  }, [analytics]);
  const modelSegments = useMemo(
    () => buildModelSegments(analytics),
    [analytics],
  );
  const providerSegments = useMemo(
    () => buildProviderSegments(analytics),
    [analytics],
  );
  const agentSpendRows = useMemo(
    () => buildAgentSpendRows(analytics),
    [analytics],
  );
  const agentCacheRows = useMemo(
    () => buildAgentCacheRows(analytics),
    [analytics],
  );
  const skillRows = useMemo(
    () => buildSkillRows(skillRanking, catalog, language),
    [catalog, language, skillRanking],
  );
  const topAgentSkillPairs = useMemo(
    () => buildAgentSkillRows(skillRanking, catalog, language).slice(0, 5),
    [catalog, language, skillRanking],
  );
  const leaderboardRows = useMemo(
    () => buildLeaderboardRows(stats, agents),
    [agents, stats],
  );
  const skillWindowCalls = useMemo(
    () => skillRanking?.overall.reduce((sum, row) => sum + row.calls, 0) ?? 0,
    [skillRanking],
  );
  const rangeDays = analytics?.days ?? periodDayCount(period);
  const peakDay = summary?.peak_day ?? null;
  const averageDailyTokens = summary?.average_daily_tokens ?? 0;
  const peakRatio =
    peakDay && averageDailyTokens > 0
      ? peakDay.total_tokens / averageDailyTokens
      : null;
  const tokenMomentumDelta = useMemo(
    () => buildWindowDelta(analytics?.daily ?? []),
    [analytics],
  );
  const cacheSavingsDelta = useMemo(
    () => buildSavingsDelta(summary),
    [summary],
  );

  return (
    <div
      data-testid="stats-page"
      className="page fade-in stats-shell mx-auto h-full w-full min-w-0 overflow-x-hidden overflow-y-auto animate-in fade-in duration-200"
    >
      <style>{STATS_SHELL_STYLES}</style>
      <div className="page fade-in">
        <section className="space-y-[14px]">
          <header data-testid="stats-page-header" className="page-header">
            <div className="min-w-0">
              <h1 className="page-title">
                {t(msg("통계", "Stats", "統計", "统计"))}
              </h1>
              <p className="page-sub">
                {t(
                  msg(
                    "토큰 / 비용 / 캐시 / 모델 분포를 한곳에서",
                    "Token, cost, cache, and model mix in one place.",
                    "トークン / コスト / キャッシュ / モデル分布を一か所で確認します。",
                    "在一个页面查看 Token、成本、缓存和模型分布。",
                  ),
                )}
              </p>
            </div>

            <div className="page-controls">
              <div className="seg" data-testid="stats-range-controls">
                {PERIOD_OPTIONS.map((option) => {
                  const active = option === period;
                  return (
                    <button
                      key={option}
                      data-testid={`stats-range-${option}`}
                      type="button"
                      className={cx(active ? "active" : "", "min-w-[4.75rem]")}
                      onClick={() => setPeriod(option)}
                      aria-pressed={active}
                    >
                      <span style={NUMERIC_STYLE}>
                        {t(
                          option === "7d"
                            ? msg("7일", "7d", "7日", "7天")
                            : option === "30d"
                              ? msg("30일", "30d", "30日", "30天")
                              : msg("90일", "90d", "90日", "90天"),
                        )}
                      </span>
                    </button>
                  );
                })}
              </div>
              <button
                type="button"
                className="chip-btn"
                data-testid="stats-refresh-button"
                onClick={() => setReloadKey((value) => value + 1)}
              >
                <RefreshCw
                  size={12}
                  className={cx(loading || skillLoading ? "animate-spin" : "")}
                />
                <span>
                  {t(msg("새로고침", "Refresh", "再読み込み", "刷新"))}
                </span>
              </button>
            </div>
          </header>

          {hasLoadError ? (
            <div className="card stats-inline-alert">
              <div className="card-body flex items-start gap-3">
                <ShieldAlert
                  size={18}
                  style={{
                    color: "var(--th-accent-warn)",
                    flexShrink: 0,
                    marginTop: 2,
                  }}
                />
                <div className="min-w-0">
                  <div
                    className="text-sm font-semibold"
                    style={{ color: "var(--th-text-heading)" }}
                  >
                    {t(
                      msg(
                        "일부 통계를 불러오지 못했습니다.",
                        "Some stats could not be loaded.",
                        "一部の統計を読み込めませんでした。",
                        "部分统计加载失败。",
                      ),
                    )}
                  </div>
                  <div
                    className="mt-1 text-xs leading-5"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {combinedError}
                  </div>
                </div>
              </div>
            </div>
          ) : null}

          <div className="grid grid-4" data-testid="stats-summary-grid">
            <div data-testid="stats-summary-total-tokens">
              <HeadlineMetricCard
                title={t(
                  msg("총 토큰", "Total Tokens", "総トークン", "总代币"),
                )}
                value={summary ? formatTokens(summary.total_tokens) : "…"}
                sub={t(
                  msg(
                    `${numberFormatter.format(rangeDays)}일 누적`,
                    `${numberFormatter.format(rangeDays)} day total`,
                    `${numberFormatter.format(rangeDays)}日累計`,
                    `${numberFormatter.format(rangeDays)} 天累计`,
                  ),
                )}
                tip={t(
                  msg(
                    "input + output + cache read / write 합산",
                    "Sum of input + output + cache read / write",
                    "input + output + cache read / write の合計",
                    "input + output + cache read / write 的总和",
                  ),
                )}
                delta={tokenMomentumDelta?.value}
                deltaTone={tokenMomentumDelta?.tone}
              />
            </div>
            <div data-testid="stats-summary-api-spend">
              <HeadlineMetricCard
                title={t(
                  msg("API 비용", "API Spend", "API コスト", "API 成本"),
                )}
                value={summary ? formatCurrency(summary.total_cost) : "…"}
                sub={
                  summary
                    ? t(
                        msg(
                          `${formatCurrency(summary.cache_discount)} 절감됨`,
                          `${formatCurrency(summary.cache_discount)} saved`,
                          `${formatCurrency(summary.cache_discount)} 節約`,
                          `${formatCurrency(summary.cache_discount)} 已节省`,
                        ),
                      )
                    : t(msg("비용 집계 대기", "Waiting for spend data"))
                }
                tip={t(
                  msg(
                    "캐시 할인을 반영한 실제 결제 비용",
                    "Actual spend after cache discounts",
                    "キャッシュ割引を反映した実支出",
                    "计入缓存折扣后的实际支出",
                  ),
                )}
                delta={cacheSavingsDelta?.value}
                deltaTone={cacheSavingsDelta?.tone}
              />
            </div>
            <div data-testid="stats-summary-cache-saved">
              <HeadlineMetricCard
                title={t(
                  msg("활성 일수", "Active Days", "稼働日数", "活跃天数"),
                )}
                value={
                  summary
                    ? `${numberFormatter.format(summary.active_days)} / ${numberFormatter.format(rangeDays)}`
                    : "…"
                }
                sub={
                  summary
                    ? t(
                        msg(
                          `일 평균 ${formatTokens(Math.round(averageDailyTokens))}`,
                          `Avg ${formatTokens(Math.round(averageDailyTokens))} per day`,
                          `平均 ${formatTokens(Math.round(averageDailyTokens))} / 日`,
                          `日均 ${formatTokens(Math.round(averageDailyTokens))}`,
                        ),
                      )
                    : t(
                        msg(
                          "활성 일수 집계 대기",
                          "Waiting for active-day data",
                        ),
                      )
                }
                tip={t(
                  msg(
                    "선택 기간 중 실제 활동이 있었던 일수",
                    "Days with activity in the selected range",
                    "選択期間で実際に稼働した日数",
                    "所选范围内有实际活动的天数",
                  ),
                )}
              />
            </div>
            <div data-testid="stats-summary-cache-hit">
              <HeadlineMetricCard
                title={t(msg("피크 데이", "Peak Day", "ピーク日", "峰值日"))}
                value={peakDay ? formatCompactDate(peakDay.date) : "—"}
                sub={
                  peakDay
                    ? t(
                        msg(
                          `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `${peakRatio.toFixed(1)}x 평균` : "평균 대비"}`,
                          `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `${peakRatio.toFixed(1)}x avg` : "vs average"}`,
                          `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `平均の ${peakRatio.toFixed(1)}x` : "平均比"}`,
                          `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `${peakRatio.toFixed(1)}x 平均` : "相对平均"}`,
                        ),
                      )
                    : t(msg("피크 데이터 없음", "No peak-day data"))
                }
                tip={t(
                  msg(
                    "선택 기간 내 최고 사용량 날짜",
                    "Highest-usage day in the selected range",
                    "選択期間内の最高使用量日",
                    "所选范围内使用量最高的一天",
                  ),
                )}
              />
            </div>
          </div>

          <div className="grid grid-feature">
            <div data-testid="stats-daily-token-chart">
              <DailyTokenCompositionCard
                t={t}
                localeTag={localeTag}
                loading={loading}
                daily={analytics?.daily ?? []}
                series={series}
              />
            </div>
            <div data-testid="stats-daily-cache-hit">
              <DailyCacheHitCard
                t={t}
                localeTag={localeTag}
                loading={loading}
                daily={analytics?.daily ?? []}
                averageHitRate={averageDailyHitRate}
              />
            </div>
          </div>

          <div className="grid grid-2">
            <div data-testid="stats-model-share">
              <ModelDistributionCard
                t={t}
                loading={loading}
                segments={modelSegments}
                totalTokens={summary?.total_tokens ?? 0}
              />
            </div>
            <div data-testid="stats-agent-cost">
              <AgentSpendCard
                t={t}
                loading={loading}
                rows={agentSpendRows}
                rangeDays={rangeDays}
              />
            </div>
          </div>

          <div data-testid="stats-agent-cache">
            <AgentCacheCard
              t={t}
              loading={loading}
              rows={agentCacheRows}
              overallCacheHitRate={overallCacheHitRate}
            />
          </div>

          <div className="grid grid-extra">
            <div data-testid="stats-provider-share">
              <ProviderDistributionCard
                t={t}
                loading={loading}
                segments={providerSegments}
              />
            </div>
            <div className="stack">
              <div data-testid="stats-skill-usage">
                <SkillUsageCard
                  t={t}
                  loading={skillLoading || catalogLoading}
                  rows={skillRows}
                  byAgentRows={topAgentSkillPairs}
                  windowCalls={skillWindowCalls}
                />
              </div>
              <div data-testid="stats-agent-leaderboard">
                <AgentLeaderboardCard t={t} rows={leaderboardRows} />
              </div>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}

function HeadlineMetricCard({
  title,
  value,
  sub,
  tip,
  delta,
  deltaTone,
}: {
  title: string;
  value: string;
  sub: string;
  tip: string;
  delta?: string;
  deltaTone?: MetricDeltaTone;
}) {
  return (
    <article className="card min-h-[128px]">
      <div className="card-body metric min-w-0">
        <div className="flex items-start justify-between gap-3">
          <div
            className="card-title text-[10.5px] font-semibold uppercase tracking-[0.18em]"
            style={{ color: "var(--th-text-muted)" }}
            data-tip={tip}
          >
            <span>{title}</span>
            <Info
              size={11}
              style={{ color: "var(--th-text-muted)", flexShrink: 0 }}
            />
          </div>
          {delta ? (
            <span className={cx("delta", deltaTone ?? "flat")}>{delta}</span>
          ) : null}
        </div>
        <div
          className="metric-value mt-3"
          style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}
        >
          {value}
        </div>
        <div
          className="metric-sub mt-2 text-xs leading-5"
          style={{ color: "var(--th-text-muted)" }}
        >
          {sub}
        </div>
      </div>
    </article>
  );
}

function CardHead({
  title,
  subtitle,
  actions,
}: {
  title: string;
  subtitle: string;
  actions?: ReactNode;
}) {
  return (
    <div className="card-head">
      <div className="min-w-0">
        <div className="card-title">{title}</div>
        <div
          className="mt-1 text-[11px] leading-5"
          style={{ color: "var(--th-text-muted)" }}
        >
          {subtitle}
        </div>
      </div>
      {actions ? (
        <div className="flex shrink-0 flex-wrap gap-2">{actions}</div>
      ) : null}
    </div>
  );
}

function LegendDot({ color, label }: { color: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span className="h-2 w-2 rounded-[2px]" style={{ background: color }} />
      <span>{label}</span>
    </span>
  );
}

function DailyTokenCompositionCard({
  t,
  localeTag,
  loading,
  daily,
  series,
}: {
  t: TFunction;
  localeTag: string;
  loading: boolean;
  daily: TokenAnalyticsDailyPoint[];
  series: DailySeriesDescriptor[];
}) {
  const maxTotal = Math.max(1, ...daily.map((day) => day.total_tokens));
  const labelStride = Math.max(1, Math.ceil(Math.max(daily.length, 1) / 7));

  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "일별 토큰 구성",
            "Daily Token Composition",
            "日次トークン構成",
            "每日 Token 构成",
          ),
        )}
        subtitle={t(
          msg(
            "input · output · cache read / write · 바 위에서 호버",
            "input · output · cache read / write · hover bars",
            "input · output · cache read / write · バーにホバー",
            "input · output · cache read / write · 悬停柱状图",
          ),
        )}
        actions={
          <div
            className="flex flex-wrap gap-3 text-[10.5px]"
            style={{ color: "var(--th-text-muted)", ...NUMERIC_STYLE }}
          >
            {series.map((item) => (
              <LegendDot key={item.key} color={item.color} label={item.label} />
            ))}
          </div>
        }
      />

      <div className="card-body">
        {daily.length === 0 ? (
          <DashboardEmptyState
            icon={<BarChart3 size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "일별 토큰 차트를 불러오는 중입니다.",
                      "Loading daily token chart.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 일별 토큰 데이터가 없습니다.",
                      "No daily token data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="overflow-x-auto overflow-y-hidden">
            <div
              className="flex items-end gap-1 pb-1"
              style={{ minWidth: `${Math.max(360, daily.length * 24)}px` }}
            >
              {daily.map((day, index) => {
                const height =
                  day.total_tokens > 0
                    ? Math.max(
                        12,
                        Math.round((day.total_tokens / maxTotal) * 184),
                      )
                    : 12;
                const compactLabel =
                  index === 0 ||
                  index === daily.length - 1 ||
                  index % labelStride === 0;
                const tooltip = [
                  formatDateLabel(day.date, localeTag),
                  `${formatTokens(day.total_tokens)} tokens`,
                  `${series[0].label}: ${formatTokens(day.cache_read_tokens)}`,
                  `${series[1].label}: ${formatTokens(day.cache_creation_tokens)}`,
                  `${series[2].label}: ${formatTokens(day.input_tokens)}`,
                  `${series[3].label}: ${formatTokens(day.output_tokens)}`,
                ].join("\n");

                return (
                  <div
                    key={day.date}
                    className="group flex min-w-[18px] flex-1 flex-col items-center gap-2"
                    title={tooltip}
                  >
                    <div className="flex h-[188px] items-end">
                      <div
                        className="flex w-[16px] flex-col-reverse overflow-hidden rounded-t-[6px] border sm:w-[18px]"
                        style={{
                          height,
                          borderColor: "var(--th-border-subtle)",
                          background: "var(--th-overlay-subtle)",
                        }}
                      >
                        {day.total_tokens > 0 ? (
                          series.map((item) => {
                            const value = day[item.key];
                            if (value <= 0) return null;
                            return (
                              <div
                                key={`${day.date}-${item.key}`}
                                style={{
                                  height: `${(value / day.total_tokens) * 100}%`,
                                  background: item.color,
                                }}
                              />
                            );
                          })
                        ) : (
                          <div
                            className="h-full w-full"
                            style={{ background: "var(--th-overlay-light)" }}
                          />
                        )}
                      </div>
                    </div>
                    <span
                      className="min-h-[1.8rem] whitespace-nowrap text-center text-[10px] leading-4"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {compactLabel ? formatCompactDate(day.date) : ""}
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        )}
      </div>
    </article>
  );
}

function DailyCacheHitCard({
  t,
  localeTag,
  loading,
  daily,
  averageHitRate,
}: {
  t: TFunction;
  localeTag: string;
  loading: boolean;
  daily: TokenAnalyticsDailyPoint[];
  averageHitRate: number;
}) {
  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "일별 캐시 히트율",
            "Daily Cache Hit Rate",
            "日次キャッシュヒット率",
            "每日缓存命中率",
          ),
        )}
        subtitle={t(
          msg(
            "prompt 토큰 중 캐시 비중",
            "Cache share among prompt tokens",
            "prompt トークン内のキャッシュ比率",
            "prompt Token 中的缓存占比",
          ),
        )}
        actions={
          <span className="chip" style={positiveChipStyle}>
            {formatPercent(averageHitRate)}{" "}
            {t(msg("평균", "avg", "平均", "平均"))}
          </span>
        }
      />

      <div className="card-body">
        {daily.length === 0 ? (
          <DashboardEmptyState
            icon={<Gauge size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "캐시 히트율을 불러오는 중입니다.",
                      "Loading cache hit rate.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 캐시 히트율 데이터가 없습니다.",
                      "No cache hit data available.",
                    ),
                  )
            }
          />
        ) : (
          <div>
            <div
              className="grid h-[180px] items-end gap-1"
              style={{
                gridTemplateColumns: `repeat(${daily.length}, minmax(0, 1fr))`,
              }}
            >
              {daily.map((day) => {
                const hitRate = computeDailyHitRate(day);
                return (
                  <div
                    key={day.date}
                    title={`${formatDateLabel(day.date, localeTag)}: ${formatPercent(hitRate)}`}
                    className="rounded-t-[4px]"
                    style={{
                      height: `${Math.max(hitRate, hitRate > 0 ? 2 : 0)}%`,
                      background:
                        "linear-gradient(180deg, var(--codex), color-mix(in oklch, var(--codex) 60%, white 40%))",
                      opacity: 0.88,
                    }}
                  />
                );
              })}
            </div>
            <div
              className="mt-2 flex justify-between text-[10px]"
              style={{ color: "var(--th-text-muted)", ...NUMERIC_STYLE }}
            >
              <span>{formatCompactDate(daily[0].date)}</span>
              <span>{formatCompactDate(daily[daily.length - 1].date)}</span>
            </div>
          </div>
        )}
      </div>
    </article>
  );
}

function ModelDistributionCard({
  t,
  loading,
  segments,
  totalTokens,
}: {
  t: TFunction;
  loading: boolean;
  segments: ShareSegment[];
  totalTokens: number;
}) {
  return (
    <article className="card">
      <CardHead
        title={t(
          msg("모델 분포", "Model Distribution", "モデル分布", "模型分布"),
        )}
        subtitle={t(
          msg(
            "Claude / Codex 모델별 토큰 배분",
            "Token share by Claude / Codex models",
            "Claude / Codex モデル別トークン配分",
            "按 Claude / Codex 模型划分的 Token 占比",
          ),
        )}
        actions={
          <span className="chip" style={numericBadgeStyle}>
            {formatTokens(totalTokens)} total
          </span>
        }
      />

      <div className="card-body">
        {segments.length === 0 ? (
          <DashboardEmptyState
            icon={<Cpu size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "모델 분포를 불러오는 중입니다.",
                      "Loading model distribution.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 모델 데이터가 없습니다.",
                      "No model data available.",
                    ),
                  )
            }
          />
        ) : (
          <div>
            <div className="mb-4 flex h-2.5 overflow-hidden rounded-full">
              {segments.map((segment) => (
                <div
                  key={segment.id}
                  title={`${segment.label} ${formatPercent(segment.percentage)}`}
                  style={{
                    width: `${segment.percentage}%`,
                    background: segment.color,
                    minWidth: segment.percentage > 0 ? "8px" : "0",
                  }}
                />
              ))}
            </div>

            <div className="space-y-3">
              {segments.map((segment) => (
                <div
                  key={segment.id}
                  className="grid grid-cols-[auto_minmax(0,1fr)_auto_auto] items-center gap-3 rounded-[14px] border border-transparent px-1 py-1"
                >
                  <span
                    className="mt-1 h-2.5 w-2.5 rounded-[3px]"
                    style={{ background: segment.color }}
                  />
                  <div className="min-w-0">
                    <div
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text-heading)" }}
                    >
                      {segment.label}
                    </div>
                    <div
                      className="mt-1 text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {segment.sublabel}
                    </div>
                  </div>
                  <div
                    className="text-right text-[11px]"
                    style={{ color: "var(--th-text-muted)", ...NUMERIC_STYLE }}
                  >
                    {formatTokens(segment.tokens)}
                  </div>
                  <div
                    className="min-w-[48px] text-right text-sm font-semibold"
                    style={{
                      color: "var(--th-text-heading)",
                      ...NUMERIC_STYLE,
                    }}
                  >
                    {formatPercent(segment.percentage)}
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}
      </div>
    </article>
  );
}

function ProviderDistributionCard({
  t,
  loading,
  segments,
}: {
  t: TFunction;
  loading: boolean;
  segments: ShareSegment[];
}) {
  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "프로바이더 분포",
            "Provider Share",
            "プロバイダー分布",
            "Provider 分布",
          ),
        )}
        subtitle={t(
          msg(
            "Claude / Codex / 기타 런타임별 토큰 비중",
            "Token mix by runtime provider",
            "ランタイムプロバイダー別トークン比率",
            "按运行时 Provider 划分的 Token 占比",
          ),
        )}
        actions={
          <span className="chip" style={numericBadgeStyle}>
            {segments.length}{" "}
            {t(msg("providers", "providers", "providers", "providers"))}
          </span>
        }
      />

      <div className="card-body">
        {segments.length === 0 ? (
          <DashboardEmptyState
            icon={<Cpu size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "프로바이더 분포를 불러오는 중입니다.",
                      "Loading provider mix.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 프로바이더 데이터가 없습니다.",
                      "No provider data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="space-y-3">
            {segments.map((segment) => (
              <div key={segment.id} className="list-card tight">
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2">
                      <span
                        className="h-2.5 w-2.5 rounded-[3px]"
                        style={{ background: segment.color }}
                      />
                      <span
                        className="truncate text-sm font-semibold"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {segment.label}
                      </span>
                    </div>
                    <div
                      className="mt-1 text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {segment.sublabel}
                    </div>
                  </div>
                  <div className="text-right" style={{ ...NUMERIC_STYLE }}>
                    <div
                      className="text-sm font-semibold"
                      style={{ color: "var(--th-text-heading)" }}
                    >
                      {formatPercent(segment.percentage)}
                    </div>
                    <div
                      className="text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {formatTokens(segment.tokens)}
                    </div>
                  </div>
                </div>
                <div className="bar-track mt-3">
                  <div
                    className="bar-fill"
                    style={{
                      width: `${Math.max(segment.percentage, segment.percentage > 0 ? 4 : 0)}%`,
                      background: segment.color,
                    }}
                  />
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </article>
  );
}

function AgentSpendCard({
  t,
  loading,
  rows,
  rangeDays,
}: {
  t: TFunction;
  loading: boolean;
  rows: AgentSpendRow[];
  rangeDays: number;
}) {
  const maxCost = Math.max(1, ...rows.map((row) => row.cost));

  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "에이전트별 비용 비교",
            "Spend by Agent",
            "エージェント別コスト比較",
            "按代理比较成本",
          ),
        )}
        subtitle={t(
          msg(
            `${rangeDays}일 누적 지출`,
            `${rangeDays}d accumulated spend`,
            `${rangeDays}日累積支出`,
            `${rangeDays} 天累计支出`,
          ),
        )}
      />

      <div className="card-body">
        {rows.length === 0 ? (
          <DashboardEmptyState
            icon={<Users size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "에이전트 비용을 불러오는 중입니다.",
                      "Loading agent spend.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 에이전트 비용 데이터가 없습니다.",
                      "No agent spend data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="flex flex-col gap-3">
            {rows.map((row, index) => (
              <div
                key={row.id}
                className="grid grid-cols-[20px_minmax(0,1fr)_auto] items-center gap-3"
              >
                <span
                  className="inline-grid h-5 w-5 place-items-center rounded-full text-[10px]"
                  style={{
                    background: "var(--th-overlay-subtle)",
                    color: "var(--th-text-muted)",
                    ...NUMERIC_STYLE,
                  }}
                >
                  {index + 1}
                </span>
                <div className="min-w-0">
                  <div className="mb-1 flex items-center gap-2">
                    <span
                      className="truncate text-sm font-medium"
                      style={{ color: "var(--th-text-heading)" }}
                    >
                      {row.label}
                    </span>
                    <span
                      className="text-[10.5px]"
                      style={{
                        color: "var(--th-text-muted)",
                        ...NUMERIC_STYLE,
                      }}
                    >
                      {formatTokens(row.tokens)} · {formatPercent(row.share)}
                    </span>
                  </div>
                  <div className="bar-track" style={{ height: 5 }}>
                    <div
                      className="bar-fill"
                      style={{
                        width: `${(row.cost / maxCost) * 100}%`,
                        background: row.color,
                      }}
                    />
                  </div>
                </div>
                <div
                  className="min-w-[68px] text-right text-sm font-semibold"
                  style={{ color: "var(--th-text-heading)", ...NUMERIC_STYLE }}
                >
                  {formatCurrency(row.cost)}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </article>
  );
}

function AgentCacheCard({
  t,
  loading,
  rows,
  overallCacheHitRate,
}: {
  t: TFunction;
  loading: boolean;
  rows: AgentCacheRow[];
  overallCacheHitRate: number;
}) {
  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "에이전트별 캐시 히트율",
            "Cache Hit Rate by Agent",
            "エージェント別キャッシュヒット率",
            "按代理的缓存命中率",
          ),
        )}
        subtitle={t(
          msg(
            "prompt 볼륨이 큰 에이전트 우선",
            "Ordered by prompt-heavy agents",
            "prompt ボリュームが大きいエージェント優先",
            "优先显示 prompt 量大的代理",
          ),
        )}
        actions={
          <span className="chip" style={positiveChipStyle}>
            {formatPercent(overallCacheHitRate)}{" "}
            {t(msg("전체", "overall", "全体", "整体"))}
          </span>
        }
      />

      <div className="card-body">
        {rows.length === 0 ? (
          <DashboardEmptyState
            icon={<Gauge size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "에이전트 캐시 데이터를 불러오는 중입니다.",
                      "Loading agent cache data.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 에이전트 캐시 데이터가 없습니다.",
                      "No agent cache data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="flex flex-col gap-4">
            {rows.map((row, index) => {
              const sub =
                row.savedCost != null
                  ? t(
                      msg(
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} 절감`,
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} saved`,
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} 節約`,
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} 已节省`,
                      ),
                    )
                  : t(
                      msg(
                        `${formatTokens(row.promptTokens)} prompt`,
                        `${formatTokens(row.promptTokens)} prompt`,
                        `${formatTokens(row.promptTokens)} prompt`,
                        `${formatTokens(row.promptTokens)} prompt`,
                      ),
                    );

              return (
                <div
                  key={row.id}
                  className="grid grid-cols-[24px_minmax(0,1fr)_auto] items-center gap-3"
                >
                  <span
                    className="inline-grid h-5 w-5 place-items-center rounded-full text-[10px] font-semibold"
                    style={{
                      background: "var(--codex-soft)",
                      color: "var(--codex)",
                      ...NUMERIC_STYLE,
                    }}
                  >
                    {index + 1}
                  </span>
                  <div className="min-w-0">
                    <div className="mb-1 flex flex-wrap items-baseline gap-x-2 gap-y-1">
                      <span
                        className="text-sm font-medium"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {row.label}
                      </span>
                      <span
                        className="text-[10.5px]"
                        style={{
                          color: "var(--th-text-muted)",
                          ...NUMERIC_STYLE,
                        }}
                      >
                        {sub}
                      </span>
                    </div>
                    <div className="bar-track" style={{ height: 5 }}>
                      <div
                        className="bar-fill"
                        style={{
                          width: `${Math.max(row.hitRate, row.hitRate > 0 ? 4 : 0)}%`,
                          background:
                            "linear-gradient(90deg, var(--codex), color-mix(in oklch, var(--codex) 60%, white 40%))",
                        }}
                      />
                    </div>
                  </div>
                  <div
                    className="min-w-[56px] text-right text-sm font-semibold"
                    style={{ color: "var(--ok)", ...NUMERIC_STYLE }}
                  >
                    {formatPercent(row.hitRate)}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </article>
  );
}

function SkillUsageCard({
  t,
  loading,
  rows,
  byAgentRows,
  windowCalls,
}: {
  t: TFunction;
  loading: boolean;
  rows: SkillUsageRow[];
  byAgentRows: AgentSkillRow[];
  windowCalls: number;
}) {
  const maxCalls = Math.max(1, ...rows.map((row) => row.windowCalls));

  return (
    <article className="card">
      <CardHead
        title={t(msg("스킬 사용", "Skill Usage", "スキル使用量", "技能使用"))}
        subtitle={t(
          msg(
            "현재 기간에 가장 자주 호출된 스킬",
            "Most-invoked skills in the selected period",
            "選択期間で最も多く呼ばれたスキル",
            "所选期间调用最多的技能",
          ),
        )}
        actions={
          <span className="chip" style={numericBadgeStyle}>
            {windowCalls.toLocaleString()}{" "}
            {t(msg("calls", "calls", "calls", "calls"))}
          </span>
        }
      />

      <div className="card-body">
        {rows.length === 0 && byAgentRows.length === 0 ? (
          <DashboardEmptyState
            icon={<BarChart3 size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "스킬 사용량을 불러오는 중입니다.",
                      "Loading skill usage.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 스킬 사용 데이터가 없습니다.",
                      "No skill usage data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="grid grid-2 gap-3">
            <div>
              <div className="list-section">
                {t(msg("상위 스킬", "Top Skills", "上位スキル", "高频技能"))}
              </div>
              <div className="space-y-3">
                {rows.slice(0, 6).map((row, index) => (
                  <div key={`${row.id}-${index}`} className="list-card">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div
                          className="text-sm font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {row.name}
                        </div>
                        <div
                          className="mt-1 line-clamp-2 text-[11px] leading-5"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {row.description ||
                            t(msg("설명 없음", "No description"))}
                        </div>
                      </div>
                      <div className="text-right" style={{ ...NUMERIC_STYLE }}>
                        <div
                          className="text-base font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {row.windowCalls.toLocaleString()}
                        </div>
                        <div
                          className="text-[10.5px]"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {t(msg("calls", "calls", "calls", "calls"))}
                        </div>
                      </div>
                    </div>
                    <div className="bar-track mt-3" style={{ height: 5 }}>
                      <div
                        className="bar-fill"
                        style={{
                          width: `${Math.max((row.windowCalls / maxCalls) * 100, row.windowCalls > 0 ? 4 : 0)}%`,
                          background:
                            "linear-gradient(90deg, var(--accent), color-mix(in oklch, var(--accent) 62%, white 38%))",
                        }}
                      />
                    </div>
                  </div>
                ))}
              </div>
            </div>

            <div>
              <div className="list-section">
                {t(
                  msg(
                    "에이전트별 상위 조합",
                    "Top Agent-Skill Pairs",
                    "エージェント別上位組み合わせ",
                    "代理-技能高频组合",
                  ),
                )}
              </div>
              <div className="space-y-3">
                {byAgentRows.length === 0 ? (
                  <DashboardEmptyState
                    icon={<Users size={18} />}
                    title={t(
                      msg(
                        "에이전트별 스킬 데이터가 없습니다.",
                        "No agent-skill data available.",
                      ),
                    )}
                  />
                ) : (
                  byAgentRows.map((row, index) => (
                    <div key={`${row.id}-${index}`} className="list-card tight">
                      <div className="flex items-start justify-between gap-3">
                        <div className="min-w-0">
                          <div
                            className="truncate text-sm font-semibold"
                            style={{ color: "var(--th-text-heading)" }}
                          >
                            {row.agentName}
                          </div>
                          <div
                            className="mt-1 truncate text-[11px]"
                            style={{ color: "var(--th-text-secondary)" }}
                          >
                            {row.skillName}
                          </div>
                          <div
                            className="mt-1 line-clamp-2 text-[11px] leading-5"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {row.description ||
                              t(msg("설명 없음", "No description"))}
                          </div>
                        </div>
                        <div
                          className="text-right"
                          style={{ ...NUMERIC_STYLE }}
                        >
                          <div
                            className="text-sm font-semibold"
                            style={{ color: "var(--th-text-heading)" }}
                          >
                            {row.calls.toLocaleString()}
                          </div>
                          <div
                            className="text-[10.5px]"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {t(msg("calls", "calls", "calls", "calls"))}
                          </div>
                        </div>
                      </div>
                    </div>
                  ))
                )}
              </div>
            </div>
          </div>
        )}
      </div>
    </article>
  );
}

function AgentLeaderboardCard({
  t,
  rows,
}: {
  t: TFunction;
  rows: LeaderboardRow[];
}) {
  const maxTokens = Math.max(1, ...rows.map((row) => row.tokens));

  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "에이전트 리더보드",
            "Agent Leaderboard",
            "エージェントリーダーボード",
            "代理排行榜",
          ),
        )}
        subtitle={t(
          msg(
            "핵심 생산성 지표를 에이전트 기준으로 정리했습니다.",
            "Core productivity signals organized by agent.",
            "主要な生産性指標をエージェント基準で整理しました。",
            "按代理整理核心生产力指标。",
          ),
        )}
        actions={
          <span className="chip" style={numericBadgeStyle}>
            {rows.length} {t(msg("agents", "agents", "agents", "agents"))}
          </span>
        }
      />

      <div className="card-body">
        {rows.length === 0 ? (
          <DashboardEmptyState
            icon={<Users size={18} />}
            title={t(
              msg(
                "표시할 에이전트 리더보드가 없습니다.",
                "No agent leaderboard available.",
              ),
            )}
          />
        ) : (
          <div className="flex flex-col gap-3">
            {rows.map((row, index) => (
              <div key={row.id} className="list-card">
                <div className="flex items-center gap-3">
                  <span
                    className="inline-grid h-6 w-6 place-items-center rounded-full text-[10px] font-semibold"
                    style={{
                      background: "var(--th-overlay-light)",
                      color: "var(--th-text-secondary)",
                      ...NUMERIC_STYLE,
                    }}
                  >
                    {index + 1}
                  </span>
                  <span
                    className="inline-grid h-9 w-9 place-items-center rounded-full text-lg"
                    style={{ background: "var(--th-overlay-subtle)" }}
                  >
                    {row.avatar}
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center justify-between gap-3">
                      <div className="min-w-0">
                        <div
                          className="truncate text-sm font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {row.label}
                        </div>
                        <div
                          className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-[10.5px]"
                          style={{
                            color: "var(--th-text-muted)",
                            ...NUMERIC_STYLE,
                          }}
                        >
                          <span>{row.tasksDone} tasks</span>
                          <span>{formatTokens(row.xp)} xp</span>
                        </div>
                      </div>
                      <div
                        className="shrink-0 text-right text-sm font-semibold"
                        style={{
                          color: "var(--th-text-heading)",
                          ...NUMERIC_STYLE,
                        }}
                      >
                        {formatTokens(row.tokens)}
                      </div>
                    </div>
                    <div className="bar-track mt-3" style={{ height: 5 }}>
                      <div
                        className="bar-fill"
                        style={{
                          width: `${Math.max((row.tokens / maxTokens) * 100, row.tokens > 0 ? 4 : 0)}%`,
                          background:
                            "linear-gradient(90deg, var(--claude), color-mix(in oklch, var(--claude) 58%, white 42%))",
                        }}
                      />
                    </div>
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </article>
  );
}

const numericBadgeStyle: CSSProperties = {
  ...NUMERIC_STYLE,
  background: "var(--th-overlay-light)",
  color: "var(--th-text-secondary)",
  borderColor: "var(--th-border-subtle)",
};

const positiveChipStyle: CSSProperties = {
  ...NUMERIC_STYLE,
  background: "color-mix(in oklch, var(--ok) 10%, transparent)",
  color: "var(--ok)",
  borderColor:
    "color-mix(in oklch, var(--ok) 20%, var(--th-border-subtle) 80%)",
};
