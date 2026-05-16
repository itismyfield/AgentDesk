import { Link } from "react-router-dom";
import { ChevronRight, Flame, Gauge, Sparkles, Target, Zap } from "lucide-react";

import AgentAvatar from "../components/AgentAvatar";
import { AgentQualityWidget } from "../components/dashboard/ExtraWidgets";
import { MiniRateLimitBar } from "../components/office-view/OfficeInsightPanel";
import { DailyMissions, StreakCounter } from "../components/gamification/GamificationShared";
import { HomeMetricTile, HomeWidgetShell } from "./HomeOverviewWidgets";

export function buildHomeWidgetSpecs(ctx: any) {
  const {
    activityItems,
    activityStreak,
    agents,
    analytics,
    blockedCards,
    costTrend,
    currentOfficeLabel,
    dailyMissions,
    fallbackActivity,
    formatCompact,
    formatCurrency,
    gamificationLeader,
    gamificationLevel,
    inProgressCards,
    inProgressTrend,
    isKo,
    kanbanCards,
    kanbanColumns,
    latestAnalyticsDay,
    localeTag,
    missionResetLabel,
    missionXpLabel,
    recentDoneCards,
    recentDoneCount,
    requestedCards,
    reviewQueue,
    stats,
    streakLeader,
    t,
    tokenTrend,
    topAgents,
    totalActionableCards,
    tr,
  } = ctx;

  return {
      m_tokens: {
        className: "lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Zap size={14} />}
            title={tr("오늘 토큰", "Today's tokens")}
            /* `analytics` is null on the first home visit until
               /api/token-analytics resolves (~9 s cold path on PG-only
               runtimes; ~10 ms once the server-side in-process cache is
               warm). The previous fallback `?? 0` showed a real-looking
               "0" while the fetch was inflight, which made the tile look
               broken. Render the loading placeholder explicitly and
               mark the trend slot as pending too so the dashed line
               doesn't briefly show as the real sparkline. */
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
        className: "lg:col-span-3",
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
      m_progress: {
        className: "lg:col-span-3",
        render: () => (
          <HomeMetricTile
            icon={<Target size={14} />}
            title={tr("진행 중", "In progress")}
            value={`${inProgressCards}`}
            sub={tr(
              `${requestedCards} 요청 · ${reviewQueue} 리뷰 · ${blockedCards} 블록`,
              `${requestedCards} requested · ${reviewQueue} review · ${blockedCards} blocked`,
            )}
            delta={tr(`${totalActionableCards} 전체`, `${totalActionableCards} total`)}
            deltaTone="flat"
            accent="var(--th-accent-warn)"
            trend={inProgressTrend}
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
        className: "lg:col-span-3",
        /* User reported "한도 UI 정보 밀도 낮음" — replace the previous
           single-percentage HomeMetricTile + sparkline with the same
           per-provider/per-bucket gauge rows used by the office "운영신호"
           panel (`MiniRateLimitBar`). One card now shows every provider's
           5h/7d bucket utilization with the same color/glow language as
           /stats, and fetches its own data on a 30 s timer so the home
           tile no longer needs the manual fetch + summary state.
           The header mirrors HomeMetricTile (icon + uppercase title +
           trailing badge slot) and the gauge uses the comfortable density
           so this card's vertical rhythm matches its row neighbours
           (오늘 토큰 / API 비용 / 진행 중). */
        render: () => (
          <div
            className="flex h-full flex-col overflow-hidden rounded-[1.15rem] border"
            style={{
              borderColor: "var(--th-border-subtle)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
            }}
          >
            <div className="flex flex-1 flex-col px-4 py-4 sm:px-5">
              <div className="flex items-center justify-between gap-3">
                <div
                  className="flex items-center gap-2 text-[11.5px] font-medium uppercase tracking-[0.08em]"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  <Gauge size={14} />
                  <span>{tr("한도", "Rate limit")}</span>
                </div>
                <span
                  className="rounded-md px-1.5 py-0.5 text-[11px] font-medium"
                  style={{
                    background: "var(--th-overlay-medium)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  {tr("30s 갱신", "30s refresh")}
                </span>
              </div>
              <div className="mt-auto">
                <MiniRateLimitBar isKo={isKo} density="comfortable" />
              </div>
            </div>
          </div>
        ),
      },
      office: {
        className: "lg:col-span-8",
        render: () => (
          <HomeWidgetShell
            title={tr("오피스 뷰", "Office view")}
            subtitle={tr(
              `${currentOfficeLabel} 기준으로 지금 일하는 에이전트를 요약합니다.`,
              `Summarized live roster for ${currentOfficeLabel}.`,
            )}
            action={
              <Link
                to="/office"
                className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-primary)" }}
              >
                {tr("전체 보기", "Open office")}
                <ChevronRight size={14} />
              </Link>
            }
          >
            <div className="relative overflow-hidden rounded-[1.5rem] border p-4 sm:p-5" style={{ borderColor: "var(--th-border-subtle)", background: "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 92%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 92%, transparent) 100%)" }}>
              <div
                className="pointer-events-none absolute inset-0 opacity-30"
                style={{
                  backgroundImage:
                    "radial-gradient(circle, color-mix(in srgb, var(--th-text-muted) 38%, transparent) 1px, transparent 1px)",
                  backgroundSize: "14px 14px",
                }}
              />
              <div className="relative grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
                {topAgents.length === 0 ? (
                  <div className="col-span-full rounded-2xl border px-4 py-8 text-center text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                    {tr("표시할 활성 에이전트가 없습니다.", "No active agents to show right now.")}
                  </div>
                ) : (
                  topAgents.map((agent: any) => {
                    const progress = Math.min(100, Math.max(12, Math.round(agent.stats_tokens / 100_000)));
                    return (
                      <div key={agent.id} className="rounded-2xl border px-3 py-3 text-center" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)" }}>
                        <div className="mx-auto flex h-12 w-12 items-center justify-center rounded-2xl border" style={{ borderColor: "var(--th-border-subtle)", background: "var(--th-card-bg)" }}>
                          <AgentAvatar agent={agent} agents={agents} size={40} rounded="2xl" />
                        </div>
                        <div className="mt-3 truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                          {isKo ? agent.name_ko : agent.name}
                        </div>
                        <div className="mt-1 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                          {tr(`${agent.stats_tasks_done}건 완료`, `${agent.stats_tasks_done} tasks done`)}
                        </div>
                        <div className="mt-3 h-1.5 rounded-full" style={{ background: "color-mix(in srgb, var(--th-border-subtle) 70%, transparent)" }}>
                          <div className="h-full rounded-full" style={{ width: `${progress}%`, background: "var(--th-accent-primary)" }} />
                        </div>
                      </div>
                    );
                  })
                )}
              </div>
            </div>
          </HomeWidgetShell>
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
        className: "lg:col-span-12",
        render: () => (
          <HomeWidgetShell
            title={tr("칸반 스냅샷", "Kanban snapshot")}
            subtitle={tr("현재 카드 흐름을 한 번에 살피는 요약 보드입니다.", "A wide snapshot of the current card flow.")}
            action={
              <Link
                to="/kanban"
                className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-white/5"
                style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-primary)" }}
              >
                {tr("칸반 열기", "Open kanban")}
                <ChevronRight size={14} />
              </Link>
            }
          >
            <div className="grid gap-3 lg:grid-cols-4">
              {kanbanColumns.map((column: any) => {
                /* The done column shows only the last 24h of shipments
                   (count + preview cards) so the snapshot stays focused
                   on today's throughput rather than the full archive. */
                const cards =
                  column.id === "done"
                    ? recentDoneCards.slice(0, 3)
                    : kanbanCards.filter((card: any) => card.status === column.id).slice(0, 3);
                return (
                  <div key={column.id} className="rounded-[1.5rem] border p-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-card-bg) 90%, transparent)" }}>
                    <div className="flex items-center justify-between gap-2">
                      <div className="text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                        {column.label}
                        {column.id === "done" ? (
                          <span
                            className="ml-1.5 align-middle text-[10px] font-medium uppercase tracking-[0.06em]"
                            style={{ color: "var(--th-text-muted)" }}
                          >
                            {tr("· 최근 24h", "· last 24h")}
                          </span>
                        ) : null}
                      </div>
                      <span className="rounded-full px-2 py-1 text-[11px] font-semibold" style={{ background: "var(--th-overlay-medium)", color: column.accent }}>
                        {column.id === "requested"
                          ? requestedCards
                          : column.id === "in_progress"
                            ? kanbanCards.filter((card: any) => card.status === "in_progress").length
                            : column.id === "review"
                              ? kanbanCards.filter((card: any) => card.status === "review").length
                              : recentDoneCount}
                      </span>
                    </div>
                    <div className="mt-3 space-y-2">
                      {cards.length === 0 ? (
                        <div className="rounded-2xl border px-3 py-4 text-sm" style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)", background: "var(--th-overlay-subtle)" }}>
                          {tr("표시할 카드 없음", "No cards")}
                        </div>
                      ) : (
                        cards.map((card: any) => (
                          <div key={card.id} className="rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle)", background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)" }}>
                            <div className="line-clamp-2 text-sm font-medium leading-6" style={{ color: "var(--th-text-primary)" }}>
                              {card.title}
                            </div>
                            <div className="mt-2 flex items-center justify-between gap-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                              <span className="truncate">
                                {card.github_repo ?? tr("repo 미지정", "No repo")}
                              </span>
                              <span className="whitespace-nowrap">
                                #{card.github_issue_number ?? "—"}
                              </span>
                            </div>
                          </div>
                        ))
                      )}
                    </div>
                  </div>
                );
              })}
            </div>
          </HomeWidgetShell>
        ),
      },

  };
}
