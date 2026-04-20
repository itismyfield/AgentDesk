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
import { timeAgo } from "./dashboard/model";
import {
  DashboardEmptyState,
  cx,
  dashboardBadge,
  dashboardButton,
  dashboardCard,
  dashboardText,
} from "./dashboard/ui";
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
  Activity,
  BarChart3,
  Bot,
  Coins,
  Cpu,
  Gauge,
  RefreshCw,
  ShieldAlert,
  Sparkles,
  Users,
} from "lucide-react";

type Period = "7d" | "30d" | "90d";
type PulseKanbanSignal = "review" | "blocked" | "requested" | "stalled";
type DailySeriesKey =
  | "input_tokens"
  | "output_tokens"
  | "cache_read_tokens"
  | "cache_creation_tokens";

interface StatsPageViewProps {
  settings: CompanySettings;
  stats?: DashboardStats | null;
  agents?: Agent[];
  sessions?: DispatchedSession[];
  meetings?: RoundTableMeeting[];
  requestedTab?: unknown;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (signal: PulseKanbanSignal) => void;
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

interface SkillUsageRow {
  id: string;
  name: string;
  description: string;
  windowCalls: number;
  windowShare: number;
  lifetimeCalls: number | null;
  lastUsedAt: number | null;
}

interface AgentSkillRow {
  id: string;
  agentName: string;
  skillName: string;
  description: string;
  calls: number;
  lastUsedAt: number;
}

interface AgentLeaderboardRow {
  id: string;
  label: string;
  tokens: number;
  share: number;
  cost: number;
  cacheHitRate: number;
}

const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];
const DAILY_BAR_HEIGHT_PX = 184;
const NUMERIC_STYLE: CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
};

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

function formatPercent(value: number): string {
  return `${value.toFixed(1)}%`;
}

function formatDateLabel(value: string, localeTag: string): string {
  const date = new Date(`${value}T00:00:00`);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(localeTag, {
    month: "2-digit",
    day: "2-digit",
  }).format(date);
}

