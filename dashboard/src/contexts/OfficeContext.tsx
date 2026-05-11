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

// ── Per-dataset loading/error state ──

export type DatasetKey = "offices" | "agents" | "allAgents" | "departments" | "allDepartments" | "auditLogs";

interface DatasetState {
  loading: boolean;
  error: string | null;
}

type DatasetStates = Record<DatasetKey, DatasetState>;

const INITIAL_DATASET: DatasetState = { loading: false, error: null };

// ── Context value ──

interface OfficeContextValue {
  // Data
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

  // Derived
  visibleDispatchedSessions: DispatchedSession[];
  subAgents: SubAgent[];
  agentsWithDispatched: Agent[];

  // Loading/error per dataset
  datasetStates: DatasetStates;
  /** True while any refresh is in flight */
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

  // ── Per-dataset loading/error tracking ──

  const [datasetStates, setDatasetStates] = useState<DatasetStates>({
    offices: INITIAL_DATASET,
    agents: INITIAL_DATASET,
    allAgents: INITIAL_DATASET,
    departments: INITIAL_DATASET,
    allDepartments: INITIAL_DATASET,
    auditLogs: INITIAL_DATASET,
  });

  const trackedFor = useCallback(
    <T,>(key: DatasetKey, promise: Promise<T>): Promise<T> => {
      setDatasetStates((prev) => ({ ...prev, [key]: { loading: true, error: null } }));
      return promise
        .then((result) => {
          setDatasetStates((prev) => ({ ...prev, [key]: { loading: false, error: null } }));
          return result;
        })
        .catch((e) => {
          const msg = e instanceof Error ? e.message : String(e);
          setDatasetStates((prev) => ({ ...prev, [key]: { loading: false, error: msg } }));
          throw e;
        });
    },
    [],
  );

  const refreshOffices = useCallback(() => {
    trackedFor("offices", api.getOffices()).then(setOffices).catch(() => {});
  }, [trackedFor]);

  const refreshAgents = useCallback(() => {
    trackedFor("agents", api.getAgents(selectedOfficeId ?? undefined)).then(setAgents).catch(() => {});
  }, [selectedOfficeId, trackedFor]);

  const refreshAllAgents = useCallback(() => {
    trackedFor("allAgents", api.getAgents()).then(setAllAgents).catch(() => {});
  }, [trackedFor]);

  const refreshDepartments = useCallback(() => {
    trackedFor("departments", api.getDepartments(selectedOfficeId ?? undefined)).then(setDepartments).catch(() => {});
  }, [selectedOfficeId, trackedFor]);

  const refreshAllDepartments = useCallback(() => {
    trackedFor("allDepartments", api.getDepartments()).then(setAllDepartments).catch(() => {});
  }, [trackedFor]);

  const refreshAuditLogs = useCallback(() => {
    trackedFor("auditLogs", api.getAuditLogs(12)).then(setAuditLogs).catch(() => {});
  }, [trackedFor]);

  const refreshing = useMemo(
    () => Object.values(datasetStates).some((s) => s.loading),
    [datasetStates],
  );

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
              push(`${label} 작업 시작`, "info");
            } else if (a.status === "idle" && previous?.status === "working") {
              push(`${label} 작업 완료`, "success");
            }
          }
          setAgents((prev) => prev.map((ag) => (ag.id === a.id ? { ...ag, ...a } : ag)));
          setAllAgents((prev) => prev.map((ag) => (ag.id === a.id ? { ...ag, ...a } : ag)));
          break;
        }
        case "agent_created":
          refreshAgents();
          refreshAllAgents();
          break;
        case "agent_deleted":
          refreshAgents();
          refreshAllAgents();
          refreshOffices();
          break;
        // #2050 P1 finding 1 — departments_changed / offices_changed had no
        // server emit path; removed from WSEventType.
        case "dispatched_session_new": {
          const s = event.payload as DispatchedSession;
          setSessions((prev) => [s, ...prev.filter((p) => p.id !== s.id)]);
          break;
        }
        case "dispatched_session_update": {
          const s = event.payload as DispatchedSession;
          setSessions((prev) => prev.map((p) => (p.id === s.id ? s : p)));
          break;
        }
        case "dispatched_session_disconnect": {
          const id = (event.payload as { id: string }).id;
          setSessions((prev) =>
            prev.map((p) => (p.id === id ? { ...p, status: "disconnected" as const } : p)),
          );
          break;
        }
        case "round_table_new": {
          const m = event.payload as RoundTableMeeting;
          setRoundTableMeetings((prev) => [m, ...prev.filter((p) => p.id !== m.id)]);
          break;
        }
        case "round_table_update": {
          const m = event.payload as RoundTableMeeting;
          setRoundTableMeetings((prev) => prev.map((p) => (p.id === m.id ? m : p)));
          break;
        }
        case "kanban_card_created":
        case "kanban_card_updated":
        case "kanban_card_deleted":
          refreshAuditLogs();
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
        datasetStates,
        refreshing,
        refreshOffices,
        refreshAgents,
        refreshAllAgents,
        refreshDepartments,
        refreshAllDepartments,
        refreshAuditLogs,
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
