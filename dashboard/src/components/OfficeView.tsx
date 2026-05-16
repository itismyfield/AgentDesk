import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Application, Container, Graphics, Text, Texture } from "pixi.js";
import type { Agent, AuditLogEntry, Department, KanbanCard, RoundTableMeeting, Task, SubAgent } from "../types";
type ThemeMode = "dark" | "light";
import type { UiLanguage } from "../i18n";
import AgentAvatar, { buildSpriteMap } from "./AgentAvatar";
import { buildOfficeScene } from "./office-view/buildScene";
import type { Notification } from "./NotificationCenter";
import { MOBILE_LAYOUT_MEDIA_QUERY } from "../app/breakpoints";
import type {
  AnimItem,
  BreakAnimItem,
  BuildOfficeSceneContext,
  CallbackSnapshot,
  DataSnapshot,
  SubCloneAnimItem,
} from "./office-view/buildScene-types";
import type {
  Delivery,
  RoomRect,
  SubCloneBurstParticle,
  WallClockVisual,
} from "./office-view/model";
import type { OfficeTickerContext } from "./office-view/officeTicker";
import { useOfficePixiRuntime } from "./office-view/useOfficePixiRuntime";
import type { SupportedLocale } from "./office-view/themes-locale";
import {
  deriveOfficeAgentState,
  type OfficeManualIntervention,
  type OfficeSeatStatus,
} from "./office-view/officeAgentState";
import { MobileAgentStatusGrid, OfficeDesktopAgentAccessList, getSeatStatusMeta } from "./office-view/OfficeViewSupport";
import { OfficeManualWarningOverlay } from "./office-view/OfficeManualWarningOverlay";
import { computeMeetingPresence, formatOfficeClock } from "./office-view/OfficeViewModel";
import { SurfaceCard, SurfaceMetricPill, SurfaceNotice, SurfaceSection } from "./common/SurfacePrimitives";

interface OfficeViewProps {
  agents: Agent[];
  departments: Department[];
  language: UiLanguage;
  theme: ThemeMode;
  subAgents?: SubAgent[];
  notifications?: Notification[];
  auditLogs?: AuditLogEntry[];
  activeMeeting?: RoundTableMeeting | null;
  kanbanCards?: KanbanCard[];
  onNavigateToKanban?: () => void;
  onSelectAgent?: (agent: Agent) => void;
  onSelectDepartment?: (dept: Department) => void;
  customDeptThemes?: Record<string, { floor1: number; floor2: number; wall: number; accent: number }>;
}

const EMPTY_TASKS: Task[] = [];
const EMPTY_SUB_AGENTS: SubAgent[] = [];
const EMPTY_NOTIFICATIONS: Notification[] = [];
const EMPTY_AUDIT_LOGS: AuditLogEntry[] = [];

