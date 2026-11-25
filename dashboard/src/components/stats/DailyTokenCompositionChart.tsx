import { useMemo, useState } from "react";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { BreakdownChart, type BreakdownDimension, type BreakdownMeasure, type BreakdownRecord } from "../../kit/pro/breakdown-chart";
import { MeteringChart, type MeterDay, type MeterTotal } from "../../kit/pro/metering-chart";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import { formatCompactDate, formatCurrency, formatTokens, msg, type DailySeriesDescriptor } from "./statsModel";

export type DailyTokenViewMode = "breakdown" | "metering";

export default function DailyTokenCompositionChart({
  t,
  daily,
  series,
  mode,
  onModeChange,
}: {
  t: TFunction;
  daily: TokenAnalyticsDailyPoint[];
  localeTag?: string;
  series: DailySeriesDescriptor[];
  mode?: DailyTokenViewMode;
  onModeChange?: (mode: DailyTokenViewMode) => void;
}) {
  const [innerMode, setInnerMode] = useState<DailyTokenViewMode>("breakdown");
  const activeMode = mode ?? innerMode;
  const handleModeChange = (next: DailyTokenViewMode) => {
    if (mode === undefined) setInnerMode(next);
    onModeChange?.(next);
  };

  const records = useMemo<BreakdownRecord[]>(
    () =>
      daily.flatMap((day) =>
        series.map((item, index) => ({
          date: day.date,
          kind: item.key,
          tokens: day[item.key],
          cost: day.total_tokens > 0 ? (day.cost * day[item.key]) / day.total_tokens : index === 0 ? day.cost : 0,
        })),
      ),
    [daily, series],
  );

  const dimensions = useMemo<BreakdownDimension[]>(
    () => [
      {
        id: "date",
        label: t(msg("날짜", "Date", "日付", "日期")),
        values: daily.map((day) => ({ id: day.date, label: formatCompactDate(day.date) })),
      },
      {
        id: "kind",
        label: t(msg("토큰 종류", "Token type", "トークン種別", "Token 类型")),
        values: series.map((item) => ({ id: item.key, label: item.label, color: item.color })),
      },
    ],
    [daily, series, t],
  );

  const measures = useMemo<BreakdownMeasure[]>(
    () => [
      { id: "tokens", label: t(msg("토큰", "Tokens", "トークン", "Token")), field: "tokens" },
      { id: "cost", label: t(msg("비용", "Cost", "コスト", "成本")), unit: "USD", field: "cost" },
    ],
    [t],
  );

  const meterDays = useMemo<MeterDay[]>(
    () =>
      daily.map((day) => ({
        label: formatCompactDate(day.date),
        total: day.total_tokens,
        slices: series.map((s) => ({ name: s.label, value: day[s.key] })),
      })),
    [daily, series],
  );

  const meterTotals = useMemo<MeterTotal[]>(() => {
    const totalTokens = daily.reduce((sum, day) => sum + day.total_tokens, 0);
    const totalCost = daily.reduce((sum, day) => sum + day.cost, 0);
    return [
      { name: t(msg("총 토큰", "Total tokens", "総トークン", "总 Token")), value: totalTokens, text: formatTokens(totalTokens) },
      { name: t(msg("총 비용", "Total cost", "総コスト", "总成本")), value: totalCost, text: formatCurrency(totalCost) },
    ];
  }, [daily, t]);

  const periodLabel =
    daily.length > 1
      ? `${formatCompactDate(daily[0].date)} – ${formatCompactDate(daily[daily.length - 1].date)}`
      : daily[0]
        ? formatCompactDate(daily[0].date)
        : undefined;

  return (
    <div className="flex min-w-0 flex-col gap-2">
      {mode === undefined && (
        <div className="ml-auto w-44 max-w-full" data-testid="stats-daily-view-mode">
          <SegmentedControl<DailyTokenViewMode>
            kind="tabs"
            variant="tray"
            size="sm"
            fullWidth
            value={activeMode}
            onChange={handleModeChange}
            aria-label={t(msg("일별 토큰 보기 방식", "Daily token view mode", "日次トークン表示形式", "每日 Token 视图模式"))}
            options={[
              { value: "breakdown", label: t(msg("분포", "Breakdown", "内訳", "分布")) },
              { value: "metering", label: t(msg("미터링", "Metering", "メーター", "计量")) },
            ]}
          />
        </div>
      )}
      <div
        data-testid="stats-daily-token-scroll"
        className="min-w-0 overflow-x-auto [&_.aui-breakdown-chart]:max-w-none! [&_.aui-breakdown-chart-chart]:mx-3! [&_.aui-breakdown-chart-chart]:mt-2! [&_.aui-breakdown-chart-chart]:mb-2! [&_.aui-breakdown-chart-heading]:hidden! min-[560px]:[&_.aui-breakdown-chart-picker]:flex! min-[560px]:[&_.aui-breakdown-chart-picker]:items-center! min-[560px]:[&_.aui-breakdown-chart-picker]:gap-2! min-[560px]:[&_.aui-breakdown-chart-picker>span]:shrink-0! min-[560px]:[&_.aui-breakdown-chart-picker-button]:w-auto! min-[560px]:[&_.aui-breakdown-chart-picker-button]:min-w-0! min-[560px]:[&_.aui-breakdown-chart-picker-button]:flex-1! [&_.aui-breakdown-chart-picker-button]:h-7! [&_.aui-breakdown-chart-picker-button]:px-2.5! [&_.aui-breakdown-chart-picker-button]:text-xs! [&_.aui-breakdown-chart-pickers]:gap-2! [&_.aui-breakdown-chart-pickers]:px-3! [&_.aui-breakdown-chart-pickers]:pt-2! [&_.aui-breakdown-chart-pickers]:pb-1! [&_.aui-breakdown-chart-tray]:bg-transparent! [&_.aui-breakdown-chart-tray]:p-0! [&_.aui-metering-chart]:p-3.5! [&_.aui-metering-chart-head_h2]:hidden!"
      >
        {activeMode === "metering" ? (
          <MeteringChart
            title={t(msg("일별 토큰·비용 미터링", "Daily token & cost metering", "日次トークン・コストメーター", "每日 Token 与成本计量"))}
            days={meterDays}
            totals={meterTotals}
            period={periodLabel}
            height={168}
            format={formatTokens}
            emptyLabel={t(msg("표시할 일별 토큰 데이터가 없습니다.", "No daily token data available.", "表示できる日次トークンデータがありません。", "暂无每日 Token 数据。"))}
            restLabel={t(msg("기타", "Other", "その他", "其他"))}
          />
        ) : (
          <BreakdownChart
            title={t(msg("일별 토큰·비용 분포", "Daily token & cost breakdown", "日次トークン・コスト内訳", "每日 Token 与成本分布"))}
            records={records}
            dimensions={dimensions}
            measures={measures}
            defaultMeasure="tokens"
            defaultSlice="date"
            defaultSegment="kind"
          />
        )}
      </div>
    </div>
  );
}
