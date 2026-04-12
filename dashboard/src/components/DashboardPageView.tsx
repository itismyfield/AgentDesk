import { lazy, Suspense, useCallback, useEffect, useMemo, useState } from "react";
import { getSkillRanking, type SkillRankingResponse } from "../api";
import { getStaleLinkedSessions } from "../agent-insights";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../types";
import TooltipLabel from "./common/TooltipLabel";
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
  AchievementWidget,
  ActivityFeedWidget,
  CookingHeartRoleBoardWidget,
  CronTimelineWidget,
  GitHubIssuesWidget,
  HeatmapWidget,
  KanbanOpsWidget,
  MachineStatusWidget,
  MvpWidget,
  SkillTrendWidget,
  StreakWidget,
} from "./dashboard/ExtraWidgets";
import HealthWidget from "./dashboard/HealthWidget";
import { DEPT_COLORS, useNow, type TFunction } from "./dashboard/model";
import TokenAnalyticsSection from "./dashboard/TokenAnalyticsSection";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));

type DashboardKanbanSignal = "review" | "blocked" | "requested" | "stalled";

interface DashboardPageViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  sessions: DispatchedSession[];
  meetings: RoundTableMeeting[];
  settings: CompanySettings;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (signal: DashboardKanbanSignal) => void;
  onOpenDispatchSessions?: () => void;
  onOpenSettings?: () => void;
  onOpenMeetings?: () => void;
}

