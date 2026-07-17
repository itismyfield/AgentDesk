// @vitest-environment happy-dom

import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it, vi } from "vitest";

import * as api from "../../api";
import type { KanbanCard } from "../../types";
import { useKanbanBoardModel } from "./useKanbanBoardModel";

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
    latest_dispatch_id: null,
    sort_order: 0,
    priority: "medium",
    depth: 0,
    blocked_reason: null,
    review_notes: null,
    github_issue_number: 4520,
    github_issue_url: null,
    metadata_json: null,
    pipeline_stage_id: null,
    review_status: null,
    created_at: "2026-07-17T00:00:00Z",
    updated_at: "2026-07-17T00:00:00Z",
    started_at: null,
    requested_at: null,
    completed_at: null,
    ...overrides,
  };
}

function ModelProbe({
  params,
  onModel,
}: {
  params: Record<string, unknown>;
  onModel: (model: ReturnType<typeof useKanbanBoardModel>) => void;
}) {
  onModel(useKanbanBoardModel(params));
  return null;
}

function createParams(cards: KanbanCard[], overrides: Record<string, unknown> = {}) {
  return {
    agentFilter: "all",
    agentMap: new Map(),
    agents: [],
    agentPipelineStages: [],
    cardTypeFilter: "all",
    cards,
    cardsById: new Map(cards.map((card) => [card.id, card])),
    deptFilter: "all",
    getAgentLabel: (agentId: string | null | undefined) => agentId ?? "Unassigned",
    issues: [],
    mobileColumnStatus: "backlog",
    nowMs: Date.parse("2026-07-18T00:00:00Z"),
    repoSources: [],
    search: "",
    selectedAgentId: null,
    selectedCardId: null,
    selectedRepo: "itismyfield/AgentDesk",
    setAgentPipelineStages: vi.fn(),
    setRecentDonePage: vi.fn(),
    setSelectedAgentId: vi.fn(),
    setSelectedCardId: vi.fn(),
    showClosed: true,
    signalStatusFilter: "all",
    staleInProgressMs: 60 * 60 * 1000,
    tr: (_ko: string, en: string) => en,
    ...overrides,
  };
}

describe("useKanbanBoardModel status grouping", () => {
  let container: HTMLDivElement | null = null;
  let root: Root | null = null;

  afterEach(async () => {
    if (root) {
      await act(async () => {
        root?.unmount();
      });
      root = null;
    }
    container?.remove();
    container = null;
    vi.restoreAllMocks();
  });

  it("places cancelled cards in Done and excludes them from the open count", async () => {
    const cancelledCard = makeCard({ id: "cancelled-card", status: "cancelled" });
    const modelRef: { current: ReturnType<typeof useKanbanBoardModel> | null } = { current: null };
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    await act(async () => {
      root?.render(
        <ModelProbe
          params={createParams([cancelledCard])}
          onModel={(nextModel) => {
            modelRef.current = nextModel;
          }}
        />,
      );
    });

    if (!modelRef.current) throw new Error("Expected board model to render");
    expect(modelRef.current.cardsByStatus.get("done")).toEqual([cancelledCard]);
    expect(modelRef.current.openCount).toBe(0);
  });

  it("preserves configured custom stages and falls back unknown statuses to Backlog", async () => {
    vi.spyOn(api, "getPipelineStagesForAgent").mockResolvedValue([]);
    const customStageCard = makeCard({ id: "custom-stage-card", status: "release_gate" });
    const unknownStatusCard = makeCard({ id: "unknown-status-card", status: "legacy_hold" });
    const modelRef: { current: ReturnType<typeof useKanbanBoardModel> | null } = { current: null };
    container = document.createElement("div");
    document.body.appendChild(container);
    root = createRoot(container);

    await act(async () => {
      root?.render(
        <ModelProbe
          params={createParams([customStageCard, unknownStatusCard], {
            selectedAgentId: "agent-1",
            agentPipelineStages: [{ stage_name: "release_gate", trigger_after: "review_pass" }],
          })}
          onModel={(nextModel) => {
            modelRef.current = nextModel;
          }}
        />,
      );
    });

    if (!modelRef.current) throw new Error("Expected board model to render");
    expect(modelRef.current.cardsByStatus.get("release_gate")).toEqual([customStageCard]);
    expect(modelRef.current.cardsByStatus.get("backlog")).toEqual([unknownStatusCard]);
  });
});