function formatDateTime(value: number | string | null | undefined, localeTag: string): string {
  if (value == null) return "—";
  const date = typeof value === "number" ? new Date(value) : new Date(value);
  if (Number.isNaN(date.getTime())) return "—";
  return new Intl.DateTimeFormat(localeTag, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
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

function buildDonutBackground(segments: ShareSegment[]): string {
  if (segments.length === 0) {
    return "conic-gradient(rgba(148,163,184,0.16) 0deg 360deg)";
  }
  let cursor = 0;
  const stops = segments.map((segment) => {
    const start = cursor;
    cursor += segment.percentage * 3.6;
    return `${segment.color} ${start}deg ${cursor}deg`;
  });
  if (cursor < 360) {
    stops.push(`rgba(148,163,184,0.14) ${cursor}deg 360deg`);
  }
  return `conic-gradient(${stops.join(", ")})`;
}

function buildModelSegments(data: TokenAnalyticsResponse | null): ShareSegment[] {
  const models = data?.receipt.models ?? [];
  const totalTokens = models.reduce((sum, item) => sum + item.total_tokens, 0);
  if (totalTokens <= 0) return [];

  const sorted = [...models].sort((left, right) => right.total_tokens - left.total_tokens);
  const visible = sorted.slice(0, 6);
  const overflow = sorted.slice(6);

  const segments = visible.map((model, index) => ({
    id: `${model.provider}-${model.model}`,
    label: model.display_name,
    tokens: model.total_tokens,
    percentage: (model.total_tokens / totalTokens) * 100,
    color: getProviderSeries(model.provider)[index % getProviderSeries(model.provider).length],
    sublabel: getProviderMeta(model.provider).label,
  }));

  if (overflow.length > 0) {
    const overflowTokens = overflow.reduce((sum, item) => sum + item.total_tokens, 0);
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

function buildProviderSegments(data: TokenAnalyticsResponse | null): ShareSegment[] {
  if (!data) return [];
  const providers = data.receipt.providers;
  if (providers.length > 0) {
    return [...providers]
      .sort((left, right) => right.tokens - left.tokens)
      .map((provider) => ({
        id: provider.provider,
        label: getProviderMeta(provider.provider).label,
        tokens: provider.tokens,
        percentage: provider.percentage,
        color: getProviderMeta(provider.provider).color,
      }));
  }

  const byProvider = new Map<string, number>();
  for (const model of data.receipt.models) {
    byProvider.set(model.provider, (byProvider.get(model.provider) ?? 0) + model.total_tokens);
  }
  const totalTokens = Array.from(byProvider.values()).reduce((sum, value) => sum + value, 0);
  return Array.from(byProvider.entries())
    .sort((left, right) => right[1] - left[1])
    .map(([provider, tokens]) => ({
      id: provider,
      label: getProviderMeta(provider).label,
      tokens,
      percentage: totalTokens > 0 ? (tokens / totalTokens) * 100 : 0,
      color: getProviderMeta(provider).color,
    }));
}

function buildSkillRows(
  ranking: SkillRankingResponse | null,
  catalog: SkillCatalogEntry[],
  language: CompanySettings["language"],
): SkillUsageRow[] {
  if (!ranking) return [];
  const catalogMap = new Map(catalog.map((entry) => [entry.name, entry]));
  const totalCalls = ranking.overall.reduce((sum, row) => sum + row.calls, 0);

  return ranking.overall.slice(0, 8).map((row) => {
    const catalogEntry = catalogMap.get(row.skill_name);
    const description =
      language === "ko"
        ? catalogEntry?.description_ko || row.skill_desc_ko || catalogEntry?.description || row.skill_name
        : catalogEntry?.description || catalogEntry?.description_ko || row.skill_desc_ko || row.skill_name;

    return {
      id: row.skill_name,
      name: row.skill_name,
      description,
      windowCalls: row.calls,
      windowShare: totalCalls > 0 ? (row.calls / totalCalls) * 100 : 0,
      lifetimeCalls: catalogEntry?.total_calls ?? null,
      lastUsedAt: row.last_used_at ?? catalogEntry?.last_used_at ?? null,
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
        ? catalogEntry?.description_ko || row.skill_desc_ko || catalogEntry?.description || row.skill_name
        : catalogEntry?.description || catalogEntry?.description_ko || row.skill_desc_ko || row.skill_name;

    return {
      id: `${row.agent_role_id}-${row.skill_name}-${index}`,
      agentName: row.agent_name,
      skillName: row.skill_name,
      description,
      calls: row.calls,
      lastUsedAt: row.last_used_at,
    };
  });
}

function buildAgentLeaderboard(data: TokenAnalyticsResponse | null): AgentLeaderboardRow[] {
  return [...(data?.receipt.agents ?? [])]
    .sort((left, right) => right.tokens - left.tokens)
    .slice(0, 10)
    .map((agent) => ({
      id: agent.agent,
      label: agent.agent,
      tokens: agent.tokens,
      share: agent.percentage,
      cost: agent.cost,
      cacheHitRate: computeCacheHitRate(
        agent.input_tokens ?? 0,
        agent.cache_read_tokens ?? 0,
        agent.cache_creation_tokens ?? 0,
      ),
    }));
}

function dailySeries(t: TFunction): Array<{ key: DailySeriesKey; label: string; color: string }> {
  return [
    {
      key: "input_tokens",
      label: t({ ko: "입력", en: "Input", ja: "入力", zh: "输入" }),
      color: "#38bdf8",
    },
    {
      key: "output_tokens",
      label: t({ ko: "출력", en: "Output", ja: "出力", zh: "输出" }),
      color: "#fb923c",
    },
    {
      key: "cache_read_tokens",
      label: t({ ko: "캐시 읽기", en: "Cache Read", ja: "キャッシュ読取", zh: "缓存读取" }),
      color: "#22c55e",
    },
    {
      key: "cache_creation_tokens",
      label: t({ ko: "캐시 쓰기", en: "Cache Write", ja: "キャッシュ書込", zh: "缓存写入" }),
      color: "#a855f7",
    },
  ];
}

export default function StatsPageView({ settings }: StatsPageViewProps) {
  const language = settings.language;
  const localeTag = useMemo(() => resolveLocaleTag(language), [language]);
  const numberFormatter = useMemo(() => new Intl.NumberFormat(localeTag), [localeTag]);
  const t: TFunction = useCallback(
    (messages) => messages[language] ?? messages.ko,
    [language],
  );

  const [period, setPeriod] = useState<Period>("30d");
  const [reloadKey, setReloadKey] = useState(0);
  const [analytics, setAnalytics] = useState<TokenAnalyticsResponse | null>(null);
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(null);
  const [catalog, setCatalog] = useState<SkillCatalogEntry[]>([]);
  const [analyticsLoading, setAnalyticsLoading] = useState(true);
  const [skillLoading, setSkillLoading] = useState(true);
  const [catalogLoading, setCatalogLoading] = useState(true);
  const [analyticsError, setAnalyticsError] = useState<string | null>(null);
  const [skillError, setSkillError] = useState<string | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [lastRefreshedAt, setLastRefreshedAt] = useState<number | null>(null);

  useEffect(() => {
    let active = true;

    const loadCatalog = async () => {
      setCatalogLoading(true);
      setCatalogError(null);
      try {
        const next = await getSkillCatalog();
        if (!active) return;
        setCatalog(next);
      } catch {
        if (!active) return;
        setCatalogError(
          t({
            ko: "스킬 카탈로그를 불러오지 못했습니다.",
            en: "Unable to load the skill catalog.",
            ja: "スキルカタログを読み込めませんでした。",
            zh: "无法加载技能目录。",
          }),
        );
      } finally {
        if (active) setCatalogLoading(false);
      }
    };

    void loadCatalog();
    return () => {
      active = false;
    };
  }, [t]);

  useEffect(() => {
    let active = true;
    const controller = new AbortController();

    const load = async () => {
      setAnalyticsLoading(true);
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
          t({
            ko: "토큰 분석을 불러오지 못했습니다.",
            en: "Unable to load token analytics.",
            ja: "トークン分析を読み込めませんでした。",
            zh: "无法加载 Token 分析。",
          }),
        );
      }
      setAnalyticsLoading(false);

      if (skillResult.status === "fulfilled") {
        setSkillRanking(skillResult.value);
      } else {
        setSkillError(
          t({
            ko: "스킬 랭킹을 불러오지 못했습니다.",
            en: "Unable to load skill ranking.",
            ja: "スキルランキングを読み込めませんでした。",
            zh: "无法加载技能排行。",
          }),
        );
      }
      setSkillLoading(false);

      if (analyticsResult.status === "fulfilled" || skillResult.status === "fulfilled") {
        setLastRefreshedAt(Date.now());
      }
    };

    void load();
    return () => {
      active = false;
      controller.abort();
    };
  }, [period, reloadKey, t]);

  const series = useMemo(() => dailySeries(t), [t]);
  const summary = analytics?.summary;
  const totalPromptTokens = useMemo(
    () =>
      analytics?.daily.reduce(
        (sum, day) => sum + day.input_tokens + day.cache_read_tokens + day.cache_creation_tokens,
        0,
      ) ?? 0,
    [analytics],
  );
  const cacheReadTokens = useMemo(
    () => analytics?.daily.reduce((sum, day) => sum + day.cache_read_tokens, 0) ?? 0,
    [analytics],
  );
  const cacheHitRate = useMemo(
    () => computeCacheHitRate(totalPromptTokens - cacheReadTokens, cacheReadTokens),
    [cacheReadTokens, totalPromptTokens],
  );
  const uncachedBaseline = (summary?.total_cost ?? 0) + (summary?.cache_discount ?? 0);
  const modelSegments = useMemo(() => buildModelSegments(analytics), [analytics]);
  const providerSegments = useMemo(() => buildProviderSegments(analytics), [analytics]);
  const skillRows = useMemo(
    () => buildSkillRows(skillRanking, catalog, language),
    [catalog, language, skillRanking],
  );
  const byAgentSkillRows = useMemo(
    () => buildAgentSkillRows(skillRanking, catalog, language),
    [catalog, language, skillRanking],
  );
  const agentLeaderboard = useMemo(() => buildAgentLeaderboard(analytics), [analytics]);
  const activeSkillCount = useMemo(
    () => new Set(skillRanking?.overall.map((row) => row.skill_name) ?? []).size,
    [skillRanking],
  );
  const catalogUsedCount = useMemo(
    () => catalog.filter((entry) => entry.total_calls > 0).length,
    [catalog],
  );
  const windowCalls = useMemo(
    () => skillRanking?.overall.reduce((sum, row) => sum + row.calls, 0) ?? 0,
    [skillRanking],
  );
  const hasAnyLoadError = analyticsError || skillError || catalogError;

  return (
    <div
      data-testid="stats-page"
      className="mx-auto h-full w-full max-w-7xl min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <section className="space-y-4">
        <header
          className={dashboardCard.accentHero}
          style={{
            borderColor: "color-mix(in oklch, var(--accent) 28%, var(--th-border) 72%)",
            background:
              "linear-gradient(145deg, color-mix(in oklch, var(--accent) 10%, var(--th-surface) 90%) 0%, var(--th-surface) 100%)",
          }}
        >
          <div className="flex flex-col gap-5 xl:flex-row xl:items-end xl:justify-between">
            <div className="min-w-0">
              <div className={dashboardText.labelMuted} style={{ color: "var(--th-text-muted)" }}>
                {t({
                  ko: "Phase 2 Stats",
                  en: "Phase 2 Stats",
                  ja: "Phase 2 Stats",
                  zh: "Phase 2 Stats",
                })}
              </div>
              <h1
                className="mt-2 text-2xl font-black tracking-tight sm:text-3xl"
                style={{ color: "var(--th-text-heading)" }}
              >
                {t({
                  ko: "전용 통계 보드",
                  en: "Dedicated Stats Board",
                  ja: "専用統計ボード",
                  zh: "专用统计面板",
                })}
              </h1>
              <p className="mt-2 max-w-3xl text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
                {t({
                  ko: "범위를 바꾸면 요약 카드, 일별 토큰 차트, 모델/프로바이더 분포, 스킬 사용, 에이전트 리더보드가 함께 갱신됩니다.",
                  en: "Changing the range refreshes the summary cards, daily token chart, model/provider mix, skill usage, and agent leaderboard together.",
                  ja: "範囲を切り替えると、概要カード、日次トークンチャート、モデル/プロバイダー比率、スキル使用、エージェントリーダーボードが一緒に更新されます。",
                  zh: "切换时间范围时，摘要卡、每日 Token 图、模型/提供商占比、技能使用和代理排行榜会一起更新。",
                })}
              </p>
              <div className="mt-4 flex flex-wrap gap-2">
                <HeaderBadge
                  icon={<Activity size={14} />}
                  label={analytics?.period_label ?? t({ ko: "데이터 동기화 중", en: "Syncing data", ja: "同期中", zh: "同步中" })}
                />
                <HeaderBadge
                  icon={<Gauge size={14} />}
                  label={
                    summary
                      ? t({
                          ko: `${numberFormatter.format(summary.active_days)}일 활성`,
                          en: `${numberFormatter.format(summary.active_days)} active days`,
                          ja: `${numberFormatter.format(summary.active_days)}日稼働`,
                          zh: `${numberFormatter.format(summary.active_days)} 天活跃`,
                        })
                      : t({ ko: "활성 일수 집계 중", en: "Calculating active days", ja: "稼働日数を計算中", zh: "计算活跃天数" })
                  }
                />
                <HeaderBadge
                  icon={<Sparkles size={14} />}
                  label={
                    summary
                      ? t({
                          ko: `평균 ${formatTokens(summary.average_daily_tokens)} / 일`,
                          en: `Avg ${formatTokens(summary.average_daily_tokens)} / day`,
                          ja: `平均 ${formatTokens(summary.average_daily_tokens)} / 日`,
                          zh: `平均 ${formatTokens(summary.average_daily_tokens)} / 天`,
                        })
                      : t({ ko: "평균 처리량 계산 중", en: "Calculating average throughput", ja: "平均処理量を計算中", zh: "计算平均吞吐量" })
                  }
                />
                <HeaderBadge
                  icon={<RefreshCw size={14} className={cx(analyticsLoading || skillLoading ? "animate-spin" : "")} />}
                  label={
                    lastRefreshedAt
                      ? t({
                          ko: `${formatDateTime(lastRefreshedAt, localeTag)} 갱신`,
                          en: `Updated ${formatDateTime(lastRefreshedAt, localeTag)}`,
                          ja: `${formatDateTime(lastRefreshedAt, localeTag)} 更新`,
                          zh: `${formatDateTime(lastRefreshedAt, localeTag)} 更新`,
                        })
                      : t({ ko: "첫 집계 대기 중", en: "Waiting for first sync", ja: "初回同期待ち", zh: "等待首次同步" })
                  }
                />
              </div>
            </div>

            <div className="flex flex-col gap-3 xl:items-end">
              <div className="flex flex-wrap gap-2" data-testid="stats-range-controls">
                {PERIOD_OPTIONS.map((option) => {
                  const active = option === period;
                  return (
                    <button
                      key={option}
                      data-testid={`stats-range-${option}`}
                      type="button"
                      className={cx(dashboardButton.sm, "min-w-[4.5rem] justify-center")}
                      onClick={() => setPeriod(option)}
                      aria-pressed={active}
                      style={
                        active
                          ? {
                              background: "color-mix(in oklch, var(--accent) 20%, var(--th-surface) 80%)",
                              borderColor: "color-mix(in oklch, var(--accent) 36%, var(--th-border) 64%)",
                              color: "var(--th-text-heading)",
                            }
                          : undefined
                      }
                    >
                      <span style={NUMERIC_STYLE}>{option}</span>
                    </button>
                  );
                })}
              </div>
              <button
                type="button"
                className={cx(dashboardButton.sm, "justify-center gap-2")}
                onClick={() => setReloadKey((value) => value + 1)}
              >
                <RefreshCw size={14} className={cx(analyticsLoading || skillLoading ? "animate-spin" : "")} />
                {t({ ko: "다시 불러오기", en: "Reload", ja: "再読み込み", zh: "重新加载" })}
              </button>
            </div>
          </div>
        </header>

        {hasAnyLoadError ? (
          <div
            className={dashboardCard.nestedCompact}
            style={{
              borderColor: "color-mix(in oklch, var(--warn) 30%, var(--th-border) 70%)",
              background:
                "linear-gradient(180deg, color-mix(in oklch, var(--warn) 8%, var(--th-surface) 92%) 0%, var(--th-surface) 100%)",
            }}
          >
            <div className="flex items-start gap-3">
              <ShieldAlert
                size={18}
                style={{ color: "var(--th-accent-warn)", flexShrink: 0, marginTop: 2 }}
              />
              <div className="min-w-0">
                <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {t({
                    ko: "일부 통계를 새로고침하지 못했습니다.",
                    en: "Some stats could not be refreshed.",
                    ja: "一部の統計を更新できませんでした。",
                    zh: "部分统计刷新失败。",
                  })}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {[analyticsError, skillError, catalogError].filter(Boolean).join(" ")}
                </div>
              </div>
            </div>
          </div>
        ) : null}

        <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4" data-testid="stats-summary-grid">
          <div data-testid="stats-summary-total-tokens">
            <SummaryMetricCard
              icon={<BarChart3 size={18} />}
              label={t({ ko: "총 토큰", en: "Total Tokens", ja: "総トークン", zh: "总代币" })}
              value={summary ? formatTokens(summary.total_tokens) : "…"}
              sub={analytics?.period_label ?? t({ ko: "기간 집계 대기", en: "Waiting for range data", ja: "範囲集計待ち", zh: "等待区间数据" })}
              accent="#f59e0b"
            />
          </div>
          <div data-testid="stats-summary-api-spend">
            <SummaryMetricCard
              icon={<Coins size={18} />}
              label={t({ ko: "API 비용", en: "API Spend", ja: "API コスト", zh: "API 成本" })}
              value={summary ? formatCurrency(summary.total_cost) : "…"}
              sub={
                summary
                  ? t({
                      ko: `메시지 ${numberFormatter.format(summary.total_messages)} / 세션 ${numberFormatter.format(summary.total_sessions)}`,
                      en: `${numberFormatter.format(summary.total_messages)} messages / ${numberFormatter.format(summary.total_sessions)} sessions`,
                      ja: `${numberFormatter.format(summary.total_messages)} メッセージ / ${numberFormatter.format(summary.total_sessions)} セッション`,
                      zh: `${numberFormatter.format(summary.total_messages)} 消息 / ${numberFormatter.format(summary.total_sessions)} 会话`,
                    })
                  : t({ ko: "비용 집계 대기", en: "Waiting for spend data", ja: "コスト集計待ち", zh: "等待成本数据" })
              }
              accent="#22c55e"
            />
          </div>
          <div data-testid="stats-summary-cache-saved">
            <SummaryMetricCard
              icon={<Sparkles size={18} />}
              label={t({ ko: "캐시 절감", en: "Cache Saved", ja: "キャッシュ節約", zh: "缓存节省" })}
              value={summary ? formatCurrency(summary.cache_discount) : "…"}
              sub={
                summary
                  ? t({
                      ko: `uncached 기준 ${formatCurrency(uncachedBaseline)}`,
                      en: `${formatCurrency(uncachedBaseline)} uncached baseline`,
                      ja: `uncached 基準 ${formatCurrency(uncachedBaseline)}`,
                      zh: `uncached 基线 ${formatCurrency(uncachedBaseline)}`,
                    })
                  : t({ ko: "절감액 계산 대기", en: "Waiting for savings data", ja: "節約額計算待ち", zh: "等待节省数据" })
              }
              accent="#14b8a6"
            />
          </div>
          <div data-testid="stats-summary-cache-hit">
            <SummaryMetricCard
              icon={<Gauge size={18} />}
              label={t({ ko: "캐시 히트율", en: "Cache Hit Rate", ja: "キャッシュヒット率", zh: "缓存命中率" })}
              value={summary ? formatPercent(cacheHitRate) : "…"}
              sub={
                summary
                  ? t({
                      ko: `${formatTokens(cacheReadTokens)} cache read / ${formatTokens(totalPromptTokens)} prompt`,
                      en: `${formatTokens(cacheReadTokens)} cache read / ${formatTokens(totalPromptTokens)} prompt`,
                      ja: `${formatTokens(cacheReadTokens)} cache read / ${formatTokens(totalPromptTokens)} prompt`,
                      zh: `${formatTokens(cacheReadTokens)} cache read / ${formatTokens(totalPromptTokens)} prompt`,
                    })
                  : t({ ko: "히트율 계산 대기", en: "Waiting for cache hit data", ja: "ヒット率計算待ち", zh: "等待命中率数据" })
              }
              accent="#8b5cf6"
            />
          </div>
        </div>

        <div className="grid gap-4 xl:grid-cols-[minmax(0,1.4fr)_minmax(0,0.85fr)]">
          <div data-testid="stats-daily-token-chart">
            <DailyTokenChartCard
              t={t}
              localeTag={localeTag}
              numberFormatter={numberFormatter}
              loading={analyticsLoading}
              daily={analytics?.daily ?? []}
              series={series}
              summary={summary}
            />
          </div>

          <div className="grid gap-4">
            <div data-testid="stats-model-share">
              <DistributionCard
                icon={<Cpu size={18} />}
                title={t({ ko: "모델 점유율", en: "Model Share", ja: "モデル比率", zh: "模型占比" })}
                description={t({
                  ko: "선택한 범위에서 어떤 모델이 토큰을 가장 많이 처리했는지 보여줍니다.",
                  en: "Shows which models processed the most tokens in the selected window.",
                  ja: "選択範囲でどのモデルが最も多くのトークンを処理したかを示します。",
                  zh: "显示所选范围内处理 Token 最多的模型。",
                })}
                emptyLabel={t({
                  ko: "표시할 모델 분포가 없습니다.",
                  en: "No model distribution available.",
                  ja: "表示するモデル分布がありません。",
                  zh: "没有可显示的模型分布。",
                })}
                loading={analyticsLoading}
                segments={modelSegments}
                centerLabel={t({ ko: "모델", en: "Models", ja: "モデル", zh: "模型" })}
                centerValue={summary ? formatTokens(summary.total_tokens) : "0"}
                centerSub={
                  analytics
                    ? t({
                        ko: `${analytics.receipt.models.length}개 추적`,
                        en: `${analytics.receipt.models.length} tracked`,
                        ja: `${analytics.receipt.models.length}件追跡`,
                        zh: `追踪 ${analytics.receipt.models.length} 个`,
                      })
                    : t({ ko: "데이터 대기", en: "Waiting for data", ja: "データ待ち", zh: "等待数据" })
                }
              />
            </div>

            <div data-testid="stats-provider-share">
              <ProviderShareCard
                t={t}
                loading={analyticsLoading}
                segments={providerSegments}
              />
            </div>
          </div>
        </div>

        <div className="grid gap-4 xl:grid-cols-[minmax(0,1.15fr)_minmax(0,0.85fr)]">
          <div data-testid="stats-skill-usage">
            <SkillUsageSection
              t={t}
              localeTag={localeTag}
              loading={skillLoading || catalogLoading}
              skillRows={skillRows}
              byAgentRows={byAgentSkillRows}
              activeSkillCount={activeSkillCount}
              catalogCount={catalog.length}
              catalogUsedCount={catalogUsedCount}
              windowCalls={windowCalls}
            />
          </div>

          <div data-testid="stats-agent-leaderboard">
            <AgentLeaderboardCard
              t={t}
              loading={analyticsLoading}
              rows={agentLeaderboard}
            />
          </div>
        </div>
      </section>
    </div>
  );
}

function HeaderBadge({ icon, label }: { icon: ReactNode; label: string }) {
  return (
    <span
      className={dashboardBadge.default}
      style={{
        background: "var(--th-overlay-light)",
        color: "var(--th-text-secondary)",
        borderColor: "var(--th-border-subtle)",
      }}
    >
      <span className="inline-flex items-center gap-1.5">
        {icon}
        <span>{label}</span>
      </span>
    </span>
  );
}

function SummaryMetricCard({
  icon,
  label,
  value,
  sub,
  accent,
}: {
  icon: ReactNode;
  label: string;
  value: string;
  sub: string;
  accent: string;
}) {
  return (
    <article
      className={dashboardCard.standard}
      style={{
        borderColor: `color-mix(in oklch, ${accent} 30%, var(--th-border) 70%)`,
        background:
          `linear-gradient(180deg, color-mix(in oklch, ${accent} 8%, var(--th-surface) 92%) 0%, var(--th-surface) 100%)`,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className={dashboardText.label} style={{ color: "var(--th-text-muted)" }}>
            {label}
          </div>
          <div
            className="mt-3 text-3xl font-black tracking-tight tabular-nums"
            style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}
          >
            {value}
          </div>
          <div className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {sub}
          </div>
        </div>
        <span
          className="inline-flex h-10 w-10 shrink-0 items-center justify-center rounded-xl border"
          style={{
            background: `color-mix(in oklch, ${accent} 14%, transparent)`,
            borderColor: `color-mix(in oklch, ${accent} 34%, var(--th-border) 66%)`,
            color: accent,
          }}
        >
          {icon}
        </span>
      </div>
    </article>
  );
}

function DailyTokenChartCard({
  t,
  localeTag,
  numberFormatter,
  loading,
  daily,
  series,
  summary,
}: {
  t: TFunction;
  localeTag: string;
  numberFormatter: Intl.NumberFormat;
  loading: boolean;
  daily: TokenAnalyticsDailyPoint[];
  series: Array<{ key: DailySeriesKey; label: string; color: string }>;
  summary: TokenAnalyticsResponse["summary"] | null | undefined;
}) {
  const maxTotal = Math.max(1, ...daily.map((day) => day.total_tokens));
  const labelStride = Math.max(1, Math.ceil(Math.max(daily.length, 1) / 7));

  return (
    <article className={dashboardCard.standard}>
      <SectionHeader
        icon={<Activity size={18} />}
        title={t({ ko: "일별 토큰 스택", en: "Daily Token Stack", ja: "日次トークンスタック", zh: "每日 Token 堆栈" })}
        description={t({
          ko: "입력, 출력, 캐시 읽기/쓰기를 하루 단위 스택 바로 비교합니다.",
          en: "Compare input, output, cache read, and cache write as daily stacked bars.",
          ja: "入力・出力・キャッシュ読取/書込を日別の積み上げバーで比較します。",
          zh: "以每日堆叠柱比较输入、输出、缓存读写。",
        })}
        actions={
          summary ? (
            <div className="flex flex-wrap gap-2">
              <span className={dashboardBadge.default} style={numericBadgeStyle}>
                {t({
                  ko: `피크 ${summary.peak_day?.date ? formatDateLabel(summary.peak_day.date, localeTag) : "—"}`,
                  en: `Peak ${summary.peak_day?.date ? formatDateLabel(summary.peak_day.date, localeTag) : "—"}`,
                  ja: `ピーク ${summary.peak_day?.date ? formatDateLabel(summary.peak_day.date, localeTag) : "—"}`,
                  zh: `峰值 ${summary.peak_day?.date ? formatDateLabel(summary.peak_day.date, localeTag) : "—"}`,
                })}
              </span>
              <span className={dashboardBadge.default} style={numericBadgeStyle}>
                {t({
                  ko: `평균 ${formatTokens(summary.average_daily_tokens)} / 일`,
                  en: `Avg ${formatTokens(summary.average_daily_tokens)} / day`,
                  ja: `平均 ${formatTokens(summary.average_daily_tokens)} / 日`,
                  zh: `平均 ${formatTokens(summary.average_daily_tokens)} / 天`,
                })}
              </span>
            </div>
          ) : null
        }
      />

      {daily.length === 0 ? (
        <DashboardEmptyState
          icon={<BarChart3 size={18} />}
          title={
            loading
              ? t({
                  ko: "일별 토큰 차트를 불러오는 중입니다.",
                  en: "Loading the daily token chart.",
                  ja: "日次トークンチャートを読み込み中です。",
                  zh: "正在加载每日 Token 图。",
                })
              : t({
                  ko: "표시할 일별 토큰 데이터가 없습니다.",
                  en: "No daily token data available.",
                  ja: "表示する日次トークンデータがありません。",
                  zh: "没有可显示的每日 Token 数据。",
                })
          }
          className="mt-4"
        />
      ) : (
        <>
          <div className="mt-4 flex flex-wrap gap-2 text-[11px]">
            {series.map((item) => (
              <span key={item.key} className="inline-flex items-center gap-1.5" style={{ color: "var(--th-text-muted)" }}>
                <span
                  className="h-3.5 w-3.5 rounded-full"
                  style={{ background: item.color, boxShadow: `0 0 0 1px ${item.color}30 inset` }}
                />
                {item.label}
              </span>
            ))}
          </div>

          <div className="mt-4 overflow-x-auto overflow-y-visible">
            <div
              className="flex items-end gap-1 pb-1"
              style={{ minWidth: `${Math.max(360, daily.length * 24)}px` }}
            >
              {daily.map((day, index) => {
                const height =
                  day.total_tokens > 0
                    ? Math.max(10, Math.round((day.total_tokens / maxTotal) * DAILY_BAR_HEIGHT_PX))
                    : 10;
                const segments = series.filter((item) => day[item.key] > 0);
                const compactLabel =
                  index === 0 || index === daily.length - 1 || index % labelStride === 0;
                return (
                  <div
                    key={day.date}
                    className="group flex min-w-[20px] flex-1 flex-col items-center gap-2"
                    title={[
                      formatDateLabel(day.date, localeTag),
                      `${formatTokens(day.total_tokens)} tokens`,
                      `${formatCurrency(day.cost)}`,
                      ...segments.map((item) => `${item.label} ${formatTokens(day[item.key])}`),
                    ].join("\n")}
                  >
                    <div className="flex h-[188px] items-end">
                      <div
                        className="flex w-[18px] flex-col-reverse overflow-hidden rounded-t-xl border sm:w-[20px]"
                        style={{
                          height,
                          borderColor: "var(--th-border-subtle)",
                          background: "var(--th-overlay-subtle)",
                        }}
                      >
                        {segments.length > 0 ? (
                          segments.map((item) => (
                            <div
                              key={`${day.date}-${item.key}`}
                              style={{
                                height: `${(day[item.key] / day.total_tokens) * 100}%`,
                                background: item.color,
                              }}
                            />
                          ))
                        ) : (
                          <div className="h-full w-full" style={{ background: "var(--th-overlay-light)" }} />
                        )}
                      </div>
                    </div>
                    <span
                      className="min-h-[1.8rem] whitespace-nowrap text-center text-[10px] leading-4"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {compactLabel ? formatDateLabel(day.date, localeTag) : ""}
                    </span>
                  </div>
                );
              })}
            </div>
          </div>

          <div className="mt-4 grid gap-3 sm:grid-cols-3">
            <MiniMetric
              label={t({ ko: "활성 일수", en: "Active Days", ja: "稼働日数", zh: "活跃天数" })}
              value={summary ? numberFormatter.format(summary.active_days) : "—"}
            />
            <MiniMetric
              label={t({ ko: "피크 토큰", en: "Peak Tokens", ja: "ピークトークン", zh: "峰值 Token" })}
              value={summary?.peak_day ? formatTokens(summary.peak_day.total_tokens) : "—"}
            />
            <MiniMetric
              label={t({ ko: "피크 비용", en: "Peak Cost", ja: "ピークコスト", zh: "峰值成本" })}
              value={summary?.peak_day ? formatCurrency(summary.peak_day.cost) : "—"}
            />
          </div>
        </>
      )}
    </article>
  );
}

function DistributionCard({
  icon,
  title,
  description,
  emptyLabel,
  loading,
  segments,
  centerLabel,
  centerValue,
  centerSub,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  emptyLabel: string;
  loading: boolean;
  segments: ShareSegment[];
  centerLabel: string;
  centerValue: string;
  centerSub: string;
}) {
  const donutBackground = buildDonutBackground(segments);

  return (
    <article className={dashboardCard.standard}>
      <SectionHeader icon={icon} title={title} description={description} />
      {segments.length === 0 ? (
        <DashboardEmptyState
          icon={<Cpu size={18} />}
          title={loading ? "Loading distribution..." : emptyLabel}
          className="mt-4"
        />
      ) : (
        <div className="mt-4 grid gap-4 sm:grid-cols-[auto_minmax(0,1fr)] sm:items-center">
          <div className="mx-auto">
            <div
              className="relative h-40 w-40 rounded-full border"
              style={{
                background: donutBackground,
                borderColor: "var(--th-border-subtle)",
                boxShadow: "inset 0 1px 0 var(--th-overlay-subtle)",
              }}
            >
              <div
                className="absolute inset-[23%] flex flex-col items-center justify-center rounded-full border text-center"
                style={{
                  borderColor: "var(--th-border-subtle)",
                  background: "var(--th-bg-surface)",
                }}
              >
                <div className={dashboardText.labelMuted} style={{ color: "var(--th-text-muted)" }}>
                  {centerLabel}
                </div>
                <div className="mt-2 text-2xl font-black tracking-tight" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                  {centerValue}
                </div>
                <div className="mt-1 px-3 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {centerSub}
                </div>
              </div>
            </div>
          </div>

          <div className="space-y-3">
            {segments.map((segment) => (
              <DistributionRow
                key={segment.id}
                label={segment.label}
                sublabel={segment.sublabel}
                value={formatTokens(segment.tokens)}
                share={segment.percentage}
                color={segment.color}
              />
            ))}
          </div>
        </div>
      )}
    </article>
  );
}

function ProviderShareCard({
  t,
  loading,
  segments,
}: {
  t: TFunction;
  loading: boolean;
  segments: ShareSegment[];
}) {
  return (
    <article className={dashboardCard.standard}>
      <SectionHeader
        icon={<Bot size={18} />}
        title={t({ ko: "프로바이더 비중", en: "Provider Share", ja: "プロバイダー比率", zh: "提供商占比" })}
        description={t({
          ko: "Claude, Codex 등 프로바이더별 토큰 처리 비중을 비교합니다.",
          en: "Compare token volume across providers such as Claude and Codex.",
          ja: "Claude、Codex などプロバイダー別のトークン処理比率を比較します。",
          zh: "比较 Claude、Codex 等提供商的 Token 处理占比。",
        })}
      />

      {segments.length === 0 ? (
        <DashboardEmptyState
          icon={<Bot size={18} />}
          title={
            loading
              ? t({
                  ko: "프로바이더 비중을 불러오는 중입니다.",
                  en: "Loading provider share.",
                  ja: "プロバイダー比率を読み込み中です。",
                  zh: "正在加载提供商占比。",
                })
              : t({
                  ko: "표시할 프로바이더 데이터가 없습니다.",
                  en: "No provider data available.",
                  ja: "表示するプロバイダーデータがありません。",
                  zh: "没有可显示的提供商数据。",
                })
          }
          className="mt-4"
        />
      ) : (
        <div className="mt-4 space-y-3">
          {segments.map((segment) => (
            <DistributionRow
              key={segment.id}
              label={segment.label}
              value={formatTokens(segment.tokens)}
              share={segment.percentage}
              color={segment.color}
            />
          ))}
        </div>
      )}
    </article>
  );
}

function DistributionRow({
  label,
  sublabel,
  value,
  share,
  color,
}: {
  label: string;
  sublabel?: string;
  value: string;
  share: number;
  color: string;
}) {
  return (
    <div className={dashboardCard.nestedCompact}>
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="h-2.5 w-2.5 shrink-0 rounded-full" style={{ background: color }} />
            <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {label}
            </span>
          </div>
          {sublabel ? (
            <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
              {sublabel}
            </div>
          ) : null}
        </div>
        <div className="text-right">
          <div className="text-sm font-bold tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
            {formatPercent(share)}
          </div>
          <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {value}
          </div>
        </div>
      </div>
      <div className="mt-3 h-2.5 overflow-hidden rounded-full" style={{ background: "var(--th-overlay-subtle)" }}>
        <div
          className="h-full rounded-full"
          style={{
            width: `${Math.max(share, share > 0 ? 4 : 0)}%`,
            background: `linear-gradient(90deg, ${color}, color-mix(in oklch, ${color} 68%, white 32%))`,
          }}
        />
      </div>
    </div>
  );
}

function SkillUsageSection({
  t,
  localeTag,
  loading,
  skillRows,
  byAgentRows,
  activeSkillCount,
  catalogCount,
  catalogUsedCount,
  windowCalls,
}: {
  t: TFunction;
  localeTag: string;
  loading: boolean;
  skillRows: SkillUsageRow[];
  byAgentRows: AgentSkillRow[];
  activeSkillCount: number;
  catalogCount: number;
  catalogUsedCount: number;
  windowCalls: number;
}) {
  return (
    <article className={dashboardCard.standard}>
      <SectionHeader
        icon={<Sparkles size={18} />}
        title={t({ ko: "스킬 랭킹과 사용량", en: "Skill Ranking & Usage", ja: "スキルランキングと使用量", zh: "技能排行与使用量" })}
        description={t({
          ko: "선택 범위의 상위 스킬 호출과 에이전트별 스킬 사용을 한 화면에 모읍니다.",
          en: "Combine top skill calls in the selected window with agent-level skill usage.",
          ja: "選択範囲の上位スキル呼び出しとエージェント別スキル使用を一画面にまとめます。",
          zh: "将所选范围的高频技能调用与代理级技能使用放到同一屏。",
        })}
        actions={
          <div className="flex flex-wrap gap-2">
            <span className={dashboardBadge.default} style={numericBadgeStyle}>
              {t({
                ko: `${activeSkillCount}/${catalogCount || 0} 범위 활성`,
                en: `${activeSkillCount}/${catalogCount || 0} active in range`,
                ja: `${activeSkillCount}/${catalogCount || 0} 件が範囲内で活性`,
                zh: `${activeSkillCount}/${catalogCount || 0} 个在区间内活跃`,
              })}
            </span>
            <span className={dashboardBadge.default} style={numericBadgeStyle}>
              {t({
                ko: `${catalogUsedCount}/${catalogCount || 0} 카탈로그 사용`,
                en: `${catalogUsedCount}/${catalogCount || 0} catalog used`,
                ja: `${catalogUsedCount}/${catalogCount || 0} カタログ使用`,
                zh: `${catalogUsedCount}/${catalogCount || 0} 个目录已使用`,
              })}
            </span>
          </div>
        }
      />

      <div
        className={cx("mt-4", dashboardCard.nestedCompact)}
        style={{
          borderColor: "color-mix(in oklch, var(--warn) 32%, var(--th-border) 68%)",
          background:
            "linear-gradient(180deg, color-mix(in oklch, var(--warn) 6%, var(--th-surface) 94%) 0%, var(--th-surface) 100%)",
        }}
      >
        <div className="flex items-start gap-3">
          <ShieldAlert
            size={18}
            style={{ color: "var(--th-accent-warn)", flexShrink: 0, marginTop: 2 }}
          />
          <div className="min-w-0">
            <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
              {t({
                ko: "p95는 아직 API에 없습니다.",
                en: "p95 is not available in the API yet.",
                ja: "p95 はまだ API にありません。",
                zh: "API 目前还没有 p95。",
              })}
            </div>
            <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
              {t({
                ko: "`getSkillRanking()` / `getSkillCatalog()`에 p95 필드가 없어 이 영역은 호출량, 카탈로그 누적 호출, 마지막 사용 시점으로 대체했습니다.",
                en: "`getSkillRanking()` and `getSkillCatalog()` do not expose a p95 field, so this section uses call count, catalog lifetime calls, and last-used recency instead.",
                ja: "`getSkillRanking()` / `getSkillCatalog()` に p95 フィールドがないため、この領域では呼び出し回数、カタログ累積回数、最終使用時刻で代替しています。",
                zh: "`getSkillRanking()` / `getSkillCatalog()` 没有提供 p95 字段，因此此区域改为展示调用次数、目录累计调用和最近使用时间。",
              })}
            </div>
          </div>
        </div>
      </div>

      {skillRows.length === 0 && byAgentRows.length === 0 ? (
        <DashboardEmptyState
          icon={<Sparkles size={18} />}
          title={
            loading
              ? t({
                  ko: "스킬 사용 데이터를 불러오는 중입니다.",
                  en: "Loading skill usage.",
                  ja: "スキル使用データを読み込み中です。",
                  zh: "正在加载技能使用数据。",
                })
              : t({
                  ko: "표시할 스킬 사용 데이터가 없습니다.",
                  en: "No skill usage data available.",
                  ja: "表示するスキル使用データがありません。",
                  zh: "没有可显示的技能使用数据。",
                })
          }
          className="mt-4"
        />
      ) : (
        <div className="mt-4 grid gap-4 2xl:grid-cols-[minmax(0,1.2fr)_minmax(0,0.8fr)]">
          <div className="space-y-3">
            {skillRows.map((row) => (
              <div key={row.id} className={dashboardCard.nestedCompact}>
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.description}
                    </div>
                    <div className="mt-1 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {row.name}
                    </div>
                  </div>
                  <span className={dashboardBadge.default} style={numericBadgeStyle}>
                    {formatPercent(row.windowShare)}
                  </span>
                </div>

                <div className="mt-3 grid gap-3 sm:grid-cols-3">
                  <MiniMetric
                    label={t({ ko: "범위 호출", en: "Range Calls", ja: "範囲呼び出し", zh: "区间调用" })}
                    value={row.windowCalls.toLocaleString(localeTag)}
                  />
                  <MiniMetric
                    label={t({ ko: "카탈로그 누적", en: "Catalog Lifetime", ja: "カタログ累積", zh: "目录累计" })}
                    value={row.lifetimeCalls != null ? row.lifetimeCalls.toLocaleString(localeTag) : "—"}
                  />
                  <MiniMetric
                    label={t({ ko: "마지막 사용", en: "Last Used", ja: "最終使用", zh: "最近使用" })}
                    value={row.lastUsedAt ? timeAgo(row.lastUsedAt, localeTag) : "—"}
                  />
                </div>

                <div className="mt-3 h-2 overflow-hidden rounded-full" style={{ background: "var(--th-overlay-subtle)" }}>
                  <div
                    className="h-full rounded-full"
                    style={{
                      width: `${Math.max(row.windowShare, row.windowShare > 0 ? 4 : 0)}%`,
                      background:
                        "linear-gradient(90deg, var(--accent), color-mix(in oklch, var(--accent) 60%, white 40%))",
                    }}
                  />
                </div>
              </div>
            ))}
          </div>

          <div className="space-y-4">
            <div className={dashboardCard.nestedCompact}>
              <div className="flex items-center justify-between gap-3">
                <div>
                  <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                    {t({ ko: "에이전트별 상위 스킬", en: "Top Agent-Skill Pairs", ja: "エージェント別上位スキル", zh: "代理-技能高频组合" })}
                  </div>
                  <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                    {t({
                      ko: "가장 많이 호출된 에이전트-스킬 조합입니다.",
                      en: "Most frequently used agent-skill combinations.",
                      ja: "最も頻繁に使われたエージェント-スキルの組み合わせです。",
                      zh: "调用次数最多的代理-技能组合。",
                    })}
                  </div>
                </div>
                <span className={dashboardBadge.default} style={numericBadgeStyle}>
                  {windowCalls.toLocaleString(localeTag)}
                </span>
              </div>

              <div className="mt-4 space-y-2">
                {byAgentRows.map((row) => (
                  <div key={row.id} className={dashboardCard.smallCompact}>
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                          {row.agentName}
                        </div>
                        <div className="mt-1 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {row.description}
                        </div>
                        <div className="mt-1 truncate text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {row.skillName}
                        </div>
                      </div>
                      <div className="shrink-0 text-right">
                        <div className="text-sm font-bold tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                          {row.calls}
                        </div>
                        <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {timeAgo(row.lastUsedAt, localeTag)}
                        </div>
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </div>
        </div>
      )}
    </article>
  );
}

function AgentLeaderboardCard({
  t,
  loading,
  rows,
}: {
  t: TFunction;
  loading: boolean;
  rows: AgentLeaderboardRow[];
}) {
  return (
    <article className={dashboardCard.standard}>
      <SectionHeader
        icon={<Users size={18} />}
        title={t({ ko: "에이전트 리더보드", en: "Agent Leaderboard", ja: "エージェントリーダーボード", zh: "代理排行榜" })}
        description={t({
          ko: "토큰 분석 receipt 기준으로 에이전트별 토큰, 점유율, 비용, 캐시 히트율을 비교합니다.",
          en: "Compare tokens, share, cost, and cache hit rate by agent using the token analytics receipt.",
          ja: "トークン分析レシートを基準に、エージェント別のトークン、比率、コスト、キャッシュヒット率を比較します。",
          zh: "基于 Token 分析回执，对比各代理的 Token、占比、成本和缓存命中率。",
        })}
      />

      {rows.length === 0 ? (
        <DashboardEmptyState
          icon={<Users size={18} />}
          title={
            loading
              ? t({
                  ko: "에이전트 리더보드를 불러오는 중입니다.",
                  en: "Loading the agent leaderboard.",
                  ja: "エージェントリーダーボードを読み込み中です。",
                  zh: "正在加载代理排行榜。",
                })
              : t({
                  ko: "표시할 에이전트 데이터가 없습니다.",
                  en: "No agent data available.",
                  ja: "表示するエージェントデータがありません。",
                  zh: "没有可显示的代理数据。",
                })
          }
          className="mt-4"
        />
      ) : (
        <>
          <div
            className="mt-4 hidden grid-cols-[2.5rem_minmax(0,1.5fr)_1fr_0.8fr_0.9fr_0.9fr] gap-3 px-3 text-[11px] font-semibold uppercase tracking-[0.18em] sm:grid"
            style={{ color: "var(--th-text-muted)" }}
          >
            <span>#</span>
            <span>{t({ ko: "에이전트", en: "Agent", ja: "エージェント", zh: "代理" })}</span>
            <span>{t({ ko: "토큰", en: "Tokens", ja: "トークン", zh: "Token" })}</span>
            <span>{t({ ko: "점유율", en: "Share", ja: "比率", zh: "占比" })}</span>
            <span>{t({ ko: "비용", en: "Cost", ja: "コスト", zh: "成本" })}</span>
            <span>{t({ ko: "캐시", en: "Cache Hit", ja: "キャッシュ", zh: "缓存" })}</span>
          </div>

          <div className="mt-3 space-y-2">
            {rows.map((row, index) => (
              <div key={row.id}>
                <div
                  className="hidden grid-cols-[2.5rem_minmax(0,1.5fr)_1fr_0.8fr_0.9fr_0.9fr] items-center gap-3 rounded-xl border px-3 py-3 sm:grid"
                  style={{
                    borderColor: "var(--th-border-subtle)",
                    background: "var(--th-bg-surface)",
                  }}
                >
                  <span className="text-sm font-bold tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                    {index + 1}
                  </span>
                  <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                    {row.label}
                  </span>
                  <span className="text-sm tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                    {formatTokens(row.tokens)}
                  </span>
                  <span className="text-sm tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                    {formatPercent(row.share)}
                  </span>
                  <span className="text-sm tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                    {formatCurrency(row.cost)}
                  </span>
                  <span className="text-sm tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
                    {formatPercent(row.cacheHitRate)}
                  </span>
                </div>

                <div
                  className="rounded-xl border p-3 sm:hidden"
                  style={{
                    borderColor: "var(--th-border-subtle)",
                    background: "var(--th-bg-surface)",
                  }}
                >
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0">
                      <div className="text-xs font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
                        #{index + 1}
                      </div>
                      <div className="mt-1 truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {row.label}
                      </div>
                    </div>
                    <span className={dashboardBadge.default} style={numericBadgeStyle}>
                      {formatPercent(row.share)}
                    </span>
                  </div>
                  <div className="mt-3 grid grid-cols-2 gap-3">
                    <MiniMetric label={t({ ko: "토큰", en: "Tokens", ja: "トークン", zh: "Token" })} value={formatTokens(row.tokens)} />
                    <MiniMetric label={t({ ko: "비용", en: "Cost", ja: "コスト", zh: "成本" })} value={formatCurrency(row.cost)} />
                    <MiniMetric label={t({ ko: "캐시", en: "Cache Hit", ja: "キャッシュ", zh: "缓存" })} value={formatPercent(row.cacheHitRate)} />
                    <MiniMetric label={t({ ko: "점유율", en: "Share", ja: "比率", zh: "占比" })} value={formatPercent(row.share)} />
                  </div>
                </div>
              </div>
            ))}
          </div>
        </>
      )}
    </article>
  );
}

function SectionHeader({
  icon,
  title,
  description,
  actions,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
      <div className="min-w-0">
        <div className="flex items-center gap-2">
          <span
            className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-xl border"
            style={{
              background: "var(--th-overlay-subtle)",
              borderColor: "var(--th-border-subtle)",
              color: "var(--accent)",
            }}
          >
            {icon}
          </span>
          <div className="min-w-0">
            <h2 className="text-lg font-bold" style={{ color: "var(--th-text-heading)" }}>
              {title}
            </h2>
            <p className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {description}
            </p>
          </div>
        </div>
      </div>
      {actions ? <div className="flex shrink-0 flex-wrap gap-2">{actions}</div> : null}
    </div>
  );
}

function MiniMetric({ label, value }: { label: string; value: string }) {
  return (
    <div
      className="rounded-xl border px-3 py-2.5"
      style={{
        borderColor: "var(--th-border-subtle)",
        background: "var(--th-overlay-subtle)",
      }}
    >
      <div className={dashboardText.labelMuted} style={{ color: "var(--th-text-muted)" }}>
        {label}
      </div>
      <div className="mt-2 text-sm font-bold tabular-nums" style={{ ...NUMERIC_STYLE, color: "var(--th-text-heading)" }}>
        {value}
      </div>
    </div>
  );
}

const numericBadgeStyle: CSSProperties = {
  ...NUMERIC_STYLE,
  background: "var(--th-overlay-light)",
  color: "var(--th-text-secondary)",
  borderColor: "var(--th-border-subtle)",
};
