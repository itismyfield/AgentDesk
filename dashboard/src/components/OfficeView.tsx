import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Application, Container, Graphics, Text, Texture } from "pixi.js";
import type { Agent, AuditLogEntry, Department, KanbanCard, RoundTableMeeting, Task, SubAgent } from "../types";
type ThemeMode = "dark" | "light";
import type { UiLanguage } from "../i18n";
import { buildSpriteMap } from "./AgentAvatar";
import { buildOfficeScene } from "./office-view/buildScene";
import type { Notification } from "./NotificationCenter";
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
    return window.matchMedia("(max-width: 639px)").matches;
  });

  useEffect(() => {
    const media = window.matchMedia("(max-width: 639px)");
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
  // Build active issue lookup map from kanban cards
  const activeIssueByAgent = useMemo(() => {
    const map = new Map<string, { number: number; url: string; startedAt?: number; title?: string }>();
    if (!kanbanCards) return map;
    for (const card of kanbanCards) {
      if (!card.assignee_agent_id || !card.github_issue_number) continue;
      if (card.status !== "in_progress" && card.status !== "review") continue;
      if (map.has(card.assignee_agent_id)) continue; // first match wins
      map.set(card.assignee_agent_id, {
        number: card.github_issue_number,
        url: card.github_issue_url || `https://github.com/${card.github_repo}/issues/${card.github_issue_number}`,
        startedAt: card.started_at != null ? (card.started_at < 1e12 ? card.started_at * 1000 : card.started_at) : undefined,
        title: card.title,
      });
    }
    return map;
  }, [kanbanCards]);
  const blockedAgentIds = useMemo(() => {
    const set = new Set<string>();
    if (!kanbanCards) return set;
    for (const card of kanbanCards) {
      if (card.status === "blocked" && card.assignee_agent_id) set.add(card.assignee_agent_id);
    }
    return set;
  }, [kanbanCards]);
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
  const [, setSceneRevision] = useState(0);

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
          <MobileAgentStatusGrid agents={agents} isKo={isKo} onSelectAgent={onSelectAgent} />
        </div>
        {/* Desktop: full Pixi office */}
        <div ref={containerRef} className="hidden w-full min-h-full pb-40 sm:block" style={{ imageRendering: "pixelated" }} />
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

const STATUS_COLORS: Record<string, string> = {
  working: "#34d399",
  idle: "#94a3b8",
  break: "#fbbf24",
  offline: "#64748b",
};

function MobileAgentStatusGrid({
  agents,
  isKo,
  onSelectAgent,
}: {
  agents: Agent[];
  isKo: boolean;
  onSelectAgent?: (agent: Agent) => void;
}) {
  const sorted = [...agents].sort((a, b) => {
    const order: Record<string, number> = { working: 0, break: 1, idle: 2, offline: 3 };
    return (order[a.status] ?? 9) - (order[b.status] ?? 9);
  });

  return (
    <div className="px-3 pb-6 mt-3">
      <div className="text-xs font-semibold uppercase tracking-[0.24em] mb-2 px-1" style={{ color: "var(--th-text-muted)" }}>
        {isKo ? "에이전트 현황" : "Agent Status"}
      </div>
      <div className="grid grid-cols-2 gap-2">
        {sorted.map((agent) => (
          <button
            key={agent.id}
            type="button"
            onClick={() => onSelectAgent?.(agent)}
            className="rounded-xl px-3 py-2.5 text-left"
            style={{
              background: "color-mix(in srgb, var(--th-card-bg) 86%, transparent)",
              border: "1px solid var(--th-card-border)",
            }}
          >
            <div className="flex items-center gap-2">
              <span className="text-base">{agent.avatar_emoji}</span>
              <span className="text-xs font-medium truncate" style={{ color: "var(--th-text-primary)" }}>
                {agent.alias || agent.name_ko || agent.name}
              </span>
            </div>
            <div className="flex items-center gap-1.5 mt-1.5">
              <span
                className="w-2 h-2 rounded-full shrink-0"
                style={{ background: STATUS_COLORS[agent.status] ?? STATUS_COLORS.offline }}
              />
              <span className="text-xs truncate" style={{ color: STATUS_COLORS[agent.status] ?? STATUS_COLORS.offline }}>
                {agent.session_info || (isKo
                  ? (agent.status === "working" ? "작업 중" : agent.status === "idle" ? "대기" : agent.status === "break" ? "휴식" : "오프라인")
                  : agent.status.charAt(0).toUpperCase() + agent.status.slice(1))}
              </span>
            </div>
            {agent.department_name_ko && (
              <div className="text-xs mt-1 truncate" style={{ color: "var(--th-text-muted)" }}>
                {isKo ? agent.department_name_ko : (agent.department_name || agent.department_name_ko)}
              </div>
            )}
          </button>
        ))}
      </div>
    </div>
  );
}
