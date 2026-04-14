import AgentAvatar from "../AgentAvatar";
import type { Agent } from "../../types";
import { getRankTier, RankBadge, XpBar, type TFunction } from "./model";
import { cx, dashboardBadge, dashboardCard, dashboardText } from "./ui";

export interface HudStat {
  id: string;
  label: string;
  value: number | string;
  sub: string;
  color: string;
  icon: string;
}

export interface RankedAgent {
  id: string;
  name: string;
  department: string;
  tasksDone: number;
  xp: number;
}

interface DashboardHeroHeaderProps {
  companyName: string;
  t: TFunction;
}

export function DashboardHeroHeader({
  companyName,
  t,
}: DashboardHeroHeaderProps) {
  return (
    <div
      className={cx(dashboardCard.accentHero, "relative overflow-hidden")}
      style={{
        borderColor: "var(--color-info-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 94%, var(--color-info) 6%), var(--th-card-bg))",
      }}
    >
      <div
        className="pointer-events-none absolute inset-0"
        style={{
          background: "repeating-linear-gradient(0deg, transparent, transparent 2px, var(--color-neutral-soft) 2px, var(--color-neutral-soft) 4px)",
        }}
      />

      <div className="relative">
        <div className="space-y-1.5">
          <h1 className="dashboard-title-gradient text-2xl font-black tracking-tight sm:text-3xl">{companyName}</h1>
          <p className="text-xs sm:text-sm" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "핵심 운영 상태와 토큰 흐름만 빠르게 확인합니다",
              en: "Track only the key operations and token flow at a glance",
              ja: "主要な運用状態とトークン推移だけを素早く確認します",
              zh: "只快速查看关键运营状态与 Token 流向",
            })}
          </p>
        </div>
      </div>
    </div>
  );
}

interface DashboardHudStatsProps {
  hudStats: HudStat[];
  numberFormatter: Intl.NumberFormat;
}

export function DashboardHudStats({ hudStats, numberFormatter }: DashboardHudStatsProps) {
  return (
    <div className="grid grid-cols-2 gap-1.5 sm:grid-cols-4 sm:gap-3">
      {hudStats.map((stat) => (
        <div
          key={stat.id}
          className={cx(dashboardCard.standard, "group relative overflow-hidden transition-all duration-300 hover:-translate-y-0.5")}
          style={{ borderColor: `color-mix(in srgb, ${stat.color} 24%, transparent)` }}
        >
          <div
            className="absolute top-0 left-0 right-0 h-[2px] opacity-60"
            style={{ background: `linear-gradient(90deg, transparent, ${stat.color}, transparent)` }}
          />
          <div className="relative flex items-center justify-between">
            <div className="min-w-0">
              <p className={cx(dashboardText.label, "truncate")} style={{ color: "var(--th-text-muted)" }}>
                {stat.label}
              </p>
              <p
                className="mt-0.5 sm:mt-1 text-lg sm:text-3xl font-black tracking-tight"
                style={{ color: stat.color, textShadow: `0 0 20px color-mix(in srgb, ${stat.color} 26%, transparent)` }}
              >
                {typeof stat.value === "number" ? numberFormatter.format(stat.value) : stat.value}
              </p>
              <p className="hidden sm:block mt-0.5 text-xs" style={{ color: "var(--th-text-muted)" }}>
                {stat.sub}
              </p>
            </div>
            <span
              className="hidden sm:inline text-3xl opacity-20 transition-all duration-300 group-hover:opacity-40 group-hover:scale-110"
              style={{ filter: `drop-shadow(0 0 8px color-mix(in srgb, ${stat.color} 28%, transparent))` }}
            >
              {stat.icon}
            </span>
          </div>
        </div>
      ))}
    </div>
  );
}

interface DashboardRankingBoardProps {
  topAgents: RankedAgent[];
  podiumOrder: RankedAgent[];
  agentMap: Map<string, Agent>;
  agents: Agent[];
  maxXp: number;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
  onSelectAgent?: (agent: Agent) => void;
}

