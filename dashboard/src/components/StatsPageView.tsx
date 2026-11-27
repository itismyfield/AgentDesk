import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import {
  getCachedTokenAnalytics,
  getCachedSkillCatalog,
  getCachedSkillRanking,
  getSkillCatalog,
  getSkillRanking,
  getTokenAnalytics,
  type SkillRankingResponse,
} from "../api";
import { cx } from "./dashboard/ui";
import type { TFunction } from "./dashboard/model";
import { Button } from "../kit/free/button-clean";
import { SegmentedControl } from "../kit/free/segmented-control-clean";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  SkillCatalogEntry,
  TokenAnalyticsResponse,
} from "../types";
import {
  buildAgentItems,
  buildAgentSkillRows,
  buildModelItems,
  buildSavingsDelta,
  buildSkillRows,
  buildWindowDelta,
  computeCacheHitRate,
  dailySeries,
  msg,
  periodDayCount,
  resolveLocaleTag,
  type Period,
} from "./stats/statsModel";
import {
  DailyTokenCompositionCard,
  SkillUsageCard,
  StatsBreakdown,
  StatsSummaryGrid,
} from "./stats/StatsCards";
import { StatsAccountLimits } from "./stats/StatsAccountLimits";
import { PageFrame } from "../app/PageFrame";
import {
  readPersistedAnalytics,
  readPersistedSkillRanking,
  writePersistedAnalytics,
  writePersistedSkillRanking,
} from "./stats/statsStorage";
import { RefreshCw, RotateCcw, TriangleAlert } from "lucide-react";
import ReceiptWidget from "./dashboard/ReceiptWidget";

interface StatsPageViewProps {
  settings: CompanySettings;
  stats?: DashboardStats | null;
  agents?: Agent[];
  sessions?: DispatchedSession[];
}

const PERIOD_OPTIONS: Period[] = ["7d", "30d", "90d"];
const DEFAULT_PERIOD: Period = "7d";


