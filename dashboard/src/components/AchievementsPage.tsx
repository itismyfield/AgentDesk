import { ChevronRight, Flame, Lock, Sparkles, Trophy } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import * as api from "../api/client";
import { localeName, useI18n } from "../i18n";
import type { Agent, CompanySettings, DashboardStats } from "../types";
import { getAgentLevel, getAgentTitle } from "./agent-manager/AgentInfoCard";
import { Drawer } from "./common/overlay";
import {
  DailyMissions,
  LevelRing,
  StreakCounter,
  getMissionResetCountdown,
  getMissionTotalXp,
  type DailyMissionViewModel,
} from "./gamification/GamificationShared";

interface AchievementsPageProps {
  settings: CompanySettings;
  stats?: DashboardStats | null;
  agents?: Agent[];
  onSelectAgent?: (agent: Agent) => void;
}

type MilestoneId =
  | "first_task"
  | "getting_started"
  | "centurion"
  | "veteran"
  | "expert"
  | "master";

type CardSection = "earned" | "progress" | "locked";
type Rarity = "common" | "rare" | "epic" | "legendary";

interface MilestoneMeta {
  id: MilestoneId;
  threshold: number;
  hue: number;
  glyph: string;
  name: {
    ko: string;
    en: string;
  };
  desc: {
    ko: string;
    en: string;
  };
}

interface LeaderboardEntry {
  id: string;
  name: string;
  avatarEmoji: string;
  xp: number;
  tasksDone: number;
  agent?: Agent;
}

interface AchievementCardModel {
  id: string;
  section: CardSection;
  milestoneId: string;
  name: string;
  desc: string;
  xp: number;
  rarity: Rarity;
  progress?: number;
  currentXp?: number;
  remainingXp?: number;
  at?: string;
  agentName?: string;
  avatarEmoji?: string;
  agent?: Agent;
  detailLines: string[];
  timelineLines: string[];
}

const MILESTONES: MilestoneMeta[] = [
  {
    id: "first_task",
    threshold: 10,
    hue: 150,
    glyph: "★",
    name: { ko: "첫 번째 작업 완료", en: "First Task" },
    desc: { ko: "첫 번째 작업을 성공적으로 완료했습니다", en: "Completed the first task" },
  },
  {
    id: "getting_started",
    threshold: 50,
    hue: 210,
    glyph: "✦",
    name: { ko: "본격적인 시작", en: "Getting Started" },
    desc: { ko: "운영 리듬이 안정적으로 시작되었습니다", en: "Settled into the operating rhythm" },
  },
  {
    id: "centurion",
    threshold: 100,
    hue: 85,
    glyph: "100",
    name: { ko: "100 XP 달성", en: "Centurion" },
    desc: { ko: "100 XP를 돌파해 첫 이정표를 넘었습니다", en: "Reached the first 100 XP milestone" },
  },
  {
    id: "veteran",
    threshold: 250,
    hue: 25,
    glyph: "V",
    name: { ko: "베테랑", en: "Veteran" },
    desc: { ko: "꾸준한 운영으로 베테랑 단계에 도달했습니다", en: "Reached the veteran tier through steady work" },
  },
  {
    id: "expert",
    threshold: 500,
    hue: 295,
    glyph: "E",
    name: { ko: "전문가", en: "Expert" },
    desc: { ko: "고난도 운영을 소화하는 전문가 구간입니다", en: "Reached the expert operating tier" },
  },
  {
    id: "master",
    threshold: 1000,
    hue: 50,
    glyph: "∞",
    name: { ko: "마스터", en: "Master" },
    desc: { ko: "최상위 운영 숙련도에 도달했습니다", en: "Reached the master tier" },
  },
];

const MILESTONE_BY_ID = new Map(MILESTONES.map((milestone) => [milestone.id, milestone]));

function formatCompact(value: number, locale: string): string {
  return new Intl.NumberFormat(locale, {
    notation: "compact",
    maximumFractionDigits: value >= 1000 ? 1 : 0,
  }).format(value);
}

function formatExact(value: number, locale: string): string {
  return new Intl.NumberFormat(locale).format(value);
}

