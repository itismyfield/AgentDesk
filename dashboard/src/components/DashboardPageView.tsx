import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from "react";
import type { Agent, DashboardStats, CompanySettings } from "../types";
import { getSkillRanking, type SkillRankingResponse } from "../api";
import TooltipLabel from "./common/TooltipLabel";
import { localeName } from "../i18n";
import { useNow, type TFunction, DEPT_COLORS } from "./dashboard/model";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
import {
  DashboardHeroHeader,
  DashboardHudStats,
  DashboardRankingBoard,
  type HudStat,
  type RankedAgent,
} from "./dashboard/HeroSections";
import {
  DashboardDeptAndSquad,
  type DepartmentPerformance,
} from "./dashboard/OpsSections";
import {
  MachineStatusWidget,
  HeatmapWidget,
  CronTimelineWidget,
  StreakWidget,
  MvpWidget,
  KanbanOpsWidget,
  SkillTrendWidget,
  CookingHeartRoleBoardWidget,
  GitHubIssuesWidget,
  AchievementWidget,
  ActivityFeedWidget,
} from "./dashboard/ExtraWidgets";
import RateLimitWidget from "./dashboard/RateLimitWidget";
import ReceiptWidget from "./dashboard/ReceiptWidget";

interface DashboardPageViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  settings: CompanySettings;
  onSelectAgent?: (agent: Agent) => void;
}

