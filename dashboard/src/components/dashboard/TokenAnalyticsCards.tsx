import type { TFunction } from "./model";
import { dashboardCard } from "./ui";
import {
  formatCost,
  formatPercentage,
  formatTokens,
} from "./tokenAnalyticsModels";

export function CacheEfficiencyCard({
  t,
  loading,
  actualCost,
  uncachedCost,
  savings,
  savingsRate,
  hitRate,
  cacheReadTokens,
  promptTokens,
}: {
  t: TFunction;
  loading: boolean;
  actualCost: number;
  uncachedCost: number;
  savings: number;
  savingsRate: number;
  hitRate: number;
  cacheReadTokens: number;
  promptTokens: number;
}) {
  return (
    <div
      className={dashboardCard.standard}
      style={{
        borderColor: "color-mix(in srgb, #22c55e 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-surface) 93%, #22c55e 7%) 0%, var(--th-surface) 100%)",
      }}
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <h3
            className="text-sm font-semibold"
            style={{ color: "var(--th-text)" }}
          >
            {t({
              ko: "캐시 절감 요약",
              en: "Cache Savings Summary",
              ja: "キャッシュ節約サマリー",
              zh: "缓存节省摘要",
            })}
          </h3>
          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "uncached 기준 비용과 실제 결제 비용 차이를 한눈에 봅니다",
              en: "See the gap between uncached baseline and actual billed spend",
              ja: "キャッシュなし基準コストと実課金コストの差をひと目で確認します",
              zh: "一眼查看未缓存基线成本与实际计费成本的差值",
            })}
          </p>
        </div>
        {loading ? (
          <LoadingIndicator
            compact
            label={t({
              ko: "캐시 절감 요약 갱신 중",
              en: "Refreshing cache savings summary",
              ja: "キャッシュ節約サマリーを更新中",
              zh: "刷新缓存节省摘要中",
            })}
          />
        ) : null}
      </div>

      <div className="mt-4 grid gap-3 sm:grid-cols-3">
        <MetricCard
          label={t({
            ko: "절감액",
            en: "Savings",
            ja: "節約額",
            zh: "节省金额",
          })}
          value={formatCost(savings)}
          sub={t({
            ko: `${formatCost(actualCost)} actual spend`,
            en: `${formatCost(actualCost)} actual spend`,
            ja: `${formatCost(actualCost)} actual spend`,
            zh: `${formatCost(actualCost)} actual spend`,
          })}
          accent="#22c55e"
        />
        <MetricCard
          label={t({
            ko: "절감율",
            en: "Savings Rate",
            ja: "節約率",
            zh: "节省率",
          })}
          value={formatPercentage(savingsRate)}
          sub={t({
            ko: `${formatCost(uncachedCost)} uncached baseline`,
            en: `${formatCost(uncachedCost)} uncached baseline`,
            ja: `${formatCost(uncachedCost)} uncached baseline`,
            zh: `${formatCost(uncachedCost)} uncached baseline`,
          })}
          accent="#14b8a6"
        />
        <MetricCard
          label={t({
            ko: "캐시 히트율",
            en: "Cache Hit Rate",
            ja: "キャッシュヒット率",
            zh: "缓存命中率",
          })}
          value={formatPercentage(hitRate)}
          sub={t({
            ko: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
            en: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
            ja: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
            zh: `${formatTokens(cacheReadTokens)} cache reads / ${formatTokens(promptTokens)} prompt`,
          })}
          accent="#f59e0b"
        />
      </div>
    </div>
  );
}

export function LoadingIndicator({
  label,
  compact = false,
}: {
  label: string;
  compact?: boolean;
}) {
  return (
    <span
      role="status"
      aria-label={label}
      title={label}
      className={`inline-flex items-center justify-center rounded-full border ${compact ? "h-6 w-6" : "h-8 w-8"}`}
      style={{
        color: "#f59e0b",
        background: "rgba(245,158,11,0.12)",
        borderColor: "rgba(245,158,11,0.24)",
      }}
    >
      <span
        className={`${compact ? "h-3 w-3" : "h-3.5 w-3.5"} inline-block animate-spin rounded-full border-2 border-current border-t-transparent`}
      />
    </span>
  );
}

export function MetricCard({
  label,
  value,
  sub,
  accent,
}: {
  label: string;
  value: string;
  sub: string;
  accent: string;
}) {
  return (
    <div
      className={dashboardCard.nested}
      style={{
        borderColor: `${accent}26`,
        background: "rgba(15,23,42,0.16)",
      }}
    >
      <div
        className="text-[11px] font-semibold uppercase tracking-[0.18em]"
        style={{ color: "var(--th-text-muted)" }}
      >
        {label}
      </div>
      <div
        className="mt-2 text-2xl font-black tracking-tight"
        style={{ color: accent }}
      >
        {value}
      </div>
      <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
        {sub}
      </div>
    </div>
  );
}
