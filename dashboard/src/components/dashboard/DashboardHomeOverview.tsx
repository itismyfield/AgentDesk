import { useCallback, useEffect, useMemo, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
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
import type { DashboardTab } from "../../app/dashboardTabs";
import type { Agent, CompanySettings, DashboardStats, DispatchedSession, RoundTableMeeting } from "../../types";
import {
  formatElapsedCompact,
  getAgentWorkElapsedMs,
  getAgentWorkSummary,
} from "../../agent-insights";
import { formatProviderFlow } from "../MeetingProviderFlow";
import { SurfaceActionButton } from "../common/SurfacePrimitives";
import {
  DashboardHomeSectionNavigatorWidget,
  DashboardSortableWidget,
} from "./DashboardHomeRenderers";
import { buildDashboardHomeWidgetSpecs } from "./DashboardHomeWidgetSpecs";
import type { TFunction } from "./model";
import {
  DEFAULT_HOME_WIDGET_ORDER,
  HOME_WIDGET_STORAGE_KEY,
  readStoredHomeWidgetOrder,
  type HomeWidgetId,
} from "./homeWidgetOrder";

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

interface DashboardHomeOverviewProps {
  t: TFunction;
  numberFormatter: Intl.NumberFormat;
  dashboardStats: DashboardStats;
  staleLinkedSessions: DispatchedSession[];
  reconnectingSessions: DispatchedSession[];
  meetingSummary: { activeCount: number; unresolvedCount: number };
  meetings: RoundTableMeeting[];
  recentMeetings: RoundTableMeeting[];
  language: CompanySettings["language"];
  agents: Agent[];
  sessions: DispatchedSession[];
  localeTag: string;
  onOpenKanbanSignal?: (signal: "review" | "blocked" | "requested" | "stalled") => void;
  onSelectAgent?: (agent: Agent) => void;
  tabDefinitions: DashboardTabDefinition[];
  activeTab: DashboardTab;
  onSelectTab: (tab: DashboardTab) => void;
  onTabKeyDown: (event: ReactKeyboardEvent<HTMLButtonElement>, tab: DashboardTab) => void;
  tabButtonRefs: { current: Record<DashboardTab, HTMLButtonElement | null> };
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

export function DashboardHomeOverview({
  t,
  numberFormatter,
  dashboardStats,
  staleLinkedSessions,
  reconnectingSessions,
  meetingSummary,
  meetings,
  recentMeetings,
  language,
  agents,
  sessions,
  localeTag,
  onOpenKanbanSignal,
  onSelectAgent,
  tabDefinitions,
  activeTab,
  onSelectTab,
  onTabKeyDown: handleTabKeyDown,
  tabButtonRefs,
}: DashboardHomeOverviewProps) {
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
        onAction: () => onSelectTab("meetings"),
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

  const homeWidgetSpecs = buildDashboardHomeWidgetSpecs({
    t,
    numberFormatter,
    dashboardStats,
    reconnectingSessions,
    activeSessions,
    meetingSummary,
    meetings,
    homeAgents,
    language,
    onSelectAgent,
    focusSignals,
    agents,
    localeTag,
    homeActivityItems,
    onSelectTab,
  });

  return (
    <>
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
        onClickTab={onSelectTab}
        onKeyDown={handleTabKeyDown}
        buttonRefs={tabButtonRefs}
      />

    </>
  );
}