export default function StatsPageView({ settings }: StatsPageViewProps) {
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

  const [period, setPeriod] = useState<Period>(DEFAULT_PERIOD);
  const [reloadKey, setReloadKey] = useState(0);
  const [skillReloadKey, setSkillReloadKey] = useState(0);
  const [catalogReloadKey, setCatalogReloadKey] = useState(0);
  const [skillsRequested, setSkillsRequested] = useState(
    () =>
      getCachedSkillRanking(DEFAULT_PERIOD, 16) !== null &&
      getCachedSkillCatalog() !== null,
  );
  const [analytics, setAnalytics] = useState<TokenAnalyticsResponse | null>(
    () =>
      getCachedTokenAnalytics(DEFAULT_PERIOD)?.data ??
      readPersistedAnalytics(DEFAULT_PERIOD),
  );
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(
    () =>
      getCachedSkillRanking(DEFAULT_PERIOD, 16)?.data ??
      readPersistedSkillRanking(DEFAULT_PERIOD),
  );
  const [catalog, setCatalog] = useState<SkillCatalogEntry[]>(
    () => getCachedSkillCatalog()?.data ?? [],
  );
  const [loading, setLoading] = useState(
    () =>
      getCachedTokenAnalytics(DEFAULT_PERIOD) === null &&
      readPersistedAnalytics(DEFAULT_PERIOD) === null,
  );
  const [skillLoading, setSkillLoading] = useState(false);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [analyticsError, setAnalyticsError] = useState<string | null>(null);
  const [skillError, setSkillError] = useState<string | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);

  useEffect(() => {
    if (!skillsRequested) return;
    let active = true;

    const load = async () => {
      const cachedCatalog = getCachedSkillCatalog();
      if (cachedCatalog) setCatalog(cachedCatalog.data);
      setCatalogLoading(cachedCatalog === null);
      setCatalogError(null);
      try {
        const next = await getSkillCatalog({ suppressErrorToast: true });
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
  }, [t, catalogReloadKey, skillsRequested]);

  useEffect(() => {
    let active = true;

    const load = async () => {
      const cachedAnalytics = getCachedTokenAnalytics(period);
      const persistedAnalytics = cachedAnalytics ? null : readPersistedAnalytics(period);
      setAnalytics(cachedAnalytics ? cachedAnalytics.data : persistedAnalytics);
      setLoading(!cachedAnalytics && persistedAnalytics === null);
      setAnalyticsError(null);

      const forceRefresh = reloadKey > 0;
      try {
        const next = await getTokenAnalytics(period, { forceRefresh, suppressErrorToast: true });
        if (!active) return;
        setAnalytics(next);
        writePersistedAnalytics(period, next);
      } catch {
        if (!active) return;
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
      } finally {
        if (active) setLoading(false);
      }
    };

    void load();
    return () => {
      active = false;
    };
  }, [period, reloadKey, t]);

  useEffect(() => {
    if (!skillsRequested) return;
    let active = true;

    const load = async () => {
      const cachedRanking = getCachedSkillRanking(period, 16);
      const persistedRanking = cachedRanking ? null : readPersistedSkillRanking(period);
      setSkillRanking(cachedRanking?.data ?? persistedRanking);
      setSkillLoading(!cachedRanking && persistedRanking === null);
      setSkillError(null);

      try {
        const next = await getSkillRanking(period, 16, { suppressErrorToast: true });
        if (!active) return;
        setSkillRanking(next);
        writePersistedSkillRanking(period, next);
      } catch {
        if (!active) return;
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
      } finally {
        if (active) setSkillLoading(false);
      }
    };

    void load();
    return () => {
      active = false;
    };
  }, [period, reloadKey, skillReloadKey, skillsRequested, t]);

  const summary = analytics?.summary;
  const hasLoadError = Boolean(analyticsError);
  const combinedError = analyticsError ?? "";
  const skillLoadError = skillError ?? catalogError;
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
  const modelItems = useMemo(() => buildModelItems(analytics), [analytics]);
  const agentItems = useMemo(() => buildAgentItems(analytics), [analytics]);
  const skillRows = useMemo(
    () => buildSkillRows(skillRanking, catalog, language),
    [catalog, language, skillRanking],
  );
  const agentSkillPairs = useMemo(
    () => buildAgentSkillRows(skillRanking, catalog, language),
    [catalog, language, skillRanking],
  );
  const topAgentSkillPairs = useMemo(() => agentSkillPairs.slice(0, 5), [agentSkillPairs]);
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

  const requestSkills = useCallback(() => {
    setSkillsRequested(true);
  }, []);

  const retrySkills = useCallback(() => {
    setSkillsRequested(true);
    setSkillReloadKey((value) => value + 1);
    if (catalogError) setCatalogReloadKey((value) => value + 1);
  }, [catalogError]);

  const retry = () => {
    setReloadKey((value) => value + 1);
  };

  // The kit control has no per-option props; tag its radios with the existing e2e hooks.
  const rangeRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    rangeRef.current?.querySelectorAll<HTMLElement>('[role="radio"]').forEach((element, index) => {
      const option = PERIOD_OPTIONS[index];
      if (option) element.dataset.testid = `stats-range-${option}`;
    });
  });

  return (
    <PageFrame
      testId="stats-page"
      title={t(msg("통계", "Stats", "統計", "统计"))}
      description={t(
        msg(
          "토큰 사용량, 비용, 캐시 효과, 모델별 비중을 봐요.",
          "Token, cost, cache, and model mix in one place.",
          "トークン / コスト / キャッシュ / モデル分布を一か所で確認します。",
          "在一个页面查看 Token、成本、缓存和模型分布。",
        ),
      )}
      actions={
        <div data-testid="stats-page-header" className="flex min-w-0 max-w-full flex-wrap items-center gap-2">
          {/* The kit control is an inline-size container, so its slot needs a definite width. */}
          <div ref={rangeRef} className="w-56 max-w-full" data-testid="stats-range-controls">
            <SegmentedControl<Period>
              variant="tray"
              fullWidth
              value={period}
              onChange={setPeriod}
              aria-label={t(msg("기간", "Range", "期間", "时间范围"))}
              options={PERIOD_OPTIONS.map((option) => ({
                value: option,
                label: t(
                  option === "7d"
                    ? msg("7일", "7d", "7日", "7天")
                    : option === "30d"
                      ? msg("30일", "30d", "30日", "30天")
                      : msg("90일", "90d", "90日", "90天"),
                ),
              }))}
            />
          </div>
          <Button
            variant="secondary"
            data-testid="stats-refresh-button"
            icon={<RefreshCw className={cx((loading || skillLoading) && "animate-spin motion-reduce:animate-none")} />}
            onClick={() => setReloadKey((value) => value + 1)}
          >
            {t(msg("새로고침", "Refresh", "再読み込み", "刷新"))}
          </Button>
        </div>
      }
    >
      <div className="flex min-w-0 flex-col gap-3 sm:gap-4">
        {hasLoadError ? (
          <div
            role="alert"
            className="flex min-w-0 flex-wrap items-center gap-x-3 gap-y-2 rounded-[var(--r-md)] border border-[color-mix(in_oklch,var(--warn)_45%,var(--line-soft))] bg-[var(--bg-2)] px-4 py-3"
          >
            <TriangleAlert size={16} className="shrink-0 text-[var(--warn)]" aria-hidden="true" />
            <div className="min-w-0 flex-1 basis-56">
              <div className="text-sm font-semibold text-[var(--fg)]">
                {t(
                  msg(
                    "일부 통계를 불러오지 못했어요.",
                    "Some stats could not be loaded.",
                    "一部の統計を読み込めませんでした。",
                    "部分统计加载失败。",
                  ),
                )}
              </div>
              <div className="mt-0.5 text-xs leading-5 text-[var(--fg-muted)]">{combinedError}</div>
            </div>
            <Button variant="secondary" icon={<RotateCcw />} data-testid="stats-retry-button" onClick={retry}>
              {t(msg("다시 시도", "Retry", "再試行", "重试"))}
            </Button>
          </div>
        ) : null}

        <StatsSummaryGrid
          t={t}
          numberFormatter={numberFormatter}
          localeTag={localeTag}
          rangeDays={rangeDays}
          summary={summary}
          daily={analytics?.daily ?? []}
          loading={loading}
          failed={Boolean(analyticsError)}
          tokenMomentumDelta={tokenMomentumDelta}
          cacheSavingsDelta={cacheSavingsDelta}
        />

        <div className="flex min-w-0 flex-wrap items-start gap-3 sm:gap-4">
          <DailyTokenCompositionCard
            t={t}
            localeTag={localeTag}
            loading={loading}
            failed={Boolean(analyticsError)}
            daily={analytics?.daily ?? []}
            series={series}
            className="min-w-0 flex-[2_1_32rem]"
          />
          <StatsAccountLimits
            t={t}
            locale={localeTag}
            allowSwitch
            defaultMasked
            className="min-w-0 flex-[1_1_18rem]"
          />
        </div>

        <StatsBreakdown
          t={t}
          loading={loading}
          failed={Boolean(analyticsError)}
          models={modelItems}
          agents={agentItems}
          overallCacheHitRate={overallCacheHitRate}
          agentSkillRows={agentSkillPairs}
          series={series}
          skillsLoading={skillsRequested && (skillLoading || catalogLoading)}
          onAgentInspect={requestSkills}
        />

        <SkillUsageCard
          t={t}
          loading={skillsRequested && (skillLoading || catalogLoading)}
          rows={skillRows}
          byAgentRows={topAgentSkillPairs}
          windowCalls={skillWindowCalls}
          requested={skillsRequested}
          error={skillLoadError}
          onRequest={requestSkills}
          onRetry={retrySkills}
        />

        <div data-testid="stats-receipt">
          <ReceiptWidget t={t} />
        </div>
      </div>
    </PageFrame>
  );
}
