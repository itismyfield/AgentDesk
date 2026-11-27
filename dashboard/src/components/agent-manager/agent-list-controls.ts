import type { Agent, DispatchedSession } from "../../types";
import { deriveDispatchedAsAgents } from "../../contexts/session-overlay";
import { isTestAgent } from "../../lib/agentHelpers";
import { AGENT_STATES, agentName, classifyAgents, type HomeAgent, type HomeAgentState } from "../home/homeModel";

export type AgentSortMode =
  | "status"
  | "name"
  | "activity"
  | "created";

export type AgentStatusFilter = HomeAgentState | "all";

export interface AgentListFilter {
  statusFilter: AgentStatusFilter;
  search: string;
  includeTests?: boolean;
}

/** Synthetic session stand-in (`dispatched:<id>`); they have no agent detail. */
export const isStandIn = (agent: Pick<Agent, "id">) => agent.id.startsWith("dispatched:");

/** Roster input classified with live sessions so state counts match home. */
export function classifyRoster(agents: Agent[], sessions: DispatchedSession[], sessionsFailed: boolean): HomeAgent[] {
  return classifyAgents([...agents, ...deriveDispatchedAsAgents(sessions)], sessions, sessionsFailed);
}

/** E2E harness agents stay hidden unless asked for or busy, as on home, so the state counts match. */
export function isListedAgent(agent: Agent, includeTests = false): boolean {
  return includeTests || !isTestAgent(agent) || agent.status === "working";
}

export function filterAgents(
  entries: HomeAgent[],
  { statusFilter, search, includeTests = false }: AgentListFilter,
): HomeAgent[] {
  const query = search.trim().toLowerCase();
  return entries.filter(({ agent, state }) =>
    isListedAgent(agent, includeTests)
    && (statusFilter === "all" || state === statusFilter)
    && (!query || [agent.name, agent.name_ko, agent.alias ?? "", agent.avatar_emoji].some((text) => text.toLowerCase().includes(query))),
  );
}

const byName = (left: Agent, right: Agent) => left.name.localeCompare(right.name);

export function compareAgents(
  sortMode: AgentSortMode,
): (a: HomeAgent, b: HomeAgent) => number {
  return ({ agent: left, state: leftState }, { agent: right, state: rightState }) => {
    if (sortMode === "name") return byName(left, right);
    if (sortMode === "activity") {
      const activity = (agent: Agent) => (agent.agentdesk_working_count ?? 0) + (agent.stats_tasks_done ?? 0);
      return activity(right) - activity(left) || byName(left, right);
    }
    if (sortMode === "created") return (right.created_at ?? 0) - (left.created_at ?? 0);
    // Home's order: state, then the Korean name.
    return AGENT_STATES.indexOf(leftState) - AGENT_STATES.indexOf(rightState)
      || agentName(left, true).localeCompare(agentName(right, true));
  };
}

export function sortAgents(entries: HomeAgent[], sortMode: AgentSortMode): HomeAgent[] {
  return [...entries].sort(compareAgents(sortMode));
}

export function filterAndSortAgents(
  entries: HomeAgent[],
  filter: AgentListFilter,
  sortMode: AgentSortMode,
): HomeAgent[] {
  return sortAgents(filterAgents(entries, filter), sortMode);
}
