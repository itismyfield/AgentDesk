import { describe, expect, it } from "vitest";

import type { Agent, KanbanCard, ReceiptSnapshotAgentShare } from "../../types";
import {
  LONG_BLOCKED_DAYS,
  REVIEW_DELAY_DAYS,
  REWORK_ALERT_THRESHOLD,
  buildAgentRoiRows,
  buildBottleneckGroups,
  estimateReworkCount,
  parseDashboardTimestamp,
} from "./dashboardInsights";

const agents: Agent[] = [
  {
    id: "agent-alpha",
    name: "agent-alpha",
    alias: "Alpha",
    name_ko: "알파",
    department_id: null,
    avatar_emoji: "🤖",
    personality: null,
    status: "working",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 0,
  },
  {
    id: "agent-beta",
    name: "agent-beta",
    alias: "Beta",
    name_ko: "베타",
    department_id: null,
    avatar_emoji: "🤖",
    personality: null,
    status: "idle",
    stats_tasks_done: 0,
    stats_xp: 0,
    stats_tokens: 0,
    created_at: 0,
  },
];

function makeCard(overrides: Partial<KanbanCard>): KanbanCard {
  return {
    id: "card-1",
    title: "Sample Card",
    description: null,
    status: "done",
    github_repo: "itismyfield/AgentDesk",
    owner_agent_id: null,
    requester_agent_id: null,
    assignee_agent_id: "agent-alpha",
    parent_card_id: null,
    latest_dispatch_id: null,
    sort_order: 0,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 1,
    github_issue_url: null,
    review_round: 0,
    metadata: null,
    metadata_json: null,
    pipeline_stage_id: null,
    review_status: null,
    created_at: "2026-04-01T00:00:00Z" as unknown as number,
    updated_at: "2026-04-10T00:00:00Z" as unknown as number,
    started_at: null,
    requested_at: null,
    completed_at: "2026-04-10T00:00:00Z" as unknown as number,
    ...overrides,
  };
}

describe("dashboardInsights", () => {
  it("parses ISO strings into timestamps", () => {
    expect(parseDashboardTimestamp("2026-04-10T00:00:00Z")).toBeGreaterThan(0);
  });

  it("builds agent ROI rows from card completions and token shares", () => {
    const shares: ReceiptSnapshotAgentShare[] = [
      { agent: "Alpha", tokens: 200_000, cost: 1.2, percentage: 60 },
      { agent: "베타", tokens: 300_000, cost: 1.4, percentage: 40 },
    ];
    const cards = [
      makeCard({ id: "card-alpha-1", assignee_agent_id: "agent-alpha" }),
      makeCard({ id: "card-alpha-2", assignee_agent_id: "agent-alpha" }),
      makeCard({ id: "card-beta-1", assignee_agent_id: "agent-beta" }),
      makeCard({
        id: "card-old",
        assignee_agent_id: "agent-beta",
        completed_at: "2026-03-01T00:00:00Z" as unknown as number,
      }),
    ];

    const rows = buildAgentRoiRows({
      cards,
      agentShares: shares,
      agents,
      periodStart: "2026-04-01",
      periodEnd: "2026-04-30",
    });

    expect(rows[0]).toMatchObject({
      id: "agent-alpha",
      completed_cards: 2,
    });
    expect(rows[0].cards_per_million_tokens).toBeCloseTo(10, 4);
    expect(rows[1]).toMatchObject({
      id: "agent-beta",
      completed_cards: 1,
    });
  });

  it("counts ROI only from completed_at without falling back to updated_at", () => {
    const shares: ReceiptSnapshotAgentShare[] = [
      { agent: "Alpha", tokens: 100_000, cost: 0.5, percentage: 100 },
    ];
    const cards = [
      makeCard({
        id: "card-missing-completed-at",
        assignee_agent_id: "agent-alpha",
        status: "done",
        completed_at: null,
        updated_at: "2026-04-12T00:00:00Z" as unknown as number,
      }),
    ];

    const rows = buildAgentRoiRows({
      cards,
      agentShares: shares,
      agents,
      periodStart: "2026-04-01",
      periodEnd: "2026-04-30",
    });

    expect(rows[0]).toMatchObject({
      id: "agent-alpha",
      completed_cards: 0,
    });
  });

  it("estimates rework count from review rounds and metadata", () => {
    expect(
      estimateReworkCount(
        makeCard({
          review_round: REWORK_ALERT_THRESHOLD + 1,
        }),
      ),
    ).toBe(REWORK_ALERT_THRESHOLD);
    expect(
      estimateReworkCount(
        makeCard({
          review_round: 0,
          metadata_json: JSON.stringify({ redispatch_count: 4 }),
        }),
      ),
    ).toBe(4);
  });

  it("groups delayed review, repeated rework, and long blocked cards", () => {
    const now = new Date("2026-04-13T00:00:00Z").getTime();
    const groups = buildBottleneckGroups([
      makeCard({
        id: "card-review",
        status: "review",
        updated_at: new Date(now - REVIEW_DELAY_DAYS * 24 * 60 * 60 * 1000).toISOString() as unknown as number,
      }),
      makeCard({
        id: "card-rework",
        status: "done",
        review_round: REWORK_ALERT_THRESHOLD + 1,
        completed_at: now,
      }),
      makeCard({
        id: "card-blocked",
        status: "blocked",
        updated_at: new Date(now - LONG_BLOCKED_DAYS * 24 * 60 * 60 * 1000).toISOString() as unknown as number,
      }),
    ], now);

    expect(groups.review_delay.map((row) => row.id)).toContain("card-review");
    expect(groups.repeat_rework.map((row) => row.id)).toContain("card-rework");
    expect(groups.long_blocked.map((row) => row.id)).toContain("card-blocked");
  });

  it("uses review_entered_at before updated_at for review delay age", () => {
    const now = new Date("2026-04-13T00:00:00Z").getTime();
    const groups = buildBottleneckGroups([
      makeCard({
        id: "card-review-entered",
        status: "review",
        review_entered_at: new Date(now - REVIEW_DELAY_DAYS * 24 * 60 * 60 * 1000).toISOString() as unknown as number,
        updated_at: new Date(now - 60 * 60 * 1000).toISOString() as unknown as number,
      }),
    ], now);

    expect(groups.review_delay.map((row) => row.id)).toContain("card-review-entered");
  });
});