export default function OfficeView({
  agents,
  departments,
  language,
  theme,
  subAgents = EMPTY_SUB_AGENTS,
  notifications = EMPTY_NOTIFICATIONS,
  auditLogs = EMPTY_AUDIT_LOGS,
  activeMeeting = null,
  kanbanCards,
  onNavigateToKanban,
  onSelectAgent,
  onSelectDepartment,
  customDeptThemes,
}: OfficeViewProps) {
  const [isMobileLite, setIsMobileLite] = useState(() => {
    if (typeof window === "undefined") return false;
    return window.matchMedia(MOBILE_LAYOUT_MEDIA_QUERY).matches;
  });
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const sceneSectionRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    const media = window.matchMedia(MOBILE_LAYOUT_MEDIA_QUERY);
    const sync = () => setIsMobileLite(media.matches);
    sync();
    media.addEventListener("change", sync);
    return () => media.removeEventListener("change", sync);
  }, []);

  // ── Refs for BuildOfficeSceneContext ──
  const containerRef = useRef<HTMLDivElement | null>(null);
  const appRef = useRef<Application | null>(null);
  const texturesRef = useRef<Record<string, Texture>>({});
  const destroyedRef = useRef(false);
  const initIdRef = useRef(0);
  const initDoneRef = useRef(false);
  const officeWRef = useRef(0);
  const scrollHostXRef = useRef<HTMLElement | null>(null);
  const scrollHostYRef = useRef<HTMLElement | null>(null);
  const deliveriesRef = useRef<Delivery[]>([]);
  const animItemsRef = useRef<AnimItem[]>([]);
  const roomRectsRef = useRef<RoomRect[]>([]);
  const deliveryLayerRef = useRef<Container | null>(null);
  const prevAssignRef = useRef<Set<string>>(new Set());
  const agentPosRef = useRef<Map<string, { x: number; y: number }>>(new Map());
  const spriteMapRef = useRef<Map<string, number>>(new Map());
  const ceoMeetingSeatsRef = useRef<Array<{ x: number; y: number }>>([]);
  const totalHRef = useRef(0);
  const ceoPosRef = useRef({ x: 200, y: 16 });
  const ceoSpriteRef = useRef<Container | null>(null);
  const crownRef = useRef<Text | null>(null);
  const highlightRef = useRef<Graphics | null>(null);
  const ceoOfficeRectRef = useRef<{ x: number; y: number; w: number; h: number } | null>(null);
  const breakRoomRectRef = useRef<{ x: number; y: number; w: number; h: number } | null>(null);
  const breakAnimItemsRef = useRef<BreakAnimItem[]>([]);
  const subCloneAnimItemsRef = useRef<SubCloneAnimItem[]>([]);
  const subCloneBurstParticlesRef = useRef<SubCloneBurstParticle[]>([]);
  const subCloneSnapshotRef = useRef<Map<string, { parentAgentId: string; x: number; y: number }>>(new Map());
  const breakSteamParticlesRef = useRef<Container | null>(null);
  const breakBubblesRef = useRef<Container[]>([]);
  const wallClocksRef = useRef<WallClockVisual[]>([]);
  const wallClockSecondRef = useRef(-1);
  const keysRef = useRef<Record<string, boolean>>({});
  const tickRef = useRef(0);
  const themeHighlightTargetIdRef = useRef<string | null>(null);
  const cliUsageRef = useRef<Record<string, { windows?: Array<{ utilization: number }> }> | null>(null);
  const eventBubbleQueueRef = useRef<Array<{ agentId: string; text: string; emoji: string; createdAt: number }>>([]);
  const eventBubblesRef = useRef<Array<{ container: Container; createdAt: number; duration: number; baseY: number }>>([]);

  // Data snapshot refs
  const localeRef = useRef<SupportedLocale>(language);
  localeRef.current = language;
  const themeRef = useRef<ThemeMode>(theme);
  themeRef.current = theme;
  const activeMeetingTaskIdRef = useRef<string | null>(null);
  const meetingMinutesOpenRef = useRef<((taskId: string) => void) | undefined>(undefined);

  const meetingPresence = computeMeetingPresence(activeMeeting, agents);

  const dataRef = useRef<DataSnapshot>({
    departments,
    agents,
    tasks: EMPTY_TASKS,
    subAgents,
    customDeptThemes,
    activeMeeting,
    meetingPresence,
  });
  const officeAgentState = useMemo(
    () => deriveOfficeAgentState(agents, kanbanCards),
    [agents, kanbanCards],
  );
  const {
    activeIssueByAgent: officeActiveIssueByAgent,
    manualInterventionByAgent,
    primaryCardByAgent,
    seatStatusByAgent,
  } = officeAgentState;
  const activeIssueByAgent = useMemo(() => {
    const map = new Map<string, { number: number; url: string; startedAt?: number; title?: string }>();
    for (const [agentId, issue] of officeActiveIssueByAgent) {
      if (issue.number == null || !issue.url) continue;
      map.set(agentId, {
        number: issue.number,
        url: issue.url,
        startedAt: issue.startedAt ?? undefined,
        title: issue.title,
      });
    }
    return map;
  }, [officeActiveIssueByAgent]);
  const blockedAgentIds = useMemo(
    () => new Set(manualInterventionByAgent.keys()),
    [manualInterventionByAgent],
  );
  const selectedAgent = useMemo(
    () => agents.find((agent) => agent.id === selectedAgentId) ?? null,
    [agents, selectedAgentId],
  );
  dataRef.current = {
    departments,
    agents,
    tasks: EMPTY_TASKS,
    subAgents,
    customDeptThemes,
    activeMeeting,
    meetingPresence,
    activeIssueByAgent,
    blockedAgentIds,
  };

  useEffect(() => {
    if (!selectedAgentId) return;
    if (!agents.some((agent) => agent.id === selectedAgentId)) {
      setSelectedAgentId(null);
    }
  }, [agents, selectedAgentId]);

  const handleSelectAgent = useCallback((agent: Agent) => {
    setSelectedAgentId(agent.id);
    onSelectAgent?.(agent);
  }, [onSelectAgent]);


  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent).detail;
      if (!detail?.type) return;
      const payload = detail.payload as Record<string, unknown> | undefined;
      if (!payload) return;

      let agentId: string | undefined;
      let text = "";
      let emoji = "";

      switch (detail.type) {
        case "kanban_card_updated": {
          agentId = payload.assignee_agent_id as string | undefined;
          const title = (payload.title as string) ?? "";
          text = title.length > 18 ? `${title.slice(0, 18)}…` : title;
          emoji = "📋";
          break;
        }
        case "task_dispatch_created":
        case "task_dispatch_updated": {
          agentId = payload.to_agent_id as string | undefined;
          const title = (payload.title as string) ?? "";
          text = title.length > 18 ? `${title.slice(0, 18)}…` : title;
          emoji = "📨";
          break;
        }
        case "agent_status": {
          agentId = payload.id as string | undefined;
          const status = payload.status as string;
          text = status;
          emoji = status === "working" ? "💼" : status === "idle" ? "☕" : "💤";
          break;
        }
        default:
          return;
      }

      if (!agentId) return;
      if (eventBubbleQueueRef.current.length >= 20) return;
      eventBubbleQueueRef.current.push({ agentId, text, emoji, createdAt: Date.now() });
    };

    window.addEventListener("pcd-ws-event", handler as EventListener);
    return () => window.removeEventListener("pcd-ws-event", handler as EventListener);
  }, []);

  const cbRef = useRef<CallbackSnapshot>({
    onSelectAgent: handleSelectAgent,
    onSelectDepartment: onSelectDepartment ?? (() => {}),
  });
  cbRef.current = {
    onSelectAgent: handleSelectAgent,
    onSelectDepartment: onSelectDepartment ?? (() => {}),
  };

  // ── Scene revision state (triggers re-render after scene build) ──
  const [sceneRevision, setSceneRevision] = useState(0);

  // ── Build scene context ──
  const sceneContext = useMemo<BuildOfficeSceneContext>(
    () => ({
      appRef,
      texturesRef,
      dataRef,
      cbRef,
      activeMeetingTaskIdRef,
      meetingMinutesOpenRef,
      localeRef,
      themeRef,
      animItemsRef,
      roomRectsRef,
      deliveriesRef,
      deliveryLayerRef,
      eventBubblesRef,
      prevAssignRef,
      agentPosRef,
      spriteMapRef,
      ceoMeetingSeatsRef,
      totalHRef,
      officeWRef,
      ceoPosRef,
      ceoSpriteRef,
      crownRef,
      highlightRef,
      ceoOfficeRectRef,
      breakRoomRectRef,
      breakAnimItemsRef,
      subCloneAnimItemsRef,
      subCloneBurstParticlesRef,
      subCloneSnapshotRef,
      breakSteamParticlesRef,
      breakBubblesRef,
      wallClocksRef,
      wallClockSecondRef,
      setSceneRevision,
    }),
    [],
  );

  const buildScene = useCallback(() => {
    buildOfficeScene(sceneContext);
  }, [sceneContext]);

  const followCeoInView = useCallback(() => {
    const app = appRef.current;
    if (!app) return;
    const canvas = app.canvas as HTMLCanvasElement;
    const hostX = scrollHostXRef.current;
    const hostY = scrollHostYRef.current;
    if (hostX) {
      const screenX = ceoPosRef.current.x - hostX.clientWidth / 2;
      hostX.scrollLeft = Math.max(0, screenX);
    }
    if (hostY) {
      const screenY = ceoPosRef.current.y - hostY.clientHeight / 2;
      hostY.scrollTop = Math.max(0, screenY);
    }
  }, []);

  const triggerDepartmentInteract = useCallback(() => {
    const ceoX = ceoPosRef.current.x;
    const ceoY = ceoPosRef.current.y;
    for (const rect of roomRectsRef.current) {
      if (ceoX >= rect.x && ceoX <= rect.x + rect.w && ceoY >= rect.y - 10 && ceoY <= rect.y + rect.h) {
        cbRef.current.onSelectDepartment(rect.dept);
        return;
      }
    }
  }, []);

  // ── Ticker context ──
  const tickerContext = useMemo<OfficeTickerContext>(
    () => ({
      tickRef,
      keysRef,
      ceoPosRef,
      ceoSpriteRef,
      crownRef,
      highlightRef,
      animItemsRef,
      cliUsageRef,
      roomRectsRef,
      deliveriesRef,
      breakAnimItemsRef,
      subCloneAnimItemsRef,
      subCloneBurstParticlesRef,
      breakSteamParticlesRef,
      breakBubblesRef,
      wallClocksRef,
      wallClockSecondRef,
      themeHighlightTargetIdRef,
      ceoOfficeRectRef,
      breakRoomRectRef,
      officeWRef,
      totalHRef,
      dataRef: dataRef as OfficeTickerContext["dataRef"],
      eventBubbleQueueRef,
      eventBubblesRef,
      deliveryLayerRef,
      agentPosRef,
      followCeoInView,
    }),
    [followCeoInView],
  );

  const [elapsedTick, setElapsedTick] = useState(0);
  useEffect(() => {
    const intervalId = setInterval(() => setElapsedTick((tick) => tick + 1), 60_000);
    return () => clearInterval(intervalId);
  }, []);

  useEffect(() => {
    if (appRef.current && !isMobileLite && initDoneRef.current) buildScene();
  }, [activeIssueByAgent, blockedAgentIds, elapsedTick, buildScene, isMobileLite]);

  // ── Pixi runtime hook ──
  useOfficePixiRuntime({
    containerRef,
    appRef,
    texturesRef,
    destroyedRef,
    initIdRef,
    initDoneRef,
    officeWRef,
    scrollHostXRef,
    scrollHostYRef,
    deliveriesRef,
    dataRef: dataRef as { current: { agents: Agent[] } },
    buildScene,
    followCeoInView,
    triggerDepartmentInteract,
    keysRef,
    tickerContext,
    departments,
    agents,
    tasks: EMPTY_TASKS,
    subAgents,
    language,
    activeMeetingTaskId: null,
    activeMeeting,
    customDeptThemes,
    currentTheme: theme,
    disabled: isMobileLite,
  });

  const isKo = language === "ko";
  const manualWarningEntries = useMemo(
    () =>
      agents
        .map((agent) => {
          const warning = manualInterventionByAgent.get(agent.id);
          const position = agentPosRef.current.get(agent.id);
          if (!warning || !position) return null;
          return { agent, warning, position };
        })
        .filter(
          (
            entry,
          ): entry is { agent: Agent; warning: OfficeManualIntervention; position: { x: number; y: number } } =>
            entry !== null,
        ),
    [agents, manualInterventionByAgent, sceneRevision],
  );
  const workingSeatCount = useMemo(
    () => Array.from(seatStatusByAgent.values()).filter((status) => status === "working").length,
    [seatStatusByAgent],
  );
  const reviewSeatCount = useMemo(
    () => Array.from(seatStatusByAgent.values()).filter((status) => status === "review").length,
    [seatStatusByAgent],
  );
  const manualCount = manualInterventionByAgent.size;
  const liveClockLabel = useMemo(
    () => formatOfficeClock(language),
    [elapsedTick, language],
  );
  const selectedAgentLabel = selectedAgent?.alias || selectedAgent?.name_ko || selectedAgent?.name;
  const officeSceneLabel = isKo
    ? `오피스 공간 보기. ${departments.length}개 부서, ${agents.length}명 에이전트, 작업 중 ${workingSeatCount}명, 리뷰 ${reviewSeatCount}명, 수동 개입 ${manualCount}건.`
    : `Spatial office view. ${departments.length} departments, ${agents.length} agents, ${workingSeatCount} working, ${reviewSeatCount} in review, ${manualCount} manual interventions.`;
  const officeStatusSummary = useMemo(
    () => {
      if (agents.length === 0) {
        return isKo ? "표시할 에이전트가 없습니다." : "No agents to display.";
      }
      return agents
        .map((agent) => {
          const agentLabel = agent.name_ko || agent.name || agent.alias || agent.role_id;
          const status = getSeatStatusMeta(
            seatStatusByAgent.get(agent.id) ?? "idle",
            isKo,
          ).label;
          const warning = manualInterventionByAgent.has(agent.id)
            ? (isKo ? "수동 개입 필요" : "manual intervention needed")
            : null;
          const card = primaryCardByAgent.get(agent.id);
          const cardLabel = card?.github_issue_number
            ? `#${card.github_issue_number}`
            : card?.title ?? null;
          return [agentLabel, status, warning, cardLabel].filter(Boolean).join(" · ");
        })
        .join(". ");
    },
    [agents, isKo, manualInterventionByAgent, primaryCardByAgent, seatStatusByAgent],
  );

  return (
    <div
      className="mx-auto h-full w-full max-w-6xl min-w-0 overflow-x-hidden overflow-y-auto p-4 pb-40 sm:p-6"
      style={{ paddingBottom: "max(10rem, calc(10rem + env(safe-area-inset-bottom)))" }}
    >
      <div className="flex flex-col gap-4">
        <div className="flex flex-col gap-3 lg:flex-row lg:items-center lg:justify-between">
          <div className="min-w-0">
            <h2 className="text-[1.65rem] font-semibold tracking-tight" style={{ color: "var(--th-text)" }}>
              {isKo ? "오피스" : "Office"}
            </h2>
            <p className="mt-1 text-sm leading-6" style={{ color: "var(--th-text-muted)" }}>
              {isKo
                ? "에이전트가 지금 하고 있는 일을 공간적으로 확인합니다."
                : "See what agents are working on in a spatial office view."}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <span
              className="inline-flex items-center gap-2 rounded-full border px-3 py-1.5 text-[11px] font-medium"
              style={{
                borderColor: "color-mix(in srgb, var(--th-accent-primary) 24%, var(--th-border) 76%)",
                background: "color-mix(in srgb, var(--th-badge-emerald-bg) 54%, var(--th-card-bg) 46%)",
                color: "var(--th-text-primary)",
              }}
            >
              <span className="h-2 w-2 rounded-full" style={{ background: "var(--th-accent-primary)" }} />
              {`live · ${liveClockLabel}`}
            </span>
          </div>
        </div>

        {/* Office tab now shows only the spatial scene — the right-rail
            "오피스 운영 신호" insight panel was duplicating signals already
            available on /home and /kanban, and the dense rail compressed
            the canvas on smaller desktops. */}
        <div className="grid min-w-0 gap-4">
          <div ref={sceneSectionRef} className="min-w-0">
            {isMobileLite ? (
              /* User feedback (#1273 follow-up): the gray SurfaceCard +
                 desktop legend bar around the mobile agent grid added
                 visible chrome that didn't carry information on mobile.
                 Render the grid flat on the page background instead, so
                 the office tab is "그냥 오피스 뷰만" — no gray empty
                 space, no canvas legend. */
              <MobileAgentStatusGrid
                agents={agents}
                isKo={isKo}
                onSelectAgent={handleSelectAgent}
                manualInterventionByAgent={manualInterventionByAgent}
                primaryCardByAgent={primaryCardByAgent}
                seatStatusByAgent={seatStatusByAgent}
              />
            ) : (
              <SurfaceCard
                className="overflow-hidden rounded-[28px] p-0"
                style={{
                  borderColor: "color-mix(in srgb, var(--th-border) 64%, transparent)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 72%, #0d1420 28%) 0%, color-mix(in srgb, var(--th-bg-surface) 82%, #090d16 18%) 100%)",
                }}
              >
                <div className="relative min-h-[28.75rem] overflow-hidden">
                  <p id="office-scene-status-summary" className="sr-only">
                    {officeStatusSummary}
                  </p>
                  <div
                    ref={containerRef}
                    role="img"
                    aria-label={officeSceneLabel}
                    aria-describedby="office-scene-status-summary"
                    className="min-h-[28.75rem] w-full"
                    style={{ imageRendering: "pixelated" }}
                  />
                  <OfficeManualWarningOverlay
                    entries={manualWarningEntries}
                    isKo={isKo}
                    onSelectAgent={handleSelectAgent}
                  />
                </div>
                <div
                  className="flex flex-wrap items-center gap-4 border-t px-4 py-3 text-[11px]"
                  style={{
                    borderColor: "color-mix(in srgb, var(--th-border) 62%, transparent)",
                    color: "var(--th-text-muted)",
                  }}
                >
                  <span className="inline-flex items-center gap-2">
                    <span className="inline-block h-2 w-2 rounded-full" style={{ background: "var(--th-accent-primary)" }} />
                    {isKo ? "작업 중" : "Working"}
                  </span>
                  <span className="inline-flex items-center gap-2">
                    <span className="inline-block h-2 w-2 rounded-full" style={{ background: "var(--th-accent-warn)" }} />
                    {isKo ? "리뷰" : "Review"}
                  </span>
                  <span className="inline-flex items-center gap-2">
                    <span className="inline-block h-2 w-2 rounded-full" style={{ background: "var(--th-text-muted)" }} />
                    {isKo ? "대기" : "Idle"}
                  </span>
                  {activeMeeting ? (
                    <span className="inline-flex items-center gap-2">
                      <span className="inline-block h-2 w-2 rounded-full" style={{ background: "var(--th-accent-info)" }} />
                      {isKo ? `회의 · ${activeMeeting.agenda}` : `Meeting · ${activeMeeting.agenda}`}
                    </span>
                  ) : null}
                  <span className="ml-auto text-right" style={{ color: "var(--th-text-faint)" }}>
                    {selectedAgentLabel
                      ? (isKo ? `${selectedAgentLabel} 상세 보기` : `${selectedAgentLabel} selected`)
                      : (isKo ? "클릭해서 상세 보기" : "Click to inspect details")}
                  </span>
                </div>
                <OfficeDesktopAgentAccessList
                  agents={agents}
                  isKo={isKo}
                  manualInterventionByAgent={manualInterventionByAgent}
                  onSelectAgent={handleSelectAgent}
                  primaryCardByAgent={primaryCardByAgent}
                  seatStatusByAgent={seatStatusByAgent}
                />
              </SurfaceCard>
            )}
          </div>

        </div>
      </div>
    </div>
  );
}
