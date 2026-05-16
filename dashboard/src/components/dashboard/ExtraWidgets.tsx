import { useEffect, useState, useMemo } from "react";
import type { Agent, DashboardStats } from "../../types";
import * as api from "../../api/client";
import { localeName, type UiLanguage } from "../../i18n";
import { getFontFamilyForText } from "../../lib/fonts";
import { getRankTier, type TFunction } from "./model";
import AgentAvatar from "../AgentAvatar";
import { cx, dashboardBadge, dashboardCard, DashboardEmptyState } from "./ui";
export { BottleneckWidget } from "./BottleneckWidget";

export {
  buildCronTimelineMetrics,
  CronTimelineWidget,
  describeCronSchedule,
  formatCompactDuration,
  type CronTimelineMetrics,
} from "./CronTimelineWidget";
export { AutoQueueHistoryWidget } from "./AutoQueueHistoryWidget";

export { AgentQualityWidget } from "./AgentQualityWidget";

// ── Achievement Wall Widget ──

interface AchievementWidgetProps {
  t: TFunction;
  agents: Agent[];
}

function fallbackAgentFromAchievement(achievement: api.Achievement): Agent {
  return {
    id: achievement.agent_id,
    name: achievement.agent_name,
    alias: null,
    name_ko: achievement.agent_name_ko || achievement.agent_name,
    department_id: null,
    avatar_emoji: achievement.avatar_emoji,
    personality: null,
    status: "idle",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 0,
  };
}

