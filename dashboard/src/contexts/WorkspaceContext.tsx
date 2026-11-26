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
import { localeName, useI18n, type UiLanguage } from "../i18n";
import { pollWhileVisible } from "../lib/pollWhileVisible";
import { applySessionOverlay, deriveDispatchedAsAgents, mergeAgentStatus } from "./session-overlay";

interface WorkspaceContextValue {
  /** Agents with live session state folded into `status`. */
  agents: Agent[];
  departments: Department[];
  sessions: DispatchedSession[];
  setSessions: React.Dispatch<React.SetStateAction<DispatchedSession[]>>;
  /** The first session list failed to load; sessions seen since then still count. */
  sessionsFailed: boolean;
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
  initialSessions?: DispatchedSession[] | null;
  pushNotification: (msg: string, level: "info" | "success" | "warning" | "error") => void;
  language?: string | null;
  children: ReactNode;
}

export function WorkspaceProvider({
  initialAgents,
  initialDepartments,
  initialSessions,
  pushNotification,
  language,
  children,
}: WorkspaceProviderProps) {
  const [agents, setAgents] = useState<Agent[]>(initialAgents);
  const [departments, setDepartments] = useState<Department[]>(initialDepartments);
  const [sessions, setSessions] = useState<DispatchedSession[]>(initialSessions ?? []);
  const [sessionsFailed, setSessionsFailed] = useState(initialSessions === null);
  const seededSessionsRef = useRef(initialSessions !== undefined);
  const i18n = useI18n(language);
  const lang: UiLanguage = i18n.__fromProvider || language ? i18n.language : "ko";
  const langRef = useRef<UiLanguage>(lang);
  langRef.current = lang;

  useEffect(() => {
    if (seededSessionsRef.current || initialSessions === undefined) return;
    seededSessionsRef.current = true;
    if (initialSessions === null) {
      setSessionsFailed(true);
      return;
    }
    setSessionsFailed(false);
    setSessions((prev) => {
      if (prev.length === 0) return initialSessions;
      const seen = new Set(prev.map((s) => s.id));
      return [...prev, ...initialSessions.filter((s) => !seen.has(s.id))];
    });
  }, [initialSessions]);

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

  // A failed bootstrap leaves every agent "unknown". Keep trying until the list arrives.
  useEffect(() => {
    if (!sessionsFailed) return;
    let active = true;
    let generation = 0;
    const load = () => {
      const requestId = ++generation;
      api.getDispatchedSessions(true, { suppressErrorToast: true, maxRetries: 0 })
        .then((next) => {
          if (!active || requestId !== generation) return;
          setSessions(next);
          setSessionsFailed(false);
        })
        .catch(() => {});
    };
    load();
    const stop = pollWhileVisible(load, 30_000);
    return () => { active = false; stop(); };
  }, [sessionsFailed]);

  useEffect(() => {
    function handleWs(e: Event) {
      const event = (e as CustomEvent<WSEvent>).detail;
      switch (event.type) {
        case "agent_status": {
          const a = event.payload as Agent;
          const previous = agentsRef.current.find((agent) => agent.id === a.id);
          const curLang = langRef.current;
          const label =
            (a.name || a.name_ko || a.alias
              ? localeName(curLang, { ...a, name: a.name || a.name_ko || "agent" })
              : "") || "agent";
          if (previous?.status !== a.status) {
            if (a.status === "working") {
              const msg =
                curLang === "ko"
                  ? `${label} 작업 시작`
                  : curLang === "ja"
                    ? `${label} 作業開始`
                    : curLang === "zh"
                      ? `${label} 开始工作`
                      : `${label} started working`;
              pushNotificationRef.current(msg, "info");
            } else if (a.status === "idle" && previous?.status === "working") {
              const msg =
                curLang === "ko"
                  ? `${label} 작업 완료`
                  : curLang === "ja"
                    ? `${label} 作業完了`
                    : curLang === "zh"
                      ? `${label} 完成工作`
                      : `${label} finished working`;
              pushNotificationRef.current(msg, "success");
            }
          }
          setAgents((prev) => mergeAgentStatus(prev, a));
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
        sessionsFailed,
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

/** The agent roster inside WorkspaceProvider, or null for components rendered outside it. */
export function useWorkspaceAgents(): Agent[] | null {
  return useContext(WorkspaceContext)?.agents ?? null;
}
