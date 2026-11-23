import type { PipelineComparison as Comparison, PipelineCondition, PipelineDefinition, PipelineState, PipelineTransition } from "../../api/serverCampaignPipeline";
import { LAYER, layoutGroups, type Point } from "./campaignGroupLayout";
import { GATE_LABELS } from "./campaignOverviewModel";
import type { Tr } from "./campaignPresentation";
import { STAGE_LABELS } from "./serverCampaignModel";

type Label = [string, string];
export const KIND_LABELS: Record<string, Label> = {
  agent_task: ["에이전트 작업", "Agent task"], auto_check: ["자동 검사", "Auto check"], auto_action: ["자동 동작", "Auto action"], human_decision: ["사람 판단", "Human decision"],
  wait: ["대기", "Wait"], end_success: ["끝 · 성공", "End · success"], end_failure: ["끝 · 실패", "End · failure"],
};
const OUTCOMES: Record<string, Label> = { succeeded: ["성공", "Succeeded"], failed: ["실패", "Failed"], quality_failed: ["품질 실패", "Quality failed"] };
const EVENTS: Record<string, Label> = {
  "evidence.accepted": ["증거 수락", "Evidence accepted"], "decision.recorded": ["결정 기록", "Decision recorded"], "action.reconciled": ["동작 확인", "Action reconciled"],
  "timer.expired": ["시간 만료", "Timer expired"], "dependency.satisfied": ["선행 충족", "Dependency satisfied"],
};
/** Template states carry no title, so their ids take the stage board's names. */
const STATE_NAMES: Record<string, Label> = { ...STAGE_LABELS, failed: ["작업 실패", "Task failed"] };
export const OPS: Record<string, string> = { eq: "=", ne: "≠", lt: "<", le: "≤", gt: ">", ge: "≥" };

export const stateName = (state: PipelineState | undefined, id: string, tr: Tr) => state?.title || (STATE_NAMES[id] ? tr(...STATE_NAMES[id]) : id);
/** A kind or gate this build does not know shows by its id. */
export const kindName = (kind: string, tr: Tr) => KIND_LABELS[kind] ? tr(...KIND_LABELS[kind]) : kind;
export const eventName = (type: string, tr: Tr) => EVENTS[type] ? tr(...EVENTS[type]) : type;
export const gateName = (gate: string, tr: Tr) => { const [ko, en] = GATE_LABELS[gate as keyof typeof GATE_LABELS] ?? [gate, gate]; return tr(`${ko} 승인`, `${en} approval`); };
export const comparisons = (condition: PipelineCondition): Comparison[] => Array.isArray(condition.all) ? (condition.all as PipelineCondition[]).flatMap(comparisons) : [condition as Comparison];
export const comparisonText = ({ field, op, value }: Comparison) => `${field} ${OPS[op] ?? op} ${String(value)}`;
export const durationText = (seconds: number, tr: Tr) => seconds % 3600 === 0 ? tr(`${seconds / 3600}시간`, `${seconds / 3600} h`) : seconds % 60 === 0 ? tr(`${seconds / 60}분`, `${seconds / 60} min`) : tr(`${seconds}초`, `${seconds} s`);

/** pass and fail follow `state.outcome`; a transition with a retry budget is a retry, whatever its outcome. */
export type Tone = "pass" | "fail" | "retry" | "other";
/** A result pin on a state's right edge: one per transition, and one more where a retry budget runs out. */
export type Pin = { id: string; transition: PipelineTransition; tone: Tone; label: string; target: string; exhausted: boolean; hint?: string };
/** `lane` routes a line past the cards: above for a forward skip, below for a loop back; `drop` lines fall to the row below. */
export type Line = { id: string; source: string; handle: string; target: string; tone: Tone | "event"; retry?: number; lane?: number; drop?: boolean };

const RANK: Record<Tone, number> = { pass: 0, retry: 1, other: 2, fail: 3 };
const outcomeOf = (transition: PipelineTransition) => comparisons(transition.condition).find((value) => value.field === "state.outcome" && value.op === "eq")?.value;
const toneOf = (transition: PipelineTransition): Tone => {
  const outcome = outcomeOf(transition);
  return transition.retry ? "retry" : outcome === "succeeded" ? "pass" : outcome === "failed" || outcome === "quality_failed" ? "fail" : "other";
};
export function pinsOf(id: string, transitions: readonly PipelineTransition[], tr: Tr): Pin[] {
  return transitions.filter((value) => value.from === id).map((transition) => ({ transition, tone: toneOf(transition) }))
    .sort((a, b) => RANK[a.tone] - RANK[b.tone] || a.transition.priority - b.transition.priority)
    .flatMap(({ transition, tone }) => {
      const outcome = outcomeOf(transition);
      const pin: Pin = { id: transition.id, transition, tone, label: OUTCOMES[String(outcome)] ? tr(...OUTCOMES[String(outcome)]) : tr("조건", "Condition"), target: transition.to, exhausted: false };
      const retry = transition.retry;
      return retry ? [pin, { id: `${transition.id}:then`, transition, tone: "fail" as const, label: tr(`${retry.max}회 넘음`, `After ${retry.max}`), target: retry.then_to, exhausted: true }] : [pin];
    });
}