export function DashboardRankingBoard({
  topAgents,
  podiumOrder,
  agentMap,
  agents,
  maxXp,
  numberFormatter,
  t,
  onSelectAgent,
}: DashboardRankingBoardProps) {
  return (
    <div
      className={cx(dashboardCard.accentHero, "relative overflow-hidden")}
      style={{
        borderColor: "var(--color-warning-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 93%, var(--color-warning) 7%), var(--th-card-bg))",
      }}
    >
      <div
        className="pointer-events-none absolute inset-0"
        style={{
          background: "linear-gradient(180deg, color-mix(in srgb, var(--color-warning) 8%, transparent), transparent 64%)",
        }}
      />

      <div className="relative mb-6 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <span
            className="text-2xl animate-crown-wiggle"
            style={{ display: "inline-block", filter: "drop-shadow(0 0 8px color-mix(in srgb, var(--color-warning) 52%, transparent))" }}
          >
            🏆
          </span>
          <div>
            <h2 className="dashboard-ranking-gradient text-lg font-black uppercase tracking-widest">
              {t({ ko: "랭킹 보드", en: "RANKING BOARD", ja: "ランキングボード", zh: "排行榜" })}
            </h2>
            <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {t({
                ko: "XP 기준 에이전트 순위",
                en: "Agent ranking by XP",
                ja: "XP 基準のエージェント順位",
                zh: "按 XP 排名",
              })}
            </p>
          </div>
        </div>
        <span
          className={dashboardBadge.default}
          style={{
            border: "1px solid var(--color-neutral-border)",
            background: "var(--th-overlay-subtle)",
            color: "var(--th-text-secondary)",
          }}
        >
          TOP {topAgents.length}
        </span>
      </div>

      {topAgents.length === 0 ? (
        <div
          className="flex min-h-[200px] flex-col items-center justify-center gap-3 text-sm"
          style={{ color: "var(--th-text-muted)" }}
        >
          <span className="text-4xl opacity-30">⚔️</span>
          <p>
            {t({
              ko: "등록된 에이전트가 없습니다",
              en: "No agents registered",
              ja: "登録されたエージェントがいません",
              zh: "暂无已注册代理",
            })}
          </p>
          <p className="text-xs">
            {t({
              ko: "에이전트를 추가하고 미션을 시작하세요",
              en: "Add agents and start missions",
              ja: "エージェントを追加してミッションを開始しましょう",
              zh: "添加代理并开始任务",
            })}
          </p>
        </div>
      ) : (
        <div className="relative space-y-5">
          {topAgents.length >= 2 && (
            <div className="flex items-end justify-center gap-4 pb-3 pt-2 sm:gap-6">
              {podiumOrder.map((agent, visualIdx) => {
                const ranks = topAgents.length >= 3 ? [2, 1, 3] : [2, 1];
                const rank = ranks[visualIdx];
                const tier = getRankTier(agent.xp);
                const isFirst = rank === 1;
                const selectedAgent = agentMap.get(agent.id);
                const avatarSize = isFirst ? 64 : 48;
                const podiumHeight = isFirst ? "h-24" : rank === 2 ? "h-16" : "h-12";

                return (
                  <div
                    key={agent.id}
                    className={`flex flex-col items-center gap-2 ${isFirst ? "animate-rank-float" : ""}`}
                  >
                    {rank === 1 && (
                      <span
                        className="text-2xl animate-crown-wiggle"
                        style={{ display: "inline-block", filter: "drop-shadow(0 0 12px color-mix(in srgb, var(--color-warning) 60%, transparent))" }}
                      >
                        🥇
                      </span>
                    )}
                    {rank === 2 && (
                      <span className="text-lg" style={{ filter: "drop-shadow(0 0 6px color-mix(in srgb, var(--th-text-secondary) 50%, transparent))" }}>
                        🥈
                      </span>
                    )}
                    {rank === 3 && (
                      <span className="text-lg" style={{ filter: "drop-shadow(0 0 6px color-mix(in srgb, var(--color-warning) 56%, var(--color-danger) 44%))" }}>
                        🥉
                      </span>
                    )}

                    {selectedAgent && onSelectAgent ? (
                      <button
                        type="button"
                        onClick={() => onSelectAgent(selectedAgent)}
                        className="flex flex-col items-center gap-2 rounded-xl text-left transition-transform duration-300 hover:scale-105 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2"
                        style={{ outlineColor: tier.color }}
                      >
                        <div
                          className="relative overflow-hidden rounded-2xl"
                          style={{
                            boxShadow: isFirst
                              ? `0 0 20px ${tier.glow}, 0 0 40px ${tier.glow}`
                              : `0 0 12px ${tier.glow}`,
                            border: `2px solid ${tier.color}80`,
                          }}
                        >
                          <AgentAvatar agent={selectedAgent} agents={agents} size={avatarSize} rounded="2xl" />
                        </div>
                        <span
                          className={`max-w-[80px] truncate text-center font-bold ${isFirst ? "text-sm" : "text-xs"}`}
                          style={{ color: tier.color, textShadow: isFirst ? `0 0 8px ${tier.glow}` : "none" }}
                        >
                          {agent.name}
                        </span>
                      </button>
                    ) : (
                      <>
                        <div
                          className="relative overflow-hidden rounded-2xl transition-transform duration-300 hover:scale-105"
                          style={{
                            boxShadow: isFirst
                              ? `0 0 20px ${tier.glow}, 0 0 40px ${tier.glow}`
                              : `0 0 12px ${tier.glow}`,
                            border: `2px solid ${tier.color}80`,
                          }}
                        >
                          <AgentAvatar agent={selectedAgent} agents={agents} size={avatarSize} rounded="2xl" />
                        </div>

                        <span
                          className={`max-w-[80px] truncate text-center font-bold ${isFirst ? "text-sm" : "text-xs"}`}
                          style={{ color: tier.color, textShadow: isFirst ? `0 0 8px ${tier.glow}` : "none" }}
                        >
                          {agent.name}
                        </span>
                      </>
                    )}

                    <div className="flex flex-col items-center gap-1">
                      <span
                        className="font-mono text-xs font-bold"
                        style={{ color: tier.color, textShadow: `0 0 6px ${tier.glow}` }}
                      >
                        {numberFormatter.format(agent.xp)} XP
                      </span>
                      <RankBadge xp={agent.xp} size="default" />
                    </div>

                    <div
                      className={`${podiumHeight} flex w-20 items-center justify-center rounded-t-xl sm:w-24 animate-podium-rise`}
                      style={{
                        background: `linear-gradient(to bottom, ${tier.color}30, ${tier.color}10)`,
                        border: `1px solid ${tier.color}40`,
                        borderBottom: "none",
                        boxShadow: `inset 0 1px 0 ${tier.color}30, 0 -4px 12px ${tier.glow}`,
                      }}
                    >
                      <span className="text-2xl font-black" style={{ color: `${tier.color}50` }}>
                        #{rank}
                      </span>
                    </div>
                  </div>
                );
              })}
            </div>
          )}

          {topAgents.length > 3 && (
            <div className="space-y-2 border-t pt-4" style={{ borderTopColor: "var(--th-border-subtle)" }}>
              {topAgents.slice(3).map((agent, idx) => {
                const rank = idx + 4;
                const tier = getRankTier(agent.xp);
                const selectedAgent = agentMap.get(agent.id);
                return (
                  <div
                    key={agent.id}
                    className={cx(dashboardCard.interactiveNestedCompact, "group flex items-center gap-3 hover:translate-x-1")}
                    style={{ borderLeftWidth: "3px", borderLeftColor: `${tier.color}60` }}
                  >
                    <span className="w-8 text-center font-mono text-sm font-black" style={{ color: `${tier.color}80` }}>
                      #{rank}
                    </span>
                    {selectedAgent && onSelectAgent ? (
                      <button
                        type="button"
                        onClick={() => onSelectAgent(selectedAgent)}
                        className="flex min-w-0 flex-1 items-center gap-3 rounded-xl text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2"
                        style={{ outlineColor: tier.color }}
                      >
                        <div
                          className="flex-shrink-0 overflow-hidden rounded-xl"
                          style={{ border: `1px solid ${tier.color}40` }}
                        >
                          <AgentAvatar agent={selectedAgent} agents={agents} size={36} rounded="xl" />
                        </div>
                        <div className="min-w-0 flex-1">
                          <p className="truncate text-sm font-bold" style={{ color: "var(--th-text-primary)" }}>
                            {agent.name}
                          </p>
                          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                            {agent.department || t({ ko: "미지정", en: "Unassigned", ja: "未指定", zh: "未指定" })}
                          </p>
                        </div>
                      </button>
                    ) : (
                      <>
                        <div
                          className="flex-shrink-0 overflow-hidden rounded-xl"
                          style={{ border: `1px solid ${tier.color}40` }}
                        >
                          <AgentAvatar agent={selectedAgent} agents={agents} size={36} rounded="xl" />
                        </div>
                        <div className="min-w-0 flex-1">
                          <p className="truncate text-sm font-bold" style={{ color: "var(--th-text-primary)" }}>
                            {agent.name}
                          </p>
                          <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                            {agent.department || t({ ko: "미지정", en: "Unassigned", ja: "未指定", zh: "未指定" })}
                          </p>
                        </div>
                      </>
                    )}
                    <div className="hidden w-28 sm:block">
                      <XpBar xp={agent.xp} maxXp={maxXp} color={tier.color} />
                    </div>
                    <div className="flex items-center gap-2">
                      <span className="font-mono text-xs font-bold" style={{ color: tier.color }}>
                        {numberFormatter.format(agent.xp)}
                      </span>
                      <RankBadge xp={agent.xp} size="default" />
                    </div>
                  </div>
                );
              })}
            </div>
          )}

          {topAgents.length === 1 &&
            (() => {
              const agent = topAgents[0];
              const tier = getRankTier(agent.xp);
              const selectedAgent = agentMap.get(agent.id);
              return (
                <div
                  className={cx(dashboardCard.nested, "flex items-center gap-4")}
                  style={{
                    background: `linear-gradient(135deg, ${tier.color}15, transparent)`,
                    border: `1px solid ${tier.color}30`,
                    boxShadow: `0 0 20px ${tier.glow}`,
                  }}
                >
                  <span className="text-2xl animate-crown-wiggle" style={{ display: "inline-block" }}>
                    🥇
                  </span>
                  {selectedAgent && onSelectAgent ? (
                    <button
                      type="button"
                      onClick={() => onSelectAgent(selectedAgent)}
                      className="flex min-w-0 flex-1 items-center gap-4 rounded-xl text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2"
                      style={{ outlineColor: tier.color }}
                    >
                      <div
                        className="overflow-hidden rounded-2xl"
                        style={{ border: `2px solid ${tier.color}60`, boxShadow: `0 0 15px ${tier.glow}` }}
                      >
                        <AgentAvatar agent={selectedAgent} agents={agents} size={52} rounded="2xl" />
                      </div>
                      <div className="min-w-0 flex-1">
                        <p className="text-base font-black" style={{ color: tier.color }}>
                          {agent.name}
                        </p>
                        <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {agent.department || t({ ko: "미지정", en: "Unassigned", ja: "未指定", zh: "未指定" })}
                        </p>
                      </div>
                    </button>
                  ) : (
                    <>
                      <div
                        className="overflow-hidden rounded-2xl"
                        style={{ border: `2px solid ${tier.color}60`, boxShadow: `0 0 15px ${tier.glow}` }}
                      >
                        <AgentAvatar agent={selectedAgent} agents={agents} size={52} rounded="2xl" />
                      </div>
                      <div className="min-w-0 flex-1">
                        <p className="text-base font-black" style={{ color: tier.color }}>
                          {agent.name}
                        </p>
                        <p className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {agent.department || t({ ko: "미지정", en: "Unassigned", ja: "未指定", zh: "未指定" })}
                        </p>
                      </div>
                    </>
                  )}
                  <div className="text-right">
                    <p
                      className="font-mono text-lg font-black"
                      style={{ color: tier.color, textShadow: `0 0 10px ${tier.glow}` }}
                    >
                      {numberFormatter.format(agent.xp)} XP
                    </p>
                    <RankBadge xp={agent.xp} size="large" />
                  </div>
                </div>
              );
            })()}
        </div>
      )}
    </div>
  );
}
