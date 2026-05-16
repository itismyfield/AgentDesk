import type { TokenAnalyticsResponse } from "../../types";
import type { TFunction } from "../dashboard/model";
import {
  formatCompactDate,
  formatCurrency,
  formatTokens,
  msg,
  type MetricDelta,
} from "./statsModel";
import { HeadlineMetricCard } from "./StatsCardPrimitives";

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
