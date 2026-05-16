import {
  Component,
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type DragEvent as ReactDragEvent,
  type ErrorInfo,
  type KeyboardEvent as ReactKeyboardEvent,
  type ReactNode,
} from "react";
import { GripVertical } from "lucide-react";
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
  SurfaceCard,
  SurfaceEmptyState,
  SurfaceListItem,
  SurfaceMetaBadge,
  SurfaceSection,
  SurfaceSegmentButton,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";
import TooltipLabel from "./common/TooltipLabel";
import AgentAvatar from "./AgentAvatar";
import {
  DashboardRankingBoard,
  type RankedAgent,
} from "./dashboard/HeroSections";
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
import { timeAgo, type TFunction } from "./dashboard/model";
import { formatProviderFlow } from "./MeetingProviderFlow";

const SkillCatalogView = lazy(() => import("./SkillCatalogView"));
const MeetingMinutesView = lazy(() => import("./MeetingMinutesView"));

type PulseKanbanSignal = "review" | "blocked" | "requested" | "stalled";
type HomeWidgetId =
  | "metric_agents"
  | "metric_dispatch"
  | "metric_review"
  | "metric_followups"
  | "office"
  | "signals"
  | "quality"
  | "roster"
  | "activity";
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

function dashboardTabButtonId(tab: DashboardTab): string {
  return `dashboard-tab-${tab}`;
}

