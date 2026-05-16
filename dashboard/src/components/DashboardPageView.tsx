import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import {
  closestCenter,
  DndContext,
  KeyboardSensor,
  MouseSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
  type DragOverEvent,
  type DragStartEvent,
} from "@dnd-kit/core";
import {
  arrayMove,
  rectSortingStrategy,
  SortableContext,
  sortableKeyboardCoordinates,
} from "@dnd-kit/sortable";
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
import {
  countOpenMeetingIssues,
  summarizeMeetings,
} from "../app/meetingSummary";
import type {
  Agent,
  CompanySettings,
  DashboardStats,
  DispatchedSession,
  RoundTableMeeting,
} from "../types";
import {
  SurfaceActionButton,
  SurfaceEmptyState,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";
import {
  DashboardRankingBoard,
  type RankedAgent,
} from "./dashboard/HeroSections";
import {
  DashboardHomeActivityWidget,
  DashboardHomeMetricTile,
  DashboardHomeOfficeWidget,
  DashboardHomeRosterWidget,
  DashboardHomeSectionNavigatorWidget,
  DashboardHomeSignalsWidget,
  DashboardSortableWidget,
  DashboardTabPanel,
  MeetingTimelineCard,
  PulseSectionShell,
  PulseSignalCard,
  SkillRankingSection,
} from "./dashboard/DashboardHomeRenderers";
import {
  AchievementWidget,
  AgentQualityWidget,
  AutoQueueHistoryWidget,
  BottleneckWidget,
  CronTimelineWidget,
  DashboardDeptAndSquad,
  GitHubIssuesWidget,
  HeatmapWidget,
  SkillTrendWidget,
  buildDepartmentPerformanceRows,
} from "./dashboard/ExtraWidgets";
import HealthWidget from "./dashboard/HealthWidget";
import RateLimitWidget from "./dashboard/RateLimitWidget";
import TokenAnalyticsSection from "./dashboard/TokenAnalyticsSection";
import ReceiptWidget from "./dashboard/ReceiptWidget";
import type { TFunction } from "./dashboard/model";
import {
  DEFAULT_HOME_WIDGET_ORDER,
  HOME_WIDGET_STORAGE_KEY,
  readStoredHomeWidgetOrder,
  type HomeWidgetId,
} from "./dashboard/homeWidgetOrder";
import { formatProviderFlow } from "./MeetingProviderFlow";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
const MeetingMinutesView = lazy(() => import("./MeetingMinutesView"));

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
  const [editingWidgets, setEditingWidgets] = useState(false);
  const [widgetOrder, setWidgetOrder] = useState<HomeWidgetId[]>(() =>
    readStoredHomeWidgetOrder(typeof window === "undefined" ? null : window.localStorage),
  );
  const [activeWidgetId, setActiveWidgetId] = useState<HomeWidgetId | null>(null);
  const [overWidgetId, setOverWidgetId] = useState<HomeWidgetId | null>(null);
  const widgetDragSensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  const activeSessions = useMemo(
    () => sessions.filter((session) => session.status !== "disconnected"),
    [sessions],
  );
  const linkedSessionsByAgent = useMemo(() => {
    const map = new Map<string, DispatchedSession[]>();
    for (const session of sessions) {
      if (!session.linked_agent_id) continue;
      const rows = map.get(session.linked_agent_id) ?? [];
      rows.push(session);
      map.set(session.linked_agent_id, rows);
    }
    return map;
  }, [sessions]);
  const homeAgents = useMemo<HomeAgentRow[]>(
    () =>
      [...agents]
        .map((agent) => {
          const linkedSessions = linkedSessionsByAgent.get(agent.id) ?? [];
          const workSummary = getAgentWorkSummary(agent, { linkedSessions });
          const elapsedMs = getAgentWorkElapsedMs(agent, linkedSessions);
          return {
            agent,
            displayName: getLocalizedAgentName(agent, language),
            workSummary,
            elapsedLabel: elapsedMs ? formatElapsedCompact(elapsedMs, language === "ko") : null,
            linkedSessions,
          };
        })
        .sort((left, right) => {
          if (left.agent.status === right.agent.status) {
            return right.agent.stats_xp - left.agent.stats_xp;
          }
          if (left.agent.status === "working") return -1;
          if (right.agent.status === "working") return 1;
          if (left.agent.status === "idle") return -1;
          if (right.agent.status === "idle") return 1;
          return 0;
        }),
    [agents, language, linkedSessionsByAgent],
  );
  const activeProviderCount = useMemo(() => {
    const providers = new Set<string>();
    for (const session of activeSessions) providers.add(session.provider);
    if (providers.size === 0) {
      for (const agent of agents) {
        if (agent.cli_provider) providers.add(agent.cli_provider);
      }
    }
    return providers.size;
  }, [activeSessions, agents]);
  const dateLabel = useMemo(() => {
    const formatted = new Intl.DateTimeFormat(localeTag, {
      weekday: "long",
      month: "short",
      day: "numeric",
    }).format(new Date());
    return formatted.replace(", ", " · ");
  }, [localeTag]);
  const systemState = useMemo(() => {
    if (staleLinkedSessions.length > 0 || reconnectingSessions.length > 0 || dashboardStats.kanban.blocked > 0) {
      return {
        label: t({
          ko: "주의 필요",
          en: "attention needed",
          ja: "注意が必要",
          zh: "需要关注",
        }),
        color: "var(--th-accent-warn)",
        pulseColor: "var(--th-accent-warn)",
      };
    }
    if (dashboardStats.kanban.review_queue > 0 || dashboardStats.kanban.waiting_acceptance > 0) {
      return {
        label: t({
          ko: "큐 모니터링 중",
          en: "watching queues",
          ja: "キューを監視中",
          zh: "监控队列中",
        }),
        color: "var(--th-accent-info)",
        pulseColor: "var(--th-accent-info)",
      };
    }
    return {
      label: t({
        ko: "all systems normal",
        en: "all systems normal",
        ja: "all systems normal",
        zh: "all systems normal",
      }),
      color: "var(--th-accent-success)",
      pulseColor: "var(--th-accent-success)",
    };
  }, [
    dashboardStats.kanban.blocked,
    dashboardStats.kanban.review_queue,
    dashboardStats.kanban.waiting_acceptance,
    reconnectingSessions.length,
    staleLinkedSessions.length,
    t,
  ]);
  const focusSignals = useMemo<HomeSignalRow[]>(
    () => [
      {
        id: "review",
        label: t({ ko: "리뷰 대기", en: "Review Queue", ja: "レビュー待ち", zh: "待审查" }),
        value: dashboardStats.kanban.review_queue,
        description: t({
          ko: "검토/판정이 필요한 카드",
          en: "Cards waiting for review or decision",
          ja: "レビューまたは判断待ちカード",
          zh: "等待审查或决策的卡片",
        }),
        accent: "#14b8a6",
        tone: "success",
        onAction: onOpenKanbanSignal ? () => onOpenKanbanSignal("review") : undefined,
      },
      {
        id: "blocked",
        label: t({ ko: "블록됨", en: "Blocked", ja: "ブロック", zh: "阻塞" }),
        value: dashboardStats.kanban.blocked,
        description: t({
          ko: "해소나 수동 개입이 필요한 카드",
          en: "Cards waiting on unblock or manual action",
          ja: "解除や手動介入が必要なカード",
          zh: "等待解除阻塞或人工处理的卡片",
        }),
        accent: "#ef4444",
        tone: "danger",
        onAction: onOpenKanbanSignal ? () => onOpenKanbanSignal("blocked") : undefined,
      },
      {
        id: "requested",
        label: t({ ko: "수락 지연", en: "Waiting Acceptance", ja: "受諾遅延", zh: "接收延迟" }),
        value: dashboardStats.kanban.waiting_acceptance,
        description: t({
          ko: "requested 상태에 머무는 카드",
          en: "Cards stalled in requested",
          ja: "requested に留まるカード",
          zh: "停留在 requested 的卡片",
        }),
        accent: "#10b981",
        tone: "info",
        onAction: onOpenKanbanSignal ? () => onOpenKanbanSignal("requested") : undefined,
      },
      {
        id: "stale",
        label: t({ ko: "진행 정체", en: "Stale In Progress", ja: "進行停滞", zh: "进行停滞" }),
        value: dashboardStats.kanban.stale_in_progress,
        description: t({
          ko: "오래 머무는 in_progress 카드",
          en: "Cards stuck in progress",
          ja: "進行が長引く in_progress カード",
          zh: "长时间停留在 in_progress 的卡片",
        }),
        accent: "#f59e0b",
        tone: "warn",
        onAction: onOpenKanbanSignal ? () => onOpenKanbanSignal("stalled") : undefined,
      },
      {
        id: "followup",
        label: t({ ko: "회의 후속", en: "Meeting Follow-up", ja: "会議フォローアップ", zh: "会议后续" }),
        value: meetingSummary.unresolvedCount,
        description: t({
          ko: `${meetingSummary.activeCount}개 진행 중 회의에서 남은 후속 이슈`,
          en: `Open follow-ups from ${meetingSummary.activeCount} active meetings`,
          ja: `${meetingSummary.activeCount}件の進行中会議に残る後続イシュー`,
          zh: `${meetingSummary.activeCount} 个进行中会议留下的后续 issue`,
        }),
        accent: "#22c55e",
        tone: "success",
        onAction: () => setActiveTab("meetings"),
      },
    ],
    [
      dashboardStats.kanban.blocked,
      dashboardStats.kanban.review_queue,
      dashboardStats.kanban.stale_in_progress,
      dashboardStats.kanban.waiting_acceptance,
      meetingSummary.activeCount,
      meetingSummary.unresolvedCount,
      onOpenKanbanSignal,
      t,
    ],
  );
  const homeActivityItems = useMemo<HomeActivityItem[]>(() => {
    const meetingItems = recentMeetings.map((meeting) => ({
      id: `meeting-${meeting.id}`,
      title: meeting.agenda,
      detail:
        meeting.primary_provider || meeting.reviewer_provider
          ? formatProviderFlow(meeting.primary_provider, meeting.reviewer_provider)
          : t({ ko: "라운드테이블", en: "Round Table", ja: "ラウンドテーブル", zh: "圆桌" }),
      timestamp: meeting.started_at || meeting.created_at,
      tone: meeting.status === "completed" ? ("success" as const) : ("warn" as const),
    }));
    const sessionItems = [...staleLinkedSessions, ...reconnectingSessions].slice(0, 2).map((session) => ({
      id: `session-${session.id}`,
      title: session.name || session.session_key,
      detail:
        session.status === "disconnected"
          ? t({ ko: "재연결 필요", en: "Needs reconnect", ja: "再接続が必要", zh: "需要重连" })
          : t({ ko: "working 세션 stale", en: "Working session stale", ja: "working セッション stale", zh: "working 会话 stale" }),
      timestamp: session.last_seen_at || session.connected_at,
      tone: "warn" as const,
    }));

    return [...meetingItems, ...sessionItems]
      .sort((left, right) => right.timestamp - left.timestamp)
      .slice(0, 5);
  }, [recentMeetings, reconnectingSessions, staleLinkedSessions, t]);

  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(HOME_WIDGET_STORAGE_KEY, JSON.stringify(widgetOrder));
  }, [widgetOrder]);

  const handleWidgetDragStart = useCallback(
    (event: DragStartEvent) => {
      if (!editingWidgets) return;
      setActiveWidgetId(event.active.id as HomeWidgetId);
      setOverWidgetId(null);
    },
    [editingWidgets],
  );

  const handleWidgetDragOver = useCallback(
    (event: DragOverEvent) => {
      if (!editingWidgets) return;
      setOverWidgetId(event.over ? (event.over.id as HomeWidgetId) : null);
    },
    [editingWidgets],
  );

  const handleWidgetDragEnd = useCallback(
    (event: DragEndEvent) => {
      const activeId = event.active.id as HomeWidgetId;
      const overId = event.over ? (event.over.id as HomeWidgetId) : null;
      setActiveWidgetId(null);
      setOverWidgetId(null);
      if (!editingWidgets || !overId || activeId === overId) return;

      setWidgetOrder((current) => {
        const fromIndex = current.indexOf(activeId);
        const toIndex = current.indexOf(overId);
        if (fromIndex === -1 || toIndex === -1) return current;
        return arrayMove(current, fromIndex, toIndex);
      });
    },
    [editingWidgets],
  );

  const handleWidgetDragCancel = useCallback(() => {
    setActiveWidgetId(null);
    setOverWidgetId(null);
  }, []);

  const homeWidgetSpecs: Record<HomeWidgetId, { className: string; render: () => ReactNode }> = {
    metric_agents: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" })}
          value={numberFormatter.format(dashboardStats.agents.working)}
          badge={t({ ko: `${numberFormatter.format(dashboardStats.agents.idle)} 대기`, en: `${numberFormatter.format(dashboardStats.agents.idle)} idle`, ja: `${numberFormatter.format(dashboardStats.agents.idle)} 待機`, zh: `${numberFormatter.format(dashboardStats.agents.idle)} 空闲` })}
          sub={t({ ko: `${numberFormatter.format(dashboardStats.agents.total)}명 등록`, en: `${numberFormatter.format(dashboardStats.agents.total)} registered`, ja: `${numberFormatter.format(dashboardStats.agents.total)}人登録`, zh: `已注册 ${numberFormatter.format(dashboardStats.agents.total)} 名` })}
          accent="#60a5fa"
          spark={[dashboardStats.agents.working, dashboardStats.agents.idle, dashboardStats.agents.break, dashboardStats.agents.offline]}
        />
      ),
    },
    metric_dispatch: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "파견 세션", en: "Dispatched", ja: "派遣セッション", zh: "派遣会话" })}
          value={numberFormatter.format(dashboardStats.dispatched_count)}
          badge={t({ ko: `${reconnectingSessions.length} reconnect`, en: `${reconnectingSessions.length} reconnect`, ja: `${reconnectingSessions.length} reconnect`, zh: `${reconnectingSessions.length} reconnect` })}
          sub={t({ ko: `${numberFormatter.format(activeSessions.length)}개 활성 연결`, en: `${numberFormatter.format(activeSessions.length)} live sessions`, ja: `${numberFormatter.format(activeSessions.length)}件 アクティブ`, zh: `${numberFormatter.format(activeSessions.length)} 个活跃连接` })}
          accent="#34d399"
          spark={[
            activeSessions.filter((session) => session.status === "working").length,
            activeSessions.filter((session) => session.status === "idle").length,
            reconnectingSessions.length,
          ]}
        />
      ),
    },
    metric_review: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "리뷰 큐", en: "Review Queue", ja: "レビューキュー", zh: "审查队列" })}
          value={numberFormatter.format(dashboardStats.kanban.review_queue)}
          badge={t({ ko: `${dashboardStats.kanban.blocked} blocked`, en: `${dashboardStats.kanban.blocked} blocked`, ja: `${dashboardStats.kanban.blocked} blocked`, zh: `${dashboardStats.kanban.blocked} blocked` })}
          sub={t({ ko: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}`, en: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}`, ja: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}`, zh: `requested ${dashboardStats.kanban.waiting_acceptance} · stale ${dashboardStats.kanban.stale_in_progress}` })}
          accent="#f59e0b"
          spark={[
            dashboardStats.kanban.review_queue,
            dashboardStats.kanban.blocked,
            dashboardStats.kanban.waiting_acceptance,
            dashboardStats.kanban.stale_in_progress,
          ]}
        />
      ),
    },
    metric_followups: {
      className: "col-span-12 sm:col-span-6 xl:col-span-3",
      render: () => (
        <DashboardHomeMetricTile
          title={t({ ko: "회의 후속", en: "Follow-ups", ja: "会議フォローアップ", zh: "会议后续" })}
          value={numberFormatter.format(meetingSummary.unresolvedCount)}
          badge={t({ ko: `${meetingSummary.activeCount} active`, en: `${meetingSummary.activeCount} active`, ja: `${meetingSummary.activeCount} active`, zh: `${meetingSummary.activeCount} active` })}
          sub={t({ ko: `회의 ${meetings.length}건 · GitHub 종료 ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)}`, en: `${meetings.length} meetings · ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)} GitHub closed`, ja: `会議 ${meetings.length}件 · GitHub 完了 ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)}`, zh: `会议 ${meetings.length} 个 · GitHub 已关闭 ${numberFormatter.format(dashboardStats.github_closed_today ?? 0)}` })}
          accent="#a855f7"
          spark={[meetingSummary.activeCount, meetingSummary.unresolvedCount, dashboardStats.github_closed_today ?? 0, meetings.length]}
        />
      ),
    },
    office: {
      className: "col-span-12 xl:col-span-8",
      render: () => (
        <DashboardHomeOfficeWidget
          rows={homeAgents.slice(0, 8)}
          stats={dashboardStats}
          language={language}
          t={t}
          onSelectAgent={onSelectAgent}
        />
      ),
    },
    signals: {
      className: "col-span-12 xl:col-span-4",
      render: () => (
        <DashboardHomeSignalsWidget
          rows={focusSignals}
          maxValue={Math.max(1, ...focusSignals.map((item) => item.value))}
          t={t}
        />
      ),
    },
    quality: {
      className: "col-span-12 xl:col-span-6",
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
      className: "col-span-12 xl:col-span-7",
      render: () => (
        <DashboardHomeRosterWidget
          rows={homeAgents.slice(0, 5)}
          t={t}
          numberFormatter={numberFormatter}
          onSelectAgent={onSelectAgent}
          onOpenAchievements={() => setActiveTab("achievements")}
        />
      ),
    },
    activity: {
      className: "col-span-12 xl:col-span-5",
      render: () => (
        <DashboardHomeActivityWidget
          items={homeActivityItems}
          localeTag={localeTag}
          t={t}
          onOpenMeetings={() => setActiveTab("meetings")}
        />
      ),
    },
  };

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
      <div className="flex flex-col gap-4">
        <div className="flex flex-col gap-4 lg:flex-row lg:items-start lg:justify-between">
          <div className="min-w-0">
            <div
              className="mb-2 flex flex-wrap items-center gap-2 text-[11px] uppercase tracking-[0.16em]"
              style={{ color: "var(--th-text-muted)" }}
            >
              <span style={{ fontFamily: "var(--font-mono)" }}>{dateLabel}</span>
              <span aria-hidden="true" className="inline-flex h-1 w-1 rounded-full" style={{ background: "var(--th-text-muted)" }} />
              <span className="inline-flex items-center gap-2" style={{ color: systemState.color }}>
                <span
                  className="inline-flex h-2 w-2 rounded-full"
                  style={{
                    background: systemState.pulseColor,
                    boxShadow: `0 0 0 4px color-mix(in srgb, ${systemState.pulseColor} 16%, transparent)`,
                  }}
                />
                <span style={{ fontFamily: "var(--font-mono)" }}>{systemState.label}</span>
              </span>
            </div>
            <h1 className="text-[1.9rem] font-black tracking-tight sm:text-[2rem]" style={{ color: "var(--th-text-heading)" }}>
              {t({
                ko: "오늘의 AgentDesk",
                en: "AgentDesk Today",
                ja: "今日の AgentDesk",
                zh: "今日 AgentDesk",
              })}
            </h1>
            <p className="mt-2 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {t({
                ko: `에이전트 ${numberFormatter.format(dashboardStats.agents.total)}명 · 세션 ${numberFormatter.format(activeSessions.length)} 활성 · 프로바이더 ${numberFormatter.format(activeProviderCount)} 연결`,
                en: `${numberFormatter.format(dashboardStats.agents.total)} agents · ${numberFormatter.format(activeSessions.length)} live sessions · ${numberFormatter.format(activeProviderCount)} providers connected`,
                ja: `エージェント ${numberFormatter.format(dashboardStats.agents.total)}名 · セッション ${numberFormatter.format(activeSessions.length)}件 稼働 · プロバイダー ${numberFormatter.format(activeProviderCount)} 接続`,
                zh: `代理 ${numberFormatter.format(dashboardStats.agents.total)} 名 · 会话 ${numberFormatter.format(activeSessions.length)} 个活跃 · ${numberFormatter.format(activeProviderCount)} 个提供商已连接`,
              })}
            </p>
          </div>
          <div className="flex items-center gap-2 self-start">
            {editingWidgets ? (
              <SurfaceActionButton
                tone="neutral"
                onClick={() => setWidgetOrder(DEFAULT_HOME_WIDGET_ORDER)}
              >
                {t({ ko: "기본값", en: "Reset", ja: "初期化", zh: "重置" })}
              </SurfaceActionButton>
            ) : null}
            <SurfaceActionButton
              tone={editingWidgets ? "accent" : "neutral"}
              onClick={() => setEditingWidgets((value) => !value)}
            >
              {editingWidgets
                ? t({ ko: "완료", en: "Done", ja: "完了", zh: "完成" })
                : t({ ko: "편집", en: "Edit", ja: "編集", zh: "编辑" })}
            </SurfaceActionButton>
          </div>
        </div>

        {editingWidgets ? (
          <div
            className="rounded-[18px] border px-4 py-3 text-sm"
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
              background: "color-mix(in srgb, var(--th-accent-primary-soft) 78%, var(--th-card-bg) 22%)",
              color: "var(--th-text-muted)",
            }}
          >
            {t({
              ko: "위젯을 드래그해서 순서를 바꿀 수 있습니다. 완료를 누르면 로컬에 저장됩니다.",
              en: "Drag widgets to reorder them. The layout is saved locally when you finish.",
              ja: "ウィジェットをドラッグして順序を変更できます。完了するとローカルに保存されます。",
              zh: "可拖拽调整组件顺序，完成后会保存到本地。",
            })}
          </div>
        ) : null}
      </div>

      <DndContext
        sensors={widgetDragSensors}
        collisionDetection={closestCenter}
        onDragStart={handleWidgetDragStart}
        onDragOver={handleWidgetDragOver}
        onDragEnd={handleWidgetDragEnd}
        onDragCancel={handleWidgetDragCancel}
      >
        <SortableContext items={widgetOrder} strategy={rectSortingStrategy}>
          <div className="grid grid-cols-12 gap-4">
            {widgetOrder.map((widgetId) => {
              const spec = homeWidgetSpecs[widgetId];
              return (
                <DashboardSortableWidget
                  key={widgetId}
                  widgetId={widgetId}
                  className={spec.className}
                  editing={editingWidgets}
                  activeWidgetId={activeWidgetId}
                  overWidgetId={overWidgetId}
                  handleLabel={t({
                    ko: "위젯 순서 변경",
                    en: "Reorder widget",
                    ja: "ウィジェットの順序を変更",
                    zh: "调整组件顺序",
                  })}
                >
                  {spec.render()}
                </DashboardSortableWidget>
              );
            })}
          </div>
        </SortableContext>
      </DndContext>

      <DashboardHomeSectionNavigatorWidget
        tabDefinitions={tabDefinitions}
        activeTab={activeTab}
        t={t}
        topRepos={dashboardStats.kanban.top_repos}
        openTotal={dashboardStats.kanban.open_total}
        onClickTab={setActiveTab}
        onKeyDown={handleTabKeyDown}
        buttonRefs={tabButtonRefs}
      />

      <DashboardTabPanel tab="operations" activeTab={activeTab} t={t}>
          <div className="grid gap-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(0,0.9fr)]">
            <SurfaceSubsection
              title={t({ ko: "운영 시그널", en: "Ops Signals", ja: "運用シグナル", zh: "运营信号" })}
              description={t({
                ko: "세션 이상, 칸반 병목, 회의 후속 정리를 현재 탭에서 바로 점검합니다.",
                en: "Inspect session anomalies, kanban bottlenecks, and meeting follow-ups from this tab.",
                ja: "セッション異常、カンバンの詰まり、会議後続整理をこのタブで直接確認します。",
                zh: "在当前标签页直接检查会话异常、看板瓶颈和会议后续整理。",
              })}
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
                background:
                  "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
              }}
            >
              <div className="mt-4 grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
                <PulseSignalCard
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
                <PulseSignalCard
                  label={t({ ko: "리뷰 대기", en: "Review Queue", ja: "レビュー待ち", zh: "待审查" })}
                  value={dashboardStats.kanban.review_queue}
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
                <PulseSignalCard
                  label={t({ ko: "블록됨", en: "Blocked", ja: "ブロック", zh: "阻塞" })}
                  value={dashboardStats.kanban.blocked}
                  accent="#ef4444"
                  sublabel={t({
                    ko: "수동 판단이나 해소를 기다리는 카드",
                    en: "Cards waiting on unblock or manual intervention",
                    ja: "解除や手動判断待ちのカード",
                    zh: "等待解除阻塞或人工判断的卡片",
                  })}
                  actionLabel={t({ ko: "막힘 카드 보기", en: "Open Blocked", ja: "Blocked を開く", zh: "打开阻塞卡片" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("blocked") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "수락 지연", en: "Waiting Acceptance", ja: "受諾遅延", zh: "接收延迟" })}
                  value={dashboardStats.kanban.waiting_acceptance}
                  accent="#10b981"
                  sublabel={t({
                    ko: "requested 상태에 머문 카드",
                    en: "Cards stalled in requested",
                    ja: "requested に留まるカード",
                    zh: "停留在 requested 的卡片",
                  })}
                  actionLabel={t({ ko: "requested 보기", en: "Open Requested", ja: "requested を開く", zh: "打开 requested" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("requested") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "진행 정체", en: "Stale In Progress", ja: "進行停滞", zh: "进行停滞" })}
                  value={dashboardStats.kanban.stale_in_progress}
                  accent="#f59e0b"
                  sublabel={t({
                    ko: "오래 머무는 in_progress 카드",
                    en: "Cards stuck in progress",
                    ja: "進行が長引く in_progress カード",
                    zh: "长时间停留在 in_progress 的卡片",
                  })}
                  actionLabel={t({ ko: "정체 카드 보기", en: "Open Stale", ja: "停滞カードを開く", zh: "打开停滞卡片" })}
                  onAction={onOpenKanbanSignal ? () => onOpenKanbanSignal("stalled") : undefined}
                />
                <PulseSignalCard
                  label={t({ ko: "회의 후속", en: "Meeting Follow-up", ja: "会議フォローアップ", zh: "会议后续" })}
                  value={meetingSummary.unresolvedCount}
                  accent="#22c55e"
                  sublabel={t({
                    ko: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                    en: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                    ja: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                    zh: `${meetingSummary.activeCount} active / ${meetings.length} total`,
                  })}
                  actionLabel={t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
                  onAction={() => setActiveTab("meetings")}
                />
              </div>
            </SurfaceSubsection>

            <MeetingTimelineCard
              meetings={recentMeetings}
              activeCount={meetingSummary.activeCount}
              followUpCount={meetingSummary.unresolvedCount}
              localeTag={localeTag}
              t={t}
              onOpenMeetings={() => setActiveTab("meetings")}
            />
          </div>
          <div className="grid gap-4 xl:grid-cols-[minmax(0,1.1fr)_minmax(0,0.9fr)]">
            <HealthWidget t={t} localeTag={localeTag} />
            <RateLimitWidget t={t} onOpenSettings={onOpenSettings} />
          </div>
          <AgentQualityWidget agents={agents} t={t} localeTag={localeTag} />
          <DashboardDeptAndSquad
            deptRows={deptPerformanceRows}
            workingAgents={workingAgents}
            idleAgentsList={idleAgentsList}
            agents={agents}
            language={language}
            numberFormatter={numberFormatter}
            t={t}
            onSelectAgent={onSelectAgent}
          />
          <GitHubIssuesWidget t={t} repo={topGithubRepo} />
          <BottleneckWidget t={t} />
      </DashboardTabPanel>

      <DashboardTabPanel tab="tokens" activeTab={activeTab} t={t}>
          <ReceiptWidget t={t} />
          <HeatmapWidget t={t} />
          <TokenAnalyticsSection
            agents={agents}
            t={t}
            numberFormatter={numberFormatter}
          />
      </DashboardTabPanel>

      <DashboardTabPanel tab="automation" activeTab={activeTab} t={t}>
          <PulseSectionShell
            eyebrow={t({ ko: "Automation", en: "Automation", ja: "Automation", zh: "Automation" })}
            title={t({ ko: "자동화 / 스킬", en: "Automation / Skills", ja: "自動化 / スキル", zh: "自动化 / 技能" })}
            subtitle=""
            badge=""
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-warn) 20%, var(--th-border) 80%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-warn) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
            <CronTimelineWidget t={t} localeTag={localeTag} />

            <AutoQueueHistoryWidget t={t} />

            <SkillRankingSection
              skillRanking={skillRanking}
              skillWindow={skillWindow}
              onChangeWindow={setSkillWindow}
              numberFormatter={numberFormatter}
              localeTag={localeTag}
              lastUpdatedAt={skillRankingUpdatedAt}
              refreshFailed={skillRankingRefreshFailed}
              t={t}
            />

            <SkillTrendWidget t={t} />

            <Suspense
              fallback={(
                <div className="py-8 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "스킬 카탈로그를 불러오는 중입니다", en: "Loading skill catalog", ja: "スキルカタログを読み込み中", zh: "正在加载技能目录" })}
                </div>
              )}
            >
              <SkillCatalogView embedded />
            </Suspense>
          </PulseSectionShell>
      </DashboardTabPanel>

      <DashboardTabPanel tab="achievements" activeTab={activeTab} t={t}>
          <PulseSectionShell
            eyebrow={t({ ko: "Achievement", en: "Achievement", ja: "Achievement", zh: "Achievement" })}
            title={t({ ko: "업적 / XP", en: "Achievements / XP", ja: "実績 / XP", zh: "成就 / XP" })}
            subtitle=""
            badge=""
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-primary) 18%, var(--th-border) 82%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-primary) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
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

            <div className="grid gap-4 lg:grid-cols-2">
              <AchievementWidget t={t} agents={agents} />
              <SurfaceSubsection
                title={t({ ko: "XP 스냅샷", en: "XP Snapshot", ja: "XP スナップショット", zh: "XP 快照" })}
                description={t({
                  ko: "최상위 랭커의 XP 규모를 간단히 확인합니다.",
                  en: "Quick read on the scale of top-ranked XP.",
                  ja: "上位ランカーの XP 規模を簡単に確認します。",
                  zh: "快速查看头部 XP 规模。",
                })}
                style={{
                  borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
                }}
              >
                {topAgents.length === 0 ? (
                  <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
                    {t({ ko: "아직 XP 집계 대상이 없습니다.", en: "No XP snapshot is available yet.", ja: "まだ XP スナップショット対象がありません。", zh: "尚无 XP 快照数据。" })}
                  </SurfaceEmptyState>
                ) : (
                  <div className="mt-4 grid gap-3 sm:grid-cols-3">
                    {topAgents.slice(0, 3).map((agent, index) => (
                      <div
                        key={agent.id}
                        className="rounded-2xl border px-4 py-3"
                        style={{
                          borderColor: "rgba(148,163,184,0.16)",
                          background: "color-mix(in srgb, var(--th-card-bg) 92%, transparent)",
                        }}
                      >
                        <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
                          {t({ ko: `${index + 1}위`, en: `Rank ${index + 1}`, ja: `${index + 1}位`, zh: `第 ${index + 1} 名` })}
                        </div>
                        <div className="mt-2 truncate text-sm font-medium" style={{ color: "var(--th-text-heading)" }}>
                          {agent.name}
                        </div>
                        <div className="mt-1 text-lg font-black tracking-tight" style={{ color: "var(--th-accent-primary)" }}>
                          {numberFormatter.format(agent.xp)} XP
                        </div>
                        <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                          {t({ ko: `${numberFormatter.format(agent.tasksDone)}개 완료`, en: `${numberFormatter.format(agent.tasksDone)} completed`, ja: `${numberFormatter.format(agent.tasksDone)} 完了`, zh: `完成 ${numberFormatter.format(agent.tasksDone)} 项` })}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </SurfaceSubsection>
            </div>
          </PulseSectionShell>
      </DashboardTabPanel>

      <DashboardTabPanel tab="meetings" activeTab={activeTab} t={t}>
          <PulseSectionShell
            eyebrow={t({ ko: "Meetings", en: "Meetings", ja: "Meetings", zh: "Meetings" })}
            title={t({ ko: "회의 기록 / 후속 일감", en: "Meeting Records / Follow-ups", ja: "会議記録 / フォローアップ", zh: "会议记录 / 后续事项" })}
            subtitle=""
            badge=""
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-success) 18%, var(--th-border) 82%)",
              background:
                "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 96%, var(--th-accent-success) 4%) 0%, color-mix(in srgb, var(--th-bg-surface) 98%, transparent) 100%)",
            }}
          >
            <Suspense
              fallback={(
                <div className="py-8 text-center text-sm" style={{ color: "var(--th-text-muted)" }}>
                  {t({ ko: "회의 기록을 불러오는 중입니다", en: "Loading meeting records", ja: "会議記録を読み込み中", zh: "正在加载会议记录" })}
                </div>
              )}
            >
              <MeetingMinutesView meetings={meetings} onRefresh={() => onRefreshMeetings?.()} embedded />
            </Suspense>
          </PulseSectionShell>
      </DashboardTabPanel>
    </div>
  );
}
