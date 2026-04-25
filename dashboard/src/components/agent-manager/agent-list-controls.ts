import type { Agent } from "../../types";

export type AgentSortMode =
  | "status"
  | "name"
  | "xp"
  | "activity"
  | "department"
  | "created"
  | "archived";

export interface AgentListFilter {
  deptTab: string;
  statusFilter: string;
  search: string;
}

const STATUS_ORDER: Record<string, number> = {
  working: 0,
  idle: 1,
  break: 2,
  offline: 3,
  archived: 4,
};

export function filterAgents(
  agents: Agent[],
  { deptTab, statusFilter, search }: AgentListFilter,
): Agent[] {
  let filtered = agents;
  if (deptTab !== "all") {
    filtered = filtered.filter((agent) => agent.department_id === deptTab);
  }
  if (statusFilter !== "all") {
    filtered = filtered.filter((agent) => agent.status === statusFilter);
  }
  const trimmed = search.trim();
  if (trimmed) {
    const q = trimmed.toLowerCase();
    filtered = filtered.filter(
      (agent) =>
        agent.name.toLowerCase().includes(q) ||
        agent.name_ko.toLowerCase().includes(q) ||
        (agent.alias && agent.alias.toLowerCase().includes(q)) ||
        agent.avatar_emoji.includes(q),
    );
  }
  return filtered;
}

export function compareAgents(
  sortMode: AgentSortMode,
): (a: Agent, b: Agent) => number {
  return (left, right) => {
    if (sortMode === "name") {
      return left.name.localeCompare(right.name);
    }
    if (sortMode === "xp") {
      return (right.stats_xp ?? 0) - (left.stats_xp ?? 0);
    }
    if (sortMode === "activity") {
      const leftActivity =
        (left.agentdesk_working_count ?? 0) + (left.stats_tasks_done ?? 0);
      const rightActivity =
        (right.agentdesk_working_count ?? 0) + (right.stats_tasks_done ?? 0);
      if (leftActivity !== rightActivity) return rightActivity - leftActivity;
      return left.name.localeCompare(right.name);
    }
    if (sortMode === "department") {
      const leftDept =
        left.department_name_ko || left.department_name || left.department_id || "";
      const rightDept =
        right.department_name_ko ||
        right.department_name ||
        right.department_id ||
        "";
      const cmp = leftDept.localeCompare(rightDept);
      if (cmp !== 0) return cmp;
      return left.name.localeCompare(right.name);
    }
    if (sortMode === "created") {
      return (right.created_at ?? 0) - (left.created_at ?? 0);
    }
    if (sortMode === "archived") {
      return (right.archived_at ?? 0) - (left.archived_at ?? 0);
    }
    const leftStatus = STATUS_ORDER[left.status] ?? 5;
    const rightStatus = STATUS_ORDER[right.status] ?? 5;
    if (leftStatus !== rightStatus) return leftStatus - rightStatus;
    return left.name.localeCompare(right.name);
  };
}

export function sortAgents(agents: Agent[], sortMode: AgentSortMode): Agent[] {
  return [...agents].sort(compareAgents(sortMode));
}

export function filterAndSortAgents(
  agents: Agent[],
  filter: AgentListFilter,
  sortMode: AgentSortMode,
): Agent[] {
  return sortAgents(filterAgents(agents, filter), sortMode);
}