export default function DashboardPageView({
  stats,
  agents,
  sessions,
  meetings,
  settings,
  onSelectAgent,
  onOpenKanbanSignal,
  onOpenDispatchSessions,
  onOpenSettings,
  onOpenMeetings,
}: DashboardPageViewProps) {
  const language = settings.language;
  const localeTag = language === "ko" ? "ko-KR" : language === "ja" ? "ja-JP" : language === "zh" ? "zh-CN" : "en-US";
  const numberFormatter = useMemo(() => new Intl.NumberFormat(localeTag), [localeTag]);

  const t: TFunction = useCallback(
    (messages) => messages[language] ?? messages.ko,
    [language],
  );

  const { date, time, briefing } = useNow(localeTag, t);
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(null);
  const [skillWindow, setSkillWindow] = useState<"7d" | "30d" | "all">("7d");

  useEffect(() => {
    let mounted = true;
    const load = async () => {
      try {
        const data = await getSkillRanking(skillWindow, 10);
        if (mounted) setSkillRanking(data);
      } catch {
        // Ignore auth/network errors in dashboard widgets.
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
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
          <div>{t({ ko: "대시보드 로딩 중...", en: "Loading dashboard...", ja: "ダッシュボード読み込み中...", zh: "仪表板加载中..." })}</div>
        </div>
      </div>
    );
  }

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

  const topAgents: RankedAgent[] = stats.top_agents.map((agent) => ({
    id: agent.id,
    name: agent.alias || agent.name_ko || agent.name,
    department: "",
    tasksDone: agent.stats_tasks_done,
    xp: agent.stats_xp,
  }));

  const podiumOrder: RankedAgent[] =
    topAgents.length >= 3
      ? [topAgents[1], topAgents[0], topAgents[2]]
      : topAgents.length === 2
        ? [topAgents[1], topAgents[0]]
        : [];

  const agentMap = new Map(agents.map((agent) => [agent.id, agent]));
  const maxXp = topAgents.reduce((max, agent) => Math.max(max, agent.xp), 1);

  const totalXpAll = stats.departments.reduce((sum, department) => sum + (department.sum_xp ?? 0), 0);
  const deptData: DepartmentPerformance[] = stats.departments.map((department, index) => ({
    id: department.id,
    name: department.name_ko || department.name,
    icon: department.icon,
    done: department.sum_xp ?? 0,
    total: totalXpAll,
    ratio: totalXpAll > 0 ? Math.round(((department.sum_xp ?? 0) / totalXpAll) * 100) : 0,
    color: DEPT_COLORS[index % DEPT_COLORS.length],
  }));

  const workingAgents = agents.filter((agent) => agent.status === "working");
  const idleAgents = agents.filter((agent) => agent.status !== "working");

  const staleLinkedSessions = useMemo(() => getStaleLinkedSessions(sessions), [sessions]);
  const reconnectingSessions = useMemo(
    () => sessions.filter((session) => session.linked_agent_id && session.status === "disconnected"),
    [sessions],
  );
  const activeMeetings = useMemo(
    () => meetings.filter((meeting) => meeting.status === "in_progress"),
    [meetings],
  );
  const recentMeetings = useMemo(
    () => [...meetings].sort((a, b) => {
      const left = a.started_at || a.created_at;
      const right = b.started_at || b.created_at;
      return right - left;
    }).slice(0, 4),
    [meetings],
  );
  const openMeetingFollowUps = useMemo(
    () => meetings.reduce((sum, meeting) => sum + countOpenMeetingIssues(meeting), 0),
    [meetings],
  );

  const xpChampion = useMemo(
    () => [...stats.departments].sort((a, b) => (b.sum_xp ?? 0) - (a.sum_xp ?? 0))[0] ?? null,
    [stats.departments],
  );
  const busiestDept = useMemo(
    () => [...stats.departments].sort((a, b) => b.working_agents - a.working_agents)[0] ?? null,
    [stats.departments],
  );
  const largestDept = useMemo(
    () => [...stats.departments].sort((a, b) => b.total_agents - a.total_agents)[0] ?? null,
    [stats.departments],
  );

  return (
    <div
      className="mx-auto h-full max-w-6xl min-w-0 space-y-5 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
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

      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.2fr)_minmax(0,0.8fr)]">
        <div className="space-y-4 min-w-0">
          <section
            className="rounded-2xl border p-4 sm:p-5"
            style={{
              borderColor: "rgba(96,165,250,0.26)",
              background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 90%, #0f172a 10%), var(--th-surface))",
            }}
          >
            <SectionHeader
              title={t({ ko: "운영 시그널", en: "Ops Signals", ja: "運用シグナル", zh: "运营信号" })}
              subtitle={t({
                ko: "병목, 세션 이상, 회의 후속 작업을 즉시 점검합니다",
                en: "Check bottlenecks, session anomalies, and meeting follow-ups at a glance",
                ja: "ボトルネック、セッション異常、会議後続タスクをひと目で確認",
                zh: "快速检查瓶颈、会话异常和会议后续动作",
              })}
            />

            <div className="mt-4 grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
              <DashboardSignalCard
                label={t({ ko: "세션 신호", en: "Session Signal", ja: "セッション信号", zh: "会话信号" })}
                value={staleLinkedSessions.length + reconnectingSessions.length}
                accent="#f97316"
                sublabel={t({
                  ko: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                  en: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                  ja: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                  zh: `${staleLinkedSessions.length} stale / ${reconnectingSessions.length} reconnecting`,
                })}
                actionLabel={t({ ko: "Dispatch 보기", en: "Open Dispatch", ja: "Dispatch を開く", zh: "打开 Dispatch" })}
                onAction={onOpenDispatchSessions}
              />
              <DashboardSignalCard
                label={t({ ko: "리뷰 대기", en: "Review Queue", ja: "レビュー待ち", zh: "待审查" })}
                value={stats.kanban.review_queue}
                accent="#14b8a6"
                sublabel={t({
                  ko: "검토/판정이 필요한 카드",
                  en: "Cards waiting for review or decision",
                  ja: "レビューまたは判断待ちカード",
                  zh: "等待审查或决策的卡片",
                })}
                actionLabel={t({ ko: "칸반 열기", en: "Open Kanban", ja: "カンバンを開く", zh: "打开看板" })}
                onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("review") : undefined}
              />
              <DashboardSignalCard
                label={t({ ko: "블록됨", en: "Blocked", ja: "ブロック", zh: "阻塞" })}
                value={stats.kanban.blocked}
                accent="#ef4444"
                sublabel={t({
                  ko: "해결을 기다리는 카드",
                  en: "Cards waiting on unblock",
                  ja: "解消待ちカード",
                  zh: "等待解除阻塞的卡片",
                })}
                actionLabel={t({ ko: "막힘 카드 보기", en: "Open Blocked", ja: "Blocked を開く", zh: "打开阻塞卡片" })}
                onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("blocked") : undefined}
              />
              <DashboardSignalCard
                label={t({ ko: "수락 대기", en: "Waiting Acceptance", ja: "受諾待ち", zh: "等待接收" })}
                value={stats.kanban.waiting_acceptance}
                accent="#8b5cf6"
                sublabel={t({
                  ko: "requested 상태에서 멈춘 카드",
                  en: "Cards stalled in requested",
                  ja: "requested で止まったカード",
                  zh: "停留在 requested 的卡片",
                })}
                actionLabel={t({ ko: "requested 보기", en: "Open Requested", ja: "requested を開く", zh: "打开 requested" })}
                onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("requested") : undefined}
              />
              <DashboardSignalCard
                label={t({ ko: "진행 정체", en: "Stale In Progress", ja: "進行停滞", zh: "进行停滞" })}
                value={stats.kanban.stale_in_progress}
                accent="#f59e0b"
                sublabel={t({
                  ko: "100분 이상 in_progress",
                  en: "In progress for 100+ minutes",
                  ja: "100分以上 in_progress",
                  zh: "in_progress 超过 100 分钟",
                })}
                actionLabel={t({ ko: "정체 카드 보기", en: "Open Stale", ja: "停滞カードを開く", zh: "打开停滞卡片" })}
                onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("stalled") : undefined}
              />
              <DashboardSignalCard
                label={t({ ko: "회의 후속", en: "Meeting Follow-up", ja: "会議フォローアップ", zh: "会议后续" })}
                value={openMeetingFollowUps}
                accent="#22c55e"
                sublabel={t({
                  ko: `${activeMeetings.length} active / ${meetings.length} total`,
                  en: `${activeMeetings.length} active / ${meetings.length} total`,
                  ja: `${activeMeetings.length} active / ${meetings.length} total`,
                  zh: `${activeMeetings.length} active / ${meetings.length} total`,
                })}
                actionLabel={t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
                onAction={onOpenMeetings}
              />
            </div>
          </section>

          <MeetingTimelineCard
            meetings={recentMeetings}
            activeCount={activeMeetings.length}
            followUpCount={openMeetingFollowUps}
            localeTag={localeTag}
            t={t}
            onOpenMeetings={onOpenMeetings}
          />
        </div>

        <div className="space-y-4 min-w-0">
          <HealthWidget t={t} />
        </div>
      </div>

      <TokenAnalyticsSection
        t={t}
        numberFormatter={numberFormatter}
        onOpenSettings={onOpenSettings}
      />

      <section className="space-y-4">
        <SectionHeader
          title={t({ ko: "게이미피케이션", en: "Gamification", ja: "ゲーミフィケーション", zh: "游戏化" })}
          subtitle={t({
            ko: "XP 순위, 보상, streak, 팀 하이라이트를 대시보드에서 바로 확인합니다",
            en: "Track XP ranks, rewards, streaks, and team highlights directly in the dashboard",
            ja: "XP 順位、報酬、連続記録、チームの見どころをダッシュボードで確認",
            zh: "在仪表板中直接查看 XP 排名、奖励、连续记录和团队亮点",
          })}
        />

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

        <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
          <StreakWidget agents={agents} t={t} />
          <AchievementWidget t={t} />
          <MvpWidget agents={agents} t={t} isKo={language === "ko"} />
        </div>

        <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
          <SkillRankingSnapshot
            skillRanking={skillRanking}
            skillWindow={skillWindow}
            onChangeWindow={setSkillWindow}
            numberFormatter={numberFormatter}
            t={t}
          />
          <TeamAchievementCard
            xpChampion={xpChampion}
            busiestDept={busiestDept}
            largestDept={largestDept}
            numberFormatter={numberFormatter}
            t={t}
          />
        </div>
      </section>

      <section className="space-y-4">
        <SectionHeader
          title={t({ ko: "운영 디테일", en: "Operations Detail", ja: "運用ディテール", zh: "运营细节" })}
          subtitle={t({
            ko: "칸반 압력, 부서 퍼포먼스, 실시간 활동을 세부적으로 봅니다",
            en: "Dive deeper into kanban pressure, department performance, and live activity",
            ja: "カンバン圧力、部門パフォーマンス、リアルタイム活動を深掘り",
            zh: "深入查看看板压力、部门表现和实时活动",
          })}
        />

        <KanbanOpsWidget kanban={stats.kanban} t={t} />

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

        <ActivityFeedWidget agents={agents} t={t} />
      </section>

      <section className="space-y-4">
        <SectionHeader
          title={t({ ko: "스킬 허브", en: "Skill Hub", ja: "スキルハブ", zh: "技能中心" })}
          subtitle={t({
            ko: "호출 랭킹, 추세, 카탈로그를 한 곳에 모읍니다",
            en: "Keep skill ranking, trends, and catalog together in one place",
            ja: "呼び出しランキング、推移、カタログを一か所に集約",
            zh: "将技能排行、趋势和目录集中到一处",
          })}
        />

        <SkillRankingSection
          skillRanking={skillRanking}
          skillWindow={skillWindow}
          onChangeWindow={setSkillWindow}
          numberFormatter={numberFormatter}
          t={t}
        />

        <SkillTrendWidget t={t} />

        <Suspense
          fallback={(
            <div className="py-8 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "카탈로그 로딩 중...", en: "Loading catalog...", ja: "カタログ読み込み中...", zh: "加载目录中..." })}
            </div>
          )}
        >
          <SkillCatalogView embedded />
        </Suspense>
      </section>

      <section className="space-y-4">
        <SectionHeader
          title={t({ ko: "인프라", en: "Infrastructure", ja: "インフラ", zh: "基础设施" })}
          subtitle={t({
            ko: "머신 상태와 자동화, 업무 히트맵까지 그대로 유지합니다",
            en: "Keep machine status, automation, and work heatmaps together in the dashboard",
            ja: "マシン状態、自動化、作業ヒートマップをダッシュボードに統合",
            zh: "将机器状态、自动化和工作热力图完整保留在仪表板中",
          })}
        />

        <MachineStatusWidget t={t} />
        <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
          <CronTimelineWidget t={t} />
          <HeatmapWidget agents={agents} t={t} />
        </div>
        <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
          <CookingHeartRoleBoardWidget agents={agents} t={t} isKo={language === "ko"} />
          <GitHubIssuesWidget t={t} repo={stats.kanban.top_repos[0]?.github_repo} />
        </div>
      </section>
    </div>
  );
}

