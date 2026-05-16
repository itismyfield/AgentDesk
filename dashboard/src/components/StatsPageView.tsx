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
  formatCompactDate,
  formatCurrency,
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
  HeadlineMetricCard,
  ModelDistributionCard,
  ProviderDistributionCard,
  SkillUsageCard,
} from "./stats/StatsCards";
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

// SWR persistence (#1250). sessionStorage so cross-tab leakage is avoided
// and reload-after-deploy doesn't render the analytics empty.
const ANALYTICS_STORAGE_PREFIX = "stats:token-analytics:";
const SKILL_RANKING_STORAGE_PREFIX = "stats:skill-ranking:";

function readPersistedAnalytics(period: Period): TokenAnalyticsResponse | null {
  if (typeof sessionStorage === "undefined") return null;
  try {
    const raw = sessionStorage.getItem(ANALYTICS_STORAGE_PREFIX + period);
    return raw ? (JSON.parse(raw) as TokenAnalyticsResponse) : null;
  } catch {
    return null;
  }
}

function writePersistedAnalytics(period: Period, value: TokenAnalyticsResponse): void {
  if (typeof sessionStorage === "undefined") return;
  try {
    sessionStorage.setItem(ANALYTICS_STORAGE_PREFIX + period, JSON.stringify(value));
  } catch {
    // quota or serialization failures are fine — next fetch refills.
  }
}

function readPersistedSkillRanking(period: Period): SkillRankingResponse | null {
  if (typeof sessionStorage === "undefined") return null;
  try {
    const raw = sessionStorage.getItem(SKILL_RANKING_STORAGE_PREFIX + period);
    return raw ? (JSON.parse(raw) as SkillRankingResponse) : null;
  } catch {
    return null;
  }
}

function writePersistedSkillRanking(period: Period, value: SkillRankingResponse): void {
  if (typeof sessionStorage === "undefined") return;
  try {
    sessionStorage.setItem(SKILL_RANKING_STORAGE_PREFIX + period, JSON.stringify(value));
  } catch { /* swallow */ }
}

const NUMERIC_STYLE: CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
};

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