export default function DashboardPageView({
  stats,
  agents,
  settings,
  onSelectAgent,
}: DashboardPageViewProps) {
  const language = settings.language;
  const localeTag = language === "ko" ? "ko-KR" : language === "ja" ? "ja-JP" : language === "zh" ? "zh-CN" : "en-US";
  const numberFormatter = useMemo(() => new Intl.NumberFormat(localeTag), [localeTag]);

  const t: TFunction = useCallback(
    (messages) => messages[language] ?? messages.ko,
    [language],
  );

  const { date, time, briefing } = useNow(localeTag, t);

  type DashTab = "overview" | "agents" | "skills" | "infra";
  const [dashTab, setDashTab] = useState<DashTab>("overview");
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(null);
  const [skillWindow, setSkillWindow] = useState<"7d" | "30d" | "all">("7d");

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const data = await getSkillRanking(skillWindow, 10);
        if (mounted) setSkillRanking(data);
      } catch {
        // ignore auth/network errors in dashboard widgets
      }
    };

    load();
    const timer = setInterval(load, 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, [skillWindow]);

  if (!stats) {
    return (
      <div className="flex items-center justify-center h-full" style={{ color: "var(--th-text-muted)" }}>
        <div className="text-center">
          <div className="text-4xl mb-4 opacity-30">📊</div>
          <div>{t({ ko: "통계 로딩 중...", en: "Loading stats...", ja: "統計読み込み中...", zh: "加载统计中..." })}</div>
        </div>
      </div>
    );
  }

  // Build HUD stats from DashboardStats
  const hudStats: HudStat[] = [
    {
      id: "total",
      label: t({ ko: "전체 직원", en: "Total Agents", ja: "全エージェント", zh: "全部代理" }),
      value: stats.agents.total,
      sub: t({ ko: "등록된 에이전트", en: "Registered agents", ja: "登録エージェント", zh: "已注册代理" }),
      color: "#60a5fa",
      icon: "👥",
    },
    {
      id: "working",
      label: t({ ko: "근무 중", en: "Working", ja: "作業中", zh: "工作中" }),
      value: stats.agents.working,
      sub: t({ ko: "실시간 활동", en: "Active now", ja: "リアルタイム活動", zh: "当前活跃" }),
      color: "#34d399",
      icon: "💼",
    },
    {
      id: "idle",
      label: t({ ko: "대기", en: "Idle", ja: "待機", zh: "空闲" }),
      value: stats.agents.idle,
      sub: t({ ko: "배치 대기", en: "Awaiting assignment", ja: "配置待ち", zh: "等待分配" }),
      color: "#94a3b8",
      icon: "⏸️",
    },
    {
      id: "dispatched",
      label: t({ ko: "파견 인력", en: "Dispatched", ja: "派遣", zh: "派遣" }),
      value: stats.dispatched_count,
      sub: t({ ko: "외부 세션", en: "External sessions", ja: "外部セッション", zh: "外部会话" }),
      color: "#fbbf24",
      icon: "⚡",
    },
  ];

  // Build ranked agents from stats.top_agents
  const deptMap = new Map(stats.departments.map((d) => [d.id, d]));
  const topAgents: RankedAgent[] = stats.top_agents.map((a) => ({
    id: a.id,
    name: a.alias || a.name_ko || a.name,
    department: "",
    tasksDone: a.stats_tasks_done,
    xp: a.stats_xp,
  }));

  const podiumOrder: RankedAgent[] =
    topAgents.length >= 3
      ? [topAgents[1], topAgents[0], topAgents[2]]
      : topAgents.length === 2
        ? [topAgents[1], topAgents[0]]
        : [];

  const agentMap = new Map(agents.map((a) => [a.id, a]));
  const maxXp = topAgents.reduce((max, a) => Math.max(max, a.xp), 1);

  // Build dept performance from stats.departments (XP share)
  const totalXpAll = stats.departments.reduce((s, d) => s + (d.sum_xp ?? 0), 0);
  const deptData: DepartmentPerformance[] = stats.departments.map((d, i) => ({
    id: d.id,
    name: d.name_ko || d.name,
    icon: d.icon,
    done: d.sum_xp ?? 0,
    total: totalXpAll,
    ratio: totalXpAll > 0 ? Math.round(((d.sum_xp ?? 0) / totalXpAll) * 100) : 0,
    color: DEPT_COLORS[i % DEPT_COLORS.length],
  }));

  const workingAgents = agents.filter((a) => a.status === "working");
  const idleAgents = agents.filter((a) => a.status !== "working");

  return (
    <div
      className="p-4 sm:p-6 space-y-4 max-w-6xl mx-auto overflow-auto h-full pb-40"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <DashboardHeroHeader
        companyName={settings.companyName}
        time={time}
        date={date}
        briefing={briefing}
        reviewQueue={stats.kanban.review_queue}
        numberFormatter={numberFormatter}
        t={t}
      />

      <DashboardHudStats hudStats={hudStats} numberFormatter={numberFormatter} />

      <RateLimitWidget t={t} />
      <ReceiptWidget t={t} />

      {/* Dashboard sub-tabs */}
      <div className="flex gap-1" style={{ borderBottom: "1px solid var(--th-border)" }}>
        {([
          { id: "overview" as DashTab, label: t({ ko: "개요", en: "Overview", ja: "概要", zh: "概览" }) },
          { id: "agents" as DashTab, label: t({ ko: "에이전트", en: "Agents", ja: "エージェント", zh: "代理" }) },
          { id: "skills" as DashTab, label: t({ ko: "스킬", en: "Skills", ja: "スキル", zh: "技能" }) },
          { id: "infra" as DashTab, label: t({ ko: "인프라", en: "Infra", ja: "インフラ", zh: "基础设施" }) },
        ]).map((tab) => (
          <button
            key={tab.id}
            onClick={() => setDashTab(tab.id)}
            className={`px-4 py-2 text-sm font-medium transition-colors ${
              dashTab === tab.id ? "text-blue-400 border-b-2 border-blue-400" : ""
            }`}
            style={dashTab !== tab.id ? { color: "var(--th-text-muted)" } : undefined}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* === Overview Tab === */}
      {dashTab === "overview" && (
        <>
          <KanbanOpsWidget kanban={stats.kanban} t={t} />
          <DashboardRankingBoard
            topAgents={topAgents}
            podiumOrder={podiumOrder}
            agentMap={agentMap}
            agents={agents}
            maxXp={maxXp}
            numberFormatter={numberFormatter}
            t={t}
            onSelectAgent={onSelectAgent}
          />
          <ActivityFeedWidget agents={agents} t={t} />
        </>
      )}

      {/* === Agents Tab === */}
      {dashTab === "agents" && (
        <>
          <DashboardDeptAndSquad
            deptData={deptData}
            workingAgents={workingAgents}
            idleAgentsList={idleAgents}
            agents={agents}
            language={language}
            numberFormatter={numberFormatter}
            t={t}
            onSelectAgent={onSelectAgent}
          />
          <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
            <StreakWidget agents={agents} t={t} />
            <MvpWidget agents={agents} t={t} isKo={language === "ko"} />
            <AchievementWidget t={t} />
          </div>
        </>
      )}

      {/* === Skills Tab === */}
      {dashTab === "skills" && (
        <>
          <section
            className="rounded-2xl border p-4 sm:p-5"
            style={{
              borderColor: "var(--th-border)",
              background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, #f59e0b 8%), var(--th-surface))",
            }}
          >
            <div className="flex items-center justify-between mb-3 gap-3 flex-wrap">
              <h3 className="text-lg font-semibold" style={{ color: "var(--th-text)" }}>
                {t({ ko: "스킬 랭킹", en: "Skill Ranking", ja: "スキルランキング", zh: "技能排行" })}
              </h3>
              <div className="flex items-center gap-2">
                {(["7d", "30d", "all"] as const).map((w) => (
                  <button
                    key={w}
                    onClick={() => setSkillWindow(w)}
                    className="text-[11px] px-2 py-1 rounded-md border"
                    style={{
                      borderColor: skillWindow === w ? "#f59e0b" : "var(--th-border)",
                      color: skillWindow === w ? "#f59e0b" : "var(--th-text-muted)",
                      background: skillWindow === w ? "rgba(245,158,11,0.12)" : "transparent",
                    }}
                  >
                    {w}
                  </button>
                ))}
                <span className="text-xs" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "1분 갱신", en: "1m refresh", ja: "1分更新", zh: "1分钟刷新" })}
                </span>
              </div>
            </div>

            {!skillRanking || skillRanking.overall.length === 0 ? (
              <div className="text-sm" style={{ color: "var(--th-text-muted)" }}>
                {t({ ko: "아직 집계된 스킬 호출이 없습니다.", en: "No skill usage aggregated yet.", ja: "まだ集計されたスキル呼び出しがありません。", zh: "尚无技能调用统计。" })}
              </div>
            ) : (
              <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
                <div>
                  <div className="text-sm font-medium mb-2" style={{ color: "var(--th-text-muted)" }}>
                    {t({ ko: "전체 TOP 10", en: "Overall TOP 10", ja: "全体 TOP 10", zh: "全体 TOP 10" })}
                  </div>
                  <ol className="space-y-1.5">
                    {skillRanking.overall.map((row, idx) => (
                      <li key={`${row.skill_name}-${idx}`} className="flex items-center justify-between text-sm gap-3">
                        <div className="min-w-0 flex-1" style={{ color: "var(--th-text)" }}>
                          <span className="inline-block w-6" style={{ color: "var(--th-text-muted)" }}>
                            {idx + 1}.
                          </span>
                          <TooltipLabel text={row.skill_desc_ko} tooltip={row.skill_name} className="align-middle" />
                        </div>
                        <span className="font-semibold shrink-0" style={{ color: "#f59e0b" }}>
                          {numberFormatter.format(row.calls)}
                        </span>
                      </li>
                    ))}
                  </ol>
                </div>

                <div>
                  <div className="text-sm font-medium mb-2" style={{ color: "var(--th-text-muted)" }}>
                    {t({ ko: "에이전트별 TOP", en: "Top by Agent", ja: "エージェント別TOP", zh: "按代理 TOP" })}
                  </div>
                  <ul className="space-y-1.5">
                    {skillRanking.byAgent.slice(0, 10).map((row, idx) => (
                      <li key={`${row.agent_role_id}-${row.skill_name}-${idx}`} className="text-sm flex items-center justify-between gap-3">
                        <div className="truncate min-w-0 flex-1" style={{ color: "var(--th-text)" }}>
                          <span className="inline-block w-6" style={{ color: "var(--th-text-muted)" }}>
                            {idx + 1}.
                          </span>
                          <span className="truncate">{row.agent_name} · </span>
                          <TooltipLabel text={row.skill_desc_ko} tooltip={row.skill_name} className="align-middle" />
                        </div>
                        <span className="font-semibold shrink-0" style={{ color: "#f59e0b" }}>
                          {numberFormatter.format(row.calls)}
                        </span>
                      </li>
                    ))}
                  </ul>
                </div>
              </div>
            )}
          </section>

          <SkillTrendWidget t={t} />

          <Suspense fallback={<div className="py-8 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>{t({ ko: "카탈로그 로딩 중...", en: "Loading catalog...", ja: "カタログ読み込み中...", zh: "加载目录中..." })}</div>}>
            <SkillCatalogView embedded />
          </Suspense>
        </>
      )}

      {/* === Infra Tab === */}
      {dashTab === "infra" && (
        <>
          <MachineStatusWidget t={t} />
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
            <CronTimelineWidget t={t} />
            <HeatmapWidget agents={agents} t={t} />
          </div>
          <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
            <CookingHeartRoleBoardWidget agents={agents} t={t} isKo={language === "ko"} />
            <GitHubIssuesWidget t={t} repo={stats.kanban.top_repos[0]?.github_repo} />
          </div>
        </>
      )}
    </div>
  );
}