/** Card size in graph units; a card grows by one row per result pin. */
export const PIPE_WIDTH = 184;
const PITCH = PIPE_WIDTH + 64;
export const cardHeight = (pins: number) => 94 + 24 * Math.max(1, pins);
const LANE = 16;

/** A run's task: the state the server puts it in and its disposition. */
export type PipelineTask = { state: string | null; disposition?: string };
const HELD = new Set(["blocked", "interrupted"]);

/** The graph laid out as the relations view lays out groups: the main flow (states on a start-to-success path that
 * takes no failure or retry) left to right by depth, then the states off it in a row below, each under its sources.
 * `approvals` (a server campaign's workflow options) marks the gates the campaign leaves off. */
export function buildPipelineGraph(pipeline: PipelineDefinition, tr: Tr, tasks: Record<string, PipelineTask> | null, approvals: Record<string, boolean> | null = null, editing = false) {
  const states = new Map(pipeline.states.map((state) => [state.id, state]));
  const pins = new Map(pipeline.states.map((state) => [state.id, pinsOf(state.id, pipeline.transitions, tr)]));
  // While editing, a state that can leave has one more row: its '+ 전환' pin.
  const rows = new Map(pipeline.states.map((state) => [state.id, pins.get(state.id)!.length + Number(editing && !state.kind.startsWith("end_"))]));
  // Two pins with one label on a card also name where they go.
  for (const list of pins.values()) for (const pin of list) if (list.filter((other) => other.label === pin.label).length > 1) pin.hint = stateName(states.get(pin.target), pin.target, tr);
  const flow = pipeline.transitions.filter((value) => value.from !== value.to && !value.retry && states.has(value.to) && toneOf(value) !== "fail");
  const reach = (seeds: string[], step: (t: PipelineTransition) => [string, string]) => {
    const seen = new Set(seeds.filter((id) => states.has(id)));
    for (const id of seen) for (const value of flow) { const [from, to] = step(value); if (from === id) seen.add(to); }
    return seen;
  };
  const forward = reach([pipeline.start], (value) => [value.from, value.to]);
  const back = reach(pipeline.states.filter((state) => state.kind === "end_success").map((state) => state.id), (value) => [value.to, value.from]);
  const both = [...forward].filter((id) => back.has(id));
  const mainSet = new Set(both.length ? both : forward);
  const main = [pipeline.start, ...pipeline.states.map((state) => state.id).filter((id) => id !== pipeline.start && mainSet.has(id))].filter((id) => states.has(id));
  const off = pipeline.states.map((state) => state.id).filter((id) => !mainSet.has(id));
  const height = Math.max(0, ...pipeline.states.map((state) => cardHeight(rows.get(state.id)!)));
  const links = flow.filter((value) => mainSet.has(value.from) && mainSet.has(value.to)).map((value) => [value.from, value.to] as const);
  const { placed, below } = layoutGroups(main, links, height);
  const positions = new Map<string, Point>(main.map((id) => [id, { x: placed.get(id)!.x / LAYER * PITCH, y: placed.get(id)!.y }]));
  const column = (id: string) => Math.round(positions.get(id)!.x / PITCH);
  // Lists number the flow as it is drawn, whatever order the states array holds.
  main.sort((a, b) => positions.get(a)!.x - positions.get(b)!.x || positions.get(a)!.y - positions.get(b)!.y);

  const lines: Line[] = [];
  let above = 0, under = 0;
  for (const [id, list] of pins) for (const pin of list) {
    if (pin.target === id || !states.has(pin.target) || !states.has(id)) continue;
    const line: Line = { id: pin.id, source: id, handle: pin.id, target: pin.target, tone: pin.tone, retry: pin.exhausted ? undefined : pin.transition.retry?.max };
    if (mainSet.has(id) && !mainSet.has(pin.target)) line.drop = true;
    else if (mainSet.has(id) && column(pin.target) <= column(id)) line.lane = below + 28 + LANE * under++;
    else if (mainSet.has(id) && column(pin.target) > column(id) + 1) line.lane = -60 - LANE * above++;
    lines.push(line);
  }
  // Off-flow states sit past the loop lanes, each centred under the states that lead to it and kept a card apart.
  const top = below + 64 + LANE * under;
  let next = -Infinity;
  const mean = (id: string) => { const from = lines.filter((line) => line.target === id && positions.has(line.source)).map((line) => positions.get(line.source)!.x); return from.length ? from.reduce((a, b) => a + b, 0) / from.length : 0; };
  for (const id of [...off].sort((a, b) => mean(a) - mean(b))) { const x = Math.max(mean(id), next); positions.set(id, { x, y: top }); next = x + PITCH; }
  const events = pipeline.events.filter((event) => states.has(event.state_id));
  next = -Infinity;
  for (const event of events) {
    const x = Math.max(positions.get(event.state_id)!.x, next);
    positions.set(`event:${event.id}`, { x, y: top + (off.length ? height + 48 : 0) });
    next = x + PITCH;
    lines.push({ id: `event:${event.id}`, source: `event:${event.id}`, handle: "event", target: event.state_id, tone: "event" });
  }
  for (const line of lines) if (line.drop) line.lane = top - 24;
  const gates = new Map<string, string[]>();
  for (const value of pipeline.transitions) if (value.gate && value.from !== value.to) gates.set(value.to, [...new Set([...gates.get(value.to) ?? [], value.gate])]);
  // Only a run's own state ids count: a node before its run (null) or on a state this graph lacks is left out. A failed
  // task has left its stage for the failure end; a blocked or interrupted one is counted apart as held.
  const failure = pipeline.states.find((state) => state.kind === "end_failure")?.id ?? "";
  const occupancy = tasks && new Map(pipeline.states.map((state) => [state.id, { here: 0, held: 0 }]));
  for (const { state, disposition = "" } of occupancy ? Object.values(tasks!) : []) {
    const at = occupancy!.get(disposition === "failed" ? failure : state ?? "");
    if (at && HELD.has(disposition)) at.held += 1; else if (at) at.here += 1;
  }
  const xs = [...positions.values()].map((point) => point.x), ys = [...positions.values()].map((point) => point.y);
  // Room above for the start and gate badges and the skip lanes; below for the tallest card in the last row.
  const bounds = { x0: Math.min(0, ...xs), x1: Math.max(0, ...xs) + PIPE_WIDTH, y0: Math.min(0, ...ys) - 44 - LANE * above, y1: Math.max(0, ...ys) + height };
  return { states, pins, rows, main, off, events, positions, lines, gates, occupancy, approvals, bounds };
}
export type PipelineGraph = ReturnType<typeof buildPipelineGraph>;

