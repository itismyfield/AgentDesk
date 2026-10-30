import { Flame, Sparkles } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import * as api from "../api/client";
import { localeName, useI18n } from "../i18n";
import type { Agent, CompanySettings, DashboardStats } from "../types";
import { getAgentLevel, getAgentTitle } from "./agent-manager/agentProgress";
import { AchievementDetailDrawer } from "./achievements/AchievementDetailDrawer";
import { AchievementAvatar } from "./achievements/AchievementCard";
import { AchievementsHero } from "./achievements/AchievementsHero";
import { AchievementsSections } from "./achievements/AchievementsSections";
import {
  DailyMissions,
  StreakCounter,
  getMissionResetCountdown,
  getMissionTotalXp,
} from "./gamification/GamificationShared";
import {
  MILESTONE_BY_ID,
  MILESTONES,
  buildDailyMissions,
  formatAchievementDate,
  formatExact,
  nextMilestone,
  normalizeRarity,
  progressToMilestone,
  type AchievementCardModel,
  type LeaderboardEntry,
  type MilestoneId,
} from "./achievementsModel";

interface AchievementsPageProps {
  settings: CompanySettings;
  stats?: DashboardStats | null;
  agents?: Agent[];
  onSelectAgent?: (agent: Agent) => void;
}

export default function AchievementsPage({
  settings,
  stats,
  agents = [],
  onSelectAgent,
}: AchievementsPageProps) {
  const { language, locale, t } = useI18n(settings.language);
  const isKo = language === "ko";
  const [response, setResponse] = useState<api.AchievementsResponse>({
    achievements: [],
    daily_missions: [],
  });
  const [streaks, setStreaks] = useState<api.AgentStreak[]>([]);
  const [selectedCard, setSelectedCard] = useState<AchievementCardModel | null>(null);

  useEffect(() => {
    let alive = true;

    const load = async () => {
      try {
        const [nextAchievements, nextStreaks] = await Promise.all([
          api.getAchievements(),
          api.getStreaks().catch(() => ({ streaks: [] })),
        ]);
        if (!alive) return;
        setResponse(nextAchievements);
        setStreaks([...nextStreaks.streaks].sort((left, right) => right.streak - left.streak));
      } catch {
        if (!alive) return;
        setResponse({ achievements: [], daily_missions: [] });
        setStreaks([]);
      }
    };

    void load();
    const timer = window.setInterval(() => {
      void load();
    }, 5 * 60 * 1000);

    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, []);

  const achievements = response.achievements;
  const agentMap = useMemo(
    () => new Map(agents.map((agent) => [agent.id, agent])),
    [agents],
  );

  const leaderboard = useMemo<LeaderboardEntry[]>(() => {
    if (stats?.top_agents?.length) {
      return stats.top_agents.map((entry) => {
        const agent = agentMap.get(entry.id);
        return {
          id: entry.id,
          name: agent
            ? localeName(language, agent)
            : language === "ko"
              ? entry.name_ko
              : entry.name,
          avatarEmoji: agent?.avatar_emoji || entry.avatar_emoji || "🤖",
          xp: entry.stats_xp,
          tasksDone: entry.stats_tasks_done,
          agent,
        };
      });
    }

    return [...agents]
      .sort(
        (left, right) =>
          right.stats_xp - left.stats_xp || right.stats_tasks_done - left.stats_tasks_done,
      )
      .map((agent) => ({
        id: agent.id,
        name: localeName(language, agent),
        avatarEmoji: agent.avatar_emoji || "🤖",
        xp: agent.stats_xp,
        tasksDone: agent.stats_tasks_done,
        agent,
      }));
  }, [agentMap, agents, language, stats]);

  const hero = leaderboard[0];
  const heroAgent = hero?.agent ?? agents[0];
  const heroXp = hero?.xp ?? heroAgent?.stats_xp ?? 0;
  const heroLevel = getAgentLevel(heroXp);
  const heroTitle = getAgentTitle(heroXp, isKo);
  const heroNextXp = Number.isFinite(heroLevel.nextThreshold)
    ? Math.max(0, heroLevel.nextThreshold - heroXp)
    : 0;
  const totalOrgXp = agents.reduce((sum, agent) => sum + agent.stats_xp, 0);
  const uniqueEarners = new Set(achievements.map((achievement) => achievement.agent_id)).size;
  const streakLeader = streaks[0] ?? null;

  const dailyMissions = useMemo(
    () => buildDailyMissions(response.daily_missions, isKo),
    [response.daily_missions, isKo],
  );
  const missionReset = useMemo(() => getMissionResetCountdown(), []);
  const missionResetLabel = isKo
    ? `리셋까지 ${missionReset.hours}시간 ${missionReset.minutes}분`
    : `Resets in ${missionReset.hours}h ${missionReset.minutes}m`;
  const missionXpLabel = `+${getMissionTotalXp(dailyMissions)} XP`;

  const earnedCards = useMemo<AchievementCardModel[]>(() => {
    return [...achievements]
      .sort((left, right) => right.earned_at - left.earned_at)
      .map((achievement) => {
        const milestone = MILESTONE_BY_ID.get(achievement.type as MilestoneId);
        const agent = agentMap.get(achievement.agent_id);
        const agentName = agent
          ? localeName(language, agent)
          : isKo
            ? achievement.agent_name_ko
            : achievement.agent_name;
        const threshold =
          achievement.progress?.threshold ?? milestone?.threshold ?? 0;
        const rarity = normalizeRarity(achievement.rarity, threshold);

        return {
          id: achievement.id,
          section: "earned",
          milestoneId: achievement.type,
          name: isKo ? achievement.name : milestone?.name.en ?? achievement.name,
          desc:
            isKo
              ? achievement.description || milestone?.desc.ko || ""
              : milestone?.desc.en || achievement.description || "",
          xp: threshold,
          rarity,
          at: formatAchievementDate(achievement.earned_at, locale),
          agentName,
          avatarEmoji: achievement.avatar_emoji || agent?.avatar_emoji || "🤖",
          agent,
          detailLines: [
            isKo
              ? `${agentName}가 이 업적을 달성했습니다`
              : `${agentName} unlocked this achievement`,
            isKo
              ? `획득 XP ${formatExact(threshold, locale)}`
              : `Reward XP ${formatExact(threshold, locale)}`,
          ],
          timelineLines: [
            achievement.earned_at
              ? isKo
                ? `${formatAchievementDate(achievement.earned_at, locale)} · 업적 달성`
                : `${formatAchievementDate(achievement.earned_at, locale)} · unlocked`
              : isKo
                ? "업적 달성 기록"
                : "achievement recorded",
            isKo
              ? `${agentName} · ${formatExact(threshold, locale)} XP milestone`
              : `${agentName} · ${formatExact(threshold, locale)} XP milestone`,
          ],
        };
      });
  }, [achievements, agentMap, isKo, language, locale]);

  const progressCards = useMemo<AchievementCardModel[]>(() => {
    return leaderboard
      .slice(0, 6)
      .map((entry): AchievementCardModel | null => {
        const milestone = nextMilestone(entry.xp);
        if (!milestone) return null;
        const remaining = Math.max(0, milestone.threshold - entry.xp);
        const card: AchievementCardModel = {
          id: `${entry.id}:${milestone.id}:progress`,
          section: "progress",
          milestoneId: milestone.id,
          name: t(milestone.name),
          desc: isKo
            ? `${entry.name} · ${formatExact(remaining, locale)} XP 남음`
            : `${entry.name} · ${formatExact(remaining, locale)} XP left`,
          xp: milestone.threshold,
          rarity: normalizeRarity(null, milestone.threshold),
          progress: progressToMilestone(entry.xp, milestone),
          currentXp: entry.xp,
          remainingXp: remaining,
          agentName: entry.name,
          avatarEmoji: entry.avatarEmoji,
          agent: entry.agent,
          detailLines: [
            isKo
              ? `현재 XP ${formatExact(entry.xp, locale)}`
              : `Current XP ${formatExact(entry.xp, locale)}`,
            isKo
              ? `다음 목표 ${formatExact(milestone.threshold, locale)} XP`
              : `Next target ${formatExact(milestone.threshold, locale)} XP`,
          ],
          timelineLines: [
            isKo
              ? `${entry.name} · 진행률 ${Math.round(progressToMilestone(entry.xp, milestone) * 100)}%`
              : `${entry.name} · ${Math.round(progressToMilestone(entry.xp, milestone) * 100)}% progress`,
            isKo
              ? `${formatExact(remaining, locale)} XP 남음`
              : `${formatExact(remaining, locale)} XP remaining`,
          ],
        };
        return card;
      })
      .filter((item): item is AchievementCardModel => item !== null);
  }, [isKo, leaderboard, locale, t]);

  const earnedMilestoneIds = useMemo(
    () => new Set(earnedCards.map((card) => card.milestoneId)),
    [earnedCards],
  );
  const progressMilestoneIds = useMemo(
    () => new Set(progressCards.map((card) => card.milestoneId)),
    [progressCards],
  );

  const lockedCards = useMemo<AchievementCardModel[]>(() => {
    return MILESTONES.filter(
      (milestone) =>
        !earnedMilestoneIds.has(milestone.id) && !progressMilestoneIds.has(milestone.id),
    )
      .slice(0, 6)
      .map((milestone) => {
        const remaining = Math.max(0, milestone.threshold - heroXp);
        return {
          id: `locked:${milestone.id}`,
          section: "locked",
          milestoneId: milestone.id,
          name: t(milestone.name),
          desc: isKo
            ? `다음 후보 · ${formatExact(remaining, locale)} XP 남음`
            : `Up next · ${formatExact(remaining, locale)} XP left`,
          xp: milestone.threshold,
          rarity: normalizeRarity(null, milestone.threshold),
          progress: 0,
          currentXp: heroXp,
          remainingXp: remaining,
          detailLines: [
            isKo
              ? `현재 대표 XP ${formatExact(heroXp, locale)}`
              : `Current lead XP ${formatExact(heroXp, locale)}`,
            isKo
              ? `${formatExact(milestone.threshold, locale)} XP 도달 시 해금`
              : `Unlocks at ${formatExact(milestone.threshold, locale)} XP`,
          ],
          timelineLines: [
            isKo
              ? `${formatExact(milestone.threshold, locale)} XP threshold`
              : `${formatExact(milestone.threshold, locale)} XP threshold`,
            isKo ? "아직 달성 전" : "Not unlocked yet",
          ],
        };
      });
  }, [earnedMilestoneIds, heroXp, isKo, locale, progressMilestoneIds, t]);

  const heroUnlockedCount = earnedCards.filter((card) => card.agent?.id === heroAgent?.id).length;
  const allCards = useMemo(
    () => [...earnedCards, ...progressCards, ...lockedCards],
    [earnedCards, progressCards, lockedCards],
  );
  const leaderboardSnapshot = leaderboard.slice(0, 4);

  return (
    <div
      data-testid="achievements-page"
      className="mx-auto h-full w-full max-w-[88rem] min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-8 sm:p-8"
      style={{
        paddingBottom: "max(2rem, env(safe-area-inset-bottom))",
      }}
    >
      <div className="flex flex-col gap-5">
        <AchievementsHero
          isKo={isKo}
          locale={locale}
          heroName={hero?.name}
          heroXp={heroXp}
          heroTitle={heroTitle}
          heroNextXp={heroNextXp}
          heroLevel={heroLevel}
          earnedCount={earnedCards.length}
          totalCount={earnedCards.length + progressCards.length + lockedCards.length}
          uniqueEarners={uniqueEarners}
          totalOrgXp={totalOrgXp}
          title={t({ ko: "업적", en: "Achievements" })}
          subtitle={t({
            ko: "획득한 업적과 다음 목표, 에이전트의 활동 기록을 확인합니다.",
            en: "Review unlocked milestones, next goals, and agent activity.",
          })}
          boardLabel={isKo ? "업적 보드" : "Achievement Board"}
          unlockedLabel={t({ ko: "달성", en: "Unlocked" })}
          agentsLabel={t({ ko: "에이전트", en: "Agents" })}
        />

        <AchievementsSections
          sections={[
            {
              key: "earned",
              title: t({ ko: "획득", en: "Unlocked" }),
              items: earnedCards,
            },
            {
              key: "progress",
              title: t({ ko: "진행 중", en: "In Progress" }),
              items: progressCards,
            },
            {
              key: "locked",
              title: t({ ko: "잠김", en: "Locked" }),
              items: lockedCards,
            },
          ]}
          isKo={isKo}
          progressLabel={t({ ko: "진행", en: "progress" })}
          agents={agents}
          onOpen={setSelectedCard}
        />

        <div className="grid min-w-0 gap-4 xl:grid-cols-[minmax(0,1.2fr)_minmax(0,0.8fr)]" data-testid="achievements-grid-bottom">
          <div
            data-testid="achievements-daily-missions"
            className="rounded-lg border p-4 sm:p-5"
            style={{
              borderColor: "var(--th-border-subtle, var(--line))",
              background:
                "var(--th-bg-surface)",
            }}
          >
            <div className="mb-4 flex items-center justify-between gap-3">
              <div>
                <div
                  className="text-[13px] font-medium"
                  style={{ color: "var(--th-text-secondary, var(--fg))" }}
                >
                  {t({ ko: "데일리 미션", en: "Daily missions" })}
                </div>
                <div
                  className="mt-1 text-[11px] leading-5"
                  style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                >
                  {t({
                    ko: "오늘의 목표와 완료 현황입니다.",
                    en: "Today’s goals and completion status.",
                  })}
                </div>
              </div>
              <Sparkles
                size={16}
                style={{ color: "var(--th-accent-primary, var(--accent))" }}
              />
            </div>
            <DailyMissions
              missions={dailyMissions}
              emptyLabel={t({
                ko: "표시할 데일리 미션이 없습니다.",
                en: "No daily missions available.",
              })}
              doneLabel={t({ ko: "완료", en: "Done" })}
              progressLabel={t({ ko: "진행", en: "Progress" })}
              resetLabel={missionResetLabel}
              totalXpLabel={missionXpLabel}
            />
          </div>

          <div className="flex flex-col gap-4">
            <div data-testid="achievements-streak">
              <StreakCounter
                title={t({ ko: "연속 활동", en: "Current streak" })}
                value={isKo ? `${streakLeader?.streak ?? heroUnlockedCount}일` : `${streakLeader?.streak ?? heroUnlockedCount}d`}
                subtitle={
                  streakLeader
                    ? isKo
                      ? `${streakLeader.name} 최장 기록`
                      : `${streakLeader.name} longest run`
                    : isKo
                      ? `${heroTitle} · lv.${heroLevel.level}`
                      : `${heroTitle} · lv.${heroLevel.level}`
                }
                detail={
                  streakLeader
                    ? isKo
                      ? `마지막 활동 ${streakLeader.last_active || "-"}`
                      : `Last active ${streakLeader.last_active || "-"}`
                    : isKo
                      ? `현재 대표 XP ${formatExact(heroXp, locale)}`
                      : `Current lead XP ${formatExact(heroXp, locale)}`
                }
                badgeLabel={isKo ? "활성" : "Live"}
                icon={<Flame size={18} />}
              />
            </div>

            <div
              data-testid="achievements-ranking"
              className="rounded-lg border p-4 sm:p-5"
              style={{
                borderColor: "var(--th-border-subtle, var(--line))",
                background:
                  "var(--th-bg-surface)",
              }}
            >
              <div className="mb-3 flex items-center justify-between gap-3">
                <div
                  className="text-[13px] font-medium"
                  style={{ color: "var(--th-text-secondary, var(--fg))" }}
                >
                  {t({ ko: "랭킹 보드", en: "Ranking board" })}
                </div>
                <span
                  className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                  style={{
                    background: "var(--th-overlay-medium, var(--bg-3))",
                    color: "var(--th-text-muted, var(--fg-muted))",
                  }}
                >
                  XP
                </span>
              </div>

              <div className="space-y-2">
                {leaderboardSnapshot.length === 0 ? (
                  <div
                    className="rounded-2xl border px-4 py-5 text-sm"
                    style={{
                      borderColor: "var(--th-border-subtle, var(--line))",
                      color: "var(--th-text-muted, var(--fg-muted))",
                    }}
                  >
                    {t({
                      ko: "아직 XP 집계 대상이 없습니다.",
                      en: "No XP snapshot is available yet.",
                    })}
                  </div>
                ) : (
                  leaderboardSnapshot.map((entry, index) => {
                    const clickable = Boolean(entry.agent && onSelectAgent);
                    return (
                      <button
                        key={entry.id}
                        type="button"
                        className="flex w-full items-center gap-3 rounded-2xl border px-3 py-3 text-left transition-colors hover:bg-white/5"
                        style={{
                          borderColor: "var(--th-border-subtle, var(--line))",
                          background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                          cursor: clickable ? "pointer" : "default",
                        }}
                        onClick={() => {
                          if (entry.agent && onSelectAgent) onSelectAgent(entry.agent);
                        }}
                        disabled={!clickable}
                      >
                        <div
                          className="w-7 text-[11px] font-semibold"
                          style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                        >
                          #{index + 1}
                        </div>
                        <AchievementAvatar
                          agent={entry.agent}
                          agents={agents}
                          emoji={entry.avatarEmoji}
                          label={entry.name}
                        />
                        <div className="min-w-0 flex-1">
                          <div
                            className="truncate text-sm font-semibold"
                            style={{ color: "var(--th-text-heading, var(--fg))" }}
                          >
                            {entry.name}
                          </div>
                          <div
                            className="mt-1 text-[11px]"
                            style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                          >
                            {isKo
                              ? `${formatExact(entry.tasksDone, locale)}개 완료`
                              : `${formatExact(entry.tasksDone, locale)} completed`}
                          </div>
                        </div>
                        <div
                          className="text-right text-[12px] font-semibold"
                          style={{ color: "var(--th-accent-primary, var(--accent))" }}
                        >
                          {formatExact(entry.xp, locale)} XP
                        </div>
                      </button>
                    );
                  })
                )}
              </div>
            </div>
          </div>
        </div>
      </div>

      <AchievementDetailDrawer
        selectedCard={selectedCard}
        isKo={isKo}
        locale={locale}
        t={t}
        agents={agents}
        onClose={() => setSelectedCard(null)}
        onSelectAgent={onSelectAgent}
      />
    </div>
  );
}
