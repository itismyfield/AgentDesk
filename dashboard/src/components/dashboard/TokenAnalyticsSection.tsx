import { type CSSProperties, useEffect, useMemo, useState } from "react";
import { getKanbanCards, getTokenAnalytics } from "../../api";
import type {
  Agent,
  KanbanCard,
  ReceiptSnapshotAgentShare,
  ReceiptSnapshotModelLine,
  TokenAnalyticsDailyPoint,
  TokenAnalyticsHeatmapCell,
  TokenAnalyticsResponse,
} from "../../types";
import type { TFunction } from "./model";
import { cx, dashboardBadge, dashboardButton, dashboardCard } from "./ui";
import TooltipLabel from "../common/TooltipLabel";
import { buildAgentRoiRows } from "./dashboardInsights";

type Period = "7d" | "30d" | "90d";

interface TokenAnalyticsSectionProps {
  agents: Agent[];
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
}

interface CachedAnalyticsEntry {
  data: TokenAnalyticsResponse;
  fetchedAt: number;
}

interface ModelSegment {
  id: string;
  label: string;
  provider: string;
  tokens: number;
  percentage: number;
  color: string;
}

interface AgentCacheRow {
  id: string;
  label: string;
  promptTokens: number;
  cacheReadTokens: number;
  savings: number;
  hitRate: number;
}

interface DailyCacheHitPoint {
  date: string;
  promptTokens: number;
  cacheReadTokens: number;
  hitRate: number;
}

type TrendSeriesKey =
  | "input_tokens"
  | "output_tokens"
  | "cache_read_tokens"
  | "cache_creation_tokens";
type TrendPattern = "diagonal" | "dots" | "horizontal" | "cross";

interface TrendLegendItem {
  key: TrendSeriesKey;
  color: string;
  label: string;
  pattern: TrendPattern;
}

const analyticsCache = new Map<Period, CachedAnalyticsEntry>();
const ANALYTICS_CACHE_TTL = 5 * 60_000;
const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];
const TREND_PLOT_HEIGHT_PX = 144;
const MODEL_PALETTES: Record<string, string[]> = {
  Claude: ["#f59e0b", "#fbbf24", "#fb7185", "#f97316"],
  Codex: ["#22c55e", "#14b8a6", "#06b6d4", "#0ea5e9"],
  Gemini: ["#60a5fa", "#818cf8", "#a855f7", "#38bdf8"],
  default: ["#94a3b8", "#c084fc", "#fb7185", "#2dd4bf"],
};
const HEATMAP_COLORS = [
  "rgba(148,163,184,0.08)",
  "rgba(14,165,233,0.24)",
  "rgba(34,197,94,0.38)",
  "rgba(245,158,11,0.52)",
  "rgba(249,115,22,0.72)",
];
const DAILY_TREND_CHART_HEIGHT_PX = 160;
const DAILY_CACHE_HIT_CHART_HEIGHT_PX = 152;

function formatTokens(value: number): string {
  if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
  if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
  if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
  return String(value);
}

function formatCost(value: number): string {
  if (value >= 100) return `$${value.toFixed(0)}`;
  if (value >= 1) return `$${value.toFixed(2)}`;
  if (value >= 0.01) return `$${value.toFixed(3)}`;
  return `$${value.toFixed(4)}`;
}

