import type { PipelineComparison, PipelineCondition, PipelineDefinition, PipelineDiagnostic, PipelineFieldSpec, PipelineState, PipelineTransition } from "../../api/serverCampaignPipeline";
import type { Tr } from "./campaignPresentation";
import { stateName } from "./pipelineModel";

type Draft = PipelineDefinition;
type Label = [string, string];

/** The server's diagnostic codes (src/domain/campaign/pipeline_validate.rs, pipeline_editor.rs) in words. */
const DIAGNOSTICS: Record<string, Label> = {
  START_MISSING: ["시작 상태가 없어요", "No start state"], START_NOT_FOUND: ["시작 상태가 목록에 없어요", "The start state is not in the list"],
  INVALID_STATE_ID: ["상태 ID가 겹치거나 쓸 수 없는 글자가 있어요", "A state ID repeats or has a character it cannot use"],
  INVALID_EXECUTOR: ["이 종류에는 이 실행기를 쓸 수 없어요", "This kind cannot use this executor"],
  INVALID_TRANSITION_ID: ["전환 ID가 겹치거나 쓸 수 없는 글자가 있어요", "A transition ID repeats or has a character it cannot use"],
  DUPLICATE_PRIORITY: ["같은 상태에서 나가는 전환의 우선순위가 겹쳐요", "Two ways out of one state share a priority"],
  TRANSITION_SOURCE_MISSING: ["전환이 떠나는 상태가 없어요", "The transition leaves a state that is gone"],
  TRANSITION_TARGET_MISSING: ["전환에 도착 상태가 없어요", "The transition has no state to go to"],
  RETRY_TARGET_MISSING: ["재시도를 다 쓴 뒤 갈 상태가 없어요", "No state to go to once the retries run out"],
  INVALID_RETRY_BUDGET: ["같은 재시도 한도를 쓰는 전환끼리 횟수가 달라요", "Transitions sharing a retry budget disagree on its size"],
  TERMINAL_HAS_EXIT: ["끝 상태에는 나가는 전환을 둘 수 없어요", "An end state cannot have a way out"],
  INVALID_TIMEOUT_TRANSITION: ["시간 초과가 이 상태의 전환을 가리키지 않아요", "The timeout does not name a way out of this state"],
  INVALID_EVENT_ID: ["이벤트 ID가 겹치거나 쓸 수 없는 글자가 있어요", "An event ID repeats or has a character it cannot use"],
  EVENT_STATE_MISSING: ["이벤트가 붙은 상태가 없어요", "The event's state is gone"],
  EMPTY_CONDITION: ["조건이 비어 있어요", "The condition is empty"],
  FIELD_NOT_ALLOWED: ["이 종류에서 쓸 수 없는 조건 필드예요", "This kind cannot use this condition field"],
  OP_NOT_ALLOWED: ["이 필드에 쓸 수 없는 비교예요", "This field does not allow this comparison"],
  VALUE_TYPE_MISMATCH: ["조건 값이 필드 형식과 맞지 않아요", "The value does not fit the field"],
  STATE_UNREACHABLE: ["시작에서 닿지 않는 상태예요", "The start never reaches this state"],
  STATE_NO_EXIT: ["나가는 전환이 없어요", "No way out of this state"],
  NO_TERMINAL_PATH: ["끝 상태로 가는 길이 없어요", "No path from here reaches an end"],
  SUCCESS_UNREACHABLE: ["시작에서 성공 끝으로 가는 길이 없어요", "No path from the start reaches a success end"],
  UNBOUNDED_CYCLE: ["재시도 한도 없이 되돌아가는 고리가 있어요", "A loop goes back without a retry limit"],
  UNSUPPORTED_VERSION: ["이 화면이 모르는 정의 버전이에요", "A definition version this screen does not know"],
  STATE_IN_USE: ["작업이 남아 있는 상태라 지울 수 없어요", "Tasks still sit in this state, so it cannot go"],
};
export const diagnosticText = (code: string, tr: Tr) => DIAGNOSTICS[code] ? tr(...DIAGNOSTICS[code]) : code;
/** The state a diagnostic belongs to: the one its transition leaves, else its own (which the draft may have removed). */
export const diagnosticState = ({ state_id: state, transition_id: transition }: PipelineDiagnostic, graphs: Draft[]) =>
  graphs.flatMap((graph) => graph.transitions).find((value) => value.id === transition)?.from ?? state;
/** Where a diagnostic points: that state by name (from the draft, or the saved graph) and the transition's id. */
export function diagnosticPlace(diagnostic: PipelineDiagnostic, graphs: Draft[], tr: Tr) {
  const state = diagnosticState(diagnostic, graphs);
  return [state && stateName(graphs.flatMap((graph) => graph.states).find((value) => value.id === state), state, tr), diagnostic.transition_id].filter(Boolean).join(" · ");
}

const stable = (value: unknown) => JSON.stringify(value, (_, item) => item && typeof item === "object" && !Array.isArray(item) ? Object.fromEntries(Object.entries(item).sort(([a], [b]) => a < b ? -1 : 1)) : item);
const fresh = (taken: { id: string }[], stem: string) => { let n = 1; while (taken.some((value) => value.id === `${stem}-${n}`)) n += 1; return `${stem}-${n}`; };

