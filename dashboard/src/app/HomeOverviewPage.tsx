import { useCallback, useEffect, useMemo, useState } from "react";
import { closestCenter, DndContext, KeyboardSensor, MouseSensor, useSensor, useSensors, type DragEndEvent, type DragOverEvent, type DragStartEvent } from "@dnd-kit/core";
import { arrayMove, rectSortingStrategy, SortableContext, sortableKeyboardCoordinates } from "@dnd-kit/sortable";
import { GripVertical } from "lucide-react";

import * as api from "../api/client";
import type { Agent, DashboardStats, TokenAnalyticsResponse } from "../types";
import type { TFunction } from "../components/dashboard/model";
import { STORAGE_KEYS } from "../lib/storageKeys";
import { useLocalStorage } from "../lib/useLocalStorage";
import { formatRelativeTime } from "./shellFormatting";
import { areStringArraysEqual, HOME_DEFAULT_WIDGETS, normalizeHomeWidgetOrder } from "./HomeOverviewConfig";
import { HomeSortableWidget } from "./HomeOverviewWidgets";
import { buildHomeWidgetSpecs } from "./HomeWidgetSpecs";
import { FreshnessIndicator } from "../components/common/FreshnessIndicator";

/**
 * Loads the 7d token analytics for the home page. It passes no abort signal so
 * the request shares an in-flight GET with other callers; the returned stop
 * function only keeps later answers and errors away from an unmounted page.
 */
export function loadHomeTokenAnalytics(onData: (next: TokenAnalyticsResponse) => void): () => void {
  let active = true;
  const cachedAnalytics = api.getCachedTokenAnalytics("7d");
  if (cachedAnalytics) {
    onData(cachedAnalytics.data);
  }
  api
    .getTokenAnalytics("7d", { suppressErrorToast: true })
    .then((next) => {
      if (!active) return;
      onData(next);
    })
    .catch((error) => {
      if (!active) return;
      console.error("Failed to load token analytics for home overview", error);
    });

  return () => {
    active = false;
  };
}