function formatPercentage(value: number): string {
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

function modelColor(provider: string, index: number): string {
  const palette = MODEL_PALETTES[provider] ?? MODEL_PALETTES.default;
  return palette[index % palette.length];
}

function buildModelSegments(
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
    provider: model.provider,
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

function buildDonutBackground(segments: ModelSegment[]): string {
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

function buildWeekLabels(
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

function periodLabel(period: Period, t: TFunction): string {
  switch (period) {
    case "7d":
      return t({ ko: "7일", en: "7d", ja: "7日", zh: "7天" });
    case "90d":
      return t({ ko: "90일", en: "90d", ja: "90日", zh: "90天" });
    default:
      return t({ ko: "30일", en: "30d", ja: "30日", zh: "30天" });
  }
}

function patternFillStyle(color: string, pattern: TrendPattern): CSSProperties {
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

function ChartTooltip({
  lines,
  placement = "top",
}: {
  lines: string[];
  placement?: "top" | "bottom";
}) {
  return (
    <div
      className={`dash-chart-tooltip dash-card-pad-compact ${placement === "bottom" ? "dash-chart-tooltip-bottom" : ""}`}
    >
      <div className="space-y-1 text-[11px] leading-4">
        {lines.map((line, index) => (
          <div key={`${index}-${line}`}>{line}</div>
        ))}
      </div>
    </div>
  );
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

export default function TokenAnalyticsSection({
  agents,
  t,
  numberFormatter,
}: TokenAnalyticsSectionProps) {
  const [period, setPeriod] = useState<Period>("30d");
  const [data, setData] = useState<TokenAnalyticsResponse | null>(null);
  const [cards, setCards] = useState<KanbanCard[]>([]);
  const [loading, setLoading] = useState(false);
  const [cardsLoading, setCardsLoading] = useState(false);

  useEffect(() => {
    let activeController: AbortController | null = null;

    const load = async () => {
      const cached = analyticsCache.get(period);
      const fresh = cached
        ? Date.now() - cached.fetchedAt < ANALYTICS_CACHE_TTL
        : false;
      if (cached) setData(cached.data);
      if (fresh) return;

      activeController?.abort();
      const controller = new AbortController();
      activeController = controller;
      setLoading(true);
      try {
        const next = await getTokenAnalytics(period, { signal: controller.signal });
        if (controller.signal.aborted) return;
        analyticsCache.set(period, { data: next, fetchedAt: Date.now() });
        setData(next);
      } catch {
        if (controller.signal.aborted) return;
        // Ignore transient dashboard fetch failures and keep cached state.
      } finally {
        if (!controller.signal.aborted) setLoading(false);
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      activeController?.abort();
      clearInterval(timer);
    };
  }, [period]);

  useEffect(() => {
    let mounted = true;

    const loadCards = async () => {
      if (mounted) setCardsLoading(true);
      try {
        const next = await getKanbanCards();
        if (mounted) setCards(next);
      } catch {
        // Ignore transient dashboard fetch failures and keep previous state.
      } finally {
        if (mounted) setCardsLoading(false);
      }
    };

    void loadCards();
    const timer = setInterval(() => void loadCards(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  const segments = useMemo(
    () => buildModelSegments(data?.receipt.models ?? []),
    [data],
  );
  const donutBackground = useMemo(
    () => buildDonutBackground(segments),
    [segments],
  );
  const weekLabels = useMemo(
    () => buildWeekLabels(data?.heatmap ?? []),
    [data],
  );
  const trendMax = useMemo(
    () => Math.max(1, ...(data?.daily.map((day) => day.total_tokens) ?? [1])),
    [data],
  );
  const topAgents = useMemo(
    () =>
      [...(data?.receipt.agents ?? [])]
        .sort((a, b) => b.cost - a.cost)
        .slice(0, 8),
    [data],
  );
  const cacheSummary = useMemo(() => {
    const models = data?.receipt.models ?? [];
    const inputTokens = models.reduce(
      (sum, model) => sum + model.input_tokens,
      0,
    );
    const cacheReadTokens = models.reduce(
      (sum, model) => sum + model.cache_read_tokens,
      0,
    );
    const cacheCreationTokens = models.reduce(
      (sum, model) => sum + model.cache_creation_tokens,
      0,
    );
    const costWithoutCache = data?.receipt.subtotal ?? 0;
    const actualCost = data?.receipt.total ?? 0;
    return {
      promptTokens: inputTokens + cacheReadTokens + cacheCreationTokens,
      cacheReadTokens,
      actualCost,
      costWithoutCache,
      savings: Math.max(costWithoutCache - actualCost, 0),
      savingsRate: cacheSavingsRatePct(costWithoutCache, actualCost),
      hitRate: cacheHitRatePct(
        inputTokens,
        cacheReadTokens,
        cacheCreationTokens,
      ),
    };
  }, [data]);
  const agentCacheRows = useMemo(
    () => buildAgentCacheRows(data?.receipt.agents ?? []),
    [data],
  );
  const dailyCacheHitPoints = useMemo(
    () => buildDailyCacheHitPoints(data?.daily ?? []),
    [data],
  );
  const roiRows = useMemo(
    () =>
      buildAgentRoiRows({
        cards,
        agentShares: data?.receipt.agents ?? [],
        agents,
        periodStart: data?.receipt.period_start,
        periodEnd: data?.receipt.period_end,
      }).slice(0, 8),
    [agents, cards, data],
  );

  return (
    <section className="min-w-0 max-w-full space-y-4 overflow-hidden">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="min-w-0">
          <h2
            className="text-lg font-semibold"
            style={{ color: "var(--th-text-heading)" }}
          >
            {t({
              ko: "토큰 컨트롤 센터",
              en: "Token Control Center",
              ja: "トークンコントロールセンター",
              zh: "Token 控制中心",
            })}
          </h2>
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "13주 활동 히트맵, 비용 흐름, 모델 분포를 한 섹션에서 봅니다",
              en: "Keep the 13-week heatmap, spend flow, and model mix in one section",
              ja: "13週ヒートマップ、コスト推移、モデル分布を一つのセクションに集約します",
              zh: "在一个专区中查看 13 周热力图、成本走势与模型分布",
            })}
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {PERIOD_OPTIONS.map((option) => (
            <button
              key={option}
              type="button"
              onClick={() => setPeriod(option)}
              className={dashboardButton.sm}
              style={
                period === option
                  ? {
                      color: "#0f172a",
                      background: "linear-gradient(135deg, #f59e0b, #fb7185)",
                    }
                  : {
                      color: "var(--th-text-muted)",
                      border: "1px solid rgba(255,255,255,0.08)",
                    }
              }
            >
              {periodLabel(option, t)}
            </button>
          ))}
          {loading ? (
            <LoadingIndicator
              compact
              label={t({
                ko: "토큰 분석을 동기화하는 중입니다",
                en: "Syncing token analytics",
                ja: "トークン分析を同期中",
                zh: "正在同步 Token 分析",
              })}
            />
          ) : null}
        </div>
      </div>

      <div className="space-y-4">
        <div
          className={dashboardCard.accentHero}
          style={{
            borderColor: "rgba(245,158,11,0.22)",
            background:
              "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #f97316 10%), var(--th-surface))",
          }}
        >
          <div className="grid gap-3 sm:grid-cols-2 xl:grid-cols-4">
            <MetricCard
              label={t({
                ko: "총 토큰",
                en: "Total Tokens",
                ja: "総トークン",
                zh: "总代币",
              })}
              value={data ? formatTokens(data.summary.total_tokens) : "…"}
              sub={
                data
                  ? data.period_label
                  : t({
                      ko: "로딩 중",
                      en: "Loading",
                      ja: "読み込み中",
                      zh: "加载中",
                    })
              }
              accent="#f59e0b"
            />
            <MetricCard
              label={t({
                ko: "API 비용",
                en: "API Spend",
                ja: "API コスト",
                zh: "API 成本",
              })}
              value={data ? formatCost(data.summary.total_cost) : "…"}
              sub={data ? `-${formatCost(data.summary.cache_discount)}` : ""}
              accent="#22c55e"
            />
            <MetricCard
              label={t({
                ko: "활성 일수",
                en: "Active Days",
                ja: "稼働日数",
                zh: "活跃天数",
              })}
              value={
                data ? numberFormatter.format(data.summary.active_days) : "…"
              }
              sub={
                data
                  ? `${formatTokens(data.summary.average_daily_tokens)} / day`
                  : ""
              }
              accent="#38bdf8"
            />
            <MetricCard
              label={t({
                ko: "피크 데이",
                en: "Peak Day",
                ja: "ピーク日",
                zh: "峰值日",
              })}
              value={data?.summary.peak_day?.date.slice(5) ?? "—"}
              sub={
                data?.summary.peak_day
                  ? formatTokens(data.summary.peak_day.total_tokens)
                  : ""
              }
              accent="#fb7185"
            />
          </div>
        </div>

        <CacheEfficiencyCard
          t={t}
          loading={loading}
          actualCost={cacheSummary.actualCost}
          cacheReadTokens={cacheSummary.cacheReadTokens}
          hitRate={cacheSummary.hitRate}
          promptTokens={cacheSummary.promptTokens}
          savings={cacheSummary.savings}
          savingsRate={cacheSummary.savingsRate}
          uncachedCost={cacheSummary.costWithoutCache}
        />

        <div
          className={dashboardCard.standard}
          style={{
            borderColor: "var(--th-border)",
            background: "var(--th-surface)",
          }}
        >
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h3
                className="text-sm font-semibold"
                style={{ color: "var(--th-text)" }}
              >
                {t({
                  ko: "토큰 활동 히트맵",
                  en: "Token Activity Heatmap",
                  ja: "トークン活動ヒートマップ",
                  zh: "Token 活动热力图",
                })}
              </h3>
              <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                {t({
                  ko: "최근 13주를 GitHub 스타일로 표시합니다",
                  en: "Last 13 weeks in a GitHub-style grid",
                  ja: "直近 13 週間を GitHub スタイルのグリッドで表示します",
                  zh: "用 GitHub 风格网格展示最近 13 周",
                })}
              </p>
            </div>
            <div className="flex items-center gap-1">
              {HEATMAP_COLORS.map((color, index) => (
                <span
                  key={color}
                  className="h-2.5 w-2.5 rounded-[4px] border"
                  style={{
                    background: color,
                    borderColor:
                      index === 0 ? "rgba(148,163,184,0.12)" : "transparent",
                  }}
                />
              ))}
            </div>
          </div>

          {!data ? (
            <div
              className="py-12 text-center text-sm"
              style={{ color: "var(--th-text-muted)" }}
            >
              {t({
                ko: "토큰 분석을 불러오는 중입니다",
                en: "Loading token analytics",
                ja: "トークン分析を読み込み中",
                zh: "正在加载 Token 分析",
              })}
            </div>
          ) : (
            <div className="mt-4">
              <div className="min-w-0 overflow-x-auto">
                <div
                  className="mb-2 ml-9 grid grid-cols-13 gap-1 text-[10px]"
                  style={{
                    color: "var(--th-text-muted)",
                    minWidth: "min-content",
                  }}
                >
                  {weekLabels.map((item) => (
                    <span key={item.week} className="truncate">
                      {item.label}
                    </span>
                  ))}
                </div>

                <div className="flex gap-3" style={{ minWidth: "min-content" }}>
                  <div
                    className="grid shrink-0 grid-rows-7 gap-1 pt-1 text-[10px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {["M", "", "W", "", "F", "", "S"].map((label, index) => (
                      <span key={`${label}-${index}`} className="h-3 leading-3">
                        {label}
                      </span>
                    ))}
                  </div>

                  <div className="grid min-w-0 grid-flow-col grid-rows-7 gap-1">
                    {data.heatmap.map((cell, idx) => (
                      <div
                        key={cell.date}
                        className="group relative h-3 w-3 outline-none"
                        role="img"
                        tabIndex={0}
                        aria-label={`${cell.date}, ${formatTokens(cell.total_tokens)} tokens, ${formatCost(cell.cost)}`}
                      >
                        <ChartTooltip
                          lines={[
                            cell.date,
                            `${formatTokens(cell.total_tokens)} tokens`,
                            formatCost(cell.cost),
                          ]}
                          placement={idx % 7 < 2 ? "bottom" : "top"}
                        />
                        <span
                          className="block h-3 w-3 rounded-[4px] border transition-transform group-hover:scale-110 group-focus-within:scale-110"
                          style={{
                            background: cell.future
                              ? "rgba(148,163,184,0.04)"
                              : (HEATMAP_COLORS[cell.level] ??
                                HEATMAP_COLORS[0]),
                            borderColor: cell.future
                              ? "rgba(148,163,184,0.05)"
                              : "rgba(255,255,255,0.04)",
                            opacity: cell.future ? 0.35 : 1,
                          }}
                        />
                      </div>
                    ))}
                  </div>
                </div>
              </div>
            </div>
          )}
        </div>

        <DailyTrendCard
          daily={data?.daily ?? []}
          trendMax={trendMax}
          t={t}
          loading={loading}
        />
        <DailyCacheHitTrendCard
          daily={dailyCacheHitPoints}
          t={t}
          loading={loading}
        />
      </div>

      <div className="grid gap-4 lg:grid-cols-[minmax(0,0.9fr)_minmax(0,1.1fr)]">
        <ModelDistributionCard
          t={t}
          segments={segments}
          donutBackground={donutBackground}
          totalTokens={data?.summary.total_tokens ?? 0}
          loading={loading}
        />
        <AgentSpendCard
          t={t}
          agents={topAgents}
          numberFormatter={numberFormatter}
          loading={loading}
        />
      </div>

      <AgentCacheHitCard t={t} rows={agentCacheRows} loading={loading} />

      <AgentRoiCard
        t={t}
        rows={roiRows}
        loading={loading || cardsLoading}
        numberFormatter={numberFormatter}
      />
    </section>
  );
}

function CacheEfficiencyCard({
  t,
  loading,
  actualCost,
  uncachedCost,
  savings,
  savingsRate,
  hitRate,
  cacheReadTokens,
  promptTokens,
}: {
  t: TFunction;
  loading: boolean;
  actualCost: number;
  uncachedCost: number;
  savings: number;
  savingsRate: number;
  hitRate: number;
  cacheReadTokens: number;
  promptTokens: number;
}) {
  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "color-mix(in srgb, #22c55e 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-surface) 93%, #22c55e 7%) 0%, var(--th-surface) 100%)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "캐시 절감 요약",
              en: "Cache Savings Summary",
              ja: "キャッシュ節約サマリー",
              zh: "缓存节省摘要",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "uncached 기준 비용과 실제 결제 비용 차이를 한눈에 봅니다",
              en: "See the gap between uncached baseline and actual billed spend",
              ja: "キャッシュなし基準コストと実課金コストの差をひと目で確認します",
              zh: "一眼查看未缓存基线成本与实际计费成本的差值",
            })}
          </p>
        </div>
        {loading ? (
          <LoadingIndicator
            compact
            label={t({
              ko: "캐시 절감 요약 갱신 중",
              en: "Refreshing cache savings summary",
              ja: "キャッシュ節約サマリーを更新中",
              zh: "刷新缓存节省摘要中",
            })}
          />
        ) : null}
      </div>

      <div className="mt-4 grid gap-3 sm:grid-cols-3">
        <MetricCard
          label={t({
            ko: "절감액",
            en: "Savings",
            ja: "節約額",
            zh: "节省金额",
          })}
          value={formatCost(savings)}
          sub={t({
            ko: `${formatCost(actualCost)} actual spend`,
            en: `${formatCost(actualCost)} actual spend`,
            ja: `${formatCost(actualCost)} actual spend`,
            zh: `${formatCost(actualCost)} actual spend`,
          })}
          accent="#22c55e"
        />
        <MetricCard
          label={t({
            ko: "절감율",
            en: "Savings Rate",
            ja: "節約率",
            zh: "节省率",
          })}
          value={formatPercentage(savingsRate)}
          sub={t({
            ko: `${formatCost(uncachedCost)} uncached baseline`,
            en: `${formatCost(uncachedCost)} uncached baseline`,
            ja: `${formatCost(uncachedCost)} uncached baseline`,
            zh: `${formatCost(uncachedCost)} uncached baseline`,
          })}
          accent="#14b8a6"
        />
        <MetricCard
          label={t({
            ko: "캐시 히트율",
            en: "Cache Hit Rate",
            ja: "キャッシュヒット率",
            zh: "缓存命中率",
          })}
          value={formatPercentage(hitRate)}
          sub={t({
            ko: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
            en: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
            ja: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
            zh: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
          })}
          accent="#f59e0b"
        />
      </div>
    </div>
  );
}

function LoadingIndicator({
  label,
  compact = false,
}: {
  label: string;
  compact?: boolean;
}) {
  return (
    <span
      role="status"
      aria-label={label}
      title={label}
      className={`inline-flex items-center justify-center rounded-full border ${compact ? "h-6 w-6" : "h-8 w-8"}`}
      style={{
        color: "#f59e0b",
        background: "rgba(245,158,11,0.12)",
        borderColor: "rgba(245,158,11,0.24)",
      }}
    >
      <span
        className={`${compact ? "h-3 w-3" : "h-3.5 w-3.5"} inline-block animate-spin rounded-full border-2 border-current border-t-transparent`}
      />
    </span>
  );
}

function MetricCard({
  label,
  value,
  sub,
  accent,
}: {
  label: string;
  value: string;
  sub: string;
  accent: string;
}) {
  return (
    <div
      className={dashboardCard.nested}
      style={{
        borderColor: `${accent}26`,
        background: "rgba(15,23,42,0.16)",
      }}
    >
      <div
        className="text-[11px] font-semibold uppercase tracking-[0.18em]"
        style={{ color: "var(--th-text-muted)" }}
      >
        {label}
      </div>
      <div
        className="mt-2 text-2xl font-black tracking-tight"
        style={{ color: accent }}
      >
        {value}
      </div>
      <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
        {sub}
      </div>
    </div>
  );
}

function DailyTrendCard({
  daily,
  trendMax,
  t,
  loading,
}: {
  daily: TokenAnalyticsDailyPoint[];
  trendMax: number;
  t: TFunction;
  loading: boolean;
}) {
  const legend: TrendLegendItem[] = [
    {
      key: "input_tokens",
      color: "#38bdf8",
      pattern: "diagonal",
      label: t({ ko: "입력", en: "Input", ja: "入力", zh: "输入" }),
    },
    {
      key: "output_tokens",
      color: "#f97316",
      pattern: "dots",
      label: t({ ko: "출력", en: "Output", ja: "出力", zh: "输出" }),
    },
    {
      key: "cache_read_tokens",
      color: "#22c55e",
      pattern: "horizontal",
      label: t({
        ko: "캐시 읽기",
        en: "Cache Read",
        ja: "キャッシュ読取",
        zh: "缓存读取",
      }),
    },
    {
      key: "cache_creation_tokens",
      color: "#a855f7",
      pattern: "cross",
      label: t({
        ko: "캐시 쓰기",
        en: "Cache Write",
        ja: "キャッシュ書込",
        zh: "缓存写入",
      }),
    },
  ];
  const hasData = hasDailyTrendData(daily);
  const labelStride = Math.max(1, Math.ceil(daily.length / 6));

  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "일별 토큰 추이",
              en: "Daily Token Trend",
              ja: "日次トークン推移",
              zh: "每日 Token 趋势",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "입력, 출력, 캐시 읽기/쓰기를 스택 바 형태로 봅니다",
              en: "Stacked bars for input, output, cache read, and cache write",
              ja: "入力・出力・キャッシュ読取/書込を積み上げバーで表示します",
              zh: "用堆叠柱显示输入、输出、缓存读写",
            })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2 text-[11px]">
          {legend.map((item) => (
            <span
              key={item.key}
              className="inline-flex items-center gap-1.5"
              style={{ color: "var(--th-text-muted)" }}
            >
              <span
                className="h-3.5 w-3.5 rounded-lg border"
                style={{
                  ...patternFillStyle(item.color, item.pattern),
                  borderColor: "rgba(255,255,255,0.12)",
                }}
              />
              {item.label}
            </span>
          ))}
          {loading ? (
            <LoadingIndicator
              compact
              label={t({
                ko: "일별 토큰 추이 갱신 중",
                en: "Refreshing daily token trend",
                ja: "日次トークン推移を更新中",
                zh: "刷新每日 Token 趋势",
              })}
            />
          ) : null}
        </div>
      </div>

      {!hasData ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "토큰 추이를 동기화하는 중입니다",
                en: "Syncing token trend",
                ja: "トークン推移を同期中",
                zh: "正在同步 Token 趋势",
              })
            : t({
                ko: "표시할 토큰 추이가 없습니다",
                en: "No token trend to show",
                ja: "表示する推移がありません",
                zh: "暂无可显示的趋势",
              })}
        </div>
      ) : (
        <div
          className="mt-4"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          <div className="min-w-0 overflow-x-auto overflow-y-visible">
            <div
              className="flex h-44 items-end gap-px sm:gap-1.5"
              style={{ minWidth: "min-content" }}
            >
              {daily.map((day, index) => {
                const segments = legend
                  .map((item) => ({
                    ...item,
                    value: day[item.key],
                  }))
                  .filter((segment) => segment.value > 0);
                const totalHeight = Math.max(
                  8,
                  Math.round(
                    (day.total_tokens / trendMax) * TREND_PLOT_HEIGHT_PX,
                  ),
                );
                const breakdown = segments.map(
                  (segment) =>
                    `${segment.label} ${formatTokens(segment.value)}`,
                );
                const compactLabel =
                  index === 0 ||
                  index === daily.length - 1 ||
                  index % labelStride === 0;

                return (
                  <div
                    key={day.date}
                    className="group relative flex w-2 shrink-0 flex-col items-center gap-1 outline-none sm:min-w-0 sm:flex-1 sm:shrink sm:gap-2"
                    tabIndex={0}
                    role="img"
                    aria-label={[
                      day.date,
                      `${formatTokens(day.total_tokens)} total tokens`,
                      formatCost(day.cost),
                      ...breakdown,
                    ].join(", ")}
                  >
                    <ChartTooltip
                      lines={[
                        day.date,
                        `${formatTokens(day.total_tokens)} tokens`,
                        formatCost(day.cost),
                        ...breakdown,
                      ]}
                    />
                    <div className="flex h-36 w-full items-end">
                      <div
                        className="flex w-full min-w-[6px] flex-col-reverse overflow-hidden rounded-t-xl border sm:min-w-[10px]"
                        style={{
                          height: totalHeight,
                          minHeight: 8,
                          maxHeight: TREND_PLOT_HEIGHT_PX,
                          borderColor: "rgba(255,255,255,0.06)",
                          background: "rgba(255,255,255,0.03)",
                        }}
                      >
                        {segments.map((segment, index) => (
                          <div
                            key={`${day.date}-${segment.color}-${index}`}
                            style={{
                              height: `${(segment.value / day.total_tokens) * 100}%`,
                              ...patternFillStyle(
                                segment.color,
                                segment.pattern,
                              ),
                            }}
                          />
                        ))}
                      </div>
                    </div>
                    <span
                      className="min-h-[1.8rem] text-center text-[9px] leading-3 sm:min-h-0 sm:text-[10px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      <span className="hidden sm:inline">
                        {day.date.slice(5)}
                      </span>
                      <span className="sm:hidden">
                        {compactLabel ? day.date.slice(5) : ""}
                      </span>
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function DailyCacheHitTrendCard({
  daily,
  t,
  loading,
}: {
  daily: DailyCacheHitPoint[];
  t: TFunction;
  loading: boolean;
}) {
  const hasData = daily.some((day) => day.promptTokens > 0);
  const labelStride = Math.max(1, Math.ceil(daily.length / 6));
  const totalPromptTokens = daily.reduce(
    (sum, day) => sum + day.promptTokens,
    0,
  );
  const weightedHitRate =
    totalPromptTokens > 0
      ? daily.reduce((sum, day) => sum + day.hitRate * day.promptTokens, 0) /
        totalPromptTokens
      : 0;

  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "일별 캐시 히트율",
              en: "Daily Cache Hit Rate",
              ja: "日次キャッシュヒット率",
              zh: "每日缓存命中率",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "일자별 prompt 토큰 중 캐시로 처리된 비중을 추적합니다",
              en: "Track how much of prompt traffic was served from cache each day",
              ja: "日ごとの prompt トークンのうちキャッシュで処理された比率を追跡します",
              zh: "追踪每天 prompt Token 中由缓存提供的占比",
            })}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span
            className={dashboardBadge.large}
            style={{ color: "#22c55e", background: "rgba(34,197,94,0.12)" }}
          >
            {formatPercentage(weightedHitRate)}
          </span>
          {loading ? (
            <LoadingIndicator
              compact
              label={t({
                ko: "일별 캐시 히트율 갱신 중",
                en: "Refreshing daily cache hit rate",
                ja: "日次キャッシュヒット率を更新中",
                zh: "刷新每日缓存命中率中",
              })}
            />
          ) : null}
        </div>
      </div>

      {!hasData ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "캐시 히트율을 동기화하는 중입니다",
                en: "Syncing cache hit rate",
                ja: "キャッシュヒット率を同期中",
                zh: "正在同步缓存命中率",
              })
            : t({
                ko: "표시할 캐시 히트율 데이터가 없습니다",
                en: "No cache hit data to show",
                ja: "表示するキャッシュヒット率データがありません",
                zh: "暂无可显示的缓存命中数据",
              })}
        </div>
      ) : (
        <div
          className="mt-4"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          <div className="min-w-0 overflow-x-auto overflow-y-visible">
            <div
              className="flex h-44 items-end gap-px sm:gap-1.5"
              style={{ minWidth: "min-content" }}
            >
              {daily.map((day, index) => {
                const height =
                  day.hitRate <= 0
                    ? 0
                    : Math.max(
                        8,
                        Math.round(
                          (day.hitRate / 100) * DAILY_CACHE_HIT_CHART_HEIGHT_PX,
                        ),
                      );
                const compactLabel =
                  index === 0 ||
                  index === daily.length - 1 ||
                  index % labelStride === 0;

                return (
                  <div
                    key={day.date}
                    className="group relative flex w-2 shrink-0 flex-col items-center gap-1 outline-none sm:min-w-0 sm:flex-1 sm:shrink sm:gap-2"
                    tabIndex={0}
                    role="img"
                    aria-label={[
                      day.date,
                      `${formatPercentage(day.hitRate)} cache hit rate`,
                      `${formatTokens(day.cacheReadTokens)} cache read tokens`,
                      `${formatTokens(day.promptTokens)} prompt tokens`,
                    ].join(", ")}
                  >
                    <ChartTooltip
                      lines={[
                        day.date,
                        `${formatPercentage(day.hitRate)} cache hit rate`,
                        `${formatTokens(day.cacheReadTokens)} cache read tokens`,
                        `${formatTokens(day.promptTokens)} prompt tokens`,
                      ]}
                    />
                    <div className="flex h-36 w-full items-end">
                      <div
                        className="w-full min-w-[6px] rounded-t-xl border sm:min-w-[10px]"
                        style={{
                          height,
                          minHeight: height > 0 ? 8 : 0,
                          maxHeight: DAILY_CACHE_HIT_CHART_HEIGHT_PX,
                          borderColor: "rgba(255,255,255,0.06)",
                          background:
                            "linear-gradient(180deg, #22c55e 0%, #14b8a6 52%, #38bdf8 100%)",
                          opacity: day.promptTokens > 0 ? 1 : 0.24,
                        }}
                      />
                    </div>
                    <span
                      className="min-h-[1.8rem] text-center text-[9px] leading-3 sm:min-h-0 sm:text-[10px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      <span className="hidden sm:inline">
                        {day.date.slice(5)}
                      </span>
                      <span className="sm:hidden">
                        {compactLabel ? day.date.slice(5) : ""}
                      </span>
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function ModelDistributionCard({
  t,
  segments,
  donutBackground,
  totalTokens,
  loading,
}: {
  t: TFunction;
  segments: ModelSegment[];
  donutBackground: string;
  totalTokens: number;
  loading: boolean;
}) {
  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "모델 분포",
              en: "Model Distribution",
              ja: "モデル分布",
              zh: "模型分布",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "Claude/Codex 모델이 토큰을 어떻게 나눠 쓰는지 확인합니다",
              en: "See how Claude and Codex models split token volume",
              ja: "Claude/Codex モデルのトークン構成を確認します",
              zh: "查看 Claude/Codex 模型如何分摊 Token 量",
            })}
          </p>
        </div>
        <div className="flex items-center gap-2">
          <span
            className={dashboardBadge.large}
            style={{ color: "#f59e0b", background: "rgba(245,158,11,0.12)" }}
          >
            {formatTokens(totalTokens)}
          </span>
          {loading ? (
            <LoadingIndicator
              compact
              label={t({
                ko: "모델 분포 갱신 중",
                en: "Refreshing model distribution",
                ja: "モデル分布を更新中",
                zh: "刷新模型分布中",
              })}
            />
          ) : null}
        </div>
      </div>

      {segments.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "모델 분포를 동기화하는 중입니다",
                en: "Syncing model distribution",
                ja: "モデル分布を同期中",
                zh: "正在同步模型分布",
              })
            : t({
                ko: "모델 분포 데이터가 없습니다",
                en: "No model distribution data",
                ja: "モデル分布データがありません",
                zh: "暂无模型分布数据",
              })}
        </div>
      ) : (
        <div
          className="mt-5 grid gap-5 md:grid-cols-[180px_minmax(0,1fr)] md:items-center"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          <div className="mx-auto flex w-full max-w-[180px] items-center justify-center">
            <div
              className="relative h-40 w-40 rounded-full"
              style={{ background: donutBackground }}
            >
              <div
                className="absolute inset-[18%] rounded-full border"
                style={{
                  background:
                    "color-mix(in srgb, var(--th-surface) 88%, #0f172a 12%)",
                  borderColor: "rgba(255,255,255,0.06)",
                }}
              />
              <div className="absolute inset-0 flex flex-col items-center justify-center text-center">
                <div
                  className="text-[11px] uppercase tracking-[0.18em]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  Mix
                </div>
                <div
                  className="mt-1 text-xl font-black"
                  style={{ color: "var(--th-text)" }}
                >
                  {segments.length}
                </div>
              </div>
            </div>
          </div>

          <div className="space-y-2">
            {segments.map((segment) => (
              <div
                key={segment.id}
                className={dashboardCard.nestedCompact}
                style={{
                  borderColor: "rgba(255,255,255,0.06)",
                  background: "var(--th-bg-surface)",
                }}
              >
                <div className="flex items-center justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2">
                      <span
                        className="h-2.5 w-2.5 rounded-full"
                        style={{ background: segment.color }}
                      />
                      <span
                        className="truncate text-sm font-semibold"
                        style={{ color: "var(--th-text)" }}
                      >
                        {segment.label}
                      </span>
                    </div>
                    <div
                      className="mt-1 text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {segment.provider}
                    </div>
                  </div>
                  <div className="text-right">
                    <div
                      className="text-sm font-bold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {segment.percentage.toFixed(1)}%
                    </div>
                    <div
                      className="text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {formatTokens(segment.tokens)}
                    </div>
                  </div>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

function AgentSpendCard({
  t,
  agents,
  numberFormatter,
  loading,
}: {
  t: TFunction;
  agents: ReceiptSnapshotAgentShare[];
  numberFormatter: Intl.NumberFormat;
  loading: boolean;
}) {
  const maxCost = Math.max(0.01, ...agents.map((agent) => agent.cost));

  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "에이전트별 비용 비교",
              en: "Agent Cost Comparison",
              ja: "エージェント別コスト比較",
              zh: "按代理比较成本",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "상위 에이전트의 토큰 소비와 비용을 함께 봅니다",
              en: "Compare token volume and spend for the busiest agents",
              ja: "主要エージェントのトークン量とコストを並べて確認します",
              zh: "对比主要代理的 Token 量与成本",
            })}
          </p>
        </div>
        {loading ? (
          <LoadingIndicator
            compact
            label={t({
              ko: "에이전트 비용 비교 갱신 중",
              en: "Refreshing agent cost comparison",
              ja: "エージェント別コスト比較を更新中",
              zh: "刷新代理成本比较中",
            })}
          />
        ) : null}
      </div>

      {agents.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "에이전트 사용량을 동기화하는 중입니다",
                en: "Syncing agent usage",
                ja: "エージェント使用量を同期中",
                zh: "正在同步代理使用量",
              })
            : t({
                ko: "에이전트 사용량 데이터가 없습니다",
                en: "No agent usage data",
                ja: "エージェント使用量データがありません",
                zh: "暂无代理使用数据",
              })}
        </div>
      ) : (
        <div
          className="mt-4 space-y-2.5"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          {agents.map((agent, index) => (
            <div
              key={agent.agent}
              className={dashboardCard.nested}
              style={{
                borderColor: "rgba(255,255,255,0.06)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span
                      className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold"
                      style={{
                        color: "#0f172a",
                        background: modelColor("default", index),
                      }}
                    >
                      {index + 1}
                    </span>
                    <span
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {agent.agent}
                    </span>
                  </div>
                  <div
                    className="mt-1 text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {formatTokens(agent.tokens)} tokens ·{" "}
                    {numberFormatter.format(
                      Math.round(agent.percentage * 10) / 10,
                    )}
                    %
                  </div>
                </div>
                <div className="text-right">
                  <div
                    className="text-sm font-bold"
                    style={{ color: "#22c55e" }}
                  >
                    {formatCost(agent.cost)}
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${Math.max(6, (agent.cost / maxCost) * 100)}%`,
                    background: "linear-gradient(90deg, #22c55e, #14b8a6)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function AgentCacheHitCard({
  t,
  rows,
  loading,
}: {
  t: TFunction;
  rows: AgentCacheRow[];
  loading: boolean;
}) {
  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "에이전트별 캐시 히트율",
              en: "Agent Cache Hit Rate",
              ja: "エージェント別キャッシュヒット率",
              zh: "按代理查看缓存命中率",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "prompt 볼륨이 큰 에이전트를 기준으로 캐시 효율을 비교합니다",
              en: "Compare cache efficiency across the busiest prompt-heavy agents",
              ja: "prompt ボリュームが大きいエージェントを基準にキャッシュ効率を比較します",
              zh: "以 prompt 体量较大的代理为基准比较缓存效率",
            })}
          </p>
        </div>
        {loading ? (
          <LoadingIndicator
            compact
            label={t({
              ko: "에이전트별 캐시 히트율 갱신 중",
              en: "Refreshing agent cache hit rate",
              ja: "エージェント別キャッシュヒット率を更新中",
              zh: "刷新代理缓存命中率中",
            })}
          />
        ) : null}
      </div>

      {rows.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {loading
            ? t({
                ko: "에이전트 캐시 히트율을 동기화하는 중입니다",
                en: "Syncing agent cache hit rate",
                ja: "エージェントキャッシュヒット率を同期中",
                zh: "正在同步代理缓存命中率",
              })
            : t({
                ko: "에이전트 캐시 데이터가 없습니다",
                en: "No agent cache data",
                ja: "エージェントキャッシュデータがありません",
                zh: "暂无代理缓存数据",
              })}
        </div>
      ) : (
        <div
          className="mt-4 space-y-2.5"
          style={{ opacity: loading ? 0.58 : 1 }}
        >
          {rows.map((row, index) => (
            <div
              key={row.id}
              className={dashboardCard.nested}
              style={{
                borderColor: "rgba(255,255,255,0.06)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span
                      className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold"
                      style={{
                        color: "#0f172a",
                        background: modelColor("Codex", index),
                      }}
                    >
                      {index + 1}
                    </span>
                    <span
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {row.label}
                    </span>
                  </div>
                  <div
                    className="mt-1 text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {formatTokens(row.cacheReadTokens)} cache reads ·{" "}
                    {formatCost(row.savings)} saved
                  </div>
                </div>
                <div className="text-right">
                  <div
                    className="text-sm font-bold"
                    style={{ color: "#22c55e" }}
                  >
                    {formatPercentage(row.hitRate)}
                  </div>
                  <div
                    className="text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {formatTokens(row.promptTokens)} prompt
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${row.hitRate > 0 ? Math.max(6, row.hitRate) : 0}%`,
                    background: "linear-gradient(90deg, #22c55e, #38bdf8)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function AgentRoiCard({
  t,
  rows,
  loading,
  numberFormatter,
}: {
  t: TFunction;
  rows: ReturnType<typeof buildAgentRoiRows>;
  loading: boolean;
  numberFormatter: Intl.NumberFormat;
}) {
  const maxScore = Math.max(
    0.01,
    ...rows.map((row) => row.cards_per_million_tokens),
  );

  return (
    <div
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "var(--th-border)",
        background: "var(--th-surface)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <TooltipLabel
            text={t({
              ko: "에이전트 ROI",
              en: "Agent ROI",
              ja: "エージェント ROI",
              zh: "代理 ROI",
            })}
            tooltip={t({
              ko: "선택 기간 동안 완료 카드 수를 토큰 소비량으로 나눈 값입니다. 카드 / 100만 토큰 기준으로 비교합니다.",
              en: "Completed cards divided by token usage in the selected window. Compared as cards per 1M tokens.",
              ja: "選択期間の完了カード数をトークン消費量で割った値です。100万トークンあたりのカード数で比較します。",
              zh: "按所选时间窗用完成卡片数除以 Token 消耗量，并以每 100 万 Token 的完成卡片数比较。",
            })}
            className="text-sm font-semibold"
          />
          <p className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "완료 카드 수와 토큰 사용량을 함께 보며 효율이 높은 담당자를 찾습니다",
              en: "Compare completed cards against token volume to spot efficient agents",
              ja: "完了カード数とトークン量を合わせて見て、効率の高い担当者を見つけます",
              zh: "将完成卡片数与 Token 用量一起比较，找出效率更高的代理",
            })}
          </p>
        </div>
        <span
          className="rounded-full px-3 py-1 text-xs font-semibold"
          style={{ color: "#38bdf8", background: "rgba(56,189,248,0.12)" }}
        >
          {numberFormatter.format(
            rows.reduce((sum, row) => sum + row.completed_cards, 0),
          )}{" "}
          {t({ ko: "완료", en: "done", ja: "完了", zh: "完成" })}
        </span>
      </div>

      {loading && rows.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {t({
            ko: "ROI 지표를 계산하는 중입니다",
            en: "Calculating ROI",
            ja: "ROI を計算中",
            zh: "正在计算 ROI",
          })}
        </div>
      ) : rows.length === 0 ? (
        <div
          className="py-10 text-center text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          {t({
            ko: "선택 기간에 계산할 ROI 데이터가 없습니다",
            en: "No ROI data for this window",
            ja: "この期間の ROI データがありません",
            zh: "当前时间窗暂无 ROI 数据",
          })}
        </div>
      ) : (
        <div className="mt-4 space-y-2.5">
          {rows.map((row, index) => (
            <div
              key={row.id}
              className="rounded-xl border px-3 py-3"
              style={{
                borderColor: "rgba(255,255,255,0.06)",
                background: "var(--th-bg-surface)",
              }}
            >
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span
                      className="flex h-6 w-6 items-center justify-center rounded-full text-xs font-bold"
                      style={{
                        color: "#0f172a",
                        background: modelColor("Codex", index),
                      }}
                    >
                      {index + 1}
                    </span>
                    <span
                      className="truncate text-sm font-semibold"
                      style={{ color: "var(--th-text)" }}
                    >
                      {row.label}
                    </span>
                  </div>
                  <div
                    className="mt-1 text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {numberFormatter.format(row.completed_cards)}{" "}
                    {t({ ko: "카드", en: "cards", ja: "カード", zh: "卡片" })}
                    {" · "}
                    {formatTokens(row.tokens)} tokens
                    {" · "}
                    {formatCost(row.cost)}
                  </div>
                </div>
                <div className="text-right">
                  <div
                    className="text-sm font-bold"
                    style={{ color: "#38bdf8" }}
                  >
                    {row.cards_per_million_tokens.toFixed(2)}
                  </div>
                  <div
                    className="text-[11px]"
                    style={{ color: "var(--th-text-muted)" }}
                  >
                    {t({
                      ko: "카드 / 1M",
                      en: "cards / 1M",
                      ja: "カード / 1M",
                      zh: "卡片 / 1M",
                    })}
                  </div>
                </div>
              </div>

              <div className="mt-3 h-2 rounded-full bg-slate-800/50">
                <div
                  className="h-full rounded-full"
                  style={{
                    width: `${Math.max(6, (row.cards_per_million_tokens / maxScore) * 100)}%`,
                    background: "linear-gradient(90deg, #38bdf8, #818cf8)",
                  }}
                />
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
