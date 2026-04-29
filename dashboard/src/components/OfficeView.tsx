import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
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
import {
  SurfaceActionButton,
  SurfaceCard,
  SurfaceMetricPill,
  SurfaceNotice,
  SurfaceSection,
  SurfaceSubsection,
} from "./common/SurfacePrimitives";

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

function inferDisplayNameLocal(roleId: string): string {
  if (roleId.startsWith("ch-")) return roleId.slice(3).toUpperCase();
  if (roleId.endsWith("-agent")) return roleId.replace(/-agent$/, "");
  return roleId;
}

function matchParticipantToAgentId(name: string, agents: Agent[]): string | null {
  const lower = name.toLowerCase();
  const abbrev = lower.replace(/\s*\(.*$/, "").trim();
  for (const agent of agents) {
    if (agent.role_id) {
      const dn = inferDisplayNameLocal(agent.role_id).toLowerCase();
      if (dn === lower || dn === abbrev) return agent.id;
    }
    const n = agent.name.toLowerCase();
    if (n === lower || n === abbrev) return agent.id;
    const nk = agent.name_ko?.toLowerCase();
    if (nk && (nk === lower || nk === abbrev)) return agent.id;
    const al = agent.alias?.toLowerCase();
    if (al && (al === lower || al === abbrev)) return agent.id;
  }
  return null;
}

function computeMeetingPresence(
  meeting: RoundTableMeeting | null | undefined,
  agents: Agent[],
): Array<{ agent_id: string; until: number }> | undefined {
  if (!meeting || meeting.status !== "in_progress") return undefined;
  const names = meeting.participant_names ?? [];
  if (names.length === 0) return undefined;
  const until = Date.now() + 60 * 60 * 1000; // 1hr future (refreshed every render)
  const result: Array<{ agent_id: string; until: number }> = [];
  for (const name of names) {
    const agentId = matchParticipantToAgentId(name, agents);
    if (agentId) result.push({ agent_id: agentId, until });
  }
  return result.length > 0 ? result : undefined;
}

function formatOfficeClock(language: UiLanguage): string {
  const locale = language === "ko" ? "ko-KR" : "en-US";
  return new Intl.DateTimeFormat(locale, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(new Date());
}

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
                  <div ref={containerRef} className="min-h-[28.75rem] w-full" style={{ imageRendering: "pixelated" }} />
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
              </SurfaceCard>
            )}
          </div>

        </div>
      </div>
    </div>
  );
}

// ── Mobile Office Lite: agent status cards ──

function getSeatStatusMeta(
  status: OfficeSeatStatus,
  isKo: boolean,
): { label: string; accent: string; textColor: string; background: string; border: string } {
  switch (status) {
    case "working":
      return {
        label: isKo ? "작업 중" : "Working",
        accent: "var(--th-accent-primary)",
        textColor: "var(--th-text-primary)",
        background: "color-mix(in srgb, var(--th-badge-emerald-bg) 62%, var(--th-card-bg) 38%)",
        border: "color-mix(in srgb, var(--th-accent-primary) 22%, var(--th-border) 78%)",
      };
    case "review":
      return {
        label: isKo ? "검토 중" : "In review",
        accent: "var(--th-accent-warn)",
        textColor: "var(--th-accent-warn)",
        background: "color-mix(in srgb, var(--th-badge-amber-bg) 62%, var(--th-card-bg) 38%)",
        border: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
      };
    case "offline":
      return {
        label: isKo ? "오프라인" : "Offline",
        accent: "var(--th-text-muted)",
        textColor: "var(--th-text-muted)",
        background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
        border: "color-mix(in srgb, var(--th-border) 72%, transparent)",
      };
    case "idle":
    default:
      return {
        label: isKo ? "대기" : "Idle",
        accent: "var(--th-text-muted)",
        textColor: "var(--th-text-muted)",
        background: "color-mix(in srgb, var(--th-bg-surface) 94%, transparent)",
        border: "color-mix(in srgb, var(--th-border) 72%, transparent)",
      };
  }
}

function previewManualReason(reason: string | null | undefined): string {
  if (!reason) return "";
  return reason.length > 72 ? `${reason.slice(0, 72)}…` : reason;
}