/** What a draft changes against its base, by id; a state counts as touched when it or a way out of it changed. */
export function draftChanges(base: Draft, draft: Draft) {
  const diff = <T extends { id: string }>(before: T[], after: T[]) => {
    const old = new Map(before.map((value) => [value.id, stable(value)]));
    return { added: after.filter((value) => !old.has(value.id)).map((value) => value.id), removed: before.filter((value) => !after.some((other) => other.id === value.id)).map((value) => value.id),
      changed: after.filter((value) => old.has(value.id) && old.get(value.id) !== stable(value)).map((value) => value.id) };
  };
  const states = diff(base.states, draft.states), transitions = diff(base.transitions, draft.transitions);
  const from = (graph: Draft, id: string) => graph.transitions.find((value) => value.id === id)!.from;
  const touched = new Set([...states.added, ...states.changed, ...[...transitions.added, ...transitions.changed].map((id) => from(draft, id)), ...transitions.removed.map((id) => from(base, id))]);
  const count = [states, transitions].reduce((sum, { added, removed, changed }) => sum + added.length + removed.length + changed.length, 0);
  return { states, transitions, touched, count };
}
export type DraftChanges = ReturnType<typeof draftChanges>;
/** `상태 +1 −1 바뀜 2 · 전환 바뀜 3`, leaving out what did not move. */
export function changesText({ states, transitions }: DraftChanges, tr: Tr) {
  const part = (ko: string, en: string, { added, removed, changed }: DraftChanges["states"]) => added.length + removed.length + changed.length === 0 ? null
    : [tr(ko, en), added.length && `+${added.length}`, removed.length && `−${removed.length}`, changed.length && tr(`바뀜 ${changed.length}`, `changed ${changed.length}`)].filter(Boolean).join(" ");
  return [part("상태", "States", states), part("전환", "Transitions", transitions)].filter(Boolean).join(" · ");
}

/** Sets fields of one state: an emptied text field or timeout leaves the graph, and an end state drops its executor, which ends cannot have. */
export function patchState(draft: Draft, id: string, patch: Partial<PipelineState>): Draft {
  return { ...draft, states: draft.states.map((state) => {
    if (state.id !== id) return state;
    const next: PipelineState = { ...state, ...patch };
    for (const key of ["title", "owner", "instructions", "timeout"] as const) if (!next[key]) delete next[key];
    if (next.kind.startsWith("end_")) delete next.executor;
    return next;
  }) };
}
export function addState(draft: Draft, kind: string): [Draft, string] {
  const id = fresh(draft.states, kind);
  return [{ ...draft, states: [...draft.states, { id, kind }] }, id];
}
/** The transitions from or to a state, which go with it. */
export const attached = (draft: Draft, id: string) => draft.transitions.filter((value) => value.from === id || value.to === id);
export function removeState(draft: Draft, id: string): Draft {
  return { ...draft, states: draft.states.filter((state) => state.id !== id), transitions: draft.transitions.filter((value) => value.from !== id && value.to !== id),
    events: draft.events.filter((event) => event.state_id !== id) };
}

/** A new way out of `from` on success, to a state still to choose, after its other priorities. */
export function addTransition(draft: Draft, from: string): [Draft, string] {
  const id = fresh(draft.transitions, `${from}-out`);
  const priority = Math.max(0, ...draft.transitions.filter((value) => value.from === from).map((value) => value.priority)) + 10;
  return [{ ...draft, transitions: [...draft.transitions, { id, from, to: "", priority, condition: { field: "state.outcome", op: "eq", value: "succeeded" } }] }, id];
}
export function patchTransition(draft: Draft, id: string, patch: Partial<PipelineTransition>): Draft {
  return { ...draft, transitions: draft.transitions.map((value) => {
    if (value.id !== id) return value;
    const next: PipelineTransition = { ...value, ...patch };
    if (!next.retry) delete next.retry;
    if (!next.gate) delete next.gate;
    return next;
  }) };
}
export const removeTransition = (draft: Draft, id: string): Draft => ({ ...draft, transitions: draft.transitions.filter((value) => value.id !== id) });
/** A retry budget is one count however many transitions share it, so its max changes on all of them. */
export const setRetryMax = (draft: Draft, budget: string, max: number): Draft =>
  ({ ...draft, transitions: draft.transitions.map((value) => value.retry?.budget_id === budget ? { ...value, retry: { ...value.retry, max } } : value) });
/** A retry for a transition without one: its own budget, once, then the first failure end. */
export const newRetry = (draft: Draft, transition: PipelineTransition) =>
  ({ budget_id: transition.id, max: 1, then_to: draft.states.find((state) => state.kind === "end_failure")?.id ?? "" });

/** Comparisons go back as one, or under `all` when there are more. */
export const conditionOf = (list: PipelineComparison[]): PipelineCondition => list.length === 1 ? list[0] : { all: list };
/** A comparison on `spec` with its first operator and a value of its type. */
export const freshComparison = (spec: PipelineFieldSpec): PipelineComparison =>
  ({ field: spec.field, op: spec.operators[0] ?? "eq", value: spec.type === "boolean" ? true : spec.type === "integer" ? 0 : spec.type === "enum" ? spec.enum_values[0] ?? "" : "" });
