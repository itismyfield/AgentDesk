import { useCallback, useEffect, useMemo, useState } from "react";
import { closestCenter, DndContext, KeyboardSensor, MouseSensor, useSensor, useSensors, type DragEndEvent, type DragOverEvent, type DragStartEvent } from "@dnd-kit/core";
import { arrayMove, rectSortingStrategy, SortableContext, sortableKeyboardCoordinates } from "@dnd-kit/sortable";
import { ChevronRight, GripVertical } from "lucide-react";

import * as api from "../api/client";
import type { Agent, DashboardStats, KanbanCard, RoundTableMeeting, TokenAnalyticsResponse } from "../types";
import type { Notification } from "../components/NotificationCenter";
import type { TFunction } from "../components/dashboard/model";
import { getAgentLevelFromXp, getMissionResetCountdown, getMissionTotalXp, type DailyMissionViewModel } from "../components/gamification/GamificationShared";
import { STORAGE_KEYS } from "../lib/storageKeys";
import { useLocalStorage } from "../lib/useLocalStorage";
import { countOpenMeetingIssues } from "./meetingSummary";
import { formatRelativeTime, notificationColor } from "./shellFormatting";
import { areStringArraysEqual, HOME_DEFAULT_WIDGETS, HOME_PRIMARY_WIDGET_SET, HOME_SUPPORT_WIDGET_SET, normalizeHomeWidgetOrder } from "./HomeOverviewConfig";
import { HomeSortableWidget } from "./HomeOverviewWidgets";
import { buildHomeWidgetSpecs } from "./HomeWidgetSpecs";