/** A gate's name; one the campaign's workflow options leave off reads as only in the definition. */
export const gateText = (graph: PipelineGraph, gate: string, tr: Tr) => graph.approvals && !graph.approvals[gate] ? tr(`정의상 ${gateName(gate, tr)}`, `${gateName(gate, tr)} (definition only)`) : gateName(gate, tr);
/** A state's tasks as badge texts: those in it, then those held there. */
export function occupancyTexts(graph: PipelineGraph, id: string, tr: Tr) {
  const { here = 0, held = 0 } = graph.occupancy?.get(id) ?? {};
  return [here > 0 && { text: tr(`작업 ${here}`, `${here} tasks`), held: false }, held > 0 && { text: tr(`막힘 ${held}`, `${held} held`), held: true }].filter((value) => value !== false);
}

/** One line per state for lists: each result and where it goes, a retry as `↻n`. */
export function pinSummary(graph: PipelineGraph, id: string, tr: Tr) {
  return graph.pins.get(id)!.map((pin) => `${pin.label}${pin.tone === "retry" ? ` ↻${pin.transition.retry!.max}` : ""} → ${pin.target === id ? tr("제자리", "in place") : stateName(graph.states.get(pin.target), pin.target, tr)}`).join(" · ");
}

/** A retry in words: how many more times, and where it goes once they run out. */
export function retryText(graph: PipelineGraph, transition: PipelineTransition, tr: Tr) {
  const { max, then_to: then } = transition.retry!;
  const after = stateName(graph.states.get(then), then, tr);
  return transition.from === transition.to ? tr(`제자리에서 ${max}회 더, 그다음 ${after}`, `${max} more here, then ${after}`) : tr(`${max}회까지, 그다음 ${after}`, `Up to ${max} times, then ${after}`);
}