function previewCardTitle(title: string | null | undefined): string {
  if (!title) return "";
  return title.length > 52 ? `${title.slice(0, 52)}…` : title;
}

function OfficeManualWarningOverlay({
  entries,
  isKo,
  onSelectAgent,
}: {
  entries: Array<{ agent: Agent; warning: OfficeManualIntervention; position: { x: number; y: number } }>;
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
}) {
  const [hoveredWarningId, setHoveredWarningId] = useState<string | null>(null);
  const [expandedWarningId, setExpandedWarningId] = useState<string | null>(null);
  const buttonRefs = useRef<Map<string, HTMLButtonElement | null>>(new Map());

  useEffect(() => {
    if (!expandedWarningId) return;
    if (!entries.some(({ warning }) => warning.cardId === expandedWarningId)) {
      setExpandedWarningId(null);
    }
  }, [entries, expandedWarningId]);

  /* Active overlay = the one being expanded (click) or hovered. We render
     exactly one tooltip via React portal to document.body so it can:
       1) escape the canvas's overflow:hidden / transform-containing
          ancestor (without portal the tooltip clips at the canvas edge);
       2) flip placement based on actual viewport space rather than a
          fixed `bottom-[calc(100%+...)]` that bleeds off the canvas top
          for warnings near the top row. */
  const activeId = expandedWarningId ?? hoveredWarningId;
  const activeEntry = useMemo(
    () => entries.find(({ warning }) => warning.cardId === activeId) ?? null,
    [entries, activeId],
  );
  const [tooltipPos, setTooltipPos] = useState<
    { top: number; left: number; placement: "top" | "bottom" } | null
  >(null);

  useLayoutEffect(() => {
    if (!activeEntry) {
      setTooltipPos(null);
      return;
    }
    const update = () => {
      const button = buttonRefs.current.get(activeEntry.warning.cardId);
      if (!button) return;
      const rect = button.getBoundingClientRect();
      const viewportW = window.innerWidth;
      const viewportH = window.innerHeight;
      const padding = 12;
      const tooltipW = Math.min(304, viewportW - padding * 2);
      // Estimated tooltip height — used only to pick top vs bottom; the
      // actual content auto-sizes via flex column, so we just need a
      // reasonable budget for the "is there enough room above" check.
      const tooltipBudgetH = 220;
      const spaceAbove = rect.top;
      const spaceBelow = viewportH - rect.bottom;
      const placement: "top" | "bottom" =
        spaceAbove >= tooltipBudgetH || spaceAbove >= spaceBelow ? "top" : "bottom";
      const centerX = rect.left + rect.width / 2;
      const left = Math.max(
        padding,
        Math.min(viewportW - tooltipW - padding, centerX - tooltipW / 2),
      );
      const top =
        placement === "top" ? Math.max(padding, rect.top - 12) : rect.bottom + 12;
      setTooltipPos({ top, left, placement });
    };
    update();
    const handle = () => update();
    window.addEventListener("scroll", handle, true);
    window.addEventListener("resize", handle);
    return () => {
      window.removeEventListener("scroll", handle, true);
      window.removeEventListener("resize", handle);
    };
  }, [activeEntry]);

  if (entries.length === 0) return null;

  const portalTarget = typeof document !== "undefined" ? document.body : null;
  const tooltipWidth = (() => {
    if (typeof window === "undefined") return 304;
    return Math.min(304, window.innerWidth - 24);
  })();

  return (
    <>
      <div className="pointer-events-none absolute inset-0 z-10">
        {entries.map(({ agent, warning, position }) => {
          const isOpen = activeId === warning.cardId;
          const agentLabel = agent.alias || agent.name_ko || agent.name;
          return (
            <div
              key={warning.cardId}
              className="absolute pointer-events-auto"
              style={{
                left: position.x + 16,
                top: position.y - 28,
                transform: "translate(-50%, -50%)",
              }}
              onMouseEnter={() => setHoveredWarningId(warning.cardId)}
              onMouseLeave={() =>
                setHoveredWarningId((current) =>
                  current === warning.cardId ? null : current,
                )
              }
              onFocusCapture={() => setHoveredWarningId(warning.cardId)}
              onBlurCapture={(event) => {
                if (event.currentTarget.contains(event.relatedTarget as Node | null)) return;
                setHoveredWarningId((current) =>
                  current === warning.cardId ? null : current,
                );
              }}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setExpandedWarningId((current) =>
                    current === warning.cardId ? null : current,
                  );
                  setHoveredWarningId((current) =>
                    current === warning.cardId ? null : current,
                  );
                }
              }}
            >
              {/* Compact "!" pill styled to match the in-canvas Pixi
                  nameplate (white 85% rounded-rect with subtle border)
                  so the warning tag reads as part of the same visual
                  language. The yellow glyph is the only colored
                  element, replacing the previous "● 수동" amber chip. */}
              <button
                ref={(el) => {
                  if (el) buttonRefs.current.set(warning.cardId, el);
                  else buttonRefs.current.delete(warning.cardId);
                }}
                type="button"
                className="relative inline-flex h-7 w-7 items-center justify-center rounded-full border transition-colors focus:outline-none focus:ring-2"
                style={{
                  borderColor: "color-mix(in srgb, white 38%, var(--th-border) 62%)",
                  background:
                    "color-mix(in srgb, white 85%, var(--th-bg-surface) 15%)",
                  boxShadow: "0 1px 0 rgba(0,0,0,0.18)",
                }}
                aria-label={
                  isKo
                    ? `${agentLabel} 수동 개입 경고`
                    : `${agentLabel} manual intervention warning`
                }
                aria-expanded={isOpen}
                onClick={() =>
                  setExpandedWarningId((current) =>
                    current === warning.cardId ? null : warning.cardId,
                  )
                }
              >
                <span
                  aria-hidden="true"
                  className="absolute -bottom-[3px] left-1/2 h-2 w-2 -translate-x-1/2 rotate-45 border-b border-r"
                  style={{
                    borderColor: "color-mix(in srgb, white 38%, var(--th-border) 62%)",
                    background:
                      "color-mix(in srgb, white 85%, var(--th-bg-surface) 15%)",
                  }}
                />
                <span
                  className="text-[15px] font-bold leading-none"
                  style={{ color: "var(--th-accent-warn)" }}
                >
                  !
                </span>
              </button>
            </div>
          );
        })}
      </div>
      {activeEntry && tooltipPos && portalTarget
        ? createPortal(
            <div
              className="pointer-events-auto"
              style={{
                position: "fixed",
                left: tooltipPos.left,
                top: tooltipPos.placement === "bottom" ? tooltipPos.top : "auto",
                bottom:
                  tooltipPos.placement === "top"
                    ? `${
                        typeof window !== "undefined"
                          ? window.innerHeight - tooltipPos.top
                          : 0
                      }px`
                    : "auto",
                width: tooltipWidth,
                maxWidth: "calc(100vw - 1.5rem)",
                zIndex: 60,
              }}
              onMouseEnter={() => setHoveredWarningId(activeEntry.warning.cardId)}
              onMouseLeave={() =>
                setHoveredWarningId((current) =>
                  current === activeEntry.warning.cardId ? null : current,
                )
              }
            >
              <SurfaceSubsection
                title={activeEntry.warning.title}
                description={
                  isKo
                    ? `${
                        activeEntry.agent.alias ||
                        activeEntry.agent.name_ko ||
                        activeEntry.agent.name
                      }에게 연결된 카드에서 수동 개입이 필요합니다.`
                    : `Manual intervention is required for the card assigned to ${
                        activeEntry.agent.alias ||
                        activeEntry.agent.name_ko ||
                        activeEntry.agent.name
                      }.`
                }
                actions={
                  <SurfaceActionButton
                    tone="warn"
                    compact
                    className="pointer-events-auto rounded-full"
                    onClick={() => onSelectAgent?.(activeEntry.agent)}
                  >
                    {isKo ? "세부 보기" : "Open detail"}
                  </SurfaceActionButton>
                }
                className="rounded-[24px] p-3 sm:p-3"
                style={{
                  borderColor:
                    "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
                  background:
                    "linear-gradient(180deg, color-mix(in srgb, var(--th-badge-amber-bg) 52%, var(--th-card-bg) 48%) 0%, color-mix(in srgb, var(--th-card-bg) 92%, transparent) 100%)",
                  boxShadow:
                    "0 14px 32px -18px color-mix(in srgb, black 70%, transparent)",
                }}
              >
                <div className="space-y-2">
                  <div className="flex flex-wrap items-center justify-between gap-2">
                    <span
                      className="inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase tracking-[0.14em]"
                      style={{
                        color: "var(--th-accent-warn)",
                        borderColor:
                          "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
                        background:
                          "color-mix(in srgb, var(--th-badge-amber-bg) 68%, var(--th-card-bg) 32%)",
                      }}
                    >
                      {isKo ? "수동 개입" : "Manual intervention"}
                    </span>
                    <span
                      className="text-[11px]"
                      style={{ color: "var(--th-text-muted)" }}
                    >
                      {activeEntry.warning.issueNumber
                        ? `#${activeEntry.warning.issueNumber}`
                        : activeEntry.warning.status}
                    </span>
                  </div>
                  <SurfaceNotice tone="warn" compact className="items-start rounded-[18px]">
                    <div className="text-[11px] leading-5">
                      {isKo
                        ? "카드 상세에서 원인과 후속 조치를 확인할 수 있습니다."
                        : "Open the card detail to inspect the cause and next action."}
                    </div>
                  </SurfaceNotice>
                </div>
                {/* Body text uses normal whitespace handling so words wrap
                    on word boundaries; `break-words` keeps long
                    unbreakable tokens (URLs, IDs) from overflowing. The
                    previous `whitespace-pre-wrap` combined with the
                    overflow-clipped placement made the body collapse
                    into 1-character lines on warnings near the canvas
                    top. */}
                <div
                  className="mt-3 break-words text-xs leading-5"
                  style={{ color: "var(--th-text-muted)" }}
                >
                  {activeEntry.warning.reason ??
                    (isKo
                      ? "구체 사유는 카드 상세에서 확인할 수 있습니다."
                      : "Open the detail drawer to inspect the full reason.")}
                </div>
              </SurfaceSubsection>
            </div>,
            portalTarget,
          )
        : null}
    </>
  );
}

