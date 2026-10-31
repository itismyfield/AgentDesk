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
    <article className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
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

      <div className="px-4 py-4 sm:px-5">
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
    <article className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
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
          <span className="inline-flex max-w-full flex-wrap items-center gap-1 rounded-md border border-th-border-subtle bg-surface-subtle px-2 py-1 text-xs tabular-nums text-th-text-secondary" style={numericBadgeStyle}>
            {formatTokens(totalTokens)} total
          </span>
        }
      />

      <div className="px-4 py-4 sm:px-5">
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
                      className="[overflow-wrap:anywhere] text-sm font-semibold"
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

