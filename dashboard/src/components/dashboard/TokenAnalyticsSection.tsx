import { useEffect, useMemo, useState } from "react";
import { getKanbanCards, getTokenAnalytics } from "../../api";
import type {
  Agent,
  KanbanCard,
  ReceiptSnapshotAgentShare,
  TokenAnalyticsDailyPoint,
  TokenAnalyticsResponse,
} from "../../types";
import type { TFunction } from "./model";
import { cx, dashboardBadge, dashboardButton, dashboardCard } from "./ui";
import TooltipLabel from "../common/TooltipLabel";
import { buildAgentRoiRows } from "./dashboardInsights";
import {
  CacheEfficiencyCard,
  LoadingIndicator,
  MetricCard,
} from "./TokenAnalyticsCards";
import {
  ChartTooltip,
  DailyCacheHitTrendCard,
  DailyTrendCard,
} from "./TokenAnalyticsTrendCards";
import { ModelDistributionCard } from "./TokenAnalyticsDistributionCard";
import {
  AgentCacheHitCard,
  AgentRoiCard,
  AgentSpendCard,
} from "./TokenAnalyticsAgentCards";
import {
  ANALYTICS_CACHE_TTL,
  DAILY_CACHE_HIT_CHART_HEIGHT_PX,
  HEATMAP_COLORS,
  PERIOD_OPTIONS,
  TREND_PLOT_HEIGHT_PX,
  buildAgentCacheRows,
  buildDailyCacheHitPoints,
  buildDonutBackground,
  buildModelSegments,
  buildWeekLabels,
  cacheHitRatePct,
  cacheSavingsRatePct,
  formatCost,
  formatPercentage,
  formatTokens,
  hasDailyTrendData,
  modelColor,
  patternFillStyle,
  periodLabel,
  type AgentCacheRow,
  type CachedAnalyticsEntry,
  type DailyCacheHitPoint,
  type ModelSegment,
  type Period,
  type TrendLegendItem,
} from "./tokenAnalyticsModels";

export {
  buildAgentCacheRows,
  buildDailyCacheHitPoints,
  buildModelSegments,
  cacheHitRatePct,
  cacheSavingsRatePct,
  dailyTrendBarHeightPx,
  hasDailyTrendData,
} from "./tokenAnalyticsModels";

interface TokenAnalyticsSectionProps {
  agents: Agent[];
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
}

const analyticsCache = new Map<Period, CachedAnalyticsEntry>();

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
