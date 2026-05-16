import { Suspense, lazy, type CSSProperties, type ReactNode } from "react";
import { BarChart3, Cpu, Gauge, Info, Users } from "lucide-react";
import type { Agent, TokenAnalyticsDailyPoint, TokenAnalyticsResponse } from "../../types";
import type { TFunction } from "../dashboard/model";
import { DashboardEmptyState, cx } from "../dashboard/ui";
import AgentAvatar from "../AgentAvatar";
import {
  computeDailyHitRate,
  formatCompactDate,
  formatCurrency,
  formatDateLabel,
  formatPercent,
  formatTokens,
  msg,
  type AgentCacheRow,
  type AgentSkillRow,
  type AgentSpendRow,
  type DailySeriesDescriptor,
  type LeaderboardRow,
  type MetricDelta,
  type MetricDeltaTone,
  type ShareSegment,
  type SkillUsageRow,
} from "./statsModel";

const DailyTokenCompositionChart = lazy(() => import("./DailyTokenCompositionChart"));

const NUMERIC_STYLE: CSSProperties = {
  fontFamily: "var(--font-mono)",
  fontVariantNumeric: "tabular-nums",
  fontFeatureSettings: '"tnum" 1',
};

export function StatsSummaryGrid({
  t,
  numberFormatter,
  rangeDays,
  summary,
  tokenMomentumDelta,
  cacheSavingsDelta,
}: {
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  rangeDays: number;
  summary: TokenAnalyticsResponse["summary"] | null | undefined;
  tokenMomentumDelta: MetricDelta | null;
  cacheSavingsDelta: MetricDelta | null;
}) {
  const peakDay = summary?.peak_day ?? null;
  const averageDailyTokens = summary?.average_daily_tokens ?? 0;
  const peakRatio =
    peakDay && averageDailyTokens > 0
      ? peakDay.total_tokens / averageDailyTokens
      : null;

  return (
    <div className="grid grid-4" data-testid="stats-summary-grid">
      <div data-testid="stats-summary-total-tokens">
        <HeadlineMetricCard
          title={t(msg("총 토큰", "Total Tokens", "総トークン", "总代币"))}
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
          title={t(msg("API 비용", "API Spend", "API コスト", "API 成本"))}
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
          title={t(msg("활성 일수", "Active Days", "稼働日数", "活跃天数"))}
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
              : t(msg("활성 일수 집계 대기", "Waiting for active-day data"))
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
  );
}

export function HeadlineMetricCard({
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
            style={{ color: "var(--th-text-muted)", cursor: "help" }}
            data-tip={tip}
            title={tip}
            aria-label={`${title}: ${tip}`}
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

export function DailyTokenCompositionCard({
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
          <Suspense
            fallback={
              <div
                className="h-[246px] rounded-xl"
                style={{ background: "var(--th-overlay-subtle)" }}
              />
            }
          >
            <DailyTokenCompositionChart
              t={t}
              daily={daily}
              localeTag={localeTag}
              series={series}
            />
          </Suspense>
        )}
      </div>
    </article>
  );
}

export function DailyCacheHitCard({
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

export function ModelDistributionCard({
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

export function ProviderDistributionCard({
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

export function AgentSpendCard({
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

export function AgentCacheCard({
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

export function SkillUsageCard({
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

export function AgentLeaderboardCard({
  t,
  rows,
  agents,
}: {
  t: TFunction;
  rows: LeaderboardRow[];
  agents?: Agent[];
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
                    className="inline-grid h-9 w-9 place-items-center overflow-hidden rounded-full"
                    style={{ background: "var(--th-overlay-subtle)" }}
                  >
                    <AgentAvatar agent={row.agent ?? undefined} agents={agents} size={32} rounded="full" />
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
