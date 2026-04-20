import { useEffect, useMemo, useState } from "react";
import * as api from "../api/client";
import { localeName, useI18n } from "../i18n";
import type { Agent, CompanySettings, DashboardStats } from "../types";
import { getAgentLevel, getAgentTitle } from "./agent-manager/AgentInfoCard";

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
  got: boolean;
  milestoneId: string;
  name: string;
  desc: string;
  xp: number;
  prog?: number;
  at?: string;
  agentName?: string;
  avatarEmoji?: string;
  agent?: Agent;
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

function formatAchievementDate(timestamp: number | null | undefined, locale: string): string | undefined {
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

function AchievementRing({
  value,
  size = 88,
  stroke = 6,
  color = "var(--accent)",
}: {
  value: number;
  size?: number;
  stroke?: number;
  color?: string;
}) {
  const radius = (size - stroke) / 2;
  const circumference = 2 * Math.PI * radius;
  const offset = circumference - (value / 100) * circumference;

  return (
    <svg width={size} height={size} aria-hidden="true">
      <circle
        cx={size / 2}
        cy={size / 2}
        r={radius}
        fill="none"
        stroke="var(--bg-3)"
        strokeWidth={stroke}
      />
      <circle
        cx={size / 2}
        cy={size / 2}
        r={radius}
        fill="none"
        stroke={color}
        strokeWidth={stroke}
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        strokeLinecap="round"
        transform={`rotate(-90 ${size / 2} ${size / 2})`}
        style={{ transition: "stroke-dashoffset 0.6s ease" }}
      />
    </svg>
  );
}

function MiniStat({ value, label }: { value: string | number; label: string }) {
  return (
    <div className="achievement-mini-stat">
      <div className="achievement-mini-value">{value}</div>
      <div className="achievement-mini-label">{label}</div>
    </div>
  );
}

function BadgeIcon({ milestoneId, achieved }: { milestoneId: string; achieved: boolean }) {
  const meta = MILESTONE_BY_ID.get(milestoneId as MilestoneId);
  const hue = meta?.hue ?? 200;
  const glyph = meta?.glyph ?? "?";
  const color = achieved ? `oklch(0.75 0.15 ${hue})` : "var(--fg-faint)";

  return (
    <div style={{ width: 48, height: 48, position: "relative", flexShrink: 0 }}>
      <div
        style={{
          position: "absolute",
          inset: 0,
          background: achieved
            ? `linear-gradient(135deg, ${color}, oklch(0.55 0.18 ${hue}))`
            : "var(--bg-3)",
          clipPath: "polygon(50% 0, 100% 25%, 100% 75%, 50% 100%, 0 75%, 0 25%)",
          boxShadow: achieved ? `0 0 18px oklch(0.75 0.15 ${hue} / 0.35)` : "none",
        }}
      />
      <div
        style={{
          position: "absolute",
          inset: 3,
          background: achieved ? `oklch(0.35 0.1 ${hue})` : "var(--bg-2)",
          clipPath: "polygon(50% 0, 100% 25%, 100% 75%, 50% 100%, 0 75%, 0 25%)",
          display: "grid",
          placeItems: "center",
          color: achieved ? color : "var(--fg-faint)",
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
  locale,
  progressLabel,
}: {
  item: AchievementCardModel;
  locale: string;
  progressLabel: string;
}) {
  return (
    <div
      className="card achievement-card"
      style={{ opacity: item.got ? 1 : 0.7 }}
    >
      {item.got ? (
        <div
          style={{
            position: "absolute",
            top: 0,
            right: 0,
            width: 40,
            height: 40,
            background:
              "radial-gradient(circle at top right, oklch(0.8 0.15 85 / 0.25), transparent 70%)",
          }}
        />
      ) : null}

      <div style={{ display: "flex", alignItems: "flex-start", gap: 12 }}>
        <BadgeIcon milestoneId={item.milestoneId} achieved={item.got} />
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 6, marginBottom: 3 }}>
            <span style={{ fontSize: 13, fontWeight: 600 }}>{item.name}</span>
            <span
              style={{
                marginLeft: "auto",
                fontSize: 10,
                fontFamily: "var(--font-mono)",
                color: item.got ? "var(--warn)" : "var(--fg-faint)",
              }}
            >
              +{formatExact(item.xp, locale)} XP
            </span>
          </div>
          <div style={{ fontSize: 11.5, color: "var(--fg-muted)", lineHeight: 1.45 }}>{item.desc}</div>
          {item.agentName ? (
            <div
              style={{
                marginTop: 8,
                display: "inline-flex",
                alignItems: "center",
                gap: 8,
                fontSize: 10.5,
                color: "var(--fg-faint)",
              }}
            >
              <AchievementAvatar emoji={item.avatarEmoji || "🤖"} label={item.agentName} />
              <span>{item.agentName}</span>
            </div>
          ) : null}
          {!item.got && typeof item.prog === "number" ? (
            <div style={{ marginTop: 10 }}>
              <div className="bar-track" style={{ height: 4 }}>
                <div className="bar-fill" style={{ width: `${Math.round(item.prog * 100)}%` }} />
              </div>
              <div
                style={{
                  marginTop: 4,
                  fontSize: 10,
                  color: "var(--fg-faint)",
                  fontFamily: "var(--font-mono)",
                }}
              >
                {Math.round(item.prog * 100)}% {progressLabel}
              </div>
            </div>
          ) : null}
          {item.got && item.at ? (
            <div
              style={{
                marginTop: 8,
                fontSize: 10.5,
                color: "var(--fg-faint)",
                fontFamily: "var(--font-mono)",
              }}
            >
              {item.at}
            </div>
          ) : null}
        </div>
      </div>
    </div>
  );
}

export default function AchievementsPage({
  settings,
  stats,
  agents = [],
  onSelectAgent,
}: AchievementsPageProps) {
  const { language, locale, t } = useI18n(settings.language);
  const [achievements, setAchievements] = useState<api.Achievement[]>([]);

  useEffect(() => {
    let alive = true;

    const load = async () => {
      try {
        const response = await api.getAchievements();
        if (!alive) return;
        setAchievements(response.achievements ?? []);
      } catch {
        if (!alive) return;
        setAchievements([]);
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
      .sort((left, right) => right.stats_xp - left.stats_xp || right.stats_tasks_done - left.stats_tasks_done)
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
  const heroTitle = getAgentTitle(heroXp, language === "ko");
  const heroNextXp = Number.isFinite(heroLevel.nextThreshold)
    ? Math.max(0, heroLevel.nextThreshold - heroXp)
    : 0;
  const totalOrgXp = agents.reduce((sum, agent) => sum + agent.stats_xp, 0);
  const uniqueEarners = new Set(achievements.map((achievement) => achievement.agent_id)).size;
  const heroMilestoneCount = MILESTONES.filter((milestone) => heroXp >= milestone.threshold).length;

  const earnedCards = useMemo<AchievementCardModel[]>(() => {
    return [...achievements]
      .sort((left, right) => right.earned_at - left.earned_at)
      .map((achievement) => {
        const milestone = MILESTONE_BY_ID.get(achievement.type as MilestoneId);
        const agent = agentMap.get(achievement.agent_id);
        const agentName = agent
          ? localeName(language, agent)
          : language === "ko"
            ? achievement.agent_name_ko
            : achievement.agent_name;

        return {
          id: achievement.id,
          got: true,
          milestoneId: achievement.type,
          name:
            language === "ko"
              ? achievement.name
              : milestone?.name.en ?? achievement.name,
          desc:
            language === "ko"
              ? achievement.description || milestone?.desc.ko || ""
              : milestone?.desc.en || achievement.description || "",
          xp: milestone?.threshold ?? 0,
          at: formatAchievementDate(achievement.earned_at, locale),
          agentName,
          avatarEmoji: achievement.avatar_emoji || agent?.avatar_emoji || "🤖",
          agent,
        };
      });
  }, [achievements, agentMap, language, locale]);

  const lockedCards = useMemo<AchievementCardModel[]>(() => {
    const cards: AchievementCardModel[] = [];
    for (const entry of leaderboard) {
      const milestone = nextMilestone(entry.xp);
      if (!milestone) continue;
      const remaining = Math.max(0, milestone.threshold - entry.xp);
      cards.push({
        id: `${entry.id}:${milestone.id}:locked`,
        got: false,
        milestoneId: milestone.id,
        name: t(milestone.name),
        desc:
          language === "ko"
            ? `${entry.name} · ${formatExact(remaining, locale)} XP 남음`
            : `${entry.name} · ${formatExact(remaining, locale)} XP left`,
        xp: milestone.threshold,
        prog: progressToMilestone(entry.xp, milestone),
        agentName: entry.name,
        avatarEmoji: entry.avatarEmoji,
        agent: entry.agent,
      });
      if (cards.length >= 6) break;
    }
    return cards;
  }, [language, leaderboard, locale, t]);

  const topSnapshot = leaderboard.slice(0, 3);
  const totalCards = heroMilestoneCount + Math.max(0, MILESTONES.length - heroMilestoneCount);

  return (
    <div
      data-testid="achievements-page"
      className="achievements-shell mx-auto h-full w-full max-w-[1440px] min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{
        paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))",
      }}
    >
      <style>{`
        .achievements-shell .page {
          display: flex;
          flex-direction: column;
          gap: 18px;
        }

        .achievements-shell .page-header {
          display: flex;
          align-items: flex-end;
          justify-content: space-between;
          gap: 16px;
        }

        .achievements-shell .page-title {
          font-size: 22px;
          font-weight: 600;
          letter-spacing: -0.5px;
          line-height: 1.2;
        }

        .achievements-shell .page-sub {
          margin-top: 4px;
          max-width: 68ch;
          font-size: 13px;
          color: var(--fg-muted);
        }

        .achievements-shell .card {
          position: relative;
          overflow: hidden;
          border-radius: 18px;
          border: 1px solid color-mix(in srgb, var(--line) 72%, transparent);
          background: color-mix(in srgb, var(--bg-2) 94%, transparent);
        }

        .achievements-shell .chip {
          display: inline-flex;
          align-items: center;
          gap: 5px;
          padding: 2px 8px;
          border-radius: 999px;
          font-size: 11px;
          font-weight: 500;
          background: color-mix(in srgb, var(--bg-3) 90%, transparent);
          color: var(--fg-muted);
          border: 1px solid color-mix(in srgb, var(--line) 70%, transparent);
          font-variant-numeric: tabular-nums;
        }

        .achievements-shell .chip .dot {
          width: 6px;
          height: 6px;
          border-radius: 999px;
          background: var(--fg-muted);
        }

        .achievements-shell .chip.ok {
          color: var(--ok);
          border-color: color-mix(in srgb, var(--ok) 28%, var(--line) 72%);
          background: color-mix(in srgb, var(--ok) 10%, transparent);
        }

        .achievements-shell .chip.ok .dot {
          background: var(--ok);
        }

        .achievements-shell .bar-track {
          width: 100%;
          border-radius: 999px;
          background: color-mix(in srgb, var(--bg-3) 92%, transparent);
          overflow: hidden;
        }

        .achievements-shell .bar-fill {
          height: 100%;
          border-radius: inherit;
          background: linear-gradient(90deg, var(--accent), var(--codex));
        }

        .achievements-shell .achievement-grid {
          display: grid;
          gap: 14px;
        }

        .achievements-shell .achievement-grid.three {
          grid-template-columns: repeat(1, minmax(0, 1fr));
        }

        .achievements-shell .achievement-card {
          padding: 16px;
        }

        .achievements-shell .achievement-mini-stat {
          text-align: center;
          padding: 0 14px;
          border-left: 1px solid color-mix(in srgb, var(--line) 55%, transparent);
        }

        .achievements-shell .achievement-mini-value {
          font-size: 22px;
          font-weight: 700;
          font-family: var(--font-display);
          font-variant-numeric: tabular-nums;
        }

        .achievements-shell .achievement-mini-label {
          font-size: 10.5px;
          color: var(--fg-muted);
          text-transform: uppercase;
          letter-spacing: 0.05em;
        }

        .achievements-shell .section-heading {
          display: flex;
          align-items: center;
          justify-content: space-between;
          gap: 12px;
          margin-bottom: 10px;
        }

        .achievements-shell .section-title {
          font-size: 13px;
          font-weight: 600;
        }

        .achievements-shell .section-title span {
          color: var(--fg-muted);
          font-weight: 400;
        }

        .achievements-shell .leaderboard-list {
          display: grid;
          gap: 10px;
        }

        .achievements-shell .leaderboard-row {
          display: flex;
          align-items: center;
          gap: 12px;
          border-radius: 14px;
          border: 1px solid color-mix(in srgb, var(--line) 66%, transparent);
          padding: 12px 14px;
          background: color-mix(in srgb, var(--bg-2) 92%, transparent);
          text-align: left;
          transition: transform 0.16s ease, border-color 0.16s ease, background 0.16s ease;
        }

        .achievements-shell .leaderboard-row.clickable:hover {
          transform: translateY(-1px);
          border-color: color-mix(in srgb, var(--accent) 24%, var(--line) 76%);
          background: color-mix(in srgb, var(--accent) 6%, var(--bg-2) 94%);
        }

        .achievements-shell .fade-in {
          animation: achievements-fade-in 0.3s ease-out;
        }

        @media (min-width: 768px) {
          .achievements-shell .achievement-grid.three {
            grid-template-columns: repeat(2, minmax(0, 1fr));
          }
        }

        @media (min-width: 1200px) {
          .achievements-shell .achievement-grid.three {
            grid-template-columns: repeat(3, minmax(0, 1fr));
          }
        }

        @media (max-width: 720px) {
          .achievements-shell .page-header {
            flex-direction: column;
            align-items: flex-start;
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

      <div className="page fade-in">
        <div className="page-header">
          <div>
            <div className="page-title">{t({ ko: "업적", en: "Achievements" })}</div>
            <div className="page-sub">
              {t({
                ko: "운영 업적과 XP 흐름을 한 화면에서 추적합니다.",
                en: "Track operational achievements and XP in one view.",
              })}
            </div>
          </div>
          <div style={{ display: "flex", gap: 8 }}>
            <span className="chip ok">
              <span className="dot" />
              {heroMilestoneCount} / {totalCards} {t({ ko: "달성", en: "unlocked" })}
            </span>
          </div>
        </div>

        <div
          className="card"
          style={{
            marginBottom: 14,
            padding: 24,
            background: "linear-gradient(135deg, var(--bg-1), oklch(0.2 0.03 280))",
            border: "1px solid var(--line)",
            display: "flex",
            alignItems: "center",
            gap: 24,
            flexWrap: "wrap",
          }}
        >
          <div style={{ position: "relative" }}>
            <AchievementRing value={heroLevel.progress * 100} />
            <div
              style={{
                position: "absolute",
                inset: 0,
                display: "grid",
                placeItems: "center",
              }}
            >
              <div style={{ textAlign: "center" }}>
                <div
                  style={{
                    fontSize: 10,
                    color: "var(--fg-muted)",
                    textTransform: "uppercase",
                    letterSpacing: "0.05em",
                  }}
                >
                  LEVEL
                </div>
                <div
                  style={{
                    fontSize: 24,
                    fontWeight: 700,
                    fontFamily: "var(--font-display)",
                    lineHeight: 1,
                  }}
                >
                  {heroLevel.level}
                </div>
              </div>
            </div>
          </div>

          <div style={{ flex: 1, minWidth: 240 }}>
            <div style={{ fontSize: 18, fontWeight: 600, marginBottom: 4 }}>{heroTitle}</div>
            <div style={{ fontSize: 12, color: "var(--fg-muted)", marginBottom: 10 }}>
              {hero
                ? `${hero.name} · XP ${formatExact(heroXp, locale)}`
                : `XP ${formatExact(heroXp, locale)}`}
              {Number.isFinite(heroLevel.nextThreshold)
                ? ` · ${t({ ko: "다음 레벨까지", en: "to next level" })} ${formatExact(heroNextXp, locale)}`
                : ` · ${t({ ko: "최상위 레벨", en: "Max level" })}`}
            </div>
            <div className="bar-track" style={{ height: 6 }}>
              <div
                className="bar-fill"
                style={{ width: `${Math.round(heroLevel.progress * 100)}%` }}
              />
            </div>
          </div>

          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(3, minmax(88px, 1fr))",
              gap: 14,
              minWidth: "min(100%, 320px)",
            }}
          >
            <MiniStat value={achievements.length} label={t({ ko: "달성", en: "Unlocked" })} />
            <MiniStat value={uniqueEarners} label={t({ ko: "에이전트", en: "Agents" })} />
            <MiniStat value={formatCompact(totalOrgXp, locale)} label="TOTAL XP" />
          </div>
        </div>

        <div>
          <div className="section-heading">
            <div className="section-title">
              {t({ ko: "달성", en: "Unlocked" })} <span>{earnedCards.length}</span>
            </div>
          </div>
          {earnedCards.length === 0 ? (
            <div
              className="card"
              style={{
                padding: 20,
                fontSize: 13,
                color: "var(--fg-muted)",
              }}
            >
              {t({
                ko: "아직 집계된 업적이 없습니다.",
                en: "No unlocked achievements have been recorded yet.",
              })}
            </div>
          ) : (
            <div className="achievement-grid three">
              {earnedCards.map((item) => (
                <AchievementCard
                  key={item.id}
                  item={item}
                  locale={locale}
                  progressLabel={t({ ko: "진행", en: "progress" })}
                />
              ))}
            </div>
          )}
        </div>

        <div>
          <div className="section-heading">
            <div className="section-title">
              {t({ ko: "잠금 해제", en: "Locked" })} <span>{lockedCards.length}</span>
            </div>
          </div>
          {lockedCards.length === 0 ? (
            <div
              className="card"
              style={{
                padding: 20,
                fontSize: 13,
                color: "var(--fg-muted)",
              }}
            >
              {t({
                ko: "현재 milestone 기준으로 잠긴 업적이 없습니다.",
                en: "No pending milestones remain for the current leaderboard.",
              })}
            </div>
          ) : (
            <div className="achievement-grid three">
              {lockedCards.map((item) => (
                <AchievementCard
                  key={item.id}
                  item={item}
                  locale={locale}
                  progressLabel={t({ ko: "진행", en: "progress" })}
                />
              ))}
            </div>
          )}
        </div>

        <div className="achievement-grid" style={{ gridTemplateColumns: "minmax(0, 1.15fr) minmax(320px, 0.85fr)" }}>
          <div className="card" style={{ padding: 18 }}>
            <div className="section-heading">
              <div className="section-title">
                {t({ ko: "랭킹 보드", en: "Ranking Board" })} <span>{leaderboard.length}</span>
              </div>
              <span className="chip">
                <span className="dot" />
                XP
              </span>
            </div>
            <div className="leaderboard-list">
              {leaderboard.slice(0, 6).map((entry, index) => {
                const clickable = Boolean(entry.agent && onSelectAgent);
                return (
                  <button
                    key={entry.id}
                    type="button"
                    className={`leaderboard-row${clickable ? " clickable" : ""}`}
                    onClick={() => {
                      if (entry.agent && onSelectAgent) onSelectAgent(entry.agent);
                    }}
                    disabled={!clickable}
                  >
                    <div
                      style={{
                        width: 28,
                        fontSize: 11,
                        fontWeight: 700,
                        color: "var(--fg-muted)",
                        fontFamily: "var(--font-mono)",
                      }}
                    >
                      #{index + 1}
                    </div>
                    <AchievementAvatar emoji={entry.avatarEmoji} label={entry.name} />
                    <div style={{ minWidth: 0, flex: 1 }}>
                      <div style={{ fontSize: 13, fontWeight: 600, color: "var(--fg)" }}>
                        {entry.name}
                      </div>
                      <div style={{ marginTop: 2, fontSize: 11, color: "var(--fg-muted)" }}>
                        {t({
                          ko: `${formatExact(entry.tasksDone, locale)}개 완료`,
                          en: `${formatExact(entry.tasksDone, locale)} completed`,
                        })}
                      </div>
                    </div>
                    <div
                      style={{
                        textAlign: "right",
                        fontSize: 12,
                        fontWeight: 600,
                        color: "var(--accent)",
                      }}
                    >
                      {formatExact(entry.xp, locale)} XP
                    </div>
                  </button>
                );
              })}
            </div>
          </div>

          <div className="card" style={{ padding: 18 }}>
            <div className="section-heading">
              <div className="section-title">
                {t({ ko: "XP 스냅샷", en: "XP Snapshot" })} <span>{topSnapshot.length}</span>
              </div>
            </div>
            <div className="achievement-grid" style={{ gap: 10 }}>
              {topSnapshot.length === 0 ? (
                <div style={{ fontSize: 13, color: "var(--fg-muted)" }}>
                  {t({
                    ko: "아직 XP 집계 대상이 없습니다.",
                    en: "No XP snapshot is available yet.",
                  })}
                </div>
              ) : (
                topSnapshot.map((entry, index) => (
                  <div
                    key={entry.id}
                    className="card"
                    style={{
                      padding: 14,
                      borderColor: "color-mix(in srgb, var(--accent) 20%, var(--line) 80%)",
                      background: "color-mix(in srgb, var(--bg-2) 92%, transparent)",
                    }}
                  >
                    <div
                      style={{
                        fontSize: 11,
                        fontWeight: 600,
                        letterSpacing: "0.16em",
                        textTransform: "uppercase",
                        color: "var(--fg-muted)",
                      }}
                    >
                      {t({
                        ko: `${index + 1}위`,
                        en: `Rank ${index + 1}`,
                      })}
                    </div>
                    <div style={{ marginTop: 8, display: "flex", alignItems: "center", gap: 10 }}>
                      <AchievementAvatar emoji={entry.avatarEmoji} label={entry.name} />
                      <div style={{ minWidth: 0 }}>
                        <div style={{ fontSize: 13, fontWeight: 600, color: "var(--fg)" }}>
                          {entry.name}
                        </div>
                        <div style={{ marginTop: 2, fontSize: 11.5, color: "var(--fg-muted)" }}>
                          {t({
                            ko: `${formatExact(entry.tasksDone, locale)}개 완료`,
                            en: `${formatExact(entry.tasksDone, locale)} completed`,
                          })}
                        </div>
                      </div>
                    </div>
                    <div
                      style={{
                        marginTop: 10,
                        fontSize: 22,
                        fontWeight: 700,
                        fontFamily: "var(--font-display)",
                        color: "var(--accent)",
                      }}
                    >
                      {formatExact(entry.xp, locale)}
                    </div>
                    <div style={{ marginTop: 2, fontSize: 11, color: "var(--fg-muted)" }}>XP</div>
                  </div>
                ))
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
