import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Application, Container, Graphics, Text, Texture } from "pixi.js";
import type { Agent, AuditLogEntry, Department, KanbanCard, RoundTableMeeting, Task, SubAgent } from "../types";
type ThemeMode = "dark" | "light";
import type { UiLanguage } from "../i18n";
import { buildSpriteMap } from "./AgentAvatar";
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
import OfficeInsightPanel from "./office-view/OfficeInsightPanel";
import { useOfficePixiRuntime } from "./office-view/useOfficePixiRuntime";
import type { SupportedLocale } from "./office-view/themes-locale";
import {
  deriveOfficeAgentState,
  type OfficeManualIntervention,
  type OfficeSeatStatus,
} from "./office-view/officeAgentState";

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
    onSelectAgent: onSelectAgent ?? (() => {}),
    onSelectDepartment: onSelectDepartment ?? (() => {}),
  });
  cbRef.current = {
    onSelectAgent: onSelectAgent ?? (() => {}),
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

  return (
    <div className="flex h-full min-h-0 w-full flex-col sm:flex-row sm:gap-3">
      <div className="relative min-h-0 min-w-0 flex-1 overflow-y-auto overflow-x-hidden">
        {/* Mobile: status-only Office Lite */}
        <div className="sm:hidden">
          <OfficeInsightPanel
            agents={agents}
            notifications={notifications}
            auditLogs={auditLogs}
            kanbanCards={kanbanCards}
            onNavigateToKanban={onNavigateToKanban}
            isKo={isKo}
            onSelectAgent={onSelectAgent}
          />
          <MobileAgentStatusGrid
            agents={agents}
            isKo={isKo}
            onSelectAgent={onSelectAgent}
            manualInterventionByAgent={manualInterventionByAgent}
            primaryCardByAgent={primaryCardByAgent}
            seatStatusByAgent={seatStatusByAgent}
          />
        </div>
        {/* Desktop: full Pixi office */}
        <div className="relative hidden w-full min-h-full pb-40 sm:block">
          <div ref={containerRef} className="w-full min-h-full" style={{ imageRendering: "pixelated" }} />
          <OfficeManualWarningOverlay
            entries={manualWarningEntries}
            isKo={isKo}
            onSelectAgent={onSelectAgent}
          />
        </div>
      </div>
      <div className="hidden min-h-0 sm:block sm:h-full sm:w-[min(22rem,calc(100vw-1.5rem))] sm:shrink-0 sm:overflow-y-auto">
        <OfficeInsightPanel
          agents={agents}
          notifications={notifications}
          auditLogs={auditLogs}
          kanbanCards={kanbanCards}
          onNavigateToKanban={onNavigateToKanban}
          isKo={isKo}
          onSelectAgent={onSelectAgent}
          docked
        />
      </div>
    </div>
  );
}

// ── Mobile Office Lite: agent status cards ──

