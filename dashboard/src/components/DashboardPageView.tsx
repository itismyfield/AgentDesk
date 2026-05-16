import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";
import { getSkillRanking, type SkillRankingResponse } from "../api";
import {
  formatElapsedCompact,
  getAgentWorkElapsedMs,
  getAgentWorkSummary,
  getStaleLinkedSessions,
} from "../agent-insights";
import {
  DASHBOARD_TABS,
  DASHBOARD_TAB_STORAGE_KEY,
  readDashboardTabFromStorage,
  readDashboardTabFromUrl,
  syncDashboardTabToUrl,
  type DashboardTab,
} from "../app/dashboardTabs";
import { summarizeMeetings } from "../app/meetingSummary";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../types";
import type { RankedAgent } from "./dashboard/HeroSections";
import { buildDepartmentPerformanceRows } from "./dashboard/ExtraWidgets";
import { DashboardHomeOverview } from "./dashboard/DashboardHomeOverview";
import { DashboardPageTabPanels } from "./dashboard/DashboardPageTabPanels";
import type { TFunction } from "./dashboard/model";
import { formatProviderFlow } from "./MeetingProviderFlow";

type PulseKanbanSignal = "review" | "blocked" | "requested" | "stalled";
type HomeSignalTone = "info" | "warn" | "danger" | "success";

interface HomeSignalRow {
  id: string;
  label: string;
  value: number;
  description: string;
  accent: string;
  tone: HomeSignalTone;
  onAction?: () => void;
}

interface HomeActivityItem {
  id: string;
  title: string;
  detail: string;
  timestamp: number;
  tone: "success" | "warn";
}

interface HomeAgentRow {
  agent: Agent;
  displayName: string;
  workSummary: string | null;
  elapsedLabel: string | null;
  linkedSessions: DispatchedSession[];
}

interface DashboardTabDefinition {
  id: DashboardTab;
  label: string;
  detail: string;
}

interface DashboardPageViewProps {
  stats: DashboardStats | null;
  agents: Agent[];
  sessions: DispatchedSession[];
  meetings: RoundTableMeeting[];
  settings: CompanySettings;
  requestedTab?: DashboardTab | null;
  onSelectAgent?: (agent: Agent) => void;
  onOpenKanbanSignal?: (signal: PulseKanbanSignal) => void;
  onOpenDispatchSessions?: () => void;
  onOpenSettings?: () => void;
  onRefreshMeetings?: () => void;
  onRequestedTabHandled?: () => void;
}

const EMPTY_DASHBOARD_STATS: DashboardStats = {
  agents: {
    total: 0,
    working: 0,
    idle: 0,
    break: 0,
    offline: 0,
  },
  top_agents: [],
  departments: [],
  dispatched_count: 0,
  github_closed_today: 0,
  kanban: {
    open_total: 0,
    review_queue: 0,
    blocked: 0,
    failed: 0,
    waiting_acceptance: 0,
    stale_in_progress: 0,
    by_status: {} as DashboardStats["kanban"]["by_status"],
    top_repos: [],
  },
};

function getLocalizedAgentName(
  agent: Pick<Agent, "alias" | "name" | "name_ko" | "name_ja" | "name_zh">,
  language: CompanySettings["language"],
): string {
  if (agent.alias?.trim()) return agent.alias;
  if (language === "ja") return agent.name_ja || agent.name_ko || agent.name;
  if (language === "zh") return agent.name_zh || agent.name_ko || agent.name;
  if (language === "en") return agent.name;
  return agent.name_ko || agent.name;
}

