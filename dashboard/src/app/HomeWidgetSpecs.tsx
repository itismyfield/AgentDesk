import { Flame, Gauge, Sparkles, Zap } from "lucide-react";

import AgentAvatar from "../components/AgentAvatar";
import { AgentQualityWidget } from "../components/dashboard/ExtraWidgets";
import { RoutinesTimelineWidget } from "../components/dashboard/RoutinesTimelineWidget";
import { MiniRateLimitBar } from "../components/dashboard/MiniRateLimitBar";
import { DailyMissions, StreakCounter } from "../components/gamification/GamificationShared";
import { HomeMetricTile, HomeWidgetShell } from "./HomeOverviewWidgets";
import { HomeWorkSnapshot } from "./HomeWorkSnapshot";

export function buildHomeWidgetSpecs(ctx: any) {
  const {
    activityItems,
    activityStreak,
    agents,
    analytics,
    costTrend,
    dailyMissions,
    fallbackActivity,
    formatCompact,
    formatCurrency,
    gamificationLeader,
    gamificationLevel,
    isKo,
    kanbanCards,
    latestAnalyticsDay,
    localeTag,
    missionResetLabel,
    missionXpLabel,
    recentDoneCards,
    reviewQueue,
    stats,
    streakLeader,
    t,
    tokenTrend,
    topAgents,
    tr,
  } = ctx;

  return {
      m_tokens: {
        className: "min-w-0 lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Zap size={14} />}
            title={tr("오늘 토큰", "Today's tokens")}
            // Keep an unresolved aggregate distinct from a measured zero.
            value={analytics ? formatCompact(latestAnalyticsDay?.total_tokens ?? 0) : "…"}
            sub={
              analytics
                ? tr(
                    `7일 평균 ${formatCompact(Math.round(analytics.summary.average_daily_tokens ?? 0))}`,
                    `7d avg ${formatCompact(Math.round(analytics.summary.average_daily_tokens ?? 0))}`,
                  )
                : tr("7일 평균 집계 중", "Loading 7-day average")
            }
            delta={
              analytics?.summary.total_tokens
                ? tr(`7일 ${formatCompact(analytics.summary.total_tokens)}`, `7d ${formatCompact(analytics.summary.total_tokens)}`)
                : undefined
            }
            deltaTone="flat"
            accent="var(--th-accent-primary)"
            trend={tokenTrend}
          />
        ),
      },
      m_cost: {
        className: "min-w-0 lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Sparkles size={14} />}
            title={tr("API 비용", "API cost")}
            value={analytics ? formatCurrency(latestAnalyticsDay?.cost ?? 0) : "…"}
            sub={
              analytics
                ? tr(
                    `캐시 절감 ${formatCurrency(analytics.summary.cache_discount ?? 0)}`,
                    `Cache saved ${formatCurrency(analytics.summary.cache_discount ?? 0)}`,
                  )
                : tr("비용 집계 중", "Loading cost")
            }
            delta={
              analytics?.summary.total_cost != null
                ? tr(`7일 ${formatCurrency(analytics.summary.total_cost)}`, `7d ${formatCurrency(analytics.summary.total_cost)}`)
                : undefined
            }
            deltaTone="flat"
            accent="var(--th-accent-success)"
            trend={costTrend}
          />
        ),
      },
      m_streak: {
        className: "lg:col-span-3",
        render: () => (
          <StreakCounter
            dataTestId="home-streak-counter"
            className="h-full"
            icon={<Flame size={18} />}
            title={tr("연속 활동", "Current streak")}
            value={tr(`${streakLeader?.streak ?? activityStreak}일`, `${streakLeader?.streak ?? activityStreak}d`)}
            subtitle={tr(
              gamificationLeader
                ? `lv.${gamificationLevel.level} · XP ${formatCompact(Math.round(gamificationLeader.stats_xp))}`
                : `${analytics?.summary.active_days ?? 0}일 활성`,
              gamificationLeader
                ? `lv.${gamificationLevel.level} · XP ${formatCompact(Math.round(gamificationLeader.stats_xp))}`
                : `${analytics?.summary.active_days ?? 0} active days`,
            )}
            badgeLabel={tr("streak", "streak")}
            detail={
              streakLeader
                ? tr(`${streakLeader.name} 최장`, `${streakLeader.name} best`)
                : analytics?.summary.active_days
                  ? tr(`${analytics.summary.active_days}/7 활성`, `${analytics.summary.active_days}/7 active`)
                : undefined
            }
            accent="var(--th-accent-danger)"
          />
        ),
      },
      m_rate_limit: {
        className: "col-span-2 min-w-0 lg:col-span-6",
        render: () => (
          <section className="h-full min-w-0 rounded-lg border border-th-border-subtle bg-th-bg-surface px-4 py-4 sm:px-5" aria-label={tr("사용 한도", "Usage limits")}>
            <div className="flex items-center justify-between gap-3 text-xs text-th-text-muted">
              <span className="inline-flex items-center gap-2"><Gauge size={14} />{tr("사용 한도", "Usage limits")}</span>
              <span>{tr("30초마다 갱신", "Updates every 30s")}</span>
            </div>
            <MiniRateLimitBar isKo={isKo} layout="homeSummary" />
          </section>
        ),
      },
      missions: {
        className: "lg:col-span-6",
        render: () => (
          <HomeWidgetShell
            title={tr("데일리 미션", "Daily missions")}
            subtitle={tr(
              "오늘 바로 확인해야 할 운영 우선순위를 정리합니다.",
              "Keep today's operational priorities in view.",
            )}
          >
            <DailyMissions
              dataTestId="home-daily-missions"
              itemTestIdPrefix="home-daily-mission"
              missions={dailyMissions}
              emptyLabel={tr("표시할 데일리 미션이 없습니다.", "No daily missions available.")}
              doneLabel={tr("완료", "Done")}
              progressLabel={tr("진행", "Progress")}
              resetLabel={missionResetLabel}
              totalXpLabel={missionXpLabel}
            />
          </HomeWidgetShell>
        ),
      },
      quality: {
        className: "lg:col-span-6",
        render: () => (
          <AgentQualityWidget
            agents={agents}
            t={t}
            localeTag={localeTag}
            compact
          />
        ),
      },
      roster: {
        className: "lg:col-span-7",
        render: () => (
          <HomeWidgetShell
            title={tr("에이전트 현황", "Agent roster")}
            subtitle={tr("상위 작업 에이전트를 빠르게 훑어봅니다.", "Quick scan of the most active agents.")}
          >
            <div className="space-y-2">
              {topAgents.length === 0 ? (
                <div className="rounded-2xl border px-4 py-8 text-center text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                  {tr("에이전트 통계가 아직 없습니다.", "Agent statistics are not available yet.")}
                </div>
              ) : (
                topAgents.map((agent: any) => (
                  <div key={agent.id} className="grid grid-cols-[auto_1fr_auto] items-center gap-3 rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)" }}>
                    <div className="flex h-10 w-10 items-center justify-center rounded-2xl border" style={{ borderColor: "var(--th-border-subtle)", background: "var(--th-bg-surface)" }}>
                      <AgentAvatar agent={agent} agents={agents} size={32} rounded="2xl" />
                    </div>
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {isKo ? agent.name_ko : agent.name}
                      </div>
                      <div className="mt-1 truncate text-xs" style={{ color: "var(--th-text-muted)" }}>
                        {tr(
                          `${agent.stats_tasks_done}건 완료 · XP ${Math.round(agent.stats_xp).toLocaleString()}`,
                          `${agent.stats_tasks_done} tasks done · XP ${Math.round(agent.stats_xp).toLocaleString()}`,
                        )}
                      </div>
                    </div>
                    <div className="text-right text-xs" style={{ color: "var(--th-text-muted)" }}>
                      <div className="font-semibold" style={{ color: "var(--th-text-primary)" }}>
                        {agent.stats_tokens > 0 ? `${Math.round(agent.stats_tokens / 1000).toLocaleString()}K` : "0"}
                      </div>
                      <div>{tr("tokens", "tokens")}</div>
                    </div>
                  </div>
                ))
              )}
            </div>
          </HomeWidgetShell>
        ),
      },
      activity: {
        className: "lg:col-span-5",
        render: () => {
          const items = activityItems.length > 0 ? activityItems : fallbackActivity;
          return (
            <HomeWidgetShell
              title={tr("최근 활동", "Recent activity")}
              subtitle={tr("알림과 회의 후속을 우선적으로 보여줍니다.", "Prioritizes alerts and meeting follow-ups.")}
            >
              <div className="space-y-2">
                {items.length === 0 ? (
                  <div className="rounded-2xl border px-4 py-8 text-center text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                    {tr("표시할 최근 활동이 없습니다.", "No recent activity to show.")}
                  </div>
                ) : (
                  items.map((item: any) => (
                    <div key={item.id} className="grid grid-cols-[auto_1fr_auto] items-start gap-3 rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)" }}>
                      <span className="mt-1 h-2.5 w-2.5 rounded-full" style={{ background: item.accent }} />
                      <div className="min-w-0">
                        <div className="text-sm leading-6" style={{ color: "var(--th-text-primary)" }}>
                          {item.title}
                        </div>
                      </div>
                      <div className="text-[11px] whitespace-nowrap" style={{ color: "var(--th-text-muted)" }}>
                        {item.meta}
                      </div>
                    </div>
                  ))
                )}
              </div>
            </HomeWidgetShell>
          );
        },
      },
      kanban: {
        className: "col-span-2 min-w-0 lg:col-span-12",
        render: () => <HomeWorkSnapshot cards={kanbanCards} recentDoneCards={recentDoneCards} isKo={isKo} />,
      },
      routines: {
        className: "col-span-2 min-w-0 lg:col-span-12",
        render: () => (
          <RoutinesTimelineWidget
            variant="home"
            t={t}
            localeTag={localeTag}
            language={isKo ? "ko" : "en"}
          />
        ),
      },

  };
}
