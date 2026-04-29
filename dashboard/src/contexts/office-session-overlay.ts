import type { Agent, DispatchedSession, SubAgent } from "../types";

function sessionIsWorking(status: DispatchedSession["status"]): boolean {
  return status === "turn_active" || status === "awaiting_bg" || status === "working";
}

export function deriveSubAgents(sessions: DispatchedSession[]): SubAgent[] {
  return sessions
    .filter((s) => sessionIsWorking(s.status) && s.linked_agent_id)
    .map((s) => ({
      id: s.id,
      parentAgentId: s.linked_agent_id!,
      task: s.name || s.session_info || "파견 세션",
      status: "working" as const,
    }));
}

export function deriveDispatchedAsAgents(sessions: DispatchedSession[]): Agent[] {
  const visible = sessions.filter((s) => s.status !== "disconnected" && !s.linked_agent_id);
  return visible
    .filter((s) => s.department_id)
    .map((s) => ({
      id: `dispatched:${s.id}`,
      name: s.name || s.session_key,
      name_ko: s.name || s.session_key,
      department_id: s.department_id,
      role: "intern" as const,
      avatar_emoji: s.avatar_emoji || "📡",
      sprite_number: s.sprite_number,
      personality: null,
      status: sessionIsWorking(s.status) ? ("working" as const) : ("idle" as const),
      current_task_id: null,
      stats_tasks_done: 0,
      stats_xp: s.stats_xp,
      stats_tokens: s.tokens || 0,
      created_at: s.connected_at,
      session_info: s.session_info,
      department_name: s.department_name,
      department_name_ko: s.department_name_ko,
      department_color: s.department_color,
    }));
}

export function applySessionOverlay(baseAgents: Agent[], sessions: DispatchedSession[]): Agent[] {
  const overlay = new Map<string, {
    workingCount: number;
    sessionInfo: string | null;
    currentThreadChannelId: string | null;
    newestTs: number;
  }>();

  for (const session of sessions) {
    if (session.status === "disconnected") continue;
    const agentId =
      session.linked_agent_id ??
      (session as DispatchedSession & { agent_id?: string | null }).agent_id ??
      null;
    const isWorking = sessionIsWorking(session.status);
    if (!agentId || !isWorking) continue;

    const ts = session.last_seen_at ?? session.connected_at ?? 0;
    const prev = overlay.get(agentId);
    // Always keep the most recent session's values by comparing timestamps,
    // regardless of array ordering (bootstrap: old→new, WS prepend: new→old).
    const prevIsNewer = prev && prev.newestTs > ts;
    overlay.set(agentId, {
      workingCount: (prev?.workingCount ?? 0) + 1,
      sessionInfo: prevIsNewer
        ? prev.sessionInfo
        : (session.session_info ?? session.name ?? "작업 중"),
      currentThreadChannelId: prevIsNewer
        ? prev.currentThreadChannelId
        : (session.thread_channel_id ?? null),
      newestTs: prevIsNewer ? prev.newestTs : ts,
    });
  }

  if (overlay.size === 0) return baseAgents;

  return baseAgents.map((agent) => {
    const sessionState = overlay.get(agent.id);
    if (!sessionState) return agent;
    return {
      ...agent,
      status: "working",
      session_info: sessionState.sessionInfo ?? agent.session_info ?? "작업 중",
      activity_source: "agentdesk",
      agentdesk_working_count: sessionState.workingCount,
      current_thread_channel_id:
        sessionState.currentThreadChannelId ?? agent.current_thread_channel_id ?? null,
    };
  });
}
