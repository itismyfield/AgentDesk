import { BarChart3, Gauge, Users } from "lucide-react";
import type { Agent } from "../../types";
import type { TFunction } from "../dashboard/model";
import { DashboardEmptyState } from "../dashboard/ui";
import {
  formatCurrency,
  formatPercent,
  formatTokens,
  msg,
  type AgentCacheRow,
  type AgentSkillRow,
  type SkillUsageRow,
} from "./statsModel";
import {
  CardHead,
  NUMERIC_STYLE,
  numericBadgeStyle,
  positiveChipStyle,
} from "./StatsCardPrimitives";

export function AgentCacheCard({
  t,
  loading,
  rows,
  overallCacheHitRate,
}: {
  t: TFunction;
  loading: boolean;
  rows: AgentCacheRow[];
  overallCacheHitRate: number;
}) {
  return (
    <article className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
      <CardHead
        title={t(
          msg(
            "에이전트별 캐시 히트율",
            "Cache Hit Rate by Agent",
            "エージェント別キャッシュヒット率",
            "按代理的缓存命中率",
          ),
        )}
        subtitle={t(
          msg(
            "prompt 볼륨이 큰 에이전트 우선",
            "Ordered by prompt-heavy agents",
            "prompt ボリュームが大きいエージェント優先",
            "优先显示 prompt 量大的代理",
          ),
        )}
        actions={
          <span className="inline-flex max-w-full flex-wrap items-center gap-1 rounded-md border border-th-border-subtle bg-surface-subtle px-2 py-1 text-xs tabular-nums text-th-text-secondary" style={positiveChipStyle}>
            {formatPercent(overallCacheHitRate)}{" "}
            {t(msg("전체", "overall", "全体", "整体"))}
          </span>
        }
      />

      <div className="px-4 py-4 sm:px-5">
        {rows.length === 0 ? (
          <DashboardEmptyState
            icon={<Gauge size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "에이전트 캐시 데이터를 불러오는 중입니다.",
                      "Loading agent cache data.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 에이전트 캐시 데이터가 없습니다.",
                      "No agent cache data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="flex flex-col gap-4">
            {rows.map((row, index) => {
              const sub =
                row.savedCost != null
                  ? t(
                      msg(
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} 절감`,
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} saved`,
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} 節約`,
                        `${formatTokens(row.promptTokens)} prompt · ${formatCurrency(row.savedCost)} 已节省`,
                      ),
                    )
                  : t(
                      msg(
                        `${formatTokens(row.promptTokens)} prompt`,
                        `${formatTokens(row.promptTokens)} prompt`,
                        `${formatTokens(row.promptTokens)} prompt`,
                        `${formatTokens(row.promptTokens)} prompt`,
                      ),
                    );

              return (
                <div
                  key={row.id}
                  className="grid grid-cols-[24px_minmax(0,1fr)_auto] items-center gap-3"
                >
                  <span
                    className="inline-grid h-5 w-5 place-items-center rounded-full text-[10px] font-semibold"
                    style={{
                      background: "var(--codex-soft)",
                      color: "var(--codex)",
                      ...NUMERIC_STYLE,
                    }}
                  >
                    {index + 1}
                  </span>
                  <div className="min-w-0">
                    <div className="mb-1 flex flex-wrap items-baseline gap-x-2 gap-y-1">
                      <span
                        className="text-sm font-medium"
                        style={{ color: "var(--th-text-heading)" }}
                      >
                        {row.label}
                      </span>
                      <span
                        className="text-[10.5px]"
                        style={{
                          color: "var(--th-text-muted)",
                          ...NUMERIC_STYLE,
                        }}
                      >
                        {sub}
                      </span>
                    </div>
                    <div className="h-1.5 overflow-hidden rounded bg-surface-medium" style={{ height: 5 }}>
                      <div
                        className="h-full rounded transition-[width] duration-300 motion-reduce:transition-none"
                        style={{
                          width: `${Math.max(row.hitRate, row.hitRate > 0 ? 4 : 0)}%`,
                          background:
                            "linear-gradient(90deg, var(--codex), color-mix(in oklch, var(--codex) 60%, white 40%))",
                        }}
                      />
                    </div>
                  </div>
                  <div
                    className="min-w-[56px] text-right text-sm font-semibold"
                    style={{ color: "var(--ok)", ...NUMERIC_STYLE }}
                  >
                    {formatPercent(row.hitRate)}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </article>
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

  return (
    <article className="min-w-0 overflow-hidden rounded-lg border border-th-border-subtle bg-th-bg-surface">
      <CardHead
        title={t(msg("스킬 사용", "Skill Usage", "スキル使用量", "技能使用"))}
        subtitle={t(
          msg(
            "현재 기간에 가장 자주 호출된 스킬",
            "Most-invoked skills in the selected period",
            "選択期間で最も多く呼ばれたスキル",
            "所选期间调用最多的技能",
          ),
        )}
        actions={
          <span className="inline-flex max-w-full flex-wrap items-center gap-1 rounded-md border border-th-border-subtle bg-surface-subtle px-2 py-1 text-xs tabular-nums text-th-text-secondary" style={numericBadgeStyle}>
            {windowCalls.toLocaleString()}{" "}
            {t(msg("calls", "calls", "calls", "calls"))}
          </span>
        }
      />

      <div className="px-4 py-4 sm:px-5">
        {rows.length === 0 && byAgentRows.length === 0 ? (
          <DashboardEmptyState
            icon={<BarChart3 size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "스킬 사용량을 불러오는 중입니다.",
                      "Loading skill usage.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 스킬 사용 데이터가 없습니다.",
                      "No skill usage data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="grid grid-cols-1 gap-4 lg:grid-cols-2 gap-3">
            <div>
              <div className="mb-2 text-xs font-medium text-th-text-muted">
                {t(msg("상위 스킬", "Top Skills", "上位スキル", "高频技能"))}
              </div>
              <div className="space-y-3">
                {rows.slice(0, 6).map((row, index) => (
                  <div key={`${row.id}-${index}`} className="min-w-0 border-b border-th-border-subtle py-3 last:border-b-0">
                    <div className="flex items-start justify-between gap-3">
                      <div className="min-w-0">
                        <div
                          className="text-sm font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {row.name}
                        </div>
                        <div
                          className="mt-1 line-clamp-2 text-[11px] leading-5"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {row.description ||
                            t(msg("설명 없음", "No description"))}
                        </div>
                      </div>
                      <div className="text-right" style={{ ...NUMERIC_STYLE }}>
                        <div
                          className="text-base font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {row.windowCalls.toLocaleString()}
                        </div>
                        <div
                          className="text-[10.5px]"
                          style={{ color: "var(--th-text-muted)" }}
                        >
                          {t(msg("calls", "calls", "calls", "calls"))}
                        </div>
                      </div>
                    </div>
                    <div className="h-1.5 overflow-hidden rounded bg-surface-medium mt-3" style={{ height: 5 }}>
                      <div
                        className="h-full rounded transition-[width] duration-300 motion-reduce:transition-none"
                        style={{
                          width: `${Math.max((row.windowCalls / maxCalls) * 100, row.windowCalls > 0 ? 4 : 0)}%`,
                          background:
                            "linear-gradient(90deg, var(--accent), color-mix(in oklch, var(--accent) 62%, white 38%))",
                        }}
                      />
                    </div>
                  </div>
                ))}
              </div>
            </div>

            <div>
              <div className="mb-2 text-xs font-medium text-th-text-muted">
                {t(
                  msg(
                    "에이전트별 상위 조합",
                    "Top Agent-Skill Pairs",
                    "エージェント別上位組み合わせ",
                    "代理-技能高频组合",
                  ),
                )}
              </div>
              <div className="space-y-3">
                {byAgentRows.length === 0 ? (
                  <DashboardEmptyState
                    icon={<Users size={18} />}
                    title={t(
                      msg(
                        "에이전트별 스킬 데이터가 없습니다.",
                        "No agent-skill data available.",
                      ),
                    )}
                  />
                ) : (
                  byAgentRows.map((row, index) => (
                    <div key={`${row.id}-${index}`} className="min-w-0 border-b border-th-border-subtle py-3 last:border-b-0">
                      <div className="flex items-start justify-between gap-3">
                        <div className="min-w-0">
                          <div
                            className="[overflow-wrap:anywhere] text-sm font-semibold"
                            style={{ color: "var(--th-text-heading)" }}
                          >
                            {row.agentName}
                          </div>
                          <div
                            className="mt-1 [overflow-wrap:anywhere] text-[11px]"
                            style={{ color: "var(--th-text-secondary)" }}
                          >
                            {row.skillName}
                          </div>
                          <div
                            className="mt-1 line-clamp-2 text-[11px] leading-5"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {row.description ||
                              t(msg("설명 없음", "No description"))}
                          </div>
                        </div>
                        <div
                          className="text-right"
                          style={{ ...NUMERIC_STYLE }}
                        >
                          <div
                            className="text-sm font-semibold"
                            style={{ color: "var(--th-text-heading)" }}
                          >
                            {row.calls.toLocaleString()}
                          </div>
                          <div
                            className="text-[10.5px]"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {t(msg("calls", "calls", "calls", "calls"))}
                          </div>
                        </div>
                      </div>
                    </div>
                  ))
                )}
              </div>
            </div>
          </div>
        )}
      </div>
    </article>
  );
}

