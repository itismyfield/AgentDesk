import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "./model";
import { dashboardBadge, dashboardCard } from "./ui";
import { LoadingIndicator } from "./TokenAnalyticsCards";
import {
  DAILY_CACHE_HIT_CHART_HEIGHT_PX,
  TREND_PLOT_HEIGHT_PX,
  formatCost,
  formatPercentage,
  formatTokens,
  hasDailyTrendData,
  patternFillStyle,
  type DailyCacheHitPoint,
  type TrendLegendItem,
} from "./tokenAnalyticsModels";

export function ChartTooltip({
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

export function DailyTrendCard({
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
                      className="min-h-[1.8rem] whitespace-nowrap text-center text-[9px] leading-3 sm:min-h-[1rem] sm:text-[10px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      <span className="hidden sm:inline">
                        {compactLabel ? day.date.slice(5) : ""}
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

export function DailyCacheHitTrendCard({
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
                      className="min-h-[1.8rem] whitespace-nowrap text-center text-[9px] leading-3 sm:min-h-[1rem] sm:text-[10px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      <span className="hidden sm:inline">
                        {compactLabel ? day.date.slice(5) : ""}
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