export default function HomeOverviewPage({
  isMobileViewport,
  isKo,
  wsConnected,
  wsLastEventTs,
  stats,
  agents,
  onOpenAgent,
}: {
  isMobileViewport: boolean;
  isKo: boolean;
  wsConnected: boolean;
  /** Required — null only until the first WS event arrives. */
  wsLastEventTs: number | null;
  stats: DashboardStats | null;
  agents: Agent[];
  onOpenAgent: (agent: Agent) => void;
}) {
  const tr = useCallback((ko: string, en: string) => (isKo ? ko : en), [isKo]);
  const t: TFunction = useCallback(
    (messages) => (isKo ? messages.ko : messages.en ?? messages.ko),
    [isKo],
  );
  const localeTag = isKo ? "ko-KR" : "en-US";
  const [editing, setEditing] = useLocalStorage<boolean>(STORAGE_KEYS.homeEditing, false);
  const [activeWidgetId, setActiveWidgetId] = useState<string | null>(null);
  const [overWidgetId, setOverWidgetId] = useState<string | null>(null);
  const [analytics, setAnalytics] = useState<TokenAnalyticsResponse | null>(
    () => api.getCachedTokenAnalytics("7d")?.data ?? null,
  );
  const [widgets, setWidgets] = useLocalStorage<string[]>(
    STORAGE_KEYS.homeOrder,
    () => [...HOME_DEFAULT_WIDGETS],
  );
  const widgetDragSensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  const agentTotal = stats?.agents.total ?? agents.length;
  const workingTotal = agents.filter((agent) => agent.status === "working").length;

  useEffect(() => loadHomeTokenAnalytics(setAnalytics), []);

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

  const todayLabel = useMemo(
    () =>
      new Intl.DateTimeFormat(localeTag, {
        timeZone: "Asia/Seoul",
        weekday: "long",
        month: "short",
        day: "numeric",
      }).format(new Date()),
    [localeTag],
  );
  const tokenTrend = analytics?.daily.slice(-7).map((day) => day.total_tokens) ?? [];
  const costTrend = analytics?.daily.slice(-7).map((day) => day.cost) ?? [];
  const analyticsGeneratedMs = analytics?.generated_at ? Date.parse(analytics.generated_at) : Number.NaN;
  const analyticsAsOf = Number.isFinite(analyticsGeneratedMs)
    ? tr(`${formatRelativeTime(analyticsGeneratedMs, true)} 기준`, `as of ${formatRelativeTime(analyticsGeneratedMs, false)}`)
    : null;
  const formatCompact = useCallback((value: number): string => {
    if (value >= 1e9) return `${(value / 1e9).toFixed(1)}B`;
    if (value >= 1e6) return `${(value / 1e6).toFixed(1)}M`;
    if (value >= 1e3) return `${(value / 1e3).toFixed(1)}K`;
    return Math.round(value).toLocaleString(localeTag);
  }, [localeTag]);
  const formatCurrency = useCallback(
    (value: number) =>
      new Intl.NumberFormat("en-US", {
        style: "currency",
        currency: "USD",
        maximumFractionDigits: value >= 100 ? 0 : 2,
      }).format(value),
    [],
  );
  const widgetSpecs = buildHomeWidgetSpecs({
    agents,
    analytics,
    analyticsAsOf,
    costTrend,
    formatCompact,
    formatCurrency,
    isKo,
    localeTag,
    onOpenAgent,
    stats,
    t,
    tokenTrend,
    tr,
  });
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

  return (
    <div data-testid="home-overview" className="h-full w-full overflow-auto bg-th-bg-primary text-th-text-primary">
      <div className="mx-auto max-w-[88rem] px-4 py-4 pb-8 sm:px-8 sm:py-7">
        <header className="flex flex-wrap items-start justify-between gap-3 border-b border-th-border-subtle pb-4 sm:pb-5">
          <div className="min-w-0">
            <div className="mb-1 text-xs text-th-text-muted">{todayLabel}</div>
            <h1 className="hidden text-2xl font-semibold tracking-tight text-th-text-heading sm:block">{tr("오늘의 운영", "Today")}</h1>
            <p className="text-xs leading-5 text-th-text-muted sm:mt-2">
              {tr(`에이전트 ${agentTotal}명 중 ${workingTotal}명 작업 중`, `${workingTotal} of ${agentTotal} agents working`)}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-3">
            <div className="flex items-center gap-2 text-xs text-th-text-secondary">
              <span className={`h-1.5 w-1.5 rounded-full ${wsConnected ? "bg-[var(--th-accent-success)]" : "bg-[var(--th-accent-danger)]"}`} aria-hidden="true" />
              <span role="status">{wsConnected ? tr("실시간 연결됨", "Live connection") : tr("실시간 연결 끊김", "Live connection lost")}</span>
              {wsConnected ? <FreshnessIndicator timestamp={wsLastEventTs} compact label={tr("업데이트", "Updated")} staleAfterSeconds={45} criticalAfterSeconds={180} /> : null}
            </div>
            {!isMobileViewport && editing && (
              <button
                type="button"
                onClick={() => setWidgets([...HOME_DEFAULT_WIDGETS])}
                data-testid="home-reset-order"
                className="min-h-11 rounded-md border border-th-border-subtle px-3 py-2 text-xs font-medium text-th-text-muted transition-colors hover:bg-surface-subtle"
              >
                {tr("기본 배치로", "Reset layout")}
              </button>
            )}
            {!isMobileViewport ? (
              <button
                type="button"
                onClick={() => setEditing((prev) => !prev)}
                data-testid="home-edit-toggle"
                className="inline-flex min-h-11 items-center gap-2 rounded-md border px-3 py-2 text-xs font-medium transition-colors hover:bg-surface-subtle"
                style={{
                  borderColor: editing ? "var(--th-accent-primary)" : "var(--th-border-subtle)",
                  background: editing ? "var(--th-accent-primary-soft)" : "transparent",
                  color: editing ? "var(--th-text-heading)" : "var(--th-text-primary)",
                }}
              >
                <GripVertical size={14} />
                {editing ? tr("배치 완료", "Done") : tr("배치 바꾸기", "Rearrange")}
              </button>
            ) : null}
          </div>
        </header>

        {!isMobileViewport && editing && (
          <div className="mt-4 rounded-lg border px-4 py-3 text-sm" style={{ borderColor: "color-mix(in srgb, var(--th-accent-primary) 26%, var(--th-border) 74%)", background: "var(--th-accent-primary-soft)", color: "var(--th-text-primary)" }}>
            <span className="inline-flex items-center gap-2">
              <GripVertical size={14} />
              {tr("카드를 끌어서 순서를 바꿔요. 배치 완료를 누르면 그대로 유지돼요.", "Drag cards to reorder. Press done to keep the layout.")}
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
          <SortableContext items={widgets} strategy={rectSortingStrategy}>
            <div className="mt-4 grid grid-cols-2 gap-3 sm:mt-5 sm:gap-4 lg:grid-cols-12">
              {widgets.map((widgetId) => {
                const spec = widgetSpecs[widgetId];
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
                    handleLabel={tr("카드 순서 바꾸기", "Reorder card")}
                  >
                    {spec.render()}
                  </HomeSortableWidget>
                );
              })}
            </div>
          </SortableContext>
        </DndContext>
      </div>
    </div>
  );
}
