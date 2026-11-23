import { BarChart3, Users } from "lucide-react";
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
}: {
  t: TFunction;
  loading: boolean;
  rows: SkillUsageRow[];
  byAgentRows: AgentSkillRow[];
  windowCalls: number;
}) {
  const maxCalls = Math.max(1, ...rows.map((row) => row.windowCalls));
  const noDescription = t(msg("설명 없음", "No description"));

  return (
    <SectionCard
      title={t(msg("스킬 사용", "Skill Usage", "スキル使用量", "技能使用"))}
      testId="stats-skill-usage"
      aside={
        <Cap className="font-display tabular-nums">
          {t(msg(`이 기간 ${windowCalls.toLocaleString()}회 호출`, `${windowCalls.toLocaleString()} calls in this range`, `この期間 ${windowCalls.toLocaleString()} 回`, `本期 ${windowCalls.toLocaleString()} 次`))}
        </Cap>
      }
    >
      <div className="@container border-t border-[var(--line-soft)]">
        {rows.length === 0 && byAgentRows.length === 0 ? (
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
