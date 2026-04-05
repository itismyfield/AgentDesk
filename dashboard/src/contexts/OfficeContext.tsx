import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import type {
  Agent,
  AuditLogEntry,
  Department,
  DispatchedSession,
  Office,
  RoundTableMeeting,
  SubAgent,
  WSEvent,
} from "../types";
import * as api from "../api/client";
import {
  applySessionOverlay,
  deriveDispatchedAsAgents,
  deriveSubAgents,
} from "./office-session-overlay";

// ── Context value ──

interface OfficeContextValue {
  offices: Office[];
  selectedOfficeId: string | null;
  setSelectedOfficeId: (id: string | null) => void;
  agents: Agent[];
  allAgents: Agent[];
  departments: Department[];
  allDepartments: Department[];
  sessions: DispatchedSession[];
  setSessions: React.Dispatch<React.SetStateAction<DispatchedSession[]>>;
  roundTableMeetings: RoundTableMeeting[];
  setRoundTableMeetings: React.Dispatch<React.SetStateAction<RoundTableMeeting[]>>;
  auditLogs: AuditLogEntry[];
  /** Sessions visible (not disconnected, not linked) */
  visibleDispatchedSessions: DispatchedSession[];
  subAgents: SubAgent[];
  /** agents + dispatched-as-agent entries */
  agentsWithDispatched: Agent[];
  /** True while any context refresh is in flight */
  refreshing: boolean;

  // Refresh functions
  refreshOffices: () => void;
  refreshAgents: () => void;
  refreshAllAgents: () => void;
  refreshDepartments: () => void;
  refreshAllDepartments: () => void;
  refreshAuditLogs: () => void;
}

const OfficeContext = createContext<OfficeContextValue | null>(null);

// ── Provider ──

interface OfficeProviderProps {
  initialOffices: Office[];
  initialAgents: Agent[];
  initialAllAgents?: Agent[];
  initialDepartments: Department[];
  initialAllDepartments?: Department[];
  initialSessions: DispatchedSession[];
  initialRoundTableMeetings: RoundTableMeeting[];
  initialAuditLogs: AuditLogEntry[];
  initialSelectedOfficeId: string | null;
  pushNotification: (msg: string, level: "info" | "success" | "warning" | "error") => void;
  children: ReactNode;
}

