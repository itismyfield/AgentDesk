import { describe, expect, it } from "vitest";
import type { Agent, KanbanCard } from "../../types";
import { deriveOfficeAgentState } from "./officeAgentState";

function makeAgent(overrides: Partial<Agent> = {}): Agent {
  return {
    id: "agent-1",
    name: "Alpha",
    alias: "alpha",
    name_ko: "알파",
    department_id: "dept-1",
    avatar_emoji: "🤖",
    personality: null,
    status: "working",
    stats_tasks_done: 3,
    stats_xp: 120,
    stats_tokens: 0,
    created_at: 1_710_000_000_000,
    ...overrides,
  };
}

function makeCard(overrides: Partial<KanbanCard> = {}): KanbanCard {
  return {
    id: "card-1",
    title: "Test card",
    description: null,
    status: "in_progress",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: null,
    requester_agent_id: null,
    assignee_agent_id: "agent-1",
    parent_card_id: null,
    latest_dispatch_id: "dispatch-1",
    sort_order: 10,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 777,
    github_issue_url: null,
    metadata_json: null,
    pipeline_stage_id: null,
    review_status: null,
    created_at: 1_710_000_000_000,
    updated_at: 1_710_000_100_000,
    started_at: 1_710_000_050_000,
    requested_at: 1_710_000_000_000,
    review_entered_at: null,
    completed_at: null,
    latest_dispatch_status: "in_progress",
    latest_dispatch_title: "dispatch",
    latest_dispatch_type: "implementation",
    latest_dispatch_result_summary: null,
    latest_dispatch_chain_depth: 0,
    child_count: 0,
    ...overrides,
  };
}

describe("deriveOfficeAgentState", () => {
  it("prefers review cards for active issue and seat status", () => {
    const agent = makeAgent();

    const state = deriveOfficeAgentState([agent], [
      makeCard({
        id: "card-in-progress",
        title: "Implement widget",
        status: "in_progress",
        github_issue_number: 780,
        updated_at: 1_710_000_100_000,
      }),
      makeCard({
        id: "card-review",
        title: "Review widget",
        status: "review",
        github_issue_number: 781,
        updated_at: 1_710_000_200_000,
      }),
    ]);

    expect(state.activeIssueByAgent.get(agent.id)).toMatchObject({
      cardId: "card-review",
      status: "review",
      number: 781,
    });
    expect(state.primaryCardByAgent.get(agent.id)?.id).toBe("card-review");
    expect(state.seatStatusByAgent.get(agent.id)).toBe("review");
  });

  it("keeps benign blocked reasons out of manual intervention and prefers explicit reasons", () => {
    const agent = makeAgent();
    const idleAgent = makeAgent({
      id: "agent-2",
      name: "Beta",
      alias: "beta",
      name_ko: "베타",
      status: "idle",
    });

    const state = deriveOfficeAgentState([agent, idleAgent], [
      makeCard({
        id: "card-benign",
        assignee_agent_id: agent.id,
        status: "requested",
        blocked_reason: "ci:waiting for checks",
        updated_at: 1_710_000_100_000,
      }),
      makeCard({
        id: "card-manual",
        assignee_agent_id: agent.id,
        status: "requested",
        blocked_reason: "maintainer approval required before deploy",
        github_issue_number: 785,
        updated_at: 1_710_000_200_000,
      }),
      makeCard({
        id: "card-dilemma",
        assignee_agent_id: idleAgent.id,
        status: "requested",
        review_status: "dilemma_pending",
        blocked_reason: "ci:waiting for reviewer",
        updated_at: 1_710_000_300_000,
      }),
    ]);

    expect(state.manualInterventionByAgent.get(agent.id)).toMatchObject({
      cardId: "card-manual",
      reason: "maintainer approval required before deploy",
      issueNumber: 785,
    });
    expect(state.manualInterventionByAgent.get(idleAgent.id)).toMatchObject({
      cardId: "card-dilemma",
      reason: null,
    });
  });
});
