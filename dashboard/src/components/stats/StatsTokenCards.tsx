import { Suspense, lazy } from "react";
import { BarChart3 } from "lucide-react";
import { Skeleton } from "../../kit/free/skeleton-clean";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import { Cap, SectionCard } from "../common/KitPrimitives";
import { msg, type DailySeriesDescriptor } from "./statsModel";
import { NUMERIC_STYLE, StatsEmpty, StatsSwatch } from "./StatsCardPrimitives";

const DailyTokenCompositionChart = lazy(() => import("./DailyTokenCompositionChart"));

function ChartPlaceholder() {
  return (
    <div aria-hidden="true">
      <Skeleton width="100%" height={232} radius={8} />
      <div className="mt-2 flex items-center gap-2">
        <Skeleton width={44} height={44} radius={8} />
        <Skeleton width="100%" height={12} line />
        <Skeleton width={44} height={44} radius={8} />
      </div>
    </div>
  );
}

/** Daily stacked token chart with its legend and fixed readout in a v3 section card (kit palette tokens). */
export function DailyTokenCompositionCard({
  t,
  localeTag,
  loading,
  failed,
  daily,
  series,
  className,
}: {
  t: TFunction;
  localeTag: string;
  loading: boolean;
  failed: boolean;
  daily: TokenAnalyticsDailyPoint[];
  series: DailySeriesDescriptor[];
  className?: string;
}) {
  const allZero = daily.length > 0 && daily.every((day) => day.total_tokens <= 0);

  return (
    <SectionCard
      title={t(msg("일별 토큰 구성", "Daily Token Composition", "日次トークン構成", "每日 Token 构成"))}
      testId="stats-daily-token-chart"
      className={className}
      aside={
        <ul className="m-0 flex list-none flex-wrap gap-x-3 gap-y-1 p-0 text-[11px] text-[var(--fg-muted)]" style={NUMERIC_STYLE} aria-label={t(msg("범례", "Legend"))}>
          {series.map((item) => (
            <li key={item.key} className="inline-flex items-center gap-1.5">
              <StatsSwatch color={item.color} />
              <span>{item.label}</span>
            </li>
          ))}
        </ul>
      }
    >
      <div className="px-3 pb-3 sm:px-4 sm:pb-4">
        <Cap className="mb-3 block">
          {t(
            msg(
              "input · output · cache read / write · 막대를 눌러 날짜별로 읽기",
              "input · output · cache read / write · tap a bar to read a day",
              "input · output · cache read / write · バーをタップして日別に表示",
              "input · output · cache read / write · 点按柱状图查看每日数据",
            ),
          )}
        </Cap>

        {daily.length === 0 ? (
          loading ? (
            <div role="status" aria-label={t(msg("일별 토큰 차트를 불러오는 중입니다.", "Loading daily token chart."))}>
              <ChartPlaceholder />
            </div>
          ) : (
            <StatsEmpty
              icon={<BarChart3 size={18} />}
              testId="stats-daily-empty"
              title={failed
                ? t(msg("일별 토큰을 불러오지 못했어요", "Daily tokens could not be loaded"))
                : t(msg("표시할 일별 토큰 데이터가 없습니다.", "No daily token data available."))}
              text={failed
                ? t(msg("위의 다시 시도를 누르면 이 기간을 다시 불러와요.", "Use Retry above to load this range again."))
                : t(msg("에이전트가 일하면 날짜별 토큰이 여기에 쌓여요.", "Tokens appear here by day once agents run."))}
            />
          )
        ) : (
          <>
            {allZero ? (
              <p className="mb-2 text-xs leading-5 text-[var(--fg-muted)]" data-testid="stats-daily-zero">
                {t(msg("이 기간에는 기록된 토큰이 0이에요.", "No tokens were recorded in this range."))}
              </p>
            ) : null}
            <Suspense fallback={<ChartPlaceholder />}>
              <DailyTokenCompositionChart t={t} daily={daily} localeTag={localeTag} series={series} />
            </Suspense>
          </>
        )}
      </div>
    </SectionCard>
  );
}
