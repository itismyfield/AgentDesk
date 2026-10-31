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
import type { Agent, Department, DispatchedSession, WSEvent } from "../types";
import * as api from "../api/client";
import { applySessionOverlay, deriveDispatchedAsAgents } from "./session-overlay";

interface WorkspaceContextValue {
  /** Agents with live session state folded into `status`. */
  agents: Agent[];
  departments: Department[];
  sessions: DispatchedSession[];
  setSessions: React.Dispatch<React.SetStateAction<DispatchedSession[]>>;
  visibleDispatchedSessions: DispatchedSession[];
  /** `agents` plus unlinked dispatched sessions shown as pseudo-agents. */
  agentsWithDispatched: Agent[];
  refreshAgents: () => void;
  refreshDepartments: () => void;
}

const WorkspaceContext = createContext<WorkspaceContextValue | null>(null);

interface WorkspaceProviderProps {
  initialAgents: Agent[];
  initialDepartments: Department[];
  initialSessions: DispatchedSession[];
  pushNotification: (msg: string, level: "info" | "success" | "warning" | "error") => void;
  children: ReactNode;
}

export function WorkspaceProvider({
  initialAgents,
  initialDepartments,
  initialSessions,
  pushNotification,
  children,
}: WorkspaceProviderProps) {
  const [agents, setAgents] = useState<Agent[]>(initialAgents);
  const [departments, setDepartments] = useState<Department[]>(initialDepartments);
  const [sessions, setSessions] = useState<DispatchedSession[]>(initialSessions);

  const sessionAwareAgents = useMemo(() => applySessionOverlay(agents, sessions), [agents, sessions]);
  const agentsRef = useRef<Agent[]>(sessionAwareAgents);
  useEffect(() => { agentsRef.current = sessionAwareAgents; }, [sessionAwareAgents]);

  const refreshAgents = useCallback(() => {
    api.getAgents().then(setAgents).catch(() => {});
  }, []);
  const refreshDepartments = useCallback(() => {
    api.getDepartments().then(setDepartments).catch(() => {});
  }, []);

  const pushNotificationRef = useRef(pushNotification);
  useEffect(() => { pushNotificationRef.current = pushNotification; }, [pushNotification]);

  useEffect(() => {
    function handleWs(e: Event) {
      const event = (e as CustomEvent<WSEvent>).detail;
      switch (event.type) {
        case "agent_status": {
          const a = event.payload as Agent;
          const previous = agentsRef.current.find((agent) => agent.id === a.id);
          const label = a.name_ko || a.name || "agent";
          if (previous?.status !== a.status) {
            if (a.status === "working") {
              pushNotificationRef.current(`${label} 작업 시작`, "info");
            } else if (a.status === "idle" && previous?.status === "working") {
              pushNotificationRef.current(`${label} 작업 완료`, "success");
            }
          }
          setAgents((prev) => prev.map((ag) => (ag.id === a.id ? { ...ag, ...a } : ag)));
          break;
        }
        case "agent_created":
        case "agent_deleted":
          refreshAgents();
          break;
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
      }
    }
    window.addEventListener("pcd-ws-event", handleWs);
    return () => window.removeEventListener("pcd-ws-event", handleWs);
  }, [refreshAgents]);

  const visibleDispatchedSessions = useMemo(
    () => sessions.filter((s) => s.status !== "disconnected" && !s.linked_agent_id),
    [sessions],
  );
  const agentsWithDispatched = useMemo(
    () => [...sessionAwareAgents, ...deriveDispatchedAsAgents(sessions)],
    [sessionAwareAgents, sessions],
  );

  return (
    <WorkspaceContext.Provider
      value={{
        agents: sessionAwareAgents,
        departments,
        sessions,
        setSessions,
        visibleDispatchedSessions,
        agentsWithDispatched,
        refreshAgents,
        refreshDepartments,
      }}
    >
      {children}
    </WorkspaceContext.Provider>
  );
}

export function useWorkspace(): WorkspaceContextValue {
  const ctx = useContext(WorkspaceContext);
  if (!ctx) throw new Error("useWorkspace must be used within WorkspaceProvider");
  return ctx;
}