export default function DashboardPageView({
  stats,
  agents,
  sessions,
  meetings,
  settings,
  requestedTab,
  onSelectAgent,
  onOpenKanbanSignal,
  onOpenDispatchSessions,
  onOpenSettings,
  onRefreshMeetings,
  onRequestedTabHandled,
}: DashboardPageViewProps) {
  const language = settings.language;
  const localeTag = language === "ko" ? "ko-KR" : language === "ja" ? "ja-JP" : language === "zh" ? "zh-CN" : "en-US";
  const numberFormatter = useMemo(() => new Intl.NumberFormat(localeTag), [localeTag]);
  const t: TFunction = useCallback((messages) => messages[language] ?? messages.ko, [language]);
  const [activeTab, setActiveTab] = useState<DashboardTab>(() => readDashboardTabFromUrl());
  const [skillRanking, setSkillRanking] = useState<SkillRankingResponse | null>(null);
  const [skillWindow, setSkillWindow] = useState<"7d" | "30d" | "all">("30d");
  const [skillRankingUpdatedAt, setSkillRankingUpdatedAt] = useState<number | null>(null);
  const [skillRankingRefreshFailed, setSkillRankingRefreshFailed] = useState(false);
  const tabButtonRefs = useRef<Record<DashboardTab, HTMLButtonElement | null>>({
    operations: null,
    tokens: null,
    automation: null,
    achievements: null,
    meetings: null,
  });
  const hasSyncedInitialTabRef = useRef(false);

  const tabDefinitions: DashboardTabDefinition[] = useMemo(
    () => [
      {
        id: "operations",
        label: t({ ko: "운영", en: "Operations", ja: "運用", zh: "运营" }),
        detail: t({ ko: "HEALTH + 프로바이더 상태", en: "HEALTH + provider status", ja: "HEALTH + provider 状態", zh: "HEALTH + provider 状态" }),
      },
      {
        id: "tokens",
        label: t({ ko: "토큰", en: "Tokens", ja: "トークン", zh: "Token" }),
        detail: t({ ko: "히트맵 + 비용 + ROI", en: "Heatmap + spend + ROI", ja: "ヒートマップ + コスト + ROI", zh: "热力图 + 成本 + ROI" }),
      },
      {
        id: "automation",
        label: t({ ko: "자동화", en: "Automation", ja: "自動化", zh: "自动化" }),
        detail: t({ ko: "크론 + 스킬 허브", en: "Cron + skill hub", ja: "Cron + スキルハブ", zh: "Cron + 技能中心" }),
      },
      {
        id: "achievements",
        label: t({ ko: "업적", en: "Achievements", ja: "実績", zh: "成就" }),
        detail: t({ ko: "랭킹 + 업적", en: "Ranking + achievements", ja: "ランキング + 実績", zh: "排行 + 成就" }),
      },
      {
        id: "meetings",
        label: t({ ko: "회의", en: "Meetings", ja: "会議", zh: "会议" }),
        detail: t({ ko: "기록 + 후속 일감", en: "Records + follow-ups", ja: "記録 + フォローアップ", zh: "记录 + 后续事项" }),
      },
    ],
    [t],
  );

  const focusDashboardTab = useCallback((tab: DashboardTab) => {
    setActiveTab(tab);
    window.requestAnimationFrame(() => {
      tabButtonRefs.current[tab]?.focus();
    });
  }, []);

  const handleTabKeyDown = useCallback(
    (event: ReactKeyboardEvent<HTMLButtonElement>, tab: DashboardTab) => {
      const currentIndex = DASHBOARD_TABS.indexOf(tab);
      if (currentIndex < 0) return;

      let nextTab: DashboardTab | null = null;
      if (event.key === "ArrowRight" || event.key === "ArrowDown") {
        nextTab = DASHBOARD_TABS[(currentIndex + 1) % DASHBOARD_TABS.length];
      } else if (event.key === "ArrowLeft" || event.key === "ArrowUp") {
        nextTab = DASHBOARD_TABS[(currentIndex - 1 + DASHBOARD_TABS.length) % DASHBOARD_TABS.length];
      } else if (event.key === "Home") {
        nextTab = DASHBOARD_TABS[0];
      } else if (event.key === "End") {
        nextTab = DASHBOARD_TABS[DASHBOARD_TABS.length - 1];
      }

      if (!nextTab) return;
      event.preventDefault();
      focusDashboardTab(nextTab);
    },
    [focusDashboardTab],
  );

  useEffect(() => {
    syncDashboardTabToUrl(activeTab, { replace: !hasSyncedInitialTabRef.current });
    hasSyncedInitialTabRef.current = true;
  }, [activeTab]);

  useEffect(() => {
    const handlePopState = () => setActiveTab(readDashboardTabFromUrl());
    window.addEventListener("popstate", handlePopState);
    return () => window.removeEventListener("popstate", handlePopState);
  }, []);

  useEffect(() => {
    const handleStorage = (event: StorageEvent) => {
      if (event.key !== DASHBOARD_TAB_STORAGE_KEY) return;
      const nextTab = readDashboardTabFromStorage() ?? "operations";
      setActiveTab((currentTab) => (currentTab === nextTab ? currentTab : nextTab));
    };

    window.addEventListener("storage", handleStorage);
    return () => window.removeEventListener("storage", handleStorage);
  }, []);

  useEffect(() => {
    if (!requestedTab) return;
    focusDashboardTab(requestedTab);
    onRequestedTabHandled?.();
  }, [focusDashboardTab, requestedTab, onRequestedTabHandled]);

  useEffect(() => {
    tabButtonRefs.current[activeTab]?.scrollIntoView({
      behavior: "smooth",
      block: "nearest",
      inline: "center",
    });
  }, [activeTab]);

  useEffect(() => {
    if (activeTab !== "achievements") return;
    let mounted = true;

    const load = async () => {
      try {
        const next = await getSkillRanking(skillWindow, 10);
        if (!mounted) return;
        setSkillRanking(next);
        setSkillRankingUpdatedAt(Date.now());
        setSkillRankingRefreshFailed(false);
      } catch {
        // Keep the last successful ranking during transient network failures.
        if (mounted) setSkillRankingRefreshFailed(true);
      }
    };

    void load();
    const timer = setInterval(() => void load(), 60_000);
    return () => {
      mounted = false;
      clearInterval(timer);
    };
  }, [activeTab, skillWindow]);

  const dashboardStats = stats ?? EMPTY_DASHBOARD_STATS;

  const topAgents: RankedAgent[] = dashboardStats.top_agents.map((agent) => ({
    id: agent.id,
    name: getLocalizedAgentName(agent, language),
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
  const workingAgents = useMemo(() => agents.filter((agent) => agent.status === "working"), [agents]);
  const idleAgentsList = useMemo(() => agents.filter((agent) => agent.status !== "working"), [agents]);
  const deptPerformanceRows = useMemo(
    () => buildDepartmentPerformanceRows(dashboardStats.departments, language),
    [dashboardStats.departments, language],
  );
  const topGithubRepo = dashboardStats.kanban.top_repos[0]?.github_repo;
  const staleLinkedSessions = useMemo(() => getStaleLinkedSessions(sessions), [sessions]);
  const reconnectingSessions = useMemo(
    () => sessions.filter((session) => session.linked_agent_id && session.status === "disconnected"),
    [sessions],
  );
  const meetingSummary = useMemo(() => summarizeMeetings(meetings), [meetings]);
  const recentMeetings = useMemo(
    () =>
      [...meetings]
        .sort((left, right) => {
          const leftTime = left.started_at || left.created_at;
          const rightTime = right.started_at || right.created_at;
          return rightTime - leftTime;
        })
        .slice(0, 4),
    [meetings],
  );
  if (!stats) {
    return (
      <div className="flex h-full items-center justify-center" style={{ color: "var(--th-text-muted)" }}>
        <div className="text-center">
          <div className="mb-4 text-4xl opacity-30">📊</div>
          <div>{t({ ko: "대시보드를 불러오는 중입니다", en: "Loading dashboard", ja: "ダッシュボードを読み込み中", zh: "正在加载仪表盘" })}</div>
        </div>
      </div>
    );
  }

  return (
    <div
      className="page fade-in mx-auto h-full w-full max-w-7xl min-w-0 space-y-4 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:space-y-5 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <DashboardHomeOverview
        t={t}
        numberFormatter={numberFormatter}
        dashboardStats={dashboardStats}
        staleLinkedSessions={staleLinkedSessions}
        reconnectingSessions={reconnectingSessions}
        meetingSummary={meetingSummary}
        meetings={meetings}
        recentMeetings={recentMeetings}
        language={language}
        agents={agents}
        sessions={sessions}
        localeTag={localeTag}
        onOpenKanbanSignal={onOpenKanbanSignal}
        onSelectAgent={onSelectAgent}
        tabDefinitions={tabDefinitions}
        activeTab={activeTab}
        onSelectTab={setActiveTab}
        onTabKeyDown={handleTabKeyDown}
        tabButtonRefs={tabButtonRefs}
      />

      <DashboardPageTabPanels
        activeTab={activeTab}
        t={t}
        localeTag={localeTag}
        staleLinkedSessions={staleLinkedSessions}
        reconnectingSessions={reconnectingSessions}
        dashboardStats={dashboardStats}
        onOpenDispatchSessions={onOpenDispatchSessions}
        onOpenKanbanSignal={onOpenKanbanSignal}
        meetingSummary={meetingSummary}
        meetings={meetings}
        recentMeetings={recentMeetings}
        onOpenSettings={onOpenSettings}
        agents={agents}
        deptPerformanceRows={deptPerformanceRows}
        workingAgents={workingAgents}
        idleAgentsList={idleAgentsList}
        language={language}
        numberFormatter={numberFormatter}
        onSelectAgent={onSelectAgent}
        topGithubRepo={topGithubRepo}
        skillRanking={skillRanking}
        skillWindow={skillWindow}
        onChangeSkillWindow={setSkillWindow}
        skillRankingUpdatedAt={skillRankingUpdatedAt}
        skillRankingRefreshFailed={skillRankingRefreshFailed}
        topAgents={topAgents}
        podiumOrder={podiumOrder}
        agentMap={agentMap}
        maxXp={maxXp}
        onRefreshMeetings={onRefreshMeetings}
        onSelectTab={setActiveTab}
      />
    </div>
  );
}
