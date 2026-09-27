import { describe, expect, it } from "vitest";

import type { PipelineConfigFull, PipelineStage } from "../../types";
import {
  buildOverridePayload,
  buildPipelineGraph,
  buildStageSavePayload,
  clonePipelineConfig,
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
    events: {
      on_dispatch_completed: ["OnDispatchCompleted"],
    },
    clocks: {
      review: {
        set: "review_entered_at",
      },
    },
    phase_gate: {
      dispatch_to: "self",
      dispatch_type: "phase-gate",
    },
  };
}

describe("pipeline-visual-editor-model", () => {
  it("keeps stored stage values the editor does not offer", () => {
    const stage = stageDraftFromApi({
      id: "stage-1",
      repo: "itismyfield/AgentDesk",
      stage_name: "e2e",
      stage_order: 0,
      provider: "codex",
      agent_override_id: null,
      skip_condition: "label:hotfix",
      trigger_after: null as unknown as PipelineStage["trigger_after"],
    });

    expect(normalizeStageTrigger(undefined)).toBe("ready");
    expect(stage).toMatchObject({ trigger_after: "ready", provider: "codex", skip_condition: "label:hotfix" });
    expect(buildStageSavePayload([stage])[0]).toMatchObject({ provider: "codex", skip_condition: "label:hotfix" });
  });

  it("saves the repo's full stage list with only runtime fields", () => {
    const payload = buildStageSavePayload([
      { stage_name: " e2e ", provider: "counter", agent_override_id: "", skip_condition: "no_rs_changes", trigger_after: "review_pass" },
      { stage_name: "  ", provider: "", agent_override_id: "", skip_condition: "", trigger_after: "ready" },
    ]);

    expect(payload).toEqual([
      { stage_name: "e2e", provider: "counter", agent_override_id: null, skip_condition: "no_rs_changes", trigger_after: "review_pass" },
    ]);
  });

  // A non-visual key the Rust override schema accepts, so the fixture stays a
  // payload the server would take.
  const fsmEdgeBindings = { "review->done": { event: "on_review_verdict" } };

  it("keeps non-visual override keys when building save payload", () => {
    const extras = extractOverrideExtras({
      events: { on_dispatch_completed: ["OnDispatchCompleted"] },
      fsm_edge_bindings: fsmEdgeBindings,
    });
    const payload = buildOverridePayload(makePipeline(), extras);

    expect(payload.events).toEqual({
      on_dispatch_completed: ["OnDispatchCompleted"],
    });
    expect(payload.fsm_edge_bindings).toEqual(fsmEdgeBindings);
    expect(payload.states).toHaveLength(6);
    expect(payload.phase_gate?.dispatch_type).toBe("phase-gate");
  });

  it("drops a stored timeouts section instead of saving it back", () => {
    const extras = extractOverrideExtras({
      timeouts: { review: { duration: "30m", clock: "review_entered_at" } },
      fsm_edge_bindings: fsmEdgeBindings,
    });
    const payload = buildOverridePayload(makePipeline(), extras);

    expect(payload).not.toHaveProperty("timeouts");
    expect(payload.fsm_edge_bindings).toEqual(fsmEdgeBindings);
  });

  it("clones and saves a GET response that has no timeouts section", () => {
    const pipeline = makePipeline();
    expect(pipeline).not.toHaveProperty("timeouts");

    const clone = clonePipelineConfig(pipeline);
    expect(clone).toEqual(pipeline);
    expect(buildOverridePayload(clone)).not.toHaveProperty("timeouts");
  });

  it("does not throw when the pipeline has no events map (runtime payload may omit it)", () => {
    const pipeline = makePipeline();
    // Simulate a backend payload that omits `events`. The interface declares
    // `events` as required, but real overrides occasionally lack it.
    delete (pipeline as { events?: unknown }).events;

    expect(() => buildOverridePayload(pipeline, {})).not.toThrow();
    const payload = buildOverridePayload(pipeline, {});
    expect(payload.events).toEqual({});
  });

  it("builds a single-column graph for compact mode", () => {
    const compact = buildPipelineGraph(makePipeline(), true);
    const desktop = buildPipelineGraph(makePipeline(), false);

    expect(compact.columns).toBe(1);
    expect(compact.nodes[1].x).toBe(compact.nodes[0].x);
    expect(compact.nodes[1].y).toBeGreaterThan(compact.nodes[0].y);
    expect(compact.edges[0].path).toContain("L");

    expect(desktop.columns).toBe(3);
    expect(desktop.nodes[1].x).toBeGreaterThan(desktop.nodes[0].x);
  });

  it("routes upward transitions through the left-side return lane", () => {
    const pipeline = makePipeline();
    pipeline.transitions.push({
      from: "review",
      to: "ready",
      type: "gated",
      gates: ["review_passed"],
    });

    const graph = buildPipelineGraph(pipeline, false);
    const edge = graph.edges.at(-1);
    const fromNode = graph.nodes.find((node) => node.id === "review");
    const toNode = graph.nodes.find((node) => node.id === "ready");

    expect(edge).toBeTruthy();
    expect(fromNode).toBeTruthy();
    expect(toNode).toBeTruthy();
    expect(
      edge?.path.startsWith(
        `M ${fromNode!.x} ${fromNode!.y + fromNode!.height / 2}`,
      ),
    ).toBe(true);
    expect(
      edge?.path.endsWith(
        `${toNode!.x} ${toNode!.y + toNode!.height / 2}`,
      ),
    ).toBe(true);
    expect(edge?.labelRotated).toBe(true);
    expect(edge?.labelX).toBeLessThan(fromNode!.x);
    expect(edge?.labelY).toBe(
      (fromNode!.y + fromNode!.height / 2 + toNode!.y + toNode!.height / 2) /
        2,
    );
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