function dashboardTabPanelId(tab: DashboardTab): string {
  return `dashboard-panel-${tab}`;
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

const HOME_WIDGET_STORAGE_KEY = "agentdesk.dashboard.home.widgets.v1";
const DEFAULT_HOME_WIDGET_ORDER: HomeWidgetId[] = [
  "metric_agents",
  "metric_dispatch",
  "metric_review",
  "metric_followups",
  "office",
  "signals",
  "quality",
  "roster",
  "activity",
];

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

function normalizeHomeWidgetOrder(value: unknown): HomeWidgetId[] {
  if (!Array.isArray(value)) return DEFAULT_HOME_WIDGET_ORDER;
  const valid = new Set<HomeWidgetId>(DEFAULT_HOME_WIDGET_ORDER);
  const next: HomeWidgetId[] = [];
  for (const item of value) {
    if (typeof item !== "string" || !valid.has(item as HomeWidgetId) || next.includes(item as HomeWidgetId)) {
      continue;
    }
    next.push(item as HomeWidgetId);
  }
  for (const item of DEFAULT_HOME_WIDGET_ORDER) {
    if (!next.includes(item)) next.push(item);
  }
  return next;
}

function readStoredHomeWidgetOrder(): HomeWidgetId[] {
  if (typeof window === "undefined") return DEFAULT_HOME_WIDGET_ORDER;
  try {
    const raw = window.localStorage.getItem(HOME_WIDGET_STORAGE_KEY);
    return raw ? normalizeHomeWidgetOrder(JSON.parse(raw)) : DEFAULT_HOME_WIDGET_ORDER;
  } catch {
    return DEFAULT_HOME_WIDGET_ORDER;
  }
}

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

function moveHomeWidget(items: HomeWidgetId[], fromIndex: number, toIndex: number): HomeWidgetId[] {
  if (fromIndex === toIndex) return items;
  const next = [...items];
  const [moved] = next.splice(fromIndex, 1);
  const targetIndex = fromIndex < toIndex ? toIndex - 1 : toIndex;
  next.splice(targetIndex, 0, moved);
  return next;
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
  const [widgetOrder, setWidgetOrder] = useState<HomeWidgetId[]>(() => readStoredHomeWidgetOrder());
  const [dragIndex, setDragIndex] = useState<number | null>(null);
  const [overIndex, setOverIndex] = useState<number | null>(null);
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
    (index: number) => (event: ReactDragEvent<HTMLDivElement>) => {
      if (!editingWidgets) return;
      setDragIndex(index);
      event.dataTransfer.effectAllowed = "move";
      try {
        event.dataTransfer.setData("text/plain", String(index));
      } catch {
        // ignore browser-specific dnd errors
      }
    },
    [editingWidgets],
  );

  const handleWidgetDragOver = useCallback(
    (index: number) => (event: ReactDragEvent<HTMLDivElement>) => {
      if (!editingWidgets) return;
      event.preventDefault();
      if (overIndex !== index) setOverIndex(index);
    },
    [editingWidgets, overIndex],
  );

  const handleWidgetDrop = useCallback(
    (index: number) => (event: ReactDragEvent<HTMLDivElement>) => {
      if (!editingWidgets) return;
      event.preventDefault();
      const transferredIndex = Number(event.dataTransfer.getData("text/plain"));
      const fromIndex =
        dragIndex ?? (Number.isInteger(transferredIndex) ? transferredIndex : null);
      if (fromIndex == null) return;
      setWidgetOrder((current) => moveHomeWidget(current, fromIndex, index));
      setDragIndex(null);
      setOverIndex(null);
    },
    [dragIndex, editingWidgets],
  );

  const handleWidgetDragEnd = useCallback(() => {
    setDragIndex(null);
    setOverIndex(null);
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

      <div className="grid grid-cols-12 gap-4">
        {widgetOrder.map((widgetId, index) => {
          const spec = homeWidgetSpecs[widgetId];
          return (
            <div
              key={widgetId}
              draggable={editingWidgets}
              onDragStart={editingWidgets ? handleWidgetDragStart(index) : undefined}
              onDragOver={editingWidgets ? handleWidgetDragOver(index) : undefined}
              onDrop={editingWidgets ? handleWidgetDrop(index) : undefined}
              onDragEnd={handleWidgetDragEnd}
              className={spec.className}
              style={{
                opacity: dragIndex === index ? 0.55 : 1,
                transform: overIndex === index && dragIndex !== index ? "translateY(-2px)" : undefined,
                transition: "opacity 160ms ease, transform 160ms ease",
              }}
            >
              <div className="relative h-full">
                {editingWidgets ? (
                  <div
                    className="pointer-events-none absolute right-3 top-3 z-10 inline-flex items-center gap-1.5 rounded-lg border px-2 py-1 text-[11px]"
                    style={{
                      borderColor: "rgba(148,163,184,0.18)",
                      background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                      color: "var(--th-text-muted)",
                    }}
                  >
                    <GripVertical size={12} />
                    drag
                  </div>
                ) : null}
                {spec.render()}
              </div>
            </div>
          );
        })}
      </div>

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

function DashboardTabPanel({
  tab,
  activeTab,
  t,
  children,
}: {
  tab: DashboardTab;
  activeTab: DashboardTab;
  t: TFunction;
  children: ReactNode;
}) {
  if (activeTab !== tab) return null;

  return (
    <DashboardTabErrorBoundary tab={tab} t={t}>
      <div
        role="tabpanel"
        id={dashboardTabPanelId(tab)}
        aria-labelledby={dashboardTabButtonId(tab)}
        tabIndex={0}
        className="space-y-5"
      >
        {children}
      </div>
    </DashboardTabErrorBoundary>
  );
}

class DashboardTabErrorBoundary extends Component<
  { tab: DashboardTab; t: TFunction; children: ReactNode },
  { hasError: boolean }
> {
  state = { hasError: false };

  static getDerivedStateFromError(): { hasError: boolean } {
    return { hasError: true };
  }

  componentDidCatch(error: Error, errorInfo: ErrorInfo) {
    console.error(`Dashboard tab "${this.props.tab}" crashed`, error, errorInfo);
  }

  render() {
    if (!this.state.hasError) {
      return this.props.children;
    }

    return (
      <SurfaceEmptyState className="rounded-3xl border px-4 py-8 text-center text-sm">
        <div className="space-y-3">
          <div className="text-3xl opacity-40">⚠️</div>
          <div style={{ color: "var(--th-text-heading)" }}>
            {this.props.t({
              ko: "이 탭을 렌더링하는 중 오류가 발생했습니다.",
              en: "This tab failed while rendering.",
              ja: "このタブの描画中にエラーが発生しました。",
              zh: "该标签页渲染时发生错误。",
            })}
          </div>
          <div style={{ color: "var(--th-text-muted)" }}>
            {this.props.t({
              ko: "다른 탭으로 이동한 뒤 다시 돌아오거나 새로고침해 주세요.",
              en: "Switch away and come back, or refresh the page.",
              ja: "別のタブに移動して戻るか、ページを更新してください。",
              zh: "请切换到其他标签页后再返回，或刷新页面。",
            })}
          </div>
          <div className="flex justify-center">
            <SurfaceActionButton
              tone="neutral"
              onClick={() => this.setState({ hasError: false })}
            >
              {this.props.t({
                ko: "다시 시도",
                en: "Try Again",
                ja: "再試行",
                zh: "重试",
              })}
            </SurfaceActionButton>
          </div>
        </div>
      </SurfaceEmptyState>
    );
  }
}

function PulseSectionShell({
  eyebrow,
  title,
  subtitle,
  badge,
  style,
  children,
}: {
  eyebrow: string;
  title: string;
  subtitle: string;
  badge: string;
  style?: CSSProperties;
  children: ReactNode;
}) {
  return (
    <SurfaceSection
      eyebrow={eyebrow}
      title={title}
      description={subtitle}
      badge={badge}
      className="rounded-[28px] p-4 sm:p-5"
      style={style ?? {
        borderColor: "color-mix(in srgb, var(--th-border) 82%, transparent)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 97%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 99%, transparent) 100%)",
      }}
    >
      <div className="mt-4 space-y-4">{children}</div>
    </SurfaceSection>
  );
}

function PulseSignalCard({
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
    <SurfaceCard
      className="min-w-0 rounded-2xl p-4"
      style={{
        borderColor: `color-mix(in srgb, ${accent} 24%, var(--th-border) 76%)`,
        background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 93%, ${accent} 7%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)`,
      }}
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div className="min-w-0 flex-1">
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
        {onAction ? (
          <SurfaceActionButton
            onClick={onAction}
            className="w-full shrink-0 sm:w-auto"
            style={{
              color: accent,
              border: `1px solid color-mix(in srgb, ${accent} 28%, var(--th-border) 72%)`,
              background: `color-mix(in srgb, ${accent} 14%, var(--th-card-bg) 86%)`,
            }}
          >
            {actionLabel}
          </SurfaceActionButton>
        ) : null}
      </div>
    </SurfaceCard>
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

  const getMeetingStatusLabel = useCallback(
    (status: RoundTableMeeting["status"]) =>
      t({
        ko: status === "in_progress" ? "진행 중" : status === "completed" ? "완료" : "초안",
        en: status === "in_progress" ? "In Progress" : status === "completed" ? "Completed" : "Draft",
        ja: status === "in_progress" ? "進行中" : status === "completed" ? "完了" : "下書き",
        zh: status === "in_progress" ? "进行中" : status === "completed" ? "已完成" : "草稿",
      }),
    [t],
  );

  return (
    <SurfaceSubsection
      title={t({ ko: "회의 타임라인", en: "Meeting Timeline", ja: "会議タイムライン", zh: "会议时间线" })}
      description={t({
        ko: `${activeCount}개 진행 중, 후속 이슈 ${followUpCount}개 미정리`,
        en: `${activeCount} active, ${followUpCount} follow-up issues still open`,
        ja: `${activeCount}件進行中、後続イシュー ${followUpCount}件 未整理`,
        zh: `${activeCount} 个进行中，${followUpCount} 个后续 issue 未整理`,
      })}
      actions={onOpenMeetings ? (
        <SurfaceActionButton tone="success" onClick={onOpenMeetings}>
          {t({ ko: "회의록 열기", en: "Open Meetings", ja: "会議録を開く", zh: "打开会议记录" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="space-y-2">
        {meetings.length === 0 ? (
          <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
            {t({ ko: "최근 회의가 없습니다.", en: "No recent meetings yet.", ja: "最近の会議はありません。", zh: "暂无最近会议。" })}
          </SurfaceEmptyState>
        ) : (
          meetings.map((meeting) => {
            const statusTone = meeting.status === "in_progress" ? "success" : meeting.status === "completed" ? "info" : "neutral";
            const issueCount = countOpenMeetingIssues(meeting);
            return (
              <SurfaceListItem
                key={meeting.id}
                tone={statusTone}
                trailing={(
                  <div className="text-right">
                    <div className="text-xs font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {meeting.primary_provider || meeting.reviewer_provider
                        ? formatProviderFlow(meeting.primary_provider, meeting.reviewer_provider)
                        : "RT"}
                    </div>
                    <div className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {t({
                        ko: `${meeting.issues_created}개 생성`,
                        en: `${meeting.issues_created} created`,
                        ja: `${meeting.issues_created}件 作成`,
                        zh: `已创建 ${meeting.issues_created} 个`,
                      })}
                    </div>
                  </div>
                )}
              >
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <SurfaceMetaBadge tone={statusTone}>{getMeetingStatusLabel(meeting.status)}</SurfaceMetaBadge>
                    <span className="text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      {formatter.format(meeting.started_at || meeting.created_at)}
                    </span>
                  </div>
                  <div className="mt-1 truncate font-medium" style={{ color: "var(--th-text)" }}>
                    {meeting.agenda}
                  </div>
                  <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                    <SurfaceMetaBadge>
                      {meeting.participant_names.length} {t({ ko: "참여자", en: "participants", ja: "参加者", zh: "参与者" })}
                    </SurfaceMetaBadge>
                    <SurfaceMetaBadge>
                      {meeting.total_rounds} {t({ ko: "라운드", en: "rounds", ja: "ラウンド", zh: "轮" })}
                    </SurfaceMetaBadge>
                    {issueCount > 0 ? (
                      <SurfaceMetaBadge tone="warn">
                        {issueCount} {t({ ko: "후속 대기", en: "follow-up pending", ja: "後続待ち", zh: "后续待处理" })}
                      </SurfaceMetaBadge>
                    ) : null}
                  </div>
                </div>
              </SurfaceListItem>
            );
          })
        )}
      </div>
    </SurfaceSubsection>
  );
}

function SkillRankingSection({
  skillRanking,
  skillWindow,
  onChangeWindow,
  numberFormatter,
  localeTag,
  lastUpdatedAt,
  refreshFailed,
  t,
}: {
  skillRanking: SkillRankingResponse | null;
  skillWindow: "7d" | "30d" | "all";
  onChangeWindow: (value: "7d" | "30d" | "all") => void;
  numberFormatter: Intl.NumberFormat;
  localeTag: string;
  lastUpdatedAt: number | null;
  refreshFailed: boolean;
  t: TFunction;
}) {
  const updatedLabel = lastUpdatedAt
    ? new Intl.DateTimeFormat(localeTag, {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
      }).format(lastUpdatedAt)
    : null;

  return (
    <SurfaceSubsection
      title={t({ ko: "스킬 랭킹", en: "Skill Ranking", ja: "スキルランキング", zh: "技能排行" })}
      description={t({
        ko: "호출량 기준 상위 스킬과 에이전트를 같은 문법으로 정리합니다.",
        en: "Top skills and agents by call volume in the same grammar.",
        ja: "呼び出し量ベースの上位スキルとエージェントを同じ文法で整理します。",
        zh: "用统一语法整理按调用量统计的技能与代理排行。",
      })}
      actions={(
        <>
          {updatedLabel ? (
            <SurfaceMetaBadge tone={refreshFailed ? "warn" : "neutral"}>
              {refreshFailed
                ? t({
                    ko: `새로고침 실패 · 마지막 ${updatedLabel}`,
                    en: `Refresh failed · last ${updatedLabel}`,
                    ja: `更新失敗 · 最終 ${updatedLabel}`,
                    zh: `刷新失败 · 最后 ${updatedLabel}`,
                  })
                : t({
                    ko: `마지막 갱신 ${updatedLabel}`,
                    en: `Last updated ${updatedLabel}`,
                    ja: `最終更新 ${updatedLabel}`,
                    zh: `最后更新 ${updatedLabel}`,
                  })}
            </SurfaceMetaBadge>
          ) : null}
          {(["7d", "30d", "all"] as const).map((windowId) => (
            <SurfaceSegmentButton
              key={windowId}
              onClick={() => onChangeWindow(windowId)}
              active={skillWindow === windowId}
              tone="warn"
            >
              {windowId}
            </SurfaceSegmentButton>
          ))}
        </>
      )}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 24%, var(--th-border) 76%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-warn) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {!skillRanking || skillRanking.overall.length === 0 ? (
        <SurfaceEmptyState className="mt-4 px-4 py-6 text-center text-sm">
          {t({ ko: "아직 집계된 스킬 호출이 없습니다.", en: "No skill usage aggregated yet.", ja: "まだ集計されたスキル呼び出しがありません。", zh: "尚无技能调用统计。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="mt-4 grid gap-4 xl:grid-cols-2">
          <SkillRankingList
            title={t({ ko: "전체 TOP 5", en: "Overall TOP 5", ja: "全体 TOP 5", zh: "全体 TOP 5" })}
            emptyLabel={t({ ko: "표시할 스킬이 없습니다.", en: "No skills to show.", ja: "表示するスキルがありません。", zh: "没有可显示的技能。" })}
            t={t}
            items={skillRanking.overall.slice(0, 5).map((row, index) => ({
              id: `${row.skill_name}-${index}`,
              leading: `${index + 1}.`,
              title: row.skill_desc_ko,
              tooltip: row.skill_name,
              trailing: numberFormatter.format(row.calls),
            }))}
          />
          <SkillRankingList
            title={t({ ko: "에이전트별 TOP 5", en: "Top by Agent", ja: "エージェント別 TOP 5", zh: "按代理 TOP 5" })}
            emptyLabel={t({ ko: "표시할 에이전트 호출이 없습니다.", en: "No agent calls to show.", ja: "表示するエージェント呼び出しがありません。", zh: "没有可显示的代理调用。" })}
            t={t}
            items={skillRanking.byAgent.slice(0, 5).map((row, index) => ({
              id: `${row.agent_role_id}-${row.skill_name}-${index}`,
              leading: `${index + 1}.`,
              title: `${row.agent_name} · ${row.skill_desc_ko}`,
              tooltip: row.skill_name,
              trailing: numberFormatter.format(row.calls),
            }))}
          />
        </div>
      )}
    </SurfaceSubsection>
  );
}

function SkillRankingList({
  title,
  emptyLabel,
  items,
  t,
}: {
  title: string;
  emptyLabel: string;
  items: Array<{
    id: string;
    leading: string;
    title: string;
    tooltip: string;
    trailing: string;
  }>;
  t: TFunction;
}) {
  return (
    <div className="min-w-0">
      <div className="mb-2 text-sm font-medium" style={{ color: "var(--th-text-muted)" }}>
        {title}
      </div>
      {items.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {emptyLabel}
        </SurfaceEmptyState>
      ) : (
        <ul className="space-y-2">
          {items.map((item) => (
            <li key={item.id}>
              <SurfaceListItem
                tone="warn"
                trailing={(
                  <span className="text-sm font-semibold" style={{ color: "var(--th-accent-warn)" }}>
                    {item.trailing}
                  </span>
                )}
              >
                <div className="min-w-0 flex flex-1 items-start gap-2 text-sm" style={{ color: "var(--th-text)" }}>
                  <span className="inline-flex w-6 shrink-0" style={{ color: "var(--th-text-muted)" }}>
                    {item.leading}
                  </span>
                  <TooltipLabel text={item.title} tooltip={item.tooltip} className="flex-1" />
                </div>
              </SurfaceListItem>
            </li>
          ))}
        </ul>
      )}
      <div className="mt-2 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
        {t({ ko: "집계 창을 바꾸면 같은 카드 안에서 즉시 다시 계산됩니다.", en: "Changing the window recalculates in place.", ja: "ウィンドウを変えると同じカード内で再計算されます。", zh: "切换窗口后会在同一卡片内重新计算。" })}
      </div>
    </div>
  );
}

function DashboardTabButton({
  tab,
  active,
  label,
  detail,
  onClick,
  onKeyDown,
  buttonRef,
}: {
  tab: DashboardTab;
  active: boolean;
  label: string;
  detail: string;
  onClick: () => void;
  onKeyDown: (event: ReactKeyboardEvent<HTMLButtonElement>, tab: DashboardTab) => void;
  buttonRef: (node: HTMLButtonElement | null) => void;
}) {
  return (
    <button
      ref={buttonRef}
      type="button"
      id={dashboardTabButtonId(tab)}
      role="tab"
      aria-selected={active}
      aria-controls={dashboardTabPanelId(tab)}
      tabIndex={active ? 0 : -1}
      onClick={onClick}
      onKeyDown={(event) => onKeyDown(event, tab)}
      className="min-h-[5.25rem] w-full rounded-[22px] border px-4 py-3.5 text-left transition-all"
      style={{
        borderColor: active
          ? "color-mix(in srgb, var(--th-accent-primary) 32%, var(--th-border) 68%)"
          : "rgba(148,163,184,0.16)",
        background: active
          ? "color-mix(in srgb, var(--th-accent-primary-soft) 74%, transparent)"
          : "color-mix(in srgb, var(--th-card-bg) 94%, transparent)",
        boxShadow: active ? "0 14px 32px rgba(15, 23, 42, 0.12)" : "none",
      }}
    >
      <div className="text-sm font-semibold" style={{ color: active ? "var(--th-text-heading)" : "var(--th-text)" }}>
        {label}
      </div>
      <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
        {detail}
      </div>
    </button>
  );
}

function DashboardHomeMetricTile({
  title,
  value,
  badge,
  sub,
  accent,
  spark,
}: {
  title: string;
  value: string;
  badge: string;
  sub: string;
  accent: string;
  spark: number[];
}) {
  const maxValue = Math.max(1, ...spark);

  return (
    <SurfaceCard
      className="h-full rounded-[28px] p-4 sm:p-5"
      style={{
        borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
        background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, ${accent} 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 97%, transparent) 100%)`,
      }}
    >
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div
            className="text-[11px] font-semibold uppercase tracking-[0.16em]"
            style={{ color: "var(--th-text-muted)" }}
          >
            {title}
          </div>
          <div className="mt-3 text-[2rem] font-black leading-none tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {value}
          </div>
          <p className="mt-2 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {sub}
          </p>
        </div>
        <SurfaceMetaBadge
          tone="neutral"
          style={{
            borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
            background: `color-mix(in srgb, ${accent} 14%, var(--th-card-bg) 86%)`,
            color: accent,
          }}
        >
          {badge}
        </SurfaceMetaBadge>
      </div>

      <div className="mt-4 flex h-10 items-end gap-1">
        {spark.map((point, index) => (
          <div
            key={`${title}-${index}`}
            className="min-w-0 flex-1 rounded-full"
            style={{
              height: `${Math.max(18, (point / maxValue) * 100)}%`,
              background: `linear-gradient(180deg, color-mix(in srgb, ${accent} 78%, white 22%) 0%, ${accent} 100%)`,
              opacity: index === spark.length - 1 ? 1 : 0.72,
            }}
          />
        ))}
      </div>
    </SurfaceCard>
  );
}

function DashboardHomeOfficeWidget({
  rows,
  stats,
  language,
  t,
  onSelectAgent,
}: {
  rows: HomeAgentRow[];
  stats: DashboardStats;
  language: CompanySettings["language"];
  t: TFunction;
  onSelectAgent?: (agent: Agent) => void;
}) {
  void language;
  const visibleRows = rows.slice(0, 8);

  return (
    <SurfaceSubsection
      title={t({ ko: "오피스 뷰", en: "Office View", ja: "オフィスビュー", zh: "办公室视图" })}
      description={t({
        ko: "지금 일하는 에이전트와 세션 상태를 한 화면에 압축해 보여줍니다.",
        en: "A compressed office snapshot of active agents and live sessions.",
        ja: "作業中エージェントとセッション状態を圧縮して見せます。",
        zh: "压缩展示当前工作中的代理与会话状态。",
      })}
      style={{
        minHeight: 320,
        borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
      actions={(
        <div className="flex flex-wrap gap-2">
          <SurfaceMetaBadge tone="success">
            {t({ ko: `${stats.agents.working} working`, en: `${stats.agents.working} working`, ja: `${stats.agents.working} working`, zh: `${stats.agents.working} working` })}
          </SurfaceMetaBadge>
          <SurfaceMetaBadge tone="neutral">
            {t({ ko: `${stats.agents.idle} idle`, en: `${stats.agents.idle} idle`, ja: `${stats.agents.idle} idle`, zh: `${stats.agents.idle} idle` })}
          </SurfaceMetaBadge>
        </div>
      )}
    >
      {visibleRows.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "표시할 에이전트가 없습니다.", en: "No agents available.", ja: "表示するエージェントがいません。", zh: "没有可显示的代理。" })}
        </SurfaceEmptyState>
      ) : (
        <>
          <div
            className="rounded-[24px] border p-4"
            style={{
              borderColor: "rgba(148,163,184,0.16)",
              background:
                "radial-gradient(circle at top, color-mix(in srgb, var(--th-accent-info) 12%, transparent), transparent 52%), linear-gradient(180deg, color-mix(in srgb, var(--th-bg-surface) 94%, transparent), color-mix(in srgb, var(--th-card-bg) 90%, transparent))",
            }}
          >
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              {visibleRows.map((row) => {
                const statusTone = getAgentStatusTone(row.agent.status);
                const accent =
                  statusTone === "success"
                    ? "var(--th-accent-success)"
                    : statusTone === "warn"
                      ? "var(--th-accent-warn)"
                      : statusTone === "danger"
                        ? "var(--th-accent-danger)"
                        : "var(--th-text-muted)";
                return (
                  <button
                    key={row.agent.id}
                    type="button"
                    onClick={onSelectAgent ? () => onSelectAgent(row.agent) : undefined}
                    className="rounded-2xl border p-3 text-left transition-transform hover:-translate-y-0.5"
                    style={{
                      borderColor: `color-mix(in srgb, ${accent} 22%, var(--th-border) 78%)`,
                      background: "color-mix(in srgb, var(--th-card-bg) 88%, transparent)",
                    }}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="relative">
                        <AgentAvatar agent={row.agent} size={44} />
                        <span
                          className="absolute -right-0.5 -top-0.5 inline-flex h-3 w-3 rounded-full border-2"
                          style={{
                            borderColor: "var(--th-card-bg)",
                            background: accent,
                            boxShadow: `0 0 0 3px color-mix(in srgb, ${accent} 16%, transparent)`,
                          }}
                        />
                      </div>
                      <SurfaceMetaBadge tone={statusTone}>
                        {getAgentStatusLabel(row.agent.status, t)}
                      </SurfaceMetaBadge>
                    </div>
                    <div className="mt-3 truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.displayName}
                    </div>
                    <div className="mt-1 min-h-[2.5rem] text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                      {row.workSummary ?? t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "待机中" })}
                    </div>
                    <div className="mt-2 flex items-center justify-between text-[10px]" style={{ color: "var(--th-text-muted)" }}>
                      <span style={{ fontFamily: "var(--font-mono)" }}>
                        {row.elapsedLabel ?? "--"}
                      </span>
                      <span style={{ fontFamily: "var(--font-mono)" }}>
                        {row.linkedSessions.length} session
                      </span>
                    </div>
                  </button>
                );
              })}
            </div>
          </div>

          <div className="mt-4 flex flex-wrap gap-2">
            <SurfaceMetaBadge tone="success">
              {t({ ko: `${stats.agents.working} working`, en: `${stats.agents.working} working`, ja: `${stats.agents.working} working`, zh: `${stats.agents.working} working` })}
            </SurfaceMetaBadge>
            <SurfaceMetaBadge tone="neutral">
              {t({ ko: `${stats.agents.idle} idle`, en: `${stats.agents.idle} idle`, ja: `${stats.agents.idle} idle`, zh: `${stats.agents.idle} idle` })}
            </SurfaceMetaBadge>
            <SurfaceMetaBadge tone="warn">
              {t({ ko: `${stats.dispatched_count} dispatched`, en: `${stats.dispatched_count} dispatched`, ja: `${stats.dispatched_count} dispatched`, zh: `${stats.dispatched_count} dispatched` })}
            </SurfaceMetaBadge>
          </div>
        </>
      )}
    </SurfaceSubsection>
  );
}

