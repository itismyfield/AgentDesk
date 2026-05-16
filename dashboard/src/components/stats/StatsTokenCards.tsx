import { Suspense, lazy } from "react";
import { BarChart3, Cpu, Gauge } from "lucide-react";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import { DashboardEmptyState } from "../dashboard/ui";
import {
  computeDailyHitRate,
  formatCompactDate,
  formatDateLabel,
  formatPercent,
  formatTokens,
  msg,
  type DailySeriesDescriptor,
  type ShareSegment,
} from "./statsModel";
import {
  CardHead,
  LegendDot,
  NUMERIC_STYLE,
  numericBadgeStyle,
  positiveChipStyle,
} from "./StatsCardPrimitives";

const DailyTokenCompositionChart = lazy(() => import("./DailyTokenCompositionChart"));

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
