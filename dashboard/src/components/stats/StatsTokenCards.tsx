import { Suspense, lazy, useState } from "react";
import { BarChart3 } from "lucide-react";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { Skeleton } from "../../kit/free/skeleton-clean";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import { Cap, SectionCard } from "../common/KitPrimitives";
import type { DailyTokenViewMode } from "./DailyTokenCompositionChart";
import { msg, type DailySeriesDescriptor } from "./statsModel";
import { StatsEmpty } from "./StatsCardPrimitives";

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

/** Daily stacked token breakdown & metering card inside a v3 SectionCard. */
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
  const [viewMode, setViewMode] = useState<DailyTokenViewMode>("breakdown");
  const allZero = daily.length > 0 && daily.every((day) => day.total_tokens <= 0);

  return (
    <SectionCard
      title={t(msg("일별 토큰 구성", "Daily Token Composition", "日次トークン構成", "每日 Token 构成"))}
      testId="stats-daily-token-chart"
      className={className}
      aside={
        <div className="w-44 max-w-full" data-testid="stats-daily-view-mode">
          <SegmentedControl<DailyTokenViewMode>
            kind="tabs"
            variant="tray"
            size="sm"
            fullWidth
            disabled={daily.length === 0}
            value={viewMode}
            onChange={setViewMode}
            aria-label={t(msg("일별 토큰 보기 방식", "Daily token view mode", "日次トークン表示形式", "每日 Token 视图模式"))}
            options={[
              { value: "breakdown", label: t(msg("분포", "Breakdown", "内訳", "分布")) },
              { value: "metering", label: t(msg("미터링", "Metering", "メーター", "计量")) },
            ]}
          />
        </div>
      }
    >
      <div className="px-3 pb-3 sm:px-4 sm:pb-4">
        <Cap className="mb-3 block">
          {t(
            msg(
              "날짜·토큰 종류 기준과 토큰·비용 지표를 전환하며 비교해요",
              "Switch dimensions and measures to compare daily tokens and cost",
              "日付・トークン種別と指標を切り替えて比較します",
              "切换日期、Token 类型与指标进行对比",
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
              <DailyTokenCompositionChart t={t} daily={daily} localeTag={localeTag} series={series} mode={viewMode} onModeChange={setViewMode} />
            </Suspense>
          </>
        )}
      </div>
    </SectionCard>
  );
}
