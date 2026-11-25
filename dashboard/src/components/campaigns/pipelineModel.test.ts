import { expect, it } from "vitest";
import template from "./fixtures/pipeline-template-campaign-development-9-v1.json";
import { campaignPipelineSchema, type PipelineDefinition } from "../../api/serverCampaignPipeline";
import { buildPipelineGraph, pinSummary, retryText } from "./pipelineModel";

const tr = (ko: string) => ko;
const pipeline = template.pipeline as PipelineDefinition;
const graph = buildPipelineGraph(pipeline, tr, null);
const line = (id: string) => graph.lines.find((value) => value.id === id);

it("lays the live template's main flow left to right from the start to the success end, with the failure end below it", () => {
  expect(graph.main).toEqual(["requirements", "design", "design_review", "implementation", "review", "landing", "main_verification", "final_verification", "complete"]);
  expect(graph.off).toEqual(["failed"]);
  const xs = graph.main.map((id) => graph.positions.get(id)!.x);
  expect(xs.every((x, index) => index === 0 || x > xs[index - 1])).toBe(true);
  expect(graph.positions.get("failed")!.y).toBeGreaterThan(Math.max(...graph.main.map((id) => graph.positions.get(id)!.y)));
  expect(graph.occupancy).toBeNull();
});

it("gives each result a pin, pass first and failure last, with a retry's budget and where it goes once spent", () => {
  expect(graph.pins.get("review")!.map((pin) => [pin.label, pin.tone, pin.target])).toEqual([
    ["성공", "pass", "landing"], ["품질 실패", "retry", "implementation"], ["3회 넘음", "fail", "failed"], ["실패", "fail", "failed"]]);
  expect(pinSummary(graph, "implementation", tr)).toBe("성공 → 리뷰 · 품질 실패 ↻3 → 제자리 · 3회 넘음 → 작업 실패 · 실패 → 작업 실패");
  expect(retryText(graph, graph.pins.get("implementation")![1].transition, tr)).toBe("제자리에서 3회 더, 그다음 작업 실패");
  expect(retryText(graph, graph.pins.get("design_review")![1].transition, tr)).toBe("1회까지, 그다음 작업 실패");
});

it("loops back under the cards, skips over them, drops to the row below, and draws no line for a retry in place", () => {
  const bottom = Math.max(...graph.main.map((id) => graph.positions.get(id)!.y));
  expect(line("review-to-implementation")).toMatchObject({ tone: "retry", retry: 3 });
  expect(line("review-to-implementation")!.lane).toBeGreaterThan(bottom);
  expect(line("requirements-to-implementation")!.lane).toBeLessThan(0);
  expect(line("design-to-design_review")!.lane).toBeUndefined();
  expect(line("landing-to-failed")).toMatchObject({ tone: "fail", drop: true });
  expect(line("implementation-to-implementation")).toBeUndefined();
  expect([graph.gates.get("landing"), graph.gates.get("implementation"), graph.gates.get("complete")]).toEqual([["landing"], ["implementation"], undefined]);
});

it("counts a run's tasks in each state, a failed one at the failure end and a blocked one apart, leaving out ids the graph lacks", () => {
  const run = buildPipelineGraph(pipeline, tr, { A: { state: "review" }, B: { state: "review" }, C: { state: "landing" }, D: { state: null }, E: { state: "gone" },
    F: { state: "implementation", disposition: "failed" }, G: { state: "implementation", disposition: "running" }, H: { state: "landing", disposition: "blocked" } });
  expect(["review", "landing", "implementation", "failed", "design"].map((id) => run.occupancy!.get(id))).toEqual([
    { here: 2, held: 0 }, { here: 1, held: 1 }, { here: 1, held: 0 }, { here: 1, held: 0 }, { here: 0, held: 0 }]);
});

it("numbers the flow as drawn, drops only from the flow, reads an unknown kind and lays out an empty graph", () => {
  const link = (from: string, to: string, value = "succeeded") => ({ id: `${from}-${to}`, from, to, priority: 10, condition: { field: "state.outcome", op: "eq", value } });
  const probe = buildPipelineGraph({ ...pipeline, start: "a", events: [], states: [{ id: "z", kind: "end_success" }, { id: "b", kind: "future_kind" }, { id: "a", kind: "agent_task" },
    { id: "fix", kind: "agent_task" }, { id: "f", kind: "end_failure" }], transitions: [link("a", "b"), link("b", "z"), link("a", "fix", "failed"), link("fix", "f", "failed")] }, tr, null);
  expect([probe.main, probe.lines.find((value) => value.id === "fix-f")!.drop, probe.lines.find((value) => value.id === "a-fix")!.drop]).toEqual([["a", "b", "z"], undefined, true]);
  expect(campaignPipelineSchema.safeParse({ pipeline: { start: "b", states: [{ id: "b", kind: "future_kind" }], transitions: [] }, definition_revision: 1, editable: false,
    execution: { mode: "legacy_stage", run_id: null, state_ids: {} } }).success).toBe(true);
  expect(Object.values(buildPipelineGraph({ ...pipeline, states: [], transitions: [] }, tr, null).bounds).every(Number.isFinite)).toBe(true);
});

it("keeps the reachable states as the flow when no path reaches a success end, and puts an event under its state", () => {
  const broken: PipelineDefinition = { ...pipeline, states: pipeline.states.filter((state) => state.kind !== "end_success"),
    transitions: pipeline.transitions.filter((value) => value.to !== "complete"), events: [{ id: "e1", state_id: "review", type: "timer.expired" }] };
  const result = buildPipelineGraph(broken, tr, null);
  expect(result.main).toContain("main_verification");
  expect(result.lines.find((value) => value.id === "event:e1")).toMatchObject({ tone: "event", target: "review" });
});