function DashboardHomeSignalsWidget({
  rows,
  maxValue,
  t,
}: {
  rows: HomeSignalRow[];
  maxValue: number;
  t: TFunction;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "운영 미션", en: "Ops Missions", ja: "運用ミッション", zh: "运营任务" })}
      description={t({
        ko: "지금 바로 처리할 운영 압력을 우선순위 카드로 정리했습니다.",
        en: "Priority cards for the operational pressure points that need action now.",
        ja: "今すぐ処理すべき運用圧力を優先カードで整理しました。",
        zh: "将需要立即处理的运营压力整理成优先级卡片。",
      })}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-primary) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="mb-4 flex items-center justify-between gap-2">
        <SurfaceMetaBadge tone="neutral">
          {t({
            ko: `${rows.length}개 트래킹`,
            en: `${rows.length} tracked`,
            ja: `${rows.length}件を追跡中`,
            zh: `跟踪 ${rows.length} 项`,
          })}
        </SurfaceMetaBadge>
        <span
          className="text-[11px]"
          style={{
            color: "var(--th-text-muted)",
            fontFamily: "var(--font-mono)",
          }}
        >
          {t({ ko: "priority live", en: "priority live", ja: "priority live", zh: "priority live" })}
        </span>
      </div>

      <div className="space-y-2.5">
        {rows.map((row) => {
          const accent = getSignalAccent(row.tone);
          const tone = row.tone === "info" ? "info" : row.tone;
          const ratio = Math.max(0, Math.min(100, (row.value / maxValue) * 100));
          const body = (
            <div
              className="rounded-[22px] border p-4 text-left transition-transform duration-150"
              style={{
                borderColor: `color-mix(in srgb, ${accent} 24%, var(--th-border) 76%)`,
                background: `linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 93%, ${accent} 7%) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)`,
              }}
            >
              <div className="flex items-start gap-3">
                <div
                  className="mt-0.5 flex h-6 w-6 shrink-0 items-center justify-center rounded-lg border text-[11px] font-semibold"
                  style={{
                    borderColor: `color-mix(in srgb, ${accent} 26%, var(--th-border) 74%)`,
                    background: `color-mix(in srgb, ${accent} 12%, var(--th-card-bg) 88%)`,
                    color: accent,
                  }}
                >
                  {row.value > 0 ? "!" : "·"}
                </div>

                <div className="min-w-0 flex-1">
                  <div className="text-[10.5px] font-semibold uppercase tracking-[0.16em]" style={{ color: accent }}>
                    {row.label}
                  </div>
                  <div className="mt-2 flex items-end justify-between gap-3">
                    <div className="text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
                      {row.value}
                    </div>
                    <SurfaceMetaBadge tone={tone}>{row.description}</SurfaceMetaBadge>
                  </div>
                  <div className="mt-3 flex items-center justify-between gap-3 text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    <span>
                      {t({
                        ko: row.value > 0 ? "지금 확인 필요" : "현재 추가 조치 없음",
                        en: row.value > 0 ? "Needs attention now" : "No extra action right now",
                        ja: row.value > 0 ? "今すぐ確認が必要" : "追加アクションなし",
                        zh: row.value > 0 ? "需要立即确认" : "当前无需额外处理",
                      })}
                    </span>
                    <span style={{ fontFamily: "var(--font-mono)" }}>
                      {t({ ko: "압력", en: "pressure", ja: "pressure", zh: "pressure" })} {Math.round(ratio)}%
                    </span>
                  </div>

                  <div className="mt-3 flex items-center gap-3">
                    <div className="h-[5px] flex-1 overflow-hidden rounded-full" style={{ background: "color-mix(in srgb, var(--th-bg-surface) 82%, transparent)" }}>
                      <div
                        className="h-full rounded-full"
                        style={{
                          width: `${Math.max(8, ratio)}%`,
                          background: `linear-gradient(90deg, color-mix(in srgb, ${accent} 68%, white 32%), ${accent})`,
                        }}
                      />
                    </div>
                    <span className="text-[11px] font-medium" style={{ color: accent }}>
                      {row.onAction
                        ? t({ ko: "열기", en: "Open", ja: "開く", zh: "打开" })
                        : t({ ko: "모니터링", en: "Monitoring", ja: "監視中", zh: "监控中" })}
                    </span>
                  </div>
                </div>
              </div>
            </div>
          );

          return row.onAction ? (
            <button
              key={row.id}
              type="button"
              onClick={row.onAction}
              className="block w-full rounded-2xl text-left transition-transform hover:-translate-y-0.5"
            >
              {body}
            </button>
          ) : (
            <div key={row.id}>{body}</div>
          );
        })}
      </div>
    </SurfaceSubsection>
  );
}