export default function HomeOverviewPage({
  isMobileViewport,
  isKo,
  wsConnected,
  currentOfficeLabel,
  stats,
  agents,
  meetings,
  notifications,
  kanbanCards,
}: {
  isMobileViewport: boolean;
  isKo: boolean;
  wsConnected: boolean;
  currentOfficeLabel: string;
  stats: DashboardStats | null;
  agents: Agent[];
  meetings: RoundTableMeeting[];
  notifications: Notification[];
  kanbanCards: KanbanCard[];
}) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const t: TFunction = useCallback(
    (messages) => (isKo ? messages.ko : messages.en ?? messages.ko),
    [isKo],
  );
  const localeTag = isKo ? "ko-KR" : "en-US";
  const [editing, setEditing] = useLocalStorage<boolean>(STORAGE_KEYS.homeEditing, false);
  const [supportOpen, setSupportOpen] = useLocalStorage<boolean>(
    STORAGE_KEYS.homeSupportOpen,
    false,
  );
  const [activeWidgetId, setActiveWidgetId] = useState<string | null>(null);
  const [overWidgetId, setOverWidgetId] = useState<string | null>(null);
  const [analytics, setAnalytics] = useState<TokenAnalyticsResponse | null>(
    () => api.getCachedTokenAnalytics("7d")?.data ?? null,
  );
  const [homeKpiTrends, setHomeKpiTrends] = useState<api.HomeKpiTrendsResponse | null>(null);
  const [gamification, setGamification] = useState<api.AchievementsResponse | null>(null);
  const [streaks, setStreaks] = useState<api.AgentStreak[]>([]);
  const defaultWidgets = useMemo(
    () => [...HOME_DEFAULT_WIDGETS],
    [],
  );
  const [widgets, setWidgets] = useLocalStorage<string[]>(
    STORAGE_KEYS.homeOrder,
    () => [...HOME_DEFAULT_WIDGETS],
  );
  const widgetDragSensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  const outstandingMeetings = meetings.filter((meeting) => countOpenMeetingIssues(meeting) > 0).length;
  const liveNotifications = notifications.filter(
    (notification) => Date.now() - notification.ts < 60_000,
  ).length;
  const requestedCards = kanbanCards.filter((card) => card.status === "requested").length;
  const inProgressCards = kanbanCards.filter(
    (card) => card.status === "in_progress" || card.status === "review",
  ).length;
  const topAgents = useMemo(
    () =>
      (stats?.top_agents?.length
        ? stats.top_agents
        : [...agents]
            .sort(
              (left, right) =>
                right.stats_xp - left.stats_xp ||
                right.stats_tasks_done - left.stats_tasks_done,
            )
            .map((agent) => ({
              id: agent.id,
              name: agent.name,
              alias: agent.alias ?? null,
              name_ko: agent.name_ko,
              avatar_emoji: agent.avatar_emoji,
              stats_tasks_done: agent.stats_tasks_done,
              stats_xp: agent.stats_xp,
              stats_tokens: agent.stats_tokens,
            })))
        .slice(0, 6),
    [agents, stats?.top_agents],
  );
  const KANBAN_DONE_RECENT_WINDOW_MS = 24 * 60 * 60 * 1000;
  const recentDoneCards = useMemo(() => {
    const cutoff = Date.now() - KANBAN_DONE_RECENT_WINDOW_MS;
    return kanbanCards
      .filter((card) => card.status === "done" && (card.completed_at ?? 0) >= cutoff)
      .sort((a, b) => (b.completed_at ?? 0) - (a.completed_at ?? 0));
  }, [kanbanCards]);
  const recentDoneCount = recentDoneCards.length;
  const blockedCards = kanbanCards.filter((card) => card.status === "blocked").length;
  const totalActionableCards = requestedCards + inProgressCards + blockedCards;
  const totalMeetings = meetings.length;
  const reviewQueue = stats?.kanban.review_queue ?? kanbanCards.filter((card) => card.status === "review").length;
  const agentTotal = stats?.agents.total ?? topAgents.length;
  const liveSessions = stats?.dispatched_count ?? 0;
  const providerSummary = tr("2/2 프로바이더 연결", "2/2 providers connected");
  const operationalMissionRows: DailyMissionViewModel[] = [
    {
      id: "review",
      label: tr("리뷰 대기 비우기", "Clear review queue"),
      current: reviewQueue === 0 ? 1 : 0,
      target: 1,
      completed: reviewQueue === 0,
      description: tr("우선 확인이 필요한 카드", "Cards waiting for reviewer action"),
      xp: 35,
    },
    {
      id: "blocked",
      label: tr("블록 카드 줄이기", "Reduce blocked cards"),
      current: Math.max(0, 1 - Math.min(stats?.kanban.blocked ?? blockedCards, 1)),
      target: 1,
      completed: blockedCards === 0,
      description: tr("의존성/외부 응답 대기", "Waiting on dependencies or replies"),
      xp: 30,
    },
    {
      id: "dispatch",
      label: tr("실시간 세션 유지", "Keep live sessions healthy"),
      current: Math.min(stats?.dispatched_count ?? 0, 3),
      target: 3,
      completed: (stats?.dispatched_count ?? 0) >= 3 && wsConnected,
      description: tr("현재 연결된 작업 세션", "Currently connected working sessions"),
      xp: 40,
    },
    {
      id: "meetings",
      label: tr("회의 후속 정리", "Close meeting follow-ups"),
      current: Math.max(0, totalMeetings - outstandingMeetings),
      target: Math.max(totalMeetings, 1),
      completed: outstandingMeetings === 0,
      description: tr("정리/이슈화가 필요한 회의", "Meetings still needing wrap-up"),
      xp: 25,
    },
  ];
  const activityItems = notifications.slice(0, 4).map((notification) => ({
    id: notification.id,
    title: notification.message,
    meta: formatRelativeTime(notification.ts, isKo),
    accent: notificationColor(notification.type),
  }));
  const fallbackActivity = meetings.slice(0, 4).map((meeting) => ({
    id: meeting.id,
    title: meeting.agenda,
    meta: meeting.status === "completed"
      ? tr("회의 종료", "Meeting completed")
      : tr("회의 진행 중", "Meeting in progress"),
    accent:
      meeting.status === "completed"
        ? "var(--th-accent-primary)"
        : "var(--th-accent-warn)",
  }));
  const kanbanColumns = [
    { id: "requested", label: tr("요청", "Requested"), accent: "#7dd3fc" },
    { id: "in_progress", label: tr("진행", "In progress"), accent: "#6ef2a3" },
    { id: "review", label: tr("리뷰", "Review"), accent: "#f5bd47" },
    { id: "done", label: tr("완료", "Done"), accent: "#c084fc" },
  ] as const;

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    const cachedAnalytics = api.getCachedTokenAnalytics("7d");
    if (cachedAnalytics) {
      setAnalytics(cachedAnalytics.data);
    }
    api
      .getTokenAnalytics("7d", { signal: controller.signal })
      .then((next) => {
        if (!active) return;
        setAnalytics(next);
      })
      .catch((error) => {
        if (!active || controller.signal.aborted) return;
        console.error("Failed to load token analytics for home overview", error);
      });

    return () => {
      active = false;
      controller.abort();
    };
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    api
      .getHomeKpiTrends(14, { signal: controller.signal })
      .then((next) => {
        if (!active) return;
        setHomeKpiTrends(next);
      })
      .catch((error) => {
        if (!active || controller.signal.aborted) return;
        console.error("Failed to load home KPI trends", error);
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, []);

  useEffect(() => {
    if (typeof window === "undefined") return;
    if (window.localStorage.getItem(STORAGE_KEYS.homeOrder) !== null) return;
    try {
      const legacyRaw =
        window.localStorage.getItem("agentdesk.widgets") ??
        window.localStorage.getItem("agentdesk.home.widgets");
      const parsed = legacyRaw ? (JSON.parse(legacyRaw) as unknown) : null;
      if (Array.isArray(parsed) && parsed.length > 0) {
        const migrated = normalizeHomeWidgetOrder(parsed);
        if (migrated.length > 0) {
          setWidgets(migrated);
        }
      }
    } catch {
    }
  }, [setWidgets]);

  useEffect(() => {
    const normalized = normalizeHomeWidgetOrder(widgets);
    if (!areStringArraysEqual(widgets, normalized)) {
      setWidgets(normalized);
    }
  }, [setWidgets, widgets]);

  useEffect(() => {
    if (!isMobileViewport || !editing) return;
    setEditing(false);
  }, [editing, isMobileViewport, setEditing]);

  useEffect(() => {
    let active = true;
    Promise.all([
      api.getAchievements().catch(() => ({ achievements: [], daily_missions: [] })),
      api.getStreaks().catch(() => ({ streaks: [] })),
    ]).then(([achievementResponse, streakResponse]) => {
      if (!active) return;
      setGamification(achievementResponse);
      setStreaks(
        [...streakResponse.streaks].sort((left, right) => right.streak - left.streak),
      );
    });
    return () => {
      active = false;
    };
  }, []);

  const todayLabel = useMemo(
    () =>
      new Intl.DateTimeFormat(isKo ? "ko-KR" : "en-US", {
        weekday: "long",
        month: "short",
        day: "numeric",
      }).format(new Date()),
    [isKo],
  );
  const latestAnalyticsDay = analytics?.daily.at(-1) ?? null;
  const tokenTrend = analytics?.daily.slice(-7).map((day) => day.total_tokens) ?? [];
  const costTrend = analytics?.daily.slice(-7).map((day) => day.cost) ?? [];
  const inProgressTrend = homeKpiTrends?.in_progress.values ?? [];
  const activityStreak = useMemo(() => {
    const daily = [...(analytics?.daily ?? [])].sort((left, right) =>
      left.date.localeCompare(right.date),
    );
    let streak = 0;
    for (let index = daily.length - 1; index >= 0; index -= 1) {
      if (daily[index].total_tokens <= 0) break;
      streak += 1;
    }
    return streak;
  }, [analytics]);
  const formatCompact = useCallback((value: number): string => {
    if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
    if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
    if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
    return Math.round(value).toString();
  }, []);
  const formatCurrency = useCallback(
    (value: number) =>
      new Intl.NumberFormat(isKo ? "en-US" : "en-US", {
        style: "currency",
        currency: "USD",
        maximumFractionDigits: value >= 100 ? 0 : 2,
      }).format(value),
    [],
  );
  const streakLeader = streaks[0] ?? null;
  const gamificationLeader = topAgents[0] ?? null;
  const gamificationLevel = getAgentLevelFromXp(gamificationLeader?.stats_xp ?? 0);
  const dailyMissions = useMemo<DailyMissionViewModel[]>(() => {
    if (gamification?.daily_missions?.length) {
      return gamification.daily_missions.map((mission) => {
        switch (mission.id) {
          case "dispatches_today":
            return {
              id: mission.id,
              label: tr("오늘 디스패치 5건 완료", "Complete 5 dispatches today"),
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
              description: tr("오늘 실제 완료된 디스패치 수", "Completed dispatches today"),
              xp: 40,
            };
          case "active_agents_today":
            return {
              id: mission.id,
              label: tr("오늘 3명 이상 출항", "Get 3 agents shipping today"),
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
              description: tr("오늘 완료 기록이 있는 에이전트 수", "Agents with completed work today"),
              xp: 35,
            };
          case "review_queue_zero":
            return {
              id: mission.id,
              label: tr("리뷰 큐 비우기", "Drain the review queue"),
              current: mission.current,
              target: mission.target,
              completed: mission.completed,
              description: tr("리뷰 대기 카드를 0으로 유지", "Keep the review queue empty"),
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
    return operationalMissionRows;
  }, [gamification?.daily_missions, operationalMissionRows, tr]);
  const missionReset = useMemo(() => getMissionResetCountdown(), []);
  const missionResetLabel = tr(
    `리셋까지 ${missionReset.hours}시간 ${missionReset.minutes}분`,
    `Resets in ${missionReset.hours}h ${missionReset.minutes}m`,
  );
  const missionXpLabel = dailyMissions.length > 0 ? `+${getMissionTotalXp(dailyMissions)} XP` : undefined;
  const widgetSpecs = buildHomeWidgetSpecs({
    activityItems,
    activityStreak,
    agents,
    analytics,
    blockedCards,
    costTrend,
    currentOfficeLabel,
    dailyMissions,
    fallbackActivity,
    formatCompact,
    formatCurrency,
    gamificationLeader,
    gamificationLevel,
    inProgressCards,
    inProgressTrend,
    isKo,
    kanbanCards,
    kanbanColumns,
    latestAnalyticsDay,
    localeTag,
    missionResetLabel,
    missionXpLabel,
    recentDoneCards,
    recentDoneCount,
    requestedCards,
    reviewQueue,
    stats,
    streakLeader,
    t,
    tokenTrend,
    topAgents,
    totalActionableCards,
    tr,
  });
  const primaryWidgets = widgets.filter((widgetId) => HOME_PRIMARY_WIDGET_SET.has(widgetId));
  const supportWidgets = widgets.filter((widgetId) => HOME_SUPPORT_WIDGET_SET.has(widgetId));
  const visibleWidgets = editing ? widgets : primaryWidgets;
  const homeWidgetDragEnabled = editing && !isMobileViewport;
  const handleHomeWidgetDragStart = useCallback(
    (event: DragStartEvent) => {
      if (!homeWidgetDragEnabled) return;
      setActiveWidgetId(String(event.active.id));
      setOverWidgetId(null);
    },
    [homeWidgetDragEnabled],
  );
  const handleHomeWidgetDragOver = useCallback(
    (event: DragOverEvent) => {
      if (!homeWidgetDragEnabled) return;
      setOverWidgetId(event.over ? String(event.over.id) : null);
    },
    [homeWidgetDragEnabled],
  );
  const handleHomeWidgetDragEnd = useCallback(
    (event: DragEndEvent) => {
      const activeId = String(event.active.id);
      const overId = event.over ? String(event.over.id) : null;
      setActiveWidgetId(null);
      setOverWidgetId(null);
      if (!homeWidgetDragEnabled || !overId || activeId === overId) return;

      const fromIndex = widgets.indexOf(activeId);
      const toIndex = widgets.indexOf(overId);
      if (fromIndex === -1 || toIndex === -1) return;
      setWidgets(arrayMove(widgets, fromIndex, toIndex));
    },
    [homeWidgetDragEnabled, setWidgets, widgets],
  );
  const handleHomeWidgetDragCancel = useCallback(() => {
    setActiveWidgetId(null);
    setOverWidgetId(null);
  }, []);
  const supportSummary = tr(
    `품질·미션 ${supportWidgets.length}개`,
    `${supportWidgets.length} quality/mission widgets`,
  );

  return (
    <div className="mx-auto h-full w-full max-w-[92rem] overflow-auto px-4 py-6 pb-32 sm:px-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="max-w-3xl">
          <div className="mb-1.5 flex flex-wrap items-center gap-2 text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-text-muted)" }}>
            <span>{todayLabel}</span>
            <span className="h-1 w-1 rounded-full" style={{ background: "var(--th-text-muted)" }} />
            <span className="inline-flex items-center gap-1.5" style={{ color: wsConnected ? "var(--th-accent-primary)" : "var(--th-accent-danger)" }}>
              <span className="h-2 w-2 rounded-full" style={{ background: wsConnected ? "var(--th-accent-primary)" : "var(--th-accent-danger)" }} />
              {wsConnected ? "all systems normal" : tr("연결 상태 확인 필요", "connection degraded")}
            </span>
          </div>
          <h1 className="text-3xl font-semibold tracking-tight sm:text-4xl" style={{ color: "var(--th-text-heading)" }}>
            {tr("오늘의 AgentDesk", "Today's AgentDesk")}
          </h1>
          <p className="mt-2 max-w-2xl text-sm leading-7 sm:text-base" style={{ color: "var(--th-text-secondary)" }}>
            {tr(
              `에이전트 ${agentTotal}명 · 세션 ${liveSessions} 활성 · ${providerSummary}`,
              `${agentTotal} agents · ${liveSessions} live sessions · ${providerSummary}`,
            )}
          </p>
        </div>

        <div className="flex flex-wrap items-center gap-2">
          {!isMobileViewport && editing && (
            <button
              type="button"
              onClick={() => setWidgets([...defaultWidgets])}
              data-testid="home-reset-order"
              className="rounded-full border px-3 py-2 text-xs font-medium transition-colors hover:bg-white/5"
              style={{ borderColor: "var(--th-border-subtle)", color: "var(--th-text-muted)" }}
            >
              {tr("기본값", "Reset")}
            </button>
          )}
          {!isMobileViewport ? (
            <button
              type="button"
              onClick={() => setEditing((prev) => !prev)}
              data-testid="home-edit-toggle"
              className="inline-flex items-center gap-2 rounded-full border px-3 py-2 text-xs font-medium transition-colors hover:bg-white/5"
              style={{
                borderColor: editing ? "var(--th-accent-primary)" : "var(--th-border-subtle)",
                background: editing ? "var(--th-accent-primary-soft)" : "transparent",
                color: editing ? "var(--th-text-heading)" : "var(--th-text-primary)",
              }}
            >
              <GripVertical size={14} />
              {editing ? tr("완료", "Done") : tr("편집", "Edit")}
            </button>
          ) : null}
        </div>
      </div>

      {!isMobileViewport && editing && (
        <div className="mt-4 rounded-2xl border px-4 py-3 text-sm" style={{ borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)", background: "var(--th-accent-primary-soft)", color: "var(--th-text-secondary)" }}>
          <span className="inline-flex items-center gap-2">
            <GripVertical size={14} />
            {tr(
              "위젯을 드래그해서 순서를 바꿀 수 있습니다. 완료를 누르면 현재 배치가 유지됩니다.",
              "Drag widgets to reorder them. The current layout will persist when you press done.",
            )}
          </span>
        </div>
      )}

      <DndContext
        sensors={widgetDragSensors}
        collisionDetection={closestCenter}
        onDragStart={handleHomeWidgetDragStart}
        onDragOver={handleHomeWidgetDragOver}
        onDragEnd={handleHomeWidgetDragEnd}
        onDragCancel={handleHomeWidgetDragCancel}
      >
        <SortableContext items={visibleWidgets} strategy={rectSortingStrategy}>
          <div className="mt-5 grid grid-cols-1 gap-4 lg:grid-cols-12">
            {visibleWidgets.map((widgetId) => {
              const spec = widgetSpecs[widgetId as keyof typeof widgetSpecs];
              if (!spec) return null;
              return (
                <HomeSortableWidget
                  key={widgetId}
                  widgetId={widgetId}
                  className={spec.className}
                  disabled={!homeWidgetDragEnabled}
                  showHandle={homeWidgetDragEnabled}
                  activeWidgetId={activeWidgetId}
                  overWidgetId={overWidgetId}
                  handleLabel={tr("위젯 순서 변경", "Reorder widget")}
                >
                  {spec.render()}
                </HomeSortableWidget>
              );
            })}
          </div>
        </SortableContext>
      </DndContext>

      {!editing && supportWidgets.length > 0 ? (
        <section
          className="mt-4 rounded-[1.15rem] border"
          style={{
            borderColor: "var(--th-border-subtle)",
            background: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
          }}
          data-testid="home-support-section"
        >
          <button
            type="button"
            className="flex min-h-[52px] w-full items-center justify-between gap-3 px-4 py-3 text-left sm:px-5"
            onClick={() => setSupportOpen((value) => !value)}
            aria-expanded={supportOpen}
            data-testid="home-support-toggle"
          >
            <span className="min-w-0">
              <span
                className="block truncate text-sm font-medium"
                style={{ color: "var(--th-text-secondary)" }}
              >
                {tr("보조 위젯", "Supporting widgets")}
              </span>
              <span
                className="mt-0.5 block truncate text-[11px]"
                style={{ color: "var(--th-text-muted)" }}
              >
                {supportSummary}
              </span>
            </span>
            <ChevronRight
              size={16}
              className={supportOpen ? "shrink-0 rotate-90 transition-transform" : "shrink-0 transition-transform"}
              style={{ color: "var(--th-text-muted)" }}
            />
          </button>
          {supportOpen ? (
            <div
              className="grid grid-cols-1 gap-4 border-t p-4 sm:p-5 lg:grid-cols-12"
              style={{ borderColor: "var(--th-border-subtle)" }}
              data-testid="home-support-grid"
            >
              {supportWidgets.map((widgetId) => {
                const spec = widgetSpecs[widgetId as keyof typeof widgetSpecs];
                if (!spec) return null;
                return (
                  <div key={widgetId} data-testid={`home-widget-${widgetId}`} className={spec.className}>
                    {spec.render()}
                  </div>
                );
              })}
            </div>
          ) : null}
        </section>
      ) : null}
    </div>
  );
}
