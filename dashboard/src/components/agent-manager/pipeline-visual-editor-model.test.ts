import { describe, expect, it } from "vitest";

import type { PipelineConfigFull, PipelineStage } from "../../types";
import {
  buildOverridePayload,
  buildPipelineGraph,
  buildStageSavePayload,
  extractOverrideExtras,
  normalizeStageTrigger,
  stageDraftFromApi,
} from "./pipeline-visual-editor-model";

function makePipeline(): PipelineConfigFull {
  return {
    name: "default",
    version: 1,
    states: [
      { id: "backlog", label: "Backlog" },
      { id: "ready", label: "Ready" },
      { id: "requested", label: "Requested" },
      { id: "in_progress", label: "In Progress" },
      { id: "review", label: "Review" },
      { id: "done", label: "Done", terminal: true },
    ],
    transitions: [
      { from: "backlog", to: "ready", type: "free", gates: [] },
      { from: "review", to: "done", type: "gated", gates: ["review_passed"] },
    ],
    gates: {
      review_passed: {
        type: "builtin",
        check: "review_verdict_pass",
        description: "Review pass",
      },
    },
    hooks: {
      review: {
        on_enter: ["OnReviewEnter"],
        on_exit: [],
      },
    },
    clocks: {
      review: {
        set: "review_entered_at",
      },
    },
    timeouts: {
      review: {
        duration: "30m",
        clock: "review_entered_at",
        on_exhaust: "review",
      },
    },
    phase_gate: {
      dispatch_to: "self",
      dispatch_type: "phase-gate",
      pass_verdict: "phase_gate_passed",
      checks: ["merge_verified", "issue_closed"],
    },
  };
}

describe("pipeline-visual-editor-model", () => {
  it("normalizes null trigger_after to ready", () => {
    const stage = stageDraftFromApi({
      id: "stage-1",
      repo: "itismyfield/AgentDesk",
      stage_name: "e2e",
      stage_order: 0,
      entry_skill: "playwright",
      provider: "counter",
      agent_override_id: null,
      timeout_minutes: 15,
      on_failure: "retry",
      on_failure_target: null,
      max_retries: 2,
      skip_condition: null,
      parallel_with: null,
      applies_to_agent_id: null,
      trigger_after: null as unknown as PipelineStage["trigger_after"],
      created_at: 0,
    });

    expect(normalizeStageTrigger(undefined)).toBe("ready");
    expect(stage.trigger_after).toBe("ready");
  });

  it("preserves other-agent stages when saving filtered stages", () => {
    const repoStages = [
      {
        id: "global",
        repo: "itismyfield/AgentDesk",
        stage_name: "global-stage",
        stage_order: 0,
        entry_skill: "skill-a",
        provider: null,
        agent_override_id: null,
        timeout_minutes: 30,
        on_failure: "fail",
        on_failure_target: null,
        max_retries: 1,
        skip_condition: null,
        parallel_with: null,
        applies_to_agent_id: null,
        trigger_after: "ready",
        created_at: 0,
      },
      {
        id: "agent-b",
        repo: "itismyfield/AgentDesk",
        stage_name: "agent-b-only",
        stage_order: 1,
        entry_skill: "skill-b",
        provider: "counter",
        agent_override_id: null,
        timeout_minutes: 20,
        on_failure: "retry",
        on_failure_target: null,
        max_retries: 2,
        skip_condition: null,
        parallel_with: null,
        applies_to_agent_id: "agent-b",
        trigger_after: "review_pass",
        created_at: 0,
      },
    ] satisfies PipelineStage[];

    const payload = buildStageSavePayload(repoStages, [stageDraftFromApi(repoStages[0])], "agent-a");

    expect(payload).toHaveLength(2);
    expect(payload[0].stage_name).toBe("global-stage");
    expect(payload[1]).toMatchObject({
      stage_name: "agent-b-only",
      applies_to_agent_id: "agent-b",
      trigger_after: "review_pass",
    });
  });

  it("keeps non-visual override keys when building save payload", () => {
    const extras = extractOverrideExtras({
      events: { on_dispatch_completed: ["OnDispatchCompleted"] },
      note: "keep me",
    });
    const payload = buildOverridePayload(makePipeline(), extras);

    expect(payload.events).toEqual({
      on_dispatch_completed: ["OnDispatchCompleted"],
    });
    expect(payload.note).toBe("keep me");
    expect(payload.states).toHaveLength(8);
    expect(payload.phase_gate?.dispatch_type).toBe("phase-gate");
  });

  it("builds a single-column graph for compact mode", () => {
    const compact = buildPipelineGraph(makePipeline(), true);
    const desktop = buildPipelineGraph(makePipeline(), false);

    expect(compact.columns).toBe(1);
    expect(compact.nodes[1].x).toBe(compact.nodes[0].x);
    expect(compact.nodes[1].y).toBeGreaterThan(compact.nodes[0].y);
    expect(compact.edges[0].path).toContain("C");

    expect(desktop.columns).toBe(4);
    expect(desktop.nodes[1].x).toBeGreaterThan(desktop.nodes[0].x);
  });

  it("routes upward transitions from the source top edge", () => {
    const pipeline = makePipeline();
    pipeline.transitions.push({
      from: "review",
      to: "in_progress",
      type: "gated",
      gates: ["review_passed"],
    });

    const graph = buildPipelineGraph(pipeline, false);
    const edge = graph.edges.at(-1);
    const fromNode = graph.nodes.find((node) => node.id === "review");
    const toNode = graph.nodes.find((node) => node.id === "in_progress");

    expect(edge).toBeTruthy();
    expect(fromNode).toBeTruthy();
    expect(toNode).toBeTruthy();
    expect(edge?.path.startsWith(`M ${fromNode!.x + fromNode!.width / 2} ${fromNode!.y}`)).toBe(
      true,
    );
    expect(
      edge?.path.endsWith(`${toNode!.x + toNode!.width / 2} ${toNode!.y + toNode!.height}`),
    ).toBe(true);
    expect(edge?.labelY).toBeLessThan(fromNode!.y);
  });

  it("renders self-loop transitions as looped bezier paths", () => {
    const pipeline = makePipeline();
    pipeline.transitions.push({
      from: "review",
      to: "review",
      type: "free",
      gates: [],
    });

    const graph = buildPipelineGraph(pipeline, false);
    const edge = graph.edges.at(-1);
    const reviewNode = graph.nodes.find((node) => node.id === "review");

    expect(edge).toBeTruthy();
    expect(reviewNode).toBeTruthy();
    expect(edge?.path.split("C")).toHaveLength(3);
    expect(edge?.path.endsWith(`${reviewNode!.x + reviewNode!.width / 2} ${reviewNode!.y}`)).toBe(
      true,
    );
    expect(edge?.labelY).toBeLessThan(reviewNode!.y);
  });
});