function DashboardHomeRosterWidget({
  rows,
  t,
  numberFormatter,
  onSelectAgent,
  onOpenAchievements,
}: {
  rows: HomeAgentRow[];
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  onSelectAgent?: (agent: Agent) => void;
  onOpenAchievements?: () => void;
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "에이전트 현황", en: "Agent Roster", ja: "エージェント現況", zh: "代理现况" })}
      description={t({
        ko: "활성 우선으로 상위 에이전트 상태를 요약합니다.",
        en: "A live-first roster summary of the top agents.",
        ja: "アクティブ優先で上位エージェントの状態を要約します。",
        zh: "按活跃优先总结头部代理状态。",
      })}
      actions={onOpenAchievements ? (
        <SurfaceActionButton tone="accent" onClick={onOpenAchievements}>
          {t({ ko: "업적 보기", en: "Open XP", ja: "XP を開く", zh: "查看 XP" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-success) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-success) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {rows.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "표시할 에이전트가 없습니다.", en: "No agents to show.", ja: "表示するエージェントがいません。", zh: "没有可显示的代理。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="space-y-2">
          {rows.map((row) => (
            <SurfaceListItem
              key={row.agent.id}
              tone={getAgentStatusTone(row.agent.status)}
              trailing={(
                <div className="flex items-center gap-2">
                  <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                    <div style={{ color: "var(--th-text-heading)" }}>
                      XP {numberFormatter.format(row.agent.stats_xp)}
                    </div>
                    <div>{numberFormatter.format(row.agent.stats_tasks_done)} done</div>
                  </div>
                  {onSelectAgent ? (
                    <SurfaceActionButton compact tone="neutral" onClick={() => onSelectAgent(row.agent)}>
                      {t({ ko: "열기", en: "Open", ja: "開く", zh: "打开" })}
                    </SurfaceActionButton>
                  ) : null}
                </div>
              )}
            >
              <div className="flex items-start gap-3">
                <AgentAvatar agent={row.agent} size={34} />
                <div className="min-w-0">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {row.displayName}
                    </span>
                    <SurfaceMetaBadge tone={getAgentStatusTone(row.agent.status)}>
                      {getAgentStatusLabel(row.agent.status, t)}
                    </SurfaceMetaBadge>
                  </div>
                  <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {row.workSummary ?? t({ ko: "대기 중", en: "Idle", ja: "待機中", zh: "待机中" })}
                  </div>
                  <div className="mt-2 flex flex-wrap gap-2 text-[11px]">
                    {row.elapsedLabel ? <SurfaceMetaBadge>{row.elapsedLabel}</SurfaceMetaBadge> : null}
                    <SurfaceMetaBadge>{row.linkedSessions.length} session</SurfaceMetaBadge>
                  </div>
                </div>
              </div>
            </SurfaceListItem>
          ))}
        </div>
      )}
    </SurfaceSubsection>
  );
}