function countOpenMeetingIssues(meeting: RoundTableMeeting): number {
  const totalIssues = meeting.proposed_issues?.length ?? 0;
  if (meeting.status !== "completed" || totalIssues === 0) return 0;

  const results = meeting.issue_creation_results ?? [];
  if (results.length === 0) {
    return Math.max(totalIssues - meeting.issues_created, 0);
  }

  const created = results.filter((result) => result.ok && result.discarded !== true).length;
  const discarded = results.filter((result) => result.discarded === true).length;
  return Math.max(totalIssues - created - discarded, 0);
}

function SectionHeader({ title, subtitle }: { title: string; subtitle: string }) {
  return (
    <div className="flex items-end justify-between gap-3">
      <div className="min-w-0">
        <h2 className="text-lg font-semibold" style={{ color: "var(--th-text-heading)" }}>
          {title}
        </h2>
        <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
          {subtitle}
        </p>
      </div>
    </div>
  );
}

function DashboardSignalCard({
  label,
  value,
  sublabel,
  accent,
  actionLabel,
  onAction,
}: {
  label: string;
  value: number;
  sublabel: string;
  accent: string;
  actionLabel: string;
  onAction?: () => void;
}) {
  return (
    <div
      className="rounded-2xl border p-4 min-w-0"
      style={{
        borderColor: `${accent}44`,
        background: `linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, ${accent} 8%), var(--th-surface))`,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="text-[11px] font-semibold uppercase tracking-[0.14em]" style={{ color: accent }}>
            {label}
          </div>
          <div className="mt-2 text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {value}
          </div>
          <p className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
            {sublabel}
          </p>
        </div>
        {onAction && (
          <button
            type="button"
            onClick={onAction}
            className="shrink-0 rounded-xl px-3 py-2 text-xs font-medium transition-colors"
            style={{
              color: accent,
              border: `1px solid ${accent}44`,
              background: `${accent}14`,
            }}
          >
            {actionLabel}
          </button>
        )}
      </div>
    </div>
  );
}

function MeetingTimelineCard({
  meetings,
  activeCount,
  followUpCount,
  localeTag,
  t,
  onOpenMeetings,
}: {
  meetings: RoundTableMeeting[];
  activeCount: number;
  followUpCount: number;
  localeTag: string;
  t: TFunction;
  onOpenMeetings?: () => void;
}) {
  const formatter = useMemo(
    () => new Intl.DateTimeFormat(localeTag, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" }),
    [localeTag],
  );

  return (
    <section
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "rgba(34,197,94,0.22)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, #14532d 8%), var(--th-surface))",
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <h3 className="text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {t({ ko: "회의 타임라인", en: "Meeting Timeline", ja: "会議タイムライン", zh: "会议时间线" })}
          </h3>
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: `${activeCount}개 진행 중, 후속 이슈 ${followUpCount}개 미정리`,
              en: `${activeCount} active, ${followUpCount} follow-up issues still open`,
              ja: `${activeCount}件進行中、後続イシュー ${followUpCount}件 未整理`,
              zh: `${activeCount} 个进行中，${followUpCount} 个后续 issue 未整理`,
            })}
          </p>
        </div>
        {onOpenMeetings && (
          <button
            type="button"
            onClick={onOpenMeetings}
            className="shrink-0 rounded-xl px-3 py-2 text-xs font-medium"
            style={{
              color: "#86efac",
              border: "1px solid rgba(134,239,172,0.28)",
              background: "rgba(34,197,94,0.12)",
            }}
          >
            {t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
          </button>
        )}
      </div>

      <div className="mt-4 space-y-2">
        {meetings.length === 0 ? (
          <div className="rounded-xl border border-dashed px-4 py-6 text-sm text-center" style={{ borderColor: "var(--th-border)", color: "var(--th-text-muted)" }}>
            {t({ ko: "최근 회의가 없습니다.", en: "No recent meetings yet.", ja: "最近の会議はありません。", zh: "暂无最近会议。" })}
          </div>
        ) : (
          meetings.map((meeting) => {
            const statusColor =
              meeting.status === "in_progress" ? "#22c55e" : meeting.status === "completed" ? "#60a5fa" : "#94a3b8";
            const issueCount = countOpenMeetingIssues(meeting);
            return (
              <div
                key={meeting.id}
                className="rounded-xl border px-3 py-3"
                style={{
                  borderColor: `${statusColor}33`,
                  background: "rgba(15,23,42,0.26)",
                }}
              >
                <div className="flex items-start justify-between gap-3">
                  <div className="min-w-0">
                    <div className="flex items-center gap-2 flex-wrap">
                      <span className="text-[10px] font-semibold uppercase tracking-[0.14em]" style={{ color: statusColor }}>
                        {meeting.status}
                      </span>
                      <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                        {formatter.format(meeting.started_at || meeting.created_at)}
                      </span>
                    </div>
                    <div className="mt-1 font-medium truncate" style={{ color: "var(--th-text)" }}>
                      {meeting.agenda}
                    </div>
                  </div>
                  <div className="text-right shrink-0">
                    <div className="text-xs font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {meeting.primary_provider ? meeting.primary_provider.toUpperCase() : "RT"}
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {meeting.issues_created} created
                    </div>
                  </div>
                </div>
                <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                  <span className="rounded-full px-2 py-1" style={{ background: "rgba(255,255,255,0.06)", color: "var(--th-text-muted)" }}>
                    {meeting.participant_names.length} {t({ ko: "참여자", en: "participants", ja: "参加者", zh: "参与者" })}
                  </span>
                  <span className="rounded-full px-2 py-1" style={{ background: "rgba(255,255,255,0.06)", color: "var(--th-text-muted)" }}>
                    {meeting.total_rounds} {t({ ko: "라운드", en: "rounds", ja: "ラウンド", zh: "轮" })}
                  </span>
                  {issueCount > 0 && (
                    <span className="rounded-full px-2 py-1" style={{ background: "rgba(245,158,11,0.14)", color: "#fbbf24" }}>
                      {issueCount} {t({ ko: "후속 대기", en: "follow-up pending", ja: "後続待ち", zh: "后续待处理" })}
                    </span>
                  )}
                </div>
              </div>
            );
          })
        )}
      </div>
    </section>
  );
}

function SkillRankingSnapshot({
  skillRanking,
  skillWindow,
  onChangeWindow,
  numberFormatter,
  t,
}: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
}) {
  return (
    <section
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "var(--th-border)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, #f59e0b 8%), var(--th-surface))",
      }}
    >
      <div className="flex min-w-0 flex-wrap items-center justify-between gap-3">
        <div className="min-w-0 flex-1">
          <h3 className="text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
            {t({ ko: "스킬 랭킹", en: "Skill Ranking", ja: "スキルランキング", zh: "技能排行" })}
          </h3>
          <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "호출량 기준 상위 스킬과 에이전트", en: "Top skills and agents by call volume", ja: "呼び出し量ベースの上位スキルとエージェント", zh: "按调用量统计的热门技能与代理" })}
          </p>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {(["7d", "30d", "all"] as const).map((windowId) => (
            <button
              key={windowId}
              type="button"
              onClick={() => onChangeWindow(windowId)}
              className="text-[11px] px-2 py-1 rounded-md border"
              style={{
                borderColor: skillWindow === windowId ? "#f59e0b" : "var(--th-border)",
                color: skillWindow === windowId ? "#f59e0b" : "var(--th-text-muted)",
                background: skillWindow === windowId ? "rgba(245,158,11,0.12)" : "transparent",
              }}
            >
              {windowId}
            </button>
          ))}
        </div>
      </div>

      {!skillRanking || skillRanking.overall.length === 0 ? (
        <div className="mt-4 text-sm" style={{ color: "var(--th-text-muted)" }}>
          {t({ ko: "아직 집계된 스킬 호출이 없습니다.", en: "No skill usage aggregated yet.", ja: "まだ集計されたスキル呼び出しがありません。", zh: "尚无技能调用统计。" })}
        </div>
      ) : (
        <div className="mt-4 grid min-w-0 grid-cols-1 gap-4 xl:grid-cols-2">
          <div className="min-w-0">
            <div className="text-sm font-medium mb-2" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "전체 TOP 5", en: "Overall TOP 5", ja: "全体 TOP 5", zh: "全体 TOP 5" })}
            </div>
            <ol className="space-y-2">
              {skillRanking.overall.slice(0, 5).map((row, index) => (
                <li key={`${row.skill_name}-${index}`} className="flex items-start justify-between gap-3 text-sm">
                  <div className="min-w-0 flex flex-1 items-start gap-2" style={{ color: "var(--th-text)" }}>
                    <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                      {index + 1}.
                    </span>
                    <TooltipLabel text={row.skill_desc_ko} tooltip={row.skill_name} className="flex-1" />
                  </div>
                  <span className="font-semibold shrink-0" style={{ color: "#f59e0b" }}>
                    {numberFormatter.format(row.calls)}
                  </span>
                </li>
              ))}
            </ol>
          </div>

          <div className="min-w-0">
            <div className="text-sm font-medium mb-2" style={{ color: "var(--th-text-muted)" }}>
              {t({ ko: "에이전트별 TOP 5", en: "Top by Agent", ja: "エージェント別 TOP 5", zh: "按代理 TOP 5" })}
            </div>
            <ul className="space-y-2">
              {skillRanking.byAgent.slice(0, 5).map((row, index) => (
                <li key={`${row.agent_role_id}-${row.skill_name}-${index}`} className="flex items-start justify-between gap-3 text-sm">
                  <div className="min-w-0 flex flex-1 items-start gap-2" style={{ color: "var(--th-text)" }}>
                    <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                      {index + 1}.
                    </span>
                    <div className="min-w-0 flex flex-1 items-center gap-1">
                      <span className="truncate" title={row.agent_name}>
                        {row.agent_name}
                      </span>
                      <span className="shrink-0" style={{ color: "var(--th-text-muted)" }}>
                        ·
                      </span>
                      <TooltipLabel text={row.skill_desc_ko} tooltip={row.skill_name} className="flex-1" />
                    </div>
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
  );
}

function SkillRankingSection(props: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
}) {
  return <SkillRankingSnapshot {...props} />;
}

