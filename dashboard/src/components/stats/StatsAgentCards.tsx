import { BarChart3, Gauge, Users } from "lucide-react";
import type { Agent } from "../../types";
import type { TFunction } from "../dashboard/model";
import { DashboardEmptyState } from "../dashboard/ui";
import AgentAvatar from "../AgentAvatar";
import {
  formatCurrency,
  formatPercent,
  formatTokens,
  msg,
  type AgentCacheRow,
  type AgentSkillRow,
  type AgentSpendRow,
  type LeaderboardRow,
  type SkillUsageRow,
} from "./statsModel";
import {
  CardHead,
  NUMERIC_STYLE,
  numericBadgeStyle,
  positiveChipStyle,
} from "./StatsCardPrimitives";

export function AgentSpendCard({
  t,
  loading,
  rows,
  rangeDays,
}: {
  t: TFunction;
  loading: boolean;
  rows: AgentSpendRow[];
  rangeDays: number;
}) {
  const maxCost = Math.max(1, ...rows.map((row) => row.cost));

  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "에이전트별 비용 비교",
            "Spend by Agent",
            "エージェント別コスト比較",
            "按代理比较成本",
          ),
        )}
        subtitle={t(
          msg(
            `${rangeDays}일 누적 지출`,
            `${rangeDays}d accumulated spend`,
            `${rangeDays}日累積支出`,
            `${rangeDays} 天累计支出`,
          ),
        )}
      />

      <div className="card-body">
        {rows.length === 0 ? (
          <DashboardEmptyState
            icon={<Users size={18} />}
            title={
              loading
                ? t(
                    msg(
                      "에이전트 비용을 불러오는 중입니다.",
                      "Loading agent spend.",
                    ),
                  )
                : t(
                    msg(
                      "표시할 에이전트 비용 데이터가 없습니다.",
                      "No agent spend data available.",
                    ),
                  )
            }
          />
        ) : (
          <div className="flex flex-col gap-3">
            {rows.map((row, index) => (
              <div
                key={row.id}
                className="grid grid-cols-[20px_minmax(0,1fr)_auto] items-center gap-3"
              >
                <span
                  className="inline-grid h-5 w-5 place-items-center rounded-full text-[10px]"
                  style={{
                    background: "var(--th-overlay-subtle)",
                    color: "var(--th-text-muted)",
                    ...NUMERIC_STYLE,
                  }}
                >
                  {index + 1}
                </span>
                <div className="min-w-0">
                  <div className="mb-1 flex items-center gap-2">
                    <span
                      className="truncate text-sm font-medium"
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
                      {formatTokens(row.tokens)} · {formatPercent(row.share)}
                    </span>
                  </div>
                  <div className="bar-track" style={{ height: 5 }}>
                    <div
                      className="bar-fill"
                      style={{
                        width: `${(row.cost / maxCost) * 100}%`,
                        background: row.color,
                      }}
                    />
                  </div>
                </div>
                <div
                  className="min-w-[68px] text-right text-sm font-semibold"
                  style={{ color: "var(--th-text-heading)", ...NUMERIC_STYLE }}
                >
                  {formatCurrency(row.cost)}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </article>
  );
}

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
    <article className="card">
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
          <span className="chip" style={positiveChipStyle}>
            {formatPercent(overallCacheHitRate)}{" "}
            {t(msg("전체", "overall", "全体", "整体"))}
          </span>
        }
      />

      <div className="card-body">
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
                    <div className="bar-track" style={{ height: 5 }}>
                      <div
                        className="bar-fill"
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
    <article className="card">
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
          <span className="chip" style={numericBadgeStyle}>
            {windowCalls.toLocaleString()}{" "}
            {t(msg("calls", "calls", "calls", "calls"))}
          </span>
        }
      />

      <div className="card-body">
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
          <div className="grid grid-2 gap-3">
            <div>
              <div className="list-section">
                {t(msg("상위 스킬", "Top Skills", "上位スキル", "高频技能"))}
              </div>
              <div className="space-y-3">
                {rows.slice(0, 6).map((row, index) => (
                  <div key={`${row.id}-${index}`} className="list-card">
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
                    <div className="bar-track mt-3" style={{ height: 5 }}>
                      <div
                        className="bar-fill"
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
              <div className="list-section">
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
                    <div key={`${row.id}-${index}`} className="list-card tight">
                      <div className="flex items-start justify-between gap-3">
                        <div className="min-w-0">
                          <div
                            className="truncate text-sm font-semibold"
                            style={{ color: "var(--th-text-heading)" }}
                          >
                            {row.agentName}
                          </div>
                          <div
                            className="mt-1 truncate text-[11px]"
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

export function AgentLeaderboardCard({
  t,
  rows,
  agents,
}: {
  t: TFunction;
  rows: LeaderboardRow[];
  agents?: Agent[];
}) {
  const maxTokens = Math.max(1, ...rows.map((row) => row.tokens));

  return (
    <article className="card">
      <CardHead
        title={t(
          msg(
            "에이전트 리더보드",
            "Agent Leaderboard",
            "エージェントリーダーボード",
            "代理排行榜",
          ),
        )}
        subtitle={t(
          msg(
            "핵심 생산성 지표를 에이전트 기준으로 정리했습니다.",
            "Core productivity signals organized by agent.",
            "主要な生産性指標をエージェント基準で整理しました。",
            "按代理整理核心生产力指标。",
          ),
        )}
        actions={
          <span className="chip" style={numericBadgeStyle}>
            {rows.length} {t(msg("agents", "agents", "agents", "agents"))}
          </span>
        }
      />

      <div className="card-body">
        {rows.length === 0 ? (
          <DashboardEmptyState
            icon={<Users size={18} />}
            title={t(
              msg(
                "표시할 에이전트 리더보드가 없습니다.",
                "No agent leaderboard available.",
              ),
            )}
          />
        ) : (
          <div className="flex flex-col gap-3">
            {rows.map((row, index) => (
              <div key={row.id} className="list-card">
                <div className="flex items-center gap-3">
                  <span
                    className="inline-grid h-6 w-6 place-items-center rounded-full text-[10px] font-semibold"
                    style={{
                      background: "var(--th-overlay-light)",
                      color: "var(--th-text-secondary)",
                      ...NUMERIC_STYLE,
                    }}
                  >
                    {index + 1}
                  </span>
                  <span
                    className="inline-grid h-9 w-9 place-items-center overflow-hidden rounded-full"
                    style={{ background: "var(--th-overlay-subtle)" }}
                  >
                    <AgentAvatar agent={row.agent ?? undefined} agents={agents} size={32} rounded="full" />
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center justify-between gap-3">
                      <div className="min-w-0">
                        <div
                          className="truncate text-sm font-semibold"
                          style={{ color: "var(--th-text-heading)" }}
                        >
                          {row.label}
                        </div>
                        <div
                          className="mt-1 flex flex-wrap gap-x-3 gap-y-1 text-[10.5px]"
                          style={{
                            color: "var(--th-text-muted)",
                            ...NUMERIC_STYLE,
                          }}
                        >
                          <span>{row.tasksDone} tasks</span>
                          <span>{formatTokens(row.xp)} xp</span>
                        </div>
                      </div>
                      <div
                        className="shrink-0 text-right text-sm font-semibold"
                        style={{
                          color: "var(--th-text-heading)",
                          ...NUMERIC_STYLE,
                        }}
                      >
                        {formatTokens(row.tokens)}
                      </div>
                    </div>
                    <div className="bar-track mt-3" style={{ height: 5 }}>
                      <div
                        className="bar-fill"
                        style={{
                          width: `${Math.max((row.tokens / maxTokens) * 100, row.tokens > 0 ? 4 : 0)}%`,
                          background:
                            "linear-gradient(90deg, var(--claude), color-mix(in oklch, var(--claude) 58%, white 42%))",
                        }}
                      />
                    </div>
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </article>
  );
}