export function AchievementWidget({ t, agents }: AchievementWidgetProps) {
  const [achievements, setAchievements] = useState<api.Achievement[]>([]);
  const agentMap = useMemo(() => new Map(agents.map((agent) => [agent.id, agent])), [agents]);

  useEffect(() => {
    api.getAchievements().then((d) => setAchievements(d.achievements)).catch(() => {});
  }, []);

  if (achievements.length === 0) return null;

  const badgeIcon: Record<string, string> = {
    xp_100: "⭐", xp_500: "🌟", xp_1000: "💫", xp_5000: "🏅",
    tasks_10: "🐝", tasks_50: "👑", tasks_100: "🎖️",
    streak_7: "🔥", streak_30: "💎",
  };

  return (
    <div
      className={dashboardCard.accentStandard}
      style={{ borderColor: "var(--th-border)", background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #eab308 10%), var(--th-surface))" }}
    >
      <h3
        className="font-pixel mb-3 text-sm font-semibold"
        style={{
          color: "var(--th-text)",
          fontFamily: getFontFamilyForText(
            t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" }),
            "pixel",
          ),
        }}
      >
        🏆 {t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" })}
      </h3>
      <div className="space-y-1.5 max-h-48 overflow-y-auto">
        {achievements.slice(0, 15).map((ach) => {
          const agent = agentMap.get(ach.agent_id) ?? fallbackAgentFromAchievement(ach);
          const agentLabel = ach.agent_name_ko || ach.agent_name;
          return (
            <div
              key={ach.id}
              className={cx(dashboardCard.nestedCompact, "flex items-center gap-2")}
              style={{ background: "var(--th-bg-surface)" }}
            >
              <div className="relative shrink-0">
                <AgentAvatar agent={agent} agents={agents} size={30} rounded="xl" className="shadow-sm" />
                <span
                  className="absolute -right-1 -top-1 flex h-5 w-5 items-center justify-center rounded-full text-[10px]"
                  style={{ background: "rgba(15,23,42,0.82)" }}
                >
                  {badgeIcon[ach.type] || "🎯"}
                </span>
              </div>
              <div className="flex-1 min-w-0">
                <div
                  className="font-pixel truncate text-xs font-medium"
                  style={{
                    color: "var(--th-text)",
                    fontFamily: getFontFamilyForText(agentLabel, "pixel"),
                  }}
                >
                  {agentLabel}
                </div>
                <div
                  className="font-pixel text-xs"
                  style={{
                    color: "var(--th-text-muted)",
                    fontFamily: getFontFamilyForText(ach.name, "pixel"),
                  }}
                >
                  {ach.name} — {ach.description}
                </div>
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ── Skill Trend Chart (simple sparkline) ──

interface SkillTrendWidgetProps {
  t: TFunction;
}

export function SkillTrendWidget({ t }: SkillTrendWidgetProps) {
  const [trend, setTrend] = useState<api.SkillTrendPoint[] | null>(null);

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const nextTrend = await api.getSkillTrend(30);
        if (mounted) setTrend(nextTrend);
      } catch {
        // Ignore transient skill trend failures in the dashboard.
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, []);

  if (!trend || trend.length === 0) return null;

  const days = trend.map((entry) => entry.day);
  const dailyTotals = trend.map((entry) => entry.count);
  const max = Math.max(1, ...dailyTotals);

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="text-sm font-semibold mb-3" style={{ color: "var(--th-text)" }}>
        {t({ ko: "스킬 사용 추이 (30일)", en: "Skill Usage Trend (30d)", ja: "スキル使用推移 (30日)", zh: "技能使用趋势 (30天)" })}
      </h3>
      <div className="flex items-end gap-[3px] h-12">
        {dailyTotals.map((total, i) => (
          <div
            key={days[i]}
            className="flex-1 rounded-t"
            style={{
              height: `${Math.max(4, (total / max) * 100)}%`,
              background: `rgba(245,158,11,${0.3 + (total / max) * 0.5})`,
              minWidth: 0,
            }}
            title={`${days[i]}: ${total} calls`}
          />
        ))}
      </div>
      <div className="flex justify-between mt-1">
        {days.length > 0 && (
          <>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {days[0].slice(5)}
            </span>
            <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
              {days[days.length - 1].slice(5)}
            </span>
          </>
        )}
      </div>
    </div>
  );
}

// ── Department Performance + Squad Roster (#1254 audit restore) ─────────────────

const DEPT_BAR_COLORS = [
  { bar: "from-blue-500 to-cyan-400", badge: "bg-blue-500/20 text-blue-200 border-blue-400/30" },
  { bar: "from-lime-500 to-emerald-400", badge: "bg-lime-500/20 text-lime-100 border-lime-400/30" },
  { bar: "from-emerald-500 to-teal-400", badge: "bg-emerald-500/20 text-emerald-200 border-emerald-400/30" },
  { bar: "from-amber-500 to-orange-400", badge: "bg-amber-500/20 text-amber-100 border-amber-400/30" },
  { bar: "from-rose-500 to-pink-400", badge: "bg-rose-500/20 text-rose-100 border-rose-400/30" },
  { bar: "from-cyan-500 to-sky-400", badge: "bg-cyan-500/20 text-cyan-100 border-cyan-400/30" },
  { bar: "from-orange-500 to-red-400", badge: "bg-orange-500/20 text-orange-100 border-orange-400/30" },
  { bar: "from-teal-500 to-lime-400", badge: "bg-teal-500/20 text-teal-100 border-teal-400/30" },
];

export interface DepartmentPerformanceRow {
  id: string;
  name: string;
  icon: string;
  done: number;
  total: number;
  ratio: number;
  color: { bar: string; badge: string };
}

export function buildDepartmentPerformanceRows(
  departments: DashboardStats["departments"],
  language: UiLanguage,
): DepartmentPerformanceRow[] {
  const totalXp = departments.reduce((sum, d) => sum + (d.sum_xp ?? 0), 0);
  return departments.map((dept, index) => ({
    id: dept.id,
    name: language === "ko" ? dept.name_ko || dept.name : dept.name,
    icon: dept.icon,
    done: dept.sum_xp ?? 0,
    total: totalXp,
    ratio: totalXp > 0 ? Math.round(((dept.sum_xp ?? 0) / totalXp) * 100) : 0,
    color: DEPT_BAR_COLORS[index % DEPT_BAR_COLORS.length],
  }));
}

interface DashboardDeptAndSquadProps {
  deptRows: DepartmentPerformanceRow[];
  workingAgents: Agent[];
  idleAgentsList: Agent[];
  agents: Agent[];
  language: UiLanguage;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
  onSelectAgent?: (agent: Agent) => void;
}

export function DashboardDeptAndSquad({
  deptRows,
  workingAgents,
  idleAgentsList,
  agents,
  language,
  numberFormatter,
  t,
  onSelectAgent,
}: DashboardDeptAndSquadProps) {
  return (
    <div className="grid grid-cols-1 gap-4 xl:grid-cols-[1.2fr_1fr]">
      <div
        className={dashboardCard.standard}
        style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
      >
        <div className="mb-4 flex items-center gap-2">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "부서 성과", en: "Dept. Performance", ja: "部署パフォーマンス", zh: "部门绩效" })}
          </h3>
          <span className="ml-auto text-xs" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "부서별 XP 비율", en: "XP ratio by department", ja: "部署別 XP 比率", zh: "按部门 XP 占比" })}
          </span>
        </div>

        {deptRows.length === 0 ? (
          <DashboardEmptyState
            icon="🏰"
            title={t({ ko: "데이터 없음", en: "No data", ja: "データなし", zh: "暂无数据" })}
            description={t({
              ko: "표시할 부서 성과 집계가 아직 없습니다.",
              en: "Department performance metrics are not available yet.",
              ja: "表示できる部署パフォーマンス集計がまだありません。",
              zh: "暂时没有可显示的部门绩效统计。",
            })}
            className="min-h-[180px]"
          />
        ) : (
          <div className="space-y-2.5">
            {deptRows.map((dept) => (
              <article
                key={dept.id}
                className={cx(dashboardCard.nestedCompact, "group")}
                style={{ background: "var(--th-bg-surface)" }}
              >
                <div className="flex items-center justify-between">
                  <div className="flex items-center gap-2.5">
                    <span
                      className="flex h-8 w-8 items-center justify-center rounded-lg text-base"
                      style={{ background: "var(--th-surface)" }}
                    >
                      {dept.icon}
                    </span>
                    <span className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                      {dept.name}
                    </span>
                  </div>
                  <span className={cx(dashboardBadge.default, dept.color.badge, "border")}>
                    {dept.ratio}%
                  </span>
                </div>

                <div
                  className="mt-2.5 relative h-2 overflow-hidden rounded-full"
                  style={{ background: "var(--th-overlay-medium)" }}
                >
                  <div
                    className={`h-full rounded-full bg-gradient-to-r ${dept.color.bar} transition-all duration-700`}
                    style={{ width: `${dept.ratio}%` }}
                  />
                </div>

                <div
                  className="mt-1.5 flex justify-between text-xs"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  <span>XP {numberFormatter.format(dept.done)}</span>
                  <span>
                    {t({ ko: "전체", en: "total", ja: "全体", zh: "总计" })}
                    {" "}XP {numberFormatter.format(dept.total)}
                  </span>
                </div>
              </article>
            ))}
          </div>
        )}
      </div>

      <div
        className={dashboardCard.standard}
        style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
      >
        <div className="mb-4 flex items-center justify-between">
          <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
            {t({ ko: "스쿼드", en: "Squad", ja: "スクワッド", zh: "小队" })}
          </h3>
          <div className="flex items-center gap-2 text-xs">
            <span
              className={dashboardBadge.default}
              style={{ background: "rgba(16,185,129,0.15)", color: "#34d399", borderColor: "rgba(16,185,129,0.3)" }}
            >
              ON {numberFormatter.format(workingAgents.length)}
            </span>
            <span
              className={dashboardBadge.default}
              style={{ background: "var(--th-overlay-medium)", color: "var(--th-text-muted)" }}
            >
              OFF {numberFormatter.format(idleAgentsList.length)}
            </span>
          </div>
        </div>

        {agents.length === 0 ? (
          <DashboardEmptyState
            icon="🤖"
            title={t({ ko: "에이전트 없음", en: "No agents", ja: "エージェントなし", zh: "暂无代理" })}
            description={t({
              ko: "등록된 에이전트가 없습니다.",
              en: "There are no agents registered.",
              ja: "登録されたエージェントがありません。",
              zh: "没有注册的代理。",
            })}
            className="min-h-[180px]"
          />
        ) : (
          <div className="flex flex-wrap gap-3">
            {agents.map((agent) => {
              const isWorking = agent.status === "working";
              const tier = getRankTier(agent.stats_xp);
              return (
                <button
                  key={agent.id}
                  type="button"
                  title={`${localeName(language, agent)} — ${
                    isWorking
                      ? t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" })
                      : t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "空闲" })
                  } — ${tier.name}`}
                  className="group flex flex-col items-center gap-1.5"
                  onClick={() => onSelectAgent?.(agent)}
                >
                  <div className="relative">
                    <div
                      className="overflow-hidden rounded-2xl transition-transform duration-200 group-hover:scale-110"
                      style={{
                        boxShadow: isWorking ? `0 0 12px ${tier.glow}` : "none",
                        border: isWorking
                          ? `2px solid ${tier.color}60`
                          : "1px solid rgba(148,163,184,0.18)",
                      }}
                    >
                      <AgentAvatar agent={agent} agents={agents} size={40} rounded="2xl" />
                    </div>
                    <span
                      className={`absolute -bottom-0.5 -right-0.5 h-3 w-3 rounded-full border-2 ${
                        isWorking ? "bg-emerald-400" : "bg-slate-600"
                      }`}
                      style={{ borderColor: "var(--th-bg-primary)" }}
                    />
                  </div>
                  <span
                    className="max-w-[60px] truncate text-center text-xs font-medium leading-tight"
                    style={{ color: isWorking ? "var(--th-text)" : "var(--th-text-muted)" }}
                  >
                    {localeName(language, agent)}
                  </span>
                </button>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

// ── GitHub Issues Widget (#1254 audit restore) ──────────────────────────────────

interface GitHubIssuesWidgetProps {
  t: TFunction;
  repo?: string;
}

export function GitHubIssuesWidget({ t, repo }: GitHubIssuesWidgetProps) {
  const [data, setData] = useState<api.GitHubIssuesResponse | null>(null);

  useEffect(() => {
    let mounted = true;
    api
      .getGitHubIssues(repo, "open", 8)
      .then((next) => {
        if (mounted) setData(next);
      })
      .catch(() => {
        // GitHub fetch failures are non-blocking — leave the widget empty.
      });
    return () => {
      mounted = false;
    };
  }, [repo]);

  if (!data || data.issues.length === 0) return null;

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <div className="mb-3 flex items-center justify-between">
        <h3 className="text-sm font-semibold" style={{ color: "var(--th-text)" }}>
          {t({ ko: "GitHub 이슈", en: "GitHub Issues", ja: "GitHub Issues", zh: "GitHub Issues" })}
        </h3>
        <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
          {data.repo}
        </span>
      </div>
      <div className="max-h-48 space-y-1.5 overflow-y-auto">
        {data.issues.map((issue) => (
          <div
            key={issue.number}
            className={cx(dashboardCard.nestedCompact, "flex items-start gap-2")}
            style={{ background: "var(--th-bg-surface)" }}
          >
            <span className="mt-0.5 shrink-0 text-xs" style={{ color: "#34d399" }}>
              #{issue.number}
            </span>
            <div className="min-w-0 flex-1">
              <div className="truncate text-xs font-medium" style={{ color: "var(--th-text)" }}>
                {issue.title}
              </div>
              <div className="mt-0.5 flex flex-wrap gap-1">
                {issue.labels.slice(0, 3).map((label) => (
                  <span
                    key={label.name}
                    className={dashboardBadge.default}
                    style={{ background: `#${label.color}33`, color: `#${label.color}` }}
                  >
                    {label.name}
                  </span>
                ))}
                {issue.assignees.length > 0 && (
                  <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                    → {issue.assignees.map((a) => a.login).join(", ")}
                  </span>
                )}
              </div>
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

// ── Activity Heatmap Widget (#1254 audit restore) ──────────────────────────────

interface HeatmapWidgetProps {
  t: TFunction;
}

export function HeatmapWidget({ t }: HeatmapWidgetProps) {
  const [data, setData] = useState<api.HeatmapData | null>(null);

  useEffect(() => {
    let mounted = true;
    api
      .getActivityHeatmap()
      .then((next) => {
        if (mounted) setData(next);
      })
      .catch(() => {
        // Heatmap is best-effort — render nothing on transient failure.
      });
    return () => {
      mounted = false;
    };
  }, []);

  if (!data || data.hours.length === 0) return null;

  const maxCount = Math.max(
    1,
    ...data.hours.map((hour) => Object.values(hour.agents).reduce((sum, value) => sum + value, 0)),
  );
  const currentHour = new Date().getHours();

  return (
    <div
      className={dashboardCard.standard}
      style={{ borderColor: "var(--th-border)", background: "var(--th-surface)" }}
    >
      <h3 className="mb-3 text-sm font-semibold" style={{ color: "var(--th-text)" }}>
        {t({
          ko: "오늘의 활동 히트맵",
          en: "Today's Activity Heatmap",
          ja: "今日の活動ヒートマップ",
          zh: "今日活动热力图",
        })}
      </h3>
      <div className="flex h-16 items-end gap-[2px]">
        {data.hours.map((hour) => {
          const total = Object.values(hour.agents).reduce((sum, value) => sum + value, 0);
          const height = Math.max(2, (total / maxCount) * 100);
          const isCurrent = hour.hour === currentHour;
          return (
            <div
              key={hour.hour}
              className="relative flex-1 cursor-default rounded-t"
              style={{
                height: `${height}%`,
                background:
                  total === 0
                    ? "rgba(100,116,139,0.15)"
                    : isCurrent
                      ? "var(--th-accent-primary)"
                      : `color-mix(in srgb, var(--th-accent-primary) ${Math.round(
                          (0.2 + (total / maxCount) * 0.6) * 100,
                        )}%, transparent)`,
                minWidth: 0,
              }}
              title={`${hour.hour}:00 — ${total} events`}
            />
          );
        })}
      </div>
      <div className="mt-1 flex justify-between text-xs" style={{ color: "var(--th-text-muted)" }}>
        <span>0h</span>
        <span>6h</span>
        <span>12h</span>
        <span>18h</span>
        <span>24h</span>
      </div>
    </div>
  );
}