function TeamAchievementCard({
  xpChampion,
  busiestDept,
  largestDept,
  numberFormatter,
  t,
}: {
  xpChampion: DashboardStats["departments"][number] | null;
  busiestDept: DashboardStats["departments"][number] | null;
  largestDept: DashboardStats["departments"][number] | null;
  numberFormatter: Intl.NumberFormat;
  t: TFunction;
}) {
  const achievements = [
    xpChampion ? {
      id: "xp",
      icon: "🏆",
      title: t({ ko: "XP 챔피언", en: "XP Champion", ja: "XP チャンピオン", zh: "XP 冠军" }),
      name: xpChampion.name_ko || xpChampion.name,
      value: `${numberFormatter.format(xpChampion.sum_xp ?? 0)} XP`,
    } : null,
    busiestDept ? {
      id: "ops",
      icon: "⚙️",
      title: t({ ko: "가장 바쁜 팀", en: "Busiest Team", ja: "最も忙しいチーム", zh: "最忙团队" }),
      name: busiestDept.name_ko || busiestDept.name,
      value: t({
        ko: `${busiestDept.working_agents}/${busiestDept.total_agents} 가동`,
        en: `${busiestDept.working_agents}/${busiestDept.total_agents} active`,
        ja: `${busiestDept.working_agents}/${busiestDept.total_agents} 稼働`,
        zh: `${busiestDept.working_agents}/${busiestDept.total_agents} 活跃`,
      }),
    } : null,
    largestDept ? {
      id: "crew",
      icon: "🛡️",
      title: t({ ko: "최대 스쿼드", en: "Largest Squad", ja: "最大スクワッド", zh: "最大小队" }),
      name: largestDept.name_ko || largestDept.name,
      value: t({
        ko: `${largestDept.total_agents}명 규모`,
        en: `${largestDept.total_agents} members`,
        ja: `${largestDept.total_agents}名規模`,
        zh: `${largestDept.total_agents} 名成员`,
      }),
    } : null,
  ].filter((item): item is NonNullable<typeof item> => Boolean(item));

  return (
    <section
      className="rounded-2xl border p-4 sm:p-5"
      style={{
        borderColor: "rgba(34,197,94,0.24)",
        background: "linear-gradient(145deg, color-mix(in srgb, var(--th-surface) 92%, #22c55e 8%), var(--th-surface))",
      }}
    >
      <h3 className="text-base font-semibold" style={{ color: "var(--th-text-heading)" }}>
        {t({ ko: "팀 업적", en: "Team Achievements", ja: "チーム実績", zh: "团队成就" })}
      </h3>
      <p className="text-sm" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "오늘 가장 눈에 띄는 부서 하이라이트입니다", en: "Department highlights worth surfacing in the dashboard", ja: "ダッシュボードに載せるべき部門ハイライト", zh: "值得在仪表板中突出的部门亮点" })}
      </p>

      <div className="mt-4 space-y-3">
        {achievements.map((achievement) => (
          <div
            key={achievement.id}
            className="rounded-xl border px-3 py-3"
            style={{ borderColor: "rgba(255,255,255,0.08)", background: "rgba(15,23,42,0.22)" }}
          >
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <div className="text-xs font-semibold uppercase tracking-[0.14em]" style={{ color: "#86efac" }}>
                  {achievement.icon} {achievement.title}
                </div>
                <div className="mt-1 font-medium truncate" style={{ color: "var(--th-text)" }}>
                  {achievement.name}
                </div>
              </div>
              <div className="shrink-0 text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                {achievement.value}
              </div>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
