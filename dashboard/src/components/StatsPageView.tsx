import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type CSSProperties,
} from "react";
import {
  getCachedTokenAnalytics,
  getSkillCatalog,
  getSkillRanking,
  getTokenAnalytics,
  type SkillRankingResponse,
} from "../api";
import { cx } from "./dashboard/ui";
import type { TFunction } from "./dashboard/model";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
  SkillCatalogEntry,
  TokenAnalyticsResponse,
} from "../types";
import {
  buildAgentCacheRows,
  buildAgentSkillRows,
  buildAgentSpendRows,
  buildLeaderboardRows,
  buildModelSegments,
  buildProviderSegments,
  buildSavingsDelta,
  buildSkillRows,
  buildWindowDelta,
  computeCacheHitRate,
  computeDailyHitRate,
  dailySeries,
  formatDateLabel,
  formatPercent,
  formatTokens,
  msg,
  periodDayCount,
  resolveLocaleTag,
  type Period,
} from "./stats/statsModel";
import {
  AgentCacheCard,
  AgentLeaderboardCard,
  AgentSpendCard,
  DailyCacheHitCard,
  DailyTokenCompositionCard,
  ModelDistributionCard,
  ProviderDistributionCard,
  SkillUsageCard,
  StatsSummaryGrid,
} from "./stats/StatsCards";
import {
  readPersistedAnalytics,
  readPersistedSkillRanking,
  writePersistedAnalytics,
  writePersistedSkillRanking,
} from "./stats/statsStorage";
import { STATS_SHELL_STYLES } from "./stats/statsStyles";
import {
  RefreshCw,
  ShieldAlert,
} from "lucide-react";
import { AgentQualityWidget } from "./dashboard/ExtraWidgets";
import ReceiptWidget from "./dashboard/ReceiptWidget";

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

const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];

const NUMERIC_STYLE: CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
};

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
      // SWR fast-path (#1250): hydrate from in-memory cache, then fall back to
      // sessionStorage so the *first* tab entry after a reload still paints
      // instantly instead of showing every "...불러오는 중" placeholder.
      const cachedAnalytics = getCachedTokenAnalytics(period);
      if (cachedAnalytics) {
        setAnalytics(cachedAnalytics.data);
      } else {
        const persisted = readPersistedAnalytics(period);
        if (persisted) setAnalytics(persisted);
      }
      const persistedRanking = readPersistedSkillRanking(period);
      if (persistedRanking) setSkillRanking(persistedRanking);

      setLoading(!cachedAnalytics && readPersistedAnalytics(period) === null);
      setSkillLoading(persistedRanking === null);
      setAnalyticsError(null);
      setSkillError(null);

      // The Refresh button increments `reloadKey`. When it's > 0 we treat
      // the fetch as user-initiated and bypass the browser cache (the
      // backend now ships SWR Cache-Control on the analytics endpoint, so
      // a default re-entry would otherwise be served by the browser cache).
      const forceRefresh = reloadKey > 0;
      const [analyticsResult, skillResult] = await Promise.allSettled([
        getTokenAnalytics(period, { signal: controller.signal, forceRefresh }),
        getSkillRanking(period, 16),
      ]);
      if (!active) return;

      if (analyticsResult.status === "fulfilled") {
        setAnalytics(analyticsResult.value);
        writePersistedAnalytics(period, analyticsResult.value);
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
        writePersistedSkillRanking(period, skillResult.value);
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

          <StatsSummaryGrid
            t={t}
            numberFormatter={numberFormatter}
            rangeDays={rangeDays}
            summary={summary}
            tokenMomentumDelta={tokenMomentumDelta}
            cacheSavingsDelta={cacheSavingsDelta}
          />

          {/* Codex review (PR #1258): grid-feature uses 2fr 1fr on desktop;
              after hiding DailyCacheHitCard the second column was empty.
              Switch to a single-column container so the chart spans the
              row. Re-introduce grid-feature when the cache card returns. */}
          <div data-testid="stats-daily-token-chart">
            <DailyTokenCompositionCard
              t={t}
              localeTag={localeTag}
              loading={loading}
              daily={analytics?.daily ?? []}
              series={series}
            />
          </div>

          <div className="grid grid-2 items-stretch [&>div]:flex [&>div>article]:flex-1">
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

          <div data-testid="stats-agent-quality">
            <AgentQualityWidget
              agents={agents ?? []}
              t={t}
              localeTag={localeTag}
            />
          </div>

          <div data-testid="stats-receipt">
            <ReceiptWidget t={t} />
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
                <AgentLeaderboardCard t={t} rows={leaderboardRows} agents={agents} />
              </div>
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}
