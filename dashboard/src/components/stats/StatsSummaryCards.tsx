import type { TokenAnalyticsResponse } from "../../types";
import type { TFunction } from "../dashboard/model";
import {
  formatCompactDate,
  formatCurrency,
  formatTokens,
  msg,
  type MetricDelta,
} from "./statsModel";
import { Info } from "lucide-react";
import type { ReactNode } from "react";
import { Skeleton } from "../../kit/free/skeleton-clean";
import { DeltaBadge, NUMERIC_STYLE } from "./StatsCardPrimitives";
import type { MetricDeltaTone } from "./statsModel";

/** One cell of the kit stats strip: label with an explainer, value, note and an optional change badge. */
function SummaryCell({
  testId,
  title,
  value,
  sub,
  tip,
  delta,
  deltaTone,
  loading,
  loadingLabel,
}: {
  testId: string;
  title: string;
  value: ReactNode;
  sub: string;
  tip: string;
  delta?: string;
  deltaTone?: MetricDeltaTone;
  loading: boolean;
  loadingLabel: string;
}) {
  return (
    <div data-testid={testId} className="flex min-w-0 flex-col gap-1 bg-[var(--bg-1)] px-4 pb-3.5 pt-2.5">
      <details className="group min-w-0">
        <summary className="flex min-h-7 w-fit max-w-full cursor-pointer list-none items-center gap-1.5 rounded-[var(--r-sm)] text-xs font-medium text-[var(--fg-muted)] pointer-coarse:min-h-11 [&::-webkit-details-marker]:hidden">
          <span className="min-w-0 [word-break:keep-all]">{title}</span>
          <Info size={12} className="shrink-0 opacity-70 group-open:opacity-100" aria-hidden="true" />
        </summary>
        <p className="mb-1 text-xs leading-5 text-[var(--fg-dim)] [word-break:keep-all]">{tip}</p>
      </details>
      <div className="min-w-0 text-[22px] font-semibold leading-7 tracking-tight text-[var(--fg)] [overflow-wrap:anywhere]" style={NUMERIC_STYLE}>
        {loading ? (
          <span className="flex h-7 items-center" role="status" aria-label={loadingLabel}>
            <Skeleton width={88} height={18} radius={5} />
          </span>
        ) : (
          value
        )}
      </div>
      <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs leading-5 text-[var(--fg-muted)]">
        <span className="min-w-0 [overflow-wrap:anywhere]">{sub}</span>
        {delta ? <DeltaBadge tone={deltaTone}>{delta}</DeltaBadge> : null}
      </div>
    </div>
  );
}

export function StatsSummaryGrid({
  t,
  numberFormatter,
  rangeDays,
  summary,
  loading,
  tokenMomentumDelta,
  cacheSavingsDelta,
}: {
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  rangeDays: number;
  summary: TokenAnalyticsResponse["summary"] | null | undefined;
  loading: boolean;
  tokenMomentumDelta: MetricDelta | null;
  cacheSavingsDelta: MetricDelta | null;
}) {
  const missingValue = "—";
  const valueLoading = loading && !summary;
  const missingNotice = t(loading
    ? msg("데이터 불러오는 중", "Loading data", "データを読み込み中", "正在加载数据")
    : msg("데이터 확인 불가", "Data unavailable", "データを確認できません", "数据不可用"));
  const peakDay = summary?.peak_day ?? null;
  const averageDailyTokens = summary?.average_daily_tokens ?? 0;
  const peakRatio =
    peakDay && averageDailyTokens > 0
      ? peakDay.total_tokens / averageDailyTokens
      : null;

  return (
    <div className="grid grid-cols-2 gap-px border-b border-[var(--line-soft)] bg-[var(--line-soft)] @min-[620px]:grid-cols-4" data-testid="stats-summary-grid">
      <SummaryCell
        testId="stats-summary-total-tokens"
        loading={valueLoading}
        loadingLabel={missingNotice}
        title={t(msg("총 토큰", "Total Tokens", "総トークン", "总代币"))}
        value={summary ? formatTokens(summary.total_tokens) : missingValue}
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
            "입력·출력·캐시 읽기/쓰기 토큰을 모두 더한 값",
            "Sum of input + output + cache read / write",
            "input + output + cache read / write の合計",
            "input + output + cache read / write 的总和",
          ),
        )}
        delta={
          tokenMomentumDelta
            ? t(msg(`앞 절반 대비 ${tokenMomentumDelta.value}`, `${tokenMomentumDelta.value} vs first half`, `前半比 ${tokenMomentumDelta.value}`, `较前半 ${tokenMomentumDelta.value}`))
            : undefined
        }
        deltaTone={tokenMomentumDelta?.tone}
      />
      <SummaryCell
        testId="stats-summary-api-spend"
        loading={valueLoading}
        loadingLabel={missingNotice}
        title={t(msg("API 비용", "API Spend", "API コスト", "API 成本"))}
        value={summary ? formatCurrency(summary.total_cost) : missingValue}
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
            : missingNotice
        }
        tip={t(
          msg(
            "캐시 할인을 반영한 실제 결제 비용",
            "Actual spend after cache discounts",
            "キャッシュ割引を反映した実支出",
            "计入缓存折扣后的实际支出",
          ),
        )}
        delta={
          cacheSavingsDelta
            ? t(msg(`캐시로 ${cacheSavingsDelta.value.replace("-", "")} 절약`, `${cacheSavingsDelta.value.replace("-", "")} saved by cache`, `キャッシュで ${cacheSavingsDelta.value.replace("-", "")} 節約`, `缓存节省 ${cacheSavingsDelta.value.replace("-", "")}`))
            : undefined
        }
        deltaTone={cacheSavingsDelta?.tone}
      />
      <SummaryCell
        testId="stats-summary-cache-saved"
        loading={valueLoading}
        loadingLabel={missingNotice}
        title={t(msg("활성 일수", "Active Days", "稼働日数", "活跃天数"))}
        value={
          summary
            ? `${numberFormatter.format(summary.active_days)} / ${numberFormatter.format(rangeDays)}`
            : missingValue
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
            : missingNotice
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
      <SummaryCell
        testId="stats-summary-cache-hit"
        loading={valueLoading}
        loadingLabel={missingNotice}
        title={t(msg("피크 데이", "Peak Day", "ピーク日", "峰值日"))}
        value={peakDay ? formatCompactDate(peakDay.date) : "—"}
        sub={
          peakDay
            ? t(
                msg(
                  `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `평균의 ${peakRatio.toFixed(1)}배` : "평균 대비"}`,
                  `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `${peakRatio.toFixed(1)}x avg` : "vs average"}`,
                  `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `平均の ${peakRatio.toFixed(1)}x` : "平均比"}`,
                  `${formatTokens(peakDay.total_tokens)} · ${peakRatio ? `${peakRatio.toFixed(1)}x 平均` : "相对平均"}`,
                ),
              )
            : summary ? t(msg("피크 데이터 없음", "No peak-day data")) : missingNotice
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
  );
}
