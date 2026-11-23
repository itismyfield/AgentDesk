import { expect, it } from "vitest";
import template from "../../../e2e/fixtures/pipeline-template-campaign-development-9-v1.json";
import type { PipelineDefinition } from "../../api/serverCampaignPipeline";
import { addState, addTransition, changesText, conditionOf, diagnosticPlace, diagnosticState, draftChanges, freshComparison, patchState, patchTransition, removal, removeState, setRetryMax } from "./pipelineEdit";

const tr = (ko: string) => ko;
const base = template.pipeline as PipelineDefinition;

it("adds a state and a way out after the state's priorities, and counts what changed, a value set back counting as none", () => {
  const [withState, id] = addState(base, "agent_task");
  const [draft, way] = addTransition(withState, "review");
  expect([id, way, draft.transitions.at(-1)]).toEqual(["agent_task-1", "review-out-1", expect.objectContaining({ from: "review", to: "", priority: 30 })]);
  const changes = draftChanges(base, patchState(draft, "review", { title: "리뷰 2차" }));
  expect([[...changes.touched].sort(), changesText(changes, tr)]).toEqual([["agent_task-1", "review"], "상태 +1 바뀜 1 · 전환 +1"]);
  expect(draftChanges(base, patchState(patchState(base, "review", { title: "x" }), "review", { title: "" })).count).toBe(0);
});

it("deletes a state with its transitions, events and the timeouts on them, counts the retries falling back to it, drops an end's executor, and keeps a shared retry budget one size", () => {
  const timed = patchState(patchTransition(base, "design_review-to-design", { retry: { budget_id: "design_repair", max: 1, then_to: "review" } }), "implementation", { timeout: { after_seconds: 60, transition_id: "implementation-to-review" } });
  const before = { ...timed, events: [{ id: "e", state_id: "review", type: "timer.expired" }] };
  expect(removal(before, "review")).toMatchObject({ events: 1, timeouts: ["implementation"], retries: 1 });
  const removed = removeState(before, "review");
  expect([removed.states.some((state) => state.id === "review"), removed.transitions.filter((value) => value.from === "review" || value.to === "review"), removed.events]).toEqual([false, [], []]);
  expect(removed.states.find((state) => state.id === "implementation")).not.toHaveProperty("timeout");
  expect(patchState(base, "landing", { kind: "end_failure" }).states.find((state) => state.id === "landing")).not.toHaveProperty("executor");
  expect(setRetryMax(base, "quality_repair", 5).transitions.filter((value) => value.retry?.budget_id === "quality_repair").map((value) => value.retry!.max)).toEqual([5, 5]);
  expect(patchTransition(base, "review-to-landing", { gate: undefined }).transitions.find((value) => value.id === "review-to-landing")).not.toHaveProperty("gate");
});

it("writes conditions back flat, with a fresh value of the field's type, and names a diagnostic by the state its transition leaves", () => {
  const count = { field: "review.p0_count", type: "integer", operators: ["eq", "lt"], enum_values: [], available: true };
  const verdict = freshComparison({ ...count, field: "review.verdict", type: "enum", operators: ["eq", "ne"], enum_values: ["clean", "issues"] });
  expect(conditionOf([freshComparison(count)])).toEqual({ field: "review.p0_count", op: "eq", value: 0 });
  expect(conditionOf([freshComparison(count), verdict])).toEqual({ all: [{ field: "review.p0_count", op: "eq", value: 0 }, { field: "review.verdict", op: "eq", value: "clean" }] });
  expect(diagnosticPlace({ code: "TRANSITION_TARGET_MISSING", path: "/transitions/13/to", state_id: "", transition_id: "review-to-landing" }, [base], tr)).toBe("리뷰 · review-to-landing");
  expect(diagnosticState({ code: "INVALID_TIMEOUT_TRANSITION", path: "/states/3/timeout", state_id: "review", transition_id: "implementation-to-review" }, [base])).toBe("review");
});
