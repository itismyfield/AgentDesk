import type { ReactNode } from "react";
import type { TokenAnalyticsDailyPoint, TokenAnalyticsResponse } from "../../types";
import type { TFunction } from "../dashboard/model";
import { KpiChart, type KpiChartText } from "../../kit/pro/kpi-chart";
import { StatCard } from "../../kit/pro/stat-card";
import { TrendChart } from "../../kit/pro/trend-chart";
import { SectionCard } from "../common/KitPrimitives";
import {
  formatAxisCurrency,
  formatCompactDate,
  formatCurrency,
  formatDateLabel,
  formatTokens,
  msg,
  type MetricDelta,
} from "./statsModel";

/** Kit StatCard summary fact with an optional compact TrendChart sparkline when multiple days are present. */
function SummaryFact({
  testId,
  label,
  value,
  note,
  chart,
}: {
  testId: string;
  label: string;
  value: string;
  note: string;
  chart?: ReactNode;
}) {
  return (
    <div data-testid={testId} className="min-w-0">
      <StatCard
        label={label}
        value={value}
        footer={note}
        chart={chart}
        layout={chart ? "beside" : "stack"}
      />
    </div>
  );
}

/** Kit chart texts besides the empty notice, which the cards pass as `emptyLabel`. */
function kpiText(t: TFunction): Partial<KpiChartText> {
  return {
    guidance: t(msg("그래프를 탭하거나 방향키로 날짜를 고르면 그날 값이 보여요.", "Tap the chart or use the arrow keys to pick a date and read its value.")),
    increase: t(msg("증가", "up")),
    decrease: t(msg("감소", "down")),
    plot: (title, count) => t(msg(`${title}. 좌우 방향키로 ${count}일의 값을 읽어요.`, `${title}. Use the left and right arrow keys to read ${count} dates.`)),
    help: (idle) => t(msg(
      `초점만으로는 날짜를 고르지 않아요. 좌우 방향키로 날짜를 옮기고 Home·End로 처음과 끝을 골라요. Escape를 누르거나 초점이나 포인터가 벗어나면 ${idle === null ? "상세를 숨겨요" : `${idle}로 돌아가요`}. 탭으로 고른 날짜는 손을 떼도 남고 같은 날짜를 다시 탭하면 풀려요.`,
      `Focus alone does not pick a date. Arrow keys move between dates and Home and End jump to the ends. Escape, leaving focus or moving the pointer away ${idle === null ? "hides the details" : `returns to ${idle}`}. A tapped date stays after lifting the finger; tap it again to let it go.`,
    )),
    noValue: t(msg("값 없음", "No value")),
  };
}

/** Usage summary: tokens and spend as kit KPI charts over the range's days, then active days and the peak day.
 * The server sends no previous-range totals, so the charts carry no change badge. */
