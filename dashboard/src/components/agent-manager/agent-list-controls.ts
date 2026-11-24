import type { Agent, Department, DispatchedSession } from "../../types";
import { localeName } from "../../i18n";
import { deriveDispatchedAsAgents } from "../../contexts/session-overlay";
import { isTestAgent } from "../../lib/agentHelpers";
import { AGENT_STATES, agentName, classifyAgents, type HomeAgent, type HomeAgentState } from "../home/homeModel";

export type AgentSortMode =
  | "status"
  | "name"
  | "activity"
  | "department"
  | "created";

export type AgentStatusFilter = HomeAgentState | "all";

/** Department filter value for agents without a department. */
export const NO_DEPARTMENT = "__none";

export interface AgentListFilter {
  deptTab: string;
  statusFilter: AgentStatusFilter;
  search: string;
  includeTests?: boolean;
  /** Existing department ids; an agent pointing elsewhere counts as unassigned. */
  departments?: ReadonlySet<string>;
}

export function hasDepartment(agent: Agent, departments?: ReadonlySet<string>): boolean {
  return Boolean(agent.department_id) && (!departments || departments.has(agent.department_id!));
}

/** Disambiguates departments that share the same localized name (e.g. two `아트팀` rows) with their English name or slug id. */
export function departmentLabels(departments: Department[], locale: string): Map<string, string> {
  const base = departments.map((dept) => ({ dept, name: localeName(locale, dept).trim() || dept.id }));
  const counts = new Map<string, number>();
  for (const { name } of base) counts.set(name, (counts.get(name) ?? 0) + 1);
  const disambiguated = base.map(({ dept, name }) => {
    if ((counts.get(name) ?? 0) < 2) return { id: dept.id, baseName: name, label: name };
    const alt = dept.name.trim() && dept.name.trim() !== name ? dept.name.trim() : dept.id;
    return { id: dept.id, baseName: name, label: `${name} (${alt})` };
  });
  const labelCounts = new Map<string, number>();
  for (const { label } of disambiguated) labelCounts.set(label, (labelCounts.get(label) ?? 0) + 1);
  return new Map(disambiguated.map(({ id, baseName, label }) => [
    id,
    (labelCounts.get(label) ?? 0) > 1 ? `${baseName} (${id})` : label,
  ]));
}

/** Sessions attached to a department only, which home counts as agents (`dispatched:<id>`); they have no detail. */
export const isStandIn = (agent: Pick<Agent, "id">) => agent.id.startsWith("dispatched:");

/** Home's roster input: real agents plus the stand-ins, classified together so the state counts match home. */
export function classifyRoster(agents: Agent[], sessions: DispatchedSession[], sessionsFailed: boolean): HomeAgent[] {
  return classifyAgents([...agents, ...deriveDispatchedAsAgents(sessions)], sessions, sessionsFailed);
}

/** E2E harness agents stay hidden unless asked for or busy, as on home, so the state counts match. */
export function isListedAgent(agent: Agent, includeTests = false): boolean {
  return includeTests || !isTestAgent(agent) || agent.status === "working";
}

export function filterAgents(
  entries: HomeAgent[],
  { deptTab, statusFilter, search, includeTests = false, departments }: AgentListFilter,
): HomeAgent[] {
  const query = search.trim().toLowerCase();
  return entries.filter(({ agent, state }) =>
    isListedAgent(agent, includeTests)
    && (deptTab === "all" || (deptTab === NO_DEPARTMENT ? !hasDepartment(agent, departments) : agent.department_id === deptTab))
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
    if (sortMode === "department") {
      const dept = (agent: Agent) => agent.department_name_ko || agent.department_name || agent.department_id || "";
      return dept(left).localeCompare(dept(right)) || byName(left, right);
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