export function OfficeProvider({
  initialOffices,
  initialAgents,
  initialAllAgents,
  initialDepartments,
  initialAllDepartments,
  initialSessions,
  initialRoundTableMeetings,
  initialAuditLogs,
  initialSelectedOfficeId,
  pushNotification,
  children,
}: OfficeProviderProps) {
  const [offices, setOffices] = useState<Office[]>(initialOffices);
  const [selectedOfficeId, setSelectedOfficeId] = useState<string | null>(initialSelectedOfficeId);
  const [agents, setAgents] = useState<Agent[]>(initialAgents);
  const [allAgents, setAllAgents] = useState<Agent[]>(initialAllAgents ?? initialAgents);
  const [departments, setDepartments] = useState<Department[]>(initialDepartments);
  const [allDepartments, setAllDepartments] = useState<Department[]>(initialAllDepartments ?? initialDepartments);
  const [sessions, setSessions] = useState<DispatchedSession[]>(initialSessions);
  const [roundTableMeetings, setRoundTableMeetings] = useState<RoundTableMeeting[]>(initialRoundTableMeetings);
  const [auditLogs, setAuditLogs] = useState<AuditLogEntry[]>(initialAuditLogs);

  const allAgentsRef = useRef<Agent[]>(initialAgents);
  const sessionAwareAgents = useMemo(() => applySessionOverlay(agents, sessions), [agents, sessions]);
  const sessionAwareAllAgents = useMemo(() => applySessionOverlay(allAgents, sessions), [allAgents, sessions]);
  useEffect(() => { allAgentsRef.current = sessionAwareAllAgents; }, [sessionAwareAllAgents]);

  // ── Reload scoped data when office selection changes ──
  // Skip the first execution — bootstrap already provides correct data.
  const mountedRef = useRef(false);
  useEffect(() => {
    if (!mountedRef.current) {
      mountedRef.current = true;
      return;
    }
    refreshAgents();
    refreshDepartments();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedOfficeId]);

  // ── Refresh functions with loading tracking ──

  const [refreshCount, setRefreshCount] = useState(0);
  const tracked = useCallback(
    <T,>(promise: Promise<T>): Promise<T> => {
      setRefreshCount((c) => c + 1);
      return promise.finally(() => setRefreshCount((c) => c - 1));
    },
    [],
  );

  const refreshOffices = useCallback(() => {
    tracked(api.getOffices()).then(setOffices).catch(() => {});
  }, [tracked]);

  const refreshAgents = useCallback(() => {
    tracked(api.getAgents(selectedOfficeId ?? undefined)).then(setAgents).catch(() => {});
  }, [selectedOfficeId, tracked]);

  const refreshAllAgents = useCallback(() => {
    tracked(api.getAgents()).then(setAllAgents).catch(() => {});
  }, [tracked]);

  const refreshDepartments = useCallback(() => {
    tracked(api.getDepartments(selectedOfficeId ?? undefined)).then(setDepartments).catch(() => {});
  }, [selectedOfficeId, tracked]);

  const refreshAllDepartments = useCallback(() => {
    tracked(api.getDepartments()).then(setAllDepartments).catch(() => {});
  }, [tracked]);

  const refreshAuditLogs = useCallback(() => {
    tracked(api.getAuditLogs(12)).then(setAuditLogs).catch(() => {});
  }, [tracked]);

  // Stable ref for pushNotification to avoid re-registering WS listener
  const pushNotificationRef = useRef(pushNotification);
  useEffect(() => { pushNotificationRef.current = pushNotification; }, [pushNotification]);

  // ── WS event handling ──
  useEffect(() => {
    function handleWs(e: Event) {
      const event = (e as CustomEvent<WSEvent>).detail;
      const push = pushNotificationRef.current;
      switch (event.type) {
        case "agent_status": {
          const a = event.payload as Agent;
          const previous = allAgentsRef.current.find((agent) => agent.id === a.id);
          const label = a.name_ko || a.name || "agent";
          if (previous?.status !== a.status) {
            if (a.status === "working") {
              push(`${label}: ${a.session_info || "작업 시작"}`, "info");
            } else if (previous?.status === "working") {
              push(`${label}: 작업 상태 ${a.status}`, "warning");
            }
          } else if (a.status === "working" && a.session_info && previous?.session_info !== a.session_info) {
            push(`${label}: ${a.session_info}`, "info");
          }
          setAgents((prev) => prev.map((p) => (p.id === a.id ? { ...p, ...a } : p)));
          setAllAgents((prev) => prev.map((p) => (p.id === a.id ? { ...p, ...a } : p)));
          break;
        }
        case "agent_created": {
          const created = event.payload as Agent;
          push(`새 에이전트: ${created.name_ko || created.name || "unknown"}`, "success");
          api.getAgents(selectedOfficeId ?? undefined).then(setAgents).catch(() => {});
          api.getAgents().then(setAllAgents).catch(() => {});
          api.getAuditLogs(12).then(setAuditLogs).catch(() => {});
          break;
        }
        case "agent_deleted":
          setAgents((prev) => prev.filter((a) => a.id !== (event.payload as { id: string }).id));
          setAllAgents((prev) => prev.filter((a) => a.id !== (event.payload as { id: string }).id));
          api.getAuditLogs(12).then(setAuditLogs).catch(() => {});
          break;
        case "departments_changed":
          api.getDepartments(selectedOfficeId ?? undefined).then(setDepartments).catch(() => {});
          api.getAuditLogs(12).then(setAuditLogs).catch(() => {});
          break;
        case "offices_changed":
          api.getOffices().then(setOffices).catch(() => {});
          api.getAuditLogs(12).then(setAuditLogs).catch(() => {});
          break;
        case "dispatched_session_new": {
          const ns = event.payload as DispatchedSession;
          setSessions((prev) => [ns, ...prev]);
          push(`파견 세션 연결: ${ns.name || ns.session_key}`, "info");
          break;
        }
        case "dispatched_session_update":
          setSessions((prev) => {
            const s = event.payload as DispatchedSession;
            return prev.map((p) => (p.id === s.id ? { ...p, ...s } : p));
          });
          break;
        case "dispatched_session_disconnect": {
          const { id } = event.payload as { id: string };
          setSessions((prev) => prev.map((p) => p.id === id ? { ...p, status: "disconnected" as const } : p));
          push("파견 세션 종료", "warning");
          break;
        }
        case "round_table_new": {
          const m = event.payload as RoundTableMeeting;
          setRoundTableMeetings((prev) => [m, ...prev.filter((p) => p.id !== m.id)]);
          push(`라운드 테이블: ${m.agenda.slice(0, 30)}`, "info");
          break;
        }
        case "round_table_update": {
          const m = event.payload as RoundTableMeeting;
          setRoundTableMeetings((prev) => prev.map((p) => (p.id === m.id ? { ...p, ...m } : p)));
          break;
        }
        // kanban events that also trigger auditLog refresh
        case "kanban_card_created":
        case "kanban_card_updated":
        case "kanban_card_deleted":
          api.getAuditLogs(12).then(setAuditLogs).catch(() => {});
          break;
      }
    }
    window.addEventListener("pcd-ws-event", handleWs);
    return () => window.removeEventListener("pcd-ws-event", handleWs);
    // selectedOfficeId is needed for scoped refresh calls inside the handler
  }, [selectedOfficeId]);

  // ── Derived values ──

  const visibleDispatchedSessions = sessions.filter(
    (s) => s.status !== "disconnected" && !s.linked_agent_id,
  );
  const subAgents = deriveSubAgents(sessions);
  const dispatchedAsAgents = deriveDispatchedAsAgents(sessions);
  const agentsWithDispatched = [...sessionAwareAgents, ...dispatchedAsAgents];

  return (
    <OfficeContext.Provider
      value={{
        offices,
        selectedOfficeId,
        setSelectedOfficeId,
        agents: sessionAwareAgents,
        allAgents: sessionAwareAllAgents,
        departments,
        allDepartments,
        sessions,
        setSessions,
        roundTableMeetings,
        setRoundTableMeetings,
        auditLogs,
        visibleDispatchedSessions,
        subAgents,
        agentsWithDispatched,
        refreshOffices,
        refreshAgents,
        refreshAllAgents,
        refreshDepartments,
        refreshAllDepartments,
        refreshAuditLogs,
        refreshing: refreshCount > 0,
      }}
    >
      {children}
    </OfficeContext.Provider>
  );
}

// ── Hook ──

export function useOffice(): OfficeContextValue {
  const ctx = useContext(OfficeContext);
  if (!ctx) throw new Error("useOffice must be used within OfficeProvider");
  return ctx;
}