export function StatsSummaryGrid({
  t,
  numberFormatter,
  localeTag,
  rangeDays,
  summary,
  daily,
  loading,
  failed,
  tokenMomentumDelta,
  cacheSavingsDelta,
}: {
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  localeTag: string;
  rangeDays: number;
  summary: TokenAnalyticsResponse["summary"] | null | undefined;
  daily: TokenAnalyticsDailyPoint[];
  loading: boolean;
  failed: boolean;
  tokenMomentumDelta: MetricDelta | null;
  cacheSavingsDelta: MetricDelta | null;
}) {
  const missing = "—";
  const notice = t(loading && !summary
    ? msg("데이터 불러오는 중", "Loading data", "データを読み込み中", "正在加载数据")
    : failed && !summary
      ? msg("데이터 확인 불가", "Data unavailable", "データを確認できません", "数据不可用")
      : msg("이 기간에 기록이 없어요", "Nothing recorded in this range"));
  const labels = daily.map((day) => formatCompactDate(day.date));
  const tooltipLabels = daily.map((day) => formatDateLabel(day.date, localeTag));
  const peakDay = summary?.peak_day ?? null;
  const average = summary?.average_daily_tokens ?? 0;
  const peakRatio = peakDay && average > 0 ? peakDay.total_tokens / average : null;
  const days = numberFormatter.format(rangeDays);
  const momentum = tokenMomentumDelta
    ? t(msg(` · 앞 절반 대비 ${tokenMomentumDelta.value}`, ` · ${tokenMomentumDelta.value} vs first half`, ` · 前半比 ${tokenMomentumDelta.value}`, ` · 较前半 ${tokenMomentumDelta.value}`))
    : "";
  const savingsRate = cacheSavingsDelta ? `(${cacheSavingsDelta.value.replace("-", "")})` : "";
  const text = kpiText(t);
  const hasSparkline = daily.length > 1;
  const dailyTokens = daily.map((day) => day.total_tokens);
  const peakIndex = peakDay ? daily.findIndex((day) => day.date === peakDay.date) : -1;

  return (
    <SectionCard title={t(msg(`사용량 · 최근 ${rangeDays}일`, `Usage · last ${rangeDays} days`, `使用量 · 直近${rangeDays}日`, `用量 · 最近 ${rangeDays} 天`))} testId="stats-usage">
      {/* The kit caps its card at 460px; here each chart fills its column, two columns once both fit their tooltip. */}
      <div className="@container flex flex-col gap-3 px-3 pb-3 sm:px-4 sm:pb-4">
        <div data-testid="stats-summary-grid" className="grid grid-cols-1 gap-3 @min-[880px]:grid-cols-2 [&_.aui-kpi-chart]:max-w-none!">
          <div data-testid="stats-summary-total-tokens" className="min-w-0">
            <KpiChart
              title={t(msg("총 토큰", "Total Tokens", "総トークン", "总代币"))}
              value={summary ? formatTokens(summary.total_tokens) : missing}
              labels={labels}
              tooltipLabels={tooltipLabels}
              defaultIndex={Math.max(0, daily.length - 1)}
              series={[{ id: "tokens", label: t(msg("토큰", "Tokens", "トークン", "Token")), values: dailyTokens, color: "var(--aui-accent)" }]}
              format={formatTokens}
              emptyLabel={notice}
              text={text}
              footer={t(msg(
                `${days}일 누적 · 입력·출력·캐시 읽기/쓰기 합계${momentum}`,
                `${days} day total · input + output + cache read / write${momentum}`,
                `${days}日累計 · input + output + cache read / write${momentum}`,
                `${days} 天累计 · input + output + cache read / write${momentum}`,
              ))}
            />
          </div>
          <div data-testid="stats-summary-api-spend" className="min-w-0">
            <KpiChart
              title={t(msg("API 비용", "API Spend", "API コスト", "API 成本"))}
              value={summary ? formatCurrency(summary.total_cost) : missing}
              labels={labels}
              tooltipLabels={tooltipLabels}
              defaultIndex={Math.max(0, daily.length - 1)}
              series={[{ id: "cost", label: t(msg("비용", "Cost", "コスト", "成本")), values: daily.map((day) => day.cost), color: "var(--aui-info)" }]}
              format={formatAxisCurrency}
              emptyLabel={notice}
              text={text}
              footer={summary
                ? t(msg(
                    `캐시 할인 반영 · ${formatCurrency(summary.cache_discount)}${savingsRate} 절감`,
                    `After cache discounts · ${formatCurrency(summary.cache_discount)}${savingsRate} saved`,
                    `キャッシュ割引反映 · ${formatCurrency(summary.cache_discount)}${savingsRate} 節約`,
                    `计入缓存折扣 · 已节省 ${formatCurrency(summary.cache_discount)}${savingsRate}`,
                  ))
                : notice}
            />
          </div>
        </div>
        <div className="grid grid-cols-1 gap-3 @min-[880px]:grid-cols-2">
          <SummaryFact
            testId="stats-summary-cache-saved"
            label={t(msg("활성 일수", "Active Days", "稼働日数", "活跃天数"))}
            value={summary ? `${numberFormatter.format(summary.active_days)} / ${days}` : missing}
            note={summary
              ? t(msg(`일 평균 ${formatTokens(Math.round(average))}`, `Avg ${formatTokens(Math.round(average))} per day`, `平均 ${formatTokens(Math.round(average))} / 日`, `日均 ${formatTokens(Math.round(average))}`))
              : notice}
            chart={hasSparkline ? (
              <TrendChart
                labels={labels}
                series={[{ name: t(msg("일별 토큰", "Daily tokens", "日次トークン", "每日 Token")), values: dailyTokens, color: "var(--aui-accent, #0190fe)" }]}
                variant="spark"
                axes="none"
                height={36}
                tooltip={false}
                format={formatTokens}
                label={t(msg("활성 일수 추이", "Active days trend", "稼働日数推移", "活跃天数趋势"))}
              />
            ) : undefined}
          />
          <SummaryFact
            testId="stats-summary-cache-hit"
            label={t(msg("피크 데이", "Peak Day", "ピーク日", "峰值日"))}
            value={peakDay ? formatCompactDate(peakDay.date) : missing}
            note={peakDay
              ? `${formatTokens(peakDay.total_tokens)}${peakRatio ? t(msg(` · 평균의 ${peakRatio.toFixed(1)}배`, ` · ${peakRatio.toFixed(1)}x avg`, ` · 平均の ${peakRatio.toFixed(1)}x`, ` · ${peakRatio.toFixed(1)}x 平均`)) : ""}`
              : summary ? t(msg("피크 데이터 없음", "No peak-day data")) : notice}
            chart={hasSparkline ? (
              <TrendChart
                labels={labels}
                series={[{ name: t(msg("피크 추이", "Peak trend", "ピーク推移", "峰值趋势")), values: dailyTokens, color: "var(--aui-info, #a651f0)" }]}
                variant="spark"
                axes="none"
                height={36}
                tooltip={false}
                defaultIndex={peakIndex >= 0 ? peakIndex : null}
                format={formatTokens}
                label={t(msg("피크 데이 추이", "Peak day trend", "ピーク日推移", "峰值日趋势"))}
              />
            ) : undefined}
          />
        </div>
      </div>
    </SectionCard>
  );
}