function DashboardHomeActivityWidget({
  items,
  localeTag,
  t,
  onOpenMeetings,
}: {
  items: HomeActivityItem[];
  localeTag: string;
  t: TFunction;
  onOpenMeetings?: () => void;
}) {
  const formatter = useMemo(
    () =>
      new Intl.DateTimeFormat(localeTag, {
        month: "short",
        day: "numeric",
        hour: "2-digit",
        minute: "2-digit",
      }),
    [localeTag],
  );

  return (
    <SurfaceSubsection
      title={t({ ko: "최근 활동", en: "Recent Activity", ja: "最近の活動", zh: "最近活动" })}
      description={t({
        ko: "회의와 세션 전환을 시간순으로 압축해 보여줍니다.",
        en: "A compressed activity stream across meetings and sessions.",
        ja: "会議とセッション遷移を時間順で圧縮表示します。",
        zh: "按时间顺序压缩展示会议与会话活动。",
      })}
      actions={onOpenMeetings ? (
        <SurfaceActionButton tone="neutral" onClick={onOpenMeetings}>
          {t({ ko: "회의 보기", en: "Open Meetings", ja: "会議を開く", zh: "打开会议" })}
        </SurfaceActionButton>
      ) : undefined}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-warn) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      {items.length === 0 ? (
        <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
          {t({ ko: "최근 활동이 없습니다.", en: "No recent activity.", ja: "最近の活動はありません。", zh: "暂无最近活动。" })}
        </SurfaceEmptyState>
      ) : (
        <div className="space-y-2">
          {items.map((item) => (
            <SurfaceListItem
              key={item.id}
              tone={item.tone === "success" ? "success" : "warn"}
              trailing={(
                <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                  <div>{timeAgo(item.timestamp, localeTag)}</div>
                  <div style={{ fontFamily: "var(--font-mono)" }}>{formatter.format(item.timestamp)}</div>
                </div>
              )}
            >
              <div className="min-w-0">
                <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                  {item.title}
                </div>
                <div className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {item.detail}
                </div>
              </div>
            </SurfaceListItem>
          ))}
        </div>
      )}
    </SurfaceSubsection>
  );
}

