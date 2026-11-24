import { useMemo } from "react";
import { BreakdownChart, type BreakdownDimension, type BreakdownMeasure, type BreakdownRecord } from "../../kit/pro/breakdown-chart";
import type { TokenAnalyticsDailyPoint } from "../../types";
import type { TFunction } from "../dashboard/model";
import { formatCompactDate, msg, type DailySeriesDescriptor } from "./statsModel";

export default function DailyTokenCompositionChart({
  t,
  daily,
  series,
}: {
  t: TFunction;
  daily: TokenAnalyticsDailyPoint[];
  localeTag?: string;
  series: DailySeriesDescriptor[];
}) {
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

  return (
    <div
      data-testid="stats-daily-token-scroll"
      className="min-w-0 overflow-x-auto [&_.aui-breakdown-chart]:max-w-none!"
    >
      <BreakdownChart
        title={t(msg("일별 토큰·비용 분포", "Daily token & cost breakdown", "日次トークン・コスト内訳", "每日 Token 与成本分布"))}
        records={records}
        dimensions={dimensions}
        measures={measures}
        defaultMeasure="tokens"
        defaultSlice="date"
        defaultSegment="kind"
      />
    </div>
  );
}
