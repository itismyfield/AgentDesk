import type { TokenAnalyticsDailyPoint, TokenAnalyticsResponse } from "../../types";
import type { TFunction } from "../dashboard/model";
import { KpiChart } from "../../kit/pro/kpi-chart";
import { Cap, SectionCard } from "../common/KitPrimitives";
import {
  formatAxisCurrency,
  formatCompactDate,
  formatCurrency,
  formatDateLabel,
  formatTokens,
  msg,
  type MetricDelta,
} from "./statsModel";
import { NUMERIC_STYLE } from "./StatsCardPrimitives";

/** A summary fact without a daily series: label, value and its note. */
function SummaryFact({ testId, label, value, note }: { testId: string; label: string; value: string; note: string }) {
  return (
    <div data-testid={testId} className="flex min-w-0 flex-col gap-0.5 px-3 py-3 sm:px-4">
      <dt className="text-xs leading-4 text-[var(--fg-muted)]">{label}</dt>
      <dd className="m-0 flex min-w-0 flex-wrap items-baseline gap-x-2 text-lg leading-7 font-semibold text-[var(--fg)]" style={NUMERIC_STYLE}>
        {value}
        <Cap className="font-normal [overflow-wrap:anywhere]">{note}</Cap>
      </dd>
    </div>
  );
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

  return (
    <SectionCard title={t(msg(`사용량 · 최근 ${rangeDays}일`, `Usage · last ${rangeDays} days`, `使用量 · 直近${rangeDays}日`, `用量 · 最近 ${rangeDays} 天`))} testId="stats-usage">
      {/* The kit caps its card at 460px; here each chart fills its column, two columns once both fit their tooltip. */}
      <div className="@container px-3 pb-3 sm:px-4 sm:pb-4">
        <div data-testid="stats-summary-grid" className="grid grid-cols-1 gap-3 @min-[880px]:grid-cols-2 [&_.aui-kpi-chart]:max-w-none!">
          <div data-testid="stats-summary-total-tokens" className="min-w-0">
            <KpiChart
              title={t(msg("총 토큰", "Total Tokens", "総トークン", "总代币"))}
              value={summary ? formatTokens(summary.total_tokens) : missing}
              labels={labels}
              tooltipLabels={tooltipLabels}
              series={[{ id: "tokens", label: t(msg("토큰", "Tokens", "トークン", "Token")), values: daily.map((day) => day.total_tokens), color: "var(--aui-accent)" }]}
              format={formatTokens}
              emptyLabel={notice}
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
              series={[{ id: "cost", label: t(msg("비용", "Cost", "コスト", "成本")), values: daily.map((day) => day.cost), color: "var(--aui-info)" }]}
              format={formatAxisCurrency}
              emptyLabel={notice}
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
      </div>
      <dl className="m-0 grid grid-cols-1 border-t border-[var(--line-soft)] sm:grid-cols-2 sm:divide-x sm:divide-[var(--line-soft)] max-sm:divide-y max-sm:divide-[var(--line-soft)]">
        <SummaryFact
          testId="stats-summary-cache-saved"
          label={t(msg("활성 일수", "Active Days", "稼働日数", "活跃天数"))}
          value={summary ? `${numberFormatter.format(summary.active_days)} / ${days}` : missing}
          note={summary
            ? t(msg(`일 평균 ${formatTokens(Math.round(average))}`, `Avg ${formatTokens(Math.round(average))} per day`, `平均 ${formatTokens(Math.round(average))} / 日`, `日均 ${formatTokens(Math.round(average))}`))
            : notice}
        />
        <SummaryFact
          testId="stats-summary-cache-hit"
          label={t(msg("피크 데이", "Peak Day", "ピーク日", "峰值日"))}
          value={peakDay ? formatCompactDate(peakDay.date) : missing}
          note={peakDay
            ? `${formatTokens(peakDay.total_tokens)}${peakRatio ? t(msg(` · 평균의 ${peakRatio.toFixed(1)}배`, ` · ${peakRatio.toFixed(1)}x avg`, ` · 平均の ${peakRatio.toFixed(1)}x`, ` · ${peakRatio.toFixed(1)}x 平均`)) : ""}`
            : summary ? t(msg("피크 데이터 없음", "No peak-day data")) : notice}
        />
      </dl>
    </SectionCard>
  );
}