function DashboardHomeSectionNavigatorWidget({
  tabDefinitions,
  activeTab,
  t,
  topRepos,
  openTotal,
  onClickTab,
  onKeyDown,
  buttonRefs,
}: {
  tabDefinitions: DashboardTabDefinition[];
  activeTab: DashboardTab;
  t: TFunction;
  topRepos: Array<{
    github_repo: string;
    open_count: number;
    pressure_count: number;
  }>;
  openTotal: number;
  onClickTab: (tab: DashboardTab) => void;
  onKeyDown: (event: ReactKeyboardEvent<HTMLButtonElement>, tab: DashboardTab) => void;
  buttonRefs: { current: Record<DashboardTab, HTMLButtonElement | null> };
}) {
  return (
    <SurfaceSubsection
      title={t({ ko: "빠른 이동", en: "Quick Navigation", ja: "クイック移動", zh: "快速导航" })}
      description={t({
        ko: "홈에서 각 운영 섹션과 칸반 압력을 바로 전환합니다.",
        en: "Jump directly into each operational section and kanban pressure lane from home.",
        ja: "ホームから各運用セクションとカンバン圧力レーンへ直接移動します。",
        zh: "从首页直接跳转到各运营分区与看板压力区。",
      })}
      style={{
        borderColor: "color-mix(in srgb, var(--th-accent-info) 22%, var(--th-border) 78%)",
        background:
          "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, var(--th-accent-info) 6%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
      }}
    >
      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.05fr)_minmax(0,0.95fr)]">
        <div
          role="tablist"
          aria-label={t({ ko: "대시보드 섹션", en: "Dashboard sections", ja: "ダッシュボードセクション", zh: "仪表盘分区" })}
          className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3"
        >
          {tabDefinitions.map((definition) => (
            <DashboardTabButton
              key={definition.id}
              tab={definition.id}
              active={activeTab === definition.id}
              label={definition.label}
              detail={definition.detail}
              onClick={() => onClickTab(definition.id)}
              onKeyDown={onKeyDown}
              buttonRef={(node) => {
                buttonRefs.current[definition.id] = node;
              }}
            />
          ))}
        </div>

        <SurfaceCard
          className="rounded-[24px] p-4"
          style={{
            borderColor: "color-mix(in srgb, var(--th-accent-primary) 20%, var(--th-border) 80%)",
            background:
              "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 95%, var(--th-accent-primary) 5%) 0%, color-mix(in srgb, var(--th-bg-surface) 96%, transparent) 100%)",
          }}
        >
          <div className="text-[11px] font-semibold uppercase tracking-[0.16em]" style={{ color: "var(--th-text-muted)" }}>
            {t({ ko: "Kanban Snapshot", en: "Kanban Snapshot", ja: "Kanban Snapshot", zh: "Kanban Snapshot" })}
          </div>
          <div className="mt-3 text-3xl font-black tracking-tight" style={{ color: "var(--th-text-heading)" }}>
            {openTotal}
          </div>
          <p className="mt-1 text-xs leading-5" style={{ color: "var(--th-text-muted)" }}>
            {t({
              ko: "현재 열려 있는 전체 카드 수와 압력이 높은 저장소입니다.",
              en: "Open card count and the repos with the heaviest pressure.",
              ja: "現在開いているカード総数と圧力の高いリポジトリです。",
              zh: "当前打开卡片总数与压力最高的仓库。",
            })}
          </p>

          <div className="mt-4 space-y-2">
            {topRepos.length === 0 ? (
              <SurfaceEmptyState className="px-4 py-6 text-center text-sm">
                {t({ ko: "추적 중인 저장소가 없습니다.", en: "No repo pressure tracked yet.", ja: "追跡中のリポジトリがありません。", zh: "暂无正在跟踪的仓库压力。" })}
              </SurfaceEmptyState>
            ) : (
              topRepos.slice(0, 3).map((repo) => (
                <SurfaceListItem
                  key={repo.github_repo}
                  tone={repo.pressure_count > 0 ? "warn" : "neutral"}
                  trailing={(
                    <div className="text-right text-[11px]" style={{ color: "var(--th-text-muted)" }}>
                      <div style={{ color: "var(--th-text-heading)" }}>{repo.open_count}</div>
                      <div>{repo.pressure_count} pressure</div>
                    </div>
                  )}
                >
                  <div className="min-w-0">
                    <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text-heading)" }}>
                      {repo.github_repo}
                    </div>
                    <div className="mt-1 text-xs" style={{ color: "var(--th-text-muted)" }}>
                      {t({
                        ko: repo.pressure_count > 0 ? "리뷰/블록 압력 있음" : "오픈 카드 추적 중",
                        en: repo.pressure_count > 0 ? "Pressure in review/blocked" : "Tracking open cards",
                        ja: repo.pressure_count > 0 ? "レビュー/ブロック圧力あり" : "オープンカード追跡中",
                        zh: repo.pressure_count > 0 ? "存在 review/blocked 压力" : "正在跟踪打开卡片",
                      })}
                    </div>
                  </div>
                </SurfaceListItem>
              ))
            )}
          </div>
        </SurfaceCard>
      </div>
    </SurfaceSubsection>
  );
}

function getAgentStatusTone(status: Agent["status"]): "neutral" | "success" | "warn" | "danger" {
  switch (status) {
    case "working":
      return "success";
    case "break":
      return "warn";
    case "offline":
      return "danger";
    case "idle":
    default:
      return "neutral";
  }
}

function getAgentStatusLabel(status: Agent["status"], t: TFunction): string {
  switch (status) {
    case "working":
      return t({ ko: "작업 중", en: "Working", ja: "作業中", zh: "工作中" });
    case "break":
      return t({ ko: "휴식", en: "Break", ja: "休憩", zh: "休息" });
    case "offline":
      return t({ ko: "오프라인", en: "Offline", ja: "オフライン", zh: "离线" });
    case "idle":
    default:
      return t({ ko: "대기", en: "Idle", ja: "待機", zh: "待机" });
  }
}

function getSignalAccent(tone: HomeSignalTone): string {
  switch (tone) {
    case "success":
      return "#22c55e";
    case "warn":
      return "#f59e0b";
    case "danger":
      return "#ef4444";
    case "info":
    default:
      return "#14b8a6";
  }
}