function MobileAgentStatusGrid({
  agents,
  isKo,
  onSelectAgent,
  manualInterventionByAgent,
  primaryCardByAgent,
  seatStatusByAgent,
}: {
  agents: Agent[];
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
  manualInterventionByAgent: Map<string, OfficeManualIntervention>;
  primaryCardByAgent: Map<string, KanbanCard>;
  seatStatusByAgent: Map<string, OfficeSeatStatus>;
}) {
  const sorted = [...agents].sort((a, b) => {
    const leftManual = manualInterventionByAgent.has(a.id) ? 0 : 1;
    const rightManual = manualInterventionByAgent.has(b.id) ? 0 : 1;
    if (leftManual !== rightManual) return leftManual - rightManual;

    const order: Record<OfficeSeatStatus, number> = {
      review: 0,
      working: 1,
      idle: 2,
      offline: 3,
    };
    const leftStatus = seatStatusByAgent.get(a.id) ?? "idle";
    const rightStatus = seatStatusByAgent.get(b.id) ?? "idle";
    const statusDiff = (order[leftStatus] ?? 9) - (order[rightStatus] ?? 9);
    if (statusDiff !== 0) return statusDiff;
    return (a.alias || a.name_ko || a.name).localeCompare(b.alias || b.name_ko || b.name);
  });

  const [expandedWarningAgentId, setExpandedWarningAgentId] = useState<string | null>(null);
  const manualCount = sorted.reduce(
    (count, agent) => count + (manualInterventionByAgent.has(agent.id) ? 1 : 0),
    0,
  );

  useEffect(() => {
    if (!expandedWarningAgentId) return;
    if (!manualInterventionByAgent.has(expandedWarningAgentId)) {
      setExpandedWarningAgentId(null);
    }
  }, [expandedWarningAgentId, manualInterventionByAgent]);

  return (
    /* User feedback: drop the gray SurfaceSubsection wrapper that
       previously wrapped this grid (title "에이전트 현황" + description
       + bordered card background). On mobile we now render the agent
       count + warning chip inline above the grid and let the grid sit
       directly on the page background. The redundant subtitle
       ("수동 개입, 좌석 상태, ... 카드로 빠르게 확인합니다.") is dropped
       since the cards themselves are the explanation. */
    <div className="mt-3 px-3 pb-6">
      <div className="flex flex-wrap items-center justify-between gap-2 px-1 pb-3">
        <span
          className="text-[11px] font-semibold uppercase tracking-[0.14em]"
          style={{ color: "var(--th-text-muted)" }}
        >
          {isKo ? `${sorted.length}명` : `${sorted.length} agents`}
        </span>
        {manualCount > 0 && (
          <span
            className="inline-flex items-center rounded-full border px-2.5 py-1 text-[10px] font-semibold uppercase tracking-[0.14em]"
            style={{
              borderColor: "color-mix(in srgb, var(--th-accent-warn) 24%, var(--th-border) 76%)",
              background: "color-mix(in srgb, var(--th-badge-amber-bg) 60%, var(--th-card-bg) 40%)",
              color: "var(--th-accent-warn)",
            }}
          >
            {isKo ? `경고 ${manualCount}` : `Warnings ${manualCount}`}
          </span>
        )}
      </div>
      {manualCount > 0 && (
        <SurfaceNotice tone="warn" compact className="mb-3">
          <div className="text-[11px] leading-5">
            {isKo
              ? `수동 개입이 필요한 에이전트 ${manualCount}명이 상단으로 정렬되어 있습니다.`
              : `${manualCount} agents with manual intervention are pinned to the top.`}
          </div>
        </SurfaceNotice>
      )}
        <div className="mt-4 grid grid-cols-1 gap-2.5 min-[520px]:grid-cols-2">
        {sorted.map((agent) => {
          const status = seatStatusByAgent.get(agent.id) ?? "idle";
          const statusMeta = getSeatStatusMeta(status, isKo);
          const manualIntervention = manualInterventionByAgent.get(agent.id) ?? null;
          const primaryCard = primaryCardByAgent.get(agent.id) ?? null;
          const agentLabel = agent.alias || agent.name_ko || agent.name;
          const sessionLabel =
            agent.session_info && agent.session_info !== statusMeta.label ? agent.session_info : null;
          const preview = manualIntervention?.reason
            ? previewManualReason(manualIntervention.reason)
            : previewCardTitle(primaryCard?.title ?? null);
          const isWarningExpanded = expandedWarningAgentId === agent.id;

          return (
            <SurfaceCard
              key={agent.id}
              className="rounded-[26px] px-3.5 py-3.5 text-left"
              style={{
                background: manualIntervention
                  ? "linear-gradient(180deg, color-mix(in srgb, var(--th-badge-amber-bg) 54%, var(--th-card-bg) 46%) 0%, color-mix(in srgb, var(--th-card-bg) 90%, transparent) 100%)"
                  : "linear-gradient(180deg, color-mix(in srgb, var(--th-card-bg) 94%, transparent) 0%, color-mix(in srgb, var(--th-bg-surface) 95%, transparent) 100%)",
                borderColor: manualIntervention
                  ? "color-mix(in srgb, var(--th-accent-warn) 26%, var(--th-border) 74%)"
                  : "color-mix(in srgb, var(--th-border) 68%, transparent)",
              }}
            >
              <button type="button" onClick={() => onSelectAgent?.(agent)} className="w-full text-left">
                <div className="flex items-start justify-between gap-2">
                  <div className="flex min-w-0 items-start gap-2.5">
                    <span
                      className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-2xl border"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                        background: "color-mix(in srgb, var(--th-bg-surface) 92%, transparent)",
                      }}
                    >
                      <AgentAvatar agent={agent} agents={agents} size={28} rounded="2xl" />
                    </span>
                    <div className="min-w-0">
                      <div className="truncate text-sm font-semibold" style={{ color: "var(--th-text)" }}>
                        {agentLabel}
                      </div>
                      <div className="mt-1 flex flex-wrap items-center gap-1.5">
                        <span
                          className="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium"
                          style={{
                            color: statusMeta.textColor,
                            background: statusMeta.background,
                            border: `1px solid ${statusMeta.border}`,
                          }}
                        >
                          <span
                            className="h-1.5 w-1.5 rounded-full"
                            style={{ background: statusMeta.accent }}
                          />
                          {statusMeta.label}
                        </span>
                        {sessionLabel && (
                          <span
                            className="inline-flex items-center rounded-full border px-2 py-0.5 text-[10px] font-medium"
                            style={{
                              borderColor: "color-mix(in srgb, var(--th-border) 68%, transparent)",
                              background: "color-mix(in srgb, var(--th-bg-surface) 90%, transparent)",
                              color: "var(--th-text-muted)",
                            }}
                          >
                            <span className="truncate">{sessionLabel}</span>
                          </span>
                        )}
                      </div>
                    </div>
                  </div>
                  {manualIntervention && (
                    <span
                      className="shrink-0 rounded-full px-2 py-0.5 text-[10px] font-semibold"
                      style={{
                        color: "var(--th-accent-warn)",
                        background: "color-mix(in srgb, var(--th-badge-amber-bg) 72%, var(--th-card-bg) 28%)",
                      }}
                    >
                      {isKo ? "수동 개입" : "Manual"}
                    </span>
                  )}
                </div>
                {preview && (
                  <div className="mt-3 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                    {preview}
                  </div>
                )}
                {agent.department_name_ko && (
                  <div className="mt-2">
                    <span
                      className="inline-flex max-w-full items-center rounded-full px-2 py-0.5 text-[10px] font-medium"
                      style={{
                        color: statusMeta.textColor,
                        background: statusMeta.background,
                        border: `1px solid ${statusMeta.border}`,
                      }}
                    >
                      <span className="truncate">
                        {isKo ? agent.department_name_ko : (agent.department_name || agent.department_name_ko)}
                      </span>
                    </span>
                  </div>
                )}
              </button>
              {manualIntervention && (
                <div className="mt-3">
                  <SurfaceNotice
                    tone="warn"
                    compact
                    className="items-start rounded-[20px]"
                    action={(
                      <SurfaceActionButton
                        tone="warn"
                        compact
                        className="shrink-0 rounded-full"
                        onClick={(event) => {
                          event.stopPropagation();
                          setExpandedWarningAgentId((current) => (current === agent.id ? null : agent.id));
                        }}
                        aria-expanded={isWarningExpanded}
                      >
                      {isWarningExpanded
                        ? (isKo ? "접기" : "Hide")
                        : (isKo ? "사유 보기" : "Show reason")}
                      </SurfaceActionButton>
                    )}
                  >
                    <div className="min-w-0">
                      <div className="text-[10px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--th-accent-warn)" }}>
                        {isKo ? "경고" : "Warning"}
                      </div>
                      <div className="mt-1 text-[11px] font-semibold leading-5" style={{ color: "var(--th-text)" }}>
                        {manualIntervention.title}
                      </div>
                    </div>
                  </SurfaceNotice>
                  {isWarningExpanded && (
                    <SurfaceCard
                      className="mt-2 rounded-[20px] px-3 py-3"
                      style={{
                        borderColor: "color-mix(in srgb, var(--th-accent-warn) 22%, var(--th-border) 78%)",
                        background: "color-mix(in srgb, var(--th-badge-amber-bg) 62%, var(--th-card-bg) 38%)",
                      }}
                    >
                      <div
                        className="break-words whitespace-pre-wrap text-[11px] leading-5"
                        style={{ color: "var(--th-text-muted)" }}
                      >
                        {manualIntervention.reason
                          ?? (isKo
                            ? "구체 사유는 상세 패널에서 확인할 수 있습니다."
                            : "Open the detail panel to inspect the full reason.")}
                      </div>
                    </SurfaceCard>
                  )}
                </div>
              )}
            </SurfaceCard>
          );
        })}
      </div>
    </div>
  );
}
