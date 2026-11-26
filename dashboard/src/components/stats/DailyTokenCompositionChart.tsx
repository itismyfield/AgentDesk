import { useMemo, useState } from "react";
import { SegmentedControl } from "../../kit/free/segmented-control-clean";
import { BreakdownChart, type BreakdownChartLabels, type BreakdownDimension, type BreakdownMeasure, type BreakdownRecord } from "../../kit/pro/breakdown-chart";
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

  const breakdownLabels = useMemo<Partial<BreakdownChartLabels>>(
    () => ({
      title: t(msg("일별 토큰·비용 분포", "Daily token & cost breakdown", "日次トークン・コスト内訳", "每日 Token 与成本分布")),
      measurePicker: t(msg("집계 방식", "Measure", "集計方法", "汇总方式")),
      slicePicker: t(msg("가로축 분류", "X-axis", "横軸の分類", "横轴分类")),
      segmentPicker: t(msg("누적 구분", "Stack by", "積み上げ区分", "堆叠分组")),
      noSegment: t(msg("분할 없음", "None", "分割なし", "不拆分")),
      unconfiguredValue: t(msg("설정 없음", "Not set", "未設定", "未设置")),
      closePicker: (label) => t(msg(`${label} 닫기`, `Close ${label}`, `${label}を閉じる`, `关闭${label}`)),
      emptySetupTitle: t(msg("차트 설정을 확인해 주세요", "Check chart configuration", "チャート設定を確認してください", "请检查图表配置")),
      emptySetupBody: t(msg("분류와 집계 정의가 각각 하나 이상 필요합니다.", "At least one dimension and one measure are required.", "分類と集計の定義がそれぞれ1つ以上必要です。", "至少需要定义一个维度和一个指标。")),
      zeroRecordsNote: t(msg("실행 기록을 기다리는 중입니다. 정의된 분류의 0값을 표시합니다.", "Waiting for records. Showing zeros for defined categories.", "実行記録を待機中です。定義された分類の0値を表示します。", "正在等待记录，当前显示已定义分类的 0 值。")),
      clearSelection: t(msg("선택 해제", "Clear selection", "選択解除", "取消选择")),
      readoutHint: t(msg("막대나 아래 행을 탭해 상세 수치를 확인하세요.", "Tap a bar or row below to inspect values.", "棒または下の行をタップして詳細を確認してください。", "点按柱形或下方行查看详细数值。")),
      tableCaption: (chartTitle) => t(msg(
        `${chartTitle} 상세 수치. 합계는 현재 보이는 구분만 포함합니다. 흐린 열은 감춘 구분의 원래 수치입니다.`,
        `${chartTitle} breakdown values. Totals include visible segments only; dimmed columns show hidden segments.`,
        `${chartTitle}の詳細数値。合計は表示中の区分のみを含み、薄い列は非表示区分の元の数値です。`,
        `${chartTitle}详细数值。合计仅包含当前显示的分组，浅色列为隐藏分组的原始数值。`,
      )),
      hiddenInChart: t(msg(" (차트에서 감춤)", " (hidden in chart)", " (チャートで非表示)", "（图表中已隐藏）")),
      emptySliceTitle: t(msg("가로축에 표시할 분류값이 없습니다", "No categories to display on X-axis", "横軸に表示する分類値がありません", "横轴没有可显示的分类值")),
      emptySliceBody: t(msg("분류 정의에 값을 추가하면 막대와 표를 표시합니다.", "Add dimension values to render bars and table.", "分類定義に値を追加すると棒と表を表示します。", "添加维度值后即可显示柱状图和表格。")),
      legendToggleLabel: (label) => t(msg(`${label} 표시 토글`, `Toggle ${label} visibility`, `${label}の表示切替`, `切换${label}显示`)),
      unsegmentedHint: t(msg("누적 구분을 선택해 작업의 구성을 살펴보세요.", "Choose a stack dimension to inspect composition.", "積み上げ区分を選択して内訳を確認してください。", "选择堆叠分组以查看构成。")),
      restoreAll: t(msg("전체 복원", "Restore all", "すべて復元", "全部恢复")),
    }),
    [t],
  );

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
        className="min-w-0 overflow-x-auto [&_.aui-breakdown-chart]:max-w-none! [&_.aui-breakdown-chart-chart]:mx-3! [&_.aui-breakdown-chart-chart]:mt-2! [&_.aui-breakdown-chart-chart]:mb-2! [&_.aui-breakdown-chart-heading]:hidden! min-[560px]:[&_.aui-breakdown-chart-picker]:flex! min-[560px]:[&_.aui-breakdown-chart-picker]:items-center! min-[560px]:[&_.aui-breakdown-chart-picker]:gap-2! min-[560px]:[&_.aui-breakdown-chart-picker>span]:shrink-0! min-[560px]:[&_.aui-breakdown-chart-picker-button]:w-auto! min-[560px]:[&_.aui-breakdown-chart-picker-button]:min-w-0! min-[560px]:[&_.aui-breakdown-chart-picker-button]:flex-1! min-[900px]:[&_.aui-breakdown-chart-picker-button]:h-7! min-[900px]:[&_.aui-breakdown-chart-picker-button]:px-2.5! min-[900px]:[&_.aui-breakdown-chart-picker-button]:text-xs! [&_.aui-breakdown-chart-pickers]:gap-2! [&_.aui-breakdown-chart-pickers]:px-3! [&_.aui-breakdown-chart-pickers]:pt-2! [&_.aui-breakdown-chart-pickers]:pb-1! [&_.aui-breakdown-chart-tray]:bg-transparent! [&_.aui-breakdown-chart-tray]:p-0! [&_.aui-metering-chart]:p-3.5! [&_.aui-metering-chart-head_h2]:hidden!"
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
            labels={breakdownLabels}
          />
        )}
      </div>
    </div>
  );
}