function formatAchievementDate(timestamp: number | null | undefined, locale: string) {
  if (!timestamp || timestamp <= 0) return undefined;
  return new Intl.DateTimeFormat(locale, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function nextMilestone(xp: number) {
  return MILESTONES.find((milestone) => xp < milestone.threshold) ?? null;
}

function progressToMilestone(xp: number, milestone: MilestoneMeta): number {
  const index = MILESTONES.findIndex((item) => item.id === milestone.id);
  const previousThreshold = index > 0 ? MILESTONES[index - 1].threshold : 0;
  const total = Math.max(1, milestone.threshold - previousThreshold);
  return Math.max(0, Math.min(1, (xp - previousThreshold) / total));
}

function normalizeRarity(raw: string | null | undefined, threshold: number): Rarity {
  const normalized = raw?.toLowerCase();
  if (
    normalized === "common" ||
    normalized === "rare" ||
    normalized === "epic" ||
    normalized === "legendary"
  ) {
    return normalized;
  }
  if (threshold >= 1000) return "legendary";
  if (threshold >= 500) return "epic";
  if (threshold >= 100) return "rare";
  return "common";
}

function getRarityTheme(rarity: Rarity) {
  switch (rarity) {
    case "legendary":
      return {
        accent: "var(--th-accent-warn, var(--warn))",
        border: "color-mix(in srgb, var(--th-accent-warn, var(--warn)) 26%, var(--th-border-subtle, var(--line)) 74%)",
        badgeBg: "color-mix(in srgb, var(--th-accent-warn, var(--warn)) 12%, transparent)",
        badgeText: "var(--th-accent-warn, var(--warn))",
      };
    case "epic":
      return {
        accent: "var(--th-accent-danger, var(--codex))",
        border: "color-mix(in srgb, var(--th-accent-danger, var(--codex)) 24%, var(--th-border-subtle, var(--line)) 76%)",
        badgeBg: "color-mix(in srgb, var(--th-accent-danger, var(--codex)) 12%, transparent)",
        badgeText: "var(--th-accent-danger, var(--codex))",
      };
    case "rare":
      return {
        accent: "var(--th-accent-primary, var(--accent))",
        border: "color-mix(in srgb, var(--th-accent-primary, var(--accent)) 22%, var(--th-border-subtle, var(--line)) 78%)",
        badgeBg: "color-mix(in srgb, var(--th-accent-primary, var(--accent)) 12%, transparent)",
        badgeText: "var(--th-accent-primary, var(--accent))",
      };
    case "common":
    default:
      return {
        accent: "var(--th-text-muted, var(--fg-muted))",
        border: "var(--th-border-subtle, color-mix(in srgb, var(--line) 70%, transparent))",
        badgeBg: "var(--th-overlay-medium, color-mix(in srgb, var(--bg-3) 90%, transparent))",
        badgeText: "var(--th-text-muted, var(--fg-muted))",
      };
  }
}

function rarityLabel(rarity: Rarity, isKo: boolean) {
  switch (rarity) {
    case "legendary":
      return isKo ? "전설" : "Legendary";
    case "epic":
      return isKo ? "에픽" : "Epic";
    case "rare":
      return isKo ? "레어" : "Rare";
    case "common":
    default:
      return isKo ? "기본" : "Common";
  }
}

function BadgeIcon({
  milestoneId,
  rarity,
  achieved,
}: {
  milestoneId: string;
  rarity: Rarity;
  achieved: boolean;
}) {
  const meta = MILESTONE_BY_ID.get(milestoneId as MilestoneId);
  const glyph = meta?.glyph ?? "?";
  const theme = getRarityTheme(rarity);

  return (
    <div style={{ width: 48, height: 48, position: "relative", flexShrink: 0 }}>
      <div
        style={{
          position: "absolute",
          inset: 0,
          background: achieved
            ? `linear-gradient(135deg, ${theme.accent}, color-mix(in srgb, ${theme.accent} 60%, black))`
            : "var(--bg-3)",
          clipPath: "polygon(50% 0, 100% 25%, 100% 75%, 50% 100%, 0 75%, 0 25%)",
          boxShadow: achieved
            ? `0 0 18px color-mix(in srgb, ${theme.accent} 28%, transparent)`
            : "none",
        }}
      />
      <div
        style={{
          position: "absolute",
          inset: 3,
          background: achieved
            ? "color-mix(in srgb, var(--bg-1) 28%, transparent)"
            : "var(--bg-2)",
          clipPath: "polygon(50% 0, 100% 25%, 100% 75%, 50% 100%, 0 75%, 0 25%)",
          display: "grid",
          placeItems: "center",
          color: achieved ? theme.accent : "var(--fg-faint)",
          fontSize: glyph.length > 1 ? 11 : 18,
          fontFamily: "var(--font-pixel)",
          fontWeight: 700,
        }}
      >
        {glyph}
      </div>
    </div>
  );
}

function AchievementAvatar({
  emoji,
  label,
}: {
  emoji: string;
  label: string;
}) {
  return (
    <div
      title={label}
      aria-label={label}
      style={{
        width: 28,
        height: 28,
        borderRadius: 999,
        display: "grid",
        placeItems: "center",
        background: "color-mix(in srgb, var(--bg-3) 88%, transparent)",
        border: "1px solid color-mix(in srgb, var(--line) 70%, transparent)",
        fontSize: 14,
      }}
    >
      {emoji || "🤖"}
    </div>
  );
}

function AchievementCard({
  item,
  isKo,
  progressLabel,
  onOpen,
}: {
  item: AchievementCardModel;
  isKo: boolean;
  progressLabel: string;
  onOpen: (item: AchievementCardModel) => void;
}) {
  const theme = getRarityTheme(item.rarity);

  return (
    <button
      type="button"
      data-testid={`achievement-card-${item.section}-${item.id}`}
      className="achievement-card"
      onClick={() => onOpen(item)}
      style={{
        position: "relative",
        overflow: "hidden",
        borderRadius: 18,
        border: `1px solid ${theme.border}`,
        background: "color-mix(in srgb, var(--bg-2) 94%, transparent)",
        padding: 16,
        textAlign: "left",
        opacity: item.section === "locked" ? 0.8 : 1,
        transition: "transform 0.16s ease, border-color 0.16s ease, background 0.16s ease",
      }}
    >
      <div
        style={{
          position: "absolute",
          right: 0,
          top: 0,
          width: 48,
          height: 48,
          background: `radial-gradient(circle at top right, color-mix(in srgb, ${theme.accent} 24%, transparent), transparent 72%)`,
        }}
      />
      <div className="flex items-start gap-3">
        <BadgeIcon
          milestoneId={item.milestoneId}
          rarity={item.rarity}
          achieved={item.section === "earned"}
        />
        <div className="min-w-0 flex-1">
          <div className="mb-1 flex items-center gap-2">
            <div className="truncate text-sm font-semibold" style={{ color: "var(--fg)" }}>
              {item.name}
            </div>
            <span
              className="shrink-0 rounded-full px-2 py-0.5 text-[10px] font-semibold"
              style={{
                marginLeft: "auto",
                background: theme.badgeBg,
                color: theme.badgeText,
              }}
            >
              {rarityLabel(item.rarity, isKo)}
            </span>
          </div>
          <div className="text-[11.5px] leading-5" style={{ color: "var(--fg-muted)" }}>
            {item.desc}
          </div>
          {item.agentName ? (
            <div
              className="mt-3 inline-flex items-center gap-2 text-[10.5px]"
              style={{ color: "var(--fg-faint)" }}
            >
              <AchievementAvatar
                emoji={item.avatarEmoji || "🤖"}
                label={item.agentName}
              />
              <span className="truncate">{item.agentName}</span>
            </div>
          ) : null}
          {item.section !== "earned" && typeof item.progress === "number" ? (
            <div className="mt-3">
              <div
                className="overflow-hidden rounded-full"
                style={{
                  height: 4,
                  background: "color-mix(in srgb, var(--bg-3) 90%, transparent)",
                }}
              >
                <div
                  style={{
                    height: "100%",
                    width: `${Math.round(item.progress * 100)}%`,
                    background: theme.accent,
                  }}
                />
              </div>
              <div
                className="mt-2 text-[10px]"
                style={{ color: "var(--fg-faint)", fontFamily: "var(--font-mono)" }}
              >
                {Math.round(item.progress * 100)}% {progressLabel}
              </div>
            </div>
          ) : null}
          <div className="mt-3 flex items-center justify-between gap-3 text-[10.5px]">
            <span style={{ color: theme.badgeText, fontFamily: "var(--font-mono)" }}>
              +{item.xp} XP
            </span>
            <span style={{ color: "var(--fg-faint)" }}>
              {item.section === "earned"
                ? item.at
                : item.section === "progress"
                  ? item.remainingXp != null
                    ? isKo
                      ? `${item.remainingXp.toLocaleString()} XP 남음`
                      : `${item.remainingXp.toLocaleString()} XP left`
                    : null
                  : isKo
                    ? "세부 보기"
                    : "View details"}
            </span>
          </div>
        </div>
      </div>
    </button>
  );
}

function buildDailyMissions(
  missions: api.DailyMission[],
  isKo: boolean,
): DailyMissionViewModel[] {
  return missions.map((mission) => {
    switch (mission.id) {
      case "dispatches_today":
        return {
          id: mission.id,
          label: isKo ? "오늘 디스패치 5건 완료" : "Complete 5 dispatches today",
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
          description: isKo ? "오늘 실제 완료된 디스패치 수" : "Completed dispatches today",
          xp: 40,
        };
      case "active_agents_today":
        return {
          id: mission.id,
          label: isKo ? "오늘 3명 이상 출항" : "Get 3 agents shipping today",
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
          description: isKo
            ? "오늘 완료 기록이 있는 에이전트 수"
            : "Agents with completed work today",
          xp: 35,
        };
      case "review_queue_zero":
        return {
          id: mission.id,
          label: isKo ? "리뷰 큐 비우기" : "Drain the review queue",
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
          description: isKo
            ? "리뷰 대기 카드를 0으로 유지"
            : "Keep the review queue empty",
          xp: 40,
        };
      default:
        return {
          id: mission.id,
          label: mission.label,
          current: mission.current,
          target: mission.target,
          completed: mission.completed,
        };
    }
  });
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
      className="mx-auto h-full w-full max-w-[1440px] min-w-0 overflow-x-hidden overflow-y-auto px-4 pb-40 pt-4 sm:px-6"
      style={{
        paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))",
      }}
    >
      <style>{`
        .achievements-fidelity-page {
          display: flex;
          flex-direction: column;
          gap: 18px;
          animation: achievements-fade-in 0.28s ease-out;
        }

        .achievements-grid {
          display: grid;
          gap: 14px;
          grid-template-columns: repeat(2, minmax(0, 1fr));
        }

        .achievements-grid.bottom {
          grid-template-columns: minmax(0, 1.2fr) minmax(320px, 0.8fr);
        }

        .achievement-card:hover {
          transform: translateY(-1px);
          background: color-mix(in srgb, var(--bg-2) 96%, transparent);
        }

        @media (min-width: 1200px) {
          .achievements-grid {
            grid-template-columns: repeat(3, minmax(0, 1fr));
          }
        }

        @media (max-width: 1023px) {
          .achievements-grid.bottom {
            grid-template-columns: minmax(0, 1fr);
          }
        }

        @keyframes achievements-fade-in {
          from {
            opacity: 0;
            transform: translateY(8px);
          }

          to {
            opacity: 1;
            transform: translateY(0);
          }
        }
      `}</style>

      <div className="achievements-fidelity-page">
        <div className="flex flex-col gap-4 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <div
              className="text-[11px] font-semibold uppercase tracking-[0.16em]"
              style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
            >
              {isKo ? "업적 보드" : "Achievement Board"}
            </div>
            <h1
              className="mt-2 text-3xl font-semibold tracking-tight sm:text-4xl"
              style={{ color: "var(--th-text-heading, var(--fg))" }}
            >
              {t({ ko: "업적", en: "Achievements" })}
            </h1>
            <p
              className="mt-2 max-w-2xl text-sm leading-7 sm:text-base"
              style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
            >
              {t({
                ko: "달성, 진행 중, 잠김 상태를 같은 문법으로 추적하고 XP 흐름과 데일리 미션을 함께 확인합니다.",
                en: "Track unlocked, in-progress, and locked milestones together with XP flow and daily missions.",
              })}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <span
              className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-semibold"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-success, var(--ok)) 26%, var(--th-border-subtle, var(--line)) 74%)",
                background: "color-mix(in srgb, var(--th-accent-success, var(--ok)) 10%, transparent)",
                color: "var(--th-accent-success, var(--ok))",
              }}
            >
              <span
                className="h-2 w-2 rounded-full"
                style={{ background: "var(--th-accent-success, var(--ok))" }}
              />
              {earnedCards.length} / {earnedCards.length + progressCards.length + lockedCards.length}{" "}
              {t({ ko: "달성", en: "unlocked" })}
            </span>
          </div>
        </div>

        <div
          className="rounded-[1.5rem] border p-5 sm:p-6"
          style={{
            borderColor: "var(--th-border-subtle, var(--line))",
            background:
              "linear-gradient(135deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 55%, color-mix(in srgb, var(--th-accent-primary, var(--accent)) 8%, transparent) 100%)",
          }}
        >
          <div className="flex flex-col gap-5 lg:flex-row lg:items-center">
            <div className="relative">
              <LevelRing
                value={heroLevel.progress * 100}
                size={94}
                stroke={7}
                color="var(--th-accent-primary, var(--accent))"
                trackColor="color-mix(in srgb, var(--th-border-subtle, var(--line)) 70%, transparent)"
              >
                <div className="text-center">
                  <div
                    className="text-[10px] font-semibold uppercase tracking-[0.14em]"
                    style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                  >
                    LEVEL
                  </div>
                  <div
                    className="mt-1 text-[26px] font-semibold leading-none"
                    style={{ color: "var(--th-text-heading, var(--fg))" }}
                  >
                    {heroLevel.level}
                  </div>
                </div>
              </LevelRing>
            </div>

            <div className="min-w-0 flex-1">
              <div className="text-xl font-semibold" style={{ color: "var(--th-text-heading, var(--fg))" }}>
                {heroTitle}
              </div>
              <div
                className="mt-1 text-sm"
                style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
              >
                {hero
                  ? `${hero.name} · XP ${formatExact(heroXp, locale)}`
                  : `XP ${formatExact(heroXp, locale)}`}
                {Number.isFinite(heroLevel.nextThreshold)
                  ? isKo
                    ? ` · 다음 레벨까지 ${formatExact(heroNextXp, locale)}`
                    : ` · ${formatExact(heroNextXp, locale)} to next level`
                  : isKo
                    ? " · 최상위 레벨"
                    : " · Max level"}
              </div>
              <div
                className="mt-4 overflow-hidden rounded-full"
                style={{
                  height: 6,
                  background: "color-mix(in srgb, var(--th-border-subtle, var(--line)) 68%, transparent)",
                }}
              >
                <div
                  style={{
                    width: `${Math.round(heroLevel.progress * 100)}%`,
                    height: "100%",
                    background: "linear-gradient(90deg, var(--th-accent-primary, var(--accent)), var(--th-accent-danger, var(--codex)))",
                  }}
                />
              </div>
            </div>

            <div className="grid min-w-[min(100%,340px)] grid-cols-3 gap-3">
              {[
                {
                  value: earnedCards.length,
                  label: t({ ko: "달성", en: "Unlocked" }),
                },
                {
                  value: uniqueEarners,
                  label: t({ ko: "에이전트", en: "Agents" }),
                },
                {
                  value: formatCompact(totalOrgXp, locale),
                  label: "TOTAL XP",
                },
              ].map((metric) => (
                <div
                  key={metric.label}
                  className="rounded-2xl border px-3 py-3 text-center"
                  style={{
                    borderColor: "var(--th-border-subtle, var(--line))",
                    background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                  }}
                >
                  <div
                    className="text-[21px] font-semibold"
                    style={{ color: "var(--th-text-heading, var(--fg))" }}
                  >
                    {metric.value}
                  </div>
                  <div
                    className="mt-1 text-[10.5px] font-semibold uppercase tracking-[0.08em]"
                    style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                  >
                    {metric.label}
                  </div>
                </div>
              ))}
            </div>
          </div>
        </div>

        {[
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
        ].map((section) => (
          <section key={section.key} data-testid={`achievements-section-${section.key}`}>
            <div className="mb-3 flex items-center justify-between gap-3">
              <div
                className="text-[13px] font-semibold"
                style={{ color: "var(--th-text-heading, var(--fg))" }}
              >
                {section.title}{" "}
                <span style={{ color: "var(--th-text-muted, var(--fg-muted))", fontWeight: 400 }}>
                  {section.items.length}
                </span>
              </div>
            </div>
            {section.items.length === 0 ? (
              <div
                className="rounded-[1.15rem] border px-4 py-5 text-sm"
                style={{
                  borderColor: "var(--th-border-subtle, var(--line))",
                  color: "var(--th-text-muted, var(--fg-muted))",
                  background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                }}
              >
                {section.key === "earned"
                  ? t({
                      ko: "아직 집계된 업적이 없습니다.",
                      en: "No unlocked achievements have been recorded yet.",
                    })
                  : section.key === "progress"
                    ? t({
                        ko: "지금 진행 중인 업적 후보가 없습니다.",
                        en: "No active milestone candidates right now.",
                      })
                    : t({
                        ko: "현재 잠긴 업적이 없습니다.",
                        en: "No locked milestones remain.",
                      })}
              </div>
            ) : (
              <div className="achievements-grid" data-testid={`achievements-grid-${section.key}`}>
                {section.items.map((item) => (
                  <AchievementCard
                    key={item.id}
                    item={item}
                    isKo={isKo}
                    progressLabel={t({ ko: "진행", en: "progress" })}
                    onOpen={setSelectedCard}
                  />
                ))}
              </div>
            )}
          </section>
        ))}

        <div className="achievements-grid bottom" data-testid="achievements-grid-bottom">
          <div
            data-testid="achievements-daily-missions"
            className="rounded-[1.15rem] border p-4 sm:p-5"
            style={{
              borderColor: "var(--th-border-subtle, var(--line))",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
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
                    ko: "시안에 없는 운영 gamification은 같은 톤으로 확장했습니다.",
                    en: "Legacy gamification items extend the same visual language.",
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
              className="rounded-[1.15rem] border p-4 sm:p-5"
              style={{
                borderColor: "var(--th-border-subtle, var(--line))",
                background:
                  "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
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
                        <AchievementAvatar emoji={entry.avatarEmoji} label={entry.name} />
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

      <Drawer
        open={Boolean(selectedCard)}
        onClose={() => setSelectedCard(null)}
        title={selectedCard?.name}
        width="min(520px, 100vw)"
      >
        {selectedCard ? (
          <div className="space-y-5" data-testid="achievements-drawer">
            <div
              className="rounded-[1.25rem] border p-4"
              style={{
                borderColor: getRarityTheme(selectedCard.rarity).border,
                background:
                  "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
              }}
            >
              <div className="flex items-start gap-3">
                <BadgeIcon
                  milestoneId={selectedCard.milestoneId}
                  rarity={selectedCard.rarity}
                  achieved={selectedCard.section === "earned"}
                />
                <div className="min-w-0 flex-1">
                  <div className="flex flex-wrap items-center gap-2">
                    <span
                      className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                      style={{
                        background: getRarityTheme(selectedCard.rarity).badgeBg,
                        color: getRarityTheme(selectedCard.rarity).badgeText,
                      }}
                    >
                      {rarityLabel(selectedCard.rarity, isKo)}
                    </span>
                    <span
                      className="rounded-full px-2.5 py-1 text-[11px] font-semibold"
                      style={{
                        background: "var(--th-overlay-medium, var(--bg-3))",
                        color: "var(--th-text-muted, var(--fg-muted))",
                      }}
                    >
                      {selectedCard.section === "earned"
                        ? t({ ko: "획득", en: "Unlocked" })
                        : selectedCard.section === "progress"
                          ? t({ ko: "진행 중", en: "In progress" })
                          : t({ ko: "잠김", en: "Locked" })}
                    </span>
                  </div>
                  <div
                    className="mt-3 text-sm leading-6"
                    style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
                  >
                    {selectedCard.desc}
                  </div>
                  {selectedCard.agentName ? (
                    <button
                      type="button"
                      className="mt-4 inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium transition-colors hover:bg-white/5"
                      style={{
                        borderColor: "var(--th-border-subtle, var(--line))",
                        color: "var(--th-text-primary, var(--fg))",
                      }}
                      onClick={() => {
                        if (selectedCard.agent && onSelectAgent) {
                          onSelectAgent(selectedCard.agent);
                        }
                      }}
                      disabled={!selectedCard.agent || !onSelectAgent}
                    >
                      <AchievementAvatar
                        emoji={selectedCard.avatarEmoji || "🤖"}
                        label={selectedCard.agentName}
                      />
                      <span>{selectedCard.agentName}</span>
                      <ChevronRight size={14} />
                    </button>
                  ) : null}
                </div>
              </div>
            </div>

            <div className="grid grid-cols-2 gap-3">
              {[
                {
                  label: t({ ko: "목표 XP", en: "Target XP" }),
                  value: formatExact(selectedCard.xp, locale),
                },
                {
                  label: t({ ko: "현재 XP", en: "Current XP" }),
                  value:
                    typeof selectedCard.currentXp === "number"
                      ? formatExact(selectedCard.currentXp, locale)
                      : "-",
                },
                {
                  label: t({ ko: "남은 XP", en: "XP left" }),
                  value:
                    typeof selectedCard.remainingXp === "number"
                      ? formatExact(selectedCard.remainingXp, locale)
                      : "-",
                },
                {
                  label: t({ ko: "상태", en: "Status" }),
                  value:
                    selectedCard.section === "earned"
                      ? t({ ko: "획득", en: "Unlocked" })
                      : selectedCard.section === "progress"
                        ? t({ ko: "진행 중", en: "In progress" })
                        : t({ ko: "잠김", en: "Locked" }),
                },
              ].map((metric) => (
                <div
                  key={metric.label}
                  className="rounded-2xl border px-3 py-3"
                  style={{
                    borderColor: "var(--th-border-subtle, var(--line))",
                    background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                  }}
                >
                  <div
                    className="text-[10.5px] font-semibold uppercase tracking-[0.08em]"
                    style={{ color: "var(--th-text-muted, var(--fg-muted))" }}
                  >
                    {metric.label}
                  </div>
                  <div
                    className="mt-1 text-lg font-semibold"
                    style={{ color: "var(--th-text-heading, var(--fg))" }}
                  >
                    {metric.value}
                  </div>
                </div>
              ))}
            </div>

            {typeof selectedCard.progress === "number" ? (
              <div data-testid="achievements-progress">
                <div
                  className="mb-2 text-[12px] font-medium"
                  style={{ color: "var(--th-text-secondary, var(--fg))" }}
                >
                  {t({ ko: "진행도", en: "Progress" })}
                </div>
                <div
                  className="overflow-hidden rounded-full"
                  style={{
                    height: 6,
                    background: "color-mix(in srgb, var(--th-border-subtle, var(--line)) 68%, transparent)",
                  }}
                >
                  <div
                    style={{
                      width: `${Math.round(selectedCard.progress * 100)}%`,
                      height: "100%",
                      background: getRarityTheme(selectedCard.rarity).accent,
                    }}
                  />
                </div>
              </div>
            ) : null}

            <div className="space-y-4">
              <div data-testid="achievements-details">
                <div
                  className="mb-2 text-[12px] font-medium"
                  style={{ color: "var(--th-text-secondary, var(--fg))" }}
                >
                  {t({ ko: "세부", en: "Details" })}
                </div>
                <div className="space-y-2">
                  {selectedCard.detailLines.map((line) => (
                    <div
                      key={line}
                      className="rounded-2xl border px-3 py-3 text-sm"
                      style={{
                        borderColor: "var(--th-border-subtle, var(--line))",
                        color: "var(--th-text-secondary, var(--fg-muted))",
                        background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                      }}
                    >
                      {line}
                    </div>
                  ))}
                </div>
              </div>

              <div data-testid="achievements-timeline">
                <div
                  className="mb-2 text-[12px] font-medium"
                  style={{ color: "var(--th-text-secondary, var(--fg))" }}
                >
                  {t({ ko: "타임라인", en: "Timeline" })}
                </div>
                <div className="space-y-2">
                  {selectedCard.timelineLines.map((line, index) => (
                    <div key={`${line}-${index}`} className="flex items-start gap-3 rounded-2xl border px-3 py-3" style={{ borderColor: "var(--th-border-subtle, var(--line))", background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)" }}>
                      <div
                        className="mt-1 h-2.5 w-2.5 rounded-full"
                        style={{
                          background: index === 0
                            ? getRarityTheme(selectedCard.rarity).accent
                            : "var(--th-text-muted, var(--fg-muted))",
                        }}
                      />
                      <div
                        className="text-sm"
                        style={{ color: "var(--th-text-secondary, var(--fg-muted))" }}
                      >
                        {line}
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            </div>
          </div>
        ) : null}
      </Drawer>
    </div>
  );
}
