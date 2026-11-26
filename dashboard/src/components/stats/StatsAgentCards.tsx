import { BarChart3, RotateCcw, Users } from "lucide-react";
import { Link, useInRouterContext } from "react-router";
import { Button } from "../../kit/free/button-clean";
import { Cap, MeterBar, SectionCard } from "../common/KitPrimitives";
import type { TFunction } from "../dashboard/model";
import { msg, type AgentSkillRow, type SkillUsageRow } from "./statsModel";
import { NUMERIC_STYLE, StatsEmpty } from "./StatsCardPrimitives";

function CallCount({ t, calls }: { t: TFunction; calls: number }) {
  return (
    <span className="shrink-0 text-right" style={NUMERIC_STYLE}>
      <span className="block text-sm font-semibold text-[var(--fg)]">{calls.toLocaleString()}</span>
      <span className="block text-[11px] text-[var(--fg-muted)]">{t(msg("회", "calls", "回", "次"))}</span>
    </span>
  );
}

export function SkillUsageCard({
  t,
  loading,
  rows,
  byAgentRows,
  windowCalls,
  requested = true,
  error = null,
  onRequest,
  onRetry,
}: {
  t: TFunction;
  loading: boolean;
  rows: SkillUsageRow[];
  byAgentRows: AgentSkillRow[];
  windowCalls: number;
  requested?: boolean;
  error?: string | null;
  onRequest?: () => void;
  onRetry?: () => void;
}) {
  const maxCalls = Math.max(1, ...rows.map((row) => row.windowCalls));
  const noDescription = t(msg("설명 없음", "No description"));
  const hasRows = rows.length > 0 || byAgentRows.length > 0;
  const inRouter = useInRouterContext();
  const catalogLinkClass = "text-xs font-medium text-[var(--accent)] hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--accent)]";
  const catalogLabel = t(msg("전체 스킬 카탈로그", "Full skill catalog", "全スキルカタログ", "全部技能目录"));

  return (
    <SectionCard
      title={t(msg("스킬 사용", "Skill Usage", "スキル使用量", "技能使用"))}
      testId="stats-skill-usage"
      aside={
        <span className="ml-auto flex flex-wrap items-center gap-x-3 gap-y-1">
          {requested && (hasRows || !loading) && !error ? (
            <Cap className="font-display tabular-nums">
              {t(msg(`이 기간 ${windowCalls.toLocaleString()}회 호출`, `${windowCalls.toLocaleString()} calls in this range`, `この期間 ${windowCalls.toLocaleString()} 回`, `本期 ${windowCalls.toLocaleString()} 次`))}
            </Cap>
          ) : null}
          {inRouter ? (
            <Link to="/agents?tab=skills" className={catalogLinkClass} data-testid="stats-skill-catalog-link">
              {catalogLabel}
            </Link>
          ) : (
            <a href="/agents?tab=skills" className={catalogLinkClass} data-testid="stats-skill-catalog-link">
              {catalogLabel}
            </a>
          )}
        </span>
      }
    >
      <div className="@container border-t border-[var(--line-soft)]">
        {!requested && !hasRows ? (
          <div className="flex flex-col items-center justify-center gap-2.5 px-4 py-7 text-center">
            <BarChart3 size={18} className="text-[var(--fg-muted)]" aria-hidden="true" />
            <div className="max-w-md">
              <p className="text-sm font-medium text-[var(--fg)]">
                {t(msg("상위 스킬 집계를 필요할 때 불러와요", "Load top skill usage on demand", "上位スキル集計を必要な時に読み込みます", "按需加载高频技能统计"))}
              </p>
              <p className="mt-0.5 text-xs leading-5 text-[var(--fg-muted)]">
                {t(msg("토큰·비용 차트를 먼저 빠르게 표시하기 위해 스킬 랭킹 요청을 분리했어요.", "Skill ranking requests are deferred so token and cost charts render faster."))}
              </p>
            </div>
            {onRequest ? (
              <Button variant="secondary" size="sm" data-testid="stats-skill-load-button" onClick={onRequest}>
                {t(msg("스킬 사용량 불러오기", "Load skill usage", "スキル使用量を読み込む", "加载技能用量"))}
              </Button>
            ) : null}
          </div>
        ) : error && !hasRows ? (
          <div className="flex flex-col items-center justify-center gap-2.5 px-4 py-7 text-center" role="alert" data-testid="stats-skill-error">
            <BarChart3 size={18} className="text-[var(--warn)]" aria-hidden="true" />
            <p className="text-sm font-medium text-[var(--fg)]">{error}</p>
            {onRetry ? (
              <Button variant="secondary" size="sm" icon={<RotateCcw size={14} />} data-testid="stats-skill-retry-button" onClick={onRetry}>
                {t(msg("다시 시도", "Retry", "再試行", "重试"))}
              </Button>
            ) : null}
          </div>
        ) : !hasRows ? (
          <StatsEmpty
            icon={<BarChart3 size={18} />}
            title={
              loading
                ? t(msg("스킬 사용량을 불러오는 중입니다.", "Loading skill usage."))
                : t(msg("표시할 스킬 사용 데이터가 없습니다.", "No skill usage data available."))
            }
          />
        ) : (
          <div className="grid grid-cols-1 divide-y divide-[var(--line-soft)] @min-[720px]:grid-cols-2 @min-[720px]:divide-x @min-[720px]:divide-y-0">
            <div className="min-w-0">
              <h3 className="px-4 pb-1 pt-3 text-xs font-medium text-[var(--fg-muted)]">
                {t(msg("상위 스킬", "Top Skills", "上位スキル", "高频技能"))}
              </h3>
              <ol className="divide-y divide-[var(--line-soft)]">
                {rows.slice(0, 6).map((row, index) => (
                  <li key={`${row.id}-${index}`} className="flex min-w-0 flex-col gap-2 px-4 py-2.5">
                    <div className="flex min-w-0 items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div className="text-[13px] font-medium text-[var(--fg)] [overflow-wrap:anywhere]">{row.name}</div>
                        <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-[var(--fg-muted)]" title={row.description || undefined}>
                          {row.description || noDescription}
                        </p>
                      </div>
                      <CallCount t={t} calls={row.windowCalls} />
                    </div>
                    <MeterBar ratio={row.windowCalls / maxCalls} />
                  </li>
                ))}
              </ol>
            </div>

            <div className="min-w-0">
              <h3 className="px-4 pb-1 pt-3 text-xs font-medium text-[var(--fg-muted)]">
                {t(msg("에이전트별 상위 조합", "Top Agent-Skill Pairs", "エージェント別上位組み合わせ", "代理-技能高频组合"))}
              </h3>
              {byAgentRows.length === 0 ? (
                <StatsEmpty
                  icon={<Users size={18} />}
                  title={t(msg("에이전트별 스킬 데이터가 없습니다.", "No agent-skill data available."))}
                />
              ) : (
                <ol className="divide-y divide-[var(--line-soft)]">
                  {byAgentRows.map((row, index) => (
                    <li key={`${row.id}-${index}`} className="flex min-w-0 items-start justify-between gap-3 px-4 py-2.5">
                      <div className="min-w-0">
                        <div className="text-[13px] font-medium text-[var(--fg)] [overflow-wrap:anywhere]">{row.agentName}</div>
                        <div className="mt-0.5 text-xs text-[var(--fg-dim)] [overflow-wrap:anywhere]">{row.skillName}</div>
                        <p className="mt-0.5 line-clamp-2 text-xs leading-5 text-[var(--fg-muted)]" title={row.description || undefined}>
                          {row.description || noDescription}
                        </p>
                      </div>
                      <CallCount t={t} calls={row.calls} />
                    </li>
                  ))}
                </ol>
              )}
            </div>
          </div>
        )}
      </div>
    </SectionCard>
  );
}
