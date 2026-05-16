import { describe, expect, it } from "vitest";

import type { Agent, KanbanCard } from "../../types";
import { filterKanbanCards, getActiveFilterCount, type KanbanFilterValues } from "./kanban-filter-state";

const baseFilters: KanbanFilterValues = {
  agentFilter: "all",
  deptFilter: "all",
  cardTypeFilter: "all",
  signalStatusFilter: "all",
  search: "",
  showClosed: false,
};

function makeAgent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-1",
    name: "Planner",
    name_ko: "플래너",
    department_id: "dept-a",
    avatar_emoji: "",
    personality: null,
    status: "idle",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 1,
    ...overrides,
  };
}

function makeCard(overrides: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id: "card-1",
    title: "Planner backlog",
    description: "Investigate onboarding",
    status: "in_progress",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: null,
    requester_agent_id: null,
    assignee_agent_id: "agent-1",
    parent_card_id: null,
    latest_dispatch_id: null,
    sort_order: 1,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 1,
    github_issue_url: null,
    metadata_json: null,
    pipeline_stage_id: null,
    review_status: null,
    created_at: 1,
    updated_at: 1,
    started_at: 1_000,
    requested_at: null,
    completed_at: null,
    ...overrides,
  };
}

describe("kanban filter state", () => {
  it("counts active filters across search, scope, advanced filters, and closed columns", () => {
    expect(getActiveFilterCount(baseFilters)).toBe(0);
    expect(getActiveFilterCount({
      ...baseFilters,
      agentFilter: "agent-1",
      deptFilter: "dept-a",
      search: "review",
      showClosed: true,
    })).toBe(4);
  });

  it("filters cards by assignee, department, type, signal, and search text", () => {
    const agents = [makeAgent(), makeAgent({ id: "agent-2", name: "Reviewer", department_id: "dept-b" })];
    const agentMap = new Map(agents.map((agent) => [agent.id, agent]));
    const cards = [
      makeCard({ id: "card-1", title: "Planner backlog", assignee_agent_id: "agent-1" }),
      makeCard({
        id: "card-2",
        title: "Review followup",
        assignee_agent_id: "agent-2",
        status: "review",
        review_status: "suggestion_pending",
        latest_dispatch_type: "review",
      }),
      makeCard({ id: "card-3", title: "Closed work", status: "done", completed_at: 1 }),
    ];

    const result = filterKanbanCards({
      agentMap,
      filters: {
        ...baseFilters,
        deptFilter: "dept-b",
        cardTypeFilter: "review",
        signalStatusFilter: "review",
        search: "reviewer",
      },
      getAgentLabel: (agentId) => agents.find((agent) => agent.id === agentId)?.name ?? "Unassigned",
      nowMs: 10_000,
      repoCards: cards,
      selectedAgentId: null,
      staleInProgressMs: 5_000,
    });

    expect(result.map((card) => card.id)).toEqual(["card-2"]);
  });

  it("detects stale in-progress cards with second or millisecond timestamps", () => {
    const agent = makeAgent();
    const agentMap = new Map([[agent.id, agent]]);
    const cards = [
      makeCard({ id: "fresh", started_at: 1_710_000_019_000 }),
      makeCard({ id: "stale-seconds", started_at: 1_710_000_000 }),
      makeCard({ id: "stale-ms", started_at: 1_710_000_000_000 }),
    ];

    const result = filterKanbanCards({
      agentMap,
      filters: { ...baseFilters, signalStatusFilter: "stalled" },
      getAgentLabel: () => agent.name,
      nowMs: 1_710_000_020_000,
      repoCards: cards,
      selectedAgentId: null,
      staleInProgressMs: 5_000,
    });

    expect(result.map((card) => card.id)).toEqual(["stale-seconds", "stale-ms"]);
  });
});