function getSeatStatusMeta(
  status: OfficeSeatStatus,
  isKo: boolean,
): { label: string; color: string; background: string; border: string } {
  switch (status) {
    case "working":
      return {
        label: isKo ? "작업 중" : "Working",
        color: "var(--ok)",
        background: "color-mix(in oklch, var(--ok) 12%, var(--bg-2) 88%)",
        border: "color-mix(in oklch, var(--ok) 28%, var(--line) 72%)",
      };
    case "review":
      return {
        label: isKo ? "검토 중" : "In review",
        color: "var(--warn)",
        background: "color-mix(in oklch, var(--warn) 12%, var(--bg-2) 88%)",
        border: "color-mix(in oklch, var(--warn) 28%, var(--line) 72%)",
      };
    case "offline":
      return {
        label: isKo ? "오프라인" : "Offline",
        color: "var(--fg-faint)",
        background: "color-mix(in oklch, var(--fg-faint) 12%, var(--bg-2) 88%)",
        border: "color-mix(in oklch, var(--fg-faint) 24%, var(--line) 76%)",
      };
    case "idle":
    default:
      return {
        label: isKo ? "대기" : "Idle",
        color: "var(--fg-muted)",
        background: "color-mix(in oklch, var(--fg-muted) 12%, var(--bg-2) 88%)",
        border: "color-mix(in oklch, var(--fg-muted) 24%, var(--line) 76%)",
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
  if (entries.length === 0) return null;

  return (
    <div className="pointer-events-none absolute inset-0 z-10">
      {entries.map(({ agent, warning, position }) => (
        <div
          key={warning.cardId}
          className="group absolute pointer-events-auto"
          style={{
            left: position.x + 16,
            top: position.y - 28,
            transform: "translate(-50%, -50%)",
          }}
        >
          <button
            type="button"
            className="flex h-7 min-w-7 items-center justify-center rounded-full border text-xs font-semibold shadow-sm transition hover:scale-[1.04] focus:outline-none focus:ring-2"
            style={{
              color: "var(--warn)",
              borderColor: "color-mix(in oklch, var(--warn) 28%, var(--line) 72%)",
              background: "color-mix(in oklch, var(--warn) 14%, var(--bg-2) 86%)",
            }}
            aria-label={
              isKo
                ? `${agent.alias || agent.name_ko || agent.name} 수동 개입 경고`
                : `${agent.alias || agent.name} manual intervention warning`
            }
          >
            !
          </button>
          <div
            className="pointer-events-none absolute bottom-[calc(100%+0.55rem)] left-1/2 hidden w-64 -translate-x-1/2 rounded-2xl border px-3 py-3 shadow-xl group-hover:block group-focus-within:block"
            style={{
              borderColor: "color-mix(in oklch, var(--warn) 26%, var(--line) 74%)",
              background: "color-mix(in oklch, var(--warn) 8%, var(--bg-2) 92%)",
            }}
          >
            <div className="text-[11px] font-semibold uppercase tracking-[0.18em]" style={{ color: "var(--warn)" }}>
              {isKo ? "수동 개입" : "Manual intervention"}
            </div>
            <div className="mt-1 text-sm font-semibold" style={{ color: "var(--fg)" }}>
              {warning.title}
            </div>
            <div className="mt-2 text-xs leading-5" style={{ color: "var(--fg-muted)" }}>
              {warning.reason
                ?? (isKo
                  ? "구체 사유는 카드 상세에서 확인할 수 있습니다."
                  : "Open the detail drawer to inspect the full reason.")}
            </div>
            <div className="mt-3 flex items-center justify-between gap-2">
              <span className="text-[11px]" style={{ color: "var(--fg-faint)" }}>
                {warning.issueNumber ? `#${warning.issueNumber}` : warning.status}
              </span>
              <button
                type="button"
                className="pointer-events-auto rounded-full border px-2.5 py-1 text-[11px] font-semibold transition hover:opacity-90"
                onClick={() => onSelectAgent?.(agent)}
                style={{
                  color: "var(--warn)",
                  borderColor: "color-mix(in oklch, var(--warn) 30%, var(--line) 70%)",
                  background: "color-mix(in oklch, var(--warn) 12%, var(--bg-2) 88%)",
                }}
              >
                {isKo ? "세부 보기" : "Open detail"}
              </button>
            </div>
          </div>
        </div>
      ))}
    </div>
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

  return (
    <div className="mt-3 px-3 pb-6">
      <div className="text-xs font-semibold uppercase tracking-[0.24em] mb-2 px-1" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "에이전트 현황" : "Agent Status"}
      </div>
      <div className="grid grid-cols-2 gap-2">
        {sorted.map((agent) => {
          const status = seatStatusByAgent.get(agent.id) ?? "idle";
          const statusMeta = getSeatStatusMeta(status, isKo);
          const manualIntervention = manualInterventionByAgent.get(agent.id) ?? null;
          const primaryCard = primaryCardByAgent.get(agent.id) ?? null;
          const preview = manualIntervention?.reason
            ? previewManualReason(manualIntervention.reason)
            : previewCardTitle(primaryCard?.title ?? null);

          return (
            <button
              key={agent.id}
              type="button"
              onClick={() => onSelectAgent?.(agent)}
              className="rounded-2xl px-3 py-3 text-left"
              style={{
                background: manualIntervention
                  ? "color-mix(in oklch, var(--warn) 8%, var(--bg-2) 92%)"
                  : "color-mix(in oklch, var(--fg-faint) 6%, var(--bg-2) 94%)",
                border: manualIntervention
                  ? "1px solid color-mix(in oklch, var(--warn) 28%, var(--line) 72%)"
                  : "1px solid var(--th-card-border)",
              }}
            >
              <div className="flex items-start justify-between gap-2">
                <div className="flex min-w-0 items-center gap-2">
                  <span className="text-base">{agent.avatar_emoji}</span>
                  <span className="truncate text-xs font-medium" style={{ color: "var(--th-text-primary)" }}>
                    {agent.alias || agent.name_ko || agent.name}
                  </span>
                </div>
                {manualIntervention && (
                  <span
                    className="shrink-0 rounded-full px-2 py-0.5 text-[10px] font-semibold"
                    style={{
                      color: "var(--warn)",
                      background: "color-mix(in oklch, var(--warn) 12%, var(--bg-2) 88%)",
                    }}
                  >
                    {isKo ? "수동 개입" : "Manual"}
                  </span>
                )}
              </div>
              <div className="mt-2 flex items-center gap-1.5">
                <span
                  className="h-2 w-2 shrink-0 rounded-full"
                  style={{ background: statusMeta.color }}
                />
                <span
                  className="truncate text-xs"
                  style={{ color: statusMeta.color }}
                >
                  {agent.session_info || statusMeta.label}
                </span>
              </div>
              {preview && (
                <div className="mt-2 text-[11px] leading-5" style={{ color: "var(--th-text-muted)" }}>
                  {preview}
                </div>
              )}
              {agent.department_name_ko && (
                <div className="mt-2">
                  <span
                    className="inline-flex max-w-full items-center rounded-full px-2 py-0.5 text-[10px] font-medium"
                    style={{
                      color: statusMeta.color,
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
          );
        })}
      </div>
    </div>
  );
}
